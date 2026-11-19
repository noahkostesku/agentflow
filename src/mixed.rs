//! The mixed view (`--source all`): every harness's session for a project,
//! side by side on one clock.
//!
//! [`merge`] folds per-harness `Session`s into one whose `groups` keep each
//! harness's part, so the views can draw a group per harness while totals,
//! stats and selection work over every agent. [`Mixed`] is the live watcher
//! behind it: one `Live` per harness, rebuilt on a shared clock.

use std::path::{Path, PathBuf};

use crate::adapters::now_ms;
use crate::discovery::{self, Found};
use crate::live::Live;
use crate::model::{Group, Handoff, Session};
use crate::pricing::Pricing;
use crate::schema::Harness;

/// One session per group, already on a shared clock (session time 0 is the
/// same instant in each), merged into a mixed view. Groups keep their order.
pub fn merge(header: String, parts: Vec<(String, Session)>) -> Session {
    let mut out = Session { header, live: parts.iter().any(|(_, s)| s.live), ..Session::default() };
    let mut groups = Vec::new();
    for (harness, s) in parts {
        let start = out.agents.len();
        out.agents.extend(s.agents.iter().cloned());
        out.orch_calls.extend(s.orch_calls.iter().cloned());
        out.events.extend(s.events.iter().cloned());
        out.handoffs.extend(s.handoffs.iter().map(|h| Handoff { from: h.from + start, to: h.to + start, ..h.clone() }));
        out.unpriced.extend(s.unpriced.iter().cloned());
        out.length = out.length.max(s.length);
        out.epoch0 = match (out.epoch0, s.epoch0) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        groups.push(Group { harness, start, sess: s });
    }
    // done when every group is (a group with no agents yet isn't)
    out.all = if groups.iter().all(|g| g.sess.all != i64::MAX) {
        groups.iter().map(|g| g.sess.all).max().unwrap_or(i64::MAX)
    } else {
        i64::MAX
    };
    out.orch_calls.sort_by_key(|c| c.time);
    out.events.sort_by_key(|e| e.time);
    out.handoffs.sort_by_key(|h| h.time);
    out.unpriced.sort();
    out.unpriced.dedup();
    out.groups = groups;
    out
}

/// Live mode's mixed view: one watched session per harness.
pub struct Mixed {
    harnesses: Vec<Harness>,
    /// where to look for sessions
    project: PathBuf,
    /// the project directory the header names
    cwd: PathBuf,
    /// the sessions shown: at most one per harness, in harness order
    lives: Vec<Live>,
    /// nothing matched the project: showing the newest sessions anywhere
    fell_back: bool,
}

impl Mixed {
    /// Attach to the project's sessions; `None` when there are none anywhere.
    pub fn open(harnesses: &[Harness], project: &Path, cwd: &Path) -> Option<Mixed> {
        let found = discovery::find_mixed(harnesses, project);
        let fell_back = found.first()?.fell_back;
        let mut m = Mixed {
            harnesses: harnesses.to_vec(),
            project: project.to_path_buf(),
            cwd: cwd.to_path_buf(),
            lives: Vec::new(),
            fell_back,
        };
        m.adopt(found);
        Some(m)
    }

    /// Take on newly found sessions: a harness's newer session replaces its
    /// old one, a harness that just became active joins. A harness that went
    /// quiet keeps its place (no flicker). Returns true if anything changed.
    fn adopt(&mut self, found: Vec<Found>) -> bool {
        let mut changed = false;
        for f in found {
            let have = self.lives.iter().position(|l| l.harness() == f.harness);
            if have.is_some_and(|i| self.lives[i].path() == f.path) {
                continue;
            }
            let mut live = Live::new(f.open(), String::new());
            live.refresh();
            match have {
                Some(i) => self.lives[i] = live,
                None => self.lives.push(live),
            }
            changed = true;
        }
        let order = |h: Harness| self.harnesses.iter().position(|x| *x == h);
        self.lives.sort_by_key(|l| order(l.harness()));
        changed
    }

    /// Look for newer or newly active sessions. Started on the fallback,
    /// sessions showing up for the project replace everything. Returns true
    /// if what's shown changed (agent indexes moved).
    pub fn rescan(&mut self) -> bool {
        let found = discovery::find_mixed(&self.harnesses, &self.project);
        let Some(first) = found.first() else {
            return false;
        };
        if self.fell_back && !first.fell_back {
            self.lives.clear();
            self.fell_back = false;
        } else if !self.fell_back && first.fell_back {
            return false;
        }
        self.adopt(found)
    }

    pub fn refresh(&mut self) {
        for l in &mut self.lives {
            l.refresh();
        }
    }

    /// Epoch ms of the shared session time 0: the earliest spawn in any of
    /// the sessions, or when we started watching.
    fn origin(&self) -> i64 {
        self.lives
            .iter()
            .filter_map(Live::origin)
            .min()
            .or_else(|| self.lives.iter().map(Live::opened).min())
            .unwrap_or_else(now_ms)
    }

    /// The render clock now (see [`Live::clock`]).
    pub fn clock(&self) -> i64 {
        now_ms() - self.origin()
    }

    /// "agentflow // all // <project>"
    fn header(&self) -> String {
        let dir = self
            .lives
            .iter()
            .find_map(|l| l.cwd())
            .filter(|_| self.fell_back)
            .map_or_else(|| self.cwd.clone(), PathBuf::from);
        crate::header("all", &dir)
    }

