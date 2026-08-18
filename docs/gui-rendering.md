# GUI rendering, terminal and PTY

Extracted from `CLAUDE.md`, which now carries a summary and points here. This is
v2 (`crates/gui`): how cells reach the screen, how the terminal is driven, and how
the PTY is plumbed through iced's subscriptions.

Almost every entry below is a bug that was diagnosed once, at cost. Read the
relevant one before changing pixel positioning, block characters, terminal
resize, mouse gestures, or shell spawning.

## Module notes

- `grid_view.rs` — the custom `iced::advanced::Widget`. Two decisions carry the
  performance: **run batching** (adjacent cells sharing fg+flags coalesce into
  one `fill_text`, so a screen costs tens of draw calls instead of thousands —
  the same trick as v1's `build_display_line`) and **`Shaping::Basic`** (no font
  fallback, no complex-script shaping; we control the font, and iced documents
  `Advanced` as expensive). Cell metrics are *measured* from the font once via
  `Paragraph::min_bounds` over a repeated glyph, not derived from an assumed
  monospace ratio, because every position downstream (background quads, cursor,
  eventual mouse hit-testing) is computed from them.

  **Every drawn position goes through `col_x` / `row_y`, which round.** Neither
  the advance (`size * 0.537`) nor the row pitch (`size * line_height`) is a whole
  number of pixels at any usable size, so a cell boundary lands mid-pixel — and
  two shapes meeting there each contribute partial coverage, which source-over
  composites *sequentially* rather than summing: `0.5 + 0.5(1 - 0.5)` is 0.75, so
  a quarter of the background shows through as a hairline. At scale 1 there is no
  supersampling to hide it, and because the fractional part accumulates across the
  grid it appears in slow bands rather than uniformly, which reads as a rendering
  glitch rather than as arithmetic.

  Rounding a **shared** boundary fixes it by construction: cell `k`'s right edge
  and cell `k+1`'s left edge are the same expression, so they cannot disagree.
  Cells come out 7px or 8px wide instead of a uniform 7.5. That is invisible for a
  solid fill, and for glyphs it's an improvement — a glyph on a fractional
  baseline is blurry at 1x.

  **The `bounds` handed to `fill_text` is the run's natural advance width, not the
  distance between its snapped ends.** Snapping can leave that distance a pixel
  short of the content, and a text bounds a pixel short **drops the last glyph
  onto a second line inside the same text object** — which lands a row down the
  screen. `Wrapping::None` does not prevent it. The symptom is unmistakable once
  seen and baffling until then: `struct Foo` renders as `struc  Fo` with a stray
  `t` on the line below, on roughly half the runs, so a syntax-highlighted screen
  comes apart into confetti. A pixel of slack is added on top: with left/top
  alignment and no wrapping this bounds governs only overflow, and the real
  clipping is `clip`.
- `blocks.rs` — **U+2580..259F are drawn as rectangles, not glyphs.** The seam
  above is worst where it matters most: a run of `█` is *supposed* to be one
  unbroken bar, and no font can make it one, because the gap is between the glyphs
  rather than inside them. Envy Code R already gives its blocks a 6-unit
  horizontal overhang for exactly this and it isn't close to enough. Every serious
  terminal (Alacritty, kitty, WezTerm, Ghostty) draws these procedurally.

  `blocks::rects(c)` is pure geometry — fractions of a cell — and `blocks::edge`
  maps a fraction onto a pair of already-snapped boundaries, leaving the endpoints
  untouched so an edge that *is* a cell boundary keeps the exact value its
  neighbour will use. Four things about it:

  - **Snapping is what removes the seam; merging is only an optimisation.**
    Adjacent cells abut exactly whether or not they're coalesced, so
    `merges_horizontally` exists to turn an 80-cell bar into one quad, not to make
    it correct. It answers `Some` only for a single full-width rect, since that's
    the only shape that tiles into one rectangle.
  - **The shades `░▒▓` (U+2591..2593) stay glyphs, deliberately.** They're 25/50/75%
    stipple, so a rect version needs either a real dot pattern or a blended
    colour — and a blend is a colour the user's `[theme]` doesn't contain, which
    `theme_guard` fails on. They also seam far less visibly, not being solid.
  - **Returning `Some` is the run-breaker.** The text pass skips any cell with
    rects, and because extending a run requires contiguous columns, skipping breaks
    the run for free — the same mechanism blanks already rely on.
  - **The caret redraws rects, not a glyph.** Reverse video over a block means
    painting its shape back in `background`; drawing the glyph instead would lose
    the shape, and drawing nothing would leave a solid cursor block.

  A thin part of a small cell can round to zero, so every quad is floored at 1px —
  a vanished eighth is worse than one drawn a pixel wide. Tests are pure geometry:
  `▀`+`▄` tile to exactly `█`, the eighths step evenly to 1.0, no character's rects
  overlap or leave the cell, and `right(k) == left(k+1)` for a fractional advance.

  Free with it: fonts that lack these glyphs now render them correctly anyway
  (Envy Code R covers only 48 of the 160 codepoints in U+2500..259F). **Not**
  covered — box drawing U+2500..257F, which still seams at its joins, and braille
  U+2800..28FF.
