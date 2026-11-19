//! Live mode: fold a source adapter's schema events into the render `Session`.
//!
//! This is the only place events become the render model, and it reads
//! nothing but schema events: no harness formats. Timestamps are epoch ms;
//! the render clock is `now - t0`, where `t0` is the earliest spawn.
//!
//! Handoffs (orbit chords) come from two places: a finished agent's result
//! quoted in a later agent's prompt (`orbit::detect_handoffs`), and explicit
//! `handoff` events (agent-team messages) resolved against agent ids,
//! aliases, roles and types.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::adapters::{now_ms, SourceAdapter};
use crate::model::{Agent, Call, Event, Handoff, Session, TestRun, Tool, ToolKind, Use, Usage};
use crate::pricing::{self, Pricing};
use crate::schema::{self, Body, Harness, MAIN};
use crate::theme;

/// A tool call that hasn't returned this long reads as "waiting". We can't see
/// *why* it's blocked (permission prompt vs. slow command), only that it is.
const WAIT_MS: i64 = 10_000;

/// Per-agent accumulator, updated incrementally as events arrive.
#[derive(Default)]
struct Acc {
    role: String,
    /// dim tag after the name
    tag: String,
    agent_type: String,
    task: String,
    prompt: String,
    /// name other agents address it by, for message routing
    alias: String,
    /// other ids it's known by
    ids: Vec<String>,
    spawn_ms: Option<i64>,
    /// its own first / latest activity
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    done_ts: Option<i64>,
    finished: bool,
    /// its latest outcome was a denied/blocked tool call or an error
    denied: bool,
    /// when the most recent error happened (drives the error glitch)
    error_ts: Option<i64>,
    size: String,
    tools: Vec<(i64, Tool)>,
    /// outstanding tool calls with no result yet: (call id, ts, blocking)
    pending: Vec<(String, i64, bool)>,
    /// an explicit `waiting` event's time, until its next tool or finish
    waiting: Option<i64>,
    order: usize,
    /// latest model id seen
    model: String,
    usage: Calls,
    uses: Vec<Use>,
    /// call ids of tool calls that ran a test suite
    test_ids: HashSet<String>,
    tests: Vec<TestRun>,
    /// final answer
    result: Option<String>,
    /// latest output text, the fallback result preview
    last_text: String,
    /// direct messages it sent: (ts, recipient, bytes)
    messages: Vec<(i64, String, u64)>,
}

/// call id -> (first seen ms, usage, model)
type Calls = HashMap<String, (i64, Usage, String)>;

pub struct Live {
    src: Box<dyn SourceAdapter>,
    header: String,
    /// from `session_started`
    session: Option<String>,
    cwd: Option<String>,
    harness: Option<Harness>,
    /// keyed by agent id; [`MAIN`] holds the orchestrator's calls
    accs: HashMap<String, Acc>,
    next_order: usize,
    /// when we started watching (clock origin until the first spawn)
    opened: i64,
}

impl Live {
    pub fn new(src: impl Into<Box<dyn SourceAdapter>>, header: String) -> Self {
        Live {
            src: src.into(),
            header,
            session: None,
            cwd: None,
            harness: None,
            accs: HashMap::new(),
            next_order: 0,
            opened: now_ms(),
        }
    }

    /// The primary file of the session this watcher is attached to.
    pub fn path(&self) -> &Path {
        self.src.path()
    }

    /// The harness the session comes from.
    pub fn harness(&self) -> Harness {
        self.src.harness()
    }

    /// The session's project directory, once `session_started` named it.
    pub fn cwd(&self) -> Option<&str> {
        self.cwd.as_deref()
    }

    pub fn set_header(&mut self, header: String) {
        self.header = header;
    }

    /// Pull what's new from the adapter. Cheap to call a few times per second.
    pub fn refresh(&mut self) {
        for ev in self.src.poll() {
            self.apply(&ev);
        }
    }

