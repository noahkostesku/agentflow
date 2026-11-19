//! `agentflow profile` and `agentflow compare`: is a workflow getting better
//! or worse, and which agent should you fix?
//!
//! Reads run history (`runs.rs`, `~/.agentflow/runs.jsonl`). The list shows
//! each workflow's runs, median wall time and ≈cost with a braille trend; the
//! detail view breaks a workflow down by role, flags regressions (a role or the
//! whole run more than 25% slower or costlier than its median over the
//! previous 10 runs) and offers a few plain-language insights once there are
//! enough runs. `compare` lays two runs side by side, across harnesses too.

use std::collections::{BTreeMap, HashMap};
use std::io::{self, IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use crossterm::event::{KeyCode, KeyModifiers};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::duration::{compact, human};
use crate::pricing::{fmt_cost, fmt_tokens, Pricing};
use crate::runs::{self, AgentRun, Run, Workflow, WorkflowRule};
use crate::schema::Harness;
use crate::theme::{self, AM, B, D, G, RED, VD};

/// A run (or a role in it) regressed when it is this much slower or costlier
/// than its median over the previous runs…
pub const REGRESSION: f64 = 0.25;
/// …counting at most this many previous runs…
pub const BASELINE_RUNS: usize = 10;
/// …and only with at least this many to compare against.
pub const MIN_BASELINE: usize = 3;
/// Differences this small are noise, whatever the percentage (2s, half a cent).
const FLOOR_MS: f64 = 2_000.0;
const FLOOR_USD: f64 = 0.005;

/// Insights need this many runs in the window.
pub const INSIGHT_RUNS: usize = 5;
pub const MAX_INSIGHTS: usize = 3;
/// A role that is sometimes off the critical path, but on it this often.
pub const CRITICAL_SHARE: f64 = 0.6;
/// A role always on the critical path that takes this share of the wall time.
pub const DOMINANT_SHARE: f64 = 0.4;
/// Two roles whose median times are within this factor run "similarly long"…
pub const SIMILAR_TIME: f64 = 1.5;
/// …and one costing this many times the other is worth a look.
pub const COST_RATIO: f64 = 4.0;
/// A role ending in error, or tests failing at the end, this often.
pub const FAIL_SHARE: f64 = 0.25;
/// Median agents running at once below this, with 3+ roles: mostly serial.
pub const SERIAL_AVG: f64 = 1.2;

/// Runs the detail view's trend and stats cover by default (`--last`).
pub const DEFAULT_LAST: usize = 20;
/// Content width.
const W: usize = 80;

// ---- numbers ------------------------------------------------------------------

pub fn median(v: &[f64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.total_cmp(b));
    let n = s.len();
    if n % 2 == 1 {
        s[n / 2]
    } else {
        (s[n / 2 - 1] + s[n / 2]) / 2.0
    }
}

/// How much worse `value` is than the median of `history` (oldest first; the
/// last [`BASELINE_RUNS`] count), if it regressed: more than [`REGRESSION`]
/// above it and by at least `floor`.
pub fn regression(history: &[f64], value: f64, floor: f64) -> Option<f64> {
    let h = &history[history.len().saturating_sub(BASELINE_RUNS)..];
    if h.len() < MIN_BASELINE {
        return None;
    }
    let base = median(h);
    if base <= 0.0 {
        return None;
    }
    let pct = value / base - 1.0;
    (pct > REGRESSION && value - base >= floor).then_some(pct)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Metric {
    Time,
    Cost,
}

/// One regression in a run: the whole run (`who` = None) or one role.
#[derive(Clone, Debug, PartialEq)]
pub struct Flag {
    pub who: Option<String>,
    pub metric: Metric,
    pub pct: f64,
}

/// A role's share of one run: its instances summed.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RoleRun {
    pub ms: i64,
    pub cost: f64,
    pub model: String,
    pub critical: bool,
    pub error: bool,
}

/// Per role in a run, in the order the roles first appear.
pub fn by_role(run: &Run) -> Vec<(String, RoleRun)> {
    let mut out: Vec<(String, RoleRun)> = Vec::new();
    for a in &run.agents {
        let i = match out.iter().position(|(r, _)| *r == a.role) {
            Some(i) => i,
            None => {
                out.push((a.role.clone(), RoleRun { model: a.model.clone(), ..RoleRun::default() }));
                out.len() - 1
            }
        };
        let r = &mut out[i].1;
        r.ms += a.ms;
        r.cost += a.cost;
        r.critical |= a.critical;
        r.error |= a.error;
    }
    out
}

fn role_in(run: &Run, role: &str) -> Option<RoleRun> {
    by_role(run).into_iter().find(|(r, _)| r == role).map(|(_, r)| r)
}

/// Regressions in `runs[i]` (oldest first) against the runs before it: the
/// whole run's wall time and cost, then each role's time and cost.
pub fn flags(runs: &[Run], i: usize) -> Vec<Flag> {
    let (prev, run) = (&runs[..i], &runs[i]);
    let mut out = Vec::new();
    let mut check = |who: Option<&str>, metric: Metric, hist: Vec<f64>, v: f64| {
        let floor = if metric == Metric::Time { FLOOR_MS } else { FLOOR_USD };
        if let Some(pct) = regression(&hist, v, floor) {
            out.push(Flag { who: who.map(str::to_string), metric, pct });
        }
    };
    check(None, Metric::Time, prev.iter().map(|r| r.wall_ms as f64).collect(), run.wall_ms as f64);
    check(None, Metric::Cost, prev.iter().map(|r| r.cost).collect(), run.cost);
    for (role, now) in by_role(run) {
        let hist: Vec<RoleRun> = prev.iter().filter_map(|r| role_in(r, &role)).collect();
        check(Some(&role), Metric::Time, hist.iter().map(|r| r.ms as f64).collect(), now.ms as f64);
        check(Some(&role), Metric::Cost, hist.iter().map(|r| r.cost).collect(), now.cost);
    }
    out
}

/// One role across a workflow's runs.
#[derive(Clone, Debug, PartialEq)]
pub struct RoleStat {
    pub role: String,
    /// its model in the latest run it was in
    pub model: String,
    /// runs it was in
    pub runs: usize,
    pub median_ms: i64,
    pub median_cost: f64,
    /// share of its runs it was on the critical path in
    pub critical: f64,
    /// runs where it ended in error
    pub errors: usize,
}

/// Per-role numbers over `runs`, in the order the latest run used them.
pub fn role_stats(runs: &[Run]) -> Vec<RoleStat> {
    let mut order: Vec<String> = Vec::new();
    for run in runs.iter().rev() {
        for (role, _) in by_role(run) {
            if !order.contains(&role) {
                order.push(role);
            }
        }
    }
    order
        .into_iter()
        .map(|role| {
            let seen: Vec<RoleRun> = runs.iter().filter_map(|r| role_in(r, &role)).collect();
            let n = seen.len().max(1) as f64;
            RoleStat {
                model: seen.last().map(|r| r.model.clone()).unwrap_or_default(),
                runs: seen.len(),
                median_ms: median(&seen.iter().map(|r| r.ms as f64).collect::<Vec<_>>()) as i64,
                median_cost: median(&seen.iter().map(|r| r.cost).collect::<Vec<_>>()),
                critical: seen.iter().filter(|r| r.critical).count() as f64 / n,
                errors: seen.iter().filter(|r| r.error).count(),
                role,
            }
        })
        .collect()
}

/// Up to [`MAX_INSIGHTS`] plain-language observations about `runs` (oldest
/// first), most useful first; none below [`INSIGHT_RUNS`] runs.
pub fn insights(runs: &[Run]) -> Vec<String> {
    let n = runs.len();
    if n < INSIGHT_RUNS {
        return Vec::new();
    }
    let stats = role_stats(runs);
    let wall = median(&runs.iter().map(|r| r.wall_ms as f64).collect::<Vec<_>>());
    let mut out = Vec::new();

    // drifting: the last 5 runs against the 5 before
    if n >= 10 {
        let half = |f: &dyn Fn(&Run) -> f64| {
            let v: Vec<f64> = runs.iter().map(f).collect();
            (median(&v[n - 5..]), median(&v[n - 10..n - 5]))
        };
        let (now, before) = half(&|r| r.wall_ms as f64);
        if before > 0.0 && now > before * (1.0 + REGRESSION) && now - before >= FLOOR_MS {
            out.push(format!(
                "runs are getting slower: median {} over the last 5, {} over the 5 before",
                compact(now as i64),
                compact(before as i64)
            ));
        }
        let (now, before) = half(&|r| r.cost);
        if before > 0.0 && now > before * (1.0 + REGRESSION) && now - before >= FLOOR_USD {
            out.push(format!(
                "runs are getting costlier: median ≈{} over the last 5, ≈{} over the 5 before",
                fmt_cost(now),
                fmt_cost(before)
            ));
        }
    }

    // what sets the pace: a role in a parallel stage that usually finishes
    // last, else one long role that every run waits on
    if stats.len() >= 2 {
        let gate = stats
            .iter()
            .filter(|s| s.runs >= INSIGHT_RUNS && s.critical >= CRITICAL_SHARE && s.critical < 1.0)
            .max_by(|a, b| a.critical.total_cmp(&b.critical));
        let dominant = stats
            .iter()
            .filter(|s| s.runs == n && s.critical >= 1.0 && wall > 0.0 && s.median_ms as f64 / wall >= DOMINANT_SHARE)
            .max_by_key(|s| s.median_ms);
        if let Some(s) = gate {
            out.push(format!(
                "{} is on the critical path in {} of runs: speeding it up shortens most runs",
                s.role,
                percent(s.critical)
            ));
        } else if let Some(s) = dominant {
            out.push(format!(
                "every run waits on {}: it takes {} of the wall time (median {})",
                s.role,
                percent(s.median_ms as f64 / wall),
                compact(s.median_ms)
            ));
        }
    }

    // a pricey model doing a job about as long as a cheap one's
    let mut best: Option<(f64, &RoleStat, &RoleStat)> = None;
    for a in &stats {
        for b in &stats {
            let known = !a.model.is_empty() && !b.model.is_empty() && a.model != b.model;
            let (ta, tb) = (a.median_ms.max(1) as f64, b.median_ms.max(1) as f64);
            if !known || b.median_cost <= 0.0 || ta.max(tb) / ta.min(tb) > SIMILAR_TIME {
                continue;
            }
            let ratio = a.median_cost / b.median_cost;
            if ratio >= COST_RATIO && best.is_none_or(|(r, ..)| ratio > r) {
                best = Some((ratio, a, b));
            }
        }
    }
    if let Some((ratio, a, b)) = best {
        out.push(format!(
            "{} on {} costs {:.0}× {} on {} with similar duration",
            a.role, a.model, ratio, b.role, b.model
        ));
    }

    if let Some(s) = stats.iter().filter(|s| s.runs >= INSIGHT_RUNS).max_by_key(|s| s.errors) {
        if s.errors as f64 / s.runs as f64 >= FAIL_SHARE {
            out.push(format!("{} ends in an error in {} of {} runs", s.role, s.errors, s.runs));
        }
    }
    let tested: Vec<bool> = runs.iter().filter_map(|r| r.tests).collect();
    let failed = tested.iter().filter(|p| !**p).count();
    if tested.len() >= INSIGHT_RUNS && failed as f64 / tested.len() as f64 >= FAIL_SHARE {
        out.push(format!("tests fail at the end of {failed} of {} runs", tested.len()));
    }

    let avg = median(&runs.iter().map(|r| r.avg).collect::<Vec<_>>());
    if stats.len() >= 3 && avg > 0.0 && avg < SERIAL_AVG {
        out.push(format!(
            "agents mostly run one at a time (avg {avg:.1} at once): roles that don't depend on each other could run in parallel"
        ));
    }
    out.truncate(MAX_INSIGHTS);
    out
}

// ---- formatting -----------------------------------------------------------------

fn percent(share: f64) -> String {
    format!("{:.0}%", share * 100.0)
}

/// "+43%", "-25%"
fn delta(pct: f64) -> String {
    format!("{}{:.0}%", if pct >= 0.0 { "+" } else { "-" }, pct.abs() * 100.0)
}

/// "nov 19 14:02" in local time.
fn when(ms: i64) -> String {
    use chrono::{Local, TimeZone};
    Local.timestamp_millis_opt(ms).single().map_or_else(String::new, |d| d.format("%b %d %H:%M").to_string().to_lowercase())
}

/// "just now", "12m ago", "5h ago", "3d ago"
fn ago(ms: i64, now: i64) -> String {
    let m = (now - ms).max(0) / 60_000;
    if m < 1 {
        "just now".into()
    } else if m < 60 {
        format!("{m}m ago")
    } else if m < 48 * 60 {
        format!("{}h ago", m / 60)
    } else {
        format!("{}d ago", m / (24 * 60))
    }
}

/// A braille trend: two values per cell, each a column 1–4 dots tall scaled
/// between the series' max and the lower of its min and half its max (so
/// small wobbles don't look like cliffs). Flagged values color their cell.
pub fn trend(v: &[f64], flagged: &[bool], color: Color) -> Vec<Span<'static>> {
    const LEFT: [u32; 4] = [0x40, 0x04, 0x02, 0x01];
    const RIGHT: [u32; 4] = [0x80, 0x20, 0x10, 0x08];
    let hi = v.iter().copied().fold(0.0, f64::max);
    let lo = v.iter().copied().fold(hi, f64::min).min(hi / 2.0);
    let level = |x: f64| if hi > lo { 1 + ((x - lo) / (hi - lo) * 3.0).round() as usize } else { 2 };
    v.chunks(2)
        .enumerate()
        .map(|(i, c)| {
            let mut bits = 0x2800;
            for (k, dots) in [LEFT, RIGHT].iter().enumerate() {
                if let Some(&x) = c.get(k) {
                    bits |= dots[..level(x).min(4)].iter().fold(0, |a, d| a | d);
                }
            }
            let hot = (0..c.len()).any(|k| flagged.get(i * 2 + k).copied().unwrap_or(false));
            sp(char::from_u32(bits).unwrap_or(' ').to_string(), if hot { AM } else { color })
        })
        .collect()
}

