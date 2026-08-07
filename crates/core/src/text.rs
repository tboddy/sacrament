//! Visual text metrics: how wide a character is on screen, and where a line
//! breaks when soft-wrapped.
//!
//! Pure functions over `&str`, so they belong in `core`. Everything here works in
//! **character indices** and **visual columns**, never bytes — a tab occupies
//! several columns but one index, and a wide CJK glyph occupies two columns but
//! one index, so the two coordinate systems genuinely differ.
//!
//! v1 has its own copies of these inside `crates/tui/src/editor.rs`. That
//! duplication is deliberate and temporary: v1 is frozen and gets deleted at the
//! cutover, and editing a shipping daily driver to deduplicate code that's about
//! to be removed is the wrong risk. If v1 outlives that plan, point it here.

/// Columns a character occupies, starting from visual column `vis_col`.
///
/// Tabs are the reason this takes a position: a tab advances to the next multiple
/// of `tab_width`, so its width depends on where it starts. Zero-width and
/// unprintable characters report 0.
pub fn char_display_width(c: char, vis_col: usize, tab_width: usize) -> usize {
    if c == '\t' {
        let tw = tab_width.max(1);
        tw - (vis_col % tw)
    } else {
        unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)
    }
}

/// Visual column at which character `char_idx` begins.
pub fn char_idx_to_vis_col(s: &str, char_idx: usize, tab_width: usize) -> usize {
    let mut vis = 0;
    for c in s.chars().take(char_idx) {
        vis += char_display_width(c, vis, tab_width);
    }
    vis
}

/// Character index at visual column `target_vis`, clamped to the line's length.
///
/// Lands on the character *containing* the column, so clicking the middle of a
/// tab selects the tab rather than falling through it.
pub fn vis_col_to_char_idx(s: &str, target_vis: usize, tab_width: usize) -> usize {
    let mut vis = 0;
    for (i, c) in s.chars().enumerate() {
        let w = char_display_width(c, vis, tab_width);
        if vis + w > target_vis {
            return i;
        }
        vis += w;
    }
    s.chars().count()
}

/// Visual column of `col`, measured from the start of the segment beginning at
/// `seg_start`. Wrapped rows restart at column 0, so segment-relative is what the
/// renderer and the caret both need.
pub fn vis_in_segment(line: &str, seg_start: usize, col: usize, tab_width: usize) -> usize {
    let mut vis = 0;
    for c in line
        .chars()
        .skip(seg_start)
        .take(col.saturating_sub(seg_start))
    {
        vis += char_display_width(c, vis, tab_width);
    }
    vis
}

/// Character index within `[seg_start, seg_end)` at segment-relative visual
/// column `target_vis`. Clamps to `seg_end`.
pub fn col_at_vis_in_segment(
    line: &str,
    seg_start: usize,
    seg_end: usize,
    target_vis: usize,
    tab_width: usize,
) -> usize {
    let mut vis = 0;
    let chars: Vec<char> = line.chars().collect();
    let mut i = seg_start;
    while i < seg_end && i < chars.len() {
        let w = char_display_width(chars[i], vis, tab_width);
        if vis + w > target_vis {
            return i;
        }
        vis += w;
        i += 1;
    }
    i
}

/// Visual width of a line's leading whitespace.
///
/// This is the hanging indent for wrapped continuation rows: a continuation that
/// starts at column 0 while its line is indented reads as a separate statement,
/// which is exactly the confusion indenting avoids.
pub fn leading_indent_cols(line: &str, tab_width: usize) -> usize {
    let mut vis = 0;
    for c in line.chars() {
        if !c.is_whitespace() {
            break;
        }
        vis += char_display_width(c, vis, tab_width);
    }
    vis
}

/// Cap a hanging indent so a continuation always has room for text.
///
/// A deeply-indented line in a narrow pane could otherwise leave zero columns,
/// and wrapping into no space doesn't terminate usefully.
pub fn clamp_hanging_indent(indent: usize, width: usize) -> usize {
    if width < 4 {
        return 0;
    }
    indent.min(width / 2)
}

