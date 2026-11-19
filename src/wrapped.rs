//! `agentflow wrapped`: a year-in-review style recap of your agents, built
//! from `~/.agentflow/lifetime.json`. Seven full-screen slides that animate in,
//! plus an SVG export of the outro card sized for X (1200×675).

use std::collections::HashMap;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::lifetime::Store;
use crate::model::rarity;
use crate::pricing::fmt_tokens;
use crate::stats::{classify, Personality};
use crate::theme::{ac, dim, B, D, G, VD};

pub const SLIDES: usize = 7;
/// Content width of a slide.
pub const CW: usize = 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Range {
    Week,
    Month,
    All,
}

impl Range {
    pub fn label(self) -> &'static str {
        match self {
            Range::Week => "this week",
            Range::Month => "this month",
            Range::All => "all time",
        }
    }
    /// Sessions starting at or after this epoch ms count (rolling window).
    fn cutoff(self, now_ms: i64) -> Option<i64> {
        const DAY: i64 = 86_400_000;
        match self {
            Range::Week => Some(now_ms - 7 * DAY),
            Range::Month => Some(now_ms - 30 * DAY),
            Range::All => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TopAgent {
    pub role: String,
    /// most-used model family for this role
    pub model: String,
    pub personality: Option<Personality>,
    pub output: u64,
    pub runs: usize,
    pub cost: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Wrapped {
    pub range: Range,
    /// no history in range: showing made-up numbers
    pub demo: bool,
    pub sessions: usize,
    pub agents: usize,
    pub cost: f64,
    pub saved: f64,
    pub tokens: u64,
    /// tool calls by local hour of day
    pub hours: [u64; 24],
    pub top: Option<TopAgent>,
    pub avg_agents: f64,
    /// summed agent run time and the wall-clock it took (ms), over sessions
    /// that recorded both
    pub work_ms: i64,
    pub wall_ms: i64,
}

impl Wrapped {
    /// Agent time that ran side by side: work beyond the wall-clock it took.
    pub fn time_saved_ms(&self) -> i64 {
        (self.work_ms - self.wall_ms).max(0)
    }
}

/// What kind of orchestrator you are, from average agents per session.
pub fn orchestrator_type(avg: f64) -> &'static str {
    match avg.round() as i64 {
        i64::MIN..=1 => "soloist",
        2 => "duo",
        3 | 4 => "conductor",
        _ => "swarm lord",
    }
}

fn blurb(kind: &str) -> &'static str {
    match kind {
        "soloist" => "one agent at a time, start to finish",
        "duo" => "you like a sidekick",
        "conductor" => "a small section, all in time",
        _ => "you don't delegate, you deploy",
    }
}

/// Roll up the sessions in `range` (as of `now_ms`).
pub fn aggregate(store: &Store, range: Range, now_ms: i64) -> Wrapped {
    let cutoff = range.cutoff(now_ms);
    let entries: Vec<_> = store
        .sessions
        .values()
        .filter(|e| cutoff.is_none_or(|c| e.started >= c && e.started > 0))
        .collect();

    let mut w = Wrapped {
        range,
        demo: false,
        sessions: entries.len(),
        agents: 0,
        cost: 0.0,
        saved: 0.0,
        tokens: 0,
        hours: [0; 24],
        top: None,
        avg_agents: 0.0,
        work_ms: 0,
        wall_ms: 0,
    };
    /// Everything one role did across the range.
    #[derive(Default)]
    struct RoleAcc<'a> {
        output: u64,
        runs: usize,
        cost: f64,
        mix: [u32; 4],
        models: HashMap<&'a str, usize>,
    }
    let mut roles: HashMap<&str, RoleAcc> = HashMap::new();
    for e in &entries {
        w.agents += e.agents;
        w.cost += e.cost;
        w.saved += e.cache_saved;
        w.tokens += e.tokens;
        // older entries have no wall-clock: leave them out of time saved
        if e.wall_ms > 0 {
            w.work_ms += e.work_ms;
            w.wall_ms += e.wall_ms;
        }
        for (h, n) in e.hours.iter().enumerate().take(24) {
            w.hours[h] += *n as u64;
        }
        for a in &e.roster {
            let r = roles.entry(a.role.as_str()).or_default();
            r.output += a.output;
            r.runs += 1;
            r.cost += a.cost;
            for i in 0..4 {
                r.mix[i] += a.mix[i];
            }
            if !a.model.is_empty() {
                *r.models.entry(a.model.as_str()).or_default() += 1;
            }
        }
    }
    if w.sessions > 0 {
        w.avg_agents = w.agents as f64 / w.sessions as f64;
    }
    // Top agent: most output tokens, then most runs, then name (stable).
    w.top = roles
        .into_iter()
        .max_by(|(an, a), (bn, b)| (a.output, a.runs, bn).cmp(&(b.output, b.runs, an)))
        .map(|(role, r)| TopAgent {
            role: role.to_string(),
            model: r
                .models
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
                .map(|(m, _)| m.to_string())
                .unwrap_or_default(),
            personality: classify(r.mix),
            output: r.output,
            runs: r.runs,
            cost: r.cost,
        });
    w
}

