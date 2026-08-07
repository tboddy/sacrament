//! A read-only text buffer: the editor pane's data source.
//!
//! Read-only on purpose. This slice proves the rendering path — one file, syntax
//! highlighted, scrolling, aligned with a gutter — without also porting v1's
//! cursor, undo, folding, and edit machinery. That logic is work rather than
//! risk, and it's cheaper to port onto a surface you can already see.
//!
//! ## The visible-row mapping
//!
//! [`Buffer::visible_rows`] is the single producer of "which file line is on
//! which screen row." Both the gutter widget and the grid consume it, which is
//! what keeps them aligned — v1 has the same arrangement (`build_screen_rows`
//! feeding both `render_gutter` and `render_body`), and it's the reason a line
//! number only prints on a row's first segment.
//!
//! ## Soft wrap
//!
//! A line occupies one screen row per wrap segment. Two consequences run through
//! everything below:
//!
//! - **Scrolling has two components**, `scroll_row` (which line) and `scroll_seg`
//!   (which segment within it). Without the second, scrolling past a line that
//!   wraps into ten rows would jump all ten at once.
//! - **Vertical movement is by screen row, not by line.** Pressing Down inside a
//!   wrapped line moves to its next segment, holding the visual column — which is
//!   what the caret appears to do on screen.
//!
//! `wrap_width == 0` means wrapping is off; `text::wrap_line` returns a single
//! segment for that, so there's one code path rather than two.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use sacrament_core::highlight::{HlSpan, Highlighter, LineState};
use sacrament_core::text;

use crate::grid::{Cell, GridSource, reset_rows};
use crate::palette::Palette;

/// One screen row's provenance.
#[derive(Clone, Copy, Debug)]
pub struct VisibleRow {
    /// Index into `Buffer::lines`.
    pub line: usize,
    /// Char range of this row within the line. Once wrap lands, a long line
    /// yields several `VisibleRow`s with different ranges.
    pub start: usize,
    pub end: usize,
    /// False for wrap continuations. The gutter prints a number only when true.
    pub is_first_segment: bool,
    /// Blank columns before the text on this row — the hanging indent. Zero on a
    /// first segment. Every position on the row is offset by it, so caret, click,
    /// and render all read it from here rather than recomputing.
    pub indent: usize,
}

/// The deepest a single level of indentation is taken to be, in columns.
///
/// No indentation convention uses more than eight columns per level, and bracket
/// alignment in real code lands much deeper than that — `compute(` puts its
/// continuation sixteen columns in. That gap is what separates a block from a
/// wrapped expression without knowing the language.
///
/// A constant rather than something measured from the file, because a measured
/// level moves as you type: once tab-indented lines outnumber space-indented
/// ones the level shrinks, and blocks that folded a moment ago stop folding.
const MAX_INDENT_STEP: usize = 8;

/// Which way a buffer is being shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum ViewMode {
    #[default]
    Edit,
    /// Rendered markdown, read-only.
    Read,
}

/// A collapsed region: `head` stays on screen, `head+1..=last` are hidden.
///
/// Folds are metadata about *visibility*, never about content — `lines`,
/// `highlights` and `line_state_before` stay exactly as they were. Everything
/// that walks rows in document order goes through `next_visible_line` /
/// `prev_visible_line`, so hiding happens in one place instead of at each caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fold {
    pub head: usize,
    pub last: usize,
}

/// What the gutter draws in the chevron column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FoldMark {
    /// Can be folded, currently open.
    Open,
    /// Currently folded.
    Closed,
}

/// What the gutter needs to know about a row, derived from the same mapping.
#[derive(Clone, Copy, Debug)]
pub struct GutterRow {
    /// 1-based display number, or `None` on a wrap continuation.
    pub number: Option<usize>,
    pub is_cursor_line: bool,
    /// The chevron, when this row heads a foldable block.
    pub fold: Option<FoldMark>,
}

pub struct Buffer {
    lines: Vec<String>,
    path: Option<PathBuf>,
    pub scroll_row: usize,
    /// Which wrap segment of `scroll_row` sits at the top of the viewport.
    pub scroll_seg: usize,
    pub cursor_row: usize,
    /// Char index within the cursor's line — chars, not bytes, so it indexes
    /// the same units as everything else here.
    pub cursor_col: usize,
    /// Viewport width in columns, or 0 for no wrapping. Set from the grid's
    /// reported size each frame; segments are derived from it, never stored.
    pub wrap_width: usize,
    pub tab_width: usize,
    pub dirty: bool,
    /// Whether the file ended with a newline when loaded. Preserved on save so
    /// editing a file doesn't silently add or drop the trailing byte — that
    /// shows up as a spurious one-line diff in git.
    had_trailing_newline: bool,
    /// mtime as of the last load or save. Compared before writing so a file
    /// changed underneath us isn't silently clobbered.
    known_mtime: Option<SystemTime>,

    undo: Vec<Edit>,
    redo: Vec<Edit>,
    /// Next id to hand out. Never reused, so two different histories that happen
    /// to be the same depth are still distinguishable.
    next_revision: u64,
    /// History position at the last save. `0` means "as loaded".
    saved_revision: u64,
    /// Position the next inserted char must land at to extend the current undo
    /// step. `None` breaks the run, so typing → moving → typing undoes as two.
    coalesce_end: Option<Pos>,
    /// The fixed end of a selection; the cursor is the moving end. `None` means
    /// no selection. Anchor-plus-cursor rather than a start/end pair so extending
    /// backwards past the origin works without special cases.
    pub selection_anchor: Option<Pos>,

    syntax_name: Option<String>,
    /// A syntax the user named explicitly, which outlives a change of extension.
    syntax_override: Option<String>,
    /// Collapsed regions, sorted by `head` and non-overlapping.
    folds: Vec<Fold>,
    view_mode: ViewMode,
    /// Rendered markdown, kept until the text or the width changes.
    rendered: Option<crate::read::Rendered>,
    /// Read mode's own scroll. Deliberately *not* `scroll_row`: that indexes
    /// source lines, while this indexes rendered ones, and v1's habit of reusing
    /// the field made its meaning depend on the mode.
    read_scroll_row: usize,
    read_scroll_seg: usize,
    /// Touched by an external tool and not looked at since. Set by a `--review`
    /// open, cleared the moment the tab is made active. Not persisted — v1
    /// doesn't either, and "unreviewed" is about this sitting, not the file.
    unreviewed: bool,
    /// Parse state *before* line `i`. `[0]` is seeded at load; the rest fill in
    /// lazily. Same lockstep-with-`lines` discipline as v1.
    line_state_before: Vec<Option<LineState>>,
    highlights: Vec<Option<Vec<HlSpan>>>,
}

impl Buffer {
    pub fn empty() -> Self {
        Self {
            lines: vec![String::new()],
            path: None,
            scroll_row: 0,
            scroll_seg: 0,
            cursor_row: 0,
            cursor_col: 0,
            wrap_width: 0,
            tab_width: 4,
            dirty: false,
            had_trailing_newline: true,
            known_mtime: None,
            undo: Vec::new(),
            redo: Vec::new(),
            next_revision: 1,
            saved_revision: 0,
            coalesce_end: None,
            selection_anchor: None,
            syntax_name: None,
            syntax_override: None,
            folds: Vec::new(),
            view_mode: ViewMode::Edit,
            rendered: None,
            read_scroll_row: 0,
            read_scroll_seg: 0,
            unreviewed: false,
            line_state_before: vec![None],
            highlights: vec![None],
        }
    }

