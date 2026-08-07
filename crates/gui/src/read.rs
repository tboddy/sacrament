//! Markdown read mode: rendered markdown as a `GridSource`.
//!
//! The rendering is `core::markdown`, which emits **logical** lines — a whole
//! paragraph is one line, however long. This module wraps them with
//! `text::wrap_line`, the same function the editor uses on source code.
//!
//! That split is the point. v1 wrapped inside its markdown renderer and drew the
//! result into a widget that did no wrapping of its own, so anything the
//! renderer emitted whole ran off the edge — which is why word wrap looks broken
//! in v1's read mode. Here wrapping is a property of the layout, so it applies
//! to every construct without the renderer having to remember, and a resize
//! re-wraps without re-parsing.
//!
//! Scroll state lives here rather than being borrowed from the editor. v1 reused
//! `scroll_row` for both, which changed its meaning by mode — in read mode it
//! indexes *rendered* rows, which usually outnumber the source lines — and any
//! code feeding it back into a source-line calculation was wrong by
//! construction. Two fields cost nothing and the ambiguity disappears.

use std::sync::{Arc, Mutex};

use sacrament_core::markdown;
use sacrament_core::text;

use crate::buffer::Buffer;
use crate::grid::{Cell, GridSource, reset_rows};
use crate::palette::Palette;

/// A rendered document, cached against what produced it.
pub struct Rendered {
    pub lines: Vec<markdown::Line>,
    /// The width it was laid out for. Rules, code slabs and table columns are
    /// width-shaped, so a resize has to re-render — but only a resize.
    width: usize,
    /// The buffer revision it was rendered from.
    revision: u64,
}

impl Rendered {
    pub fn new(lines: Vec<markdown::Line>, width: usize, revision: u64) -> Self {
        Self {
            lines,
            width,
            revision,
        }
    }

    pub fn is_stale(&self, width: usize, revision: u64) -> bool {
        self.width != width || self.revision != revision
    }
}

/// Absolute screen-row index of `(line, seg)`.
pub fn row_index(lines: &[markdown::Line], line: usize, seg: usize, cols: usize) -> usize {
    let mut index = 0;
    for l in lines.iter().take(line.min(lines.len())) {
        index += segments_of(l, &l.text(), cols).len();
    }
    index + seg
}

/// Inverse of [`row_index`]: which `(line, seg)` an absolute row lands on.
pub fn row_at(lines: &[markdown::Line], target: usize, cols: usize) -> (usize, usize) {
    let mut seen = 0;
    for (i, l) in lines.iter().enumerate() {
        let segs = segments_of(l, &l.text(), cols).len();
        if target < seen + segs {
            return (i, target - seen);
        }
        seen += segs;
    }
    (lines.len().saturating_sub(1), 0)
}

/// Render `source` at `width`.
pub fn render(source: &str, width: usize) -> Vec<markdown::Line> {
    markdown::render(source, width.max(1))
}

/// One screen row of the rendered document.
#[derive(Clone, Copy, Debug)]
pub struct ReadRow {
    /// Index into the rendered lines.
    pub line: usize,
    /// Char range of this row within that line.
    pub start: usize,
    pub end: usize,
    /// Blank columns before the text — zero on a first row, the line's own
    /// hanging indent on a continuation.
    pub indent: usize,
}

/// Walk `rows` screen rows from `(line, seg)`, wrapping at `cols`.
///
/// The single producer of the rendered-row mapping, in the same spirit as
/// `Buffer::visible_rows`: scrolling, drawing and the end-of-content clamp all
/// read it, so they can't disagree about where a row begins.
pub fn visible_rows(
    lines: &[markdown::Line],
    from_line: usize,
    from_seg: usize,
    rows: usize,
    cols: usize,
) -> Vec<ReadRow> {
    let mut out = Vec::with_capacity(rows);
    let mut line = from_line.min(lines.len().saturating_sub(1));
    let mut seg = from_seg;
    while out.len() < rows && line < lines.len() {
        let text = lines[line].text();
        let indent = text::clamp_hanging_indent(lines[line].indent, cols);
        let segments = segments_of(&lines[line], &text, cols);
        if seg >= segments.len() {
            seg = 0;
            line += 1;
            continue;
        }
        while seg < segments.len() && out.len() < rows {
            let (start, end) = segments[seg];
            out.push(ReadRow {
                line,
                start,
                end,
                indent: if seg == 0 { 0 } else { indent },
            });
            seg += 1;
        }
        if seg >= segments.len() {
            seg = 0;
            line += 1;
        }
    }
    out
}

