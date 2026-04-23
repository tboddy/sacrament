# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Overview

`sacrament` is a single-binary terminal text editor written in Rust, built on `ratatui` + `crossterm`. It uses `syntect` for syntax highlighting (no themes — ANSI 16-color mapping only, so the terminal palette drives colors). The window is a three-pane workspace: editor (top-left), a bottom shell pane, and a right-side shell pane. Each shell pane holds its own tabs of PTY-backed shells (`portable-pty` + `vt100`).

## Commands

- `cargo build` — build
- `cargo run -- <file>[:line]` — launch and open a file; `:N` suffix jumps to line N
- `cargo run` — launch with restored session (or empty untitled buffer)

No tests exist yet. There is no lint config beyond `cargo clippy` defaults.

## Architecture

### Client/server over a Unix socket

`sacrament` runs as a **single shared process per user**. On launch, `main.rs` first tries to connect to `/tmp/sacrament-$USER.sock` (`client::try_send_open`). If a server is already running, the new invocation just sends an `OPEN` request and exits — the already-running editor pops the file up as a new tab. If no server exists, the current process becomes the server (`server::run`).

This is why opening files from other terminals "joins" the live session rather than spawning a second editor.

- `protocol.rs` — line-framed text protocol (`OPEN <path>\t<line>\n` → `ok\n` / `err <msg>\n`)
- `client.rs` — fire-and-forget client, falls through to server mode if socket is missing/stale
- `server.rs` — owns the listener thread + the UI event loop; incoming remote commands arrive via `mpsc::Receiver<RemoteCommand>` and are applied between frames

### Event loop quirks (src/server.rs)

Three non-obvious pieces of input handling live in `event_loop`:

1. **Kitty keyboard protocol** (`DISAMBIGUATE_ESCAPE_CODES | REPORT_ALL_KEYS_AS_ESCAPE_CODES`) is pushed on startup. This is what makes modifier combinations like `Ctrl+Shift+S`, `Ctrl+Shift+Z`, `Cmd+C`, `Cmd+Option+[` reach us with their full modifier bitmask. Terminals without kitty support fall back gracefully (`Alt+S`, `Ctrl+Y`, etc.).
2. **CSI leak guard** (`is_csi_intro` / `drain_escape_tail`): when crossterm occasionally fails to parse an SGR mouse report (commonly during fast mouse-wheel scrolling), the raw bytes leak through as `Esc` + `[` + `<digits;digits;digits[Mm]`. Without the guard, `Esc` clears selection and the rest gets typed into the buffer. The guard detects `Esc` immediately followed in the same poll batch by `[` or `O` and drains chars up to the CSI terminator (ASCII letter or `~`). Same-batch is what separates leaked sequences from a user typing `Esc` then `[`.
3. **Shell output drain**: each frame calls `editor.drain_shell_output()` *before* `event::poll`. The poll timeout is 20ms (down from the original 50ms) so shell output feels live. PTY reader threads push bytes onto an `mpsc` channel; `drain_shell_output` pulls them off, feeds them to the right shell's `vt100::Parser`, extracts OSC 7 cwd updates, and polls each shell's process cwd via `proc_pidinfo` (macOS) / `/proc/$pid/cwd` (Linux) so labels track `cd` even without OSC 7.

### Modifier unification

In `handle_key_normal` / `handle_key_prompt` the `ctrl` flag is `CONTROL || SUPER`. Every `Ctrl+X` shortcut also responds to `Cmd+X` on macOS. Bracketed paste (`EnableBracketedPaste`) is also on so `Cmd+V` routes through `Event::Paste` in addition to the key binding.

### Editor core (src/editor.rs)

`Editor` owns a `Vec<Buffer>` plus a single `active` index. Each `Buffer` has its own text, cursor, scroll, undo/redo stacks, and highlight cache. Key subsystems:

- **Undo/redo** stores full `Snapshot`s (text + cursor + dirty + folds). Consecutive character inserts coalesce into one step via `last_edit: Option<EditKind>`. `MAX_UNDO = 500`.
- **File watching** uses `notify` with one watcher shared across buffers. `reload_if_changed` compares mtime and skips reloads for the buffer's own saves by tracking `known_mtime`.
- **Tab rendering** is a single horizontal row across the top of the editor column, rendered by `render_tab_bar` as one `Paragraph` with a horizontal `scroll((0, tabs_scroll))` offset (measured in columns, not tab indices). Active tab is white text, inactive tabs are gray. Dirty marker is a `•` in light yellow after the name. Active tab auto-scrolls into view (`ensure_active_tab_visible`); mouse-wheel over the row scrolls `tabs_scroll` horizontally without changing the active buffer. Click-hit testing goes through `buffer_tab_at_column` / `buffer_tab_drag_target` which walk the same width arithmetic as the renderer (`buffer_tab_width` = name + `" •"` if dirty + trailing space).
- **Tab characters** are expanded to spaces at render time via `char_display_width(c, vis_col, tab_width)`, which snaps to the next multiple of `tab_width`. Cursor math (`char_idx_to_vis_col`, `vis_col_to_char_idx`) uses the same function so click/arrow positions stay aligned.
- **Layout**: the window splits horizontally into a **left block** (`Fill(6)`) | 1-col gap | **right shell pane** (`Fill(4)`). The left block splits vertically as: 1-row tab bar | 1-row gap | editor body (`Min(1)`) | 1-row gap | 26-row bottom shell pane. The editor body's inner layout is gutter + text area. An optional 1-row status strip sits at the bottom of the editor column (not the window) and appears only when there's a prompt or a transient status. No permanent status bar. Each shell pane splits vertically as: 1-row tab strip | 1-row gap | body.

### Highlight cache (src/highlight.rs + Buffer fields)

`syntect` parses line-by-line, where each line's parse state depends on the previous. `Buffer` keeps two parallel vectors in lockstep with `text`:

- `line_state_before[i]`: `Option<LineState>` — the parser state *before* line `i`. `[0]` is seeded on load from the syntax's initial state.
- `highlights[i]`: `Option<Vec<HlSpan>>` — lazily computed per visible line.

`ensure_highlights(up_to)` runs once per frame, walking forward from the nearest live `line_state_before` to fill gaps. On edits, `invalidate_highlights_from(row)` zeroes from `row` onward (the state *before* the edited row stays valid). Every mutation path that changes line count (insert/delete line, join on backspace, `insert_text`, `delete_range`) must `insert`/`drain` the cache vecs alongside `text`.

**Color theme**: `style_for` in `highlight.rs` maps TextMate scopes to `ratatui::style::Color`. Only the 16 named ANSI colors are used, never `Color::Rgb` or `Color::Indexed` — this is intentional so the user's terminal palette *is* the theme. When tweaking colors, edit `style_for` directly; there is no other theme layer.

### Code folding

Indent-based: a row is foldable if its next non-blank line has greater visual indent (tab-aware via `char_display_width`). Blank lines don't terminate a fold body. Detection lives in `compute_fold_end` (free fn); results cached per-buffer in `foldable_at` and invalidated via `foldable_dirty` when any edit changes line count or indentation.

Folds are **metadata about visibility**, not content — `text`, `highlights`, and `line_state_before` stay exactly in lockstep. Collapsed ranges live in `Buffer::folds: Vec<Fold>`. Anything that walks rows in document order (`render_body`, `render_gutter`, `screen_to_doc`, `place_cursor`, `adjust_scroll`, `move_up`/`move_down`, scroll wheel) routes through the visible-row helpers: `next_visible_row`, `prev_visible_row`, `nth_visible_row_from`, `visible_offset`. Edits that change line count call `adjust_folds_for_edit(at, removed, added)` which shifts fold boundaries and drops folds that intersect a deletion.

Fold state is part of the undo `Snapshot` and is also round-tripped through `session.toml` (with clamping on restore).

### Shell panes (src/shell.rs + Editor::{bottom_pane, right_pane})

