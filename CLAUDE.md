# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repo layout (2.0 rewrite in progress)

A Cargo workspace holding two frontends over one shared core:

```
crates/core/   sacrament-core — model + logic. NO UI framework dependency.
crates/tui/    sacrament-tui  — v1, binary `sacrament1`. Fallback; frozen.
crates/gui/    sacrament-gui  — v2, binary `sacrament`. The iced rebuild.
```

**Why a workspace and not a branch:** sacrament is a daily driver, so v1 has to
keep working (and keep being *installed*) for the months v2 takes. Both binaries
build and run simultaneously — edit v2's source in v1, run v2 beside it. A
long-lived branch would mean checking out to switch editors, plus cherry-pick
decisions on every v1 fix.

**What `core` is for:** the crate has no `ratatui`/`crossterm`/`iced` dependency,
so the frontend/model seam is a *compile error* rather than a discipline problem
— a leaked `ratatui::style::Color` won't build there. Anything genuinely
framework-independent belongs in `core`; if it needs a UI type, it doesn't.

Currently in `core`: `config`, `session`, `git`, `lint`, `protocol`, `client`,
`paths`, `theme`, `font`, `highlight`, `text`, `jira`, `secret`, `work`. Three sets of types moved out of v1's
`editor.rs`/`highlight.rs` to live with their producers — `ChangeKind` into
`git`, `Severity`/`Diagnostic` into `lint`, and the whole highlighter into
`core::highlight` with `Slot`/`Emphasis` replacing the ratatui color types (v1
converts back via `tui/src/hlstyle.rs`).

`core::text` holds the visual-metrics functions — `char_display_width`,
`wrap_line`, and the visual-column conversions. Everything there works in
**character indices** and **visual columns**, never bytes: a tab is one index but
several columns, a wide CJK glyph is one index but two columns, so the coordinate
systems genuinely differ and conflating them is the bug you get.

v1 keeps its own copies of those inside `crates/tui/src/editor.rs`. That
duplication is deliberate: v1 is frozen and gets deleted at the cutover, and
editing a shipping daily driver to deduplicate code that's about to be removed is
the wrong risk. If v1 outlives that plan, point it at `core::text`.

Still frontend-bound in `tui`: `markdown.rs` (returns `Vec<Line<'static>>`) and
the model half of `editor.rs`. Neither blocks v2 right now.

**v2 deliberately drops git and linting.** No change bars, no diff view, no lint
glyphs or diagnostics. `core::git` and `core::lint` stay because v1 still uses
them; v2 simply never calls them, which is why its gutter is only
`[number][space][chevron]`.

**Running both side by side** (`crates/core/src/paths.rs`): every path that
represents live state is namespaced by app id (`APP_TUI` / `APP_GUI`), because
sharing it breaks things in non-obvious ways. The socket would otherwise be owned
by whichever booted first, silently routing every `sacrament <file>` and every
`--review` hook to the wrong editor; the session file would have both instances
stomping each other's tabs on quit. `config.toml` is the deliberate exception and
*is* shared — v1's `Config` is `#[serde(default)]` without `deny_unknown_fields`,
so v2 can add a `[gui]` section without breaking v1.

Everything below the "Overview" heading describes **v1** (`crates/tui`) unless it
says otherwise, and is accurate for it. Treat it as the spec to port from,
including the "Dead ends and sharp edges" section — that's the list of things
*not* to carry forward.

### Companion documents (`docs/`)

This file is the map. Four subsystems keep their full write-ups beside it, because
each is long and each is mostly a list of bugs that were diagnosed once and are
expensive to rediscover. **A summary here always points at the doc; the doc is the
authority.**

| doc | covers |
|---|---|
| `docs/gui-rendering.md` | `grid_view.rs`, `blocks.rs`, `term.rs`, `pty.rs` — pixel snapping, block geometry, terminal reflow, shell spawning and sizing, mouse gestures |
| `docs/gui-chrome.md` | tab strips, markers, drag-reorder, pane chrome and the divider — and the approaches that were tried and failed at scale 1 |
| `docs/jira-runs.md` | the Jira dashboard and the Run button, including the guard-rail table for unattended runs |
| `docs/jira-integration.md` | the forward-looking build plan the Jira work came from |

### v2 spike (crates/gui)

Deliberately a spike, not a foundation: it exists to answer whether iced can host
a fast styled cell grid before more is built on it. The grid is first because it's
the risk — both shell panes and the editor surface are the *same* widget fed from
different sources, so if it can't be drawn fast, iced is the wrong choice. The
editor buffer logic is work, not risk; it ports from a known-good v1.

- `grid.rs` — the `GridSource` trait and the `Cell` type. **This is the seam
  that makes "shell panes and the editor are the same widget" true rather than
  asserted.** `TerminalSource` (in `term.rs`) and `BufferSource` (in `buffer.rs`)
  are the two implementations; `GridView` knows about neither.
