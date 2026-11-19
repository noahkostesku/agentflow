//! Run history for the profiler: one record per finished session, appended to
//! `~/.agentflow/runs.jsonl` (never anywhere under a harness's own storage).
//!
//! Runs come from two places, both built from the same `model::Session` the
//! views draw (so the numbers match what you saw):
//!   * live mode records a session when its batch of agents finishes
//!     ([`Recorder`]);
//!   * `agentflow profile` / `compare` index the project's sessions that have
//!     gone quiet and aren't on record yet ([`index`]).
//!
//! The file is append-only JSON lines. A session recorded again (it grew, or
//! was indexed after being watched) appends a newer line with the same id;
//! loading keeps the last one. Runs group into workflows: the same project and
//! the same set of agent roles, unless tagged (`--workflow`, or a
//! `[workflows.<name>]` rule in the config).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::discovery::Found;
use crate::model::{Agent, Session};
use crate::pricing::Pricing;
use crate::schema::Harness;
use crate::stats;

/// Record version in `v`. Adding optional fields is not a bump.
pub const VERSION: u32 = 1;

/// How long after a batch of agents finishes live mode writes its run, so the
/// orchestrator's wrap-up is counted. Exiting writes it right away.
pub const GRACE_MS: i64 = 30_000;

/// One finished session.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Run {
    pub v: u32,
    /// "claude", "codex"
    pub harness: String,
    /// the harness's session id
    pub session: String,
    /// the project directory it ran in ("" when unknown)
    pub project: String,
    /// explicit workflow tag (`--workflow` while it was recorded)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// epoch ms it started (first billed call or dispatch)
    pub started: i64,
    /// the session's span, ms: its first billed call or dispatch to the last
    /// agent's end or billed call (the orchestrator's work included)
    pub wall_ms: i64,
    /// summed agent run time, ms
    pub work_ms: i64,
    /// most agents running at once
    pub peak: usize,
    /// average agents running (work ÷ wall)
    pub avg: f64,
    /// roles on the critical path, in time order
    pub critical: Vec<String>,
    /// ≈USD at API list prices, orchestrator included
    pub cost: f64,
    pub tokens: u64,
    /// the orchestrator ("" model and zero cost when one agent worked alone)
    #[serde(skip_serializing_if = "String::is_empty")]
    pub orch_model: String,
    pub orch_cost: f64,
    pub orch_tokens: u64,
    pub agents: Vec<AgentRun>,
    pub tool_calls: u64,
    /// agents that ended in error
    pub errors: u32,
    /// agents that never reported back (interrupted)
    pub unfinished: u32,
    /// the last test command's outcome (`None`: no tests ran)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tests: Option<bool>,
    /// the session file it was indexed from, and its activity time (epoch
    /// ms) then, so `profile` re-reads it only when it changed
    #[serde(skip_serializing_if = "String::is_empty")]
    pub src: String,
    pub src_mtime: i64,
}

/// One agent in a run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentRun {
    /// what it is in the workflow: the role, or for a Codex subagent its path
    /// ("repo_overview"), not its per-run nickname
    pub role: String,
    /// model family ("opus", "haiku", …)
    pub model: String,
    pub ms: i64,
    pub cost: f64,
    pub tokens: u64,
    pub tool_calls: u64,
    pub error: bool,
    /// on the run's critical path
    pub critical: bool,
}

impl Run {
    /// Unique across harnesses.
    pub fn id(&self) -> String {
        format!("{}:{}", self.harness, self.session)
    }

    /// The distinct roles that worked in it.
    pub fn roles(&self) -> BTreeSet<&str> {
        self.agents.iter().map(|a| a.role.as_str()).collect()
    }

    /// Every agent finished, none in error, and the last test run (if any)
    /// passed.
    pub fn ok(&self) -> bool {
        self.errors == 0 && self.unfinished == 0 && self.tests != Some(false)
    }

