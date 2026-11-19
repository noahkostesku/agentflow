//! Codex CLI adapter: a rollout file → schema events.
//!
//! Format (observed on codex-cli 0.170.0):
//!   * `$CODEX_HOME/sessions/YYYY/MM/DD/rollout-<local time>-<uuid>.jsonl`
//!     (`$CODEX_HOME` defaults to `~/.codex`), one per session. Each line is
//!     `{timestamp, ordinal, type, payload}`.
//!   * `session_meta`: the session `id` and `cwd`. `turn_context` and
//!     `event_msg/thread_settings_applied`: the current `model`.
//!   * Tools run through one `response_item/custom_tool_call` named `exec`
//!     whose `input` is a short JS script calling `tools.exec_command({cmd})`,
//!     `tools.apply_patch(patch)`, or `tools.write_stdin({session_id})` (a poll
//!     of a command still running). The script returns in a
//!     `custom_tool_call_output` with the same `call_id`.
//!   * A command's real end is `event_msg/item_completed` with a
//!     `CommandExecution` item (`command`, `exit_code`, ms times); a patch's is
//!     a `FileChange` item (`changes` keyed by path, `status`). Neither names
//!     the call, so commands are matched by their text and patches by path.
//!   * `token_usage_record`: one per model response (`response_id`), with
//!     `input_tokens` (cached included), `cached_input_tokens`,
//!     `cache_write_input_tokens` and `output_tokens` (reasoning included).
//!   * `event_msg/task_complete` ends a turn with `last_agent_message`;
//!     `event_msg/turn_aborted` (`reason: "interrupted"`) ends one early.
//!   * A `request_user_input` function call asks the user a question: the
//!     agent is waiting on them until its output comes back.
//!
//! Subagents (`collaboration.spawn_agent`) each get their own rollout in the
//! same folder. Its `session_meta` names `parent_thread_id`, `agent_path`
//! ("/root/repo_overview") and `agent_nickname` ("Socrates"), and its first
//! `subagent_history_start_ordinal` records are a copy of the parent's
//! history (skipped: not its work, and it would count the cost twice). A
//! subagent is the agent with its thread id, named by its nickname with its
//! path as the tag; `send_message` to another agent is a handoff.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde_json::Value;

use super::{is_test_command, iso_to_ms, now_ms, tail, SourceAdapter};
use crate::schema::{Body, Event, Harness, ToolKind, MAIN};

/// A tool started inside an `exec` script, waiting for its end.
#[derive(Clone, Debug, PartialEq)]
enum Op {
    /// a shell command, matched to its `CommandExecution` by text
    Cmd(String),
    /// a patch to one file, matched to its `FileChange` by path
    Patch(String),
    /// anything else: ends when the script returns
    Other,
}

/// One rollout file: the session's main thread, or one subagent's.
struct Thread {
    /// the main session's id (every event's envelope)
    session: String,
    /// the agent its events are about: [`MAIN`], or the subagent's thread id
    agent: String,
    path: PathBuf,
    offset: u64,
    /// records below this ordinal are the parent's history, copied into a
    /// subagent's file when it forks: not this agent's work
    skip_below: i64,
    /// latest model id
    model: Option<String>,
    /// the turn's approval policy and sandbox
    policy: Policy,
    /// call ids already seen
    seen: HashSet<String>,
    /// started, not yet finished: (event call id, the exec call id, op)
    open: Vec<(String, String, Op)>,
}

/// A subagent's rollout, from its `session_meta`.
#[derive(Clone, Debug, PartialEq)]
struct Child {
    id: String,
    parent: String,
    nickname: String,
    /// "/root/repo_overview"
    agent_path: String,
    start_ordinal: i64,
    ts: i64,
}

pub struct CodexAdapter {
    session: String,
    path: PathBuf,
    started: bool,
    /// main thread first, then subagents as they're found
    threads: Vec<Thread>,
    /// rollout files already looked at: their subagent header, if any
    seen_files: HashMap<PathBuf, Option<Child>>,
}

impl SourceAdapter for CodexAdapter {
    fn harness(&self) -> Harness {
        Harness::Codex
    }

    fn session_id(&self) -> &str {
        &self.session
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            let meta = head(&self.path);
            let cwd = meta.as_ref().and_then(|m| m.pointer("/payload/cwd")).and_then(Value::as_str);
            let ts = meta.as_ref().and_then(record_ts).unwrap_or_else(now_ms);
            let body = Body::SessionStarted { cwd: cwd.map(str::to_string), subagents: true };
            out.push(Event::new(Harness::Codex, &self.session, ts, body));
        }
        self.find_children(&mut out);
        for th in &mut self.threads {
            out.extend(th.poll());
        }
        // threads are read one after another: put them back in time order
        // (stable, so a spawn stays ahead of its agent's first record)
        out.sort_by_key(|e| e.ts);
        out
    }
}

impl CodexAdapter {
    pub fn open(path: &Path) -> Self {
        let session = session_id_of(path);
        CodexAdapter {
            threads: vec![Thread::open(path, &session, MAIN, 0)],
            session,
            path: path.to_path_buf(),
            started: false,
            seen_files: HashMap::new(),
        }
    }

