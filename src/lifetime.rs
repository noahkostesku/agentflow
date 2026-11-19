//! Lifetime totals across sessions, kept in `~/.agentflow/lifetime.json` when
//! `persistence` is on. Stored per session id and upserted, so watching the
//! same session twice (or re-recording it as it grows) never double counts.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Session;
use crate::stats;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub cost: f64,
    pub tokens: u64,
    pub agents: usize,
    pub cache_saved: f64,
    pub achievements: Vec<String>,
    /// fastest agent finish, ms
    pub fastest_ms: Option<i64>,
    /// epoch ms the session started; 0 for entries written before `wrapped`
    /// existed (those only count toward `--all`)
    pub started: i64,
    /// tool calls per local hour of day (24 buckets), for "busiest hours"
    pub hours: Vec<u32>,
    /// one record per agent, for the top-agent card
    pub roster: Vec<AgentRec>,
    /// summed agent run time, ms (0 in entries from before it was recorded)
    pub work_ms: i64,
    /// wall-clock from the first dispatch to the last agent's end, ms
    pub wall_ms: i64,
}

/// What `wrapped` needs to know about one agent.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AgentRec {
    pub role: String,
    /// model family ("opus", …)
    pub model: String,
    /// tool calls per personality category (reader, builder, runner, researcher)
    pub mix: [u32; 4],
    pub output: u64,
    pub cost: f64,
    /// how long it ran
    pub ms: i64,
}

/// Local hour of day (0–23) for an epoch-ms instant.
pub fn local_hour(epoch_ms: i64) -> usize {
    use chrono::{Local, TimeZone, Timelike};
    Local
        .timestamp_millis_opt(epoch_ms)
        .single()
        .map_or(0, |d| d.hour() as usize)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Store {
    pub sessions: BTreeMap<String, Entry>,
}

/// Lifetime sums for the `l` view.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Totals {
    pub sessions: usize,
    pub agents: usize,
    pub cost: f64,
    pub tokens: u64,
    pub cache_saved: f64,
    /// distinct achievements ever earned, and total times earned
    pub unlocked: Vec<String>,
    pub earned: usize,
    pub fastest_ms: Option<i64>,
}

pub fn path() -> PathBuf {
    crate::config::dir().join("lifetime.json")
}

impl Store {
    pub fn load(path: &Path) -> Store {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default()
    }

    /// Write atomically (temp file + rename) so a crash can't truncate it.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    /// Record a session's numbers as of `t`. Returns true if anything changed.
    pub fn record(&mut self, s: &Session, t: i64) -> bool {
        let Some(id) = &s.id else {
            return false;
        };
        let r = stats::recap(s, t);
        let par = stats::parallelism(&s.agents, t);
        let entry = Entry {
            work_ms: par.work_ms,
            wall_ms: par.wall_ms,
            cost: r.cost,
            tokens: r.tokens,
            agents: s.agents.iter().filter(|a| a.spawn <= t).count(),
            cache_saved: r.savings,
            achievements: stats::unlocks(s, t)
                .into_iter()
                .map(|u| u.kind.title().to_string())
                .collect(),
            fastest_ms: s
                .agents
                .iter()
                .filter_map(|a| a.finish.filter(|f| *f <= t).map(|f| f - a.spawn))
                .min(),
            started: s.epoch0.unwrap_or(0),
            hours: match s.epoch0 {
                Some(e0) => {
                    let mut h = vec![0u32; 24];
                    for u in s.agents.iter().flat_map(|a| a.uses.iter()).filter(|u| u.time <= t) {
                        h[local_hour(e0 + u.time)] += 1;
                    }
                    h
                }
                None => Vec::new(),
            },
            roster: s
                .agents
                .iter()
                .filter(|a| a.spawn <= t)
                .map(|a| AgentRec {
                    role: a.name.clone(),
                    model: a.model.clone(),
                    mix: stats::tool_mix(a.uses.iter().filter(|u| u.time <= t)),
                    output: a.usage_at(t).output,
                    cost: a.cost_at(t),
                    ms: (a.finish.unwrap_or(t).min(t) - a.spawn).max(0),
                })
                .collect(),
        };
        if self.sessions.get(id) == Some(&entry) {
            return false;
        }
        self.sessions.insert(id.clone(), entry);
        true
    }