    /// Build a run from a session as of `t`. Agents still running at `t`
    /// count as ending at their last sign of life (an inactive session's
    /// unfinished agents were interrupted). `None` for sessions without an
    /// id (the demo) or without any agent.
    pub fn from_session(s: &Session, t: i64) -> Option<Run> {
        let id = s.id.as_ref()?;
        let mut unfinished = 0;
        let agents: Vec<Agent> = s
            .agents
            .iter()
            .filter(|a| a.spawn <= t)
            .map(|a| {
                let mut a = a.clone();
                if a.finish.is_none_or(|f| f > t) {
                    unfinished += 1;
                    a.finish = Some(stats::last_activity(&a, t).max(a.spawn));
                }
                a
            })
            .collect();
        if agents.is_empty() {
            return None;
        }
        let par = stats::parallelism(&agents, t);
        let norm = Session { agents, handoffs: s.handoffs.clone(), live: true, ..Session::default() };
        let path = stats::critical_path(&norm, t);
        let mut tokens = 0;
        for c in s.all_calls_at(t) {
            tokens += c.usage.total();
        }
        let orch: Vec<_> = s.orch_calls.iter().filter(|c| c.time <= t).collect();
        let tests = norm
            .agents
            .iter()
            .flat_map(|a| a.tests.iter())
            .filter(|r| r.time <= t)
            .max_by_key(|r| r.time)
            .map(|r| r.passed);
        // the orchestrator works before the first dispatch and after the last
        // result: the run spans that too
        let first = norm.agents.iter().map(|a| a.spawn).chain(orch.iter().map(|c| c.time)).min().unwrap_or(0);
        let last = norm.agents.iter().filter_map(|a| a.finish).chain(orch.iter().map(|c| c.time)).max().unwrap_or(first);
        Some(Run {
            v: VERSION,
            harness: s.harness.unwrap_or(Harness::Claude).name().to_string(),
            session: id.clone(),
            project: s.project.clone().unwrap_or_default(),
            workflow: None,
            started: s.epoch0.unwrap_or(0) + first,
            wall_ms: last - first,
            work_ms: par.work_ms,
            peak: par.peak,
            avg: par.avg(),
            critical: path.iter().map(|&i| role_of(&norm.agents[i])).collect(),
            cost: s.cost_at(t),
            tokens,
            orch_model: if orch.is_empty() { String::new() } else { s.orch_model.clone() },
            orch_cost: orch.iter().map(|c| c.cost).sum(),
            orch_tokens: orch.iter().map(|c| c.usage.total()).sum(),
            agents: norm
                .agents
                .iter()
                .enumerate()
                .map(|(i, a)| AgentRun {
                    role: role_of(a),
                    model: a.model.clone(),
                    ms: a.finish.unwrap_or(t) - a.spawn,
                    cost: a.cost_at(t),
                    tokens: a.usage_at(t).total(),
                    tool_calls: a.uses.iter().filter(|u| u.time <= t).count() as u64,
                    error: a.error,
                    critical: path.contains(&i),
                })
                .collect(),
            tool_calls: norm.agents.iter().flat_map(|a| a.uses.iter()).filter(|u| u.time <= t).count() as u64,
            errors: norm.agents.iter().filter(|a| a.error).count() as u32,
            unfinished,
            tests,
            src: String::new(),
            src_mtime: 0,
        })
    }
}

/// An agent's role in a workflow: Codex names subagents per run
/// ("socrates") and keeps the stable part in the tag ("repo_overview").
fn role_of(a: &Agent) -> String {
    if a.tag.is_empty() {
        a.name.clone()
    } else {
        a.tag.clone()
    }
}

/// When a session's activity ends, on its own clock: the latest finish, sign
/// of life, tool call or billed call.
pub fn session_end(s: &Session) -> i64 {
    let agents = s.agents.iter().map(|a| stats::last_activity(a, i64::MAX).max(a.finish.unwrap_or(0)));
    let orch = s.orch_calls.iter().map(|c| c.time);
    agents.chain(orch).max().unwrap_or(0)
}

// ---- runs.jsonl ----------------------------------------------------------------

pub fn path() -> PathBuf {
    crate::config::dir().join("runs.jsonl")
}

/// Every run on record, one per id (the newest record wins), oldest first.
#[derive(Clone, Debug, Default)]
pub struct Store {
    pub runs: Vec<Run>,
    /// lines in the file, superseded ones included (drives compaction)
    lines: usize,
}