/// Break a line into soft-wrapped segments, as `[start, end)` character ranges
/// that tile the whole line.
///
/// `hanging_indent` is the number of columns reserved on every row *after* the
/// first, so continuations line up under the line's own indentation. The first
/// segment gets the full width; later ones get `width - hanging_indent`.
///
/// Breaks on whitespace where possible, and hard-breaks a run too long to fit —
/// otherwise a single unbroken 300-character token would produce no break at all
/// and run off the edge. An empty line yields one empty segment, so every line
/// occupies at least one screen row.
///
/// `width == 0` disables wrapping (one segment covering the line), which is how
/// wrap-off is expressed without a second code path.
pub fn wrap_line(
    line: &str,
    width: usize,
    tab_width: usize,
    hanging_indent: usize,
) -> Vec<(usize, usize)> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    if n == 0 || width == 0 {
        return vec![(0, n)];
    }

    let indent = clamp_hanging_indent(hanging_indent, width);
    let mut segs = Vec::new();
    let mut start = 0;
    while start < n {
        // Continuations lose the indent from their usable width, or the text
        // would be pushed past the right edge.
        let width = if segs.is_empty() {
            width
        } else {
            width.saturating_sub(indent).max(1)
        };
        let mut vis = 0;
        let mut last_ws_end: Option<usize> = None;
        let mut j = start;
        while j < n {
            let w = char_display_width(chars[j], vis, tab_width);
            // `j > start` guarantees progress: a single character wider than the
            // whole width still consumes one slot rather than looping forever.
            if vis + w > width && j > start {
                break;
            }
            vis += w;
            j += 1;
            if chars[j - 1].is_whitespace() {
                last_ws_end = Some(j);
            }
        }
        let end = if j < n {
            match last_ws_end {
                Some(b) if b > start => b,
                _ => j.max(start + 1),
            }
        } else {
            j
        };
        segs.push((start, end));
        start = end;
    }
    segs
}

#[cfg(test)]
mod tests {
    use super::*;

    const TW: usize = 4;

    #[test]
    fn tab_advances_to_the_next_stop() {
        assert_eq!(char_display_width('\t', 0, TW), 4);
        assert_eq!(char_display_width('\t', 1, TW), 3);
        assert_eq!(char_display_width('\t', 3, TW), 1);
        assert_eq!(char_display_width('\t', 4, TW), 4);
    }

    #[test]
    fn wide_and_zero_width_characters() {
        assert_eq!(char_display_width('a', 0, TW), 1);
        assert_eq!(char_display_width('世', 0, TW), 2);
        assert_eq!(char_display_width('\u{200b}', 0, TW), 0);
    }

    #[test]
    fn vis_col_and_char_idx_are_inverses_over_plain_text() {
        let s = "hello";
        for i in 0..=s.chars().count() {
            let vis = char_idx_to_vis_col(s, i, TW);
            assert_eq!(vis_col_to_char_idx(s, vis, TW), i);
        }
    }

    #[test]
    fn tabs_make_the_mapping_non_identity() {
        let s = "a\tb";
        assert_eq!(char_idx_to_vis_col(s, 0, TW), 0);
        assert_eq!(char_idx_to_vis_col(s, 1, TW), 1);
        assert_eq!(char_idx_to_vis_col(s, 2, TW), 4, "tab fills to the next stop");
        assert_eq!(char_idx_to_vis_col(s, 3, TW), 5);
        // Any column inside the tab resolves to the tab itself.
        for vis in 1..4 {
            assert_eq!(vis_col_to_char_idx(s, vis, TW), 1);
        }
    }

    #[test]
    fn segments_tile_the_line_exactly() {
        // The invariant the renderer depends on: no gaps, no overlaps, full cover.
        for line in [
            "",
            "short",
            "the quick brown fox jumps over the lazy dog",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "tabs\there\tand\tthere",
            "世界世界世界世界世界",
        ] {
            for width in 1..20 {
                let segs = wrap_line(line, width, TW, 0);
                assert!(!segs.is_empty(), "{line:?} @ {width} produced no segments");
                assert_eq!(segs[0].0, 0);
                assert_eq!(segs.last().unwrap().1, line.chars().count());
                for pair in segs.windows(2) {
                    assert_eq!(pair[0].1, pair[1].0, "gap in {line:?} @ {width}");
                }
            }
        }
    }

