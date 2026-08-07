//! Wrapper around `alacritty_terminal`'s `Term` — the VT state machine that
//! replaces v1's `vt100::Parser`.
//!
//! `Term` is the source of truth for what's on screen, same contract as v1's
//! parser. The differences that matter: it tracks damage (so a renderer can
//! repaint only changed lines), it handles far more of the VT spec, and its
//! `renderable_content()` already accounts for the scrollback display offset.

use alacritty_terminal::event::{Event as TermEvent, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::Processor;

pub const SCROLLBACK: usize = 2_000;

/// `Term::new` wants something that reports its own dimensions.
#[derive(Clone, Copy)]
pub struct Size {
    pub rows: usize,
    pub cols: usize,
}

impl Dimensions for Size {
    // `screen_lines()`, not `screen_lines + history` — this mirrors
    // `alacritty_terminal`'s own `TermSize`. Scrollback depth is configured on
    // `Term` via `Config::scrolling_history`, and reporting it here as well
    // double-counts.
    fn total_lines(&self) -> usize {
        self.screen_lines()
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// We don't consume `Term`'s outbound events in the spike. Real use will need
/// this for `PtyWrite` (device status reports, bracketed-paste replies) and
/// `Title`/`ClipboardStore`.
#[derive(Clone)]
pub struct Listener;

impl EventListener for Listener {
    fn send_event(&self, _event: TermEvent) {}
}

pub struct Terminal {
    pub term: Term<Listener>,
    parser: Processor,
    size: Size,
}

/// Schemes that may be handed to the system opener.
///
/// An allowlist, not a blocklist, and the difference matters: macOS `open` will
/// launch whatever application has registered a scheme, so `open`ing an arbitrary
/// one from terminal output is a way to launch arbitrary software from text a
/// remote server produced. Web pages are what a link in a terminal means.
const URL_SCHEMES: [&str; 2] = ["https://", "http://"];

/// Trailing characters to peel off a candidate.
///
/// A URL at the end of a sentence, or inside brackets or quotes, is the normal
/// case in program output — `see https://example.com/x.` should not include the
/// full stop. Peeled from the end only: every one of these is legal *within* a
/// URL, and several (`?`, `=`, `#`) are common in query strings.
const URL_TRAILING: [char; 12] = ['.', ',', ';', ':', '!', '?', ')', ']', '}', '>', '"', '\''];

/// Find a URL in `text` spanning character index `hit`.
///
/// A free function so it can be tested on plain strings, with no terminal to
/// drive — which is the only practical way to cover the punctuation and boundary
/// cases.
fn url_around(text: &str, hit: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    if hit >= chars.len() {
        return None;
    }
    // A URL can't contain whitespace, so the whitespace-delimited run containing
    // the click is the widest thing the URL could be.
    let start = chars[..hit]
        .iter()
        .rposition(|c| c.is_whitespace())
        .map_or(0, |i| i + 1);
    let end = chars[hit..]
        .iter()
        .position(|c| c.is_whitespace())
        .map_or(chars.len(), |i| hit + i);

    let token: String = chars[start..end].iter().collect();
    // Leading punctuation too — `(https://example.com)` is as common as trailing.
    let token = token.trim_start_matches(['(', '[', '{', '<', '"', '\'']);
    let token = token.trim_end_matches(URL_TRAILING);

    let scheme = URL_SCHEMES.iter().find(|s| token.starts_with(**s))?;
    // Reject a bare scheme with nothing after it.
    (token.len() > scheme.len()).then(|| token.to_string())
}

impl Terminal {
    pub fn new(rows: usize, cols: usize) -> Self {
        let size = Size {
            rows: rows.max(1),
            cols: cols.max(1),
        };
        let config = Config {
            scrolling_history: SCROLLBACK,
            ..Config::default()
        };
        Self {
            term: Term::new(config, &size, Listener),
            parser: Processor::new(),
            size,
        }
    }

    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Returns true if the size actually changed, so the caller knows whether
    /// to push a matching resize down to the PTY.
    pub fn resize(&mut self, rows: usize, cols: usize) -> bool {
        let rows = rows.max(1);
        let cols = cols.max(1);
        if rows == self.size.rows && cols == self.size.cols {
            return false;
        }
        self.size = Size { rows, cols };
        self.term.resize(self.size);
        true
    }

    /// Begin a selection at a viewport cell. `semantic` selects by word (what a
    /// double click wants), otherwise it's a plain character range.
    ///
    /// alacritty's own `Selection` is used rather than a hand-rolled one: it works
    /// in *grid* coordinates, so a selection survives scrolling, and it already
    /// knows how to grow to word and line boundaries.
    pub fn begin_selection(&mut self, row: usize, col: usize, semantic: bool) {
        use alacritty_terminal::selection::{Selection, SelectionType};
        let ty = if semantic {
            SelectionType::Semantic
        } else {
            SelectionType::Simple
        };
        let point = self.grid_point(row, col);
        self.term.selection = Some(Selection::new(ty, point, Side::Left));
    }

    /// Select the whole line under a viewport cell (triple click).
    pub fn begin_line_selection(&mut self, row: usize, col: usize) {
        use alacritty_terminal::selection::{Selection, SelectionType};
        let point = self.grid_point(row, col);
        self.term.selection = Some(Selection::new(SelectionType::Lines, point, Side::Left));
    }

    pub fn update_selection(&mut self, row: usize, col: usize) {
        let point = self.grid_point(row, col);
        if let Some(sel) = self.term.selection.as_mut() {
            sel.update(point, Side::Left);
        }
    }

    pub fn clear_selection(&mut self) {
        self.term.selection = None;
    }


    /// The selected text. alacritty handles the grid walk, including wrapped
    /// lines and the scrollback region.
    pub fn selected_text(&self) -> Option<String> {
        self.term.selection_to_string().filter(|s| !s.is_empty())
    }

    /// The URL under a viewport cell, if there is one.
    ///
    /// **Reads the whole logical line, not the screen row.** A URL long enough to
    /// be worth clicking is exactly the kind that wraps, and a wrapped line is
    /// several grid rows — so scanning one row would find `https://example.com/a`
    /// and miss the `/b/c` that continued on the next. `WRAPLINE` on a row's last
    /// cell is what joins them, so the walk follows it in both directions and
    /// tokenizes the joined text.
    ///
    /// Only `http` and `https` are returned, and that's a safety boundary rather
    /// than a limitation: terminal output is frequently attacker-influenced — any
    /// file you `cat`, any repo you clone, any server you curl — and macOS `open`
    /// will launch a registered handler for *any* scheme it's given. Restricting
    /// the schemes means the worst a hostile line can do is open a web page.
    pub fn url_at(&self, row: usize, col: usize) -> Option<String> {
        // `grid_point` clamps the row to the last row with content, which is right
        // for a *drag* — pulling into the blank region below a short prompt should
        // stop at the content rather than select a screenful of nothing. It is
        // wrong here: clicking empty space well below the output would resolve to
        // whatever the last line happened to hold and open its link.
        if row > self.last_content_row() {
            return None;
        }
        let point = self.grid_point(row, col);
        let grid = self.term.grid();
        let cols = self.size.cols.max(1);
        let topmost = grid.topmost_line().0;
        let bottommost = grid.bottommost_line().0;

        // Walk back to the first row of the logical line: a row is a continuation
        // when the row *above* it is flagged as wrapping into it.
        let mut first = point.line.0;
        while first > topmost && Self::wraps(grid, Line(first - 1), cols) {
            first -= 1;
        }

        // Collect the joined text, noting where the clicked cell lands in it.
        let mut text = String::new();
        let mut hit: Option<usize> = None;
        let mut line = first;
        loop {
            for column in 0..cols {
                if line == point.line.0 && column == point.column.0 {
                    hit = Some(text.chars().count());
                }
                text.push(grid[Line(line)][Column(column)].c);
            }
            if line >= bottommost || !Self::wraps(grid, Line(line), cols) {
                break;
            }
            line += 1;
        }

        url_around(&text, hit?)
    }

    /// Does this row continue onto the next one?
    fn wraps(
        grid: &alacritty_terminal::grid::Grid<alacritty_terminal::term::cell::Cell>,
        line: Line,
        cols: usize,
    ) -> bool {
        use alacritty_terminal::term::cell::Flags;
        grid[line][Column(cols - 1)]
            .flags
            .contains(Flags::WRAPLINE)
    }

    /// Viewport row/col to a grid `Point`. The viewport's top row is grid line
    /// `-display_offset`, so scrolling shifts the mapping — which is exactly why
    /// selections are stored in grid space and not viewport space.
    ///
    /// The row is clamped to the last row holding anything, so dragging into the
    /// empty region below a short prompt stops at the content instead of
    /// selecting a screenful of blanks. Same behavior as the editor, where
    /// `visible_rows` simply stops at the last line.
    fn grid_point(&self, row: usize, col: usize) -> Point {
        let offset = self.term.grid().display_offset() as i32;
        let row = row.min(self.last_content_row());
        Point::new(
            Line(row as i32 - offset),
            Column(col.min(self.size.cols.saturating_sub(1))),
        )
    }

    /// Last viewport row containing anything, cursor included.
    ///
    /// A fresh shell has one line of prompt and a screenful of blanks under it;
    /// without this, a drag downward selects all of that emptiness.
    fn last_content_row(&self) -> usize {
        let content = self.term.renderable_content();
        // Same grid-line-to-viewport-row conversion as `TerminalSource::fill`; both
        // have to agree or a selection clamps to a different row than it paints.
        let offset = content.display_offset as i32;
        let mut last = 0usize;
        let cursor_line = content.cursor.point.line.0 + offset;
        if cursor_line >= 0 {
            last = cursor_line as usize;
        }
        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset;
            if line >= 0 && indexed.cell.c != ' ' && indexed.cell.c != '\0' {
                last = last.max(line as usize);
            }
        }
        last.min(self.size.rows.saturating_sub(1))
    }

    /// Scroll the viewport through scrollback. Positive moves toward older
    /// content. `Term` clamps internally, and `renderable_content()` already
    /// applies the resulting display offset, so nothing else has to know.
    pub fn scroll(&mut self, lines: i32) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Delta(lines));
    }

    /// Jump back to live output. Called on keystrokes so typing while scrolled up
    /// snaps to the prompt, which is what every terminal does.
    pub fn scroll_to_bottom(&mut self) {
        use alacritty_terminal::grid::Scroll;
        self.term.scroll_display(Scroll::Bottom);
    }

    /// Viewport-relative cursor position, or None when hidden or when the user
    /// has scrolled up into the scrollback.
    pub fn cursor(&self) -> Option<(usize, usize)> {
        let content = self.term.renderable_content();
        if content.display_offset != 0 {
            return None;
        }
        let Line(line) = content.cursor.point.line;
        let Column(col) = content.cursor.point.column;
        if line < 0 {
            return None;
        }
        Some((line as usize, col))
    }
}

