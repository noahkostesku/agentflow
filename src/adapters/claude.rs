//! Claude Code adapter: a session's transcripts on disk → schema events.
//!
//! Format (observed):
//!   * main transcript `<session>.jsonl` — the orchestrator. Each subagent spawn
//!     is an `assistant` entry with a `tool_use` block named `Agent` (older
//!     builds: `Task`) carrying `input.subagent_type`, `description`, `prompt`.
//!     Its `tool_result` either says `status: "async_launched"` with an
//!     `agentId` (background agent) or is the agent's final result (foreground).
//!   * `<session>/subagents/agent-<id>.jsonl` — one per subagent, created at
//!     launch. Its first entry is the prompt; then its own `tool_use` /
//!     `tool_result` activity, ending in a `SubagentHandback`.
//!   * `<session>/subagents/agent-<id>.meta.json` — summary sidecar
//!     (`agentType`, `description`, `toolUseId`, `finished`). Written only when
//!     the agent *finishes*, so it can't be used to find running agents.
//!
//! A running subagent file is linked to its spawn by (1) the `agentId` in the
//! launch result, or (2) matching its first message against the spawn's prompt.
//! Agents are keyed by the spawn's `tool_use` id; the orchestrator is
//! `schema::MAIN`. Teammate `SendMessage` calls (agent teams) become
//! `handoff` events.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use super::{is_test_command, iso_to_ms, now_ms, tail, SourceAdapter};
use crate::discovery::Resolved;
use crate::schema::{Body, Event, Harness, ToolKind, MAIN};

#[cfg(test)]
use crate::live::Live;
#[cfg(test)]
use crate::pricing::Pricing;

/// What we know about one subagent, keyed by its spawn's tool_use id.
#[derive(Default)]
struct Sub {
    agent_type: String,
    description: String,
    prompt: String,
    /// teammate name given at spawn (`input.name`), for message routing
    alias: String,
    /// agentIds of its transcript
    agent_ids: Vec<String>,
    /// the last `agent_spawned` we emitted for it: (role, type, task, prompt,
    /// alias, ids) — re-emitted when any of it changes
    announced: Option<(String, String, String, String, String, usize)>,
    sub_path: Option<PathBuf>,
    sub_offset: u64,
    /// tool_use ids already emitted (guards against re-emitted entries)
    seen: HashSet<String>,
    /// a SubagentHandback carried its result
    handed_back: bool,
    /// some finish was emitted for it
    finished: bool,
    /// its meta.json says finished
    meta_finished: bool,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
}

pub struct ClaudeAdapter {
    session: String,
    main_path: PathBuf,
    main_offset: u64,
    subagents_dir: PathBuf,
    started: bool,
    subs: HashMap<String, Sub>,
    /// agentId -> spawn tool_use id, for subagent files already linked
    linked: HashMap<String, String>,
}

impl From<&Resolved> for Box<dyn SourceAdapter> {
    fn from(r: &Resolved) -> Self {
        Box::new(ClaudeAdapter::open(&r.path))
    }
}

impl SourceAdapter for ClaudeAdapter {
    fn harness(&self) -> Harness {
        Harness::Claude
    }

    fn session_id(&self) -> &str {
        &self.session
    }

    fn path(&self) -> &Path {
        &self.main_path
    }

    fn poll(&mut self) -> Vec<Event> {
        let mut out = Vec::new();
        if !self.started {
            self.started = true;
            let cwd = read_cwd(&self.main_path).map(|p| p.to_string_lossy().into_owned());
            let ts = first_timestamp(&self.main_path).unwrap_or_else(now_ms);
            out.push(self.ev(ts, Body::SessionStarted { cwd, subagents: true }));
        }
        self.scan_main(&mut out);
        self.scan_subagents(&mut out);
        out
    }
}

impl ClaudeAdapter {
    /// Watch the session whose main transcript is `main_path`.
    pub fn open(main_path: &Path) -> Self {
        ClaudeAdapter {
            session: main_path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            subagents_dir: subagents_dir(main_path),
            main_path: main_path.to_path_buf(),
            main_offset: 0,
            started: false,
            subs: HashMap::new(),
            linked: HashMap::new(),
        }
    }

    fn ev(&self, ts: i64, body: Body) -> Event {
        Event::new(Harness::Claude, &self.session, ts, body)
    }

