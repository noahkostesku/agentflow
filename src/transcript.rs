//! Live mode: turn a real Claude Code session on disk into a render `Session`.
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
//! Everything is keyed by the spawn's `tool_use` id. Timestamps are epoch ms;
//! the render clock is `now - t0`, where `t0` is the earliest spawn.
//!
//! Handoffs (orbit chords) come from two places: a finished agent's result
//! quoted in a later agent's prompt (`orbit::detect_handoffs`), and, for agent
//! teams, a teammate's `SendMessage` call addressed to another teammate.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::discovery::Resolved;
use crate::model::{Agent, Call, Event, Session, TestRun, Tool, ToolKind, Use, Usage};
use crate::pricing::{self, Pricing};
use crate::theme;

/// A tool call that hasn't returned this long reads as "waiting". We can't see
/// *why* it's blocked (permission prompt vs. slow command), only that it is.
const WAIT_MS: i64 = 10_000;

/// Per-subagent accumulator, updated incrementally across refreshes.
#[derive(Default)]
struct Acc {
    agent_type: String,
    description: String,
    prompt: String,
    spawn_ms: Option<i64>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    /// foreground agents: when the orchestrator received the final result
    done_ts: Option<i64>,
    finished: bool,
    /// the agent's latest outcome was a denied/blocked tool call
    denied: bool,
    /// when the most recent denial happened (drives the error glitch)
    error_ts: Option<i64>,
    size: String,
    tools: Vec<(i64, Tool)>,
    /// tool_use ids already counted (guards against re-emitted entries)
    seen: HashSet<String>,
    /// outstanding tool calls with no result yet: (tool_use_id, ts, can_prompt)
    pending: Vec<(String, i64, bool)>,
    sub_path: Option<PathBuf>,
    sub_offset: u64,
    order: usize,
    /// latest model id seen in its transcript
    model: String,
    /// billed calls keyed by message id (one message is logged once per
    /// content block, each copy repeating the same usage)
    usage: Usage_,
    uses: Vec<Use>,
    /// tool_use ids of Bash calls that ran a test command
    test_ids: HashSet<String>,
    tests: Vec<TestRun>,
    /// final answer (handback message or foreground result)
    result: Option<String>,
    /// latest assistant text, the fallback result preview
    last_text: String,
    /// teammate name given at spawn (`input.name`), for message routing
    alias: String,
    /// direct teammate messages it sent: (ts, recipient, bytes)
    messages: Vec<(i64, String, usize)>,
}

/// message id -> (first seen ms, usage, model)
type Usage_ = HashMap<String, (i64, Usage, String)>;

pub struct Live {
    header: String,
    main_path: PathBuf,
    main_offset: u64,
    subagents_dir: PathBuf,
    /// keyed by the spawn's tool_use id
    accs: HashMap<String, Acc>,
    /// agentId -> spawn tool_use id, for subagent files already linked
    linked: HashMap<String, String>,
    next_order: usize,
    /// when we started watching (clock origin until the first spawn)
    opened: i64,
    /// the orchestrator's own model and billed calls
    orch_model: String,
    orch_usage: Usage_,
    /// the session's project dir: touched files are shown relative to it
    root: Option<String>,
}

impl Live {
    pub fn new(resolved: &Resolved, header: String) -> Self {
        let main_path = resolved.path.clone();
        Live {
            header,
            root: read_cwd(&main_path).map(|p| p.to_string_lossy().trim_end_matches('/').to_string() + "/"),
            subagents_dir: subagents_dir(&main_path),
            main_path,
            main_offset: 0,
            accs: HashMap::new(),
            linked: HashMap::new(),
            next_order: 0,
            opened: now_ms(),
            orch_model: String::new(),
            orch_usage: HashMap::new(),
        }
    }

    /// The transcript this watcher is attached to.
    pub fn path(&self) -> &Path {
        &self.main_path
    }

    /// Re-read what's new on disk. Cheap to call a few times per second.
    pub fn refresh(&mut self) {
        self.scan_main();
        self.scan_subagents();
    }

