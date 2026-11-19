//! Orbit view (`v`): the orchestrator at the center, subagents evenly spaced
//! on an elliptical ring around it. In a mixed view, one ring per harness.
//!
//! Why each element exists:
//!   * spokes + packets: dispatch and return between the orchestrator and each
//!     agent, so you can see who is waiting on whom
//!   * peer chords: real handoffs (one agent's result fed into another agent's
//!     task, or a teammate's direct message), so data flow is visible
//!   * validator gate: a configured checker role (`validators` in the config)
//!     whose verdict shows as pass/fail
//!   * braille trace under each agent: progress at a glance
//!   * "latest" line: the most recent dispatch/handoff/result, for a glance
//!     mid-run
//!
//! Draw order: ring → spokes → chords → packets → labels. The ring, spokes
//! and chords only fill blank cells; packets and labels draw over them.

use std::f64::consts::PI;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::model::{Agent, Handoff, Session};
use crate::render::{clip, current_tool, idle_label, running_label, View, SPIN};
use crate::stats;
use crate::theme::*;

/// Grid rows. Its width is the frame width.
pub const GRID_H: usize = 20;
/// Frame rows around the grid: header, top rule, bottom rule, latest, hints.
pub const CHROME: usize = 5;
/// Most agents on the ring; with more, the busiest (and the selected one) show.
pub const MAX_RING: usize = 8;
/// Each label block is 13 columns (x-6..=x+6) by 4 rows (face, name, status,
/// trace), anchored on the name row.
const HALF: i64 = 6;

// Timings (ms).
const DISPATCH_MS: i64 = 500;
const RETURN_MS: i64 = 500;
const HANDOFF_MS: i64 = 600;
/// a validator's verdict travels a little slower than a plain result
const VERDICT_MS: i64 = 600;
/// a chord appears this long before its packet leaves…
const CHORD_LEAD: i64 = 200;
/// …and fades out this long after it lands
const CHORD_FADE: i64 = 1_500;
/// orchestrator's happy face after a result lands
const HAPPY_MS: i64 = 900;
/// name flash on arrival
const FLASH_MS: i64 = 220;
const TRACE: usize = 6;

// ---- layout -----------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct Layout {
    pub w: usize,
    pub h: usize,
    pub cx: i64,
    pub cy: i64,
    pub rx: f64,
    pub ry: f64,
    /// each ring slot's name-row anchor, clockwise from the top
    pub pos: Vec<(i64, i64)>,
}

/// Lay out `n` agents on a `w`×`h` grid: evenly spaced on an ellipse starting
/// at the top, ~3:1 wide to make up for ~2:1 tall character cells (the
/// design's rx=24, ry=8 on 66×20). Takes the tallest ring whose label blocks
/// stay on the grid without touching each other or the orchestrator's;
/// `None` when nothing fits (the caller falls back to the tree).
pub fn layout(n: usize, w: usize, h: usize) -> Option<Layout> {
    if n > MAX_RING || w < (2 * HALF + 1) as usize || h < 4 {
        return None;
    }
    let (cx, cy) = ((w / 2) as i64, (h / 2) as i64);
    let ry_max = cy - 2;
    let rx = (3 * ry_max).min(cx - HALF - 1) as f64;
    if n == 0 {
        let pos = Vec::new();
        return Some(Layout { w, h, cx, cy, rx: rx.max(1.0), ry: ry_max.max(1) as f64, pos });
    }
    (2..=ry_max).rev().find_map(|ry| {
        let ry = ry as f64;
        let pos: Vec<(i64, i64)> = (0..n)
            .map(|k| {
                let a = -PI / 2.0 + 2.0 * PI * k as f64 / n as f64;
                ((cx as f64 + rx * a.cos()).round() as i64, (cy as f64 + ry * a.sin()).round() as i64)
            })
            .collect();
        fits(&pos, cx, cy, w as i64, h as i64).then_some(Layout { w, h, cx, cy, rx, ry, pos })
    })
}

/// Every block on the grid, and no two blocks (orchestrator included) within
/// a column or row of each other.
fn fits(pos: &[(i64, i64)], cx: i64, cy: i64, w: i64, h: i64) -> bool {
    let on_grid = |&(x, y): &(i64, i64)| x >= HALF && x + HALF < w && y > 0 && y + 2 < h;
    let clash = |a: (i64, i64), b: (i64, i64)| (a.0 - b.0).abs() <= 2 * HALF + 1 && (a.1 - b.1).abs() <= 3;
    let all: Vec<(i64, i64)> = std::iter::once((cx, cy)).chain(pos.iter().copied()).collect();
    all.iter().all(on_grid)
        && all.iter().enumerate().all(|(i, a)| all[i + 1..].iter().all(|b| !clash(*a, *b)))
}

/// Grid size for the frame: its width, `GRID_H` tall, both shrunk to a
/// known terminal size (`(0, 0)` = unknown, e.g. debug frames and tests).
pub fn grid_size(w: i64, term: (u16, u16)) -> (usize, usize) {
    let gw = if term.0 == 0 { w as usize } else { (w as usize).min(term.0 as usize) };
    let gh = if term.1 == 0 { GRID_H } else { GRID_H.min((term.1 as usize).saturating_sub(CHROME)) };
    (gw, gh)
}