#[cfg(test)]
fn braille_trend(v: &[f64]) -> String {
    trend(v, &[], G).iter().map(|s| s.content.to_string()).collect()
}

fn sp<S: Into<String>>(s: S, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(c))
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Left-aligned in `n` columns, cut with "…" when longer.
fn pad(s: &str, n: usize) -> String {
    if clen(s) > n {
        let mut t: String = s.chars().take(n.saturating_sub(1)).collect();
        t.push('…');
        t
    } else {
        format!("{s}{}", " ".repeat(n - clen(s)))
    }
}

/// Right-aligned in `n` columns.
fn rpad(s: &str, n: usize) -> String {
    format!("{}{s}", " ".repeat(n.saturating_sub(clen(s))))
}

fn header(title: &str, right: &str) -> Vec<Line<'static>> {
    let t = format!(" {title}");
    let (head, rest) = t.split_at(t.char_indices().nth(10).map_or(t.len(), |(i, _)| i));
    let room = W.saturating_sub(clen(&t) + 1);
    vec![
        Line::from(vec![sp(head.to_string(), theme::ac()), sp(rest.to_string(), G), sp(rpad(right, room), D)]),
        Line::from(sp(format!(" {}", "─".repeat(W - 1)), D)),
    ]
}

/// " ── name ───…── right ──"
fn rule(name: &str, right: &str) -> Line<'static> {
    let left = format!(" ── {name} ");
    let right = if right.is_empty() { String::new() } else { format!(" {right} ──") };
    let fill = W.saturating_sub(clen(&left) + clen(&right));
    Line::from(vec![sp(format!("{left}{}", "─".repeat(fill)), D), sp(right, D)])
}