    pub fn load(path: &Path, hl: Option<&Highlighter>) -> std::io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let had_trailing_newline = text.ends_with('\n');
        let mut lines: Vec<String> = text.split('\n').map(str::to_string).collect();
        if had_trailing_newline {
            lines.pop();
        }
        if lines.is_empty() {
            lines.push(String::new());
        }
        let n = lines.len();
        let mut buf = Self {
            lines,
            path: Some(path.to_path_buf()),
            scroll_row: 0,
            scroll_seg: 0,
            cursor_row: 0,
            cursor_col: 0,
            wrap_width: 0,
            tab_width: 4,
            dirty: false,
            had_trailing_newline,
            known_mtime: mtime_of(path),
            undo: Vec::new(),
            redo: Vec::new(),
            next_revision: 1,
            saved_revision: 0,
            coalesce_end: None,
            selection_anchor: None,
            syntax_name: None,
            syntax_override: None,
            folds: Vec::new(),
            view_mode: ViewMode::Edit,
            rendered: None,
            read_scroll_row: 0,
            read_scroll_seg: 0,
            unreviewed: false,
            line_state_before: vec![None; n],
            highlights: vec![None; n],
        };
        if let Some(hl) = hl {
            buf.seed_syntax(hl);
        }
        Ok(buf)
    }

    /// Force a specific syntax, as `--syntax=Rust` asks for.
    ///
    /// Kept separate from the path-derived choice because it has to *survive* it:
    /// `seed_syntax` runs again on save-as, and re-deriving from the new
    /// extension would silently discard an override the user asked for.
    pub fn set_syntax_override(&mut self, name: &str, hl: &Highlighter) {
        if let Some(syntax) = hl.syntax_by_name(name) {
            self.syntax_override = Some(name.to_string());
            self.syntax_name = Some(syntax.name.clone());
            self.line_state_before[0] = Some(hl.initial_state(syntax));
            self.invalidate_from(0);
        }
    }

    fn seed_syntax(&mut self, hl: &Highlighter) {
        // An explicit override outranks the extension.
        if let Some(name) = self.syntax_override.clone() {
            self.set_syntax_override(&name, hl);
            return;
        }
        let syntax = self.path.as_deref().and_then(|p| hl.syntax_for_path(p));
        match syntax {
            Some(s) => {
                self.syntax_name = Some(s.name.clone());
                self.line_state_before[0] = Some(hl.initial_state(s));
            }
            None => {
                self.syntax_name = None;
                self.line_state_before[0] = None;
            }
        }
    }

    pub fn unreviewed(&self) -> bool {
        self.unreviewed
    }

    pub fn set_unreviewed(&mut self, v: bool) {
        self.unreviewed = v;
    }

    /// The explicitly-requested syntax, if any, so a session can restore it.
    pub fn syntax_override(&self) -> Option<&str> {
        self.syntax_override.as_deref()
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn display_name(&self) -> String {
        self.path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[no file]".to_string())
    }

    /// Digits needed for the largest line number — the gutter's number width.
    pub fn number_width(&self) -> usize {
        let mut n = self.lines.len().max(1);
        let mut d = 0;
        while n > 0 {
            n /= 10;
            d += 1;
        }
        d
    }

    /// Scroll by screen rows, not lines — a wrapped line takes several notches to
    /// pass. Stops when the last screen row of the buffer reaches the bottom.
    pub fn scroll_by(&mut self, delta: isize, viewport_rows: usize) {
        let rows = viewport_rows.max(1);
        if delta < 0 {
            for _ in 0..(-delta) {
                if !self.scroll_back_one() {
                    break;
                }
            }
            return;
        }
        for _ in 0..delta {
            // Refuse to scroll past the point where the final row is visible,
            // otherwise the wheel walks off into blank space.
            let saved = (self.scroll_row, self.scroll_seg);
            if !self.scroll_forward_one() {
                break;
            }
            if self.visible_rows(rows).len() < rows {
                self.scroll_row = saved.0;
                self.scroll_seg = saved.1;
                break;
            }
        }
    }

    /// Hanging indent for a line's continuation rows: its own leading whitespace,
    /// capped so text always has room.
    pub fn continuation_indent(&self, line: usize) -> usize {
        let text = self.lines.get(line).map(String::as_str).unwrap_or("");
        text::clamp_hanging_indent(
            text::leading_indent_cols(text, self.tab_width),
            self.wrap_width,
        )
    }

    /// Wrap segments of one line, as `[start, end)` char ranges. Always at least
    /// one, so every line occupies a row.
    pub fn segments(&self, line: usize) -> Vec<(usize, usize)> {
        let text = self.lines.get(line).map(String::as_str).unwrap_or("");
        text::wrap_line(
            text,
            self.wrap_width,
            self.tab_width,
            text::leading_indent_cols(text, self.tab_width),
        )
    }

    /// Which segment of `line` contains char index `col`.
    fn segment_of(&self, line: usize, col: usize) -> usize {
        let segs = self.segments(line);
        for (i, &(_, end)) in segs.iter().enumerate() {
            if col < end {
                return i;
            }
        }
        segs.len().saturating_sub(1)
    }

    /// The one producer of screen-row → file-line mapping. See module docs.
    ///
    /// Walks forward from `(scroll_row, scroll_seg)`, emitting one entry per screen
    /// row. Both the gutter and the grid consume this, which is what keeps them
    /// aligned.
    pub fn visible_rows(&self, rows: usize) -> Vec<VisibleRow> {
        let mut out = Vec::with_capacity(rows);
        let mut line = self.scroll_row.min(self.lines.len().saturating_sub(1));
        let mut seg = self.scroll_seg;
        while out.len() < rows && line < self.lines.len() {
            let segs = self.segments(line);
            // A stale `scroll_seg` (the line shortened under it) clamps rather
            // than skipping the line entirely.
            if seg >= segs.len() {
                seg = 0;
                match self.next_visible_line(line) {
                    Some(next) => line = next,
                    None => break,
                }
                continue;
            }
            let indent = self.continuation_indent(line);
            while seg < segs.len() && out.len() < rows {
                let (start, end) = segs[seg];
                out.push(VisibleRow {
                    line,
                    start,
                    end,
                    is_first_segment: seg == 0,
                    // First segments sit at the margin; continuations hang under
                    // their line's indentation.
                    indent: if seg == 0 { 0 } else { indent },
                });
                seg += 1;
            }
            if seg >= segs.len() {
                seg = 0;
                match self.next_visible_line(line) {
                    Some(next) => line = next,
                    None => break,
                }
            }
        }
        out
    }

    /// Step the top of the viewport one screen row later.
    fn scroll_forward_one(&mut self) -> bool {
        let segs = self.segments(self.scroll_row);
        if self.scroll_seg + 1 < segs.len() {
            self.scroll_seg += 1;
            true
        } else if let Some(next) = self.next_visible_line(self.scroll_row) {
            self.scroll_row = next;
            self.scroll_seg = 0;
            true
        } else {
            false
        }
    }

    /// Step the top of the viewport one screen row earlier.
    fn scroll_back_one(&mut self) -> bool {
        if self.scroll_seg > 0 {
            self.scroll_seg -= 1;
            true
        } else if let Some(prev) = self.prev_visible_line(self.scroll_row) {
            self.scroll_row = prev;
            self.scroll_seg = self.segments(self.scroll_row).len().saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Screen cell to a document position, through the wrap mapping.
    ///
    /// The inverse of what the renderer does: a screen row names a *segment*, and
    /// the column is visual, so both have to be resolved rather than added to
    /// `scroll_row`.
    pub fn screen_to_doc(&self, row: usize, col: usize, rows: usize) -> Pos {
        let visible = self.visible_rows(rows.max(1));
        let Some(vr) = visible.get(row).or_else(|| visible.last()) else {
            return (self.cursor_row, self.cursor_col);
        };
        let line = self.lines.get(vr.line).map(String::as_str).unwrap_or("");
        // Screen column minus the row's hanging indent gives the column *within*
        // the segment; clicking inside the indent lands on the segment start.
        let target = col.saturating_sub(vr.indent);
        let c = text::col_at_vis_in_segment(line, vr.start, vr.end, target, self.tab_width);
        (vr.line, c)
    }

    /// Screen column of a position: its segment-relative visual column plus that
    /// row's hanging indent.
    pub fn screen_col_of(&self, row: usize, col: usize) -> usize {
        let segs = self.segments(row);
        let seg = self.segment_of(row, col);
        let line = self.lines.get(row).map(String::as_str).unwrap_or("");
        let indent = if seg == 0 {
            0
        } else {
            self.continuation_indent(row)
        };
        indent + text::vis_in_segment(line, segs[seg].0, col, self.tab_width)
    }

    /// Screen row of the cursor within the viewport, if visible.
    pub fn cursor_screen_row(&self, rows: usize) -> Option<usize> {
        let seg = self.segment_of(self.cursor_row, self.cursor_col);
        self.visible_rows(rows)
            .iter()
            .position(|r| r.line == self.cursor_row && r.start == self.segments(self.cursor_row)[seg].0)
    }

    /// Gutter view of the same mapping.
    pub fn gutter_rows(&self, rows: usize) -> Vec<GutterRow> {
        self.visible_rows(rows)
            .into_iter()
            .map(|r| GutterRow {
                number: r.is_first_segment.then_some(r.line + 1),
                is_cursor_line: r.line == self.cursor_row,
                // Only on a first segment: a wrap continuation is the same line,
                // and a second chevron for it would claim to fold something else.
                fold: if r.is_first_segment {
                    self.fold_mark(r.line)
                } else {
                    None
                },
            })
            .collect()
    }

    /// What the gutter should show for `row`, if anything.
    pub fn fold_mark(&self, row: usize) -> Option<FoldMark> {
        if self.folds.iter().any(|f| f.head == row) {
            return Some(FoldMark::Closed);
        }
        self.fold_end(row).map(|_| FoldMark::Open)
    }

    /// Is `row` inside a collapsed region, and therefore not drawn?
    fn is_hidden(&self, row: usize) -> bool {
        self.folds.iter().any(|f| row > f.head && row <= f.last)
    }

    /// The next line that would actually be drawn after `row`.
    fn next_visible_line(&self, row: usize) -> Option<usize> {
        let mut next = row + 1;
        // Jump the whole region rather than stepping through it: a fold over a
        // thousand lines would otherwise cost a thousand checks per screen row.
        while let Some(f) = self.folds.iter().find(|f| next > f.head && next <= f.last) {
            next = f.last + 1;
        }
        (next < self.lines.len()).then_some(next)
    }

    /// The previous line that would actually be drawn before `row`.
    fn prev_visible_line(&self, row: usize) -> Option<usize> {
        let mut prev = row.checked_sub(1)?;
        while let Some(f) = self.folds.iter().find(|f| prev > f.head && prev <= f.last) {
            prev = f.head;
        }
        Some(prev)
    }

    /// Where the block headed by `row` ends, if it heads one.
    ///
    /// Indent-based: a row heads a block when the next non-blank line is indented
    /// further. Blank lines don't end a block — a gap between two indented
    /// statements is still inside it — but trailing blanks aren't swallowed
    /// either, so folding a function doesn't eat the space before the next one.
    ///
    /// **The body must start no more than one indent level deeper.** v1 folds on
    /// *any* increase, so every wrapped call aligned under its open bracket reads
    /// as a block:
    ///
    /// ```text
    /// let x = compute(a,
    ///                 b);      <- deeper, but not a block
    /// ```
    ///
    /// Alignment lands on whatever column the bracket happened to sit at — far
    /// deeper than a level — so a ceiling separates the two without knowing
    /// anything about the language. A *multiple* of the level wouldn't: the
    /// continuation above sits 16 columns in, a clean multiple of 4.
    ///
    /// It's a ceiling rather than an exact match because one file has more than
    /// one legitimate step. Editing a 4-space file with `indent_with_tabs` and
    /// `tab_width = 2` writes 2-column indents next to 4-column ones, and
    /// demanding exactness meant newly typed functions never got a chevron at
    /// all. Both are one level; neither is alignment.
    ///
    /// The ceiling is a **constant**, not measured from the file. Deriving it
    /// from the text made it a moving target: as tab-indented lines came to
    /// outnumber the 4-space ones, the measured level dropped to 2 and the
    /// original functions lost their chevrons — the file changing under you,
    /// which is worse than being slightly too permissive.
    ///
    /// Only the first body line is tested — once a block is established, what's
    /// inside it can be aligned however it likes.
    ///
    /// Known limit: a method chain indented one level (`.foo()` under its
    /// receiver) still reads as a block. Telling that apart needs the grammar,
    /// not the indentation.
    pub fn fold_end(&self, row: usize) -> Option<usize> {
        let base = self.indent_of(row)?;
        let level = MAX_INDENT_STEP.max(self.tab_width);
        let mut end = None;
        for probe in (row + 1)..self.lines.len() {
            match self.indent_of(probe) {
                // Blank: might be inside the block, decided by what follows.
                None => continue,
                Some(indent) if indent > base => {
                    if end.is_none() && indent - base > level {
                        return None;
                    }
                    end = Some(probe);
                }
                // Back to or above the header's indent: the block is over.
                Some(_) => break,
            }
        }
        end
    }

    /// Visual indent of a line, or `None` when it's blank.
    fn indent_of(&self, row: usize) -> Option<usize> {
        let line = self.lines.get(row)?;
        (!line.trim().is_empty()).then(|| text::leading_indent_cols(line, self.tab_width))
    }

    /// The row heading the innermost block containing `row`.
    ///
    /// Returns `row` itself when it heads a block. Folding from inside a body is
    /// what you usually want — the caret is rarely parked on the `fn` line.
    pub fn enclosing_fold_head(&self, row: usize) -> usize {
        if self.fold_end(row).is_some() {
            return row;
        }
        let Some(here) = self.indent_of(row) else {
            return row;
        };
        for candidate in (0..row).rev() {
            let Some(indent) = self.indent_of(candidate) else {
                continue;
            };
            if indent < here && self.fold_end(candidate).is_some_and(|last| last >= row) {
                return candidate;
            }
        }
        row
    }

    /// Fold or unfold the block headed by `row`.
    pub fn toggle_fold(&mut self, row: usize) -> bool {
        if let Some(i) = self.folds.iter().position(|f| f.head == row) {
            self.folds.remove(i);
            return true;
        }
        let Some(last) = self.fold_end(row) else {
            return false;
        };
        self.insert_fold(Fold { head: row, last });
        true
    }

    /// Fold every block that has one. Outermost first, so the inner blocks a
    /// fold already hides are skipped rather than stacked.
    pub fn fold_all(&mut self) {
        self.folds.clear();
        let mut row = 0;
        while row < self.lines.len() {
            match self.fold_end(row) {
                Some(last) => {
                    self.folds.push(Fold { head: row, last });
                    row = last + 1;
                }
                None => row += 1,
            }
        }
        self.settle_after_fold();
    }

    pub fn unfold_all(&mut self) {
        self.folds.clear();
    }

    fn insert_fold(&mut self, fold: Fold) {
        // A block already hidden inside another needs no fold of its own.
        if self.is_hidden(fold.head) {
            return;
        }
        // Drop anything this fold swallows, so the list stays non-overlapping.
        self.folds
            .retain(|f| f.head <= fold.head || f.head > fold.last);
        let at = self
            .folds
            .iter()
            .position(|f| f.head > fold.head)
            .unwrap_or(self.folds.len());
        self.folds.insert(at, fold);
        self.settle_after_fold();
    }

    /// Keep the caret and the viewport off hidden lines.
    ///
    /// Without this the caret can sit inside a collapsed block, where it draws
    /// nowhere and every subsequent movement starts from a row that isn't on
    /// screen.
    fn settle_after_fold(&mut self) {
        if self.is_hidden(self.cursor_row)
            && let Some(f) = self.folds.iter().find(|f| {
                self.cursor_row > f.head && self.cursor_row <= f.last
            })
        {
            self.cursor_row = f.head;
            self.cursor_col = self.cursor_col.min(self.line_len(f.head));
        }
        if self.is_hidden(self.scroll_row)
            && let Some(f) = self.folds.iter().find(|f| {
                self.scroll_row > f.head && self.scroll_row <= f.last
            })
        {
            self.scroll_row = f.head;
            self.scroll_seg = 0;
        }
    }

    /// Collapsed regions, for the session.
    pub fn fold_ranges(&self) -> Vec<(usize, usize)> {
        self.folds.iter().map(|f| (f.head, f.last)).collect()
    }

    /// Restore folds, dropping any that no longer fit the file.
    pub fn set_fold_ranges(&mut self, ranges: &[(usize, usize)]) {
        self.folds.clear();
        for (head, last) in ranges {
            if *head < *last && *last < self.lines.len() {
                self.insert_fold(Fold {
                    head: *head,
                    last: *last,
                });
            }
        }
    }

    /// Shift or drop folds around an edit.
    ///
    /// Conservative on purpose: a fold whose lines the edit touches is dropped
    /// rather than tracked through the change. Guessing where a block still ends
    /// after its body was rewritten is how folds start hiding the wrong lines,
    /// and losing a fold is a cheap mistake to make.
    fn adjust_folds_for_edit(&mut self, at: usize, removed: usize, added: usize) {
        let delta = added as isize - removed as isize;
        let touched_end = at + removed;
        self.folds.retain_mut(|f| {
            if touched_end < f.head {
                // Entirely before: the whole fold slides.
                f.head = f.head.saturating_add_signed(delta);
                f.last = f.last.saturating_add_signed(delta);
                true
            } else {
                // Overlaps the block, or sits after it — after needs no change,
                // overlapping gets dropped. `at > f.last` separates them.
                at > f.last
            }
        });
    }

    /// Fill the highlight cache up to and including `line`, walking forward from
    /// the nearest live parse state. Lazy so opening a large file doesn't parse
    /// all of it — only what's been scrolled into view.
    pub fn ensure_highlights(&mut self, line: usize, hl: &Highlighter) {
        if self.syntax_name.is_none() {
            return;
        }
        let cap = (line + 1).min(self.lines.len());
        for i in 0..cap {
            if self.highlights[i].is_some() {
                continue;
            }
            let base = (0..=i)
                .rev()
                .find(|&j| self.line_state_before[j].is_some())
                .unwrap_or(0);
            let Some(seed) = self.line_state_before[base].clone() else {
                return;
            };
            let mut state = seed;
            for j in base..=i {
                self.highlights[j] = Some(hl.highlight_line(&self.lines[j], &mut state));
                if j + 1 < self.line_state_before.len() {
                    self.line_state_before[j + 1] = Some(state.clone());
                }
            }
        }
    }
}


// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------
//
// Everything routes through one primitive, `replace`. That's deliberate: the
// invariant carried over from v1 is that any change to the line count must splice
// `highlights` and `line_state_before` in the same step, and a desync shows up as
// highlighting that belongs to a different line. With four independent mutators
// that's four places to get it right; with one, it's one.
//
// Undo stores *deltas* rather than v1's full-text `Snapshot`s. v1 kept 500 copies
// of the whole buffer, which on a large file is a lot of memory for an app whose
// whole pitch is a small footprint. A delta costs the size of the edit.
//
// Still absent: selection and clipboard.

/// `(row, char index within row)`. Char index, never bytes.
pub type Pos = (usize, usize);

/// Undo steps retained. Cheap now that steps are deltas rather than whole-buffer
/// copies, so this is generous compared to v1's 500.
const MAX_UNDO: usize = 2000;

/// One reversible change: at `at`, `before` became `after`.
///
/// Undo replaces `after` with `before`; redo does the reverse. Storing the cursor
/// and dirty flag from *before* the edit means undo restores where you were and
/// whether the file was clean, rather than approximating both.
#[derive(Debug, Clone)]
struct Edit {
    at: Pos,
    before: String,
    after: String,
    cursor_before: Pos,
    /// Monotonic id. Identifies this history position uniquely even after
    /// branching, which is what makes the dirty check exact — see
    /// `Buffer::refresh_dirty`.
    revision: u64,
}

impl Buffer {
    fn line_len(&self, row: usize) -> usize {
        self.lines.get(row).map(|l| l.chars().count()).unwrap_or(0)
    }

    /// Byte offset of a char index — `String` indexing is byte-based but every
    /// cursor position here is a char index.
    fn byte_of(line: &str, char_idx: usize) -> usize {
        line.char_indices()
            .nth(char_idx)
            .map(|(i, _)| i)
            .unwrap_or(line.len())
    }

    /// Highlights for `row` and after are now wrong; the parse state *before*
    /// `row` is still good, so state invalidation starts one later.
    fn invalidate_from(&mut self, row: usize) {
        for h in self.highlights.iter_mut().skip(row) {
            *h = None;
        }
        for st in self.line_state_before.iter_mut().skip(row + 1) {
            *st = None;
        }
    }

    fn clamp_pos(&self, pos: Pos) -> Pos {
        let row = pos.0.min(self.lines.len().saturating_sub(1));
        (row, pos.1.min(self.line_len(row)))
    }

    fn clamp_cursor(&mut self) {
        let (r, c) = self.clamp_pos((self.cursor_row, self.cursor_col));
        self.cursor_row = r;
        self.cursor_col = c;
    }

    /// Public form of the clamp, for positions that came from outside — a mouse
    /// click can name a cell past the end of a short line.
    pub fn clamp_to_content(&mut self) {
        self.clamp_cursor();
    }

    fn cursor(&self) -> Pos {
        (self.cursor_row, self.cursor_col)
    }

    fn set_cursor(&mut self, pos: Pos) {
        let (r, c) = self.clamp_pos(pos);
        self.cursor_row = r;
        self.cursor_col = c;
    }

    /// **The one mutation primitive.** Replace `remove` characters starting at
    /// `at` with `text`; return what was removed and the position just past what
    /// was inserted.
    ///
    /// Cache lockstep and invalidation live here and nowhere else. Newlines in
    /// either direction are handled, so this covers single-char edits, line
    /// joins, splits, and multi-line pastes uniformly.
    fn replace(&mut self, at: Pos, remove: usize, text: &str) -> (String, Pos) {
        let (row, col) = self.clamp_pos(at);

        // Walk forward `remove` chars to find the end of the removed span,
        // collecting it. A newline counts as one char.
        let mut removed = String::new();
        let (mut er, mut ec) = (row, col);
        let mut left = remove;
        while left > 0 {
            let len = self.line_len(er);
            if ec < len {
                let take = left.min(len - ec);
                let line = &self.lines[er];
                let (s, e) = (Self::byte_of(line, ec), Self::byte_of(line, ec + take));
                removed.push_str(&line[s..e]);
                ec += take;
                left -= take;
            } else if er + 1 < self.lines.len() {
                removed.push('\n');
                er += 1;
                ec = 0;
                left -= 1;
            } else {
                break; // end of buffer
            }
        }

        let prefix: String = self.lines[row].chars().take(col).collect();
        let suffix: String = self.lines[er].chars().skip(ec).collect();
        let inserted_chars = text.chars().count();
        let combined = format!("{prefix}{text}{suffix}");
        let new_lines: Vec<String> = combined.split('\n').map(str::to_string).collect();

        // The parse state *before* `row` survives this edit; everything from
        // `row` onward is rebuilt, so preserve that one entry across the splice.
        let keep_state = self.line_state_before.get(row).cloned().flatten();
        let n = new_lines.len();
        // Folds ride along with the same splice that moves the lines, so undo and
        // redo get the adjustment for free — both go through here.
        self.adjust_folds_for_edit(row, er - row, n - 1);
        self.lines.splice(row..=er, new_lines);
        self.highlights.splice(row..=er, vec![None; n]);
        self.line_state_before.splice(row..=er, vec![None; n]);
        if let Some(state) = keep_state {
            self.line_state_before[row] = Some(state);
        }
        self.invalidate_from(row);

        // Where the caret lands: past the inserted text.
        let end = match text.rfind('\n') {
            None => (row, col + inserted_chars),
            Some(idx) => {
                let newlines = text.matches('\n').count();
                let tail = text[idx + 1..].chars().count();
                (row + newlines, tail)
            }
        };
        (removed, end)
    }

    /// Record a change on the undo stack, coalescing runs of typing.
    ///
    /// Consecutive single-character inserts that continue where the last left off
    /// merge into one step, so undo removes a word rather than a letter. v1 did
    /// the same via `EditKind::InsertChar`; the difference is that here the merge
    /// extends a delta instead of discarding a snapshot.
    /// Where the history currently sits. `0` is the loaded state.
    fn head_revision(&self) -> u64 {
        self.undo.last().map(|e| e.revision).unwrap_or(0)
    }

    /// Dirty means "the buffer differs from what's on disk", which is exactly
    /// "the history head isn't where it was when we saved".
    ///
    /// Deriving it this way rather than restoring a flag fixes a real hole: undo
    /// past a save used to report clean, because the undone edit remembered that
    /// the buffer *had been* clean before it — while the file on disk now held
    /// the saved version. Closing there would have discarded the undo silently.
    fn refresh_dirty(&mut self) {
        self.dirty = self.head_revision() != self.saved_revision;
    }

    fn record(&mut self, edit: Edit) {
        let coalesces = edit.before.is_empty()
            && edit.after.chars().count() == 1
            && !edit.after.contains('\n')
            && self
                .undo
                .last()
                .is_some_and(|prev| prev.before.is_empty() && self.coalesce_end == Some(edit.at));

        let revision = self.next_revision;
        self.next_revision += 1;
        if coalesces {
            let prev = self.undo.last_mut().expect("coalesces implies a last edit");
            prev.after.push_str(&edit.after);
            // Extending changes the state, so it's a new history position.
            prev.revision = revision;
        } else {
            let mut edit = edit.clone();
            edit.revision = revision;
            self.undo.push(edit);
            if self.undo.len() > MAX_UNDO {
                self.undo.remove(0);
            }
        }
        // Where the next keystroke must land to continue this run.
        self.coalesce_end = Some((edit.at.0, edit.at.1 + edit.after.chars().count()));
        self.redo.clear();
        self.refresh_dirty();
    }

    /// Break the coalescing run, so the next insert starts a fresh undo step.
    /// Called on cursor moves — typing, moving, then typing again should undo as
    /// two steps, not one.
    fn break_coalescing(&mut self) {
        self.coalesce_end = None;
    }

    fn edit(&mut self, at: Pos, remove: usize, text: &str) -> Pos {
        let cursor_before = self.cursor();
        let (before, end) = self.replace(at, remove, text);
        if before.is_empty() && text.is_empty() {
            return end; // nothing happened; don't record an empty step
        }
        self.record(Edit {
            at: self.clamp_pos(at),
            before,
            after: text.to_string(),
            cursor_before,
            revision: 0, // assigned by `record`
        });
        end
    }

    pub fn insert_char(&mut self, c: char) {
        self.clamp_cursor();
        // Typing over a selection replaces it, in one undo step.
        if self.replace_selection(&c.to_string()) {
            return;
        }
        let end = self.edit(self.cursor(), 0, &c.to_string());
        self.set_cursor(end);
    }

    pub fn insert_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.clamp_cursor();
        if self.replace_selection(text) {
            return;
        }
        // A multi-char paste is one undo step, not one per character.
        if text.chars().count() > 1 || text.contains('\n') {
            self.break_coalescing();
            let end = self.edit(self.cursor(), 0, text);
            self.set_cursor(end);
            self.break_coalescing();
        } else {
            let end = self.edit(self.cursor(), 0, text);
            self.set_cursor(end);
        }
    }

    pub fn insert_newline(&mut self) {
        self.clamp_cursor();
        if self.replace_selection("\n") {
            return;
        }
        self.break_coalescing();
        let end = self.edit(self.cursor(), 0, "\n");
        self.set_cursor(end);
        self.break_coalescing();
    }

    pub fn backspace(&mut self) {
        self.clamp_cursor();
        if self.delete_selection() {
            return;
        }
        self.break_coalescing();
        let at = if self.cursor_col > 0 {
            (self.cursor_row, self.cursor_col - 1)
        } else if self.cursor_row > 0 {
            (self.cursor_row - 1, self.line_len(self.cursor_row - 1))
        } else {
            return; // start of buffer
        };
        self.edit(at, 1, "");
        self.set_cursor(at);
    }

    pub fn delete_forward(&mut self) {
        self.clamp_cursor();
        if self.delete_selection() {
            return;
        }
        self.break_coalescing();
        let at = self.cursor();
        // At the very end of the buffer there's nothing to remove.
        if at.1 == self.line_len(at.0) && at.0 + 1 >= self.lines.len() {
            return;
        }
        self.edit(at, 1, "");
        self.set_cursor(at);
    }

    /// Undo one step. Returns false when there's nothing left.
    pub fn undo(&mut self) -> bool {
        let Some(edit) = self.undo.pop() else {
            return false;
        };
        let (_, _) = self.replace(edit.at, edit.after.chars().count(), &edit.before);
        self.set_cursor(edit.cursor_before);
        self.coalesce_end = None;
        self.redo.push(edit);
        self.refresh_dirty();
        true
    }

    /// Redo one step. Returns false when there's nothing left.
    pub fn redo(&mut self) -> bool {
        let Some(edit) = self.redo.pop() else {
            return false;
        };
        let (_, end) = self.replace(edit.at, edit.before.chars().count(), &edit.after);
        self.set_cursor(end);
        self.coalesce_end = None;
        self.undo.push(edit);
        self.refresh_dirty();
        true
    }

    pub fn can_undo(&self) -> bool {
        !self.undo.is_empty()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo.is_empty()
    }

    /// Prepare for a cursor move: `extend` starts or keeps a selection, otherwise
    /// the selection is dropped. Every movement method calls this.
    fn before_move(&mut self, extend: bool) {
        if extend {
            self.begin_selection();
        } else {
            self.clear_selection();
        }
        self.break_coalescing();
    }

    pub fn move_left(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        if self.cursor_col > 0 {
            self.cursor_col -= 1;
        } else if self.cursor_row > 0 {
            self.cursor_row -= 1;
            self.cursor_col = self.line_len(self.cursor_row);
        }
    }

    pub fn move_right(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        if self.cursor_col < self.line_len(self.cursor_row) {
            self.cursor_col += 1;
        } else if self.cursor_row + 1 < self.lines.len() {
            self.cursor_row += 1;
            self.cursor_col = 0;
        }
    }

    /// Move one **screen row**, holding the visual column.
    ///
    /// Inside a wrapped line that's the previous/next segment; at a line's edge it
    /// crosses into the neighbouring line's last/first segment. Moving by *line*
    /// instead would make the caret skip the wrapped remainder of a long line,
    /// which doesn't match what the user sees.
    fn move_screen_row(&mut self, down: bool) {
        let line = self.lines[self.cursor_row].clone();
        let segs = self.segments(self.cursor_row);
        let seg = self.segment_of(self.cursor_row, self.cursor_col);
        // Track the *screen* column, not the segment-relative one: moving between a
        // first segment (no indent) and a continuation (indented) has to keep the
        // caret under the same pixel, which means accounting for the indent on both
        // sides.
        let target_screen = self.screen_col_of(self.cursor_row, self.cursor_col);
        let own_indent = self.continuation_indent(self.cursor_row);

        if down && seg + 1 < segs.len() {
            let (s, e) = segs[seg + 1];
            let target = target_screen.saturating_sub(own_indent);
            self.cursor_col = text::col_at_vis_in_segment(&line, s, e, target, self.tab_width);
            return;
        }
        if !down && seg > 0 {
            let (s, e) = segs[seg - 1];
            // Row above is a continuation unless it's segment 0.
            let indent = if seg - 1 == 0 { 0 } else { own_indent };
            let target = target_screen.saturating_sub(indent);
            self.cursor_col = text::col_at_vis_in_segment(&line, s, e, target, self.tab_width);
            return;
        }

        // Next/previous *visible* line: a collapsed block is one row on screen,
        // so arrowing through it must not walk its hidden lines one at a time.
        let next_line = if down {
            match self.next_visible_line(self.cursor_row) {
                Some(n) => n,
                None => return,
            }
        } else {
            match self.prev_visible_line(self.cursor_row) {
                Some(n) => n,
                None => return,
            }
        };
        self.cursor_row = next_line;
        let text_line = self.lines[next_line].clone();
        let next_segs = self.segments(next_line);
        // Entering from above lands on the first segment; from below, the last.
        let (last_idx, (s, e)) = if down {
            (0, next_segs[0])
        } else {
            (next_segs.len() - 1, next_segs[next_segs.len() - 1])
        };
        let indent = if last_idx == 0 {
            0
        } else {
            self.continuation_indent(next_line)
        };
        let target = target_screen.saturating_sub(indent);
        self.cursor_col = text::col_at_vis_in_segment(&text_line, s, e, target, self.tab_width);
    }

    pub fn move_up(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        self.move_screen_row(false);
    }

    pub fn move_down(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        self.move_screen_row(true);
    }

    /// Start of the current *screen row*. On a wrapped line that's the segment
    /// start, not the line start — matching where the caret visually sits.
    pub fn move_home(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        let seg = self.segment_of(self.cursor_row, self.cursor_col);
        self.cursor_col = self.segments(self.cursor_row)[seg].0;
    }

    /// End of the current screen row.
    pub fn move_end(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        let segs = self.segments(self.cursor_row);
        let seg = self.segment_of(self.cursor_row, self.cursor_col);
        let (_, end) = segs[seg];
        // On a wrapped segment the break sits *between* characters, so stopping at
        // `end` would put the caret on the next row. Back off one unless this is
        // the final segment, where `end` is the true line end.
        self.cursor_col = if seg + 1 < segs.len() {
            end.saturating_sub(1)
        } else {
            end
        };
    }

    /// The character at `pos`, with the line break itself reported as `'\n'`.
    ///
    /// Word movement has to treat a line end as a word separator it can cross,
    /// otherwise `Option+Right` stops dead at every end of line. Returns `None`
    /// only at the very end of the buffer.
    fn char_at(&self, (row, col): Pos) -> Option<char> {
        let line = self.lines.get(row)?;
        match col.cmp(&line.chars().count()) {
            std::cmp::Ordering::Less => line.chars().nth(col),
            // Past the last line's end is the end of the buffer, not a newline.
            std::cmp::Ordering::Equal if row + 1 < self.lines.len() => Some('\n'),
            _ => None,
        }
    }

    /// One position forward, crossing line ends. Saturates at the buffer end.
    fn advance(&self, (row, col): Pos) -> Pos {
        if col < self.line_len(row) {
            (row, col + 1)
        } else if row + 1 < self.lines.len() {
            (row + 1, 0)
        } else {
            (row, col)
        }
    }

    /// One position backward, crossing line ends. Saturates at the buffer start.
    fn retreat(&self, (row, col): Pos) -> Pos {
        if col > 0 {
            (row, col - 1)
        } else if row > 0 {
            (row - 1, self.line_len(row - 1))
        } else {
            (0, 0)
        }
    }

    /// Word-wise movement, macOS `Option+Right`: skip whatever separators are in
    /// the way, then run to the end of the word. Landing on the *end* of the next
    /// word rather than its start is what distinguishes this from `Option+Left`.
    pub fn move_word_right(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        let mut p = (self.cursor_row, self.cursor_col);
        while let Some(c) = self.char_at(p) {
            if is_word_char(c) {
                break;
            }
            let next = self.advance(p);
            if next == p {
                break;
            }
            p = next;
        }
        while let Some(c) = self.char_at(p) {
            if !is_word_char(c) {
                break;
            }
            let next = self.advance(p);
            if next == p {
                break;
            }
            p = next;
        }
        (self.cursor_row, self.cursor_col) = p;
    }

    /// macOS `Option+Left`: back over separators, then back to the word's start.
    pub fn move_word_left(&mut self, extend: bool) {
        self.clamp_cursor();
        self.before_move(extend);
        let mut p = (self.cursor_row, self.cursor_col);
        // Looks at the character *before* the position, since the caret sits
        // between characters and it's the left neighbour that decides.
        let before = |b: &Self, p: Pos| {
            let q = b.retreat(p);
            (q != p).then(|| b.char_at(q)).flatten()
        };
        while let Some(c) = before(self, p) {
            if is_word_char(c) {
                break;
            }
            p = self.retreat(p);
        }
        while let Some(c) = before(self, p) {
            if !is_word_char(c) {
                break;
            }
            p = self.retreat(p);
        }
        (self.cursor_row, self.cursor_col) = p;
    }

    /// Every start column at which `needle` occurs in `row`.
    ///
    /// Collecting all of them, rather than threading direction and bounds through
    /// the scan, is what keeps the wrap-around cases in `find` obviously correct.
    /// A line is short and this runs on a keystroke, so the allocation is fine.
    fn matches_in_row(&self, row: usize, needle: &[char], fold_case: bool) -> Vec<usize> {
        let hay: Vec<char> = if fold_case {
            self.lines[row].chars().map(fold_char).collect()
        } else {
            self.lines[row].chars().collect()
        };
        if needle.is_empty() || hay.len() < needle.len() {
            return Vec::new();
        }
        (0..=hay.len() - needle.len())
            .filter(|i| hay[*i..*i + needle.len()] == *needle)
            .collect()
    }

    /// Search for `query`, returning the match as a `(start, end)` pair.
    ///
    /// **Smart case**, the Sublime rule: an all-lowercase query matches
    /// case-insensitively, and any uppercase character makes the whole query
    /// case-sensitive. So `fn` finds `Fn` but `Fn` doesn't find `fn`.
    ///
    /// Searching **wraps**, which is why each direction is two passes: the rows
    /// from `from` onward, then the rows before it. Single-line queries only —
    /// a newline in the query never matches.
    ///
    /// Folding is per character via [`fold_char`], never `str::to_lowercase`.
    /// Full Unicode lowercasing can change a string's *length* — `İ` lowercases
    /// to two chars — and every match column after such a character would then be
    /// wrong. Simple one-to-one folding keeps char indices aligned with the line,
    /// which is the property the returned `Pos` depends on.
    pub fn find(&self, query: &str, from: Pos, forward: bool) -> Option<(Pos, Pos)> {
        if query.is_empty() || query.contains('\n') {
            return None;
        }
        let fold_case = !query.chars().any(|c| c.is_uppercase());
        let needle: Vec<char> = if fold_case {
            query.chars().map(fold_char).collect()
        } else {
            query.chars().collect()
        };
        let n = self.lines.len();
        let (from_row, from_col) = self.clamp_pos(from);
        let span = |row: usize, col: usize| ((row, col), (row, col + needle.len()));

        if forward {
            // Pass 1: from the cursor to the end of the buffer.
            for row in from_row..n {
                let lo = if row == from_row { from_col } else { 0 };
                if let Some(c) = self
                    .matches_in_row(row, &needle, fold_case)
                    .into_iter()
                    .find(|c| *c >= lo)
                {
                    return Some(span(row, c));
                }
            }
            // Pass 2: wrap, stopping where pass 1 began so nothing repeats.
            for row in 0..=from_row.min(n - 1) {
                if let Some(c) = self
                    .matches_in_row(row, &needle, fold_case)
                    .into_iter()
                    .find(|c| row < from_row || *c < from_col)
                {
                    return Some(span(row, c));
                }
            }
        } else {
            for row in (0..=from_row).rev() {
                if let Some(c) = self
                    .matches_in_row(row, &needle, fold_case)
                    .into_iter()
                    .filter(|c| row < from_row || *c < from_col)
                    .next_back()
                {
                    return Some(span(row, c));
                }
            }
            for row in (from_row..n).rev() {
                if let Some(c) = self
                    .matches_in_row(row, &needle, fold_case)
                    .into_iter()
                    .filter(|c| row > from_row || *c >= from_col)
                    .next_back()
                {
                    return Some(span(row, c));
                }
            }
        }
        None
    }

    /// Delete a span directly, for tests that need a multi-line edit.
    #[cfg(test)]
    pub fn replace_range_for_test(&mut self, start: Pos, end: Pos) {
        let len = self.span_len(start, end);
        self.edit(start, len, "");
    }

    /// Select `start..end` and leave the cursor at `end`, the way a found match
    /// should read: highlighted, with typing about to replace it.
    pub fn select_range(&mut self, start: Pos, end: Pos) {
        let start = self.clamp_pos(start);
        let end = self.clamp_pos(end);
        self.selection_anchor = Some(start);
        (self.cursor_row, self.cursor_col) = end;
        self.break_coalescing();
    }

    /// Jump to a 1-based line number, clamped to the file.
    pub fn goto_line(&mut self, line: usize) {
        self.clear_selection();
        self.cursor_row = line.saturating_sub(1).min(self.lines.len().saturating_sub(1));
        self.cursor_col = 0;
        self.break_coalescing();
    }

    /// Rows a line-wise command applies to: the selected rows, or the caret's.
    ///
    /// A selection ending at column 0 doesn't include that last row — dragging
    /// down to the start of a line reads as "not this one", and indenting it
    /// would be a surprise.
    fn line_range(&self) -> (usize, usize) {
        match self.selection_range() {
            Some(((sr, _), (er, ec))) => (sr, if er > sr && ec == 0 { er - 1 } else { er }),
            None => (self.cursor_row, self.cursor_row),
        }
    }

    /// Rewrite whole lines `first..=last` as **one** edit.
    ///
    /// One `edit` rather than one per line, because each would be its own undo
    /// step — commenting ten lines has to come back with a single `Cmd+Z`. It
    /// also means one highlight splice instead of ten.
    ///
    /// `shift[i]` is how far row `first + i`'s text moved, so the caret and the
    /// selection anchor can be carried along; a negative value pulls them left,
    /// never past the start of the line.
    fn rewrite_lines(&mut self, first: usize, last: usize, new: Vec<String>, shift: &[isize]) {
        let start = (first, 0);
        let end = (last, self.line_len(last));
        let len = self.span_len(start, end);
        let (cursor, anchor) = (self.cursor(), self.selection_anchor);
        self.edit(start, len, &new.join("\n"));
        self.break_coalescing();

        let moved = |(row, col): Pos| -> Pos {
            if row < first || row > last {
                return (row, col);
            }
            let d = shift.get(row - first).copied().unwrap_or(0);
            (row, col.saturating_add_signed(d))
        };
        let c = moved(cursor);
        self.cursor_row = c.0.min(self.lines.len().saturating_sub(1));
        self.cursor_col = c.1.min(self.line_len(self.cursor_row));
        self.selection_anchor = anchor.map(moved).map(|(r, col)| {
            let r = r.min(self.lines.len().saturating_sub(1));
            (r, col.min(self.line_len(r)))
        });
    }

    /// Indent the selected lines, or the caret's line, by one level.
    pub fn indent_selection(&mut self, tab_width: usize, use_tabs: bool) {
        let (first, last) = self.line_range();
        let indent = if use_tabs {
            "\t".to_string()
        } else {
            " ".repeat(tab_width.max(1))
        };
        let add = indent.chars().count() as isize;
        let new: Vec<String> = (first..=last)
            .map(|r| format!("{indent}{}", self.lines[r]))
            .collect();
        let shift = vec![add; new.len()];
        self.rewrite_lines(first, last, new, &shift);
    }

    /// Remove one level of indentation, where there is one to remove.
    pub fn outdent_selection(&mut self, tab_width: usize) {
        let (first, last) = self.line_range();
        let width = tab_width.max(1);
        let mut new = Vec::with_capacity(last - first + 1);
        let mut shift = Vec::with_capacity(last - first + 1);
        for r in first..=last {
            let line = &self.lines[r];
            // A leading tab counts as one level however wide it displays;
            // otherwise take up to `tab_width` spaces, and fewer is fine.
            let drop = if line.starts_with('\t') {
                1
            } else {
                line.chars().take(width).take_while(|c| *c == ' ').count()
            };
            new.push(line.chars().skip(drop).collect::<String>());
            shift.push(-(drop as isize));
        }
        if shift.iter().all(|d| *d == 0) {
            return; // nothing was indented; don't record an empty undo step
        }
        self.rewrite_lines(first, last, new, &shift);
    }

    /// Comment the selected lines, or uncomment them if they all already are.
    ///
    /// Returns false when nothing happened, which only occurs for a selection of
    /// entirely blank lines that were already bare.
    pub fn toggle_comment(&mut self, prefix: &str) -> bool {
        let (first, last) = self.line_range();
        let mut non_blank = Vec::new();
        let mut min_indent = usize::MAX;
        let mut all_commented = true;
        for r in first..=last {
            let line = &self.lines[r];
            let ws = line.chars().take_while(|c| c.is_whitespace()).count();
            if ws == line.chars().count() {
                continue; // blank: neither commented nor in the way
            }
            non_blank.push(r);
            min_indent = min_indent.min(ws);
            if !line.chars().skip(ws).collect::<String>().starts_with(prefix) {
                all_commented = false;
            }
        }

        let mut new = Vec::with_capacity(last - first + 1);
        let mut shift = Vec::with_capacity(last - first + 1);
        if non_blank.is_empty() {
            return false;
        }
        if all_commented {
            let plen = prefix.chars().count();
            for r in first..=last {
                let line = &self.lines[r];
                if !non_blank.contains(&r) {
                    new.push(line.clone());
                    shift.push(0);
                    continue;
                }
                let ws = line.chars().take_while(|c| c.is_whitespace()).count();
                let rest: String = line.chars().skip(ws).collect();
                // Take the one space commenting added back off, if it's there.
                let body = rest.strip_prefix(prefix).unwrap_or(&rest);
                let (body, extra) = match body.strip_prefix(' ') {
                    Some(b) => (b, 1),
                    None => (body, 0),
                };
                let indent: String = line.chars().take(ws).collect();
                new.push(format!("{indent}{body}"));
                shift.push(-((plen + extra) as isize));
            }
        } else {
            // Insert at the shallowest indent among the lines, so a block keeps
            // its own shape instead of every marker landing at column 0.
            let insert = format!("{prefix} ");
            let add = insert.chars().count() as isize;
            for r in first..=last {
                let line = &self.lines[r];
                if !non_blank.contains(&r) {
                    new.push(line.clone());
                    shift.push(0);
                    continue;
                }
                let head: String = line.chars().take(min_indent).collect();
                let tail: String = line.chars().skip(min_indent).collect();
                new.push(format!("{head}{insert}{tail}"));
                shift.push(add);
            }
        }
        self.rewrite_lines(first, last, new, &shift);
        true
    }

    pub fn view_mode(&self) -> ViewMode {
        self.view_mode
    }

    /// Switch between source and rendered markdown.
    ///
    /// Refuses on anything that isn't markdown, and reports so — a key that
    /// silently does nothing reads as broken. Resets the read scroll, since the
    /// source line you were on has no defined position in the rendering.
    pub fn toggle_read_mode(&mut self) -> bool {
        let markdown = self
            .path
            .as_deref()
            .map(sacrament_core::markdown::is_markdown_path)
            .unwrap_or(false);
        if !markdown {
            return false;
        }
        self.view_mode = match self.view_mode {
            ViewMode::Edit => ViewMode::Read,
            ViewMode::Read => ViewMode::Edit,
        };
        self.read_scroll_row = 0;
        self.read_scroll_seg = 0;
        self.clear_selection();
        true
    }

    /// Put the buffer into read mode, if it can be. Used by session restore,
    /// which needs to *set* the mode rather than flip whatever it happens to be.
    pub fn set_read_mode(&mut self) -> bool {
        if self.view_mode == ViewMode::Read {
            return true;
        }
        self.toggle_read_mode()
    }

    pub fn read_scroll(&self) -> (usize, usize) {
        (self.read_scroll_row, self.read_scroll_seg)
    }

    /// Render if the text or the width has changed since last time.
    pub fn ensure_rendered(&mut self, width: usize) {
        let revision = self.head_revision();
        let fresh = self
            .rendered
            .as_ref()
            .is_some_and(|r| !r.is_stale(width, revision));
        if fresh {
            return;
        }
        self.rendered = Some(crate::read::Rendered::new(
            crate::read::render(&self.to_text(), width),
            width,
            revision,
        ));
    }

    pub fn rendered(&self) -> Option<&crate::read::Rendered> {
        self.rendered.as_ref()
    }

    /// Scroll the rendered view, clamped so the last row can't leave the top.
    pub fn scroll_read(&mut self, delta: isize, viewport_rows: usize, cols: usize) {
        let Some(rendered) = &self.rendered else {
            return;
        };
        let total = crate::read::total_rows(&rendered.lines, cols.max(1));
        // Flattening to an absolute row index keeps this arithmetic instead of a
        // walk, and the wrapped-row count is what the user is actually moving
        // through.
        let current = crate::read::row_index(
            &rendered.lines,
            self.read_scroll_row,
            self.read_scroll_seg,
            cols.max(1),
        );
        let max = total.saturating_sub(viewport_rows.max(1));
        let target = current.saturating_add_signed(delta).min(max);
        let (line, seg) = crate::read::row_at(&rendered.lines, target, cols.max(1));
        self.read_scroll_row = line;
        self.read_scroll_seg = seg;
    }

    /// The syntax in force, so the caller can find its comment marker.
    pub fn syntax_name(&self) -> Option<&str> {
        self.syntax_name.as_deref()
    }

    /// Re-read the file from disk, keeping the caret roughly where it was.
    ///
    /// Refuses when the buffer is dirty: the whole point of reloading is that
    /// the file is the truth, and if there are unsaved edits then it isn't —
    /// overwriting them here would be silent data loss. v1 reloads
    /// unconditionally, which is a real bug rather than a behaviour to port.
    /// The caller decides what to do about a conflict.
    ///
    /// `Ok(false)` means "nothing to do": no path, unreadable, or the file is
    /// exactly what was loaded last time — which is the common case, since our
    /// own save fires the same watch event that lands here.
    pub fn reload(&mut self, hl: Option<&Highlighter>) -> Result<bool, ReloadError> {
        let Some(path) = self.path.clone() else {
            return Ok(false);
        };
        let disk = mtime_of(&path);
        if disk.is_none() || disk == self.known_mtime {
            return Ok(false);
        }
        if self.dirty {
            return Err(ReloadError::Dirty);
        }
        let (row, col, scroll_row, scroll_seg) =
            (self.cursor_row, self.cursor_col, self.scroll_row, self.scroll_seg);
        let syntax_override = self.syntax_override.clone();
        let fresh = Self::load(&path, hl).map_err(ReloadError::Io)?;
        let tab_width = self.tab_width;
        let wrap_width = self.wrap_width;
        *self = fresh;
        self.tab_width = tab_width;
        self.wrap_width = wrap_width;
        if let Some(name) = syntax_override
            && let Some(hl) = hl
        {
            self.set_syntax_override(&name, hl);
        }
        // Clamped, because the file may have shrunk under us.
        let last = self.lines.len().saturating_sub(1);
        self.cursor_row = row.min(last);
        self.cursor_col = col.min(self.line_len(self.cursor_row));
        self.scroll_row = scroll_row.min(last);
        self.scroll_seg = scroll_seg;
        Ok(true)
    }

    /// Throw away local edits and take what's on disk.
    ///
    /// The mirror image of `save_overwriting`, for the other answer to the same
    /// question. Clearing `dirty` first is what gets it past `reload`'s guard —
    /// the guard exists to stop this happening by accident, not by request.
    pub fn discard_and_reload(&mut self, hl: Option<&Highlighter>) {
        self.saved_revision = self.head_revision();
        self.refresh_dirty();
        let _ = self.reload(hl);
    }

    /// Write over a file that changed underneath us, discarding what's there.
    ///
    /// Adopting the disk mtime first is what gets past `save`'s guard. Reaching
    /// for this is always a decision — it's the only way out of a conflict when
    /// the buffer is the version worth keeping.
    pub fn save_overwriting(&mut self) -> Result<(), SaveError> {
        if let Some(path) = self.path.clone() {
            self.known_mtime = mtime_of(&path);
        }
        self.save()
    }

    /// Point the buffer at a new path and write it there.
    ///
    /// `known_mtime` is adopted from the target rather than kept from the old
    /// file, or `save`'s changed-on-disk guard would reject every save-as onto an
    /// existing file. Overwrite confirmation belongs to the caller — this is the
    /// write, not the decision to write.
    pub fn save_as(&mut self, path: PathBuf, hl: Option<&Highlighter>) -> Result<(), SaveError> {
        self.known_mtime = mtime_of(&path);
        self.path = Some(path);
        // The extension may have changed, so the syntax has to be re-derived and
        // the whole highlight cache dropped — every line's parse state was
        // computed under the old grammar.
        if let Some(hl) = hl {
            self.seed_syntax(hl);
        }
        self.invalidate_from(0);
        self.save()
    }

    /// Top of the file. `Cmd+Up` on macOS.
    pub fn move_doc_start(&mut self, extend: bool) {
        self.before_move(extend);
        self.cursor_row = 0;
        self.cursor_col = 0;
    }

    /// End of the last line. `Cmd+Down` on macOS.
    pub fn move_doc_end(&mut self, extend: bool) {
        self.before_move(extend);
        self.cursor_row = self.lines.len().saturating_sub(1);
        self.cursor_col = self.line_len(self.cursor_row);
    }

    pub fn move_page(&mut self, delta: isize, extend: bool) {
        self.before_move(extend);
        let target = self.cursor_row as isize + delta;
        self.cursor_row = target.clamp(0, self.lines.len().saturating_sub(1) as isize) as usize;
        self.cursor_col = self.cursor_col.min(self.line_len(self.cursor_row));
    }

    /// Scroll the minimum needed to keep the cursor on screen.
    ///
    /// Works in screen rows because that's what wrapping makes meaningful: a
    /// cursor on the same *line* as the viewport top can still be several rows
    /// below its bottom.
    pub fn ensure_cursor_visible(&mut self, viewport_rows: usize) {
        let rows = viewport_rows.max(1);
        // Above the viewport: pull the top up to the cursor's own segment.
        let cursor_seg = self.segment_of(self.cursor_row, self.cursor_col);
        if (self.cursor_row, cursor_seg) < (self.scroll_row, self.scroll_seg) {
            self.scroll_row = self.cursor_row;
            self.scroll_seg = cursor_seg;
            return;
        }
        // Below: step the top forward until it comes into view. Bounded by the
        // buffer's screen-row count so a runaway can't spin.
        let mut guard = 0;
        while self.cursor_screen_row(rows).is_none() {
            if !self.scroll_forward_one() {
                break;
            }
            guard += 1;
            if guard > 100_000 {
                break;
            }
        }
    }

    /// Text as it would be written to disk, trailing newline included only if
    /// the file had one.
    pub fn to_text(&self) -> String {
        let mut s = self.lines.join("\n");
        if self.had_trailing_newline {
            s.push('\n');
        }
        s
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Write the buffer to its path.
    ///
    /// Two protections, both because this is the one operation that can destroy
    /// the user's work:
    ///
    /// - **Refuses to clobber.** If the file's mtime no longer matches what we
    ///   read, someone else wrote it and this save would silently discard their
    ///   change. There's no reload/merge UI yet, so refusing is the honest
    ///   behavior — better a message than lost work.
    /// - **Writes via a temp file and renames.** `fs::write` truncates first, so
    ///   a crash or a full disk mid-write leaves a half-written source file.
    ///   Rename within the same directory is atomic, so the file is either the
    ///   old contents or the new ones.
    pub fn save(&mut self) -> Result<(), SaveError> {
        let Some(path) = self.path.clone() else {
            return Err(SaveError::NoPath);
        };
        let on_disk = mtime_of(&path);
        // Only a *mismatch* blocks: a file that didn't exist and still doesn't
        // is a normal first write.
        if on_disk.is_some() && on_disk != self.known_mtime {
            return Err(SaveError::ChangedOnDisk);
        }

        let dir = path.parent().unwrap_or(Path::new("."));
        let tmp = dir.join(format!(
            ".{}.sacrament-tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("buffer")
        ));
        std::fs::write(&tmp, self.to_text()).map_err(SaveError::Io)?;
        if let Err(e) = std::fs::rename(&tmp, &path) {
            let _ = std::fs::remove_file(&tmp);
            return Err(SaveError::Io(e));
        }

        self.known_mtime = mtime_of(&path);
        self.saved_revision = self.head_revision();
        self.refresh_dirty();
        self.break_coalescing();
        Ok(())
    }
}

/// Why a reload didn't happen.
#[derive(Debug)]
pub enum ReloadError {
    /// Unsaved edits — reloading would discard them.
    Dirty,
    Io(std::io::Error),
}

#[derive(Debug)]
pub enum SaveError {
    /// Untitled buffer — `save` has nowhere to write. `save_as` is the way in.
    NoPath,
    /// The file changed underneath us; saving would discard that change.
    ChangedOnDisk,
    Io(std::io::Error),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::NoPath => write!(f, "no file to save to — use Cmd+Shift+S"),
            SaveError::ChangedOnDisk => {
                write!(f, "file changed on disk — not overwriting; reopen to pick up changes")
            }
            SaveError::Io(e) => write!(f, "save failed: {e}"),
        }
    }
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).ok()?.modified().ok()
}


// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

impl Buffer {
    /// Normalized `(start, end)` in document order, or `None` when there's no
    /// selection or it's collapsed.
    pub fn selection_range(&self) -> Option<(Pos, Pos)> {
        let anchor = self.selection_anchor?;
        let cursor = (self.cursor_row, self.cursor_col);
        if anchor == cursor {
            return None;
        }
        Some(if anchor < cursor {
            (anchor, cursor)
        } else {
            (cursor, anchor)
        })
    }

    pub fn has_selection(&self) -> bool {
        self.selection_range().is_some()
    }

    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Start a selection at the current cursor, if one isn't already running.
    /// Called by shift-movement; plain movement clears instead.
    pub fn begin_selection(&mut self) {
        if self.selection_anchor.is_none() {
            self.selection_anchor = Some(self.cursor());
        }
    }

    /// Is this cell inside the selection? Used by `fill` per cell, so it stays
    /// arithmetic on the normalized range rather than any per-row allocation.
    fn is_selected(&self, row: usize, col: usize) -> bool {
        let Some(((sr, sc), (er, ec))) = self.selection_range() else {
            return false;
        };
        if row < sr || row > er {
            return false;
        }
        if sr == er {
            return col >= sc && col < ec;
        }
        if row == sr {
            return col >= sc;
        }
        if row == er {
            return col < ec;
        }
        true
    }

