# GUI chrome — tab strips, panes, and the window

Extracted from `CLAUDE.md`, which now carries a summary and points here. This is
v2 (`crates/gui`): everything drawn around the grid rather than in it — the four
tab strips, the pane divider and padding, and the deliberate absence of a status
bar.

The recurring theme is that this display runs at **scale 1**, so every fractional
edge is a chance to antialias over a hairline. Several obvious-looking approaches
are recorded here as failures for that reason.

## Tab strips

**Every tab strip is built from one `State::tab_strip`** so they read as the same
control. There are four: the editor pane's *section* strip, the file strip below
it, and one per shell pane. The three holding things the user opened carry a
trailing `+`, and it means the same in each: one more of these. In a shell pane
that's a new shell; in the editor a new empty buffer, the same as `Cmd+N`. Opening
an existing file is `Cmd+O` — a `+` on a tab strip isn't "go find me a file". The
section strip has no `+`, because the set of sections is the app's.

**Shell tabs made PTY identity dynamic.** `ShellKey { pane, serial }` replaced the
fixed two-variant enum, and `subscription()` builds one `run_with(key, …)` per live
shell. Because the list is derived from state each frame, **adding a key spawns a
PTY and removing one stops it** — there are no imperative spawn/kill calls. Two
things that follow:

- `serial` is never reused. A recycled serial would let a closed shell's
  subscription be mistaken for a new one's and hand back the dead stream.
- `pty::run` **kills the child** before `wait`. The reader loop also exits when the
  subscription is dropped (tab closed), and there the shell is still alive — without
  the kill it would keep running unread and `wait` would block that thread forever.
  Verify with `ps -o ppid=` after quitting: orphans reparent to launchd (ppid 1).

`Cmd+T` spawns in the focused pane and `Cmd+W` closes there — see "Keybindings" in `CLAUDE.md`.
A shell pane is allowed to be empty — closing the last tab leaves it blank until
`+`, which is v1's behavior, not a missing case.

**Shell tabs are labelled by the shell's cwd basename**, following `cd`
(`core::proc::cwd_of`). v1 had two mechanisms and its own notes concluded only one
was reliable, so v2 ports only that one: polling the child process. OSC 7 parsing was
per-chunk and stateless, so a sequence split across two PTY reads was missed
entirely, and its percent-decoding mangled non-ASCII paths.

Two things about it:

- **The pid rides on `Event::Started`, not `Event::Attached`.** `Attached` fires
  *before* `spawn_command` — the spawn waits for a size — so there's no pid yet.
- **The cwd check is deliberately not throttled.** A 150ms throttle looked obviously
  right and silently broke it: a `cd` whose only output landed inside the window had
  its update *dropped*, and nothing re-checked afterwards. The PTY coalescer already
  bounds `Output` to roughly one message per frame, so the syscall runs at frame rate
  at worst.

The macOS offset in `cwd_of` (152 into `proc_vnodepathinfo`) is verified, not
inherited: a throwaway program searched the returned buffer for the known cwd. The
`our_own_cwd_is_readable` test keeps it honest — a label-only feature would otherwise
hide the syscall silently returning `None` forever.

**Tab markers** are `•` when the buffer is unsaved and `◇` when an external tool
touched it and you haven't looked since, matching v1 exactly: bright yellow
(`ansi_slot(11)`) and bright cyan (`ansi_slot(14)`).

They are **separate `text` widgets, not part of the label string**. Baking them
into the name would tint them with the tab's own active/inactive/hover color,
which defeats the point of having a color at all.

**The marker glyph has to survive a font that lacks it.** `◇` (U+25C7) is absent
from Envy Code R, and `text` defaults to `Shaping::Basic` — so it would draw as
*nothing*, and an invisible unreviewed marker is strictly worse than none, since
it silently reports "reviewed". `State::marker` consults `FontSpec::can_draw` and
shapes with `Advanced` only when needed, the same rule `GridView` applies per
cell. Verified on screen, not by inspection: the dirty dot's core pixel samples
`#fabd2f`, the theme's `bright_yellow`.

