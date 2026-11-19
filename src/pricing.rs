//! Token pricing, in USD per million tokens. **Edit this table to update
//! rates.** Per-model rates can also be overridden without rebuilding via
//! `[pricing.<model-or-family>]` in `~/.agentflow/config.toml`.
//!
//! Rates are Anthropic first-party API list prices (as of 2026-11). Cache
//! writes are 1.25× input for the 5-minute TTL and 2× for the 1-hour TTL.
//! Lookup tries the exact model id first, then falls back to the family's
//! current-generation rates (so a new point release still gets a sane price).
//! Costs are estimates: they ignore batch/priority discounts, fast mode, and
//! subscription plans.

use std::collections::HashMap;

use crate::model::Usage;

/// USD per million tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write_5m: f64,
    pub cache_write_1h: f64,
}

const fn r(input: f64, output: f64, cache_read: f64) -> Rates {
    Rates {
        input,
        output,
        cache_read,
        cache_write_5m: input * 1.25,
        cache_write_1h: input * 2.0,
    }
}

/// Exact model ids → rates.
const MODELS: &[(&str, Rates)] = &[
    ("claude-fable-5-2", r(8.0, 40.0, 0.20)),
    ("claude-mythos-5-2", r(8.0, 40.0, 0.20)),
    ("claude-fable-5-1", r(10.0, 50.0, 0.25)),
    ("claude-mythos-5-1", r(10.0, 50.0, 0.25)),
    ("claude-fable-5", r(10.0, 50.0, 1.00)),
    ("claude-mythos-5", r(10.0, 50.0, 1.00)),
    ("claude-opus-5-5", r(4.0, 20.0, 0.20)),
    ("claude-opus-5", r(5.0, 25.0, 0.50)),
    ("claude-opus-4-8", r(5.0, 25.0, 0.50)),
    ("claude-opus-4-7", r(5.0, 25.0, 0.50)),
    ("claude-opus-4-6", r(5.0, 25.0, 0.50)),
    ("claude-sonnet-5-5", r(2.0, 10.0, 0.10)),
    ("claude-sonnet-5", r(2.0, 10.0, 0.20)),
    ("claude-sonnet-4-6", r(3.0, 15.0, 0.30)),
    // Haiku 5.5's cache-read rate isn't published in our reference; 0.1× input assumed.
    ("claude-haiku-5-5", r(0.10, 0.50, 0.01)),
    ("claude-haiku-4-5", r(1.0, 5.0, 0.10)),
];

/// Family fallback (current generation) for ids not listed above.
const FAMILIES: &[(&str, Rates)] = &[
    ("fable", r(8.0, 40.0, 0.20)),
    ("mythos", r(8.0, 40.0, 0.20)),
    ("opus", r(4.0, 20.0, 0.20)),
    ("sonnet", r(2.0, 10.0, 0.10)),
    ("haiku", r(0.10, 0.50, 0.01)),
];

/// The short badge for a model id: "claude-opus-5-5" → "opus". Unknown ids
/// keep their first word after "claude-"; synthetic/empty ids give "".
pub fn family(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    if m.is_empty() || m.starts_with('<') {
        return String::new();
    }
    for (fam, _) in FAMILIES {
        if m.contains(fam) {
            return (*fam).to_string();
        }
    }
    let rest = m.strip_prefix("claude-").unwrap_or(&m);
    rest.split(['-', '.', '@']).next().unwrap_or("").chars().take(6).collect()
}

/// Rate table plus user overrides from the config file.
#[derive(Clone, Debug, Default)]
pub struct Pricing {
    /// keyed by exact model id or family name
    overrides: HashMap<String, Rates>,
}

/// A partial override from the config (unset fields keep the built-in rate).
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RatesOverride {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write_5m: Option<f64>,
    pub cache_write_1h: Option<f64>,
}

impl Pricing {
    pub fn new(overrides: &HashMap<String, RatesOverride>) -> Self {
        let mut map = HashMap::new();
        for (key, o) in overrides {
            let key = key.to_ascii_lowercase();
            let base = builtin(&key).unwrap_or(r(0.0, 0.0, 0.0));
            let input = o.input.unwrap_or(base.input);
            map.insert(
                key,
                Rates {
                    input,
                    output: o.output.unwrap_or(base.output),
                    cache_read: o.cache_read.unwrap_or(base.cache_read),
                    // If only `input` is overridden, keep writes at the usual multiples.
                    cache_write_5m: o.cache_write_5m.unwrap_or(if o.input.is_some() {
                        input * 1.25
                    } else {
                        base.cache_write_5m
                    }),
                    cache_write_1h: o.cache_write_1h.unwrap_or(if o.input.is_some() {
                        input * 2.0
                    } else {
                        base.cache_write_1h
                    }),
                },
            );
        }
        Pricing { overrides: map }
    }