/// Which agents sit on the ring (indexes into `sess.agents`, in spawn order).
/// Agents not dispatched yet are included and idle. Past `MAX_RING`, the ones
/// still working or newest win, and the selected agent always gets a seat.
pub fn ring_agents(sess: &Session, t: i64, selected: Option<usize>) -> Vec<usize> {
    let n = sess.agents.len();
    if n <= MAX_RING {
        return (0..n).collect();
    }
    let a = &sess.agents;
    let settled = |i: usize| a[i].finish.is_some_and(|f| t >= f + 1700);
    let mut idx: Vec<usize> = (0..n).collect();
    idx.sort_by_key(|&i| (a[i].spawn > t, settled(i), std::cmp::Reverse(i)));
    idx.truncate(MAX_RING);
    if let Some(sel) = selected.filter(|s| *s < n && !idx.contains(s)) {
        idx[MAX_RING - 1] = sel;
    }
    idx.sort_unstable();
    idx
}

// ---- handoffs ---------------------------------------------------------------

/// Results fed straight into a later agent's task: agent A finished before B
/// was dispatched, and B's prompt quotes A's result (or one of A's ids).
/// The handoff happens at B's spawn, in place of B's own dispatch packet.
pub fn detect_handoffs(agents: &[Agent]) -> Vec<Handoff> {
    let mut out = Vec::new();
    for (j, b) in agents.iter().enumerate() {
        if b.prompt.trim().is_empty() {
            continue;
        }
        for (i, a) in agents.iter().enumerate() {
            let before = a.finish.is_some_and(|f| f <= b.spawn);
            if i == j || !before || !fed_into(a, &b.prompt) {
                continue;
            }
            out.push(Handoff { from: i, to: j, time: b.spawn, what: handoff_word(a).into(), size: a.size.clone() });
        }
    }
    out.sort_by_key(|h| h.time);
    out
}

/// A's output shows up in `prompt`: one of its ids, its whole result, or
/// enough of its distinctive lines (two of them, or one long one).
fn fed_into(a: &Agent, prompt: &str) -> bool {
    if a.ids.iter().any(|id| id.len() >= 6 && prompt.contains(id.as_str())) {
        return true;
    }
    let Some(result) = a.result.as_deref() else {
        return false;
    };
    let p = squash(prompt);
    let whole = squash(result);
    if whole.chars().count() >= 24 && p.contains(&whole) {
        return true;
    }
    let lines: Vec<String> = result
        .lines()
        .map(|l| squash(l.trim_start_matches(|c: char| "-*•>#0123456789. ".contains(c))))
        .filter(|l| l.chars().count() >= 24)
        .collect();
    let hits: Vec<&String> = lines.iter().filter(|l| p.contains(l.as_str())).collect();
    hits.len() >= 2 || hits.iter().any(|l| l.chars().count() >= 48)
}

/// Collapse whitespace runs (quoting often re-wraps or re-indents).
fn squash(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What a handoff carries, for the latest line: a reviewer's findings, a
/// builder's patch, anything else's result.
fn handoff_word(a: &Agent) -> &'static str {
    if a.name.contains("review") {
        "findings"
    } else if a.uses.iter().any(|u| u.edit) {
        "patch"
    } else {
        "result"
    }
}

// ---- validators -------------------------------------------------------------

/// This role is a configured validator (case-insensitive).
pub fn is_validator(name: &str, validators: &[String]) -> bool {
    validators.iter().any(|v| v.trim().eq_ignore_ascii_case(name))
}

/// A finished validator's verdict: pass (`true`) or fail (`false`), or `None`
/// when it can't be told. In order: an error; an explicit "verdict: …" line in
/// its result (a reviewer's call); the exit status of its last test command
/// (a test runner's); the wording of its result's first or last line.
pub fn verdict(a: &Agent) -> Option<bool> {
    let end = a.finish?;
    if a.error {
        return Some(false);
    }
    let text = a.result.as_deref().unwrap_or("");
    const KEYS: [&str; 6] = ["verdict", "result", "status", "outcome", "conclusion", "overall"];
    for line in text.lines() {
        let l = line.trim().trim_start_matches(['#', '*', '-', '>', ' ']).to_lowercase();
        let labeled = KEYS
            .iter()
            .any(|k| l.starts_with(k) && l[k.len()..].trim_start_matches(['*', '_', ' ']).starts_with(':'));
        if let Some(v) = labeled.then(|| classify(&l)).flatten() {
            return Some(v);
        }
    }
    if let Some(r) = a.tests.iter().rfind(|r| r.time <= end) {
        return Some(r.passed);
    }
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    lines.first().and_then(|l| classify(l)).or_else(|| lines.last().and_then(|l| classify(l)))
}

