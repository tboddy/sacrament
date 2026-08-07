//! Bridging `core`'s framework-independent highlight styles into ratatui.
//!
//! `core::highlight` hands back a [`Slot`] (0-15) and an [`Emphasis`] instead of
//! ratatui types, so this is where v1 resolves them. The mapping is lossless
//! because v1 was already restricted to the 16 named ANSI colors — the terminal
//! palette *is* the theme here, which is exactly what a `Slot` denotes. v1
//! deliberately ignores `[theme]` in config.toml; that's v2's business.

use ratatui::style::{Color, Modifier};
use sacrament_core::highlight::{Emphasis, Slot};

/// The 16 ANSI slots as ratatui's named colors, so the terminal resolves them.
pub fn slot_to_color(slot: Slot) -> Color {
    match slot.index() {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        _ => Color::White,
    }
}

pub fn emphasis_to_modifier(e: Emphasis) -> Modifier {
    let mut m = Modifier::empty();
    if e.bold {
        m |= Modifier::BOLD;
    }
    if e.italic {
        m |= Modifier::ITALIC;
    }
    if e.underline {
        m |= Modifier::UNDERLINED;
    }
    m
}
