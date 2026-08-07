//! The cell grid contract: what a drawable surface looks like, independent of
//! where its content comes from.
//!
//! This is the seam that makes "shell panes and the editor are the same widget"
//! true rather than merely asserted. `GridView` draws a `GridSource`; the
//! terminal is one implementation and a text buffer is another, and the widget
//! knows nothing about either.

use iced::Color;

use crate::palette::Palette;

/// One rendered cell. Colors are already resolved — a source does its own
/// palette lookups so the widget's draw loop stays free of them.
#[derive(Clone, Copy, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Cell {
    pub fn blank(fg: Color, bg: Color) -> Self {
        Self {
            c: ' ',
            fg,
            bg,
            bold: false,
            italic: false,
            underline: false,
        }
    }

    /// Whether this cell needs a text draw at all. Spaces and the spacer half
    /// of a wide character contribute nothing but background.
    pub fn is_blank(&self) -> bool {
        self.c == ' ' || self.c == '\0'
    }

    /// Run-batching key: adjacent cells agreeing on all of this can coalesce
    /// into a single `fill_text` call.
    pub fn run_key(&self) -> (u32, u32, u32, u32, bool, bool, bool) {
        let c = self.fg;
        (
            c.r.to_bits(),
            c.g.to_bits(),
            c.b.to_bits(),
            c.a.to_bits(),
            self.bold,
            self.italic,
            self.underline,
        )
    }
}

/// Anything `GridView` can draw.
pub trait GridSource {
    /// Fill `out` with at most `rows` rows of at most `cols` cells each.
    ///
    /// `out` is a scratch buffer owned by the caller and reused across frames —
    /// clear and refill it rather than allocating. Rows shorter than `cols` are
    /// fine; the widget treats missing cells as background.
    fn fill(&self, palette: &Palette, rows: usize, cols: usize, out: &mut Vec<Vec<Cell>>);

    /// Viewport-relative `(row, col)` of the caret, or `None` when it shouldn't
    /// be drawn (hidden, scrolled out of view, pane unfocused).
    ///
    /// Every surface draws the same block caret — the editor matches the terminal
    /// rather than using a thin bar, so the two panes read as one app. Making the
    /// shape configurable later means adding it back here and in `GridView::draw`;
    /// carrying an unused variant for that eventuality isn't worth it.
    fn cursor(&self) -> Option<(usize, usize)>;
}

/// Resize `out` to `rows` empty rows, reusing the existing allocations.
///
/// Called by every `GridSource::fill` implementation, which is why it lives
/// here: a source that allocates fresh `Vec`s per frame would churn the
/// allocator at the frame rate for no reason.
pub fn reset_rows(out: &mut Vec<Vec<Cell>>, rows: usize) {
    for row in out.iter_mut() {
        row.clear();
    }
    if out.len() < rows {
        out.resize_with(rows, Vec::new);
    } else {
        out.truncate(rows);
    }
}
