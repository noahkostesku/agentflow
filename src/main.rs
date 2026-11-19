//! agentflow — a full-screen terminal visualizer for Claude Code multi-agent
//! sessions. `--demo` replays the scripted mockup; default watches a live
//! session's transcripts.

mod adapters;
mod config;
mod demo;
mod discovery;
mod lifetime;
mod live;
mod model;
mod orbit;
mod pricing;
mod render;
mod schema;
mod stats;
mod theme;
mod wrapped;

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use config::Config;
use pricing::Pricing;
use render::{Panel, View};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// How long the key-hint footer stays up (on startup and after `?`).
const HINT: Duration = Duration::from_secs(3);

#[derive(Parser, Debug)]
#[command(
    name = "agentflow",
    about = "A cute cyberpunk terminal visualizer for Claude Code multi-agent sessions",
    version
)]
struct Cli {
    /// Replay the scripted demo session on a loop.
    #[arg(long)]
    demo: bool,

    /// Disable glitch, shimmer, and the finish wave (steady, low-motion).
    #[arg(long)]
    calm: bool,

    /// Watch a specific session id instead of auto-detecting.
    #[arg(long, value_name = "ID")]
    session: Option<String>,

    /// Watch the session for a specific project path (default: cwd).
    #[arg(long, value_name = "PATH")]
    project: Option<String>,

    /// Turn off achievement popups (overrides config).
    #[arg(long)]
    no_achievements: bool,

    /// Turn off the recap card (overrides config).
    #[arg(long)]
    no_recap: bool,

    /// Hide costs: per-agent column, header total, and flashes (overrides config).
    #[arg(long)]
    no_cost: bool,

    /// Debug: render a single frame at the given ms and print it, then exit.
    #[arg(long, value_name = "MS", hide = true)]
    frame: Option<i64>,

    /// Debug: render N frames across the demo loop to stdout, then exit.
    #[arg(long, value_name = "N", hide = true)]
    probe: Option<usize>,

    /// Debug: render a synthetic live frame (active + just-finished) at the given ms.
    #[arg(long, value_name = "MS", hide = true)]
    synth: Option<i64>,

    /// Debug: panel for --frame / --probe (main, orbit, stats, timeline, lifetime, detail).
    #[arg(long, value_name = "NAME", hide = true)]
    panel: Option<String>,

    /// Debug: selected agent index for --frame / --probe.
    #[arg(long, value_name = "N", hide = true)]
    select: Option<usize>,

    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Your agents, wrapped: seven animated slides from your lifetime history
    /// (space / → next, ← back, q quit).
    Wrapped {
        /// Only the last 7 days.
        #[arg(long, conflicts_with_all = ["month", "all"])]
        week: bool,
        /// Only the last 30 days (the default).
        #[arg(long, conflicts_with = "all")]
        month: bool,
        /// Everything on record.
        #[arg(long)]
        all: bool,
        /// Write the outro card as an SVG (1200×675, sized for X) and exit.
        #[arg(long, value_name = "FILE")]
        export: Option<PathBuf>,
        /// Debug: print slide N (1–7) at --at ms, then exit.
        #[arg(long, value_name = "N", hide = true)]
        slide: Option<usize>,
        /// Debug: animation time for --slide.
        #[arg(long, value_name = "MS", hide = true, default_value_t = 10_000)]
        at: i64,
    },
}

/// Settings resolved from config + CLI.
struct Settings {
    cfg: Config,
    pricing: Pricing,
}

/// Validators are opt-in; the demo shows its own gates when none are set.
static DEMO_VALIDATORS: std::sync::LazyLock<Vec<String>> =
    std::sync::LazyLock::new(|| demo::VALIDATORS.iter().map(|v| v.to_string()).collect());