impl Store {
    /// Load the file; a missing file is empty and bad lines are skipped.
    pub fn load(path: &Path) -> Store {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let mut st = Store::default();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            st.lines += 1;
            if let Ok(run) = serde_json::from_str::<Run>(line) {
                st.upsert(run);
            }
        }
        st.runs.sort_by_key(|r| r.started);
        st
    }

    pub fn get(&self, id: &str) -> Option<&Run> {
        self.runs.iter().find(|r| r.id() == id)
    }

    /// Replace or add in memory. A newer record without a tag keeps the
    /// older one's (indexing a session that was watched with `--workflow`).
    pub fn upsert(&mut self, mut run: Run) {
        let id = run.id();
        match self.runs.iter_mut().find(|r| r.id() == id) {
            Some(old) => {
                if run.workflow.is_none() {
                    run.workflow = old.workflow.take();
                }
                *old = run;
            }
            None => self.runs.push(run),
        }
    }

    /// Record a run: append it to the file and update the store. Rewrites
    /// the file without superseded lines once they outnumber the runs.
    pub fn append(&mut self, path: &Path, run: Run) -> std::io::Result<()> {
        self.upsert(run.clone());
        self.runs.sort_by_key(|r| r.started);
        let run = self.get(&run.id()).cloned().unwrap_or(run);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        self.lines += 1;
        if self.lines > 2 * self.runs.len() + 20 {
            // another process (`notify`, a view) may have appended since this
            // store was loaded: rewrite from the file, not from memory
            let mut disk = Store::load(path);
            disk.upsert(run);
            disk.runs.sort_by_key(|r| r.started);
            *self = disk;
            return self.compact(path);
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        let mut line = serde_json::to_string(&run).unwrap_or_default();
        line.push('\n');
        f.write_all(line.as_bytes())
    }

    /// Rewrite atomically with one line per run.
    fn compact(&mut self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("jsonl.tmp");
        let mut out = String::new();
        for r in &self.runs {
            out.push_str(&serde_json::to_string(r).unwrap_or_default());
            out.push('\n');
        }
        std::fs::write(&tmp, out)?;
        std::fs::rename(tmp, path)?;
        self.lines = self.runs.len();
        Ok(())
    }
}

// ---- live recording -------------------------------------------------------------

/// Live mode's run recorder: a session's run is written [`GRACE_MS`] after
/// its batch of agents finishes (once per batch), and again on exit or on
/// switching sessions if it changed since.
pub struct Recorder {
    path: PathBuf,
    store: Store,
    /// `--workflow`: tag every run recorded this time
    tag: Option<String>,
    /// (run id, the batch's `Session::all`) already written
    written: HashSet<(String, i64)>,
    /// latest numbers per finished session, not yet written
    pending: HashMap<String, Run>,
}

impl Recorder {
    pub fn new(path: PathBuf, tag: Option<String>) -> Self {
        let store = Store::load(&path);
        Recorder { path, store, tag, written: HashSet::new(), pending: HashMap::new() }
    }

    /// Note the session's state at `t`; writes when a finished batch's grace
    /// period is up.
    pub fn update(&mut self, s: &Session, t: i64) {
        if s.all == i64::MAX || t < s.all {
            return;
        }
        let Some(mut run) = Run::from_session(s, t) else {
            return;
        };
        run.workflow.clone_from(&self.tag);
        let batch = (run.id(), s.all);
        if t >= s.all + GRACE_MS && !self.written.contains(&batch) {
            self.pending.remove(&batch.0);
            self.write(run);
            self.written.insert(batch);
        } else {
            self.pending.insert(batch.0, run);
        }
    }

    /// Write what's pending (exit, session switch) unless already on record.
    pub fn flush(&mut self) {
        let pending: Vec<Run> = self.pending.drain().map(|(_, r)| r).collect();
        for run in pending {
            self.write(run);
        }
    }

    /// Write a finished session's run unless it's on record unchanged (the
    /// inbox and `notify`, for every session that finishes).
    pub fn record(&mut self, run: Run) {
        self.write(run);
    }

    fn write(&mut self, mut run: Run) {
        if let Some(old) = self.store.get(&run.id()) {
            // keep what indexing learned about the file, so it isn't re-read
            if run.src.is_empty() {
                run.src.clone_from(&old.src);
                run.src_mtime = old.src_mtime;
            }
            if run.workflow.is_none() {
                run.workflow.clone_from(&old.workflow);
            }
            if *old == run {
                return;
            }
        }
        // best effort: a read-only home just means no history
        let _ = self.store.append(&self.path, run);
    }
}

// ---- indexing finished sessions --------------------------------------------------

fn epoch_ms(t: SystemTime) -> i64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

/// Read a stored session from start to end and build its run, as of its last
/// activity. `when` is that activity (the file's, subagents' included): it
/// becomes `src_mtime`, so it isn't re-read until it changes. A session where
/// nothing worked still gets a record (no agents), so it isn't re-read
/// either. Reads only the harness's files.
pub fn read_session(found: &Found, when: SystemTime, pricing: &Pricing) -> Run {
    let mut live = crate::live::Live::new(found.open(), String::new());
    // subagents can surface a poll after their parent names them
    live.refresh();
    live.refresh();
    let sess = live.build(pricing);
    let mut run = Run::from_session(&sess, session_end(&sess)).unwrap_or_else(|| Run {
        v: VERSION,
        harness: found.harness.name().to_string(),
        session: sess.id.clone().unwrap_or_else(|| found.path.to_string_lossy().into_owned()),
        ..Run::default()
    });
    if run.project.is_empty() {
        if let Some(dir) = found.project_dir.as_deref().or(live.cwd().map(Path::new)) {
            run.project = dir.to_string_lossy().into_owned();
        }
    }
    run.src = found.path.to_string_lossy().into_owned();
    run.src_mtime = epoch_ms(when);
    run
}

