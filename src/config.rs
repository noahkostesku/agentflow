//! `~/.agentflow/config.toml`: feature toggles, accent color, and pricing
//! overrides. Created with commented-out defaults on first run; a missing or
//! partial file falls back to the defaults below. CLI `--no-*` flags win.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::pricing::RatesOverride;

/// Default accent: dusty mauve.
pub const DEFAULT_ACCENT: u32 = 0xc4a5b4;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// one-line achievement popups
    pub achievements: bool,
    /// recap card when every agent has finished
    pub recap: bool,
    /// per-agent cost column and session total in the header
    pub cost: bool,
    /// brief "+$0.02" flash on each billed call (needs `cost`)
    pub flashes: bool,
    /// keep lifetime totals in ~/.agentflow/lifetime.json (the `l` view)
    pub persistence: bool,
    /// accent color, "#rrggbb"
    pub accent: String,
    /// seconds without transcript activity before a running agent reads as
    /// stuck (sleepy face, "idle 2m"); 0 turns it off
    pub stuck_after: u64,
    /// optional: roles the orbit view labels as pass/fail gates (display
    /// only; agentflow is read-only and never gates anything). Empty = none.
    pub validators: Vec<String>,
    /// USD per million tokens, keyed by model id or family
    pub pricing: HashMap<String, RatesOverride>,
    /// `agentflow inbox`
    pub inbox: InboxConfig,
    /// desktop notifications (`agentflow notify`, `inbox --notify`)
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct InboxConfig {
    /// a session counts as active (shows in the inbox) if it was written to
    /// within this many minutes
    pub active_minutes: u64,
}