    /// The mixed view now.
    pub fn build(&self, pricing: &Pricing) -> Session {
        let t0 = self.origin();
        let parts = self.lives.iter().map(|l| (l.harness().name().to_string(), l.build_at(pricing, t0))).collect();
        merge(self.header(), parts)
    }

    /// Each session on its own clock, with that clock's time now, for
    /// lifetime records.
    pub fn parts(&self, pricing: &Pricing) -> Vec<(Session, i64)> {
        self.lives.iter().map(|l| (l.build(pricing), l.clock())).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::SourceAdapter;
    use crate::model::{Agent, Call};
    use crate::schema::{Body, Event, ToolKind, MAIN};

    /// Replays fixed events as one harness's session.
    struct Script(Harness, Vec<Event>);
    impl SourceAdapter for Script {
        fn harness(&self) -> Harness {
            self.0
        }
        fn session_id(&self) -> &str {
            "s"
        }
        fn path(&self) -> &Path {
            Path::new("s")
        }
        fn poll(&mut self) -> Vec<Event> {
            std::mem::take(&mut self.1)
        }
    }

    fn tool(h: Harness, sid: &str, ts: i64) -> Event {
        Event::new(
            h,
            sid,
            ts,
            Body::ToolStarted {
                agent: MAIN.into(),
                call: Some(format!("c{ts}")),
                tool: "bash".into(),
                label: "ls".into(),
                kind: ToolKind::Look,
                file: None,
                edit: false,
                arg: "ls".into(),
                blocking: false,
                test: false,
            },
        )
    }

    fn live(h: Harness, sid: &str, ts: i64) -> Live {
        let start = Event::new(h, sid, ts - 10, Body::SessionStarted { cwd: Some("/work/p".into()), subagents: false });
        let mut l = Live::new(Box::new(Script(h, vec![start, tool(h, sid, ts)])) as Box<dyn SourceAdapter>, String::new());
        l.refresh();
        l
    }

    /// Live: each harness's session on one clock from the earliest start;
    /// lifetime records keep each on its own clock, with its harness.
    #[test]
    fn live_sessions_share_one_clock() {
        let base = now_ms() - 60_000;
        let m = Mixed {
            harnesses: discovery::SUPPORTED.to_vec(),
            project: PathBuf::from("/work/p"),
            cwd: PathBuf::from("/work/p"),
            lives: vec![live(Harness::Claude, "c", base), live(Harness::Codex, "x", base + 5_000)],
            fell_back: false,
        };
        let s = m.build(&Pricing::default());
        assert_eq!(s.header, "agentflow // all // p");
        let names: Vec<(&str, i64)> = s.agents.iter().map(|a| (a.name.as_str(), a.spawn)).collect();
        assert_eq!(names, [("claude", 0), ("codex", 5_000)], "solo rows named for their harness, one clock");
        assert!(s.groups.iter().all(|g| g.sess.solo), "each rooted at you");
        assert!((m.clock() - 60_000).abs() < 1_000);

        let parts = m.parts(&Pricing::default());
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1].0.agents[0].spawn, 0, "own clock for the record");
        assert_eq!(parts[1].0.harness, Some(Harness::Codex));
        assert!((parts[1].1 - 55_000).abs() < 1_000);
    }

    fn part(names: &[&str], spawn: i64, finish: Option<i64>, cost: f64) -> Session {
        let agents: Vec<Agent> = names
            .iter()
            .map(|n| Agent {
                name: n.to_string(),
                spawn,
                finish,
                calls: vec![Call { time: spawn + 10, cost, saved: 0.0, usage: Default::default() }],
                ..Agent::default()
            })
            .collect();
        let all = finish.map_or(i64::MAX, |f| f + 500);
        let handoffs = if names.len() > 1 {
            vec![Handoff { from: 0, to: 1, time: spawn, what: "result".into(), size: "1b".into() }]
        } else {
            vec![]
        };
        Session { agents, all, live: true, epoch0: Some(1_000 + spawn), handoffs, ..Session::default() }
    }

    /// Groups keep their agents in order; handoffs are re-indexed; totals
    /// cover everyone; the view is done only when every group is.
    #[test]
    fn merge_groups_by_harness() {
        let s = merge(
            "agentflow // all // p".into(),
            vec![
                ("claude".into(), part(&["scout", "fixer"], 0, Some(4_000), 0.10)),
                ("codex".into(), part(&["codex"], 1_000, None, 0.05)),
            ],
        );
        assert!(s.mixed());
        assert_eq!(s.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(), ["scout", "fixer", "codex"]);
        assert_eq!((s.groups[0].range(), s.groups[1].range()), (0..2, 2..3));
        assert_eq!(s.group_of(2).unwrap().harness, "codex");
        assert_eq!(s.groups[1].local(Some(2)), Some(0));
        assert_eq!(s.groups[0].local(Some(2)), None);
        assert_eq!((s.handoffs[0].from, s.handoffs[0].to), (0, 1));
        assert!((s.cost_at(10_000) - 0.25).abs() < 1e-9);
        assert_eq!(s.epoch0, Some(1_000));
        assert_eq!(s.all, i64::MAX, "codex is still working");

        let done = merge(
            "h".into(),
            vec![
                ("claude".into(), part(&["scout"], 0, Some(4_000), 0.1)),
                ("codex".into(), part(&["codex"], 0, Some(6_000), 0.1)),
            ],
        );
        assert_eq!(done.all, 6_500);
    }
}
