//! Live mode: turn a real Claude Code session on disk into a render `Session`.
//!
//! Format (observed in this harness):
//!   * main transcript `<session>.jsonl` — the orchestrator. Each subagent spawn
//!     is an `assistant` entry with a `tool_use` block named `Agent`
//!     (`input.subagent_type`, `input.description`).
//!   * `<session>/subagents/agent-<id>.jsonl` — one per subagent, its own
//!     `tool_use` activity ending in a `SubagentHandback`.
//!   * `<session>/subagents/agent-<id>.meta.json` — sidecar with `agentType`,
//!     `description`, `toolUseId`, `finished`.
//!
//! We read the main file for spawns (so an agent appears the instant it's
//! dispatched, booting), and tail each subagent file for live tool activity and
//! completion. Everything is keyed by `toolUseId`. Timestamps are real epoch ms;
//! the render clock is `now - t0` where `t0` is the earliest spawn.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::discovery::Resolved;
use crate::model::{Agent, Event, Session, Tool, ToolKind};
use crate::theme;

/// Per-subagent accumulator, updated incrementally across refreshes.
struct Acc {
    role: String,
    agent_type: String,
    description: String,
    spawn_ms: Option<i64>,
    first_ts: Option<i64>,
    last_ts: Option<i64>,
    finished: bool,
    error: bool,
    size: String,
    tools: Vec<(i64, Tool)>,
    /// outstanding tool calls with no result yet: (tool_use_id, ts, needs_permission)
    pending: Vec<(String, i64, bool)>,
    sub_path: Option<PathBuf>,
    sub_offset: u64,
    order: usize,
}

pub struct Live {
    header: String,
    main_path: PathBuf,
    main_offset: u64,
    subagents_dir: PathBuf,
    /// keyed by toolUseId
    accs: HashMap<String, Acc>,
    /// agentId(from filename) -> toolUseId, so repeated scans map cheaply
    file_to_id: HashMap<String, String>,
    next_order: usize,
}

impl Live {
    pub fn new(resolved: &Resolved, header: String) -> Self {
        let main_path = resolved.path.clone();
        // subagents live in "<stem>/subagents" beside the transcript.
        let stem = main_path
            .file_stem()
            .map(|s| s.to_os_string())
            .unwrap_or_default();
        let subagents_dir = main_path
            .parent()
            .map(|p| p.join(stem).join("subagents"))
            .unwrap_or_else(|| PathBuf::from("subagents"));
        Live {
            header,
            main_path,
            main_offset: 0,
            subagents_dir,
            accs: HashMap::new(),
            file_to_id: HashMap::new(),
            next_order: 0,
        }
    }

    /// Re-read what's new on disk. Cheap to call a few times per second.
    pub fn refresh(&mut self) {
        self.scan_main();
        self.scan_subagents();
    }

