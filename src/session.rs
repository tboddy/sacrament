use std::env;
use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Default)]
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
}

fn session_path() -> Option<PathBuf> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("sacrament").join("session.toml"))
}

pub fn load() -> Option<Session> {
    let path = session_path()?;
    let s = fs::read_to_string(&path).ok()?;
    toml::from_str(&s).ok()
}

pub fn save(session: &Session) -> Result<()> {
    let Some(path) = session_path() else {
        return Ok(());
    };
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let s = toml::to_string_pretty(session)?;
    fs::write(path, s)?;
    Ok(())
}
