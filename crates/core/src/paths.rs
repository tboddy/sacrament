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

/// The user's home directory, where a fresh shell starts.
///
/// A new shell deliberately does *not* inherit the editor's own working
/// directory. That's wherever the app happened to be launched from — often the
/// last project, sometimes `/` when started from the Finder — which makes the
/// starting directory depend on trivia the user can't see. Home is somewhere
/// they can predict.
pub fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME").map(PathBuf::from).filter(|p| p.is_dir())
}

fn config_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sacrament"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_is_readable_and_is_a_directory() {
        // A fresh shell starts here, so this returning `None` would silently put
        // every new tab wherever the editor was launched from instead.
        let home = home_dir().expect("HOME should resolve on this machine");
        assert!(home.is_dir());
        assert!(home.is_absolute());
    }

    #[test]
    fn the_socket_and_session_are_namespaced_by_app() {
        // The two versions run at once; sharing either would have them fighting
        // over tabs or routing opens to the wrong editor.
        assert_ne!(socket_path(crate::APP_TUI), socket_path(crate::APP_GUI));
        assert_ne!(session_path(crate::APP_TUI), session_path(crate::APP_GUI));
        assert!(session_path(crate::APP_GUI).unwrap().to_string_lossy().contains("sacrament2"));
    }
}