/// Pass/fail wording in one line. Failure words win ("not approved"), except
/// where they're negated ("0 failed", "no blocking issues").
fn classify(line: &str) -> Option<bool> {
    let mut l = line.to_lowercase();
    const NEGATED: [&str; 11] = [
        "0 failed", "0 failures", "0 errors", "zero failures", "no failures", "no failing",
        "no blocking", "no issues", "no problems", "nothing blocking", "without failures",
    ];
    for p in NEGATED {
        l = replace_word(&l, p, " pass ");
    }
    const FAIL: [&str; 13] = [
        "fail", "reject", "changes requested", "request changes", "requesting changes",
        "needs changes", "needs work", "not approved", "do not merge", "don't merge",
        "blocking", "✗", "❌",
    ];
    const PASS: [&str; 10] =
        ["pass", "approve", "lgtm", "looks good", "ship it", "all good", "green", "✓", "✅", "ok to merge"];
    if FAIL.iter().any(|k| l.contains(k)) {
        Some(false)
    } else if PASS.iter().any(|k| l.contains(k)) {
        Some(true)
    } else {
        None
    }
}

/// Replace `pat` where it doesn't continue a number ("10 failed" stays).
fn replace_word(s: &str, pat: &str, with: &str) -> String {
    let mut out = String::new();
    let mut rest = s;
    while let Some(i) = rest.find(pat) {
        let digit_before = rest[..i].chars().next_back().is_some_and(|c| c.is_ascii_digit())
            || (i == 0 && out.chars().next_back().is_some_and(|c| c.is_ascii_digit()));
        out.push_str(&rest[..i]);
        out.push_str(if digit_before { pat } else { with });
        rest = &rest[i + pat.len()..];
    }
    out.push_str(rest);
    out
}

// ---- packets ----------------------------------------------------------------

/// One packet's trip, and the latest-line entry it makes.
#[derive(Clone, Debug, PartialEq)]
pub struct Packet {
    /// agent index; `None` = the orchestrator
    pub from: Option<usize>,
    pub to: Option<usize>,
    pub start: i64,
    pub dur: i64,
    pub label: String,
    pub detail: String,
}

/// The agent's dispatch arrives by a handoff chord instead of its spoke.
fn handed_off(sess: &Session, i: usize) -> bool {
    sess.handoffs.iter().any(|h| h.to == i && h.time == sess.agents[i].spawn)
}

/// When the agent gets its task (its dispatch or handoff packet lands).
fn arrival(sess: &Session, i: usize) -> i64 {
    sess.agents[i].spawn + if handed_off(sess, i) { HANDOFF_MS } else { DISPATCH_MS }
}

/// Every packet in the session, in start order: dispatches (orchestrator →
/// agent), handoffs (agent → agent), results (agent → orchestrator).
pub fn packets(sess: &Session, validators: &[String]) -> Vec<Packet> {
    let mut out = Vec::new();
    for (i, a) in sess.agents.iter().enumerate() {
        if !handed_off(sess, i) {
            out.push(Packet {
                from: None,
                to: Some(i),
                start: a.spawn,
                dur: DISPATCH_MS,
                label: format!("{} \u{2192} {}", sess.root_name(), a.name),
                detail: "dispatch".into(),
            });
        }
        let Some(f) = a.finish else { continue };
        let validator = is_validator(&a.name, validators);
        let detail = match (a.error, validator.then(|| verdict(a)).flatten()) {
            (true, _) => "error".to_string(),
            (_, Some(true)) => "\u{2713} validated".into(),
            (_, Some(false)) => "\u{2717} failed".into(),
            _ => format!("result {}", a.size),
        };
        out.push(Packet {
            from: Some(i),
            to: None,
            start: f,
            dur: if validator { VERDICT_MS } else { RETURN_MS },
            label: format!("{} \u{2192} {}", a.name, sess.root_name()),
            detail,
        });
    }
    for h in &sess.handoffs {
        out.push(Packet {
            from: Some(h.from),
            to: Some(h.to),
            start: h.time,
            dur: HANDOFF_MS,
            label: format!("{} \u{2192} {}", sess.agents[h.from].name, sess.agents[h.to].name),
            detail: format!("{} {}", h.what, h.size),
        });
    }
    out.sort_by_key(|p| p.start);
    out
}

// ---- drawing ----------------------------------------------------------------

struct Grid {
    w: i64,
    h: i64,
    cells: Vec<Vec<(char, Color)>>,
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        Grid { w: w as i64, h: h as i64, cells: vec![vec![(' ', G); w]; h] }
    }
    fn on(&self, x: i64, y: i64) -> bool {
        (0..self.w).contains(&x) && (0..self.h).contains(&y)
    }
    fn put(&mut self, x: i64, y: i64, s: &str, c: Color) {
        for (k, ch) in s.chars().enumerate() {
            let xx = x + k as i64;
            if self.on(xx, y) {
                self.cells[y as usize][xx as usize] = (ch, c);
            }
        }
    }
    /// centered on `x`
    fn putc(&mut self, x: i64, y: i64, s: &str, c: Color) {
        self.put(x - s.chars().count() as i64 / 2, y, s, c);
    }
    /// only into a blank cell
    fn soft(&mut self, x: i64, y: i64, ch: char, c: Color) {
        if self.on(x, y) && self.cells[y as usize][x as usize].0 == ' ' {
            self.cells[y as usize][x as usize] = (ch, c);
        }
    }
    fn lines(self) -> Vec<Line<'static>> {
        self.cells
            .into_iter()
            .map(|row| {
                let mut spans: Vec<Span<'static>> = Vec::new();
                let mut cur = String::new();
                let mut col = G;
                for (ch, c) in row {
                    // spaces take whatever color is running: fewer spans
                    if c != col && ch != ' ' && !cur.is_empty() {
                        spans.push(Span::styled(std::mem::take(&mut cur), Style::default().fg(col)));
                    }
                    if ch != ' ' {
                        col = c;
                    }
                    cur.push(ch);
                }
                let trimmed = cur.trim_end().to_string();
                if !trimmed.is_empty() {
                    spans.push(Span::styled(trimmed, Style::default().fg(col)));
                }
                Line::from(spans)
            })
            .collect()
    }
}