    /// Parse new main-transcript lines for `Agent` spawns.
    fn scan_main(&mut self) {
        for line in tail(&self.main_path, &mut self.main_offset) {
            // cheap prefilter before JSON parse
            if !line.contains("\"Agent\"") || !line.contains("tool_use") {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if v.get("type").and_then(Value::as_str) != Some("assistant") {
                continue;
            }
            let ts = v.get("timestamp").and_then(Value::as_str).and_then(iso_to_ms);
            let content = match v.pointer("/message/content").and_then(Value::as_array) {
                Some(c) => c,
                None => continue,
            };
            for block in content {
                if block.get("type").and_then(Value::as_str) != Some("tool_use")
                    || block.get("name").and_then(Value::as_str) != Some("Agent")
                {
                    continue;
                }
                let id = match block.get("id").and_then(Value::as_str) {
                    Some(s) => s.to_string(),
                    None => continue,
                };
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                let atype = input
                    .get("subagent_type")
                    .and_then(Value::as_str)
                    .unwrap_or("general-purpose")
                    .to_string();
                let desc = input
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let acc = self.ensure(&id, &atype, &desc);
                if acc.spawn_ms.is_none() {
                    acc.spawn_ms = ts;
                }
            }
        }
    }

    /// Scan the subagents dir: read meta sidecars and tail each subagent file.
    fn scan_subagents(&mut self) {
        let entries = match std::fs::read_dir(&self.subagents_dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut meta_files = Vec::new();
        for e in entries.flatten() {
            let p = e.path();
            let name = p.file_name().and_then(|s| s.to_str()).unwrap_or("");
            if name.starts_with("agent-") && name.ends_with(".meta.json") {
                meta_files.push(p);
            }
        }
        for meta in meta_files {
            self.load_meta(&meta);
        }
        // Tail every known subagent file for new activity.
        let ids: Vec<String> = self.accs.keys().cloned().collect();
        for id in ids {
            self.tail_subagent(&id);
        }
    }

    fn load_meta(&mut self, meta: &Path) {
        let fname = meta.file_name().and_then(|s| s.to_str()).unwrap_or("");
        // agent-<agentId>.meta.json
        let agent_id = fname
            .strip_prefix("agent-")
            .and_then(|s| s.strip_suffix(".meta.json"))
            .unwrap_or("")
            .to_string();
        if agent_id.is_empty() {
            return;
        }
        let text = match std::fs::read_to_string(meta) {
            Ok(t) => t,
            Err(_) => return,
        };
        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => return,
        };
        let tool_use_id = match v.get("toolUseId").and_then(Value::as_str) {
            Some(s) => s.to_string(),
            None => return,
        };
        let atype = v
            .get("agentType")
            .and_then(Value::as_str)
            .unwrap_or("general-purpose")
            .to_string();
        let desc = v
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let finished = v.get("finished").and_then(Value::as_bool).unwrap_or(false);

        let sub_path = self.subagents_dir.join(format!("agent-{agent_id}.jsonl"));
        self.file_to_id
            .insert(agent_id.clone(), tool_use_id.clone());
        let acc = self.ensure(&tool_use_id, &atype, &desc);
        if acc.sub_path.is_none() {
            acc.sub_path = Some(sub_path);
        }
        if finished {
            acc.finished = true;
        }
    }

    fn tail_subagent(&mut self, tool_use_id: &str) {
        let (path, mut offset) = match self.accs.get(tool_use_id) {
            Some(a) => match &a.sub_path {
                Some(p) => (p.clone(), a.sub_offset),
                None => return,
            },
            None => return,
        };
        let lines = tail(&path, &mut offset);
        let acc = match self.accs.get_mut(tool_use_id) {
            Some(a) => a,
            None => return,
        };
        acc.sub_offset = offset;
        for line in lines {
            // tool_use blocks, and tool_result blocks (which carry "tool_use_id")
            if !line.contains("tool_use") {
                continue;
            }
            let v: Value = match serde_json::from_str(&line) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let typ = v.get("type").and_then(Value::as_str);
            let ts = v
                .get("timestamp")
                .and_then(Value::as_str)
                .and_then(iso_to_ms);
            if let Some(ts) = ts {
                acc.first_ts.get_or_insert(ts);
                acc.last_ts = Some(acc.last_ts.map_or(ts, |l| l.max(ts)));
            }
            // entry-level permission decision (denial => error)
            if v.pointer("/permissionDecision/decision").and_then(Value::as_str) == Some("reject") {
                acc.error = true;
            }
            let content = match v.pointer("/message/content").and_then(Value::as_array) {
                Some(c) => c,
                None => continue,
            };
            match typ {
                Some("assistant") => {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                            continue;
                        }
                        let name = block.get("name").and_then(Value::as_str).unwrap_or("");
                        if name == "SubagentHandback" {
                            acc.finished = true;
                            acc.pending.clear();
                            if let Some(msg) =
                                block.pointer("/input/message").and_then(Value::as_str)
                            {
                                acc.size = human_size(msg.len());
                            }
                            continue;
                        }
                        let tts = ts.unwrap_or_else(now_ms);
                        acc.tools.push((
                            tts,
                            Tool {
                                name: tool_label(name, block),
                                kind: tool_kind(name),
                            },
                        ));
                        if let Some(id) = block.get("id").and_then(Value::as_str) {
                            acc.pending
                                .push((id.to_string(), tts, needs_permission(name)));
                        }
                    }
                }
                Some("user") => {
                    for block in content {
                        if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                            continue;
                        }
                        // a result resolves the matching pending tool call
                        if let Some(id) = block.get("tool_use_id").and_then(Value::as_str) {
                            acc.pending.retain(|(pid, _, _)| pid != id);
                        }
                        // a *denied* tool (not an ordinary command failure) => error
                        let is_err = block.get("is_error").and_then(Value::as_bool) == Some(true);
                        if is_err {
                            let text = block
                                .get("content")
                                .map(|c| c.to_string().to_lowercase())
                                .unwrap_or_default();
                            if text.contains("tool_use_error")
                                || text.contains("denied")
                                || text.contains("not allowed")
                                || text.contains("permission")
                            {
                                acc.error = true;
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn ensure(&mut self, tool_use_id: &str, atype: &str, desc: &str) -> &mut Acc {
        let order = self.next_order;
        let entry = self.accs.entry(tool_use_id.to_string()).or_insert_with(|| {
            let acc = Acc {
                role: String::new(),
                agent_type: atype.to_string(),
                description: desc.to_string(),
                spawn_ms: None,
                first_ts: None,
                last_ts: None,
                finished: false,
                error: false,
                size: String::new(),
                tools: Vec::new(),
                pending: Vec::new(),
                sub_path: None,
                sub_offset: 0,
                order,
            };
            acc
        });
        if entry.order == order {
            self.next_order += 1;
        }
        // keep the richest type/desc we've seen
        if entry.agent_type.is_empty() || entry.agent_type == "general-purpose" {
            entry.agent_type = atype.to_string();
        }
        if entry.description.is_empty() {
            entry.description = desc.to_string();
        }
        entry.role = role_name(&entry.agent_type, &entry.description, &entry.tools);
        entry
    }

    /// Build the render model for the current instant.
    pub fn build(&self) -> Session {
        let mut accs: Vec<&Acc> = self.accs.values().collect();
        accs.sort_by_key(|a| (a.spawn_ms.or(a.first_ts).unwrap_or(i64::MAX), a.order));

        let now = now_ms();
        // Session origin = earliest spawn we know about.
        let t0 = accs
            .iter()
            .filter_map(|a| a.spawn_ms.or(a.first_ts))
            .min()
            .unwrap_or(now);

        let mut agents = Vec::new();
        let mut events: Vec<Event> = Vec::new();
        for a in &accs {
            let spawn_ms = a.spawn_ms.or(a.first_ts).unwrap_or(t0);
            let spawn = spawn_ms - t0;
            let finish = if a.finished {
                Some(a.last_ts.unwrap_or(spawn_ms) - t0)
            } else {
                None
            };
            let tools: Vec<Tool> = a.tools.iter().map(|(_, t)| t.clone()).collect();
            let live_tool = if tools.is_empty() {
                None
            } else {
                Some(tools.len() - 1)
            };
            let size = if a.size.is_empty() {
                "·".into()
            } else {
                a.size.clone()
            };

            // Live "waiting" state: an outstanding permission-capable tool call
            // with no result for ~10s. The transcript records permission
            // *decisions* only at resolution, so we can't tell whether it's a
            // prompt or just a slow command — hence "waiting", not "permission".
            const WAIT_MS: i64 = 10_000;
            let wait = a
                .pending
                .iter()
                .filter(|(_, _, needs)| *needs)
                .map(|(_, ts, _)| *ts)
                .min()
                .filter(|pts| !a.finished && now - *pts > WAIT_MS)
                .map(|pts| (pts - t0, i64::MAX));

            // events: dispatch, each tool, result
            events.push(Event {
                time: spawn,
                label: format!("orchestrator \u{2192} {}", a.role),
                detail: "dispatch".into(),
                color: theme::B,
            });
            for (ts, tool) in &a.tools {
                events.push(Event {
                    time: ts - t0,
                    label: a.role.clone(),
                    detail: tool.name.clone(),
                    color: theme::G,
                });
            }
            if let Some(f) = finish {
                events.push(Event {
                    time: f,
                    label: format!("{} \u{2192} orchestrator", a.role),
                    detail: format!("result {size}"),
                    color: theme::B,
                });
            }
            if let Some((ws, _)) = wait {
                events.push(Event {
                    time: ws,
                    label: a.role.clone(),
                    detail: "? waiting".into(),
                    color: theme::AM,
                });
            }
            if a.error {
                events.push(Event {
                    time: a.last_ts.map(|l| l - t0).unwrap_or(spawn),
                    label: a.role.clone(),
                    detail: "error".into(),
                    color: theme::RED,
                });
            }

            agents.push(Agent {
                name: a.role.clone(),
                spawn,
                finish,
                wait,
                tools,
                live_tool,
                size,
                bob: (a.order as i64) * 3,
                error: a.error,
            });
        }

        let all = if !agents.is_empty() && agents.iter().all(|a| a.finish.is_some()) {
            agents.iter().filter_map(|a| a.finish).max().unwrap_or(0) + 500
        } else {
            i64::MAX
        };

        events.sort_by_key(|e| e.time);
        // keep the tail (render only shows the last 4)
        let keep = 64usize;
        if events.len() > keep {
            let start = events.len() - keep;
            events.drain(0..start);
        }

        Session {
            header: self.header.clone(),
            agents,
            events,
            all,
            length: 0,
            live: true,
        }
    }

    /// The render clock `t` for the current instant = now - t0, where t0 is the
    /// earliest spawn (same origin build() uses). 0 when nothing is spawned yet.
    pub fn clock(&self) -> i64 {
        match self
            .accs
            .values()
            .filter_map(|a| a.spawn_ms.or(a.first_ts))
            .min()
        {
            Some(t0) => now_ms() - t0,
            None => 0,
        }
    }
}

// ---- helpers ---------------------------------------------------------------

/// Read appended bytes of a file from `*offset`, return parsed-ready lines and
/// advance the offset to the last complete line. Handles truncation/rotation.
fn tail(path: &Path, offset: &mut u64) -> Vec<String> {
    let mut f = match File::open(path) {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0; // file shrank: re-read from start
    }
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = String::new();
    if f.read_to_string(&mut buf).is_err() {
        return Vec::new();
    }
    let consumed = match buf.rfind('\n') {
        Some(i) => i + 1,
        None => return Vec::new(),
    };
    *offset += consumed as u64;
    buf[..consumed]
        .lines()
        .map(|l| l.to_string())
        .filter(|l| !l.is_empty())
        .collect()
}

/// Read the real project `cwd` recorded in a transcript (scans the first lines),
/// so the header can show the actual directory name even when resolving a
/// session by id or falling back to another project.
pub fn read_cwd(path: &Path) -> Option<PathBuf> {
    let f = File::open(path).ok()?;
    let reader = BufReader::new(f);
    for line in reader.lines().take(80).map_while(Result::ok) {
        if !line.contains("\"cwd\"") {
            continue;
        }
        if let Ok(v) = serde_json::from_str::<Value>(&line) {
            if let Some(cwd) = v.get("cwd").and_then(Value::as_str) {
                if !cwd.is_empty() {
                    return Some(PathBuf::from(cwd));
                }
            }
        }
    }
    None
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse "2026-11-19T06:21:38.273Z" (always UTC) to epoch ms.
fn iso_to_ms(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let year = num(0, 4)?;
    let mon = num(5, 7)?;
    let day = num(8, 10)?;
    let hour = num(11, 13)?;
    let min = num(14, 16)?;
    let sec = num(17, 19)?;
    // fractional seconds (optional)
    let mut ms = 0i64;
    if b.len() > 20 && b[19] == b'.' {
        let frac: String = s[20..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .take(3)
            .collect();
        if !frac.is_empty() {
            let mut f = frac.clone();
            while f.len() < 3 {
                f.push('0');
            }
            ms = f.parse().unwrap_or(0);
        }
    }
    let days = days_from_civil(year, mon, day);
    Some(((days * 86400 + hour * 3600 + min * 60 + sec) * 1000) + ms)
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

/// Tools that can block on a permission prompt (mutating / external). Pure reads
/// return instantly and never prompt, so a pending read isn't a wait.
fn needs_permission(name: &str) -> bool {
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

/// Role display name from the agent type + description hint + tools used.
fn role_name(agent_type: &str, desc: &str, tools: &[(i64, Tool)]) -> String {
    let at = agent_type.to_lowercase();
    match at.as_str() {
        "explore" => "scout".into(),
        "plan" => "architect".into(),
        "general-purpose" => {
            if looks_researchy(desc, tools) {
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

fn looks_researchy(desc: &str, tools: &[(i64, Tool)]) -> bool {
    let d = desc.to_lowercase();
    if d.contains("research")
        || d.contains("web")
        || d.contains("look up")
        || d.contains("latest")
        || d.contains("docs")
    {
        return true;
    }
    tools
        .iter()
        .any(|(_, t)| t.name.contains("web") || t.name.contains("fetch") || t.name.contains("search"))
}

/// Byte count to a compact "x.ykb" / "nb" string.
fn human_size(bytes: usize) -> String {
    if bytes < 1024 {
        format!("{bytes}b")
    } else {
        format!("{:.1}kb", bytes as f64 / 1024.0)
    }
}
