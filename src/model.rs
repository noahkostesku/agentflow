//! The data model the renderer draws. Both demo mode (scripted) and, later,
//! live mode (parsed from transcripts) produce a `Session` that `render` consumes.

use ratatui::style::Color;

/// How an agent's current tool reads on its face.
/// `Look` = reading/searching (eyes glance side to side).
/// `Run`  = running a command (wide-eyed).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToolKind {
    Look,
    Run,
}

#[derive(Clone)]
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

/// One subagent spawned by the orchestrator.
/// Times are milliseconds on the session clock.
#[derive(Clone)]
pub struct Agent {
    pub name: String,
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
pub struct Session {
    /// full header string, e.g. "agentflow // <project> // <hash>"
    pub header: String,
    pub agents: Vec<Agent>,
    pub events: Vec<Event>,
    /// moment all agents are done (max finish + 500ms); `i64::MAX` while any run
    pub all: i64,
    /// loop length for demo playback; live mode ignores it
    pub length: i64,
    /// live mode: real elapsed seconds (×1) and real current tool; demo: ×3 + cycling
    pub live: bool,
}
