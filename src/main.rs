//! agentflow — one live view of every coding agent you run: a full-screen
//! terminal visualizer for Claude Code, Codex and other harnesses' sessions.
//! `--demo` replays a scripted session; default watches a live one.

mod adapters;
mod config;
mod demo;
mod discovery;
mod duration;
mod inbox;
mod lifetime;
mod live;
mod mixed;
mod model;
mod notify;
mod orbit;
mod pricing;
mod procs;
mod profile;
mod render;
mod runs;
mod schema;
mod stats;
mod theme;
mod wrapped;

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::{Parser, Subcommand, ValueEnum};
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use adapters::SourceAdapter;
use config::Config;
use schema::Harness;
use pricing::Pricing;
use render::{Panel, View};

type Term = Terminal<CrosstermBackend<Stdout>>;

/// How long the key-hint footer stays up (on startup and after `?`).
const HINT: Duration = Duration::from_secs(3);

#[derive(Parser, Debug)]
#[command(
    name = "agentflow",
    about = "One live view of every coding agent you run (Claude Code, Codex)",
    version
)]
struct Cli {
    /// Replay the scripted demo session on a loop (with `inbox`: a scripted
    /// inbox of sessions in every state).
    #[arg(long, global = true)]
    demo: bool,

    /// Disable glitch, shimmer, and the finish wave (steady, low-motion).
    #[arg(long, global = true)]
    calm: bool,

    /// Which harness's sessions to watch (default: the most recently active
    /// session in any of them). `all` shows every harness's session for the
    /// project together.
    #[arg(long, value_enum, global = true)]
    source: Option<Source>,

    /// Watch a specific session id instead of auto-detecting.
    #[arg(long, value_name = "ID", global = true)]
    session: Option<String>,

    /// Watch the session for a specific project path (default: cwd).
    #[arg(long, value_name = "PATH", global = true)]
    project: Option<String>,