    /// Parse new main-transcript lines: spawns, the results of known spawns,
    /// and the orchestrator's billed calls.
    fn scan_main(&mut self, out: &mut Vec<Event>) {
        for line in tail(&self.main_path, &mut self.main_offset) {
            let spawn_line = line.contains("\"name\":\"Agent\"") || line.contains("\"name\":\"Task\"");
            let result_line =
                line.contains("tool_result") && self.subs.keys().any(|id| line.contains(id.as_str()));
            let usage_line = line.contains("\"usage\"");
            if !spawn_line && !result_line && !usage_line {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
            let at = ts.unwrap_or_else(now_ms);
            let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("assistant") => {
                    // Older builds inlined subagent turns as sidechains; those
                    // are counted from the subagent side, not here.
                    if v.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                        if let Some(b) = usage_body(&v, MAIN) {
                            out.push(self.ev(at, b));
                        }
                    }
                    for block in content {
                        let name = block.get("name").and_then(Value::as_str);
                        if block.get("type").and_then(Value::as_str) != Some("tool_use")
                            || !matches!(name, Some("Agent") | Some("Task"))
                        {
                            continue;
                        }
                        let Some(id) = block.get("id").and_then(Value::as_str) else {
                            continue;
                        };
                        let input = block.get("input");
                        let field = |k: &str| {
                            input
                                .and_then(|i| i.get(k))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string()
                        };
                        let sub = self.ensure(id, &field("subagent_type"), &field("description"));
                        if sub.prompt.is_empty() {
                            sub.prompt = field("prompt");
                        }
                        if sub.alias.is_empty() {
                            sub.alias = field("name");
                        }
                        self.announce(id, at, true, out);
                    }
                }
                Some("user") => {
                    let tur = v.get("toolUseResult");
                    let agent_id = tur.and_then(|t| t.get("agentId")).and_then(Value::as_str);
                    let launched = tur
                        .and_then(|t| t.get("status"))
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.contains("launched"));
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        let Some(id) = block.get("tool_use_id").and_then(Value::as_str) else {
                            continue;
                        };
                        if !self.subs.contains_key(id) {
                            continue;
                        }
                        if let Some(aid) = agent_id {
                            self.link(aid, id, out);
                        }
                        // A non-launch result is a foreground agent returning.
                        if !launched {
                            let error = block.get("is_error").and_then(Value::as_bool) == Some(true);
                            let len = block.get("content").map_or(0, |c| c.to_string().len());
                            let text = block.get("content").map(content_text);
                            let Some(sub) = self.subs.get_mut(id) else {
                                continue;
                            };
                            sub.finished = true;
                            // a handback already carried the result: keep it
                            let fresh = !sub.handed_back;
                            let body = Body::AgentFinished {
                                agent: id.to_string(),
                                result: text.filter(|_| fresh),
                                bytes: fresh.then_some(len as u64),
                                error,
                            };
                            out.push(self.ev(at, body));
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Point a spawn at its subagent transcript.
    fn link(&mut self, agent_id: &str, tool_use_id: &str, out: &mut Vec<Event>) {
        if self.linked.contains_key(agent_id) {
            return;
        }
        let path = self.subagents_dir.join(format!("agent-{agent_id}.jsonl"));
        if let Some(sub) = self.subs.get_mut(tool_use_id) {
            if sub.sub_path.is_none() {
                sub.sub_path = Some(path);
                sub.agent_ids.push(agent_id.to_string());
                self.linked.insert(agent_id.to_string(), tool_use_id.to_string());
                self.announce(tool_use_id, now_ms(), false, out);
            }
        }
    }

    /// Scan the subagents dir: link new transcripts, read finished sidecars,
    /// and tail every linked transcript for new activity.
    fn scan_subagents(&mut self, out: &mut Vec<Event>) {
        let Ok(entries) = std::fs::read_dir(&self.subagents_dir) else {
            return;
        };
        let mut metas = Vec::new();
        let mut unlinked = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            let Some(name) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let Some(rest) = name.strip_prefix("agent-") else {
                continue;
            };
            if let Some(id) = rest.strip_suffix(".meta.json") {
                metas.push((id.to_string(), p.clone()));
            } else if let Some(id) = rest.strip_suffix(".jsonl") {
                if !self.linked.contains_key(id) {
                    unlinked.push((id.to_string(), p.clone()));
                }
            }
        }
        // Running agents have no sidecar yet: match by prompt instead.
        for (agent_id, path) in unlinked {
            if let Some(prompt) = first_prompt(&path) {
                let hit = self
                    .subs
                    .iter()
                    .find(|(_, a)| a.sub_path.is_none() && !a.prompt.is_empty() && a.prompt.trim() == prompt.trim())
                    .map(|(id, _)| id.clone());
                if let Some(tool_use_id) = hit {
                    self.link(&agent_id, &tool_use_id, out);
                }
            }
        }
        for (agent_id, path) in metas {
            self.load_meta(&agent_id, &path, out);
        }
        let ids: Vec<String> = self.subs.keys().cloned().collect();
        for id in &ids {
            self.tail_subagent(id, out);
        }
        // Finished per its sidecar, with no handback or result seen: it ended
        // at its last activity.
        for id in &ids {
            let Some(sub) = self.subs.get_mut(id) else {
                continue;
            };
            if sub.meta_finished && !sub.finished {
                sub.finished = true;
                let at = sub.last_ts.or(sub.first_ts).unwrap_or_else(now_ms);
                let body = Body::AgentFinished { agent: id.clone(), result: None, bytes: None, error: false };
                out.push(self.ev(at, body));
            }
        }
    }

    fn load_meta(&mut self, agent_id: &str, meta: &Path, out: &mut Vec<Event>) {
        let Ok(text) = std::fs::read_to_string(meta) else {
            return;
        };
        let Ok(v) = serde_json::from_str::<Value>(&text) else {
            return;
        };
        let Some(tool_use_id) = v.get("toolUseId").and_then(Value::as_str) else {
            return;
        };
        let s = |k: &str| v.get(k).and_then(Value::as_str).unwrap_or("").to_string();
        self.ensure(tool_use_id, &s("agentType"), &s("description"));
        self.link(agent_id, tool_use_id, out);
        self.announce(tool_use_id, now_ms(), false, out);
        if v.get("finished").and_then(Value::as_bool) == Some(true) {
            if let Some(sub) = self.subs.get_mut(tool_use_id) {
                sub.meta_finished = true;
            }
        }
    }

    fn tail_subagent(&mut self, tool_use_id: &str, out: &mut Vec<Event>) {
        let session = self.session.clone();
        let ev = |ts: i64, body: Body| Event::new(Harness::Claude, &session, ts, body);
        let Some(sub) = self.subs.get_mut(tool_use_id) else {
            return;
        };
        let agent = tool_use_id.to_string();
        let mut batch: Vec<Event> = Vec::new();
        if let Some(path) = sub.sub_path.clone() {
            for line in tail(&path, &mut sub.sub_offset) {
                // Any entry at all is a sign of life (stuck detection), so take
                // its timestamp before filtering, without a full parse.
                let qts = quick_timestamp(&line);
                if let Some(ts) = qts {
                    sub.first_ts.get_or_insert(ts);
                    sub.last_ts = Some(sub.last_ts.map_or(ts, |l| l.max(ts)));
                }
                let before = batch.len();
                sub_line(&line, &agent, sub, &mut batch, &ev);
                // nothing else said it was active: a heartbeat does
                let quiet = batch[before..].iter().all(|e| matches!(e.body, Body::AgentFinished { .. }));
                if let (true, Some(ts)) = (quiet, qts) {
                    batch.push(ev(ts, Body::Heartbeat { agent: agent.clone() }));
                }
            }
        }
        // A spawn not seen in the main transcript (sidecar only) is announced
        // before its first activity.
        if sub.announced.is_none() {
            let at = sub.first_ts.unwrap_or_else(now_ms);
            self.announce(tool_use_id, at, true, out);
        }
        out.extend(batch);
    }

    fn ensure(&mut self, tool_use_id: &str, atype: &str, desc: &str) -> &mut Sub {
        let entry = self.subs.entry(tool_use_id.to_string()).or_default();
        // keep the most specific type/description we've seen
        if !atype.is_empty() && (entry.agent_type.is_empty() || entry.agent_type == "general-purpose") {
            entry.agent_type = atype.to_string();
        }
        if entry.description.is_empty() {
            entry.description = desc.to_string();
        }
        entry
    }

    /// Emit `agent_spawned` if it's new (and `first` allows announcing it)
    /// or what we know about it changed since we last did.
    fn announce(&mut self, tool_use_id: &str, at: i64, first: bool, out: &mut Vec<Event>) {
        let Some(sub) = self.subs.get_mut(tool_use_id) else {
            return;
        };
        if sub.announced.is_none() && !first {
            return;
        }
        let role = role_name(&sub.agent_type, &sub.description);
        let now = (
            role.clone(),
            sub.agent_type.clone(),
            sub.description.clone(),
            sub.prompt.clone(),
            sub.alias.clone(),
            sub.agent_ids.len(),
        );
        if sub.announced.as_ref() == Some(&now) {
            return;
        }
        sub.announced = Some(now);
        let body = Body::AgentSpawned {
            agent: tool_use_id.to_string(),
            parent: Some(MAIN.to_string()),
            role,
            agent_type: sub.agent_type.clone(),
            task: sub.description.clone(),
            prompt: sub.prompt.clone(),
            alias: sub.alias.clone(),
            ids: sub.agent_ids.clone(),
        };
        out.push(self.ev(at, body));
    }
}

/// One subagent transcript line → events for `agent`.
fn sub_line(line: &str, agent: &str, sub: &mut Sub, out: &mut Vec<Event>, ev: &dyn Fn(i64, Body) -> Event) {
    // tool_use blocks, tool_result blocks (which carry "tool_use_id"),
    // and billed assistant messages
    if !line.contains("tool_use") && !line.contains("\"usage\"") {
        return;
    }
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    let ts = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
    let at = ts.unwrap_or_else(now_ms);
    let rejected = v.pointer("/permissionDecision/decision").and_then(Value::as_str) == Some("reject");
    let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
        return;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("assistant") => {
            if let Some(b) = usage_body(&v, agent) {
                out.push(ev(at, b));
            }
            let text = content_text(&Value::Array(content.clone()));
            if !text.trim().is_empty() {
                out.push(ev(at, Body::Output { agent: agent.to_string(), text }));
            }
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                    continue;
                }
                let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                if !id.is_empty() && !sub.seen.insert(id.to_string()) {
                    continue; // already emitted
                }
                if name == "SubagentHandback" {
                    let msg = block.pointer("/input/message").and_then(Value::as_str);
                    sub.finished = true;
                    sub.handed_back |= msg.is_some();
                    out.push(ev(
                        at,
                        Body::AgentFinished {
                            agent: agent.to_string(),
                            result: msg.map(str::to_string),
                            bytes: msg.map(|m| m.len() as u64),
                            error: false,
                        },
                    ));
                    continue;
                }
                let cmd = block.pointer("/input/command").and_then(Value::as_str);
                out.push(ev(
                    at,
                    Body::ToolStarted {
                        agent: agent.to_string(),
                        call: (!id.is_empty()).then(|| id.to_string()),
                        tool: short_tool(name),
                        label: tool_label(name, block),
                        kind: tool_kind(name),
                        file: touched_file(name, block),
                        edit: is_edit(name),
                        arg: short_arg(name, block),
                        blocking: can_prompt(name),
                        test: name == "Bash" && !id.is_empty() && cmd.is_some_and(is_test_command),
                    },
                ));
                if let Some((to, len)) = team_message(name, block) {
                    out.push(ev(
                        at,
                        Body::Handoff { agent: agent.to_string(), to, what: "message".into(), bytes: len as u64 },
                    ));
                }
            }
        }
        Some("user") => {
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                    continue;
                }
                out.push(ev(
                    at,
                    Body::ToolFinished {
                        agent: agent.to_string(),
                        call: block.get("tool_use_id").and_then(Value::as_str).map(str::to_string),
                        // A Bash call that exits non-zero comes back is_error.
                        error: block.get("is_error").and_then(Value::as_bool) == Some(true),
                        denied: is_denial(block),
                    },
                ));
            }
            // The user turned the call down at the permission prompt.
            if rejected {
                out.push(ev(at, Body::AgentError { agent: agent.to_string(), message: "rejected".into() }));
            }
        }
        _ => {}
    }
}

