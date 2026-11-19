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
    /// outstanding tool calls with no result yet
    pending: Vec<Pending>,
    /// an explicit `waiting` event's time, until its next tool or finish
    waiting: Option<i64>,
    /// what it's waiting on ("question: which db?"), with `waiting`
    wait_what: String,
    /// what its latest error was about ("denied $ rm -rf build")
    error_what: String,
    /// when its current turn of work began (its first record, or `main`'s
    /// first tool call after a finish)
    turn_ts: Option<i64>,
    /// the latest tool call it made, described
    doing: String,
    /// the latest tool call that returned, described
    last_call: String,
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

/// A tool call with no result yet.
struct Pending {
    id: String,
    /// when it started (epoch ms)
    ts: i64,
    /// it can block on an approval prompt
    blocking: bool,
    /// what it is: "$ cargo test", "edit lib.rs"
    what: String,
    /// its argument alone ("which database?" for a question)
    arg: String,
}

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
            acc.turn_ts.get_or_insert(ts);
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
                    if acc.finished {
                        acc.turn_ts = Some(ts);
                    }
                    acc.finished = false;
                    acc.done_ts = None;
                }
                acc.doing = describe(tool, arg);
                let kind = match kind {
                    schema::ToolKind::Look => ToolKind::Look,
                    schema::ToolKind::Run => ToolKind::Run,
                };
                acc.tools.push((ts, Tool { name: label.clone(), kind }));
                if let Some(id) = call {
                    acc.pending.push(Pending {
                        id: id.clone(),
                        ts,
                        blocking: *blocking,
                        what: describe(tool, arg),
                        arg: arg.clone(),
                    });
                    if *test {
                        acc.test_ids.insert(id.clone());
                    }
                }
                acc.uses.push(Use { time: ts, tool: tool.clone(), file: file.clone(), edit: *edit, arg: arg.clone() });
            }
            Body::ToolFinished { call, error, denied, .. } => {
                acc.waiting = None;
                if let Some(id) = call {
                    if let Some(p) = acc.pending.iter().find(|p| &p.id == id) {
                        acc.last_call = p.what.clone();
                    }
                    if *denied {
                        acc.error_what = format!("denied {}", acc.last_call).trim_end().to_string();
                    }
                    acc.pending.retain(|p| &p.id != id);
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
            Body::Waiting { call, reason, .. } => {
                acc.waiting.get_or_insert(ts);
                // "question: which database?", or the call itself
                let on = call.as_ref().and_then(|c| acc.pending.iter().find(|p| &p.id == c));
                acc.wait_what = match (reason.as_str(), on) {
                    ("", Some(p)) => p.what.clone(),
                    ("", None) => String::new(),
                    (r, Some(p)) if !p.arg.trim().is_empty() => format!("{r}: {}", p.arg.trim()),
                    (r, _) => r.to_string(),
                };
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
            Body::AgentError { message, .. } => {
                acc.denied = true;
                acc.error_ts = Some(noticed(ts));
                acc.error_what = format!("{message} {}", acc.last_call).trim().to_string();
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
                .filter(|p| p.blocking)
                .map(|p| p.ts)
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
                    a.pending.iter().map(|p| p.ts).min().map(|ts| ts - t0)
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

    /// The session id, once `session_started` named it.
    pub fn session_id(&self) -> &str {
        self.session.as_deref().unwrap_or_else(|| self.src.session_id())
    }

    /// The whole session at a glance as of epoch ms `now`, for the inbox:
    /// is anyone waiting on the user, in error, running; what it's doing.
    /// Every agent counts, the orchestrator included.
    pub fn pulse(&self, pricing: &Pricing, now: i64) -> Pulse {
        let orchestrated = self.orchestrated();
        let mut p = Pulse { empty: self.accs.is_empty(), ..Pulse::default() };
        // the latest tool call of any running agent: (ts, who, what)
        let mut doing: Option<(i64, &str, &Acc)> = None;
        for (key, a) in &self.accs {
            let sub = key != MAIN;
            if sub || !orchestrated {
                p.total += 1;
                p.active += usize::from(!a.finished);
            }
            let start = a.spawn_ms.or(a.first_ts);
            p.started = match (p.started, start) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, y) => x.or(y),
            };
            let seen = [a.last_ts, a.spawn_ms, a.done_ts].into_iter().flatten().max();
            p.last = p.last.max(seen.unwrap_or(0));
            for (_, usage, model) in a.usage.values() {
                p.cost += pricing.cost(model, usage);
                p.unpriced |= !model.is_empty() && pricing.rates(model).is_none();
            }
            if a.finished {
                p.done = Some(p.done.unwrap_or(0).max(a.done_ts.or(a.last_ts).unwrap_or(0)));
                // the orchestrator ending its turn with an error stays one
                if a.denied && !sub {
                    p.error = Some((a.error_ts.unwrap_or(0), a.error_what.clone()));
                }
                continue;
            }
            p.running = true;
            p.in_flight |= !a.pending.is_empty();
            p.turn = Some(p.turn.unwrap_or(i64::MAX).min(a.turn_ts.or(start).unwrap_or(now)));
            if a.denied {
                let e = (a.error_ts.unwrap_or(0), a.error_what.clone());
                if p.error.as_ref().is_none_or(|old| e.0 < old.0) {
                    p.error = Some(e);
                }
                continue;
            }
            // waiting: an explicit prompt, or a call that can prompt and
            // hasn't returned for a while (could be a slow command)
            let inferred = a.pending.iter().filter(|c| c.blocking && now - c.ts > WAIT_MS).min_by_key(|c| c.ts);
            let wait = match (a.waiting, inferred) {
                (Some(ts), _) => Some((ts, a.wait_what.clone())),
                (None, Some(c)) => Some((c.ts, c.what.clone())),
                (None, None) => None,
            };
            if let Some(w) = wait {
                let who = if orchestrated && sub { format!("{}: ", self.role_of(key, a)) } else { String::new() };
                let w = (w.0, format!("{who}{}", w.1).trim_end_matches(": ").to_string());
                if p.waiting.as_ref().is_none_or(|old| w.0 < old.0) {
                    p.waiting = Some(w);
                }
            }
            if let Some(&(ts, _)) = a.tools.last() {
                if doing.is_none_or(|(t, _, _)| ts > t) {
                    doing = Some((ts, key, a));
                }
            }
        }
        if p.running {
            p.done = None;
        }
        p.doing = doing
            .map(|(_, key, a)| {
                let who = if orchestrated && key != MAIN { format!("{}: ", self.role_of(key, a)) } else { String::new() };
                format!("{who}{}", a.doing)
            })
            .unwrap_or_default();
        // what it said last: the orchestrator's (or only agent's) answer
        let main = self.accs.get(MAIN);
        p.said = main
            .and_then(|m| m.result.clone().filter(|r| !r.trim().is_empty()).or_else(|| Some(m.last_text.clone())))
            .unwrap_or_default();
        p
    }

    /// A subagent's display name for the inbox's "who: what".
    fn role_of(&self, key: &str, a: &Acc) -> String {
        [&a.role, &a.agent_type].into_iter().find(|s| !s.is_empty()).cloned().unwrap_or_else(|| key.chars().take(8).collect())
    }
}

/// One session at a glance (see [`Live::pulse`]). Times are epoch ms.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Pulse {
    /// nothing has happened in it yet
    pub empty: bool,
    /// some agent is blocked on the user: since when, and on what
    pub waiting: Option<(i64, String)>,
    /// some agent's latest outcome was an error (a denial, a rejection)
    pub error: Option<(i64, String)>,
    /// some agent hasn't finished
    pub running: bool,
    /// some running agent has a tool call that hasn't returned
    pub in_flight: bool,
    /// latest sign of life from any agent
    pub last: i64,
    /// every agent finished: when the last one did
    pub done: Option<i64>,
    /// when the work now running began (the current turn)
    pub turn: Option<i64>,
    /// earliest agent start
    pub started: Option<i64>,
    /// the latest tool call of a running agent ("scout: read lib.rs")
    pub doing: String,
    /// the orchestrator's (or only agent's) latest message
    pub said: String,
    /// working rows still running / all rows (subagents, or the one agent)
    pub active: usize,
    pub total: usize,
    /// API-equivalent USD so far
    pub cost: f64,
    /// some tokens came from a model with no known price
    pub unpriced: bool,
}

