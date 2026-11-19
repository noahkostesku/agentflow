//! Finding which session to watch, across harnesses. Each adapter knows its
//! own storage (`adapters::<harness>::find`); this module picks among them:
//!   1. `--session <id>`: that session, in whichever harness has it.
//!   2. the project's (`--project`, default cwd) most recently active session
//!      in any of the chosen harnesses.
//!   3. fallback: the most recently active session anywhere.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::adapters::{claude, codex, SourceAdapter};
use crate::schema::Harness;

#[cfg(test)]
pub use crate::adapters::claude::{encode_path, projects_root};

/// Harnesses with an adapter; `auto` / `all` search these.
pub const SUPPORTED: &[Harness] = &[Harness::Claude, Harness::Codex];

/// A session found to watch.
pub struct Found {
    pub harness: Harness,
    /// its primary file
    pub path: PathBuf,
    /// absolute project directory (best effort)
    pub project_dir: Option<PathBuf>,
    /// nothing matched the project: the most recent session anywhere
    pub fell_back: bool,
}

impl Found {
    /// An adapter reading this session.
    pub fn open(&self) -> Box<dyn SourceAdapter> {
        match self.harness {
            Harness::Claude => Box::new(claude::ClaudeAdapter::open(&self.path)),
            Harness::Codex => Box::new(codex::CodexAdapter::open(&self.path)),
        }
    }
}

/// When a session last showed signs of life (subagents' files included).
pub fn activity(roots: &Roots, harness: Harness, path: &Path) -> SystemTime {
    match harness {
        Harness::Claude => claude::activity(path),
        Harness::Codex => codex::activity(&roots.codex, path),
    }
}

/// Where each harness keeps its sessions.
pub struct Roots {
    pub claude: PathBuf,
    pub codex: PathBuf,
}

impl Roots {
    /// The real locations (`~/.claude/projects`, `$CODEX_HOME/sessions`).
    pub fn here() -> Self {
        Roots { claude: claude::projects_root(), codex: codex::sessions_root() }
    }
}

/// One harness's candidate.
fn find_in(roots: &Roots, harness: Harness, session: Option<&str>, project: Option<&Path>, fallback: bool) -> Option<Found> {
    match harness {
        Harness::Claude => claude::find(&roots.claude, session, project, fallback).map(|r| Found {
            harness,
            path: r.path,
            project_dir: r.project_dir,
            fell_back: r.fell_back,
        }),
        Harness::Codex => {
            let root = &roots.codex;
            let exact = codex::find(root, session, project, false);
            let fell_back = exact.is_none() && session.is_none();
            let path = exact.or_else(|| (fallback && fell_back).then(|| codex::find(root, None, None, true)).flatten())?;
            Some(Found { harness, path, project_dir: None, fell_back })
        }
    }
}

/// The session to watch among `harnesses` (see the module doc).
pub fn find(harnesses: &[Harness], session: Option<&str>, project: Option<&Path>) -> Option<Found> {
    find_at(&Roots::here(), harnesses, session, project)
}

/// [`find`] in the given storage roots.
pub fn find_at(roots: &Roots, harnesses: &[Harness], session: Option<&str>, project: Option<&Path>) -> Option<Found> {
    if session.is_some() {
        return harnesses.iter().find_map(|&h| find_in(roots, h, session, None, false));
    }
    let newest = |found: Vec<Found>| found.into_iter().max_by_key(|f| activity(roots, f.harness, &f.path));
    let here: Vec<Found> = harnesses.iter().filter_map(|&h| find_in(roots, h, None, project, false)).collect();
    if !here.is_empty() {
        return newest(here);
    }
    let anywhere: Vec<Found> = harnesses
        .iter()
        .filter_map(|&h| find_in(roots, h, None, None, true))
        .map(|f| Found { fell_back: true, ..f })
        .collect();
    newest(anywhere)
}