fn hints(keys: &[(&str, &str)]) -> Line<'static> {
    let mut v = vec![sp(" ", D)];
    for (i, (k, what)) in keys.iter().enumerate() {
        if i > 0 {
            v.push(sp(" · ", VD));
        }
        v.push(sp(k.to_string(), theme::ac()));
        v.push(sp(format!(" {what}"), D));
    }
    Line::from(v)
}

/// Word-wrap to `w` columns.
fn wrap(s: &str, w: usize) -> Vec<String> {
    let mut out = vec![String::new()];
    for word in s.split_whitespace() {
        let cur = out.last_mut().expect("never empty");
        if !cur.is_empty() && clen(cur) + 1 + clen(word) > w {
            out.push(word.to_string());
        } else {
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
    }
    out
}

/// What regressed in a run, briefly: "▲ +43% time · +30% cost".
fn flag_text(fl: &[&Flag]) -> String {
    if fl.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = fl
        .iter()
        .map(|f| format!("{} {}", delta(f.pct), if f.metric == Metric::Time { "time" } else { "cost" }))
        .collect();
    format!("▲ {}", parts.join(" · "))
}

fn outcome(r: &Run) -> (String, Color) {
    let mut bad = Vec::new();
    if r.tests == Some(false) {
        bad.push("tests fail".to_string());
    }
    if r.errors > 0 {
        bad.push(format!("{} error{}", r.errors, if r.errors == 1 { "" } else { "s" }));
    }
    if r.unfinished > 0 {
        bad.push(format!("{} unfinished", r.unfinished));
    }
    match (bad.is_empty(), r.tests) {
        (true, Some(true)) => ("✓ tests pass".into(), G),
        (true, _) => ("✓ ok".into(), G),
        (false, _) => (format!("✗ {}", bad.join(", ")), RED),
    }
}

/// The shortest id prefix (8+ characters) that tells every run apart.
pub fn short_ids(runs: &[Run]) -> HashMap<String, String> {
    let ids: Vec<&str> = runs.iter().map(|r| r.session.as_str()).collect();
    let mut n = 8;
    while n < 64 {
        let mut seen = std::collections::HashSet::new();
        if ids.iter().all(|s| seen.insert(s.chars().take(n).collect::<String>())) {
            break;
        }
        n += 2;
    }
    runs.iter().map(|r| (r.id(), r.session.chars().take(n).collect())).collect()
}

// ---- screens --------------------------------------------------------------------

/// Everything the profile screens draw.
pub struct State {
    pub workflows: Vec<Workflow>,
    pub sel: usize,
    pub detail: bool,
    /// runs the detail view covers (the most recent)
    pub last: usize,
    /// shown under the header (demo data, …)
    pub note: Option<String>,
    pub ids: HashMap<String, String>,
    pub now: i64,
    /// terminal rows, for scrolling a long list (0 = unknown, show all)
    pub rows: usize,
}

impl State {
    pub fn new(runs: &[Run], rules: &BTreeMap<String, WorkflowRule>, last: usize, now: i64) -> State {
        State {
            workflows: runs::group(runs, rules),
            sel: 0,
            detail: false,
            last: last.max(2),
            note: None,
            ids: short_ids(runs),
            now,
            rows: 0,
        }
    }

    /// Handle a key; true to quit.
    pub fn key(&mut self, code: KeyCode) -> bool {
        let n = self.workflows.len();
        match code {
            KeyCode::Char('q') => return true,
            KeyCode::Esc | KeyCode::Left | KeyCode::Backspace if self.detail => self.detail = false,
            KeyCode::Esc => return true,
            KeyCode::Enter | KeyCode::Right => self.detail = n > 0,
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => self.sel = (self.sel + 1).min(n.saturating_sub(1)),
            _ => {}
        }
        false
    }

    pub fn screen(&self) -> Vec<Line<'static>> {
        match self.workflows.get(self.sel) {
            Some(w) if self.detail => self.detail_screen(w),
            _ => self.list_screen(),
        }
    }

    fn window<'a>(&self, w: &'a Workflow) -> &'a [Run] {
        &w.runs[w.runs.len().saturating_sub(self.last)..]
    }

    fn note(&self, out: &mut Vec<Line<'static>>) {
        if let Some(n) = &self.note {
            for l in wrap(n, W - 2) {
                out.push(Line::from(sp(format!(" {l}"), AM)));
            }
            out.push(Line::from(""));
        }
    }

    fn list_screen(&self) -> Vec<Line<'static>> {
        let runs: usize = self.workflows.iter().map(|w| w.runs.len()).sum();
        let right = format!("{} workflow{} · {runs} runs", self.workflows.len(), if self.workflows.len() == 1 { "" } else { "s" });
        let mut out = header("agentflow // profile", &right);
        self.note(&mut out);
        if self.workflows.is_empty() {
            out.push(Line::from(sp(" no runs on record yet", G)));
            out.push(Line::from(""));
            out.push(hints(&[("q", "quit")]));
            return out;
        }
        out.push(Line::from(sp(
            format!(
                "   {} {}{}{}{}  {}  {}",
                pad("workflow", 17),
                pad("project", 9),
                rpad("runs", 4),
                rpad("median", 8),
                rpad("≈cost", 7),
                pad("trend", 6),
                "last"
            ),
            D,
        )));
        // scroll to keep the selection on screen
        let room = if self.rows > 8 { self.rows - 7 - out.len() } else { usize::MAX };
        let skip = (self.sel + 1).saturating_sub(room);
        for (i, w) in self.workflows.iter().enumerate().skip(skip).take(room) {
            let on = i == self.sel;
            let recent = &w.runs[w.runs.len().saturating_sub(12)..];
            let walls: Vec<f64> = recent.iter().map(|r| r.wall_ms as f64).collect();
            let off = w.runs.len() - recent.len();
            let hot: Vec<bool> = (0..recent.len())
                .map(|k| flags(&w.runs, off + k).iter().any(|f| f.who.is_none() && f.metric == Metric::Time))
                .collect();
            // the latest run's regression, briefly: what got worse, or which role
            let latest = flags(&w.runs, w.runs.len() - 1);
            let whole: Vec<&str> = latest
                .iter()
                .filter(|f| f.who.is_none())
                .map(|f| if f.metric == Metric::Time { "time" } else { "cost" })
                .collect();
            let flag = match (whole.is_empty(), latest.first()) {
                (false, _) => format!("▲ {}", whole.join("+")),
                (true, Some(f)) => format!("▲ {}", pad(f.who.as_deref().unwrap_or(""), 10).trim_end()),
                (true, None) => String::new(),
            };
            let med_ms = median(&w.runs.iter().map(|r| r.wall_ms as f64).collect::<Vec<_>>()) as i64;
            let med_cost = median(&w.runs.iter().map(|r| r.cost).collect::<Vec<_>>());
            let name_c = if on { B } else { G };
            let mut line = vec![
                sp(if on { " ▸ " } else { "   " }, theme::ac()),
                sp(pad(&w.name, 17) + " ", name_c),
                sp(pad(&w.project, 9), D),
                sp(rpad(&w.runs.len().to_string(), 4), G),
                sp(rpad(&human(med_ms), 8), G),
                sp(rpad(&fmt_cost(med_cost), 7), G),
                sp("  ", D),
            ];
            let tr = trend(&walls, &hot, if on { theme::ac() } else { D });
            let tw: usize = tr.len();
            line.extend(tr);
            line.push(sp(" ".repeat(6usize.saturating_sub(tw) + 2), D));
            line.push(sp(pad(&ago(w.runs.last().map_or(0, |r| r.started), self.now), 9), D));
            line.push(sp(flag, AM));
            out.push(Line::from(line));
        }
        out.push(Line::from(""));
        out.push(hints(&[("↑/↓", "select"), ("enter", "detail"), ("q", "quit")]));
        out
    }

    fn detail_screen(&self, w: &Workflow) -> Vec<Line<'static>> {
        let runs = self.window(w);
        let mut harnesses: Vec<&str> = w.runs.iter().map(|r| r.harness.as_str()).collect();
        harnesses.sort_unstable();
        harnesses.dedup();
        let right = [w.project.as_str(), &harnesses.join("+"), &format!("{} runs", w.runs.len())]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" · ");
        let mut out = header(&format!("agentflow // profile // {}", w.name), &right);
        self.note(&mut out);

        let off = w.runs.len() - runs.len();
        let all_flags: Vec<Vec<Flag>> = (0..runs.len()).map(|k| flags(&w.runs, off + k)).collect();
        let latest = all_flags.last().cloned().unwrap_or_default();
        let run_flag = |m: Metric| -> Vec<bool> {
            all_flags.iter().map(|f| f.iter().any(|f| f.who.is_none() && f.metric == m)).collect()
        };
        let last = runs.last().expect("a workflow has runs");
        // the run's trends
        for (label, metric, vals, med, now) in [
            (
                "wall ",
                Metric::Time,
                runs.iter().map(|r| r.wall_ms as f64).collect::<Vec<_>>(),
                human(median(&runs.iter().map(|r| r.wall_ms as f64).collect::<Vec<_>>()) as i64),
                human(last.wall_ms),
            ),
            (
                "≈cost",
                Metric::Cost,
                runs.iter().map(|r| r.cost).collect(),
                fmt_cost(median(&runs.iter().map(|r| r.cost).collect::<Vec<_>>())),
                fmt_cost(last.cost),
            ),
        ] {
            let mut line = vec![sp(format!("  {label}  "), D)];
            let tr = trend(&vals, &run_flag(metric), theme::ac());
            let tw = tr.len();
            line.extend(tr);
            line.push(sp(" ".repeat(11usize.saturating_sub(tw)), D));
            line.push(sp("median ", D));
            line.push(sp(pad(&med, 8), G));
            line.push(sp("last ", D));
            line.push(sp(now + " ", G));
            let mine: Vec<&Flag> = latest.iter().filter(|f| f.who.is_none() && f.metric == metric).collect();
            line.push(sp(flag_text(&mine), AM));
            out.push(Line::from(line));
        }
        let mut strip = vec![sp("  done   ", D)];
        for r in runs.iter().rev().take(20).rev() {
            strip.push(if r.ok() { sp("✓", G) } else { sp("✗", RED) });
        }
        let ok = runs.iter().filter(|r| r.ok()).count();
        strip.push(sp(format!("  {ok} of {} ok", runs.len()), D));
        out.push(Line::from(strip));

        // roles
        out.push(rule("roles", &format!("median of the last {}", runs.len())));
        out.push(Line::from(sp(
            format!(
                "  {} {} {}{}  {}  {}",
                pad("role", 15),
                pad("model", 8),
                rpad("time", 8),
                rpad("≈cost", 8),
                pad("critical", 10),
                "last run"
            ),
            D,
        )));
        for s in role_stats(runs) {
            let mine: Vec<&Flag> = latest.iter().filter(|f| f.who.as_deref() == Some(s.role.as_str())).collect();
            let c = if mine.is_empty() { G } else { AM };
            out.push(Line::from(vec![
                sp(format!("  {} ", pad(&s.role, 15)), c),
                sp(pad(&s.model, 8) + " ", D),
                sp(rpad(&human(s.median_ms), 8), c),
                sp(rpad(&fmt_cost(s.median_cost), 8), c),
                sp("  ", D),
                sp(crate::stats::braille_bar((s.critical * 100.0).round() as usize, 100, 5), theme::ac()),
                sp(rpad(&percent(s.critical), 5), D),
                sp("  ", D),
                sp(flag_text(&mine), AM),
            ]));
        }
        let orch: Vec<&Run> = runs.iter().filter(|r| !r.orch_model.is_empty()).collect();
        if !orch.is_empty() {
            let cost = median(&orch.iter().map(|r| r.orch_cost).collect::<Vec<_>>());
            out.push(Line::from(vec![
                sp(format!("  {} ", pad("orchestrator", 15)), D),
                sp(pad(&orch.last().map_or(String::new(), |r| r.orch_model.clone()), 8) + " ", D),
                sp(rpad("—", 8), D),
                sp(rpad(&fmt_cost(cost), 8), D),
            ]));
        }

        // insights
        let ins = insights(runs);
        if ins.is_empty() {
            out.push(rule("insights", ""));
            out.push(Line::from(sp(
                format!("  after {INSIGHT_RUNS} runs ({} so far)", runs.len()),
                D,
            )));
        } else {
            out.push(rule("insights", ""));
            for i in ins {
                for (k, l) in wrap(&i, W - 6).into_iter().enumerate() {
                    out.push(Line::from(vec![sp(if k == 0 { "  · " } else { "    " }, theme::ac()), sp(l, G)]));
                }
            }
        }

        // recent runs, newest first; `#` is what `compare --workflow` takes
        out.push(rule("recent runs", "agentflow compare 1 2 --workflow …"));
        out.push(Line::from(sp(
            format!(
                "  {} {} {} {}{}{}  {} {}",
                rpad("#", 2),
                pad("when", 12),
                pad("harness", 7),
                rpad("wall", 8),
                rpad("≈cost", 8),
                rpad("agents", 7),
                pad("outcome", 17),
                "id"
            ),
            D,
        )));
        let show = if self.rows > 0 { self.rows.saturating_sub(out.len() + 3).clamp(2, 6) } else { 6 };
        for (k, r) in w.runs.iter().rev().take(show).enumerate() {
            let i = w.runs.len() - 1 - k;
            let fl = flags(&w.runs, i);
            let (oc, occ) = outcome(r);
            out.push(Line::from(vec![
                sp(format!("  {} ", rpad(&(k + 1).to_string(), 2)), D),
                sp(pad(&when(r.started), 12) + " ", G),
                sp(pad(&r.harness, 7) + " ", D),
                sp(rpad(&human(r.wall_ms), 8), if fl.iter().any(|f| f.who.is_none() && f.metric == Metric::Time) { AM } else { G }),
                sp(rpad(&fmt_cost(r.cost), 8), if fl.iter().any(|f| f.who.is_none() && f.metric == Metric::Cost) { AM } else { G }),
                sp(rpad(&r.agents.len().to_string(), 7), G),
                sp("  ", D),
                sp(pad(&oc, 17) + " ", occ),
                sp(self.ids.get(&r.id()).cloned().unwrap_or_default(), D),
                sp(if fl.is_empty() { "" } else { "  ▲" }, AM),
            ]));
        }
        out.push(Line::from(""));
        out.push(hints(&[("esc", "back"), ("↑/↓", "workflow"), ("q", "quit")]));
        out
    }
}