    /// Fold one event in.
    pub fn apply(&mut self, ev: &schema::Event) {
        let ts = ev.ts;
        let Some(agent) = ev.agent() else {
            if let Body::SessionStarted { cwd, .. } = &ev.body {
                self.session = Some(ev.session.clone());
                self.harness = Some(ev.harness);
                if cwd.is_some() {
                    self.cwd = cwd.clone();
                }
            }
            return;
        };
        let is_main = agent == MAIN;
        let acc = self.acc(agent);
        // The agent's own records are signs of life (stuck detection); a spawn
        // or finish may be reported by its parent, so those don't count.
        if !matches!(ev.body, Body::AgentSpawned { .. } | Body::AgentFinished { .. }) {
            acc.first_ts.get_or_insert(ts);
            acc.last_ts = Some(acc.last_ts.map_or(ts, |l| l.max(ts)));
        }
        match &ev.body {
            Body::SessionStarted { .. } => {}
            Body::AgentSpawned { role, tag, agent_type, task, prompt, alias, ids, .. } => {
                let set = |dst: &mut String, v: &String| {
                    if !v.is_empty() {
                        dst.clone_from(v);
                    }
                };
                set(&mut acc.role, role);
                set(&mut acc.tag, tag);
                set(&mut acc.agent_type, agent_type);
                set(&mut acc.task, task);
                set(&mut acc.prompt, prompt);
                set(&mut acc.alias, alias);
                for id in ids {
                    if !acc.ids.contains(id) {
                        acc.ids.push(id.clone());
                    }
                }
                if acc.spawn_ms.is_none() {
                    acc.spawn_ms = Some(noticed(ts));
                }
            }
            Body::ToolStarted { call, label, kind, tool, file, edit, arg, blocking, test, .. } => {
                acc.denied = false; // it moved on: not stuck on the denial
                acc.waiting = None;
                // `main` finishes every turn while it's the working agent;
                // its next tool call means it's working again. (Subagents
                // don't reopen: their own records can arrive after a finish
                // their parent reported.)
                if is_main {
                    acc.finished = false;
                    acc.done_ts = None;
                }
                let kind = match kind {
                    schema::ToolKind::Look => ToolKind::Look,
                    schema::ToolKind::Run => ToolKind::Run,
                };
                acc.tools.push((ts, Tool { name: label.clone(), kind }));
                if let Some(id) = call {
                    acc.pending.push((id.clone(), ts, *blocking));
                    if *test {
                        acc.test_ids.insert(id.clone());
                    }
                }
                acc.uses.push(Use { time: ts, tool: tool.clone(), file: file.clone(), edit: *edit, arg: arg.clone() });
            }
            Body::ToolFinished { call, error, denied, .. } => {
                acc.waiting = None;
                if let Some(id) = call {
                    acc.pending.retain(|(pid, _, _)| pid != id);
                    if acc.test_ids.remove(id) && !denied {
                        acc.tests.push(TestRun { time: noticed(ts), passed: !error });
                    }
                }
                if *denied {
                    acc.denied = true;
                    acc.error_ts = Some(noticed(ts));
                }
            }
            Body::Usage { id, model, input, output, cache_read, cache_write_5m, cache_write_1h, .. } => {
                if let Some(m) = model {
                    acc.model.clone_from(m);
                }
                let usage = Usage {
                    input: *input,
                    output: *output,
                    cache_read: *cache_read,
                    cache_write_5m: *cache_write_5m,
                    cache_write_1h: *cache_write_1h,
                };
                if usage != Usage::default() {
                    let m = model.clone().unwrap_or_default();
                    let e = acc.usage.entry(id.clone()).or_insert((noticed(ts), Usage::default(), m));
                    e.1.max(&usage);
                }
            }
            Body::Output { text, .. } => acc.last_text.clone_from(text),
            Body::Handoff { to, bytes, .. } => acc.messages.push((noticed(ts), to.clone(), *bytes)),
            Body::Waiting { .. } => {
                acc.waiting.get_or_insert(ts);
            }
            Body::AgentFinished { result, bytes, error, .. } => {
                acc.finished = true;
                acc.done_ts = Some(noticed(ts));
                acc.pending.clear();
                acc.waiting = None;
                if let Some(b) = bytes {
                    acc.size = human_size(*b as usize);
                }
                if let Some(r) = result {
                    acc.result = Some(r.clone());
                }
                if *error {
                    acc.denied = true;
                    acc.error_ts = Some(noticed(ts));
                }
            }
            Body::AgentError { .. } => {
                acc.denied = true;
                acc.error_ts = Some(noticed(ts));
            }
            Body::Heartbeat { .. } => {}
        }
    }

