//! Color palette. Terminal background only; one muted accent (dusty mauve by
//! default, configurable); amber = waiting; red = error. A four-step grayscale
//! ramp plus the accent and two tints derived from it.

use std::sync::atomic::{AtomicU32, Ordering};

use ratatui::style::Color;

use crate::config::DEFAULT_ACCENT;

/// bright #e8e8e8 — names, emphasis
pub const B: Color = Color::Rgb(0xe8, 0xe8, 0xe8);
/// gray #b3b3b3 — primary text (status, elapsed)
pub const G: Color = Color::Rgb(0xb3, 0xb3, 0xb3);
/// mid gray #737373 — structure (branches, rules, tree, log times) and dimmed rows
pub const D: Color = Color::Rgb(0x73, 0x73, 0x73);
/// deep dim #2a2a2a — placeholders / empty progress cells
pub const VD: Color = Color::Rgb(0x2a, 0x2a, 0x2a);
/// amber #ffb000 — waiting (blocked on a call that hasn't returned)
pub const AM: Color = Color::Rgb(0xff, 0xb0, 0x00);
/// red #ff4d6d — error
pub const RED: Color = Color::Rgb(0xff, 0x4d, 0x6d);

static ACCENT: AtomicU32 = AtomicU32::new(DEFAULT_ACCENT);

/// Set the accent from config (0xRRGGBB).
pub fn set_accent(rgb: u32) {
    ACCENT.store(rgb & 0xff_ff_ff, Ordering::Relaxed);
}

fn rgb(v: u32) -> (u8, u8, u8) {
    ((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

/// accent — the one accent color
pub fn ac() -> Color {
    let (r, g, b) = rgb(ACCENT.load(Ordering::Relaxed));
    Color::Rgb(r, g, b)
}

/// bright flash — node pop and the progress shine cell (accent tinted ~77% to white)
pub fn wh() -> Color {
    let a = ACCENT.load(Ordering::Relaxed);
    if a == DEFAULT_ACCENT {
        return Color::Rgb(0xf2, 0xe9, 0xee);
    }
    let (r, g, b) = rgb(a);
    let t = |c: u8| (c as f64 + (255.0 - c as f64) * 0.77).round() as u8;
    Color::Rgb(t(r), t(g), t(b))
}

/// shimmer — the "on" state of the progress bar's leading cell (accent at ~55%)
pub fn shim() -> Color {
    let a = ACCENT.load(Ordering::Relaxed);
    if a == DEFAULT_ACCENT {
        return Color::Rgb(0x6e, 0x58, 0x62);
    }
    dim(ac(), 0.55)
}

/// Scale a color toward black (fades for popups).
pub fn dim(c: Color, k: f64) -> Color {
    match c {
        Color::Rgb(r, g, b) => {
            let s = |v: u8| (v as f64 * k).round().clamp(0.0, 255.0) as u8;
            Color::Rgb(s(r), s(g), s(b))
        }
        other => other,
    }
}
