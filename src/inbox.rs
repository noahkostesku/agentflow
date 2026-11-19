//! The inbox: every active session, across projects and harnesses, one line
//! each, most urgent first. It answers "which of my agents needs me right
//! now?"
//!
//! [`Inbox`] watches the sessions (one `Live` per session that
//! `discovery::find_active` turns up); [`classify`] turns a session's
//! [`Pulse`] into a [`State`]; [`render`] draws the list. `notify` sends
//! desktop notifications off the same entries.
//!
//! Urgency, most first: waiting on you (a permission prompt, a question),
//! error, stuck (quiet, nothing in flight), done but not yet reviewed,
//! working, idle (reviewed, or nothing happened yet).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use ratatui::text::{Line, Span};
use serde::{Deserialize, Serialize};

use crate::discovery::{self, Found, Roots};
use crate::duration::{compact, human};
use crate::live::{Live, Pulse};
use crate::pricing::{approx_cost, Pricing};
use crate::procs::{self, Alive, Proc};
use crate::render::{clen, ctake, lpad, pad, sp, SPIN};
use crate::schema::Harness;
use crate::theme::*;

/// Where a session stands, most urgent first (the derived order is the
/// inbox's sort order).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum State {
    /// blocked on the user: a permission prompt (inferred: a call that can
    /// prompt hasn't returned for ~10s), a question, a plan to approve
    Waiting,
    /// a tool call was denied or rejected
    Error,
    /// running, but quiet past `stuck_after` with nothing in flight
    Stuck,
    /// every agent finished (the turn is over), not yet reviewed
    Done,
    Working,
    /// was waiting or working, but its process is gone (the terminal was
    /// closed, the process killed), or, where processes can't be seen, it
    /// has been quiet for 12h+: nothing will happen until it's resumed
    Closed,
    /// reviewed, or nothing has happened in it yet
    Idle,
}

impl State {
    pub const ALL: [State; 7] = [State::Waiting, State::Error, State::Stuck, State::Done, State::Working, State::Closed, State::Idle];

    pub fn name(self) -> &'static str {
        match self {
            State::Waiting => "waiting",
            State::Error => "error",
            State::Stuck => "stuck",
            State::Done => "done",
            State::Working => "working",
            State::Closed => "closed",
            State::Idle => "idle",
        }
    }

    /// The user should look at it.
    pub fn needs_you(self) -> bool {
        matches!(self, State::Waiting | State::Error | State::Stuck | State::Done)
    }

    /// `r` can mark it reviewed (it isn't going anywhere on its own).
    pub fn reviewable(self) -> bool {
        matches!(self, State::Done | State::Error)
    }

    fn color(self) -> ratatui::style::Color {
        match self {
            State::Waiting => AM,
            State::Error => RED,
            State::Stuck => dim(AM, 0.72),
            State::Done => ac(),
            State::Working => G,
            State::Closed | State::Idle => D,
        }
    }
}

/// A session's state as of epoch ms `now`, and since when (epoch ms).
pub fn classify(p: &Pulse, now: i64, stuck_ms: i64) -> (State, i64) {
    if p.empty {
        return (State::Idle, p.last);
    }
    if let Some((since, _)) = &p.waiting {
        return (State::Waiting, *since);
    }
    if let Some((since, _)) = &p.error {
        return (State::Error, *since);
    }
    if p.running {
        if stuck_ms > 0 && !p.in_flight && now - p.last >= stuck_ms {
            return (State::Stuck, p.last);
        }
        return (State::Working, p.turn.unwrap_or(p.last));
    }
    (State::Done, p.done.unwrap_or(p.last))
}

/// One line of the inbox.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    /// "claude:<session id>": stable across runs (reviewed marks key on it)
    pub key: String,
    pub harness: Harness,
    /// the session's primary file
    pub path: PathBuf,
    pub project_dir: Option<PathBuf>,
    /// the project directory's name
    pub project: String,
    pub state: State,
    /// epoch ms the state began
    pub since: i64,
    /// what it's doing, waiting on, or said last
    pub what: String,
    /// working rows still running / all rows
    pub active: usize,
    pub total: usize,
    pub cost: f64,
    pub unpriced: bool,
    /// epoch ms of its latest sign of life
    pub last: i64,
}

impl Entry {
    /// The entry for a session's pulse. A done (or errored) session whose
    /// state began no later than its reviewed mark reads as idle.
    #[allow(clippy::too_many_arguments)]
    pub fn new(harness: Harness, id: &str, path: &Path, project_dir: Option<PathBuf>, p: &Pulse, now: i64, stuck_ms: i64, reviewed: &Reviewed) -> Entry {
        let key = format!("{}:{id}", harness.name());
        let (mut state, since) = classify(p, now, stuck_ms);
        let reviewed = state.reviewable() && reviewed.covers(&key, since);
        if reviewed {
            state = State::Idle;
        }
        let what = match state {
            State::Waiting => p.waiting.as_ref().map(|w| w.1.clone()).filter(|w| !w.is_empty()).unwrap_or_else(|| "waiting on you".into()),
            State::Error => p.error.as_ref().map(|e| e.1.clone()).filter(|e| !e.is_empty()).unwrap_or_else(|| "error".into()),
            State::Stuck if p.doing.is_empty() => "no activity".into(),
            State::Stuck => format!("last: {}", p.doing),
            State::Done => first_line(&p.said).unwrap_or_else(|| "done".into()),
            State::Working if p.doing.is_empty() => "thinking".into(),
            State::Working => p.doing.clone(),
            State::Closed | State::Idle => first_line(&p.said).unwrap_or_default(),
        };
        let project = project_dir
            .as_deref()
            .and_then(|d| d.file_name())
            .and_then(|n| n.to_str())
            .unwrap_or("session")
            .to_string();
        Entry {
            key,
            harness,
            path: path.to_path_buf(),
            project_dir,
            project,
            state,
            since,
            what: one_line(&what),
            active: p.active,
            total: p.total,
            cost: p.cost,
            unpriced: p.unpriced,
            last: p.last,
        }
    }

