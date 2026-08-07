use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, LinkType, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// Render a markdown source into a sequence of styled Lines, wrapped to `width`.
pub fn render(source: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(1);
    let mut state = State::new(width);
    let parser = Parser::new_ext(
        source,
        Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_TABLES,
    );
    for event in parser {
        state.handle(event);
    }
    state.finish()
}

/// One open list context. None = bullet list, Some(n) = next ordered number.
#[derive(Clone, Copy)]
struct ListCtx {
    next_num: Option<u64>,
    /// Indent (in cells) applied to every line of this list's items.
    indent: usize,
}

struct TableState {
    rows: Vec<TableRow>,
    cur_row: Option<TableRow>,
    cur_cell: Option<Vec<Span<'static>>>,
}

struct TableRow {
    cells: Vec<Vec<Span<'static>>>,
    is_header: bool,
}

struct State {
    width: usize,
    out: Vec<Line<'static>>,
    /// Inline span buffer for the current block (paragraph/heading/item).
    spans: Vec<Span<'static>>,
    /// Style stack — the current effective style is the merged stack.
    style_stack: Vec<Style>,
    /// Open list contexts (innermost last).
    lists: Vec<ListCtx>,
    /// Blockquote nesting.
    blockquote: usize,
    /// True while inside a code block (between Start(CodeBlock) and End).
    in_code_block: bool,
    /// Heading we're currently inside, if any.
    heading: Option<HeadingLevel>,
    /// Whether a blank separator should precede the next block.
    need_blank: bool,
    /// Active link href, if any (for trailing display).
    link_href: Option<String>,
    /// Pending first-line prefix for the next block (e.g., "  - " for list item).
    first_prefix: Option<(String, Style)>,
    /// Indent for the current block (applies to every line).
    block_indent: usize,
    /// Active table accumulator, if we're inside one.
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
            heading: None,
            need_blank: false,
            link_href: None,
            first_prefix: None,
            block_indent: 0,
            table: None,
        }
    }

    fn current_style(&self) -> Style {
        let mut s = Style::default();
        for layer in &self.style_stack {
            s = s.patch(*layer);
        }
        s
    }

    fn push_span(&mut self, text: String, extra: Style) {
        if text.is_empty() {
            return;
        }
        let style = self.current_style().patch(extra);
        let span = Span::styled(text, style);
        if let Some(table) = self.table.as_mut() {
            if let Some(cell) = table.cur_cell.as_mut() {
                cell.push(span);
                return;
            }
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
                let bg = Style::default().bg(Color::Black).fg(Color::LightCyan);
                self.push_span(s.into_string(), bg);
            }
            Event::SoftBreak => {
                if !self.in_code_block {
                    self.push_span(" ".to_string(), Style::default());
                }
            }
            Event::HardBreak => {
                self.flush_block();
            }
            Event::Rule => self.rule(),
            Event::TaskListMarker(checked) => {
                let mark = if checked { "[x] " } else { "[ ] " };
                self.push_span(
                    mark.to_string(),
                    Style::default().fg(Color::LightYellow),
                );
            }
            // HTML, math, footnotes — render as plain text or skip.
            Event::Html(s) | Event::InlineHtml(s) => {
                if !self.in_code_block {
                    self.push_span(s.into_string(), Style::default().fg(Color::DarkGray));
                }
            }
            Event::FootnoteReference(_)
            | Event::InlineMath(_)
            | Event::DisplayMath(_) => {}
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.before_block();
            }
            Tag::Heading { level, .. } => {
                self.before_block();
                self.heading = Some(level);
                let style = match level {
                    HeadingLevel::H1 => Style::default()
                        .fg(Color::LightMagenta)
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H2 => Style::default()
                        .fg(Color::LightYellow)
                        .add_modifier(Modifier::BOLD),
                    HeadingLevel::H3 => Style::default()
                        .fg(Color::LightCyan)
                        .add_modifier(Modifier::BOLD),
                    _ => Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                };
                self.style_stack.push(style);
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
                        // Emit a small label line above the block.
                        let label = format!("─── {} ", lang);
                        let pad = self.width.saturating_sub(label.width()).max(0);
                        let mut s = label;
                        for _ in 0..pad {
                            s.push('─');
                        }
                        self.out.push(Line::from(Span::styled(
                            s,
                            Style::default().fg(Color::DarkGray),
                        )));
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
                let (indent, marker) = if let Some(ctx) = self.lists.last_mut() {
                    let m = match ctx.next_num {
                        Some(n) => format!("{}. ", n),
                        None => "- ".to_string(),
                    };
                    if let Some(n) = ctx.next_num.as_mut() {
                        *n += 1;
                    }
                    (ctx.indent, m)
                } else {
                    (0, "- ".to_string())
                };
                self.block_indent = indent;
                self.first_prefix = Some((
                    marker,
                    Style::default().fg(Color::LightYellow),
                ));
            }
            Tag::Emphasis => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::UNDERLINED));
            }
            Tag::Strong => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::BOLD));
            }
            Tag::Strikethrough => {
                self.style_stack
                    .push(Style::default().add_modifier(Modifier::CROSSED_OUT));
            }
            Tag::Link {
                link_type, dest_url, ..
            } => {
                if !matches!(link_type, LinkType::Autolink | LinkType::Email) {
                    self.link_href = Some(dest_url.into_string());
                }
                self.style_stack.push(
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
            Tag::Image { .. } => {
                // Inline image: render as "[image: <alt>]" — alt text comes
                // through as Text events between Start and End(Image).
                self.style_stack
                    .push(Style::default().fg(Color::Magenta));
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
            Tag::FootnoteDefinition(_) | Tag::DefinitionList | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition | Tag::MetadataBlock(_) | Tag::HtmlBlock => {}
            Tag::Superscript | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.flush_block();
            }
            TagEnd::Heading(_) => {
                self.flush_block();
                self.style_stack.pop();
                self.heading = None;
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote = self.blockquote.saturating_sub(1);
            }
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
                    let pretty = format!(" ({})", href);
                    self.push_span(pretty, Style::default().fg(Color::DarkGray));
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
                if let Some(t) = self.table.as_mut() {
                    if let Some(row) = t.cur_row.take() {
                        t.rows.push(row);
                    }
                }
            }
            TagEnd::TableCell => {
                if let Some(t) = self.table.as_mut() {
                    if let Some(cell) = t.cur_cell.take() {
                        if let Some(row) = t.cur_row.as_mut() {
                            row.cells.push(cell);
                        }
                    }
                }
            }
            TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::MetadataBlock(_)
            | TagEnd::HtmlBlock => {}
            TagEnd::Superscript | TagEnd::Subscript => {}
        }
    }

    /// Emit a blank separator line if needed before starting a new block.
    fn before_block(&mut self) {
        if self.need_blank && !self.out.is_empty() {
            self.out.push(Line::from(Span::raw("")));
        }
        self.need_blank = false;
    }

    fn rule(&mut self) {
        self.before_block();
        let mut s = String::with_capacity(self.width);
        for _ in 0..self.width {
            s.push('─');
        }
        self.out.push(Line::from(Span::styled(
            s,
            Style::default().fg(Color::DarkGray),
        )));
        self.need_blank = true;
    }

    fn emit_code_block_text(&mut self, text: &str) {
        // Code-block Text events arrive line-by-line, each with a trailing
        // newline. Drop the trailing newline (it terminates rather than
        // separates) before splitting.
        let trimmed = text.strip_suffix('\n').unwrap_or(text);
        let style = Style::default().fg(Color::White);
        for raw in trimmed.split('\n') {
            let mut padded = String::from("  ");
            padded.push_str(raw);
            let pad = self.width.saturating_sub(padded.width());
            for _ in 0..pad {
                padded.push(' ');
            }
            self.out.push(Line::from(Span::styled(padded, style)));
        }
    }

    /// Flush the accumulated inline spans into one or more wrapped lines.
    fn flush_block(&mut self) {
        if self.spans.is_empty() && self.first_prefix.is_none() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let first_prefix = self.first_prefix.take();
        let indent = self.block_indent;
        let bq = self.blockquote;

        let bq_style = Style::default().fg(Color::DarkGray);
        let bq_prefix: String = "│ ".repeat(bq);
        let first_pre_w = first_prefix
            .as_ref()
            .map(|(s, _)| s.width())
            .unwrap_or(0);

        // Content area width on every line: total width minus block-level
        // indent, blockquote prefix, and the bullet/number indent (which is
        // recreated as spaces on continuation lines).
        let content_width = self
            .width
            .saturating_sub(indent + bq_prefix.width() + first_pre_w)
            .max(1);

        let tokens = spans_to_tokens(spans);
        let mut wrapped = wrap_tokens(tokens, content_width);
        if wrapped.is_empty() {
            wrapped.push(Vec::new());
        }

        for (i, line_spans) in wrapped.into_iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            if indent > 0 {
                spans.push(Span::raw(" ".repeat(indent)));
            }
            if bq > 0 {
                spans.push(Span::styled(bq_prefix.clone(), bq_style));
            }
            if i == 0 {
                if let Some((p, st)) = &first_prefix {
                    spans.push(Span::styled(p.clone(), *st));
                }
            } else if first_pre_w > 0 {
                spans.push(Span::raw(" ".repeat(first_pre_w)));
            }
            spans.extend(line_spans);
            self.out.push(Line::from(spans));
        }

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

        // Compute the natural width of each cell.
        let mut col_widths = vec![0usize; n_cols];
        for row in &table.rows {
            for (i, cell) in row.cells.iter().enumerate() {
                let w: usize = cell.iter().map(|s| s.content.width()).sum();
                if w > col_widths[i] {
                    col_widths[i] = w;
                }
            }
        }

        // If the natural total exceeds available width, scale columns down
        // proportionally (minimum 3 cells per column so single chars + a pad
        // still fit). The separator " │ " between columns costs 3 cells per
        // gap.
        let sep_total = if n_cols > 1 { 3 * (n_cols - 1) } else { 0 };
        let avail = self.width.saturating_sub(sep_total);
        let natural: usize = col_widths.iter().sum();
        if natural > avail {
            let factor = avail as f64 / natural as f64;
            for w in col_widths.iter_mut() {
                *w = ((*w as f64 * factor) as usize).max(3);
            }
        }

        let sep_style = Style::default().fg(Color::DarkGray);

        for (row_idx, row) in table.rows.iter().enumerate() {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for col in 0..n_cols {
                if col > 0 {
                    spans.push(Span::styled(" │ ".to_string(), sep_style));
                }
                let cell = row.cells.get(col);
                let cell_w: usize = cell
                    .map(|c| c.iter().map(|s| s.content.width()).sum())
                    .unwrap_or(0);
                let target = col_widths[col];
                if let Some(cell) = cell {
                    for sp in cell {
                        spans.push(sp.clone());
                    }
                }
                if cell_w < target {
                    spans.push(Span::raw(" ".repeat(target - cell_w)));
                }
            }
            self.out.push(Line::from(spans));

            // Header underline.
            if row_idx == 0 && row.is_header {
                let mut sep_spans: Vec<Span<'static>> = Vec::new();
                for col in 0..n_cols {
                    if col > 0 {
                        sep_spans.push(Span::styled("─┼─".to_string(), sep_style));
                    }
                    sep_spans.push(Span::styled(
                        "─".repeat(col_widths[col]),
                        sep_style,
                    ));
                }
                self.out.push(Line::from(sep_spans));
            }
        }
        self.need_blank = true;
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_block();
        self.out
    }
}

