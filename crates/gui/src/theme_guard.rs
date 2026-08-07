//! A test that enforces the palette rule, because a convention nobody can check
//! is a convention that decays.
//!
//! **The rule:** every color drawn by this crate comes from [`crate::palette::Palette`],
//! which is built from the user's `[theme]` table. Nothing synthesizes a color —
//! no literals, no alpha blending, no arithmetic on channels.
//!
//! Alpha blending is the one that slips through review. Dimming a color to 45%
//! doesn't *look* like hardcoding, but blending against a background produces a
//! value that appears nowhere in the theme. It's the same violation as writing
//! `#928374`, just harder to spot. Use `Palette::dim()` for muted text and
//! `Palette::background` for hidden text; for a caret over a glyph, use reverse
//! video rather than translucency.
//!
//! `palette.rs` is exempt — turning theme values into renderer colors is its job.

#![cfg(test)]

use std::path::Path;

/// Files allowed to construct colors: the palette itself, and this guard (whose
/// source necessarily contains the patterns it looks for).
const EXEMPT: &[&str] = &["palette.rs", "theme_guard.rs"];

/// Each entry is `(needle, why)`. Split into fragments at the match point so
/// this file's own source doesn't trip the scan it defines.
fn forbidden() -> Vec<(String, &'static str)> {
    vec![
        (
            format!("from_{}", "rgb"),
            "construct colors from the theme via Palette, not from literal channels",
        ),
        (
            format!("Color::{}", "BLACK"),
            "use a Palette slot instead of an iced color constant",
        ),
        (
            format!("Color::{}", "WHITE"),
            "use a Palette slot instead of an iced color constant",
        ),
        (
            format!("Color::{}", "TRANSPARENT"),
            "hide things with Palette::background, not transparency",
        ),
        (
            format!(".a {}=", "*"),
            "alpha blending invents a color outside the theme — use Palette::dim()",
        ),
        (
            format!("a: 0{}", "."),
            "a partial-alpha color is not a theme color — use Palette::dim()",
        ),
        (
            format!(".r {}", "*"),
            "arithmetic on color channels invents a color outside the theme",
        ),
        (
            format!(".g {}", "*"),
            "arithmetic on color channels invents a color outside the theme",
        ),
        (
            format!(".b {}", "*"),
            "arithmetic on color channels invents a color outside the theme",
        ),
    ]
}

#[test]
fn no_colors_are_synthesized_outside_the_palette() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let needles = forbidden();
    let mut violations = Vec::new();

    let entries = std::fs::read_dir(&src).expect("gui src directory should be readable");
    let mut scanned = 0usize;

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
            let trimmed = line.trim_start();
            // Comments and doc comments discuss the rule; they don't break it.
            if trimmed.starts_with("//") {
                continue;
            }
            for (needle, why) in &needles {
                if line.contains(needle.as_str()) {
                    violations.push(format!("{name}:{}: `{needle}` — {why}", line_no + 1));
                }
            }
        }
    }

    // A scan that silently matched nothing would pass forever. Pin that it ran.
    assert!(
        scanned >= 5,
        "expected to scan the gui sources, only saw {scanned} files"
    );
    assert!(
        violations.is_empty(),
        "colors must come from the theme (see theme_guard.rs):\n  {}",
        violations.join("\n  ")
    );
}