    /// Rates for a model id: override (exact, then family), then built-in
    /// (exact, then family). `None` for unknown / synthetic models.
    pub fn rates(&self, model: &str) -> Option<Rates> {
        let m = model.to_ascii_lowercase();
        let fam = family(&m);
        self.overrides
            .get(&m)
            .or_else(|| self.overrides.get(&fam))
            .copied()
            .or_else(|| builtin(&m))
    }

    /// USD for one call's usage. Unknown models cost 0 (we'd rather show
    /// nothing than a made-up number).
    pub fn cost(&self, model: &str, u: &Usage) -> f64 {
        let Some(r) = self.rates(model) else {
            return 0.0;
        };
        (u.input as f64 * r.input
            + u.output as f64 * r.output
            + u.cache_read as f64 * r.cache_read
            + u.cache_write_5m as f64 * r.cache_write_5m
            + u.cache_write_1h as f64 * r.cache_write_1h)
            / 1_000_000.0
    }

    /// USD the cache reads saved vs. sending those tokens as plain input.
    pub fn cache_savings(&self, model: &str, u: &Usage) -> f64 {
        let Some(r) = self.rates(model) else {
            return 0.0;
        };
        u.cache_read as f64 * (r.input - r.cache_read).max(0.0) / 1_000_000.0
    }
}

fn builtin(model: &str) -> Option<Rates> {
    if let Some((_, r)) = MODELS.iter().find(|(id, _)| *id == model) {
        return Some(*r);
    }
    // Ids sometimes carry a date or platform suffix ("claude-opus-4-6-2026…",
    // "claude-opus-5-5@…"): longest listed id that prefixes it wins.
    // "claude-opus-5" must not swallow "claude-opus-5-5", so a bare "-5" isn't
    // a suffix; only a date (8+ digits), "@…" or "[…]" is.
    let is_suffix = |rest: &str| {
        rest.starts_with('@')
            || rest.starts_with('[')
            || rest
                .strip_prefix('-')
                .is_some_and(|d| d.len() >= 8 && d.bytes().take(8).all(|b| b.is_ascii_digit()))
    };
    if let Some((_, r)) = MODELS
        .iter()
        .filter(|(id, _)| model.strip_prefix(id).is_some_and(is_suffix))
        .max_by_key(|(id, _)| id.len())
    {
        return Some(*r);
    }
    let fam = family(model);
    FAMILIES.iter().find(|(f, _)| *f == fam).map(|(_, r)| *r)
}

/// "$0.42", "$12.3", "$123"; "<$0.01" for a nonzero sub-cent amount.
pub fn fmt_cost(c: f64) -> String {
    if c > 0.0 && c < 0.005 {
        "<$0.01".into()
    } else if c < 10.0 {
        format!("${c:.2}")
    } else if c < 100.0 {
        format!("${c:.1}")
    } else {
        format!("${c:.0}")
    }
}

/// The per-call flash: "+$0.02"; sub-cent calls in cents ("+0.4¢", "+<.1¢").
pub fn fmt_delta(c: f64) -> String {
    if c >= 0.0095 {
        format!("+{}", fmt_cost(c))
    } else if c >= 0.00095 {
        format!("+{:.1}¢", c * 100.0)
    } else {
        "+<.1¢".into()
    }
}