- `buffer.rs` — the editor pane's text buffer. Its `visible_rows`/`gutter_rows`
  pair is the **single producer** of screen-row → file-line mapping, which is what
  keeps the gutter aligned with the text.

  **One mutation primitive.** Every edit goes through `Buffer::replace(at, remove,
  text)`. v1's discipline — any line-count change must splice `highlights` and
  `line_state_before` in the same step, or highlighting starts belonging to the
  wrong line — therefore lives in exactly one function instead of one per edit
  method. `replace` also preserves `line_state_before[row]` across the splice,
  since the parse state *before* the edited row is still valid.

  **Undo stores deltas, not snapshots.** v1 kept up to 500 full copies of the
  buffer text, which on a large file is a lot of memory for an app whose pitch is
  a small footprint. An `Edit` here is `{at, before, after}` and costs the size of
  the change, so `MAX_UNDO` is 2000. Runs of typing coalesce into one step
  (`coalesce_end` tracks where the next keystroke must land); cursor movement,
  newlines, and multi-char pastes break the run so they undo separately.

  **Horizontal scroll** (`scroll_col`) exists for when wrapping is off, where a
  long line would otherwise be unreachable past the right edge. Held in *visual
  columns*, not character indices, because that's what the viewport measures — a
  tab is one character but several columns.

  Three consequences, and each is a bug if missed: `fill` tracks the line's
  column and the pane's column separately (they differ by the scroll) rather than
  deriving one from the other; `screen_to_doc` adds the scroll back, or clicking
  lands several columns left of the pointer; and `ensure_cursor_visible_in`
  carries the view with the caret, or typing past the right edge edits text you
  can't see. Wrapping being on pins it to zero — nothing is off-screen then — and
  the right-hand bound comes from the widest line in the whole buffer rather than
  the visible ones, so the stop doesn't move as you scroll down.

  Read mode has its own (`read_scroll_col`), and it is the reason **table rows
  are unwrappable** (`markdown::Line::wrappable`). Breaking one at the row level
  puts half its cells on a row of their own and destroys the column alignment
  that makes it a table — so a table that still doesn't fit after `emit_table`
  has budgeted its columns (see "Tables") is reached sideways instead.
  Everything else in a document still wraps, so a file with no table has nothing
  to scroll to and the bound collapses to the pane width.

  **Soft wrap.** A line occupies one screen row per wrap segment, from
  `text::wrap_line`. Two consequences run through everything:

  - **Scrolling has two components**, `scroll_row` *and* `scroll_seg`. Without the
    second, scrolling past a line that wraps into ten rows jumps all ten at once.
  - **Vertical movement is by screen row, not by line** (`move_screen_row`), holding
    the visual column. Moving by line would skip the wrapped remainder of a long
    line, which doesn't match what the caret appears to do. `Home`/`End` act on the
    segment for the same reason.

  **Continuations hang under their line's indentation** rather than returning to the
  margin, so a wrapped continuation doesn't read as a new statement. The indent is
  the line's own leading whitespace, capped at half the pane width by
  `text::clamp_hanging_indent` — a deeply-indented line in a narrow pane would
  otherwise leave no room for text. It also *costs* width: continuations wrap at
  `width - indent`, or the text would run past the right edge.

  `VisibleRow::indent` carries it so caret, click, and render all read one value
  instead of recomputing. The subtle one is vertical movement: `move_screen_row`
  tracks the **screen** column, not the segment-relative one, because crossing
  between a first segment (no indent) and a continuation (indented) has to keep the
  caret under the same pixel.

  `wrap_width == 0` means wrapping off, and `wrap_line` returns a single segment for
  it — one code path, not two. The width is pushed in from the grid's reported size
  (`Message::EditorResized`), gated on `config.word_wrap`.

  **Tabs are expanded to cells in `fill`.** One character becomes several cells, so
  the grid stays a grid and every downstream position — caret, click, selection —
  computes in the same units. Wide characters get their extra columns as blanks
  carrying the same styling.

  **Selection is anchor + cursor**, not a start/end pair — extending backwards
  past the origin then needs no special case. `selection_range()` normalizes on
  read. Two consumers must agree or what you see isn't what you copy:
  `is_selected(row, col)` paints (called per cell in `fill`) and `selected_text()`
  extracts; a test walks both over the same region and compares.

  Replacing a selection is **one** `replace` call — remove the span, insert the
  new text — so typing or pasting over a selection is a single undo step rather
  than a delete followed by an insert. `span_len` converts a selection into the
  char count `replace` wants, and a test pins it against the text it describes,
  because an off-by-one there eats a neighbouring character.

  **Dirty is derived, not stored.** `refresh_dirty` compares the history head's
  monotonic `revision` against `saved_revision`. This fixes a real hole from the
  first pass: restoring a remembered dirty flag meant undoing past a save reported
  *clean* while the buffer differed from the file on disk — closing there would
  have discarded work silently. Monotonic revisions also distinguish two different
  edits at the same history depth, which a depth counter can't.

  **Save** (`Buffer::save`) has two protections, because it's the one operation
  that can destroy work: it refuses to write when the file's mtime no longer
  matches what was read (someone else changed it, and there's no merge UI yet), and
  it writes to a temp file and renames rather than truncating in place, so a crash
  mid-write can't leave a half-written source file. `had_trailing_newline` is
  preserved from load so saving doesn't add or drop the final byte — that would
  read as a spurious one-line diff in git.

  Feedback for save is the **window title**, not a status strip: `• name` when
  dirty, `name` when clean. A marker that disappears tells you more, continuously,
  than a message that flashes once — and it means no permanent chrome.
- `gutter.rs` — line numbers as a real widget, not cells. v1 drew the gutter
  inside the text `Paragraph`, which forced `render_gutter`, `gutter_width`, and
  the click hit-test to agree on column arithmetic by hand. As a widget, iced's
  layout owns the width and that whole class of drift is gone.
- `grid_view.rs` — the custom `iced::advanced::Widget`. **Run batching** (adjacent
  cells sharing fg+flags coalesce into one `fill_text`) and **`Shaping::Basic`**
  are what carry the performance; cell metrics are *measured* from the font once,
  never derived from an assumed monospace ratio. Every drawn position goes through
  `col_x` / `row_y`, **which round** — at scale 1 a cell boundary on a fractional
  pixel composites into a visible hairline seam.
- `blocks.rs` — **U+2580..259F are drawn as rectangles, not glyphs.** That seam is
  worst exactly where it matters most: a run of `█` is *supposed* to be one
  unbroken bar and no font can make it one, because the gap is between the glyphs
  rather than inside them. The shades `░▒▓` stay glyphs, deliberately.
- `term.rs` — `alacritty_terminal::Term` replacing v1's `vt100::Parser`. Shell
  selection uses alacritty's own `Selection`, which works in *grid* coordinates,
  so a selection survives scrolling and word/line selection come for free.
  `Shift+click` opens a URL, allowlisted to `http`/`https` — a security boundary,
  since terminal output is routinely attacker-influenced.
- `pty.rs` — the non-obvious plumbing. `Subscription::run_with` can't capture, so
  the PTY is built *inside* the stream and hands a `Handle` back out; reader bytes
  cross the thread→async boundary over an `unbounded` channel, so output is
  genuinely push-driven. **The shell is not spawned until the real pane size is
  known**, and it is spawned as a **login *and* interactive** shell — without
  that, `brew`, `docker` and MacPorts are unfindable from a Dock launch.
- `palette.rs` — where v1's "terminal palette is the theme" rule is formally
  retired. The 16 slots now come from `[theme]` in `config.toml`, and indexed
  16-255 plus truecolor resolve faithfully instead of collapsing to the default
  (v1's `vt_color_to_ratatui` flattened everything above index 15, which is why
  256-color TUIs drew blank while `TERM` advertised 256 colors). `Palette` is
  resolved once at startup and passed into the widget, so `draw` never touches
  config or converts per cell.
- `metrics.rs` — feed time, throughput, footprint. **Opt-in**: gated on
  `SACRAMENT_METRICS`, mirrored to stderr, never drawn. It's diagnostics, not
  UI.

**Grid and shell resize in lockstep, every frame — no throttling.** A debounce
was added here and removed; it *created* the artifact it was meant to prevent.

**Mouse gestures** come out of the grid as one `GridMouse` enum
(`Press{row,col,count}` / `Drag` / `Release` / `Scroll`) rather than four
callbacks, since the app has to correlate them as a gesture anyway. Scroll
deltas leave the widget in **pixels** and the app draws the sub-row remainder,
which is what makes scrolling smooth.

**All of this is in `docs/gui-rendering.md`**: pixel snapping and why a
fractional `fill_text` bounds drops a glyph onto a row of its own, block
geometry, the removed debounce and the four reflow tests that cover that ground,
the three separate shell-sizing holes, the scroll-offset rules, and URL
detection. Nearly every entry there is a bug that was diagnosed once, at cost —
read the relevant one before changing any of it.
**Widget-size reporting**: the grid derives rows/cols from its real layout bounds
and publishes them via `shell.publish` in `update`. It can't resize the PTY
itself (no handle, and `layout` has no `Shell`), so it reports and the app acts.
That indirection is structural to iced, not incidental.

**Why gutter and text stay aligned.** Two properties, and it's worth knowing they
are the *only* two:

1. **Row pitch is arithmetic, never measured.** Both widgets compute it as
   `font.size * font.line_height` from the same `FontSpec`. Nothing is measured,
   so nothing can disagree. (Cell *width* is measured, but only the gutter's own
   width depends on it — `font::advance_width` is the single implementation.)
2. **The row mapping has one producer.** `Buffer::visible_rows` and
   `Buffer::gutter_rows` come off the same walk, and each widget calls it with a
   row count derived from its own height. Same function, same inputs, same
   answer — no state is passed between the widgets at all.

A test pins property 2 directly
(`buffer::tests::gutter_rows_and_visible_rows_describe_the_same_screen`); break
the mapping and it fails rather than the two silently drifting on screen.

**Locking**: sources and the gutter hold `Arc<Mutex<_>>` and lock inside
`fill`/`draw`, never in `view()`. A `MutexGuard` taken while building the view
can't live long enough to reach `draw`. This is also why `GridView` *owns* its
source (`Box<dyn GridSource>`) rather than borrowing one.

**Every tab strip is built from one `State::tab_strip`** so they read as the same
control. There are four: the editor pane's *section* strip, the file strip below
it, and one per shell pane. The three holding things the user opened carry a
trailing `+`, and it means the same in each — one more of these. The section
strip has none, because the set of sections is the app's.
#### Sections — the editor pane is not just the editor (`State::section`)

The editor pane holds several **sections**, chosen by an outer tab strip:
`Section::Editor` (the file tabs and the text surface, i.e. everything the pane
used to be), `Section::Jira` (the ticket dashboard — see "Jira and the Run button") and
`Section::Scratchpad` (one permanent plain-text document — see below). The file
tabs are *subtabs of the editor section*, not chrome the pane always carries —
switch section and they go with it, because they describe that section's contents.

`Section::ALL` is the single definition of both the set and its left-to-right
order: `section_bar` renders from it and `select_section` indexes back through it,
so adding a section is a variant, a `label`, and an arm in `view`. A test pins that
no variant appears twice in `ALL` — `section_bar` finds the active tab with
`position`, which returns the *first* match, so a duplicate would leave one tab
permanently unselectable.

**`Focus::Editor` now names the pane, not the surface**, and that distinction is
the whole correctness problem here. With Jira showing there is no text on screen
and no caret, but focus is still `Editor` — so every command that reaches the
buffer had to start asking `State::editing()` (`focus == Editor && section ==
Editor`) instead. Without it, typing, `Cmd+Z`, `Cmd+/` and the arrow keys all edit
a file the user cannot see. It's the same hole read mode has, closed the same way:
**at the routing layer, once**, not inside each command.

Read mode is deliberately *not* folded into `editing()`. It's a different question
— there the text is on screen and merely can't be typed into — and the two give
different answers for navigation keys.

**Two sections hold text, so "which buffer?" became its own question**
(`Section::has_text`, `State::text_target`). The editor pane has two editable
surfaces now — the active file tab and the scratchpad — so typing, undo and
select-all have to ask *which* before they act. `text_target` answers it once, the
way `read_target` does for scrolling, and returns `None` when nothing editable is
showing; a command that has one is a command that can run.

**`editing()` and `text_target()` are deliberately not the same question**, and
the split is chosen for its failure modes rather than its tidiness:

- `editing()` still means the **file** editor specifically. Everything belonging
  to the tab strip keeps asking it — close, cycle, select tab N, save-as.
- `text_target()` means **some** editable surface. Everything that acts on text
  asks it instead.

Get it wrong in one direction and a text command simply doesn't work in the
scratchpad — visible, harmless, fixed in a line. Get it wrong in the other and a
tab command edits or closes a file nobody can see. So the wide one is opt-in: new
text commands must reach for `text_target` deliberately, and forgetting fails safe.

The rule that decides which commands survive a section with no text:

- **Commands that act on what's on screen are inert**: typing, paste, undo/redo,
  copy/cut, select-all, comment, indent, fold, save, save-as, close tab, select
  tab N, cycle tabs. Nothing is lost by refusing — the buffer stays dirty and
  saveable, and the quit prompt still catches it.
- **Commands whose purpose is to show you something switch back** (`show_editor`):
  `Cmd+O`, a dropped file, a non-review IPC open, `Cmd+N`/`Cmd+T`, `Cmd+F`,
  `Ctrl+G`, `Cmd+G`, `Cmd+Shift+M`. A tab nobody can see isn't an answer to "open
  this".
- **A `--review` open still doesn't switch**, for the reason it doesn't take the
  active tab either: a tool writing files in the background must not yank the view.

Two smaller things:

- **Section tabs don't reorder.** `TabGroup::reorderable()` is false for
  `TabGroup::Section`, so `TabPressed` starts no drag — the set is fixed, and a
  drag would have to persist an order that means nothing next launch. Answered by
  the group rather than by a parameter to `tab_strip`, so the fact lives in one
  place. `move_tab`'s `Section` arm is therefore unreachable, and says so rather
  than being a `_` that would silently absorb a future group that *should* reorder.
- **The active section isn't persisted.** A section has no state of its own to
  restore, and landing on the editor is right for a launch that was given files.

**Fixed on the way past, twice.** `Message::Pasted` was gated on neither read mode nor
(now) the section. Paste arrives as its own message rather than through `keymap`,
so the key dispatch's read-mode arm never covered it — meaning `Cmd+V` in read mode
edited the source behind the rendering, exactly the v1 hole this file claims v2
closed. It now requires `editing() && !reading()`.

And `reading()` asked the *file* buffer rather than the surface being typed into,
which the scratchpad turned into a real hole: with a markdown tab left in read
mode, the key dispatch's `reading()` arm claimed every keystroke while the
scratchpad was on screen, so typing there silently scrolled a rendered document
nobody could see. It asks `text_target` now. The same bug in the wheel handler
(`read_target`) scrolled the invisible file instead of the scratchpad.

Not added, and worth knowing they're absent: no keybinding switches sections
(clicking is the only way), and `Cmd+1..9` still means "select file tab N", not
"select section N".

#### The Scratchpad section (`State::scratchpad`)

One permanent plain-text document — somewhere to put a note that isn't a file and
isn't worth naming. **It is the editor**, pointed at a different buffer: the same
`GridView` over the same `BufferSource`, so selection, undo, wrapping, the caret
and mouse handling come for free and cannot drift from how they behave on a file.

What it deliberately lacks, and why each is a refusal rather than an omission:

- **No gutter.** Line numbers in a notes file say nothing. Folding goes with it —
  `fold_command` asks `editing()`, not `text_target()`, because a fold is only
  legible through the chevron the gutter draws. Folding here would be text that
  vanished with nothing on screen to say why, or that it could come back.
- **No tab strip.** One document, always this one. The `+` elsewhere means "one
  more of these", and there is no more of this.
- **No highlighting and no markdown.** `BufferSource` is handed `None` rather than
  the highlighter, and `load_scratchpad` seeds no syntax — so the work isn't
  discarded, it's never done. `Cmd+/` lands on "no syntax for this file", which is
  the honest answer. Read mode can't reach it either: the file is `.txt` and
  `set_read_mode` gates on the extension, which is why that extension is
  load-bearing rather than cosmetic.

**It is not in `buffers`.** That list is the file tabs — things the user opened,
that the session restores, that `Cmd+W` closes and the quit prompt asks about. The
scratchpad is none of those, and putting it in the list would have meant excluding
it by index from every one of those operations: the kind of exception that gets
missed once and then edits the wrong document.

**It autosaves, and is never asked about.** There is no tab to carry a dirty dot,
no `Cmd+W` to prompt on, and nothing about it in the quit dialog — a scratchpad you
have to remember to save is one that eventually loses a note. `save_scratchpad`
returns before touching the disk when the buffer is clean, which is what lets it
hang off three cheap boundaries instead of a timer:

- **Leaving the section** (`set_section`, the single place `section` changes).
- **Focus leaving the editor pane**, compared across `handle` in `update`. Focus
  has no single setter — a pane click, `Ctrl+2` and spawning a run all move it —
  so one comparison in the wrapper catches every route, including later ones.
- **Quitting** (`quit_now`).

A timer was not added on purpose: idle repaints are the thing this app doesn't do,
and `time::every` would wake it forever to write nothing.

**A changed-on-disk conflict overwrites.** `Buffer::save`'s guard is for files two
people might edit; this one is ours, at a path nothing else knows. Honouring the
guard would give a scratchpad that silently stopped saving with no dialog anywhere
to resolve it, so the buffer on screen wins.

**A scratchpad that can't be opened says so at startup**, alongside the font
warning and by the same reasoning — the section still works, so nothing looks
wrong, and it would discard everything typed into it at quit. Detected by the
buffer having no path, which is both what `load_scratchpad` falls back to and what
`save_scratchpad` refuses to write.

`set_section` also **resets `editor_scroll_px`**. It is a sub-row pixel remainder
belonging to whichever surface was last scrolled; carried across a switch it
offsets the next one by up to a row for no reason.

The window title reads `Scratchpad` with **no dirty dot**: it autosaves, so a
marker would report a state the user has no action to take about, blinking on and
off as they type. The Jira section still shows the active file, because there is no
document on screen there and the last thing edited is the most useful thing the
title can say.

Stored at `paths::scratchpad_path` — `<config>/sacrament2-scratchpad.txt`, beside
the session file. That is state the app owns and rewrites without being asked,
which is the session's category and the opposite of a file the user chose; a
documents folder would imply a name they picked and a lifetime they control.

#### Jira and the Run button (`core::jira`, `core::secret`, `core::work`)

A read-only ticket dashboard, and a `Run` link on each row that starts Claude Code
on that ticket in a shell pane: branch, do the work, commit, push, open a **draft**
PR.

**The detail is in `docs/jira-runs.md`**; the plan it came from is
`docs/jira-integration.md`. What has to be known here:

- **The app does not embed an agent loop, it orchestrates the one already
  installed.** The whole feature is a prepared prompt, a shell tab, and a check
  afterwards.
- **The dashboard is real widgets, not the markdown renderer.** Its refresh
  control and its clickable issue key are things a cell grid has no way to
  express. Every column is a `FillPortion`, which is what keeps rows aligned.
- **The agent never works in the checkout you are working in.** Every run gets a
  `git worktree` of its own (`work::add_worktree`, `paths::worktree_dir`), so
  starting a ticket is an aside rather than something you stash your own work for.
  A finished run's worktree is tidied away unless there's something in it to lose.
- **The agent's account of its own work is not evidence.** `work::verify` asks git
  whether the branch exists, how far it got and whether it was pushed, then asks
  `gh` for the PR. This is why the app dictates the branch name.
- **Secrets do not go in `config.toml`** — that file is plaintext, hand-edited and
  shared with v1. `core::secret::lookup` reads the environment variable, then the
  macOS Keychain via the `security` CLI.
- **Everything outside the base system runs through a login *and interactive*
  shell.** A Dock-launched app inherits launchd's minimal `PATH`, and `~/.zshrc`
  is read only for *interactive* shells — so both flags are needed to find
  `claude` and `gh`, and omitting either is invisible in development and total in
  production.
- **Unattended is a deliberate choice** (`--dangerously-skip-permissions`), and
  the guard rails are what pay for it. `docs/jira-runs.md` has the table of each
  guard against what it prevents; removing any one of them changes the trade.

Configuration lives in `[jira]`, with `[jira.repos]` keyed by project key.
`[jira.repos]` must be the **last** thing in `[jira]`: a sub-table header closes
the table above it, so a plain key written after it lands in `repos`.

**Untested end to end.** Everything pure is covered and the git half is driven
against real repositories, but a whole run against a live Jira instance and a
real remote has never been driven.

**Shell tabs made PTY identity dynamic.** `ShellKey { pane, serial }` replaced a
fixed two-variant enum, and `subscription()` builds one `run_with(key, …)` per
live shell. Because the list is derived from state each frame, **adding a key
spawns a PTY and removing one stops it** — there are no imperative spawn/kill
calls. `serial` is never reused, and `pty::run` kills the child before `wait`.

**Shell tabs are labelled by the shell's cwd basename**, from polling the child
process (`core::proc::cwd_of`). v1's OSC 7 path was per-chunk and stateless, so
it isn't ported.

**Tab markers** are `•` when the buffer is unsaved and `◇` when an external tool
touched it and you haven't looked since, as *separate* `text` widgets so they
keep their own colour rather than the tab's.

**Tabs reorder by dragging**, and the strip reorders **live** — the movement is
the feedback. That makes oscillation possible, and the fix is **direction, not
geometry**.

**All of this is in `docs/gui-chrome.md`**, along with the four strips, the
underline that breaks under the active tab, why separators are `rule` widgets and
why cells mostly don't paint a background at scale 1, and why tabs are not
`button`s. Read it before changing a tab strip: several obvious-looking
implementations are recorded there as failures, each having been tried.
#### Live config reload (`State::reload_config`, `config::load_result`)

**`config.toml` is watched like an open file, so editing it takes effect without a
relaunch.** It rides on the same declared watch set (`sync_watches`) rather than a
watcher of its own — one mechanism, and it can't be forgotten on a path that
re-syncs.

**A parse failure keeps the config already in use.** This is why
`config::load_result` exists beside `load`: `load` answers a broken file with
`Config::default()`, which is right at startup — there's nothing yet to lose — and
actively harmful on reload, where saving a half-typed line would reset the theme,
the font and every editor setting mid-session with nothing on screen connecting
that to the save. The error names the file, line and column, since `toml`'s own
message beats anything invented here. Startup reports it too now, rather than
running on defaults in silence.

**`FileChanged` does both jobs, not one or the other.** `config.toml` open as a tab
is a file like any other: it has to refresh on screen *and* take effect. An
either/or would drop whichever branch wasn't written first.

What lands immediately:

- **`tab_width` and `word_wrap`** are per-buffer, so they're written to every open
  buffer *and* the scratchpad. This is the reason the feature exists — they were
  read only when a buffer was built, so an open file kept the old width until it
  was closed and reopened. Each surface is re-settled with `ensure_cursor_visible`
  afterwards, because a wrap change alters how many screen rows sit above the
  caret.
- **`indent_with_tabs`, `line_numbers`, `[jira]`** are read from `self.config`
  where they're used, so they need nothing at all.
- **`[theme]`** rebuilds `Palette`. The widgets take `&self.palette` each frame
  rather than caching a copy, so the next redraw has it.

What can't, and is **reported rather than silently ignored** — a setting that looks
applied but isn't is worse than one that says it needs a restart:

- **`[font]`** resolves the family against `fontdb` and leaks both the name and its
  coverage table to obtain `&'static str`, and the fallback chain is consumed into
  a static at startup. Re-resolving per save would leak per save.
- **`syntax_highlighting`** decides whether a `Highlighter` is built *at all* —
  the startup cost the option exists to avoid — and every open buffer seeded its
  parse state under whichever answer applied when it loaded.

`restart_required` is a free function so it can be tested, because this is the part
that rots: add a config field and it silently classifies as "applies live", which
is the wrong way round. A test names every field, so a new one has to be
classified deliberately.

Known limit: a `config.toml` that doesn't exist yet can't be watched — notify fails
on the path and the watcher records it as handled either way — so creating one for
the first time still needs a restart.

**Config options v2 honors** (`tab_width` and `word_wrap` reach a buffer through
`empty_buffer` — see "Conventions"): `tab_width`, `indent_with_tabs`, `line_numbers`,
`syntax_highlighting`, `word_wrap`, plus `[theme]` and `[font]`.
`syntax_highlighting = false` skips *building* the `Highlighter`, not just its
output — loading syntect's syntax set is the cost the option exists to avoid.
Deliberately unread: `status_timeout_ms` (v2 has no status line at all — see
"Alerts") and `[lint]` (v2 does no linting).

#### Pane chrome and the window

**Pane chrome** (`PANE_PADDING` / `PANE_BORDER` in `main.rs`): each pane's content
is wrapped in a `container` carrying the padding and background, and the grid
derives rows/cols from its own layout bounds, so it shrinks to fit and nothing
else needs to know the chrome exists. **The divider between panes is the gap
behind them, not a border on each.** **There is no status bar at all** — the
window is the grid, edge to edge, and the bottom row exists only while a prompt is
open. Metrics go to stderr under `SACRAMENT_METRICS`, never to the screen. Details
in `docs/gui-chrome.md`.
#### Alerts (`State::alert`)

**Everything the app has to *say* is a native alert with an OK button.** There was
a one-row strip at the bottom for this, and it failed in the two ways a status
line does: it was easy not to notice, and a persistent condition (`failure`,
`font_warning`) left text sitting there with nothing to clear it — "not a markdown
file" stayed on screen until something else overwrote it.

The rule that separates the three ways of speaking: a **dialog with choices** is
for a decision the app can't make (see "Native dialogs"), an **alert** is for
something it can only report, and the **window title** is for state that changes
continuously (the dirty dot).

Three things about the implementation:

- **Alerts are queued, not returned.** `State::alert` pushes onto
  `State::alerts`, and `update` flushes it after `handle` has run. Threading a
  `Task` out of every site that needs to say something — `toggle_comment`,
  `search`, `save_session`, the watcher — would have restructured half the file to
  deliver a dialog. One flush point also means a queued message is shown exactly
  once, and everything from a single message goes in *one* dialog rather than a
  queue the user has to clear.
- **Repeats are suppressed while one is on screen** (`State::showing_alerts`,
  cleared by `Message::AlertDismissed`). This isn't hypothetical: `notify` emits
  several events for one write, and a refused reload reports on every one of them,
  so a dirty-buffer conflict would otherwise stack a dialog per event. The same
  message can be raised again once the first is acknowledged.
- **The prompt's own note stays inline.** `Prompt::note` ("no match") is the one
  message that arrives *while typing*, and a dialog there would have to be
  dismissed between keystrokes. A promptless `Cmd+G` miss is an alert, and it names
  the query — with no prompt open there's nothing else on screen to say what was
  searched for.

The font warning is raised from `State::new`'s returned `Task` rather than the
queue: it's a config error, and there's no message yet to queue it behind.

#### Keybindings — v2

**Every application binding is `Cmd`.** Not a style preference: v1 lived inside a
terminal emulator and had to fit around whatever that emulator already claimed,
which is why it reached for `Ctrl`, `Ctrl+Shift`, and `Alt` and then had to make
`Ctrl+C` mean two different things by focus. v2 owns its window, so it uses the
platform convention and leaves `Ctrl` entirely alone.

The payoff is that **the shell gets all of `Ctrl` back**. `Ctrl+C` is SIGINT,
`Ctrl+R` is reverse history search, `Ctrl+W` is kill-word, `Ctrl+Z` suspends —
none of which worked while the app was intercepting them globally.

| binding | action |
|---|---|
| `Cmd+S` | Save (asks what to do if the file changed on disk; the Scratchpad just writes) |
| `Cmd+O` | Open — native panel |
| `Cmd+N` | New buffer tab |
| `Cmd+T` | New tab in the focused pane (shell pane → shell; editor → buffer) |
| `Cmd+W` | Close the focused pane's active tab |
| `Cmd+Q` | Quit (saves session) |
| `Cmd+Z` / `Cmd+Shift+Z` | Undo / redo |
| `Cmd+C` / `Cmd+X` / `Cmd+V` | Copy / cut / paste — copy reads the shell selection when a shell has focus |
| `Cmd+A` | Select all (editor) |
| `Cmd+/` | Toggle line comment |
| `Cmd+]` / `Cmd+[` | Indent / outdent the selected lines |
| `Esc` | Clear the selection |
| `Cmd+Option+[` / `]` | Fold / unfold at the caret |
| `Cmd+Option+Shift+[` / `]` | Fold / unfold everything |
| `Cmd+1..9` | Select tab N **in the focused pane** |
| `Cmd+Shift+[` / `]`, `Cmd+Tab` | Cycle tabs in the focused pane |
| `Cmd+←` / `→` | Start / end of screen row |
| `Cmd+↑` / `↓` | Start / end of document |
| `Cmd+Shift+S` | Save as — native panel |
| `Cmd+F` | Find (prompt) |
| `Cmd+G` / `Cmd+Shift+G` | Find next / previous — repeats the last query with no prompt open |
| `Ctrl+G` | Goto line (prompt) |
| `Option+←` / `→` | Word left / right (`Shift` extends) |
| `Cmd+Shift+M` | Toggle markdown read mode |
| `Cmd+R` | Refresh the section, where it has something to refresh (Jira; the editor is kept current by the watcher) |
| `Cmd+Shift+R` | Reset metrics counters (only visible under `SACRAMENT_METRICS`) |
| `Ctrl+1` / `2` / `3` | Focus editor / bottom shell / right shell |

`Cmd+G` is find-next, the macOS convention, so goto-line takes `Ctrl+G` —
Sublime's binding. Both of the app's non-`Cmd` chords go through `is_app_ctrl`.

**Every editor binding in this table is conditional on the editor *section* being
the one on screen**, not merely on the pane having focus — `State::editing()`, not
`focus == Focus::Editor`. Some are inert while another section shows and some
switch back to the editor; which is which, and why, is under "Sections".

Four things here are load-bearing:

- **`Ctrl` is used by exactly two bindings, both because their macOS key is
  already spoken for**: pane focus (`Cmd+1..9` is "select tab N") and goto-line
  (`Cmd+G` is "find next"). `is_app_ctrl` gates both — `Ctrl && !Alt` on macOS,
  `Ctrl+Alt` elsewhere, because off macOS `Modifiers::command()` *is* `Ctrl` and
  a plain `Ctrl+G` would be ambiguous with the Cmd table. The shell gives up
  `Ctrl+G` (BEL) and nothing else; `Ctrl+<digit>` isn't a control code at all.
- **Tab and window commands act on the focused pane**, not on the buffer list.
  One `Cmd+T` and one `Cmd+W`, rather than v1's split of `Ctrl+W` for buffers and
  `Ctrl+Shift+W` for shells.
- **`keymap` drops any chord with `logo()` set.** Otherwise an *unbound* `Cmd+K`
  falls through to the composed-text branch and sends a bare `k` down the PTY.
  For the same reason `command_key` returns `Some(Task::none())` for unmatched
  `Cmd` combinations — consuming them is the point.
- **Shifted punctuation is matched in composed form.** `Cmd+Shift+[` arrives as
  `{` on a US layout, so both spellings are matched rather than reading the raw
  key. v1's `apply_shift` table is still not wanted.

`key_tests` in `main.rs` pins the two properties worth pinning: that `Cmd` never
reaches the PTY, and that `Ctrl+C`/`R`/`W`/`Z` still produce `0x03`/`0x12`/`0x17`/
`0x1a`.

Not bound in the *editor*, because the underlying capability doesn't exist yet:
`Option+Backspace` (delete word), `Cmd+Shift+F` (find in project), replace.

**What a shell gets beyond the plain keys** (`keymap`'s `Named` arm), all of it
the macOS terminal convention rather than anything invented here:

| key | bytes | why |
|---|---|---|
| `Shift+Enter` | `\n` | A newline for a TUI composing multi-line input |
| `Shift+Tab` | `\x1b[Z` | Back-tab, so a TUI can bind it apart from `Tab` |
| `Option+←` / `→` | `\x1bb` / `\x1bf` | `backward-word` / `forward-word` |
| `Option+Backspace` | `\x1b\x7f` | `backward-kill-word` |

**Every one of these is a modifier that would otherwise be silently dropped.**
The `Named` table matches the key alone, so without an explicit arm `Shift+Tab`
sends a plain `\t` and the program at the other end cannot tell the two apart —
`cat -v` is the check, and it should print `^[[Z`. `\x1b[Z` is terminfo's `kcbt`
and what every terminal sends.

Two details worth keeping:

- **`Shift+Enter` sends `\n`, not `\x1b\r`.** The `\x1b\r` form is what Claude
  Code's own `/terminal-setup` installs into iTerm2 and VS Code, and it works —
  but at a *plain shell prompt* `\e\r` is unbound in zsh's emacs keymap, so
  Shift+Enter would silently stop submitting commands. `\n` is what `Ctrl+J`
  sends, which is the escape hatch people already use for this, and it's also
  `accept-line` in zsh and readline. One byte that satisfies both.
- **Word movement is Esc-prefixed, not CSI.** `\x1bb` / `\x1bf` is what iTerm2's
  "natural text editing" preset and Ghostty's defaults send, and zsh and readline
  bind them out of the box. The xterm form (`\x1b[1;3D`) needs the shell to have
  bound it, so it does nothing in a default zsh.

The `alt()` check runs *before* the plain-arrow table, which otherwise ignores
modifiers and would swallow the whole thing.

#### Drag and drop (`State::drop_file`)

A file dropped on the window is **routed by focus**, not by where the pointer was:
`window::Event::FileDropped` carries no position — winit exposes none, so neither
does iced — and nothing else is available, because the OS owns the pointer for the
length of a drag and no `CursorMoved` arrives during one. v1 behaved the same way
for a different reason: the emulator handed the path over as pasted text, which
went wherever focus was.

- **Shell focus** → the escaped path, as a bracketed paste, plus a trailing space.
  That's what every terminal puts there, and it's how Claude Code receives a
  dropped image.
- **Editor focus** → opened as a tab, the same path as `Cmd+O`. A file it can't
  read (an image) reports that rather than opening blank.

`shell_escaped` backslashes everything that isn't alphanumeric or safe
punctuation. Non-ASCII letters pass through — they aren't shell-special, and
escaping them turns a legible path into noise.

#### Native dialogs (`rfd`)

Anything that is a *question with consequences* is a system dialog rather than
in-app chrome. The rule that separates them: a dialog with choices is for a
decision the app can't make and must block on; an alert (see "Alerts") is for
what it can only report.

| flow | dialog |
|---|---|
| `Cmd+Shift+S`, and `Cmd+S` on an untitled buffer | Save panel |
| `Cmd+O` | Open panel, multi-select |
| Closing a dirty tab | Save / Don't Save / Cancel |
| Quitting with unsaved work | Save / Don't Save / Cancel |
| Saving over a file that changed on disk | Overwrite / Reload / Cancel |

**All of them are async, through `Task::perform`.** A blocking dialog on the UI
thread deadlocks against the event loop that has to keep drawing it. The
consequence is that anything happening *after* a dialog has to travel with it —
hence `AfterSave`, which is how "save this untitled buffer, then close its tab"
or "…then quit" survive the round trip through the message system.

**The save panel replaced a hand-rolled prompt, and deleted the fiddliest part
of it**: overwrite confirmation is the panel's job, so `PromptKind::SaveAs`, its
`confirm_overwrite` field and the "press Enter again" rule are all gone. Same for
`Cmd+S`'s two-press overwrite: a conflict now *asks*, and can answer "reload"
too, which the two-press idiom had no way to express.

**`Cmd+Q` needed an AppKit fix to work at all** (`macos.rs`). winit builds the
macOS application menu, and its Quit item is wired to `terminate:` with `Cmd+Q`
as the key equivalent. Menu key equivalents are matched before the event reaches
the key window, so the process died before any of our code ran — no
`CloseRequested`, no keystroke, no chance to ask. Measured, not assumed: with a
dirty buffer, `Cmd+Q` killed the process outright.

The fix is one selector — retarget that item to `performClose:`, which goes down
the responder chain to the key window and takes exactly the path the window's
close button already takes. Deliberately *not* done by replacing the application
delegate to implement `applicationShouldTerminate:`: winit owns that, and taking
it over would put us inside its event handling for no extra benefit. It runs
once from `update`, because the menu doesn't exist until winit has finished
launching and AppKit state must be touched on the main thread.

**Testing these needs a screenshot, not the accessibility API.** With no parent
window `rfd` shows message dialogs via `CFUserNotificationDisplayAlert`, which is
owned by another process — System Events reports zero windows and zero sheets on
ours, which looks exactly like "the dialog never appeared". It had appeared. File
panels differ: those are sheets on our own window and *are* visible to System
Events. Passing a parent handle (via `iced::window::run`) would make the message
dialogs sheets too; worth doing if their look ever matters.

#### The prompt (`State::prompt`)

One `Prompt` serves find and goto-line — a single line of text with a label and an
optional note, in a bottom row that exists only while it's open. Two lookalike
inputs would have been two sets of the same bugs. (Save-as was a third until the
native panel replaced it.)

**It's a real `text_input`, not a hand-rolled line.** Caret, selection, arrow
keys, `Cmd+V` and IME all come from iced. Colors still come from `Palette`, and
the border is zero-width — the row's presence is the affordance.

**This forced the key subscription from `keyboard::listen()` to
`event::listen_with`, and that change is load-bearing.** `keyboard::listen()`
yields only events the widget tree *ignored*, and `text_input` **captures
`Escape`** — it unfocuses and calls `shell.capture_event()`
(`iced_widget/src/text_input.rs`). The failure that produces is worth recognising
because it doesn't look like a focus bug at all:

1. `Escape` is captured, so `update` never sees it and `self.prompt` stays `Some`.
2. The input has nonetheless unfocused itself, so every subsequent key arrives
   *uncaptured*.
3. `update` swallows them all, because a prompt is still open.

The editor appears frozen — typing does nothing — and a *second* `Escape` fixes
it, which makes the whole thing look intermittent. It was found by driving the
real UI with System Events, not by reading the code.

The cost of receiving captured events is that `update` must not re-handle what
the input already did. Hence the prompt arm runs **first** and claims exactly
three keys: `Escape` (close), `Enter` / `Shift+Enter` (confirm / reverse), and
`Cmd+Q` (so an open prompt can't trap the app). Anything else — characters,
`Backspace`, `Cmd+V` — belongs to the input, and an arm added for it here would
apply it twice. The `text_input` is deliberately given **no `on_submit`**, so
plain and shifted `Enter` can be told apart.

Per-prompt behavior:

- **Find** searches as you type, always from `Prompt::origin` — the caret position
  when the prompt opened — so editing the query re-searches the same span instead
  of walking forward through the file one keystroke at a time. `Enter` searches on
  from the *current match* instead (forward from its end, backward from its
  start), or it would return the same hit forever. Matches are shown by selecting
  them, so `Escape` then typing replaces the match. Prefills from the selection.
- **Find next** (`Cmd+G`) repeats `State::last_query` with no prompt open, which
  is why the query is recorded inside `search` rather than at its call sites —
  every route into a search then leaves `Cmd+G` working. With no query yet it
  opens the find prompt, rather than being a key that does nothing the first time
  you press it. `Cmd+G` also advances *while* the prompt is open, so it isn't
  swallowed as "not one of my keys".
- **A promptless search needs somewhere to report a miss**, so `Cmd+G` with no
  prompt open raises an *alert*, naming the query — with no prompt on screen
  nothing else says what was searched for, and a silent no-op reads as a broken
  key. A miss *with* the prompt open is `Prompt::note` instead: that one arrives
  while typing, and a dialog between keystrokes would be unusable.
- **Goto line** is 1-based and clamps.

`Buffer::save_as` adopts the *target's* mtime before writing. Without that,
`save`'s changed-on-disk guard rejects every save-as onto an existing file — the
buffer has never read it, so the mtimes can't match. It also re-derives the syntax
and drops the whole highlight cache, since the extension may have changed and
every line's parse state was computed under the old grammar.

**Search folds case one char at a time** (`fold_char`), never `str::to_lowercase`.
Full Unicode lowercasing can change a string's *length* — `İ` becomes two chars —
and every match column after such a character would be wrong, since `find`
returns positions into the *original* line. Smart case is the Sublime rule: an
all-lowercase query is case-insensitive, any uppercase makes the whole query
exact.

#### Markdown read mode (`core::markdown` + `gui::read`)

`Cmd+Shift+M` on a `.md`/`.markdown`/`.mdx` buffer swaps the grid's source for a
rendered one. Same widget, different `GridSource` — the editor pane isn't a
second renderer. The gutter disappears with it: there are no source line numbers
to show and nothing to fold.

**Markdown opens rendered.** `open_path` calls `set_read_mode` on a fresh load —
which gates on the extension itself, so nothing else needs checking — because a
markdown file is a document first and source second. `Cmd+Shift+M` gets to the
source.

Two carve-outs, both because they mean "show me the source":

- **A requested line opts out.** `sacrament NOTES.md:42` asks for source line 42,
  and read mode has no such thing — its rows are *rendered* rows, which don't
  correspond to source lines at all. Honoring the line means showing the source.
- **Session restore is untouched.** It reproduces the mode you left
  (`SessionBuffer::read_mode`), rather than reapplying the default over it. Note v1
  writes `read_mode = false` unconditionally, so a session written by v1 restores
  markdown in edit mode — correct, since that's what the session says.

**The renderer does not wrap, and that is the whole point of the port.** v1
wrapped inline text inside `markdown.rs` and handed finished lines to a
`Paragraph` that did no wrapping of its own, so wrapping only happened where the
renderer remembered it — anything emitted whole ran off the edge and was clipped.
That's why word wrap looks broken in v1's read mode.

Here a block becomes **one logical `markdown::Line`**, however long, carrying the
`indent` its continuations should hang under. `gui::read::visible_rows` wraps it
with `text::wrap_line` — the same function the editor uses on source code. Wrap
is a property of the layout, so it can't be missed for a particular construct,
and a resize re-wraps without re-parsing.

`render` still takes a width, because some blocks are genuinely width-shaped: a
rule spans the pane, a fenced block is padded into a slab, table columns scale to
fit. `Buffer::ensure_rendered` caches the result against `(width, revision)`, so
scrolling costs nothing and only a resize or an edit re-renders.

The mode itself persists: `SessionBuffer::read_mode` is `#[serde(default)]`, so a
session written by v1 — or by a build predating it — still loads. v1 writes
`false` unconditionally; it has read mode but has never restored it, and it's
frozen.

**Read scroll is its own state** (`read_scroll_row` / `read_scroll_seg`), not
`scroll_row`. v1 reused the editor's field, which silently changed its meaning by
mode — in read mode it indexes *rendered* rows, which usually outnumber the
source lines — so anything feeding it back into a source-line calculation was
wrong by construction. That's the root of v1's mouse-drag hole.

Read mode still **scrolls**, so the arm that swallows editor gestures while
reading is restricted to press/drag/release. Matching `_` there ate the wheel.

**Read-only is enforced at the routing layer**, not inside `edit_key`: the
`Focus::Editor if self.reading()` arm takes navigation and drops the rest, mouse
gestures are swallowed, and paste is gated too. v1 gated its key handler and left
`Event::Paste` free to edit the source behind the rendering — its own notes call
that out, and doing it one level up covers every editing path at once.

The paste gate is the one that has to be *stated separately*, and this file claimed
it before it was true. `Message::Pasted` doesn't come through `keymap`, so the
`reading()` key arm never covered it, and `Cmd+V` in read mode edited the source
until the gate was added explicitly (see "Sections"). "One level up" only covers
paths that actually pass through that level.

Colors are `Slot`s, so `[theme]` drives them. Strikethrough renders as muted text:
`Emphasis` has no strikethrough and a cell grid has nowhere to draw one.

**Tables** (`emit_table`) are the one construct laid out *here* rather than by
the frontend's wrapper, and they have to be: wrapping is per **cell**, inside a
column, and the row-level wrap has no idea where the columns are. A row becomes
as many `Line`s as its tallest cell, each padded to the same boundaries and each
redrawing its ` │ ` separators — so **alignment holds by construction** rather
than by the cells happening to be short enough.

That last clause is the bug this replaced. The old version scaled the columns
down to fit and then never made a cell honour the result: a cell wider than its
column was emitted whole, shifting every separator after it right and pushing the
trailing columns off the pane, where they were clipped and simply lost. Rows with
short cells landed on the grid and rows with long ones didn't, which reads as a
rendering fault rather than as arithmetic.

Four things about the replacement:

- **Columns are water-filled, not scaled.** Each gets its natural width if that's
  under its fair share; only the columns *over* their share are squeezed, and they
  split what the others left. A one-character `#` column therefore keeps its
  column and the prose column absorbs the whole shortfall. One scaling factor is
  the wrong shape — it takes width from the columns that have none to give while
  leaving the wide one still too narrow, which is precisely how the old output
  came apart.
- **Squeezing stops at `MIN_COL_WIDTH`**, below which the table exceeds the pane
  and is reached by `read_scroll_col`. Rows stay aligned because they're still a
  grid. A table that already fits is left at its natural width — stretching a
  two-column table across a wide pane is worse than leaving it alone.
- **A cell is capped at `MAX_CELL_ROWS` and elided with `…`.** Without it one cell
  holding a paragraph makes its row taller than the pane, and the rows either side
  of it are no longer visible together — which is the whole reason to draw a table.
- **`Tag::Table`'s alignment spec is honoured**, so `---:` right-aligns. A header
  keeps its own left edge regardless: a title over the left edge of a column of
  numbers reads better than one pushed right with them.

`slice_spans` is what keeps a wrapped cell styled — it cuts the cell's spans by
the char range `text::wrap_line` returned, so the second row of an emphasised cell
is emphasised too.

**Nothing the app styles for itself is bold.** Heading level is carried entirely
by colour, which separates them far better than weight does in a 16-colour
scheme. `**strong**` renders *italic* — with bold gone, it's the only attribute
left that isn't already spoken for, since underline belongs to emphasis and
links; a brighter colour isn't an option either, because in a typical theme
`bright_white` is the same value as `foreground` (both `#ebdbb2` in Gruvbox) and
strong text would vanish into the body. `markup.bold` in the *highlighter* is
italic for the same reason, so the editing and reading views of the same
`**text**` agree.

`markdown::Style` has no `bold` constructor at all, so this is a property of the
type rather than a rule to remember, and `nothing_the_renderer_emits_is_bold`
pins it from the outside. Bold arriving from a *shell* is untouched — that's
another program's output, not our styling.

#### Code folding

A row heads a block when the next non-blank line is indented further by **at most
one level**, and the block runs to the last line indented past it. Blank lines don't
end a block — a gap between two indented statements is still inside it — but
trailing blanks aren't swallowed either, so folding a function doesn't eat the
space before the next one.

**The ceiling is stricter than v1, deliberately.** v1 folds on any increase, so
every wrapped call aligned under its open bracket grows a chevron:

```text
let x = compute(a,
                b);      <- deeper, but not a block
```

A multiple of the level isn't enough either — that continuation sits 16 columns
in, a clean multiple of 4.

It's a **ceiling, not an exact match**, because one file legitimately has more
than one step. Editing a 4-space file with `indent_with_tabs` and `tab_width = 2`
writes 2-column indents beside 4-column ones, and demanding exactness meant newly
typed functions never got a chevron at all.

The ceiling is a **constant** (`MAX_INDENT_STEP`, 8 columns, floored at
`tab_width`), not measured from the file. Measuring it was tried and reverted
twice, and the failure is worth remembering: any statistic over the text is a
moving target. Taking the smallest step let one odd line redefine the level;
taking the most common one meant that as tab-indented lines came to outnumber the
4-space ones, the level shrank to 2 and functions that folded a moment ago
stopped. A file reorganising itself under you is worse than a rule that's
slightly too permissive.

Eight columns works because no indentation convention exceeds it, while bracket
alignment in real code lands much deeper — `compute(` puts its continuation
sixteen columns in. Alignment shallower than eight columns still reads as a
block; that's the accepted cost.

Known limit: a method chain indented one level (`.foo()` under its receiver)
still reads as a block. Telling that apart needs the grammar, not the indentation.

**The chevron is clickable** — `Gutter::on_fold` publishes the line, and the
hit-test claims only column `digits + 1`. Clicking a line *number* deliberately
does nothing: a gutter that folds wherever you touch it is one you can't click
without consequence.

**Folds are metadata about visibility, never about content.** `lines`,
`highlights` and `line_state_before` are untouched; a test asserts `to_text()` is
identical before and after folding everything.

**The integration point is `visible_rows`, and only `visible_rows`.** Because it
is the single producer of the screen-row → file-line mapping, teaching it to skip
hidden lines makes the gutter, click hit-testing (`screen_to_doc`), the caret and
the line numbers all fold-aware at once. Only three other places walk lines in
document order — `move_screen_row`, `scroll_forward_one`, `scroll_back_one` — and
each swapped `row ± 1` for `next_visible_line` / `prev_visible_line`. That's the
whole of it; v1 had to chase this through every row-walking call site.

`next_visible_line` jumps to `fold.last + 1` rather than stepping, so a fold over
a thousand lines costs one comparison per screen row instead of a thousand.

Three rules worth knowing:

- **The caret can't be left inside a collapsed block.** `settle_after_fold` pulls
  it up to the header, and does the same for `scroll_row`. Without it the caret
  draws nowhere and every subsequent movement starts from a row that isn't on
  screen.
- **An edit that touches a folded block drops the fold** rather than tracking it
  through the change (`adjust_folds_for_edit`, called from `replace` — so undo and
  redo get it for free, since both go through the same primitive). Guessing where a
  block still ends after its body was rewritten is how folds start hiding the wrong
  lines; losing a fold is the cheap mistake. Folds entirely after an edit just
  slide by the line delta.
- **`fold_all` takes outermost blocks only**, stepping past each block's body, so
  the list never contains a fold nested inside another one it already hides.

Folding from *inside* a body works (`enclosing_fold_head` walks outward to the
header), because the caret is rarely parked on the `fn` line.

**The binding is matched on the physical key**, not the character. With Option
held macOS composes `[` and `]` into `“` and `‘`, so matching the character would
bind the US layout only — exactly the class of bug v1's `apply_shift` table
existed to paper over. `Message::Key` carries `physical_key` for this.

The chevron (`▾` open, `▸` closed) draws in the gutter column that was reserved
for it all along, and goes through the same coverage check the grid uses — neither
arrow is in every monospace face, and an invisible chevron is worse than none since
it would hide that a block can be folded at all.

#### Line-wise commands (comment, indent, outdent)

All three share `Buffer::line_range` — the selected rows, or the caret's row. A
selection ending at **column 0 doesn't include that last row**: dragging down to
the start of a line reads as "not this one", and indenting it would be a surprise.

**Each is one `edit`, not one per line** (`rewrite_lines` replaces the whole span
in a single call). Per-line edits would be per-line *undo steps*, and commenting
ten lines has to come back with one `Cmd+Z`; it also means one highlight splice
instead of ten. The caret and the selection anchor are carried across by a
per-row shift, so they stay on the same character rather than the same column.

**Commenting inserts at the shallowest indent** among the non-blank lines, so a
block keeps its shape instead of every marker landing at column 0. Blank lines are
skipped — they're neither commented nor evidence that the block isn't. A block is
uncommented only when *every* non-blank line already starts with the marker;
otherwise the rest get commented, which is what makes the key a toggle rather than
a coin flip. Uncommenting removes one following space if there is one, undoing
exactly what commenting added.

`outdent_selection` returns without recording anything when no line has
indentation to give up — an empty undo step would otherwise swallow a later
`Cmd+Z`. The marker comes from `core::highlight::line_comment_for`, which was
already in `core` and used only by v1; a syntax it doesn't know reports why rather
than doing nothing.

**Word movement treats a line break as a separator to cross**, not a wall
(`Buffer::char_at` reports the line end as `'\n'`). Stopping at every end of line
is the obvious wrong behavior and `word_movement_crosses_line_ends` pins it.

#### Spike results (release build, macOS, 18 MB flood via `cat`)

Reproduce with `SACRAMENT_METRICS=1 SACRAMENT_SPIKE_CMD='cat <bigfile>' cargo run --release -p sacrament-gui`.

| config | throughput | worst parse | footprint |
|---|---|---|---|
| wgpu, 10k scrollback, message-per-read | 0.8 MiB/s | 6.9 ms | — |
| wgpu, 10k, coalesced | 7.1 MiB/s | 1.1 ms | — |
| tiny-skia, 10k, coalesced | 16.8 MiB/s | 0.6 ms | — |
| **tiny-skia, 2k, coalesced** | **~18-20 MiB/s** | **0.5 ms** | **~39 MB** |

**Measure footprint, not RSS.** The first pass through this table used RSS and
reported ~117-160 MB, which was wrong in a way worth remembering: RSS counts
every resident page including shared system frameworks and file-backed mappings
(cosmic-text memory-maps font files), so it wildly overstates a GUI app. The same
process reads ~39 MB physical footprint against ~82 MB RSS. Footprint is the
dirty, process-owned memory and the only number comparable to v1.
`metrics.rs::resident_bytes` uses `proc_pid_rusage(RUSAGE_INFO_V2)` →
`ri_phys_footprint` for exactly this reason — don't "simplify" it back to
`proc_pidinfo`.

Verdict: iced is viable. The grid draws correctly inside `pane_grid`, and CPU
returns to **0% at idle** — it's genuinely event-driven, no repaint loop. What the
numbers actually taught us:

- **The VT parser was never the bottleneck** (~9µs per batch). The runtime
  overhead of one message per PTY read was, which is why `pty.rs` coalesces. This
  is the Elm-architecture cost being real but entirely fixable.
- **tiny-skia beats wgpu here, 2.4×**, and halves the binary (5.3 MB vs 11 MB).
  A cell grid is axis-aligned quads and glyphs, so there's nothing for a GPU
  pipeline to win back against its per-frame overhead. `default-features = false`
  in `crates/gui/Cargo.toml` is what selects it — note it also needs an explicit
  executor feature (`thread-pool`), or iced fails to compile.
- **Memory is a non-issue.** ~39 MB footprint under load. The scare in the first
  pass was a measurement error (RSS vs footprint, above), not a real cost. What
  *is* real and tunable: scrollback is `lines × cols × ~32B` of
  `alacritty_terminal` `Cell`, so the 10k default is worth keeping at v1's 2k
  unless there's a reason. Renderer choice barely affects memory either way.

Already gone versus v1, and not to be reintroduced: kitty-protocol negotiation,
the `apply_shift` US-layout table (iced hands over composed text with IME and
dead keys already applied), and the CSI leak guard.

## Overview

`sacrament` is a single-binary terminal text editor written in Rust, built on `ratatui` + `crossterm`. It uses `syntect` for syntax highlighting (no themes — ANSI 16-color mapping only, so the terminal palette drives colors) and `pulldown-cmark` for an optional markdown read-mode renderer. The window is a three-pane workspace: editor (top-left), a bottom shell pane, and a right-side shell pane. Each shell pane holds its own tabs of PTY-backed shells (`portable-pty` + `vt100`).

It also doubles as a review surface for agent-written code: it shells out to `git` for an inline diff view and per-line change-bars (`crates/core/src/git.rs`), runs user-configured linters and shows diagnostics in the gutter (`crates/core/src/lint.rs`), and — via a Claude Code hook (`.claude/settings.json`) — surfaces files an agent edits as "unreviewed" background tabs.

## Commands

Workspace root has no package, so `-p` (or `cargo run --bin`) is required.

- `cargo build` — build everything; `cargo build -p sacrament-tui` for just v1
- `cargo run -p sacrament-tui -- <file>[:line]` — launch v1 and open a file; `:N` suffix jumps to line N
- `cargo run -p sacrament-tui` — launch v1 with restored session (or empty untitled buffer)
- `cargo run -p sacrament-gui` — launch v2
- `cargo clippy --workspace --all-targets` — lint everything
- `sacrament --review <file>` — open a file as an unreviewed background tab in a *running* instance (silent no-op if none is running, and never boots a server); this is what the Claude Code hook calls, and since the rename it reaches v2 by default

Install both side by side with `scripts/install-gui.sh` (v2) and
`scripts/install-gui.sh --tui` (v1) — different binary names (`sacrament`,
`sacrament1`), so they coexist. The script passes `--target-dir target` so an
install reuses the workspace's own artifacts; without it every install is a cold
build of iced and its whole tree.

**`scripts/bundle-mac.sh` builds `Sacrament.app`** and installs it to
`~/Applications` (no admin rights needed, and Spotlight and the Dock treat it the
same as `/Applications`). The bundle exists for one reason: on macOS an app's
icon comes from its `.app`, never from the window — winit doesn't support window
icons on macOS at all — so `assets/icon.png` does nothing visible until
`iconutil` has turned it into an `.icns` inside a bundle.

Two details that are easy to get wrong and hard to diagnose:

- **`-psn_…` must be tolerated in argv.** macOS passes a process-serial argument
  to bundled apps on some launch paths, and `parse_args` rejected unknown flags
  with `exit(2)` — so the Dock icon would launch *nothing* while the same binary
  run from a shell worked perfectly. `the_finder_process_serial_argument_is_ignored`
  pins it.
- **The `@2x` iconset entries aren't optional.** Without them the Dock scales the
  1x art and it looks soft on any modern display. The script also `touch`es the
  installed bundle, because the Finder caches icons per bundle path and otherwise
  a changed icon appears not to take until logout.

The bundle carries its own copy of the binary, so it and the `$PATH` install are
updated separately — but both reach the same running editor through the same
socket, so `sacrament foo.rs` in a terminal opens a tab in the window launched
from the Dock.

**v2 is the daily driver as of the cutover.** `sacrament` is v2; v1 is installed
as `sacrament1` and still builds. Nothing is deleted. The hook needs no override
now that the script's default resolves to v2.

**The app ids did not change with the binaries** (`APP_TUI = "sacrament"`,
`APP_GUI = "sacrament2"`). They name the socket and the session file, not the
executable, and swapping them would be destructive rather than tidy: `APP_GUI`
becoming `"sacrament"` would point v2 at `sacrament-session.toml` — v1's file —
so v2 would restore v1's tabs and shells over its own, and both would then
contend for one socket. `/tmp/sacrament2-$USER.sock` belonging to a binary called
`sacrament` is the cost of not doing that.

352 tests (`cargo test --workspace`): 123 in `core`, 229 in the gui — buffer
mutation and undo, terminal reflow, the key map, fonts, block geometry,
`work`'s worktrees against real git repositories, and
`theme_guard`. v1 has
none, and getting any would mean standing up a `Buffer` first. Still untested and
worth covering next, all pure functions: `git::parse_hunks`, `lint::parse_output`,
and `protocol::Request::{parse,encode}`.

There is no lint config for *this* repo beyond `cargo clippy` defaults — note the in-editor lint feature (`Alt+L`, `[lint.linters]`) is a separate, user-configured thing, unrelated to how you'd lint sacrament itself.

## Architecture

### Client/server over a Unix socket

`sacrament` runs as a **single shared process per user, per app id**. On launch, `main.rs` first tries to connect to `paths::socket_path(APP_TUI)` — `/tmp/sacrament-$USER.sock` — via `client::try_send_open`. If a server is already running, the new invocation just sends an `OPEN` request and exits; the already-running editor pops the file up as a new tab. If no server exists, the current process becomes the server (`server::run`).

This is why opening files from other terminals "joins" the live session rather than spawning a second editor. v2 uses `APP_GUI` and therefore its own socket, so the two versions never join each other's sessions.

- `core/protocol.rs` — line-framed text protocol (`OPEN <path>\t<line>\t<syntax>\t<review>\n` → `ok\n` / `err <msg>\n`); the trailing tab fields are optional and positional, and the literal `review` token marks an agent/tool open (threaded through `RemoteCommand::Open` → `Editor::try_load_remote(.., review)`). Frontend-independent — v2 speaks the same protocol.
- `core/client.rs` — fire-and-forget client, falls through to server mode if socket is missing/stale. Takes the app id as its first argument.
- `tui/server.rs` — owns the listener thread + the UI event loop; incoming remote commands arrive via `mpsc::Receiver<RemoteCommand>` and are applied between frames. The listener half is reusable in principle; the event loop is not, so `RemoteCommand` lives here rather than in `core`.

### Event loop quirks (crates/tui/src/server.rs)

Three non-obvious pieces of input handling live in `event_loop`:

1. **Kitty keyboard protocol** (`DISAMBIGUATE_ESCAPE_CODES | REPORT_ALL_KEYS_AS_ESCAPE_CODES`) is pushed on startup. This is what makes modifier combinations like `Ctrl+Shift+S`, `Ctrl+Shift+Z`, `Cmd+C`, `Cmd+Option+[` reach us with their full modifier bitmask. Terminals without kitty support fall back gracefully (`Alt+S`, `Ctrl+Y`, etc.).
2. **CSI leak guard** (`is_csi_intro` / `drain_escape_tail`): when crossterm occasionally fails to parse an SGR mouse report (commonly during fast mouse-wheel scrolling), the raw bytes leak through as `Esc` + `[` + `<digits;digits;digits[Mm]`. Without the guard, `Esc` clears selection and the rest gets typed into the buffer. The guard detects `Esc` immediately followed in the same poll batch by `[` or `O` and drains chars up to the CSI terminator (ASCII letter or `~`). Same-batch is what separates leaked sequences from a user typing `Esc` then `[`.
3. **Shell output drain**: each frame calls `editor.drain_shell_output()` *before* `event::poll`. The poll timeout is 20ms (down from the original 50ms) so shell output feels live. PTY reader threads push bytes onto an `mpsc` channel; `drain_shell_output` pulls them off, feeds them to the right shell's `vt100::Parser`, extracts OSC 7 cwd updates, and polls each shell's process cwd via `proc_pidinfo` (macOS) / `/proc/$pid/cwd` (Linux) so labels track `cd` even without OSC 7.

### Modifier unification

In `handle_key_normal` / `handle_key_prompt` the `ctrl` flag is `CONTROL || SUPER`. Every `Ctrl+X` shortcut also responds to `Cmd+X` on macOS. Bracketed paste (`EnableBracketedPaste`) is also on so `Cmd+V` routes through `Event::Paste` in addition to the key binding.

### Input dispatch order

`Editor::handle_key` is a fixed three-layer funnel; a binding added at the wrong layer is silently shadowed by an earlier one.

1. `try_handle_global_key` — wins everywhere, including inside a prompt and while a shell has focus. Matches only `ctrl && !shift`: `Ctrl+1/2/3` (pane focus) and `Ctrl+Q` (quit). A shell therefore never receives these four.
2. If focus is a shell pane: `try_handle_shell_reserved` (`Ctrl+Shift+T`/`W`, Cmd or `Ctrl+Shift` copy/paste, `Alt+1..9`), then everything else goes to `shell::key_to_bytes` and out to the PTY.
3. If focus is the editor: `handle_key_normal` or `handle_key_prompt` by `Mode`. `handle_key_normal` then checks the `Alt+M` / `Alt+D` toggles, early-returns into `handle_key_read` for any non-Edit `view_mode`, and only then reaches the `Alt`-prefixed and `(ctrl, code)` tables.

Consequences worth knowing: the `Ctrl+Q` arms in `handle_key_normal` and `handle_key_read` are unreachable duplicates of layer 1. Because the read-mode early return precedes the `Alt` block, `Alt+1..9` (buffer switch), `Alt+S`, and `Alt+B`/`Alt+F` don't work in Read/Diff mode. And mouse (`handle_mouse`) and paste (`handle_paste`) are separate entry points that bypass all three layers — they do their own focus and mode checks, imperfectly (see the markdown read-mode notes).

### Editor core (crates/tui/src/editor.rs)

`Editor` owns a `Vec<Buffer>` plus a single `active` index. Each `Buffer` has its own text, cursor, scroll, undo/redo stacks, and highlight cache. Key subsystems:

- **Undo/redo** stores full `Snapshot`s (text + cursor + dirty + folds). Consecutive character inserts coalesce into one step via `last_edit: Option<EditKind>`. `MAX_UNDO = 500`.
- **File watching** uses `notify` with one watcher shared across buffers. `reload_if_changed` compares mtime and skips reloads for the buffer's own saves by tracking `known_mtime`.
- **Tab rendering** is a single horizontal row across the top of the editor column, rendered by `render_tab_bar` as one `Paragraph` with a horizontal `scroll((0, tabs_scroll))` offset (measured in columns, not tab indices). Active tab is white text, inactive tabs are gray. Dirty marker is a `•` in light yellow after the name. Active tab auto-scrolls into view (`ensure_active_tab_visible`) only on the frame the active buffer changes — tracked via `tabs_scroll_anchor: Option<usize>` — so manual mouse-wheel scrolling on the tab strip isn't reverted on subsequent frames. `clamp_tabs_scroll_for_buffers` runs every frame to keep `tabs_scroll` within bounds when the buffer list shrinks. Mouse-wheel over the row scrolls `tabs_scroll` horizontally without changing the active buffer. Click-hit testing goes through `buffer_tab_at_column` / `buffer_tab_drag_target` which walk the same width arithmetic as the renderer (`buffer_tab_width` = name + `" •"` if dirty + `" ◇"` if unreviewed + trailing space). The cyan `◇` marks a buffer touched by an external tool and not yet viewed (`Buffer::unreviewed`), set by a review-open and cleared by `mark_active_reviewed` whenever the user makes a tab active (`switch_to` / `next_buffer` / `prev_buffer`).
- **Tab characters** are expanded to spaces at render time via `char_display_width(c, vis_col, tab_width)`, which snaps to the next multiple of `tab_width`. Cursor math (`char_idx_to_vis_col`, `vis_col_to_char_idx`) uses the same function so click/arrow positions stay aligned.
- **Layout**: the window splits horizontally into a **left block** (`Fill(6)`) | 1-col gap | **right shell pane** (`Fill(4)`). The left block splits vertically as: 1-row tab bar | 1-row gap | editor body (`Min(1)`) | 1-row gap | 26-row bottom shell pane. The editor body's inner layout is gutter + text area; the gutter (`render_gutter` / `gutter_width`, kept in lockstep) is `[number][space][chevron]` plus an optional `[lint glyph]` column (when a linter is configured for the buffer *or* it already carries diagnostics) and `[change-bar]` column (when the file is in a git repo), then a trailing space — so it ranges from `digits + 3` (plain file) to `digits + 5` wide in Edit mode, and is `0` in the read-only view modes. `gutter_overlays(buf) -> (lint, change)` is the single source of truth for which overlay columns are present. An optional 1-row status strip sits at the bottom of the editor column (not the window) and appears when there's a prompt, a transient status, or a lint diagnostic on the cursor's line (`render_status_line` shows that diagnostic's message, colored by severity, when no transient status is set). No permanent status bar. Each shell pane splits vertically as: 1-row tab strip | 1-row gap | body.

### Markdown read mode (crates/tui/src/markdown.rs + Buffer::view_mode)

Every `Buffer` carries a `view_mode: ViewMode` — `Edit`, `Read`, or the `Diff` variant covered in the next section. `Alt+M` (and `Ctrl+Shift+M` as a fallback for terminals that don't surface `Alt`) toggles it on markdown buffers — detected by extension (`md` / `markdown` / `mdx`); non-markdown buffers get a "not a markdown file" status and stay in edit mode. Toggling resets scroll + cursor to the top (the source-line ↔ rendered-line mapping isn't preserved) and clears the selection anchor.

In read mode (and the diff view, which reuses the same read-only plumbing — the gates below are written `view_mode != ViewMode::Edit`, not `== Read`):
- `render_body` short-circuits to `render_read_body`, which calls `markdown::render(&text.join("\n"), width)` to get a `Vec<Line<'static>>` and renders it as a single `Paragraph` with `scroll((scroll_row, 0))`. Read-mode scroll is plain row scroll (no segment/visual-column tracking) — wheel events and arrow keys bump `scroll_row` directly, and the only clamp happens at render time, against `lines.len().saturating_sub(height)`.
- **`scroll_row` changes meaning in these modes.** In Edit mode it's a row index into `text`; in Read/Diff it's a row index into the *rendered* line vector, which is usually longer than the source (wrapping, blank separators, code-fence labels, table rules). So a read-mode `scroll_row` can legitimately exceed `text.len()`, and anything that feeds it back into a source-row calculation is wrong by construction. This is the root of the mouse-drag hole noted below.
- `gutter_width` returns 0 (no line numbers).
- `place_cursor` returns early so no cursor is drawn.
- Mouse *clicks* in the text area only move focus — the `Down(Left)` arm in `handle_mouse` checks `view_mode` before placing a cursor. The `Drag(Left)` arm does **not**: it still calls `screen_to_doc` and writes `cursor_row` / `selection_anchor`. Because `screen_to_doc` walks `text` from `scroll_row` (see above), a drag in a long read-mode buffer can leave `cursor_row` past the end of `text`. Nothing in the read-only paths dereferences it, and every route back to Edit mode (`toggle_view_mode` / `toggle_diff_view`) resets the cursor to 0 — but see the paste hole below for how it escapes.
- `handle_key_read` (called from `handle_key_normal` after the non-Edit early return) accepts only navigation (`Up`/`Down`/`PageUp`/`PageDown`/`Home` — no `End`), save / save-as / find prompts, tab switching, close, and quit. Everything else is swallowed. Note there is **no copy binding** here: `Ctrl+C` does nothing in read or diff mode.
- **`Event::Paste` bypasses all of this.** Bracketed paste arrives as `Event::Paste`, not a key, so it never reaches `handle_key_read`; `Editor::handle_paste` checks focus and `Mode`, but not `view_mode`, and calls `insert_paste_text` unconditionally. Pasting into a read-mode buffer therefore edits the source (and, if a prior drag pushed `cursor_row` out of range, panics on the `self.text[..]` index inside `delete_range` / `insert_text`). If you add a new `Event::*` handler, gate it on `view_mode` explicitly — the `handle_key_read` funnel won't do it for you.

`markdown.rs` walks `pulldown_cmark::Parser` events with `ENABLE_STRIKETHROUGH | ENABLE_TASKLISTS | ENABLE_TABLES`. It maintains a style stack (so nested emphasis composes), open list contexts (indent + ordered counter), a blockquote depth, and an optional table accumulator that emits a column-aligned table on `TagEnd::Table`. Inline spans are wrapped to the body width via a width-aware `flush` that breaks on whitespace using `unicode-width`. Like the editor, only the 16 ANSI colors are used — though `Event::Code` does pin a `Color::Black` background, the one place the "terminal palette is the theme" rule is bent.

Two deliberate-looking quirks: `Tag::Emphasis` maps to `Modifier::UNDERLINED`, not `ITALIC` (so `*emphasis*` and links render alike, and read mode disagrees with `highlight.rs`, which uses `ITALIC` for `markup.italic`). And `emit_table`'s proportional column scaling only shrinks *padding* — it never truncates cell spans, so a table whose natural width exceeds the pane still overflows and gets clipped by the `Paragraph`. Note v2 has a function of the same name that no longer works this way; this paragraph describes v1's, which is frozen with the bug in it.

### Diff/review view, change-bars & linting (crates/core/src/git.rs, crates/core/src/lint.rs)

These are the only one-shot `std::process::Command` uses (everything else is PTY-based); both degrade silently when the tool or repo is absent.

**git (`crates/core/src/git.rs`)** exposes `changed_lines(path) -> GitInfo` and `diff_text(path) -> Option<String>`, each shelling out in the file's parent dir. `GitInfo` pairs a `FileChange` with an `in_repo` flag — a successful `git diff` (exit 0) doubles as the "this path is inside a work tree" signal, and `in_repo` is what gates the gutter's change-bar column (see `gutter_overlays`), so an unchanged file in a repo still reserves the column while a file outside one doesn't. `changed_lines` parses `git diff --unified=0` hunk headers into `(row, ChangeKind)` pairs — `Added` when a hunk only adds, `Modified` when it also removes; pure deletions yield nothing (a bar can't mark a missing line). Untracked/new files, which `git diff` ignores, are detected via `git status --porcelain` and reported as `FileChange::AllAdded`; `diff_text` falls back to `git diff --no-index /dev/null <file>` for them (accepting that command's exit code 1). `Editor::recompute_change_bars(idx)` maps the result into `Buffer::change_bars` (a `Vec<Option<ChangeKind>>` in lockstep with `text`) and runs after load/save/reload/restore and on a review-open.

**Diff view** is a third `ViewMode::Diff`. `Alt+D` (or `Ctrl+Shift+D`) → `toggle_diff_view`, which stashes `git::diff_text` into `Buffer::diff_view` and flips the mode; `render_body` dispatches to `render_diff_body` (sibling of `render_read_body`) which colors `+`/`-`/`@@` lines. The read-only gates listed under markdown read mode (`view_mode != Edit`) cover it.

**Linting (`crates/core/src/lint.rs`)** is on-demand. `Alt+L` (or `Ctrl+Shift+L`) → `Editor::run_lint`, which resolves a command from `config.lint.linters` (keyed by `syntax_name`, then file extension), then spawns a worker thread running it via `sh -c` in the file's dir. Results come back over an `mpsc` channel (`lint_tx`/`lint_rx`, mirroring the shell channel), drained by `drain_lint_results` in the event loop — so a slow `cargo clippy` never blocks the UI. `lint::parse_output` keeps `path:line[:col]: message` lines whose basename matches the linted file (so multi-file output is filtered), classifying a leading `error` as `Severity::Error`, else `Warning`. Diagnostics are a positional snapshot in `Buffer::diagnostics`, **cleared on any line-count edit** (in `adjust_folds_for_edit`) and reset on undo (`restore`) — so they can drift on same-line edits but never desync from `text`. They surface as the gutter `●` (per-row max severity via `diagnostic_severity_at`), the cursor-line status message, and `Alt+]`/`Alt+[` (`jump_diagnostic`, which also lands on the diagnostic's column).

### Highlight cache (crates/tui/src/highlight.rs + Buffer fields)

`syntect` parses line-by-line, where each line's parse state depends on the previous. `Buffer` keeps two parallel vectors in lockstep with `text`:

- `line_state_before[i]`: `Option<LineState>` — the parser state *before* line `i`. `[0]` is seeded on load from the syntax's initial state.
- `highlights[i]`: `Option<Vec<HlSpan>>` — lazily computed per visible line.

`ensure_highlights(up_to)` runs once per frame, walking forward from the nearest live `line_state_before` to fill gaps. On edits, `invalidate_highlights_from(row)` zeroes from `row` onward (the state *before* the edited row stays valid). Every mutation path that changes line count (insert/delete line, join on backspace, `insert_text`, `delete_range`) must `insert`/`drain` the cache vecs alongside `text` — this includes `change_bars` (the git overlay), which follows the identical `insert`/`remove`/`drain` discipline. `restore` (undo) and `load` rebuild all of them to length `n`.

**TOML is highlighted by hand** (`core::highlight::toml_syntax`). syntect's
bundled set ships no TOML — nor INI, cfg or conf — so `Cargo.toml` and the app's
own `config.toml` came out plain. The alternatives were vendoring a third-party
`.sublime-syntax` and turning on syntect's YAML loader, which is a dependency and
a startup parse for one language. TOML is line-oriented enough that a grammar is
more machinery than it needs: the only state crossing a line is whether a block
string is open, which `LineState::Builtin` carries.

`LineState` is therefore an enum — `Syntect` or `Builtin` — and
`Highlighter::seed_for_path` returns whichever applies, so callers seeding a
buffer don't have to know which languages come from where. Colours are picked to
match what `style_for` gives the equivalent scopes, so a `.toml` file sits beside
a `.rs` one without looking like a different program rendered it. A test asserts
syntect still *lacks* TOML, so if it ever gains one, that fails and we prefer the
real grammar.

**Color theme**: `style_for` in `highlight.rs` maps TextMate scopes to `ratatui::style::Color`. Only the 16 named ANSI colors are used, never `Color::Rgb` or `Color::Indexed` — this is intentional so the user's terminal palette *is* the theme. When tweaking colors, edit `style_for` directly; there is no other theme layer.

### Code folding

Indent-based: a row is foldable if its next non-blank line has greater visual indent (tab-aware via `char_display_width`). Blank lines don't terminate a fold body. Detection lives in `compute_fold_end` (free fn); results cached per-buffer in `foldable_at` and invalidated via `foldable_dirty` when any edit changes line count or indentation.

Folds are **metadata about visibility**, not content — `text`, `highlights`, and `line_state_before` stay exactly in lockstep. Collapsed ranges live in `Buffer::folds: Vec<Fold>`. Anything that walks rows in document order (`render_body`, `render_gutter`, `screen_to_doc`, `place_cursor`, `adjust_scroll`, `move_up`/`move_down`, scroll wheel) routes through the visible-row helpers: `next_visible_row`, `prev_visible_row`, `nth_visible_row_from`, `visible_offset`. Edits that change line count call `adjust_folds_for_edit(at, removed, added)` which shifts fold boundaries and drops folds that intersect a deletion.

Fold state is part of the undo `Snapshot` and is also round-tripped through `session.toml` (with clamping on restore).

### Shell panes (crates/tui/src/shell.rs + Editor::{bottom_pane, right_pane})

Each pane is a `ShellPane` = `Vec<Shell>` + `active: usize` + `tabs_scroll`. A `Shell` owns a `portable_pty` master/writer/child plus a `vt100::Parser` (2000-row scrollback) that's the source of truth for what to render. `Editor` also owns a `PaneFocus` (Editor/Bottom/Right) and an `mpsc` channel (`shell_tx` / `shell_rx`) — each `spawn_shell` creates a reader thread that forwards PTY bytes as `ShellMsg::Bytes { id, data }` / `ShellMsg::Exited { id }`.

- **Spawning**: `Editor::new` auto-spawns one shell per pane with the process cwd.
  (v2 differs — see below.) `restore_session` replaces them with the persisted list (one shell per saved `ShellTabSession { cwd }`), falling back to current dir if the saved cwd no longer exists.
- **Rendering**: `render_shell_body` reads cells directly from `vt100::Parser::screen()` and writes them into the ratatui buffer via `vt_cell_style` / `vt_color_to_ratatui`. Only the 16 ANSI colors are emitted — truecolor / indexed beyond 15 collapse to `Color::Reset`, matching the editor's no-RGB rule. Before rendering, `resize_pane_shells` calls `Shell::resize` to sync the PTY + parser to the current body rect.
- **Input routing**: `handle_key` first runs `try_handle_global_key` (Ctrl+1/2/3 switch panes, Ctrl+Q quits). If focus is a shell pane, `try_handle_shell_reserved` handles Ctrl+Shift+T (new tab), Ctrl+Shift+W (close tab), Alt+1..9 (switch tab); otherwise keys go through `shell::key_to_bytes` and are written to the PTY. Cmd/Ctrl+V pastes the clipboard as a bracketed paste sequence (`\x1b[200~` … `\x1b[201~`). Mouse events in a pane body are forwarded as SGR mouse reports via `shell::mouse_to_bytes` *only* when the shell's vt100 parser reports a non-`None` `mouse_protocol_mode` — outside that, clicks just move focus.
- **Cwd tracking**: two paths, both handled in `drain_shell_output`. OSC 7 (`\x1b]7;file://host/path\x07`) is parsed by `extract_osc7_cwd` on every chunk of PTY output; as a fallback (and to catch shells without OSC 7), `poll_shell_cwds` calls `query_process_cwd` (macOS `proc_pidinfo PROC_PIDVNODEPATHINFO` / Linux `/proc/$pid/cwd`) and updates the shell's `cwd` + `label` when it changes. The label is the cwd's basename via `derive_label`.
- **Exit**: when the child process exits, `mark_shell_dead` removes the tab and clamps `active`. A pane is allowed to be empty — `Ctrl+Shift+W` on the last tab, or exiting the last shell, leaves `shells` empty and the body renders blank until you click the `+` (intended; see README). Only `restore_session` force-spawns a fallback, so "a pane always has a shell" is **not** a runtime invariant — code touching `active_shell()` must handle `None`.
- **`Shell::alive` is vestigial.** Nothing ever sets it to `false` (exit removes the tab instead), so the `write()` guard never fires and the `" [dead]"` tab decoration in `render_shell_tabs` / `shell_tab_hit_at` / `shell_tabs_total_width` is unreachable. Either drive it from `ShellMsg::Exited` or delete it — don't write new code that trusts it.
- **`TERM` mismatch**: `spawn_shell` sets `TERM=xterm-256color`, which tells TUIs 256 colors are safe, while `vt_color_to_ratatui` collapses everything above index 15 to `Color::Reset`. A program that takes the hint and emits 256-color output renders with default fg/bg instead of a near match. The in-code comment claims the opposite intent; `xterm-16color` would match the renderer.

### Single instance — v2 (crates/gui/src/ipc.rs)

Same shape as v1 and the same wire protocol (`core::protocol`, `core::client`),
so the two versions differ only in plumbing — and never talk to each other, since
the socket path is namespaced by app id (`/tmp/sacrament2-$USER.sock`).

`main` decides before iced starts: try to hand the files to a running server via
`client::try_send_open`; on success the process exits, otherwise it binds the
socket and becomes the server. That ordering is the whole point — a client run
must never open a window.

- **The listener is bound in `main`, not in the subscription.** `Subscription::run`
  takes a bare `fn() -> Stream` that can't capture, the same constraint
  `pty::stream` works around; here the value is passed *in* through a `OnceLock`
  (`ipc::attach` sets it, `ipc::stream` takes it). Starting the thread before the
  window means a request arriving during startup queues rather than being lost.
- **Replies travel back to the client**, rather than being answered `ok` in the
  listener thread. A path that can't be read should fail at the shell that asked,
  not silently in a window that may not be visible. `update` sends the `Response`;
  the client thread waits with a 2s timeout, which only elapses if the UI is
  wedged.
- **The socket file is deliberately never unlinked.** It can't be reliably: `Cmd+Q`
  terminates the process without unwinding, so a `Drop` guard would be skipped.
  `bind_socket` therefore treats an existing path as stale *only if connecting to
  it fails*, since `bind` returns `EADDRINUSE` either way. Getting this wrong is
  invisible in the common path — the handoff attempt removes a stale socket as a
  side effect, so it's only a **bare** `sacrament2` after a `Cmd+Q` that lands on
  the untested branch and silently loses IPC for that session.
- **`--review` is a tool's open** (the Claude Code hook). It never creates files
  and never boots a server — a hook firing where no editor is open does nothing —
  and the tab it opens does *not* become active, because something writing files
  in the background must not yank the cursor out of what you're typing. The tab
  carries v1's cyan `◇` until it's viewed — see "Tab markers".
- **An already-open path reuses its tab** rather than stacking duplicates, which
  an agent touching the same file repeatedly would otherwise fill the strip with.
- **A file that doesn't exist is created before the request is sent**, because the
  server resolves paths with `canonicalize`, which fails on a missing file.

`parse_args` is shared by `main` and `State::new`. That sharing is load-bearing:
`State::new` previously treated *every* argument as a path, so `--syntax=Rust`
became a request to open a file by that name. `-s/--syntax` now reaches
`Buffer::set_syntax_override`, which outranks the extension and survives a
save-as — and round-trips through `session.syntax_override`.

### External changes — v2 (crates/gui/src/watch.rs)

A `notify` watcher keeps open buffers in step with the files they show. This is
load-bearing for the review workflow rather than a convenience: without it an
agent-edited file goes quietly stale, and `Buffer::save`'s changed-on-disk guard
then refuses to write, leaving the editor stuck with no way out.

**The watch set is declared, not managed.** Callers hand over the current list of
open paths and the thread diffs it against what it already watches — the same
shape as the PTY subscriptions, where the live set is derived from state rather
than kept in sync by paired add/remove calls. It's synced from `persist`, since
the events that change which files are open (open, close, save-as) are exactly
the ones that change the session.

**Three outcomes, and the middle one is where v1 is wrong:**

- **Clean buffer, file changed** → reload, keeping the caret and scroll (clamped,
  since the file may have shrunk).
- **Dirty buffer, file changed** → *refuse*, and say so. v1 reloads regardless and
  silently discards the unsaved edits; that's a bug, not behaviour to port.
  `Buffer::reload` returns `ReloadError::Dirty` and nothing is touched.
- **Same mtime** → nothing. This is the common case, because our own save fires
  the same watch event, and it's also why **no debouncing is needed**: after a
  reload the buffer's mtime matches disk, so the duplicate events `notify` emits
  for one write are no-ops.

**A conflict has to be escapable, or the watcher makes things worse.** With
unsaved edits *and* a changed file, plain `Cmd+S` is refused by the mtime guard —
forever. So a refused save arms `State::pending_overwrite`, and pressing `Cmd+S`
again calls `Buffer::save_overwriting`, which adopts the disk mtime and writes.
Same "ask once" shape save-as uses for replacing an existing path. The message
names the escape hatch, because a guard you can't get past reads as a broken key.

The file itself is watched, non-recursively, as in v1. On macOS the backend is
FSEvents, which tracks a *path* rather than an inode, so this survives the
write-temp-then-rename that our own `save` and most other tools use. On Linux
(inotify) a replaced file would need the parent directory watched instead — worth
knowing before assuming this is portable as written.

### Session persistence — v2 (crates/gui + core::session)

**Written when state changes, not only on close** — and that distinction is the
whole point. The first version saved only on `Message::CloseRequested`, which in
practice meant *only when quit via the window button*: on macOS `Cmd+Q` is handled
by AppKit, which terminates the process without the key ever reaching the
application. No `CloseRequested`, no save, and the session file had simply never
been written. Two things hid it — `save_session`'s result was discarded with
`let _ =`, and every test up to that point had killed the app with `pkill`, which
is equally silent.

So `State::persist` is called from the mutators that change what a restore would
reproduce: tab select / close / cycle / reorder, new buffer, shell spawn / close /
select, save-as, and `Event::Started`. All are human-paced, so an immediate ~650
byte write costs nothing measurable, and persistence no longer depends on *how*
the app goes away — a crash or a `SIGTERM` keeps the tabs too. A failed write now
sets `State::failure` rather than vanishing.

**Geometry is the exception, and flushes on mouse-up.** A splitter drag emits a
`PaneResized` per frame, so those handlers only set `geometry_dirty`;
`Message::LeftReleased` writes it. Both gestures that change geometry — dragging a
splitter, resizing the window — end with the button coming up, so this needs no
timer and costs nothing at idle. `CloseRequested` still saves, for the window
button.

Two consequences worth knowing:

- **Every shell tab produces a session entry, even one whose pid can't be read.**
  Mapping with `filter_map` over `sh.pid` silently dropped tabs: a shell spawned
  moments ago has no pid yet (it arrives with `Event::Started`, after the deferred
  spawn), so a session written in that window lost the tab rather than just its
  directory. The cwd falls back to the shell's `start_cwd`, then the process cwd.
  `Event::Started` then persists again, replacing the fallback with the real one.
- **Two instances would stomp each other's session**, which is what the socket
  (below) exists to prevent: `sacrament foo.rs` joins the running window rather
  than starting a rival writer. A bare `sacrament` launched alongside one still
  opens a second window, and that one *will* overwrite the session on its own
  changes.

**Geometry lives here, not in `config.toml`.** Serializing TOML discards comments, so
writing geometry into config would delete the user's own annotations on the first
resize. Config is hand-written preference; session is state the app owns and rewrites.

Pane ratios are tracked in `State::geometry` as `PaneResized` events arrive, because
`pane_grid::State` exposes no getter for a split's ratio — the event is the only place
it can be observed. `split_vertical`/`split_horizontal` are captured from `split()`'s
return value so a `ResizeEvent` (which carries only a `Split` id) can be attributed.

**A new shell starts at `$HOME`**, not in the editor's own working directory.
That would be wherever the app happened to be launched from — the last project,
or `/` when started from the Finder — so the starting directory would depend on
trivia the user can't see. `paths::home_dir` is the one place that decides;
`pty::run` still falls back to the process cwd if it somehow isn't a directory.
Restored shells are unaffected: they go through `Shell::in_dir` with their saved
directory.

**Shell directories persist, not shells.** A PTY isn't serializable, so restore
re-spawns a shell in the saved cwd. That's why `pty::Spawn` bundles the key with a
starting directory rather than putting the directory on `ShellKey`: the key is hashed
for subscription identity, and two shells differing only by directory must still be
two shells. A saved directory that no longer exists falls back to the process cwd.

`Geometry::sanitized` clamps sizes and ratios — the session file is user-editable, so
a stored `0` or `NaN` would otherwise produce a window you can't see or a pane you
can't grab. Every field is `#[serde(default)]`, so a session written by an older build
still loads.

Restore only happens when **no paths were given on the command line** — an explicit
`sacrament foo.rs` means "open this", not "and also everything from last time". Same
rule as v1.

Verified end-to-end by hand-writing a session file and launching: shell cwds via
`lsof`, window size and active-buffer title via System Events, and pane ratios by the
editor's reported column count (73 at `vertical_split = 0.7`, 27 at `0.3`).

### Session persistence — v1 (crates/core/src/session.rs)

On quit, `capture_session` serializes file-backed buffers (path, cursor, scroll, folds) plus each shell pane's tab cwds (`bottom_shells`, `right_shells`, and their active indices) to `$XDG_CONFIG_HOME/sacrament/session.toml` (falls back to `~/.config`). Untitled buffers are dropped; the active index is remapped past them. `restore_session` runs on launch *only* when no CLI path was given. Missing files are skipped silently; cursor/scroll are clamped to the new file bounds in case the file shrank. Shells are re-spawned (not literally restored — PTYs are not persistable); the auto-spawned defaults in `Editor::new` are cleared and replaced. If a pane ends up empty after restore, one default shell is spawned — restore is the *only* place that guarantee is applied (a pane can go empty later; see the shell-pane notes). `scroll_seg` is not persisted, so a restored buffer always lands on the first segment of its saved `scroll_row`.

### Theming (crates/core/src/theme.rs) — v2 only

`[theme]` in `config.toml` holds the 16 ANSI slots plus background, foreground,
cursor, and selection fg/bg, each as a `"#rrggbb"` string. Every field has a
default (Gruvbox Dark), and the table is `#[serde(default)]`, so a partial table
works — set `background` alone and the other twenty keys stay put.

`Rgb` is plain data, deliberately not `iced::Color`: `core` carries no
UI-framework dependency, so the gui crate converts at the boundary
(`palette::Palette::from_theme`). `Rgb::parse` accepts `#rrggbb`, `#rgb`, and
either without the `#`, so a palette pasted from any terminal's config parses
unedited.

**There is no system-wide "macOS terminal palette."** Each emulator keeps its
own, and they disagree. Ranked by how painful they are to read:

| source | format | notes |
|---|---|---|
| Ghostty | `palette = 1=#cc241d`, plain text | trivial — this is what the current theme was imported from |
| Kitty / Alacritty | plain text / TOML | trivial |
| iTerm2 | plist, `Red/Green/Blue Component` reals | easy |
| Terminal.app | base64 **NSKeyedArchiver-encoded `NSColor`** | avoid — needs archive decoding or regex-scraping floats |

Importing is currently a manual step (read the emulator config, write `[theme]`).
An auto-detecting importer is a reasonable future addition; if one is written,
prefer Ghostty/iTerm2 as sources and treat Terminal.app as last resort.

v1 parses `[theme]` and ignores it — it renders through the terminal's own
palette by design. Shared config, one consumer.

`Palette::iced_theme()` derives iced's `Theme` from the same 16 slots so app
chrome (tab strips, the prompt row, splitters) lives in one color system rather
than looking like two apps stapled together.

### Font selection (crates/core/src/font.rs + crates/gui/src/font.rs) — v2 only

`[font]` in `config.toml` carries `family` (optional; unset means the platform
default monospace), `size`, and `line_height`. `line_height` is a multiple of
`size` and *is* the terminal's cell height, so it sets row pitch directly.
`FontConfig::sanitized` clamps size and line-height into renderable ranges —
that's a correctness guard, not taste: a zero size makes a degenerate cell and
divides by zero when deriving grid dimensions from pixel bounds.

Two things here are non-obvious enough to state plainly:

**Unknown family names have no safety net.** `Shaping::Basic` disables font
fallback — that's what makes it cheap — so naming a family that isn't installed
would draw *nothing*. `gui/src/font.rs` therefore validates the name up front
against `fontdb`, which is the same crate and version cosmic-text loads system
fonts through, so the list is exactly what iced can resolve rather than an
approximation from something like `system_profiler`. An unmatched name falls
back to the default monospace and says so, in an alert raised from `State::new`'s
returned `Task` — a bad font is a config error, and it's the one message that has
nothing to queue behind it.

**A resolved family still isn't enough — the weight has to be one the family
ships.** cosmic-text honours a *named* family only through a face whose weight
matches the request **exactly**: `fallback/mod.rs` filters candidates on
`font_weight_diff == 0` before it even looks for the family, so a mismatch
doesn't fall back to a nearby weight of the same font — it leaves the family
entirely and lands on the *system* font, which is proportional. On a grid built
for fixed cells that draws each word tight inside a much wider cell stride, so
the words look normally set and the gaps look enormous. Nothing warns, because
the *name* resolved perfectly.

Measured, not theorised: Cozette Vector declares weight 500 (Medium). Asking it
for `Weight::Normal` drew `.SF NS` at 21.6px per cell against a 12px grid, and
`Weight::Bold` drew Menlo at 15.7px.

So `SystemFonts::weights` reads the weight off the face fontdb resolves, and
`FontSpec::with_weights` adopts it; `variant(bold, ..)` asks for the family's own
bold face or keeps the base weight, never `Weight::Bold` on spec. A face
declaring a weight `iced::font::Weight` can't name exactly (450, say) is
*reported* rather than rounded — rounding is how the wrong font gets on screen.
The consequence for a single-weight font like Cozette is that bold cells render
at regular weight, which is the honest outcome; the alternative was a different
typeface at a different width.

**And glyph coverage is separate again.** Validating
the *name* says nothing about whether the face has the *characters*, and with no
fallback a missing glyph draws as nothing. This is not hypothetical: Envy Code R
covers 48 of the 160 codepoints in U+2500..259F, having the light single-line set
(`─ │ └ ├ ┌ ┴`) but **not** the heavy (`━ ┃ ┏ ┫`), dashed (`┄ ╎`), rounded
(`╭ ╮ ╯ ╰`), half-line (`╴ ╵`) or most partial-block variants — nor `⎿`, `✓`, or
`▏`. A TUI drawing a rounded frame lost its corners silently while its straight
edges rendered fine, which reads as a rendering bug rather than a font gap.

`font::Coverage` closes this: the resolved face's `cmap` is read once at startup
via `ttf-parser` (candidates from `codepoints()` are each confirmed against
`glyph_index`, since a candidate can map to no glyph) and stored as an ASCII array
plus a `HashSet` for the rest — ASCII is nearly every cell and this is consulted
per cell per frame, so the common case must not hash. `GridView` then substitutes
for the uncovered cells (`Glyphs`, below). Three things about it:

- **A fallback run is always one cell.** The fallback font's advance width isn't
  ours, so a multi-cell run drawn in it would drift progressively out of the grid.
  Per-cell `fill_text` at the cell's own x-position is what keeps alignment
  independent of whatever font gets substituted.
- **`Shaping::Advanced` needs no Cargo feature.** iced's `advanced-shaping` /
  `basic-shaping` features only set the *default* value of `Shaping`;
  `iced_graphics` maps `Shaping::Advanced` to cosmic-text unconditionally. Don't
  add the feature — it would flip the default and silently make every cell
  expensive.
- **Coverage failing open is deliberate.** An unparseable or unresolvable face
  yields `None`, and `FontSpec::can_draw` then answers `true` for everything —
  i.e. exactly the old behavior. The alternative, assuming nothing is covered,
  would route every cell through `Advanced`.

Measured: no throughput change (18.3 MiB/s on an 18 MB flood whose every line
carries four fallback glyphs, against ~18-20 baseline; feed worst 0.4 ms).
Footprint rises ~39 → ~50 MB, which is the substituted faces cosmic-text loads.
Bold and italic are judged by the regular face's coverage — a font shipping box
drawing in one weight but not another is pathological, and checking all four
means parsing all four.

**`Family::Name` needs `&'static str`.** A family read from config is a runtime
`String`, so `resolve_family` does one deliberate `Box::leak` at startup. The
alternative is threading a lifetime through every widget that draws text, which
buys nothing for a string that must outlive the process anyway. `Coverage` is
leaked the same way, which is what keeps `FontSpec` `Copy`.

`SystemFonts` wraps one `fontdb::Database` because loading system fonts isn't
free and both family validation and coverage need it — previously
`installed_families()` built and dropped its own. It's consumed at the end of
startup by `into_fallback`, which keeps the database alive for the process.

#### Fallback is ours, not cosmic-text's — and that's what keeps emoji out

`Shaping::Advanced` is the only shaping that consults system fonts, so it was
doing the substituting. **It's also where the emoji came from.** cosmic-text's
macOS chain is `.SF NS`, `Menlo`, **`Apple Color Emoji`**, `Geneva`,
`Arial Unicode MS`, walked in order — so any character the first two lack and the
emoji font has is drawn as a colour emoji. Claude Code's bullet is `⏺` (U+23FA, a
*record button* whose Unicode default presentation happens to be emoji), Menlo
doesn't have it, and the result was a cartoon dot in the middle of terminal output.

