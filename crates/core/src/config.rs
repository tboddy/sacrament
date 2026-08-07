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
    let Some(path) = config_path() else {
        return Config::default();
    };
    let Ok(s) = fs::read_to_string(&path) else {
        return Config::default();
    };
    toml::from_str(&s).unwrap_or_default()
}
