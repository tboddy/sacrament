//! The custom iced widget that draws a cell grid.
//!
//! Source-agnostic: it draws a [`GridSource`], so the shell panes and the editor
//! pane are literally the same widget with different content behind them. That
//! was the architectural bet; this is where it's cashed.
//!
//! Two decisions carry the performance, both measured during the spike:
//!
//! 1. **Run batching.** One `fill_text` per cell would mean one shaped text
//!    object per cell per frame — thousands. Adjacent cells agreeing on color
//!    and emphasis coalesce into a single run instead, so a screenful costs tens
//!    of draw calls. Same trick as v1's `build_display_line`.
//! 2. **`Shaping::Basic`.** No font fallback, no complex-script shaping. We
//!    control the font, and iced documents `Advanced` as expensive. The cost is
//!    that an unresolvable font family draws nothing, which is why `font.rs`
//!    validates the family up front.
//!
//! Cell metrics are *measured* from the font rather than assumed from a
//! monospace ratio, because every position downstream — background quads, the
//! caret, mouse hit-testing — derives from them, and a guessed ratio drifts
//! across sizes and faces.

use std::time::{Duration, Instant};

use iced::advanced::layout::{self, Layout};
use iced::advanced::renderer::{Quad, Style};
use iced::advanced::text::{self, Renderer as TextRenderer};
use iced::advanced::widget::{Tree, tree};
use iced::advanced::{Clipboard, Shell, Widget};
use iced::alignment;
use iced::{Color, Element, Event, Length, Point, Rectangle, Size, mouse};

use crate::font::FontSpec;
use crate::grid::{Cell, GridSource};
use crate::palette::Palette;

/// Window within which a second press at the same cell counts as a double click.
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

/// What the grid reports about the mouse. One enum rather than four callbacks —
/// the app needs to correlate press/drag/release as a single gesture anyway.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum GridMouse {
    /// `count` is 1 for a single click, 2 for a double click. `shift` is the
    /// modifier state at press time — see `State::modifiers` for why the widget
    /// has to remember it rather than read it off the event.
    Press {
        row: usize,
        col: usize,
        count: u8,
        shift: bool,
    },
    /// Cell under the cursor while the button is held. Clamped to the grid, so
    /// dragging past an edge extends to that edge rather than stopping.
    Drag { row: usize, col: usize },
    Release,
    /// Positive scrolls toward earlier content (wheel up), matching how both a
    /// buffer and a terminal scrollback are indexed.
    ///
    /// `dy` is in **pixels**, not rows, and that is what makes scrolling smooth:
    /// the app keeps the sub-row remainder and the renderer draws at that offset,
    /// instead of the widget truncating every event to a whole row and the view
    /// stepping a row at a time. `cols` stays whole — horizontal is already
    /// column-quantised and the widget owns the only cell-width measurement.
    Scroll { dy: f32, cols: i32 },
}

#[derive(Default)]
struct State {
    /// Measured `(cell_width, cell_height)`, computed once — the font is fixed
    /// for the widget's lifetime.
    metrics: Option<(f32, f32)>,
    /// Last reported `(rows, cols)`, so only real changes are published.
    reported: Option<(usize, usize)>,
    /// True between press and release, so `CursorMoved` knows it's a drag. Kept
    /// in widget state rather than the app so a drag can't be confused across
    /// panes.
    dragging: bool,
    /// For double-click detection.
    last_press: Option<(Instant, usize, usize)>,
    /// Modifier state, tracked because iced delivers it as its own event.
    ///
    /// A `mouse::Event::ButtonPressed` carries only the button — there is no
    /// modifier field on it — so a shift-click is recognisable only by remembering
    /// the last `keyboard::Event::ModifiersChanged`. Held per widget rather than in
    /// the app because the app's key subscription sees modifiers *with a key
    /// press*, which is a different question from "what was held during that
    /// click".
    modifiers: iced::keyboard::Modifiers,
    /// Sub-column scroll carried between events, for the same reason as
    /// `scroll_carry`: a trackpad's sideways movement is fractions of a column.
    scroll_carry_x: f32,
    // No vertical carry: `dy` leaves here in pixels and the app accumulates it,
    // because the remainder has to be *drawn* and the gutter has to see the same
    // value. Widget state can't be read by a sibling widget.
}