    /// Subagents write their own rollouts beside the parent's (or in a later
    /// day's folder): spawn each one whose parent is in this session's tree.
    fn find_children(&mut self, out: &mut Vec<Event>) {
        let mut dirs: Vec<PathBuf> = self.path.parent().map(Path::to_path_buf).into_iter().collect();
        if let Some(root) = self.path.ancestors().nth(4) {
            let today = chrono::Local::now().format("%Y/%m/%d").to_string();
            let d = root.join(today);
            if !dirs.contains(&d) {
                dirs.push(d);
            }
        }
        let mut found = Vec::new();
        for d in dirs {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for e in rd.flatten() {
                let p = e.path();
                let rollout = p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-") && n.ends_with(".jsonl"));
                if !rollout || p == self.path || self.seen_files.contains_key(&p) {
                    continue;
                }
                // a file still being created has no header yet: look again later
                let Some(meta) = head(&p) else {
                    continue;
                };
                let child = child_of(&meta);
                self.seen_files.insert(p.clone(), child.clone());
                if let Some(c) = child {
                    found.push((p, c));
                }
            }
        }
        // parents before children (a grandchild's parent must be known first)
        found.sort_by_key(|(_, c)| c.ts);
        let mut pending = found;
        loop {
            let before = pending.len();
            pending.retain(|(p, c)| {
                let parent = if c.parent == self.session {
                    Some(MAIN.to_string())
                } else {
                    self.threads.iter().find(|t| t.agent == c.parent).map(|t| t.agent.clone())
                };
                let Some(parent) = parent else {
                    return true; // not ours (yet)
                };
                let role = c.agent_path.rsplit('/').next().unwrap_or("").to_string();
                let body = Body::AgentSpawned {
                    agent: c.id.clone(),
                    parent: Some(parent),
                    role: if c.nickname.is_empty() { role.clone() } else { c.nickname.to_lowercase() },
                    tag: if c.nickname.is_empty() { String::new() } else { role.clone() },
                    agent_type: String::new(),
                    task: role,
                    prompt: String::new(),
                    alias: c.agent_path.clone(),
                    ids: Vec::new(),
                };
                out.push(Event::new(Harness::Codex, &self.session, c.ts, body));
                self.threads.push(Thread::open(p, &self.session, &c.id, c.start_ordinal));
                false
            });
            if pending.is_empty() || pending.len() == before {
                break;
            }
        }
        // files whose parent isn't in this tree stay unclaimed; forget them
        // so a parent found later can still claim its children
        for (p, _) in pending {
            self.seen_files.remove(&p);
        }
    }
}

/// A rollout's subagent header (`source.subagent.thread_spawn`), if it is one.
fn child_of(meta: &Value) -> Option<Child> {
    let p = meta.get("payload")?;
    let spawn = p.pointer("/source/subagent/thread_spawn");
    let parent = p
        .get("parent_thread_id")
        .and_then(Value::as_str)
        .or_else(|| spawn.and_then(|s| s.get("parent_thread_id")).and_then(Value::as_str))?;
    let s = |k: &str| {
        p.get(k)
            .and_then(Value::as_str)
            .or_else(|| spawn.and_then(|s| s.get(k)).and_then(Value::as_str))
            .unwrap_or("")
            .to_string()
    };
    Some(Child {
        id: p.get("id").and_then(Value::as_str)?.to_string(),
        parent: parent.to_string(),
        nickname: s("agent_nickname"),
        agent_path: s("agent_path"),
        start_ordinal: p.get("subagent_history_start_ordinal").and_then(Value::as_i64).unwrap_or(0),
        ts: record_ts(meta).unwrap_or_else(now_ms),
    })
}

impl Thread {
    fn open(path: &Path, session: &str, agent: &str, skip_below: i64) -> Self {
        Thread {
            session: session.to_string(),
            agent: agent.to_string(),
            path: path.to_path_buf(),
            offset: 0,
            skip_below,
            model: None,
            policy: Policy::default(),
            seen: HashSet::new(),
            open: Vec::new(),
        }
    }