// ---- helpers ---------------------------------------------------------------

/// An assistant entry's model and token usage, as a `usage` event. The same
/// message id appears once per content block, each repeating the usage; the
/// schema's "same id updates the call" rule dedupes them. An entry with a
/// model but no usage still names the agent's model (zero tokens).
fn usage_body(v: &Value, agent: &str) -> Option<Body> {
    let msg = v.get("message")?;
    let m = msg.get("model").and_then(Value::as_str).unwrap_or("");
    let model = (!m.is_empty() && !m.starts_with('<')).then(|| m.to_string());
    let usage = msg.get("usage").map(parse_usage).unwrap_or_default();
    if model.is_none() && usage == Tokens::default() {
        return None;
    }
    let id = msg
        .get("id")
        .or_else(|| v.get("requestId"))
        .or_else(|| v.get("uuid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    Some(Body::Usage {
        agent: agent.to_string(),
        id,
        model,
        input: usage.0,
        output: usage.1,
        cache_read: usage.2,
        cache_write_5m: usage.3,
        cache_write_1h: usage.4,
    })
}

/// (input, output, cache_read, cache_write_5m, cache_write_1h)
type Tokens = (u64, u64, u64, u64, u64);

/// An API `usage` object to token counts. Cache writes are split by TTL when
/// the breakdown is present; otherwise all writes bill at the 5-minute rate.
fn parse_usage(u: &Value) -> Tokens {
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let writes = n("/cache_creation_input_tokens");
    let (w5, w1) = match u.get("cache_creation") {
        Some(_) => {
            let w1 = n("/cache_creation/ephemeral_1h_input_tokens");
            (writes.saturating_sub(w1), w1)
        }
        None => (writes, 0),
    };
    (n("/input_tokens"), n("/output_tokens"), n("/cache_read_input_tokens"), w5, w1)
}