impl Settings {
    fn view<'a>(&'a self, calm: bool, panel: Panel, lifetime: Option<&'a lifetime::Totals>) -> View<'a> {
        View {
            calm,
            cost: self.cfg.cost,
            flashes: self.cfg.cost && self.cfg.flashes,
            achievements: self.cfg.achievements,
            recap: false,
            panel,
            footer: false,
            lifetime: if self.cfg.persistence { lifetime } else { None },
            selected: None,
            stuck_ms: self.cfg.stuck_after.saturating_mul(1000).min(i64::MAX as u64) as i64,
            base: if render::VIEWS.iter().any(|(p, _)| *p == panel) { panel } else { Panel::Main },
            term: (0, 0),
            validators: &self.cfg.validators,
        }
    }

    /// The configured validators, or the demo's when replaying the scripted
    /// session with none configured.
    fn validators(&self, sess: &model::Session) -> &[String] {
        if !sess.live && self.cfg.validators.is_empty() {
            &DEMO_VALIDATORS
        } else {
            &self.cfg.validators
        }
    }
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let mut rng = rand::thread_rng();

    let (mut cfg, warning) = Config::load_or_create(&config::dir());
    cfg.achievements &= !cli.no_achievements;
    cfg.recap &= !cli.no_recap;
    cfg.cost &= !cli.no_cost;
    theme::set_accent(cfg.accent_rgb());
    let set = Settings { pricing: Pricing::new(&cfg.pricing), cfg };
    if let Some(w) = &warning {
        eprintln!("agentflow: {w}");
    }

    // `--project` as Claude Code sees it: absolute, symlinks resolved, no
    // trailing slash (it names the storage dir after the real cwd).
    let project = cli.project.as_deref().map(discovery::normalize_project);
    let cwd = project
        .clone()
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    if let Some(Cmd::Wrapped { week, all, export, slide, at, .. }) = &cli.cmd {
        let range = if *week {
            wrapped::Range::Week
        } else if *all {
            wrapped::Range::All
        } else {
            wrapped::Range::Month
        };
        return run_wrapped(range, export.as_deref(), slide.map(|n| (n, *at)));
    }

    let panel = match cli.panel.as_deref() {
        Some("stats") => Panel::Stats,
        Some("timeline") => Panel::Timeline,
        Some("lifetime") => Panel::Lifetime,
        Some("detail") => Panel::Detail,
        Some("orbit") => Panel::Orbit,
        _ => Panel::Main,
    };
    let store = lifetime::Store::load(&lifetime::path());
    let totals = store.totals();
    // Debug frames: show what the UI would at that moment.
    let debug_view = |sess: &model::Session, t: i64, app_t: i64| View {
        recap: set.cfg.recap && render::recap_due(sess, t),
        footer: app_t < HINT.as_millis() as i64,
        selected: cli.select,
        validators: set.validators(sess),
        ..set.view(cli.calm, panel, Some(&totals))
    };

    if let Some(ms) = cli.synth {
        let sess = demo::synth(header_for(&cwd));
        print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
        return Ok(());
    }

    // --- Demo mode -------------------------------------------------------
    if cli.demo {
        let sess = demo::session(header_for(&cwd), &set.pricing);
        if let Some(ms) = cli.frame {
            print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
            return Ok(());
        }
        if let Some(n) = cli.probe {
            for i in 0..n {
                let ms = (sess.length * i as i64) / n.max(1) as i64;
                println!("--- t={ms}ms ---");
                print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
                println!();
            }
            return Ok(());
        }
        return finish(run_demo(&sess, &set, cli.calm), warning);
    }

    // --- Live mode -------------------------------------------------------
    let resolved = discovery::resolve(cli.session.as_deref(), project.as_deref());
    match resolved {
        Some(r) => {
            let live = open_live(&r, &cwd);
            if let Some(ms) = cli.frame {
                let sess = live.build(&set.pricing);
                let t = live.clock();
                print_ansi(&render::frame(&sess, t, ms, &debug_view(&sess, t, ms), &mut rng));
                return Ok(());
            }
            // Follow newer sessions unless pinned to one with --session.
            let follow = cli.session.is_none().then(|| Follow {
                project: project.clone(),
                cwd: cwd.clone(),
                fell_back: r.fell_back,
            });
            finish(run_live(live, &set, store, cli.calm, follow), warning)
        }
        None if cli.session.is_some() => {
            eprintln!(
                "agentflow: no session \"{}\" found under ~/.claude/projects",
                cli.session.as_deref().unwrap_or_default()
            );
            std::process::exit(1);
        }
        None => {
            // Nothing to watch anywhere — fall back to the demo so the tool
            // still shows something recognizable.
            let sess = demo::session(header_for(&cwd), &set.pricing);
            if let Some(ms) = cli.frame {
                print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
                return Ok(());
            }
            finish(run_demo(&sess, &set, cli.calm), warning)
        }
    }
}

