//! Derived numbers: achievements, the recap card, and the stats panel. All
//! pure functions of a `Session` at session time `t`, so demo playback, live
//! mode, and tests share one implementation.

use std::collections::HashMap;

use crate::model::{Agent, Session, Usage};

// ---- achievements ----------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    /// an agent finished in under 10s
    Speedrun,
    /// an agent finished without editing a file
    Pacifist,
    /// a test command went from failing to passing
    BugHunter,
    /// one agent cost more than $0.50
    BigSpender,
    /// 3+ agents running at once
    TeamPlayer,
}

impl Kind {
    pub fn title(self) -> &'static str {
        match self {
            Kind::Speedrun => "speedrun",
            Kind::Pacifist => "pacifist",
            Kind::BugHunter => "bug hunter",
            Kind::BigSpender => "big spender",
            Kind::TeamPlayer => "team player",
        }
    }
}

pub const SPEEDRUN_MS: i64 = 10_000;
pub const BIG_SPENDER_USD: f64 = 0.50;
pub const TEAM_SIZE: usize = 3;
/// how long a popup stays up
pub const POPUP_MS: i64 = 3_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Unlock {
    pub kind: Kind,
    /// session ms when it was earned
    pub time: i64,
    /// short context, e.g. "scout · 7s"
    pub detail: String,
}

/// Achievements earned by time `t`, each kind at most once (its first
/// occurrence), in the order they were earned.
pub fn unlocks(s: &Session, t: i64) -> Vec<Unlock> {
    let scale = s.scale();
    let mut out: Vec<Unlock> = Vec::new();
    let mut first = |kind: Kind, time: i64, detail: String| {
        if time > t {
            return;
        }
        match out.iter_mut().find(|u| u.kind == kind) {
            Some(u) if u.time <= time => {}
            Some(u) => *u = Unlock { kind, time, detail },
            None => out.push(Unlock { kind, time, detail }),
        }
    };

    for a in &s.agents {
        let Some(f) = a.finish.filter(|f| *f <= t) else {
            continue;
        };
        // Elapsed as displayed (the demo runs ×3), so the popup never
        // contradicts the timer on screen.
        let took = ((f - a.spawn).max(0) as f64 * scale) as i64;
        if !a.error && took < SPEEDRUN_MS {
            first(Kind::Speedrun, f, format!("{} · {}", a.name, crate::duration::compact(took)));
        }
        if !a.error && !a.uses.iter().any(|u| u.edit && u.time <= f) {
            first(Kind::Pacifist, f, format!("{} · zero edits", a.name));
        }
    }

    for a in &s.agents {
        let mut spent = 0.0;
        for c in a.calls_at(t) {
            spent += c.cost;
            if spent > BIG_SPENDER_USD {
                first(Kind::BigSpender, c.time, format!("{} · ${spent:.2}", a.name));
                break;
            }
        }
    }

    // Bug hunter: across all agents (one may fail, another fix and re-run).
    let mut runs: Vec<(i64, bool, &str)> = s
        .agents
        .iter()
        .flat_map(|a| a.tests.iter().map(move |r| (r.time, r.passed, a.name.as_str())))
        .filter(|r| r.0 <= t)
        .collect();
    runs.sort_by_key(|r| r.0);
    let mut failed = false;
    for (time, passed, who) in runs {
        if !passed {
            failed = true;
        } else if failed {
            first(Kind::BugHunter, time, format!("{who} · red → green"));
            break;
        }
    }

    if let Some((time, n)) = team_moment(&s.agents, t) {
        first(Kind::TeamPlayer, time, format!("{n} agents at once"));
    }

    out.sort_by_key(|u| u.time);
    out
}

/// First moment `TEAM_SIZE`+ agents were running at the same time.
fn team_moment(agents: &[Agent], t: i64) -> Option<(i64, usize)> {
    let mut spawns: Vec<i64> = agents.iter().map(|a| a.spawn).filter(|s| *s <= t).collect();
    spawns.sort_unstable();
    spawns.into_iter().find_map(|at| {
        let n = agents
            .iter()
            .filter(|a| a.spawn <= at && a.finish.is_none_or(|f| f > at))
            .count();
        (n >= TEAM_SIZE).then_some((at, n))
    })
}

/// The popup to show at `t`, one at a time: each unlock earned at or after
/// `since` gets a `POPUP_MS` slot starting when it was earned or when the
/// previous popup ends, whichever is later. Returns the unlock and its age
/// within the slot (0.0 → 1.0) for the fade.
pub fn popup(unlocks: &[Unlock], since: i64, t: i64) -> Option<(&Unlock, f64)> {
    let mut free = i64::MIN;
    for u in unlocks.iter().filter(|u| u.time >= since) {
        let start = u.time.max(free);
        let end = start.saturating_add(POPUP_MS);
        if t >= start && t < end {
            return Some((u, (t - start) as f64 / POPUP_MS as f64));
        }
        free = end;
    }
    None
}

