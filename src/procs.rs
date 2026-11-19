//! Which coding-agent processes are running, and in which directory: how the
//! inbox tells a session that's waiting on you from one whose terminal was
//! closed while it waited (its transcript just stops, so on disk the two
//! look the same).
//!
//! `ps` lists the processes; a process is an agent when its command is
//! `claude` or `codex` (directly, or as the script `node` / `bun` runs, or a
//! package path like `@anthropic-ai/claude-code/…`). Its working directory
//! comes from `/proc/<pid>/cwd` on Linux and `lsof` on macOS.
//!
//! When it can't tell (no `ps`, a sandbox that hides other processes, an
//! agent whose directory it can't read), it says so ([`Alive::Unknown`],
//! or a [`Proc`] without a `cwd`) rather than guess "nothing is running".

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;

use crate::schema::Harness;

/// One running agent process.
#[derive(Clone, Debug, PartialEq)]
pub struct Proc {
    pub harness: Harness,
    /// its working directory; `None` when it couldn't be read
    pub cwd: Option<PathBuf>,
    /// its command line (a resumed session's id can be in it)
    pub args: String,
}

/// What's known about running agents.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Alive {
    /// couldn't look (or only saw part of the process table)
    #[default]
    Unknown,
    /// every running agent process
    Known(Vec<Proc>),
}

/// Look now. Costs a `ps` (and on macOS one `lsof`), so call it every few
/// seconds at most.
pub fn scan() -> Alive {
    let Ok(out) = Command::new("ps").args(["-axww", "-o", "pid=,args="]).output() else {
        return Alive::Unknown;
    };
    if !out.status.success() {
        return Alive::Unknown;
    }
    let Some(found) = agents_in_ps(&String::from_utf8_lossy(&out.stdout), std::process::id()) else {
        return Alive::Unknown;
    };
    let pids: Vec<u32> = found.iter().map(|f| f.0).collect();
    let cwds = cwds_of(&pids);
    Alive::Known(found.into_iter().map(|(pid, harness, args)| Proc { harness, cwd: cwds.get(&pid).cloned(), args }).collect())
}

/// The agent processes in `ps -o pid=,args=` output: (pid, harness, args).
/// `None` when our own process (`me`) isn't listed: a view that hides other
/// users' or other sandboxes' processes can't say what's running.
fn agents_in_ps(ps: &str, me: u32) -> Option<Vec<(u32, Harness, String)>> {
    let mut saw_me = false;
    let mut out = Vec::new();
    for line in ps.lines() {
        let Some((pid, args)) = line.trim_start().split_once(char::is_whitespace) else {
            continue;
        };
        let Ok(pid) = pid.parse::<u32>() else {
            continue;
        };
        if pid == me {
            saw_me = true;
            continue;
        }
        let args = args.trim();
        if let Some(h) = harness_of(args) {
            out.push((pid, h, args.to_string()));
        }
    }
    saw_me.then_some(out)
}

/// Is this command line a Claude Code or Codex CLI process?
fn harness_of(args: &str) -> Option<Harness> {
    let mut words = args.split_whitespace();
    let first = words.next()?;
    let base = |w: &str| w.rsplit('/').next().unwrap_or(w).to_string();
    // a script run by a JS runtime: look at the script
    let cmd = match base(first).as_str() {
        "node" | "bun" | "deno" => words.find(|w| !w.starts_with('-')).unwrap_or(""),
        _ => first,
    };
    if base(cmd) == "claude" || cmd.contains("/@anthropic-ai/claude-code/") {
        Some(Harness::Claude)
    } else if base(cmd) == "codex" || cmd.contains("/@openai/codex/") {
        Some(Harness::Codex)
    } else {
        None
    }
}

/// Working directories of `pids` (those it could read).
fn cwds_of(pids: &[u32]) -> HashMap<u32, PathBuf> {
    if pids.is_empty() {
        return HashMap::new();
    }
    if cfg!(target_os = "linux") {
        return pids
            .iter()
            .filter_map(|p| std::fs::read_link(format!("/proc/{p}/cwd")).ok().map(|d| (*p, d)))
            .collect();
    }
    let list = pids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    match Command::new("lsof").args(["-a", "-d", "cwd", "-Fpn", "-p", &list]).output() {
        Ok(out) => parse_lsof(&String::from_utf8_lossy(&out.stdout)),
        Err(_) => HashMap::new(),
    }
}

/// `lsof -Fpn` output: a `p<pid>` line, then `n<path>` for its cwd.
fn parse_lsof(text: &str) -> HashMap<u32, PathBuf> {
    let mut out = HashMap::new();
    let mut pid = None;
    for line in text.lines() {
        if let Some(p) = line.strip_prefix('p') {
            pid = p.parse::<u32>().ok();
        } else if let (Some(n), Some(p)) = (line.strip_prefix('n'), pid) {
            out.insert(p, PathBuf::from(n));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agent_command_lines() {
        assert_eq!(harness_of("claude --permission-mode default"), Some(Harness::Claude));
        assert_eq!(harness_of("/Users/me/.local/bin/claude"), Some(Harness::Claude));
        assert_eq!(harness_of("node /opt/homebrew/lib/node_modules/@anthropic-ai/claude-code/cli.js"), Some(Harness::Claude));
        assert_eq!(harness_of("codex resume 0000aaaa"), Some(Harness::Codex));
        assert_eq!(harness_of("node --no-warnings /usr/local/bin/codex"), Some(Harness::Codex));
        assert_eq!(harness_of("/Applications/Claude.app/Contents/MacOS/Claude"), None, "the desktop app isn't a CLI session");
        assert_eq!(harness_of("vim claude.md"), None);
        assert_eq!(harness_of("agentflow inbox"), None);
        assert_eq!(harness_of(""), None);
    }

    #[test]
    fn ps_output_and_a_restricted_view() {
        let ps = "  1 /sbin/launchd\n 42 claude\n 77 /bin/zsh -l\n 99 agentflow inbox\n 120 node /x/@openai/codex/bin/codex.js\n";
        assert_eq!(
            agents_in_ps(ps, 99),
            Some(vec![(42, Harness::Claude, "claude".into()), (120, Harness::Codex, "node /x/@openai/codex/bin/codex.js".into())])
        );
        assert_eq!(agents_in_ps(ps, 12345), None, "our own process missing: we can't see the whole table");
    }

    #[test]
    fn lsof_cwds() {
        let out = "p42\nfcwd\nn/private/tmp/af-a\np120\nfcwd\nn/Users/me/repo\n";
        let m = parse_lsof(out);
        assert_eq!(m.get(&42), Some(&PathBuf::from("/private/tmp/af-a")));
        assert_eq!(m.get(&120), Some(&PathBuf::from("/Users/me/repo")));
        assert!(parse_lsof("").is_empty());
    }
}