    /// The accumulator for an agent, created on first sight. Subagents are
    /// numbered in the order they appear; [`MAIN`] isn't one of them.
    fn acc(&mut self, id: &str) -> &mut Acc {
        if !self.accs.contains_key(id) {
            let order = if id == MAIN { 0 } else { self.next_order };
            self.next_order += usize::from(id != MAIN);
            self.accs.insert(id.to_string(), Acc { order, ..Acc::default() });
        }
        self.accs.get_mut(id).expect("inserted above")
    }

    /// Build the render model for the current instant, on its own clock.
    pub fn build(&self, pricing: &Pricing) -> Session {
        self.build_at(pricing, self.origin().unwrap_or_else(now_ms))
    }

    /// Build the render model with session time 0 at epoch ms `t0` (a mixed
    /// view puts every harness's session on one clock).
    pub fn build_at(&self, pricing: &Pricing, t0: i64) -> Session {
        // `main` is the working row until a subagent spawns; from then on
        // it's the orchestrator above them
        let orchestrated = self.orchestrated();
        let rows = |k: &str| k != MAIN || !orchestrated;
        let mut accs: Vec<(&str, &Acc)> =
            self.accs.iter().filter(|(k, _)| rows(k)).map(|(k, a)| (k.as_str(), a)).collect();
        accs.sort_by_key(|(_, a)| (a.spawn_ms.or(a.first_ts).unwrap_or(i64::MAX), a.order));
        let root = self.cwd.as_deref().map(|c| c.trim_end_matches('/').to_string() + "/");

        let now = now_ms();
        let boss = if orchestrated { "orchestrator" } else { "you" };

        let mut agents = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        for &(key, a) in &accs {
            // the working main agent goes by its harness ("claude", "codex")
            let role = match (key == MAIN, self.harness) {
                (true, Some(h)) if a.role.is_empty() => h.name().to_string(),
                (true, None) if a.role.is_empty() => "agent".to_string(),
                _ => a.role.clone(),
            };
            let spawn_ms = a.spawn_ms.or(a.first_ts).unwrap_or(t0);
            let spawn = spawn_ms - t0;
            let finish = a
                .finished
                .then(|| a.done_ts.or(a.last_ts).unwrap_or(spawn_ms).max(spawn_ms) - t0);
            let tools: Vec<Tool> = a.tools.iter().map(|(_, t)| t.clone()).collect();
            let live_tool = tools.len().checked_sub(1);
            let size = if a.size.is_empty() { "·".to_string() } else { a.size.clone() };
            let error = a.denied;
            let error_at = if error { a.error_ts.map(|e| e - t0) } else { None };

            let inferred = a
                .pending
                .iter()
                .filter(|(_, _, can)| *can)
                .map(|(_, ts, _)| *ts)
                .min()
                .filter(|pts| now - *pts > WAIT_MS)
                .map(|pts| pts + WAIT_MS);
            let wait = a
                .waiting
                .or(inferred)
                .filter(|_| !a.finished && !error)
                .map(|ws| (ws - t0, i64::MAX));

            events.push(Event {
                time: spawn,
                label: format!("{boss} \u{2192} {role}"),
                detail: "dispatch".into(),
                color: theme::B,
            });
            for (ts, tool) in &a.tools {
                events.push(Event {
                    time: ts - t0,
                    label: role.clone(),
                    detail: tool.name.clone(),
                    color: theme::G,
                });
            }
            if let Some((ws, _)) = wait {
                events.push(Event {
                    time: ws,
                    label: role.clone(),
                    detail: "? waiting".into(),
                    color: theme::AM,
                });
            }
            if let Some(ea) = error_at {
                events.push(Event {
                    time: ea,
                    label: role.clone(),
                    detail: "error".into(),
                    color: theme::RED,
                });
            }
            if let Some(f) = finish {
                events.push(Event {
                    time: f,
                    label: format!("{role} \u{2192} {boss}"),
                    detail: format!("result {size}"),
                    color: theme::B,
                });
            }

            agents.push(Agent {
                name: role,
                model: pricing::family(&a.model),
                spawn,
                finish,
                wait,
                tools,
                live_tool,
                size,
                bob: (a.order as i64) * 3,
                error,
                error_at,
                calls: priced(&a.usage, t0, pricing),
                uses: a
                    .uses
                    .iter()
                    .map(|u| Use {
                        time: u.time - t0,
                        file: u.file.as_ref().map(|f| match &root {
                            Some(r) => f.strip_prefix(r.as_str()).unwrap_or(f).to_string(),
                            None => f.clone(),
                        }),
                        ..u.clone()
                    })
                    .collect(),
                tests: a.tests.iter().map(|r| TestRun { time: r.time - t0, ..*r }).collect(),
                task: if a.task.trim().is_empty() {
                    a.prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
                } else {
                    a.task.trim().to_string()
                },
                result: a.result.clone().or_else(|| {
                    (a.finished && !a.last_text.trim().is_empty()).then(|| a.last_text.clone())
                }),
                last_seen: a.last_ts.map(|l| l - t0),
                in_flight: if a.finished {
                    None
                } else {
                    a.pending.iter().map(|(_, ts, _)| *ts).min().map(|ts| ts - t0)
                },
                prompt: a.prompt.clone(),
                tag: a.tag.clone(),
                ids: std::iter::once(key.to_string()).chain(a.ids.iter().cloned()).collect(),
            });
        }

        // Handoffs: results quoted in a later prompt, plus direct messages.
        let mut handoffs = crate::orbit::detect_handoffs(&agents);
        for (from, (_, a)) in accs.iter().enumerate() {
            for (ts, to, len) in &a.messages {
                let target = to.trim().to_lowercase();
                let hit = accs.iter().enumerate().find(|(j, (key, b))| {
                    *j != from
                        && (b.alias.to_lowercase() == target
                            || agents[*j].name == target
                            || b.agent_type.to_lowercase() == target
                            || key.to_lowercase() == target)
                });
                if let Some((to, _)) = hit {
                    handoffs.push(Handoff {
                        from,
                        to,
                        time: ts - t0,
                        what: "message".into(),
                        size: human_size(*len as usize),
                    });
                }
            }
        }
        handoffs.sort_by_key(|h| h.time);
        for h in &handoffs {
            events.push(Event {
                time: h.time,
                label: format!("{} \u{2192} {}", agents[h.from].name, agents[h.to].name),
                detail: format!("{} {}", h.what, h.size),
                color: theme::B,
            });
        }

        let all = if !agents.is_empty() && agents.iter().all(|a| a.finish.is_some()) {
            agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500
        } else {
            i64::MAX
        };

        events.sort_by_key(|e| e.time);
        let keep = 64usize; // render only shows the last 4
        if events.len() > keep {
            events.drain(0..events.len() - keep);
        }

        let main = self.accs.get(MAIN).filter(|_| orchestrated);
        // models with billed tokens but no price: they count as $0, flagged
        let mut unpriced: Vec<String> = self
            .accs
            .values()
            .flat_map(|a| a.usage.values())
            .map(|(_, _, m)| m.clone())
            .filter(|m| !m.is_empty() && pricing.rates(m).is_none())
            .collect();
        unpriced.sort();
        unpriced.dedup();
        Session {
            header: self.header.clone(),
            id: self.session.clone(),
            agents,
            events,
            all,
            length: 0,
            live: true,
            orch_model: main.map_or_else(String::new, |m| pricing::family(&m.model)),
            orch_calls: main.map_or_else(Vec::new, |m| priced(&m.usage, t0, pricing)),
            epoch0: Some(t0),
            handoffs,
            unpriced,
            solo: !orchestrated,
            harness: Some(self.harness.unwrap_or_else(|| self.src.harness())),
            groups: Vec::new(),
        }
    }

