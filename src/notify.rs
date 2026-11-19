//! Desktop notifications when a session needs you: it starts waiting on you,
//! hits an error, goes quiet (stuck), or finishes.
//!
//! [`Notifier`] decides: one notification per state change per session, never
//! a repeat while the state holds. The first look at a session that was
//! already in that state when the watcher started stays silent, so starting
//! it doesn't fire a burst. Per-state toggles, quiet hours and a repeat guard
//! (a slow command can flip working ↔ waiting) come from `[notify]` in the
//! config. [`Desktop`] sends them: `osascript` on macOS, `notify-send` on
//! Linux. [`Watcher`] drives the inbox and the notifier together, for the
//! headless `agentflow notify` and for `agentflow inbox --notify`.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::adapters::now_ms;
use crate::config::NotifyConfig;
use crate::inbox::{Entry, Inbox, Reviewed, State};
use crate::pricing::Pricing;

/// One desktop notification.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    /// the session it's about
    pub key: String,
    pub state: State,
    pub title: String,
    pub body: String,
}

impl Note {
    fn new(e: &Entry) -> Note {
        let p = &e.project;
        let title = match e.state {
            State::Waiting => format!("{p} is waiting on you"),
            State::Error => format!("{p} hit an error"),
            State::Stuck => format!("{p} looks stuck"),
            _ => format!("{p} is done"),
        };
        let body = if e.what.is_empty() { e.harness.name().to_string() } else { format!("{} · {}", e.harness.name(), e.what) };
        Note { key: e.key.clone(), state: e.state, title, body }
    }
}

/// Decides which state changes become notifications.
pub struct Notifier {
    cfg: NotifyConfig,
    /// each session's state at the last update
    seen: HashMap<String, State>,
    /// (session, state) → epoch ms it was last notified
    sent: HashMap<(String, State), i64>,
    /// the first update only records what's there
    primed: bool,
}

impl Notifier {
    pub fn new(cfg: &NotifyConfig) -> Notifier {
        Notifier { cfg: cfg.clone(), seen: HashMap::new(), sent: HashMap::new(), primed: false }
    }

    fn wants(&self, s: State) -> bool {
        match s {
            State::Waiting => self.cfg.waiting,
            State::Error => self.cfg.error,
            State::Stuck => self.cfg.stuck,
            State::Done => self.cfg.done,
            State::Working | State::Closed | State::Idle => false,
        }
    }

    /// Feed the latest entries (as of epoch ms `now`, local minute of the
    /// day `minute`); returns what to send. A session's state change is
    /// notified once, if its state is turned on, it isn't quiet hours, and
    /// the same state wasn't notified for it within `repeat_after`. A change
    /// during quiet hours is dropped, not delayed.
    pub fn update(&mut self, entries: &[Entry], now: i64, minute: u32) -> Vec<Note> {
        let quiet = self.cfg.is_quiet(minute);
        let guard = (self.cfg.repeat_after as i64).saturating_mul(1000);
        let mut out = Vec::new();
        for e in entries {
            let prev = self.seen.insert(e.key.clone(), e.state);
            if !self.primed || prev == Some(e.state) || !self.wants(e.state) || quiet {
                continue;
            }
            let k = (e.key.clone(), e.state);
            if self.sent.get(&k).is_some_and(|at| now - at < guard) {
                continue;
            }
            self.sent.insert(k, now);
            out.push(Note::new(e));
        }
        self.primed = true;
        out
    }
}

/// Where notifications go.
pub trait Sink {
    fn send(&mut self, note: &Note);
}

/// The desktop: `osascript` on macOS, `notify-send` on Linux (if installed).
pub struct Desktop;

impl Desktop {
    /// Can this machine show notifications? `Err` says why not.
    pub fn check() -> Result<(), String> {
        if cfg!(target_os = "macos") {
            return Ok(());
        }
        let found = std::env::var_os("PATH")
            .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("notify-send").is_file()));
        if found {
            Ok(())
        } else {
            Err("notify-send not found (install libnotify); printing notifications instead".into())
        }
    }

    fn command(note: &Note) -> Command {
        if cfg!(target_os = "macos") {
            let script = format!(
                "display notification {} with title {}",
                applescript_str(&note.body),
                applescript_str(&note.title)
            );
            let mut c = Command::new("osascript");
            c.arg("-e").arg(script);
            c
        } else {
            let mut c = Command::new("notify-send");
            c.args(["--app-name=agentflow", &note.title, &note.body]);
            c
        }
    }
}

impl Sink for Desktop {
    fn send(&mut self, note: &Note) {
        let mut cmd = Desktop::command(note);
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
        // off the UI thread; reaped when it exits
        std::thread::spawn(move || {
            let _ = cmd.status();
        });
    }
}

/// A quoted AppleScript string literal.
fn applescript_str(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control()).collect();
    format!("\"{}\"", clean.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Local minute of the day, for quiet hours.
pub fn local_minute() -> u32 {
    use chrono::Timelike;
    let t = chrono::Local::now();
    t.hour() * 60 + t.minute()
}

/// The inbox, kept current on its own clock: rescans for sessions every few
/// seconds, re-reads them every second, and (optionally) notifies.
pub struct Watcher {
    pub inbox: Inbox,
    notify: Option<(Notifier, Box<dyn Sink>)>,
    last_scan: Option<Instant>,
    last_read: Option<Instant>,
    /// the latest entries, most urgent first
    pub entries: Vec<Entry>,
}

/// What a [`Watcher`] needs from the settings.
pub struct Tuning<'a> {
    pub pricing: &'a Pricing,
    pub stuck_ms: i64,
}

impl Watcher {
    pub fn new(inbox: Inbox, notify: Option<(Notifier, Box<dyn Sink>)>) -> Watcher {
        Watcher { inbox, notify, last_scan: None, last_read: None, entries: Vec::new() }
    }