// ---- recap -----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Recap {
    pub cost: f64,
    pub tokens: u64,
    /// (agent, output tokens): who produced the most
    pub mvp: Option<(String, u64)>,
    /// (agent, ms): the longest-running task
    pub longest: Option<(String, i64)>,
    /// USD saved by cache reads
    pub savings: f64,
    /// cache hit rate 0..1 over prompt tokens
    pub hit: f64,
}

pub fn recap(s: &Session, t: i64) -> Recap {
    let mut u = Usage::default();
    let mut savings = 0.0;
    for c in s.all_calls_at(t) {
        u.add(&c.usage);
        savings += c.saved;
    }
    let mvp = s
        .agents
        .iter()
        .map(|a| (a.name.clone(), a.usage_at(t).output))
        .filter(|(_, o)| *o > 0)
        .max_by_key(|(_, o)| *o);
    let longest = s
        .agents
        .iter()
        .map(|a| {
            let end = a.finish.unwrap_or(t).min(t);
            (a.name.clone(), ((end - a.spawn).max(0) as f64 * s.scale()) as i64)
        })
        .max_by_key(|(_, ms)| *ms);
    Recap {
        cost: s.cost_at(t),
        tokens: u.total(),
        mvp,
        longest,
        savings,
        hit: hit_rate(&u),
    }
}

/// Share of prompt tokens served from cache.
pub fn hit_rate(u: &Usage) -> f64 {
    let prompt = u.input + u.cache_read + u.cache_write();
    if prompt == 0 {
        0.0
    } else {
        u.cache_read as f64 / prompt as f64
    }
}

// ---- stats panel -----------------------------------------------------------

/// Output tokens per bucket across the agent's life so far (spawn → finish or
/// `t`), `n` buckets. A call's output was generated between the previous call
/// and its own, so its tokens are spread over that gap: a rate curve rather
/// than a spike per call.
pub fn token_buckets(a: &Agent, t: i64, n: usize) -> Vec<u64> {
    let mut out = vec![0f64; n];
    let end = a.finish.unwrap_or(t).min(t);
    let span = (end - a.spawn).max(1) as f64;
    let pos = |ms: i64| (ms - a.spawn) as f64 / span * n as f64;
    let mut prev = a.spawn;
    for c in a.calls_at(t) {
        let (s, e) = (prev.clamp(a.spawn, end), c.time.clamp(a.spawn, end));
        prev = c.time;
        let tok = c.usage.output as f64;
        if e <= s {
            // no gap to spread over: lands in its own bucket
            out[(pos(e) as usize).min(n - 1)] += tok;
            continue;
        }
        let (fs, fe) = (pos(s), pos(e));
        for (i, b) in out.iter_mut().enumerate() {
            let overlap = (fe.min(i as f64 + 1.0) - fs.max(i as f64)).max(0.0);
            *b += tok * overlap / (fe - fs);
        }
    }
    out.into_iter().map(|x| x.round() as u64).collect()
}

/// Average output tokens/sec over the agent's life so far (real seconds).
pub fn tokens_per_sec(a: &Agent, t: i64, scale: f64) -> f64 {
    let end = a.finish.unwrap_or(t).min(t);
    let secs = ((end - a.spawn).max(0) as f64 * scale / 1000.0).max(1.0);
    a.usage_at(t).output as f64 / secs
}