    /// Some subagent exists: `main` is the orchestrator, not a working row.
    fn orchestrated(&self) -> bool {
        self.accs.keys().any(|k| k != MAIN)
    }

    /// Earliest spawn we know about (epoch ms).
    pub fn origin(&self) -> Option<i64> {
        let orchestrated = self.orchestrated();
        self.accs
            .iter()
            .filter(|(k, _)| *k != MAIN || !orchestrated)
            .filter_map(|(_, a)| a.spawn_ms.or(a.first_ts))
            .min()
    }

    /// The render clock `t` now: ms since the earliest spawn, or since we
    /// started watching if nothing has spawned yet (keeps the face alive).
    pub fn clock(&self) -> i64 {
        now_ms() - self.origin().unwrap_or(self.opened)
    }

    /// When we started watching (epoch ms).
    pub fn opened(&self) -> i64 {
        self.opened
    }
}

/// Usage map -> priced calls on the session clock, in time order.
fn priced(calls: &Calls, t0: i64, pricing: &Pricing) -> Vec<Call> {
    let mut v: Vec<Call> = calls
        .values()
        .map(|(time, usage, model)| Call {
            time: time - t0,
            usage: *usage,
            cost: pricing.cost(model, usage),
            saved: pricing.cache_savings(model, usage),
        })
        .collect();
    v.sort_by_key(|c| c.time);
    v
}