    /// Character count from `from` to `to`, counting each line break as one.
    /// This is what `replace` wants for `remove`.
    fn span_len(&self, from: Pos, to: Pos) -> usize {
        if from.0 == to.0 {
            return to.1.saturating_sub(from.1);
        }
        let mut n = self.line_len(from.0).saturating_sub(from.1) + 1;
        for row in (from.0 + 1)..to.0 {
            n += self.line_len(row) + 1;
        }
        n + to.1
    }

    pub fn selected_text(&self) -> Option<String> {
        let ((sr, sc), (er, ec)) = self.selection_range()?;
        if sr == er {
            let line = &self.lines[sr];
            let (a, b) = (Self::byte_of(line, sc), Self::byte_of(line, ec));
            return Some(line[a..b].to_string());
        }
        let mut out = String::new();
        let first = &self.lines[sr];
        out.push_str(&first[Self::byte_of(first, sc)..]);
        for row in (sr + 1)..er {
            out.push('\n');
            out.push_str(&self.lines[row]);
        }
        out.push('\n');
        let last = &self.lines[er];
        out.push_str(&last[..Self::byte_of(last, ec)]);
        Some(out)
    }

    /// Replace the selection with `text` as a **single** undo step. Returns false
    /// if there was no selection.
    ///
    /// This is why every edit routes through `replace`: deleting a region and
    /// inserting in its place is one call, so typing over a selection undoes in
    /// one go rather than two.
    fn replace_selection(&mut self, text: &str) -> bool {
        let Some((start, end)) = self.selection_range() else {
            return false;
        };
        let len = self.span_len(start, end);
        self.clear_selection();
        self.break_coalescing();
        let end_pos = self.edit(start, len, text);
        self.set_cursor(end_pos);
        self.break_coalescing();
        true
    }