fn lerp(f: (i64, i64), to: (i64, i64), u: f64) -> (i64, i64) {
    (
        (f.0 as f64 + (to.0 - f.0) as f64 * u).round() as i64,
        (f.1 as f64 + (to.1 - f.1) as f64 * u).round() as i64,
    )
}

/// `u` from `a` to `b` inclusive in steps of `step`.
fn steps(a: f64, b: f64, step: f64) -> impl Iterator<Item = f64> {
    let n = ((b - a) / step + 1e-9).floor() as usize;
    (0..=n).map(move |k| a + k as f64 * step)
}

/// The grid lines and the latest line for the orbit, or `None` when the ring
/// doesn't fit the terminal.
pub fn panel(sess: &Session, t: i64, view: &View, w: i64) -> Option<(Vec<Line<'static>>, Line<'static>)> {
    let (gw, gh) = grid_size(w, view.term);
    let (grid, latest) = draw(sess, t, view, gw, gh)?;
    Some((grid, latest_line(None, latest)))
}

/// Height of each ring when a mixed view stacks them.
const STACK_H: usize = 14;

/// The mixed view's orbit: one ring per harness, its name dim above it. Side
/// by side when every ring fits half the width, else stacked when the
/// terminal is tall enough, else `None` (the caller shows the tree). Returns
/// the lines between the rules and the latest line (the newest across rings,
/// tagged with its harness).
pub fn mixed_panel(sess: &Session, t: i64, view: &View, w: i64) -> Option<(Vec<Line<'static>>, Line<'static>)> {
    let n = sess.groups.len().max(1);
    let cols = if view.term.0 == 0 { w as usize } else { (w as usize).min(view.term.0 as usize) };
    let rows = if view.term.1 == 0 { usize::MAX } else { (view.term.1 as usize).saturating_sub(CHROME) };
    let views: Vec<View> = sess.groups.iter().map(|g| View { selected: g.local(view.selected), ..*view }).collect();
    let rings = |gw: usize, gh: usize| -> Option<Vec<(Vec<Line<'static>>, Option<Latest>)>> {
        sess.groups.iter().zip(&views).map(|(g, v)| draw(&g.sess, t, v, gw, gh)).collect()
    };
    let latest = |drawn: &[(Vec<Line<'static>>, Option<Latest>)]| {
        let newest = drawn
            .iter()
            .zip(&sess.groups)
            .filter_map(|((_, l), g)| l.clone().map(|l| (g.harness.as_str(), l)))
            .max_by_key(|(_, l)| l.0);
        match newest {
            Some((tag, l)) => latest_line(Some(tag), Some(l)),
            None => Line::from(""),
        }
    };
    // a harness name centered over its ring, padded to the ring's width
    let label = |name: &str, width: usize| -> Span<'static> {
        let name = clip(name, width);
        let len = name.chars().count();
        let left = (width - len) / 2;
        Span::styled(format!("{}{name}{}", " ".repeat(left), " ".repeat(width - len - left)), Style::default().fg(D))
    };

    // side by side
    let cw = cols.saturating_sub(n - 1) / n;
    let gh = GRID_H.min(rows.saturating_sub(1));
    if let Some(drawn) = rings(cw, gh) {
        let mut out = vec![Line::from(
            sess.groups
                .iter()
                .enumerate()
                .flat_map(|(k, g)| {
                    let gap = (k > 0).then(|| Span::raw(" "));
                    gap.into_iter().chain([label(&g.harness, cw)])
                })
                .collect::<Vec<_>>(),
        )];
        for r in 0..gh {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (k, (grid, _)) in drawn.iter().enumerate() {
                if k > 0 {
                    spans.push(Span::raw(" "));
                }
                let line = grid.get(r).cloned().unwrap_or_default();
                let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
                spans.extend(line.spans);
                if k + 1 < drawn.len() {
                    spans.push(Span::raw(" ".repeat(cw.saturating_sub(used))));
                }
            }
            out.push(Line::from(spans));
        }
        return Some((out, latest(&drawn)));
    }

    // stacked
    let sh = STACK_H.min(rows.saturating_sub(n) / n);
    let drawn = rings(cols, sh)?;
    let mut out = Vec::new();
    for ((grid, _), g) in drawn.iter().zip(&sess.groups) {
        out.push(Line::from(label(&g.harness, cols)));
        out.extend(grid.iter().cloned());
    }
    Some((out, latest(&drawn)))
}

