// One-shot git helpers for the diff/review view. These shell out to `git`
// (the first std::process::Command in the codebase) and degrade to "no diff"
// when the path isn't in a repo or git is unavailable.
//
// New files matter for a Claude-review tool, but `git diff` ignores untracked
// files — so we detect those and treat the whole file as added.

use std::path::{Path, PathBuf};
use std::process::Command;

/// A single line's change status vs the git index/HEAD. Owned here rather than
/// by a frontend: it's produced by `changed_lines` and merely *rendered* as a
/// gutter bar, so it stays framework-independent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChangeKind {
    Added,
    Modified,
}

pub enum FileChange {
    /// Tracked file with edits: specific changed rows in the new file.
    Hunks(Vec<(usize, ChangeKind)>),
    /// Untracked (new) file: every line is an addition.
    AllAdded,
    /// Unchanged, or not in a git repo.
    None,
}

fn file_dir(path: &Path) -> PathBuf {
    // Run in the file's own directory so git discovers the enclosing repo.
    match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
        _ => PathBuf::from("."),
    }
}

fn run_git_diff(path: &Path, extra: &[&str]) -> Option<String> {
    let file_name = path.file_name()?;
    let output = Command::new("git")
        .current_dir(file_dir(path))
        .arg("diff")
        .args(extra)
        .arg("--no-color")
        .arg("--")
        .arg(file_name)
        .output()
        .ok()?;
    if !output.status.success() {
        return None; // not a git repo, or git missing/errored
    }
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

fn is_untracked(path: &Path) -> bool {
    let Some(file_name) = path.file_name() else {
        return false;
    };
    let Ok(output) = Command::new("git")
        .current_dir(file_dir(path))
        .args(["status", "--porcelain", "--"])
        .arg(file_name)
        .output()
    else {
        return false;
    };
    output.status.success()
        && String::from_utf8_lossy(&output.stdout)
            .lines()
            .any(|l| l.starts_with("??"))
}

/// Whether the file lives in a git repo, plus its per-line change status.
pub struct GitInfo {
    pub in_repo: bool,
    pub change: FileChange,
}

/// `git diff` succeeds (exit 0) for any file inside a work tree and fails for
/// one outside a repo (or when git is absent), so a successful diff doubles as
/// the "in a repo" signal. Pure deletions yield no hunk rows (a missing line has
/// no row to mark — the full diff view shows those).
pub fn changed_lines(path: &Path) -> GitInfo {
    let Some(diff) = run_git_diff(path, &["--unified=0"]) else {
        return GitInfo {
            in_repo: false,
            change: FileChange::None,
        };
    };
    let hunks = parse_hunks(&diff);
    let change = if !hunks.is_empty() {
        FileChange::Hunks(hunks)
    } else if is_untracked(path) {
        // Empty diff + untracked = a new file that git diff skipped.
        FileChange::AllAdded
    } else {
        FileChange::None
    };
    GitInfo {
        in_repo: true,
        change,
    }
}

/// The full unified diff for one file, for the diff view. Untracked files are
/// diffed against /dev/null so a brand-new file still renders as all-added.
pub fn diff_text(path: &Path) -> Option<String> {
    if let Some(text) = run_git_diff(path, &[]) {
        if !text.trim().is_empty() {
            return Some(text);
        }
    }
    if is_untracked(path) {
        return diff_no_index(path);
    }
    None
}

fn diff_no_index(path: &Path) -> Option<String> {
    let file_name = path.file_name()?;
    let output = Command::new("git")
        .current_dir(file_dir(path))
        .args(["diff", "--no-index", "--no-color", "--", "/dev/null"])
        .arg(file_name)
        .output()
        .ok()?;
    // `git diff --no-index` exits 1 when the files differ (always, here).
    match output.status.code() {
        Some(0) | Some(1) => Some(String::from_utf8_lossy(&output.stdout).into_owned()),
        _ => None,
    }
}

fn parse_hunks(diff: &str) -> Vec<(usize, ChangeKind)> {
    let mut out = Vec::new();
    for line in diff.lines() {
        // Hunk header: "@@ -a,b +c,d @@ optional section heading".
        let Some(rest) = line.strip_prefix("@@ ") else {
            continue;
        };
        let Some((ranges, _)) = rest.split_once(" @@") else {
            continue;
        };
        let mut parts = ranges.split(' ');
        let old = parts.next().unwrap_or("");
        let new = parts.next().unwrap_or("");
        let (_, old_count) = parse_range(old.trim_start_matches('-'));
        let (new_start, new_count) = parse_range(new.trim_start_matches('+'));
        if new_count == 0 {
            continue; // pure deletion: no new-file rows
        }
        // A hunk that removed old lines and added new ones is a modification;
        // one that only added lines is an insertion.
        let kind = if old_count > 0 {
            ChangeKind::Modified
        } else {
            ChangeKind::Added
        };
        let start = new_start.saturating_sub(1);
        for row in start..start + new_count {
            out.push((row, kind));
        }
    }
    out
}

// "c,d" -> (c, d); bare "c" -> (c, 1) (unified diff omits a count of 1).
fn parse_range(s: &str) -> (usize, usize) {
    match s.split_once(',') {
        Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(1)),
        None => (s.parse().unwrap_or(0), 1),
    }
}
