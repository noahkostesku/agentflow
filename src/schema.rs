//! Event schema v1: the normalized, versioned stream every harness adapter
//! produces and everything downstream consumes. See `docs/schema.md`.
//!
//! One `Event` is one fact about one session: an envelope (`v`, `harness`,
//! `session`, `ts`) plus a `type`-tagged body. Serialized, it is one JSON
//! object per line (`agentflow events --json`).
//!
//! Rules adapters follow (the reducer in `live.rs` relies on them):
//!   * `session_started` comes first, once per session.
//!   * `agent_spawned` comes before any other event for that agent. It may be
//!     re-emitted as the harness reveals more (a transcript id, a better type);
//!     consumers merge: non-empty scalars replace, `ids` accumulate.
//!   * the session's top-level thread is the agent [`MAIN`]. No agent spawns
//!     it, but an adapter may emit `agent_spawned` for it (no `parent`) to
//!     record what the user asked, again on each new prompt. It's the one
//!     working agent until a subagent spawns, then the orchestrator.
//!   * `tool_started` is emitted once per call id.

use serde::{Deserialize, Serialize};

/// The schema version every event carries in `v`. Bump on a breaking change
/// (a field removed or its meaning changed); adding optional fields is not one.
pub const VERSION: u32 = 1;

/// Agent id of a session's top-level thread (the orchestrator, or the only
/// agent in a harness without subagents).
pub const MAIN: &str = "main";

/// The coding-agent harness an event came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    pub fn name(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }
}

/// How a tool call reads: `look` = reading/searching, `run` = acting
/// (commands, edits, fetches).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ToolKind {
    Look,
    Run,
}

/// One normalized event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// schema version ([`VERSION`])
    pub v: u32,
    pub harness: Harness,
    /// the harness's own session id
    pub session: String,
    /// epoch milliseconds when it happened (as the harness recorded it)
    pub ts: i64,
    #[serde(flatten)]
    pub body: Body,
}

impl Event {
    pub fn new(harness: Harness, session: &str, ts: i64, body: Body) -> Self {
        Event { v: VERSION, harness, session: session.to_string(), ts, body }
    }

    /// Point an agent event at another agent (adapters build bodies for
    /// [`MAIN`] and retarget them to a subagent). No-op on session events.
    pub fn set_agent(&mut self, to: &str) {
        match &mut self.body {
            Body::SessionStarted { .. } => {}
            Body::AgentSpawned { agent, .. }
            | Body::ToolStarted { agent, .. }
            | Body::ToolFinished { agent, .. }
            | Body::Usage { agent, .. }
            | Body::Output { agent, .. }
            | Body::Handoff { agent, .. }
            | Body::Waiting { agent, .. }
            | Body::AgentFinished { agent, .. }
            | Body::AgentError { agent, .. }
            | Body::Heartbeat { agent } => *agent = to.to_string(),
        }
    }