/// A tool call in a few words: "$ cargo test -q", "edit lib.rs", "grep".
fn describe(tool: &str, arg: &str) -> String {
    match (tool, arg.trim()) {
        (_, "") => tool.to_string(),
        ("bash", a) => format!("$ {a}"),
        (t, a) => format!("{t} {a}"),
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

    /// The inbox's view of a session: a subagent's call that can prompt and
    /// hasn't returned for 10s is waiting, named for its agent; agents count
    /// active/total; the orchestrator's denial is an error; a finished
    /// session reports when.
    #[test]
    fn pulse_for_the_inbox() {
        let mut live = Live::new(
            Box::new(Script(vec![
                ev(0, Body::SessionStarted { cwd: Some("/w".into()), subagents: true }),
                tool(100, MAIN, "m1"),
                ev(150, Body::ToolFinished { agent: MAIN.into(), call: Some("m1".into()), error: false, denied: false }),
                spawn(200, "a", "scout"),
                spawn(300, "b", "fixer"),
                tool(400, "a", "c1"),
                ev(500, Body::AgentFinished { agent: "b".into(), result: Some("ok".into()), bytes: None, error: false }),
            ])) as Box<dyn SourceAdapter>,
            "h".into(),
        );
        live.refresh();
        let p = live.pulse(&Pricing::default(), 5_000);
        assert_eq!((p.waiting.clone(), p.active, p.total), (None, 1, 2), "a 4.6s call isn't a wait yet");
        assert_eq!(p.doing, "scout: $ make");
        assert!(p.running && p.in_flight);
        let p = live.pulse(&Pricing::default(), 20_000);
        assert_eq!(p.waiting, Some((400, "scout: $ make".into())));

        let mut live = Live::new(
            Box::new(Script(vec![
                tool(100, MAIN, "m1"),
                ev(200, Body::ToolFinished { agent: MAIN.into(), call: Some("m1".into()), error: true, denied: true }),
                ev(300, Body::AgentFinished { agent: MAIN.into(), result: Some("Blocked.\nby policy".into()), bytes: None, error: false }),
            ])) as Box<dyn SourceAdapter>,
            "h".into(),
        );
        live.refresh();
        let p = live.pulse(&Pricing::default(), 5_000);
        assert!(!p.running);
        assert_eq!(p.error.map(|e| e.1).as_deref(), Some("denied $ make"));
        assert_eq!(p.done, Some(300));
        assert_eq!(p.said, "Blocked.\nby policy");
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