    /// Catch up if due. Returns the notifications sent (already sent).
    pub fn tick(&mut self, tune: &Tuning, reviewed: &Reviewed) -> Vec<Note> {
        let due = |at: Option<Instant>, every: Duration| at.is_none_or(|t| t.elapsed() >= every);
        if due(self.last_scan, Duration::from_secs(3)) {
            self.last_scan = Some(Instant::now());
            self.inbox.rescan();
        }
        if !due(self.last_read, Duration::from_secs(1)) {
            return Vec::new();
        }
        self.last_read = Some(Instant::now());
        self.inbox.refresh();
        self.recompute(tune, reviewed)
    }

    /// Rebuild the entries now (after a reviewed mark, say) and notify.
    pub fn recompute(&mut self, tune: &Tuning, reviewed: &Reviewed) -> Vec<Note> {
        let now = now_ms();
        self.entries = self.inbox.entries(tune.pricing, now, tune.stuck_ms, reviewed);
        let Some((notifier, sink)) = &mut self.notify else {
            return Vec::new();
        };
        let notes = notifier.update(&self.entries, now, local_minute());
        for n in &notes {
            sink.send(n);
        }
        notes
    }

    pub fn notifying(&self) -> bool {
        self.notify.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Harness;
    use std::path::PathBuf;

    fn e(key: &str, state: State) -> Entry {
        Entry {
            key: key.into(),
            harness: Harness::Claude,
            path: PathBuf::from(key),
            project_dir: None,
            project: format!("proj-{key}"),
            state,
            since: 0,
            what: "$ cargo publish".into(),
            active: 1,
            total: 1,
            cost: 0.0,
            unpriced: false,
            last: 0,
        }
    }

    fn states(notes: &[Note]) -> Vec<(&str, State)> {
        notes.iter().map(|n| (n.key.as_str(), n.state)).collect()
    }

    /// One notification per state change per session; holding a state never
    /// repeats; working and idle never notify; startup is silent.
    #[test]
    fn state_changes_notify_once() {
        let cfg = NotifyConfig { repeat_after: 0, ..NotifyConfig::default() };
        let mut n = Notifier::new(&cfg);
        let noon = 12 * 60;
        // startup: already waiting, nothing sent
        assert!(n.update(&[e("a", State::Waiting), e("b", State::Working)], 0, noon).is_empty());
        assert!(n.update(&[e("a", State::Waiting), e("b", State::Working)], 1_000, noon).is_empty(), "no repeat");

        let notes = n.update(&[e("a", State::Working), e("b", State::Waiting)], 2_000, noon);
        assert_eq!(states(&notes), [("b", State::Waiting)]);
        assert_eq!(notes[0].title, "proj-b is waiting on you");
        assert_eq!(notes[0].body, "claude · $ cargo publish");
        for t in 3..6 {
            assert!(n.update(&[e("a", State::Working), e("b", State::Waiting)], t * 1_000, noon).is_empty());
        }
        let notes = n.update(&[e("a", State::Error), e("b", State::Done)], 7_000, noon);
        assert_eq!(states(&notes), [("a", State::Error), ("b", State::Done)]);
        let notes = n.update(&[e("a", State::Stuck), e("b", State::Idle)], 8_000, noon);
        assert_eq!(states(&notes), [("a", State::Stuck)], "reviewed (idle) is not news");
        // a session showing up after startup, already waiting, is news
        let notes = n.update(&[e("a", State::Stuck), e("c", State::Waiting)], 9_000, noon);
        assert_eq!(states(&notes), [("c", State::Waiting)]);
    }

    /// Per-state toggles, quiet hours (spanning midnight), and the repeat
    /// guard against a session flapping between working and waiting.
    #[test]
    fn toggles_quiet_hours_and_repeat_guard() {
        let cfg = NotifyConfig { done: false, quiet: "22:00-07:30".into(), repeat_after: 60, ..NotifyConfig::default() };
        let mut n = Notifier::new(&cfg);
        let noon = 12 * 60;
        n.update(&[e("a", State::Working)], 0, noon);
        assert!(n.update(&[e("a", State::Done)], 1_000, noon).is_empty(), "done is off");
        assert_eq!(n.update(&[e("a", State::Waiting)], 2_000, noon).len(), 1);
        n.update(&[e("a", State::Working)], 3_000, noon);
        assert!(n.update(&[e("a", State::Waiting)], 4_000, noon).is_empty(), "flapping: within repeat_after");
        n.update(&[e("a", State::Working)], 5_000, noon);
        assert_eq!(n.update(&[e("a", State::Waiting)], 70_000, noon).len(), 1, "after repeat_after it's news again");

        n.update(&[e("a", State::Working)], 200_000, noon);
        assert!(n.update(&[e("a", State::Error)], 201_000, 23 * 60).is_empty(), "23:00 is quiet");
        assert!(n.update(&[e("a", State::Error)], 202_000, noon).is_empty(), "dropped, not delayed: no repeat at noon");
        n.update(&[e("a", State::Working)], 300_000, noon);
        assert!(n.update(&[e("a", State::Error)], 301_000, 7 * 60).is_empty(), "07:00 is quiet");
        n.update(&[e("a", State::Working)], 400_000, noon);
        assert_eq!(n.update(&[e("a", State::Error)], 401_000, 7 * 60 + 30).len(), 1, "07:30 is not");
    }

    #[test]
    fn applescript_strings_are_escaped() {
        assert_eq!(applescript_str(r#"say "hi" \ bye"#), r#""say \"hi\" \\ bye""#);
        assert_eq!(applescript_str("a\nb"), "\"ab\"");
    }
}
