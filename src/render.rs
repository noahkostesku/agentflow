//! Frame renderer — ported from `frame(t)` / `row(a,t,last)` in
//! agentflow-mockup.html, generalized so the same visuals drive both the demo
//! (known finish times, looping clock) and live mode (unknown finish, real
//! wall-clock timestamps).
//!
//! Clocks:
//!   `t`     — session clock (ms). Agent spawn/finish are relative to the same
//!             origin, so `t - spawn` is real elapsed since that agent started.
//!   `app_t` — ms since agentflow launched. Drives the one-time boot intro
//!             (header type-in, "linking…", rule draw) so it plays on startup
//!             even when attaching to a session that began long ago.
//! In demo mode `app_t == t`, reproducing the mockup exactly.

use rand::Rng;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::lifetime::Totals;
use crate::model::{rarity, Agent, Session, Tool, ToolKind};
use crate::pricing::{approx_cost, fmt_cost, fmt_delta, fmt_tokens};
use crate::stats;
use crate::theme::*;

/// Base content width (the mockup's W=64). The model badge and cost columns
/// widen it; see `width`.
const W: i64 = 64;
/// model badge column after the role name
const BADGE: i64 = 7;
/// per-agent cost column at the end of each row
const COST: i64 = 8;
/// Frame height in lines (16 + the hint footer).
pub const H: u16 = 17;
/// The recap card appears this long after the last agent finishes, so the
/// finish wave and the final "done" play first.
pub const RECAP_DELAY: i64 = 1_500;
/// How long a "+$0.02" flash stays up.
const FLASH_MS: i64 = 1_200;

/// Which view fills the middle of the frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    Main,
    Stats,
    Timeline,
    Lifetime,
    /// one agent up close (the selected one)
    Detail,
    /// orchestrator at the center, agents on a ring (see `orbit`)
    Orbit,
}

/// The three views `v` cycles through, as shown in the header switcher.
pub const VIEWS: [(Panel, &str); 3] = [(Panel::Main, "tree"), (Panel::Orbit, "orbit"), (Panel::Timeline, "timeline")];

/// Display options and UI state for one frame.
#[derive(Clone, Copy, Default)]
pub struct View<'a> {
    pub calm: bool,
    /// per-agent cost column + header total
    pub cost: bool,
    /// "+$0.02" flash on each call (needs `cost`)
    pub flashes: bool,
    pub achievements: bool,
    /// show the recap card (caller handles the toggle and dismissal)
    pub recap: bool,
    pub panel: Panel,
    /// show the hint footer
    pub footer: bool,
    /// lifetime totals for the `l` view; `None` = persistence off
    pub lifetime: Option<&'a Totals>,
    /// selected agent (index into `Session::agents`), marked in the tree
    pub selected: Option<usize>,
    /// silence before a running agent reads as stuck (ms; 0 = off)
    pub stuck_ms: i64,
    /// the view (tree / orbit / timeline) the switcher marks; panels like
    /// stats or detail open on top of it
    pub base: Panel,
    /// terminal (columns, rows); `(0, 0)` = unknown, assume roomy
    pub term: (u16, u16),
    /// roles drawn as pass/fail gates in the orbit
    pub validators: &'a [String],
}

/// Content width for these options.
pub fn width(v: &View) -> i64 {
    W + BADGE + if v.cost { COST } else { 0 }
}

/// The recap card is due: every agent finished a moment ago.
pub fn recap_due(sess: &Session, t: i64) -> bool {
    sess.all != i64::MAX && t >= sess.all.saturating_add(RECAP_DELAY)
}

pub(crate) const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn sp<S: Into<String>>(s: S, c: Color) -> Span<'static> {
    Span::styled(s.into(), Style::default().fg(c))
}

fn clen(s: &str) -> usize {
    s.chars().count()
}

/// Right-pad with spaces to `n` chars, or truncate to `n`. Mirrors JS `pad`.
fn pad(s: &str, n: usize) -> String {
    let len = clen(s);
    if len >= n {
        s.chars().take(n).collect()
    } else {
        format!("{s}{}", " ".repeat(n - len))
    }
}

/// Left-pad with spaces to `n` chars. Mirrors JS `lpad`.
fn lpad(s: &str, n: usize) -> String {
    let len = clen(s);
    if len >= n {
        s.to_string()
    } else {
        format!("{}{s}", " ".repeat(n - len))
    }
}

fn ctake(s: &str, n: usize) -> String {
    s.chars().take(n).collect()
}

fn cfrom(s: &str, start: usize) -> String {
    s.chars().skip(start).collect()
}