    /// The session to open in the tree/orbit view.
    pub fn found(&self) -> Found {
        Found { harness: self.harness, path: self.path.clone(), project_dir: self.project_dir.clone(), fell_back: false }
    }
}

fn first_line(s: &str) -> Option<String> {
    s.lines().map(str::trim).find(|l| !l.is_empty()).map(str::to_string)
}

/// Whitespace collapsed to single spaces, markdown emphasis dropped.
fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ").replace("**", "").replace('`', "")
}

/// Most urgent first. Within a state that needs you, whoever has waited
/// longest comes first; working and idle sessions, most recently active.
pub fn sort(v: &mut [Entry]) {
    v.sort_by(|a, b| {
        a.state
            .cmp(&b.state)
            .then_with(|| if a.state.needs_you() { a.since.cmp(&b.since) } else { b.last.cmp(&a.last) })
            .then_with(|| a.key.cmp(&b.key))
    });
}

/// "2 need you · 1 error · 4 working": a count per state that has any,
/// most urgent first (idle sessions aren't counted).
pub fn counts(v: &[Entry]) -> Vec<(State, String)> {
    State::ALL
        .iter()
        .filter(|s| !matches!(s, State::Idle | State::Closed))
        .filter_map(|&s| {
            let n = v.iter().filter(|e| e.state == s).count();
            let word = match s {
                State::Waiting if n == 1 => "needs you",
                State::Waiting => "need you",
                State::Error if n == 1 => "error",
                State::Error => "errors",
                other => other.name(),
            };
            (n > 0).then(|| (s, format!("{n} {word}")))
        })
        .collect()
}

// ---- reviewed marks ------------------------------------------------------------

/// Sessions the user has looked at: session key → the epoch ms of the state
/// they reviewed. Lives in `~/.agentflow/reviewed.json` (agentflow never
/// writes to a harness's directories). A later finish is newer than the mark,
/// so the session comes back as done.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Reviewed(HashMap<String, i64>);

/// Marks older than this are dropped on save.
const KEEP_MS: i64 = 14 * 24 * 3600 * 1000;

impl Reviewed {
    pub fn path() -> PathBuf {
        crate::config::dir().join("reviewed.json")
    }

    pub fn load(path: &Path) -> Reviewed {
        std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str(&t).ok()).unwrap_or_default()
    }

    /// Write atomically, dropping marks older than two weeks.
    pub fn save(&mut self, path: &Path, now: i64) -> std::io::Result<()> {
        self.0.retain(|_, ms| now - *ms < KEEP_MS);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self).unwrap_or_default())?;
        std::fs::rename(tmp, path)
    }

    /// Mark a session reviewed as of its state that began at `since`.
    pub fn mark(&mut self, key: &str, since: i64) {
        let e = self.0.entry(key.to_string()).or_insert(since);
        *e = (*e).max(since);
    }

    /// The state that began at `since` has been reviewed.
    pub fn covers(&self, key: &str, since: i64) -> bool {
        self.0.get(key).is_some_and(|ms| *ms >= since)
    }
}

// ---- the watcher -----------------------------------------------------------------

struct Watched {
    found: Found,
    live: Live,
    /// its state at the last `entries` call: a waiting or errored session
    /// stays in the inbox past the active window
    state: State,
}

/// Every active session, watched.
pub struct Inbox {
    harnesses: Vec<Harness>,
    window: Duration,
    watched: Vec<Watched>,
    /// the running agent processes, as last looked at
    alive: Alive,
    /// when `alive` was last looked at
    looked: Option<std::time::Instant>,
}

/// How often to look at the process table (a `ps`, plus `lsof` on macOS).
const PROCS_EVERY: Duration = Duration::from_secs(10);

impl Inbox {
    pub fn new(harnesses: &[Harness], window: Duration) -> Inbox {
        Inbox { harnesses: harnesses.to_vec(), window, watched: Vec::new(), alive: Alive::Unknown, looked: None }
    }

    /// Use this view of the running processes (tests, or a caller that
    /// already looked).
    #[cfg(test)]
    pub fn set_alive(&mut self, alive: Alive) {
        self.alive = alive;
    }

    /// Pick up sessions that became active and drop ones that went quiet
    /// (unless they're still waiting on you or in error).
    pub fn rescan(&mut self) {
        self.rescan_at(&Roots::here(), SystemTime::now());
        if self.looked.is_none_or(|t| t.elapsed() >= PROCS_EVERY) {
            self.looked = Some(std::time::Instant::now());
            self.alive = procs::scan();
        }
    }

    /// [`Inbox::rescan`] in the given storage roots, as of `now`.
    pub fn rescan_at(&mut self, roots: &Roots, now: SystemTime) {
        let found = discovery::find_active_at(roots, &self.harnesses, self.window, now);
        let fresh: HashSet<&Path> = found.iter().map(|f| f.path.as_path()).collect();
        self.watched.retain(|w| fresh.contains(w.found.path.as_path()) || matches!(w.state, State::Waiting | State::Error));
        for f in found {
            if self.watched.iter().any(|w| w.found.path == f.path) {
                continue;
            }
            let mut live = Live::new(f.open(), String::new());
            live.refresh();
            self.watched.push(Watched { found: f, live, state: State::Working });
        }
    }

    /// Read what's new in every watched session.
    pub fn refresh(&mut self) {
        for w in &mut self.watched {
            w.live.refresh();
        }
    }

