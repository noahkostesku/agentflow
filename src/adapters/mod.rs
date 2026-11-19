//! Source adapters: one per harness. Each reads the session files its harness
//! already writes (never writes, never sits in the request path) and
//! translates them into schema events. Nothing outside this module knows a
//! harness's on-disk format. See `docs/adapters.md`.

pub mod claude;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::schema::{Event, Harness};

/// A watched session in one harness's storage.
pub trait SourceAdapter {
    fn harness(&self) -> Harness;

    /// The harness's id for the session.
    fn session_id(&self) -> &str;

    /// The session's primary file (what auto-switch compares against).
    fn path(&self) -> &Path;

    /// Read whatever is new on disk and return it as events, in order. The
    /// first call starts with `session_started` and replays the session so
    /// far; later calls return only what was appended. Cheap to call a few
    /// times per second.
    fn poll(&mut self) -> Vec<Event>;
}

// ---- helpers shared by adapters --------------------------------------------

/// Read appended bytes of a file from `*offset` and return the complete lines,
/// advancing the offset past the last newline. Handles truncation, and never
/// gets stuck on invalid UTF-8.
pub fn tail(path: &Path, offset: &mut u64) -> Vec<String> {
    let Ok(mut f) = File::open(path) else {
        return Vec::new();
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len < *offset {
        *offset = 0; // file shrank: re-read from start
    }
    if f.seek(SeekFrom::Start(*offset)).is_err() {
        return Vec::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return Vec::new();
    }
    let Some(last_nl) = buf.iter().rposition(|&b| b == b'\n') else {
        return Vec::new();
    };
    *offset += last_nl as u64 + 1;
    String::from_utf8_lossy(&buf[..last_nl])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Parse "2026-11-19T06:21:38.273Z" (always UTC) to epoch ms.
pub fn iso_to_ms(s: &str) -> Option<i64> {
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (year, mon, day) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (hour, min, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    let mut ms = 0i64;
    if s.as_bytes().get(19) == Some(&b'.') {
        let frac: String = s[20..].chars().take_while(char::is_ascii_digit).take(3).collect();
        if !frac.is_empty() {
            ms = format!("{frac:0<3}").parse().unwrap_or(0);
        }
    }
    let days = days_from_civil(year, mon, day);
    Some((days * 86400 + hour * 3600 + min * 60 + sec) * 1000 + ms)
}

/// Days since 1970-01-01 (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// A shell command that runs a test suite.
pub fn is_test_command(cmd: &str) -> bool {
    let c = cmd.to_lowercase();
    [
        "cargo test", "cargo nextest", "pytest", "py.test", "unittest", "npm test",
        "npm run test", "yarn test", "pnpm test", "bun test", "deno test", "go test",
        "jest", "vitest", "mocha", "rspec", "phpunit", "mix test", "ctest", "make test",
        "make check", "gradle test", "mvn test", "dotnet test", "swift test", "tox",
    ]
    .iter()
    .any(|k| c.contains(k))
}