/// Record every session of `project` (in `harnesses`) that has been quiet for
/// [`crate::discovery::ACTIVE`] and isn't on record as of its latest activity.
/// Reads the harnesses' files, writes only `path`, and only when `write`
/// (`persistence` on; off, the runs are added in memory). Returns how many
/// runs it added or updated.
pub fn index(store: &mut Store, path: &Path, write: bool, harnesses: &[Harness], project: &Path, pricing: &Pricing) -> usize {
    let now = SystemTime::now();
    let seen: HashMap<&str, i64> =
        store.runs.iter().filter(|r| !r.src.is_empty()).map(|r| (r.src.as_str(), r.src_mtime)).collect();
    let todo: Vec<_> = crate::discovery::sessions(harnesses, project)
        .into_iter()
        .filter(|(_, when)| now.duration_since(*when).unwrap_or_default() >= crate::discovery::ACTIVE)
        .filter(|(f, when)| seen.get(f.path.to_string_lossy().as_ref()) != Some(&epoch_ms(*when)))
        .collect();
    if todo.len() > 20 {
        eprintln!("agentflow: indexing {} sessions…", todo.len());
    }
    let mut added = 0;
    for (found, when) in todo {
        let run = read_session(&found, when, pricing);
        if !write {
            store.upsert(run);
            added += 1;
        } else if store.append(path, run).is_ok() {
            added += 1;
        }
    }
    added
}

// ---- importing every stored session ----------------------------------------------

/// What [`import`] did.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Imported {
    /// sessions found in the harnesses' storage
    pub found: usize,
    /// runs added (sessions with at least one agent)
    pub runs: usize,
    /// workflows those runs belong to
    pub workflows: usize,
    /// sessions skipped: already on record (by session id or file)
    pub known: usize,
    /// sessions skipped: active within [`crate::discovery::ACTIVE`], not
    /// finished yet (recorded once they are)
    pub running: usize,
    /// sessions read where nothing worked (recorded without agents, so a
    /// second import skips them)
    pub empty: usize,
}

/// Backfill run history from every stored session in `sessions` (each with
/// its latest activity): read each one that isn't on record yet and has gone
/// quiet, and append its run to `path`. A session is on record when a run
/// has its id (`claude:<id>`, from a watched view, the inbox or `notify`) or
/// was indexed from its file, at any activity time: importing twice adds
/// nothing. Reads the harnesses' files, writes only `path`. `progress(done,
/// total)` is called before each session read and once at the end.
pub fn import(
    store: &mut Store,
    path: &Path,
    sessions: Vec<(Found, SystemTime)>,
    now: SystemTime,
    pricing: &Pricing,
    rules: &BTreeMap<String, WorkflowRule>,
    progress: &mut dyn FnMut(usize, usize),
) -> std::io::Result<Imported> {
    let mut out = Imported { found: sessions.len(), ..Imported::default() };
    let ids: HashSet<String> = store.runs.iter().map(Run::id).collect();
    let srcs: HashSet<&str> = store.runs.iter().filter(|r| !r.src.is_empty()).map(|r| r.src.as_str()).collect();
    let mut todo = Vec::new();
    for (found, when) in sessions {
        // the adapter names the session from its file alone (nothing read yet)
        let id = format!("{}:{}", found.harness.name(), found.open().session_id());
        if ids.contains(&id) || srcs.contains(found.path.to_string_lossy().as_ref()) {
            out.known += 1;
        } else if now.duration_since(when).unwrap_or_default() < crate::discovery::ACTIVE {
            out.running += 1;
        } else {
            todo.push((found, when));
        }
    }
    // oldest first, so the file reads in the order things happened
    todo.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.path.cmp(&b.0.path)));
    let total = todo.len();
    let mut added: Vec<Run> = Vec::new();
    for (i, (found, when)) in todo.into_iter().enumerate() {
        progress(i, total);
        let run = read_session(&found, when, pricing);
        if run.agents.is_empty() {
            out.empty += 1;
        } else {
            added.push(run.clone());
        }
        store.append(path, run)?;
    }
    progress(total, total);
    out.runs = added.len();
    out.workflows = group(&added, rules).len();
    Ok(out)
}

// ---- workflows ------------------------------------------------------------------