`font::Fallback` takes the decision back. `GridView::glyphs_for` returns a three-way
`Glyphs`, and it's a strict preference order:

1. `Configured` — the font has it. `Shaping::Basic`, batched into runs. Unchanged.
2. `Family(name)` — the first family in `FALLBACK_CHAIN` that has it, drawn with
   `Shaping::Basic` in that family. Monochrome, deterministic, one cell.
3. `System` — nothing in the chain has it, so `Shaping::Advanced` picks. This is
   the *genuine* emoji path (`🔴`, `✅` and `😀` exist in no text font on macOS)
   and the path for scripts the chain doesn't reach.

So a symbol that merely has emoji *presentation* renders as text in the theme's
colours, and an emoji that only exists as an emoji still renders as one — which is
the rule for the app: never turn something into an emoji, but don't refuse to draw
one that's genuinely in the content.

Measured on this machine, which is where the chain order came from:

| chars | resolves to |
|---|---|
| `⏺ ⏸ ⏹ ⏱` | STIX Two Math |
| `⎿ ⧉` | Apple Symbols |
| `✻ ✽ ✳ ✓ ❯ ⚒ ⚠ ─ ╰` | Menlo |
| `中` | Arial Unicode MS |
| `✅ 🔴 😀` | nothing — `Advanced` |

