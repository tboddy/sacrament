//! Resolving terminal cell colors against the active theme.
//!
//! v1's rule was "no RGB — the terminal palette is the theme," which worked
//! because we *were* in a terminal. In a GUI we own the palette, so this is
//! where that decision is made explicitly instead of inherited: the 16 ANSI
//! slots come from `[theme]` in `config.toml` (see `sacrament_core::theme`),
//! and indexed 16-255 plus truecolor resolve faithfully rather than collapsing.
//!
//! That last part is a fix, not just a change. v1's `vt_color_to_ratatui`
//! flattened everything above index 15 to the default color while `TERM`
//! advertised 256-color support, so 256-color TUIs drew blank.

use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Rgb as VteRgb};
use iced::Color;
use sacrament_core::theme::{Rgb, Theme};

/// A theme flattened into renderer-native colors, resolved once at startup so
/// `draw` never touches config or converts per cell.
#[derive(Clone, Debug)]
pub struct Palette {
    pub background: Color,
    pub foreground: Color,
    pub cursor: Color,
    pub selection_background: Color,
    pub selection_foreground: Color,
    ansi: [Color; 16],
}

fn to_color(c: Rgb) -> Color {
    Color::from_rgb8(c.r, c.g, c.b)
}

impl Palette {
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            background: to_color(theme.background),
            foreground: to_color(theme.foreground),
            cursor: to_color(theme.cursor),
            selection_background: to_color(theme.selection_background),
            selection_foreground: to_color(theme.selection_foreground),
            ansi: theme.ansi().map(to_color),
        }
    }

    /// The theme's muted color — `bright_black`, which is what slot 8 is for in
    /// every 16-color scheme. Anything wanting "dimmer text" uses this rather
    /// than blending alpha, because a blend produces a color the theme doesn't
    /// contain.
    pub fn dim(&self) -> Color {
        self.ansi[8]
    }

    /// One of the 16 theme slots by index. Used to resolve `highlight::Slot`,
    /// which is how syntax colors reach the same palette the terminal uses.
    pub fn ansi_slot(&self, i: usize) -> Color {
        self.ansi[i.min(15)]
    }

    /// Resolve a cell color. `is_bg` only picks the fallback for colors with no
    /// better mapping; named defaults already carry their own meaning.
    pub fn resolve(&self, c: AnsiColor, is_bg: bool) -> Color {
        match c {
            AnsiColor::Named(n) => self.named(n, is_bg),
            AnsiColor::Spec(VteRgb { r, g, b }) => Color::from_rgb8(r, g, b),
            AnsiColor::Indexed(i) => self.indexed(i),
        }
    }

    fn named(&self, n: NamedColor, is_bg: bool) -> Color {
        match n {
            NamedColor::Black => self.ansi[0],
            NamedColor::Red => self.ansi[1],
            NamedColor::Green => self.ansi[2],
            NamedColor::Yellow => self.ansi[3],
            NamedColor::Blue => self.ansi[4],
            NamedColor::Magenta => self.ansi[5],
            NamedColor::Cyan => self.ansi[6],
            NamedColor::White => self.ansi[7],
            NamedColor::BrightBlack => self.ansi[8],
            NamedColor::BrightRed => self.ansi[9],
            NamedColor::BrightGreen => self.ansi[10],
            NamedColor::BrightYellow => self.ansi[11],
            NamedColor::BrightBlue => self.ansi[12],
            NamedColor::BrightMagenta => self.ansi[13],
            NamedColor::BrightCyan => self.ansi[14],
            NamedColor::BrightWhite => self.ansi[15],
            NamedColor::Foreground | NamedColor::BrightForeground => self.foreground,
            NamedColor::Background => self.background,
            NamedColor::Cursor => self.cursor,
            // Dim variants map to the normal slot; actual dimming comes from
            // the DIM cell flag in grid_view, so these only matter for an
            // explicit SGR 2 combined with a color.
            NamedColor::DimBlack => self.ansi[0],
            NamedColor::DimRed => self.ansi[1],
            NamedColor::DimGreen => self.ansi[2],
            NamedColor::DimYellow => self.ansi[3],
            NamedColor::DimBlue => self.ansi[4],
            NamedColor::DimMagenta => self.ansi[5],
            NamedColor::DimCyan => self.ansi[6],
            NamedColor::DimWhite => self.ansi[7],
            NamedColor::DimForeground => {
                if is_bg {
                    self.background
                } else {
                    self.foreground
                }
            }
        }
    }

    /// Standard xterm 256-color layout: 0-15 from the theme, 16-231 a 6×6×6
    /// cube, 232-255 a 24-step grayscale ramp.
    fn indexed(&self, i: u8) -> Color {
        match i {
            0..=15 => self.ansi[i as usize],
            16..=231 => {
                let i = i - 16;
                const STEPS: [u8; 6] = [0, 95, 135, 175, 215, 255];
                Color::from_rgb8(
                    STEPS[(i / 36) as usize],
                    STEPS[((i % 36) / 6) as usize],
                    STEPS[(i % 6) as usize],
                )
            }
            _ => {
                let level = 8 + (i - 232) * 10;
                Color::from_rgb8(level, level, level)
            }
        }
    }

    /// Derive iced's own theme from ours, so app chrome (tab strips, the prompt
    /// row, splitters) sits in the same palette as the grid instead of defaulting
    /// to iced's light theme. This is the "chrome derives from the 16 slots"
    /// choice — one coherent system rather than two.
    pub fn iced_theme(&self) -> iced::Theme {
        iced::Theme::custom(
            "sacrament".to_string(),
            iced::theme::Palette {
                background: self.background,
                text: self.foreground,
                primary: self.ansi[12],
                success: self.ansi[10],
                warning: self.ansi[11],
                danger: self.ansi[9],
            },
        )
    }
}

impl Default for Palette {
    fn default() -> Self {
        Self::from_theme(&Theme::default())
    }
}
