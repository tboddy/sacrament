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

/// `$XDG_CONFIG_HOME/sacrament/<app>-runs/<name>/` — one ticket run's artifacts:
/// the prompt it was given and the transcript of what it did.
///
/// Under the config directory because that's already where this app keeps state it
/// owns and rewrites (`<app>-session.toml`), so there's one place to look rather
/// than two. Namespaced by app id for the same reason everything else here is,
/// even though only v2 has runs — the rule is cheaper to keep than to remember the
/// exception to.
///
/// **Not cleaned up.** A run's transcript is the only record of what an unattended
/// agent did to a repository, and deleting it on a timer would remove exactly the
/// evidence someone comes looking for a week later.
pub fn run_dir(app: &str, name: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(format!("{app}-runs")).join(name))
}

/// `$XDG_CACHE_HOME/sacrament/<app>-worktrees/<name>/` — the checkout one ticket
/// run works in, so it never touches the tree the user is working in.
///
/// **Cache, not config, and the distinction is the point.** Everything else here
/// lives under the config directory because it is state the app owns and cannot
/// reproduce — a session, a scratchpad, a transcript. A worktree is the opposite
/// on both counts: it is *derivable* (the commits live on the branch, in the
/// repository's own object store, and survive the directory being deleted) and it
/// is *large* — a full checkout, plus whatever `target/` or `node_modules/` the
/// agent's own test run builds inside it. Putting gigabytes of build output under
/// `~/.config` would also feed it to anything that syncs or backs that directory
/// up.
///
/// Keyed by branch, like [`run_dir`], and safe to key that way because the branch
/// name begins with the project key and `[jira.repos]` maps one project key to one
/// repository — so two runs cannot want the same directory in different trees.
pub fn worktree_dir(app: &str, name: &str) -> Option<PathBuf> {
    Some(cache_dir()?.join(format!("{app}-worktrees")).join(name))
}

/// `$XDG_CONFIG_HOME/sacrament/<app>-scratchpad.txt` — the Scratchpad section's
/// one permanent document.
///
/// Beside the session file rather than somewhere like `~/Documents`: it is state
/// this app owns and rewrites without being asked, which is the same category as
/// `<app>-session.toml` and the opposite of a file the user chose to open. Putting
/// it in a documents folder would imply a file they manage, with a name they
/// picked and a lifetime they control — none of which is true.
///
/// `.txt` deliberately, and the extension is load-bearing: it's what keeps the
/// buffer out of markdown read mode, which gates on the extension itself.
pub fn scratchpad_path(app: &str) -> Option<PathBuf> {
    Some(config_dir()?.join(format!("{app}-scratchpad.txt")))
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

/// The XDG cache root, for things the app can rebuild — currently only worktrees.
///
/// `~/.cache` on macOS is not an Apple convention, but neither is `~/.config`, and
/// this app already chose the XDG layout; a second scheme for one directory would
/// be worse than being consistently unconventional.
fn cache_dir() -> Option<PathBuf> {
    let base = env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
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

    #[test]
    fn a_worktree_is_a_cache_and_a_transcript_is_not() {
        // The two live under different roots on purpose: the transcript is the only
        // record of what an unattended agent did, and the worktree is a checkout
        // that can be recreated from the branch. Landing a full checkout — and the
        // build output the agent's own test run leaves in it — under the config
        // directory is what this asserts against.
        let tree = worktree_dir(crate::APP_GUI, "TFE-954-fix-it").expect("resolves");
        let run = run_dir(crate::APP_GUI, "TFE-954-fix-it").expect("resolves");
        assert!(tree.to_string_lossy().contains(".cache"), "got: {}", tree.display());
        assert!(run.to_string_lossy().contains(".config"), "got: {}", run.display());
        assert!(tree.ends_with("TFE-954-fix-it"));
        assert_ne!(
            worktree_dir(crate::APP_TUI, "TFE-1-x"),
            worktree_dir(crate::APP_GUI, "TFE-1-x")
        );
    }
}