    /// Every watched session's line, most urgent first. A session counts by
    /// its own activity, not its file's: a harness can touch a transcript
    /// long after the work in it ended (Claude Code appends bookkeeping
    /// records), so one whose latest sign of life is older than the window
    /// is left out, unless it's still waiting on you or in error.
    pub fn entries(&mut self, pricing: &Pricing, now: i64, stuck_ms: i64, reviewed: &Reviewed) -> Vec<Entry> {
        let window = self.window.as_millis() as i64;
        let mut rows: Vec<(Entry, bool)> = self
            .watched
            .iter()
            .map(|w| {
                let dir = w.live.cwd().map(PathBuf::from).or_else(|| w.found.project_dir.clone());
                let p = w.live.pulse(pricing, now);
                let e = Entry::new(w.found.harness, w.live.session_id(), &w.found.path, dir, &p, now, stuck_ms, reviewed);
                (e, !p.empty && now - p.last <= window)
            })
            .collect();
        let mut all: Vec<Entry> = rows.iter().map(|r| r.0.clone()).collect();
        settle(&mut all, &self.alive, now);
        for ((row, settled), w) in rows.iter_mut().zip(all).zip(&mut self.watched) {
            w.state = settled.state;
            row.0 = settled;
        }
        // a quiet session stays listed while it's still waiting on you or in
        // error (a closed one doesn't: it's going nowhere)
        let mut v: Vec<Entry> = rows
            .into_iter()
            .filter(|(e, recent)| *recent || matches!(e.state, State::Waiting | State::Error))
            .map(|(e, _)| e)
            .collect();
        sort(&mut v);
        v
    }
}

/// Sessions quiet this long count as closed where processes can't be seen.
pub const STALE_MS: i64 = 12 * 3600 * 1000;

/// Mark waiting, working and stuck sessions whose agent process is gone as
/// closed. A process is matched to a session by its id on the command line
/// (`claude --resume <id>`), else by directory: with `n` agent processes of a
/// harness in a directory, that harness's `n` most recently active sessions
/// there are the live ones (done sessions included: a process sitting at its
/// prompt is one of them). A harness whose processes can't all be placed
/// (unknown table, an unreadable directory) falls back to age: quiet for
/// [`STALE_MS`] or more.
pub fn settle(entries: &mut [Entry], alive: &Alive, now: i64) {
    let open = |s: State| matches!(s, State::Waiting | State::Working | State::Stuck);
    let norm = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    for h in [Harness::Claude, Harness::Codex] {
        let procs: Option<Vec<&Proc>> = match alive {
            Alive::Known(ps) => {
                let mine: Vec<&Proc> = ps.iter().filter(|p| p.harness == h).collect();
                mine.iter().all(|p| p.cwd.is_some()).then_some(mine)
            }
            Alive::Unknown => None,
        };
        let idx: Vec<usize> = (0..entries.len()).filter(|&i| entries[i].harness == h).collect();
        let Some(procs) = procs else {
            for &i in &idx {
                let e = &mut entries[i];
                if open(e.state) && now - e.last >= STALE_MS {
                    close(e, "stale");
                }
            }
            continue;
        };
        // id on a command line: that session is live, and that process is its
        let id_of = |e: &Entry| e.key.split_once(':').map_or(String::new(), |(_, id)| id.to_string());
        let mut live: Vec<bool> = vec![false; entries.len()];
        let mut free: Vec<&Proc> = Vec::new();
        for p in procs {
            match idx.iter().find(|&&i| { let id = id_of(&entries[i]); !id.is_empty() && p.args.contains(&id) }) {
                Some(&i) => live[i] = true,
                None => free.push(p),
            }
        }
        // the rest by directory, most recently active first
        let mut per_dir: HashMap<PathBuf, usize> = HashMap::new();
        for p in free {
            *per_dir.entry(norm(p.cwd.as_deref().expect("checked above"))).or_default() += 1;
        }
        let mut by_recency = idx.clone();
        by_recency.sort_by_key(|&i| std::cmp::Reverse(entries[i].last));
        for i in by_recency {
            if live[i] {
                continue;
            }
            let Some(dir) = entries[i].project_dir.as_deref().map(norm) else {
                live[i] = true; // nowhere to look: leave it be
                continue;
            };
            if let Some(n) = per_dir.get_mut(&dir).filter(|n| **n > 0) {
                *n -= 1;
                live[i] = true;
            }
        }
        for &i in &idx {
            if !live[i] && open(entries[i].state) {
                close(&mut entries[i], "closed");
            }
        }
    }
}

/// "closed while waiting · $ cargo publish"
fn close(e: &mut Entry, how: &str) {
    let was = if e.state == State::Waiting { "waiting" } else { "working" };
    e.what = if e.what.is_empty() { format!("{how} while {was}") } else { format!("{how} while {was} · {}", e.what) };
    e.state = State::Closed;
}

// ---- rendering --------------------------------------------------------------------

/// Content width: fits an 80-column terminal with a margin.
pub const W: usize = 78;

/// Display options for the inbox.
#[derive(Clone, Copy, Debug, Default)]
pub struct Look {
    pub calm: bool,
    /// the cost column
    pub cost: bool,
    /// desktop notifications are on (`--notify`)
    pub notify: bool,
    /// the active window, minutes (for the footer)
    pub window_min: u64,
    /// scripted entries, not real sessions
    pub demo: bool,
}