    pub fn totals(&self) -> Totals {
        let mut t = Totals { sessions: self.sessions.len(), ..Totals::default() };
        for e in self.sessions.values() {
            t.agents += e.agents;
            t.cost += e.cost;
            t.tokens += e.tokens;
            t.cache_saved += e.cache_saved;
            t.earned += e.achievements.len();
            for a in &e.achievements {
                if !t.unlocked.contains(a) {
                    t.unlocked.push(a.clone());
                }
            }
            if let Some(f) = e.fastest_ms {
                t.fastest_ms = Some(t.fastest_ms.map_or(f, |b| b.min(f)));
            }
        }
        t.unlocked.sort();
        t
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Agent, Call, Usage};

    fn session(id: &str, cost: f64) -> Session {
        Session {
            id: Some(id.into()),
            epoch0: Some(1_000_000),
            live: true,
            all: i64::MAX,
            agents: vec![Agent {
                name: "scout".into(),
                spawn: 0,
                finish: Some(4_000),
                calls: vec![Call {
                    time: 10,
                    cost,
                    saved: 0.01,
                    usage: Usage { output: 100, ..Usage::default() },
                }],
                ..Agent::default()
            }],
            ..Session::default()
        }
    }

    #[test]
    fn upsert_is_idempotent_and_totals_sum() {
        let mut st = Store::default();
        assert!(st.record(&session("a", 0.20), 10_000));
        assert!(!st.record(&session("a", 0.20), 10_000), "same numbers: no change");
        assert!(st.record(&session("a", 0.30), 10_000), "session grew: replaced");
        assert!(st.record(&session("b", 0.10), 10_000));
        let t = st.totals();
        assert_eq!(t.sessions, 2);
        assert_eq!(t.agents, 2);
        assert!((t.cost - 0.40).abs() < 1e-9);
        assert_eq!(t.fastest_ms, Some(4_000));
        assert!(t.unlocked.contains(&"speedrun".to_string()));
        let e = &st.sessions["a"];
        assert_eq!(e.roster.len(), 1);
        assert_eq!((e.roster[0].role.as_str(), e.roster[0].output, e.roster[0].ms), ("scout", 100, 4_000));
        assert_eq!(e.started, 1_000_000);
        assert_eq!(e.hours.len(), 24);
        // demo sessions have no id and are never recorded
        let mut demo = session("x", 1.0);
        demo.id = None;
        assert!(!st.record(&demo, 10_000));
    }

    /// Files written before wrapped existed still load; new fields default.
    #[test]
    fn records_work_and_wall_clock() {
        let mut s = session("p", 0.1);
        let mut b = s.agents[0].clone();
        b.spawn = 1_000;
        b.finish = Some(3_000);
        s.agents.push(b); // 0–4s and 1–3s: 6s of work in 4s
        let mut st = Store::default();
        st.record(&s, 10_000);
        let e = &st.sessions["p"];
        assert_eq!((e.work_ms, e.wall_ms), (6_000, 4_000));
    }

    #[test]
    fn old_entries_load_with_defaults() {
        let st: Store = serde_json::from_str(
            r#"{"sessions":{"x":{"cost":1.5,"tokens":10,"agents":2,"cache_saved":0.1,"achievements":[],"fastest_ms":null}}}"#,
        )
        .unwrap();
        let e = &st.sessions["x"];
        assert_eq!((e.cost, e.started, e.hours.len(), e.roster.len()), (1.5, 0, 0, 0));
    }

    #[test]
    fn save_and_load_round_trip() {
        let p = std::env::temp_dir()
            .join(format!("aflow-life-{}", std::process::id()))
            .join("lifetime.json");
        let mut st = Store::default();
        st.record(&session("a", 0.25), 10_000);
        st.save(&p).unwrap();
        assert_eq!(Store::load(&p).totals(), st.totals());
        // corrupt file → empty store, not a crash
        std::fs::write(&p, "{nope").unwrap();
        assert_eq!(Store::load(&p).sessions.len(), 0);
        let _ = std::fs::remove_dir_all(p.parent().unwrap());
    }
}