    /// Delete the selection, if any. Returns whether anything was removed.
    pub fn delete_selection(&mut self) -> bool {
        self.replace_selection("")
    }

    /// Select the word under `pos`. Returns false when there's no word there, so
    /// a double click on whitespace doesn't produce an empty selection.
    pub fn select_word_at(&mut self, pos: Pos) -> bool {
        let (row, col) = self.clamp_pos(pos);
        let chars: Vec<char> = self.lines[row].chars().collect();
        if col >= chars.len() || !is_word_char(chars[col]) {
            return false;
        }
        let mut start = col;
        let mut end = col;
        while start > 0 && is_word_char(chars[start - 1]) {
            start -= 1;
        }
        while end < chars.len() && is_word_char(chars[end]) {
            end += 1;
        }
        self.selection_anchor = Some((row, start));
        self.cursor_row = row;
        self.cursor_col = end;
        self.break_coalescing();
        true
    }

    pub fn select_all(&mut self) {
        let last = self.lines.len().saturating_sub(1);
        self.selection_anchor = Some((0, 0));
        self.cursor_row = last;
        self.cursor_col = self.line_len(last);
        self.break_coalescing();
    }

    /// Select the whole line under `pos`, including its trailing break when there
    /// is one, so a line selection pasted elsewhere arrives as a whole line.
    pub fn select_line_at(&mut self, pos: Pos) {
        let (row, _) = self.clamp_pos(pos);
        self.selection_anchor = Some((row, 0));
        if row + 1 < self.lines.len() {
            self.cursor_row = row + 1;
            self.cursor_col = 0;
        } else {
            self.cursor_row = row;
            self.cursor_col = self.line_len(row);
        }
        self.break_coalescing();
    }
}