/// The inbox frame for a terminal `rows` tall: header with counts, a rule,
/// one line per session (scrolling to keep the selection in view), a rule
/// and the key hints. `t` drives the animation; `now` is epoch ms.
pub fn render(entries: &[Entry], selected: Option<usize>, now: i64, t: i64, look: &Look, rows: u16) -> Vec<Line<'static>> {
    let mut out = Vec::new();

    // header: title left, counts right
    let title = " agentflow // inbox";
    let mut right: Vec<Span<'static>> = Vec::new();
    let cs = counts(entries);
    if cs.is_empty() {
        right.push(sp("nothing active", D));
    }
    for (i, (s, txt)) in cs.iter().enumerate() {
        if i > 0 {
            right.push(sp(" · ", DIM));
        }
        let c = if *s == State::Working { G } else { s.color() };
        right.push(sp(txt.clone(), c));
    }
    // drop counts from the least urgent end until it fits
    let room = W.saturating_sub(clen(title) + 1);
    while right.iter().map(|s| clen(&s.content)).sum::<usize>() > room && right.len() > 1 {
        right.truncate(right.len().saturating_sub(2));
    }
    let used = clen(title) + right.iter().map(|s| clen(&s.content)).sum::<usize>();
    let mut hdr = vec![sp(ctake(title, 10), ac()), sp(cfrom(title, 10), G), sp(" ".repeat(W.saturating_sub(used)), D)];
    hdr.extend(right);
    out.push(Line::from(hdr));
    out.push(Line::from(sp(format!(" {}", "─".repeat(W - 1)), D)));

    // the list, scrolled so the selection stays in view. The frame is as
    // tall as the other views (or the list), never taller than the terminal.
    let n = entries.len();
    let want = (n.max(3) + 5).max(crate::render::H as usize);
    let height = if rows == 0 { want } else { want.min(rows as usize) }.max(8);
    let avail = height - 5;
    let sel = selected.filter(|s| *s < n);
    let (first, shown) = if n <= avail {
        (0, n)
    } else {
        let shown = avail - 1; // the last line says how many more
        let first = sel.map_or(0, |s| s.saturating_sub(shown - 1)).min(n - shown);
        (first, shown)
    };
    if n == 0 {
        out.push(Line::from(""));
        let msg = format!("  no sessions active in the last {}", compact(look.window_min as i64 * 60_000));
        out.push(Line::from(sp(msg, D)));
        out.push(Line::from(sp("  start Claude Code or Codex anywhere; they show up here", DIM)));
    }
    for (i, e) in entries.iter().enumerate().skip(first).take(shown) {
        out.push(row(e, sel == Some(i), now, t, look));
    }
    if shown < n {
        let below = n - first - shown;
        let above = first;
        let txt = match (above, below) {
            (0, b) => format!("  +{b} more ↓"),
            (a, 0) => format!("  +{a} more ↑"),
            (a, b) => format!("  +{a} ↑ · +{b} ↓"),
        };
        out.push(Line::from(sp(txt, D)));
    }
    while out.len() < height - 2 {
        out.push(Line::from(""));
    }

    // bottom rule and hints
    out.push(Line::from(sp(format!(" {}", "─".repeat(W - 1)), D)));
    let mut hints = vec![sp(" ", D)];
    let keys: &[(&str, &str)] = &[("↑↓", "select"), ("enter", "open"), ("r", "reviewed"), ("q", "quit")];
    for (i, (k, what)) in keys.iter().enumerate() {
        if i > 0 {
            hints.push(sp(" · ", VD));
        }
        hints.push(sp(*k, ac()));
        hints.push(sp(format!(" {what}"), D));
    }
    let mut tail = format!("active ≤ {}", compact(look.window_min as i64 * 60_000));
    if look.notify {
        tail = format!("notify on · {tail}");
    }
    if look.demo {
        tail = "demo · scripted sessions".into();
    }
    let used: usize = hints.iter().map(|s| clen(&s.content)).sum();
    hints.push(sp(lpad(&tail, W.saturating_sub(used)), DIM));
    out.push(Line::from(hints));
    out
}

fn cfrom(s: &str, n: usize) -> String {
    s.chars().skip(n).collect()
}

/// One session: `› ? (·_·)  claude  agentflow    $ cargo publish   1/3   4m  ≈$0.42`
fn row(e: &Entry, selected: bool, now: i64, t: i64, look: &Look) -> Line<'static> {
    let c = e.state.color();
    let idle = matches!(e.state, State::Idle | State::Closed);
    let bob = e.key.bytes().fold(0i64, |h, b| h.wrapping_mul(31).wrapping_add(b as i64)).rem_euclid(7);
    let glyph = match e.state {
        // the waiting mark breathes, so it catches the eye
        State::Waiting if !look.calm && (t / 600) % 2 == 1 => sp("?", dim(AM, 0.55)),
        State::Waiting => sp("?", AM),
        State::Error => sp("✗", RED),
        State::Stuck => sp("z", c),
        State::Done => sp("✓", ac()),
        State::Working if look.calm => sp("•", G),
        State::Working => sp(SPIN[((t / 80 + bob).rem_euclid(10)) as usize].to_string(), ac()),
        State::Closed => sp("○", D),
        State::Idle => sp("·", D),
    };
    let face = match e.state {
        State::Waiting => "(·_·)",
        State::Error => "(×_×)",
        State::Stuck => "(-_-)",
        State::Done => "(^‿^)",
        State::Closed => "(·.·)",
        State::Idle => "(-‿-)",
        State::Working if look.calm => "(•‿•)",
        State::Working if (t + bob * 500) % 3700 < 130 => "(-‿-)",
        State::Working => {
            if (t / 300 + bob) % 2 != 0 {
                "(•o•)"
            } else {
                "(•‿•)"
            }
        }
    };
    // right side: agents, time in this state, cost
    let agents = if e.total > 1 { format!("{}/{}", e.active, e.total) } else { String::new() };
    let elapsed = human(now - e.since);
    let cost = if look.cost { format!("{}{}", approx_cost(e.cost), if e.unpriced { "?" } else { "" }) } else { String::new() };
    // marker, glyph, face, harness, project | what | agents, elapsed, cost
    let left_w = 2 + 2 + 7 + 8 + 14;
    let right_w = 5 + 8 + if look.cost { 9 } else { 0 };
    let what_w = W - left_w - right_w - 1;
    let what = if clen(&e.what) > what_w { format!("{}…", ctake(&e.what, what_w - 1)) } else { e.what.clone() };
    let project = if clen(&e.project) > 13 { format!("{}…", ctake(&e.project, 12)) } else { e.project.clone() };

    let name_c = if idle { D } else { B };
    let what_c = match e.state {
        State::Working => G,
        State::Closed | State::Idle => D,
        _ => c,
    };
    let mut v = vec![
        sp(if selected { "› " } else { "  " }, ac()),
        glyph,
        sp(" ", D),
        sp(pad(face, 7), if e.state == State::Working { G } else { c }),
        sp(pad(e.harness.name(), 8), D),
        // the selected row's project in the accent
        sp(pad(&project, 14), if selected { ac() } else { name_c }),
        sp(pad(&what, what_w + 1), what_c),
        sp(lpad(&agents, 5), if idle { D } else { G }),
        sp(lpad(&elapsed, 8), if e.state.needs_you() { c } else { D }),
    ];
    if look.cost {
        v.push(sp(lpad(&cost, 9), D));
    }
    Line::from(v)
}