/// Repeat a config warning after the TUI exits (the alternate screen hid it).
fn finish(res: io::Result<()>, warning: Option<String>) -> io::Result<()> {
    if let Some(w) = warning {
        eprintln!("agentflow: {w}");
    }
    res
}

/// Attach to a resolved session and read what's already on disk. The header
/// uses the real cwd the session reported (handles --session and the
/// fallback, where the storage dir name is path-encoded).
fn open_live(r: &discovery::Resolved, cwd: &Path) -> live::Live {
    let mut live = live::Live::new(r, String::new());
    live.refresh();
    let hdr_dir = live
        .cwd()
        .map(PathBuf::from)
        .or_else(|| r.project_dir.clone())
        .unwrap_or_else(|| cwd.to_path_buf());
    live.set_header(header_for(&hdr_dir));
    live
}

/// Auto-switch settings for live mode (absent when pinned with --session).
struct Follow {
    project: Option<PathBuf>,
    cwd: PathBuf,
    fell_back: bool,
}

/// Re-scan for a session that has become newer than the one we're watching.
/// Started in a directory with sessions: only that directory's sessions are
/// candidates. Started on the global fallback: a session appearing in our own
/// directory (or a newer global one) wins. A candidate must have been active
/// at least 5s more recently than the current session, so two sessions that
/// are both busy don't make the view flip back and forth.
fn newer_session(follow: &Follow, current: &Path) -> Option<discovery::Resolved> {
    let cand = discovery::resolve(None, follow.project.as_deref())?;
    if cand.path == current || (!follow.fell_back && cand.fell_back) {
        return None;
    }
    let ahead = discovery::activity(&cand.path)
        .duration_since(discovery::activity(current))
        .unwrap_or_default();
    (ahead >= Duration::from_secs(5)).then_some(cand)
}

/// Build the header "agentflow // <project> // <hash>" from a directory.
fn header_for(dir: &Path) -> String {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("session");
    let hash = short_hash(&dir.to_string_lossy());
    format!("agentflow // {name} // {hash}")
}

/// FNV-1a over the path, folded to 4 lowercase hex chars (stands in for a sid).
fn short_hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:04x}", (h ^ (h >> 32)) as u16)
}

// ---- terminal setup / teardown ---------------------------------------------

fn enter() -> io::Result<Term> {
    // If anything panics, put the terminal back before the message prints,
    // instead of leaving the user's shell in raw mode on the alternate screen.
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(io::stdout(), terminal::LeaveAlternateScreen, cursor::Show);
        default_hook(info);
    }));
    let mut out = io::stdout();
    terminal::enable_raw_mode()?;
    execute!(out, terminal::EnterAlternateScreen, cursor::Hide)?;
    Terminal::new(CrosstermBackend::new(out))
}

fn leave(term: &mut Term) -> io::Result<()> {
    terminal::disable_raw_mode()?;
    execute!(term.backend_mut(), terminal::LeaveAlternateScreen, cursor::Show)
}

/// Terminal (columns, rows), for layouts that adapt to it.
fn term_size(term: &Term) -> (u16, u16) {
    term.size().map_or((0, 0), |s| (s.width, s.height))
}