/// Session clock as "HH:MM:SS", offset by 161s like the mockup. The demo stays
/// under an hour so it reads "00:MM:SS"; live sessions can run longer.
fn clk(ms: i64) -> String {
    let s = (ms / 1000).max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Fit "a // b // c" into `room` columns by shortening its longest part with
/// "…" (the project name: "agentflow-playground" → "agentflow-pl…").
fn fit_title(title: &str, room: usize) -> String {
    if clen(title) <= room {
        return title.to_string();
    }
    let mut parts: Vec<String> = title.split(" // ").map(str::to_string).collect();
    let longest = (0..parts.len()).max_by_key(|&i| clen(&parts[i])).unwrap_or(0);
    let over = clen(title) - room;
    let keep = clen(&parts[longest]).saturating_sub(over + 1).max(1);
    parts[longest] = format!("{}…", ctake(&parts[longest], keep));
    let out = parts.join(" // ");
    if clen(&out) <= room {
        out
    } else {
        ctake(&out, room)
    }
}

/// Glitch a string: ~22% of chars become a random shade block. No-op when calm.
fn glitch(s: &str, calm: bool, rng: &mut impl Rng) -> String {
    if calm {
        return s.to_string();
    }
    let blocks = ['░', '▒', '▓'];
    s.chars()
        .map(|ch| {
            if rng.gen::<f64>() < 0.22 {
                blocks[rng.gen_range(0..3)]
            } else {
                ch
            }
        })
        .collect()
}

/// Current tool: real (live) or time-cycled (demo, a tool every 1.6s once it
/// has booted). May be absent early in live mode.
pub(crate) fn current_tool(a: &Agent, t: i64) -> Option<&Tool> {
    if a.tools.is_empty() {
        None
    } else if let Some(i) = a.live_tool {
        Some(&a.tools[i.min(a.tools.len() - 1)])
    } else {
        let k = ((t - a.spawn - 1300).max(0) / 1600) as usize;
        Some(&a.tools[k % a.tools.len()])
    }
}

/// Build one subagent row. Port of `row(a,t,last)`, generalized for live mode.
fn row(
    a: &Agent,
    t: i64,
    last: bool,
    selected: bool,
    view: &View,
    live: bool,
    rng: &mut impl Rng,
) -> Vec<Span<'static>> {
    let calm = view.calm;
    let r = t - a.spawn;
    let act = a.spawn + 1300;
    let fo = a.finish; // Option: None = still running
    let mut s: Vec<Span<'static>> = Vec::new();

    // A subtle accent marker in the gutter for the selected agent.
    s.push(if selected { sp("›", ac()) } else { sp(" ", D) });
    s.push(sp(if last { "└" } else { "├" }, D));

    // 9-cell branch line, with the dispatch / return packet riding along it.
    let mut br = [' '; 9];
    let mut bc = [D; 9];
    let n = if r < 400 {
        (r as f64 / 400.0 * 9.0).floor() as i64
    } else {
        9
    };
    for i in 0..n.clamp(0, 9) {
        br[i as usize] = '─';
        bc[i as usize] = if r < 400 { ac() } else { D };
    }
    let setcell = |br: &mut [char; 9], bc: &mut [Color; 9], i: i64, ch: char| {
        if (0..9).contains(&i) {
            br[i as usize] = ch;
            bc[i as usize] = ac();
        }
    };
    if (400..800).contains(&r) {
        let p = ((r - 400) as f64 / 400.0 * 11.0).floor() as i64;
        setcell(&mut br, &mut bc, p, '●');
        setcell(&mut br, &mut bc, p - 1, '•');
        setcell(&mut br, &mut bc, p - 2, '·');
    }
    if let Some(f) = fo {
        if t >= f && t < f + 400 {
            let p = 8 - ((t - f) as f64 / 400.0 * 11.0).floor() as i64;
            setcell(&mut br, &mut bc, p, '●');
            setcell(&mut br, &mut bc, p + 1, '•');
            setcell(&mut br, &mut bc, p + 2, '·');
        }
    }
    for i in 0..9 {
        s.push(sp(br[i].to_string(), bc[i]));
    }

    // Before 800ms only the branch exists — node hasn't booted yet.
    if r < 800 {
        return s;
    }

    let done = matches!(fo, Some(f) if t >= f + 400);
    let ret = matches!(fo, Some(f) if t >= f && t < f + 400);
    let dim = matches!(fo, Some(f) if t >= f + 1700);
    let boot = r < 1300;
    // Silent for a while: sleepy instead of busy (overrides "waiting")…
    let stuck = if boot { None } else { stats::stuck_idle(a, t, view.stuck_ms) };
    // …unless a tool call is still in flight: then it's a long-running
    // command, so it reads "running 2m" with a slow spinner, awake.
    let running = if boot { None } else { stats::running_long(a, t, view.stuck_ms) };
    let wait = running.is_none() && (stuck.is_some() || a.wait.is_some_and(|(ws, we)| t >= ws && t < we));
    // ~300ms glitch the moment an error lands (spec: "Error: ~300ms glitch").
    let glitching =
        a.error && !calm && a.error_at.is_some_and(|e| t >= e && t < e + 300);

    // Node marker.
    let mut mk = "●";
    let mut mc = ac();
    if r < 900 {
        mc = wh();
    } else if boot {
        mk = if (t / 100) % 2 != 0 { "●" } else { "○" };
    }
    if done {
        mk = "○";
        mc = if dim { D } else { G };
    }
    if wait {
        mc = AM;
    }
    if a.error {
        mk = "○";
        mc = RED;
    }
    s.push(sp(format!(" {} ", mk), mc));

    // Name types out during boot.
    let shown = ((r - 800) / 30).max(0) as usize;
    let mut name = ctake(&a.name, shown);
    if glitching {
        name = glitch(&name, calm, rng);
    }
    let name_c = if glitching { RED } else if dim { D } else { B };
    // Rarity glyph by model, just before the name.
    let named = shown > 0;
    s.push(match rarity(&a.model).filter(|_| named) {
        Some((g, _)) => sp(format!("{g} "), rarity_color(&a.model, dim)),
        None => sp("  ", D),
    });
    s.push(sp(pad(&name, 12), name_c));
    // Model badge, dim, appears once the name has typed in.
    let badge = if shown > clen(&a.name) { a.model.as_str() } else { "" };
    s.push(sp(pad(badge, BADGE as usize), if dim { VD } else { D }));

    let tl = current_tool(a, t);

    // Face reacts to phase and tool kind.
    let face;
    let mut fc = G;
    if a.error {
        face = if glitching { glitch("(×_×)", calm, rng) } else { "(×_×)".into() };
        fc = RED;
    } else if boot {
        face = if r < 1100 { "(·_·)" } else { "(•_•)" }.to_string();
    } else if done {
        face = "(^‿^)".into();
        fc = if dim { D } else { ac() };
    } else if stuck.is_some() {
        face = "(-_-)".into();
        fc = AM;
    } else if wait {
        face = "(·_·)".into();
        fc = AM;
    } else if (t + a.bob * 500) % 3700 < 130 {
        face = "(-‿-)".into();
    } else if tl.map(|x| x.kind) == Some(ToolKind::Look) {
        face = if (t / 450) % 2 != 0 { "(•_• )" } else { "( •_•)" }.into();
    } else {
        face = if (t / 300) % 2 != 0 { "(•o•)" } else { "(•‿•)" }.into();
    }
    s.push(sp(pad(&face, 8), fc));

    // Status word.
    let st;
    let mut sc = G;
    if a.error {
        st = if glitching { glitch("error", calm, rng) } else { "error".into() };
        sc = RED;
    } else if boot {
        let stage = ((r - 800) / 170).clamp(0, 2) as usize;
        st = glitch(["init", "link", "ready"][stage], calm, rng);
        sc = ac();
    } else if let Some(idle) = stuck {
        st = idle_label(idle);
        sc = AM;
    } else if let Some(ms) = running {
        st = running_label(ms);
    } else if wait {
        // Live mode can't tell *why* a call is blocked, so it says "waiting";
        // the scripted demo knows its wait is a permission prompt.
        let word = if live { "waiting" } else { "permission" };
        st = format!("{}{word}", if (t / 300) % 2 != 0 { "? " } else { "  " });
        sc = AM;
    } else if ret {
        st = "returning".into();
        sc = ac();
    } else if done {
        let f = fo.unwrap();
        let shown = ((t - f - 400) / 60).max(0) as usize;
        st = ctake("done ✓", shown);
        sc = if dim { D } else { ac() };
    } else {
        st = tl.map(|x| x.name.clone()).unwrap_or_else(|| "working".into());
    }
    s.push(sp(pad(&st, 12), sc));

    // Spinner / sparkle cell.
    let mut s1 = " ".to_string();
    if !boot && !done && !wait && !ret && !a.error {
        // a long-running call spins slowly
        let step = if running.is_some() { 400 } else { 80 };
        s1 = SPIN[((t / step + a.bob).rem_euclid(10)) as usize].to_string();
    }
    if let Some(f) = fo {
        if t >= f + 700 && t < f + 1100 {
            let i = ((t - f - 700) / 134).clamp(0, 2) as usize;
            s1 = ['·', '✦', '·'][i].to_string();
        }
    }
    s.push(sp(s1, ac()));

    // Elapsed time. Demo scales ×3 for liveliness; live shows real seconds.
    let scale = if live { 1.0 } else { 3.0 };
    let end = fo.unwrap_or(t);
    let secs = (t.min(end) - a.spawn).max(0) as f64 / 1000.0 * scale;
    s.push(sp(
        format!("{}  ", lpad(&format!("{}s", secs.floor() as i64), 5)),
        if dim { D } else { G },
    ));

    // Progress bar.
    let pr = if done {
        1.0
    } else if let Some(f) = fo {
        // Known finish (demo): linear interpolation toward 0.85.
        (((t - act) as f64 / (f - act).max(1) as f64) * 0.85).clamp(0.0, 0.85)
    } else {
        // Live, unknown finish: asymptotic crawl that never quite fills.
        let e = (t - act).max(0) as f64;
        (0.85 * (1.0 - (-e / 9000.0).exp())).clamp(0.0, 0.85)
    };
    let cells = (pr * 8.0).round() as i64;
    let shine = match fo {
        Some(f) if t >= f + 400 && t < f + 800 => {
            ((t - f - 400) as f64 / 400.0 * 8.0).floor() as i64
        }
        _ => -1,
    };
    let bar_done = if dim { D } else if a.error { RED } else { ac() };
    for i in 0..8 {
        if i < cells {
            s.push(sp("⣿", if i == shine { wh() } else { bar_done }));
        } else if i == cells && !boot && !wait && !a.error {
            // Shimmering leading cell — steady when calm.
            if calm {
                s.push(sp("⣀", VD));
            } else {
                let on = (t / 200) % 2 != 0;
                s.push(sp(if on { "⣷" } else { "⣀" }, if on { shim() } else { VD }));
            }
        } else {
            s.push(sp("⣀", VD));
        }
    }

    // Cost so far, or a brief "+$0.02" as each call lands.
    if view.cost {
        let flash = a
            .calls_at(t)
            .last()
            .filter(|c| view.flashes && t - c.time < FLASH_MS);
        let (txt, c) = match flash {
            Some(c) => (fmt_delta(c.cost), ac()),
            None if a.calls_at(t).next().is_none() => (String::new(), D),
            None => (fmt_cost(a.cost_at(t)), if dim { D } else { G }),
        };
        s.push(sp(format!("  {}", lpad(&txt, COST as usize - 2)), c));
    }

    s
}

