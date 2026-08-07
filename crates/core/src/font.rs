//! Font selection, from the `[font]` table in `config.toml`.
//!
//! Plain data, like [`crate::theme`] — no `iced::Font` here, since core carries
//! no UI-framework dependency. The gui crate converts at the boundary.
//!
//! Only v2 reads this. v1 draws with whatever font the terminal is configured
//! to use, which is not something a TUI can or should override.

use serde::{Deserialize, Serialize};

#[derive(Deserialize, Serialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct FontConfig {
    /// System font family name, e.g. `"Fira Code"` or `"Menlo"`. `None` uses
    /// the platform's default monospace family.
    ///
    /// Must name a family that's actually installed — see
    /// the gui crate's `font::resolve_family` for what happens when it isn't.
    pub family: Option<String>,

    /// Point size.
    pub size: f32,

    /// Row height as a multiple of `size`. This *is* the terminal's cell
    /// height, so it controls line density directly; 1.0 packs rows tight and
    /// anything below that will clip descenders.
    pub line_height: f32,
}

impl Default for FontConfig {
    fn default() -> Self {
        Self {
            family: None,
            size: 13.0,
            line_height: 1.25,
        }
    }
}

impl FontConfig {
    /// Clamp to values that can actually be rendered. A zero or negative size
    /// yields a degenerate cell and a divide-by-zero when deriving grid
    /// dimensions, so this is a correctness guard, not a preference.
    pub fn sanitized(&self) -> Self {
        Self {
            family: self
                .family
                .as_ref()
                .map(|f| f.trim().to_string())
                .filter(|f| !f.is_empty()),
            size: self.size.clamp(4.0, 200.0),
            line_height: self.line_height.clamp(0.8, 4.0),
        }
    }

    /// Cell height in pixels.
    pub fn cell_height(&self) -> f32 {
        self.size * self.line_height
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_renderable() {
        let f = FontConfig::default();
        assert!(f.cell_height() > 0.0);
        assert_eq!(f.sanitized(), f);
    }

    #[test]
    fn clamps_degenerate_values() {
        let f = FontConfig {
            family: None,
            size: 0.0,
            line_height: 0.0,
        }
        .sanitized();
        assert!(f.size >= 4.0);
        assert!(f.line_height >= 0.8);
        assert!(f.cell_height() > 0.0);
    }

    #[test]
    fn blank_family_is_treated_as_unset() {
        let f = FontConfig {
            family: Some("   ".to_string()),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(f.family, None);
    }

    #[test]
    fn trims_family_whitespace() {
        let f = FontConfig {
            family: Some("  Fira Code  ".to_string()),
            ..Default::default()
        }
        .sanitized();
        assert_eq!(f.family.as_deref(), Some("Fira Code"));
    }

    #[test]
    fn partial_table_keeps_defaults() {
        let f: FontConfig = toml::from_str(r#"size = 15.0"#).unwrap();
        assert_eq!(f.size, 15.0);
        assert_eq!(f.line_height, FontConfig::default().line_height);
        assert_eq!(f.family, None);
    }
}