impl Default for InboxConfig {
    fn default() -> Self {
        InboxConfig { active_minutes: 30 }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    /// notify when a session starts waiting on you (a prompt, a question)
    pub waiting: bool,
    /// … when it hits an error (a denied or rejected tool call)
    pub error: bool,
    /// … when it goes quiet with nothing in flight
    pub stuck: bool,
    /// … when it finishes (its turn is over)
    pub done: bool,
    /// "22:00-08:00": no notifications in this local-time window (may span
    /// midnight). Empty = none.
    pub quiet: String,
    /// seconds before the same state may notify again for the same session
    /// (a slow command can flip working ↔ waiting); 0 = no guard
    pub repeat_after: u64,
}

impl Default for NotifyConfig {
    fn default() -> Self {
        NotifyConfig { waiting: true, error: true, stuck: true, done: true, quiet: String::new(), repeat_after: 300 }
    }
}

impl NotifyConfig {
    /// Is local minute-of-day `min` (0..1440) inside the quiet window?
    pub fn is_quiet(&self, min: u32) -> bool {
        match parse_quiet(&self.quiet) {
            Some((a, b)) if a <= b => (a..b).contains(&min),
            Some((a, b)) => min >= a || min < b,
            None => false,
        }
    }
}

/// "22:00-08:00" → (1320, 480) in minutes of the day.
pub fn parse_quiet(s: &str) -> Option<(u32, u32)> {
    let hm = |t: &str| -> Option<u32> {
        let (h, m) = t.trim().split_once(':')?;
        let (h, m) = (h.parse::<u32>().ok()?, m.parse::<u32>().ok()?);
        (h < 24 && m < 60).then_some(h * 60 + m)
    };
    let (a, b) = s.split_once('-')?;
    Some((hm(a)?, hm(b)?))
}

impl Default for Config {
    fn default() -> Self {
        Config {
            achievements: true,
            recap: true,
            cost: true,
            flashes: true,
            persistence: true,
            accent: format!("#{DEFAULT_ACCENT:06x}"),
            stuck_after: (crate::stats::STUCK_MS / 1000) as u64,
            validators: Vec::new(),
            pricing: HashMap::new(),
            inbox: InboxConfig::default(),
            notify: NotifyConfig::default(),
        }
    }
}

/// Written on first run. Every setting is commented out, so the file documents
/// the defaults without pinning them.
pub const DEFAULT_FILE: &str = r##"# agentflow config. Uncomment a line to change it; deleted or commented lines
# use the default shown. CLI flags (--no-achievements, --no-recap, --no-cost)
# override this file.

# achievements = true   # one-line popups: speedrun, pacifist, bug hunter, ...
# recap = true          # recap card when all agents finish (any key dismisses)
# cost = true           # per-agent cost + session total in the header
# flashes = true        # brief "+$0.02" flash on each billed call
# persistence = true    # keep lifetime totals in ~/.agentflow/lifetime.json
# accent = "#c4a5b4"    # the one accent color (dusty mauve)
# stuck_after = 120     # seconds of silence before an agent reads "idle" (0 = off)
# validators = ["test-runner", "reviewer"]  # optional: label these roles pass/fail in orbit

# The inbox (`agentflow inbox`, or `i`): every active session, any project.
# [inbox]
# active_minutes = 30   # a session written to this recently counts as active

# Desktop notifications (`agentflow notify`, `agentflow inbox --notify`): one
# per state change per session.
# [notify]
# waiting = true        # it's waiting on you (permission prompt, question)
# error = true          # a tool call was denied or rejected
# stuck = true          # quiet with nothing in flight (see stuck_after)
# done = true           # it finished its turn
# quiet = "22:00-08:00" # no notifications in this local-time window
# repeat_after = 300    # seconds before the same state re-notifies a session

# Pricing overrides, USD per million tokens. Key by family (opus, sonnet,
# haiku, fable) or exact model id. Unset fields keep the built-in rate (see
# src/pricing.rs); overriding only `input` moves cache writes with it.
#
# [pricing.opus]
# input = 4.0
# output = 20.0
# cache_read = 0.20
# cache_write_5m = 5.0
# cache_write_1h = 8.0
#
# [pricing."claude-sonnet-5-5"]
# output = 10.0
"##;

/// `~/.agentflow`
pub fn dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    Path::new(&home).join(".agentflow")
}

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let cfg: Config = toml::from_str(text).map_err(|e| e.message().to_string())?;
        if parse_hex(&cfg.accent).is_none() {
            return Err(format!("accent \"{}\" is not a #rrggbb color", cfg.accent));
        }
        if !cfg.notify.quiet.trim().is_empty() && parse_quiet(&cfg.notify.quiet).is_none() {
            return Err(format!("notify.quiet \"{}\" is not a \"HH:MM-HH:MM\" window", cfg.notify.quiet));
        }
        Ok(cfg)
    }

    /// Load `<dir>/config.toml`, creating it with commented defaults when it
    /// doesn't exist. Returns the config and a warning to show, if any. Never
    /// fails: an unreadable or invalid file falls back to defaults.
    pub fn load_or_create(dir: &Path) -> (Config, Option<String>) {
        let path = dir.join("config.toml");
        match std::fs::read_to_string(&path) {
            Ok(text) => match Config::parse(&text) {
                Ok(cfg) => (cfg, None),
                Err(e) => (
                    Config::default(),
                    Some(format!("{}: {e} (using defaults)", path.display())),
                ),
            },
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Best effort: a read-only home just means no file.
                let _ = std::fs::create_dir_all(dir)
                    .and_then(|_| std::fs::write(&path, DEFAULT_FILE));
                (Config::default(), None)
            }
            Err(e) => (
                Config::default(),
                Some(format!("{}: {e} (using defaults)", path.display())),
            ),
        }
    }

    pub fn accent_rgb(&self) -> u32 {
        parse_hex(&self.accent).unwrap_or(DEFAULT_ACCENT)
    }
}