/// Render a full frame (`H` logical lines).
pub fn frame(
    sess: &Session,
    t: i64,
    app_t: i64,
    view: &View,
    rng: &mut impl Rng,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let all = sess.all;
    let live = sess.live;
    let calm = view.calm;
    let w = width(view);

    // --- Header: title types in with a blinking cursor; "linking… ok" then
    // clock, with the running session cost in front of it.
    // The title gets the room the right side leaves once it's all up (view
    // keys, cost, clock), so it never runs into them; a long project name is
    // shortened with "…".
    let steady = clen(&clk(t))
        + if view.cost { clen(&approx_cost(sess.cost_at(t))) + 4 } else { 0 }
        + view_switcher(view).iter().map(|s| clen(&s.content)).sum::<usize>();
    let fitted = fit_title(&sess.header, (w as usize).saturating_sub(steady + 3));
    let ttl = &fitted;
    let shown = ctake(ttl, (app_t / 16).max(0) as usize);
    let cur = if clen(&shown) < clen(ttl) && (app_t / 120) % 2 != 0 {
        "█"
    } else {
        ""
    };
    let linking = app_t < 1500;
    let rt = if app_t < 700 {
        String::new()
    } else if app_t < 1100 {
        "linking…".into()
    } else if linking {
        "linking… ok".into()
    } else {
        clk(t)
    };
    // "?": some tokens were billed by a model with no known price ($0)
    let unpriced = if sess.unpriced.is_empty() { "" } else { "?" };
    let total = (view.cost && !linking).then(|| format!("{}{unpriced} · ", approx_cost(sess.cost_at(t))));
    let lt = format!(" {}{}", shown, cur);
    // View switcher "v  tree · orbit · timeline", the current one in the accent.
    let switcher = (!linking).then(|| view_switcher(view));
    let sw_len = switcher.as_ref().map_or(0, |v| v.iter().map(|s| clen(&s.content)).sum::<usize>());
    let rt_len = clen(&rt) + total.as_ref().map_or(0, |s| clen(s)) + sw_len;
    let mid_w = (w as usize).saturating_sub(10).saturating_sub(rt_len);
    let mut hdr = vec![sp(ctake(&lt, 10), ac()), sp(pad(&cfrom(&lt, 10), mid_w), G)];
    hdr.extend(switcher.unwrap_or_default());
    if let Some(total) = total {
        hdr.push(sp(total, G));
    }
    hdr.push(sp(rt.clone(), if rt.starts_with("link") { ac() } else { D }));
    out.push(Line::from(hdr));

    // --- Top rule draws left to right.
    let rn = if app_t < 600 {
        (app_t as f64 / 600.0 * w as f64).floor() as i64
    } else {
        w
    };
    let dashes = "─".repeat((rn - 1).max(0) as usize);
    out.push(Line::from(sp(
        format!(" {}", dashes),
        if app_t < 600 { ac() } else { D },
    )));

    // --- Orbit: its own grid and "latest" line in place of everything below,
    // or the tree when the terminal is too small for the ring.
    let mut orbit_fallback = false;
    if view.panel == Panel::Orbit {
        if let Some((grid, latest)) = crate::orbit::panel(sess, t, view, w) {
            out.extend(grid);
            out.push(bottom_rule(sess, t, view, w));
            out.push(latest);
            out.push(if view.footer { footer_line() } else { Line::from("") });
            return out;
        }
        orbit_fallback = true;
    }

    out.push(Line::from(""));

    // --- Orchestrator row.
    let fl = sess
        .agents
        .iter()
        .find(|a| matches!(a.finish, Some(f) if t >= f + 400 && t < f + 1400));
    let of = if t >= all || fl.is_some() {
        "(^‿^)"
    } else if t % 4100 < 130 {
        "(-‿-)"
    } else {
        "(•‿•)"
    };
    let os = if app_t < 1000 && !live {
        "booting".to_string()
    } else if t >= all {
        "all done ✓".to_string()
    } else if sess.agents.is_empty() {
        "waiting".to_string()
    } else {
        format!("delegating {}", SPIN[((t / 80).rem_euclid(10)) as usize])
    };
    if view.panel == Panel::Detail {
        out.extend(detail_panel(sess, t, view, w));
        return finish_frame(out, sess, t, view, w);
    }

    let mut o = vec![
        sp(" ◆ ", ac()),
        sp(pad(sess.root_name(), 13), B),
        sp(pad(&sess.orch_model, BADGE as usize), D),
        sp(pad(of, 8), if t >= all || fl.is_some() { ac() } else { G }),
        sp(pad(&os, 14), if t >= all { ac() } else { G }),
    ];
    if let Some(fl) = fl {
        let f = fl.finish.unwrap();
        o.push(sp(
            format!("+{}", fl.size),
            if t - f - 400 < 600 { ac() } else { D },
        ));
    }
    out.push(Line::from(o));

    // --- Middle: 10 lines of tree + log, or a panel.
    let vis = visible_agents(sess, t, view.selected);
    let mid = match view.panel {
        Panel::Main | Panel::Orbit => main_panel(sess, t, &vis, view, w, rng),
        Panel::Stats => stats_panel(sess, t, &vis, view, w),
        Panel::Timeline => timeline_panel(sess, t, view, w),
        Panel::Lifetime => lifetime_panel(view, w),
        Panel::Detail => unreachable!("handled above"),
    };
    debug_assert_eq!(mid.len(), 10);
    out.extend(mid);

    // --- Achievement popup (one at a time, fades out), or why the orbit
    // isn't showing.
    let popup = if view.achievements { popup_line(sess, t, app_t, calm) } else { Line::from("") };
    out.push(if orbit_fallback && popup.spans.is_empty() {
        Line::from(sp(" orbit needs a bigger terminal · showing the tree", D))
    } else {
        popup
    });
    finish_frame(out, sess, t, view, w)
}

/// "v  tree · orbit · timeline": the base view in the accent, the rest dim.
fn view_switcher(view: &View) -> Vec<Span<'static>> {
    let current = if VIEWS.iter().any(|(p, _)| *p == view.panel) { view.panel } else { view.base };
    let mut v = vec![sp("v  ", DIM)];
    for (i, (p, name)) in VIEWS.iter().enumerate() {
        if i > 0 {
            v.push(sp(" · ", DIM));
        }
        v.push(sp(*name, if *p == current { ac() } else { DIM }));
    }
    v.push(sp("  ", D));
    v
}

/// Bottom rule (with the finish wave) and the key-hint footer.
fn finish_frame(
    mut out: Vec<Line<'static>>,
    sess: &Session,
    t: i64,
    view: &View,
    w: i64,
) -> Vec<Line<'static>> {
    out.push(bottom_rule(sess, t, view, w));

    // --- Key hints (startup and `?`).
    out.push(if view.footer { footer_line() } else { Line::from("") });

    out
}

/// Bottom rule, with an accent wave that ripples across when all are done.
fn bottom_rule(sess: &Session, t: i64, view: &View, w: i64) -> Line<'static> {
    let (all, calm) = (sess.all, view.calm);
    // Only touch `t - all` once all are done: while agents run `all` is
    // i64::MAX, and a negative live clock would overflow the subtraction.
    let mut bl = vec![sp(" ", D)];
    let wave = !calm && t >= all && t - all < 900;
    for i in 1..w {
        let on = wave && {
            let x = (t - all) as f64 / 900.0 * w as f64;
            (i as f64 - x).abs() < 4.0
        };
        bl.push(sp(if on { "━" } else { "─" }, if on { ac() } else { D }));
    }
    Line::from(bl)
}

/// Up to 3 agents for the tree, as indexes into `sess.agents`. When more
/// exist, agents still working (or just finished) win over long-done ones,
/// newest first, and the selected agent is always kept; the chosen rows are
/// then shown in spawn order.
fn visible_agents(sess: &Session, t: i64, selected: Option<usize>) -> Vec<usize> {
    let started: Vec<usize> = (0..sess.agents.len()).filter(|&i| t >= sess.agents[i].spawn).collect();
    if started.len() <= 3 {
        return started;
    }
    let settled = |i: usize| sess.agents[i].finish.is_some_and(|f| t >= f + 1700);
    let mut idx = started.clone();
    // unsettled first, then by recency (later index = newer spawn)
    idx.sort_by_key(|&i| (settled(i), std::cmp::Reverse(i)));
    idx.truncate(3);
    if let Some(sel) = selected.filter(|s| started.contains(s) && !idx.contains(s)) {
        idx[2] = sel;
    }
    idx.sort_unstable();
    idx
}

/// A section rule: " ── name ───…── right ──" spanning the width.
fn rule(name: &str, right: &str, w: i64) -> Line<'static> {
    let left = format!(" ── {name} ");
    if right.is_empty() {
        let fill = (w as usize).saturating_sub(clen(&left));
        return Line::from(sp(format!("{left}{}", "─".repeat(fill)), D));
    }
    let right = format!(" {right} ");
    let fill = (w as usize).saturating_sub(clen(&left) + clen(&right) + 2);
    Line::from(vec![
        sp(format!("{left}{}", "─".repeat(fill)), D),
        sp(right, D),
        sp("──", D),
    ])
}

