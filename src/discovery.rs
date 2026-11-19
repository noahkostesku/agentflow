//! Finding which transcript to watch.
//!
//! Claude Code stores one directory per project under `~/.claude/projects/`,
//! named by replacing every non-alphanumeric char in the project's absolute
//! path with `-`. Inside are `<session-id>.jsonl` transcripts.
//!
//! Resolution order (matches the spec):
//!   1. `--session <id>` : that session, wherever it lives.
//!   2. the session for `--project` / cwd : newest `.jsonl` in that project dir.
//!   3. fallback : the most-recently-active session across ALL projects.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// A resolved transcript to watch.
pub struct Resolved {
    /// path to the `.jsonl` transcript
    pub path: PathBuf,
    /// absolute project directory this session belongs to (best effort)
    pub project_dir: Option<PathBuf>,
    /// true when we fell back to the global most-recent session
    pub fell_back: bool,
}

/// `~/.claude/projects`.
pub fn projects_root() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".claude").join("projects")
}

/// Encode an absolute path to its project-directory name.
pub fn encode_path(p: &Path) -> String {
    p.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn mtime(p: &Path) -> SystemTime {
    fs::metadata(p)
        .and_then(|m| m.modified())
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// When a session last showed signs of life: the newest write to its main
/// transcript *or* any of its subagent transcripts. Background agents keep
/// writing to their own files while the main transcript sits quiet, so the
/// main file's mtime alone would make a busy session look idle.
pub fn activity(transcript: &Path) -> SystemTime {
    let mut newest = mtime(transcript);
    if let Ok(entries) = fs::read_dir(crate::transcript::subagents_dir(transcript)) {
        for e in entries.flatten() {
            newest = newest.max(e.metadata().and_then(|m| m.modified()).unwrap_or(newest));
        }
    }
    newest
}

/// Most recently active `.jsonl` session directly inside `dir`, if any.
fn newest_jsonl(dir: &Path) -> Option<(PathBuf, SystemTime)> {
    let mut best: Option<(PathBuf, SystemTime)> = None;
    for entry in fs::read_dir(dir).ok()?.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let when = activity(&path);
        if best.as_ref().is_none_or(|(_, t)| when > *t) {
            best = Some((path, when));
        }
    }
    best
}

/// Resolve the transcript to watch.
pub fn resolve(
    session: Option<&str>,
    project: Option<&Path>,
) -> Option<Resolved> {
    let root = projects_root();

    // 1. Explicit session id: search every project dir for "<id>.jsonl".
    if let Some(id) = session {
        for entry in fs::read_dir(&root).ok()?.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let candidate = dir.join(format!("{id}.jsonl"));
            if candidate.is_file() {
                return Some(Resolved {
                    path: candidate,
                    project_dir: Some(dir),
                    fell_back: false,
                });
            }
        }
        return None;
    }

    // 2. Session for the given project / cwd.
    let target = project
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok());
    if let Some(dir) = &target {
        let proj_dir = root.join(encode_path(dir));
        if let Some((path, _)) = newest_jsonl(&proj_dir) {
            return Some(Resolved {
                path,
                project_dir: Some(dir.clone()),
                fell_back: false,
            });
        }
    }

    // 3. Fallback: most-recently-active session across all projects.
    let mut best: Option<(PathBuf, PathBuf, SystemTime)> = None; // (jsonl, projdir, mtime)
    for entry in fs::read_dir(&root).ok()?.flatten() {
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        if let Some((path, when)) = newest_jsonl(&dir) {
            if best.as_ref().is_none_or(|(_, _, t)| when > *t) {
                best = Some((path, dir, when));
            }
        }
    }
    best.map(|(path, dir, _)| Resolved {
        path,
        project_dir: Some(dir),
        fell_back: true,
    })
}