/// The newest packet (or the all-done line): (time, label, detail).
type Latest = (i64, String, String);

/// " latest  [harness]  label  detail"
fn latest_line(tag: Option<&str>, latest: Option<Latest>) -> Line<'static> {
    let Some((_, label, detail)) = latest else {
        return Line::from("");
    };
    let dc = if detail.contains('✓') {
        ac()
    } else if detail.contains('✗') || detail == "error" {
        RED
    } else {
        G
    };
    let mut l = vec![Span::styled(" latest  ", Style::default().fg(D))];
    if let Some(tag) = tag {
        l.push(Span::styled(format!("{tag:<8}"), Style::default().fg(D)));
    }
    l.push(Span::styled(format!("{label:<30}"), Style::default().fg(B)));
    l.push(Span::styled(detail, Style::default().fg(dc)));
    Line::from(l)
}

/// Draw one ring on a `gw`×`gh` grid: the grid lines and what's latest, or
/// `None` when the ring doesn't fit.
fn draw(sess: &Session, t: i64, view: &View, gw: usize, gh: usize) -> Option<(Vec<Line<'static>>, Option<Latest>)> {
    let ring = ring_agents(sess, t, view.selected);
    let lay = layout(ring.len(), gw, gh)?;
    let (cx, cy) = (lay.cx, lay.cy);
    let center = (cx, cy);
    let slot = |i: usize| ring.iter().position(|&k| k == i).map(|s| lay.pos[s]);
    let pt = |who: Option<usize>| match who {
        None => Some(center),
        Some(i) => slot(i),
    };
    // spokes cover only part of the line so they never run into the labels
    let span = |p: (i64, i64)| (if p.1 > cy { 0.45 } else { 0.25 }, 0.62);
    let packets = packets(sess, view.validators);
    let mut g = Grid::new(lay.w, lay.h);

    // 1. ring (very dim) + one slow satellite dot circling it
    for k in 0..160 {
        let r = k as f64 / 160.0 * 2.0 * PI;
        let p = ((cx as f64 + lay.rx * r.cos()).round() as i64, (cy as f64 + lay.ry * r.sin()).round() as i64);
        g.soft(p.0, p.1, '·', RING);
    }
    if !view.calm {
        let sa = t as f64 * 0.0006;
        let (sx, sy) = ((cx as f64 + lay.rx * sa.cos()).round() as i64, (cy as f64 + lay.ry * sa.sin()).round() as i64);
        if g.on(sx, sy) && g.cells[sy as usize][sx as usize] == ('·', RING) {
            g.cells[sy as usize][sx as usize] = ('•', D);
        }
    }

    // 2. dotted spokes
    for &p in &lay.pos {
        let (us, ue) = span(p);
        for u in steps(us, ue, 0.02) {
            let (x, y) = lerp(center, p, u);
            g.soft(x, y, '·', DIM);
        }
    }

    // 3. chords for handoffs: lit while the packet flies, then fading
    for h in &sess.handoffs {
        let (Some(f), Some(to)) = (slot(h.from), slot(h.to)) else { continue };
        if t < h.time - CHORD_LEAD || t >= h.time + HANDOFF_MS + CHORD_FADE {
            continue;
        }
        let c = if t < h.time + HANDOFF_MS { D } else { DIM };
        for u in steps(0.25, 0.75, 0.025) {
            let (x, y) = lerp(f, to, u);
            g.soft(x, y, '·', c);
        }
    }

    // 4. packets: head ● with a • · trail
    for p in packets.iter().filter(|p| t >= p.start && t < p.start + p.dur) {
        let (Some(f), Some(to)) = (pt(p.from), pt(p.to)) else { continue };
        let (s, e) = match (p.from, p.to) {
            (None, Some(_)) => span(to),
            (Some(_), None) => {
                let (us, ue) = span(f);
                (1.0 - ue, 1.0 - us)
            }
            _ => (0.25, 0.75),
        };
        let q = (t - p.start) as f64 / p.dur as f64;
        for (qq, ch) in [(q - 0.16, "·"), (q - 0.08, "•"), (q, "●")] {
            if qq >= 0.0 {
                let (x, y) = lerp(f, to, s + (e - s) * qq);
                g.put(x, y, ch, ac());
            }
        }
    }

    // 5. orchestrator: happy for a moment whenever a result lands
    let done = sess.all != i64::MAX && t >= sess.all;
    let landed = packets.iter().any(|p| p.to.is_none() && t >= p.start + p.dur && t < p.start + p.dur + HAPPY_MS);
    let (face, fc) = if done || landed {
        ("(^‿^)", ac())
    } else if t.rem_euclid(4100) < 130 {
        ("(-‿-)", G)
    } else {
        ("(•‿•)", G)
    };
    let (status, sc) = if done {
        ("all done ✓".to_string(), ac())
    } else if !sess.live && t < 600 {
        ("booting".into(), D)
    } else if sess.agents.is_empty() {
        ("waiting".into(), D)
    } else {
        (crate::render::root_working(sess, t), D)
    };
    g.putc(cx, cy - 1, "◆", ac());
    g.putc(cx, cy, face, fc);
    g.putc(cx, cy + 1, sess.root_name(), B);
    g.putc(cx, cy + 2, &status, sc);

    // 6. agents
    for (s, &i) in ring.iter().enumerate() {
        let (x, y) = lay.pos[s];
        let flash = packets.iter().any(|p| p.to == Some(i) && t >= p.start + p.dur && t < p.start + p.dur + FLASH_MS);
        let l = label(sess, i, t, view, flash);
        g.putc(x, y - 1, &l.face, l.face_c);
        let name = clip(&sess.agents[i].name, 12);
        g.putc(x, y, &name, l.name_c);
        if view.selected == Some(i) {
            g.put(x - name.chars().count() as i64 / 2 - 2, y, "›", ac());
        }
        g.putc(x, y + 1, &l.status, l.status_c);
        g.putc(x, y + 2, &l.trace, l.trace_c);
        if l.dozing && !view.calm {
            // z's drift up and right out of the sleepy face
            let k = (t + sess.agents[i].bob * 211).rem_euclid(3000) / 1000;
            let (zc, dy) = [(DIM, 0), (D, 1), (G, 2)][k as usize];
            g.soft(x + 3 + dy, y - 1 - dy, 'z', zc);
        }
    }

    // latest: the most recent packet (or the all-done line)
    let mut last = packets.iter().rfind(|p| p.start <= t).map(|p| (p.start, p.label.clone(), p.detail.clone()));
    if done {
        last = Some((sess.all, sess.root_name().into(), "all agents done ✓".into()));
    }
    Some((g.lines(), last))
}

