//! The data model the renderer draws. Both demo mode (scripted) and live mode
//! (parsed from transcripts) produce a `Session` that `render` consumes.

use ratatui::style::Color;

use crate::schema::Harness;

/// How an agent's current tool reads on its face.
/// `Look` = reading/searching (eyes glance side to side).
/// `Run`  = running a command (wide-eyed).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ToolKind {
    Look,
    Run,
}

#[derive(Clone, Debug)]
pub struct Tool {
    pub name: String,
    pub kind: ToolKind,
}

impl Tool {
    pub fn look(name: &str) -> Self {
        Tool { name: name.into(), kind: ToolKind::Look }
    }
    pub fn run(name: &str) -> Self {
        Tool { name: name.into(), kind: ToolKind::Run }
    }
}

/// Token counts from one API response's `usage` block.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    /// cache writes billed at the 5-minute rate
    pub cache_write_5m: u64,
    /// cache writes billed at the 1-hour rate
    pub cache_write_1h: u64,
}

impl Usage {
    pub fn add(&mut self, o: &Usage) {
        self.input += o.input;
        self.output += o.output;
        self.cache_read += o.cache_read;
        self.cache_write_5m += o.cache_write_5m;
        self.cache_write_1h += o.cache_write_1h;
    }
    /// Field-wise max: the same message can be logged more than once as it
    /// streams, and the latest copy carries the final counts.
    pub fn max(&mut self, o: &Usage) {
        self.input = self.input.max(o.input);
        self.output = self.output.max(o.output);
        self.cache_read = self.cache_read.max(o.cache_read);
        self.cache_write_5m = self.cache_write_5m.max(o.cache_write_5m);
        self.cache_write_1h = self.cache_write_1h.max(o.cache_write_1h);
    }
    pub fn cache_write(&self) -> u64 {
        self.cache_write_5m + self.cache_write_1h
    }
    /// Every token processed (prompt side + output).
    pub fn total(&self) -> u64 {
        self.input + self.output + self.cache_read + self.cache_write()
    }
}

/// One billed model call (one assistant message).
#[derive(Clone, Debug)]
pub struct Call {
    /// session ms
    pub time: i64,
    pub usage: Usage,
    /// USD, priced when the session was built
    pub cost: f64,
    /// USD saved by cache reads vs. paying full input price for them
    pub saved: f64,
}

/// One tool invocation, for stats (tool counts, files touched, edits).
#[derive(Clone, Debug)]
pub struct Use {
    pub time: i64,
    /// short lowercase tool name ("read", "edit", "bash", …)
    pub tool: String,
    /// file the tool read or wrote, if any
    pub file: Option<String>,
    /// it modified a file (edit / write / multiedit / notebookedit)
    pub edit: bool,
    /// short argument for the detail view ("cargo test", "render.rs", a host)
    pub arg: String,
}

/// A test command's outcome.
#[derive(Clone, Copy, Debug)]
pub struct TestRun {
    pub time: i64,
    pub passed: bool,
}

/// One subagent spawned by the orchestrator.
/// Times are milliseconds on the session clock.
#[derive(Clone, Default)]
pub struct Agent {
    pub name: String,
    /// model family badge ("opus", "sonnet", "haiku", …); empty if unknown
    pub model: String,
    /// when the orchestrator dispatched it
    pub spawn: i64,
    /// when it returned a result; `None` = still running (live, unknown end)
    pub finish: Option<i64>,
    /// optional [start, end) window where it is blocked (amber "waiting")
    pub wait: Option<(i64, i64)>,
    /// tools it works through
    pub tools: Vec<Tool>,
    /// In demo mode (`None`) the row cycles tools by time. In live mode, the
    /// index of the tool that is current *right now*.
    pub live_tool: Option<usize>,
    /// result size string flashed on return, e.g. "2.1kb"
    pub size: String,
    /// per-agent phase offset so blinks/spinners don't march in lockstep
    pub bob: i64,
    /// agent is in error (a denied/blocked tool call was its latest outcome)
    pub error: bool,
    /// when the error happened (session ms) — drives the brief error glitch
    pub error_at: Option<i64>,
    /// billed model calls, in time order
    pub calls: Vec<Call>,
    /// tool invocations, in time order
    pub uses: Vec<Use>,
    /// test-command outcomes, in time order
    pub tests: Vec<TestRun>,
    /// what it was asked to do (the spawn's description, else the prompt's first line)
    pub task: String,
    /// its final answer, when known
    pub result: Option<String>,
    /// session ms of its latest transcript entry of any kind (live only; the
    /// demo derives activity from its calls and tool uses)
    pub last_seen: Option<i64>,
    /// session ms when its oldest still-unanswered tool call started (a
    /// tool_use with no tool_result yet). Busy, not stuck.
    pub in_flight: Option<i64>,
    /// the full prompt it was spawned with (handoff detection)
    pub prompt: String,
    /// ids another agent's prompt may quote: spawn tool_use id, agentId
    pub ids: Vec<String>,
    /// short dim tag shown after the name where there's room (Codex: the
    /// agent's path role, "repo_overview")
    pub tag: String,
}