    /// New records → events about this thread's agent.
    fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        for line in tail(&self.path, &mut self.offset) {
            let Ok(r) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts) = record_ts(&r) else {
                continue;
            };
            // the parent's copied history: skip it (else its cost counts twice)
            if r.get("ordinal").and_then(Value::as_i64).is_some_and(|o| o < self.skip_below) {
                continue;
            }
            let before = out.len();
            self.record(&r, ts, &mut out);
            // nothing else said it was alive: a heartbeat does
            if out.len() == before {
                out.push(self.ev(ts, Body::Heartbeat { agent: MAIN.into() }));
            }
        }
        if self.agent != MAIN {
            for e in &mut out {
                e.set_agent(&self.agent);
            }
        }
        out
    }

    fn ev(&self, ts: i64, body: Body) -> Event {
        Event::new(Harness::Codex, &self.session, ts, body)
    }

    /// One rollout record → events.
    fn record(&mut self, r: &Value, ts: i64, out: &mut Vec<Event>) {
        let p = r.get("payload").unwrap_or(&Value::Null);
        let ptype = p.get("type").and_then(Value::as_str).unwrap_or("");
        let str_at = |v: &Value, ptr: &str| v.pointer(ptr).and_then(Value::as_str).map(str::to_string);
        match (r.get("type").and_then(Value::as_str).unwrap_or(""), ptype) {
            ("session_meta", _) => {
                if let Some(m) = str_at(p, "/base_instructions/provenance/model") {
                    self.model.get_or_insert(m);
                }
            }
            ("turn_context", _) => {
                if let Some(m) = str_at(p, "/model") {
                    self.model = Some(m);
                }
                if let Some(a) = str_at(p, "/approval_policy") {
                    self.policy.approval = Some(a);
                }
                if let Some(s) = str_at(p, "/sandbox_policy/type") {
                    self.policy.sandbox = Some(s);
                }
            }
            ("event_msg", "thread_settings_applied") => {
                if let Some(m) = str_at(p, "/thread_settings/model") {
                    self.model = Some(m);
                }
            }
            ("response_item", "custom_tool_call" | "function_call") => self.tool_call(p, ts, out),
            ("response_item", "custom_tool_call_output" | "function_call_output") => {
                let Some(call) = p.get("call_id").and_then(Value::as_str) else {
                    return;
                };
                // The script returned: its patches and other tools are done;
                // a command may still be running (its item ends it).
                let (done, keep): (Vec<_>, Vec<_>) = std::mem::take(&mut self.open)
                    .into_iter()
                    .partition(|(_, c, op)| c == call && !matches!(op, Op::Cmd(_)));
                self.open = keep;
                for (id, _, _) in done {
                    out.push(self.finished(ts, &id, false));
                }
            }
            ("response_item", "message") if p.get("role").and_then(Value::as_str) == Some("assistant") => {
                let text = p
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| {
                        parts
                            .iter()
                            .filter_map(|c| c.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    out.push(self.ev(ts, Body::Output { agent: MAIN.into(), text }));
                }
            }
            ("event_msg", "item_completed") => self.item(p, ts, out),
            ("token_usage_record", _) => {
                let n = |k: &str| p.pointer(&format!("/usage/{k}")).and_then(Value::as_u64).unwrap_or(0);
                let (cached, writes) = (n("cached_input_tokens"), n("cache_write_input_tokens"));
                out.push(self.ev(
                    ts,
                    Body::Usage {
                        agent: MAIN.into(),
                        id: str_at(p, "/response_id").unwrap_or_default(),
                        model: self.model.clone(),
                        input: n("input_tokens").saturating_sub(cached + writes),
                        output: n("output_tokens"),
                        cache_read: cached,
                        cache_write_5m: writes,
                        cache_write_1h: 0,
                    },
                ));
            }
            ("event_msg", "turn_aborted") => {
                // the user interrupted the turn (`reason: "interrupted"`): it's
                // over, and whatever was open went with it
                for (id, _, _) in std::mem::take(&mut self.open) {
                    out.push(self.finished(ts, &id, false));
                }
                out.push(self.ev(ts, Body::AgentFinished { agent: MAIN.into(), result: None, bytes: None, error: false }));
            }
            ("event_msg", "task_complete") => {
                // the turn is over: anything still open ended with it
                for (id, _, _) in std::mem::take(&mut self.open) {
                    out.push(self.finished(ts, &id, false));
                }
                let msg = str_at(p, "/last_agent_message");
                out.push(self.ev(
                    ts,
                    Body::AgentFinished {
                        agent: MAIN.into(),
                        bytes: msg.as_ref().map(|m| m.len() as u64),
                        result: msg,
                        error: false,
                    },
                ));
            }
            _ => {}
        }
    }

    /// A model tool call: an `exec` script (one event per tool it calls) or a
    /// plain function tool.
    fn tool_call(&mut self, p: &Value, ts: i64, out: &mut Vec<Event>) {
        let Some(call) = p.get("call_id").and_then(Value::as_str) else {
            return;
        };
        if !self.seen.insert(call.to_string()) {
            return;
        }
        let name = p.get("name").and_then(Value::as_str).unwrap_or("tool");
        let ops: Vec<(Body, Op)> = if name == "exec" {
            let input = p.get("input").and_then(Value::as_str).unwrap_or("");
            script_ops(input, &self.policy)
        } else {
            vec![(tool_body(name, name, ToolKind::Run, ""), Op::Other)]
        };
        // a message to another agent ("/root", "/root/repo_overview")
        if name == "send_message" {
            let args: Value = p
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|a| serde_json::from_str(a).ok())
                .unwrap_or(Value::Null);
            if let Some(to) = args.get("target").and_then(Value::as_str) {
                let bytes = args.get("message").and_then(Value::as_str).map_or(0, str::len) as u64;
                let body = Body::Handoff { agent: MAIN.into(), to: to.to_string(), what: "message".into(), bytes };
                out.push(self.ev(ts, body));
            }
        }
        for (i, (mut body, op)) in ops.into_iter().enumerate() {
            let id = if i == 0 { call.to_string() } else { format!("{call}#{i}") };
            if let Body::ToolStarted { call: c, .. } = &mut body {
                *c = Some(id.clone());
            }
            out.push(self.ev(ts, body));
            self.open.push((id, call.to_string(), op));
        }
        // A question for the user: blocked on them until it returns. (The tool
        // is offered in 0.170.0's tool list; a call to it is not yet observed.)
        if name.ends_with("request_user_input") {
            let body = Body::Waiting { agent: MAIN.into(), call: Some(call.to_string()), reason: "question".into() };
            out.push(self.ev(ts, body));
        }
    }

    /// A completed item: a command's exit code, or a patch landing.
    fn item(&mut self, p: &Value, ts: i64, out: &mut Vec<Event>) {
        let item = p.get("item").unwrap_or(&Value::Null);
        let end = p.get("completed_at_ms").and_then(Value::as_i64).unwrap_or(ts);
        match item.get("type").and_then(Value::as_str) {
            // what the user asked: the one agent's task
            Some("UserMessage") => {
                let text = item
                    .get("content")
                    .and_then(Value::as_array)
                    .map(|parts| parts.iter().filter_map(|c| c.get("text").and_then(Value::as_str)).collect::<Vec<_>>().join("\n"))
                    .unwrap_or_default();
                if !text.trim().is_empty() {
                    let body = Body::AgentSpawned {
                        agent: MAIN.into(),
                        parent: None,
                        role: String::new(),
                        tag: String::new(),
                        agent_type: String::new(),
                        task: String::new(),
                        prompt: text,
                        alias: String::new(),
                        ids: Vec::new(),
                    };
                    out.push(self.ev(ts, body));
                }
            }
            Some("CommandExecution") => {
                // ["/bin/zsh", "-lc", "<cmd>"]: the script's cmd is the last part
                let cmd = item
                    .get("command")
                    .and_then(Value::as_array)
                    .and_then(|c| c.last())
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let Some(i) = self.open.iter().position(|(_, _, op)| *op == Op::Cmd(cmd.to_string())) else {
                    return;
                };
                let (id, _, _) = self.open.remove(i);
                let code = item.get("exit_code").and_then(Value::as_i64).unwrap_or(0);
                out.push(self.finished(end, &id, code != 0));
            }
            Some("FileChange") => {
                let ok = item.get("status").and_then(Value::as_str) == Some("completed");
                let paths: Vec<String> =
                    item.get("changes").and_then(Value::as_object).map(|m| m.keys().cloned().collect()).unwrap_or_default();
                for path in paths {
                    if let Some(i) = self.open.iter().position(|(_, _, op)| *op == Op::Patch(path.clone())) {
                        let (id, _, _) = self.open.remove(i);
                        out.push(self.finished(end, &id, !ok));
                    }
                }
            }
            _ => {}
        }
    }

    fn finished(&self, ts: i64, call: &str, error: bool) -> Event {
        self.ev(ts, Body::ToolFinished { agent: MAIN.into(), call: Some(call.to_string()), error, denied: false })
    }
}