Four things about it:

- **The colour test is structural, not a name list.** A face carrying `COLR`,
  `CBDT`, `sbix` or `SVG` is refused, so adding an emoji family to the chain can't
  reintroduce emoji, and a renamed font can't sneak past.
- **Lookups are lazy and cached per character.** Resolving one means reading a
  font file — measured at ~2.4 ms for a chain walk that reaches the third entry —
  and reading every chain member at startup would mean paying for the 20 MB CJK
  faces to answer a question nothing has asked. Each character is resolved once;
  after that it's a `HashMap` hit.
- **The chain is ordered monospaced-first**, so a substituted glyph matches the
  grid's width where it can, then symbol faces, then broad text coverage, then CJK.
- **An empty or unresolvable chain degrades to the old behavior**, since everything
  then falls to `System`.

`Family::Name` wants `&'static str`, which the chain entries already are — that's
why they can be handed straight to `iced::Font`.

Ligatures do not form, and shouldn't: they're a shaping feature `Basic` skips,
and a ligature spanning two cells would break grid alignment. Fira Code renders
as its glyphs, not its ligatures — correct for a terminal. Note the fallback path
does use `Advanced`, but only ever on a single cell, where there's nothing to
ligate.

### Config (crates/core/src/config.rs)

TOML at `$XDG_CONFIG_HOME/sacrament/config.toml`. All fields have defaults (`Config::default`), so a missing file is fine. `load` still answers an unparseable file with defaults, but `load_result` keeps the error apart from it — v2 uses that one everywhere (see "Live config reload"), because collapsing "broken" into "defaults" makes a typo indistinguishable from settings that don't work. The `[lint.linters]` table maps a language name (as `syntect` reports it, e.g. `Rust`) or a file extension to a `LinterSpec { command }` template (`{file}` is substituted); it defaults to empty, so `Alt+L` reports "no linter configured" until one is set.

