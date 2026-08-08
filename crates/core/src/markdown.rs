//! Markdown → styled lines, for read mode.
//!
//! Ported from v1's `tui/markdown.rs` with one structural change, which is the
//! whole point of the port: **this does not wrap.**
//!
//! v1 wrapped inline text here, inside the renderer, and handed finished lines
//! to a `Paragraph` that did no wrapping of its own. Wrapping therefore only
//! happened where the renderer remembered to do it — anything it emitted whole
//! (tables, long unbroken spans) ran off the edge and was clipped, which is why
//! word wrap looks broken in v1's read mode. It also meant a resize had to
//! re-render the document to re-wrap it.
//!
//! Here a block becomes **one logical [`Line`]**, however long, carrying the
//! `indent` its continuations should hang under. The frontend then wraps it with
//! the same `text::wrap_line` the editor uses for source code. Wrapping is a
//! property of the layout rather than something the renderer has to remember,
//! so it can't be missed for a particular construct.
//!
//! `width` is still taken, because some blocks are genuinely width-shaped: a
//! horizontal rule spans the pane, a fenced code block is padded to it so its
//! background is a solid slab, and a table is laid out as a grid whose columns
//! are budgeted to fit. Those are laid out here; only *inline* wrapping moved out.
//!
//! A table is the one construct that wraps here rather than in the frontend, and
//! it has to: wrapping is per *cell*, inside a column, and the generic row-level
//! wrap has no idea where the columns are. See [`State::emit_table`].
//!
//! Colors are [`Slot`]s, not RGB — same rule as the rest of the app, so the
//! theme drives them.

use std::path::Path;

use pulldown_cmark::{
    Alignment, CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd,
};
use unicode_width::UnicodeWidthStr;

use crate::highlight::{Emphasis, Slot};
use crate::text;

/// A run's appearance. `None` means "inherit", which is what makes the style
/// stack compose — nested emphasis inside a link keeps the link's color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Slot>,
    pub bg: Option<Slot>,
    pub emphasis: Emphasis,
}

impl Style {
    pub const fn fg(slot: Slot) -> Self {
        Self {
            fg: Some(slot),
            bg: None,
            emphasis: Emphasis::NONE,
        }
    }

    // No `bold` constructor, deliberately: with nothing able to set it here,
    // "the renderer emits no bold" is a property of the type rather than a rule
    // to remember. `nothing_the_renderer_emits_is_bold` pins the same thing from
    // the outside.
    const fn underline(mut self) -> Self {
        self.emphasis.underline = true;
        self
    }

    const fn italic(mut self) -> Self {
        self.emphasis.italic = true;
        self
    }

    /// Layer `other` on top: its set fields win, its emphasis accumulates.
    fn patch(self, other: Self) -> Self {
        Self {
            fg: other.fg.or(self.fg),
            bg: other.bg.or(self.bg),
            emphasis: Emphasis {
                bold: self.emphasis.bold || other.emphasis.bold,
                italic: self.emphasis.italic || other.emphasis.italic,
                underline: self.emphasis.underline || other.emphasis.underline,
            },
        }
    }
}

/// A styled run of text with no internal style change.
#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

/// One logical line of rendered markdown — a whole paragraph, however long.
#[derive(Clone, Debug)]
pub struct Line {
    pub spans: Vec<Span>,
    /// Columns that continuations should hang under, so a wrapped list item
    /// lines up with its own text rather than returning to the margin. The
    /// first row's prefix is already in `spans`; this is what the rows *after*
    /// it owe.
    pub indent: usize,
    /// Whether this line may be broken across rows.
    ///
    /// False for table rows. Wrapping one moves half its cells onto a row of
    /// their own, and the column alignment that makes a table legible is gone —
    /// so a wide table scrolls sideways instead, which keeps its shape.
    pub wrappable: bool,
}

impl Default for Line {
    fn default() -> Self {
        Self {
            spans: Vec::new(),
            indent: 0,
            wrappable: true,
        }
    }
}

impl Line {
    /// The line's text with styling dropped — what gets wrapped.
    pub fn text(&self) -> String {
        self.spans.iter().map(|s| s.text.as_str()).collect()
    }

    /// Style of the character at `index`, for painting a wrapped row.
    pub fn style_at(&self, index: usize) -> Style {
        let mut seen = 0;
        for span in &self.spans {
            let len = span.text.chars().count();
            if index < seen + len {
                return span.style;
            }
            seen += len;
        }
        Style::default()
    }
}

/// Does this path look like markdown? Extension-based, as v1.
pub fn is_markdown_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("md") || e.eq_ignore_ascii_case("markdown") || e.eq_ignore_ascii_case("mdx"))
        .unwrap_or(false)
}