/// The tools an `exec` script calls, in order. Polls of a running command
/// (`write_stdin`) aren't new tools; a script calling nothing we know is one
/// generic `exec` call.
fn script_ops(js: &str, policy: &Policy) -> Vec<(Body, Op)> {
    let mut ops = Vec::new();
    let mut patched = false;
    let mut rest = js;
    while let Some(i) = rest.find("tools.") {
        rest = &rest[i + 6..];
        let fname: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let args = &rest[fname.len()..];
        // this call's own arguments: up to the next tool call
        let own = &args[..args.find("tools.").unwrap_or(args.len())];
        match fname.as_str() {
            "exec_command" => {
                let cmd = js_string_field(args, "cmd").unwrap_or_default();
                let escalated = js_string_field(own, "sandbox_permissions").as_deref() == Some("require_escalated");
                let mut body = command_body(&cmd);
                if let Body::ToolStarted { blocking, .. } = &mut body {
                    *blocking = *blocking && policy.asks(escalated, false);
                }
                ops.push((body, Op::Cmd(cmd)));
            }
            "apply_patch" if !patched => {
                // the patch text is usually a `const` earlier in the script,
                // so take every file header in it, once
                patched = true;
                for (verb, file) in patch_files(js) {
                    let tool = if verb == "Add" { "write" } else { "edit" };
                    let name = file.rsplit('/').next().unwrap_or(&file).to_string();
                    let body = Body::ToolStarted {
                        agent: MAIN.into(),
                        call: None,
                        tool: tool.into(),
                        label: tool.into(),
                        kind: ToolKind::Run,
                        file: Some(file.clone()),
                        edit: true,
                        arg: name,
                        blocking: policy.asks(false, true),
                        test: false,
                    };
                    ops.push((body, Op::Patch(file)));
                }
            }
            "write_stdin" | "apply_patch" | "" => {}
            other => ops.push((tool_body(other, other, ToolKind::Run, ""), Op::Other)),
        }
    }
    if ops.is_empty() && !js.contains("tools.write_stdin") {
        ops.push((tool_body("exec", "exec", ToolKind::Run, ""), Op::Other));
    }
    ops
}

/// When Codex asks before running something, from the turn's
/// `approval_policy` and `sandbox_policy` (`turn_context`).
#[derive(Clone, Debug, Default, PartialEq)]
struct Policy {
    /// "on-request", "never", "untrusted", "on-failure"
    approval: Option<String>,
    /// "workspace-write", "read-only", "danger-full-access"
    sandbox: Option<String>,
}

impl Policy {
    /// Can a call sit at an approval prompt? `never` never asks.
    /// `on-request` runs commands in the sandbox unasked unless they request
    /// `sandbox_permissions: "require_escalated"`, and asks before a patch
    /// only in a read-only sandbox. Anything else (or not recorded): any can.
    fn asks(&self, escalated: bool, patch: bool) -> bool {
        match self.approval.as_deref() {
            Some("never") => false,
            Some("on-request") if patch => self.sandbox.as_deref() == Some("read-only"),
            Some("on-request") => escalated,
            _ => true,
        }
    }
}

fn tool_body(tool: &str, label: &str, kind: ToolKind, arg: &str) -> Body {
    Body::ToolStarted {
        agent: MAIN.into(),
        call: None,
        tool: tool.to_lowercase(),
        label: label.to_lowercase(),
        kind,
        file: None,
        edit: false,
        arg: arg.to_string(),
        blocking: false,
        test: false,
    }
}

/// Commands that only look: a command made of nothing else is a read
/// (or a search / listing), not a run.
const LOOKS: &[&str] = &["cat", "sed", "head", "tail", "nl", "less", "wc", "rg", "grep", "ls", "find", "pwd", "tree"];

/// A shell command as a tool: reads and searches look; anything else runs,
/// labelled by its first non-reading command (`sed … && cargo test` → cargo).
fn command_body(cmd: &str) -> Body {
    let segs: Vec<&str> = cmd
        .split(['|', ';', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let word = |s: &str| {
        let w = s.split_whitespace().next().unwrap_or("");
        w.rsplit('/').next().unwrap_or(w).to_string()
    };
    let doer = segs.iter().find(|s| !LOOKS.contains(&word(s).as_str()));
    let test = is_test_command(cmd);
    match doer {
        None if !segs.is_empty() => {
            let first = word(segs[0]);
            let tool = match first.as_str() {
                "rg" | "grep" => "grep",
                "ls" | "find" | "tree" | "pwd" => "ls",
                _ => "read",
            };
            // a lone read of one file: its last plain argument
            let file = (segs.len() == 1 && tool == "read")
                .then(|| segs[0].split_whitespace().skip(1).filter(|a| !a.starts_with('-') && !a.contains('\'')).last())
                .flatten()
                .map(str::to_string);
            let arg = file.as_deref().map_or_else(|| short(segs[0]), |f| f.rsplit('/').next().unwrap_or(f).to_string());
            Body::ToolStarted {
                agent: MAIN.into(),
                call: None,
                tool: tool.into(),
                label: tool.into(),
                kind: ToolKind::Look,
                file,
                edit: false,
                arg,
                blocking: false,
                test,
            }
        }
        _ => {
            let seg = doer.copied().unwrap_or(cmd);
            let w = word(seg);
            Body::ToolStarted {
                agent: MAIN.into(),
                call: None,
                tool: "bash".into(),
                label: if !w.is_empty() && w.len() <= 10 { w } else { "bash".into() },
                kind: ToolKind::Run,
                file: None,
                edit: false,
                arg: short(seg),
                blocking: true,
                test,
            }
        }
    }
}

/// The first three words of a command.
fn short(s: &str) -> String {
    s.split_whitespace().take(3).collect::<Vec<_>>().join(" ")
}

/// A string field in a JS object literal, `cmd:"…"` or `"cmd":"…"`: the
/// double-quoted literal after it, unescaped (JSON string rules).
fn js_string_field(js: &str, key: &str) -> Option<String> {
    let mut rest = js;
    loop {
        let i = rest.find(key)?;
        let after = rest[i + key.len()..].trim_start_matches('"').trim_start();
        rest = &rest[i + key.len()..];
        let Some(after) = after.strip_prefix(':') else {
            continue;
        };
        let lit = after.trim_start();
        if !lit.starts_with('"') {
            return None;
        }
        // find the closing quote, skipping escapes
        let bytes = lit.as_bytes();
        let mut j = 1;
        while j < bytes.len() {
            match bytes[j] {
                b'\\' => j += 2,
                b'"' => return serde_json::from_str(&lit[..=j]).ok(),
                _ => j += 1,
            }
        }
        return None;
    }
}

/// `*** Update File: <path>` / `Add File` / `Delete File` headers in a patch
/// (inside a JS string, so lines end in a literal `\n`).
fn patch_files(s: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for verb in ["Update", "Add", "Delete"] {
        let tag = format!("*** {verb} File: ");
        let mut rest = s;
        while let Some(i) = rest.find(&tag) {
            rest = &rest[i + tag.len()..];
            let end = ["\\n", "\n", "\""].iter().filter_map(|t| rest.find(t)).min().unwrap_or(rest.len());
            let path = rest[..end].trim();
            if !path.is_empty() {
                out.push((verb, path.to_string()));
            }
        }
    }
    out
}

/// A record's time: its `timestamp` (ISO UTC).
fn record_ts(r: &Value) -> Option<i64> {
    r.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms)
}

/// The first record (`session_meta`).
fn head(path: &Path) -> Option<Value> {
    let f = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    serde_json::from_str(line.trim_end()).ok()
}

/// "rollout-2026-11-19T11-06-39-<uuid>.jsonl" → "<uuid>".
fn session_id_of(path: &Path) -> String {
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    match stem.len().checked_sub(36) {
        Some(i) if stem.is_char_boundary(i) && stem[i..].matches('-').count() == 4 => stem[i..].to_string(),
        _ => stem,
    }
}

// ---- discovery ---------------------------------------------------------------

/// `$CODEX_HOME/sessions`, `~/.codex/sessions` by default.
pub fn sessions_root() -> PathBuf {
    let home = std::env::var("CODEX_HOME").map(PathBuf::from).unwrap_or_else(|_| {
        PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| ".".into())).join(".codex")
    });
    home.join("sessions")
}