/// The terminal wired up as something the grid can draw.
///
/// The counterpart to `buffer::BufferSource` — same trait, same widget, entirely
/// different content. That symmetry is the point of the abstraction.
pub struct TerminalSource {
    pub terminal: std::sync::Arc<std::sync::Mutex<Terminal>>,
    /// Whether to draw the caret — only the focused pane does, so it's
    /// unambiguous which surface is receiving keystrokes.
    pub show_cursor: bool,
}

impl crate::grid::GridSource for TerminalSource {
    fn fill(
        &self,
        palette: &crate::palette::Palette,
        rows: usize,
        cols: usize,
        out: &mut Vec<Vec<crate::grid::Cell>>,
    ) {
        use alacritty_terminal::term::cell::Flags;
        use crate::grid::{Cell, reset_rows};

        reset_rows(out, rows);
        let Ok(terminal) = self.terminal.lock() else {
            return;
        };
        let content = terminal.term.renderable_content();
        let selection = content.selection;
        // Grid line -> viewport row. When scrolled up, `display_iter` yields
        // *negative* lines for the scrollback region now on screen, so the offset
        // has to be added back. Treating negatives as out-of-range instead drops
        // those rows and shifts everything up, which reads as rows being eaten from
        // the bottom.
        let offset = content.display_offset as i32;
        for indexed in content.display_iter {
            let line = indexed.point.line.0 + offset;
            if line < 0 {
                continue;
            }
            let (row, col) = (line as usize, indexed.point.column.0);
            if row >= rows || col >= cols {
                continue;
            }
            let flags = indexed.cell.flags;
            // INVERSE swaps fg/bg at the source, so downstream never has to
            // know about it.
            let (fg_src, bg_src) = if flags.contains(Flags::INVERSE) {
                (indexed.cell.bg, indexed.cell.fg)
            } else {
                (indexed.cell.fg, indexed.cell.bg)
            };
            let mut cell = Cell {
                c: if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    '\0'
                } else {
                    indexed.cell.c
                },
                fg: palette.resolve(fg_src, false),
                bg: palette.resolve(bg_src, true),
                // SGR 1 is dropped rather than drawn. The app uses no bold
                // faces anywhere, and a shell is where that rule is most
                // visible — prompts and TUI headers set it constantly, and a
                // second weight reads as a different font mid-line.
                //
                // Only the *weight* goes: the colour the program asked for is
                // still its own. Terminals that brighten bold text do so by
                // remapping colours 0-7 to 8-15, which is a separate setting and
                // isn't something this ever did.
                bold: false,
                italic: flags.contains(Flags::ITALIC),
                underline: flags.contains(Flags::UNDERLINE),
            };
            // SGR 2 / 8 resolve to theme colors rather than alpha blends: dim
            // text takes the theme's muted slot, hidden text takes the
            // background so it genuinely disappears against it.
            if flags.contains(Flags::DIM) {
                cell.fg = palette.dim();
            }
            if flags.contains(Flags::HIDDEN) {
                cell.fg = cell.bg;
            }
            // Selection overrides cell colors, same rule as the editor: a
            // highlighted region has to stay legible over whatever's under it.
            if let Some(range) = selection
                && range.contains(indexed.point)
            {
                cell.fg = palette.selection_foreground;
                cell.bg = palette.selection_background;
            }
            // display_iter walks in order, but pad defensively so a gap can't
            // shift the rest of the row left.
            let dest = &mut out[row];
            while dest.len() < col {
                dest.push(Cell::blank(palette.foreground, palette.background));
            }
            if dest.len() == col {
                dest.push(cell);
            } else {
                dest[col] = cell;
            }
        }
    }

    fn cursor(&self) -> Option<(usize, usize)> {
        if !self.show_cursor {
            return None;
        }
        self.terminal.lock().ok()?.cursor()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    /// Everything the grid currently shows, one string per viewport row with
    /// trailing blanks trimmed.
    fn rows_of(t: &Terminal) -> Vec<String> {
        let size = t.size;
        let mut rows = vec![String::new(); size.rows];
        for indexed in t.term.renderable_content().display_iter {
            let line = indexed.point.line.0;
            if line < 0 || line as usize >= size.rows {
                continue;
            }
            let row = &mut rows[line as usize];
            let col = indexed.point.column.0;
            while row.chars().count() < col {
                row.push(' ');
            }
            row.push(indexed.cell.c);
        }
        rows.into_iter().map(|r| r.trim_end().to_string()).collect()
    }

    fn nonblank(t: &Terminal) -> Vec<String> {
        rows_of(t).into_iter().filter(|r| !r.is_empty()).collect()
    }

    /// Scrolling up must *reveal* scrollback at the top, not drop rows off the
    /// bottom.
    ///
    /// When the viewport is scrolled, alacritty's `display_iter` yields **negative**
    /// line numbers for the scrollback region now on screen. Treating those as
    /// out-of-range silently shifts everything up and blanks the bottom rows, which
    /// looks like rows being eaten from the bottom.
    #[test]
    fn scrolling_up_reveals_scrollback_at_the_top() {
        let t = Arc::new(Mutex::new(Terminal::new(5, 20)));
        for i in 1..=12 {
            t.lock().unwrap().feed(format!("line{i}\r\n").as_bytes());
        }

        let at_bottom = painted(&t, 5, 20);
        assert!(
            at_bottom.contains("line12"),
            "unscrolled view should show the newest line, got {at_bottom:?}"
        );

        t.lock().unwrap().scroll(3);
        let scrolled = painted(&t, 5, 20);

        // Scrolling back three rows moves the top of the view three lines earlier.
        // Comparing the *top* line is exact; comparing filled-row counts isn't,
        // because the unscrolled view ends on the cursor's empty row.
        let top_before = at_bottom.lines().next().unwrap().to_string();
        let top_after = scrolled.lines().next().unwrap().to_string();
        assert_eq!(top_before, "line9");
        assert_eq!(
            top_after, "line6",
            "top should move back by the scroll amount\nbefore: {at_bottom:?}\nafter: {scrolled:?}"
        );
        // The bug blanked rows off the bottom, so the filled count must not shrink.
        assert!(
            scrolled.lines().count() >= at_bottom.lines().count(),
            "scrolling must not blank rows\nbefore: {at_bottom:?}\nafter: {scrolled:?}"
        );
        // And scrolling back really did move past the newest line.
        assert!(!scrolled.contains("line12"), "got {scrolled:?}");
    }

    #[test]
    fn content_survives_shrink_then_grow() {
        let mut t = Terminal::new(10, 40);
        t.feed(b"boddy@Mac sacrament % asdfasdfasdfasdf\r\n");
        t.feed(b"zsh: command not found: asdfasdfasdfasdf\r\n");
        t.feed(b"boddy@Mac sacrament % ");
        let before = nonblank(&t);

        // The reported repro: shrink very small, then grow back.
        t.resize(10, 8);
        t.resize(10, 40);

        let after = nonblank(&t);
        assert_eq!(
            after, before,
            "\nbefore: {before:#?}\nafter:  {after:#?}\nreflow corrupted the grid"
        );
    }

    /// What `GridView` would actually draw, via the real source, flattened.
    fn painted(t: &Arc<Mutex<Terminal>>, rows: usize, cols: usize) -> String {
        use crate::grid::GridSource;
        let src = TerminalSource {
            terminal: t.clone(),
            show_cursor: false,
        };
        let mut out = Vec::new();
        src.fill(&crate::palette::Palette::default(), rows, cols, &mut out);
        out.iter()
            .map(|row| {
                row.iter()
                    .map(|c| if c.c == '\0' { ' ' } else { c.c })
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .filter(|r| !r.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// A resize must never make the render **gain** a character.
    ///
    /// Not "must show everything": narrowing wraps lines, and rows that no longer
    /// fit move into scrollback above the viewport, so seeing *less* mid-drag is
    /// correct — the shell redraws once the debounced `SIGWINCH` lands. What is
    /// never correct is a character appearing that was never written, which is the
    /// reported "extra character at the front" turned into an assertion instead of
    /// something you have to catch by eye mid-drag.
    ///
    /// Renders at a layout width that differs from the terminal's own, because
    /// during a drag those two genuinely disagree for a frame.
    #[test]
    fn a_resize_never_invents_characters() {
        const PROMPT: &str = "boddy@Mac sacrament % ";
        let expected: String = PROMPT.split_whitespace().collect::<Vec<_>>().join("");

        for cols in 2..=90usize {
            let t = Arc::new(Mutex::new(Terminal::new(20, 90)));
            t.lock().unwrap().feed(PROMPT.as_bytes());
            t.lock().unwrap().resize(20, cols);

            for render_cols in [cols, cols.saturating_add(20).min(90), 90, 1] {
                let got = painted(&t, 20, render_cols.max(1));
                let normalized: String = got.split_whitespace().collect::<Vec<_>>().join("");
                assert!(
                    expected.contains(&normalized),
                    "cols={cols} render_cols={render_cols} invented text\n                     rendered: {normalized:?}\nnot found in: {expected:?}\nraw: {got:?}"
                );
            }
        }
    }

    /// Rendered rows must stay *homogeneous*: a row made of `A`s can wrap into
    /// several rows of `A`s, but must never contain a `B`.
    ///
    /// The reported symptom is "pieces from the line below show up for one frame"
    /// at narrow widths. The previous test used a single line of content, so
    /// cross-row leakage was impossible for it to detect — this one uses lines that
    /// can't be confused for each other.
    #[test]
    fn no_row_mixes_content_from_two_source_lines() {
        let t = Arc::new(Mutex::new(Terminal::new(20, 90)));
        t.lock().unwrap().feed(b"AAAAAAAAAAAAAAAAAAAAAAAA\r\n");
        t.lock().unwrap().feed(b"BBBBBBBBBBBBBBBBBBBBBBBB\r\n");
        t.lock().unwrap().feed(b"CCCCCCCCCCCCCCCCCCCCCCCC\r\n");

        for cols in 2..=90usize {
            t.lock().unwrap().resize(20, cols);
            // Also render at widths that disagree with the terminal's, which is
            // what happens for a frame during a drag.
            for render_cols in [cols, 90, cols / 2 + 1, cols + 15] {
                let src = TerminalSource {
                    terminal: t.clone(),
                    show_cursor: false,
                };
                let mut out = Vec::new();
                crate::grid::GridSource::fill(
                    &src,
                    &crate::palette::Palette::default(),
                    20,
                    render_cols.max(1),
                    &mut out,
                );
                for (i, row) in out.iter().enumerate() {
                    let letters: std::collections::BTreeSet<char> = row
                        .iter()
                        .map(|c| c.c)
                        .filter(|c| c.is_ascii_alphabetic())
                        .collect();
                    assert!(
                        letters.len() <= 1,
                        "cols={cols} render_cols={render_cols} row {i} mixes {letters:?}\n                         row: {:?}",
                        row.iter().map(|c| c.c).collect::<String>()
                    );
                }
            }
        }
    }

    #[test]
    fn content_survives_a_drag_sized_sequence() {
        // A drag fires many resizes, not two. This is closer to what actually
        // happens when the splitter is dragged across the window.
        let mut t = Terminal::new(10, 60);
        t.feed(b"boddy@Mac sacrament % asdfasdfasdfasdf\r\n");
        t.feed(b"zsh: command not found: asdfasdfasdfasdf\r\n");
        let before = nonblank(&t);

        for cols in (4..60).rev().step_by(3) {
            t.resize(10, cols);
        }
        for cols in (4..=60).step_by(3) {
            t.resize(10, cols);
        }
        t.resize(10, 60);

        let after = nonblank(&t);
        assert_eq!(after, before, "\nbefore: {before:#?}\nafter:  {after:#?}");
    }
}

#[cfg(test)]
mod url_tests {
    use super::*;

    /// Index of the first character of `needle`, for pointing `url_around` at a
    /// spot inside the URL rather than counting by hand.
    fn at(text: &str, needle: &str) -> usize {
        text.chars().collect::<String>().find(needle).map(|b| text[..b].chars().count()).unwrap()
    }

    #[test]
    fn a_bare_url_is_found_from_anywhere_inside_it() {
        let line = "see https://example.com/a/b?q=1 for details";
        for probe in ["https", "example", "a/b", "q=1"] {
            assert_eq!(
                url_around(line, at(line, probe)).as_deref(),
                Some("https://example.com/a/b?q=1"),
                "probing at {probe}"
            );
        }
    }

    #[test]
    fn clicking_off_the_url_finds_nothing() {
        let line = "see https://example.com/a for details";
        assert_eq!(url_around(line, at(line, "see")), None);
        assert_eq!(url_around(line, at(line, "details")), None);
    }

    #[test]
    fn sentence_punctuation_is_not_part_of_the_url() {
        // The overwhelmingly common shape in program output, and getting it wrong
        // sends a 404 to the browser.
        for (line, want) in [
            ("Visit https://example.com/x.", "https://example.com/x"),
            ("Visit https://example.com/x, then", "https://example.com/x"),
            ("(https://example.com/x)", "https://example.com/x"),
            ("[https://example.com/x]", "https://example.com/x"),
            ("<https://example.com/x>", "https://example.com/x"),
            ("\"https://example.com/x\"", "https://example.com/x"),
        ] {
            assert_eq!(
                url_around(line, at(line, "example")).as_deref(),
                Some(want),
                "line: {line}"
            );
        }
    }

    #[test]
    fn a_query_string_keeps_its_own_punctuation() {
        // `?`, `=` and `#` are peeled only from the *end*, or every query string
        // would be truncated.
        let line = "https://example.com/s?q=a&b=c#frag";
        assert_eq!(url_around(line, 0).as_deref(), Some(line));
    }

    #[test]
    fn only_web_schemes_are_returned() {
        // The safety boundary: `open` launches a registered handler for any
        // scheme, and terminal output is often not yours.
        for line in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "javascript:alert(1)",
            "vscode://file/tmp/x",
            "mailto:a@b.com",
            "https://",
        ] {
            assert_eq!(url_around(line, 2), None, "line: {line}");
        }
    }

    #[test]
    fn a_wrapped_url_is_recovered_whole() {
        // The case that motivates reading the logical line: a long URL is exactly
        // the kind that wraps, and one screen row holds only part of it.
        let mut t = Terminal::new(4, 20);
        t.feed(b"see https://example.com/a/very/long/path?x=1 here");
        // Row 0 holds the first 20 columns; the URL continues onto rows 1 and 2.
        let found = t.url_at(0, 8).expect("finds the url from its first row");
        assert_eq!(found, "https://example.com/a/very/long/path?x=1");
        // And from a continuation row.
        assert_eq!(
            t.url_at(1, 2).as_deref(),
            Some("https://example.com/a/very/long/path?x=1"),
            "must resolve the same url from a wrapped row"
        );
    }

    #[test]
    fn a_click_past_the_end_of_the_text_is_not_a_url() {
        let mut t = Terminal::new(4, 40);
        t.feed(b"https://example.com/a");
        assert_eq!(t.url_at(0, 38), None, "trailing blanks are not the url");
        assert_eq!(t.url_at(3, 0), None, "an empty row below has none");
    }
}