fn pad_lines(mut v: Vec<Line<'static>>, n: usize) -> Vec<Line<'static>> {
    v.truncate(n);
    while v.len() < n {
        v.push(Line::from(""));
    }
    v
}

/// Tree (│ + 3 rows), blank, then the log — or the recap card in its place.
fn main_panel(
    sess: &Session,
    t: i64,
    vis: &[usize],
    view: &View,
    w: i64,
    rng: &mut impl Rng,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(if vis.is_empty() { Line::from("") } else { Line::from(sp(" │", D)) });
    for i in 0..3 {
        out.push(match vis.get(i) {
            Some(&k) => {
                let a = &sess.agents[k];
                let sel = view.selected == Some(k);
                Line::from(row(a, t, i == vis.len() - 1, sel, view, sess.live, rng))
            }
            None => Line::from(""),
        });
    }
    // Sleepy agents: "z"s drift up out of the face into the row above.
    for (i, &k) in vis.iter().enumerate() {
        let a = &sess.agents[k];
        if t - a.spawn >= 1300 && stats::stuck_idle(a, t, view.stuck_ms).is_some() {
            for (up, col, ch, c) in zzz(t + a.bob * 211, view.calm) {
                overlay(&mut out[i + 1 - up], col, ch, c);
            }
        }
    }
    out.push(Line::from(""));

    if view.recap {
        out.extend(recap_lines(sess, t, view, w));
        return out;
    }

    // Log of the last 4 events.
    out.push(rule("log", "", w));
    let visible: Vec<&crate::model::Event> =
        sess.events.iter().filter(|e| e.time <= t).collect();
    let start = visible.len().saturating_sub(4);
    let tail = &visible[start..];
    for i in 0..4 {
        if let Some(e) = tail.get(i) {
            let detail_color = if e.detail.starts_with('?') {
                AM
            } else if e.color == ac() || e.color == RED {
                e.color
            } else {
                G
            };
            out.push(Line::from(vec![
                sp(format!(" {}  ", clk(e.time)), D),
                sp(pad(&e.label, 28), e.color),
                sp(e.detail.clone(), detail_color),
            ]));
        } else {
            out.push(Line::from(""));
        }
    }
    out
}

/// The recap card: 5 lines in place of the log.
fn recap_lines(sess: &Session, t: i64, view: &View, w: i64) -> Vec<Line<'static>> {
    let r = stats::recap(sess, t);
    let kv = |k: &str, v: String, extra: String| {
        Line::from(vec![
            sp(format!(" {}", pad(k, 9)), D),
            sp(pad(&v, 14), B),
            sp(extra, G),
        ])
    };
    let mut out = vec![rule("recap", "any key to dismiss", w)];
    let tokens = format!("{} tokens", fmt_tokens(r.tokens));
    out.push(if view.cost {
        kv("total", approx_cost(r.cost), tokens)
    } else {
        kv("total", tokens, String::new())
    });
    out.push(match &r.mvp {
        Some((name, o)) => kv("mvp", name.clone(), format!("{} tokens out", fmt_tokens(*o))),
        None => kv("mvp", "·".into(), String::new()),
    });
    out.push(match &r.longest {
        Some((name, ms)) => kv("longest", name.clone(), format!("{}s", ms / 1000)),
        None => kv("longest", "·".into(), String::new()),
    });
    let hit = format!("{:.0}% hit", r.hit * 100.0);
    out.push(if view.cost {
        kv("cache", format!("saved {}", fmt_cost(r.savings)), hit)
    } else {
        kv("cache", hit, String::new())
    });
    out
}

/// `s`: per-agent tokens/sec sparklines, cache-hit %, top tools, top files.
fn stats_panel(
    sess: &Session,
    t: i64,
    vis: &[usize],
    view: &View,
    w: i64,
) -> Vec<Line<'static>> {
    let vis: Vec<&Agent> = vis.iter().map(|&k| &sess.agents[k]).collect();
    let mut u = crate::model::Usage::default();
    for c in sess.all_calls_at(t) {
        u.add(&c.usage);
    }
    let mut hit = if u.total() == 0 {
        String::new()
    } else {
        format!("cache hit {:.0}%", stats::hit_rate(&u) * 100.0)
    };
    if view.cost {
        // what the tokens would cost on the pay-per-token API, not a bill
        let sep = if hit.is_empty() { "" } else { " · " };
        hit = format!("api-equivalent cost{sep}{hit}");
        if !sess.unpriced.is_empty() {
            hit = format!("{hit} · no price: {} ($0)", sess.unpriced.join(", "));
        }
    }
    let mut out = vec![rule("stats", &hit, w)];

    // 3 agent lines
    for i in 0..3 {
        let Some(a) = vis.get(i) else {
            out.push(if i == 0 { Line::from(sp(" no agents yet", D)) } else { Line::from("") });
            continue;
        };
        let spark = stats::sparkline(&stats::token_buckets(a, t, if view.cost { 14 } else { 12 }));
        let tps = stats::tokens_per_sec(a, t, sess.scale());
        let ah = stats::hit_rate(&a.usage_at(t));
        let done = a.finish.is_some_and(|f| t >= f);
        let pers = stats::personality(a, t).map_or("", |p| p.label());
        let mut l = vec![
            sp(" ", D),
            rarity_span(&a.model, false),
            sp(pad(&a.name, 12), if done { G } else { B }),
            sp(pad(&a.model, BADGE as usize), D),
            sp(spark, if done { D } else { ac() }),
            sp(format!("  {} tok/s", lpad(&format!("{tps:.0}"), 5)), G),
            sp(format!("  {} cache", lpad(&format!("{:.0}%", ah * 100.0), 4)), D),
            sp(format!(" {}", pad(pers, 11)), G),
        ];
        if view.cost {
            l.push(sp(lpad(&fmt_cost(a.cost_at(t)), 6), G));
        }
        out.push(Line::from(l));
    }

    // tools
    let tools = stats::top_tools(sess, t, 5);
    let mut l = vec![sp(" tools  ", D)];
    if tools.is_empty() {
        l.push(sp("·", D));
    }
    for (i, (name, n)) in tools.iter().enumerate() {
        if i > 0 {
            l.push(sp(" · ", VD));
        }
        l.push(sp(format!("{name} "), G));
        l.push(sp(n.to_string(), ac()));
    }
    out.push(Line::from(l));

    // parallelism: how much the agents overlapped
    let par = stats::parallelism(&sess.agents, t);
    let mut l = vec![sp(" team   ", D)];
    if par.work_ms == 0 {
        l.push(sp("·", D));
    } else {
        l.extend(parallel_spans(&par));
        // "· 49s of work in 36s", when it fits (long sessions run wide)
        let tail = format!(" · {}", work_in_wall(&par, sess.scale()));
        let used: usize = l.iter().map(|s| clen(&s.content)).sum();
        if used + clen(&tail) <= w as usize {
            l.push(sp(tail, G));
        }
    }
    out.push(Line::from(l));

    // files heatmap: top 4
    let files = stats::top_files(sess, t, 4);
    let max = files.first().map_or(0, |f| f.1);
    let name_w = 26usize;
    let bar_w = 12usize;
    for i in 0..4 {
        let label = if i == 0 { " files  " } else { "        " };
        let Some((f, n)) = files.get(i) else {
            out.push(if i == 0 { Line::from(vec![sp(label, D), sp("·", D)]) } else { Line::from("") });
            continue;
        };
        // hotter files get the accent, cooler ones fade toward gray
        let heat = *n as f64 / max.max(1) as f64;
        let bar_c = if heat > 0.66 { ac() } else if heat > 0.33 { shim() } else { D };
        out.push(Line::from(vec![
            sp(label, D),
            sp(pad(&short_path(f, name_w), name_w), G),
            sp(stats::braille_bar(*n, max, bar_w), bar_c),
            sp(format!("  {n}"), D),
        ]));
    }
    out
}

/// Last `n` chars of a path, cut at a '/' when possible ("…/src/render.rs").
fn short_path(p: &str, n: usize) -> String {
    let len = clen(p);
    if len <= n {
        return p.to_string();
    }
    let tail: String = p.chars().skip(len - (n - 1)).collect();
    match tail.find('/') {
        Some(i) if i + 1 < tail.len() => format!("…{}", &tail[i..]),
        _ => format!("…{tail}"),
    }
}