/// Render markdown into styled logical lines.
pub fn render(source: &str, width: usize) -> Vec<Line> {
    let mut state = State::new(width.max(1));
    let parser = Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS | Options::ENABLE_TABLES,
    );
    for event in parser {
        state.handle(event);
    }
    state.finish()
}

// Palette. Bright slots for headings so they carry over a dark background;
// `BRIGHT_BLACK` is the muted slot used everywhere else in the app for chrome.
//
// **Nothing here is bold.** The app doesn't use bold faces: a second weight
// reads as a different font at these sizes, and in a themed 16-color scheme
// hue already separates headings from body text far more clearly than weight
// would. Heading level is carried entirely by color.
const H1: Style = Style::fg(Slot::BRIGHT_MAGENTA);

/// The slots a level-1 and level-2 heading render in.
///
/// Public because a frontend may draw headings as *chrome* rather than as
/// markdown — the Jira section draws its title and its refresh control that way,
/// so the latter can be clicked. Reading the colours from here is what keeps the
/// widgets and the renderer from drifting apart, which a second hardcoded copy
/// would eventually guarantee.
///
/// Heading level is carried entirely by colour here (see the palette note above),
/// so a `text` widget in the same slot at the same size is indistinguishable from
/// a rendered heading — there is no weight or size difference to reproduce.
pub const fn h1_slot() -> Slot {
    Slot::BRIGHT_MAGENTA
}

pub const fn h2_slot() -> Slot {
    Slot::BRIGHT_YELLOW
}

const H2: Style = Style::fg(Slot::BRIGHT_YELLOW);
const H3: Style = Style::fg(Slot::BRIGHT_CYAN);
const H_REST: Style = Style::fg(Slot::CYAN);
const MUTED: Style = Style::fg(Slot::BRIGHT_BLACK);
const MARKER: Style = Style::fg(Slot::BRIGHT_YELLOW);
const LINK: Style = Style::fg(Slot::BRIGHT_BLUE).underline();
const CODE_TEXT: Style = Style::fg(Slot::WHITE);

/// Columns each table column is separated from the next by (` │ `).
const COL_SEP: usize = 3;

/// The narrowest a column may be *squeezed* to when a table doesn't fit.
///
/// Not a minimum width: a column whose content is one character wide keeps its
/// one column. This is the point at which shrinking stops helping and the table
/// overflows into horizontal scroll instead — below about this, every cell wraps
/// to one word a row and the table is less readable than a wide one you scroll.
const MIN_COL_WIDTH: usize = 6;

/// Most screen rows a single cell may wrap to before it's elided.
///
/// Without a cap, one cell holding a paragraph makes its row taller than the
/// pane, and the rows either side of it are no longer visible together — which
/// is the entire reason to draw a table rather than a list.
const MAX_CELL_ROWS: usize = 10;

/// One open list context. `next_num` is `None` for a bullet list.
#[derive(Clone, Copy)]
struct ListCtx {
    next_num: Option<u64>,
    indent: usize,
}

struct TableState {
    rows: Vec<TableRow>,
    cur_row: Option<TableRow>,
    cur_cell: Option<Vec<Span>>,
    /// Per-column alignment from the delimiter row (`---:` and friends). Short
    /// or empty when the author wrote none, so it's read with `get`.
    aligns: Vec<Alignment>,
}

struct TableRow {
    cells: Vec<Vec<Span>>,
    is_header: bool,
}

struct State {
    width: usize,
    out: Vec<Line>,
    spans: Vec<Span>,
    style_stack: Vec<Style>,
    lists: Vec<ListCtx>,
    blockquote: usize,
    in_code_block: bool,
    need_blank: bool,
    link_href: Option<String>,
    first_prefix: Option<(String, Style)>,
    block_indent: usize,
    table: Option<TableState>,
}