pub struct GridView<'a, Message> {
    /// Owned rather than borrowed: a source holds an `Arc<Mutex<_>>` and locks
    /// inside `fill`, because a `MutexGuard` taken in `view()` can't outlive it.
    source: Box<dyn GridSource + 'a>,
    palette: &'a Palette,
    font: FontSpec,
    /// Emitted when the widget's pixel bounds imply a different row/col count.
    /// The widget can't act on it — it has no handle to a PTY or buffer, and
    /// `layout` has no `Shell` — so it reports and the app decides.
    on_resize: Box<dyn Fn(usize, usize) -> Message + 'a>,
    /// Mouse gestures, in cell coordinates. The widget reports; what a press or
    /// drag *means* (focus, caret, selection) is the app's decision.
    on_mouse: Option<Box<dyn Fn(GridMouse) -> Message + 'a>>,
    /// Pixels the content is shifted **up** by — the sub-row remainder of the
    /// scroll position, so scrolling moves by pixels rather than by whole rows.
    ///
    /// Supplied by the app rather than kept here, because the gutter is a separate
    /// widget that must shift by exactly the same amount or the line numbers
    /// desync from their text, and one widget cannot read another's state.
    ///
    /// Rounded to a whole pixel when drawing: this display is 1x, so a glyph at a
    /// fractional y would be blurry. A whole pixel is still ~16x finer than a row.
    offset: f32,
}

impl<'a, Message> GridView<'a, Message> {
    pub fn new(
        source: impl GridSource + 'a,
        palette: &'a Palette,
        font: FontSpec,
        on_resize: impl Fn(usize, usize) -> Message + 'a,
    ) -> Self {
        Self {
            source: Box::new(source),
            palette,
            font,
            on_resize: Box::new(on_resize),
            offset: 0.0,
            on_mouse: None,
        }
    }

    pub fn on_mouse(mut self, f: impl Fn(GridMouse) -> Message + 'a) -> Self {
        self.on_mouse = Some(Box::new(f));
        self
    }

    /// Shift the content up by this many pixels. See [`GridView::offset`].
    pub fn offset(mut self, pixels: f32) -> Self {
        self.offset = pixels;
        self
    }

    /// Where this character's glyph has to come from.
    fn glyphs_for(&self, c: char) -> Glyphs {
        if self.font.can_draw(c) {
            return Glyphs::Configured;
        }
        match self.font.fallback_family(c) {
            Some(name) => Glyphs::Family(name),
            None => Glyphs::System,
        }
    }

    /// The font and shaping mode a run is drawn with.
    ///
    /// `Advanced` is the only shaping that consults system fonts, and it's the
    /// expensive one — so it's reserved for the cells nothing else can draw.
    fn draw_font(&self, glyphs: Glyphs, bold: bool, italic: bool) -> (iced::Font, text::Shaping) {
        let font = self.font.variant(bold, italic);
        match glyphs {
            Glyphs::Configured => (font, text::Shaping::Basic),
            Glyphs::Family(name) => (
                iced::Font {
                    family: iced::font::Family::Name(name),
                    ..font
                },
                text::Shaping::Basic,
            ),
            Glyphs::System => (font, text::Shaping::Advanced),
        }
    }
}

/// Rows moved per notch of a stepped mouse wheel. Pixel deltas from a trackpad
/// are *not* scaled by this — they already carry a real distance.
const ROWS_PER_NOTCH: f32 = 3.0;