/// "▁▃▇…" scaled to the largest bucket; empty buckets are spaces.
pub fn sparkline(v: &[u64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let max = v.iter().copied().max().unwrap_or(0);
    v.iter()
        .map(|&x| {
            if x == 0 || max == 0 {
                ' '
            } else {
                BARS[((x as f64 / max as f64) * 7.0).round() as usize]
            }
        })
        .collect()
}

/// Tool name → count, most used first (ties alphabetical), by time `t`.
pub fn top_tools(s: &Session, t: i64, n: usize) -> Vec<(String, usize)> {
    let mut m: HashMap<&str, usize> = HashMap::new();
    for u in s.agents.iter().flat_map(|a| a.uses.iter()).filter(|u| u.time <= t) {
        *m.entry(u.tool.as_str()).or_default() += 1;
    }
    let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, c)| (k.to_string(), c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

/// File → touch count (reads + writes), most touched first, by time `t`.
pub fn top_files(s: &Session, t: i64, n: usize) -> Vec<(String, usize)> {
    let mut m: HashMap<&str, usize> = HashMap::new();
    for u in s.agents.iter().flat_map(|a| a.uses.iter()).filter(|u| u.time <= t) {
        if let Some(f) = &u.file {
            *m.entry(f.as_str()).or_default() += 1;
        }
    }
    let mut v: Vec<(String, usize)> = m.into_iter().map(|(k, c)| (k.to_string(), c)).collect();
    v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    v.truncate(n);
    v
}

/// A braille intensity bar `cells` wide: full cells are ⣿, the last partial
/// cell has 1–7 dots, so 8 steps of resolution per cell.
pub fn braille_bar(value: usize, max: usize, cells: usize) -> String {
    const PART: [char; 8] = [' ', '⡀', '⣀', '⣄', '⣤', '⣦', '⣶', '⣷'];
    if max == 0 || cells == 0 {
        return " ".repeat(cells);
    }
    let dots = ((value as f64 / max as f64) * (cells * 8) as f64).round() as usize;
    let dots = dots.clamp(if value > 0 { 1 } else { 0 }, cells * 8);
    let mut s = "⣿".repeat(dots / 8);
    if dots % 8 > 0 {
        s.push(PART[dots % 8]);
    }
    let len = s.chars().count();
    s + &" ".repeat(cells - len)
}

// ---- personality -----------------------------------------------------------

/// What an agent mostly does, from its tool mix.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Personality {
    /// read / grep / glob
    Reader,
    /// edit / write
    Builder,
    /// shell commands
    Runner,
    /// web fetch / search
    Researcher,
}

impl Personality {
    /// Index order used by `tool_mix` arrays.
    pub const ALL: [Personality; 4] =
        [Personality::Reader, Personality::Builder, Personality::Runner, Personality::Researcher];

    pub fn label(self) -> &'static str {
        match self {
            Personality::Reader => "reader",
            Personality::Builder => "builder",
            Personality::Runner => "runner",
            Personality::Researcher => "researcher",
        }
    }

    /// Edits and web calls say more about intent than reads (which every agent
    /// does), so they count double.
    fn weight(self) -> u32 {
        match self {
            Personality::Builder | Personality::Researcher => 2,
            Personality::Reader | Personality::Runner => 1,
        }
    }

    /// Tie-break: the more specific habit wins.
    fn rank(self) -> u8 {
        match self {
            Personality::Builder => 3,
            Personality::Researcher => 2,
            Personality::Runner => 1,
            Personality::Reader => 0,
        }
    }
}

/// Which personality a tool counts toward (`None` for bookkeeping tools like
/// todo lists or spawning agents).
pub fn tool_category(tool: &str) -> Option<Personality> {
    let t: String = tool.to_lowercase().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    match t.as_str() {
        "read" | "grep" | "glob" | "ls" | "notebookread" => Some(Personality::Reader),
        "edit" | "write" | "multiedit" | "notebookedit" => Some(Personality::Builder),
        "bash" | "bashoutput" | "killshell" => Some(Personality::Runner),
        "webfetch" | "websearch" => Some(Personality::Researcher),
        _ => None,
    }
}

/// Raw call counts per category, in `Personality::ALL` order.
pub fn tool_mix<'a>(uses: impl Iterator<Item = &'a crate::model::Use>) -> [u32; 4] {
    let mut mix = [0u32; 4];
    for u in uses {
        if let Some(p) = tool_category(&u.tool) {
            mix[p as usize] += 1;
        }
    }
    mix
}

/// The dominant category by weighted count; `None` with no categorized calls.
pub fn classify(mix: [u32; 4]) -> Option<Personality> {
    Personality::ALL
        .into_iter()
        .filter(|p| mix[*p as usize] > 0)
        .max_by_key(|p| (mix[*p as usize] * p.weight(), p.rank()))
}

pub fn personality(a: &Agent, t: i64) -> Option<Personality> {
    classify(tool_mix(a.uses.iter().filter(|u| u.time <= t)))
}

// ---- stuck / parallelism ---------------------------------------------------

/// Default for "no transcript activity for this long reads as stuck".
pub const STUCK_MS: i64 = 120_000;

/// Latest sign of life by `t`: spawn, its last transcript entry, a tool call,
/// or a billed message.
pub fn last_activity(a: &Agent, t: i64) -> i64 {
    let mut last = a.spawn;
    if let Some(s) = a.last_seen {
        last = last.max(s.min(t));
    }
    for u in a.uses.iter().filter(|u| u.time <= t) {
        last = last.max(u.time);
    }
    for c in a.calls_at(t) {
        last = last.max(c.time);
    }
    last
}

/// Silent for at least `after_ms` while still running: how long, in ms.
/// Finished and errored agents never are; `after_ms <= 0` disables it.
fn quiet_for(a: &Agent, t: i64, after_ms: i64) -> Option<i64> {
    if after_ms <= 0 || a.error || t < a.spawn || a.finish.is_some_and(|f| f <= t) {
        return None;
    }
    let idle = t - last_activity(a, t);
    (idle >= after_ms).then_some(idle)
}

/// A tool call it made is still waiting for its result at `t`.
pub fn in_flight(a: &Agent, t: i64) -> bool {
    a.in_flight.is_some_and(|s| s <= t)
}

