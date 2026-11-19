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
//!   * `event_msg/task_complete` ends a turn with `last_agent_message`.
//!
//! Codex runs one agent per session here (`subagents: false`): everything is
//! the agent `main`. Its `collaboration.spawn_agent` tool exists, but no
//! session with subagents has been observed, so spawns show as tool calls.

use std::collections::HashSet;
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

pub struct CodexAdapter {
    session: String,
    path: PathBuf,
    offset: u64,
    started: bool,
    /// latest model id
    model: Option<String>,
    /// call ids already seen
    seen: HashSet<String>,
    /// started, not yet finished: (event call id, the exec call id, op)
    open: Vec<(String, String, Op)>,
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
            out.push(self.ev(ts, Body::SessionStarted { cwd: cwd.map(str::to_string), subagents: false }));
        }
        for line in tail(&self.path, &mut self.offset) {
            let Ok(r) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let Some(ts) = record_ts(&r) else {
                continue;
            };
            let before = out.len();
            self.record(&r, ts, &mut out);
            // nothing else said it was alive: a heartbeat does
            if out.len() == before {
                out.push(self.ev(ts, Body::Heartbeat { agent: MAIN.into() }));
            }
        }
        out
    }
}

impl CodexAdapter {
    pub fn open(path: &Path) -> Self {
        CodexAdapter {
            session: session_id_of(path),
            path: path.to_path_buf(),
            offset: 0,
            started: false,
            model: None,
            seen: HashSet::new(),
            open: Vec::new(),
        }
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
            script_ops(input)
        } else {
            vec![(tool_body(name, name, ToolKind::Run, ""), Op::Other)]
        };
        for (i, (mut body, op)) in ops.into_iter().enumerate() {
            let id = if i == 0 { call.to_string() } else { format!("{call}#{i}") };
            if let Body::ToolStarted { call: c, .. } = &mut body {
                *c = Some(id.clone());
            }
            out.push(self.ev(ts, body));
            self.open.push((id, call.to_string(), op));
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
fn script_ops(js: &str) -> Vec<(Body, Op)> {
    let mut ops = Vec::new();
    let mut patched = false;
    let mut rest = js;
    while let Some(i) = rest.find("tools.") {
        rest = &rest[i + 6..];
        let fname: String = rest.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
        let args = &rest[fname.len()..];
        match fname.as_str() {
            "exec_command" => {
                let cmd = js_string_field(args, "cmd").unwrap_or_default();
                ops.push((command_body(&cmd), Op::Cmd(cmd)));
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
                        blocking: true,
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

/// The session to watch: `id` anywhere; else the most recently active one
/// that ran in `project`; else (when `fallback`) the most recent anywhere.
pub fn find(root: &Path, id: Option<&str>, project: Option<&Path>, fallback: bool) -> Option<PathBuf> {
    let mut all = rollouts(root);
    all.sort_by_key(|(_, m)| std::cmp::Reverse(*m));
    if let Some(id) = id {
        return all.into_iter().map(|(p, _)| p).find(|p| session_id_of(p) == id);
    }
    if let Some(dir) = project {
        let want = dir.to_string_lossy();
        let hit = all.iter().find(|(p, _)| {
            head(p).and_then(|m| m.pointer("/payload/cwd").and_then(Value::as_str).map(|c| c.trim_end_matches('/') == want.trim_end_matches('/'))) == Some(true)
        });
        if let Some((p, _)) = hit {
            return Some(p.clone());
        }
    }
    fallback.then(|| all.into_iter().next().map(|(p, _)| p)).flatten()
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
        assert!(matches!(&evs[0].body, Body::SessionStarted { cwd: Some(c), subagents: false } if c == "/w/proj"));
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