/// Where a cell's glyph comes from.
///
/// Only [`Glyphs::Configured`] batches: the other two are per-cell, because a
/// substituted face's advance width isn't ours and a multi-cell run drawn in one
/// would drift progressively out of the grid.
#[derive(Clone, Copy, PartialEq)]
enum Glyphs {
    /// The configured font has the character.
    Configured,
    /// It doesn't, but a monochrome family in the fallback chain does. Drawn with
    /// `Shaping::Basic` in that family — deliberately *not* left to cosmic-text's
    /// own fallback, which would reach `Apple Color Emoji` and turn a record
    /// button or a warning sign into a cartoon. See `font::Fallback`.
    Family(&'static str),
    /// Nothing in the chain has it, so cosmic-text picks. This is the path a
    /// genuine emoji takes — no text font on macOS has `🔴` — and the one for
    /// scripts the chain doesn't reach.
    System,
}

/// One coalesced run of cells sharing color, emphasis and glyph source.
struct Run {
    text: String,
    glyphs: Glyphs,
    col: usize,
    fg: Color,
    bold: bool,
    italic: bool,
}

/// `Quad` doesn't derive `Default`, and every quad here is a plain filled rect.
/// `snap: true` aligns to the pixel grid, which stops adjacent cell backgrounds
/// showing seams at fractional cell widths.
fn cell_quad(bounds: Rectangle) -> Quad {
    Quad {
        bounds,
        border: iced::Border::default(),
        shadow: iced::Shadow::default(),
        snap: true,
    }
}

impl<Message, Theme, Renderer> Widget<Message, Theme, Renderer> for GridView<'_, Message>
where
    Renderer: TextRenderer,
    Renderer::Font: From<iced::Font>,
{
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<State>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(State::default())
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let measured = (
            crate::font::advance_width(renderer, &self.font),
            self.font.cell_height().max(1.0),
        );
        let state = tree.state.downcast_mut::<State>();
        state.metrics.get_or_insert(measured);
        layout::Node::new(limits.max())
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        _renderer: &Renderer,
        _clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        _viewport: &Rectangle,
    ) {
        // Grid size comes from real layout bounds. This lives in `update` rather
        // than `layout` because it's the only hook with both a layout and a
        // `Shell` to publish through.
        let state = tree.state.downcast_mut::<State>();
        let Some((cw, ch)) = state.metrics else {
            return;
        };
        let bounds = layout.bounds();
        let cols = ((bounds.width / cw).floor() as usize).max(1);
        let rows = ((bounds.height / ch).floor() as usize).max(1);
        if state.reported != Some((rows, cols)) {
            state.reported = Some((rows, cols));
            shell.publish((self.on_resize)(rows, cols));
        }

        // Tracked before the mouse-only early return, so the state is current by
        // the time a press arrives.
        if let Event::Keyboard(iced::keyboard::Event::ModifiersChanged(mods)) = event {
            state.modifiers = *mods;
        }

        let Some(on_mouse) = &self.on_mouse else {
            return;
        };
        let Event::Mouse(mouse_event) = event else {
            return;
        };

        // Cell under a point, clamped into the grid. Used for drags too, which is
        // why it takes an absolute position rather than requiring containment:
        // dragging past an edge should extend to that edge, not freeze.
        let cell_at = |p: iced::Point| -> (usize, usize) {
            let col = ((p.x - bounds.x) / cw).floor().clamp(0.0, (cols - 1) as f32) as usize;
            let row = ((p.y - bounds.y + self.offset.round()) / ch)
                .floor()
                .clamp(0.0, (rows - 1) as f32) as usize;
            (row, col)
        };

        match mouse_event {
            mouse::Event::ButtonPressed(mouse::Button::Left) => {
                let Some(pos) = cursor.position_in(bounds) else {
                    return;
                };
                let (row, col) = cell_at(iced::Point::new(bounds.x + pos.x, bounds.y + pos.y));
                let now = Instant::now();
                let count = match state.last_press {
                    Some((at, r, c))
                        if r == row && c == col && now.duration_since(at) < DOUBLE_CLICK =>
                    {
                        2
                    }
                    _ => 1,
                };
                state.last_press = Some((now, row, col));
                state.dragging = true;
                shell.publish(on_mouse(GridMouse::Press {
                    row,
                    col,
                    count,
                    shift: state.modifiers.shift(),
                }));
                // Claim it so pane_grid doesn't read the press as a splitter grab.
                shell.capture_event();
            }
            mouse::Event::CursorMoved { .. } if state.dragging => {
                let Some(p) = cursor.position() else {
                    return;
                };
                let (row, col) = cell_at(p);
                shell.publish(on_mouse(GridMouse::Drag { row, col }));
                shell.capture_event();
            }
            mouse::Event::ButtonReleased(mouse::Button::Left) if state.dragging => {
                state.dragging = false;
                shell.publish(on_mouse(GridMouse::Release));
                shell.capture_event();
            }
            mouse::Event::WheelScrolled { delta } => {
                if cursor.position_in(bounds).is_none() {
                    return;
                }
                // The two delta kinds mean genuinely different things and must
                // not be scaled alike. A wheel notch is one `Lines` unit and
                // should move several rows; a trackpad reports the distance the
                // finger actually travelled, which is already a real measurement
                // and only needs converting to rows. Multiplying that by the
                // notch factor as well made trackpad scrolling fly.
                let (rows, columns) = match delta {
                    // A notch is a stepped unit, so it becomes a distance here;
                    // a pixel delta already *is* one.
                    mouse::ScrollDelta::Lines { x, y } => {
                        (*y * ROWS_PER_NOTCH * ch, *x * ROWS_PER_NOTCH)
                    }
                    mouse::ScrollDelta::Pixels { x, y } => (*y, x / cw),
                };
                // Only the horizontal remainder is carried here. The vertical one
                // goes out as pixels for the app to accumulate and the renderer to
                // draw.
                state.scroll_carry_x += columns;
                let whole_x = state.scroll_carry_x.trunc();
                state.scroll_carry_x -= whole_x;
                if rows != 0.0 || whole_x != 0.0 {
                    shell.publish(on_mouse(GridMouse::Scroll {
                        dy: rows,
                        cols: whole_x as i32,
                    }));
                }
                // Captured even when the carry hasn't reached a whole row yet:
                // the event was for this pane either way.
                shell.capture_event();
            }
            _ => {}
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        _style: &Style,
        layout: Layout<'_>,
        _cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let state = tree.state.downcast_ref::<State>();
        let Some((cw, ch)) = state.metrics else {
            return;
        };
        let bounds = layout.bounds();
        let Some(clip) = bounds.intersection(viewport) else {
            return;
        };

        renderer.fill_quad(cell_quad(bounds), self.palette.background);

        let cols = ((bounds.width / cw).floor() as usize).max(1);
        let rows = ((bounds.height / ch).floor() as usize).max(1);

        // Whole pixels: at 1x a glyph drawn at a fractional y is blurry, and a
        // pixel is already ~16x finer than the row this used to step by.
        let shift = self.offset.round();
        // One row more than fits whenever the content is mid-row, so the bottom
        // shows the next line coming in rather than a band of background. Every
        // `fill_text` and quad below is clipped to `clip`, so the partial rows at
        // both ends are trimmed by the renderer.
        let drawn = if shift > 0.0 { rows + 1 } else { rows };

        // Allocated per frame. A persistent scratch would need mutable widget
        // state during `draw`, which iced doesn't offer; at a few dozen rows the
        // allocation is well under the draw cost it would save.
        let mut grid: Vec<Vec<Cell>> = Vec::with_capacity(drawn);
        self.source.fill(self.palette, drawn, cols, &mut grid);

        // Background spans: one quad per contiguous same-color run, skipping the
        // default (already covered by the pane background above).
        for (row_idx, cells) in grid.iter().enumerate() {
            let y = bounds.y + row_idx as f32 * ch - shift;
            let mut i = 0usize;
            while i < cells.len() {
                let bg = cells[i].bg;
                if bg == self.palette.background {
                    i += 1;
                    continue;
                }
                let start = i;
                while i < cells.len() && cells[i].bg == bg {
                    i += 1;
                }
                renderer.fill_quad(
                    cell_quad(Rectangle {
                        x: bounds.x + start as f32 * cw,
                        y,
                        width: (i - start) as f32 * cw,
                        height: ch,
                    }),
                    bg,
                );
            }
        }

        // Text runs, coalesced by color + emphasis.
        for (row_idx, cells) in grid.iter().enumerate() {
            let y = bounds.y + row_idx as f32 * ch - shift;
            let mut runs: Vec<Run> = Vec::new();
            for (col, cell) in cells.iter().enumerate() {
                if cell.is_blank() {
                    continue;
                }
                // A glyph the font doesn't have would draw as nothing under
                // `Shaping::Basic` — no fallback is the price of the cheap path.
                // Those cells get their own run and a substitute below.
                let glyphs = self.glyphs_for(cell.c);
                let extends = glyphs == Glyphs::Configured
                    && runs.last().is_some_and(|r| {
                        r.glyphs == Glyphs::Configured
                            && r.fg == cell.fg
                            && r.bold == cell.bold
                            && r.italic == cell.italic
                            && r.col + r.text.chars().count() == col
                    });
                if extends {
                    // `extends` guarantees a last element.
                    runs.last_mut().unwrap().text.push(cell.c);
                } else {
                    runs.push(Run {
                        text: cell.c.to_string(),
                        glyphs,
                        col,
                        fg: cell.fg,
                        bold: cell.bold,
                        italic: cell.italic,
                    });
                }
            }
            for run in runs {
                let (font, shaping) =
                    self.draw_font(run.glyphs, run.bold, run.italic);
                let width = run.text.chars().count() as f32 * cw;
                renderer.fill_text(
                    text::Text {
                        content: run.text,
                        bounds: Size::new(width, ch),
                        size: self.font.size.into(),
                        line_height: text::LineHeight::Relative(self.font.line_height),
                        font: font.into(),
                        align_x: text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping,
                        wrapping: text::Wrapping::None,
                    },
                    Point::new(bounds.x + run.col as f32 * cw, y),
                    run.fg,
                    clip,
                );
            }
        }

        if let Some((crow, ccol)) = self.source.cursor()
            && crow < rows
            && ccol < cols
        {
            let x = bounds.x + ccol as f32 * cw;
            // Same shift as the text, or the caret drifts off its own glyph while
            // the view sits between rows.
            let y = bounds.y + crow as f32 * ch - shift;
            renderer.fill_quad(
                cell_quad(Rectangle {
                    x,
                    y,
                    width: cw,
                    height: ch,
                }),
                self.palette.cursor,
            );

            // The caret covers its whole cell, so the glyph is redrawn on top in
            // the background color — reverse video, the way a terminal does it.
            // Blending the caret to translucency would be easier and would also
            // invent a color that isn't in the theme.
            //
            // No cell means the caret sits past the end of a line, which draws as
            // a solid block. That's correct, and matches the terminal pane.
            if let Some(cell) = grid.get(crow).and_then(|r| r.get(ccol))
                && !cell.is_blank()
            {
                let (font, shaping) =
                    self.draw_font(self.glyphs_for(cell.c), cell.bold, cell.italic);
                renderer.fill_text(
                    text::Text {
                        content: cell.c.to_string(),
                        bounds: Size::new(cw, ch),
                        size: self.font.size.into(),
                        line_height: text::LineHeight::Relative(self.font.line_height),
                        font: font.into(),
                        align_x: text::Alignment::Left,
                        align_y: alignment::Vertical::Top,
                        shaping,
                        wrapping: text::Wrapping::None,
                    },
                    Point::new(x, y),
                    self.palette.background,
                    clip,
                );
            }
        }
    }
}

impl<'a, Message, Theme, Renderer> From<GridView<'a, Message>>
    for Element<'a, Message, Theme, Renderer>
where
    Message: 'a,
    Theme: 'a,
    Renderer: TextRenderer + 'a,
    Renderer::Font: From<iced::Font>,
{
    fn from(w: GridView<'a, Message>) -> Self {
        Element::new(w)
    }
}