/// `[workflows.<name>]` in the config: runs with all of these roles (in this
/// project, if given) belong to the named workflow.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WorkflowRule {
    pub roles: Vec<String>,
    pub project: Option<String>,
}

impl WorkflowRule {
    fn matches(&self, run: &Run) -> bool {
        let roles = run.roles();
        !self.roles.is_empty()
            && self.roles.iter().all(|r| roles.contains(r.as_str()))
            && self.project.as_deref().is_none_or(|p| {
                crate::discovery::normalize_project(p).to_string_lossy().trim_end_matches('/')
                    == run.project.trim_end_matches('/')
            })
    }
}

/// Runs of one workflow.
#[derive(Clone, Debug, PartialEq)]
pub struct Workflow {
    /// grouping key: "tag:<name>", or "<project>|<roles>"
    pub key: String,
    /// display name: the tag, or the roles ("scout+reviewer+fixer")
    pub name: String,
    /// project directory name ("" when unknown or mixed)
    pub project: String,
    /// oldest first
    pub runs: Vec<Run>,
}

/// The workflow a run belongs to: (key, name).
pub fn workflow_of(run: &Run, rules: &BTreeMap<String, WorkflowRule>) -> (String, String) {
    let tag = run.workflow.clone().or_else(|| rules.iter().find(|(_, r)| r.matches(run)).map(|(n, _)| n.clone()));
    match tag {
        Some(t) => (format!("tag:{t}"), t),
        None => {
            let roles: Vec<&str> = run.roles().into_iter().collect();
            (format!("{}|{}", run.project, roles.join("+")), roles.join("+"))
        }
    }
}