/// Rarity by model family: (glyph, name). haiku common, sonnet rare,
/// opus/fable/mythos legendary.
pub fn rarity(model: &str) -> Option<(&'static str, &'static str)> {
    match model {
        "haiku" => Some(("·", "common")),
        "sonnet" => Some(("◆", "rare")),
        "opus" | "fable" | "mythos" => Some(("✦", "legendary")),
        _ => None,
    }
}

impl Agent {
    /// Calls billed by session time `t`.
    pub fn calls_at(&self, t: i64) -> impl Iterator<Item = &Call> {
        self.calls.iter().filter(move |c| c.time <= t)
    }
    pub fn cost_at(&self, t: i64) -> f64 {
        self.calls_at(t).map(|c| c.cost).sum()
    }
    pub fn usage_at(&self, t: i64) -> Usage {
        let mut u = Usage::default();
        for c in self.calls_at(t) {
            u.add(&c.usage);
        }
        u
    }
}

/// One agent's output fed straight to another: its result quoted in the next
/// agent's prompt, or a direct teammate message. Drawn as a chord in the orbit.
#[derive(Clone, Debug, PartialEq)]
pub struct Handoff {
    /// indexes into `Session::agents`
    pub from: usize,
    pub to: usize,
    /// session ms the data moved (the receiver's spawn, or the message time)
    pub time: i64,
    /// what moved: "findings", "patch", "result", "message"
    pub what: String,
    /// "1.4kb"
    pub size: String,
}

/// A line in the scrolling handoff/tool log.
#[derive(Clone)]
pub struct Event {
    pub time: i64,
    pub label: String,
    pub detail: String,
    pub color: Color,
}

/// Everything the renderer needs for a frame, independent of where it came from.
#[derive(Default)]
pub struct Session {
    /// full header string, e.g. "agentflow // <project> // <hash>"
    pub header: String,
    /// stable id for lifetime persistence (live: the transcript's session id)
    pub id: Option<String>,
    pub agents: Vec<Agent>,
    pub events: Vec<Event>,
    /// moment all agents are done (max finish + 500ms); `i64::MAX` while any run
    pub all: i64,
    /// loop length for demo playback; live mode ignores it
    pub length: i64,
    /// live mode: real elapsed seconds (×1) and real current tool; demo: ×3 + cycling
    pub live: bool,
    /// the orchestrator's own model badge and billed calls
    pub orch_model: String,
    pub orch_calls: Vec<Call>,
    /// epoch ms of session time 0 (live only): places activity in wall-clock hours
    pub epoch0: Option<i64>,
    /// agent → agent handoffs, in time order
    pub handoffs: Vec<Handoff>,
    /// model ids with billed tokens but no known price (counted as $0)
    pub unpriced: Vec<String>,
    /// one agent working for the user (a harness without subagents): the
    /// root of the tree is "you", not an orchestrator
    pub solo: bool,
    /// the harness it came from (live and recorded sessions; `None` for a
    /// mixed view, which spans several)
    pub harness: Option<Harness>,
    /// mixed view (`--source all`): one group per harness, each with its own
    /// root. `agents`, `orch_calls`, `events` and `handoffs` are then every
    /// group's, concatenated in group order. Empty for a single session.
    pub groups: Vec<Group>,
}

/// One harness's part of a mixed view.
pub struct Group {
    /// harness name ("claude", "codex"), the group's dim label
    pub harness: String,
    /// index of its first agent in the mixed `Session::agents`
    pub start: usize,
    /// its session alone, on the mixed view's clock
    pub sess: Session,
}

impl Group {
    /// Its agents' indexes in the mixed session.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.start..self.start + self.sess.agents.len()
    }
    /// A mixed-session agent index as an index into this group's session.
    pub fn local(&self, i: Option<usize>) -> Option<usize> {
        i.filter(|i| self.range().contains(i)).map(|i| i - self.start)
    }
}

impl Session {
    /// Who dispatches the agents: an orchestrator, or "you" when one agent
    /// works for the user directly.
    pub fn root_name(&self) -> &'static str {
        if self.solo {
            "you"
        } else {
            "orchestrator"
        }
    }
    /// Total session cost (orchestrator + every subagent) by time `t`.
    pub fn cost_at(&self, t: i64) -> f64 {
        let orch: f64 = self.orch_calls.iter().filter(|c| c.time <= t).map(|c| c.cost).sum();
        orch + self.agents.iter().map(|a| a.cost_at(t)).sum::<f64>()
    }
    /// Every call in the session (orchestrator first), by time `t`.
    pub fn all_calls_at(&self, t: i64) -> impl Iterator<Item = &Call> {
        self.orch_calls
            .iter()
            .chain(self.agents.iter().flat_map(|a| a.calls.iter()))
            .filter(move |c| c.time <= t)
    }
    /// A mixed view of several harnesses' sessions.
    pub fn mixed(&self) -> bool {
        !self.groups.is_empty()
    }
    /// The group an agent belongs to (mixed view only).
    pub fn group_of(&self, i: usize) -> Option<&Group> {
        self.groups.iter().find(|g| g.range().contains(&i))
    }
    /// Elapsed-time multiplier: the demo runs ×3 for liveliness.
    pub fn scale(&self) -> f64 {
        if self.live {
            1.0
        } else {
            3.0
        }
    }
}