/// How long a still-running agent has been silent, if at least `after_ms`
/// and nothing is in flight: it is dozing, not waiting on a slow command.
pub fn stuck_idle(a: &Agent, t: i64, after_ms: i64) -> Option<i64> {
    if in_flight(a, t) {
        return None;
    }
    quiet_for(a, t, after_ms)
}

/// Silent past the threshold *with* a tool call in flight (a slow build, a
/// long test run): busy, not stuck. Returns how long that call has run.
pub fn running_long(a: &Agent, t: i64, after_ms: i64) -> Option<i64> {
    let start = a.in_flight.filter(|s| *s <= t)?;
    quiet_for(a, t, after_ms).map(|_| t - start)
}

/// Most agents running at the same moment, by `t`.
pub fn peak_parallel(agents: &[Agent], t: i64) -> usize {
    agents
        .iter()
        .filter(|a| a.spawn <= t)
        .map(|at| {
            agents
                .iter()
                .filter(|a| a.spawn <= at.spawn && a.finish.is_none_or(|f| f > at.spawn))
                .count()
        })
        .max()
        .unwrap_or(0)
}

/// How parallel the agents ran, by `t`. Each agent runs from its dispatch to
/// its finish (or `t`); the window is the first dispatch to the last end.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Parallelism {
    /// most agents running at once
    pub peak: usize,
    /// summed agent run time (ms)
    pub work_ms: i64,
    /// wall-clock from the first dispatch to the last agent's end (ms)
    pub wall_ms: i64,
    /// time with 2+ agents running (ms)
    pub multi_ms: i64,
}

impl Parallelism {
    /// Average agents running over the window (time-weighted): work ÷ wall.
    pub fn avg(&self) -> f64 {
        if self.wall_ms <= 0 {
            0.0
        } else {
            self.work_ms as f64 / self.wall_ms as f64
        }
    }
    /// Share of the window (0..1) with 2+ agents running.
    pub fn multi_share(&self) -> f64 {
        if self.wall_ms <= 0 {
            0.0
        } else {
            self.multi_ms as f64 / self.wall_ms as f64
        }
    }
    /// Agent time that overlapped: work done beyond the wall-clock it took.
    pub fn saved_ms(&self) -> i64 {
        (self.work_ms - self.wall_ms).max(0)
    }
}

pub fn parallelism(agents: &[Agent], t: i64) -> Parallelism {
    let spans: Vec<(i64, i64)> = agents
        .iter()
        .filter(|a| a.spawn <= t)
        .map(|a| (a.spawn, a.finish.unwrap_or(t).min(t).max(a.spawn)))
        .collect();
    let (Some(start), Some(end)) = (spans.iter().map(|s| s.0).min(), spans.iter().map(|s| s.1).max()) else {
        return Parallelism::default();
    };
    // Sweep the starts and ends; at the same instant ends go first, so one
    // agent handing straight to the next doesn't count as overlap.
    let mut ev: Vec<(i64, i32)> =
        spans.iter().filter(|(s, e)| e > s).flat_map(|&(s, e)| [(s, 1), (e, -1)]).collect();
    ev.sort_unstable();
    let (mut n, mut prev, mut multi) = (0i32, start, 0i64);
    for (x, d) in ev {
        if n >= 2 {
            multi += x - prev;
        }
        prev = x;
        n += d;
    }
    Parallelism {
        peak: peak_parallel(agents, t),
        work_ms: spans.iter().map(|(s, e)| e - s).sum(),
        wall_ms: end - start,
        multi_ms: multi,
    }
}