/// Made-up but plausible numbers for a first run with no history.
pub fn demo(range: Range) -> Wrapped {
    let mut hours = [0u64; 24];
    let shape = [0, 0, 0, 0, 0, 0, 0, 1, 3, 9, 14, 12, 6, 10, 18, 22, 19, 13, 7, 5, 8, 6, 2, 1];
    for (h, n) in shape.iter().enumerate() {
        hours[h] = *n * 7;
    }
    Wrapped {
        range,
        demo: true,
        sessions: 34,
        agents: 118,
        cost: 42.18,
        saved: 19.40,
        tokens: 61_400_000,
        hours,
        top: Some(TopAgent {
            role: "researcher".into(),
            model: "opus".into(),
            personality: Some(Personality::Researcher),
            output: 1_240_000,
            runs: 41,
            cost: 17.90,
        }),
        avg_agents: 118.0 / 34.0,
        work_ms: (11 * 60 + 20) * 60_000,
        wall_ms: (6 * 60 + 5) * 60_000,
    }
}

/// "45s", "12m", "3h05m"
pub fn hm(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h{:02}m", s / 3600, (s / 60) % 60)
    }
}

// ---- animation helpers -----------------------------------------------------

/// "$42.18" (wrapped always shows cents, up to $1000).
fn money(c: f64) -> String {
    if c < 1000.0 {
        format!("${c:.2}")
    } else {
        format!("${c:.0}")
    }
}

fn sp<S: Into<String>>(s: S, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(c))
}

/// 0 → 1 over `dur` ms after `delay`, eased out (fast start, soft landing).
fn ease(ms: i64, delay: i64, dur: i64) -> f64 {
    let x = ((ms - delay) as f64 / dur as f64).clamp(0.0, 1.0);
    1.0 - (1.0 - x).powi(3)
}

/// Typewriter: the first chars of `s` visible at `ms` (one per 22ms).
fn typed(s: &str, ms: i64, delay: i64) -> String {
    s.chars().take(((ms - delay).max(0) / 22) as usize).collect()
}

fn center(s: &str) -> String {
    let n = s.chars().count();
    format!("{}{s}", " ".repeat(CW.saturating_sub(n) / 2))
}

fn centered(spans: Vec<Span<'static>>) -> Line<'static> {
    let n: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let mut v = vec![Span::raw(" ".repeat(CW.saturating_sub(n) / 2))];
    v.extend(spans);
    Line::from(v)
}

fn blank() -> Line<'static> {
    Line::from("")
}

// ---- slides ----------------------------------------------------------------

/// Render slide `i` at `ms` since it appeared.
pub fn slide(w: &Wrapped, i: usize, ms: i64) -> Vec<Line<'static>> {
    let mut out = vec![dots(i), blank()];
    let body = match i {
        0 => slide_agents(w, ms),
        1 => slide_spend(w, ms),
        2 => slide_top(w, ms),
        3 => slide_hours(w, ms),
        4 => slide_time(w, ms),
        5 => slide_type(w, ms),
        _ => outro_lines(w, ms),
    };
    out.extend(body);
    while out.len() < 15 {
        out.push(blank());
    }
    let hint = if i + 1 < SLIDES { "space → next · q quit" } else { "space / q to finish" };
    out.push(Line::from(sp(center(hint), VD)));
    if w.demo {
        out.push(Line::from(sp(center("demo data · no history yet"), D)));
    }
    out
}