## Dead ends and sharp edges

State that exists but doesn't do what its name suggests. Don't build on it without wiring it up first.

- **`Buffer::external_change`** is write-only. `check_disk` sets it from an mtime comparison every `DISK_CHECK_INTERVAL` (1.5s, active buffer only) and nothing ever reads it — there is no "changed on disk" indicator. Automatic reloads come entirely from the `notify` watcher → `drain_fs_events` → `reload_if_changed` path, which compares mtimes itself. So `check_disk` is currently a `stat` per 1.5s for nothing.
- **`Shell::alive`** — see the shell-pane notes; never set false, so `[dead]` never renders.
- **`shell::arrow(out, letter, shift, alt, ctrl)`** takes `shift` and `alt` but every one of the four call sites passes `false` for both. That's the intended behavior (modifier-encoded arrows leak `;2C`-style garbage into zsh, per the comment above the arrow arms), but the parameters are inert — the collapse is done by the caller, not by `arrow`.
- **OSC 7 detection is per-chunk and stateless.** `extract_osc7_cwd` scans one PTY read at a time, so a sequence split across two reads is missed entirely; its loop bound (`i + MARKER.len() < bytes.len()`) also skips a marker landing exactly at the buffer end. The `poll_shell_cwds` / `query_process_cwd` fallback is what actually makes cwd tracking reliable — OSC 7 is the fast path, not the source of truth.
- **`parse_file_url` percent-decoding is byte-wise**: each `%XX` becomes `out.push(b as char)`, i.e. a Latin-1 codepoint, so a percent-encoded UTF-8 path (`%C3%A9` → `é`) decodes to mojibake. Only reachable via OSC 7 with a non-ASCII cwd.
- **`vt_color_to_ratatui`'s `is_bg` parameter** is threaded through but both branches of the `Color::Default` arm return `Color::Reset` — clippy's `if_same_then_else` flags it. Intentional under the no-RGB rule; collapse it if you ever want distinct fg/bg defaults.