/// "#c4a5b4" / "c4a5b4" → 0xc4a5b4
pub fn parse_hex(s: &str) -> Option<u32> {
    let h = s.trim().trim_start_matches('#');
    (h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u32::from_str_radix(h, 16).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_is_all_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        let d = Config::default();
        assert!(d.achievements && d.recap && d.cost && d.flashes && d.persistence);
        assert_eq!(d.accent_rgb(), DEFAULT_ACCENT);
    }

    #[test]
    fn generated_file_parses_to_defaults() {
        // the shipped template must stay valid TOML that means "defaults"
        assert_eq!(Config::parse(DEFAULT_FILE).unwrap(), Config::default());
        // and each commented line, uncommented, must parse too
        let uncommented: String = DEFAULT_FILE
            .lines()
            .filter(|l| l.starts_with("# ") && l.contains(" = "))
            .map(|l| l.trim_start_matches("# ").split("  #").next().unwrap().to_string() + "\n")
            .collect();
        let top: String = uncommented.lines().take(7).map(|l| format!("{l}\n")).collect();
        assert_eq!(Config::parse(&top).unwrap(), Config::default());
    }

    #[test]
    fn toggles_accent_and_pricing() {
        let cfg = Config::parse(
            r##"
            achievements = false
            flashes = false
            accent = "#88c0d0"
            stuck_after = 45
            validators = ["qa"]

            [pricing.opus]
            input = 1.5

            [pricing."claude-haiku-5-5"]
            output = 0.75
            cache_read = 0.02
            "##,
        )
        .unwrap();
        assert!(!cfg.achievements && !cfg.flashes);
        assert!(cfg.recap && cfg.cost && cfg.persistence, "unset keys keep defaults");
        assert_eq!(cfg.accent_rgb(), 0x88c0d0);
        assert_eq!(cfg.stuck_after, 45);
        assert_eq!(cfg.validators, vec!["qa".to_string()]);
        assert!(Config::default().validators.is_empty(), "opt-in");
        assert_eq!(cfg.pricing["opus"].input, Some(1.5));
        assert_eq!(cfg.pricing["opus"].output, None);
        assert_eq!(cfg.pricing["claude-haiku-5-5"].output, Some(0.75));
    }

    #[test]
    fn bad_values_are_errors() {
        assert!(Config::parse("accent = \"mauve\"").is_err());
        assert!(Config::parse("cost = \"yes\"").is_err());
        assert!(Config::parse("costs = true").is_err(), "typos are reported");
        assert!(Config::parse("stuck_after = -5").is_err());
        assert!(Config::parse("[pricing.opus]\ninptu = 1.0").is_err());
    }

    #[test]
    fn inbox_and_notify_sections() {
        let d = Config::default();
        assert_eq!(d.inbox.active_minutes, 30);
        assert!(d.notify.waiting && d.notify.error && d.notify.stuck && d.notify.done);
        assert!(d.notify.quiet.is_empty() && !d.notify.is_quiet(0));
        let cfg = Config::parse(
            r#"
            [inbox]
            active_minutes = 90
            [notify]
            done = false
            quiet = "22:30-07:00"
            repeat_after = 0
            "#,
        )
        .unwrap();
        assert_eq!(cfg.inbox.active_minutes, 90);
        assert!(!cfg.notify.done && cfg.notify.waiting, "unset toggles stay on");
        assert_eq!(cfg.notify.repeat_after, 0);
        assert!(cfg.notify.is_quiet(23 * 60) && cfg.notify.is_quiet(6 * 60 + 59));
        assert!(!cfg.notify.is_quiet(7 * 60) && !cfg.notify.is_quiet(22 * 60 + 29));
        let day = NotifyConfig { quiet: "12:00-13:00".into(), ..NotifyConfig::default() };
        assert!(day.is_quiet(12 * 60 + 30) && !day.is_quiet(13 * 60));
        assert!(Config::parse("[notify]\nquiet = \"late\"").is_err());
        assert!(Config::parse("[notify]\nquiet = \"25:00-07:00\"").is_err());
        assert!(Config::parse("[notify]\nsounds = true").is_err(), "typos are reported");
    }

    #[test]
    fn hex() {
        assert_eq!(parse_hex("#C4A5B4"), Some(0xc4a5b4));
        assert_eq!(parse_hex("c4a5b4"), Some(0xc4a5b4));
        assert_eq!(parse_hex("#c4a5b"), None);
        assert_eq!(parse_hex("#c4a5bz"), None);
    }

    #[test]
    fn first_run_creates_file_and_bad_file_falls_back() {
        let dir = std::env::temp_dir().join(format!("aflow-cfg-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (cfg, warn) = Config::load_or_create(&dir);
        assert_eq!(cfg, Config::default());
        assert!(warn.is_none());
        assert_eq!(std::fs::read_to_string(dir.join("config.toml")).unwrap(), DEFAULT_FILE);

        std::fs::write(dir.join("config.toml"), "recap = false\n").unwrap();
        assert!(!Config::load_or_create(&dir).0.recap);

        std::fs::write(dir.join("config.toml"), "recap = nope\n").unwrap();
        let (cfg, warn) = Config::load_or_create(&dir);
        assert_eq!(cfg, Config::default());
        assert!(warn.is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