fn dots(i: usize) -> Line<'static> {
    let mut v = Vec::new();
    for k in 0..SLIDES {
        if k > 0 {
            v.push(sp(" ", D));
        }
        v.push(sp(if k == i { "●" } else { "○" }, if k <= i { ac() } else { VD }));
    }
    centered(v)
}

fn slide_agents(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let n = (w.agents as f64 * ease(ms, 500, 1100)).round() as usize;
    let s = (w.sessions as f64 * ease(ms, 900, 900)).round() as usize;
    vec![
        Line::from(sp(center(&typed(&format!("your agents, {}", w.range.label()), ms, 0)), G)),
        blank(),
        blank(),
        Line::from(sp(center(&n.to_string()), ac())),
        Line::from(sp(center(if w.agents == 1 { "agent" } else { "agents" }), B)),
        blank(),
        Line::from(sp(center(&format!("across {s} session{}", if w.sessions == 1 { "" } else { "s" })), G)),
    ]
}

fn slide_spend(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let spent = w.cost * ease(ms, 400, 1000);
    let saved = w.saved * ease(ms, 900, 1000);
    let max = w.cost.max(w.saved).max(1e-9);
    let bar = |v: f64, delay: i64| {
        let cells = 30.0 * v / max * ease(ms, delay, 1000);
        let full = cells.floor() as usize;
        let part = ["", "⡀", "⣀", "⣄", "⣤", "⣦", "⣶", "⣷"][((cells - full as f64) * 8.0) as usize % 8];
        format!("{}{part}", "⣿".repeat(full))
    };
    let row = |label: &str, v: String, b: String, c: Color| {
        Line::from(vec![
            sp(format!("    {label:<7}"), D),
            sp(format!("{v:>8}  "), B),
            sp(b, c),
        ])
    };
    vec![
        Line::from(sp(center(&typed("what it cost", ms, 0)), G)),
        blank(),
        blank(),
        row("spent", money(spent), bar(w.cost, 400), ac()),
        blank(),
        row("saved", money(saved), bar(w.saved, 900), dim(ac(), 0.6)),
        blank(),
        Line::from(sp(
            center(&typed(&format!("{} tokens processed", fmt_tokens(w.tokens)), ms, 1700)),
            D,
        )),
    ]
}

fn slide_top(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(sp(center(&typed("your top agent", ms, 0)), G)), blank()];
    let Some(t) = &w.top else {
        out.push(Line::from(sp(center("no agents yet"), D)));
        return out;
    };
    let (glyph, tier) = rarity(&t.model).unwrap_or(("", ""));
    let inner = 30usize;
    let line = |content: String, c: Color, delay: i64| {
        let shown = typed(&content, ms, delay);
        let pad = inner.saturating_sub(content.chars().count()) / 2;
        let text = format!("{}{shown}", " ".repeat(pad));
        let fill = inner.saturating_sub(text.chars().count());
        centered(vec![sp("│", D), sp(format!("{text}{}", " ".repeat(fill)), c), sp("│", D)])
    };
    let top = format!("╭{}╮", "─".repeat(inner));
    let bot = format!("╰{}╯", "─".repeat(inner));
    out.push(Line::from(sp(center(&top), D)));
    out.push(line(String::new(), G, 0));
    out.push(line(if ms < 600 { "(·_·)".into() } else { "(^‿^)".into() }, ac(), 200));
    out.push(line(format!("{glyph} {}", t.role).trim().to_string(), B, 500));
    out.push(line(
        if tier.is_empty() { t.model.clone() } else { format!("{} · {tier}", t.model) },
        D,
        800,
    ));
    out.push(line(
        t.personality.map_or(String::new(), |p| format!("personality: {}", p.label())),
        G,
        1100,
    ));
    out.push(line(format!("{} runs · {} out", t.runs, fmt_tokens(t.output)), D, 1400));
    out.push(line(String::new(), G, 0));
    out.push(Line::from(sp(center(&bot), D)));
    out
}

/// Peak hour of day, if any activity.
pub fn peak_hour(w: &Wrapped) -> Option<usize> {
    let (h, n) = w.hours.iter().enumerate().max_by_key(|(h, n)| (**n, std::cmp::Reverse(*h)))?;
    (*n > 0).then_some(h)
}

