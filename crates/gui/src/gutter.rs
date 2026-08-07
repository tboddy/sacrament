//! The gutter: line numbers and the fold chevron, as a real widget.
//!
//! v1 drew this as cells inside the same `Paragraph` as the text, which meant
//! `render_gutter`, `gutter_width`, and the click hit-test all had to agree on
//! the same column arithmetic by hand — a convention with no compiler behind it.
//! As a widget, iced's layout owns the width, so that class of drift is gone.
//!
//! ## What keeps it aligned with the text
//!
//! Two things, and only two:
//!
//! 1. **Row height is arithmetic, not measurement.** Both this and `GridView`
//!    derive it from the same [`FontSpec`] as `size * line_height`. No measuring
//!    means no possibility of the two disagreeing.
//! 2. **The row mapping has one producer.** `Buffer::gutter_rows` and
//!    `Buffer::visible_rows` come off the same walk, so a wrap continuation
//!    prints no number here *because* the grid drew a continuation there.
//!
//! Deliberately absent versus v1: the lint glyph and git change-bar columns. v2
//! doesn't do linting or git, so the layout is just `[number][space][chevron]`
//! plus a trailing space.

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{Quad, Style};
use iced::advanced::text::{self, Renderer as TextRenderer};
use iced::advanced::widget::Tree;
use iced::advanced::{Clipboard, Shell, Widget};
use iced::alignment;
use iced::{Element, Event, Length, Point, Rectangle, Size, mouse};

use std::sync::{Arc, Mutex};

use crate::buffer::{Buffer, FoldMark, GutterRow};
use crate::font::FontSpec;
use crate::palette::Palette;

/// Columns beyond the digits: one space, the chevron, one trailing space.
const PADDING_COLS: usize = 3;

pub struct Gutter<'a, Message> {
    /// Same `Arc` the grid's source holds. Both derive their rows from it at
    /// draw time rather than being handed a precomputed list, which is what
    /// keeps them in step without any plumbing between the two widgets.
    buffer: Arc<Mutex<Buffer>>,
    font: FontSpec,
    palette: &'a Palette,
    /// Emitted with a line index when its chevron is clicked.
    on_fold: Option<Box<dyn Fn(usize) -> Message + 'a>>,
    /// Pixels the content is shifted up by — must equal the grid's, or the numbers
    /// slide out of line with their text. See `GridView::offset`.
    offset: f32,
}

impl<'a, Message> Gutter<'a, Message> {
    pub fn new(buffer: Arc<Mutex<Buffer>>, font: FontSpec, palette: &'a Palette) -> Self {
        Self {
            buffer,
            font,
            palette,
            on_fold: None,
            offset: 0.0,
        }
    }

    /// Shift the numbers up by this many pixels, matching the grid beside it.
    pub fn offset(mut self, pixels: f32) -> Self {
        self.offset = pixels;
        self
    }

    pub fn on_fold(mut self, f: impl Fn(usize) -> Message + 'a) -> Self {
        self.on_fold = Some(Box::new(f));
        self
    }

    /// Digits reserved for the number, from the buffer's total line count — not
    /// from what's on screen, so the text column doesn't shift while scrolling.
    fn digits(&self) -> usize {
        self.buffer.lock().map(|b| b.number_width()).unwrap_or(1)
    }

