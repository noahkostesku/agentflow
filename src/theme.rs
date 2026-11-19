//! Color palette. Terminal background only; one muted accent (dusty mauve);
//! amber = waiting; red = error. A four-step grayscale ramp plus the accent.

use ratatui::style::Color;

/// bright #e8e8e8 — names, emphasis
pub const B: Color = Color::Rgb(0xe8, 0xe8, 0xe8);
/// gray #b3b3b3 — primary text (status, elapsed)
pub const G: Color = Color::Rgb(0xb3, 0xb3, 0xb3);
/// mid gray #737373 — structure (branches, rules, tree, log times) and dimmed rows
pub const D: Color = Color::Rgb(0x73, 0x73, 0x73);
/// deep dim #2a2a2a — placeholders / empty progress cells
pub const VD: Color = Color::Rgb(0x2a, 0x2a, 0x2a);
/// accent — dusty mauve #c4a5b4 (the one accent; replaces cyan)
pub const AC: Color = Color::Rgb(0xc4, 0xa5, 0xb4);
/// amber #ffb000 — waiting (blocked on a call that hasn't returned)
pub const AM: Color = Color::Rgb(0xff, 0xb0, 0x00);
/// red #ff4d6d — error
pub const RED: Color = Color::Rgb(0xff, 0x4d, 0x6d);
/// bright flash #f2e9ee — node pop and the progress shine cell
pub const WH: Color = Color::Rgb(0xf2, 0xe9, 0xee);
/// shimmer #6e5862 — the "on" state of the progress bar's leading cell
pub const SHIM: Color = Color::Rgb(0x6e, 0x58, 0x62);