    /// Profiler: tag the runs recorded while watching as this workflow; with
    /// `profile`, open it; with `compare`, count runs within it.
    #[arg(long, value_name = "NAME", global = true)]
    workflow: Option<String>,

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

/// Where to look for sessions.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
enum Source {
    Claude,
    Codex,
    /// planned
    Pi,
    /// planned
    Opencode,
    /// planned
    Gemini,
    /// every supported harness at once (the mixed view)
    All,
}

impl Source {
    /// The harnesses to search, or why there are none.
    fn harnesses(source: Option<Source>) -> Result<Vec<Harness>, String> {
        match source {
            None | Some(Source::All) => Ok(discovery::SUPPORTED.to_vec()),
            Some(Source::Claude) => Ok(vec![Harness::Claude]),
            Some(Source::Codex) => Ok(vec![Harness::Codex]),
            Some(other) => Err(format!(
                "{} support is planned but not built yet (see docs/adapters.md)",
                format!("{other:?}").to_lowercase()
            )),
        }
    }
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Stream the watched session as normalized events (schema v1), one JSON
    /// object per line on stdout: the session so far, then live as it grows.
    Events {
        /// JSON lines (the only format; accepted for clarity).
        #[arg(long)]
        json: bool,
        /// Print what's on disk now and exit instead of following.
        #[arg(long)]
        once: bool,
    },
    /// Every active session across projects and harnesses, most urgent first:
    /// which of your agents needs you right now (↑/↓ select, Enter open,
    /// r reviewed, q quit).
    Inbox {
        /// Also send a desktop notification when a session needs you.
        #[arg(long)]
        notify: bool,
        /// A session counts as active if written to in the last MIN minutes
        /// (default: `[inbox] active_minutes`, 30).
        #[arg(long, value_name = "MIN")]
        active: Option<u64>,
    },
    /// Headless watcher: a desktop notification when a session starts
    /// waiting on you, errors, gets stuck, or finishes. Runs until stopped.
    Notify {
        /// Watch sessions written to in the last MIN minutes (default:
        /// `[inbox] active_minutes`, 30).
        #[arg(long, value_name = "MIN")]
        active: Option<u64>,
        /// Print the notifications instead of sending them.
        #[arg(long)]
        dry_run: bool,
        /// Print every active session's state once and exit.
        #[arg(long)]
        once: bool,
    },
    /// Profile your workflows from run history: trends, per-role breakdown,
    /// regressions and insights (↑/↓ select, enter detail, esc back, q quit).
    Profile {
        /// Show seeded demo history instead of yours.
        #[arg(long)]
        demo: bool,
        /// How many recent runs the detail view covers.
        #[arg(long, value_name = "N", default_value_t = profile::DEFAULT_LAST)]
        last: usize,
        /// Print the screen once and exit (also when stdout isn't a terminal).
        #[arg(long)]
        print: bool,
        /// Backfill run history from every stored session of every
        /// supported harness (all projects; narrow with --source), skipping
        /// sessions already on record, then exit. Reads session files only.
        #[arg(long, conflicts_with_all = ["demo", "print"])]
        import: bool,
    },
    /// Two runs side by side: a number counts back from the latest run (1 =
    /// latest; within --workflow if given), anything else is a session id prefix.
    Compare {
        #[arg(value_name = "RUN_A")]
        a: String,
        #[arg(value_name = "RUN_B")]
        b: String,
        /// Compare seeded demo runs instead of yours.
        #[arg(long)]
        demo: bool,
    },
    /// Your agents, wrapped: eight animated slides from your lifetime history
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
        /// Debug: print slide N (1–8) at --at ms, then exit.
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
            stuck_ms: self.stuck_ms(),
            base: if render::VIEWS.iter().any(|(p, _)| *p == panel) { panel } else { Panel::Main },
            term: (0, 0),
            validators: &self.cfg.validators,
        }
    }

    /// Silence before a running agent reads as stuck, ms (0 = off).
    fn stuck_ms(&self) -> i64 {
        self.cfg.stuck_after.saturating_mul(1000).min(i64::MAX as u64) as i64
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
    let harnesses = Source::harnesses(cli.source).unwrap_or_else(|why| {
        eprintln!("agentflow: {why}");
        std::process::exit(2);
    });
    // `--source all` (unless pinned to one session): every harness's
    // session for the project, together
    let mixed = cli.source == Some(Source::All) && cli.session.is_none();
    if let Some(Cmd::Events { once, .. }) = &cli.cmd {
        let found = if mixed {
            discovery::find_mixed(&harnesses, &cwd)
        } else {
            discovery::find(&harnesses, cli.session.as_deref(), Some(&cwd)).into_iter().collect()
        };
        if found.is_empty() {
            no_session(&harnesses, cli.session.as_deref());
        }
        return run_events(found.iter().map(discovery::Found::open).collect(), *once);
    }
    if let Some(Cmd::Profile { demo, .. } | Cmd::Compare { demo, .. }) = &cli.cmd {
        let opts = profile::Opts {
            rules: &set.cfg.workflows,
            pricing: &set.pricing,
            project: cwd.clone(),
            harnesses: harnesses.clone(),
            demo: *demo || cli.demo,
            persist: set.cfg.persistence,
        };
        let wf = cli.workflow.as_deref();
        return match &cli.cmd {
            Some(Cmd::Profile { import: true, .. }) => profile::run_import(&opts),
            Some(Cmd::Profile { last, print, .. }) => profile::run(&opts, wf, *last, *print),
            Some(Cmd::Compare { a, b, .. }) => profile::run_compare(&opts, a, b, wf).or_else(|why| {
                eprintln!("agentflow: {why}");
                std::process::exit(2)
            }),
            _ => Ok(()),
        };
    }

    let window_min = match &cli.cmd {
        Some(Cmd::Inbox { active: Some(m), .. } | Cmd::Notify { active: Some(m), .. }) => *m,
        _ => set.cfg.inbox.active_minutes,
    }
    .max(1);
    if let Some(Cmd::Notify { dry_run, once, .. }) = &cli.cmd {
        return run_notify(&harnesses, &set, window_min, *dry_run, *once);
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
        let sess = demo::synth(header("demo", &cwd));
        print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
        return Ok(());
    }

    let notify = matches!(&cli.cmd, Some(Cmd::Inbox { notify: true, .. }));
    let mut app = App {
        set: &set,
        calm: cli.calm,
        persist: Persist {
            on: set.cfg.persistence,
            totals: store.totals(),
            store,
            dirty: false,
            runs: runs::Recorder::new(runs::path(), cli.workflow.clone()),
        },
        demo: cli.demo,
        harnesses: harnesses.clone(),
        window_min,
        notify,
        watcher: None,
        reviewed: if cli.demo { inbox::Reviewed::default() } else { inbox::Reviewed::load(&inbox::Reviewed::path()) },
        selected: None,
        demo_t0: adapters::now_ms(),
        cwd: cwd.clone(),
    };

    // --- Inbox: every active session, any project -------------------------
    if let Some(Cmd::Inbox { .. }) = &cli.cmd {
        if notify && !cli.demo {
            if let Err(why) = notify::Desktop::check() {
                eprintln!("agentflow: {why}");
            }
        }
        if let Some(ms) = cli.frame {
            // debug: one frame of the inbox (live: what's on disk now)
            app.demo_t0 -= ms;
            let entries = app.inbox_entries();
            let sel = cli.select.or((!entries.is_empty()).then_some(0));
            print_ansi(&inbox::render(&entries, sel, adapters::now_ms(), ms, &app.look(), render::H + 7));
            return Ok(());
        }
        return finish(run_app(Start::Inbox, &mut app), warning);
    }

    // --- Demo mode -------------------------------------------------------
    if cli.demo {
        let sess = if mixed {
            demo::mixed(header("all", &cwd), &set.pricing)
        } else {
            demo::session(header("demo", &cwd), &set.pricing)
        };
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
        return finish(run_app(Start::Demo(sess), &mut app), warning);
    }

    // --- Live mode -------------------------------------------------------
    if mixed {
        let Some(m) = mixed::Mixed::open(&harnesses, &cwd, &cwd) else {
            no_session(&harnesses, None);
        };
        if let Some(ms) = cli.frame {
            let sess = m.build(&set.pricing);
            let t = m.clock();
            print_ansi(&render::frame(&sess, t, ms, &debug_view(&sess, t, ms), &mut rng));
            return Ok(());
        }
        return finish(run_app(Start::Live(Watch::Mixed(m)), &mut app), warning);
    }
    match discovery::find(&harnesses, cli.session.as_deref(), Some(&cwd)) {
        Some(found) => {
            let live = open_live(&found, &cwd);
            if let Some(ms) = cli.frame {
                let sess = live.build(&set.pricing);
                let t = live.clock();
                print_ansi(&render::frame(&sess, t, ms, &debug_view(&sess, t, ms), &mut rng));
                return Ok(());
            }
            // Follow newer sessions unless pinned to one with --session.
            let follow = cli.session.is_none().then(|| Follow {
                harnesses: harnesses.clone(),
                project: project.clone(),
                cwd: cwd.clone(),
                fell_back: found.fell_back,
            });
            finish(run_app(Start::Live(Watch::One { live, follow }), &mut app), warning)
        }
        None if cli.session.is_some() || cli.source.is_some() => {
            no_session(&harnesses, cli.session.as_deref());
        }
        None => {
            // Nothing to watch anywhere — fall back to the demo so the tool
            // still shows something recognizable.
            let sess = demo::session(header("demo", &cwd), &set.pricing);
            if let Some(ms) = cli.frame {
                print_ansi(&render::frame(&sess, ms, ms, &debug_view(&sess, ms, ms), &mut rng));
                return Ok(());
            }
            app.demo = true;
            finish(run_app(Start::Demo(sess), &mut app), warning)
        }
    }
}