/// One-to-one lowercase fold, for case-insensitive search.
///
/// Deliberately *not* `char::to_lowercase`, which yields an iterator because some
/// characters lowercase to several. Search needs char indices in the folded text
/// to line up exactly with the original line, so a fold that can change length is
/// unusable here — taking the first character keeps the mapping one-to-one.
fn fold_char(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// A buffer wired up as something the grid can draw.
///
/// Holds the `Arc` rather than a borrow: a `MutexGuard` taken while building the
/// view can't live long enough to reach `draw`, so the lock is taken inside
/// `fill` instead. The gutter widget does the same thing for the same reason.
///
/// It derives its own row mapping from its own viewport height rather than
/// being handed one. That's what keeps it in step with the gutter without any
/// plumbing between them: same function, same inputs, same answer.
pub struct BufferSource {
    pub buffer: Arc<Mutex<Buffer>>,
    /// Viewport height in rows. Needed because the caret's screen row depends on
    /// how many wrapped rows precede it, which depends on the viewport.
    pub rows: usize,
    /// Shared so lazy highlighting can happen during the draw that needs it —
    /// same arrangement as v1, where `render_body` calls `ensure_highlights`.
    /// `None` when `syntax_highlighting = false`.
    pub highlighter: Option<Arc<Highlighter>>,
    /// Whether this pane has focus — controls whether a caret is drawn at all.
    pub focused: bool,
}

impl GridSource for BufferSource {
    fn fill(&self, palette: &Palette, rows: usize, cols: usize, out: &mut Vec<Vec<Cell>>) {
        reset_rows(out, rows);
        let Ok(mut buffer) = self.buffer.lock() else {
            return;
        };
        let visible = buffer.visible_rows(rows);
        // Parse only what's on screen. Opening a large file costs nothing until
        // it's scrolled.
        if let Some(last) = visible.last()
            && let Some(hl) = &self.highlighter
        {
            buffer.ensure_highlights(last.line, hl);
        }
        let buffer = &*buffer;
        let fg = palette.foreground;
        let bg = palette.background;

        for (screen_row, vr) in visible.iter().take(rows).enumerate() {
            let Some(text) = buffer.lines.get(vr.line) else {
                continue;
            };
            let spans = buffer.highlights.get(vr.line).and_then(|s| s.as_deref());
            let dest = &mut out[screen_row];

            // Walk chars with their byte offsets so highlight spans (which are
            // byte-ranged) can be matched without a second pass.
            let mut span_cursor = 0usize;
            // Start past the hanging indent, emitting it as blanks so every
            // downstream column lines up with what's drawn.
            let mut vis = vr.indent.min(cols);
            for _ in 0..vis {
                dest.push(Cell::blank(fg, bg));
            }
            for (col, (byte_offset, c)) in text.char_indices().enumerate() {
                if col < vr.start {
                    continue;
                }
                if col >= vr.end || vis >= cols {
                    break;
                }
                let width = text::char_display_width(c, vis, buffer.tab_width);
                let mut cell = Cell::blank(fg, bg);
                // A tab is one character but several columns: emit spaces so the
                // grid stays a grid and every downstream position (caret, click,
                // selection) computes in the same units.
                cell.c = if c == '\t' { ' ' } else { c };
                let selected = buffer.is_selected(vr.line, col);
                if let Some(spans) = spans {
                    while span_cursor < spans.len() && byte_offset >= spans[span_cursor].byte_end {
                        span_cursor += 1;
                    }
                    if let Some(span) = spans.get(span_cursor)
                        && byte_offset >= span.byte_start
                    {
                        if let Some(slot) = span.color {
                            cell.fg = palette.ansi_slot(slot.index());
                        }
                        cell.bold = span.emphasis.bold;
                        cell.italic = span.emphasis.italic;
                        cell.underline = span.emphasis.underline;
                    }
                }
                // Selection overrides syntax color — both come from the theme, and
                // a highlighted region has to be legible regardless of what's
                // under it.
                if selected {
                    cell.fg = palette.selection_foreground;
                    cell.bg = palette.selection_background;
                }
                // Zero-width characters occupy no cell; wide ones occupy their
                // extra columns as blanks carrying the same styling.
                for i in 0..width {
                    if vis + i >= cols {
                        break;
                    }
                    let mut c = cell;
                    if i > 0 {
                        c.c = ' ';
                    }
                    dest.push(c);
                }
                vis += width;
            }
        }
    }

    fn cursor(&self) -> Option<(usize, usize)> {
        if !self.focused {
            return None;
        }
        let buffer = self.buffer.lock().ok()?;
        let rows = self.rows.max(1);
        let row = buffer.cursor_screen_row(rows)?;
        // Column is visual and segment-relative: tabs occupy several columns, and
        // a wrapped row restarts at zero.
        Some((
            row,
            buffer.screen_col_of(buffer.cursor_row, buffer.cursor_col),
        ))
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf_of(lines: &[&str]) -> Buffer {
        let mut b = Buffer::empty();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        let n = b.lines.len();
        b.line_state_before = vec![None; n];
        b.highlights = vec![None; n];
        b
    }

    impl Buffer {
        fn clone_for_test(&self) -> Buffer {
            let mut b = Buffer::empty();
            b.lines = self.lines.clone();
            let n = b.lines.len();
            b.line_state_before = vec![None; n];
            b.highlights = vec![None; n];
            b
        }
    }

    fn source(b: Buffer, focused: bool) -> BufferSource {
        BufferSource {
            buffer: Arc::new(Mutex::new(b)),
            rows: 40,
            highlighter: Some(Arc::new(Highlighter::new())),
            focused,
        }
    }

    #[test]
    fn number_width_tracks_line_count() {
        assert_eq!(buf_of(&["a"]).number_width(), 1);
        assert_eq!(buf_of(&[""; 9]).number_width(), 1);
        assert_eq!(buf_of(&[""; 10]).number_width(), 2);
        assert_eq!(buf_of(&[""; 200]).number_width(), 3);
    }

    #[test]
    fn visible_rows_start_at_scroll_and_stop_at_eof() {
        let mut b = buf_of(&["one", "two", "three"]);
        b.scroll_row = 1;
        let rows = b.visible_rows(10);
        assert_eq!(rows.len(), 2, "must not invent rows past the last line");
        assert_eq!(rows[0].line, 1);
        assert_eq!(rows[1].line, 2);
    }

    #[test]
    fn visible_rows_respects_viewport_height() {
        let b = buf_of(&["a", "b", "c", "d"]);
        assert_eq!(b.visible_rows(2).len(), 2);
    }

    /// The alignment guarantee: the gutter and the grid must describe the same
    /// rows. They call different methods, so this pins them to one another.
    #[test]
    fn gutter_rows_and_visible_rows_describe_the_same_screen() {
        let mut b = buf_of(&["a", "b", "c", "d", "e"]);
        b.scroll_row = 2;
        let visible = b.visible_rows(3);
        let gutter = b.gutter_rows(3);
        assert_eq!(visible.len(), gutter.len());
        for (v, g) in visible.iter().zip(gutter.iter()) {
            assert_eq!(g.number, Some(v.line + 1));
        }
    }

    #[test]
    fn gutter_numbers_are_one_based_and_track_cursor() {
        let mut b = buf_of(&["a", "b", "c"]);
        b.cursor_row = 1;
        let g = b.gutter_rows(3);
        assert_eq!(g[0].number, Some(1));
        assert_eq!(g[1].number, Some(2));
        assert!(g[1].is_cursor_line);
        assert!(!g[0].is_cursor_line);
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut b = buf_of(&["a", "b", "c", "d", "e"]);
        b.scroll_by(-5, 3);
        assert_eq!(b.scroll_row, 0, "cannot scroll above the first line");
        b.scroll_by(100, 3);
        assert_eq!(b.scroll_row, 2, "last screenful stays filled");
    }

    #[test]
    fn scroll_is_a_noop_when_content_fits() {
        let mut b = buf_of(&["a", "b"]);
        b.scroll_by(50, 10);
        assert_eq!(b.scroll_row, 0);
    }

    #[test]
    fn fill_projects_text_into_cells() {
        let src = source(buf_of(&["hi", "there"]), true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 2, 80, &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].iter().map(|c| c.c).collect::<String>(), "hi");
        assert_eq!(out[1].iter().map(|c| c.c).collect::<String>(), "there");
    }

    #[test]
    fn fill_truncates_at_column_limit() {
        let src = source(buf_of(&["abcdefghij"]), true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 1, 4, &mut out);
        assert_eq!(out[0].len(), 4);
    }

    #[test]
    fn fill_reuses_the_scratch_buffer_across_calls() {
        let src = source(buf_of(&["aaa", "bbb"]), true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 2, 80, &mut out);
        src.fill(&Palette::default(), 2, 80, &mut out);
        // Rows must be cleared, not appended to, or content doubles each frame.
        assert_eq!(out[0].len(), 3);
        assert_eq!(out.len(), 2);
    }

    #[test]
    fn fill_shrinks_when_the_viewport_does() {
        let src = source(buf_of(&["a", "b", "c"]), true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 3, 80, &mut out);
        src.fill(&Palette::default(), 1, 80, &mut out);
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn unfocused_pane_draws_no_caret() {
        assert!(source(buf_of(&["a"]), false).cursor().is_none());
    }

    #[test]
    fn caret_hidden_when_cursor_line_is_scrolled_off() {
        let mut b = buf_of(&["a", "b", "c"]);
        b.cursor_row = 0;
        b.scroll_row = 2;
        assert!(source(b, true).cursor().is_none());
    }

    #[test]
    fn caret_row_is_relative_to_the_viewport() {
        let mut b = buf_of(&["a", "b", "c", "d"]);
        b.cursor_row = 3;
        b.scroll_row = 2;
        assert_eq!(source(b, true).cursor(), Some((1, 0)));
    }

    #[test]
    fn caret_sits_on_the_cursor_cell() {
        // The editor caret is a block on the current character, same as the
        // terminal's — so its column is the cursor column, not an inter-character
        // position.
        let mut b = buf_of(&["abc"]);
        b.cursor_col = 2;
        assert_eq!(source(b, true).cursor(), Some((0, 2)));
    }

    // --- soft wrap -------------------------------------------------------

    fn wrapped(lines: &[&str], width: usize) -> Buffer {
        let mut b = buf_of(lines);
        b.wrap_width = width;
        b
    }

    #[test]
    fn a_long_line_occupies_several_screen_rows() {
        let b = wrapped(&["aaaa bbbb cccc dddd"], 10);
        let rows = b.visible_rows(10);
        assert!(rows.len() > 1, "expected wrapping, got {rows:?}");
        assert!(rows.iter().all(|r| r.line == 0), "all rows are the same line");
        assert!(rows[0].is_first_segment);
        assert!(!rows[1].is_first_segment, "continuations are not first segments");
    }

    /// The gutter prints a number only on a first segment. If this breaks, wrapped
    /// rows get spurious line numbers.
    #[test]
    fn continuation_rows_get_no_gutter_number() {
        let b = wrapped(&["aaaa bbbb cccc dddd", "short"], 10);
        let g = b.gutter_rows(10);
        assert_eq!(g[0].number, Some(1));
        assert_eq!(g[1].number, None, "continuation");
        assert!(g.iter().filter(|r| r.number == Some(2)).count() == 1);
    }

    #[test]
    fn gutter_and_visible_rows_still_agree_when_wrapped() {
        let b = wrapped(&["aaaa bbbb cccc dddd", "x", "yyyy zzzz"], 6);
        let v = b.visible_rows(12);
        let g = b.gutter_rows(12);
        assert_eq!(v.len(), g.len());
        for (vr, gr) in v.iter().zip(g.iter()) {
            let expected = vr.is_first_segment.then_some(vr.line + 1);
            assert_eq!(gr.number, expected);
        }
    }

    #[test]
    fn continuation_rows_hang_under_the_lines_indentation() {
        let b = wrapped(&["    aaaa bbbb cccc dddd eeee"], 16);
        let rows = b.visible_rows(10);
        assert!(rows.len() > 1);
        assert_eq!(rows[0].indent, 0, "first segment sits at the margin");
        assert_eq!(rows[1].indent, 4, "continuation hangs under the indent");
    }

    #[test]
    fn an_unindented_line_has_no_hanging_indent() {
        let b = wrapped(&["aaaa bbbb cccc dddd"], 10);
        assert!(b.visible_rows(10).iter().all(|r| r.indent == 0));
    }

    #[test]
    fn the_indent_is_rendered_as_leading_blanks() {
        let b = wrapped(&["    aaaa bbbb cccc dddd"], 14);
        let rows = b.visible_rows(10);
        let indent = rows[1].indent;
        assert!(indent > 0);
        let src = source(b, true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 10, 14, &mut out);
        let second: String = out[1].iter().map(|c| c.c).collect();
        assert!(
            second.starts_with(&" ".repeat(indent)),
            "continuation should start with {indent} blanks, got {second:?}"
        );
    }

    #[test]
    fn a_click_in_the_hanging_indent_lands_on_the_segment_start() {
        let b = wrapped(&["    aaaa bbbb cccc dddd"], 14);
        let rows = b.visible_rows(10);
        let vr = rows[1];
        assert!(vr.indent > 0);
        for col in 0..vr.indent {
            assert_eq!(b.screen_to_doc(1, col, 10), (0, vr.start));
        }
    }

    /// Moving vertically across the indent boundary has to hold the *screen*
    /// column, not the segment-relative one, or the caret jumps sideways.
    #[test]
    fn vertical_movement_holds_the_screen_column_across_the_indent() {
        let mut b = wrapped(&["    aaaa bbbb cccc dddd eeee"], 16);
        let rows = b.visible_rows(10);
        assert!(rows.len() > 2 && rows[1].indent > 0);

        b.cursor_col = 8;
        let before = b.screen_col_of(0, b.cursor_col);
        b.move_down(false);
        let after = b.screen_col_of(b.cursor_row, b.cursor_col);
        assert_eq!(after, before, "screen column preserved moving down");
        b.move_up(false);
        assert_eq!(b.screen_col_of(0, b.cursor_col), before, "and back up");
    }

    #[test]
    fn wrapping_off_gives_one_row_per_line() {
        let b = wrapped(&["a very long line indeed", "b"], 0);
        let rows = b.visible_rows(10);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.is_first_segment));
    }

    #[test]
    fn scrolling_steps_through_segments_not_whole_lines() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd", "next"], 10);
        assert!(b.segments(0).len() >= 2);
        // Viewport smaller than the content, or there'd be nothing to scroll.
        b.scroll_by(1, 2);
        assert_eq!(b.scroll_row, 0, "still inside the first line");
        assert_eq!(b.scroll_seg, 1, "advanced by one segment");
    }

    #[test]
    fn a_buffer_that_fits_the_viewport_does_not_scroll() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd", "next"], 10);
        b.scroll_by(5, 20);
        assert_eq!((b.scroll_row, b.scroll_seg), (0, 0));
    }

    #[test]
    fn scroll_back_crosses_into_the_previous_lines_last_segment() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd", "next"], 10);
        b.scroll_row = 1;
        b.scroll_seg = 0;
        b.scroll_by(-1, 4);
        assert_eq!(b.scroll_row, 0);
        assert_eq!(b.scroll_seg, b.segments(0).len() - 1);
    }

    #[test]
    fn down_moves_within_a_wrapped_line() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd"], 10);
        b.cursor_col = 2;
        b.move_down(false);
        assert_eq!(b.cursor_row, 0, "same line");
        assert!(b.cursor_col > 2, "moved into the next segment");
        b.move_up(false);
        assert_eq!(b.cursor_col, 2, "and back, holding the visual column");
    }

    #[test]
    fn home_and_end_act_on_the_screen_row_when_wrapped() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd"], 10);
        let segs = b.segments(0);
        // Put the cursor inside the second segment.
        b.cursor_col = segs[1].0 + 1;
        b.move_home(false);
        assert_eq!(b.cursor_col, segs[1].0, "start of the segment, not the line");
        b.move_end(false);
        assert!(b.cursor_col <= segs[1].1);
        assert!(b.cursor_col > segs[1].0);
    }

    #[test]
    fn caret_row_accounts_for_wrapped_rows_above_it() {
        let mut b = wrapped(&["aaaa bbbb cccc dddd", "second"], 10);
        let first_rows = b.segments(0).len();
        b.cursor_row = 1;
        b.cursor_col = 0;
        assert_eq!(
            b.cursor_screen_row(20),
            Some(first_rows),
            "line 2 sits below all of line 1's segments"
        );
    }

    #[test]
    fn screen_to_doc_inverts_the_wrap_mapping() {
        let b = wrapped(&["aaaa bbbb cccc dddd", "second"], 10);
        let rows = b.visible_rows(20);
        for (i, vr) in rows.iter().enumerate() {
            let (line, col) = b.screen_to_doc(i, 0, 20);
            assert_eq!(line, vr.line);
            assert_eq!(col, vr.start, "column 0 of a row is its segment start");
        }
    }

    #[test]
    fn a_click_on_a_tab_resolves_to_the_tab() {
        let mut b = wrapped(&["\tx"], 40);
        b.tab_width = 4;
        // Visual columns 0..3 are all the tab.
        for vis in 0..4 {
            assert_eq!(b.screen_to_doc(0, vis, 10), (0, 0));
        }
        assert_eq!(b.screen_to_doc(0, 4, 10), (0, 1), "column 4 is the x");
    }

    #[test]
    fn tabs_are_expanded_to_cells_in_the_render() {
        let mut b = wrapped(&["\tx"], 40);
        b.tab_width = 4;
        let src = source(b, true);
        let mut out = Vec::new();
        src.fill(&Palette::default(), 1, 40, &mut out);
        let text: String = out[0].iter().map(|c| c.c).collect();
        assert_eq!(text, "    x", "a tab occupies four cells");
    }

    #[test]
    fn ensure_cursor_visible_counts_screen_rows_not_lines() {
        // A cursor on a line that's *partly* visible must still scroll into view.
        let mut b = wrapped(&["aaaa bbbb cccc dddd eeee ffff"], 6);
        let segs = b.segments(0).len();
        assert!(segs > 3, "need more segments than the viewport");
        b.cursor_col = b.lines[0].chars().count();
        b.ensure_cursor_visible(3);
        assert!(
            b.cursor_screen_row(3).is_some(),
            "cursor must be on screen after scrolling"
        );
    }

    // --- editing ---------------------------------------------------------

    fn text_of(b: &Buffer) -> String {
        b.lines.join("\n")
    }

    /// The invariant most likely to break silently: any line-count change must
    /// splice both positional caches, or highlighting starts belonging to the
    /// wrong line.
    fn assert_caches_in_lockstep(b: &Buffer) {
        assert_eq!(b.highlights.len(), b.lines.len(), "highlights desynced");
        assert_eq!(
            b.line_state_before.len(),
            b.lines.len(),
            "line_state_before desynced"
        );
    }

    #[test]
    fn insert_char_advances_the_cursor() {
        let mut b = buf_of(&["ac"]);
        b.cursor_col = 1;
        b.insert_char('b');
        assert_eq!(text_of(&b), "abc");
        assert_eq!(b.cursor_col, 2);
        assert!(b.dirty);
    }

    #[test]
    fn insert_newline_splits_and_keeps_caches_in_lockstep() {
        let mut b = buf_of(&["abcd"]);
        b.cursor_col = 2;
        b.insert_newline();
        assert_eq!(text_of(&b), "ab\ncd");
        assert_eq!((b.cursor_row, b.cursor_col), (1, 0));
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn backspace_at_column_zero_joins_lines_and_keeps_lockstep() {
        let mut b = buf_of(&["ab", "cd"]);
        b.cursor_row = 1;
        b.cursor_col = 0;
        b.backspace();
        assert_eq!(text_of(&b), "abcd");
        assert_eq!((b.cursor_row, b.cursor_col), (0, 2), "cursor lands at the seam");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn backspace_at_start_of_file_is_a_noop() {
        let mut b = buf_of(&["ab"]);
        b.backspace();
        assert_eq!(text_of(&b), "ab");
    }

    #[test]
    fn delete_forward_at_end_of_line_pulls_the_next_line_up() {
        let mut b = buf_of(&["ab", "cd"]);
        b.cursor_col = 2;
        b.delete_forward();
        assert_eq!(text_of(&b), "abcd");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn delete_forward_at_end_of_file_is_a_noop() {
        let mut b = buf_of(&["ab"]);
        b.cursor_col = 2;
        b.delete_forward();
        assert_eq!(text_of(&b), "ab");
    }

    #[test]
    fn editing_is_char_indexed_not_byte_indexed() {
        // é is two bytes; a byte-indexed insert would split it or panic.
        let mut b = buf_of(&["é"]);
        b.cursor_col = 1;
        b.insert_char('x');
        assert_eq!(text_of(&b), "éx");
        b.cursor_col = 1;
        b.backspace();
        assert_eq!(text_of(&b), "x");
    }

    #[test]
    fn cursor_wraps_across_lines() {
        let mut b = buf_of(&["ab", "cd"]);
        b.cursor_col = 2;
        b.move_right(false);
        assert_eq!((b.cursor_row, b.cursor_col), (1, 0));
        b.move_left(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 2));
    }

    #[test]
    fn vertical_movement_clamps_to_shorter_lines() {
        let mut b = buf_of(&["longer line", "hi"]);
        b.cursor_col = 9;
        b.move_down(false);
        assert_eq!(b.cursor_col, 2, "clamped to the shorter line");
    }

    #[test]
    fn ensure_cursor_visible_scrolls_the_minimum() {
        let mut b = buf_of(&["a", "b", "c", "d", "e", "f"]);
        b.cursor_row = 5;
        b.ensure_cursor_visible(3);
        assert_eq!(b.scroll_row, 3, "cursor lands on the last visible row");
        b.cursor_row = 0;
        b.ensure_cursor_visible(3);
        assert_eq!(b.scroll_row, 0);
    }

    #[test]
    fn ensure_cursor_visible_leaves_an_onscreen_cursor_alone() {
        let mut b = buf_of(&["a", "b", "c", "d"]);
        b.scroll_row = 1;
        b.cursor_row = 2;
        b.ensure_cursor_visible(3);
        assert_eq!(b.scroll_row, 1);
    }

    #[test]
    fn clamp_to_content_pulls_a_click_past_the_line_end_back() {
        let mut b = buf_of(&["ab", "cdef"]);
        b.cursor_row = 0;
        b.cursor_col = 99;
        b.clamp_to_content();
        assert_eq!(b.cursor_col, 2);
    }

    #[test]
    fn insert_str_handles_embedded_newlines() {
        let mut b = buf_of(&["ad"]);
        b.cursor_col = 1;
        b.insert_str("b\nc");
        assert_eq!(text_of(&b), "ab\ncd");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn edits_invalidate_highlights_from_the_edited_line_onward() {
        let mut b = buf_of(&["a", "b", "c"]);
        for h in b.highlights.iter_mut() {
            *h = Some(Vec::new());
        }
        b.cursor_row = 1;
        b.insert_char('x');
        assert!(b.highlights[0].is_some(), "lines before the edit stay valid");
        assert!(b.highlights[1].is_none());
        assert!(b.highlights[2].is_none());
    }

    // --- undo ------------------------------------------------------------

    #[test]
    fn undo_reverses_an_insert() {
        let mut b = buf_of(&["ac"]);
        b.cursor_col = 1;
        b.insert_char('b');
        assert_eq!(text_of(&b), "abc");
        assert!(b.undo());
        assert_eq!(text_of(&b), "ac");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn undo_restores_the_cursor_and_the_clean_flag() {
        let mut b = buf_of(&["ac"]);
        b.cursor_col = 1;
        assert!(!b.dirty);
        b.insert_char('b');
        assert!(b.dirty);
        b.undo();
        assert_eq!((b.cursor_row, b.cursor_col), (0, 1), "cursor goes back too");
        assert!(!b.dirty, "undoing the only edit returns to clean");
    }

    #[test]
    fn redo_reapplies_an_undone_edit() {
        let mut b = buf_of(&["ac"]);
        b.cursor_col = 1;
        b.insert_char('b');
        b.undo();
        assert!(b.redo());
        assert_eq!(text_of(&b), "abc");
        assert!(!b.redo(), "nothing further to redo");
    }

    #[test]
    fn typing_a_run_coalesces_into_one_undo_step() {
        let mut b = buf_of(&[""]);
        for c in "hello".chars() {
            b.insert_char(c);
        }
        assert_eq!(text_of(&b), "hello");
        assert!(b.undo());
        assert_eq!(text_of(&b), "", "a typing run undoes as one step");
        assert!(!b.can_undo());
    }

    #[test]
    fn moving_the_cursor_breaks_the_coalescing_run() {
        let mut b = buf_of(&[""]);
        b.insert_char('a');
        b.insert_char('b');
        b.move_left(false);
        b.move_right(false);
        b.insert_char('c');
        b.undo();
        assert_eq!(text_of(&b), "ab", "only the post-move typing was undone");
        b.undo();
        assert_eq!(text_of(&b), "");
    }

    #[test]
    fn a_newline_is_its_own_undo_step() {
        let mut b = buf_of(&[""]);
        b.insert_char('a');
        b.insert_newline();
        b.insert_char('b');
        b.undo();
        assert_eq!(text_of(&b), "a\n");
        b.undo();
        assert_eq!(text_of(&b), "a");
        b.undo();
        assert_eq!(text_of(&b), "");
    }

    #[test]
    fn a_multiline_paste_is_one_undo_step() {
        let mut b = buf_of(&["xy"]);
        b.cursor_col = 1;
        b.insert_str("A\nB\nC");
        assert_eq!(text_of(&b), "xA\nB\nCy");
        assert!(b.undo());
        assert_eq!(text_of(&b), "xy");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn undo_restores_a_joined_line() {
        // The multi-line case: backspace at column zero merges two lines, and
        // undoing it has to split them again and put the caches back.
        let mut b = buf_of(&["ab", "cd"]);
        b.cursor_row = 1;
        b.cursor_col = 0;
        b.backspace();
        assert_eq!(text_of(&b), "abcd");
        assert!(b.undo());
        assert_eq!(text_of(&b), "ab\ncd");
        assert_eq!(b.lines.len(), 2);
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn undo_restores_a_forward_deleted_join() {
        let mut b = buf_of(&["ab", "cd"]);
        b.cursor_col = 2;
        b.delete_forward();
        assert_eq!(text_of(&b), "abcd");
        b.undo();
        assert_eq!(text_of(&b), "ab\ncd");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn a_new_edit_discards_the_redo_stack() {
        let mut b = buf_of(&[""]);
        b.insert_char('a');
        b.undo();
        assert!(b.can_redo());
        b.insert_char('z');
        assert!(!b.can_redo(), "branching history drops the old future");
    }

    #[test]
    fn undo_past_the_beginning_reports_false() {
        let mut b = buf_of(&["a"]);
        assert!(!b.undo());
        assert!(!b.can_undo());
    }

    /// The property that matters most: any sequence of edits, fully undone,
    /// returns the exact original text.
    #[test]
    fn full_undo_round_trips_to_the_original() {
        let original = ["fn main() {", "    let x = 1;", "}"];
        let mut b = buf_of(&original);
        let before = text_of(&b);

        b.cursor_row = 1;
        b.cursor_col = 4;
        b.insert_str("// note\n    ");
        b.move_down(false);
        b.insert_char('!');
        b.insert_char('?');
        b.move_up(false);
        b.backspace();
        b.insert_newline();
        b.cursor_row = 0;
        b.cursor_col = 0;
        b.delete_forward();

        let mut guard = 0;
        while b.undo() {
            guard += 1;
            assert!(guard < 100, "undo failed to terminate");
        }
        assert_eq!(text_of(&b), before);
        assert!(!b.dirty, "back to the original means back to clean");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn full_redo_returns_to_the_edited_state() {
        let mut b = buf_of(&["one", "two"]);
        b.insert_char('X');
        b.insert_newline();
        b.cursor_row = 1;
        b.insert_str("mid");
        let edited = text_of(&b);
        while b.undo() {}
        while b.redo() {}
        assert_eq!(text_of(&b), edited);
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn undo_history_is_capped() {
        let mut b = buf_of(&[""]);
        // Each insert is its own step because the move breaks coalescing.
        for _ in 0..(MAX_UNDO + 50) {
            b.insert_char('x');
            b.break_coalescing();
        }
        assert!(b.undo.len() <= MAX_UNDO, "history must not grow without bound");
    }

    #[test]
    fn undo_past_a_save_reports_dirty_again() {
        let path = scratch("undo-save", "abc\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        assert!(!b.dirty);
        b.undo();
        assert_eq!(text_of(&b), "abc");
        assert!(
            b.dirty,
            "buffer now differs from the saved file, so it is dirty"
        );
        // And redoing back to the saved state is clean again.
        b.redo();
        assert!(!b.dirty);
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn branching_after_a_save_is_dirty_even_at_the_same_depth() {
        // Depth alone can't distinguish these states; monotonic revisions can.
        let path = scratch("undo-branch", "a\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        b.undo();
        b.insert_char('y'); // same history depth as the saved state, different text
        assert!(b.dirty, "a different edit at the same depth is still dirty");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    // --- selection -------------------------------------------------------

    #[test]
    fn selection_normalizes_regardless_of_drag_direction() {
        let mut b = buf_of(&["abcdef"]);
        b.selection_anchor = Some((0, 4));
        b.cursor_col = 1;
        assert_eq!(b.selection_range(), Some(((0, 1), (0, 4))));
        assert_eq!(b.selected_text().unwrap(), "bcd");
    }

    #[test]
    fn a_collapsed_selection_is_no_selection() {
        let mut b = buf_of(&["abc"]);
        b.cursor_col = 2;
        b.selection_anchor = Some((0, 2));
        assert!(!b.has_selection());
        assert_eq!(b.selected_text(), None);
    }

    #[test]
    fn selection_spans_lines() {
        let mut b = buf_of(&["one", "two", "three"]);
        b.selection_anchor = Some((0, 1));
        b.cursor_row = 2;
        b.cursor_col = 2;
        assert_eq!(b.selected_text().unwrap(), "ne\ntwo\nth");
    }

    #[test]
    fn is_selected_matches_the_extracted_text() {
        // The renderer uses is_selected per cell while copy uses selected_text;
        // if they disagree, what you see isn't what you get.
        let mut b = buf_of(&["abcd", "efgh"]);
        b.selection_anchor = Some((0, 2));
        b.cursor_row = 1;
        b.cursor_col = 2;
        let mut painted = String::new();
        for row in 0..2 {
            for col in 0..4 {
                if b.is_selected(row, col) {
                    painted.push(b.lines[row].chars().nth(col).unwrap());
                }
            }
        }
        assert_eq!(painted, "cdef");
        assert_eq!(b.selected_text().unwrap(), "cd\nef");
    }

    #[test]
    fn typing_over_a_selection_replaces_it_in_one_undo_step() {
        let mut b = buf_of(&["hello world"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_col = 5;
        b.insert_char('X');
        assert_eq!(text_of(&b), "X world");
        assert!(!b.has_selection());
        assert!(b.undo());
        assert_eq!(text_of(&b), "hello world", "one step, not two");
        assert!(!b.can_undo());
    }

    #[test]
    fn backspace_with_a_selection_deletes_the_region() {
        let mut b = buf_of(&["abc", "def"]);
        b.selection_anchor = Some((0, 1));
        b.cursor_row = 1;
        b.cursor_col = 2;
        b.backspace();
        assert_eq!(text_of(&b), "af");
        b.undo();
        assert_eq!(text_of(&b), "abc\ndef");
        assert_caches_in_lockstep(&b);
    }

    #[test]
    fn pasting_over_a_selection_replaces_it() {
        let mut b = buf_of(&["keep DROP keep"]);
        b.selection_anchor = Some((0, 5));
        b.cursor_col = 9;
        b.insert_str("NEW\nTEXT");
        assert_eq!(text_of(&b), "keep NEW\nTEXT keep");
        assert!(b.undo());
        assert_eq!(text_of(&b), "keep DROP keep");
    }

    #[test]
    fn shift_movement_extends_and_plain_movement_clears() {
        let mut b = buf_of(&["abcdef"]);
        b.move_right(true);
        b.move_right(true);
        assert_eq!(b.selected_text().unwrap(), "ab");
        b.move_right(false);
        assert!(!b.has_selection(), "moving without shift drops the selection");
    }

    #[test]
    fn shift_movement_can_extend_backwards_past_the_anchor() {
        let mut b = buf_of(&["abcdef"]);
        b.cursor_col = 3;
        b.move_left(true);
        b.move_left(true);
        assert_eq!(b.selected_text().unwrap(), "bc");
    }

    #[test]
    fn double_click_selects_a_word_not_its_punctuation() {
        let mut b = buf_of(&["let foo_bar = 1;"]);
        assert!(b.select_word_at((0, 5)));
        assert_eq!(b.selected_text().unwrap(), "foo_bar");
    }

    #[test]
    fn double_click_on_whitespace_selects_nothing() {
        let mut b = buf_of(&["a  b"]);
        assert!(!b.select_word_at((0, 1)));
        assert!(!b.has_selection());
    }

    #[test]
    fn select_all_covers_the_buffer() {
        let mut b = buf_of(&["one", "two"]);
        b.select_all();
        assert_eq!(b.selected_text().unwrap(), "one\ntwo");
    }

    #[test]
    fn span_len_agrees_with_the_text_it_describes() {
        // `replace` is driven by span_len; if it's off by one, deleting a
        // selection eats a neighbouring character.
        let b = buf_of(&["abc", "de", "fghi"]);
        let cases = [((0, 0), (0, 3)), ((0, 1), (1, 1)), ((0, 0), (2, 4))];
        for (from, to) in cases {
            let mut probe = b.clone_for_test();
            probe.selection_anchor = Some(from);
            probe.cursor_row = to.0;
            probe.cursor_col = to.1;
            let text = probe.selected_text().unwrap();
            assert_eq!(
                probe.span_len(from, to),
                text.chars().count(),
                "span {from:?}..{to:?} = {text:?}"
            );
        }
    }

    // --- save ------------------------------------------------------------

    fn scratch(name: &str, contents: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("sacrament-save-{name}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("file.txt");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn a_freshly_loaded_file_is_clean() {
        let path = scratch("fresh", "abc\n");
        let b = Buffer::load(&path, None).unwrap();
        assert!(!b.dirty, "loading is not an edit");
        assert!(!b.can_undo());
        assert!(!b.can_redo());
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_round_trips_content() {
        let path = scratch("roundtrip", "one\ntwo\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.cursor_row = 1;
        b.cursor_col = 3;
        b.insert_str("!");
        assert!(b.dirty);
        b.save().unwrap();
        assert!(!b.dirty, "save clears the dirty flag");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo!\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_a_missing_trailing_newline() {
        // A file with no final newline must not gain one, or every save shows up
        // as a spurious one-line diff.
        let path = scratch("no-newline", "abc");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xabc");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_preserves_an_existing_trailing_newline() {
        let path = scratch("newline", "abc\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xabc\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_refuses_to_clobber_a_file_changed_on_disk() {
        let path = scratch("clobber", "original\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        // Someone else writes the file after we loaded it.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&path, "theirs\n").unwrap();

        let err = b.save().expect_err("must refuse rather than discard their write");
        assert!(matches!(err, SaveError::ChangedOnDisk));
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "theirs\n",
            "their content survives"
        );
        assert!(b.dirty, "our edit is still unsaved");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn a_second_save_after_the_first_succeeds() {
        // The mtime guard must accept our own writes; only *foreign* changes
        // block. Regression test for the guard being too strict.
        let path = scratch("resave", "a\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        b.insert_char('y');
        b.save().expect("our own previous save must not block the next one");
        // Cursor advances after each insert, so the second char lands after the first.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "xya\n");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn save_leaves_no_temp_file_behind() {
        let path = scratch("tmp", "a\n");
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_char('x');
        b.save().unwrap();
        let strays: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("sacrament-tmp"))
            .collect();
        assert!(strays.is_empty(), "temp file should be renamed, not left");
        let _ = std::fs::remove_dir_all(path.parent().unwrap());
    }

    #[test]
    fn untitled_buffer_has_nowhere_to_save() {
        let mut b = Buffer::empty();
        b.insert_char('x');
        assert!(matches!(b.save(), Err(SaveError::NoPath)));
        assert!(b.dirty, "nothing was written, so it stays dirty");
    }

    #[test]
    fn syntax_highlighting_colors_a_rust_keyword() {
        let hl = Highlighter::new();
        let dir = std::env::temp_dir().join("sacrament-hl-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.rs");
        std::fs::write(&path, "fn main() {}\n").unwrap();
        let b = Buffer::load(&path, Some(&hl)).unwrap();
        let src = BufferSource {
            buffer: Arc::new(Mutex::new(b)),
            rows: 40,
            highlighter: Some(Arc::new(hl)),
            focused: true,
        };
        let palette = Palette::default();
        let mut out = Vec::new();
        src.fill(&palette, 1, 80, &mut out);
        let fgs: Vec<_> = out[0].iter().map(|c| c.fg).collect();
        assert!(
            fgs.iter().any(|c| *c != palette.foreground),
            "at least one cell should be syntax-colored"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod doc_move_tests {
    use super::*;

    fn buf() -> Buffer {
        let mut b = Buffer::empty();
        b.lines = vec!["one".into(), "two".into(), "three".into()];
        b
    }

    #[test]
    fn doc_start_and_end_land_on_the_extremes() {
        let mut b = buf();
        b.cursor_row = 1;
        b.cursor_col = 2;
        b.move_doc_end(false);
        assert_eq!((b.cursor_row, b.cursor_col), (2, 5), "end of the last line");
        b.move_doc_start(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 0));
    }

    #[test]
    fn doc_movement_can_extend_a_selection() {
        let mut b = buf();
        b.cursor_row = 1;
        b.cursor_col = 0;
        b.move_doc_end(true);
        assert_eq!(b.selection_anchor, Some((1, 0)), "anchor stays where it began");
        assert!(b.has_selection());
    }

    #[test]
    fn doc_end_on_an_empty_buffer_does_not_panic() {
        let mut b = Buffer::empty();
        b.move_doc_end(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 0));
    }
}

#[cfg(test)]
mod word_and_find_tests {
    use super::*;

    fn buf(lines: &[&str]) -> Buffer {
        let mut b = Buffer::empty();
        b.lines = lines.iter().map(|s| s.to_string()).collect();
        b.line_state_before = vec![None; b.lines.len()];
        b.highlights = vec![None; b.lines.len()];
        b
    }

    #[test]
    fn word_right_lands_on_word_ends() {
        let mut b = buf(&["foo bar  baz"]);
        for expected in [3, 7, 12] {
            b.move_word_right(false);
            assert_eq!(b.cursor_col, expected);
        }
        // At the end it stays put rather than wrapping or panicking.
        b.move_word_right(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 12));
    }

    #[test]
    fn word_left_lands_on_word_starts() {
        let mut b = buf(&["foo bar  baz"]);
        b.cursor_col = 12;
        for expected in [9, 4, 0] {
            b.move_word_left(false);
            assert_eq!(b.cursor_col, expected);
        }
        b.move_word_left(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 0));
    }

    #[test]
    fn word_movement_crosses_line_ends() {
        // A line break is a separator you move *through*, not a wall. Stopping at
        // every end of line is the bug this pins.
        let mut b = buf(&["one", "two"]);
        b.cursor_col = 3;
        b.move_word_right(false);
        assert_eq!((b.cursor_row, b.cursor_col), (1, 3), "end of 'two'");
        b.move_word_left(false);
        assert_eq!((b.cursor_row, b.cursor_col), (1, 0), "start of 'two'");
        b.move_word_left(false);
        assert_eq!((b.cursor_row, b.cursor_col), (0, 0), "start of 'one'");
    }

    #[test]
    fn word_movement_extends_a_selection() {
        let mut b = buf(&["foo bar"]);
        b.move_word_right(true);
        assert_eq!(b.selection_anchor, Some((0, 0)));
        assert!(b.has_selection());
    }

    #[test]
    fn find_is_case_insensitive_until_the_query_has_a_capital() {
        let b = buf(&["Foo foo FOO"]);
        // Lowercase query matches any casing, first hit from the top.
        assert_eq!(b.find("foo", (0, 0), true), Some(((0, 0), (0, 3))));
        // A capital makes it exact, so the lowercase run is skipped.
        assert_eq!(b.find("FOO", (0, 0), true), Some(((0, 8), (0, 11))));
        assert_eq!(b.find("Foo", (0, 1), true), Some(((0, 0), (0, 3))), "wraps");
    }

    #[test]
    fn find_wraps_in_both_directions() {
        let b = buf(&["alpha", "beta", "alpha"]);
        // Forward from past the last match wraps to the first.
        assert_eq!(b.find("alpha", (2, 1), true), Some(((0, 0), (0, 5))));
        // Backward from the top wraps to the last.
        assert_eq!(b.find("alpha", (0, 0), false), Some(((2, 0), (2, 5))));
    }

    #[test]
    fn find_advances_rather_than_sticking_on_the_current_match() {
        let b = buf(&["xx xx xx"]);
        let first = b.find("xx", (0, 0), true).unwrap();
        assert_eq!(first, ((0, 0), (0, 2)));
        // Callers search from one past the match start; that must move on.
        let second = b.find("xx", (0, first.0.1 + 1), true).unwrap();
        assert_eq!(second, ((0, 3), (0, 5)));
    }

    #[test]
    fn find_rejects_queries_it_cannot_honour() {
        let b = buf(&["anything"]);
        assert_eq!(b.find("", (0, 0), true), None);
        assert_eq!(b.find("multi\nline", (0, 0), true), None);
        assert_eq!(b.find("absent", (0, 0), true), None);
    }

    #[test]
    fn case_folding_does_not_shift_match_columns() {
        // `İ` lowercases to two chars under full Unicode rules, which would push
        // every later column off by one if search folded with `to_lowercase`.
        let b = buf(&["İx target"]);
        let (start, end) = b.find("target", (0, 0), true).expect("should match");
        assert_eq!(start, (0, 3));
        assert_eq!(end, (0, 9));
        let chars: Vec<char> = b.lines[0].chars().collect();
        assert_eq!(chars[3..9].iter().collect::<String>(), "target");
    }

    #[test]
    fn goto_line_is_one_based_and_clamps() {
        let mut b = buf(&["a", "b", "c"]);
        b.goto_line(2);
        assert_eq!(b.cursor_row, 1);
        b.goto_line(999);
        assert_eq!(b.cursor_row, 2, "clamps to the last line");
        b.goto_line(0);
        assert_eq!(b.cursor_row, 0, "line 0 is treated as line 1");
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("sacrament-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write with an mtime the buffer will actually see as different.
    ///
    /// The sleep isn't padding: mtime resolution can be coarse enough that a
    /// write in the same instant as the load looks unchanged, which would make
    /// these tests pass or fail on timing rather than on behaviour.
    fn write_distinct(path: &std::path::Path, body: &str) {
        std::thread::sleep(std::time::Duration::from_millis(15));
        std::fs::write(path, body).unwrap();
    }

    fn code() -> Buffer {
        buf(&[
            "fn one() {",      // 0  heads a block ending at 2
            "    let a = 1;",  // 1
            "    let b = 2;",  // 2
            "",                // 3  blank between blocks
            "fn two() {",      // 4  heads a block ending at 5
            "    body();",     // 5
            "}",               // 6  wait: closing brace is at indent 0
        ])
    }

    #[test]
    fn a_block_is_foldable_when_the_next_line_is_indented_further() {
        let b = code();
        assert_eq!(b.fold_end(0), Some(2), "body runs to the last indented line");
        assert_eq!(b.fold_end(4), Some(5));
        assert_eq!(b.fold_end(1), None, "a body line heads nothing");
        assert_eq!(b.fold_end(6), None, "closing brace heads nothing");
    }

    #[test]
    fn a_blank_line_does_not_end_a_block_but_is_not_swallowed_either() {
        let b = buf(&["head", "  a", "", "  b", "", "after"]);
        // The gap between two indented lines stays inside...
        assert_eq!(b.fold_end(0), Some(3));
        // ...but the trailing blank isn't dragged in, so folding doesn't eat the
        // space before whatever comes next.
        assert!(b.lines[4].is_empty());
    }

    #[test]
    fn an_aligned_continuation_is_not_a_block() {
        // The complaint this rule exists for. v1's any-increase rule puts a
        // chevron on the `let`, because its continuation is indented further.
        let b = buf(&[
            "fn f() {",
            "    let x = compute(a,",
            "                    b);",
            "    done();",
            "}",
        ]);
        assert_eq!(b.fold_end(1), None, "20 columns in is alignment, not a block");
        assert_eq!(b.fold_mark(1), None, "so no chevron either");
        assert_eq!(b.fold_end(0), Some(3), "the function itself still folds");
    }

    #[test]
    fn a_newly_typed_function_gets_a_chevron() {
        // The reported bug. A 4-space file edited with `indent_with_tabs` and
        // `tab_width = 2` writes 2-column indents beside 4-column ones; demanding
        // the measured unit exactly meant new functions never got a chevron.
        let mut b = buf(&["fn old() {", "    body();", "}"]);
        b.tab_width = 2;

        // Type a new tab-indented function at the end.
        b.cursor_row = 2;
        b.cursor_col = b.line_len(2);
        b.insert_newline();
        b.insert_str("fn new() {");
        b.insert_newline();
        b.insert_str("\tbody();");

        let head = b.lines.len() - 2;
        assert_eq!(b.lines[head], "fn new() {");
        assert_eq!(b.fold_mark(head), Some(FoldMark::Open), "new function folds");
        assert_eq!(b.fold_mark(0), Some(FoldMark::Open), "and the old one still does");
    }

    #[test]
    fn a_block_typed_into_an_empty_buffer_folds() {
        // Nothing to measure from, so `tab_width` has to carry it.
        let mut b = Buffer::empty();
        b.tab_width = 2;
        b.insert_str("fn f() {");
        b.insert_newline();
        b.insert_str("\tbody();");
        assert_eq!(b.fold_mark(0), Some(FoldMark::Open));
    }

    #[test]
    fn mixed_indent_styles_all_fold() {
        // The regression that killed the measured level: as tab-indented lines
        // came to outnumber the 4-space ones, the level shrank and the original
        // functions stopped folding. Every block here folds regardless of which
        // style is in the majority.
        let b = buf(&[
            "fn spaces() {", "    body();", "}",
            "fn tabs() {", "\tbody();", "}",
            "fn more_tabs() {", "\tbody();", "}",
            "fn yet_more() {", "\tbody();", "}",
        ]);
        for head in [0, 3, 6, 9] {
            assert_eq!(
                b.fold_mark(head),
                Some(FoldMark::Open),
                "line {head} should fold whatever the majority style is"
            );
        }
    }

    #[test]
    fn folding_does_not_depend_on_tab_width_matching_the_file() {
        // Config and file disagree constantly; neither combination may gate it.
        let mut b = buf(&["fn f() {", "    body();", "}"]);
        b.tab_width = 2;
        assert_eq!(b.fold_end(0), Some(1), "4-space file, tab_width 2");

        let mut b = buf(&["fn f() {", "  body();", "}"]);
        b.tab_width = 4;
        assert_eq!(b.fold_end(0), Some(1), "2-space file, tab_width 4");
    }

    #[test]
    fn only_the_first_body_line_has_to_land_on_a_level() {
        // Once a block is established its contents can be aligned however they
        // like — an argument list inside a function is still inside it.
        let b = buf(&["fn f() {", "    call(a,", "         b);", "}"]);
        assert_eq!(b.fold_end(0), Some(2), "block runs past the aligned line");
    }

    #[test]
    fn folding_hides_the_body_and_keeps_the_header() {
        let mut b = code();
        let before = b.visible_rows(20).len();
        assert!(b.toggle_fold(0));
        let rows = b.visible_rows(20);
        assert_eq!(rows.len(), before - 2, "two body lines went away");
        assert_eq!(rows[0].line, 0, "header stays");
        assert_eq!(rows[1].line, 3, "next visible row skips the body");
    }

    #[test]
    fn folding_leaves_the_text_untouched() {
        // Folds are metadata about visibility, never about content.
        let mut b = code();
        let text = b.to_text();
        b.fold_all();
        assert_eq!(b.to_text(), text);
        b.unfold_all();
        assert_eq!(b.to_text(), text);
    }

    #[test]
    fn arrowing_over_a_fold_steps_across_it_in_one_press() {
        let mut b = code();
        b.toggle_fold(0);
        b.cursor_row = 0;
        b.move_down(false);
        assert_eq!(b.cursor_row, 3, "one press clears the whole folded body");
        b.move_up(false);
        assert_eq!(b.cursor_row, 0);
    }

    #[test]
    fn the_caret_cannot_be_left_inside_a_folded_block() {
        let mut b = code();
        b.cursor_row = 2;
        b.cursor_col = 3;
        b.toggle_fold(0);
        assert_eq!(b.cursor_row, 0, "pulled up to the header");
        assert!(b.cursor_col <= b.line_len(0));
    }

    #[test]
    fn the_gutter_marks_headers_only_and_never_a_wrap_continuation() {
        let mut b = code();
        b.wrap_width = 0;
        assert_eq!(b.fold_mark(0), Some(FoldMark::Open));
        b.toggle_fold(0);
        assert_eq!(b.fold_mark(0), Some(FoldMark::Closed));
        assert_eq!(b.fold_mark(1), None);
        // Every gutter row lines up with a visible row, folded or not.
        assert_eq!(b.gutter_rows(20).len(), b.visible_rows(20).len());
    }

    #[test]
    fn an_edit_inside_a_folded_block_drops_the_fold() {
        let mut b = code();
        b.toggle_fold(0);
        assert_eq!(b.fold_ranges().len(), 1);
        // Deleting across the body must not leave a fold describing lines that
        // moved or vanished.
        b.cursor_row = 0;
        b.cursor_col = 0;
        b.replace_range_for_test((0, 0), (2, 0));
        assert!(b.fold_ranges().is_empty(), "fold dropped rather than mistracked");
    }

    #[test]
    fn an_edit_above_a_fold_slides_it() {
        let mut b = code();
        b.toggle_fold(4);
        let before = b.fold_ranges()[0];
        // Insert a line at the very top; the fold should move down by one.
        b.cursor_row = 0;
        b.cursor_col = 0;
        b.insert_newline();
        let after = b.fold_ranges()[0];
        assert_eq!(after.0, before.0 + 1);
        assert_eq!(after.1, before.1 + 1);
    }

    #[test]
    fn fold_all_takes_the_outermost_blocks_only() {
        let mut b = buf(&["a", "  b", "    c", "d", "  e"]);
        b.fold_all();
        let ranges = b.fold_ranges();
        assert_eq!(ranges, vec![(0, 2), (3, 4)], "no fold nested inside another");
        assert_eq!(b.visible_rows(20).len(), 2, "only the two headers show");
    }

    #[test]
    fn folding_from_inside_a_body_finds_the_enclosing_header() {
        let b = code();
        assert_eq!(b.enclosing_fold_head(1), 0);
        assert_eq!(b.enclosing_fold_head(2), 0);
        assert_eq!(b.enclosing_fold_head(0), 0, "a header is its own");
    }

    #[test]
    fn restored_folds_are_dropped_when_they_no_longer_fit() {
        let mut b = buf(&["a", "  b"]);
        b.set_fold_ranges(&[(0, 1), (5, 9)]);
        assert_eq!(b.fold_ranges(), vec![(0, 1)], "out-of-range fold discarded");
    }

    #[test]
    fn indent_and_outdent_are_inverses_over_a_selection() {
        let mut b = buf(&["one", "two", "three"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 2;
        b.cursor_col = 5;
        b.indent_selection(2, false);
        assert_eq!(b.lines, vec!["  one", "  two", "  three"]);
        b.outdent_selection(2);
        assert_eq!(b.lines, vec!["one", "two", "three"]);
    }

    #[test]
    fn indenting_a_block_is_a_single_undo_step() {
        // One `edit` for the whole range, not one per line — ten commented lines
        // must come back with one Cmd+Z.
        let mut b = buf(&["a", "b", "c", "d"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 3;
        b.indent_selection(4, false);
        assert_eq!(b.lines[0], "    a");
        b.undo();
        assert_eq!(b.lines, vec!["a", "b", "c", "d"], "one undo restores all four");
    }

    #[test]
    fn outdent_takes_what_is_there_and_no_more() {
        let mut b = buf(&["      six", "  two", "none", "\ttabbed"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 3;
        b.cursor_col = 1;
        b.outdent_selection(4);
        assert_eq!(b.lines[0], "  six", "four of six spaces");
        assert_eq!(b.lines[1], "two", "only the two that existed");
        assert_eq!(b.lines[2], "none", "nothing to remove");
        assert_eq!(b.lines[3], "tabbed", "a tab is one level");
    }

    #[test]
    fn outdenting_nothing_records_no_undo_step() {
        let mut b = buf(&["flush", "left"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 1;
        let before = b.lines.clone();
        b.outdent_selection(4);
        b.undo();
        assert_eq!(b.lines, before, "an empty outdent must not eat a later undo");
    }

    #[test]
    fn a_selection_ending_at_column_zero_excludes_that_line() {
        // Dragging down to the start of a line reads as "not this one".
        let mut b = buf(&["one", "two", "three"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 2;
        b.cursor_col = 0;
        b.indent_selection(2, false);
        assert_eq!(b.lines[1], "  two");
        assert_eq!(b.lines[2], "three", "untouched");
    }

    #[test]
    fn commenting_aligns_on_the_shallowest_indent() {
        let mut b = buf(&["    deep", "  shallow", "      deeper"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 2;
        b.cursor_col = 5;
        assert!(b.toggle_comment("//"));
        // Every marker lands at the shallowest indent, so the block keeps shape.
        assert_eq!(b.lines[0], "  //   deep");
        assert_eq!(b.lines[1], "  // shallow");
        assert_eq!(b.lines[2], "  //     deeper");
    }

    #[test]
    fn toggling_twice_restores_the_original_text() {
        let mut b = buf(&["  fn main() {", "      body();", "  }"]);
        let before = b.lines.clone();
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 2;
        b.cursor_col = 3;
        assert!(b.toggle_comment("//"));
        assert!(b.lines.iter().all(|l| l.trim_start().starts_with("//")));
        assert!(b.toggle_comment("//"), "all commented -> uncomment");
        assert_eq!(b.lines, before);
    }

    #[test]
    fn a_partly_commented_block_comments_the_rest() {
        let mut b = buf(&["// done", "not yet"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 1;
        b.cursor_col = 7;
        b.toggle_comment("//");
        assert_eq!(b.lines, vec!["// // done", "// not yet"]);
    }

    #[test]
    fn blank_lines_inside_a_block_are_left_alone() {
        let mut b = buf(&["code", "", "more"]);
        b.selection_anchor = Some((0, 0));
        b.cursor_row = 2;
        b.cursor_col = 4;
        b.toggle_comment("#");
        assert_eq!(b.lines, vec!["# code", "", "# more"]);
    }

    #[test]
    fn commenting_with_no_selection_acts_on_the_caret_line() {
        let mut b = buf(&["one", "two"]);
        b.cursor_row = 1;
        b.toggle_comment("//");
        assert_eq!(b.lines, vec!["one", "// two"]);
    }

    #[test]
    fn the_caret_follows_the_text_it_was_sitting_in() {
        let mut b = buf(&["value"]);
        b.cursor_col = 2; // between 'a' and 'l'
        b.toggle_comment("//");
        assert_eq!(b.lines[0], "// value");
        assert_eq!(b.cursor_col, 5, "still before the same character");
        b.toggle_comment("//");
        assert_eq!(b.cursor_col, 2);
    }

    #[test]
    fn a_clean_buffer_picks_up_a_change_on_disk() {
        let dir = scratch("reload");
        let path = dir.join("f.txt");
        std::fs::write(&path, "one\ntwo\nthree\n").unwrap();
        let mut b = Buffer::load(&path, None).unwrap();
        b.cursor_row = 2;
        b.cursor_col = 3;

        write_distinct(&path, "ONE\nTWO\nTHREE\n");
        assert!(b.reload(None).unwrap(), "should have reloaded");
        assert_eq!(b.lines[0], "ONE");
        assert_eq!((b.cursor_row, b.cursor_col), (2, 3), "caret is kept");
        assert!(!b.dirty);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reloading_clamps_a_caret_past_a_shrunken_file() {
        let dir = scratch("reload-shrink");
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let mut b = Buffer::load(&path, None).unwrap();
        b.cursor_row = 3;
        b.cursor_col = 1;

        write_distinct(&path, "a\n");
        assert!(b.reload(None).unwrap());
        assert_eq!(b.cursor_row, 0, "clamped to the last line");
        assert!(b.cursor_col <= b.line_len(0));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_dirty_buffer_is_never_silently_reloaded() {
        // v1 reloads regardless and loses the edits. This is the guard.
        let dir = scratch("reload-dirty");
        let path = dir.join("f.txt");
        std::fs::write(&path, "disk\n").unwrap();
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_str("mine");
        assert!(b.dirty);

        write_distinct(&path, "changed by someone else\n");
        assert!(matches!(b.reload(None), Err(ReloadError::Dirty)));
        assert!(b.lines[0].starts_with("mine"), "edits survive");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn our_own_save_does_not_count_as_an_external_change() {
        // The watcher fires for our own writes too, so this is the common path,
        // and it's what makes debouncing unnecessary.
        let dir = scratch("reload-self");
        let path = dir.join("f.txt");
        std::fs::write(&path, "before\n").unwrap();
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_str("after");
        b.save().unwrap();
        assert!(!b.reload(None).unwrap(), "own save is not a reload");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn overwriting_gets_past_the_changed_on_disk_guard() {
        let dir = scratch("overwrite");
        let path = dir.join("f.txt");
        std::fs::write(&path, "theirs\n").unwrap();
        let mut b = Buffer::load(&path, None).unwrap();
        b.insert_str("mine");

        write_distinct(&path, "theirs, edited\n");
        assert!(matches!(b.save(), Err(SaveError::ChangedOnDisk)));
        b.save_overwriting().expect("second save wins");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("mine"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_as_writes_to_the_new_path_and_adopts_it() {
        let dir = std::env::temp_dir().join(format!("sacrament-saveas-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("out.txt");
        let mut b = buf(&["hello", "world"]);
        b.save_as(target.clone(), None).expect("save_as should write");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "hello\nworld\n");
        assert_eq!(b.path(), Some(target.as_path()));
        assert!(!b.dirty, "a saved buffer is clean");
        // And a plain save now goes to the new path rather than erroring.
        b.lines[0] = "changed".into();
        b.save().expect("subsequent save should work");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "changed\nworld\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_as_onto_an_existing_file_is_not_blocked_by_the_mtime_guard() {
        // `save` refuses to write when the file changed underneath it. Save-as
        // targets a file it has never read, so that guard would reject every
        // save-as onto an existing path unless the mtime is adopted first.
        let dir = std::env::temp_dir().join(format!("sacrament-saveas2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("existing.txt");
        std::fs::write(&target, "old contents\n").unwrap();
        let mut b = buf(&["new"]);
        b.save_as(target.clone(), None).expect("should overwrite");
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn select_range_leaves_the_cursor_at_the_end() {
        let mut b = buf(&["hello world"]);
        b.select_range((0, 6), (0, 11));
        assert_eq!(b.selection_anchor, Some((0, 6)));
        assert_eq!((b.cursor_row, b.cursor_col), (0, 11));
        assert_eq!(b.selected_text().as_deref(), Some("world"));
    }
}