/// 950 → "950", 12_300 → "12.3k", 4_100_000 → "4.1M"
pub fn fmt_tokens(n: u64) -> String {
    if n < 1000 {
        format!("{n}")
    } else if n < 1_000_000 {
        format!("{:.1}k", n as f64 / 1e3)
    } else {
        format!("{:.1}M", n as f64 / 1e6)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u(input: u64, output: u64, cr: u64, cw5: u64, cw1: u64) -> Usage {
        Usage { input, output, cache_read: cr, cache_write_5m: cw5, cache_write_1h: cw1 }
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn family_badges() {
        assert_eq!(family("claude-opus-5-5"), "opus");
        assert_eq!(family("claude-sonnet-4-6"), "sonnet");
        assert_eq!(family("claude-haiku-5-5"), "haiku");
        assert_eq!(family("claude-fable-5-2"), "fable");
        assert_eq!(family("<synthetic>"), "");
        assert_eq!(family(""), "");
        assert_eq!(family("claude-nova-1"), "nova");
    }

    #[test]
    fn cost_of_each_token_kind() {
        let p = Pricing::default();
        // opus 5.5: $4 in, $20 out, $0.20 cache read, $5 / $8 cache writes
        let m = "claude-opus-5-5";
        assert!(close(p.cost(m, &u(1_000_000, 0, 0, 0, 0)), 4.0));
        assert!(close(p.cost(m, &u(0, 1_000_000, 0, 0, 0)), 20.0));
        assert!(close(p.cost(m, &u(0, 0, 1_000_000, 0, 0)), 0.20));
        assert!(close(p.cost(m, &u(0, 0, 0, 1_000_000, 0)), 5.0));
        assert!(close(p.cost(m, &u(0, 0, 0, 0, 1_000_000)), 8.0));
        // a realistic call: 2 in, 520 out, 37,671 written to the 5m cache
        let c = p.cost(m, &u(2, 520, 0, 37_671, 0));
        assert!(close(c, (2.0 * 4.0 + 520.0 * 20.0 + 37_671.0 * 5.0) / 1e6));
    }

    #[test]
    fn exact_id_beats_family_and_prefixes_dont_collide() {
        let p = Pricing::default();
        let one = u(0, 1_000_000, 0, 0, 0);
        assert!(close(p.cost("claude-opus-5", &one), 25.0));
        assert!(close(p.cost("claude-opus-5-5", &one), 20.0));
        assert!(close(p.cost("claude-sonnet-4-6", &one), 15.0));
        // unknown point release → family's current rate
        assert!(close(p.cost("claude-sonnet-9-9", &one), 10.0));
        // dated suffix still resolves to the listed id
        assert!(close(p.cost("claude-opus-4-6-20260101", &one), 25.0));
        // unknown / synthetic model: free rather than invented
        assert_eq!(p.cost("<synthetic>", &one), 0.0);
        assert_eq!(p.cost("gpt-x", &one), 0.0);
    }

    #[test]
    fn overrides_by_family_and_by_id() {
        let mut o = HashMap::new();
        o.insert("opus".to_string(), RatesOverride { input: Some(1.0), ..Default::default() });
        o.insert(
            "claude-haiku-5-5".to_string(),
            RatesOverride { output: Some(9.0), ..Default::default() },
        );
        let p = Pricing::new(&o);
        // family override applies to any opus id; writes follow the new input
        let r = p.rates("claude-opus-5").unwrap();
        assert_eq!((r.input, r.cache_write_5m, r.cache_write_1h), (1.0, 1.25, 2.0));
        // untouched fields keep the built-in rate
        assert_eq!(p.rates("claude-opus-5-5").unwrap().output, 20.0);
        let h = p.rates("claude-haiku-5-5").unwrap();
        assert_eq!((h.input, h.output), (0.10, 9.0));
        assert!(close(p.cost("claude-sonnet-5-5", &u(0, 1_000_000, 0, 0, 0)), 10.0));
    }

    #[test]
    fn cache_savings_is_read_tokens_at_the_input_discount() {
        let p = Pricing::default();
        // sonnet 5.5: input $2, cache read $0.10 → $1.90 saved per M read
        assert!(close(p.cache_savings("claude-sonnet-5-5", &u(0, 0, 1_000_000, 0, 0)), 1.9));
        assert_eq!(p.cache_savings("<synthetic>", &u(0, 0, 1_000_000, 0, 0)), 0.0);
    }

    #[test]
    fn formatting() {
        assert_eq!(fmt_cost(0.4249), "$0.42");
        assert_eq!(fmt_cost(12.34), "$12.3");
        assert_eq!(fmt_cost(123.4), "$123");
        assert_eq!(fmt_delta(0.02), "+$0.02");
        assert_eq!(fmt_delta(0.0042), "+0.4¢");
        assert_eq!(fmt_delta(0.00002), "+<.1¢");
        assert_eq!(fmt_cost(0.003), "<$0.01");
        assert_eq!(fmt_cost(0.0), "$0.00");
        assert_eq!(fmt_tokens(950), "950");
        assert_eq!(fmt_tokens(12_300), "12.3k");
        assert_eq!(fmt_tokens(4_100_000), "4.1M");
    }
}