// ---- compare --------------------------------------------------------------------

/// A run by `arg`: a number N is the Nth most recent (1 = latest), anything
/// else a session id prefix.
pub fn resolve<'a>(runs: &'a [Run], arg: &str) -> Result<&'a Run, String> {
    let a = arg.trim().to_lowercase();
    if let Ok(n) = a.parse::<usize>() {
        if a.len() <= 4 {
            return match n {
                1.. if n <= runs.len() => Ok(&runs[runs.len() - n]),
                _ => Err(format!("no run #{n} ({} on record)", runs.len())),
            };
        }
    }
    let hits: Vec<&Run> = runs
        .iter()
        .filter(|r| !a.is_empty() && (r.session.to_lowercase().starts_with(&a) || r.id().to_lowercase().starts_with(&a)))
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no run matches \"{arg}\"")),
        _ => Err(format!("\"{arg}\" matches {} runs; give more of the id", hits.len())),
    }
}

/// Two runs side by side, `b` against `a`.
pub fn compare(a: &Run, b: &Run, rules: &BTreeMap<String, WorkflowRule>, ids: &HashMap<String, String>) -> Vec<Line<'static>> {
    const LW: usize = 16;
    const CW: usize = 27;
    let mut out = header("agentflow // compare", "b against a");
    let id = |r: &Run| ids.get(&r.id()).cloned().unwrap_or_else(|| r.session.chars().take(8).collect());
    out.push(Line::from(vec![
        sp(format!("  {}", pad("", LW)), D),
        sp("a ", theme::ac()),
        sp(pad(&id(a), CW - 2), B),
        sp("b ", theme::ac()),
        sp(pad(&id(b), CW - 2), B),
    ]));
    // a long cell (a critical path) wraps onto more lines
    let row = |out: &mut Vec<Line<'static>>, label: &str, x: String, y: String, d: Option<(f64, f64)>| {
        let fit = |c: &str| if clen(c) <= CW - 2 { vec![c.to_string()] } else { wrap(c, CW - 2) };
        let (wx, wy) = (fit(&x), fit(&y));
        let more: Vec<Line<'static>> = (1..wx.len().max(wy.len()))
            .map(|k| {
                let cell = |w: &[String]| pad(w.get(k).map_or("", String::as_str), CW);
                Line::from(vec![sp(format!("  {}", pad("", LW)), D), sp(cell(&wx), G), sp(cell(&wy), G)])
            })
            .collect();
        let mut l = vec![sp(format!("  {}", pad(label, LW)), D), sp(pad(&wx[0], CW), G), sp(pad(&wy[0], CW), G)];
        if let Some((va, vb)) = d.filter(|(va, _)| *va > 0.0) {
            let pct = vb / va - 1.0;
            let c = if pct.abs() < 0.05 {
                D
            } else if pct > 0.0 {
                AM
            } else {
                theme::ac()
            };
            l.push(sp(rpad(&delta(pct), 6), c));
        }
        out.push(Line::from(l));
        out.extend(more);
    };
    let wf = |r: &Run| runs::workflow_of(r, rules).1;
    let proj = |r: &Run| std::path::Path::new(&r.project).file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
    let path = |r: &Run| {
        let mut p: Vec<String> = Vec::new();
        for c in &r.critical {
            if p.last() != Some(c) {
                p.push(c.clone());
            }
        }
        p.join(" → ")
    };
    row(&mut out, "harness", a.harness.clone(), b.harness.clone(), None);
    row(&mut out, "workflow", wf(a), wf(b), None);
    row(&mut out, "project", proj(a), proj(b), None);
    row(&mut out, "started", when(a.started), when(b.started), None);
    row(&mut out, "wall time", human(a.wall_ms), human(b.wall_ms), Some((a.wall_ms as f64, b.wall_ms as f64)));
    row(&mut out, "agent time", human(a.work_ms), human(b.work_ms), Some((a.work_ms as f64, b.work_ms as f64)));
    row(&mut out, "≈cost", fmt_cost(a.cost), fmt_cost(b.cost), Some((a.cost, b.cost)));
    row(&mut out, "tokens", fmt_tokens(a.tokens), fmt_tokens(b.tokens), Some((a.tokens as f64, b.tokens as f64)));
    row(&mut out, "agents", a.agents.len().to_string(), b.agents.len().to_string(), None);
    let conc = |r: &Run| format!("peak {} · avg {:.1}", r.peak, r.avg);
    row(&mut out, "concurrency", conc(a), conc(b), None);
    row(&mut out, "critical path", path(a), path(b), None);
    row(&mut out, "tool calls", a.tool_calls.to_string(), b.tool_calls.to_string(), Some((a.tool_calls as f64, b.tool_calls as f64)));
    let (oa, ca) = outcome(a);
    let (ob, cb) = outcome(b);
    out.push(Line::from(vec![sp(format!("  {}", pad("outcome", LW)), D), sp(pad(&oa, CW), ca), sp(pad(&ob, CW), cb)]));

    // per role, a's roles first
    out.push(rule("roles", "time · ≈cost · model"));
    let (ra, rb) = (by_role(a), by_role(b));
    let mut roles: Vec<String> = ra.iter().map(|(r, _)| r.clone()).collect();
    for (r, _) in &rb {
        if !roles.contains(r) {
            roles.push(r.clone());
        }
    }
    let cell = |x: Option<&RoleRun>| match x {
        Some(x) => format!("{} {} {}", rpad(&human(x.ms), 7), rpad(&fmt_cost(x.cost), 6), x.model),
        None => rpad("—", 7),
    };
    for role in roles {
        let x = ra.iter().find(|(r, _)| *r == role).map(|(_, v)| v);
        let y = rb.iter().find(|(r, _)| *r == role).map(|(_, v)| v);
        let d = match (x, y) {
            (Some(x), Some(y)) => Some((x.ms as f64, y.ms as f64)),
            _ => None,
        };
        row(&mut out, &role, cell(x), cell(y), d);
    }
    if !a.orch_model.is_empty() || !b.orch_model.is_empty() {
        let o = |r: &Run| {
            if r.orch_model.is_empty() {
                rpad("—", 7)
            } else {
                format!("{} {} {}", rpad("", 7), rpad(&fmt_cost(r.orch_cost), 6), r.orch_model)
            }
        };
        row(&mut out, "orchestrator", o(a), o(b), None);
    }
    out
}

