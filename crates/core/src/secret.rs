//! Reading secrets that must not live in `config.toml`.
//!
//! `config.toml` is plaintext, hand-edited, and **shared with v1**, so an API
//! token in it would sit in the same file the user pastes into issue reports and
//! syncs between machines. Tokens therefore come from the macOS Keychain, read
//! through the `security` CLI rather than a crate: it needs no dependency, it is
//! present on every macOS install, and the prompt-and-allow flow is the system's
//! own rather than something reimplemented here.
//!
//! Non-secret settings for the same service (site, account, query) stay in
//! `config.toml`, where the user can see and edit them.

use std::process::Command;

/// Look up a secret, preferring an environment variable over the Keychain.
///
/// `env` wins when it's set, which is what makes the integration scriptable and
/// testable — a CI run or a one-off `SACRAMENT_JIRA_TOKEN=… sacrament` needs no
/// Keychain entry. The Keychain is the path that matters in normal use: an app
/// launched from the Dock inherits launchd's environment, not a shell's, so an
/// exported variable isn't there at all.
///
/// Returns `None` when neither source has it, which callers report as
/// "not configured" rather than as an error — an unconfigured integration is a
/// normal state, not a failure.
pub fn lookup(env: &str, service: &str, account: &str) -> Option<String> {
    if let Ok(value) = std::env::var(env) {
        let value = value.trim().to_string();
        if !value.is_empty() {
            return Some(value);
        }
    }
    keychain(service, account)
}

/// Read a generic password from the macOS Keychain.
///
/// `-w` prints the bare secret and nothing else. A missing item exits non-zero,
/// which is the common case rather than an error worth reporting — hence the
/// `Option` instead of a `Result`.
#[cfg(target_os = "macos")]
fn keychain(service: &str, account: &str) -> Option<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    // Trailing newline from the CLI. Trimming the whole string rather than
    // stripping one newline, since a token never has meaningful whitespace at
    // either end and a stray space would fail authentication invisibly.
    let secret = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!secret.is_empty()).then_some(secret)
}

/// No Keychain equivalent is wired up off macOS, so the environment variable is
/// the only source there. Reported as "not configured", which is accurate.
#[cfg(not(target_os = "macos"))]
fn keychain(_service: &str, _account: &str) -> Option<String> {
    None
}

/// The command that stores a secret where [`lookup`] will find it.
///
/// Returned as a string for an alert to show, because "not configured" is only
/// actionable if the fix comes with it — otherwise the user has to go and find
/// the `security` invocation themselves.
pub fn store_hint(service: &str, account: &str) -> String {
    format!("security add-generic-password -s {service} -a {account} -w '<token>'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_environment_variable_outranks_the_keychain() {
        // Names chosen so a real Keychain can't hold them, which keeps the test
        // honest on a developer machine that has the real entry installed.
        let env = "SACRAMENT_TEST_SECRET_PRESENT";
        unsafe { std::env::set_var(env, "from-env") };
        assert_eq!(
            lookup(env, "sacrament-test-absent", "nobody"),
            Some("from-env".to_string())
        );
        unsafe { std::env::remove_var(env) };
    }

    #[test]
    fn a_blank_environment_variable_is_not_a_secret() {
        // An exported-but-empty variable is a common shell accident. Treating it
        // as a secret would send an empty token and produce a 401 that looks like
        // a wrong token rather than a missing one.
        let env = "SACRAMENT_TEST_SECRET_BLANK";
        unsafe { std::env::set_var(env, "   ") };
        assert_eq!(lookup(env, "sacrament-test-absent", "nobody"), None);
        unsafe { std::env::remove_var(env) };
    }

    #[test]
    fn a_missing_secret_is_none_rather_than_an_error() {
        assert_eq!(
            lookup(
                "SACRAMENT_TEST_SECRET_UNSET",
                "sacrament-test-absent",
                "nobody"
            ),
            None
        );
    }
}
