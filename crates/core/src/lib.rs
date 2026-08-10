//! Framework-independent core for sacrament: everything that isn't rendering
//! or input handling. Shared by the v1 TUI (`crates/tui`) and the v2 iced GUI
//! (`crates/gui`) while the rewrite is in flight.
//!
//! The one hard rule: no UI-framework dependency. No `ratatui`, no `crossterm`,
//! no `iced`. That's enforced by this crate's `Cargo.toml` rather than by
//! discipline — a leaked `ratatui::style::Color` won't compile here.

pub mod client;
pub mod config;
pub mod font;
pub mod git;
pub mod highlight;
pub mod jira;
pub mod lint;
pub mod markdown;
pub mod paths;
pub mod proc;
pub mod protocol;
pub mod secret;
pub mod session;
pub mod text;
pub mod theme;
pub mod work;

/// App ids for per-user runtime paths. v1 and v2 run side by side during the
/// rewrite, so they must not share a socket or a session file — see
/// [`paths`] for what is namespaced and what is deliberately shared.
/// Identity for the socket and session file, **not** the binary name.
///
/// These deliberately did not change when the binaries were renamed at the
/// cutover, and swapping them would be destructive rather than tidy: `APP_GUI`
/// becoming `"sacrament"` would point v2 at `sacrament-session.toml`, which is
/// v1's file — v2 would restore v1's tabs and shells over its own — and both
/// would then contend for the same socket. The names are internal identity that
/// happens to look like the old binary names.
pub const APP_TUI: &str = "sacrament";
pub const APP_GUI: &str = "sacrament2";