`Buffer::unreviewed` is set by a `--review` open and cleared by
`mark_active_reviewed` the moment the tab is made active — viewing is reviewing.
That's called from the paths where the *user* chose a tab (`select_tab`,
`cycle_tab`, a non-review open), not from every place `active` moves: closing or
reordering a tab shifts the index without anyone having read what's in it. A
review-open of the tab *already* on screen doesn't mark it, since the mark could
never be cleared without navigating away and back. Not persisted — v1 doesn't
either, because "unreviewed" is about this sitting, not the file.

**Tabs reorder by dragging** within their own strip, and the strip reorders
**live** as the pointer moves — the movement is the feedback, so there's no drop
marker to invent. Press begins the drag (and selects, as a click would); release
only ends the gesture and writes the session, so a drag across five tabs is one
file write rather than five. A plain click can't reorder, since nothing moves
until the pointer crosses into another tab.

**Live movement makes oscillation possible, and it is not hypothetical.** Drag a
narrow tab past a wide one: the swap puts the narrow tab where the wide one
began, leaving the pointer still inside the *wide* tab. The next pointer movement
fires that tab's hover and swaps back, and the two flip-flop for as long as the
mouse moves.

The fix is **direction, not geometry**: a tab moves forward into a tab ahead of it
only while the pointer travels right, and backward only while it travels left.
The rebound asks to move backward during a rightward drag, so it's refused, and
undoing a move requires actually reversing direction. That's the hysteresis a
midpoint rule would give, without needing to know where any midpoint is — which
matters because tab bounds aren't available in `update`.

Two things this rests on, both found by driving the real UI:

- **The decision runs on every pointer move, not when the pointer crosses into a
  tab.** A crossing publishes exactly one `on_enter`, and on the first one of a
  drag the direction is still unknown — deciding there meant the refused move was
  never retried and the tab simply never followed the pointer.
- **`on_exit` must only clear the hover it owns.** Widgets publish in tree order,
  so moving *left* onto a neighbour emits the neighbour's `on_enter` **before** the
  departed tab's `on_exit`; an unconditional `hovered_tab = None` then wiped the
  hover that had just been set. Rightward moves happened to emit them in the
  harmless order, so this surfaced as "dragging left does nothing" rather than as a
  hover bug. Hence `Message::TabExited(group, i)` carries which tab left.

**Hover styling is suppressed for the whole gesture**, and the dragged tab holds
the *active* look instead. The tabs slide under a stationary pointer, so lighting
up whichever one it happens to be over is a second highlight competing with the
one that matters, landing on tabs the pointer never deliberately visited. The
hover is still *tracked* throughout — it's what tells the drag where the pointer
is — only the styling is dropped. Measured: the tab under the pointer reads `dim`
(131) mid-drag and `foreground` (210) once released.

Direction comes from a strip-wide `mouse_area` reporting `p.x`, not from the
per-tab one: `mouse_area` gives `cursor.position_in(bounds)`, so a per-tab
position is relative to whichever tab is under the pointer and jumps at every
boundary — which reads as a direction reversal that never happened.

`move_item` is remove-then-insert, not swap: dragging a tab three places left
should slide the three it passes one step right. Because `to` indexes the list
*before* the removal, neither direction needs an adjustment. The dragged tab stays
active throughout, so the strip moves under a tab that keeps its identity.

**Buffer tabs** (`State::tab_bar`): `buffers: Vec<Arc<Mutex<Buffer>>>` plus an
`active` index. Built from `button` widgets in a `row`, so hover and hit-testing
come from iced — it's chrome, and its height is independent of the grid's row
pitch. `Cmd+1..9` jumps, `Cmd+Shift+[`/`]` and `Cmd+Tab` cycle, `Cmd+W` and
middle-click close. Every command-line path opens as a tab.

**Distinction is text color, not backgrounds**, and that's forced rather than
stylistic. Sixteen theme colors with no blending doesn't provide a "slightly
lighter than the background" to raise the active tab with — `black` equals
`background` in most dark schemes. Filling the strip with `dim` instead made
inactive tabs *invisible*, since their text was `dim` too. v1 reached the same
answer: active is `foreground`, inactive is `dim`, strip shares the pane
background.