/// One agent's block: face, name color, status line, braille trace.
struct Label {
    face: String,
    face_c: Color,
    name_c: Color,
    status: String,
    status_c: Color,
    trace: String,
    trace_c: Color,
    dozing: bool,
}

fn label(sess: &Session, i: usize, t: i64, view: &View, flash: bool) -> Label {
    let a = &sess.agents[i];
    let validator = is_validator(&a.name, view.validators);
    let arrive = arrival(sess, i);
    let done = a.finish.is_some_and(|f| t >= f);
    let spin = SPIN[((t / 80) + i as i64).rem_euclid(10) as usize];
    let trace = |n: usize| -> String { (0..TRACE).map(|k| if k < n { '⣿' } else { '⣀' }).collect() };
    let name_c = |c: Color| if flash { light() } else { c };

    // idle: not dispatched yet (or its dispatch is still in flight)
    if t < arrive && !done {
        return Label {
            face: "(-‿-)".into(),
            face_c: DIM,
            name_c: name_c(D),
            status: if validator { "validator" } else { "idle" }.into(),
            status_c: DIM,
            trace: trace(0),
            trace_c: RING,
            dozing: false,
        };
    }
    if a.error {
        return Label {
            face: "(×_×)".into(),
            face_c: RED,
            name_c: name_c(B),
            status: "error".into(),
            status_c: RED,
            trace: trace(TRACE),
            trace_c: RED,
            dozing: false,
        };
    }
    if done {
        let (status, status_c, face_c, face) = match validator.then(|| verdict(a)) {
            Some(Some(true)) => ("✓ pass", ac(), D, "(^‿^)"),
            Some(Some(false)) => ("✗ fail", RED, RED, "(•_•)"),
            Some(None) => ("no verdict", D, D, "(^‿^)"),
            None => ("done ✓", D, D, "(^‿^)"),
        };
        return Label {
            face: face.into(),
            face_c,
            name_c: name_c(D),
            status: status.into(),
            status_c,
            trace: trace(TRACE),
            trace_c: D,
            dozing: false,
        };
    }

    // working
    let stuck = stats::stuck_idle(a, t, view.stuck_ms);
    let running = stats::running_long(a, t, view.stuck_ms);
    let wait = running.is_none() && a.wait.is_some_and(|(ws, we)| t >= ws && t < we);
    let pr = match a.finish {
        // scripted (demo): the finish is known
        Some(f) if !sess.live => (t - arrive) as f64 / (f - arrive).max(1) as f64,
        // live: an asymptotic crawl that never quite fills
        _ => 0.85 * (1.0 - (-((t - arrive).max(0) as f64) / 9000.0).exp()),
    };
    let cells = ((pr.clamp(0.0, 1.0) * TRACE as f64).round() as usize).min(TRACE);
    let (face, face_c) = if stuck.is_some() {
        ("(-_-)", AM)
    } else if wait {
        ("(·_·)", AM)
    } else if validator {
        ("(•_•)", ac())
    } else if (t / 450) % 2 != 0 {
        ("(•_• )", ac())
    } else {
        ("( •_•)", ac())
    };
    let (status, status_c) = if let Some(idle) = stuck {
        (idle_label(idle), AM)
    } else if let Some(ms) = running {
        // the slow spinner rides along when it fits the 13-wide label
        let label = running_label(ms);
        let spin = SPIN[((t / 400) + i as i64).rem_euclid(10) as usize];
        (if label.chars().count() <= 11 { format!("{label} {spin}") } else { label }, G)
    } else if wait {
        (if sess.live { "waiting" } else { "permission" }.to_string(), AM)
    } else if validator {
        (format!("validating {spin}"), G)
    } else {
        let tool = current_tool(a, t).map_or("working".to_string(), |x| clip(&x.name, 10));
        (format!("{tool} {spin}"), G)
    };
    Label {
        face: face.into(),
        face_c,
        name_c: name_c(B),
        status,
        status_c,
        trace: trace(cells),
        trace_c: if stuck.is_some() || wait { AM } else { ac() },
        dozing: stuck.is_some(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TestRun, Use};

    fn agent(name: &str, spawn: i64, finish: Option<i64>) -> Agent {
        Agent { name: name.into(), spawn, finish, size: "1.0kb".into(), ..Agent::default() }
    }

    /// No two label blocks overlap (orchestrator included), all stay on the
    /// grid, and the first agent sits straight above the center.
    #[test]
    fn ring_layout_fits_one_to_eight_agents() {
        for n in 1..=MAX_RING {
            for (w, h) in [(66, 20), (71, 20), (79, 20), (79, 18)] {
                let l = layout(n, w, h).unwrap_or_else(|| panic!("{n} agents on {w}×{h}"));
                assert_eq!(l.pos.len(), n);
                assert!(fits(&l.pos, l.cx, l.cy, w as i64, h as i64));
                assert_eq!(l.pos[0].0, l.cx, "first agent at the top, centered");
                assert!(l.pos[0].1 < l.cy, "first agent above the orchestrator");
                // evenly spaced: the ring is symmetric left/right of the center
                let xs: i64 = l.pos.iter().map(|p| p.0 - l.cx).sum();
                assert!(xs.abs() <= n as i64, "{n}: lopsided {xs}");
            }
        }
        // the original design's geometry: 66×20 → center (33,10), rx 24, ry 8
        let l = layout(5, 66, 20).unwrap();
        assert_eq!((l.cx, l.cy, l.rx, l.ry), (33, 10, 24.0, 8.0));
        assert_eq!(l.pos, vec![(33, 2), (56, 8), (47, 16), (19, 16), (10, 8)]);
        // an agent straight below the center needs a slightly flatter ring
        let l = layout(4, 66, 20).unwrap();
        assert_eq!(l.ry, 7.0);
        assert_eq!(l.pos[2], (33, 17));
        // wide aspect: about 3 columns per row of radius
        assert!(l.rx / l.ry >= 2.0);
    }

    #[test]
    fn ring_layout_gives_up_when_too_small() {
        assert!(layout(8, 40, 20).is_none(), "too narrow for 8");
        assert!(layout(2, 79, 12).is_none(), "too short for top + bottom");
        assert!(layout(1, 79, 12).is_some(), "one agent fits above");
        assert!(layout(1, 79, 10).is_none(), "but not right on top of the orchestrator");
        assert!(layout(9, 200, 60).is_none(), "the ring holds at most 8");
        assert!(layout(0, 20, 4).is_some(), "just the orchestrator");
        // the terminal shrinks the grid
        assert_eq!(grid_size(79, (0, 0)), (79, GRID_H));
        assert_eq!(grid_size(79, (60, 30)), (60, GRID_H));
        assert_eq!(grid_size(79, (100, 17)), (79, 12));
    }

    #[test]
    fn ring_keeps_busy_and_selected_agents_past_eight() {
        let mut s = Session::default();
        for k in 0..10 {
            s.agents.push(agent(&format!("a{k}"), k * 100, if k < 5 { Some(k * 100 + 50) } else { None }));
        }
        let t = 50_000;
        let r = ring_agents(&s, t, None);
        assert_eq!(r.len(), MAX_RING);
        assert!((5..10).all(|k| r.contains(&k)), "running agents keep their seats: {r:?}");
        let r = ring_agents(&s, t, Some(0));
        assert!(r.contains(&0) && r.len() == MAX_RING, "{r:?}");
        assert!(r.windows(2).all(|w| w[0] < w[1]), "spawn order");
    }

    #[test]
    fn handoff_when_a_result_is_quoted_in_the_next_prompt() {
        let mut rev = agent("reviewer", 0, Some(5_000));
        rev.result = Some(
            "Verdict: changes requested\n\
             - parse_header() splits on every ':' and truncates values\n\
             - no regression test for 'Host: a:b'"
                .into(),
        );
        let mut fix = agent("fixer", 6_000, Some(9_000));
        // the orchestrator re-wraps the findings into the fixer's task
        fix.prompt = "Fix what the reviewer found:\n\nVerdict: changes requested\n  - parse_header() splits on \
                      every ':'\n    and truncates values\n- no regression test for 'Host: a:b'\n"
            .into();
        fix.uses.push(Use { time: 7_000, tool: "edit".into(), file: None, edit: true, arg: String::new() });
        fix.result = Some("Patched parser.py to split(':', 1) and added a regression test for colons.".into());
        let mut tr = agent("test-runner", 10_000, Some(12_000));
        tr.prompt = "Run the tests for this patch: Patched parser.py to split(':', 1) and added a regression test \
                     for colons."
            .into();
        let mut other = agent("scout", 11_000, None);
        other.prompt = "map the repo".into();

        let hs = detect_handoffs(&[rev, fix, tr, other]);
        assert_eq!(hs.len(), 2, "{hs:?}");
        assert_eq!((hs[0].from, hs[0].to, hs[0].time, hs[0].what.as_str()), (0, 1, 6_000, "findings"));
        assert_eq!((hs[1].from, hs[1].to, hs[1].time, hs[1].what.as_str()), (1, 2, 10_000, "patch"));
    }

    #[test]
    fn handoff_by_id_and_not_by_coincidence() {
        let mut a = agent("scout", 0, Some(1_000));
        a.ids = vec!["toolu_01ABCDEF".into(), "a1b2c3d4".into()];
        a.result = Some("Found the render loop in main.rs.".into());
        let mut b = agent("builder", 2_000, None);
        b.prompt = "Continue from agent a1b2c3d4's notes.".into();
        assert_eq!(detect_handoffs(&[a.clone(), b.clone()]).len(), 1);

        // a short or generic overlap isn't a handoff
        b.prompt = "Found the render loop? Check main.rs.".into();
        assert!(detect_handoffs(&[a.clone(), b.clone()]).is_empty());
        // one shared ordinary line isn't enough either
        a.result = Some("The parser lives in src/parser.rs\nIt has three entry points total".into());
        b.prompt = "Note: The parser lives in src/parser.rs. Write docs.".into();
        assert!(detect_handoffs(&[a.clone(), b.clone()]).is_empty());
        // and a result can't be handed to an agent spawned before it existed
        a.result = Some("x".repeat(80));
        b.prompt = "x".repeat(80);
        b.spawn = 500;
        assert!(detect_handoffs(&[a, b]).is_empty());
    }

    #[test]
    fn validator_verdicts() {
        let v = vec!["test-runner".to_string(), "reviewer".to_string()];
        assert!(is_validator("Reviewer", &v) && is_validator("test-runner", &v));
        assert!(!is_validator("fixer", &v));

        // test runner: the last test command's exit status
        let mut tr = agent("test-runner", 0, Some(5_000));
        tr.tests = vec![TestRun { time: 1_000, passed: false }, TestRun { time: 3_000, passed: true }];
        tr.result = Some("Ran the suite.".into());
        assert_eq!(verdict(&tr), Some(true));
        tr.tests.push(TestRun { time: 4_000, passed: false });
        assert_eq!(verdict(&tr), Some(false));
        assert_eq!(verdict(&agent("test-runner", 0, None)), None, "still running");

        // reviewer: its stated verdict
        let mut rv = agent("reviewer", 0, Some(5_000));
        for (text, want) in [
            ("Verdict: changes requested\n- fix the parser", Some(false)),
            ("**Verdict:** APPROVE", Some(true)),
            ("Looked at the diff.\nStatus: not approved", Some(false)),
            ("LGTM, ship it", Some(true)),
            ("No blocking issues found.", Some(true)),
            ("pytest: 14 passed, 0 failed", Some(true)),
            ("pytest: 4 passed, 10 failed", Some(false)),
            ("Summary of the code.", None),
        ] {
            rv.result = Some(text.into());
            assert_eq!(verdict(&rv), want, "{text}");
        }
        // an explicit verdict line beats a passing test run
        rv.result = Some("Tests pass but\nVerdict: request changes".into());
        rv.tests = vec![TestRun { time: 1_000, passed: true }];
        assert_eq!(verdict(&rv), Some(false));
        // an error is a fail
        rv.error = true;
        rv.result = Some("Verdict: approve".into());
        assert_eq!(verdict(&rv), Some(false));
    }

    #[test]
    fn packets_route_handoffs_along_chords() {
        let mut s = Session {
            agents: vec![agent("reviewer", 0, Some(1_000)), agent("fixer", 1_500, Some(3_000))],
            ..Session::default()
        };
        s.agents[0].result = Some("Verdict: changes requested".into());
        s.handoffs = vec![Handoff { from: 0, to: 1, time: 1_500, what: "findings".into(), size: "1.4kb".into() }];
        let v = vec!["reviewer".to_string()];
        let p = packets(&s, &v);
        let route: Vec<(Option<usize>, Option<usize>, &str)> =
            p.iter().map(|p| (p.from, p.to, p.detail.as_str())).collect();
        assert_eq!(
            route,
            vec![
                (None, Some(0), "dispatch"),
                (Some(0), None, "✗ failed"),
                (Some(0), Some(1), "findings 1.4kb"),
                (Some(1), None, "result 1.0kb"),
            ],
            "the fixer's task arrives by chord, not by its spoke"
        );
        assert_eq!(arrival(&s, 1), 1_500 + HANDOFF_MS);
    }
}