/// How a rendered line breaks into rows.
///
/// A table row is unwrappable, so it stays one row however wide it is and is
/// reached by scrolling sideways instead. Wrapping it would put half its cells
/// on a row of their own and destroy the column alignment that makes it a table.
fn segments_of(line: &markdown::Line, text: &str, cols: usize) -> Vec<(usize, usize)> {
    if line.wrappable {
        text::wrap_line(text, cols, 1, line.indent)
    } else {
        vec![(0, text.chars().count())]
    }
}

/// How many screen rows the whole document occupies at `cols`.
pub fn total_rows(lines: &[markdown::Line], cols: usize) -> usize {
    lines
        .iter()
        .map(|l| segments_of(l, &l.text(), cols).len())
        .sum()
}

/// Widest rendered row, in columns — how far right scrolling may go.
pub fn widest_row(lines: &[markdown::Line], cols: usize) -> usize {
    lines
        .iter()
        .filter(|l| !l.wrappable)
        .map(|l| l.text().chars().count())
        .max()
        .unwrap_or(0)
        .max(cols)
}

/// Draws a buffer's markdown rendering.
pub struct ReadSource {
    pub buffer: Arc<Mutex<Buffer>>,
}

impl GridSource for ReadSource {
    fn fill(&self, palette: &Palette, rows: usize, cols: usize, out: &mut Vec<Vec<Cell>>) {
        reset_rows(out, rows);
        let Ok(mut buffer) = self.buffer.lock() else {
            return;
        };
        // Re-render only when the text or the width changed; scrolling doesn't
        // touch it.
        buffer.ensure_rendered(cols);
        let Some(rendered) = buffer.rendered() else {
            return;
        };
        let (from_line, from_seg) = buffer.read_scroll();
        let shift = buffer.read_scroll_col();
        let fg = palette.foreground;
        let bg = palette.background;

        for (screen_row, rr) in visible_rows(&rendered.lines, from_line, from_seg, rows, cols)
            .iter()
            .enumerate()
        {
            let Some(line) = rendered.lines.get(rr.line) else {
                continue;
            };
            let text = line.text();
            let dest = &mut out[screen_row];
            // `col` counts columns of the line, `screen` counts columns of the
            // pane; they differ by the horizontal scroll.
            let mut col = rr.indent;
            let mut screen = col.saturating_sub(shift).min(cols);
            for _ in 0..screen {
                dest.push(Cell::blank(fg, bg));
            }
            for (index, c) in text.chars().enumerate() {
                if index < rr.start {
                    continue;
                }
                if index >= rr.end || screen >= cols {
                    break;
                }
                let style = line.style_at(index);
                let mut cell = Cell::blank(fg, bg);
                cell.c = c;
                if let Some(slot) = style.fg {
                    cell.fg = palette.ansi_slot(slot.index());
                }
                if let Some(slot) = style.bg {
                    cell.bg = palette.ansi_slot(slot.index());
                }
                cell.bold = style.emphasis.bold;
                cell.italic = style.emphasis.italic;
                cell.underline = style.emphasis.underline;
                let width = text::char_display_width(c, col, 1).max(1);
                for i in 0..width {
                    // Skip columns scrolled off the left rather than drawing and
                    // clipping them.
                    if col + i < shift {
                        continue;
                    }
                    if screen >= cols {
                        break;
                    }
                    let mut c = cell;
                    // The glyph belongs to the first column; the rest are the
                    // spacer half of a wide character.
                    if i > 0 || col < shift {
                        c.c = '\0';
                    }
                    dest.push(c);
                    screen += 1;
                }
                col += width;
            }
        }
    }

