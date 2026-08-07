//! Session state: what was open and where the window was.
//!
//! Distinct from `config.toml` on purpose. Config is *preference* — hand-written,
//! comment-bearing, shared with v1. Session is *state* the app owns and rewrites,
//! and serializing TOML discards comments, so writing geometry into config would
//! delete the user's own annotations on the first resize.
//!
//! Every field is `#[serde(default)]`, so a session file written by an older build
//! still loads — a missing `[geometry]` just means "use the defaults".

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

use crate::paths::session_path;

/// Window and pane geometry.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(default)]
pub struct Geometry {
    pub window_width: f32,
    pub window_height: f32,
    /// Fraction of the width given to the left column (editor + bottom shell);
    /// the right shell pane gets the rest.
    pub vertical_split: f32,
    /// Fraction of the left column's height given to the editor.
    pub horizontal_split: f32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self {
            window_width: 1100.0,
            window_height: 720.0,
            vertical_split: 0.5,
            horizontal_split: 0.5,
        }
    }
}

impl Geometry {
    /// Clamp to something usable. A stored zero or a NaN would otherwise produce a
    /// window you can't see or a pane you can't grab — and a session file is
    /// user-editable, so it can contain anything.
    pub fn sanitized(&self) -> Self {
        let d = Self::default();
        let size = |v: f32, fallback: f32| {
            if v.is_finite() && v >= 200.0 {
                v
            } else {
                fallback
            }
        };
        let ratio = |v: f32| {
            if v.is_finite() {
                v.clamp(0.05, 0.95)
            } else {
                0.5
            }
        };
        Self {
            window_width: size(self.window_width, d.window_width),
            window_height: size(self.window_height, d.window_height),
            vertical_split: ratio(self.vertical_split),
            horizontal_split: ratio(self.horizontal_split),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Default)]
#[serde(default)]
pub struct Session {
    #[serde(default)]
    pub active: usize,
    #[serde(default)]
    pub buffers: Vec<SessionBuffer>,
    #[serde(default)]
    pub bottom_shells: Vec<ShellTabSession>,
    #[serde(default)]
    pub bottom_active: usize,
    #[serde(default)]
    pub right_shells: Vec<ShellTabSession>,
    #[serde(default)]
    pub right_active: usize,
    #[serde(default)]
    pub geometry: Geometry,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ShellTabSession {
    pub cwd: PathBuf,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct SessionBuffer {
    pub path: PathBuf,
    #[serde(default)]
    pub cursor_row: usize,
    #[serde(default)]
    pub cursor_col: usize,
    #[serde(default)]
    pub scroll_row: usize,
    #[serde(default)]
    pub scroll_col: usize,
    #[serde(default)]
    pub folds: Vec<(usize, usize)>,
    #[serde(default)]
    pub syntax_override: Option<String>,
    /// Showing rendered markdown rather than source. v2 only; v1 ignores it,
    /// and `serde(default)` means a session written by either still loads.
    #[serde(default)]
    pub read_mode: bool,
}

pub fn load(app: &str) -> Option<Session> {
    let path = session_path(app)?;
    let s = fs::read_to_string(&path).ok()?;
    toml::from_str(&s).ok()
}

pub fn save(app: &str, session: &Session) -> Result<()> {
    let Some(path) = session_path(app) else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let s = toml::to_string_pretty(session)?;
    fs::write(path, s)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_file_loads_as_defaults() {
        let s: Session = toml::from_str("").unwrap();
        assert_eq!(s.geometry, Geometry::default());
        assert!(s.buffers.is_empty());
    }

    #[test]
    fn a_session_without_geometry_still_loads() {
        // Written by a build predating geometry; must not fail to parse.
        let s: Session = toml::from_str("active = 0\n").unwrap();
        assert_eq!(s.geometry, Geometry::default());
    }

    #[test]
    fn a_session_without_read_mode_still_loads() {
        // Written by a build predating read mode, or by v1, which never sets it.
        let s: SessionBuffer = toml::from_str("path = \"/tmp/a.md\"\n").unwrap();
        assert!(!s.read_mode);
    }

    #[test]
    fn read_mode_round_trips() {
        let s = Session {
            buffers: vec![SessionBuffer {
                path: "/tmp/a.md".into(),
                cursor_row: 0,
                cursor_col: 0,
                scroll_row: 0,
                scroll_col: 0,
                folds: vec![],
                syntax_override: None,
                read_mode: true,
            }],
            ..Default::default()
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert!(back.buffers[0].read_mode);
    }

    #[test]
    fn geometry_round_trips() {
        let mut g = Geometry::default();
        g.window_width = 1234.0;
        g.vertical_split = 0.3;
        let s = Session {
            geometry: g.clone(),
            ..Default::default()
        };
        let text = toml::to_string_pretty(&s).unwrap();
        let back: Session = toml::from_str(&text).unwrap();
        assert_eq!(back.geometry, g);
    }

    #[test]
    fn degenerate_geometry_is_clamped() {
        let g = Geometry {
            window_width: 0.0,
            window_height: f32::NAN,
            vertical_split: 5.0,
            horizontal_split: -1.0,
        }
        .sanitized();
        let d = Geometry::default();
        assert_eq!(g.window_width, d.window_width);
        assert_eq!(g.window_height, d.window_height);
        assert!(g.vertical_split <= 0.95 && g.vertical_split >= 0.05);
        assert!(g.horizontal_split >= 0.05);
    }

    #[test]
    fn plausible_geometry_is_left_alone() {
        let g = Geometry {
            window_width: 1440.0,
            window_height: 900.0,
            vertical_split: 0.62,
            horizontal_split: 0.4,
        };
        assert_eq!(g.sanitized(), g);
    }
}