    /// The agent this event is about (`None` for session-level events).
    pub fn agent(&self) -> Option<&str> {
        match &self.body {
            Body::SessionStarted { .. } => None,
            Body::AgentSpawned { agent, .. }
            | Body::ToolStarted { agent, .. }
            | Body::ToolFinished { agent, .. }
            | Body::Usage { agent, .. }
            | Body::Output { agent, .. }
            | Body::Handoff { agent, .. }
            | Body::Waiting { agent, .. }
            | Body::AgentFinished { agent, .. }
            | Body::AgentError { agent, .. }
            | Body::Heartbeat { agent } => Some(agent),
        }
    }
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What happened. Serialized with a `type` tag in snake_case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Body {
    /// A session began (or we attached to it).
    SessionStarted {
        /// the project directory the harness ran in
        #[serde(default, skip_serializing_if = "Option::is_none")]
        cwd: Option<String>,
        /// the harness can run subagents under [`MAIN`] (informational: main
        /// is the working agent until one actually spawns)
        #[serde(default)]
        subagents: bool,
    },

    /// A subagent was dispatched (or more became known about it).
    AgentSpawned {
        agent: String,
        /// the agent that dispatched it (usually [`MAIN`])
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        /// display name ("scout", "test-runner", "socrates", …)
        #[serde(default)]
        role: String,
        /// a short dim tag shown after the name where there's room (Codex:
        /// the agent's path, "repo_overview")
        #[serde(default, skip_serializing_if = "String::is_empty")]
        tag: String,
        /// the harness's own agent type ("Explore", "general-purpose", …)
        #[serde(default, skip_serializing_if = "String::is_empty")]
        agent_type: String,
        /// short description of what it was asked to do
        #[serde(default, skip_serializing_if = "String::is_empty")]
        task: String,
        /// the full prompt it was given
        #[serde(default, skip_serializing_if = "String::is_empty")]
        prompt: String,
        /// a name other agents can address it by (agent teams)
        #[serde(default, skip_serializing_if = "String::is_empty")]
        alias: String,
        /// other ids the harness knows it by (quoted in prompts → handoffs)
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        ids: Vec<String>,
    },

    /// An agent invoked a tool.
    ToolStarted {
        agent: String,
        /// the harness's call id; pairs with `tool_finished`
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        /// normalized lowercase tool name ("bash", "read", "edit", …)
        tool: String,
        /// short face label ("cargo" for `cargo test`, else the tool name)
        label: String,
        kind: ToolKind,
        /// the file it reads or writes, if any
        #[serde(default, skip_serializing_if = "Option::is_none")]
        file: Option<String>,
        /// it modifies a file
        #[serde(default, skip_serializing_if = "is_false")]
        edit: bool,
        /// a few words of argument ("cargo test", "lib.rs", a host)
        #[serde(default, skip_serializing_if = "String::is_empty")]
        arg: String,
        /// it can block on an approval prompt (commands, edits, fetches)
        #[serde(default, skip_serializing_if = "is_false")]
        blocking: bool,
        /// it runs a test suite; its `tool_finished.error` is the outcome
        #[serde(default, skip_serializing_if = "is_false")]
        test: bool,
    },

    /// A tool call returned.
    ToolFinished {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        /// it failed (non-zero exit, tool error)
        #[serde(default, skip_serializing_if = "is_false")]
        error: bool,
        /// it was denied or blocked by permissions (puts the agent in error)
        #[serde(default, skip_serializing_if = "is_false")]
        denied: bool,
    },

    /// Tokens billed for one model call. A repeat of the same `id` updates
    /// that call (field-wise max), it is not a second call.
    Usage {
        agent: String,
        /// the model call's id
        #[serde(default)]
        id: String,
        /// model id as the provider names it; absent when unknown
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default)]
        input: u64,
        #[serde(default)]
        output: u64,
        #[serde(default)]
        cache_read: u64,
        #[serde(default)]
        cache_write_5m: u64,
        #[serde(default)]
        cache_write_1h: u64,
    },

    /// The agent produced text (its latest is the fallback result).
    Output { agent: String, text: String },

    /// The agent sent something directly to another agent.
    Handoff {
        agent: String,
        /// recipient as the harness named it: an agent id, alias, role or type
        to: String,
        /// "message", "findings", "patch", …
        what: String,
        #[serde(default)]
        bytes: u64,
    },

    /// The agent is blocked on the user (an approval prompt). Cleared by its
    /// next tool or finish event. Consumers also infer waiting from a
    /// `blocking` call that hasn't returned for a while.
    Waiting {
        agent: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        call: Option<String>,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        reason: String,
    },

    /// The agent finished and handed back.
    AgentFinished {
        agent: String,
        /// its final answer, when known (replaces an earlier one)
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<String>,
        /// size of what it handed back
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bytes: Option<u64>,
        /// it finished in error
        #[serde(default, skip_serializing_if = "is_false")]
        error: bool,
    },

    /// The agent hit an error it may recover from (cleared by its next tool).
    AgentError {
        agent: String,
        #[serde(default, skip_serializing_if = "String::is_empty")]
        message: String,
    },

    /// The agent wrote something that carries no other event: a sign of
    /// life for stuck detection.
    Heartbeat { agent: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every JSON example in docs/schema.md parses as an event, and
    /// re-serializes to the same JSON: the doc can't drift from the code.
    #[test]
    fn doc_examples_round_trip() {
        let doc = include_str!("../docs/schema.md");
        let mut n = 0;
        for block in doc.split("```json").skip(1) {
            let json = block.split("```").next().unwrap_or("").trim();
            let ev: Event = serde_json::from_str(json).unwrap_or_else(|e| panic!("{e}: {json}"));
            assert_eq!(ev.v, VERSION);
            let back: serde_json::Value = serde_json::to_value(&ev).unwrap();
            let orig: serde_json::Value = serde_json::from_str(json).unwrap();
            assert_eq!(back, orig, "round trip changed {json}");
            n += 1;
        }
        assert!(n >= 11, "every event type has an example ({n} found)");
    }

    #[test]
    fn compact_and_forward_compatible() {
        let ev = Event::new(
            Harness::Claude,
            "s1",
            5,
            Body::ToolFinished { agent: "a".into(), call: None, error: false, denied: false },
        );
        let line = serde_json::to_string(&ev).unwrap();
        assert_eq!(line, r#"{"v":1,"harness":"claude","session":"s1","ts":5,"type":"tool_finished","agent":"a"}"#);
        // unknown fields from a newer minor revision are ignored
        let newer = r#"{"v":1,"harness":"claude","session":"s1","ts":5,"type":"heartbeat","agent":"a","extra":1}"#;
        let ev: Event = serde_json::from_str(newer).unwrap();
        assert_eq!(ev.agent(), Some("a"));
    }
}