- `term.rs` — `alacritty_terminal::Term` replacing v1's `vt100::Parser`. More of
  the VT spec, damage tracking, and `renderable_content()` already applies the
  scrollback display offset.

  **Shell selection uses alacritty's own `Selection`**, not a hand-rolled one, and
  that's load-bearing: it works in *grid* coordinates, so a selection survives
  scrolling, and `SelectionType::{Semantic, Lines}` give word and line selection
  for free on double and triple click. `selection_to_string()` handles the grid
  walk including wrapped lines and scrollback. `grid_point` converts a viewport
  cell to a grid `Point` by subtracting `display_offset` — the one place the two
  coordinate systems meet.

  `grid_point` clamps the row to `last_content_row()`, so dragging into the blank
  region below a short prompt stops at the content instead of selecting a
  screenful of nothing — matching the editor, where `visible_rows` simply stops at
  the last line. A terminal grid is always full-height, so unlike the editor there
  is no natural end of content to fall off; it has to be found.

  `Size::total_lines()` returns `screen_lines()`, mirroring `alacritty_terminal`'s
  own `TermSize`. Reporting `rows + SCROLLBACK` there double-counts, since
  scrollback depth is configured on `Term` via `Config::scrolling_history`.

  **`Ctrl+C` in a shell must stay SIGINT**, which the Cmd-only binding scheme
  gives for free: nothing in the app claims plain `Ctrl` at all, so copying is
  `Cmd+C` and every control code reaches the PTY untouched. See "Keybindings".

  **`Shift+click` opens a URL** (`Terminal::url_at` → `open_url`), the terminal
  convention, and it falls through to a normal selection when there's no URL under
  the pointer so the gesture is never dead. Three things it gets right:

  - **It reads the whole logical line, not the screen row.** A URL long enough to
    be worth clicking is exactly the kind that wraps, so scanning one row would
    find `https://example.com/a` and miss the `/b/c` that continued below.
    `WRAPLINE` on a row's last cell joins them, and the walk follows it both ways.
  - **Only `http`/`https` are ever returned, and that's a security boundary.**
    macOS `open` launches a registered handler for *any* scheme, and terminal
    output is routinely attacker-influenced — any file you `cat`, any repo you
    clone. The allowlist means the worst a hostile line can do is open a web page.
    `file://`, `vscode://` and friends are refused, with a test pinning it.
  - **It does not use `grid_point`'s row clamping.** That clamp is right for a drag
    (pulling below a short prompt should stop at the content) and wrong here: it
    made a click on empty space resolve to the last line's link.

  Trailing sentence punctuation and wrapping brackets are peeled off the candidate
  — `see https://example.com/x.` must not include the full stop — but only from the
  *end*, since `?`, `=` and `#` are ordinary inside a query string.

  **Modifier state is tracked in `GridView::State`**, because iced's
  `mouse::Event::ButtonPressed` carries no modifiers; a shift-click is only
  recognisable by remembering the last `keyboard::Event::ModifiersChanged`. That's
  why `GridMouse::Press` carries `shift`.
