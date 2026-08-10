use std::collections::HashMap;
use std::fs;

use serde::Deserialize;

use crate::font::FontConfig;
use crate::jira::JiraConfig;
use crate::paths::config_path;
use crate::theme::Theme;

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Config {
    pub tab_width: usize,
    pub indent_with_tabs: bool,
    pub line_numbers: bool,
    pub status_timeout_ms: u64,
    pub syntax_highlighting: bool,
    pub word_wrap: bool,
    pub lint: LintConfig,
    /// Colors for the v2 GUI. v1 ignores this — it renders through the
    /// terminal's own palette by design.
    pub theme: Theme,
    /// Font for the v2 GUI. v1 ignores this — a TUI draws in whatever font the
    /// terminal is set to.
    pub font: FontConfig,
    /// Jira site, account, and query for v2's Jira section. v1 ignores it.
    ///
    /// The API **token is deliberately not here** — `config.toml` is plaintext
    /// and shared with v1, so secrets come from the Keychain via
    /// [`crate::secret`]. See `docs/jira-integration.md`.
    pub jira: JiraConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            tab_width: 4,
            indent_with_tabs: false,
            line_numbers: true,
            status_timeout_ms: 2000,
            syntax_highlighting: true,
            word_wrap: true,
            lint: LintConfig::default(),
            theme: Theme::default(),
            font: FontConfig::default(),
            jira: JiraConfig::default(),
        }
    }
}

// `[lint.linters]` maps a syntect language name (e.g. "Rust", "Python") or a
// file extension to a command template. Example config.toml:
//
//   [lint.linters.Rust]
//   command = "cargo clippy --message-format=short"
//   [lint.linters.Python]
//   command = "ruff check {file}"
//
// `{file}` is replaced with the file name; the command runs in the file's
// directory. Output is parsed for `path:line:col: message` lines.
#[derive(Deserialize, Debug, Clone, Default)]
#[serde(default)]
pub struct LintConfig {
    pub linters: HashMap<String, LinterSpec>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct LinterSpec {
    pub command: String,
}

pub fn load() -> Config {
    load_result().unwrap_or_default()
}

/// Read the config, telling a **parse failure apart from a missing file**.
///
/// [`load`] collapses the two into "defaults", which is right at startup — there
/// is nothing yet to lose — and actively harmful once the file is reloaded while
/// the app runs. Saving a half-typed `config.toml` would otherwise reset the
/// theme, the font and every editor setting to their built-in values, with
/// nothing on screen to connect that to the keystroke that caused it. The caller
/// keeps its existing config and reports the error instead.
///
/// A **missing** file stays `Ok(default)`: never having written a config is a
/// normal state, not an error.
///
/// The error is a sentence for a human. `toml`'s own message names the line and
/// what it expected, which is far more use than anything this could invent.
pub fn load_result() -> Result<Config, String> {
    let Some(path) = config_path() else {
        return Ok(Config::default());
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        // Unreadable covers "not there", which is the common case and not a
        // problem. A permissions error is rare enough to read the same way.
        Err(_) => return Ok(Config::default()),
    };
    toml::from_str(&text).map_err(|e| format!("{}:\n\n{e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_broken_config_is_an_error_rather_than_silent_defaults() {
        // The distinction `load_result` exists for: reloading a half-typed file
        // must not reset the theme and every editor setting to built-in values
        // with nothing on screen to explain it.
        let broken: Result<Config, _> = toml::from_str::<Config>("tab_width = \"two\"");
        assert!(broken.is_err());
    }

    #[test]
    fn an_absent_table_leaves_every_default_in_place() {
        // Every field is `#[serde(default)]`, which is what lets a partial config
        // work — and what makes a *parse* failure indistinguishable from an empty
        // file unless the error is kept, hence `load_result`.
        let config: Config = toml::from_str("").expect("an empty config is valid");
        assert_eq!(config.tab_width, 4);
        assert!(config.word_wrap);
        assert_eq!(config.theme, Theme::default());
    }

    #[test]
    fn a_partial_config_keeps_the_rest_of_the_defaults() {
        let config: Config = toml::from_str("tab_width = 2").expect("valid");
        assert_eq!(config.tab_width, 2);
        // Untouched neighbours must not come back as zero/false.
        assert!(config.line_numbers);
        assert!(config.syntax_highlighting);
    }
}