/// Draw the frame centered in the terminal.
fn paint(term: &mut Term, lines: Vec<Line<'static>>, width: i64) -> io::Result<()> {
    let height = (lines.len() as u16).max(render::H);
    term.draw(|f| {
        let area = f.area();
        let bw = (width as u16).min(area.width);
        let bh = height.min(area.height);
        let x = area.x + area.width.saturating_sub(bw) / 2;
        let y = area.y + area.height.saturating_sub(bh) / 2;
        f.render_widget(Paragraph::new(lines), Rect::new(x, y, bw, bh));
    })?;
    Ok(())
}

// ---- shared UI state (panels, hints, recap) ---------------------------------

struct Ui {
    calm: bool,
    /// what's on screen: a base view, or a panel (stats, detail, …) over it
    panel: Panel,
    /// the view `v` cycles: tree (`Main`) → orbit → timeline
    base: Panel,
    /// when the key hints were last asked for (startup counts)
    hint_at: Instant,
    /// the `all` value whose recap was dismissed, so a later batch of agents
    /// gets its own recap
    recap_dismissed: Option<i64>,
    /// selected agent (index into `Session::agents`)
    selected: Option<usize>,
}

enum Key {
    Quit,
    /// the UI handled it
    Done,
    /// mode-specific key (demo: space/r; live: r)
    Other(KeyCode),
}

impl Ui {
    fn new(calm: bool) -> Self {
        Ui { calm, panel: Panel::Main, base: Panel::Main, hint_at: Instant::now(), recap_dismissed: None, selected: None }
    }

    fn recap_showing(&self, set: &Settings, sess: &model::Session, t: i64) -> bool {
        set.cfg.recap
            && self.panel == Panel::Main
            && render::recap_due(sess, t)
            && self.recap_dismissed != Some(sess.all)
    }

    fn view<'a>(
        &self,
        set: &'a Settings,
        sess: &model::Session,
        t: i64,
        life: Option<&'a lifetime::Totals>,
        term: (u16, u16),
    ) -> View<'a> {
        View {
            recap: self.recap_showing(set, sess, t),
            footer: self.hint_at.elapsed() < HINT,
            selected: self.selected.filter(|i| *i < sess.agents.len()),
            base: self.base,
            term,
            validators: set.validators(sess),
            ..set.view(self.calm, self.panel, life)
        }
    }

    /// Show a base view (tree, orbit or timeline), closing any panel over it.
    fn show(&mut self, base: Panel) {
        self.base = base;
        self.panel = base;
    }

    fn key(&mut self, k: KeyEvent, set: &Settings, sess: &model::Session, t: i64) -> Key {
        let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
        if ctrl_c {
            return Key::Quit;
        }
        // Any key dismisses the recap card (and does nothing else).
        if self.recap_showing(set, sess, t) {
            self.recap_dismissed = Some(sess.all);
            return Key::Done;
        }
        let base = self.base;
        let toggle = |cur: Panel, p: Panel| if cur == p { base } else { p };
        match k.code {
            KeyCode::Char('q') => Key::Quit,
            // Esc backs out one step: a panel → its view → no selection →
            // the tree → quit.
            KeyCode::Esc if self.panel != self.base => {
                self.panel = self.base;
                Key::Done
            }
            KeyCode::Esc if self.selected.is_some() => {
                self.selected = None;
                Key::Done
            }
            KeyCode::Esc if self.base != Panel::Main => {
                self.show(Panel::Main);
                Key::Done
            }
            KeyCode::Esc => Key::Quit,
            KeyCode::Char('v') => {
                let i = render::VIEWS.iter().position(|(p, _)| *p == self.base).unwrap_or(0);
                self.show(render::VIEWS[(i + 1) % render::VIEWS.len()].0);
                Key::Done
            }
            KeyCode::Up | KeyCode::Down => {
                self.step(k.code == KeyCode::Down, sess, t);
                Key::Done
            }
            KeyCode::Enter => {
                if self.selected.is_none() {
                    self.step(true, sess, t);
                }
                if self.selected.is_some() {
                    self.panel = Panel::Detail;
                }
                Key::Done
            }
            KeyCode::Char('c') => {
                self.calm = !self.calm;
                Key::Done
            }
            KeyCode::Char('s') => {
                self.panel = toggle(self.panel, Panel::Stats);
                Key::Done
            }
            KeyCode::Char('t') => {
                self.show(if self.panel == Panel::Timeline { Panel::Main } else { Panel::Timeline });
                Key::Done
            }
            KeyCode::Char('l') => {
                self.panel = toggle(self.panel, Panel::Lifetime);
                Key::Done
            }
            KeyCode::Char('?') | KeyCode::Char('h') => {
                self.hint_at = Instant::now();
                Key::Done
            }
            other => Key::Other(other),
        }
    }
}