impl State {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: Vec::new(),
            spans: Vec::new(),
            style_stack: Vec::new(),
            lists: Vec::new(),
            blockquote: 0,
            in_code_block: false,
            need_blank: false,
            link_href: None,
            first_prefix: None,
            block_indent: 0,
            table: None,
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .iter()
            .fold(Style::default(), |acc, layer| acc.patch(*layer))
    }

    fn push_span(&mut self, text: String, extra: Style) {
        if text.is_empty() {
            return;
        }
        let style = self.current_style().patch(extra);
        let span = Span { text, style };
        if let Some(table) = self.table.as_mut()
            && let Some(cell) = table.cur_cell.as_mut()
        {
            cell.push(span);
            return;
        }
        self.spans.push(span);
    }

    fn handle(&mut self, ev: Event<'_>) {
        match ev {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(s) => {
                if self.in_code_block {
                    self.emit_code_block_text(&s);
                } else {
                    self.push_span(s.into_string(), Style::default());
                }
            }
            Event::Code(s) => {
                self.push_span(
                    s.into_string(),
                    Style {
                        fg: Some(Slot::BRIGHT_CYAN),
                        bg: Some(Slot::BLACK),
                        emphasis: Emphasis::NONE,
                    },
                );
            }
            Event::SoftBreak => {
                // A source newline inside a paragraph is a space: the paragraph
                // is one logical line and the layout decides where it breaks.
                if !self.in_code_block {
                    self.push_span(" ".to_string(), Style::default());
                }
            }
            Event::HardBreak => self.flush_block(),
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.push_span(mark.to_string(), MARKER);
            }
            Event::Html(s) | Event::InlineHtml(s) => {
                if !self.in_code_block {
                    self.push_span(s.into_string(), MUTED);
                }
            }
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => self.before_block(),
            Tag::Heading { level, .. } => {
                self.before_block();
                self.style_stack.push(match level {
                    HeadingLevel::H1 => H1,
                    HeadingLevel::H2 => H2,
                    HeadingLevel::H3 => H3,
                    _ => H_REST,
                });
            }
            Tag::BlockQuote(_) => {
                self.before_block();
                self.blockquote += 1;
            }
            Tag::CodeBlock(kind) => {
                self.before_block();
                self.in_code_block = true;
                if let CodeBlockKind::Fenced(lang) = kind {
                    let lang = lang.into_string();
                    if !lang.is_empty() {
                        let label = format!("─── {lang} ");
                        let pad = self.width.saturating_sub(label.width());
                        let mut s = label;
                        for _ in 0..pad {
                            s.push('─');
                        }
                        self.out.push(Line {
                            spans: vec![Span { text: s, style: MUTED }],
                            ..Line::default()
                        });
                    }
                }
            }
            Tag::List(start) => {
                self.before_block();
                let indent = self.lists.last().map(|c| c.indent + 4).unwrap_or(0);
                self.lists.push(ListCtx {
                    next_num: start,
                    indent,
                });
            }
            Tag::Item => {
                let (indent, marker) = match self.lists.last_mut() {
                    Some(ctx) => {
                        let m = match ctx.next_num {
                            Some(n) => format!("{n}. "),
                            None => "- ".to_string(),
                        };
                        if let Some(n) = ctx.next_num.as_mut() {
                            *n += 1;
                        }
                        (ctx.indent, m)
                    }
                    None => (0, "- ".to_string()),
                };
                self.block_indent = indent;
                self.first_prefix = Some((marker, MARKER));
            }
            // Emphasis underlines rather than italicises, so `*emphasis*` and
            // links read alike. v1 made the same call.
            Tag::Emphasis => self.style_stack.push(Style::default().underline()),
            // `**strong**` would be bold anywhere else. With no bold to reach
            // for, italic is the only attribute left that isn't already spoken
            // for — underline belongs to emphasis and links — and a *brighter*
            // colour isn't available either: in a typical theme `bright_white`
            // is the same value as `foreground`, so strong text would simply
            // vanish into the body.
            Tag::Strong => self.style_stack.push(Style::default().italic()),
            // No strikethrough in `Emphasis`, and a cell grid has nowhere to
            // draw one — muting the text carries "struck out" instead.
            Tag::Strikethrough => self.style_stack.push(MUTED),
            Tag::Link {
                link_type, dest_url, ..
            } => {
                if !matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    self.link_href = Some(dest_url.into_string());
                }
                self.style_stack.push(LINK);
            }
            Tag::Image { .. } => {
                self.style_stack.push(Style::fg(Slot::MAGENTA));
                self.push_span("[image: ".to_string(), Style::default());
            }
            Tag::Table(aligns) => {
                self.before_block();
                self.table = Some(TableState {
                    rows: Vec::new(),
                    cur_row: None,
                    cur_cell: None,
                    aligns,
                });
            }
            Tag::TableHead => {
                if let Some(t) = self.table.as_mut() {
                    t.cur_row = Some(TableRow {
                        cells: Vec::new(),
                        is_header: true,
                    });
                }
            }
            Tag::TableRow => {
                if let Some(t) = self.table.as_mut() {
                    t.cur_row = Some(TableRow {
                        cells: Vec::new(),
                        is_header: false,
                    });
                }
            }
            Tag::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    t.cur_cell = Some(Vec::new());
                }
            }
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::MetadataBlock(_)
            | Tag::HtmlBlock
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_block(),
            TagEnd::Heading(_) => {
                self.flush_block();
                self.style_stack.pop();
            }
            TagEnd::BlockQuote(_) => self.blockquote = self.blockquote.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.in_code_block = false;
                self.need_blank = true;
            }
            TagEnd::List(_) => {
                self.lists.pop();
                if self.lists.is_empty() {
                    self.block_indent = 0;
                    self.need_blank = true;
                }
            }
            TagEnd::Item => {
                self.flush_block();
                self.need_blank = false;
            }
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                self.style_stack.pop();
            }
            TagEnd::Link => {
                self.style_stack.pop();
                if let Some(href) = self.link_href.take() {
                    self.push_span(format!(" ({href})"), MUTED);
                }
            }
            TagEnd::Image => {
                self.push_span("]".to_string(), Style::default());
                self.style_stack.pop();
            }
            TagEnd::Table => {
                if let Some(t) = self.table.take() {
                    self.emit_table(t);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if let Some(t) = self.table.as_mut()
                    && let Some(row) = t.cur_row.take()
                {
                    t.rows.push(row);
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut()
                    && let Some(cell) = t.cur_cell.take()
                    && let Some(row) = t.cur_row.as_mut()
                {
                    row.cells.push(cell);
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::HtmlBlock
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn before_block(&mut self) {
        if self.need_blank && !self.out.is_empty() {
            self.out.push(Line::default());
        }
        self.need_blank = false;
    }

    fn rule(&mut self) {
        self.before_block();
        self.out.push(Line {
            spans: vec![Span {
                text: "─".repeat(self.width),
                style: MUTED,
            }],
            ..Line::default()
        });
        self.need_blank = true;
    }

    fn emit_code_block_text(&mut self, text: &str) {
        // Code-block text arrives line by line with a trailing newline that
        // terminates rather than separates.
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        for raw in trimmed.split('\n') {
            let mut padded = format!("  {raw}");
            // Padded to the pane so the block reads as a slab rather than
            // ragged text. A line longer than the pane still wraps, which is
            // what you want for code you can't otherwise see.
            let pad = self.width.saturating_sub(padded.width());
            for _ in 0..pad {
                padded.push(' ');
            }
            self.out.push(Line {
                spans: vec![Span {
                    text: padded,
                    style: CODE_TEXT,
                }],
                // Continuations of an over-long code line hang under the two
                // spaces of padding, not under the margin.
                indent: 2,
                wrappable: true,
            });
        }
    }

    /// Emit the accumulated inline spans as **one** logical line.
    ///
    /// The line's own prefixes — list indent, blockquote bars, bullet — go in
    /// front of the text, and `indent` records how wide they were so wrapped
    /// rows hang under the text rather than under the bullet.
    fn flush_block(&mut self) {
        if self.spans.is_empty() && self.first_prefix.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let first_prefix = self.first_prefix.take();
        let indent = self.block_indent;
        let bq = self.blockquote;
        let bq_prefix: String = "│ ".repeat(bq);
        let prefix_width = first_prefix.as_ref().map(|(s, _)| s.width()).unwrap_or(0);

        let mut line = Line {
            spans: Vec::new(),
            indent: indent + bq_prefix.width() + prefix_width,
            wrappable: true,
        };
        if indent > 0 {
            line.spans.push(Span {
                text: " ".repeat(indent),
                style: Style::default(),
            });
        }
        if bq > 0 {
            line.spans.push(Span {
                text: bq_prefix,
                style: MUTED,
            });
        }
        if let Some((text, style)) = first_prefix {
            line.spans.push(Span { text, style });
        }
        line.spans.extend(spans);
        self.out.push(line);
        self.need_blank = true;
    }

    /// Lay a table out as a fixed grid, wrapping cells inside their columns.
    ///
    /// **Every emitted line is padded to the same column boundaries**, so the
    /// alignment holds by construction rather than by the cells happening to be
    /// short enough. A row is as tall as its tallest cell, and each of its screen
    /// rows redraws the separators — so a wrapped cell stays inside its own
    /// column instead of pushing the ones after it right.
    ///
    /// This is *not* the wrapping [`Line::wrappable`] forbids. That one breaks the
    /// concatenated row text, which drops the trailing cells to column 0 and
    /// destroys the table; these rows are laid out per cell and stay unwrappable,
    /// already fitted to the pane.
    fn emit_table(&mut self, table: TableState) {
        if table.rows.is_empty() {
            return;
        }
        let n_cols = table.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        if n_cols == 0 {
            return;
        }

        let mut natural = vec![0usize; n_cols];
        for row in &table.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                let w: usize = cell.iter().map(|s| s.text.width()).sum();
                natural[i] = natural[i].max(w);
            }
        }
        let sep_total = COL_SEP * (n_cols - 1);
        let widths = budget_columns(&natural, self.width.saturating_sub(sep_total));

        for (row_idx, row) in table.rows.iter().enumerate() {
            const NO_CELL: &[Span] = &[];
            let cells: Vec<Vec<Vec<Span>>> = widths
                .iter()
                .enumerate()
                .map(|(col, &w)| {
                    let cell = row.cells.get(col).map_or(NO_CELL, |c| c.as_slice());
                    wrap_cell(cell, w)
                })
                .collect();
            let height = cells.iter().map(|c| c.len()).max().unwrap_or(1);

            for sub in 0..height {
                let mut spans: Vec<Span> = Vec::new();
                for (col, &target) in widths.iter().enumerate() {
                    if col > 0 {
                        spans.push(Span {
                            text: " │ ".to_string(),
                            style: MUTED,
                        });
                    }
                    let content = cells[col].get(sub).map_or(NO_CELL, |r| r.as_slice());
                    // A header cell keeps its own alignment rather than the
                    // column's: a right-aligned numeric column still reads
                    // better with its title over the left edge of the numbers.
                    let align = if row.is_header {
                        Alignment::Left
                    } else {
                        table.aligns.get(col).copied().unwrap_or(Alignment::None)
                    };
                    push_cell(&mut spans, content, target, align);
                }
                self.out.push(Line {
                    spans,
                    indent: 0,
                    wrappable: false,
                });
            }

            if row_idx == 0 && row.is_header {
                let mut sep: Vec<Span> = Vec::new();
                for (col, width) in widths.iter().enumerate() {
                    if col > 0 {
                        sep.push(Span {
                            text: "─┼─".to_string(),
                            style: MUTED,
                        });
                    }
                    sep.push(Span {
                        text: "─".repeat(*width),
                        style: MUTED,
                    });
                }
                self.out.push(Line {
                    spans: sep,
                    indent: 0,
                    wrappable: false,
                });
            }
        }
        self.need_blank = true;
    }

    fn finish(mut self) -> Vec<Line> {
        self.flush_block();
        self.out
    }
}

/// Decide each column's width, given what each one wants and what there is.
///
/// **Water-filling, not proportional scaling.** Every column gets its natural
/// width if that's under the fair share; only the columns over their share are
/// squeezed, and they split what the others didn't use. So a narrow column (`#`,
/// a section symbol) keeps its width and the prose column absorbs the whole
/// shortfall — which is what you'd do by hand.
///
/// Scaling every column by one factor is the wrong shape and was the visible bug:
/// it took a column off `#`, which had none to give, while leaving the one wide
/// column still too narrow to fit.
///
/// Squeezing stops at [`MIN_COL_WIDTH`], so the result may exceed `avail`. That's
/// deliberate — the frontend reaches an over-wide table by scrolling sideways
/// (`read_scroll_col`), and the rows stay aligned because they're still a grid.
fn budget_columns(natural: &[usize], avail: usize) -> Vec<usize> {
    let mut out = natural.to_vec();
    if natural.iter().sum::<usize>() <= avail {
        return out;
    }

    // Ascending, so the columns most likely to fit inside their share are settled
    // first and release their surplus to the ones that don't.
    let mut order: Vec<usize> = (0..natural.len()).collect();
    order.sort_by_key(|&i| natural[i]);

    let mut remaining = avail;
    let mut left = natural.len();
    for (pos, &i) in order.iter().enumerate() {
        let fair = remaining / left;
        if natural[i] <= fair {
            out[i] = natural[i];
            remaining -= natural[i];
            left -= 1;
            continue;
        }
        // This column is over its share, and so is every one after it. Split what
        // is left evenly, handing the first few an extra column so the rounding
        // remainder is spent rather than left as a ragged right edge.
        let extra = remaining % left;
        for (k, &j) in order[pos..].iter().enumerate() {
            out[j] = (fair + usize::from(k < extra)).max(MIN_COL_WIDTH);
        }
        break;
    }
    out
}

/// Break one cell into the screen rows it occupies at `width`.
///
/// Always at least one row, so an empty cell still holds its column open. Uses
/// the same [`text::wrap_line`] the editor wraps source code with, so a token too
/// long to fit hard-breaks instead of overflowing.
fn wrap_cell(cell: &[Span], width: usize) -> Vec<Vec<Span>> {
    let text: String = cell.iter().map(|s| s.text.as_str()).collect();
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || width == 0 {
        return vec![Vec::new()];
    }

    let segments = text::wrap_line(&text, width, 1, 0);
    let elided = segments.len() > MAX_CELL_ROWS;
    let mut rows = Vec::with_capacity(segments.len().min(MAX_CELL_ROWS));

    for (i, &(start, end)) in segments.iter().take(MAX_CELL_ROWS).enumerate() {
        // `wrap_line` breaks *after* the whitespace, so a segment carries its own
        // trailing space. Dropping it matters for a right-aligned column, where it
        // would otherwise show up as a gap against the separator.
        let mut end = trim_end(&chars, start, end);
        if elided && i + 1 == MAX_CELL_ROWS {
            // Out of rows with text still to place. Give the ellipsis its column
            // back before slicing, or the row runs one wide and breaks the grid.
            while end > start && visual_width(&chars[start..end]) + 1 > width {
                end -= 1;
            }
            let mut row = slice_spans(cell, start, trim_end(&chars, start, end));
            row.push(Span {
                text: "…".to_string(),
                style: MUTED,
            });
            rows.push(row);
            break;
        }
        rows.push(slice_spans(cell, start, end));
    }
    rows
}

/// Append a cell's content padded to `target` columns, honouring alignment.
///
/// The pad is what keeps the grid: every column ends where the next one begins,
/// on every screen row, whatever the cell holds.
fn push_cell(spans: &mut Vec<Span>, content: &[Span], target: usize, align: Alignment) {
    let width: usize = content.iter().map(|s| s.text.width()).sum();
    let pad = target.saturating_sub(width);
    let (before, after) = match align {
        Alignment::Right => (pad, 0),
        Alignment::Center => (pad / 2, pad - pad / 2),
        Alignment::Left | Alignment::None => (0, pad),
    };
    let space = |n: usize| Span {
        text: " ".repeat(n),
        style: Style::default(),
    };
    if before > 0 {
        spans.push(space(before));
    }
    spans.extend(content.iter().cloned());
    if after > 0 {
        spans.push(space(after));
    }
}

/// The slice of `spans` covering characters `[start, end)`, keeping each run's
/// style — so a wrapped cell's second row is styled like its first.
fn slice_spans(spans: &[Span], start: usize, end: usize) -> Vec<Span> {
    let mut out = Vec::new();
    let mut seen = 0;
    for span in spans {
        if seen >= end {
            break;
        }
        let len = span.text.chars().count();
        let from = seen.max(start);
        let to = (seen + len).min(end);
        if from < to {
            out.push(Span {
                text: span.text.chars().skip(from - seen).take(to - from).collect(),
                style: span.style,
            });
        }
        seen += len;
    }
    out
}

fn visual_width(chars: &[char]) -> usize {
    chars
        .iter()
        .map(|c| unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0))
        .sum()
}