    /// Total columns, matching v1's `digits + 3` with the overlay columns gone.
    pub fn cols(&self) -> usize {
        self.digits() + PADDING_COLS
    }

}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for Gutter<'_, Message>
where
    Renderer: TextRenderer,
    Renderer::Font: From<iced::Font>,
{
    fn size(&self) -> Size<Length> {
        // Width is content-driven; height fills so rows line up with the grid.
        Size::new(Length::Shrink, Length::Fill)
    }

    fn layout(
        &mut self,
        _tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let cw = crate::font::advance_width(renderer, &self.font);
        let width = cw * self.cols() as f32;
        layout::Node::new(Size::new(width, limits.max().height))
    }

    fn update(
        &mut self,
        _tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        let Some(on_fold) = &self.on_fold else {
            return;
        };
        if !matches!(
            event,
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
        ) {
            return;
        }
        let bounds = layout.bounds();
        let Some(point) = cursor.position_in(bounds) else {
            return;
        };
        let cw = crate::font::advance_width(renderer, &self.font);
        let ch = self.font.cell_height();
        // The chevron owns one column, `digits + 1`. Clicking a line number
        // shouldn't fold — that's a click on the number, and a gutter that folds
        // wherever you touch it is one you can't click without consequence.
        let column = (point.x / cw) as usize;
        if column != self.digits() + 1 {
            return;
        }
        let row = (point.y / ch) as usize;
        let rows = (bounds.height / ch).ceil() as usize;
        let Ok(buffer) = self.buffer.lock() else {
            return;
        };
        let Some(gutter_row) = buffer.gutter_rows(rows).get(row).copied() else {
            return;
        };
        // Only a row that actually shows a chevron. `number` is the line, and is
        // `None` on a wrap continuation — which never carries one.
        let (Some(number), Some(_)) = (gutter_row.number, gutter_row.fold) else {
            return;
        };
        drop(buffer);
        shell.publish(on_fold(number - 1));
        // Claimed, so the click doesn't also reach whatever is behind.
        shell.capture_event();
    }

    fn draw(
        &self,
        _tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };
        let cw = crate::font::advance_width(renderer, &self.font);
        let ch = self.font.cell_height();
        let digits = self.digits();
        // Derive rows from this widget's own height. The grid does the same from
        // its own; both are Fill in the same row, and row pitch is arithmetic
        // rather than measured, so the two counts agree by construction.
        let visible = ((bounds.height / ch).floor() as usize).max(1);
        // The grid's sub-row scroll offset, applied identically here. This is the
        // third thing gutter/text alignment now depends on, alongside the shared
        // row pitch and the single row-mapping producer — which is exactly why the
        // value lives in app state and is handed to both widgets, rather than being
        // recomputed or kept in either one's widget state.
        let shift = self.offset.round();
        let visible = if shift > 0.0 { visible + 1 } else { visible };
        let rows: Vec<GutterRow> = self
            .buffer
            .lock()
            .map(|b| b.gutter_rows(visible))
            .unwrap_or_default();

        renderer.fill_quad(
            Quad {
                bounds,
                border: iced::Border::default(),
                shadow: iced::Shadow::default(),
                snap: true,
            },
            self.palette.background,
        );

        let fg = self.palette.foreground;
        for (i, row) in rows.iter().enumerate() {
            let y = bounds.y + i as f32 * ch - shift;
            if y >= bounds.y + bounds.height {
                break;
            }

            if let Some(n) = row.number {
                // Right-aligned within the digit field, same as v1's `{:>width$}`.
                let label = format!("{n:>width$}", width = digits);
                // Two real theme colors, no blending: the cursor's line reads
                // at full foreground, every other number sits in the theme's
                // muted slot.
                let color = if row.is_cursor_line {
                    fg
                } else {
                    self.palette.dim()
                };
                renderer.fill_text(
                    text::Text {
                        content: label,
                        bounds: Size::new(cw * digits as f32, ch),
                        size: self.font.size.into(),
                        line_height: text::LineHeight::Relative(self.font.line_height),
                        font: self.font.font.into(),
                        align_x: text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping: text::Shaping::Basic,
                        wrapping: text::Wrapping::None,
                    },
                    Point::new(bounds.x, y),
                    color,
                    clip,
                );
            }

            // Column `digits + 1` is the fold chevron.
            if let Some(mark) = row.fold {
                // Pointing down when the block is open, right when it's closed —
                // the arrow shows where the hidden lines would go, which is the
                // convention every editor uses.
                let glyph = match mark {
                    FoldMark::Open => "\u{25be}",
                    FoldMark::Closed => "\u{25b8}",
                };
                // The active line's chevron is as bright as its number; the rest
                // stay `dim`, so an open block doesn't compete with the text.
                let color = if row.is_cursor_line {
                    self.palette.foreground
                } else {
                    self.palette.dim()
                };
                // Fall back only when the font lacks the glyph — the same rule
                // the grid applies per cell. Neither arrow is in every monospace
                // face, and an invisible chevron is worse than none: it would
                // hide that a block can be folded at all.
                let shaping = if glyph.chars().all(|c| self.font.can_draw(c)) {
                    text::Shaping::Basic
                } else {
                    text::Shaping::Advanced
                };
                renderer.fill_text(
                    text::Text {
                        content: glyph.to_string(),
                        bounds: Size::new(cw, ch),
                        size: self.font.size.into(),
                        line_height: text::LineHeight::Relative(self.font.line_height),
                        font: self.font.font.into(),
                        align_x: text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping,
                        wrapping: text::Wrapping::None,
                    },
                    Point::new(bounds.x + cw * (digits + 1) as f32, y),
                    color,
                    clip,
                );
            }
        }
    }
}

impl<'a, Message, Theme, Renderer> From<Gutter<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: TextRenderer + 'a,
    Renderer::Font: From<iced::Font>,
{
    fn from(w: Gutter<'a, Message>) -> Self {
        Element::new(w)
    }
}
