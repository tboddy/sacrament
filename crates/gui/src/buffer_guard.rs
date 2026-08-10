//! A test that keeps buffer construction going through one place, because this
//! convention has already decayed twice and both times it shipped.
//!
//! **The rule:** a `Buffer` handed to the user comes from [`crate::empty_buffer`]
//! (untitled) or from `Buffer::load` followed by the same settings. `buffer.rs`
//! knows nothing about the config — it can't, it's the model — so `Buffer::empty()`
//! defaults `tab_width` to 4 and `wrap_width` to 0, and anything that constructs
//! one has to supply both.
//!
//! What went wrong, twice, in the way an "everyone must remember" rule always does:
//!
//! - Five sites set `tab_width` and `new_buffer` didn't, so `Cmd+N` drew every tab
//!   four columns wide however `tab_width` was configured.
//! - *No* site set `wrap_width`. It arrived only from `Message::EditorResized`,
//!   which fires on a size **change** — so any buffer made after the first layout
//!   didn't wrap until the window was next resized, while files opened at startup
//!   were fine. That asymmetry is what made it read as intermittent.
//!
//! Neither is visible as a fault: the file opens, typing works, and only the
//! layout quietly disagrees with the user's config. So it gets a guard rather than
//! a comment, in the shape `theme_guard` already established here.
//!
//! `buffer.rs` is exempt: its tests construct buffers by the dozen and *want* the
//! bare defaults, which is the whole point of testing the model in isolation.

#![cfg(test)]

use std::path::Path;

/// Files allowed to call the bare constructor: the model's own tests, and this
/// guard, whose source necessarily contains the pattern it looks for.
const EXEMPT: &[&str] = &["buffer.rs", "buffer_guard.rs"];

/// Split at the match point so this file's own source doesn't trip the scan.
fn needle() -> String {
    format!("Buffer::{}()", "empty")
}

#[test]
fn untitled_buffers_are_built_in_exactly_one_place() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let needle = needle();
    let mut found = Vec::new();
    let mut scanned = 0usize;

    let entries = std::fs::read_dir(&src).expect("gui src directory should be readable");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("rs") {
            continue;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        if EXEMPT.contains(&name.as_str()) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("source file should be readable");
        scanned += 1;

        for (line_no, line) in text.lines().enumerate() {
            // Comments discuss the rule; they don't break it.
            if line.trim_start().starts_with("//") {
                continue;
            }
            if line.contains(&needle) {
                found.push(format!("{name}:{}", line_no + 1));
            }
        }
    }

    // A scan that silently matched nothing would pass forever. Pin that it ran.
    assert!(
        scanned >= 5,
        "expected to scan the gui sources, only saw {scanned} files"
    );
    assert_eq!(
        found.len(),
        1,
        "`{needle}` belongs inside `empty_buffer` and nowhere else — a buffer built \
         anywhere else silently ignores `tab_width` and `word_wrap`. Found at:\n  {}",
        found.join("\n  ")
    );
    assert!(
        found[0].starts_with("main.rs"),
        "the one permitted call should be `empty_buffer` in main.rs, found {}",
        found[0]
    );
}
