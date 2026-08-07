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
//! background is a solid slab, and table columns are scaled to fit. Those are
//! laid out here; only *inline* wrapping moved out.
//!
//! Colors are [`Slot`]s, not RGB — same rule as the rest of the app, so the
//! theme drives them.

use std::path::Path;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use unicode_width::UnicodeWidthStr;

use crate::highlight::{Emphasis, Slot};

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
const H2: Style = Style::fg(Slot::BRIGHT_YELLOW);
const H3: Style = Style::fg(Slot::BRIGHT_CYAN);
const H_REST: Style = Style::fg(Slot::CYAN);
const MUTED: Style = Style::fg(Slot::BRIGHT_BLACK);
const MARKER: Style = Style::fg(Slot::BRIGHT_YELLOW);
const LINK: Style = Style::fg(Slot::BRIGHT_BLUE).underline();
const CODE_TEXT: Style = Style::fg(Slot::WHITE);

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
            Tag::Table(_) => {
                self.before_block();
                self.table = Some(TableState {
                    rows: Vec::new(),
                    cur_row: None,
                    cur_cell: None,
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

    fn emit_table(&mut self, table: TableState) {
        if table.rows.is_empty() {
            return;
        }
        let n_cols = table.rows.iter().map(|r| r.cells.len()).max().unwrap_or(0);
        if n_cols == 0 {
            return;
        }

        let mut col_widths = vec![0usize; n_cols];
        for row in &table.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                let w: usize = cell.iter().map(|s| s.text.width()).sum();
                col_widths[i] = col_widths[i].max(w);
            }
        }

        // Scale columns down proportionally when the natural width doesn't fit.
        // Three cells minimum, so a single character plus a pad still lands.
        let sep_total = if n_cols > 1 { 3 * (n_cols - 1) } else { 0 };
        let avail = self.width.saturating_sub(sep_total);
        let natural: usize = col_widths.iter().sum();
        if natural > avail && natural > 0 {
            let factor = avail as f64 / natural as f64;
            for w in col_widths.iter_mut() {
                *w = ((*w as f64 * factor) as usize).max(3);
            }
        }

        for (row_idx, row) in table.rows.iter().enumerate() {
            let mut spans: Vec<Span> = Vec::new();
            for (col, target) in col_widths.iter().enumerate() {
                if col > 0 {
                    spans.push(Span {
                        text: " │ ".to_string(),
                        style: MUTED,
                    });
                }
                let cell = row.cells.get(col);
                let cell_w: usize = cell
                    .map(|c| c.iter().map(|s| s.text.width()).sum())
                    .unwrap_or(0);
                if let Some(cell) = cell {
                    spans.extend(cell.iter().cloned());
                }
                if cell_w < *target {
                    spans.push(Span {
                        text: " ".repeat(target - cell_w),
                        style: Style::default(),
                    });
                }
            }
            self.out.push(Line {
                spans,
                indent: 0,
                wrappable: false,
            });

            if row_idx == 0 && row.is_header {
                let mut sep: Vec<Span> = Vec::new();
                for (col, width) in col_widths.iter().enumerate() {
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