/// Group runs into workflows, the most recently run first.
pub fn group(runs: &[Run], rules: &BTreeMap<String, WorkflowRule>) -> Vec<Workflow> {
    let mut out: Vec<Workflow> = Vec::new();
    for run in runs.iter().filter(|r| !r.agents.is_empty()) {
        let (key, name) = workflow_of(run, rules);
        let dir = Path::new(&run.project).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
        match out.iter_mut().find(|w| w.key == key) {
            Some(w) => {
                if w.project != dir {
                    w.project.clear();
                }
                w.runs.push(run.clone());
            }
            None => out.push(Workflow { key, name, project: dir, runs: vec![run.clone()] }),
        }
    }
    for w in &mut out {
        w.runs.sort_by_key(|r| r.started);
    }
    out.sort_by_key(|w| std::cmp::Reverse(w.runs.last().map_or(0, |r| r.started)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Call, TestRun, Usage, Use};

    fn agent(name: &str, spawn: i64, finish: Option<i64>, cost: f64) -> Agent {
        Agent {
            name: name.into(),
            model: "haiku".into(),
            spawn,
            finish,
            calls: vec![Call { time: spawn + 10, cost, saved: 0.0, usage: Usage { output: 100, ..Usage::default() } }],
            uses: vec![Use { time: spawn + 20, tool: "read".into(), file: None, edit: false, arg: String::new() }],
            ..Agent::default()
        }
    }

    fn session(id: &str, agents: Vec<Agent>) -> Session {
        Session {
            id: Some(id.into()),
            epoch0: Some(1_000_000),
            live: true,
            harness: Some(Harness::Claude),
            project: Some("/work/app".into()),
            all: agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500,
            agents,
            ..Session::default()
        }
    }

    pub fn run(session: &str, project: &str, roles: &[&str], started: i64) -> Run {
        Run {
            v: VERSION,
            harness: "claude".into(),
            session: session.into(),
            project: project.into(),
            started,
            agents: roles.iter().map(|r| AgentRun { role: r.to_string(), ..AgentRun::default() }).collect(),
            ..Run::default()
        }
    }

    #[test]
    fn run_from_session_numbers() {
        // scout 0–4s and researcher 1–3s in parallel, then fixer 5–9s
        let mut s = session(
            "s1",
            vec![agent("scout", 0, Some(4_000), 0.10), agent("researcher", 1_000, Some(3_000), 0.20), agent("fixer", 5_000, Some(9_000), 0.30)],
        );
        s.agents[2].tests = vec![TestRun { time: 6_000, passed: false }, TestRun { time: 8_000, passed: true }];
        s.agents[1].error = true;
        s.orch_model = "opus".into();
        s.orch_calls = vec![Call { time: 0, cost: 0.05, saved: 0.0, usage: Usage { input: 7, ..Usage::default() } }];
        let r = Run::from_session(&s, 9_500).unwrap();
        assert_eq!((r.id().as_str(), r.project.as_str(), r.started), ("claude:s1", "/work/app", 1_000_000));
        assert_eq!((r.wall_ms, r.work_ms, r.peak), (9_000, 10_000, 2));
        // the orchestrator planning before the first dispatch and summing up
        // after the last result is part of the run
        let mut s2 = session("s3", vec![agent("scout", 2_000, Some(4_000), 0.1)]);
        s2.orch_calls = [-3_000, 9_000].map(|time| Call { time, cost: 0.5, saved: 0.0, usage: Usage::default() }).to_vec();
        let r2 = Run::from_session(&s2, 10_000).unwrap();
        assert_eq!((r2.started, r2.wall_ms, r2.work_ms), (997_000, 12_000, 2_000));
        assert_eq!(r.critical, ["scout", "fixer"]);
        assert!((r.cost - 0.65).abs() < 1e-9);
        assert_eq!((r.orch_model.as_str(), r.orch_tokens, r.tokens), ("opus", 7, 307));
        assert_eq!((r.tool_calls, r.errors, r.unfinished, r.tests), (3, 1, 0, Some(true)));
        assert!(!r.ok(), "an agent errored");
        let crit: Vec<bool> = r.agents.iter().map(|a| a.critical).collect();
        assert_eq!(crit, [true, false, true]);
        assert_eq!(r.agents[0].ms, 4_000);

        // an agent that never reported back ends at its last sign of life
        let s = session("s2", vec![agent("scout", 0, None, 0.1)]);
        let r = Run::from_session(&s, 60_000).unwrap();
        assert_eq!((r.unfinished, r.agents[0].ms, r.wall_ms), (1, 20, 20));
        // no id (the demo), or nothing spawned: not a run
        let mut demo = session("x", vec![agent("scout", 0, Some(1), 0.0)]);
        demo.id = None;
        assert!(Run::from_session(&demo, 10).is_none());
        assert!(Run::from_session(&session("e", vec![]), 10).is_none());
    }

    #[test]
    fn codex_roles_are_the_path_not_the_nickname() {
        let mut a = agent("socrates", 0, Some(10), 0.0);
        a.tag = "repo_overview".into();
        let r = Run::from_session(&session("c", vec![a]), 100).unwrap();
        assert_eq!(r.agents[0].role, "repo_overview");
    }

    #[test]
    fn append_and_load_keep_the_newest_record() {
        let dir = std::env::temp_dir().join(format!("aflow-runs-{}", std::process::id()));
        let p = dir.join("runs.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Store::load(&p).runs.is_empty(), "no file: empty");

        let mut st = Store::default();
        let mut a = run("a", "/w", &["scout"], 2_000);
        a.workflow = Some("nightly".into());
        st.append(&p, a.clone()).unwrap();
        st.append(&p, run("b", "/w", &["scout"], 1_000)).unwrap();
        let mut a2 = run("a", "/w", &["scout", "fixer"], 2_000);
        a2.cost = 1.0;
        st.append(&p, a2).unwrap();
        std::fs::write(&p, std::fs::read_to_string(&p).unwrap() + "{not json\n\n").unwrap();

        let back = Store::load(&p);
        assert_eq!(std::fs::read_to_string(&p).unwrap().lines().count(), 5, "append-only");
        let ids: Vec<String> = back.runs.iter().map(Run::id).collect();
        assert_eq!(ids, ["claude:b", "claude:a"], "one per id, oldest first");
        let a = back.get("claude:a").unwrap();
        assert_eq!((a.cost, a.agents.len()), (1.0, 2), "the newest line wins");
        assert_eq!(a.workflow.as_deref(), Some("nightly"), "an untagged update keeps the tag");

        // many superseded lines: compacted to one per run
        let mut st = back;
        for i in 0..30 {
            let mut r = run("b", "/w", &["scout"], 1_000);
            r.tool_calls = i;
            st.append(&p, r).unwrap();
        }
        let lines = std::fs::read_to_string(&p).unwrap().lines().count();
        assert!(lines < 25, "{lines} lines");
        assert_eq!(Store::load(&p).get("claude:b").unwrap().tool_calls, 29);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recorder_writes_once_per_batch_after_grace_and_on_flush() {
        let dir = std::env::temp_dir().join(format!("aflow-rec-{}", std::process::id()));
        let p = dir.join("runs.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let lines = || std::fs::read_to_string(&p).map_or(0, |t| t.lines().count());
        let mut rec = Recorder::new(p.clone(), Some("pr-review".into()));
        let mut s = session("live", vec![agent("scout", 0, Some(4_000), 0.1)]);
        s.agents.push(agent("fixer", 1_000, None, 0.1));
        s.all = i64::MAX;
        rec.update(&s, 5_000);
        rec.flush();
        assert_eq!(lines(), 0, "still running: nothing to record");

        s.agents[1].finish = Some(6_000);
        s.all = 6_500;
        rec.update(&s, 7_000);
        assert_eq!(lines(), 0, "within the grace period");
        rec.update(&s, 6_500 + GRACE_MS);
        rec.update(&s, 6_500 + GRACE_MS + 1_000);
        assert_eq!(lines(), 1, "once per batch");
        rec.flush();
        assert_eq!(lines(), 1, "unchanged: not written again");
        s.orch_calls.push(Call { time: 40_000, cost: 0.02, saved: 0.0, usage: Usage::default() });
        rec.update(&s, 41_000);
        rec.flush();
        assert_eq!(lines(), 2, "the orchestrator's wrap-up landed: written on exit");
        let st = Store::load(&p);
        let r = st.get("claude:live").unwrap();
        assert_eq!(r.workflow.as_deref(), Some("pr-review"));
        assert!((r.cost - 0.22).abs() < 1e-9);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Another process appending (`notify` running while a view records)
    /// doesn't lose its runs when this store compacts the file.
    #[test]
    fn compaction_keeps_runs_another_process_appended() {
        let dir = std::env::temp_dir().join(format!("aflow-compact-{}", std::process::id()));
        let p = dir.join("runs.jsonl");
        let _ = std::fs::remove_dir_all(&dir);
        let mut mine = Store::load(&p);
        let mut theirs = Store::load(&p);
        theirs.append(&p, run("theirs", "/w", &["scout"], 5)).unwrap();
        for i in 0..30 {
            let mut r = run("mine", "/w", &["scout"], 1);
            r.tool_calls = i;
            mine.append(&p, r).unwrap();
        }
        let back = Store::load(&p);
        assert!(std::fs::read_to_string(&p).unwrap().lines().count() < 25, "compacted");
        assert!(back.get("claude:theirs").is_some(), "the other process's run survived");
        assert_eq!(back.get("claude:mine").unwrap().tool_calls, 29);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `profile --import`: every harness's stored sessions in every project
    /// become runs (a Codex subagent is part of its parent's run, not one of
    /// its own); sessions on record (by id from a watched view, or by file),
    /// still running, or where nothing worked aren't imported as runs; a
    /// second import adds nothing.
    #[test]
    fn import_backfills_every_harness_and_skips_what_is_on_record() {
        use crate::adapters::claude::encode_path;
        use crate::discovery::{all_sessions_at, Roots};
        use std::fs;
        use std::time::Duration;

        let base = std::env::temp_dir().join(format!("aflow-import-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let roots = Roots { claude: base.join("claude"), codex: base.join("codex") };
        let p = base.join("agentflow").join("runs.jsonl");
        let now = SystemTime::now();
        let write = |path: &Path, text: &str, age_s: u64| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
            fs::File::options().write(true).open(path).unwrap().set_modified(now - Duration::from_secs(age_s)).unwrap();
        };
        let worked = |cwd: &str| {
            [
                format!(r#"{{"type":"user","timestamp":"2026-01-01T00:00:00.000Z","cwd":"{cwd}","message":{{"role":"user","content":"run the tests"}}}}"#),
                format!(r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:05.000Z","cwd":"{cwd}","message":{{"id":"m1","model":"claude-haiku-5-5","role":"assistant","stop_reason":"end_turn","content":[{{"type":"text","text":"All pass."}}],"usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#),
            ]
            .join("\n")
                + "\n"
        };
        let claude = |dir: &str, id: &str| roots.claude.join(encode_path(Path::new(dir))).join(format!("{id}.jsonl"));
        let app = claude("/w/app", "c-app");
        write(&app, &worked("/w/app"), 3_600);
        write(&claude("/w/app", "c-app-2"), &worked("/w/app"), 7_200);
        write(&claude("/w/app", "c-watched"), &worked("/w/app"), 3_600);
        write(&claude("/w/app", "c-indexed"), &worked("/w/app"), 3_600);
        write(&claude("/w/app", "c-empty"), "", 3_600);
        write(&claude("/w/app", "c-running"), &worked("/w/app"), 10);
        // a Codex parent and its subagent: one session
        let day = roots.codex.join("2026").join("01").join("02");
        let codex_s = day.join("rollout-2026-01-02T00-00-00-1111aaaa-0000-4000-8000-000000000001.jsonl");
        write(&codex_s, include_str!("../tests/fixtures/codex/subagents/rollout-2026-01-02T00-00-00-1111aaaa-0000-4000-8000-000000000001.jsonl"), 3_600);
        write(
            &day.join("rollout-2026-01-02T00-00-01-1111aaaa-0000-4000-8000-000000000002.jsonl"),
            include_str!("../tests/fixtures/codex/subagents/rollout-2026-01-02T00-00-01-1111aaaa-0000-4000-8000-000000000002.jsonl"),
            3_600,
        );

        let sessions = all_sessions_at(&roots, crate::discovery::SUPPORTED);
        assert_eq!(sessions.len(), 7, "six Claude sessions and one Codex session (its subagent isn't its own)");
        assert_eq!(all_sessions_at(&roots, &[Harness::Codex]).len(), 1, "--source narrows");

        // already on record: one recorded live (by id, no file), one indexed
        // from its file under an earlier activity time
        let mut store = Store::load(&p);
        store.append(&p, run("c-watched", "/w/app", &["claude"], 1)).unwrap();
        let mut indexed = run("c-indexed-old-id", "/w/app", &["claude"], 2);
        indexed.src = claude("/w/app", "c-indexed").to_string_lossy().into_owned();
        indexed.src_mtime = 1;
        store.append(&p, indexed).unwrap();

        let pricing = Pricing::default();
        let rules = BTreeMap::new();
        let mut ticks = Vec::new();
        let got = import(&mut store, &p, sessions, now, &pricing, &rules, &mut |d, t| ticks.push((d, t))).unwrap();
        assert_eq!(
            got,
            Imported { found: 7, runs: 3, workflows: 2, known: 2, running: 1, empty: 1 },
            "both /w/app runs are one workflow; the Codex team in /w/team another"
        );
        assert_eq!(ticks, [(0, 4), (1, 4), (2, 4), (3, 4), (4, 4)], "progress per session read, then done");
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 6, "two on record + four read (the empty one too)");

        let back = Store::load(&p);
        let a = back.get("claude:c-app").unwrap();
        assert_eq!((a.project.as_str(), a.src.as_str()), ("/w/app", app.to_string_lossy().as_ref()));
        assert_eq!(a.agents.len(), 1);
        let c = back.get("codex:1111aaaa-0000-4000-8000-000000000001").unwrap();
        assert_eq!(c.project, "/w/team");
        assert_eq!(c.src, codex_s.to_string_lossy());
        assert_eq!(c.roles().into_iter().collect::<Vec<_>>(), ["scan"], "the subagent's role, from its own rollout");
        assert!(back.get("claude:c-running").is_none(), "still running: not yet");
        assert!(back.get("claude:c-empty").is_some_and(|r| r.agents.is_empty()), "remembered, so it isn't read again");

        // importing again finds everything on record
        let mut store = back;
        let again = import(&mut store, &p, all_sessions_at(&roots, crate::discovery::SUPPORTED), now, &pricing, &rules, &mut |_, _| {}).unwrap();
        assert_eq!(again, Imported { found: 7, known: 6, running: 1, ..Imported::default() });
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 6, "nothing appended");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn workflows_group_by_project_and_role_set_or_tag() {
        let mut runs = vec![
            run("1", "/w/app", &["scout", "fixer"], 1),
            run("2", "/w/app", &["fixer", "scout", "scout"], 2),
            run("3", "/w/other", &["scout", "fixer"], 3),
            run("4", "/w/app", &["scout"], 4),
            run("5", "/w/app", &["scout", "fixer", "reviewer"], 5),
            run("6", "/w/app", &[], 6),
        ];
        runs[3].workflow = Some("Nightly".into());
        let none = BTreeMap::new();
        let g = group(&runs, &none);
        let summary: Vec<(&str, &str, usize)> = g.iter().map(|w| (w.name.as_str(), w.project.as_str(), w.runs.len())).collect();
        assert_eq!(
            summary,
            [("fixer+reviewer+scout", "app", 1), ("Nightly", "app", 1), ("fixer+scout", "other", 1), ("fixer+scout", "app", 2)],
            "same roles in another project is another workflow; empty runs are left out; newest first"
        );

        // a config rule names runs with at least its roles, in its project
        let rules = BTreeMap::from([(
            "pr-review".to_string(),
            WorkflowRule { roles: vec!["scout".into(), "fixer".into()], project: Some("/w/app".into()) },
        )]);
        let g = group(&runs, &rules);
        let pr = g.iter().find(|w| w.name == "pr-review").unwrap();
        let ids: Vec<&str> = pr.runs.iter().map(|r| r.session.as_str()).collect();
        assert_eq!(ids, ["1", "2", "5"], "an explicit --workflow tag beats a rule");
        assert_eq!(workflow_of(&runs[2], &rules).1, "fixer+scout", "other project");
    }
}