fn slide_hours(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(sp(center(&typed("your busiest hours", ms, 0)), G)), blank()];
    let max = *w.hours.iter().max().unwrap_or(&0);
    const ROWS: usize = 4;
    // each hour is 2 columns of braille; 4 rows × 4 dots = 16 levels
    let levels: Vec<usize> = w
        .hours
        .iter()
        .enumerate()
        .map(|(h, n)| {
            let grow = ease(ms, 300 + h as i64 * 35, 700);
            if max == 0 {
                0
            } else {
                ((*n as f64 / max as f64) * (ROWS * 4) as f64 * grow).round() as usize
            }
        })
        .collect();
    let peak = peak_hour(w);
    for r in 0..ROWS {
        let floor = (ROWS - 1 - r) * 4; // dots below this row
        let mut v = vec![sp("    ", D)];
        for (h, lv) in levels.iter().enumerate() {
            let d = lv.saturating_sub(floor).min(4);
            let ch = [' ', '⣀', '⣤', '⣶', '⣿'][d];
            let c = if Some(h) == peak { ac() } else { dim(ac(), 0.55) };
            v.push(sp(format!("{ch}{ch}"), c));
        }
        out.push(Line::from(v));
    }
    out.push(Line::from(sp(format!("    {}", "─".repeat(48)), VD)));
    out.push(Line::from(sp("    0           6           12          18        23", D)));
    out.push(blank());
    out.push(Line::from(sp(
        center(&typed(
            &match peak {
                Some(h) => format!("peak: {h:02}:00–{:02}:00", (h + 1) % 24),
                None => "no activity yet".into(),
            },
            ms,
            1200,
        )),
        B,
    )));
    out
}

/// Time saved by running agents side by side: summed agent time vs the
/// wall-clock it took, as two bars.
fn slide_time(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(sp(center(&typed("running agents side by side saved you", ms, 0)), G)), blank()];
    if w.wall_ms <= 0 {
        out.push(blank());
        out.push(Line::from(sp(center("nothing ran in parallel yet"), D)));
        return out;
    }
    let saved = (w.time_saved_ms() as f64 * ease(ms, 500, 1100)) as i64;
    let max = w.work_ms.max(w.wall_ms).max(1) as f64;
    let bar = |v: i64, delay: i64| {
        let cells = 30.0 * v as f64 / max * ease(ms, delay, 1000);
        let full = cells.floor() as usize;
        let part = ["", "⡀", "⣀", "⣄", "⣤", "⣦", "⣶", "⣷"][((cells - full as f64) * 8.0) as usize % 8];
        format!("{}{part}", "⣿".repeat(full))
    };
    let row = |label: &str, v: i64, delay: i64, c: Color| {
        Line::from(vec![
            sp(format!("    {label:<7}"), D),
            sp(format!("{:>8}  ", hm((v as f64 * ease(ms, delay, 1000)) as i64)), B),
            sp(bar(v, delay), c),
        ])
    };
    out.push(Line::from(sp(center(&hm(saved)), ac())));
    out.push(Line::from(sp(center("of waiting"), B)));
    out.push(blank());
    out.push(row("work", w.work_ms, 900, ac()));
    out.push(row("wall", w.wall_ms, 1300, dim(ac(), 0.6)));
    out.push(blank());
    out.push(Line::from(sp(
        center(&typed(&format!("{} of agent work in {}", hm(w.work_ms), hm(w.wall_ms)), ms, 2000)),
        D,
    )));
    out
}

fn slide_type(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let kind = orchestrator_type(w.avg_agents);
    let mut scale = Vec::new();
    for (i, k) in ["soloist", "duo", "conductor", "swarm lord"].iter().enumerate() {
        if i > 0 {
            scale.push(sp(" · ", VD));
        }
        let on = *k == kind && ms > 1600;
        scale.push(sp(*k, if on { ac() } else { D }));
    }
    vec![
        Line::from(sp(center(&typed("as an orchestrator, you're a", ms, 0)), G)),
        blank(),
        blank(),
        Line::from(sp(center(&typed(kind, ms, 700)), ac())),
        blank(),
        Line::from(sp(center(&typed(blurb(kind), ms, 1100)), B)),
        Line::from(sp(
            center(&typed(&format!("{:.1} agents per session on average", w.avg_agents), ms, 1500)),
            D,
        )),
        blank(),
        centered(scale),
    ]
}