/// The chain of agents that set how long the session took, in time order.
/// Start from the agent that ended last (among ties, the one running
/// longest, then the first dispatched), then step back to the agent it waited on: the last one to finish
/// before it was dispatched (a handoff to it breaks ties). Stop when nothing
/// had finished before it.
pub fn critical_path(sess: &Session, t: i64) -> Vec<usize> {
    let a = &sess.agents;
    let end = |i: usize| a[i].finish.unwrap_or(t).min(t);
    let started: Vec<usize> = (0..a.len()).filter(|&i| a[i].spawn <= t).collect();
    let Some(mut cur) = started.iter().copied().max_by_key(|&i| (end(i), std::cmp::Reverse((a[i].spawn, i)))) else {
        return Vec::new();
    };
    let mut path = vec![cur];
    loop {
        let spawn = a[cur].spawn;
        let fed = |i: usize| sess.handoffs.iter().any(|h| h.from == i && h.to == cur);
        let prev = started
            .iter()
            .copied()
            .filter(|&i| a[i].finish.is_some_and(|f| f <= spawn && f <= t))
            .max_by_key(|&i| (a[i].finish, fed(i), a[i].spawn));
        match prev {
            Some(p) if !path.contains(&p) => {
                path.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    path.reverse();
    path
}

/// Event counts per cell over [0, span): drives swimlane braille density.
pub fn density(times: impl Iterator<Item = i64>, span: i64, cells: usize) -> Vec<u32> {
    let mut out = vec![0u32; cells];
    let span = span.max(1) as i128;
    for t in times {
        if (0..span).contains(&(t as i128)) {
            let i = (t as i128 * cells as i128 / span) as usize;
            out[i.min(cells - 1)] += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Call, TestRun, Use};

    fn tool(time: i64, name: &str) -> Use {
        Use { time, tool: name.into(), file: None, edit: name == "edit", arg: String::new() }
    }

    #[test]
    fn personality_from_tool_mix() {
        let mix = |names: &[&str]| tool_mix(names.iter().map(|n| tool(0, n)).collect::<Vec<_>>().iter());
        assert_eq!(classify(mix(&["read", "grep", "Glob", "read"])), Some(Personality::Reader));
        assert_eq!(classify(mix(&["bash", "bash", "read"])), Some(Personality::Runner));
        assert_eq!(classify(mix(&["WebFetch", "web_search", "read", "read", "read"])), Some(Personality::Researcher));
        // edits count double: 2 edits beat 3 reads
        assert_eq!(classify(mix(&["read", "read", "read", "edit", "MultiEdit"])), Some(Personality::Builder));
        // ties go to the more specific habit
        assert_eq!(classify(mix(&["read", "bash"])), Some(Personality::Runner));
        assert_eq!(classify(mix(&["webfetch", "edit"])), Some(Personality::Builder));
        // bookkeeping tools don't count; nothing → no label
        assert_eq!(classify(mix(&["todowrite", "agent"])), None);
        assert_eq!(classify([0; 4]), None);
        // only calls made by `t` count
        let mut a = agent("a", 0, None);
        a.uses = vec![tool(10, "read"), tool(20, "edit"), tool(30, "edit")];
        assert_eq!(personality(&a, 15), Some(Personality::Reader));
        assert_eq!(personality(&a, 40), Some(Personality::Builder));
    }

    #[test]
    fn stuck_after_threshold_and_clears_on_activity() {
        let mut a = agent("a", 0, None);
        a.uses = vec![tool(1_000, "bash")];
        assert_eq!(stuck_idle(&a, 120_999, STUCK_MS), None, "just under 2m");
        assert_eq!(stuck_idle(&a, 121_000, STUCK_MS), Some(120_000));
        // new transcript activity (any entry, not just a tool call) resets it
        a.last_seen = Some(100_000);
        assert_eq!(stuck_idle(&a, 121_000, STUCK_MS), None);
        assert_eq!(stuck_idle(&a, 220_000, STUCK_MS), Some(120_000));
        // a billed message counts as activity too
        a.calls = vec![call(200_000, 0.0, 1)];
        assert_eq!(stuck_idle(&a, 220_000, STUCK_MS), None);
        // threshold from config
        assert_eq!(stuck_idle(&a, 230_000, 30_000), Some(30_000));
        assert_eq!(stuck_idle(&a, 230_000, 0), None, "0 disables");
    }

    /// A tool call still in flight (tool_use, no tool_result yet) means busy:
    /// "busy 2m 30s", never the sleepy face. Only silence with nothing in
    /// flight reads as stuck.
    #[test]
    fn in_flight_tool_call_is_running_not_stuck() {
        let mut a = agent("builder", 0, None);
        a.last_seen = Some(1_000);
        a.in_flight = Some(1_000); // `cargo build` issued at 1s, no result yet
        let t = 1_000 + 150_000;
        assert_eq!(stuck_idle(&a, t, STUCK_MS), None, "in flight: not stuck");
        assert_eq!(running_long(&a, t, STUCK_MS), Some(150_000));
        assert_eq!(running_long(&a, 60_000, STUCK_MS), None, "not quiet long enough yet");
        assert_eq!(running_long(&a, t, 0), None, "0 disables");
        // the result comes back: nothing in flight, silence is stuck again
        a.in_flight = None;
        assert_eq!(stuck_idle(&a, t, STUCK_MS), Some(150_000));
        assert_eq!(running_long(&a, t, STUCK_MS), None);
        // a call issued after `t` isn't in flight yet at `t`
        a.in_flight = Some(t + 1);
        assert!(stuck_idle(&a, t, STUCK_MS).is_some());
        // finished agents are neither
        a.in_flight = Some(1_000);
        a.finish = Some(100_000);
        assert_eq!(running_long(&a, t, STUCK_MS), None);
    }

    #[test]
    fn finished_or_errored_agents_are_never_stuck() {
        let a = agent("a", 0, Some(5_000));
        assert_eq!(stuck_idle(&a, 4_000, 1_000), Some(4_000), "still running at 4s");
        assert_eq!(stuck_idle(&a, 500_000, 1_000), None);
        let mut e = agent("e", 0, None);
        e.error = true;
        assert_eq!(stuck_idle(&e, 500_000, 1_000), None);
        // spawned with nothing since: stuck counts from spawn
        assert_eq!(stuck_idle(&agent("s", 100, None), 1_100, 1_000), Some(1_000));
    }

    #[test]
    fn parallelism_and_density() {
        let agents = vec![
            agent("a", 0, Some(500)),
            agent("b", 100, None),
            agent("c", 200, Some(300)),
            agent("d", 600, None),
        ];
        assert_eq!(peak_parallel(&agents, 1_000), 3);
        assert_eq!(peak_parallel(&agents, 150), 2);
        assert_eq!(peak_parallel(&[], 10), 0);
        assert_eq!(density([0, 1, 5, 9, 10, -1].into_iter(), 10, 2), vec![2, 2]);
    }

    #[test]
    fn parallelism_math() {
        // a 0–4s, b 1–3s overlap for 2s; c runs alone 5–6s after a 1s gap
        let agents = vec![agent("a", 0, Some(4_000)), agent("b", 1_000, Some(3_000)), agent("c", 5_000, Some(6_000))];
        let p = parallelism(&agents, 10_000);
        assert_eq!((p.peak, p.work_ms, p.wall_ms, p.multi_ms), (2, 7_000, 6_000, 2_000));
        assert!((p.avg() - 7.0 / 6.0).abs() < 1e-9, "time-weighted average = work / wall");
        assert!((p.multi_share() - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(p.saved_ms(), 1_000, "7s of work in 6s");

        // at t=2s: only what has happened so far (a 0–2, b 1–2)
        let p = parallelism(&agents, 2_000);
        assert_eq!((p.work_ms, p.wall_ms, p.multi_ms), (3_000, 2_000, 1_000));

        // three fully parallel: 9s of work in 3s
        let par = vec![agent("a", 0, Some(3_000)), agent("b", 0, Some(3_000)), agent("c", 0, Some(3_000))];
        let p = parallelism(&par, 10_000);
        assert_eq!((p.peak, p.work_ms, p.wall_ms, p.multi_ms, p.saved_ms()), (3, 9_000, 3_000, 3_000, 6_000));
        assert!((p.avg() - 3.0).abs() < 1e-9 && (p.multi_share() - 1.0).abs() < 1e-9);

        // back to back (a handoff): no overlap, nothing saved
        let seq = vec![agent("a", 0, Some(1_000)), agent("b", 1_000, Some(2_000))];
        let p = parallelism(&seq, 5_000);
        assert_eq!((p.multi_ms, p.saved_ms(), p.wall_ms), (0, 0, 2_000));
        assert!((p.avg() - 1.0).abs() < 1e-9);

        // still running: clipped at t; nothing at all: zeros, no division by zero
        let p = parallelism(&[agent("a", 1_000, None)], 1_500);
        assert_eq!((p.work_ms, p.wall_ms, p.peak), (500, 500, 1));
        let z = parallelism(&[], 1_000);
        assert_eq!(z, Parallelism::default());
        assert_eq!((z.avg(), z.multi_share(), z.saved_ms()), (0.0, 0.0, 0));
    }

    #[test]
    fn critical_path_follows_what_gated_each_dispatch() {
        use crate::model::Handoff;
        let h = |from, to| Handoff { from, to, time: 0, what: String::new(), size: String::new() };
        // r 0–2s → f 2.5–6s → v 6.5–8s, beside x 0–5s (parallel, not gating)
        let mut s = live(vec![
            agent("r", 0, Some(2_000)),
            agent("x", 0, Some(5_000)),
            agent("f", 2_500, Some(6_000)),
            agent("v", 6_500, Some(8_000)),
        ]);
        assert_eq!(critical_path(&s, 9_000), vec![0, 2, 3]);
        // mid-run: x and f both run until "now"; x has run longest (since 0),
        // so x alone accounts for the elapsed time
        assert_eq!(critical_path(&s, 4_000), vec![1]);
        // once x is done, f (still running) ends last, gated by r
        assert_eq!(critical_path(&s, 5_500), vec![0, 2]);
        // and before anything finished: just the longest-running agent
        assert_eq!(critical_path(&s, 1_000), vec![0]);

        // the orchestrator also waited on y (done at 6.2s) before v: y gated it,
        // even though f handed v its task
        s.agents.push(agent("y", 3_000, Some(6_200)));
        s.handoffs = vec![h(2, 3)];
        assert_eq!(critical_path(&s, 9_000), vec![0, 4, 3]);
        // a tie on finish time goes to the handoff source
        s.agents[4].finish = Some(6_000);
        assert_eq!(critical_path(&s, 9_000), vec![0, 2, 3]);
        s.handoffs.clear();
        // no handoff: the tie goes to the later dispatch (y)
        assert_eq!(critical_path(&s, 9_000), vec![0, 4, 3]);

        assert!(critical_path(&live(vec![]), 1_000).is_empty());
    }

    fn agent(name: &str, spawn: i64, finish: Option<i64>) -> Agent {
        Agent { name: name.into(), spawn, finish, ..Agent::default() }
    }

    fn live(agents: Vec<Agent>) -> Session {
        Session { agents, all: i64::MAX, live: true, ..Session::default() }
    }

    fn call(time: i64, cost: f64, output: u64) -> Call {
        Call { time, cost, saved: 0.0, usage: Usage { output, ..Usage::default() } }
    }

    fn kinds(s: &Session, t: i64) -> Vec<Kind> {
        unlocks(s, t).into_iter().map(|u| u.kind).collect()
    }

    #[test]
    fn speedrun_needs_under_ten_seconds_and_no_error() {
        let s = live(vec![agent("a", 0, Some(9_999))]);
        assert!(kinds(&s, 20_000).contains(&Kind::Speedrun));
        let s = live(vec![agent("a", 0, Some(10_000))]);
        assert!(!kinds(&s, 20_000).contains(&Kind::Speedrun));
        let mut e = agent("a", 0, Some(500));
        e.error = true;
        assert!(!kinds(&live(vec![e]), 20_000).contains(&Kind::Speedrun));
        // not yet earned before it finishes
        let s = live(vec![agent("a", 0, Some(5_000))]);
        assert!(kinds(&s, 4_999).is_empty());
    }

    #[test]
    fn speedrun_uses_displayed_time_in_demo() {
        // 5s real in the ×3 demo shows as 15s: no speedrun
        let mut s = live(vec![agent("a", 0, Some(5_000))]);
        s.live = false;
        assert!(!kinds(&s, 20_000).contains(&Kind::Speedrun));
    }

    #[test]
    fn pacifist_is_zero_edits() {
        let mut reader = agent("scout", 0, Some(20_000));
        reader.uses.push(Use { time: 1, tool: "read".into(), file: Some("a.rs".into()), edit: false, arg: String::new() });
        let mut writer = agent("fixer", 0, Some(15_000));
        writer.uses.push(Use { time: 1, tool: "edit".into(), file: Some("a.rs".into()), edit: true, arg: String::new() });
        let u = unlocks(&live(vec![reader, writer]), 30_000);
        let p: Vec<_> = u.iter().filter(|u| u.kind == Kind::Pacifist).collect();
        assert_eq!(p.len(), 1);
        assert_eq!(p[0].time, 20_000);
        assert!(p[0].detail.starts_with("scout"));
        // still running: not a pacifist yet
        assert!(!kinds(&live(vec![agent("x", 0, None)]), 30_000).contains(&Kind::Pacifist));
    }

    #[test]
    fn bug_hunter_needs_fail_then_pass_across_agents() {
        let mut a = agent("test-runner", 0, None);
        a.tests.push(TestRun { time: 1_000, passed: false });
        let mut b = agent("fixer", 0, None);
        b.tests.push(TestRun { time: 3_000, passed: true });
        let s = live(vec![a, b]);
        let u = unlocks(&s, 10_000);
        let hit = u.iter().find(|u| u.kind == Kind::BugHunter).unwrap();
        assert_eq!(hit.time, 3_000);
        assert!(hit.detail.starts_with("fixer"));
        assert!(!kinds(&s, 2_000).contains(&Kind::BugHunter));

        // pass then fail is not a fix; pass-only isn't either
        let mut c = agent("x", 0, None);
        c.tests = vec![TestRun { time: 1, passed: true }, TestRun { time: 2, passed: false }];
        assert!(!kinds(&live(vec![c]), 10).contains(&Kind::BugHunter));
    }

    #[test]
    fn big_spender_fires_on_the_call_that_crosses() {
        let mut a = agent("researcher", 0, None);
        a.calls = vec![call(100, 0.30, 10), call(200, 0.15, 10), call(300, 0.10, 10)];
        let s = live(vec![a]);
        let u = unlocks(&s, 1_000);
        let b = u.iter().find(|u| u.kind == Kind::BigSpender).unwrap();
        assert_eq!(b.time, 300);
        assert!(!kinds(&s, 250).contains(&Kind::BigSpender));
        // exactly $0.50 is not "more than"
        let mut e = agent("x", 0, None);
        e.calls = vec![call(1, 0.25, 0), call(2, 0.25, 0)];
        assert!(!kinds(&live(vec![e]), 10).contains(&Kind::BigSpender));
    }

    #[test]
    fn team_player_needs_three_overlapping() {
        // sequential: never 3 at once
        let s = live(vec![
            agent("a", 0, Some(100)),
            agent("b", 200, Some(300)),
            agent("c", 400, None),
        ]);
        assert!(!kinds(&s, 1_000).contains(&Kind::TeamPlayer));
        // overlapping: earned when the third starts
        let s = live(vec![agent("a", 0, None), agent("b", 50, Some(900)), agent("c", 400, None)]);
        let u = unlocks(&s, 1_000);
        assert_eq!(u.iter().find(|u| u.kind == Kind::TeamPlayer).unwrap().time, 400);
    }

    #[test]
    fn each_kind_once_in_time_order() {
        let s = live(vec![agent("a", 0, Some(1_000)), agent("b", 0, Some(2_000))]);
        let u = unlocks(&s, 5_000);
        // both are speedruns and pacifists; only the first of each counts
        assert_eq!(u.iter().filter(|u| u.kind == Kind::Speedrun).count(), 1);
        assert_eq!(u.iter().filter(|u| u.kind == Kind::Pacifist).count(), 1);
        assert!(u.windows(2).all(|w| w[0].time <= w[1].time));
    }

    #[test]
    fn popups_queue_one_at_a_time_and_skip_old_ones() {
        let u = |kind, time| Unlock { kind, time, detail: String::new() };
        let list = vec![u(Kind::TeamPlayer, 1_000), u(Kind::Speedrun, 2_000), u(Kind::Pacifist, 9_000)];
        assert!(popup(&list, 0, 999).is_none());
        assert_eq!(popup(&list, 0, 1_000).unwrap().0.kind, Kind::TeamPlayer);
        // the second waits for the first slot to end
        assert_eq!(popup(&list, 0, 3_999).unwrap().0.kind, Kind::TeamPlayer);
        assert_eq!(popup(&list, 0, 4_000).unwrap().0.kind, Kind::Speedrun);
        let (_, age) = popup(&list, 0, 5_500).unwrap();
        assert!((age - 0.5).abs() < 1e-9);
        assert!(popup(&list, 0, 7_000).is_none());
        assert_eq!(popup(&list, 0, 9_000).unwrap().0.kind, Kind::Pacifist);
        // earned before we started watching: never shown
        assert!(popup(&list, 5_000, 1_500).is_none());
        assert_eq!(popup(&list, 5_000, 9_100).unwrap().0.kind, Kind::Pacifist);
    }

    #[test]
    fn recap_numbers() {
        let mut a = agent("scout", 0, Some(4_000));
        a.calls = vec![Call {
            time: 100,
            cost: 0.10,
            saved: 0.05,
            usage: Usage { input: 10, output: 500, cache_read: 900, cache_write_5m: 90, ..Usage::default() },
        }];
        let mut b = agent("researcher", 1_000, Some(9_000));
        b.calls = vec![call(2_000, 0.40, 200)];
        let mut s = live(vec![a, b]);
        s.orch_calls = vec![call(0, 0.05, 50)];
        let r = recap(&s, 10_000);
        assert!((r.cost - 0.55).abs() < 1e-9);
        assert_eq!(r.tokens, 10 + 500 + 900 + 90 + 200 + 50);
        assert_eq!(r.mvp, Some(("scout".into(), 500)));
        assert_eq!(r.longest, Some(("researcher".into(), 8_000)));
        assert!((r.savings - 0.05).abs() < 1e-9);
        assert!((r.hit - 0.9).abs() < 1e-9);
    }

    #[test]
    fn panel_helpers() {
        assert_eq!(sparkline(&[0, 1, 2, 4, 8]), " ▂▃▅█");
        assert_eq!(braille_bar(10, 10, 3), "⣿⣿⣿");
        assert_eq!(braille_bar(0, 10, 3), "   ");
        assert_eq!(braille_bar(5, 10, 3), "⣿⣤ ");
        assert_eq!(braille_bar(1, 1000, 3).chars().next(), Some('⡀'), "tiny but visible");

        let mut a = agent("a", 0, Some(10_000));
        // 10 tokens at spawn (no gap: own bucket), 40 spread over 0–10s,
        // 5 more right at the end (no gap since the previous call)
        a.calls = vec![call(0, 0.0, 10), call(10_000, 0.0, 40), call(10_000, 0.0, 5)];
        assert_eq!(token_buckets(&a, 20_000, 4), vec![20, 10, 10, 15]);
        assert!((tokens_per_sec(&a, 20_000, 1.0) - 5.5).abs() < 1e-9);
        // still running: only calls so far, window ends at t
        let mut r = agent("r", 0, None);
        r.calls = vec![call(2_000, 0.0, 20), call(9_000, 0.0, 99)];
        assert_eq!(token_buckets(&r, 4_000, 2), vec![20, 0]);

        let mut b = agent("b", 0, None);
        let use_ = |tool: &str, file: Option<&str>| Use {
            time: 1,
            tool: tool.into(),
            file: file.map(str::to_string),
            edit: tool == "edit",
            arg: String::new(),
        };
        b.uses = vec![
            use_("read", Some("x.rs")),
            use_("read", Some("y.rs")),
            use_("edit", Some("x.rs")),
            use_("bash", None),
            use_("read", Some("x.rs")),
        ];
        let s = live(vec![b]);
        assert_eq!(top_tools(&s, 5, 2), vec![("read".into(), 3), ("bash".into(), 1)]);
        assert_eq!(top_files(&s, 5, 5), vec![("x.rs".into(), 3), ("y.rs".into(), 1)]);
    }
}