Separators sit *between* tabs, so there's none after the trailing `+` — a rule
there would be dividing the `+` from empty space.

**Separators run the full height of the strip and never go through `cell`.** The
separator is a tab's side border, and the strip's bottom pixel is part of it — so a
separator that stops at `TAB_BAR_HEIGHT - TAB_BORDER` leaves the border visibly 1px
short at *both* bottom corners of *every* tab.

They also don't need to cover the underline: at a separator's column the rule and
the underline are the same colour, so a full-height rule hides the line by painting
over it identically. That is what makes the active tab's break read correctly — it
is bounded by two divider columns rather than by a stub of underline.

Two ways of handling the separator near the active tab were tried and both were
wrong, in ways worth remembering:

- **Removing it.** The active tab loses its side borders entirely, and because the
  row loses a child the whole strip shifts every time the selection moves. **The
  cell count must not depend on which tab is active.**
- **Making it cover, with its rule wrapped to the short height.** Keeps the layout
  stable, but the covering cell is full height while the rule is one pixel shorter,
  so the bottom pixel of the border becomes background — the missing corner above,
  now on every tab.

The strip's underline and the separators between tabs are `rule` widgets, not a
`Border`: `iced::Border` applies to all four sides at once, so there's no way to
ask it for "bottom only" or "right only". A 1px rule per edge is how you get a
single side. Both use `Palette::dim()`, the same slot as the pane divider and the
inactive line numbers, so every chrome line shares one color.

**The underline breaks under the active tab**, so that tab reads as joined to the
pane below it while every other tab and the empty space past them stay fenced off.

It is still **one rule**, not one per tab: a `stack!` puts a single full-width
`rule::horizontal` at the bottom of the strip and the tabs on top of it. A cell
either covers its slice or doesn't — the active tab is `Length::Fill` tall and
paints over the line, every other cell stops `TAB_BORDER` short and lets it
through. The empty space past the last tab needs no filler, because the rule
already spans the full width underneath.

**Two per-cell approaches were tried first and both were wrong**, which is worth
recording because each looked obviously correct:

1. **A `rule` inside each cell's column.** `rule::horizontal` is `width: Fill`, so
   every cell demanded the whole strip and the row split itself evenly between
   them — all the tabs came out the same width.
2. **A background-plus-padding border per cell** (outer container painted the line
   colour, content inset 1px at the bottom). This sized correctly but introduced
   **subpixel blur**: tab widths come from measured text and land on fractional
   positions, so where two cell backgrounds abut, rounding leaves a sliver of
   whatever is behind — and a line-coloured backdrop bled through those seams as
   hairlines, crisp at boundaries that happened to fall on whole pixels and blurry
   at the ones that didn't. It also filled the whole negative space with the line
   colour, because the filler's content painted no background over it.

**`snap: true` does nothing on this build, and that is worth knowing before
trusting it.** `renderer::Quad::snap` is consumed *only* by `iced_wgpu` — there
are zero references to it in `iced_tiny_skia`, `iced_graphics`, or
`iced_renderer`, and we build with tiny-skia. iced's `crisp` feature is no help
either: all it does is default that same dead field to `true`. The rule styles in
this file set `snap: true` because it is correct intent and costs nothing, not
because it protects anything.

What actually keeps a rule crisp is `Rule::draw` rounding **its own** position
(`bounds.x.round()` for a vertical, `bounds.y.round()` for a horizontal). Nothing
rounds anything else.

**Which is why cells don't paint.** This display runs at **scale 1** — a logical
pixel is a physical pixel, with no supersampling to hide anything. Tab widths come
from measured text, so cell edges land on fractional x, and a filled rect with a
fractional edge is antialiased across the neighbouring column — exactly where the
1px separator sits. Every per-cell background therefore painted over part of the
rule next to it, and the separators rendered as less than a full pixel. Only the
active tab paints a background now (it has to, to break the underline), so there
is one fractional fill in the strip instead of one per cell.