/// The outro summary rows: (text, color) segments. Shared by the last slide
/// and the SVG export so they always match.
pub fn outro_rows(w: &Wrapped) -> Vec<Vec<(String, Color)>> {
    // top agent: rarity glyph in the accent, then role (and personality when it adds something)
    let mut top_row: Vec<(String, Color)> = vec![("top agent    ".into(), D)];
    match &w.top {
        Some(t) => {
            if let Some((g, _)) = rarity(&t.model) {
                top_row.push((format!("{g} "), ac()));
            }
            top_row.push((t.role.clone(), B));
            if let Some(p) = t.personality.filter(|p| p.label() != t.role) {
                top_row.push((format!(" · {}", p.label()), D));
            }
        }
        None => top_row.push(("·".into(), B)),
    }
    let hour = peak_hour(w).map_or("·".into(), |h| format!("{h:02}:00"));
    let time_row: Vec<(String, Color)> = if w.wall_ms > 0 {
        vec![("time saved   ".into(), D), (hm(w.time_saved_ms()), B), (" running in parallel".into(), D)]
    } else {
        vec![("time saved   ".into(), D), ("·".into(), B)]
    };
    vec![
        vec![("agentflow wrapped".into(), ac()), (format!(" · {}", w.range.label()), D)],
        vec![],
        vec![(w.agents.to_string(), B), (" agents · ".into(), D), (w.sessions.to_string(), B), (" sessions".into(), D)],
        vec![(money(w.cost), B), (" spent · ".into(), D), (money(w.saved), B), (" saved by cache".into(), D)],
        top_row,
        vec![("busiest hour ".into(), D), (hour, B)],
        time_row,
        vec![("you're a     ".into(), D), (orchestrator_type(w.avg_agents).into(), ac())],
    ]
}

fn outro_lines(w: &Wrapped, ms: i64) -> Vec<Line<'static>> {
    let rows = outro_rows(w);
    let inner = 44usize;
    let mut out = vec![Line::from(sp(center(&format!("╭{}╮", "─".repeat(inner))), D))];
    for (i, row) in rows.iter().enumerate() {
        // rows slide in one after another
        let show = ms >= 150 * i as i64;
        let mut v = vec![sp("│  ", D)];
        let mut n = 2;
        if show {
            for (text, c) in row {
                n += text.chars().count();
                v.push(sp(text.clone(), *c));
            }
        }
        v.push(sp(format!("{}│", " ".repeat(inner.saturating_sub(n))), D));
        out.push(centered(v));
    }
    out.push(Line::from(sp(center(&format!("╰{}╯", "─".repeat(inner))), D)));
    out
}

// ---- SVG export ------------------------------------------------------------