/// Live events reach us up to a refresh late. A *recent* event is pinned to
/// the moment we noticed it so its animation (packet, glitch) plays in full;
/// older events, e.g. when attaching mid-session, keep their real time.
fn noticed(ts: i64) -> i64 {
    let now = now_ms();
    if (0..2000).contains(&(now - ts)) {
        now
    } else {
        ts
    }
}

/// Byte count to a compact "x.ykb" / "nb" string.
fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}b")
    } else {
        format!("{:.1}kb", bytes as f64 / 1024.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Event as Ev, Harness, ToolKind as K};

    /// Replays a fixed list of events, for testing the reducer without files.
    struct Script(Vec<Ev>);
    impl SourceAdapter for Script {
        fn harness(&self) -> Harness {
            Harness::Claude
        }
        fn session_id(&self) -> &str {
            "s"
        }
        fn path(&self) -> &Path {
            Path::new("s")
        }
        fn poll(&mut self) -> Vec<Ev> {
            std::mem::take(&mut self.0)
        }
    }

    fn ev(ts: i64, body: Body) -> Ev {
        Ev::new(Harness::Claude, "s", ts, body)
    }

    fn spawn(ts: i64, agent: &str, role: &str) -> Ev {
        ev(
            ts,
            Body::AgentSpawned {
                agent: agent.into(),
                parent: Some(MAIN.into()),
                role: role.into(),
                tag: String::new(),
                agent_type: String::new(),
                task: String::new(),
                prompt: String::new(),
                alias: String::new(),
                ids: vec![],
            },
        )
    }

    fn usage(ts: i64, agent: &str, id: &str, output: u64) -> Ev {
        ev(
            ts,
            Body::Usage {
                agent: agent.into(),
                id: id.into(),
                model: Some("claude-haiku-5-5".into()),
                input: 0,
                output,
                cache_read: 0,
                cache_write_5m: 0,
                cache_write_1h: 0,
            },
        )
    }

    fn tool(ts: i64, agent: &str, call: &str) -> Ev {
        ev(
            ts,
            Body::ToolStarted {
                agent: agent.into(),
                call: Some(call.into()),
                tool: "bash".into(),
                label: "make".into(),
                kind: K::Run,
                file: None,
                edit: false,
                arg: "make".into(),
                blocking: true,
                test: false,
            },
        )
    }

    fn run(events: Vec<Ev>) -> Session {
        let mut live = Live::new(Box::new(Script(events)) as Box<dyn SourceAdapter>, "h".into());
        live.refresh();
        live.build(&Pricing::default())
    }

    /// The orchestrator's events don't take a subagent slot: the first
    /// subagent keeps phase offset 0 however early `main` shows up.
    #[test]
    fn main_does_not_shift_agent_order() {
        let s = run(vec![
            ev(0, Body::SessionStarted { cwd: Some("/w".into()), subagents: true }),
            usage(1, MAIN, "m0", 10),
            spawn(2, "a", "scout"),
            spawn(3, "b", "fixer"),
        ]);
        assert_eq!(s.id.as_deref(), Some("s"));
        assert_eq!(s.agents.iter().map(|a| (a.name.as_str(), a.bob)).collect::<Vec<_>>(), [("scout", 0), ("fixer", 3)]);
        assert_eq!(s.orch_calls.len(), 1);
        assert_eq!(s.orch_model, "haiku");
    }

    /// Schema rules: a repeated usage id updates one call; a re-emitted spawn
    /// merges (ids accumulate, the spawn time stays); an explicit `waiting`
    /// shows at once and clears on the next tool; `agent_error` sets the error
    /// until the agent moves on.
    #[test]
    fn schema_merge_rules() {
        let mut respawn = spawn(9_000, "a", "");
        if let Body::AgentSpawned { ids, task, .. } = &mut respawn.body {
            ids.push("x1".into());
            *task = "map the repo".into();
        }
        let s = run(vec![
            spawn(1_000, "a", "scout"),
            usage(1_100, "a", "m1", 5),
            usage(1_200, "a", "m1", 9),
            respawn,
            tool(2_000, "a", "c1"),
            ev(2_100, Body::Waiting { agent: "a".into(), call: Some("c1".into()), reason: "approval".into() }),
        ]);
        let a = &s.agents[0];
        assert_eq!((a.name.as_str(), a.task.as_str(), a.spawn), ("scout", "map the repo", 0));
        assert!(a.ids.contains(&"a".to_string()) && a.ids.contains(&"x1".to_string()));
        assert_eq!(a.calls.len(), 1, "same id: one call");
        assert_eq!(a.calls[0].usage.output, 9, "the latest counts win");
        assert_eq!(a.wait, Some((1_100, i64::MAX)), "explicit wait shows without the 10s delay");
        assert_eq!(a.last_seen, Some(1_100), "usage, tools and waits are signs of life; spawns aren't");

        let s = run(vec![
            spawn(0, "a", "scout"),
            ev(100, Body::AgentError { agent: "a".into(), message: "rejected".into() }),
        ]);
        assert!(s.agents[0].error);
        let s = run(vec![
            spawn(0, "a", "scout"),
            ev(100, Body::AgentError { agent: "a".into(), message: "rejected".into() }),
            tool(200, "a", "c2"),
        ]);
        assert!(!s.agents[0].error, "cleared by its next tool call");
    }

    /// Without subagents `main` is the one row, named for the harness; it
    /// finishes each turn and reopens on its next tool call. A subagent
    /// whose own records land after its parent reported it done stays done.
    #[test]
    fn solo_main_reopens_subagents_dont() {
        let fin = |ts, agent: &str| {
            ev(ts, Body::AgentFinished { agent: agent.into(), result: Some("ok".into()), bytes: None, error: false })
        };
        let solo = |more: Vec<Ev>| {
            let mut evs = vec![ev(0, Body::SessionStarted { cwd: None, subagents: false }), tool(100, MAIN, "c1"), fin(200, MAIN)];
            evs.extend(more);
            run(evs)
        };
        let s = solo(vec![]);
        assert_eq!((s.agents.len(), s.agents[0].name.as_str()), (1, "claude"));
        assert!(s.agents[0].finish.is_some(), "turn over");
        let s = solo(vec![tool(300, MAIN, "c2")]);
        assert!(s.agents[0].finish.is_none(), "next turn: working again");

        let s = run(vec![spawn(0, "a", "fixer"), fin(500, "a"), tool(100, "a", "c1")]);
        assert!(s.agents[0].finish.is_some());
    }
}
