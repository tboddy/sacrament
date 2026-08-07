//! Color theme, loaded from the `[theme]` table in `config.toml`.
//!
//! Lives in `core` because it's plain data — deliberately *not* `iced::Color`,
//! since core carries no UI-framework dependency. The gui crate converts these
//! into whatever its renderer wants.
//!
//! Only v2 reads this. v1 renders through the terminal's own palette by design
//! (that's the whole "the terminal is the theme" rule), so it parses `[theme]`
//! and ignores it. Harmless: shared config, one consumer.

use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// An 8-bit-per-channel color, written in config as `"#rrggbb"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    /// Accepts `#rrggbb`, `#rgb`, and the same without the leading `#`, so a
    /// palette pasted from any terminal's config parses without editing.
    pub fn parse(s: &str) -> Option<Self> {
        let h = s.trim().trim_start_matches('#');
        let val = |i: usize, n: usize| u8::from_str_radix(&h[i..i + n], 16).ok();
        match h.len() {
            6 => Some(Self::new(val(0, 2)?, val(2, 2)?, val(4, 2)?)),
            // #rgb shorthand: each nibble is doubled (f -> ff).
            3 => {
                let d = |i: usize| val(i, 1).map(|v| v * 17);
                Some(Self::new(d(0)?, d(1)?, d(2)?))
            }
            _ => None,
        }
    }

    pub fn to_hex(self) -> String {
        format!("#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl Serialize for Rgb {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_hex())
    }
}

impl<'de> Deserialize<'de> for Rgb {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct HexVisitor;
        impl Visitor<'_> for HexVisitor {
            type Value = Rgb;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a hex color like \"#rrggbb\"")
            }
            fn visit_str<E: de::Error>(self, v: &str) -> Result<Rgb, E> {
                Rgb::parse(v).ok_or_else(|| E::custom(format!("invalid hex color: {v:?}")))
            }
        }
        d.deserialize_str(HexVisitor)
    }
}

/// The 16 ANSI slots plus the surface colors.
///
/// Every field has a default, so a partial `[theme]` table works — set only
/// `background` and the rest stay at the built-in values.
#[derive(Deserialize, Serialize, Debug, Clone, PartialEq, Eq)]
#[serde(default)]
pub struct Theme {
    pub background: Rgb,
    pub foreground: Rgb,
    pub cursor: Rgb,
    pub selection_background: Rgb,
    pub selection_foreground: Rgb,

    pub black: Rgb,
    pub red: Rgb,
    pub green: Rgb,
    pub yellow: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub cyan: Rgb,
    pub white: Rgb,
    pub bright_black: Rgb,
    pub bright_red: Rgb,
    pub bright_green: Rgb,
    pub bright_yellow: Rgb,
    pub bright_blue: Rgb,
    pub bright_magenta: Rgb,
    pub bright_cyan: Rgb,
    pub bright_white: Rgb,
}

impl Theme {
    /// The 16 ANSI slots in index order — the layout every terminal palette
    /// uses, so `ansi()[n]` is directly indexable by an SGR color number.
    pub fn ansi(&self) -> [Rgb; 16] {
        [
            self.black,
            self.red,
            self.green,
            self.yellow,
            self.blue,
            self.magenta,
            self.cyan,
            self.white,
            self.bright_black,
            self.bright_red,
            self.bright_green,
            self.bright_yellow,
            self.bright_blue,
            self.bright_magenta,
            self.bright_cyan,
            self.bright_white,
        ]
    }
}

/// Built-in default: Gruvbox Dark. Also what a missing `[theme]` table yields.
impl Default for Theme {
    fn default() -> Self {
        Self {
            background: Rgb::new(0x28, 0x28, 0x28),
            foreground: Rgb::new(0xeb, 0xdb, 0xb2),
            cursor: Rgb::new(0xeb, 0xdb, 0xb2),
            selection_background: Rgb::new(0xeb, 0xdb, 0xb2),
            selection_foreground: Rgb::new(0x28, 0x28, 0x28),

            black: Rgb::new(0x28, 0x28, 0x28),
            red: Rgb::new(0xcc, 0x24, 0x1d),
            green: Rgb::new(0x98, 0x97, 0x1a),
            yellow: Rgb::new(0xd7, 0x99, 0x21),
            blue: Rgb::new(0x45, 0x85, 0x88),
            magenta: Rgb::new(0xb1, 0x62, 0x86),
            cyan: Rgb::new(0x68, 0x9d, 0x6a),
            white: Rgb::new(0xa8, 0x99, 0x84),
            bright_black: Rgb::new(0x92, 0x83, 0x74),
            bright_red: Rgb::new(0xfb, 0x49, 0x34),
            bright_green: Rgb::new(0xb8, 0xbb, 0x26),
            bright_yellow: Rgb::new(0xfa, 0xbd, 0x2f),
            bright_blue: Rgb::new(0x83, 0xa5, 0x98),
            bright_magenta: Rgb::new(0xd3, 0x86, 0x9b),
            bright_cyan: Rgb::new(0x8e, 0xc0, 0x7c),
            bright_white: Rgb::new(0xeb, 0xdb, 0xb2),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hex_forms() {
        assert_eq!(Rgb::parse("#cc241d"), Some(Rgb::new(0xcc, 0x24, 0x1d)));
        assert_eq!(Rgb::parse("cc241d"), Some(Rgb::new(0xcc, 0x24, 0x1d)));
        assert_eq!(Rgb::parse("  #CC241D "), Some(Rgb::new(0xcc, 0x24, 0x1d)));
        assert_eq!(Rgb::parse("#f0a"), Some(Rgb::new(0xff, 0x00, 0xaa)));
        assert_eq!(Rgb::parse("#12345"), None);
        assert_eq!(Rgb::parse("nope"), None);
    }

    #[test]
    fn hex_roundtrips() {
        let c = Rgb::new(0x28, 0x28, 0x28);
        assert_eq!(Rgb::parse(&c.to_hex()), Some(c));
    }

    #[test]
    fn partial_theme_table_keeps_defaults() {
        // Extra hashes: the value itself contains `"#`, which would close a
        // plain r#"..."# literal.
        let t: Theme = toml::from_str(r##"background = "#000000""##).unwrap();
        assert_eq!(t.background, Rgb::new(0, 0, 0));
        assert_eq!(t.red, Theme::default().red);
    }

    #[test]
    fn ansi_is_index_ordered() {
        let t = Theme::default();
        assert_eq!(t.ansi()[0], t.black);
        assert_eq!(t.ansi()[9], t.bright_red);
        assert_eq!(t.ansi()[15], t.bright_white);
    }
}