/// A direct message from one teammate to another (agent teams): the
/// recipient and the body's size. Tolerant of field names, since the tool's
/// input shape isn't documented: `to` / `recipient` / `target`, and
/// `message` / `content` / `text`. Broadcasts ("*", "all", "team") are not
/// one-to-one handoffs and are skipped.
fn team_message(name: &str, block: &Value) -> Option<(String, usize)> {
    if !matches!(name, "SendMessage" | "SendMessageToTeammate" | "MessageTeammate") {
        return None;
    }
    let get = |k: &str| block.pointer(&format!("/input/{k}"));
    let to = ["to", "recipient", "target", "teammate"]
        .iter()
        .find_map(|k| get(k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|t| !t.is_empty() && !matches!(*t, "*" | "all" | "team" | "everyone"))?;
    let len = ["message", "content", "text", "body"]
        .iter()
        .find_map(|k| get(k))
        .map_or(0, |v| v.as_str().map_or_else(|| v.to_string().len(), str::len));
    Some((to.to_string(), len))
}

/// Tools that modify files.
fn is_edit(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "MultiEdit" | "NotebookEdit")
}

/// The file a file tool read or wrote (search tools take a directory, skip them).
fn touched_file(name: &str, block: &Value) -> Option<String> {
    if !matches!(name, "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "NotebookRead") {
        return None;
    }
    ["/input/file_path", "/input/notebook_path", "/input/path"]
        .iter()
        .find_map(|p| block.pointer(p).and_then(Value::as_str))
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// The text of a message `content` (a string, or an array of blocks whose
/// `text` fields are joined).
fn content_text(c: &Value) -> String {
    match c {
        Value::String(s) => s.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// `"timestamp":"…"` from a raw line without parsing the JSON.
fn quick_timestamp(line: &str) -> Option<i64> {
    let i = line.find("\"timestamp\":\"")? + 13;
    iso_to_ms(line.get(i..)?)
}

/// The first timestamp in a transcript's opening lines.
fn first_timestamp(path: &Path) -> Option<i64> {
    let f = File::open(path).ok()?;
    BufReader::new(f).lines().take(80).map_while(Result::ok).find_map(|l| quick_timestamp(&l))
}

/// A few words saying what a tool call was about, for the detail view.
fn short_arg(name: &str, block: &Value) -> String {
    let get = |k: &str| block.pointer(&format!("/input/{k}")).and_then(Value::as_str).unwrap_or("");
    let s = match name {
        "Bash" => get("command").split_whitespace().take(3).collect::<Vec<_>>().join(" "),
        "Read" | "Edit" | "Write" | "MultiEdit" | "NotebookEdit" | "NotebookRead" => {
            let f = [get("file_path"), get("notebook_path"), get("path")]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("");
            f.rsplit('/').next().unwrap_or(f).to_string()
        }
        "Grep" | "Glob" => get("pattern").to_string(),
        "WebFetch" => {
            let url = get("url");
            let rest = url.split("://").nth(1).unwrap_or(url);
            rest.split('/').next().unwrap_or(rest).to_string()
        }
        "WebSearch" => get("query").to_string(),
        "Agent" | "Task" => get("description").to_string(),
        _ => String::new(),
    };
    s.lines().next().unwrap_or("").to_string()
}

/// Stats name for a tool: lowercase; MCP tools keep only their own name.
fn short_tool(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_lowercase()
}

/// `<dir>/<session-id>/subagents` beside a `<dir>/<session-id>.jsonl` transcript.
pub fn subagents_dir(main: &Path) -> PathBuf {
    let stem = main.file_stem().map(|s| s.to_os_string()).unwrap_or_default();
    main.parent()
        .map(|p| p.join(stem).join("subagents"))
        .unwrap_or_else(|| PathBuf::from("subagents"))
}

/// The prompt text a subagent transcript starts with (its first entry).
fn first_prompt(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    let mut line = String::new();
    BufReader::new(f).read_line(&mut line).ok()?;
    let v: Value = serde_json::from_str(line.trim_end()).ok()?;
    match v.pointer("/message/content")? {
        Value::String(s) => Some(s.clone()),
        Value::Array(parts) => Some(
            parts
                .iter()
                .filter_map(|p| p.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n"),
        ),
        _ => None,
    }
}

/// A tool result that was denied or blocked by permissions — not an ordinary
/// command failure (a failing test, or a shell "Permission denied", is routine).
fn is_denial(block: &Value) -> bool {
    if block.get("is_error").and_then(Value::as_bool) != Some(true) {
        return false;
    }
    let text = block
        .get("content")
        .map(|c| c.to_string().to_lowercase())
        .unwrap_or_default();
    text.contains("<tool_use_error>")
        && (text.contains("denied")
            || text.contains("permission settings")
            || text.contains("not allowed")
            || text.contains("rejected"))
}

/// Read a transcript's real project `cwd` (scans the first lines), so the
/// header shows the actual directory even when resolving by session id.
pub fn read_cwd(path: &Path) -> Option<PathBuf> {
    let f = File::open(path).ok()?;
    for line in BufReader::new(f).lines().take(80).map_while(Result::ok) {
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str).filter(|c| !c.is_empty()) {
                return Some(PathBuf::from(cwd));
            }
        }
    }
    None
}

/// Tools that can block on a permission prompt (mutating / external). Pure
/// reads return instantly and never prompt, so a pending read isn't a wait.
fn can_prompt(name: &str) -> bool {
    matches!(
        name,
        "Bash" | "Write" | "Edit" | "MultiEdit" | "NotebookEdit" | "WebFetch" | "WebSearch"
    )
}

/// Map a Claude Code tool name to a face-driving kind.
fn tool_kind(name: &str) -> ToolKind {
    match name {
        "Read" | "Grep" | "Glob" | "LS" | "NotebookRead" => ToolKind::Look,
        _ => ToolKind::Run,
    }
}

/// Short lowercase label for a tool. For Bash, prefer the command's first word.
fn tool_label(name: &str, block: &Value) -> String {
    if name == "Bash" {
        if let Some(cmd) = block.pointer("/input/command").and_then(Value::as_str) {
            if let Some(word) = cmd.split_whitespace().next() {
                let w = word.rsplit('/').next().unwrap_or(word);
                if !w.is_empty() && w.len() <= 10 {
                    return w.to_string();
                }
            }
        }
        return "bash".into();
    }
    name.to_lowercase()
}

/// Role display name from the agent type, using the description as a hint.
/// Depends only on the spawn (never on tools seen later), so a name never
/// changes mid-run.
fn role_name(agent_type: &str, desc: &str) -> String {
    match agent_type.to_lowercase().as_str() {
        "explore" => "scout".into(),
        "plan" => "architect".into(),
        "general-purpose" | "" => {
            // An explicit "role: task" description wins over the heuristics.
            if let Some(role) = role_prefix(desc) {
                role
            } else if looks_researchy(desc) {
                "researcher".into()
            } else {
                "operator".into()
            }
        }
        // custom subagent_type (scout, test-runner, reviewer, fixer, researcher…):
        // use it as its own role name, trimmed of any "-agent" suffix noise.
        other => other.trim_end_matches("-agent").to_string(),
    }
}

/// Parse a leading "role:" from a description, e.g. "test-runner: run cargo
/// tests" -> "test-runner". The role must be a single word (letters, digits,
/// '-' or '_'), so prose like "Phase 1: explore" is left alone.
fn role_prefix(desc: &str) -> Option<String> {
    let (head, rest) = desc.split_once(':')?;
    let role = head.trim();
    let ok = !role.is_empty()
        && role.len() <= 24
        && !rest.trim().is_empty()
        && role
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    ok.then(|| role.to_lowercase())
}

fn looks_researchy(desc: &str) -> bool {
    let d = desc.to_lowercase();
    ["research", "web", "look up", "latest", "docs"]
        .iter()
        .any(|k| d.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn general_purpose_uses_role_prefix() {
        assert_eq!(role_name("general-purpose", "test-runner: run cargo tests"), "test-runner");
        assert_eq!(role_name("general-purpose", "Reviewer: check diff"), "reviewer");
    }

    #[test]
    fn general_purpose_falls_back_without_prefix() {
        assert_eq!(role_name("general-purpose", "List Cargo dependencies"), "operator");
        assert_eq!(role_name("general-purpose", "Phase 1: explore"), "operator");
        assert_eq!(role_name("general-purpose", "research crate versions"), "researcher");
        assert_eq!(role_name("general-purpose", "fixer:"), "operator");
    }

    #[test]
    fn builtins_and_custom_types_unchanged() {
        assert_eq!(role_name("Explore", "test-runner: x"), "scout");
        assert_eq!(role_name("Plan", ""), "architect");
        assert_eq!(role_name("reviewer", ""), "reviewer");
    }

    #[test]
    fn denial_vs_ordinary_failure() {
        let denied: Value = serde_json::json!({"is_error": true,
            "content": "<tool_use_error>File is in a directory that is denied by your permission settings.</tool_use_error>"});
        let shell: Value = serde_json::json!({"is_error": true,
            "content": "Exit code 1\nmkdir: /tmp/x: Permission denied"});
        let ok: Value = serde_json::json!({"is_error": false, "content": "fine"});
        assert!(is_denial(&denied));
        assert!(!is_denial(&shell));
        assert!(!is_denial(&ok));
    }

    #[test]
    fn iso_parse() {
        assert_eq!(iso_to_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        // cross-checked with `date -j -u -f %Y-%m-%dT%H:%M:%S 2026-11-19T06:21:38 +%s`
        assert_eq!(iso_to_ms("2026-11-19T06:21:38.273Z"), Some(1_795_069_298_273));
        assert_eq!(iso_to_ms("garbage"), None);
    }

    /// End-to-end on a synthetic session: a background agent that is still
    /// running (no meta.json yet) is linked by prompt and shows live activity.
    #[test]
    fn running_background_agent_is_detected() {
        let dir = std::env::temp_dir().join(format!("aflow-test-{}", std::process::id()));
        let sub = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let main = dir.join("sess.jsonl");
        std::fs::write(
            &main,
            concat!(
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{"subagent_type":"general-purpose","description":"test-runner: run tests","prompt":"run the tests"}}]}}"#,
                "\n",
                r#"{"type":"user","timestamp":"2026-01-01T00:00:00.100Z","toolUseResult":{"status":"async_launched","agentId":"zz9"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"launched"}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        // A second, unlinked agent found only by its prompt.
        std::fs::write(
            &main,
            std::fs::read_to_string(&main).unwrap()
                + r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"content":[{"type":"tool_use","id":"toolu_B","name":"Agent","input":{"subagent_type":"Explore","description":"map","prompt":"map the repo"}}]}}"#
                + "\n",
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-zz9.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-01-01T00:00:00.200Z","message":{"content":"run the tests"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.500Z","message":{"content":[{"type":"tool_use","id":"t1","name":"Bash","input":{"command":"cargo test"}}]}}"#,
                "\n"
            ),
        )
        .unwrap();
        std::fs::write(
            sub.join("agent-yy8.jsonl"),
            concat!(
                r#"{"type":"user","timestamp":"2026-01-01T00:00:01.100Z","message":{"content":"map the repo"}}"#,
                "\n",
                r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.300Z","message":{"content":[{"type":"tool_use","id":"t2","name":"Grep","input":{}}]}}"#,
                "\n"
            ),
        )
        .unwrap();

        let resolved = Resolved { path: main, project_dir: None, fell_back: false };
        let mut live = Live::new(&resolved, "h".into());
        live.refresh();
        let s = live.build(&Pricing::default());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(s.agents.len(), 2);
        let tr = &s.agents[0];
        assert_eq!(tr.name, "test-runner");
        assert!(tr.finish.is_none(), "still running");
        assert_eq!(tr.tools.last().map(|t| t.name.as_str()), Some("cargo"));
        let sc = &s.agents[1];
        assert_eq!(sc.name, "scout");
        assert_eq!(sc.tools.last().map(|t| t.name.as_str()), Some("grep"));
        // the Bash call has been pending for "years" (2026-01-01): it's waiting
        assert!(tr.wait.is_some());
        assert!(sc.wait.is_none(), "reads never count as waiting");
    }

    /// Usage is deduped per message id (Claude Code logs one entry per content
    /// block, each repeating the usage), priced by model, and tool activity
    /// feeds files touched, edits, and test outcomes.
    #[test]
    fn usage_files_and_tests_from_a_subagent() {
        let dir = std::env::temp_dir().join(format!("aflow-usage-{}", std::process::id()));
        let sub = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let main = dir.join("sess.jsonl");
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"id":"m0","model":"claude-opus-5-5","usage":{"input_tokens":1000000,"output_tokens":0},"content":[{"type":"tool_use","id":"toolu_A","name":"Agent","input":{"subagent_type":"fixer","description":"fix","prompt":"fix it"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.100Z","toolUseResult":{"status":"async_launched","agentId":"q1"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_A","content":"launched"}]}}"#,
        ];
        std::fs::write(&main, lines.join("\n") + "\n").unwrap();
        let sub_lines = [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:00.200Z","message":{"content":"fix it"}}"#,
            // one message, two content blocks: logged twice with the same usage
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"input_tokens":0,"output_tokens":1000000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"text","text":"running"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:01.000Z","message":{"id":"m1","model":"claude-haiku-5-5","usage":{"input_tokens":0,"output_tokens":1000000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0},"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cargo test -q"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:02.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"b1","is_error":true,"content":"Exit code 101\ntest failed"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:03.000Z","message":{"id":"m2","model":"claude-haiku-5-5","usage":{"input_tokens":0,"output_tokens":0,"cache_read_input_tokens":1000000,"cache_creation_input_tokens":2000000,"cache_creation":{"ephemeral_5m_input_tokens":1000000,"ephemeral_1h_input_tokens":1000000}},"content":[{"type":"tool_use","id":"e1","name":"Edit","input":{"file_path":"/w/src/lib.rs","old_string":"a","new_string":"b"}},{"type":"tool_use","id":"b2","name":"Bash","input":{"command":"cargo test -q"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:04.000Z","message":{"content":[{"type":"tool_result","tool_use_id":"e1","content":"ok"},{"type":"tool_result","tool_use_id":"b2","content":"test result: ok"}]}}"#,
        ];
        std::fs::write(sub.join("agent-q1.jsonl"), sub_lines.join("\n") + "\n").unwrap();

        let resolved = Resolved { path: main, project_dir: None, fell_back: false };
        let mut live = Live::new(&resolved, "h".into());
        live.refresh();
        let s = live.build(&Pricing::default());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(s.orch_model, "opus");
        assert_eq!(s.orch_calls.len(), 1);
        assert!((s.orch_calls[0].cost - 4.0).abs() < 1e-9, "1M opus input = $4");
        let a = &s.agents[0];
        assert_eq!(a.model, "haiku");
        assert_eq!(a.calls.len(), 2, "duplicate message entries count once");
        // m1: 1M haiku output = $0.50; m2: 1M read $0.01 + 1M 5m write $0.125 + 1M 1h write $0.20
        assert!((a.calls[0].cost - 0.50).abs() < 1e-9);
        assert!((a.calls[1].cost - 0.335).abs() < 1e-9);
        assert!((a.calls[1].saved - 0.09).abs() < 1e-9);
        assert_eq!(a.uses.iter().filter(|u| u.edit).count(), 1);
        assert_eq!(a.uses.iter().find(|u| u.edit).unwrap().file.as_deref(), Some("/w/src/lib.rs"));
        let outcomes: Vec<bool> = a.tests.iter().map(|r| r.passed).collect();
        assert_eq!(outcomes, vec![false, true]);
        assert!(crate::stats::unlocks(&s, i64::MAX - 1)
            .iter()
            .any(|u| u.kind == crate::stats::Kind::BugHunter));
        assert_eq!(s.id.as_deref(), Some("sess"));
        assert_eq!(a.task, "fix");
        assert_eq!(a.uses.iter().map(|u| u.arg.as_str()).collect::<Vec<_>>(), ["cargo test -q", "lib.rs", "cargo test -q"]);
        // the latest entry of any kind (here a tool_result at 00:00:04) is activity
        assert_eq!(a.last_seen, Some(4_000));
        assert!(a.result.is_none(), "still running: no result yet");
    }

    /// Live: a tool call with no result yet is in flight; a result quoted in
    /// the next spawn's prompt is a handoff; a teammate's SendMessage to
    /// another teammate is one too.
    #[test]
    fn in_flight_calls_handoffs_and_team_messages() {
        let dir = std::env::temp_dir().join(format!("aflow-handoff-{}", std::process::id()));
        let sub = dir.join("sess").join("subagents");
        std::fs::create_dir_all(&sub).unwrap();
        let main = dir.join("sess.jsonl");
        let review = "Verdict: changes requested\\n- parse_header() splits on every colon and truncates values\\n- no regression test covers a value like Host: a:b";
        let lines = [
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:00.000Z","message":{"content":[{"type":"tool_use","id":"toolu_R","name":"Agent","input":{"subagent_type":"reviewer","name":"rev","description":"review","prompt":"review the parser"}}]}}"#.to_string(),
            format!(r#"{{"type":"user","timestamp":"2026-01-01T00:00:05.000Z","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_R","content":"{review}"}}]}}}}"#),
            format!(r#"{{"type":"assistant","timestamp":"2026-01-01T00:00:06.000Z","message":{{"content":[{{"type":"tool_use","id":"toolu_F","name":"Agent","input":{{"subagent_type":"fixer","name":"fix","description":"fix","prompt":"Fix these findings:\n\n{review}"}}}}]}}}}"#),
            r#"{"type":"user","timestamp":"2026-01-01T00:00:06.100Z","toolUseResult":{"status":"async_launched","agentId":"f1"},"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_F","content":"launched"}]}}"#.to_string(),
        ];
        std::fs::write(&main, lines.join("\n") + "\n").unwrap();
        let sub_lines = [
            r#"{"type":"user","timestamp":"2026-01-01T00:00:06.200Z","message":{"content":"x"}}"#,
            // a message to the reviewer teammate, then a build that never returns
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:07.000Z","message":{"content":[{"type":"tool_use","id":"m1","name":"SendMessage","input":{"to":"rev","message":"is a:b the only case?"}}]}}"#,
            r#"{"type":"user","timestamp":"2026-01-01T00:00:07.100Z","message":{"content":[{"type":"tool_result","tool_use_id":"m1","content":"sent"}]}}"#,
            r#"{"type":"assistant","timestamp":"2026-01-01T00:00:08.000Z","message":{"content":[{"type":"tool_use","id":"b1","name":"Bash","input":{"command":"cargo build --release"}}]}}"#,
        ];
        std::fs::write(sub.join("agent-f1.jsonl"), sub_lines.join("\n") + "\n").unwrap();

        let resolved = Resolved { path: main, project_dir: None, fell_back: false };
        let mut live = Live::new(&resolved, "h".into());
        live.refresh();
        let s = live.build(&Pricing::default());
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(s.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["reviewer", "fixer"]);
        let (rev, fix) = (&s.agents[0], &s.agents[1]);
        assert!(rev.in_flight.is_none(), "finished");
        assert_eq!(fix.in_flight, Some(8_000), "the build has no result yet");
        assert!(fix.ids.contains(&"toolu_F".to_string()) && fix.ids.contains(&"f1".to_string()));
        // stuck forever by the clock, but busy: running, not dozing
        let t = 8_000 + 600_000;
        assert!(crate::stats::stuck_idle(fix, t, 120_000).is_none());
        assert!(crate::stats::running_long(fix, t, 120_000).is_some());

        let hs: Vec<(usize, usize, i64, &str)> =
            s.handoffs.iter().map(|h| (h.from, h.to, h.time, h.what.as_str())).collect();
        assert_eq!(hs, vec![(0, 1, 6_000, "findings"), (1, 0, 7_000, "message")]);
        assert!(s.events.iter().any(|e| e.label == "reviewer \u{2192} fixer" && e.detail.starts_with("findings")));
    }

    #[test]
    fn team_message_shapes() {
        let msg = |input: Value| serde_json::json!({"type": "tool_use", "name": "SendMessage", "input": input});
        assert_eq!(team_message("SendMessage", &msg(serde_json::json!({"to": "qa", "message": "hi"}))), Some(("qa".into(), 2)));
        assert_eq!(team_message("SendMessage", &msg(serde_json::json!({"recipient": "qa", "content": "abc"}))), Some(("qa".into(), 3)));
        assert_eq!(team_message("SendMessage", &msg(serde_json::json!({"to": "*", "message": "all"}))), None, "broadcast");
        assert_eq!(team_message("Bash", &msg(serde_json::json!({"to": "qa"}))), None);
    }

    #[test]
    fn quick_timestamp_and_content_text() {
        assert_eq!(
            quick_timestamp(r#"{"type":"user","timestamp":"1970-01-01T00:00:02.000Z","x":1}"#),
            Some(2_000)
        );
        assert_eq!(quick_timestamp(r#"{"type":"user"}"#), None);
        let v: Value = serde_json::json!([{"type":"text","text":"a"},{"type":"tool_use"},{"type":"text","text":"b"}]);
        assert_eq!(content_text(&v), "a\nb");
        assert_eq!(content_text(&Value::String("x".into())), "x");
    }

    #[test]
    fn test_commands() {
        assert!(is_test_command("cd x && cargo test --all"));
        assert!(is_test_command("python -m pytest -q"));
        assert!(is_test_command("npm run test -- --watch=false"));
        assert!(!is_test_command("cargo build"));
        assert!(!is_test_command("ls tests/"));
    }
}
