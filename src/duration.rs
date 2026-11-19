//! The one way a span of time is written, everywhere it's shown: tree rows,
//! stuck and busy labels, the recap, the detail view, the timeline, stats,
//! lifetime, wrapped, the inbox and notifications.
//!
//!   under a minute   "42s"
//!   under an hour    "17m 55s"  (seconds always two digits: "2m 00s", so the
//!                                width holds steady as it ticks)
//!   an hour or more  "1h 02m"   (seconds dropped)
//!
//! That padded form ([`human`]) is for aligned columns, where a value ticks in
//! place: tree rows, status labels, the inbox's time column. Labels and prose
//! use [`compact`], which drops zero parts and padding: "30m", "2m 5s", "1h 2m".
//!
//! Partial seconds round down; a negative span reads "0s".

/// The same, for labels and prose, where nothing has to line up: zero parts
/// and padding dropped. "42s", "30m", "2m 5s", "1h", "1h 2m".
pub fn compact(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    let (big, small, units) = if s < 60 {
        return format!("{s}s");
    } else if s < 3600 {
        (s / 60, s % 60, ("m", "s"))
    } else {
        (s / 3600, (s % 3600) / 60, ("h", "m"))
    };
    if small == 0 {
        format!("{big}{}", units.0)
    } else {
        format!("{big}{} {small}{}", units.0, units.1)
    }
}

/// `ms` as a short human duration (see the module doc), padded for aligned
/// columns where it ticks in place.
pub fn human(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::human;

    #[test]
    fn the_format() {
        let s = |secs: i64| human(secs * 1000);
        assert_eq!(s(59), "59s");
        assert_eq!(s(60), "1m 00s");
        assert_eq!(s(120), "2m 00s");
        assert_eq!(s(3599), "59m 59s");
        assert_eq!(s(3600), "1h 00m");
        assert_eq!(s(3720), "1h 02m");
        // edges
        assert_eq!(s(0), "0s");
        assert_eq!(human(59_999), "59s", "partial seconds round down");
        assert_eq!(human(-5), "0s");
        assert_eq!(s(1075), "17m 55s");
        assert_eq!(s(3600 + 59), "1h 00m", "seconds are dropped once hours appear");
        assert_eq!(s(26 * 3600 + 5 * 60), "26h 05m", "no days: hours keep counting");
    }

    #[test]
    fn the_compact_format() {
        use super::compact;
        let s = |secs: i64| compact(secs * 1000);
        assert_eq!(s(59), "59s");
        assert_eq!(s(60), "1m");
        assert_eq!(s(125), "2m 5s");
        assert_eq!(s(30 * 60), "30m");
        assert_eq!(s(3599), "59m 59s");
        assert_eq!(s(3600), "1h");
        assert_eq!(s(3720), "1h 2m");
        assert_eq!(s(3600 + 59), "1h", "seconds are dropped once hours appear");
        assert_eq!(compact(-5), "0s");
    }
}