/// `t`: swimlanes. One lane per agent (orchestrator first) across the session,
/// braille dots as dense as the activity in each slice of time: accent while
/// active, dim while waiting or idle, a red tick where an error landed. Below:
/// a time axis and the session's peak parallelism.
fn timeline_panel(sess: &Session, t: i64, view: &View, w: i64) -> Vec<Line<'static>> {
    // Once everyone is done the axis stops at the last finish, so a session
    // that ended a while ago still fills the width.
    let span = t.min(sess.all).max(1);
    let scale = sess.scale();
    let par = stats::parallelism(&sess.agents, t);
    let mut right = work_in_wall(&par, scale);
    if par.saved_ms() > 0 {
        right += &format!(" · saved {}", dur((par.saved_ms() as f64 * scale / 1000.0) as i64));
    }
    let mut out = vec![rule("timeline", &right, w)];
    // the chain of agents that set the session's length: brighter mauve
    let critical = stats::critical_path(sess, t);

    const LABEL: usize = 17;
    let cells = (w as usize).saturating_sub(LABEL + 1).max(8);
    let cell_of = |ms: i64| ((ms.clamp(0, span) as i128 * cells as i128 / span as i128) as usize).min(cells - 1);
    let range = |c: usize| {
        let c0 = (span as i128 * c as i128 / cells as i128) as i64;
        let c1 = (span as i128 * (c + 1) as i128 / cells as i128) as i64;
        (c0, c1)
    };

    let started: Vec<usize> = (0..sess.agents.len()).filter(|&i| sess.agents[i].spawn <= t).collect();
    let shown: Vec<(usize, &Agent)> =
        started[started.len().saturating_sub(6)..].iter().map(|&i| (i, &sess.agents[i])).collect();

    // Activity per lane: orchestrator = its own calls + every dispatch.
    let orch_times = sess
        .orch_calls
        .iter()
        .map(|c| c.time)
        .chain(sess.agents.iter().map(|a| a.spawn))
        .filter(|x| *x <= t);
    let orch = stats::density(orch_times, span, cells);
    let lanes: Vec<Vec<u32>> = shown
        .iter()
        .map(|(_, a)| {
            let times = a.uses.iter().map(|u| u.time).chain(a.calls.iter().map(|c| c.time));
            stats::density(times.filter(|x| *x <= t), span, cells)
        })
        .collect();
    let max = lanes.iter().chain([&orch]).flatten().copied().max().unwrap_or(0).max(1);
    let level = |n: u32| -> char {
        // alive but quiet keeps a faint baseline so the lane reads as a span
        const L: [char; 8] = ['⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿', '⣿'];
        if n == 0 {
            '⡀'
        } else {
            L[((n as f64 / max as f64) * 7.0).ceil().clamp(1.0, 7.0) as usize]
        }
    };

    // orchestrator lane: alive across the whole axis
    let mut l = vec![sp(" ◆ ", ac()), sp(pad(sess.root_name(), LABEL - 3), B)];
    for n in &orch {
        l.push(sp(level(*n).to_string(), ac()));
    }
    out.push(Line::from(l));

    for ((i, a), dens) in shown.iter().zip(&lanes) {
        let end = a.finish.unwrap_or(t).min(t);
        let crit = critical.contains(i);
        let on = if crit { light() } else { ac() };
        let mut l = vec![sp(" ", D), rarity_span(&a.model, false), sp(pad(&a.name, LABEL - 3), if crit { B } else { G })];
        let err_cell = a.error_at.filter(|_| a.error).filter(|e| *e <= t).map(cell_of);
        // idle span: from when it went quiet (+ threshold) until now
        let idle_from = stats::stuck_idle(a, t, view.stuck_ms).map(|idle| t - idle + view.stuck_ms);
        for (c, n) in dens.iter().enumerate() {
            let (c0, c1) = range(c);
            let alive = a.spawn < c1.max(c0 + 1) && end >= c0;
            if err_cell == Some(c) {
                l.push(sp("┃", RED));
            } else if !alive {
                l.push(sp(" ", D));
            } else {
                let waiting = a.wait.is_some_and(|(ws, we)| ws < c1 && we.min(t) >= c0)
                    || idle_from.is_some_and(|f| f < c1);
                l.push(sp(level(*n).to_string(), if waiting { D } else { on }));
            }
        }
        out.push(Line::from(l));
    }
    if started.is_empty() {
        out.push(Line::from(sp(" no agents yet", D)));
    }

    // time axis: labels at 0, ¼, ½, ¾ and the end
    let mut axis: Vec<char> = vec![' '; cells];
    for k in 0..=4usize {
        let secs = (span as f64 * scale / 1000.0 * k as f64 / 4.0) as i64;
        let label: Vec<char> = dur(secs).chars().collect();
        let pos = (cells - 1) * k / 4;
        let start = if k == 4 { pos + 1 - label.len() } else { pos.min(cells - label.len()) };
        if k > 0 && axis[start.saturating_sub(1)..(start + label.len()).min(cells)].iter().any(|c| *c != ' ') {
            continue; // too tight to fit without overlapping
        }
        for (i, ch) in label.into_iter().enumerate() {
            if start + i < cells {
                axis[start + i] = ch;
            }
        }
    }
    out = pad_lines(out, 8);
    // the axis line's label column doubles as the critical-path legend
    let legend = shown.iter().any(|(i, _)| critical.contains(i)) && critical.len() > 1;
    out.push(Line::from(vec![
        if legend { sp(pad(" ⣿ critical path", LABEL), light()) } else { sp(" ".repeat(LABEL), D) },
        sp(axis.into_iter().collect::<String>(), D),
    ]));

    let now = sess.agents.iter().filter(|a| a.spawn <= t && a.finish.is_none_or(|f| f > t)).count();
    let mut p = vec![sp(" parallelism  ", D)];
    p.extend(parallel_spans(&par));
    if now > 0 && sess.all == i64::MAX {
        p.push(sp(format!("  ·  {now} now"), G));
    }
    out.push(Line::from(p));
    out
}

/// "38s of work in 13s": summed agent time vs the wall-clock it took (empty
/// with no agents yet). Durations are as displayed (the demo runs ×3).
fn work_in_wall(p: &stats::Parallelism, scale: f64) -> String {
    if p.work_ms <= 0 {
        return String::new();
    }
    let secs = |ms: i64| (ms as f64 * scale / 1000.0) as i64;
    format!("{} of work in {}", dur(secs(p.work_ms)), dur(secs(p.wall_ms)))
}

/// "peak 3 agents · avg 1.7× · 2+ active 42%"
fn parallel_spans(p: &stats::Parallelism) -> Vec<Span<'static>> {
    vec![
        sp(format!("peak {} agent{}", p.peak, if p.peak == 1 { "" } else { "s" }), B),
        sp(" · avg ", D),
        sp(format!("{:.1}×", p.avg()), B),
        sp(" · 2+ active ", D),
        sp(format!("{:.0}%", p.multi_share() * 100.0), B),
    ]
}

/// Detail view for the selected agent: header (rarity, role, model, tier,
/// personality, state, elapsed, cost), its task, then its tool-call timeline
/// beside a preview of its last result. Fills the 12 lines between the top
/// rule's blank and the bottom rule.
fn detail_panel(sess: &Session, t: i64, view: &View, w: i64) -> Vec<Line<'static>> {
    let Some(a) = view.selected.and_then(|i| sess.agents.get(i)) else {
        return pad_lines(vec![Line::from(sp(" no agent selected · ↑/↓ to pick one", D))], 12);
    };
    let mut out = Vec::new();

    // header
    let (glyph, tier) = rarity(&a.model).unwrap_or(("", ""));
    let pers = stats::personality(a, t).map(|p| p.label());
    let done = a.finish.is_some_and(|f| t >= f);
    let (state, sc) = if a.error {
        ("error".to_string(), RED)
    } else if done {
        ("done".to_string(), ac())
    } else if let Some(idle) = stats::stuck_idle(a, t, view.stuck_ms) {
        (idle_label(idle), AM)
    } else if let Some(ms) = stats::running_long(a, t, view.stuck_ms) {
        (running_label(ms), G)
    } else if a.wait.is_some_and(|(ws, we)| t >= ws && t < we) {
        ("waiting".to_string(), AM)
    } else {
        ("working".to_string(), G)
    };
    let end = a.finish.unwrap_or(t).min(t);
    let secs = ((end - a.spawn).max(0) as f64 * sess.scale() / 1000.0) as i64;
    let mut left = vec![
        sp("‹ ", ac()),
        sp(if glyph.is_empty() { String::new() } else { format!("{glyph} ") }, rarity_color(&a.model, false)),
        sp(a.name.clone(), B),
    ];
    let mut meta = Vec::new();
    if !a.model.is_empty() {
        meta.push(if tier.is_empty() { a.model.clone() } else { format!("{} · {tier}", a.model) });
    }
    if let Some(p) = pers {
        meta.push(p.to_string());
    }
    if !meta.is_empty() {
        left.push(sp(format!("   {}", meta.join(" · ")), D));
    }
    let mut right = vec![sp(state, sc), sp(format!(" · {}", dur(secs)), G)];
    if view.cost {
        right.push(sp(format!(" · {}", fmt_cost(a.cost_at(t))), G));
    }
    out.push(spread(left, right, w));

    // task
    let task = if a.task.is_empty() { "·".to_string() } else { a.task.clone() };
    out.push(Line::from(vec![sp(" task  ", D), sp(clip(&task, w as usize - 8), G)]));

    // two columns: tool calls | last result
    const LW: usize = 36;
    let rw = (w as usize).saturating_sub(LW + 3);
    let head = |name: &str, n: usize| {
        let h = format!(" ── {name} ");
        format!("{h}{}", "─".repeat(n.saturating_sub(clen(&h))))
    };
    out.push(Line::from(vec![sp(head("tool calls", LW), D), sp(" ┬", VD), sp(head("last result", rw + 1), D)]));

    const ROWS: usize = 9;
    let uses: Vec<_> = a.uses.iter().filter(|u| u.time <= t).collect();
    let calls = &uses[uses.len().saturating_sub(ROWS)..];
    let result: Vec<String> = match &a.result {
        Some(r) if done => r.lines().skip_while(|l| l.trim().is_empty()).take(ROWS).map(str::to_string).collect(),
        _ if done => vec!["(no result captured)".into()],
        _ => vec!["(still working…)".into()],
    };
    let more = a.result.as_ref().is_some_and(|r| done && r.lines().count() > ROWS);
    for i in 0..ROWS {
        let mut l = match calls.get(i) {
            Some(u) => {
                let tool = clip(&u.tool, 9);
                let arg = clip(&u.arg, LW.saturating_sub(21));
                vec![
                    sp(format!(" {}", clk(u.time)), D),
                    sp(format!("  {}", pad(&tool, 10)), G),
                    sp(pad(&arg, LW.saturating_sub(21)), D),
                ]
            }
            None if i == 0 => vec![sp(pad(" no tool calls yet", LW), D)],
            None => vec![sp(" ".repeat(LW), D)],
        };
        l.push(sp(" │ ", VD));
        if let Some(r) = result.get(i) {
            let last = i == ROWS - 1 && more;
            let text = if last { clip(&format!("{r} …"), rw) } else { clip(r, rw) };
            l.push(sp(text, if a.result.is_some() && done { B } else { D }));
        }
        out.push(Line::from(l));
    }
    out
}