// ---- entry points ----------------------------------------------------------------

/// What `profile` and `compare` read.
pub struct Opts<'a> {
    pub rules: &'a BTreeMap<String, WorkflowRule>,
    pub pricing: &'a Pricing,
    /// the project whose finished sessions to index
    pub project: PathBuf,
    pub harnesses: Vec<Harness>,
    /// show the seeded demo history instead
    pub demo: bool,
    /// `persistence`: record what indexing finds (off: show it, write nothing)
    pub persist: bool,
}

/// Runs to show (indexing the project first), and a note when it's demo data.
fn load(o: &Opts) -> (Vec<Run>, Option<String>) {
    let now = crate::adapters::now_ms();
    if o.demo {
        return (demo(now), Some("demo data (--demo)".into()));
    }
    let path = runs::path();
    let mut store = runs::Store::load(&path);
    runs::index(&mut store, &path, o.persist, &o.harnesses, &o.project, o.pricing);
    let runs: Vec<Run> = store.runs.into_iter().filter(|r| !r.agents.is_empty()).collect();
    if runs.is_empty() {
        let note = format!(
            "no runs on record yet, so this is demo data. agentflow records a run when a session you watch finishes, and `profile` indexes the finished sessions of {} (use --project for another).",
            o.project.display()
        );
        return (demo(now), Some(note));
    }
    (runs, None)
}

/// `agentflow profile`.
pub fn run(o: &Opts, workflow: Option<&str>, last: usize, print: bool) -> io::Result<()> {
    let (runs, note) = load(o);
    let mut st = State::new(&runs, o.rules, last, crate::adapters::now_ms());
    st.note = note;
    if let Some(name) = workflow {
        let Some(i) = st.workflows.iter().position(|w| w.name.eq_ignore_ascii_case(name)) else {
            let names: Vec<&str> = st.workflows.iter().map(|w| w.name.as_str()).collect();
            eprintln!("agentflow: no workflow \"{name}\" (have: {})", names.join(", "));
            std::process::exit(2);
        };
        st.sel = i;
        st.detail = true;
    }
    if print || !io::stdout().is_terminal() {
        emit(&st.screen());
        return Ok(());
    }
    let mut term = crate::enter()?;
    let res = (|| -> io::Result<()> {
        loop {
            st.rows = crate::term_size(&term).1 as usize;
            crate::paint(&mut term, st.screen(), W as i64 + 1)?;
            let Some(k) = crate::next_key(Duration::from_millis(500))? else {
                continue;
            };
            if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
                break;
            }
            if st.key(k.code) {
                break;
            }
        }
        Ok(())
    })();
    crate::leave(&mut term)?;
    res
}