`cargo clippy` is clean of errors and carries ~45 warnings, ~38 of which are `collapsible_if` nudges toward edition-2024 let-chains. Worth knowing before you assume a warning you just introduced is visible in the noise.

## Conventions

- No backwards-compat shims or feature flags. Behavior changes go straight in.
- **The app never renders an emoji it wasn't given.** No emoji in labels, markers,
  messages, comments or docs, and — the part that needs code to enforce — no
  *turning a symbol into one*. A codepoint that merely has emoji presentation
  (`⏺`, `⏸`, `⚠`) is drawn from a monochrome font in the theme's colours; only a
  character that exists nowhere but a colour font is drawn as a colour emoji,
  because at that point it genuinely is one and refusing would just be a blank
  cell. See "Fallback is ours, not cosmic-text's" — the mechanism is a
  monochrome-first chain we control, not a substitution table.
- Terminal palette is the source of truth for colors — don't introduce RGB colors. This applies to the shell renderer too (`vt_color_to_ratatui` collapses `vt100::Color::Rgb` to `Color::Reset`).
- **v2: every color comes from `Palette`, and nothing synthesizes one.** No alpha
  blending, no darkening a theme color to make a variant, no color literals. A
  blend produces a color the user's `[theme]` doesn't contain, which is the same
  violation as hardcoding RGB — it just looks more innocent, which is exactly why
  it slipped in twice. Where a "dimmer" color is wanted use `Palette::dim()` (the
  theme's `bright_black`, which is what slot 8 is for); where something must be
  hidden set it to `background`; a block caret uses reverse video (fill with
  `cursor`, redraw the glyph in `background`) rather than a translucent overlay.

  **This is enforced, not just documented.** `crates/gui/src/theme_guard.rs` scans
  the crate's own sources at test time and fails on the offending patterns, with
  `palette.rs` exempt. If you need a color that `Palette` can't give you, the fix
  is a new named accessor on `Palette` derived from the theme — not an exemption.
- **v2: a `Buffer` shown to the user is built by `empty_buffer`, never by
  `Buffer::empty()` directly.** `buffer.rs` is the model and knows nothing about
  the config, so the bare constructor defaults `tab_width` to 4 and `wrap_width`
  to 0 — anything constructing one has to supply both, and "everyone remembers"
  failed twice. `tab_width` was set at five sites and missed at `new_buffer`, so
  `Cmd+N` drew four-column tabs whatever the config said. `wrap_width` was set at
  *none*: it arrived only from `Message::EditorResized`, which fires on a size
  **change**, so any buffer created after the first layout didn't wrap until the
  window was next dragged — while files opened at startup were fine, which is what
  made it look intermittent. Neither reads as a fault on screen; the file opens,
  typing works, and only the layout disagrees with the config. `State::wrap_width`
  is the single definition of the width, and **this is enforced**:
  `crates/gui/src/buffer_guard.rs` fails the suite if the bare constructor appears
  outside `empty_buffer` (`buffer.rs`'s own tests are exempt — they want the raw
  defaults). Same shape as `theme_guard`, for the same reason: a convention nobody
  can check is one that decays.
- Any code that mutates `Buffer::text` line count must also update `highlights`, `line_state_before`, and `change_bars` in the same step, then call `invalidate_highlights_from` and `adjust_folds_for_edit` (the latter also clears `diagnostics`).
- The gutter has three consumers that must agree: `render_gutter` and `gutter_width` (which both derive `digits` from the line count and ask `gutter_overlays` which of the lint / change-bar columns are present), and the gutter-click hit-test in `handle_mouse` (which locates the fold chevron at `digits + 1`). The chevron sits before the overlay columns, so its position depends only on the line count — but if you change the column order or `gutter_overlays`, revisit all three. Same lockstep rule as the tab-bar helpers below.
- Shell tab hit-testing and rendering share their width math — if you change one of `render_tab_bar` / `render_shell_tabs` or their helpers (`buffer_tab_width`, `shell_tabs_total_width`, `shell_tab_hit_at`), update the others in the same pass or clicks will miss the drawn tabs.
