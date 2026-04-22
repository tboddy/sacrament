use std::env;
use std::fs;
use std::path::PathBuf;

use serde::Deserialize;

#[derive(Deserialize, Debug, Clone)]
#[serde(default)]
pub struct Config {
    pub tab_width: usize,
    pub indent_with_tabs: bool,
    pub line_numbers: bool,
    pub status_timeout_ms: u64,
    pub syntax_highlighting: bool,
    pub word_wrap: bool,
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
        }
    }
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

fn config_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sacrament").join("config.toml"))
}