/// Every rollout under the sessions root, with its last-modified time.
fn rollouts(root: &Path) -> Vec<(PathBuf, SystemTime)> {
    let mut out = Vec::new();
    let mut dirs = vec![root.to_path_buf()];
    while let Some(d) = dirs.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                dirs.push(p);
            } else if p.extension().and_then(|x| x.to_str()) == Some("jsonl")
                && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with("rollout-"))
            {
                let m = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
                out.push((p, m));
            }
        }
    }
    out
}

/// The session to watch: `id` anywhere; else the most recently active
/// top-level session that ran in `project`; else (when `fallback`) the most
/// recently active one anywhere. A session's activity includes its
/// subagents' files (a parent sits quiet while they work), and subagent
/// rollouts are never picked on their own.
pub fn find(root: &Path, id: Option<&str>, project: Option<&Path>, fallback: bool) -> Option<PathBuf> {
    let mut all = rollouts(root);
    all.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    if let Some(id) = id {
        return all.into_iter().map(|(p, _)| p).find(|p| session_id_of(p) == id);
    }
    if project.is_none() && !fallback {
        return None;
    }
    let want = project.map(|d| d.to_string_lossy().trim_end_matches('/').to_string());
    // top-level session id -> (activity, its rollout once seen)
    let mut tops: HashMap<String, (SystemTime, Option<PathBuf>)> = HashMap::new();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    // newest first: stop once the most active session is known and nothing
    // older can beat it
    for (p, m) in all {
        let lead = tops.values().max_by_key(|(a, _)| *a);
        if lead.is_some_and(|(a, path)| path.is_some() && *a > m) {
            break;
        }
        let Some(meta) = head(&p) else {
            continue;
        };
        let cwd = meta.pointer("/payload/cwd").and_then(Value::as_str).map(|c| c.trim_end_matches('/'));
        if want.as_deref().is_some_and(|w| cwd != Some(w)) {
            continue;
        }
        let (top, path) = match child_of(&meta) {
            Some(c) => {
                parent_of.insert(c.id, c.parent.clone());
                let mut top = c.parent;
                while let Some(up) = parent_of.get(&top) {
                    top = up.clone();
                }
                (top, None)
            }
            None => (session_id_of(&p), Some(p)),
        };
        let e = tops.entry(top).or_insert((m, None));
        e.0 = e.0.max(m);
        if path.is_some() {
            e.1 = path;
        }
    }
    let best = tops.into_values().filter_map(|(a, p)| p.map(|p| (a, p))).max_by_key(|(a, _)| *a).map(|(_, p)| p);
    match best {
        // nothing ran in the project: the most recent session anywhere
        None if fallback && project.is_some() => find(root, None, None, true),
        found => found,
    }
}

/// Every top-level session, in any project, that showed signs of life at or
/// after `since` (its own rollout or a subagent's), with that activity. A
/// subagent's write makes its top-level session active even when the
/// session's own rollout is older.
pub fn active(root: &Path, since: SystemTime) -> Vec<(PathBuf, SystemTime)> {
    let all = rollouts(root);
    let by_id: HashMap<String, &PathBuf> = all.iter().map(|(p, _)| (session_id_of(p), p)).collect();
    let mut parent_of: HashMap<String, String> = HashMap::new();
    let mut fresh: Vec<(PathBuf, SystemTime)> = all.iter().filter(|(_, m)| *m >= since).cloned().collect();
    // parents before children, so a chain resolves in one pass
    fresh.sort_by_key(|(_, m)| *m);
    let mut tops: HashMap<String, SystemTime> = HashMap::new();
    for (p, m) in &fresh {
        let Some(meta) = head(p) else {
            continue;
        };
        let top = match child_of(&meta) {
            Some(c) => {
                parent_of.insert(c.id, c.parent.clone());
                let mut top = c.parent;
                // a parent we haven't read yet: its header names its own parent
                let mut hops = 0;
                loop {
                    if let Some(up) = parent_of.get(&top) {
                        top = up.clone();
                    } else if let Some(c) = by_id.get(&top).and_then(|pp| head(pp)).as_ref().and_then(child_of) {
                        parent_of.insert(c.id, c.parent.clone());
                        top = c.parent;
                    } else {
                        break;
                    }
                    hops += 1;
                    if hops > 16 {
                        break;
                    }
                }
                top
            }
            None => session_id_of(p),
        };
        let e = tops.entry(top).or_insert(*m);
        *e = (*e).max(*m);
    }
    tops.into_iter().filter_map(|(id, m)| by_id.get(&id).map(|p| ((*p).clone(), m))).collect()
}