impl Ui {
    /// Move the selection through the agents that have started, in spawn
    /// order. With nothing selected, ↓ picks the first and ↑ the last.
    fn step(&mut self, down: bool, sess: &model::Session, t: i64) {
        let started: Vec<usize> = (0..sess.agents.len()).filter(|&i| sess.agents[i].spawn <= t).collect();
        let Some(&last) = started.last() else {
            return;
        };
        let pos = self.selected.and_then(|s| started.iter().position(|&i| i == s));
        self.selected = Some(match (pos, down) {
            (None, true) => started[0],
            (None, false) => last,
            (Some(p), true) => started[(p + 1).min(started.len() - 1)],
            (Some(p), false) => started[p.saturating_sub(1)],
        });
    }
}

/// Next key press within `timeout`, ignoring releases.
fn next_key(timeout: Duration) -> io::Result<Option<KeyEvent>> {
    if event::poll(timeout)? {
        if let Event::Key(k) = event::read()? {
            if k.kind != KeyEventKind::Release {
                return Ok(Some(k));
            }
        }
    }
    Ok(None)
}

// ---- demo loop (looping clock, pause/restart) ------------------------------

fn run_demo(sess: &model::Session, set: &Settings, calm: bool) -> io::Result<()> {
    let mut term = enter()?;
    let mut rng = rand::thread_rng();
    let mut ui = Ui::new(calm);
    let length = sess.length.max(1);
    let mut offset: i64 = 0;
    let mut since = Instant::now();
    let mut running = true;
    let mut last_t = 0;

    let res = (|| -> io::Result<()> {
        loop {
            let t = (offset
                + if running {
                    since.elapsed().as_millis() as i64
                } else {
                    0
                })
                % length;
            if t < last_t {
                ui.recap_dismissed = None; // new loop: the recap comes back
            }
            last_t = t;
            let view = ui.view(set, sess, t, None, term_size(&term));
            paint(&mut term, render::frame(sess, t, t, &view, &mut rng), render::width(&view))?;
            let Some(k) = next_key(Duration::from_millis(50))? else {
                continue;
            };
            match ui.key(k, set, sess, t) {
                Key::Quit => break,
                Key::Done => {}
                Key::Other(KeyCode::Char(' ')) => {
                    if running {
                        offset = (offset + since.elapsed().as_millis() as i64) % length;
                        running = false;
                    } else {
                        since = Instant::now();
                        running = true;
                    }
                }
                Key::Other(KeyCode::Char('r')) => {
                    offset = 0;
                    since = Instant::now();
                    ui.recap_dismissed = None;
                }
                Key::Other(_) => {}
            }
        }
        Ok(())
    })();

    leave(&mut term)?;
    res
}

// ---- live loop (real clock, refresh from disk ~3x/sec) ---------------------

/// Lifetime persistence for live mode: an in-memory store updated on every
/// refresh, written to disk when a batch of agents finishes and on exit.
struct Persist {
    on: bool,
    store: lifetime::Store,
    totals: lifetime::Totals,
    dirty: bool,
}