/// Left spans, then right spans flush to width `w`.
fn spread(left: Vec<Span<'static>>, right: Vec<Span<'static>>, w: i64) -> Line<'static> {
    let n = |v: &[Span]| v.iter().map(|s| clen(&s.content)).sum::<usize>();
    let gap = (w as usize).saturating_sub(n(&left) + n(&right) + 1).max(1);
    let mut v = left;
    v.push(sp(" ".repeat(gap), D));
    v.extend(right);
    Line::from(v)
}

/// Truncate to `n` chars with an ellipsis.
pub(crate) fn clip(s: &str, n: usize) -> String {
    if clen(s) <= n {
        s.to_string()
    } else {
        format!("{}…", ctake(s, n.saturating_sub(1)))
    }
}

/// "running 45s", "running 2m", "running 1h"
pub(crate) fn running_label(ms: i64) -> String {
    format!("running {}", short_dur(ms))
}

/// "45s", "2m", "1h"
fn short_dur(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m", s / 60)
    } else {
        format!("{}h", s / 3600)
    }
}

/// "idle 45s", "idle 2m", "idle 1h"
pub(crate) fn idle_label(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        format!("idle {s}s")
    } else if s < 3600 {
        format!("idle {}m", s / 60)
    } else {
        format!("idle {}h", s / 3600)
    }
}

/// Rarity glyph color: legendary in the accent, rare bright, common dim.
fn rarity_color(model: &str, dim: bool) -> Color {
    match rarity(model).map(|r| r.1) {
        _ if dim => VD,
        Some("legendary") => ac(),
        Some("rare") => G,
        _ => D,
    }
}

/// "✦ " / "◆ " / "· " or two spaces.
fn rarity_span(model: &str, dim: bool) -> Span<'static> {
    match rarity(model) {
        Some((g, _)) => sp(format!("{g} "), rarity_color(model, dim)),
        None => sp("  ", D),
    }
}

/// The face column starts here in a tree row: gutter+branch (2) + 9 cells +
/// " ● " (3) + rarity (2) + name (12) + badge.
const FACE_COL: usize = 2 + 9 + 3 + 2 + 12 + BADGE as usize;

/// Three slow "z"s: each drifts right from the sleepy face, then up into the
/// row above, then fades. Returns (rows up, column, char, color).
fn zzz(t: i64, calm: bool) -> Vec<(usize, usize, char, Color)> {
    let c = FACE_COL + 5; // just past "(-_-)"
    if calm {
        return vec![(0, c, 'z', D)];
    }
    const PERIOD: i64 = 3_000;
    (0..3)
        .filter_map(|k| {
            let p = (t + k * PERIOD / 3).rem_euclid(PERIOD) as f64 / PERIOD as f64;
            match p {
                p if p < 0.25 => Some((0, c, 'z', VD)),
                p if p < 0.5 => Some((0, c + 1, 'z', D)),
                p if p < 0.85 => Some((1, c + 2, 'z', G)),
                _ => None,
            }
        })
        .collect()
}

/// Put `ch` at column `col` of `line` if that cell is blank (never covers text).
fn overlay(line: &mut Line<'static>, col: usize, ch: char, c: Color) {
    let mut cells: Vec<(char, Style)> = line
        .spans
        .iter()
        .flat_map(|s| s.content.chars().map(move |ch| (ch, s.style)))
        .collect();
    while cells.len() <= col {
        cells.push((' ', Style::default()));
    }
    if cells[col].0 != ' ' {
        return;
    }
    cells[col] = (ch, Style::default().fg(c));
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut cur = String::new();
    let mut style = cells[0].1;
    for (ch, st) in cells {
        if st != style && !cur.is_empty() {
            spans.push(Span::styled(std::mem::take(&mut cur), style));
        }
        style = st;
        cur.push(ch);
    }
    spans.push(Span::styled(cur, style));
    line.spans = spans;
}

/// "45s", "2m05s", "1h02m"
fn dur(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs / 60) % 60)
    }
}

/// `l`: lifetime totals across sessions (needs persistence).
fn lifetime_panel(view: &View, w: i64) -> Vec<Line<'static>> {
    let mut out = vec![rule("lifetime", "", w)];
    let Some(lt) = view.lifetime else {
        out.push(Line::from(""));
        out.push(Line::from(sp(" persistence is off", G)));
        out.push(Line::from(sp(" set persistence = true in ~/.agentflow/config.toml", D)));
        return pad_lines(out, 10);
    };
    let kv = |k: &str, v: String, k2: &str, v2: String| {
        Line::from(vec![
            sp(format!(" {}", pad(k, 10)), D),
            sp(pad(&v, 14), B),
            sp(pad(k2, 10), D),
            sp(v2, B),
        ])
    };
    out.push(kv("sessions", lt.sessions.to_string(), "agents", lt.agents.to_string()));
    out.push(if view.cost {
        kv("api cost", approx_cost(lt.cost), "tokens", fmt_tokens(lt.tokens))
    } else {
        kv("tokens", fmt_tokens(lt.tokens), "", String::new())
    });
    out.push(kv(
        "cache",
        if view.cost { format!("saved {}", fmt_cost(lt.cache_saved)) } else { "·".into() },
        "fastest",
        lt.fastest_ms.map_or("·".into(), |ms| format!("{:.1}s", ms as f64 / 1000.0)),
    ));
    out.push(Line::from(""));
    let kinds = [
        stats::Kind::Speedrun,
        stats::Kind::Pacifist,
        stats::Kind::BugHunter,
        stats::Kind::BigSpender,
        stats::Kind::TeamPlayer,
    ];
    let has = |k: stats::Kind| lt.unlocked.iter().any(|u| u == k.title());
    let got = kinds.iter().filter(|k| has(**k)).count();
    out.push(Line::from(vec![
        sp(" achievements  ", D),
        sp(format!("{got}/{}", kinds.len()), B),
        sp(format!("  ·  earned {}×", lt.earned), D),
    ]));
    let mut l = vec![sp(" ", D)];
    for k in kinds {
        l.push(sp(format!("{} ✦  ", k.title()), if has(k) { ac() } else { VD }));
    }
    out.push(Line::from(l));
    pad_lines(out, 10)
}

/// The current achievement popup, if any: "speedrun ✦  scout · 7s". Pops in
/// bright, holds in the accent, fades over its last 750ms. Only achievements
/// earned since agentflow attached are announced.
fn popup_line(sess: &Session, t: i64, app_t: i64, calm: bool) -> Line<'static> {
    let list = stats::unlocks(sess, t);
    let since = t.saturating_sub(app_t);
    let Some((u, age)) = stats::popup(&list, since, t) else {
        return Line::from("");
    };
    let fade = ((1.0 - age) / 0.25).clamp(0.0, 1.0);
    let title_c = if age < 0.06 && !calm { wh() } else { dim(ac(), fade) };
    Line::from(vec![
        sp(format!(" {} ✦", u.kind.title()), title_c),
        sp(format!("  {}", u.detail), dim(G, fade)),
    ])
}