Each pane is a `ShellPane` = `Vec<Shell>` + `active: usize` + `tabs_scroll`. A `Shell` owns a `portable_pty` master/writer/child plus a `vt100::Parser` (2000-row scrollback) that's the source of truth for what to render. `Editor` also owns a `PaneFocus` (Editor/Bottom/Right) and an `mpsc` channel (`shell_tx` / `shell_rx`) — each `spawn_shell` creates a reader thread that forwards PTY bytes as `ShellMsg::Bytes { id, data }` / `ShellMsg::Exited { id }`.

- **Spawning**: `Editor::new` auto-spawns one shell per pane with the process cwd. `restore_session` replaces them with the persisted list (one shell per saved `ShellTabSession { cwd }`), falling back to current dir if the saved cwd no longer exists.
- **Rendering**: `render_shell_body` reads cells directly from `vt100::Parser::screen()` and writes them into the ratatui buffer via `vt_cell_style` / `vt_color_to_ratatui`. Only the 16 ANSI colors are emitted — truecolor / indexed beyond 15 collapse to `Color::Reset`, matching the editor's no-RGB rule. Before rendering, `resize_pane_shells` calls `Shell::resize` to sync the PTY + parser to the current body rect.
- **Input routing**: `handle_key` first runs `try_handle_global_key` (Ctrl+1/2/3 switch panes, Ctrl+Q quits). If focus is a shell pane, `try_handle_shell_reserved` handles Ctrl+Shift+T (new tab), Ctrl+Shift+W (close tab), Alt+1..9 (switch tab); otherwise keys go through `shell::key_to_bytes` and are written to the PTY. Cmd/Ctrl+V pastes the clipboard as a bracketed paste sequence (`\x1b[200~` … `\x1b[201~`). Mouse events in a pane body are forwarded as SGR mouse reports via `shell::mouse_to_bytes` *only* when the shell's vt100 parser reports a non-`None` `mouse_protocol_mode` — outside that, clicks just move focus.
- **Cwd tracking**: two paths, both handled in `drain_shell_output`. OSC 7 (`\x1b]7;file://host/path\x07`) is parsed by `extract_osc7_cwd` on every chunk of PTY output; as a fallback (and to catch shells without OSC 7), `poll_shell_cwds` calls `query_process_cwd` (macOS `proc_pidinfo PROC_PIDVNODEPATHINFO` / Linux `/proc/$pid/cwd`) and updates the shell's `cwd` + `label` when it changes. The label is the cwd's basename via `derive_label`.
- **Exit**: when the child process exits, `mark_shell_dead` removes the tab and clamps `active`.

### Session persistence (src/session.rs)

On quit, `capture_session` serializes file-backed buffers (path, cursor, scroll, folds) plus each shell pane's tab cwds (`bottom_shells`, `right_shells`, and their active indices) to `$XDG_CONFIG_HOME/sacrament/session.toml` (falls back to `~/.config`). Untitled buffers are dropped; the active index is remapped past them. `restore_session` runs on launch *only* when no CLI path was given. Missing files are skipped silently; cursor/scroll are clamped to the new file bounds in case the file shrank. Shells are re-spawned (not literally restored — PTYs are not persistable); the auto-spawned defaults in `Editor::new` are cleared and replaced. If a pane ends up empty after restore, one default shell is spawned so a pane always has a shell.

### Config (src/config.rs)

TOML at `$XDG_CONFIG_HOME/sacrament/config.toml`. All fields have defaults (`Config::default`), so a missing file is fine and an unparseable file silently falls back to defaults.

## Conventions

- No backwards-compat shims or feature flags. Behavior changes go straight in.
- Terminal palette is the source of truth for colors — don't introduce RGB colors. This applies to the shell renderer too (`vt_color_to_ratatui` collapses `vt100::Color::Rgb` to `Color::Reset`).
- Any code that mutates `Buffer::text` line count must also update `highlights` and `line_state_before` in the same step, then call `invalidate_highlights_from` and `adjust_folds_for_edit`.
- Shell tab hit-testing and rendering share their width math — if you change one of `render_tab_bar` / `render_shell_tabs` or their helpers (`buffer_tab_width`, `shell_tabs_total_width`, `shell_tab_hit_at`), update the others in the same pass or clicks will miss the drawn tabs.
