//! Per-user runtime paths, namespaced by app id.
//!
//! v1 (TUI) and v2 (GUI) are installed and run simultaneously during the
//! rewrite. Anything that represents *live state* must therefore be namespaced,
//! or the two instances fight over it:
//!
//! - **socket** — whichever booted first would own `/tmp/sacrament-$USER.sock`,
//!   so every `sacrament <file>` and every `--review` hook would silently route
//!   to the wrong editor.
//! - **session** — both would read and write the same `session.toml` and stomp
//!   each other's open tabs on quit.
//!
//! Config is the deliberate exception: it's shared. v1's `Config` derives
//! `#[serde(default)]` without `deny_unknown_fields`, so unknown keys are
//! ignored and v2 can add its own `[gui]` section (font, theme, …) to the same
//! file without breaking v1.

use std::env;
use std::path::PathBuf;

/// `/tmp/<app>-$USER.sock` — the client/server rendezvous point.
pub fn socket_path(app: &str) -> PathBuf {
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/{app}-{user}.sock"))
}

/// `$XDG_CONFIG_HOME/sacrament/<app>-session.toml`.
pub fn session_path(app: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(format!("{app}-session.toml")))
}

/// `$XDG_CONFIG_HOME/sacrament/config.toml` — shared by both versions.
pub fn config_path() -> Option<PathBuf> {
    Some(config_dir()?.join("config.toml"))
}

fn config_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sacrament"))
}
