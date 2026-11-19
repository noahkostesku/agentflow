//! agentflow — a full-screen terminal visualizer for Claude Code multi-agent
//! sessions. `--demo` replays the scripted mockup; default watches a live
//! session's transcripts.

mod demo;
mod discovery;
mod model;
mod render;
mod theme;
mod transcript;

use std::io::{self, Stdout, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use clap::Parser;
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::{cursor, execute, terminal};
use ratatui::layout::Rect;
use ratatui::prelude::CrosstermBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;
use ratatui::Terminal;

use render::W;

type Term = Terminal<CrosstermBackend<Stdout>>;

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

    /// Debug: render a single frame at the given ms and print it, then exit.
    #[arg(long, value_name = "MS", hide = true)]
    frame: Option<i64>,

    /// Debug: render N frames across the demo loop to stdout, then exit.
    #[arg(long, value_name = "N", hide = true)]
    probe: Option<usize>,

    /// Debug: render a synthetic live frame (active + just-finished) at the given ms.
    #[arg(long, value_name = "MS", hide = true)]
    synth: Option<i64>,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let mut rng = rand::thread_rng();

    let cwd = cli
        .project
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_default();

    if let Some(ms) = cli.synth {
        let sess = demo::synth(header_for(&cwd));
        print_ansi(&render::frame(&sess, ms, ms, cli.calm, &mut rng));
        return Ok(());
    }

    // --- Demo mode -------------------------------------------------------
    if cli.demo {
        let sess = demo::session(header_for(&cwd));
        if let Some(ms) = cli.frame {
            print_ansi(&render::frame(&sess, ms, ms, cli.calm, &mut rng));
            return Ok(());
        }
        if let Some(n) = cli.probe {
            for i in 0..n {
                let ms = (sess.length * i as i64) / n.max(1) as i64;
                println!("--- t={ms}ms ---");
                print_ansi(&render::frame(&sess, ms, ms, cli.calm, &mut rng));
                println!();
            }
            return Ok(());
        }
        return run_demo(&sess, cli.calm);
    }

    // --- Live mode -------------------------------------------------------
    let resolved = discovery::resolve(
        cli.session.as_deref(),
        cli.project.as_deref().map(Path::new),
    );
    match resolved {
        Some(r) => {
            let live = open_live(&r, &cwd);
            if let Some(ms) = cli.frame {
                let sess = live.build();
                let t = live.clock();
                print_ansi(&render::frame(&sess, t, ms, cli.calm, &mut rng));
                return Ok(());
            }
            // Follow newer sessions unless pinned to one with --session.
            let follow = cli.session.is_none().then(|| Follow {
                project: cli.project.as_ref().map(PathBuf::from),
                cwd: cwd.clone(),
                fell_back: r.fell_back,
            });
            run_live(live, cli.calm, follow)
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
            let sess = demo::session(header_for(&cwd));
            if let Some(ms) = cli.frame {
                print_ansi(&render::frame(&sess, ms, ms, cli.calm, &mut rng));
                return Ok(());
            }
            run_demo(&sess, cli.calm)
        }
    }
}

/// Attach to a resolved session and read what's already on disk. The header
/// uses the real cwd recorded in the transcript (handles --session and the
/// fallback, where the storage dir name is path-encoded).
fn open_live(r: &discovery::Resolved, cwd: &Path) -> transcript::Live {
    let hdr_dir = transcript::read_cwd(&r.path)
        .or_else(|| r.project_dir.clone())
        .unwrap_or_else(|| cwd.to_path_buf());
    let mut live = transcript::Live::new(r, header_for(&hdr_dir));
    live.refresh();
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

/// Draw the 16-line frame centered in the terminal.
fn paint(term: &mut Term, lines: Vec<Line<'static>>) -> io::Result<()> {
    term.draw(|f| {
        let area = f.area();
        let bw = (W as u16).min(area.width);
        let bh = 16u16.min(area.height);
        let x = area.x + area.width.saturating_sub(bw) / 2;
        let y = area.y + area.height.saturating_sub(bh) / 2;
        f.render_widget(Paragraph::new(lines), Rect::new(x, y, bw, bh));
    })?;
    Ok(())
}

// ---- demo loop (looping clock, pause/restart) ------------------------------

fn run_demo(sess: &model::Session, mut calm: bool) -> io::Result<()> {
    let mut term = enter()?;
    let mut rng = rand::thread_rng();
    let length = sess.length.max(1);
    let mut offset: i64 = 0;
    let mut since = Instant::now();
    let mut running = true;

    let res = (|| -> io::Result<()> {
        loop {
            let t = (offset
                + if running {
                    since.elapsed().as_millis() as i64
                } else {
                    0
                })
                % length;
            paint(&mut term, render::frame(sess, t, t, calm, &mut rng))?;
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char('c'));
                    match k.code {
                        _ if ctrl_c => break,
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char(' ') => {
                            if running {
                                offset = (offset + since.elapsed().as_millis() as i64) % length;
                                running = false;
                            } else {
                                since = Instant::now();
                                running = true;
                            }
                        }
                        KeyCode::Char('r') => {
                            offset = 0;
                            since = Instant::now();
                        }
                        KeyCode::Char('c') => calm = !calm,
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    })();

    leave(&mut term)?;
    res
}

// ---- live loop (real clock, refresh from disk ~3x/sec) ---------------------

fn run_live(
    mut live: transcript::Live,
    mut calm: bool,
    mut follow: Option<Follow>,
) -> io::Result<()> {
    let mut term = enter()?;
    let mut rng = rand::thread_rng();
    let mut app_start = Instant::now();
    let mut last_refresh = Instant::now();
    let mut last_scan = Instant::now();
    let mut sess = live.build();

    let res = (|| -> io::Result<()> {
        loop {
            // Every ~2s, hop to a newer session (unless pinned by --session).
            if last_scan.elapsed() >= Duration::from_secs(2) {
                last_scan = Instant::now();
                if let Some(fw) = follow.as_mut() {
                    if let Some(next) = newer_session(fw, live.path()) {
                        fw.fell_back = next.fell_back;
                        live = open_live(&next, &fw.cwd);
                        sess = live.build();
                        app_start = Instant::now(); // replay the header intro
                    }
                }
            }
            if last_refresh.elapsed() >= Duration::from_millis(300) {
                live.refresh();
                sess = live.build();
                last_refresh = Instant::now();
            }
            let t = live.clock();
            let app_t = app_start.elapsed().as_millis() as i64;
            paint(&mut term, render::frame(&sess, t, app_t, calm, &mut rng))?;
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(k) = event::read()? {
                    if k.kind == KeyEventKind::Release {
                        continue;
                    }
                    let ctrl_c = k.modifiers.contains(KeyModifiers::CONTROL)
                        && matches!(k.code, KeyCode::Char('c'));
                    match k.code {
                        _ if ctrl_c => break,
                        KeyCode::Char('q') | KeyCode::Esc => break,
                        KeyCode::Char('c') => calm = !calm,
                        KeyCode::Char('r') => {
                            live.refresh();
                            sess = live.build();
                        }
                        _ => {}
                    }
                }
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