/// A session that has become newer than the one being watched, to switch
/// to. Started in a directory with sessions: only that directory's sessions
/// are candidates. Started on the global fallback: a session appearing in
/// our own directory (or a newer global one) wins. A candidate must have been
/// active at least 5s more recently, so two busy sessions don't make the view
/// flip back and forth.
pub fn newer(harnesses: &[Harness], project: &Path, fell_back: bool, current: (Harness, &Path)) -> Option<Found> {
    let roots = Roots::here();
    let cand = find_at(&roots, harnesses, None, Some(project))?;
    if cand.path == current.1 || (!fell_back && cand.fell_back) {
        return None;
    }
    let ahead = activity(&roots, cand.harness, &cand.path)
        .duration_since(activity(&roots, current.0, current.1))
        .unwrap_or_default();
    (ahead >= std::time::Duration::from_secs(5)).then_some(cand)
}

/// A `--project` argument as Claude Code would have recorded it: absolute,
/// symlinks resolved (macOS `/tmp` → `/private/tmp`), no `.`/`..`, no
/// trailing slash, `~` expanded. Falls back to a lexical cleanup when the
/// path doesn't exist.
pub fn normalize_project(arg: &str) -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    let expanded = match arg.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => arg.to_string(),
    };
    let p = Path::new(&expanded);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(p)
    };
    if let Ok(real) = fs::canonicalize(&abs) {
        return real;
    }
    let mut out = PathBuf::new();
    for c in abs.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Auto picks the most recently active session for the project across
    /// harnesses; `--session` finds an id in whichever has it; a project with
    /// no sessions falls back to the newest anywhere.
    #[test]
    fn auto_picks_the_newest_session_across_harnesses() {
        let base = std::env::temp_dir().join(format!("aflow-disc-{}", std::process::id()));
        let roots = Roots { claude: base.join("claude"), codex: base.join("codex") };
        let project = PathBuf::from("/work/both");
        let claude_s = roots.claude.join(encode_path(&project)).join("c-1.jsonl");
        let day = roots.codex.join("2026").join("01").join("01");
        let codex_s = day.join("rollout-2026-01-01T00-00-00-0000bbbb-1111-2222-3333-444455556666.jsonl");
        fs::create_dir_all(claude_s.parent().unwrap()).unwrap();
        fs::create_dir_all(&day).unwrap();
        fs::write(&claude_s, "").unwrap();
        fs::write(&codex_s, r#"{"timestamp":"2026-01-01T00:00:00.000Z","type":"session_meta","payload":{"id":"0000bbbb-1111-2222-3333-444455556666","cwd":"/work/both"}}"#.to_string() + "\n").unwrap();
        let age = |p: &Path, secs: u64| {
            let t = SystemTime::now() - std::time::Duration::from_secs(secs);
            fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
        };
        let all = SUPPORTED;

        age(&claude_s, 30);
        age(&codex_s, 300);
        let f = find_at(&roots, all, None, Some(&project)).unwrap();
        assert_eq!((f.harness, f.fell_back), (Harness::Claude, false));

        age(&codex_s, 1);
        let f = find_at(&roots, all, None, Some(&project)).unwrap();
        assert_eq!((f.harness, &f.path), (Harness::Codex, &codex_s));
        assert_eq!(find_at(&roots, &[Harness::Claude], None, Some(&project)).unwrap().harness, Harness::Claude, "--source claude");

        let f = find_at(&roots, all, Some("c-1"), None).unwrap();
        assert_eq!(f.harness, Harness::Claude);
        let f = find_at(&roots, all, Some("0000bbbb-1111-2222-3333-444455556666"), None).unwrap();
        assert_eq!(f.harness, Harness::Codex);

        let f = find_at(&roots, all, None, Some(Path::new("/work/elsewhere"))).unwrap();
        assert!(f.fell_back && f.harness == Harness::Codex, "newest anywhere");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn project_args_are_normalized() {
        let cwd = std::env::current_dir().unwrap();
        let real = fs::canonicalize(&cwd).unwrap();
        assert_eq!(normalize_project("."), real);
        assert_eq!(normalize_project(&format!("{}/", cwd.display())), real);
        assert_eq!(normalize_project("src/.."), real);
        // nonexistent paths are cleaned lexically
        assert_eq!(normalize_project("/no/such/./dir/../x/"), PathBuf::from("/no/such/x"));
        assert_ne!(encode_path(&normalize_project(".")), "-", "dot used to encode to a bare dash");
    }
}