fn no_session(harnesses: &[Harness], session: Option<&str>) -> ! {
    let places: Vec<String> = harnesses
        .iter()
        .map(|h| match h {
            Harness::Claude => adapters::claude::projects_root().display().to_string(),
            Harness::Codex => adapters::codex::sessions_root().display().to_string(),
        })
        .collect();
    let what = session.map_or_else(|| "no session".to_string(), |id| format!("no session \"{id}\""));
    eprintln!("agentflow: {what} found under {}", places.join(" or "));
    std::process::exit(1);
}

/// `agentflow events`: print each event as a JSON line, then keep polling.
/// Several sources (`--source all`) interleave; each event names its harness
/// and session.
fn run_events(mut srcs: Vec<Box<dyn SourceAdapter>>, once: bool) -> io::Result<()> {
    let mut out = io::stdout().lock();
    loop {
        let mut write = || -> io::Result<()> {
            for src in &mut srcs {
                for ev in src.poll() {
                    serde_json::to_writer(&mut out, &ev)?;
                    out.write_all(b"\n")?;
                }
            }
            out.flush()
        };
        match write() {
            // the reader went away (`| head`): done, not an error
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => return Ok(()),
            r => r?,
        }
        if once {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(300));
    }
}

/// Repeat a config warning after the TUI exits (the alternate screen hid it).
fn finish(res: io::Result<()>, warning: Option<String>) -> io::Result<()> {
    if let Some(w) = warning {
        eprintln!("agentflow: {w}");
    }
    res
}