/// The `cwd` a rollout's `session_meta` names.
pub fn read_cwd(path: &Path) -> Option<PathBuf> {
    head(path)?.pointer("/payload/cwd").and_then(Value::as_str).map(PathBuf::from)
}

/// Every top-level session that ran in `project`, with when it last showed
/// signs of life (its subagents' rollouts, at any depth, included). For
/// indexing finished runs (`agentflow profile`).
pub fn sessions(root: &Path, project: &Path) -> Vec<(PathBuf, SystemTime)> {
    let want = project.to_string_lossy().trim_end_matches('/').to_string();
    // thread id -> (parent id for a subagent, rollout, mtime, ran in project)
    let mut threads: HashMap<String, (Option<String>, PathBuf, SystemTime, bool)> = HashMap::new();
    for (p, m) in rollouts(root) {
        let Some(meta) = head(&p) else {
            continue;
        };
        let here = meta.pointer("/payload/cwd").and_then(Value::as_str).map(|c| c.trim_end_matches('/')) == Some(&want);
        match child_of(&meta) {
            Some(c) => threads.insert(c.id, (Some(c.parent), p, m, here)),
            None => threads.insert(session_id_of(&p), (None, p, m, here)),
        };
    }
    let top = |id: &str| {
        let mut id = id.to_string();
        for _ in 0..64 {
            match threads.get(&id) {
                Some((Some(parent), ..)) => id = parent.clone(),
                _ => break,
            }
        }
        id
    };
    let mut newest: HashMap<String, SystemTime> = HashMap::new();
    for (id, (_, _, m, _)) in &threads {
        let e = newest.entry(top(id)).or_insert(*m);
        *e = (*e).max(*m);
    }
    threads
        .iter()
        .filter(|(_, (parent, _, _, here))| parent.is_none() && *here)
        .map(|(id, (_, p, m, _))| (p.clone(), newest.get(id).copied().unwrap_or(*m)))
        .collect()
}