fn footer_line() -> Line<'static> {
    let mut v = vec![sp(" ", D)];
    let hints = [("v", "view"), ("s", "stats"), ("t", "timeline"), ("l", "lifetime"), ("?", "help")];
    for (i, (k, what)) in hints.iter().enumerate() {
        if i > 0 {
            v.push(sp(" · ", VD));
        }
        v.push(sp(*k, ac()));
        v.push(sp(format!(" {what}"), D));
    }
    Line::from(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(lines: &[Line]) -> String {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect::<String>() + "\n")
            .collect()
    }

    /// A long project name is shortened with "…" so the title never runs
    /// into the view keys, cost and clock on the right.
    #[test]
    fn long_titles_shorten_instead_of_colliding() {
        assert_eq!(fit_title("agentflow // codex // demo", 40), "agentflow // codex // demo");
        let t = fit_title("agentflow // codex // agentflow-playground", 34);
        assert_eq!(t, "agentflow // codex // agentflow-p…");
        assert!(clen(&fit_title("agentflow // a-very-long-project-name-indeed // 1a2b", 30)) <= 30);

        let p = crate::pricing::Pricing::default();
        let mut s = crate::demo::session("agentflow // codex // a-really-quite-long-project-directory-name".into(), &p);
        s.length = 1;
        let v = View { calm: true, cost: true, ..View::default() };
        let header = text(&frame(&s, 9_000, 9_000, &v, &mut rand::thread_rng())).lines().next().unwrap().to_string();
        assert!(header.contains("…") && header.contains("v  tree"), "{header}");
        assert!(clen(&header) <= width(&v) as usize, "{header}");
    }

    /// Clock times are real elapsed time (the mockup's 00:02:41 start is gone).
    #[test]
    fn clock_starts_at_zero() {
        assert_eq!(clk(0), "00:00:00");
        assert_eq!(clk(61_500), "00:01:01");
        assert_eq!(clk(-500), "00:00:00");
    }

    /// One agent working for the user: the root reads "you", not orchestrator.
    #[test]
    fn solo_sessions_root_is_you() {
        let p = crate::pricing::Pricing::default();
        let mut s = crate::demo::session("h".into(), &p);
        let v = View { calm: true, ..View::default() };
        let mut rng = rand::thread_rng();
        assert!(text(&frame(&s, 9_000, 9_000, &v, &mut rng)).contains("orchestrator"));
        s.solo = true;
        s.events.clear(); // the demo's scripted log names its orchestrator
        for panel in [Panel::Main, Panel::Orbit, Panel::Timeline] {
            let txt = text(&frame(&s, 9_000, 9_000, &View { panel, ..v }, &mut rng));
            assert!(txt.contains("you") && !txt.contains("orchestrator"), "{panel:?}: {txt}");
        }
    }

    /// Live clock can read slightly negative (transcript timestamps ahead of the
    /// local clock). With agents still running `all` is i64::MAX, and the
    /// finish-wave math used to compute `t - all` unconditionally: an overflow
    /// panic in debug builds.
    #[test]
    fn negative_clock_on_running_session_does_not_overflow() {
        let s = crate::demo::synth("h".into());
        assert_eq!(s.all, i64::MAX);
        let mut rng = rand::thread_rng();
        for panel in [Panel::Main, Panel::Stats, Panel::Timeline, Panel::Lifetime] {
            let v = View { calm: true, cost: true, panel, ..View::default() };
            let _ = frame(&s, -5, 0, &v, &mut rng);
            let _ = frame(&s, i64::MIN / 2, 0, &v, &mut rng);
        }
    }

    /// Every panel renders exactly H lines (the orbit: its grid plus chrome)
    /// and stays within the frame width.
    #[test]
    fn panels_fit_the_frame() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("agentflow // demo // abcd".into(), &p);
        let totals = crate::lifetime::Totals::default();
        let vals = vec!["test-runner".to_string(), "reviewer".to_string()];
        let mut rng = rand::thread_rng();
        for cost in [false, true] {
            for panel in [Panel::Main, Panel::Orbit, Panel::Stats, Panel::Timeline, Panel::Lifetime, Panel::Detail] {
                for t in (0..s.length).step_by(250) {
                    let v = View {
                        calm: t % 500 == 0,
                        cost,
                        flashes: cost,
                        achievements: true,
                        recap: recap_due(&s, t),
                        panel,
                        footer: t < 3000,
                        lifetime: Some(&totals),
                        selected: Some((t / 250) as usize % 3),
                        // tiny threshold so sleepy rows (and their z's) get exercised
                        stuck_ms: if t % 1000 == 0 { 300 } else { 120_000 },
                        base: if panel == Panel::Orbit { Panel::Orbit } else { Panel::Main },
                        term: (0, 0),
                        validators: &vals,
                    };
                    let lines = frame(&s, t, t, &v, &mut rng);
                    let want = if panel == Panel::Orbit { 2 + crate::orbit::GRID_H + 3 } else { H as usize };
                    assert_eq!(lines.len(), want, "{panel:?} t={t}");
                    for l in &lines {
                        let n = l.spans.iter().map(|s| s.content.chars().count()).sum::<usize>();
                        assert!(n as i64 <= width(&v), "{panel:?} t={t}: {n} > {}", width(&v));
                    }
                }
            }
        }
    }

    #[test]
    fn demo_shows_costs_badges_popups_and_recap() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("agentflow // demo // abcd".into(), &p);
        let mut rng = rand::thread_rng();
        let v = View { calm: true, cost: true, flashes: true, achievements: true, ..View::default() };
        let mid = text(&frame(&s, 4000, 4000, &v, &mut rng));
        assert!(mid.contains("haiku") && mid.contains("opus") && mid.contains("sonnet"), "{mid}");
        // scout, researcher and the first test-runner start together
        let team = text(&frame(&s, 2000, 2000, &v, &mut rng));
        assert!(team.contains("team player ✦"), "{team}");
        assert!(mid.contains('$'), "header total / cost column");
        // the fixer's pytest fails at 9.5s, the test-runner's passes at 12.4s
        let hunt = text(&frame(&s, 12_600, 12_600, &v, &mut rng));
        assert!(hunt.contains("bug hunter ✦"), "{hunt}");
        let end = text(&frame(&s, 13_000, 13_000, &View { recap: true, ..v }, &mut rng));
        assert!(end.contains("── recap") && end.contains("mvp") && end.contains("saved $"), "{end}");
        let off = text(&frame(&s, 6000, 6000, &View { achievements: false, cost: false, ..v }, &mut rng));
        assert!(!off.contains('$') && !off.contains("bug hunter"));
    }

    #[test]
    fn rarity_and_selection_marker() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("h".into(), &p);
        let mut rng = rand::thread_rng();
        let v = View { calm: true, selected: Some(1), stuck_ms: 120_000, ..View::default() };
        let lines = frame(&s, 4000, 4000, &v, &mut rng);
        let rows: Vec<String> = text(&lines[5..8]).lines().map(str::to_string).collect();
        assert!(rows[0].contains("· scout"), "haiku is common: {}", rows[0]);
        assert!(rows[1].starts_with('›') && rows[1].contains("✦ researcher"), "{}", rows[1]);
        assert!(rows[2].starts_with(' ') && rows[2].contains("◆ test-runner"), "{}", rows[2]);
    }

    /// A live agent silent past the threshold dozes: sleepy face, amber
    /// "idle Nm", z's drifting into the row above. Activity wakes it.
    #[test]
    fn stuck_agent_dozes_and_wakes() {
        let mut s = crate::demo::synth("h".into());
        s.agents.truncate(1);
        let a = &mut s.agents[0];
        a.wait = None;
        a.last_seen = Some(10_000);
        let mut rng = rand::thread_rng();
        let v = View { calm: false, stuck_ms: 120_000, ..View::default() };
        let t = 10_000 + 150_000;
        let lines = frame(&s, t, t, &v, &mut rng);
        let txt = text(&lines);
        assert!(txt.contains("(-_-)") && txt.contains("idle 2m"), "{txt}");
        let status = lines[5].spans.iter().find(|sp| sp.content.contains("idle")).unwrap();
        assert_eq!(status.style.fg, Some(AM));
        // over a full cycle the z's show up on its row and the row above
        let mut own = false;
        let mut above = false;
        for dt in (0..3_000).step_by(100) {
            let l = frame(&s, t + dt, t + dt, &v, &mut rng);
            own |= text(&l[5..6]).chars().skip(FACE_COL + 5).take(2).any(|c| c == 'z');
            above |= text(&l[4..5]).chars().nth(FACE_COL + 7) == Some('z');
        }
        assert!(own && above);

        // activity resumes: awake again
        s.agents[0].last_seen = Some(t - 1_000);
        let txt = text(&frame(&s, t, t, &v, &mut rng));
        assert!(!txt.contains("(-_-)") && !txt.contains("idle"), "{txt}");
        // threshold 0 turns it off
        s.agents[0].last_seen = Some(10_000);
        let txt = text(&frame(&s, t, t, &View { stuck_ms: 0, ..v }, &mut rng));
        assert!(!txt.contains("idle"));
    }

    #[test]
    fn detail_panel_shows_the_selected_agent() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("h".into(), &p);
        let mut rng = rand::thread_rng();
        let v = View { calm: true, cost: true, panel: Panel::Detail, selected: Some(1), ..View::default() };
        let txt = text(&frame(&s, 13_000, 13_000, &v, &mut rng));
        assert!(txt.contains("✦ researcher") && txt.contains("opus · legendary · researcher"), "{txt}");
        assert!(txt.contains("compare braille sparkline approaches"), "task: {txt}");
        assert!(txt.contains("webfetch") && txt.contains("docs.rs"), "tool calls: {txt}");
        assert!(txt.contains("Two ways to draw sparklines"), "result preview: {txt}");
        assert!(txt.contains("done") && txt.contains('$'));
        // still running: no result yet
        let txt = text(&frame(&s, 4_000, 4_000, &v, &mut rng));
        assert!(txt.contains("(still working…)"), "{txt}");
    }

    #[test]
    fn swimlanes_have_every_lane_an_axis_and_peak() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("h".into(), &p);
        let mut rng = rand::thread_rng();
        let v = View { calm: true, panel: Panel::Timeline, stuck_ms: 120_000, ..View::default() };
        let txt = text(&frame(&s, 14_000, 14_000, &v, &mut rng));
        for lane in ["◆ orchestrator", "· scout", "✦ researcher", "◆ test-runner", "◆ reviewer", "◆ fixer", "· test-runner"] {
            assert!(txt.contains(lane), "{lane}: {txt}");
        }
        assert!(txt.contains("0s") && txt.contains("39s"), "axis: {txt}");
        assert!(txt.contains("parallelism  peak 3 agents"), "{txt}");
        assert!(txt.contains("avg 1.4×") && txt.contains("2+ active 28%"), "{txt}");
        // 6 agents × ×3 demo time: 49s of work in the 36s from first dispatch to last end
        assert!(txt.contains("49s of work in 36s · saved 13s"), "{txt}");
        assert!(txt.contains("⣿ critical path"), "legend: {txt}");
        // the researcher's permission wait is drawn dim; the critical path
        // (researcher → reviewer → fixer → test-runner) is brighter mauve
        let lines = frame(&s, 14_000, 14_000, &v, &mut rng);
        let cells = |name: &str, nth: usize| -> Vec<Option<Color>> {
            let lane = lines.iter().filter(|l| text(std::slice::from_ref(l)).contains(name)).nth(nth).unwrap();
            lane.spans.iter().filter(|sp| sp.content.contains(['⣀', '⣄', '⣤', '⣦', '⣶', '⣷', '⣿', '⡀'])).map(|sp| sp.style.fg).collect()
        };
        let researcher = cells("researcher", 0);
        assert!(researcher.contains(&Some(D)) && researcher.contains(&Some(light())));
        assert!(!cells("scout", 0).contains(&Some(light())) && cells("scout", 0).contains(&Some(ac())));
        assert!(cells("reviewer", 0).contains(&Some(light())) && cells("fixer", 0).contains(&Some(light())));
        assert!(!cells("test-runner", 0).contains(&Some(light())), "the first test-runner ran in parallel");
        assert!(cells("test-runner", 1).contains(&Some(light())), "the validating one closed the session");
    }

    /// The orbit: header switcher with "orbit" in the accent, the ring with
    /// the reviewer → fixer → test-runner handoff chain, both validator
    /// verdicts, and the latest line.
    #[test]
    fn orbit_view_shows_the_handoff_chain_and_verdicts() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("h".into(), &p);
        let vals = vec!["test-runner".to_string(), "reviewer".to_string()];
        let mut rng = rand::thread_rng();
        let v = View { calm: true, panel: Panel::Orbit, base: Panel::Orbit, validators: &vals, ..View::default() };
        let at = |t: i64, rng: &mut rand::rngs::ThreadRng| text(&frame(&s, t, t, &v, rng));

        let lines = frame(&s, 3000, 3000, &v, &mut rng);
        let hdr = &lines[0];
        let orbit = hdr.spans.iter().find(|sp| sp.content == "orbit").expect("switcher");
        assert_eq!(orbit.style.fg, Some(ac()));
        let tree = hdr.spans.iter().find(|sp| sp.content == "tree").unwrap();
        assert_eq!(tree.style.fg, Some(DIM));
        let txt = text(&lines);
        assert!(txt.contains("orchestrator") && txt.contains('◆'), "{txt}");
        for name in ["scout", "researcher", "reviewer", "fixer", "test-runner"] {
            assert!(txt.contains(name), "{name}: {txt}");
        }
        assert!(txt.contains("validator"), "idle validators say so: {txt}");

        let txt = at(6000, &mut rng);
        assert!(txt.contains("validating"), "{txt}");
        let txt = at(8200, &mut rng);
        assert!(txt.contains("✗ fail"), "the reviewer rejects: {txt}");
        assert!(txt.contains("latest  reviewer → fixer") && txt.contains("findings 1.4kb"), "{txt}");
        let txt = at(11000, &mut rng);
        assert!(txt.contains("fixer → test-runner") && txt.contains("patch 0.9kb"), "{txt}");
        let txt = at(13000, &mut rng);
        assert!(txt.contains("✓ pass") && txt.contains("test-runner → orchestrator"), "{txt}");
        assert!(txt.contains("✓ validated"), "{txt}");
        let txt = at(14000, &mut rng);
        assert!(txt.contains("all agents done ✓"), "{txt}");

        // no validators configured: plain agents
        let none: Vec<String> = Vec::new();
        let plain = View { validators: &none, ..v };
        let txt = text(&frame(&s, 13000, 13000, &plain, &mut rng));
        assert!(!txt.contains("✓ pass") && !txt.contains("✗ fail") && txt.contains("done ✓"), "{txt}");
    }

    #[test]
    fn orbit_falls_back_to_the_tree_in_a_small_terminal() {
        let p = crate::pricing::Pricing::default();
        let s = crate::demo::session("h".into(), &p);
        let mut rng = rand::thread_rng();
        let small = View { calm: true, panel: Panel::Orbit, base: Panel::Orbit, term: (60, 14), ..View::default() };
        let lines = frame(&s, 7000, 7000, &small, &mut rng);
        assert_eq!(lines.len(), H as usize);
        let txt = text(&lines);
        assert!(txt.contains("├─") && txt.contains("── log"), "tree: {txt}");
        assert!(txt.contains("orbit needs a bigger terminal"), "{txt}");
        // roomy enough: the orbit
        let big = View { term: (100, 30), ..small };
        let txt = text(&frame(&s, 7000, 7000, &big, &mut rng));
        assert!(txt.contains("latest") && !txt.contains("── log"), "{txt}");
    }

    /// The stuck fix: a running agent silent for 2m+ with a tool call still in
    /// flight reads "running 2m" with an awake face and no z's; only with
    /// nothing in flight does it doze.
    #[test]
    fn in_flight_tool_call_runs_instead_of_dozing() {
        let mut s = crate::demo::synth("h".into());
        s.agents.truncate(1);
        let a = &mut s.agents[0];
        a.wait = Some((10_000, i64::MAX)); // a Bash call pending since 10s
        a.last_seen = Some(10_000);
        a.in_flight = Some(10_000);
        let mut rng = rand::thread_rng();
        let t = 10_000 + 150_000;
        for panel in [Panel::Main, Panel::Orbit, Panel::Detail] {
            let v = View { calm: false, stuck_ms: 120_000, panel, base: panel, selected: Some(0), ..View::default() };
            let txt = text(&frame(&s, t, t, &v, &mut rng));
            assert!(txt.contains("running 2m"), "{panel:?}: {txt}");
            assert!(!txt.contains("(-_-)") && !txt.contains("idle") && !txt.contains('z'), "{panel:?}: {txt}");
        }
        // the slow spinner: steps every 400ms, not every 80ms
        let v = View { calm: true, stuck_ms: 120_000, ..View::default() };
        let spin = |t: i64, rng: &mut rand::rngs::ThreadRng| {
            text(&frame(&s, t, t, &v, rng)).lines().nth(5).unwrap().chars().find(|c| SPIN.contains(c))
        };
        let t0 = t - t.rem_euclid(400);
        assert_eq!(spin(t0, &mut rng), spin(t0 + 300, &mut rng));
        assert_ne!(spin(t0, &mut rng), spin(t0 + 400, &mut rng));
        // the result comes back and it stays quiet: now it dozes
        s.agents[0].in_flight = None;
        s.agents[0].wait = None;
        let txt = text(&frame(&s, t, t, &v, &mut rng));
        assert!(txt.contains("(-_-)") && txt.contains("idle 2m"), "{txt}");
    }

    /// The stats panel's team line: parallelism, plus work vs wall-clock
    /// when it fits the width.
    #[test]
    fn stats_panel_shows_parallelism() {
        let p = crate::pricing::Pricing::default();
        let mut s = crate::demo::session("h".into(), &p);
        let mut rng = rand::thread_rng();
        let v = View { calm: true, cost: true, panel: Panel::Stats, ..View::default() };
        let txt = text(&frame(&s, 14_000, 14_000, &v, &mut rng));
        assert!(txt.contains("team   peak 3 agents · avg 1.4× · 2+ active 28% · 49s of work in 36s"), "{txt}");
        // a long session at the narrow width: the tail is dropped, not clipped
        for a in &mut s.agents {
            a.spawn *= 400;
            a.finish = a.finish.map(|f| f * 400);
        }
        let narrow = View { cost: false, ..v };
        let t = 14_000 * 400;
        let lines = frame(&s, t, t, &narrow, &mut rng);
        let team = lines.iter().map(|l| text(std::slice::from_ref(l))).find(|l| l.contains("team")).unwrap();
        assert!(team.trim_end().chars().count() as i64 <= width(&narrow) && !team.contains("of work"), "{team}");
    }

    #[test]
    fn overlay_only_fills_blanks() {
        let mut l = Line::from(vec![sp("ab", B), sp("  ", D)]);
        overlay(&mut l, 1, 'z', G);
        overlay(&mut l, 3, 'z', G);
        overlay(&mut l, 6, 'z', G);
        assert_eq!(text(&[l]), "ab z  z\n");
    }
}