/// Inline token: either a styled word (no internal whitespace) or a styled
/// space. Used by the wrapper.
#[derive(Clone)]
struct Token {
    text: String,
    style: Style,
    is_space: bool,
}

fn spans_to_tokens(spans: Vec<Span<'static>>) -> Vec<Token> {
    let mut tokens = Vec::new();
    for sp in spans {
        let style = sp.style;
        let text = sp.content.into_owned();
        let mut chars = text.chars().peekable();
        let mut buf = String::new();
        let mut buf_is_space = false;
        let mut started = false;
        while let Some(c) = chars.next() {
            let is_space = c == ' ';
            if !started {
                buf.push(c);
                buf_is_space = is_space;
                started = true;
                continue;
            }
            if is_space != buf_is_space {
                tokens.push(Token {
                    text: std::mem::take(&mut buf),
                    style,
                    is_space: buf_is_space,
                });
                buf.push(c);
                buf_is_space = is_space;
            } else {
                buf.push(c);
            }
        }
        if started {
            tokens.push(Token {
                text: buf,
                style,
                is_space: buf_is_space,
            });
        }
    }
    tokens
}

/// Greedy word wrap into lines no wider than `width` cells.
fn wrap_tokens(tokens: Vec<Token>, width: usize) -> Vec<Vec<Span<'static>>> {
    let mut lines: Vec<Vec<Span<'static>>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut col = 0usize;
    for tok in tokens {
        let tw = tok.text.width();
        if tok.is_space {
            if cur.is_empty() {
                // Drop leading whitespace on a new line.
                continue;
            }
            cur.push(Span::styled(tok.text, tok.style));
            col += tw;
            continue;
        }
        // Word: if it doesn't fit, wrap (after trimming trailing spaces).
        if col > 0 && col + tw > width {
            while let Some(last) = cur.last() {
                if last.content.chars().all(|c| c == ' ') {
                    let trim_w = last.content.width();
                    cur.pop();
                    col = col.saturating_sub(trim_w);
                } else {
                    break;
                }
            }
            lines.push(std::mem::take(&mut cur));
            col = 0;
        }
        if tw > width && cur.is_empty() {
            // Single word too wide to fit: hard-break.
            let mut chunk = String::new();
            let mut chunk_w = 0usize;
            for ch in tok.text.chars() {
                let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if chunk_w + ch_w > width && !chunk.is_empty() {
                    cur.push(Span::styled(std::mem::take(&mut chunk), tok.style));
                    lines.push(std::mem::take(&mut cur));
                    chunk_w = 0;
                }
                chunk.push(ch);
                chunk_w += ch_w;
            }
            if !chunk.is_empty() {
                cur.push(Span::styled(chunk, tok.style));
                col = chunk_w;
            }
            continue;
        }
        cur.push(Span::styled(tok.text, tok.style));
        col += tw;
    }
    if !cur.is_empty() {
        lines.push(cur);
    }
    lines
}