fn hex(c: Color) -> String {
    match c {
        Color::Rgb(r, g, b) => format!("#{r:02x}{g:02x}{b:02x}"),
        _ => "#e8e8e8".into(),
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The outro card as a standalone 1200×675 SVG (X's 16:9 card size).
pub fn svg(w: &Wrapped) -> String {
    const W: f64 = 1200.0;
    const H: f64 = 675.0;
    const FS: f64 = 30.0;
    const CHAR: f64 = FS * 0.6; // monospace advance
    const LH: f64 = FS * 1.6;
    let rows = outro_rows(w);
    let cols = rows
        .iter()
        .map(|r| r.iter().map(|(t, _)| t.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0) as f64;
    let x0 = ((W - cols * CHAR) / 2.0).max(40.0);
    let y0 = (H - LH * rows.len() as f64) / 2.0 + FS;
    let pad = 36.0;
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" viewBox="0 0 {W} {H}">
<rect width="100%" height="100%" fill="#0a0a0a"/>
<rect x="{bx:.0}" y="{by:.0}" width="{bw:.0}" height="{bh:.0}" rx="18" fill="none" stroke="{stroke}" stroke-width="2"/>
<g font-family="'JetBrains Mono','SF Mono',Menlo,Consolas,monospace" font-size="{FS}" xml:space="preserve">
"##,
        bx = x0 - pad,
        by = y0 - FS - pad / 1.5,
        bw = cols * CHAR + pad * 2.0,
        bh = LH * rows.len() as f64 + pad,
        stroke = hex(dim(ac(), 0.45)),
    );
    for (i, row) in rows.iter().enumerate() {
        if row.is_empty() {
            continue;
        }
        s.push_str(&format!(r#"<text x="{x0:.0}" y="{:.0}">"#, y0 + LH * i as f64));
        for (text, c) in row {
            s.push_str(&format!(r#"<tspan fill="{}">{}</tspan>"#, hex(*c), xml(text)));
        }
        s.push_str("</text>\n");
    }
    if w.demo {
        s.push_str(&format!(
            r#"<text x="{:.0}" y="{:.0}" fill="{}" font-size="18" text-anchor="middle">demo data</text>
"#,
            W / 2.0,
            H - 30.0,
            hex(D)
        ));
    }
    s.push_str("</g>\n</svg>\n");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifetime::{AgentRec, Entry};

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 400 * DAY;

    fn rec(role: &str, model: &str, mix: [u32; 4], output: u64) -> AgentRec {
        AgentRec { role: role.into(), model: model.into(), mix, output, cost: 0.1, ms: 1_000 }
    }

    fn entry(days_ago: i64, roster: Vec<AgentRec>, hours: &[(usize, u32)]) -> Entry {
        let mut h = vec![0u32; 24];
        for (i, n) in hours {
            h[*i] = *n;
        }
        Entry {
            cost: 1.0,
            cache_saved: 0.5,
            tokens: 100,
            agents: roster.len(),
            started: if days_ago < 0 { 0 } else { NOW - days_ago * DAY },
            hours: h,
            roster,
            ..Entry::default()
        }
    }

    fn store(entries: Vec<(&str, Entry)>) -> Store {
        let mut s = Store::default();
        for (id, e) in entries {
            s.sessions.insert(id.into(), e);
        }
        s
    }

    #[test]
    fn ranges_filter_by_start_time() {
        let st = store(vec![
            ("a", entry(1, vec![rec("scout", "haiku", [5, 0, 0, 0], 10)], &[])),
            ("b", entry(10, vec![rec("scout", "haiku", [5, 0, 0, 0], 10)], &[])),
            ("c", entry(100, vec![rec("scout", "haiku", [5, 0, 0, 0], 10)], &[])),
            // written before wrapped existed: no start time, only in --all
            ("d", entry(-1, vec![], &[])),
        ]);
        assert_eq!(aggregate(&st, Range::Week, NOW).sessions, 1);
        assert_eq!(aggregate(&st, Range::Month, NOW).sessions, 2);
        let all = aggregate(&st, Range::All, NOW);
        assert_eq!(all.sessions, 4);
        assert_eq!(all.agents, 3);
        assert!((all.cost - 4.0).abs() < 1e-9 && (all.saved - 2.0).abs() < 1e-9);
        assert!((all.avg_agents - 0.75).abs() < 1e-9);
    }

    #[test]
    fn top_agent_hours_and_type() {
        let st = store(vec![
            (
                "a",
                entry(
                    2,
                    vec![
                        rec("researcher", "opus", [2, 0, 0, 3], 500),
                        rec("scout", "haiku", [9, 0, 0, 0], 300),
                        rec("fixer", "sonnet", [1, 4, 1, 0], 100),
                    ],
                    &[(14, 5), (9, 2)],
                ),
            ),
            (
                "b",
                entry(
                    3,
                    vec![
                        rec("researcher", "sonnet", [0, 0, 0, 2], 400),
                        rec("researcher", "opus", [1, 0, 0, 1], 50),
                        rec("scout", "haiku", [4, 0, 0, 0], 300),
                    ],
                    &[(14, 1), (22, 4)],
                ),
            ),
        ]);
        let w = aggregate(&st, Range::Month, NOW);
        let top = w.top.as_ref().unwrap();
        // researcher: 950 output over 3 runs beats scout's 600
        assert_eq!((top.role.as_str(), top.output, top.runs), ("researcher", 950, 3));
        assert_eq!(top.model, "opus", "its most-used model");
        assert_eq!(top.personality, Some(Personality::Researcher));
        assert_eq!(w.hours[14], 6);
        assert_eq!(w.hours[22], 4);
        assert_eq!(peak_hour(&w), Some(14));
        assert!((w.avg_agents - 3.0).abs() < 1e-9);
        assert_eq!(orchestrator_type(w.avg_agents), "conductor");
    }

    #[test]
    fn time_saved_sums_sessions_that_recorded_it() {
        let mut a = entry(1, vec![], &[]);
        (a.work_ms, a.wall_ms) = (8 * 60_000, 3 * 60_000); // 8m of work in 3m
        let mut b = entry(2, vec![], &[]);
        (b.work_ms, b.wall_ms) = (60_000, 60_000); // one agent: nothing saved
        let mut old = entry(3, vec![], &[]);
        old.work_ms = 99 * 60_000; // from before wall-clock was recorded: skipped
        let w = aggregate(&store(vec![("a", a), ("b", b), ("old", old)]), Range::Month, NOW);
        assert_eq!((w.work_ms, w.wall_ms, w.time_saved_ms()), (9 * 60_000, 4 * 60_000, 5 * 60_000));
        let txt = |w: &Wrapped, i: usize| -> String {
            slide(w, i, 10_000).iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n").collect()
        };
        let s = txt(&w, 4);
        assert!(s.contains("5m") && s.contains("9m of agent work in 4m"), "{s}");
        assert!(txt(&w, 6).contains("time saved   5m running in parallel"));
        // nothing recorded: says so instead of "0s"
        let none = aggregate(&Store::default(), Range::Month, NOW);
        assert!(txt(&none, 4).contains("nothing ran in parallel yet"));
        assert!(svg(&none).contains("time saved"));
    }

    #[test]
    fn hm_formats() {
        assert_eq!(hm(45_000), "45s");
        assert_eq!(hm(12 * 60_000 + 59_000), "12m");
        assert_eq!(hm((3 * 60 + 5) * 60_000), "3h05m");
        assert_eq!(hm(-5), "0s");
    }

    #[test]
    fn orchestrator_types() {
        assert_eq!(orchestrator_type(0.0), "soloist");
        assert_eq!(orchestrator_type(1.0), "soloist");
        assert_eq!(orchestrator_type(1.4), "soloist");
        assert_eq!(orchestrator_type(1.6), "duo");
        assert_eq!(orchestrator_type(2.4), "duo");
        assert_eq!(orchestrator_type(3.0), "conductor");
        assert_eq!(orchestrator_type(4.4), "conductor");
        assert_eq!(orchestrator_type(4.5), "swarm lord");
        assert_eq!(orchestrator_type(12.0), "swarm lord");
    }

    #[test]
    fn empty_history_aggregates_to_nothing() {
        let w = aggregate(&Store::default(), Range::Month, NOW);
        assert_eq!((w.sessions, w.agents, w.top.is_none(), peak_hour(&w)), (0, 0, true, None));
        assert!(!w.demo);
        assert!(demo(Range::Month).demo);
    }

    #[test]
    fn slides_render_at_every_stage_and_svg_is_well_formed() {
        for w in [demo(Range::Month), aggregate(&Store::default(), Range::All, NOW)] {
            for i in 0..SLIDES {
                for ms in [0, 300, 1_000, 5_000] {
                    let lines = slide(&w, i, ms);
                    for l in &lines {
                        let n: usize = l.spans.iter().map(|s| s.content.chars().count()).sum();
                        assert!(n <= CW + 2, "slide {i} at {ms}ms: {n} cols");
                    }
                }
            }
            let svg = svg(&w);
            assert!(svg.starts_with("<svg") && svg.trim_end().ends_with("</svg>"));
            assert!(svg.contains(r#"width="1200" height="675""#) && svg.contains("#0a0a0a"));
            assert_eq!(svg.matches("<text").count(), svg.matches("</text>").count());
        }
        let mut w = demo(Range::Week);
        w.top.as_mut().unwrap().role = "<&>".into();
        assert!(svg(&w).contains("&lt;&amp;&gt;"), "text is escaped");
    }

    #[test]
    fn numbers_count_up() {
        let w = demo(Range::Month);
        let text = |ms| {
            slide(&w, 0, ms)
                .iter()
                .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>())
                .collect::<Vec<_>>()
                .join("\n")
        };
        assert!(!text(0).contains("118"));
        assert!(text(5_000).contains("118") && text(5_000).contains("34 sessions"));
    }
}