    /// Parse new main-transcript lines: spawns, and the results of known spawns.
    fn scan_main(&mut self) {
        for line in tail(&self.main_path, &mut self.main_offset) {
            let spawn_line = line.contains("\"name\":\"Agent\"") || line.contains("\"name\":\"Task\"");
            let result_line =
                line.contains("tool_result") && self.accs.keys().any(|id| line.contains(id.as_str()));
            let usage_line = line.contains("\"usage\"");
            if !spawn_line && !result_line && !usage_line {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
            let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("assistant") => {
                    // Older builds inlined subagent turns as sidechains; those
                    // are counted from the subagent side, not here.
                    if v.get("isSidechain").and_then(Value::as_bool) != Some(true) {
                        record_usage(&v, ts, &mut self.orch_model, &mut self.orch_usage);
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
                        let atype = field("subagent_type");
                        let acc = self.ensure(id, &atype, &field("description"));
                        if acc.prompt.is_empty() {
                            acc.prompt = field("prompt");
                        }
                        if acc.alias.is_empty() {
                            acc.alias = field("name");
                        }
                        if acc.spawn_ms.is_none() {
                            acc.spawn_ms = noticed(ts);
                        }
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
                        if !self.accs.contains_key(id) {
                            continue;
                        }
                        if let Some(aid) = agent_id {
                            self.link(aid, id);
                        }
                        // A non-launch result is a foreground agent returning.
                        if !launched {
                            let is_err = block.get("is_error").and_then(Value::as_bool) == Some(true);
                            let len = block.get("content").map_or(0, |c| c.to_string().len());
                            if let Some(acc) = self.accs.get_mut(id) {
                                acc.finished = true;
                                acc.done_ts = noticed(ts);
                                acc.pending.clear();
                                if acc.size.is_empty() {
                                    acc.size = human_size(len);
                                }
                                if let Some(text) = block.get("content").map(content_text) {
                                    acc.result.get_or_insert(text);
                                }
                                if is_err {
                                    acc.denied = true;
                                    acc.error_ts = noticed(ts);
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// Point a spawn at its subagent transcript.
    fn link(&mut self, agent_id: &str, tool_use_id: &str) {
        if self.linked.contains_key(agent_id) {
            return;
        }
        let path = self.subagents_dir.join(format!("agent-{agent_id}.jsonl"));
        if let Some(acc) = self.accs.get_mut(tool_use_id) {
            if acc.sub_path.is_none() {
                acc.sub_path = Some(path);
                self.linked
                    .insert(agent_id.to_string(), tool_use_id.to_string());
            }
        }
    }

    /// Scan the subagents dir: link new transcripts, read finished sidecars,
    /// and tail every linked transcript for new activity.
    fn scan_subagents(&mut self) {
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
                    .accs
                    .iter()
                    .find(|(_, a)| a.sub_path.is_none() && !a.prompt.is_empty() && a.prompt.trim() == prompt.trim())
                    .map(|(id, _)| id.clone());
                if let Some(tool_use_id) = hit {
                    self.link(&agent_id, &tool_use_id);
                }
            }
        }
        for (agent_id, path) in metas {
            self.load_meta(&agent_id, &path);
        }
        let ids: Vec<String> = self.accs.keys().cloned().collect();
        for id in ids {
            self.tail_subagent(&id);
        }
    }

    fn load_meta(&mut self, agent_id: &str, meta: &Path) {
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
        self.link(agent_id, tool_use_id);
        if v.get("finished").and_then(Value::as_bool) == Some(true) {
            if let Some(acc) = self.accs.get_mut(tool_use_id) {
                acc.finished = true;
                acc.pending.clear();
            }
        }
    }

    fn tail_subagent(&mut self, tool_use_id: &str) {
        let Some(acc) = self.accs.get_mut(tool_use_id) else {
            return;
        };
        let Some(path) = acc.sub_path.clone() else {
            return;
        };
        for line in tail(&path, &mut acc.sub_offset) {
            // Any entry at all is a sign of life (stuck detection), so take
            // its timestamp before filtering, without a full parse.
            if let Some(ts) = quick_timestamp(&line) {
                acc.first_ts.get_or_insert(ts);
                acc.last_ts = Some(acc.last_ts.map_or(ts, |l| l.max(ts)));
            }
            // tool_use blocks, tool_result blocks (which carry "tool_use_id"),
            // and billed assistant messages
            if !line.contains("tool_use") && !line.contains("\"usage\"") {
                continue;
            }
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
            let rejected =
                v.pointer("/permissionDecision/decision").and_then(Value::as_str) == Some("reject");
            let Some(content) = v.pointer("/message/content").and_then(Value::as_array) else {
                continue;
            };
            match v.get("type").and_then(Value::as_str) {
                Some("assistant") => {
                    record_usage(&v, ts, &mut acc.model, &mut acc.usage);
                    let text = content_text(&Value::Array(content.clone()));
                    if !text.trim().is_empty() {
                        acc.last_text = text;
                    }
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        let id = block.get("id").and_then(Value::as_str).unwrap_or("");
                        if !id.is_empty() && !acc.seen.insert(id.to_string()) {
                            continue; // already counted
                        }
                        if name == "SubagentHandback" {
                            // A handback right after a denial keeps the error.
                            acc.finished = true;
                            acc.done_ts = noticed(ts);
                            acc.pending.clear();
                            if let Some(msg) = block.pointer("/input/message").and_then(Value::as_str) {
                                acc.size = human_size(msg.len());
                                acc.result = Some(msg.to_string());
                            }
                            continue;
                        }
                        let tts = ts.unwrap_or_else(now_ms);
                        if let Some((to, len)) = team_message(name, block) {
                            acc.messages.push((noticed(ts).unwrap_or(tts), to, len));
                        }
                        acc.denied = false; // it moved on: not stuck on the denial
                        acc.tools.push((
                            tts,
                            Tool {
                                name: tool_label(name, block),
                                kind: tool_kind(name),
                            },
                        ));
                        if !id.is_empty() {
                            acc.pending.push((id.to_string(), tts, can_prompt(name)));
                        }
                        acc.uses.push(Use {
                            time: tts,
                            tool: short_tool(name),
                            file: touched_file(name, block),
                            edit: is_edit(name),
                            arg: short_arg(name, block),
                        });
                        let cmd = block.pointer("/input/command").and_then(Value::as_str);
                        if name == "Bash" && !id.is_empty() && cmd.is_some_and(is_test_command) {
                            acc.test_ids.insert(id.to_string());
                        }
                    }
                }
                Some("user") => {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                            acc.pending.retain(|(pid, _, _)| pid != id);
                            // A Bash call that exits non-zero comes back is_error.
                            if acc.test_ids.remove(id) && !is_denial(block) {
                                acc.tests.push(TestRun {
                                    time: noticed(ts).unwrap_or_else(now_ms),
                                    passed: block.get("is_error").and_then(Value::as_bool) != Some(true),
                                });
                            }
                        }
                        if rejected || is_denial(block) {
                            acc.denied = true;
                            acc.error_ts = noticed(ts);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn ensure(&mut self, tool_use_id: &str, atype: &str, desc: &str) -> &mut Acc {
        let order = self.next_order;
        let entry = self
            .accs
            .entry(tool_use_id.to_string())
            .or_insert_with(|| Acc {
                order,
                ..Acc::default()
            });
        if entry.order == order {
            self.next_order += 1;
        }
        // keep the most specific type/description we've seen
        if !atype.is_empty() && (entry.agent_type.is_empty() || entry.agent_type == "general-purpose") {
            entry.agent_type = atype.to_string();
        }
        if entry.description.is_empty() {
            entry.description = desc.to_string();
        }
        entry
    }

    /// Build the render model for the current instant.
    pub fn build(&self, pricing: &Pricing) -> Session {
        let mut accs: Vec<(&str, &Acc)> = self.accs.iter().map(|(k, a)| (k.as_str(), a)).collect();
        accs.sort_by_key(|(_, a)| (a.spawn_ms.or(a.first_ts).unwrap_or(i64::MAX), a.order));

        let now = now_ms();
        let t0 = self.origin().unwrap_or(now);
        // spawn tool_use id -> agentIds of its transcript
        let mut agent_ids: HashMap<&str, Vec<String>> = HashMap::new();
        for (aid, tid) in &self.linked {
            agent_ids.entry(tid.as_str()).or_default().push(aid.clone());
        }

        let mut agents = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        for &(key, a) in &accs {
            let role = role_name(&a.agent_type, &a.description);
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

            let wait = a
                .pending
                .iter()
                .filter(|(_, _, can)| *can)
                .map(|(_, ts, _)| *ts)
                .min()
                .filter(|pts| !a.finished && !error && now - *pts > WAIT_MS)
                .map(|pts| (pts - t0 + WAIT_MS, i64::MAX));

            events.push(Event {
                time: spawn,
                label: format!("orchestrator \u{2192} {role}"),
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
                    label: format!("{role} \u{2192} orchestrator"),
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
                        file: u.file.as_ref().map(|f| match &self.root {
                            Some(r) => f.strip_prefix(r.as_str()).unwrap_or(f).to_string(),
                            None => f.clone(),
                        }),
                        ..u.clone()
                    })
                    .collect(),
                tests: a.tests.iter().map(|r| TestRun { time: r.time - t0, ..*r }).collect(),
                task: if a.description.trim().is_empty() {
                    a.prompt.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string()
                } else {
                    a.description.trim().to_string()
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
                ids: std::iter::once(key.to_string())
                    .chain(agent_ids.get(key).cloned().unwrap_or_default())
                    .collect(),
            });
        }

        // Handoffs: results quoted in a later prompt, plus teammate messages.
        let mut handoffs = crate::orbit::detect_handoffs(&agents);
        for (from, (_, a)) in accs.iter().enumerate() {
            for (ts, to, len) in &a.messages {
                let target = to.trim().to_lowercase();
                let hit = accs.iter().enumerate().find(|(j, (_, b))| {
                    *j != from
                        && (b.alias.to_lowercase() == target
                            || agents[*j].name == target
                            || b.agent_type.to_lowercase() == target)
                });
                if let Some((to, _)) = hit {
                    handoffs.push(crate::model::Handoff {
                        from,
                        to,
                        time: ts - t0,
                        what: "message".into(),
                        size: human_size(*len),
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

        Session {
            header: self.header.clone(),
            id: self.main_path.file_stem().map(|s| s.to_string_lossy().into_owned()),
            agents,
            events,
            all,
            length: 0,
            live: true,
            orch_model: pricing::family(&self.orch_model),
            orch_calls: priced(&self.orch_usage, t0, pricing),
            epoch0: Some(t0),
            handoffs,
        }
    }

    /// Earliest spawn we know about.
    fn origin(&self) -> Option<i64> {
        self.accs.values().filter_map(|a| a.spawn_ms.or(a.first_ts)).min()
    }

    /// The render clock `t` now: ms since the earliest spawn, or since we
    /// started watching if nothing has spawned yet (keeps the face alive).
    pub fn clock(&self) -> i64 {
        now_ms() - self.origin().unwrap_or(self.opened)
    }
}

// ---- helpers ---------------------------------------------------------------

/// Note an assistant entry's model and usage. The same message id appears
/// once per content block; keep its first-seen time and the largest counts.
fn record_usage(v: &Value, ts: Option<i64>, model: &mut String, calls: &mut Usage_) {
    let Some(msg) = v.get("message") else {
        return;
    };
    let m = msg.get("model").and_then(Value::as_str).unwrap_or("");
    if !m.is_empty() && !m.starts_with('<') {
        *model = m.to_string();
    }
    let Some(u) = msg.get("usage") else {
        return;
    };
    let usage = parse_usage(u);
    if usage == Usage::default() {
        return;
    }
    let id = msg
        .get("id")
        .or_else(|| v.get("requestId"))
        .or_else(|| v.get("uuid"))
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let time = noticed(ts).unwrap_or_else(now_ms);
    let e = calls.entry(id).or_insert((time, Usage::default(), m.to_string()));
    e.1.max(&usage);
}

/// An API `usage` object to token counts. Cache writes are split by TTL when
/// the breakdown is present; otherwise all writes bill at the 5-minute rate.
fn parse_usage(u: &Value) -> Usage {
    let n = |p: &str| u.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let writes = n("/cache_creation_input_tokens");
    let (w5, w1) = match u.get("cache_creation") {
        Some(_) => {
            let w1 = n("/cache_creation/ephemeral_1h_input_tokens");
            (writes.saturating_sub(w1), w1)
        }
        None => (writes, 0),
    };
    Usage {
        input: n("/input_tokens"),
        output: n("/output_tokens"),
        cache_read: n("/cache_read_input_tokens"),
        cache_write_5m: w5,
        cache_write_1h: w1,
    }
}

/// Usage map -> priced calls on the session clock, in time order.
fn priced(calls: &Usage_, t0: i64, pricing: &Pricing) -> Vec<Call> {
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

/// Stats name for a tool: lowercase; MCP tools keep only their own name.
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

fn short_tool(name: &str) -> String {
    name.rsplit("__").next().unwrap_or(name).to_lowercase()
}

/// A shell command that runs a test suite.
fn is_test_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    [
        "cargo test", "cargo nextest", "pytest", "py.test", "unittest", "npm test",
        "npm run test", "yarn test", "pnpm test", "bun test", "deno test", "go test",
        "jest", "vitest", "mocha", "rspec", "phpunit", "mix test", "ctest", "make test",
        "make check", "gradle test", "mvn test", "dotnet test", "swift test", "tox",
    ]
    .iter()
    .any(|k| c.contains(k))
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

/// Read appended bytes of a file from `*offset` and return the complete lines,
/// advancing the offset past the last newline. Handles truncation, and never
/// gets stuck on invalid UTF-8.
fn tail(path: &Path, offset: &mut u64) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0; // file shrank: re-read from start
    }
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    *offset += last_nl as u64 + 1;
    String::from_utf8_lossy(&buf[..last_nl])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Live events reach us up to a refresh late. A *recent* event is pinned to
/// the moment we noticed it so its animation (packet, glitch) plays in full;
/// older events, e.g. when attaching mid-session, keep their real time.
fn noticed(ts: Option<i64>) -> Option<i64> {
    let now = now_ms();
    ts.map(|t| if (0..2000).contains(&(now - t)) { now } else { t })
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse "2026-11-19T06:21:38.273Z" (always UTC) to epoch ms.
fn iso_to_ms(s: &str) -> Option<i64> {
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (year, mon, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut ms = 0i64;
    if s.as_bytes().get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(char::is_ascii_digit).take(3).collect();
        if !frac.is_empty() {
            ms = format!("{frac:0<3}").parse().unwrap_or(0);
        }
    }
    let days = days_from_civil(year, mon, day);
    Some((days * 86400 + hour * 3600 + min * 60 + sec) * 1000 + ms)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
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