The general rule: **anything that must be a hairline is a `rule`, and nothing else
gets a fill it doesn't need** — at scale 1 every extra filled rect is a chance to
antialias over one.

**The tabs live in a horizontal `scrollable`, which is doing two jobs.** It
**clips** — nothing in iced clips a child to its parent by default, so once the
tabs were wider than the pane the strip drew straight over the pane beside it,
with editor tab names appearing on top of a shell. And it makes the overflow
reachable, which v1 had (`tabs_scroll`) and v2 had lost; the wheel over a strip
scrolls it. The scrollbar is suppressed to zero width, as in the Jira pane — a bar
under a 26px strip would be most of its height.

Because `tab_strip` is shared, all of this applies to all four strips at once —
the section strip, the file strip, and both shell panes.

Tab text uses `font.size` — the same size as the editor and the shells, so the
chrome doesn't read as a different app.

`close_tab` refuses to discard a dirty buffer and says so, and closing the last tab
leaves an empty untitled one — so `buf()` never has to handle an empty list.

**Tabs are not `button`s, deliberately.** `iced::widget::button` returns
`mouse::Interaction::Pointer` whenever it's hovered and has an `on_press`, and
that isn't reachable from a style function — so a `button` tab always shows the
hand cursor, which is wrong for a tab strip. `State::tab_strip` therefore builds
each tab as `mouse_area(container(text(..)))`: a `container` reports no
interaction, so the pointer stays the normal arrow, and `mouse_area` supplies
`on_press` / `on_middle_press` (close) / `on_enter` / `on_exit`. The cost is that
`button::Status::Hovered` is gone, so hover state lives in `State::hovered_tab`
(`Option<(TabGroup, usize)>`, with `usize::MAX` standing in for the `+` button).
If a tab ever becomes a `button` again, the hand cursor comes back with it.

## Panes and the window

**Pane chrome** (`PANE_PADDING` / `PANE_BORDER` in `main.rs`): each pane's content
is wrapped in a `container` carrying the padding, background, and border. The grid
derives rows/cols from its own layout bounds, so it shrinks to fit automatically —
nothing else needs to know the chrome exists.

**The divider between panes is the gap behind them, not a border on each.** Panes
carry no border at all. `pane_grid`'s `spacing` leaves a `PANE_DIVIDER`-wide gap,
and the container *behind* the grid is painted `Palette::dim()`, so that gap shows
through as one permanent line. Two reasons it's done this way rather than with
borders: a border outlines every pane (four sides, including the window edges),
and `pane_grid::Style` draws its own split line only on hover or drag with no
always-visible option.

`Palette::dim()` (the theme's `bright_black`) is the same slot the inactive line
numbers use, so chrome stays one family. It's also the only one of the 16 that
reliably reads against the background: in most dark schemes `black` *is* the
background — in Gruvbox both are `#282828` — so a divider drawn from it would be
invisible.

`hovered_split` and `picked_split` are overridden to width `0.0`, which iced draws
as a zero-area quad, i.e. nothing. The divider is permanent and the resize mouse
cursor already signals grabbability, so the highlight was redundant motion. Width
zero rather than a transparent color — transparency isn't a theme color, and
`theme_guard` enforces that. `hovered_region` keeps iced's default: it belongs to
pane *dragging*, which is a separate interaction.

Divider width is independent of the drag target — `on_resize`'s leeway is what you
grab, so a 1px line is still easy to hit.

The one thing padding does complicate: clicks in the inset band. The grid captures
clicks on cells, so a `mouse_area` around the padded container catches the rest
and emits `FocusPane` (focus moves, cursor doesn't). Without it the band would be
dead to clicks, which reads as the pane ignoring you.

**No status bar at all.** The window is the grid, edge to edge. The bottom row
exists only while a *prompt* is open, and a prompt is an input, not a message.
Throughput and memory counters are *not* UI: run with `SACRAMENT_METRICS=1` and
they go to stderr every 200 chunks. `Cmd+Shift+R` resets them.
`SACRAMENT_SPIKE_CMD='<cmd>'` auto-runs a command on attach, so a scripted perf
run needs no typing.