// ---- demo ---------------------------------------------------------------------------

/// Scripted sessions for `--demo`: several projects in every state.
pub fn demo(now: i64) -> Vec<Entry> {
    let s = |ago_s: i64| now - ago_s * 1000;
    #[allow(clippy::type_complexity)]
    let rows: [(Harness, &str, State, i64, &str, (usize, usize), f64); 9] = [
        (Harness::Claude, "payments-api", State::Waiting, 252, "$ cargo publish --dry-run", (1, 3), 1.84),
        (Harness::Codex, "infra", State::Waiting, 41, "question: deploy to staging or prod?", (1, 1), 0.31),
        (Harness::Claude, "web-app", State::Error, 95, "denied $ rm -rf node_modules", (0, 1), 0.22),
        (Harness::Codex, "data-pipeline", State::Stuck, 380, "last: $ python etl.py --full", (1, 2), 0.67),
        (Harness::Claude, "docs-site", State::Done, 62, "Rewrote the quickstart; all links check out.", (0, 2), 0.48),
        (Harness::Claude, "agentflow", State::Working, 734, "reviewer: read inbox.rs", (2, 4), 2.91),
        (Harness::Codex, "mobile", State::Working, 128, "$ npm test -- --watch=false", (1, 1), 0.12),
        (Harness::Codex, "scratch", State::Closed, 3_900, "closed while waiting · $ make deploy", (0, 1), 0.09),
        (Harness::Claude, "dotfiles", State::Idle, 1500, "Switched the prompt theme to mauve.", (0, 1), 0.05),
    ];
    let mut v: Vec<Entry> = rows
        .iter()
        .enumerate()
        .map(|(i, (h, project, state, since, what, (active, total), cost))| Entry {
            key: format!("{}:demo-{i}", h.name()),
            harness: *h,
            path: PathBuf::from(format!("demo-{i}")),
            project_dir: Some(PathBuf::from(format!("/demo/{project}"))),
            project: project.to_string(),
            state: *state,
            since: s(*since),
            what: what.to_string(),
            active: *active,
            total: *total,
            cost: *cost,
            unpriced: false,
            last: if state.needs_you() || *state == State::Closed { s(*since) } else { s(2) },
        })
        .collect();
    sort(&mut v);
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::now_ms;

    fn entry(key: &str, state: State, since: i64, last: i64) -> Entry {
        Entry {
            key: key.into(),
            harness: Harness::Claude,
            path: PathBuf::from(key),
            project_dir: None,
            project: key.into(),
            state,
            since,
            what: String::new(),
            active: 0,
            total: 1,
            cost: 0.0,
            unpriced: false,
            last,
        }
    }

    /// waiting, error, stuck, done, working, idle; the longest-waiting first
    /// among those that need you, the most recently active first otherwise.
    #[test]
    fn urgency_ordering() {
        let mut v = vec![
            entry("idle", State::Idle, 0, 900),
            entry("working-old", State::Working, 100, 500),
            entry("done", State::Done, 300, 300),
            entry("working-new", State::Working, 100, 800),
            entry("waiting-new", State::Waiting, 700, 700),
            entry("stuck", State::Stuck, 200, 200),
            entry("error", State::Error, 400, 400),
            entry("waiting-old", State::Waiting, 50, 50),
        ];
        sort(&mut v);
        let keys: Vec<&str> = v.iter().map(|e| e.key.as_str()).collect();
        assert_eq!(keys, ["waiting-old", "waiting-new", "error", "stuck", "done", "working-new", "working-old", "idle"]);
        let c: Vec<String> = counts(&v).into_iter().map(|(_, s)| s).collect();
        assert_eq!(c, ["2 need you", "1 error", "1 stuck", "1 done", "2 working"]);
        assert!(counts(&[]).is_empty());
    }

    /// The pulse → state rules.
    #[test]
    fn classify_states() {
        let now = 1_000_000;
        let base = Pulse { running: true, last: now - 5_000, turn: Some(now - 60_000), ..Pulse::default() };
        assert_eq!(classify(&base, now, 120_000), (State::Working, now - 60_000));
        let quiet = Pulse { last: now - 200_000, ..base.clone() };
        assert_eq!(classify(&quiet, now, 120_000), (State::Stuck, now - 200_000));
        assert_eq!(classify(&quiet, now, 0).0, State::Working, "stuck_after = 0 turns it off");
        let busy = Pulse { in_flight: true, ..quiet.clone() };
        assert_eq!(classify(&busy, now, 120_000).0, State::Working, "a slow command isn't stuck");
        let waiting = Pulse { waiting: Some((now - 30_000, "$ cargo publish".into())), error: Some((now, "x".into())), ..busy };
        assert_eq!(classify(&waiting, now, 120_000), (State::Waiting, now - 30_000), "waiting wins over error");
        let error = Pulse { waiting: None, ..waiting };
        assert_eq!(classify(&error, now, 120_000).0, State::Error);
        let done = Pulse { running: false, done: Some(now - 9_000), ..Pulse::default() };
        assert_eq!(classify(&done, now, 120_000), (State::Done, now - 9_000));
        assert_eq!(classify(&Pulse { empty: true, ..Pulse::default() }, now, 120_000).0, State::Idle);
    }

    /// `r` marks the done state reviewed; it persists; a later finish is new
    /// and shows as done again.
    #[test]
    fn reviewed_marks_persist_and_expire_on_a_new_finish() {
        let dir = std::env::temp_dir().join(format!("aflow-reviewed-{}", std::process::id()));
        let path = dir.join("reviewed.json");
        let _ = std::fs::remove_dir_all(&dir);
        let now = now_ms();
        let done = |at: i64| Pulse { running: false, done: Some(at), said: "All green.\nmore".into(), total: 1, ..Pulse::default() };
        let mk = |p: &Pulse, r: &Reviewed| Entry::new(Harness::Codex, "s1", Path::new("p"), None, p, now, 120_000, r);

        let mut r = Reviewed::load(&path);
        let e = mk(&done(now - 5_000), &r);
        assert_eq!((e.state, e.what.as_str(), e.key.as_str()), (State::Done, "All green.", "codex:s1"));
        r.mark(&e.key, e.since);
        r.save(&path, now).unwrap();

        let r = Reviewed::load(&path);
        assert_eq!(mk(&done(now - 5_000), &r).state, State::Idle, "reviewed survives a restart");
        assert_eq!(mk(&done(now - 1_000), &r).state, State::Done, "a newer finish needs a new look");
        let working = Pulse { running: true, last: now, ..Pulse::default() };
        assert_eq!(mk(&working, &r).state, State::Working, "marks never hide a running session");

        // old marks are dropped on save
        let mut r = r;
        r.mark("claude:ancient", now - KEEP_MS - 1);
        r.save(&path, now).unwrap();
        assert!(!Reviewed::load(&path).covers("claude:ancient", 0));
        assert!(Reviewed::load(&path).covers("codex:s1", now - 5_000));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Fake storage for both harnesses: Claude sessions in two projects (one
    /// waiting on a permission prompt, one done), a stale one, and a Codex
    /// session whose own rollout is old but whose subagent is busy. The inbox
    /// finds every active one across projects, in urgency order.
    #[test]
    fn cross_project_discovery_finds_every_active_session() {
        use crate::adapters::claude::encode_path;
        use std::fs;

        let base = std::env::temp_dir().join(format!("aflow-inbox-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let roots = Roots { claude: base.join("claude"), codex: base.join("codex") };
        let now = SystemTime::now();
        let now_ms = now_ms();
        let iso = |ago_ms: i64| {
            chrono::DateTime::from_timestamp_millis(now_ms - ago_ms).unwrap().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
        };
        let write = |path: &Path, lines: &[String], age_s: u64| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, lines.join("\n") + "\n").unwrap();
            let f = fs::File::options().write(true).open(path).unwrap();
            f.set_modified(now - Duration::from_secs(age_s)).unwrap();
        };
        let claude = |dir: &str, id: &str| roots.claude.join(encode_path(Path::new(dir))).join(format!("{id}.jsonl"));
        let prompt = |cwd: &str, ago: i64, text: &str| {
            format!(r#"{{"type":"user","timestamp":"{}","cwd":"{cwd}","message":{{"role":"user","content":"{text}"}}}}"#, iso(ago))
        };

        // alpha: a Bash call a minute old with no result (a permission prompt)
        let alpha = claude("/work/alpha", "a-1");
        write(&alpha, &[
            prompt("/work/alpha", 70_000, "publish it"),
            format!(r#"{{"type":"assistant","timestamp":"{}","cwd":"/work/alpha","message":{{"id":"m1","model":"claude-sonnet-5-5","role":"assistant","stop_reason":"tool_use","content":[{{"type":"tool_use","id":"tu1","name":"Bash","input":{{"command":"cargo publish --dry-run"}}}}],"usage":{{"input_tokens":1000,"output_tokens":200}}}}}}"#, iso(60_000)),
        ], 60);
        // beta: finished its turn
        let beta = claude("/work/beta", "b-1");
        write(&beta, &[
            prompt("/work/beta", 40_000, "run the tests"),
            format!(r#"{{"type":"assistant","timestamp":"{}","cwd":"/work/beta","message":{{"id":"m2","model":"claude-haiku-5-5","role":"assistant","stop_reason":"end_turn","content":[{{"type":"text","text":"All 14 tests pass.\nNothing else to do."}}],"usage":{{"input_tokens":10,"output_tokens":5}}}}}}"#, iso(30_000)),
        ], 30);
        // stale: three hours quiet, out of the window
        write(&claude("/work/stale", "s-1"), &[prompt("/work/stale", 10_800_000, "old")], 10_800);
        // touched: written to just now (bookkeeping), but its work ended two hours ago
        let touched = claude("/work/touched", "t-1");
        write(&touched, &[
            prompt("/work/touched", 7_300_000, "old work"),
            format!(r#"{{"type":"assistant","timestamp":"{}","cwd":"/work/touched","message":{{"id":"m3","role":"assistant","stop_reason":"end_turn","content":[{{"type":"text","text":"Done long ago."}}]}}}}"#, iso(7_200_000)),
            r#"{"type":"summary","summary":"old work","leafUuid":"x"}"#.to_string(),
        ], 10);

        // gamma (codex): the session's rollout is two hours old, its subagent is busy now
        let day = roots.codex.join("2026").join("01").join("01");
        let parent_id = "0000dddd-1111-2222-3333-444455556666";
        let child_id = "0000eeee-1111-2222-3333-444455556666";
        let parent = day.join(format!("rollout-2026-01-01T00-00-00-{parent_id}.jsonl"));
        write(&parent, &[
            format!(r#"{{"timestamp":"{}","ordinal":0,"type":"session_meta","payload":{{"id":"{parent_id}","cwd":"/work/gamma"}}}}"#, iso(7_200_000)),
        ], 7_200);
        write(&day.join(format!("rollout-2026-01-01T00-00-01-{child_id}.jsonl")), &[
            format!(r#"{{"timestamp":"{}","ordinal":0,"type":"session_meta","payload":{{"id":"{child_id}","parent_thread_id":"{parent_id}","agent_nickname":"Hume","agent_path":"/root/build","cwd":"/work/gamma"}}}}"#, iso(20_000)),
            format!(r#"{{"timestamp":"{}","ordinal":1,"type":"response_item","payload":{{"type":"custom_tool_call","call_id":"c1","name":"exec","input":"const r = await tools.exec_command({{cmd:\"cargo build\"}});"}}}}"#, iso(5_000)),
        ], 5);

        let found = discovery::find_active_at(&roots, discovery::SUPPORTED, Duration::from_secs(30 * 60), now);
        let mut paths: Vec<&Path> = found.iter().map(|f| f.path.as_path()).collect();
        paths.sort();
        let mut want = vec![alpha.as_path(), beta.as_path(), parent.as_path(), touched.as_path()];
        want.sort();
        assert_eq!(paths, want, "the subagent's write makes its session active; the stale one is out");
        let gamma = found.iter().find(|f| f.harness == Harness::Codex).unwrap();
        assert_eq!(gamma.project_dir.as_deref(), Some(Path::new("/work/gamma")));

        let mut ib = Inbox::new(discovery::SUPPORTED, Duration::from_secs(30 * 60));
        ib.rescan_at(&roots, now);
        let v = ib.entries(&Pricing::default(), now_ms, 120_000, &Reviewed::default());
        let got: Vec<(&str, Harness, State, &str)> =
            v.iter().map(|e| (e.project.as_str(), e.harness, e.state, e.what.as_str())).collect();
        assert_eq!(
            got,
            [
                ("alpha", Harness::Claude, State::Waiting, "$ cargo publish --dry-run"),
                ("beta", Harness::Claude, State::Done, "All 14 tests pass."),
                ("gamma", Harness::Codex, State::Working, "hume: $ cargo build"),
            ],
            "a recent write without recent work (touched) isn't active"
        );
        assert!(v[0].since <= now_ms - 59_000, "waiting since the call was made");
        assert!(v[0].cost > 0.0);

        // the waiting session stays listed after it falls out of the window
        let later = now + Duration::from_secs(40 * 60);
        ib.rescan_at(&roots, later);
        let v = ib.entries(&Pricing::default(), now_ms + 40 * 60_000, 120_000, &Reviewed::default());
        assert_eq!(v.iter().map(|e| e.project.as_str()).collect::<Vec<_>>(), ["alpha"]);
        let _ = fs::remove_dir_all(&base);
    }

    fn at(key: &str, h: Harness, dir: &str, state: State, last: i64) -> Entry {
        Entry { harness: h, project_dir: Some(PathBuf::from(dir)), what: "$ cargo publish".into(), ..entry(key, state, last, last) }
    }
    fn proc_in(h: Harness, dir: &str, args: &str) -> Proc {
        Proc { harness: h, cwd: Some(PathBuf::from(dir)), args: args.into() }
    }

    /// A session whose agent process is gone is closed, not waiting: one
    /// process in a directory keeps that directory's most recent session
    /// live (a done one at its prompt counts), the older waiting one there is
    /// a ghost; a working session with no process anywhere is too; an id on
    /// a command line pins its session; done and error sessions are left be.
    #[test]
    fn sessions_whose_process_is_gone_are_closed() {
        let now = 100 * 3_600_000;
        let (c, x) = (Harness::Claude, Harness::Codex);
        let mut v = vec![
            at("claude:ghost", c, "/w/a", State::Waiting, now - 3_600_000),
            at("claude:open", c, "/w/a", State::Done, now - 60_000),
            at("claude:busy", c, "/w/b", State::Working, now - 5_000),
            at("claude:resumed", c, "/w/c", State::Waiting, now - 7_200_000),
            at("claude:gone", c, "/w/d", State::Stuck, now - 600_000),
            at("claude:fin", c, "/w/e", State::Done, now - 600_000),
            at("codex:cx-1", x, "/w/a", State::Working, now - 1_000),
        ];
        let alive = Alive::Known(vec![
            proc_in(c, "/w/a", "claude"),
            proc_in(c, "/w/b", "claude --permission-mode default"),
            proc_in(c, "/elsewhere", "claude --resume resumed"),
            proc_in(x, "/w/a", "codex"),
        ]);
        settle(&mut v, &alive, now);
        let got: Vec<(&str, State)> = v.iter().map(|e| (e.key.as_str(), e.state)).collect();
        assert_eq!(
            got,
            [
                ("claude:ghost", State::Closed),
                ("claude:open", State::Done),
                ("claude:busy", State::Working),
                ("claude:resumed", State::Waiting),
                ("claude:gone", State::Closed),
                ("claude:fin", State::Done),
                ("codex:cx-1", State::Working),
            ]
        );
        assert_eq!(v[0].what, "closed while waiting · $ cargo publish");
        assert_eq!(v[4].what, "closed while working · $ cargo publish", "stuck was working");

        // closed sorts after working, out of the urgent group and the counts
        sort(&mut v);
        let order: Vec<State> = v.iter().map(|e| e.state).collect();
        assert_eq!(order, [State::Waiting, State::Done, State::Done, State::Working, State::Working, State::Closed, State::Closed]);
        assert!(!State::Closed.needs_you());
        assert!(counts(&v).iter().all(|(s, _)| *s != State::Closed));
    }

    /// Where processes can't be seen (or one's directory can't be read), a
    /// waiting or working session quiet for 12h+ is stale; anything younger
    /// is left as it is.
    #[test]
    fn without_process_info_old_sessions_go_stale() {
        let now = 100 * 3_600_000;
        let c = Harness::Claude;
        let fresh = || {
            vec![
                at("claude:old", c, "/w/a", State::Waiting, now - STALE_MS - 1),
                at("claude:young", c, "/w/b", State::Waiting, now - 3_600_000),
                at("claude:old-done", c, "/w/c", State::Done, now - 2 * STALE_MS),
            ]
        };
        let mut v = fresh();
        settle(&mut v, &Alive::Unknown, now);
        assert_eq!(v.iter().map(|e| e.state).collect::<Vec<_>>(), [State::Closed, State::Waiting, State::Done]);
        assert_eq!(v[0].what, "stale while waiting · $ cargo publish");

        // a claude process whose directory couldn't be read: same fallback
        let mut v = fresh();
        settle(&mut v, &Alive::Known(vec![Proc { harness: c, cwd: None, args: "claude".into() }]), now);
        assert_eq!(v.iter().map(|e| e.state).collect::<Vec<_>>(), [State::Closed, State::Waiting, State::Done]);
        // known and empty: nothing is running, so the young one is closed too
        let mut v = fresh();
        settle(&mut v, &Alive::Known(vec![]), now);
        assert_eq!(v.iter().map(|e| e.state).collect::<Vec<_>>(), [State::Closed, State::Closed, State::Done]);
    }

    /// Through the watcher: a closed session isn't kept past the active
    /// window the way a waiting one is.
    #[test]
    fn a_closed_session_leaves_the_inbox_with_the_window() {
        use crate::adapters::claude::encode_path;
        use std::fs;
        let base = std::env::temp_dir().join(format!("aflow-ghost-{}", std::process::id()));
        let _ = fs::remove_dir_all(&base);
        let roots = Roots { claude: base.join("claude"), codex: base.join("codex") };
        let now = SystemTime::now();
        let ms = now_ms();
        let iso = |ago: i64| chrono::DateTime::from_timestamp_millis(ms - ago).unwrap().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string();
        let path = roots.claude.join(encode_path(Path::new("/work/ghost"))).join("g-1.jsonl");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let lines = [
            format!(r#"{{"type":"user","timestamp":"{}","cwd":"/work/ghost","message":{{"content":"ship it"}}}}"#, iso(70_000)),
            format!(r#"{{"type":"assistant","timestamp":"{}","message":{{"id":"m1","stop_reason":"tool_use","content":[{{"type":"tool_use","id":"b1","name":"Bash","input":{{"command":"make deploy"}}}}]}}}}"#, iso(60_000)),
        ];
        fs::write(&path, lines.join("\n") + "\n").unwrap();
        let mut ib = Inbox::new(&[Harness::Claude], Duration::from_secs(30 * 60));
        ib.rescan_at(&roots, now);
        ib.set_alive(Alive::Known(vec![]));
        let v = ib.entries(&Pricing::default(), ms, 120_000, &Reviewed::default());
        assert_eq!((v[0].state, v[0].what.as_str()), (State::Closed, "closed while waiting · $ make deploy"));
        // 40 minutes on: a waiting session would still be listed; this one is gone
        ib.rescan_at(&roots, now + Duration::from_secs(40 * 60));
        assert!(ib.entries(&Pricing::default(), ms + 40 * 60_000, 120_000, &Reviewed::default()).is_empty());
        let _ = fs::remove_dir_all(&base);
    }

    fn text(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect()).collect()
    }

    /// Every line fits 80 columns; the header counts; the list scrolls to
    /// keep the selection in view.
    #[test]
    fn renders_within_80_columns() {
        let now = 10_000_000;
        let v = demo(now);
        assert_eq!(v[0].state, State::Waiting);
        assert_eq!(v.last().unwrap().state, State::Idle);
        let look = Look { cost: true, window_min: 30, demo: true, ..Look::default() };
        let lines = text(&render(&v, Some(0), now, 0, &look, 24));
        for l in &lines {
            assert!(clen(l) <= 80, "{} cols: {l}", clen(l));
        }
        assert!(lines[0].contains("2 need you · 1 error · 1 stuck · 1 done · 2 working"), "{}", lines[0]);
        assert!(lines.iter().any(|l| l.contains("payments-api") && l.contains("$ cargo publish") && l.contains("1/3") && l.contains("≈$1.84")));
        assert!(lines.iter().any(|l| l.starts_with("› ") && l.contains("(·_·)")), "selection marker on the first row");

        // a short terminal: the selected row stays visible, the rest are counted
        let lines = text(&render(&v, Some(v.len() - 1), now, 0, &look, 9));
        assert!(lines.iter().any(|l| l.contains("dotfiles")), "{lines:#?}");
        assert!(lines.iter().any(|l| l.contains("more ↑")), "{lines:#?}");
        let lines = text(&render(&[], None, now, 0, &look, 24));
        assert!(lines.iter().any(|l| l.contains("no sessions active in the last 30m")));
    }
}
