// On-demand linting. A configured command (see config::LintConfig) is run on a
// background thread; the parsed diagnostics come back over an mpsc channel,
// mirroring how shell/fs events are delivered. This is one of the first
// std::process::Command uses in the codebase (alongside git.rs).

use std::path::{Path, PathBuf};
use std::process::Command;

/// Lint severity. `Ord` matters: `Error > Warning`, so a gutter showing one
/// glyph per row can pick the most severe diagnostic on that row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Warning,
    Error,
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    pub row: usize,
    pub col: usize,
    pub severity: Severity,
    pub message: String,
}

pub struct LintMsg {
    pub path: PathBuf,
    pub diagnostics: Vec<Diagnostic>,
}

/// Run a linter `command` (a template; `{file}` -> the file name) on `path`,
/// in the file's directory, capturing stdout+stderr. Blocking — call from a
/// worker thread. Returns the parsed diagnostics (empty on spawn failure).
pub fn run(command: &str, path: &Path) -> Vec<Diagnostic> {
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dir = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    };
    // Run through the shell so templates can include flags and pipes. Commands
    // without `{file}` (e.g. project-wide `cargo clippy`) run as-is.
    let cmd_str = command.replace("{file}", &file_name);
    let output = match Command::new("sh")
        .arg("-c")
        .arg(&cmd_str)
        .current_dir(&dir)
        .output()
    {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.push('\n');
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    parse_output(&text, &file_name)
}

/// Parse `path:line:col: message` / `path:line: message` lines, keeping only
/// those whose path basename matches `file_name` (so multi-file tool output —
/// clippy, tsc — is filtered to the linted buffer).
pub fn parse_output(text: &str, file_name: &str) -> Vec<Diagnostic> {
    text.lines()
        .filter_map(|line| parse_line(line, file_name))
        .collect()
}

fn parse_line(line: &str, file_name: &str) -> Option<Diagnostic> {
    let mut parts = line.splitn(4, ':');
    let path = parts.next()?;
    let base = Path::new(path.trim()).file_name().and_then(|n| n.to_str())?;
    if base != file_name {
        return None;
    }
    let line_no: usize = parts.next()?.trim().parse().ok()?;
    let third = parts.next()?;
    // The third field is either a column (with the message in the fourth) or
    // the message itself (no column).
    let (col, message) = match third.trim().parse::<usize>() {
        Ok(c) => (c, parts.next().unwrap_or("").trim().to_string()),
        Err(_) => {
            let mut msg = third.trim().to_string();
            if let Some(rest) = parts.next() {
                msg.push(':');
                msg.push_str(rest);
            }
            (1, msg.trim().to_string())
        }
    };
    if message.is_empty() {
        return None;
    }
    Some(Diagnostic {
        row: line_no.saturating_sub(1),
        col: col.saturating_sub(1),
        severity: classify(&message),
        message,
    })
}

// Leading "error" => Error (rustc/clippy/shellcheck/gcc); everything else is a
// warning. Coarse but good enough — the gutter color carries the rest.
fn classify(message: &str) -> Severity {
    if message.trim_start().to_ascii_lowercase().starts_with("error") {
        Severity::Error
    } else {
        Severity::Warning
    }
}