- `pty.rs` — the non-obvious plumbing. `Subscription::run_with` takes a bare
  `fn(&D) -> S` with no captures, so the PTY must be built *inside* the stream,
  which leaves the app with no way to write to it. The stream's first item
  (`Event::Attached`) therefore hands a `Handle` back out. Reader bytes cross the
  thread→async boundary via `futures::channel::mpsc::unbounded`, whose receiver
  *is* a `Stream` — so output is genuinely push-driven, unlike v1's
  `event::poll(20ms)`.

  **The shell is not spawned until the real pane size is known.** `openpty` runs,
  `Event::Attached` hands the input side out, and only after the first
  `Msg::Resize` arrives (or `SIZE_WAIT` elapses) does `spawn_command` run.

  This is not tidiness — spawning first is visibly broken. The grid can only
  report its size after layout, which is after the PTY exists, so the shell drew
  its prompt at the 24×80 placeholder and then took a `SIGWINCH`. zsh's redraw
  scattered fragments of the prompt across the row and left a reverse-video `%`
  behind (its partial-line marker). Writes arriving before the size are stashed and
  replayed rather than dropped.

  The bound on the wait matters too: an unconditional wait would hang forever if a
  size never came. A shell at the placeholder size beats no shell.

  **The measured size is a property of the pane, not of the shell**
  (`ShellPane::size`), and that is what makes the deferred spawn work for more
  than one tab. `view` builds a `GridView` for `active()` only, so an *inactive*
  tab never lays out and never reports a size — so on a restored session its PTY
  waited out `SIZE_WAIT` and spawned zsh at 24x80, which is what the tab was still
  showing when you switched to it. Measured on a 5-shell session: only the 2 active
  shells were sized before spawn.

  `GridResized` therefore applies the size to **every shell in the reporting
  shell's pane**, and `Event::Attached` reads the size from the pane rather than
  from its own terminal. Tabs in a pane all share the pane's geometry, so one
  measurement is the correct answer for all of them. This replaced `Shell::sized`,
  which asked a per-shell question that only the active shell could answer.

  **A new tab must be *constructed* at the pane's size too** (`Shell::in_dir`'s
  `size`), and that is a third distinct hole rather than a restatement of the two
  above. `GridView` publishes a size only when it **changes** (`State::reported`),
  and iced reuses one widget state for a pane's grid however many tabs come and
  go — so a tab added to an already-measured pane receives no `GridResized` at
  all, ever. Its PTY is still told the truth by `Event::Attached`, and that gap is
  exactly what's visible: zsh writes `COLUMNS` cells into a grid still 24x80, they
  wrap, and its reverse-video partial-line marker (`%`, from `PROMPT_SP`) is
  stranded on the row above the prompt. Reproducible by opening a shell tab in any
  pane wider than 80 columns, which is every pane on a large display.

  Note the marker is the *honest* rendering of a wrapped line, not a stray glyph —
  which is why it looks like the spawn-ordering bug this file records under
  `pty.rs` and isn't one. `shell_tests` pins the constructor from both directions.

  **The shell is spawned as a *login* shell** —
  `CommandBuilder::new_default_prog()`, which resolves `$SHELL` (falling back to
  the password database rather than `/bin/sh`) and sets argv0 to `-zsh`, the
  leading dash being how a shell knows. This is what every terminal emulator does,
  and getting it wrong is why `docker`, `brew` and MacPorts couldn't be found:
  without the dash zsh reads `~/.zshrc` and nothing else — no `/etc/zprofile`, so
  `/usr/libexec/path_helper` never runs and `/etc/paths.d` is never read, and no
  `~/.zprofile`, where `brew shellenv` and credential helpers usually live.

  **It's invisible when the app is started from a terminal**, which is why it
  survived the whole spike: the full `PATH` is inherited from the launching shell,
  so only a Dock launch shows it — there the parent environment is launchd's
  `/usr/bin:/bin:/usr/sbin:/sbin`. Measured, Dock-launched: the non-login `PATH`
  had no `/usr/local/bin` (where Docker Desktop puts its CLI *and*
  `docker-credential-osxkeychain`) and no `/opt/local/bin`; the login one had both,
  plus everything `path_helper` contributes. `ps -axo comm` is the quick check —
  the children should read `-zsh`, not `/bin/zsh`. v1 never had the bug because it
  only ever ran inside an emulator.

  **Two shells means two subscriptions**, distinguished by hashing a `ShellId`
  through `run_with` — plain `run` would identify them by function pointer alone
  and collapse both panes onto one PTY. Three details there cost real time and
  will again if undone:

  1. `stream` returns `UnboundedReceiver<..>` **concretely**, not `impl Stream`.
     `run_with` wants `fn(&D) -> S` for a single `S`, and an opaque return type
     can't unify with that higher-ranked signature; the error is an unhelpful
     "one type is more general than the other".
  2. Events are tagged with their `ShellId` **inside** the stream, not by
     `.map()` on the subscription — `Subscription::map` panics at compile time on
     a capturing closure, and one that captures the id captures.
  3. Verify two PTYs actually spawned by counting child processes, not by looking
     at the window. Subscription deduplication is silent: you get one shell in two
     panes and it looks like a rendering bug.

## Resize, and mouse gestures

**Grid and shell resize in lockstep, every frame — no throttling.** Both
`Terminal::resize` and the PTY `SIGWINCH` happen in the same `GridResized` handler.

There was a debounce here and it was removed, which is worth recording because the
reasoning looked sound and was wrong twice over:

1. It was added to fix prompt fragments appearing after a shrink/grow drag, on the
   theory that a `SIGWINCH` per frame meant the shell's redraw for one width landed
   after we'd moved to another. Plausible, and real terminals do resize per frame
   without corrupting — which should have been the tell.
2. Debouncing *both* the reflow and the `SIGWINCH` made the text visibly trail the
   splitter. Debouncing only the `SIGWINCH` fixed the lag but caused a *new*
   symptom: fragments of adjacent lines flickering at narrow widths. That one isn't
   a rendering bug at all — it's the honest state of a grid that has reflowed while
   the shell still believes the old width. The delay *created* the artifact it was
   supposed to prevent.

The original corruption is most likely explained by the two spawn-ordering fixes
made around the same time (deferred spawn, and `Attached` not pushing a placeholder
size), not by resize frequency.

Four tests in `term.rs` now cover this ground, and a regression should start by
checking which of them breaks:

- `content_survives_shrink_then_grow` / `content_survives_a_drag_sized_sequence` —
  reflow down to 4 columns and back is lossless.
- `a_resize_never_invents_characters` — renders through the real `TerminalSource` at
  every width from 2 up, *including* layout widths that disagree with the
  terminal's own, and asserts nothing appears that wasn't written.
- `no_row_mixes_content_from_two_source_lines` — feeds `AAA`/`BBB`/`CCC` lines and
  asserts no rendered row ever contains two of them. Written specifically because
  the previous test used a single line of content and so could not detect cross-row
  leakage at all.

Note what these deliberately do **not** assert: that everything written stays
visible. Narrowing wraps lines, and rows that no longer fit move into scrollback
above the viewport, so seeing less mid-drag is correct. An earlier version of
`a_resize_never_invents_characters` failed on exactly that, and the test was wrong,
not the code.

**Mouse gestures** come out of the grid as one `GridMouse` enum
(`Press{row,col,count}` / `Drag` / `Release` / `Scroll`) rather than four
callbacks, since the app has to correlate them as a gesture anyway. Four details
that matter:

- **Scrolling leaves the widget in pixels, and the app draws the remainder.**
  `GridMouse::Scroll` carries `dy` in **pixels**; the app accumulates it, spends
  whole rows on the buffer or terminal, and keeps the sub-row remainder for the
  renderer to draw at. That is what makes scrolling smooth: the widget used to
  truncate every event to a whole row, so the view stepped a full row (~16px) at a
  time however gently you scrolled.

  The two `ScrollDelta` kinds still mean different things — a wheel notch is a
  stepped unit and becomes `ROWS_PER_NOTCH * cell_height`, while `Pixels` already
  *is* a distance. Scaling both by the notch factor made trackpad scrolling fly.

  **The offset lives in app state, not widget state** (`State::editor_scroll_px`,
  `Shell::scroll_px`), and that is forced: the gutter is a separate widget that
  must shift by exactly the same amount or the line numbers desync from their
  text, and no widget can read another's state. It is **rounded to a whole pixel**
  when drawing — at 1x a glyph at a fractional y is blurry, and a pixel is already
  ~16x finer than a row.

  Three consequences:

  - **The grid draws one row more than fits** while mid-row, so the bottom shows
    the next line arriving rather than a band of background. Every quad and
    `fill_text` is clipped to the viewport, so both partial rows are trimmed.
  - **A clamped row move zeroes the offset.** At either end of the content there is
    no partial row to show, so the view sits exactly on the boundary rather than
    leaving a sliver nothing can scroll away. Detected by comparing the scroll
    position before and after, since `scroll_by`/`scroll_read` clamp internally.
  - **Shells only get it inside scrollback.** The extra row comes from the grid line
    *below* the viewport, which exists only when `display_offset > 0`; at the live
    screen the offset is forced to zero — where it also belongs, since live output
    should not sit half a row out of line. `TerminalSource::fill` fills that row
    through `convert_cell`, the one shared cell converter — a second copy of that
    logic drifted immediately, re-enabling bold, which the app drops everywhere.

- **Drag coordinates aren't required to be inside the widget.** `Press` uses
  `cursor.position_in`, but `Drag` uses `cursor.position()` and clamps into the
  grid, so dragging past an edge extends the selection to that edge instead of
  freezing.
- **`dragging` lives in widget state, not the app.** A drag can't then be confused
  across panes.
- **Every gesture calls `shell.capture_event()`.** Without it `pane_grid` reads
  the press as a splitter grab and the two fight.

Double-click detection is the widget's own (`last_press` + a 400ms window) because
iced's `ButtonPressed` carries no click count.