/// `agentflow profile --import`: backfill run history from every stored
/// session of the chosen harnesses, any project. A progress line on stderr
/// (when it's a terminal), then a summary.
pub fn run_import(o: &Opts) -> io::Result<()> {
    if !o.persist {
        eprintln!("agentflow: persistence is off in the config, so there's no run history to import into");
        std::process::exit(2);
    }
    let path = runs::path();
    let mut store = runs::Store::load(&path);
    let roots = crate::discovery::Roots::here();
    let sessions = crate::discovery::all_sessions_at(&roots, &o.harnesses);
    let tty = io::stderr().is_terminal();
    let mut progress = |done: usize, total: usize| {
        if tty && total > 0 {
            eprint!("\r\x1b[2Kagentflow: importing {done}/{total} sessions…");
            if done == total {
                eprint!("\r\x1b[2K");
            }
        }
    };
    let res = runs::import(&mut store, &path, sessions, std::time::SystemTime::now(), o.pricing, o.rules, &mut progress);
    let got = match res {
        Ok(got) => got,
        Err(e) => {
            if tty {
                eprintln!();
            }
            eprintln!("agentflow: couldn't write {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    for line in import_summary(&got) {
        println!("{line}");
    }
    Ok(())
}

/// "imported 87 runs across 12 workflows", then what was skipped.
pub fn import_summary(got: &runs::Imported) -> Vec<String> {
    let n = |k: usize, one: &str, many: &str| format!("{k} {}", if k == 1 { one } else { many });
    let mut out = vec![format!(
        "imported {} across {}",
        n(got.runs, "run", "runs"),
        n(got.workflows, "workflow", "workflows")
    )];
    let skipped: Vec<String> = [
        (got.known, "already recorded"),
        (got.running, "still running"),
        (got.empty, "with no agent work"),
    ]
    .into_iter()
    .filter(|(k, _)| *k > 0)
    .map(|(k, why)| format!("{k} {why}"))
    .collect();
    if !skipped.is_empty() {
        out.push(format!("skipped {} of {}: {}", got.known + got.running + got.empty, n(got.found, "session", "sessions"), skipped.join(" · ")));
    }
    if got.runs > 0 {
        out.push("see them with `agentflow profile`".into());
    }
    out
}

/// `agentflow compare <a> <b>`: numbers count back from the latest run (of
/// `workflow`, when given).
pub fn run_compare(o: &Opts, a: &str, b: &str, workflow: Option<&str>) -> Result<(), String> {
    let (runs, note) = load(o);
    let ids = short_ids(&runs);
    let pool: Vec<Run> = match workflow {
        Some(name) => runs
            .iter()
            .filter(|r| runs::workflow_of(r, o.rules).1.eq_ignore_ascii_case(name))
            .cloned()
            .collect(),
        None => runs.clone(),
    };
    if pool.is_empty() {
        return Err(format!("no runs for workflow \"{}\"", workflow.unwrap_or("")));
    }
    let (ra, rb) = (resolve(&pool, a)?, resolve(&pool, b)?);
    let mut lines = compare(ra, rb, o.rules, &ids);
    if let Some(n) = note {
        lines.insert(2, Line::from(sp(format!(" {}", wrap(&n, W - 2).join(" ")), AM)));
    }
    emit(&lines);
    Ok(())
}

/// Print lines: truecolor on a terminal, plain text when piped.
fn emit(lines: &[Line<'static>]) {
    if io::stdout().is_terminal() {
        crate::print_ansi(lines);
        return;
    }
    let mut out = io::stdout().lock();
    for l in lines {
        let s: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
        let _ = writeln!(out, "{}", s.trim_end());
    }
}

// ---- demo history ------------------------------------------------------------------

/// Seeded history for `--demo` (and when nothing is on record): a Claude
/// Code review workflow run 12 times, its latest run regressed (the reviewer
/// ran 70% longer), plus a Codex workflow on the same project, so `compare`
/// has a cross-harness pair. Deterministic for a given `now`.
pub fn demo(now: i64) -> Vec<Run> {
    const DAY: i64 = 86_400_000;
    // seconds per run, oldest first; scout and researcher run in parallel,
    // then reviewer → fixer → test-runner
    let scout = [62, 70, 58, 66, 74, 61, 69, 58, 71, 60, 67, 65];
    let research = [55, 48, 63, 52, 50, 58, 47, 60, 54, 49, 57, 52];
    let review = [80, 85, 78, 90, 82, 88, 79, 84, 86, 81, 83, 141];
    let fix = [70, 64, 75, 68, 72, 66, 71, 69, 74, 67, 70, 73];
    let test = [35, 38, 33, 36, 40, 34, 37, 35, 39, 36, 38, 36];
    // $ per second of each model, roughly
    let rate = |m: &str| match m {
        "opus" => 0.012,
        "sonnet" => 0.004,
        "luna" => 0.003,
        "sol" => 0.006,
        _ => 0.0006,
    };
    let agent = |role: &str, model: &str, secs: i64, critical: bool, error: bool| AgentRun {
        role: role.into(),
        model: model.into(),
        ms: secs * 1000,
        cost: secs as f64 * rate(model),
        tokens: (secs * 1000 * if model == "haiku" { 9 } else { 3 }) as u64,
        tool_calls: (secs / 6) as u64,
        error,
        critical,
    };
    let id = |i: u64| {
        let x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x5bd1_e995;
        format!("{:08x}-{:04x}-4{:03x}-a{:03x}-{:012x}", x >> 32, (x >> 16) & 0xffff, x & 0xfff, (x >> 20) & 0xfff, x & 0xffff_ffff_ffff)
    };
    let finish = |mut r: Run| {
        r.work_ms = r.agents.iter().map(|a| a.ms).sum();
        r.avg = r.work_ms as f64 / r.wall_ms.max(1) as f64;
        r.tokens = r.agents.iter().map(|a| a.tokens).sum::<u64>() + r.orch_tokens;
        r.cost = r.agents.iter().map(|a| a.cost).sum::<f64>() + r.orch_cost;
        r.tool_calls = r.agents.iter().map(|a| a.tool_calls).sum();
        r.errors = r.agents.iter().filter(|a| a.error).count() as u32;
        r.critical = r.agents.iter().filter(|a| a.critical).map(|a| a.role.clone()).collect();
        r
    };
    let mut out = Vec::new();
    for i in 0..12 {
        let gate_scout = scout[i] >= research[i];
        let wall = scout[i].max(research[i]) + review[i] + fix[i] + test[i] + 12;
        out.push(finish(Run {
            v: runs::VERSION,
            harness: "claude".into(),
            session: id(i as u64 + 1),
            project: "/demo/shop-api".into(),
            workflow: Some("pr-review".into()),
            started: now - (11 - i as i64) * DAY - (2 + i as i64 % 3) * 3_600_000,
            wall_ms: wall * 1000,
            peak: 2,
            orch_model: "opus".into(),
            orch_cost: 0.28 + 0.01 * (i % 4) as f64,
            orch_tokens: 60_000,
            agents: vec![
                agent("scout", "haiku", scout[i], gate_scout, false),
                agent("researcher", "sonnet", research[i], !gate_scout, false),
                agent("reviewer", "opus", review[i], true, false),
                agent("fixer", "sonnet", fix[i], true, i == 5),
                agent("test-runner", "haiku", test[i], true, false),
            ],
            tests: Some(i != 3 && i != 8),
            ..Run::default()
        }));
    }
    // the same project in Codex, a few times; its latest run is the second
    // most recent overall (`compare --demo 1 2` crosses harnesses)
    let overview = [96, 88, 101, 92];
    let audit = [130, 118, 125, 121];
    for i in 0..4 {
        out.push(finish(Run {
            v: runs::VERSION,
            harness: "codex".into(),
            session: id(100 + i as u64),
            project: "/demo/shop-api".into(),
            started: now - (7 - 2 * i as i64) * DAY / 2 - 5 * 3_600_000,
            wall_ms: (overview[i].max(audit[i]) + 6) * 1000,
            peak: 2,
            agents: vec![
                agent("repo_overview", "sol", overview[i], false, false),
                agent("test_audit", "luna", audit[i], true, false),
            ],
            tests: Some(true),
            ..Run::default()
        }));
    }
    out.sort_by_key(|r| r.started);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect()
    }

    /// A run of roles with (seconds, $, model, critical).
    fn run(n: i64, wall_s: i64, cost: f64, roles: &[(&str, i64, f64, &str, bool)]) -> Run {
        Run {
            harness: "claude".into(),
            session: format!("{n:08}-run"),
            project: "/w/app".into(),
            started: n * 1000,
            wall_ms: wall_s * 1000,
            cost,
            avg: 1.5,
            agents: roles
                .iter()
                .map(|&(role, s, c, model, critical)| AgentRun {
                    role: role.into(),
                    model: model.into(),
                    ms: s * 1000,
                    cost: c,
                    critical,
                    ..AgentRun::default()
                })
                .collect(),
            ..Run::default()
        }
    }

    fn steady(n: usize) -> Vec<Run> {
        (0..n as i64)
            .map(|i| run(i, 100 + i % 3, 1.0, &[("scout", 40, 0.1, "haiku", true), ("fixer", 60, 0.9, "opus", true)]))
            .collect()
    }

    #[test]
    fn median_and_regression_threshold() {
        assert_eq!(median(&[3.0, 1.0, 2.0]), 2.0);
        assert_eq!(median(&[4.0, 1.0, 2.0, 3.0]), 2.5);
        assert_eq!(median(&[]), 0.0);
        let h = [100_000.0; 10];
        assert_eq!(regression(&h, 125_000.0, FLOOR_MS), None, "exactly 25% is not more than 25%");
        assert!((regression(&h, 126_000.0, FLOOR_MS).unwrap() - 0.26).abs() < 1e-9);
        assert_eq!(regression(&h[..2], 200_000.0, FLOOR_MS), None, "needs 3 previous runs");
        assert_eq!(regression(&[4_000.0; 5], 5_500.0, FLOOR_MS), None, "1.5s is noise");
        // only the previous 10 count: an old slow era doesn't hide a regression
        let mut h: Vec<f64> = vec![500_000.0; 20];
        h.extend([100_000.0; 10]);
        assert!(regression(&h, 140_000.0, FLOOR_MS).is_some());
    }

    #[test]
    fn regressions_flag_the_run_and_the_role() {
        let mut runs = steady(12);
        assert!(flags(&runs, 11).is_empty(), "steady: nothing flagged");
        // the fixer ran 70% longer, which made the run 42% slower and costlier
        runs[11] = run(11, 142, 1.6, &[("scout", 40, 0.1, "haiku", true), ("fixer", 102, 1.5, "opus", true)]);
        let f = flags(&runs, 11);
        let got: Vec<(Option<&str>, Metric)> = f.iter().map(|f| (f.who.as_deref(), f.metric)).collect();
        assert_eq!(
            got,
            [(None, Metric::Time), (None, Metric::Cost), (Some("fixer"), Metric::Time), (Some("fixer"), Metric::Cost)]
        );
        assert!((f[2].pct - 0.7).abs() < 1e-9);
        // a role new to the workflow has no baseline
        runs[11].agents.push(AgentRun { role: "new".into(), ms: 999_000, ..AgentRun::default() });
        assert!(flags(&runs, 11).iter().all(|f| f.who.as_deref() != Some("new")));
        // instances of a role in one run add up
        let r = run(0, 10, 0.0, &[("t", 3, 0.1, "haiku", false), ("t", 4, 0.2, "haiku", true)]);
        assert_eq!(by_role(&r), [("t".to_string(), RoleRun { ms: 7_000, cost: 0.30000000000000004, model: "haiku".into(), critical: true, error: false })]);
    }

    #[test]
    fn insights_need_five_runs_and_cross_thresholds() {
        // scout and researcher in parallel: scout gates 4 of 5 runs
        let mk = |n: usize, scout_gates: usize| -> Vec<Run> {
            (0..n as i64)
                .map(|i| {
                    let gate = (i as usize) < scout_gates;
                    run(
                        i,
                        200,
                        1.0,
                        &[
                            ("scout", 60, 0.03, "haiku", gate),
                            ("researcher", 55, 0.05, "sonnet", !gate),
                            ("reviewer", 70, 0.8, "opus", true),
                        ],
                    )
                })
                .collect()
        };
        assert!(insights(&mk(4, 4)).is_empty(), "under 5 runs: no insights");
        let ins = insights(&mk(5, 4));
        assert_eq!(ins.len(), 2, "{ins:?}");
        assert_eq!(ins[0], "scout is on the critical path in 80% of runs: speeding it up shortens most runs");
        assert_eq!(ins[1], "reviewer on opus costs 27× scout on haiku with similar duration");
        // 3 of 5 = 60% is still a pace-setter; 2 of 5 isn't
        assert!(insights(&mk(5, 3))[0].starts_with("scout is on the critical path in 60%"));
        assert!(insights(&mk(5, 2))[0].starts_with("researcher is on the critical path in 60%"));
        // durations too far apart are not "similar": no cost comparison
        let mut far = mk(5, 4);
        for r in &mut far {
            r.agents[2].ms = 200_000;
        }
        assert!(insights(&far).iter().all(|i| !i.contains("costs")), "{:?}", insights(&far));

        // failures, a mostly serial workflow, and the cap of 3
        let mut bad = mk(8, 8); // scout always gates: no pace-setter note
        for (i, r) in bad.iter_mut().enumerate() {
            r.tests = Some(i % 3 != 0);
            r.avg = 1.1;
            r.agents[2].error = i < 2;
        }
        let ins = insights(&bad);
        assert_eq!(
            ins,
            [
                "reviewer on opus costs 27× scout on haiku with similar duration",
                "reviewer ends in an error in 2 of 8 runs",
                "tests fail at the end of 3 of 8 runs",
            ],
            "capped at 3: the serial note is dropped"
        );
        bad[1].agents[2].error = false; // 1 of 8: under 25%
        let ins = insights(&bad);
        assert!(ins.iter().all(|i| !i.contains("ends in an error")), "{ins:?}");
        assert!(ins[2].starts_with("agents mostly run one at a time (avg 1.1 at once)"), "{ins:?}");
    }

    #[test]
    fn drift_and_dominant_role_insights() {
        let mut runs = steady(10);
        for r in &mut runs[5..] {
            r.wall_ms = 140_000;
        }
        let ins = insights(&runs);
        assert_eq!(ins[0], "runs are getting slower: median 2m 20s over the last 5, 1m 41s over the 5 before");
        // fixer is on every run's path and takes 60% of it
        assert!(insights(&steady(6)).iter().any(|i| i == "every run waits on fixer: it takes 59% of the wall time (median 1m)"));
    }

    #[test]
    fn braille_trend_scales_and_pairs() {
        assert_eq!(braille_trend(&[]), "");
        // a flat series is full columns; two values per cell; an odd tail
        assert_eq!(braille_trend(&[5.0, 5.0]), "⣿");
        assert_eq!(braille_trend(&[0.0, 10.0, 10.0]), "⣸⡇", "lowest = 1 dot, highest = full column");
        // small wobbles stay small: scaled from half the max, not the min
        assert_eq!(braille_trend(&[90.0, 100.0]), "⣾");
        let flagged = trend(&[1.0, 1.0, 1.0, 9.0], &[false, false, false, true], G);
        assert_eq!((flagged[0].style.fg, flagged[1].style.fg), (Some(G), Some(AM)));
    }

    #[test]
    fn import_summary_counts_runs_workflows_and_skips() {
        let got = runs::Imported { found: 120, runs: 87, workflows: 12, known: 20, running: 2, empty: 11 };
        assert_eq!(
            import_summary(&got),
            [
                "imported 87 runs across 12 workflows",
                "skipped 33 of 120 sessions: 20 already recorded · 2 still running · 11 with no agent work",
                "see them with `agentflow profile`",
            ]
        );
        let one = runs::Imported { found: 1, runs: 1, workflows: 1, ..runs::Imported::default() };
        assert_eq!(import_summary(&one), ["imported 1 run across 1 workflow", "see them with `agentflow profile`"]);
        let none = runs::Imported { found: 3, known: 3, ..runs::Imported::default() };
        assert_eq!(import_summary(&none), ["imported 0 runs across 0 workflows", "skipped 3 of 3 sessions: 3 already recorded"]);
    }

    #[test]
    fn resolve_by_number_or_id_prefix() {
        let runs = vec![run(1, 10, 0.0, &[]), run(2, 10, 0.0, &[]), run(3, 10, 0.0, &[])];
        assert_eq!(resolve(&runs, "1").unwrap().session, "00000003-run", "1 = latest");
        assert_eq!(resolve(&runs, "3").unwrap().session, "00000001-run");
        assert!(resolve(&runs, "4").is_err());
        assert!(resolve(&runs, "0").is_err());
        assert_eq!(resolve(&runs, "00000002").unwrap().started, 2_000);
        assert_eq!(resolve(&runs, "claude:00000002").unwrap().started, 2_000);
        assert!(resolve(&runs, "0000000").unwrap_err().contains("matches 3 runs"));
        assert!(resolve(&runs, "zz").unwrap_err().contains("no run matches"));
        let ids = short_ids(&runs);
        assert_eq!(ids["claude:00000001-run"], "00000001");
    }

    #[test]
    fn compare_lays_runs_side_by_side_across_harnesses() {
        let mut a = run(1, 252, 1.84, &[("scout", 48, 0.04, "haiku", true), ("reviewer", 112, 1.2, "opus", true)]);
        a.critical = vec!["scout".into(), "reviewer".into()];
        a.tests = Some(true);
        a.orch_model = "opus".into();
        a.orch_cost = 0.3;
        let mut b = run(2, 361, 0.92, &[("repo_overview", 96, 0.5, "sol", false), ("test_audit", 130, 0.42, "luna", true)]);
        b.harness = "codex".into();
        b.errors = 1;
        b.critical = vec!["test_audit".into()];
        let out = text(&compare(&a, &b, &BTreeMap::new(), &short_ids(&[a.clone(), b.clone()])));
        let line = |k: &str| out.lines().find(|l| l.trim_start().starts_with(k)).unwrap_or_else(|| panic!("{k} in\n{out}")).to_string();
        assert!(line("harness").contains("claude") && line("harness").contains("codex"));
        assert!(line("wall time").contains("4m 12s") && line("wall time").contains("6m 01s") && line("wall time").ends_with("+43%"), "{out}");
        assert!(line("≈cost").contains("$1.84") && line("≈cost").ends_with("-50%"));
        assert!(line("critical path").contains("scout → reviewer"));
        let mut long = a.clone();
        long.critical = ["scout", "researcher", "reviewer", "fixer", "test-runner"].map(String::from).to_vec();
        let out2 = text(&compare(&long, &b, &BTreeMap::new(), &HashMap::new()));
        let at = out2.lines().position(|l| l.trim_start().starts_with("critical path")).unwrap();
        let upto = out2.lines().position(|l| l.trim_start().starts_with("tool calls")).unwrap();
        let cell: Vec<&str> = out2.lines().skip(at).take(upto - at).collect();
        assert!(cell.len() > 1 && cell.join(" ").contains("test-runner"), "wraps:\n{out2}");
        assert!(out2.lines().all(|l| l.chars().count() <= W + 2), "{out2}");
        assert!(line("outcome").contains("✓ tests pass") && line("outcome").contains("✗ 1 error"));
        assert!(line("scout").contains("48s") && line("scout").contains("—"), "a role only in a");
        assert!(line("scout").starts_with(&format!("  {}{}", pad("scout", 16), rpad("48s", 7))), "times right-aligned: {}", line("scout"));
        assert!(line("test_audit").contains("2m 10s"), "a role only in b");
        assert!(line("orchestrator").contains("$0.30"));
        assert!(out.lines().all(|l| l.chars().count() <= W + 2), "{out}");
    }

    #[test]
    fn demo_history_has_one_regression_and_insights() {
        let runs = demo(1_800_000_000_000);
        let st = State::new(&runs, &BTreeMap::new(), DEFAULT_LAST, 1_800_000_000_000);
        let names: Vec<&str> = st.workflows.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, ["pr-review", "repo_overview+test_audit"]);
        let pr = &st.workflows[0];
        assert_eq!(pr.runs.len(), 12);
        let flagged: Vec<usize> = (0..12).filter(|&i| !flags(&pr.runs, i).is_empty()).collect();
        assert_eq!(flagged, [11], "only the latest run regressed");
        let f = flags(&pr.runs, 11);
        assert!(f.iter().any(|f| f.who.as_deref() == Some("reviewer") && f.metric == Metric::Time));
        let ins = insights(&pr.runs);
        assert!(ins[0].starts_with("scout is on the critical path in 83%"), "{ins:?}");
        assert!(ins[1].starts_with("reviewer on opus costs"), "{ins:?}");
        // the two latest runs cross harnesses
        assert_eq!((runs[runs.len() - 1].harness.as_str(), runs[runs.len() - 2].harness.as_str()), ("claude", "codex"));
    }

    #[test]
    fn screens_render_and_keys_navigate() {
        let now = 1_800_000_000_000;
        let runs = demo(now);
        let mut st = State::new(&runs, &BTreeMap::new(), DEFAULT_LAST, now);
        let list = text(&st.screen());
        assert!(list.contains("2 workflows · 16 runs"), "{list}");
        assert!(list.contains("▸ pr-review") && list.contains("▲"), "{list}");
        assert!(list.lines().all(|l| l.chars().count() <= W + 1), "{list}");

        assert!(!st.key(KeyCode::Enter));
        let detail = text(&st.screen());
        for want in ["agentflow // profile // pr-review", "reviewer", "orchestrator", "insights", "recent runs", "▲ +"] {
            assert!(detail.contains(want), "{want} in\n{detail}");
        }
        assert!(detail.lines().all(|l| l.chars().count() <= W + 1), "{detail}");
        st.key(KeyCode::Down);
        let codex = text(&st.screen());
        assert!(codex.contains("after 5 runs (4 so far)"), "{codex}");
        st.key(KeyCode::Down);
        assert_eq!(st.sel, 1, "clamps");
        assert!(!st.key(KeyCode::Esc));
        assert!(!st.detail);
        assert!(st.key(KeyCode::Esc), "esc on the list quits");
        assert!(st.key(KeyCode::Char('q')));

        let empty = State::new(&[], &BTreeMap::new(), 20, now);
        assert!(text(&empty.screen()).contains("no runs on record yet"));
    }
}