impl Persist {
    fn update(&mut self, sess: &model::Session, t: i64) {
        if !self.on || !self.store.record(sess, t) {
            return;
        }
        self.totals = self.store.totals();
        self.dirty = true;
        if sess.all != i64::MAX && t >= sess.all {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.on && self.dirty {
            // Best effort: a read-only home just means no lifetime file.
            let _ = self.store.save(&lifetime::path());
            self.dirty = false;
        }
    }
}

fn run_live(
    mut live: live::Live,
    set: &Settings,
    store: lifetime::Store,
    calm: bool,
    mut follow: Option<Follow>,
) -> io::Result<()> {
    let mut term = enter()?;
    let mut rng = rand::thread_rng();
    let mut ui = Ui::new(calm);
    let mut app_start = Instant::now();
    let mut last_refresh = Instant::now();
    let mut last_scan = Instant::now();
    let mut sess = live.build(&set.pricing);
    let totals = store.totals();
    let mut persist = Persist { on: set.cfg.persistence, store, totals, dirty: false };
    persist.update(&sess, live.clock());

    let res = (|| -> io::Result<()> {
        loop {
            // Every ~2s, hop to a newer session (unless pinned by --session).
            if last_scan.elapsed() >= Duration::from_secs(2) {
                last_scan = Instant::now();
                if let Some(fw) = follow.as_mut() {
                    if let Some(next) = newer_session(fw, live.path()) {
                        persist.flush();
                        fw.fell_back = next.fell_back;
                        live = open_live(&next, &fw.cwd);
                        sess = live.build(&set.pricing);
                        app_start = Instant::now(); // replay the header intro
                        ui.selected = None;
                        ui.panel = ui.base;
                    }
                }
            }
            if last_refresh.elapsed() >= Duration::from_millis(300) {
                live.refresh();
                sess = live.build(&set.pricing);
                persist.update(&sess, live.clock());
                last_refresh = Instant::now();
            }
            let t = live.clock();
            let app_t = app_start.elapsed().as_millis() as i64;
            let view = ui.view(set, &sess, t, Some(&persist.totals), term_size(&term));
            paint(&mut term, render::frame(&sess, t, app_t, &view, &mut rng), render::width(&view))?;
            let Some(k) = next_key(Duration::from_millis(50))? else {
                continue;
            };
            match ui.key(k, set, &sess, t) {
                Key::Quit => break,
                Key::Done => {}
                Key::Other(KeyCode::Char('r')) => {
                    live.refresh();
                    sess = live.build(&set.pricing);
                }
                Key::Other(_) => {}
            }
        }
        Ok(())
    })();

    persist.flush();
    leave(&mut term)?;
    res
}

// ---- wrapped -----------------------------------------------------------------

/// `agentflow wrapped`: aggregate lifetime history (or demo data when there
/// is none in range), then export, print a debug slide, or play the slides.
fn run_wrapped(range: wrapped::Range, export: Option<&Path>, debug: Option<(usize, i64)>) -> io::Result<()> {
    let store = lifetime::Store::load(&lifetime::path());
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let mut w = wrapped::aggregate(&store, range, now);
    if w.sessions == 0 {
        w = wrapped::demo(range);
    }

    if let Some(path) = export {
        std::fs::write(path, wrapped::svg(&w))?;
        println!(
            "agentflow: wrote {} (1200×675){}",
            path.display(),
            if w.demo { " · demo data, no history yet" } else { "" }
        );
        return Ok(());
    }
    if let Some((n, ms)) = debug {
        print_ansi(&wrapped::slide(&w, n.clamp(1, wrapped::SLIDES) - 1, ms));
        return Ok(());
    }

    let mut term = enter()?;
    let mut i = 0usize;
    let mut since = Instant::now();
    let res = (|| -> io::Result<()> {
        loop {
            let ms = since.elapsed().as_millis() as i64;
            paint(&mut term, wrapped::slide(&w, i, ms), wrapped::CW as i64 + 2)?;
            let Some(k) = next_key(Duration::from_millis(33))? else {
                continue;
            };
            let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
            match k.code {
                _ if ctrl_c => break,
                KeyCode::Char('q') | KeyCode::Esc => break,
                KeyCode::Char(' ') | KeyCode::Right | KeyCode::Enter | KeyCode::Char('l') => {
                    if i + 1 >= wrapped::SLIDES {
                        break;
                    }
                    i += 1;
                    since = Instant::now();
                }
                KeyCode::Left | KeyCode::Backspace | KeyCode::Char('h') => {
                    i = i.saturating_sub(1);
                    since = Instant::now();
                }
                _ => {}
            }
        }
        Ok(())
    })();
    leave(&mut term)?;
    res
}

/// Print rendered lines to stdout using truecolor ANSI (for --frame / --probe).
fn print_ansi(lines: &[Line<'static>]) {
    let mut buf = String::new();
    for line in lines {
        for span in &line.spans {
            match span.style.fg.unwrap_or(ratatui::style::Color::Reset) {
                ratatui::style::Color::Rgb(r, g, b) => {
                    buf.push_str(&format!("\x1b[38;2;{r};{g};{b}m{}", span.content));
                }
                _ => buf.push_str(&span.content),
            }
        }
        buf.push_str("\x1b[0m\n");
    }
    print!("{buf}");
    let _ = io::stdout().flush();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;
    use std::time::SystemTime;

    fn touch(path: &Path, secs_ago: u64) {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        let f = File::create(path).unwrap();
        f.set_modified(SystemTime::now() - Duration::from_secs(secs_ago)).unwrap();
    }

    #[test]
    fn keys_dismiss_recap_then_toggle_panels() {
        let cfg = Config::default();
        let set = Settings { pricing: Pricing::new(&cfg.pricing), cfg };
        let sess = demo::session("h".into(), &set.pricing);
        let press = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        let mut ui = Ui::new(false);
        let done = sess.all + render::RECAP_DELAY;

        // recap up: the first key (even q) only dismisses it
        assert!(ui.recap_showing(&set, &sess, done));
        assert!(matches!(ui.key(press(KeyCode::Char('q')), &set, &sess, done), Key::Done));
        assert!(!ui.recap_showing(&set, &sess, done));
        assert_eq!(ui.panel, Panel::Main);

        // panels toggle; Esc backs out of a panel before it quits
        ui.key(press(KeyCode::Char('s')), &set, &sess, done);
        assert_eq!(ui.panel, Panel::Stats);
        ui.key(press(KeyCode::Char('t')), &set, &sess, done);
        assert_eq!(ui.panel, Panel::Timeline);
        ui.key(press(KeyCode::Char('t')), &set, &sess, done);
        assert_eq!(ui.panel, Panel::Main);
        ui.key(press(KeyCode::Char('l')), &set, &sess, done);
        assert!(matches!(ui.key(press(KeyCode::Esc), &set, &sess, done), Key::Done));
        assert_eq!(ui.panel, Panel::Main);
        assert!(matches!(ui.key(press(KeyCode::Esc), &set, &sess, done), Key::Quit));

        // recap off in config: never shows
        let mut set2 = Settings { pricing: set.pricing.clone(), cfg: Config::default() };
        set2.cfg.recap = false;
        assert!(!Ui::new(false).recap_showing(&set2, &sess, done));
        // ↑/↓ select, Enter opens the detail panel, Esc backs out, Esc again clears
        let mut ui = Ui::new(false);
        let mid = 5_000; // the first three demo agents have started, the reviewer hasn't
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        assert_eq!(ui.selected, Some(0));
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        assert_eq!(ui.selected, Some(2), "clamps at the last agent");
        ui.key(press(KeyCode::Up), &set, &sess, mid);
        assert_eq!(ui.selected, Some(1));
        ui.key(press(KeyCode::Enter), &set, &sess, mid);
        assert_eq!(ui.panel, Panel::Detail);
        ui.key(press(KeyCode::Esc), &set, &sess, mid);
        assert_eq!((ui.panel, ui.selected), (Panel::Main, Some(1)));
        ui.key(press(KeyCode::Esc), &set, &sess, mid);
        assert_eq!(ui.selected, None);
        // before anyone spawns there's nothing to select
        let mut ui = Ui::new(false);
        ui.key(press(KeyCode::Enter), &set, &sess, 0);
        assert_eq!((ui.panel, ui.selected), (Panel::Main, None));

        // v cycles tree → orbit → timeline; Enter opens the detail view over
        // the current one and Esc returns to it
        let mut ui = Ui::new(false);
        ui.key(press(KeyCode::Char('v')), &set, &sess, mid);
        assert_eq!((ui.base, ui.panel), (Panel::Orbit, Panel::Orbit));
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        ui.key(press(KeyCode::Down), &set, &sess, mid);
        assert_eq!(ui.selected, Some(1), "↑/↓ work in the orbit");
        ui.key(press(KeyCode::Enter), &set, &sess, mid);
        assert_eq!((ui.base, ui.panel), (Panel::Orbit, Panel::Detail));
        ui.key(press(KeyCode::Esc), &set, &sess, mid);
        assert_eq!(ui.panel, Panel::Orbit, "back to the orbit, not the tree");
        ui.key(press(KeyCode::Char('v')), &set, &sess, mid);
        assert_eq!(ui.panel, Panel::Timeline);
        ui.key(press(KeyCode::Char('s')), &set, &sess, mid);
        ui.key(press(KeyCode::Char('s')), &set, &sess, mid);
        assert_eq!(ui.panel, Panel::Timeline, "closing a panel returns to the view");
        ui.key(press(KeyCode::Char('v')), &set, &sess, mid);
        assert_eq!((ui.base, ui.panel), (Panel::Main, Panel::Main), "and around to the tree");
        // Esc steps back: selection, then the view, then quits
        ui.key(press(KeyCode::Char('v')), &set, &sess, mid);
        ui.key(press(KeyCode::Esc), &set, &sess, mid);
        assert_eq!((ui.panel, ui.selected), (Panel::Orbit, None));
        ui.key(press(KeyCode::Esc), &set, &sess, mid);
        assert_eq!(ui.panel, Panel::Main);
        assert!(matches!(ui.key(press(KeyCode::Esc), &set, &sess, mid), Key::Quit));

        // ctrl-c always quits, recap or not
        let mut ui = Ui::new(false);
        let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert!(matches!(ui.key(ctrl_c, &set, &sess, done), Key::Quit));
    }

    /// Validators are opt-in: none for a live session unless configured; the
    /// demo shows its own so the gates are on display.
    #[test]
    fn validators_are_opt_in() {
        let cfg = Config::default();
        let mut set = Settings { pricing: Pricing::new(&cfg.pricing), cfg };
        let demo = demo::session("h".into(), &set.pricing);
        let live = demo::synth("h".into());
        assert!(set.validators(&live).is_empty());
        assert_eq!(set.validators(&demo), ["test-runner", "reviewer"]);
        set.cfg.validators = vec!["qa".into()];
        assert_eq!(set.validators(&live), ["qa"]);
        assert_eq!(set.validators(&demo), ["qa"], "a configured list wins in the demo too");
    }

    /// Synthetic ~/.claude/projects: a stale session and a fresh one in the same
    /// project. We follow the fresh one — unless a background agent is still
    /// writing to the stale session's subagent files, in which case it's active
    /// and we stay put.
    #[test]
    fn auto_switch_follows_newer_session_in_same_dir() {
        let home = std::env::temp_dir().join(format!("aflow-home-{}", std::process::id()));
        std::env::set_var("HOME", &home);
        let project = PathBuf::from("/work/demo-proj");
        let pdir = discovery::projects_root().join(discovery::encode_path(&project));
        let old = pdir.join("old-session.jsonl");
        let new = pdir.join("new-session.jsonl");
        touch(&old, 120);
        touch(&new, 0);

        let follow = Follow { project: Some(project.clone()), cwd: project.clone(), fell_back: false };
        let next = newer_session(&follow, &old).expect("should switch to the newer session");
        assert_eq!(next.path, new);
        assert!(newer_session(&follow, &new).is_none(), "already on the newest");

        // A background agent keeps the old session alive: no switch.
        touch(&pdir.join("old-session").join("subagents").join("agent-x.jsonl"), 0);
        assert!(newer_session(&follow, &old).is_none(), "busy session must not be dropped");

        let _ = std::fs::remove_dir_all(&home);
    }
}