    #[test]
    fn wrapping_prefers_whitespace_breaks() {
        let segs = wrap_line("hello world", 8, TW, 0);
        assert_eq!(segs.len(), 2);
        // Break after the space, not mid-word.
        assert_eq!(&"hello world"[0..segs[0].1], "hello ");
    }

    #[test]
    fn an_unbreakable_run_hard_breaks() {
        let segs = wrap_line("aaaaaaaaaa", 4, TW, 0);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[0], (0, 4));
        assert_eq!(segs[1], (4, 8));
        assert_eq!(segs[2], (8, 10));
    }

    #[test]
    fn a_character_wider_than_the_viewport_still_advances() {
        // Width 1 with a 2-column glyph must not loop forever.
        let segs = wrap_line("世世世", 1, TW, 0);
        assert_eq!(segs.len(), 3);
        assert_eq!(segs.last().unwrap().1, 3);
    }

    #[test]
    fn empty_line_occupies_one_row() {
        assert_eq!(wrap_line("", 10, TW, 0), vec![(0, 0)]);
    }

    #[test]
    fn zero_width_disables_wrapping() {
        let line = "a fairly long line that would otherwise wrap";
        assert_eq!(wrap_line(line, 0, TW, 0), vec![(0, line.chars().count())]);
    }

    #[test]
    fn segment_relative_columns_restart_at_zero() {
        let line = "hello world";
        let segs = wrap_line(line, 8, TW, 0);
        let (s, e) = segs[1];
        assert_eq!(vis_in_segment(line, s, s, TW), 0, "segment starts at column 0");
        // And the inverse lands back on the same character.
        assert_eq!(col_at_vis_in_segment(line, s, e, 0, TW), s);
    }

    #[test]
    fn leading_indent_measures_visually() {
        assert_eq!(leading_indent_cols("no indent", TW), 0);
        assert_eq!(leading_indent_cols("    four", TW), 4);
        assert_eq!(leading_indent_cols("\tone tab", TW), 4);
        assert_eq!(leading_indent_cols("  \tmixed", TW), 4, "tab completes the stop");
        assert_eq!(leading_indent_cols("     ", TW), 5, "all whitespace");
    }

    #[test]
    fn a_hanging_indent_narrows_continuations_only() {
        let line = "aaaa bbbb cccc dddd eeee";
        let plain = wrap_line(line, 10, TW, 0);
        let hung = wrap_line(line, 10, TW, 4);
        // First segment is unaffected; the rest have less room, so there are more.
        assert_eq!(plain[0], hung[0]);
        assert!(
            hung.len() >= plain.len(),
            "indent costs width: {plain:?} vs {hung:?}"
        );
    }

    #[test]
    fn hanging_indent_still_tiles_the_line() {
        for line in ["    indented and quite long indeed", "\ttabbed and long as well"] {
            for width in 4..24 {
                let indent = leading_indent_cols(line, TW);
                let segs = wrap_line(line, width, TW, indent);
                assert_eq!(segs[0].0, 0);
                assert_eq!(segs.last().unwrap().1, line.chars().count());
                for pair in segs.windows(2) {
                    assert_eq!(pair[0].1, pair[1].0, "gap in {line:?} @ {width}");
                }
            }
        }
    }

    #[test]
    fn a_huge_indent_is_capped_so_text_still_fits() {
        // Indent wider than the pane must not starve the continuation.
        assert_eq!(clamp_hanging_indent(100, 20), 10);
        assert_eq!(clamp_hanging_indent(100, 3), 0, "too narrow to indent at all");
        let segs = wrap_line("                    aaaa bbbb cccc", 10, TW, 20);
        assert_eq!(segs.last().unwrap().1, 34, "still covers the line");
    }

    #[test]
    fn col_at_vis_clamps_to_the_segment_end() {
        let line = "abc";
        assert_eq!(col_at_vis_in_segment(line, 0, 2, 99, TW), 2);
    }
}