/// When a session last showed signs of life: its rollout or any of its
/// subagents' (at any depth).
pub fn activity(root: &Path, path: &Path) -> SystemTime {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
    let own = mtime(path);
    let mut ids: HashSet<String> = HashSet::from([session_id_of(path)]);
    let mut newer: Vec<(PathBuf, SystemTime)> = rollouts(root).into_iter().filter(|(_, m)| *m > own).collect();
    newer.sort_by_key(|(_, m)| *m);
    let mut best = own;
    for (p, m) in newer {
        if let Some(c) = head(&p).as_ref().and_then(child_of) {
            if ids.contains(&c.parent) {
                ids.insert(c.id);
                best = best.max(m);
            }
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::live::Live;
    use crate::pricing::Pricing;

    /// Synthetic, but structured like a real codex-cli 0.170.0 rollout:
    /// a read + test run in one script (the command outlives the script and
    /// is polled with write_stdin), a lone read, a patch, then the turn ends.
    const FIXTURE: &str = include_str!("../../tests/fixtures/codex/rollout-2026-01-01T00-00-00-0000aaaa-1111-2222-3333-444455556666.jsonl");

    fn write_fixture(dir: &Path) -> PathBuf {
        let day = dir.join("2026").join("01").join("01");
        std::fs::create_dir_all(&day).unwrap();
        let p = day.join("rollout-2026-01-01T00-00-00-0000aaaa-1111-2222-3333-444455556666.jsonl");
        std::fs::write(&p, FIXTURE).unwrap();
        p
    }

    #[test]
    fn events_from_a_rollout() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-ev-{}", std::process::id()));
        let path = write_fixture(&dir);
        let evs = CodexAdapter::open(&path).poll();
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(evs[0].session, "0000aaaa-1111-2222-3333-444455556666");
        assert!(matches!(&evs[0].body, Body::SessionStarted { cwd: Some(c), subagents: true } if c == "/w/proj"));
        let tools: Vec<(String, String, bool, Option<String>)> = evs
            .iter()
            .filter_map(|e| match &e.body {
                Body::ToolStarted { tool, label, test, file, .. } => Some((tool.clone(), label.clone(), *test, file.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(
            tools,
            [
                ("bash".into(), "cargo".into(), true, None),
                ("read".into(), "read".into(), false, Some("README.md".into())),
                ("edit".into(), "edit".into(), false, Some("/w/proj/README.md".into())),
            ],
            "write_stdin polls are not tools"
        );
        // the test command ends at its CommandExecution item, failed (exit 101)
        let fin: Vec<(&str, bool)> = evs
            .iter()
            .filter_map(|e| match &e.body {
                Body::ToolFinished { call: Some(c), error, .. } => Some((c.as_str(), *error)),
                _ => None,
            })
            .collect();
        assert_eq!(fin, [("call_1", true), ("call_3", false), ("call_4", false)]);
        let usage: Vec<(u64, u64, u64)> = evs
            .iter()
            .filter_map(|e| match &e.body {
                Body::Usage { input, output, cache_read, model, .. } => {
                    assert_eq!(model.as_deref(), Some("gpt-6-luna"));
                    Some((*input, *output, *cache_read))
                }
                _ => None,
            })
            .collect();
        assert_eq!(usage, [(1000, 100, 0), (200, 50, 1000)], "cached input is split out of input");
        assert!(matches!(&evs.last().unwrap().body, Body::AgentFinished { result: Some(r), .. } if r == "Done."));
    }

    #[test]
    fn one_agent_session_renders() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-live-{}", std::process::id()));
        let path = write_fixture(&dir);
        let mut live = Live::new(Box::new(CodexAdapter::open(&path)) as Box<dyn SourceAdapter>, "h".into());
        live.refresh();
        let s = live.build(&Pricing::default());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(s.agents.len(), 1);
        let a = &s.agents[0];
        assert_eq!((a.name.as_str(), a.model.as_str()), ("codex", "luna"));
        assert_eq!(a.tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(), ["cargo", "read", "edit"]);
        assert_eq!(a.uses.iter().filter(|u| u.edit).count(), 1);
        assert_eq!(a.uses[2].file.as_deref(), Some("README.md"), "shown relative to the project");
        assert_eq!(a.tests.iter().map(|t| t.passed).collect::<Vec<_>>(), [false]);
        assert!(a.finish.is_some() && a.result.as_deref() == Some("Done."));
        assert_eq!(a.task, "Read src/main.rs, run the tests, then add a comment to README.md.", "the user's prompt");
        assert!(s.orch_calls.is_empty(), "the one agent's calls are its own");
        // 1000 in + 100 out, then 200 in + 1000 cached + 50 out, at luna rates
        let want = (1000.0 * 0.10 + 100.0 * 0.50 + 200.0 * 0.10 + 1000.0 * 0.01 + 50.0 * 0.50) / 1e6;
        assert!((s.cost_at(i64::MAX) - want).abs() < 1e-12);
        assert!(s.unpriced.is_empty());
    }

    /// A question for the user waits on them until its output lands; an
    /// interrupted turn (`turn_aborted`) is over, its open command with it.
    #[test]
    fn questions_and_interrupted_turns() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-abort-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("rollout-2026-01-01T00-00-00-0000ffff-1111-2222-3333-444455556666.jsonl");
        let lines = [
            r#"{"timestamp":"2026-01-01T00:00:00.000Z","ordinal":0,"type":"session_meta","payload":{"id":"0000ffff-1111-2222-3333-444455556666","cwd":"/w/proj"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:01.000Z","ordinal":1,"type":"response_item","payload":{"type":"function_call","call_id":"u1","name":"request_user_input","arguments":"{}"}}"#,
        ];
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut live = Live::new(Box::new(CodexAdapter::open(&path)) as Box<dyn SourceAdapter>, "h".into());
        live.refresh();
        let now = iso_to_ms("2026-01-01T00:05:00.000Z").unwrap();
        let p = live.pulse(&Pricing::default(), now);
        assert_eq!(p.waiting, Some((iso_to_ms("2026-01-01T00:00:01.000Z").unwrap(), "question".into())));

        let more = [
            r#"{"timestamp":"2026-01-01T00:00:30.000Z","ordinal":2,"type":"response_item","payload":{"type":"function_call_output","call_id":"u1","output":"staging"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:31.000Z","ordinal":3,"type":"response_item","payload":{"type":"custom_tool_call","call_id":"c1","name":"exec","input":"const r = await tools.exec_command({cmd:\"cargo build\"});"}}"#,
            r#"{"timestamp":"2026-01-01T00:00:40.000Z","ordinal":4,"type":"event_msg","payload":{"type":"turn_aborted","reason":"interrupted"}}"#,
        ];
        std::fs::write(&path, std::fs::read_to_string(&path).unwrap() + &more.join("\n") + "\n").unwrap();
        live.refresh();
        let p = live.pulse(&Pricing::default(), now);
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(p.waiting, None);
        assert!(!p.running && !p.in_flight, "the turn is over and nothing is left open");
        assert_eq!(p.done, Some(iso_to_ms("2026-01-01T00:00:40.000Z").unwrap()));
    }

    /// `on-request` (the default) runs sandboxed commands unasked: only one
    /// that requests escalation can sit at a prompt, and a patch only in a
    /// read-only sandbox. `never` never asks; `untrusted` can ask for any.
    #[test]
    fn approval_policy_decides_what_can_wait() {
        let blocking = |js: &str, approval: &str, sandbox: &str| -> Vec<bool> {
            let policy = Policy { approval: Some(approval.into()), sandbox: Some(sandbox.into()) };
            script_ops(js, &policy)
                .into_iter()
                .filter_map(|(b, _)| match b {
                    Body::ToolStarted { blocking, .. } => Some(blocking),
                    _ => None,
                })
                .collect()
        };
        let js = r#"await tools.exec_command({cmd:"cargo build"}); await tools.exec_command({cmd:"git push", sandbox_permissions:"require_escalated", justification:"push"});"#;
        assert_eq!(blocking(js, "on-request", "workspace-write"), [false, true]);
        assert_eq!(blocking(js, "never", "workspace-write"), [false, false]);
        assert_eq!(blocking(js, "untrusted", "workspace-write"), [true, true]);
        let patch = "const p = `*** Begin Patch\n*** Update File: src/lib.rs\n@@\n-a\n+b\n*** End Patch`;\nawait tools.apply_patch(p);";
        assert_eq!(blocking(patch, "on-request", "workspace-write"), [false]);
        assert_eq!(blocking(patch, "on-request", "read-only"), [true]);
        assert!(Policy::default().asks(false, false), "not recorded: any call can ask");
    }

    #[test]
    fn command_classification() {
        let tool = |cmd: &str| match command_body(cmd) {
            Body::ToolStarted { tool, label, kind, file, .. } => (tool, label, kind, file),
            _ => unreachable!(),
        };
        assert_eq!(tool("sed -n '1,8p' README.md"), ("read".into(), "read".into(), ToolKind::Look, Some("README.md".into())));
        assert_eq!(tool("rg -n parse src"), ("grep".into(), "grep".into(), ToolKind::Look, None));
        assert_eq!(tool("sed -n '1,240p' src/main.rs && cargo test").1, "cargo");
        assert_eq!(tool("/usr/bin/make -j8").1, "make");
        assert_eq!(js_string_field(r#"tools.exec_command({cmd:"echo \"hi\" && ls","workdir":"/w"})"#, "cmd").as_deref(), Some(r#"echo "hi" && ls"#));
        assert_eq!(js_string_field(r#"{"cmd": "ls"}"#, "cmd").as_deref(), Some("ls"));
        assert_eq!(session_id_of(Path::new("rollout-2026-11-19T11-06-39-01a1f48f-60ec-7ca2-9b80-b01e148932ec.jsonl")), "01a1f48f-60ec-7ca2-9b80-b01e148932ec");
    }

    const PARENT: &str = include_str!("../../tests/fixtures/codex/subagents/rollout-2026-01-02T00-00-00-1111aaaa-0000-4000-8000-000000000001.jsonl");
    const CHILD: &str = include_str!("../../tests/fixtures/codex/subagents/rollout-2026-01-02T00-00-01-1111aaaa-0000-4000-8000-000000000002.jsonl");
    const CHILD_ID: &str = "1111aaaa-0000-4000-8000-000000000002";

    /// A parent rollout and one subagent's, in a day folder under `root`.
    fn write_team(root: &Path) -> (PathBuf, PathBuf) {
        let day = root.join("2026").join("01").join("02");
        std::fs::create_dir_all(&day).unwrap();
        let parent = day.join("rollout-2026-01-02T00-00-00-1111aaaa-0000-4000-8000-000000000001.jsonl");
        let child = day.join(format!("rollout-2026-01-02T00-00-01-{CHILD_ID}.jsonl"));
        std::fs::write(&parent, PARENT).unwrap();
        std::fs::write(&child, CHILD).unwrap();
        (parent, child)
    }

    /// A subagent's rollout opens with a copy of its parent's history (here:
    /// the user's prompt, the spawn call and a 2000-token response). None of
    /// it is the subagent's work, so none of it becomes its events.
    #[test]
    fn subagent_history_is_skipped() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-team-{}", std::process::id()));
        let (parent, _) = write_team(&dir);
        let evs = CodexAdapter::open(&parent).poll();
        let _ = std::fs::remove_dir_all(&dir);

        let child: Vec<&Event> = evs.iter().filter(|e| e.agent() == Some(CHILD_ID)).collect();
        let usage: Vec<&str> = child
            .iter()
            .filter_map(|e| match &e.body {
                Body::Usage { id, .. } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(usage, ["resp_c1", "resp_c2"], "the parent's resp_p1 copy is not the child's");
        let tools: Vec<&str> = child
            .iter()
            .filter_map(|e| match &e.body {
                Body::ToolStarted { tool, .. } => Some(tool.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tools, ["grep", "send_message"], "no copied spawn_agent call");
        assert!(
            !child.iter().any(|e| matches!(&e.body, Body::AgentSpawned { prompt, .. } if !prompt.is_empty())),
            "the parent's prompt is not the child's task"
        );
        // spawned once, named by its nickname with its path as the tag
        let spawn = child.iter().find(|e| matches!(e.body, Body::AgentSpawned { .. })).unwrap();
        assert!(matches!(&spawn.body, Body::AgentSpawned { role, tag, parent: Some(p), alias, .. }
            if role == "hypatia" && tag == "scan" && p == MAIN && alias == "/root/scan"));
        assert!(child.iter().any(|e| matches!(&e.body, Body::Handoff { to, .. } if to == "/root")));
        // one ordered stream: the spawn comes before the child's own records
        assert!(evs.windows(2).all(|w| w[0].ts <= w[1].ts));
    }

    #[test]
    fn team_session_renders_once_per_token() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-team-live-{}", std::process::id()));
        let (parent, _) = write_team(&dir);
        let mut live = Live::new(Box::new(CodexAdapter::open(&parent)) as Box<dyn SourceAdapter>, "h".into());
        live.refresh();
        let s = live.build(&Pricing::default());
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!s.solo, "a subagent spawned: main is the orchestrator");
        assert_eq!(s.orch_model, "luna");
        assert_eq!(s.orch_calls.len(), 1);
        assert_eq!(s.agents.len(), 1);
        let a = &s.agents[0];
        assert_eq!((a.name.as_str(), a.tag.as_str(), a.task.as_str()), ("hypatia", "scan", "scan"));
        assert_eq!(a.calls.len(), 2);
        assert_eq!(a.result.as_deref(), Some("Found 3 files."));
        // parent 2000 in + 50 out; child 1000 in + 20 out, then 300 in + 1000 cached + 30 out
        let want = (2000.0 * 0.10 + 50.0 * 0.50 + 1000.0 * 0.10 + 20.0 * 0.50 + 300.0 * 0.10 + 1000.0 * 0.01 + 30.0 * 0.50) / 1e6;
        assert!((s.cost_at(i64::MAX) - want).abs() < 1e-12, "{} vs {want}", s.cost_at(i64::MAX));
    }

    /// Discovery picks the parent, never a subagent's rollout on its own, and
    /// counts the subagent's writes as the parent's activity.
    #[test]
    fn discovery_skips_subagent_rollouts() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-team-find-{}", std::process::id()));
        let (parent, child) = write_team(&dir);
        let t = |secs: u64| SystemTime::now() - std::time::Duration::from_secs(secs);
        File::options().write(true).open(&parent).unwrap().set_modified(t(600)).unwrap();
        File::options().write(true).open(&child).unwrap().set_modified(t(5)).unwrap();
        assert_eq!(find(&dir, None, Some(Path::new("/w/team")), false), Some(parent.clone()));
        assert_eq!(find(&dir, None, None, true), Some(parent.clone()));
        let act = activity(&dir, &parent);
        assert!(act > t(60), "the busy subagent keeps the parent active");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discovery_by_project_and_id() {
        let dir = std::env::temp_dir().join(format!("aflow-codex-find-{}", std::process::id()));
        let path = write_fixture(&dir);
        assert_eq!(find(&dir, None, Some(Path::new("/w/proj/")), false), Some(path.clone()));
        assert_eq!(find(&dir, None, Some(Path::new("/elsewhere")), false), None);
        assert_eq!(find(&dir, None, Some(Path::new("/elsewhere")), true), Some(path.clone()));
        assert_eq!(find(&dir, Some("0000aaaa-1111-2222-3333-444455556666"), None, false), Some(path));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