/// `end` with trailing whitespace excluded, never going past `start`.
fn trim_end(chars: &[char], start: usize, mut end: usize) -> usize {
    while end > start && chars[end - 1].is_whitespace() {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.text()).collect()
    }

    #[test]
    fn a_paragraph_is_one_logical_line_however_long() {
        // The property the whole port exists for: no wrapping happens here, so
        // the frontend can wrap with the same code the editor uses.
        let long = "word ".repeat(200);
        let out = render(&long, 40);
        let paragraphs: Vec<&Line> = out.iter().filter(|l| !l.text().trim().is_empty()).collect();
        assert_eq!(paragraphs.len(), 1, "one paragraph, one line");
        assert!(paragraphs[0].text().chars().count() > 900, "not pre-wrapped");
    }

    #[test]
    fn a_soft_break_inside_a_paragraph_becomes_a_space() {
        let out = render("one\ntwo", 80);
        assert_eq!(texts(&out), vec!["one two"]);
    }

    #[test]
    fn headings_are_told_apart_by_colour_not_weight() {
        let h1 = render("# Title", 80);
        assert_eq!(h1[0].text(), "Title");
        assert_eq!(h1[0].style_at(0).fg, Some(Slot::BRIGHT_MAGENTA));
        let h2 = render("## Title", 80);
        assert_eq!(h2[0].style_at(0).fg, Some(Slot::BRIGHT_YELLOW));
        assert_ne!(h1[0].style_at(0).fg, h2[0].style_at(0).fg);
    }

    #[test]
    fn nothing_the_renderer_emits_is_bold() {
        // The app uses no bold faces anywhere it chooses its own styling.
        let src = "# H1\n## H2\n### H3\n#### H4\n\n**strong** and *em* and `code`\n\n\
                   - item\n\n> quote\n\n| a | b |\n|---|---|\n| 1 | 2 |\n\n---\n\n\
                   ```rust\nfn f() {}\n```\n\n[link](http://x)";
        for line in render(src, 40) {
            for span in &line.spans {
                assert!(
                    !span.style.emphasis.bold,
                    "bold slipped into {:?}",
                    span.text
                );
            }
        }
    }

    #[test]
    fn a_list_item_hangs_under_its_own_text() {
        let out = render("- item text", 80);
        let item = &out[0];
        assert_eq!(item.text(), "- item text");
        // "- " is two columns, so wrapped rows start two in and line up with
        // the text rather than the bullet.
        assert_eq!(item.indent, 2);
    }

    #[test]
    fn a_nested_list_indents_further() {
        let out = render("- outer\n    - inner", 80);
        let inner = out.iter().find(|l| l.text().contains("inner")).unwrap();
        assert!(inner.indent > 2, "nested item hangs deeper: {}", inner.indent);
        assert!(inner.text().starts_with("    "));
    }

    #[test]
    fn ordered_lists_number_themselves() {
        let out = render("1. one\n2. two", 80);
        let items: Vec<String> = texts(&out).into_iter().filter(|t| !t.trim().is_empty()).collect();
        assert_eq!(items, vec!["1. one", "2. two"]);
    }

    #[test]
    fn a_blockquote_gets_a_bar_and_hangs_past_it() {
        let out = render("> quoted", 80);
        let line = out.iter().find(|l| l.text().contains("quoted")).unwrap();
        assert!(line.text().starts_with("│ "));
        assert_eq!(line.indent, 2, "continuations clear the bar");
    }

    #[test]
    fn inline_code_gets_a_background() {
        let out = render("some `code` here", 80);
        let line = &out[0];
        let at = line.text().find("code").unwrap();
        assert_eq!(line.style_at(at).bg, Some(Slot::BLACK));
    }

    #[test]
    fn a_fenced_block_is_padded_to_the_pane() {
        let out = render("```\nx\n```", 20);
        let code = out.iter().find(|l| l.text().contains('x')).unwrap();
        assert_eq!(code.text().width(), 20, "padded into a slab");
        assert_eq!(code.indent, 2);
    }

    #[test]
    fn a_rule_spans_the_pane() {
        let out = render("---", 12);
        assert_eq!(out[0].text(), "─".repeat(12));
    }


    #[test]
    fn a_table_lays_out_its_columns() {
        let out = render("| a | b |\n|---|---|\n| 1 | 2 |", 40);
        let rows: Vec<String> = texts(&out).into_iter().filter(|t| !t.trim().is_empty()).collect();
        assert!(rows[0].contains('a') && rows[0].contains('b'));
        assert!(rows[1].contains('┼'), "header underline: {:?}", rows[1]);
        assert!(rows[2].contains('1') && rows[2].contains('2'));
    }

    /// Rows of a rendered table, in the order they were emitted.
    fn table_rows(out: &[Line]) -> Vec<String> {
        out.iter()
            .filter(|l| !l.wrappable)
            .map(|l| l.text())
            .collect()
    }

    /// Where each ` │ ` separator sits, in display columns.
    fn seps(row: &str) -> Vec<usize> {
        let mut cols = Vec::new();
        let mut vis = 0;
        for c in row.chars() {
            if c == '│' {
                cols.push(vis);
            }
            vis += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        }
        cols
    }

    const RAGGED: &str = "\
| # | Gap | Impact |
|---|-----|--------|
| 1 | Player shot tiers disabled; power accrual much too fast | 10 |
| 2 | Music off | 8 |
| 3 | Boot flow: boots straight into gameplay, no splash or title menu | 9 |";

    #[test]
    fn every_row_of_a_table_puts_its_separators_in_the_same_columns() {
        // The bug this rewrite exists for: a cell wider than its column used to be
        // emitted whole, shifting every separator after it right and pushing the
        // trailing columns off the pane. Alignment now holds however long a cell is.
        for width in [30usize, 45, 60, 80, 120] {
            let rows = table_rows(&render(RAGGED, width));
            let first = seps(&rows[0]);
            assert_eq!(first.len(), 2, "two separators at width {width}");
            for row in &rows {
                if row.contains('┼') {
                    continue; // the header underline joins with ┼, not │
                }
                assert_eq!(seps(row), first, "width {width}, row {row:?}");
            }
        }
    }

    #[test]
    fn a_long_cell_wraps_inside_its_column_instead_of_overflowing() {
        let rows = table_rows(&render(RAGGED, 40));
        for row in &rows {
            let w = row.width();
            assert!(w <= 40, "row is {w} wide at pane width 40: {row:?}");
        }
        // Nothing is lost to the wrap: the tail of the long cell is on a later row.
        let all = rows.join("\n");
        assert!(all.contains("Player shot tiers"), "{all}");
        assert!(all.contains("too fast"), "wrapped tail survives:\n{all}");
    }

    #[test]
    fn a_narrow_column_keeps_its_width_and_the_prose_column_gives() {
        // Water-filling, not proportional scaling. `#` holds one character, so it
        // must not be squeezed on behalf of a column that needs forty.
        let rows = table_rows(&render(RAGGED, 40));
        let first = seps(&rows[0]);
        assert_eq!(first[0], 2, "the `#` column stays one wide: {:?}", rows[0]);
    }

    #[test]
    fn a_table_that_fits_is_left_at_its_natural_width() {
        let rows = table_rows(&render(RAGGED, 200));
        assert!(rows[0].width() < 100, "no stretching to the pane: {:?}", rows[0]);
        for row in &rows {
            assert_eq!(row.width(), rows[0].width(), "a uniform grid: {row:?}");
        }
    }

    #[test]
    fn a_table_too_narrow_to_squeeze_overflows_rather_than_collapsing() {
        // Below `MIN_COL_WIDTH` per column, shrinking stops helping — the table
        // exceeds the pane and the frontend reaches it by scrolling sideways.
        let rows = table_rows(&render(RAGGED, 12));
        assert!(rows[0].width() > 12, "overflows: {:?}", rows[0]);
        let first = seps(&rows[0]);
        for row in &rows {
            if !row.contains('┼') {
                assert_eq!(seps(row), first, "still a grid: {row:?}");
            }
        }
    }

    #[test]
    fn a_wrapped_cell_keeps_its_styling_on_every_row() {
        let src = "| a |\n|---|\n| *one two three four five six seven* |";
        let out = render(src, 20);
        let wrapped: Vec<&Line> = out
            .iter()
            .filter(|l| !l.wrappable && l.text().contains("seven"))
            .collect();
        let line = wrapped.first().expect("the tail landed on its own row");
        let at = line.text().find("seven").unwrap();
        assert!(
            line.style_at(at).emphasis.underline,
            "emphasis survives the wrap"
        );
    }

    #[test]
    fn a_cell_longer_than_the_row_cap_is_elided() {
        let long = "word ".repeat(200);
        let src = format!("| a |\n|---|\n| {long} |");
        let out = render(&src, 20);
        let rows = table_rows(&out);
        // One header, one underline, then at most the cap.
        assert_eq!(rows.len(), 2 + MAX_CELL_ROWS, "capped: {}", rows.len());
        assert!(rows.last().unwrap().contains('…'), "{:?}", rows.last());
        for row in &rows {
            assert!(row.width() <= 20, "the ellipsis stays inside: {row:?}");
        }
    }

    #[test]
    fn a_column_marked_right_aligned_is_right_aligned() {
        let out = render("| n |\n|--:|\n| 1 |\n| 100 |", 40);
        let rows = table_rows(&out);
        let short = rows.iter().find(|r| r.contains('1') && !r.contains("100")).unwrap();
        assert!(short.starts_with("  1"), "padded on the left: {short:?}");
        let head = &rows[0];
        assert!(head.starts_with('n'), "a header keeps its own side: {head:?}");
    }

    #[test]
    fn budgeting_spends_the_whole_width_it_is_given() {
        let natural = [1, 60, 6, 8];
        let got = budget_columns(&natural, 50);
        assert_eq!(got.iter().sum::<usize>(), 50, "{got:?}");
        assert_eq!(got[0], 1, "a column under its share is untouched");
        assert_eq!(got[2], 6);
        assert_eq!(got[3], 8);
        assert_eq!(got[1], 35, "the wide column absorbs the shortfall");
    }

    #[test]
    fn budgeting_leaves_a_table_that_fits_alone() {
        let natural = [1, 20, 6];
        assert_eq!(budget_columns(&natural, 50), natural);
    }

    #[test]
    fn budgeting_stops_squeezing_at_the_floor() {
        let got = budget_columns(&[40, 40, 40], 6);
        assert_eq!(got, vec![MIN_COL_WIDTH; 3], "overflow rather than collapse");
    }

    #[test]
    fn a_link_shows_its_target() {
        let out = render("[text](http://example.com)", 80);
        assert_eq!(out[0].text(), "text (http://example.com)");
        assert_eq!(out[0].style_at(0).fg, Some(Slot::BRIGHT_BLUE));
    }

    #[test]
    fn nested_emphasis_composes_rather_than_replacing() {
        let out = render("**bold and *also italic***", 80);
        let line = &out[0];
        let at = line.text().find("also").unwrap();
        let style = line.style_at(at);
        assert!(style.emphasis.italic, "outer strong survives");
        assert!(style.emphasis.underline, "inner emphasis applies too");
    }

    #[test]
    fn markdown_paths_are_recognised_by_extension() {
        assert!(is_markdown_path(Path::new("a.md")));
        assert!(is_markdown_path(Path::new("a.MARKDOWN")));
        assert!(is_markdown_path(Path::new("a.mdx")));
        assert!(!is_markdown_path(Path::new("a.rs")));
        assert!(!is_markdown_path(Path::new("README")));
    }
}