/// Attach to a session and read what's already on disk. The header uses the
/// real cwd the session reported (handles --session and the fallback, where
/// Claude's storage dir name is path-encoded).
fn open_live(found: &discovery::Found, cwd: &Path) -> live::Live {
    let mut live = live::Live::new(found.open(), String::new());
    live.refresh();
    let hdr_dir = live
        .cwd()
        .map(PathBuf::from)
        .or_else(|| found.project_dir.clone())
        .unwrap_or_else(|| cwd.to_path_buf());
    live.set_header(header(found.harness.name(), &hdr_dir));
    live
}

/// Auto-switch settings for live mode (absent when pinned with --session,
/// or opened from the inbox).
struct Follow {
    harnesses: Vec<Harness>,
    project: Option<PathBuf>,
    cwd: PathBuf,
    fell_back: bool,
}

/// A newer Claude session to switch to (see `discovery::newer`).
#[cfg(test)]
fn newer_session(follow: &Follow, current: &Path) -> Option<discovery::Found> {
    let dir = follow.project.clone().unwrap_or_else(|| follow.cwd.clone());
    discovery::newer(&[Harness::Claude], &dir, follow.fell_back, (Harness::Claude, current))
}

/// The header "agentflow // <harness> // <project>".
fn header(label: &str, dir: &Path) -> String {
    let name = dir
        .file_name()
        .and_then(|s| s.to_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("session");
    format!("agentflow // {label} // {name}")
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

fn run_demo(term: &mut Term, sess: &model::Session, app: &mut App, back: bool) -> io::Result<Exit> {
    let set = app.set;
    let mut rng = rand::thread_rng();
    let mut ui = Ui::new(app.calm);
    let length = sess.length.max(1);
    let mut offset: i64 = 0;
    let mut since = Instant::now();
    let mut running = true;
    let mut last_t = 0;

    let exit = loop {
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
        let view = ui.view(set, sess, t, None, term_size(term));
        paint(term, render::frame(sess, t, t, &view, &mut rng), render::width(&view))?;
        let Some(k) = next_key(Duration::from_millis(50))? else {
            continue;
        };
        match ui.key(k, set, sess, t) {
            Key::Quit => break leave_to(k, back),
            Key::Done => {}
            Key::Other(KeyCode::Char('i')) => break Exit::Inbox,
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
    };
    app.calm = ui.calm;
    Ok(exit)
}

/// How a session screen was left.
#[derive(Debug, PartialEq)]
enum Exit {
    Quit,
    /// `i`, or Esc out of a session opened from the inbox
    Inbox,
}

/// A quitting key in a session view: Esc goes back to the inbox when the
/// session was opened from it; `q` and ctrl-c always quit.
fn leave_to(k: KeyEvent, back: bool) -> Exit {
    if back && k.code == KeyCode::Esc {
        Exit::Inbox
    } else {
        Exit::Quit
    }
}

// ---- live loop (real clock, refresh from disk ~3x/sec) ---------------------

/// Lifetime persistence for live mode: an in-memory store updated on every
/// refresh, written to disk when a batch of agents finishes and on exit.
struct Persist {
    on: bool,
    store: lifetime::Store,
    totals: lifetime::Totals,
    dirty: bool,
    /// run history for the profiler (`runs.rs`)
    runs: runs::Recorder,
}

impl Persist {
    fn update(&mut self, sess: &model::Session, t: i64) {
        if !self.on {
            return;
        }
        self.runs.update(sess, t);
        if !self.store.record(sess, t) {
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
    /// Leaving a session (exit, switch, back to the inbox): write everything,
    /// finished runs too.
    fn close(&mut self) {
        self.flush();
        if self.on {
            self.runs.flush();
        }
    }
}

/// What live mode watches.
enum Watch {
    /// One session, hopping to a newer one (unless pinned: no `follow`).
    One { live: live::Live, follow: Option<Follow> },
    /// Every harness's session for the project (`--source all`).
    Mixed(mixed::Mixed),
}

impl Watch {
    fn refresh(&mut self) {
        match self {
            Watch::One { live, .. } => live.refresh(),
            Watch::Mixed(m) => m.refresh(),
        }
    }
    fn build(&self, p: &Pricing) -> model::Session {
        match self {
            Watch::One { live, .. } => live.build(p),
            Watch::Mixed(m) => m.build(p),
        }
    }
    fn clock(&self) -> i64 {
        match self {
            Watch::One { live, .. } => live.clock(),
            Watch::Mixed(m) => m.clock(),
        }
    }
    /// Pick up newer sessions. True if what's shown changed.
    fn rescan(&mut self) -> bool {
        match self {
            Watch::One { live, follow } => {
                let Some(fw) = follow.as_mut() else {
                    return false;
                };
                let dir = fw.project.clone().unwrap_or_else(|| fw.cwd.clone());
                let Some(f) = discovery::newer(&fw.harnesses, &dir, fw.fell_back, (live.harness(), live.path())) else {
                    return false;
                };
                fw.fell_back = f.fell_back;
                *live = open_live(&f, &fw.cwd);
                true
            }
            Watch::Mixed(m) => m.rescan(),
        }
    }
    /// Record the watched sessions' lifetime numbers (each harness's session
    /// separately, with its harness).
    fn record(&self, sess: &model::Session, p: &Pricing, persist: &mut Persist) {
        match self {
            Watch::One { live, .. } => persist.update(sess, live.clock()),
            Watch::Mixed(m) => {
                for (s, t) in m.parts(p) {
                    persist.update(&s, t);
                }
            }
        }
    }
}

fn run_live(term: &mut Term, mut watch: Watch, app: &mut App, back: bool) -> io::Result<Exit> {
    let set = app.set;
    let tune = notify::Tuning { pricing: &set.pricing, stuck_ms: set.stuck_ms() };
    let mut rng = rand::thread_rng();
    let mut ui = Ui::new(app.calm);
    let mut app_start = Instant::now();
    let mut last_refresh = Instant::now();
    let mut last_scan = Instant::now();
    let mut sess = watch.build(&set.pricing);
    watch.record(&sess, &set.pricing, &mut app.persist);

    let exit = loop {
        // Opened from the inbox: keep watching every session from here too
        // (notifications with `--notify`, and run history either way).
        if let Some(w) = app.watcher.as_mut() {
            w.tick(&tune, &app.reviewed);
        }
        // Every ~2s, look for a newer session (unless pinned).
        if last_scan.elapsed() >= Duration::from_secs(2) {
            last_scan = Instant::now();
            if watch.rescan() {
                app.persist.close();
                sess = watch.build(&set.pricing);
                app_start = Instant::now(); // replay the header intro
                ui.selected = None;
                ui.panel = ui.base;
            }
        }
        if last_refresh.elapsed() >= Duration::from_millis(300) {
            watch.refresh();
            sess = watch.build(&set.pricing);
            watch.record(&sess, &set.pricing, &mut app.persist);
            last_refresh = Instant::now();
        }
        let t = watch.clock();
        let app_t = app_start.elapsed().as_millis() as i64;
        let view = ui.view(set, &sess, t, Some(&app.persist.totals), term_size(term));
        paint(term, render::frame(&sess, t, app_t, &view, &mut rng), render::width(&view))?;
        let Some(k) = next_key(Duration::from_millis(50))? else {
            continue;
        };
        match ui.key(k, set, &sess, t) {
            Key::Quit => break leave_to(k, back),
            Key::Done => {}
            Key::Other(KeyCode::Char('i')) => break Exit::Inbox,
            Key::Other(KeyCode::Char('r')) => {
                watch.refresh();
                sess = watch.build(&set.pricing);
            }
            Key::Other(_) => {}
        }
    };

    app.persist.close();
    app.calm = ui.calm;
    Ok(exit)
}

// ---- the inbox, and moving between it and sessions ---------------------------

/// What every screen shares for one run of the TUI.
struct App<'a> {
    set: &'a Settings,
    calm: bool,
    persist: Persist,
    /// the inbox lists scripted sessions, not real ones
    demo: bool,
    harnesses: Vec<Harness>,
    /// the inbox's active window, minutes
    window_min: u64,
    /// `inbox --notify`
    notify: bool,
    /// the inbox's watcher, from the first time it's opened (kept so the
    /// notifier remembers what it already said)
    watcher: Option<notify::Watcher>,
    reviewed: inbox::Reviewed,
    /// the selected inbox row, by session key (rows move as states change)
    selected: Option<String>,
    /// when the demo inbox's clock started (epoch ms)
    demo_t0: i64,
    /// where agentflow was started
    cwd: PathBuf,
}

/// The screen to show.
enum Start {
    Live(Watch),
    Demo(model::Session),
    Inbox,
}

impl App<'_> {
    fn look(&self) -> inbox::Look {
        inbox::Look {
            calm: self.calm,
            cost: self.set.cfg.cost,
            notify: self.notify && !self.demo,
            window_min: self.window_min,
            demo: self.demo,
        }
    }

    /// The inbox's rows right now, most urgent first.
    fn inbox_entries(&mut self) -> Vec<inbox::Entry> {
        if self.demo {
            let mut v = inbox::demo(self.demo_t0);
            for e in &mut v {
                if e.state.reviewable() && self.reviewed.covers(&e.key, e.since) {
                    e.state = inbox::State::Idle;
                }
            }
            inbox::sort(&mut v);
            return v;
        }
        let set = self.set;
        if self.watcher.is_none() {
            let mut ib = inbox::Inbox::new(&self.harnesses, Duration::from_secs(self.window_min * 60));
            if set.cfg.persistence {
                // every session that finishes, not just ones opened here
                ib = ib.record_runs(runs::Recorder::new(runs::path(), None));
            }
            let sink: Box<dyn notify::Sink> = Box::new(notify::Desktop);
            let notifier = self.notify.then(|| (notify::Notifier::new(&set.cfg.notify), sink));
            self.watcher = Some(notify::Watcher::new(ib, notifier));
        }
        let tune = notify::Tuning { pricing: &set.pricing, stuck_ms: set.stuck_ms() };
        let w = self.watcher.as_mut().expect("set above");
        w.tick(&tune, &self.reviewed);
        w.entries.clone()
    }

    /// Mark a done (or errored) session reviewed, and remember it.
    fn review(&mut self, e: &inbox::Entry) {
        if !e.state.reviewable() {
            return;
        }
        self.reviewed.mark(&e.key, e.since);
        if self.demo {
            return;
        }
        // best effort: a read-only home just means it isn't remembered
        let _ = self.reviewed.save(&inbox::Reviewed::path(), adapters::now_ms());
        let set = self.set;
        let tune = notify::Tuning { pricing: &set.pricing, stuck_ms: set.stuck_ms() };
        if let Some(w) = self.watcher.as_mut() {
            w.recompute(&tune, &self.reviewed);
        }
    }

    /// The screen for an inbox row: its session, pinned (no auto-switch).
    fn open(&self, e: &inbox::Entry) -> Start {
        if self.demo {
            let dir = e.project_dir.clone().unwrap_or_default();
            return Start::Demo(demo::session(header(e.harness.name(), &dir), &self.set.pricing));
        }
        Start::Live(Watch::One { live: open_live(&e.found(), &self.cwd), follow: None })
    }
}

/// Run the TUI from `start`, moving between the inbox and sessions until the
/// user quits. One alternate screen for the whole run.
fn run_app(start: Start, app: &mut App) -> io::Result<()> {
    let mut term = enter()?;
    let res = (|| -> io::Result<()> {
        let mut screen = start;
        // a session opened from the inbox: Esc goes back to it
        let mut back = false;
        loop {
            let exit = match screen {
                Start::Live(w) => run_live(&mut term, w, app, back)?,
                Start::Demo(s) => run_demo(&mut term, &s, app, back)?,
                Start::Inbox => match run_inbox(&mut term, app)? {
                    Some(e) => {
                        back = true;
                        screen = app.open(&e);
                        continue;
                    }
                    None => Exit::Quit,
                },
            };
            match exit {
                Exit::Quit => return Ok(()),
                Exit::Inbox => screen = Start::Inbox,
            }
        }
    })();
    app.persist.close();
    leave(&mut term)?;
    res
}

/// What a key does in the inbox.
#[derive(Debug, PartialEq)]
enum InboxKey {
    Quit,
    /// open this row
    Open(usize),
    /// mark this row reviewed
    Review(usize),
    /// select this row
    Select(usize),
    Calm,
    Nothing,
}

/// The inbox's keys, given the selected row and how many there are.
fn inbox_key(k: KeyEvent, sel: Option<usize>, n: usize) -> InboxKey {
    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c');
    match (k.code, sel) {
        _ if ctrl_c => InboxKey::Quit,
        (KeyCode::Char('q') | KeyCode::Esc, _) => InboxKey::Quit,
        (KeyCode::Up | KeyCode::Char('k'), Some(i)) => InboxKey::Select(i.saturating_sub(1)),
        (KeyCode::Down | KeyCode::Char('j'), Some(i)) => InboxKey::Select((i + 1).min(n.saturating_sub(1))),
        (KeyCode::Enter, Some(i)) => InboxKey::Open(i),
        (KeyCode::Char('r'), Some(i)) => InboxKey::Review(i),
        (KeyCode::Char('c'), _) => InboxKey::Calm,
        _ => InboxKey::Nothing,
    }
}

/// The inbox screen. Returns the row to open, or `None` to quit.
fn run_inbox(term: &mut Term, app: &mut App) -> io::Result<Option<inbox::Entry>> {
    let started = Instant::now();
    loop {
        let entries = app.inbox_entries();
        let mut sel = app.selected.as_ref().and_then(|k| entries.iter().position(|e| &e.key == k));
        if sel.is_none() && !entries.is_empty() {
            sel = Some(0);
        }
        app.selected = sel.map(|i| entries[i].key.clone());
        let t = started.elapsed().as_millis() as i64;
        let rows = term_size(term).1;
        paint(term, inbox::render(&entries, sel, adapters::now_ms(), t, &app.look(), rows), inbox::W as i64 + 2)?;
        let Some(k) = next_key(Duration::from_millis(50))? else {
            continue;
        };
        match inbox_key(k, sel, entries.len()) {
            InboxKey::Quit => return Ok(None),
            InboxKey::Open(i) => return Ok(Some(entries[i].clone())),
            InboxKey::Review(i) => app.review(&entries[i]),
            InboxKey::Select(i) => app.selected = Some(entries[i].key.clone()),
            InboxKey::Calm => app.calm = !app.calm,
            InboxKey::Nothing => {}
        }
    }
}

/// `agentflow notify`: no TUI. Watch every active session and send a desktop
/// notification when one needs you, logging each to stdout.
fn run_notify(harnesses: &[Harness], set: &Settings, window_min: u64, dry_run: bool, once: bool) -> io::Result<()> {
    let ib = inbox::Inbox::new(harnesses, Duration::from_secs(window_min * 60));
    let tune = notify::Tuning { pricing: &set.pricing, stuck_ms: set.stuck_ms() };
    let stamp = || chrono::Local::now().format("%H:%M:%S").to_string();
    let mut out = io::stdout().lock();
    if once {
        let mut w = notify::Watcher::new(ib, None);
        w.tick(&tune, &inbox::Reviewed::load(&inbox::Reviewed::path()));
        for e in &w.entries {
            let what: String = e.what.chars().take(72).collect();
            let ago = duration::human(adapters::now_ms() - e.since);
            writeln!(out, "{:<8} {:>7}  {:<6} {:<20} {what}", e.state.name(), ago, e.harness.name(), e.project)?;
        }
        if w.entries.is_empty() {
            writeln!(out, "no sessions active in the last {}", duration::compact(window_min as i64 * 60_000))?;
        }
        return Ok(());
    }
    let sink: Box<dyn notify::Sink> = match (dry_run, notify::Desktop::check()) {
        (true, _) => Box::new(Discard),
        (false, Ok(())) => Box::new(notify::Desktop),
        (false, Err(why)) => {
            writeln!(out, "agentflow notify: {why}")?;
            Box::new(Discard)
        }
    };
    let names: Vec<&str> = harnesses.iter().map(|h| h.name()).collect();
    // every session that finishes goes into run history, as in the inbox
    let ib = if set.cfg.persistence { ib.record_runs(runs::Recorder::new(runs::path(), None)) } else { ib };
    let mut w = notify::Watcher::new(ib, Some((notify::Notifier::new(&set.cfg.notify), sink)));
    writeln!(
        out,
        "{}  agentflow notify: watching {} sessions active in the last {}{} (ctrl-c to stop)",
        stamp(),
        names.join(" + "),
        duration::compact(window_min as i64 * 60_000),
        if dry_run { ", dry run" } else { "" }
    )?;
    out.flush()?;
    let mut reviewed = inbox::Reviewed::default();
    let mut loaded: Option<Instant> = None;
    loop {
        // pick up marks made in the inbox
        if loaded.is_none_or(|t| t.elapsed() >= Duration::from_secs(5)) {
            reviewed = inbox::Reviewed::load(&inbox::Reviewed::path());
            loaded = Some(Instant::now());
        }
        for n in w.tick(&tune, &reviewed) {
            // a closed stdout (launchd, a gone pipe) doesn't stop notifying
            let _ = writeln!(out, "{}  {:<8} {}  {}", stamp(), n.state.name(), n.title, n.body).and_then(|_| out.flush());
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// A sink that drops notifications (`--dry-run`, or nowhere to send them).
struct Discard;

impl notify::Sink for Discard {
    fn send(&mut self, _: &notify::Note) {}
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

    /// The inbox's keys, and leaving a session view: `i` and (when it was
    /// opened from the inbox) Esc go back to the inbox; `q` always quits.
    #[test]
    fn inbox_keys_and_the_way_back() {
        let press = |c: KeyCode| KeyEvent::new(c, KeyModifiers::NONE);
        assert_eq!(inbox_key(press(KeyCode::Down), Some(0), 3), InboxKey::Select(1));
        assert_eq!(inbox_key(press(KeyCode::Down), Some(2), 3), InboxKey::Select(2), "clamps at the last row");
        assert_eq!(inbox_key(press(KeyCode::Up), Some(0), 3), InboxKey::Select(0));
        assert_eq!(inbox_key(press(KeyCode::Enter), Some(1), 3), InboxKey::Open(1));
        assert_eq!(inbox_key(press(KeyCode::Char('r')), Some(2), 3), InboxKey::Review(2));
        assert_eq!(inbox_key(press(KeyCode::Enter), None, 0), InboxKey::Nothing, "nothing to open");
        assert_eq!(inbox_key(press(KeyCode::Esc), Some(0), 3), InboxKey::Quit);
        assert_eq!(inbox_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL), Some(0), 3), InboxKey::Quit);

        assert_eq!(leave_to(press(KeyCode::Esc), true), Exit::Inbox);
        assert_eq!(leave_to(press(KeyCode::Esc), false), Exit::Quit);
        assert_eq!(leave_to(press(KeyCode::Char('q')), true), Exit::Quit);

        // `i` reaches the screen loop from any view (the UI doesn't eat it)
        let cfg = Config::default();
        let set = Settings { pricing: Pricing::new(&cfg.pricing), cfg };
        let sess = demo::session("h".into(), &set.pricing);
        let mut ui = Ui::new(false);
        ui.key(press(KeyCode::Char('v')), &set, &sess, 5_000);
        assert!(matches!(ui.key(press(KeyCode::Char('i')), &set, &sess, 5_000), Key::Other(KeyCode::Char('i'))));
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

        let follow = Follow { harnesses: vec![Harness::Claude], project: Some(project.clone()), cwd: project.clone(), fell_back: false };
        let next = newer_session(&follow, &old).expect("should switch to the newer session");
        assert_eq!(next.path, new);
        assert!(newer_session(&follow, &new).is_none(), "already on the newest");

        // A background agent keeps the old session alive: no switch.
        touch(&pdir.join("old-session").join("subagents").join("agent-x.jsonl"), 0);
        assert!(newer_session(&follow, &old).is_none(), "busy session must not be dropped");

        let _ = std::fs::remove_dir_all(&home);
    }
}