    /// No caret: read mode is read-only, and drawing one would invite typing.
    fn cursor(&self) -> Option<(usize, usize)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str, width: usize) -> Vec<markdown::Line> {
        render(src, width)
    }

    #[test]
    fn a_long_paragraph_wraps_to_the_pane() {
        // The bug this whole design exists to prevent: v1 emitted the paragraph
        // pre-wrapped (or not at all) and the layout clipped the rest.
        let lines = doc(&"word ".repeat(40), 20);
        let rows = visible_rows(&lines, 0, 0, 100, 20);
        assert!(rows.len() > 5, "one paragraph became many rows: {}", rows.len());
        for row in &rows {
            let text = lines[row.line].text();
            let width: usize = text
                .chars()
                .skip(row.start)
                .take(row.end - row.start)
                .count();
            assert!(width + row.indent <= 20, "row overflows the pane");
        }
    }

    #[test]
    fn narrowing_the_pane_produces_more_rows() {
        let src = "some fairly long paragraph text that will need several rows";
        assert!(
            total_rows(&doc(src, 20), 20) > total_rows(&doc(src, 60), 60),
            "wrapping tracks the width"
        );
    }

    #[test]
    fn a_wrapped_list_item_hangs_under_its_text() {
        let lines = doc("- an item long enough to need wrapping across rows", 20);
        let rows = visible_rows(&lines, 0, 0, 100, 20);
        let item: Vec<&ReadRow> = rows.iter().filter(|r| lines[r.line].indent == 2).collect();
        assert!(item.len() > 1, "needs at least one continuation");
        assert_eq!(item[0].indent, 0, "first row starts at the bullet");
        assert_eq!(item[1].indent, 2, "continuation clears it");
    }

    #[test]
    fn every_row_tiles_its_line_exactly() {
        // Same invariant the editor's wrapping holds: no gaps, no overlaps.
        let lines = doc("# Head\n\nparagraph one here\n\n- a\n- b\n\n> quoted text", 16);
        let rows = visible_rows(&lines, 0, 0, 500, 16);
        for (line_idx, line) in lines.iter().enumerate() {
            let mine: Vec<&ReadRow> = rows.iter().filter(|r| r.line == line_idx).collect();
            if mine.is_empty() {
                continue;
            }
            assert_eq!(mine[0].start, 0);
            assert_eq!(mine.last().unwrap().end, line.text().chars().count());
            for pair in mine.windows(2) {
                assert_eq!(pair[0].end, pair[1].start, "gap in the tiling");
            }
        }
    }

    #[test]
    fn scrolling_starts_where_it_is_told() {
        let lines = doc("a\n\nb\n\nc", 40);
        let all = visible_rows(&lines, 0, 0, 100, 40);
        let from_second = visible_rows(&lines, all[2].line, 0, 100, 40);
        assert_eq!(from_second[0].line, all[2].line);
    }
}

#[cfg(test)]
mod hscroll_tests {
    use super::*;

    const TABLE: &str = "| name | description | notes |\n|---|---|---|\n\
                         | one | a fairly long description cell | plus more |";

    #[test]
    fn a_table_row_stays_one_row_however_narrow_the_pane() {
        // Wrapping a table row puts half its cells on a row of their own and
        // the column alignment is gone, so it scrolls sideways instead.
        let lines = render(TABLE, 200);
        let table: Vec<&markdown::Line> = lines.iter().filter(|l| !l.wrappable).collect();
        assert!(!table.is_empty(), "the table rows are marked unwrappable");
        let rows = visible_rows(&lines, 0, 0, 100, 20);
        for (line_idx, line) in lines.iter().enumerate() {
            if line.wrappable {
                continue;
            }
            let mine = rows.iter().filter(|r| r.line == line_idx).count();
            assert_eq!(mine, 1, "table row {line_idx} occupies one row");
        }
    }

    #[test]
    fn prose_still_wraps_beside_a_table() {
        let lines = render(&format!("{TABLE}\n\nsome prose that is long enough to wrap"), 200);
        let rows = visible_rows(&lines, 0, 0, 200, 20);
        let prose = lines
            .iter()
            .position(|l| l.text().starts_with("some prose"))
            .expect("prose line");
        assert!(
            rows.iter().filter(|r| r.line == prose).count() > 1,
            "prose is unaffected by the table rule"
        );
    }

    #[test]
    fn horizontal_scroll_reaches_past_the_pane_but_no_further() {
        let lines = render(TABLE, 200);
        let widest = widest_row(&lines, 20);
        assert!(widest > 20, "the table is wider than the pane");
        // The bound is the widest unwrappable row minus the pane, so the last
        // column can be brought on screen and no further.
        assert_eq!(widest.saturating_sub(20), widest - 20);
    }

    #[test]
    fn a_document_with_no_table_has_nothing_to_scroll_to() {
        let lines = render("just prose, wrapped like everything else", 200);
        assert_eq!(widest_row(&lines, 30), 30, "bound collapses to the pane width");
    }
}
