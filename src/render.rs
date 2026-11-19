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

use crate::model::{Agent, Session, Tool, ToolKind};
use crate::theme::*;

/// Logical content width (matches the mockup's W=64).
pub const W: i64 = 64;

const SPIN: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

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
        let mut r = s.to_string();
        r.extend(std::iter::repeat(' ').take(n - len));
        r
    }
}

/// Left-pad with spaces to `n` chars. Mirrors JS `lpad`.
fn lpad(s: &str, n: usize) -> String {
    let len = clen(s);
    if len >= n {
        s.to_string()
    } else {
        let mut r = String::new();
        r.extend(std::iter::repeat(' ').take(n - len));
        r.push_str(s);
        r
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
    let s = 161 + ms / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
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

/// Build one subagent row. Port of `row(a,t,last)`, generalized for live mode.
fn row(
    a: &Agent,
    t: i64,
    last: bool,
    calm: bool,
    live: bool,
    rng: &mut impl Rng,
) -> Vec<Span<'static>> {
    let r = t - a.spawn;
    let act = a.spawn + 1300;
    let fo = a.finish; // Option: None = still running
    let mut s: Vec<Span<'static>> = Vec::new();

    s.push(sp(if last { " └" } else { " ├" }, D));

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
        bc[i as usize] = if r < 400 { AC } else { D };
    }
    let setcell = |br: &mut [char; 9], bc: &mut [Color; 9], i: i64, ch: char| {
        if (0..9).contains(&i) {
            br[i as usize] = ch;
            bc[i as usize] = AC;
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
    let wait = a.wait.map_or(false, |(ws, we)| t >= ws && t < we);
    let boot = r < 1300;

    // Node marker.
    let mut mk = "●";
    let mut mc = AC;
    if r < 900 {
        mc = WH;
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
    s.push(sp(pad(&ctake(&a.name, shown), 12), if dim { D } else { B }));

    // Current tool: real (live) or time-cycled (demo). May be absent early live.
    let tl: Option<&Tool> = if a.tools.is_empty() {
        None
    } else if let Some(i) = a.live_tool {
        Some(&a.tools[i.min(a.tools.len() - 1)])
    } else {
        let k = ((t - act).max(0) / 1600) as usize;
        Some(&a.tools[k % a.tools.len()])
    };

    // Face reacts to phase and tool kind.
    let face;
    let mut fc = G;
    if a.error {
        face = "(×_×)".into();
        fc = RED;
    } else if boot {
        face = if r < 1100 { "(·_·)" } else { "(•_•)" }.to_string();
    } else if done {
        face = "(^‿^)".into();
        fc = if dim { D } else { AC };
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
        st = "error".into();
        sc = RED;
    } else if boot {
        let stage = ((r - 800) / 170).clamp(0, 2) as usize;
        st = glitch(["init", "link", "ready"][stage], calm, rng);
        sc = AC;
    } else if wait {
        // Live mode can't tell *why* a call is blocked, so it says "waiting";
        // the scripted demo knows its wait is a permission prompt.
        let word = if live { "waiting" } else { "permission" };
        st = format!("{}{word}", if (t / 300) % 2 != 0 { "? " } else { "  " });
        sc = AM;
    } else if ret {
        st = "returning".into();
        sc = AC;
    } else if done {
        let f = fo.unwrap();
        let shown = ((t - f - 400) / 60).max(0) as usize;
        st = ctake("done ✓", shown);
        sc = if dim { D } else { AC };
    } else {
        st = tl.map(|x| x.name.clone()).unwrap_or_else(|| "working".into());
    }
    s.push(sp(pad(&st, 12), sc));

    // Spinner / sparkle cell.
    let mut s1 = " ".to_string();
    if !boot && !done && !wait && !ret && !a.error {
        s1 = SPIN[((t / 80 + a.bob).rem_euclid(10)) as usize].to_string();
    }
    if let Some(f) = fo {
        if t >= f + 700 && t < f + 1100 {
            let i = ((t - f - 700) / 134).clamp(0, 2) as usize;
            s1 = ['·', '✦', '·'][i].to_string();
        }
    }
    s.push(sp(s1, AC));

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
    let bar_done = if dim { D } else if a.error { RED } else { AC };
    for i in 0..8 {
        if i < cells {
            s.push(sp("⣿", if i == shine { WH } else { bar_done }));
        } else if i == cells && !boot && !wait && !a.error {
            // Shimmering leading cell — steady when calm.
            if calm {
                s.push(sp("⣀", VD));
            } else {
                let on = (t / 200) % 2 != 0;
                s.push(sp(if on { "⣷" } else { "⣀" }, if on { SHIM } else { VD }));
            }
        } else {
            s.push(sp("⣀", VD));
        }
    }

    s
}

/// Render a full frame (16 logical lines).
pub fn frame(
    sess: &Session,
    t: i64,
    app_t: i64,
    calm: bool,
    rng: &mut impl Rng,
) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    let all = sess.all;
    let live = sess.live;

    // --- Header: title types in with a blinking cursor; "linking… ok" then clock.
    let ttl = &sess.header;
    let shown = ctake(ttl, (app_t / 16).max(0) as usize);
    let cur = if clen(&shown) < clen(ttl) && (app_t / 120) % 2 != 0 {
        "█"
    } else {
        ""
    };
    let rt = if app_t < 700 {
        String::new()
    } else if app_t < 1100 {
        "linking…".into()
    } else if app_t < 1500 {
        "linking… ok".into()
    } else {
        clk(t)
    };
    let lt = format!(" {}{}", shown, cur);
    let rt_len = clen(&rt);
    let mid_w = (W as usize).saturating_sub(10).saturating_sub(rt_len);
    out.push(Line::from(vec![
        sp(ctake(&lt, 10), AC),
        sp(pad(&cfrom(&lt, 10), mid_w), G),
        sp(rt.clone(), if rt.starts_with("link") { AC } else { D }),
    ]));

    // --- Top rule draws left to right.
    let rn = if app_t < 600 {
        (app_t as f64 / 600.0 * W as f64).floor() as i64
    } else {
        W
    };
    let dashes = "─".repeat((rn - 1).max(0) as usize);
    out.push(Line::from(sp(
        format!(" {}", dashes),
        if app_t < 600 { AC } else { D },
    )));

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
    let mut o = vec![
        sp(" ◆ ", AC),
        sp("orchestrator  ", B),
        sp(pad(of, 8), if t >= all || fl.is_some() { AC } else { G }),
        sp(pad(&os, 14), if t >= all { AC } else { G }),
    ];
    if let Some(fl) = fl {
        let f = fl.finish.unwrap();
        o.push(sp(
            format!("+{}", fl.size),
            if t - f - 400 < 600 { AC } else { D },
        ));
    }
    out.push(Line::from(o));

    // --- Tree of up to 3 visible agents (most-recent spawns when more exist).
    let mut vis: Vec<&Agent> = sess.agents.iter().filter(|a| t >= a.spawn).collect();
    if vis.len() > 3 {
        let start = vis.len() - 3;
        vis = vis.split_off(start);
    }
    out.push(if vis.is_empty() {
        Line::from("")
    } else {
        Line::from(sp(" │", D))
    });
    for i in 0..3 {
        if let Some(a) = vis.get(i) {
            out.push(Line::from(row(a, t, i == vis.len() - 1, calm, live, rng)));
        } else {
            out.push(Line::from(""));
        }
    }

    out.push(Line::from(""));

    // --- Log of the last 4 events.
    out.push(Line::from(sp(
        format!(" ── log {}", "─".repeat((W - 8) as usize)),
        D,
    )));
    let visible: Vec<&crate::model::Event> =
        sess.events.iter().filter(|e| e.time <= t).collect();
    let start = visible.len().saturating_sub(4);
    let tail = &visible[start..];
    for i in 0..4 {
        if let Some(e) = tail.get(i) {
            let detail_color = if e.detail.starts_with('?') {
                AM
            } else if e.color == AC {
                AC
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

    out.push(Line::from(""));

    // --- Bottom rule, with a cyan wave that ripples across when all are done.
    let mut bl = vec![sp(" ", D)];
    for i in 1..W {
        let x = (t - all) as f64 / 900.0 * W as f64;
        let on = !calm && t >= all && t < all + 900 && (i as f64 - x).abs() < 4.0;
        bl.push(sp(if on { "━" } else { "─" }, if on { AC } else { D }));
    }
    out.push(Line::from(bl));

    out
}
