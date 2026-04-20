# te — feature roadmap

Ranked by "what should I build next" — highest impact + lowest risk first, with
dependencies accounted for. Each tier is a reasonable pause point.

## Tier 1 — Fix the rough edges in v1

These are things v1 is currently *wrong* about, not new features. Do these before
anything else or they'll compound.

1. **Horizontal scrolling / long-line handling.** Right now a line longer than the
   terminal width renders off-screen and the cursor goes with it. Track a
   `scroll_col`, adjust like `scroll_row`.
2. **Tab key does something.** Currently `KeyCode::Tab` is ignored. Insert spaces
   (configurable width, default 4) or a literal `\t`. Default to spaces.
3. **Unicode-width-aware cursor.** Current code treats every char as 1 column. Breaks
   on CJK, emoji, combining marks. Use `unicode-width` crate. Affects rendering *and*
   cursor placement.
4. **Auto-clearing status line.** After Ctrl-S the "saved" message sticks forever.
   Clear on next keypress or after ~2 seconds.
5. **Preserve trailing-newline semantics on save.** Right now `buffer.join("\n")`
   drops the file's trailing newline. Track whether the loaded file ended with `\n`
   and restore it.

## Tier 2 — Core editing you'll miss within an hour

6. **Undo/redo.** `Ctrl-Z` / `Ctrl-Y`. Simplest approach: stack of buffer+cursor
   snapshots, coalesce consecutive character inserts into one undo step.
7. **Selection + copy/paste.** Shift+arrows to select, Ctrl-C/Ctrl-X/Ctrl-V. Use the
   `arboard` crate for system clipboard so it plays with the rest of macOS.
8. **Search.** Ctrl-F opens a prompt at the bottom, find-next with Enter,
   case-insensitive by default. Forward only is fine for v1 of this feature.
9. **Go-to-line + `te foo.js:42` syntax.** Ctrl-G prompts for a line number. The
   `file:line` form also works as a CLI arg and through remote-open — this is the
   piece that finally matches your old Sublime muscle memory.

## Tier 3 — Multiple files (where remote-open gets good)

This tier changes the remote-open UX from "replace current file" to "stack it as a
tab" — which is what you actually wanted from the Sublime days.

10. **Multiple buffers.** Replace `Editor`'s single buffer with `Vec<Buffer>` +
    `active_index`. Each buffer has its own path/cursor/dirty/scroll state.
11. **Tab bar at the top.** One line listing buffer filenames, active one highlighted.
12. **Buffer switching.** Ctrl-Tab or Alt-1..9 to jump. Ctrl-W closes current buffer
    (prompts if dirty).
13. **Remote-open appends a buffer** instead of replacing. The "unsaved changes"
    rejection from v1 becomes obsolete — new file just opens in a new tab.

## Tier 4 — Daily-driver polish

14. **Line numbers.** Optional gutter on the left. Toggle with a keybind or config.
15. **Config file.** TOML at `~/.config/te/config.toml`. Keybindings, tab width,
    tabs-vs-spaces, line numbers on/off, theme colors. Keep it small.
16. **Save As + new-file handling.** Right now opening a nonexistent file loads an
    empty buffer but save writes to the given path, which works but is silent. Add
    an explicit "untitled" state and a Ctrl-Shift-S "save as" prompt.
17. **External-change detection.** If the file on disk changes while open (git pull,
    another tool), show a marker in the status line. Don't auto-reload.
18. **Word-wise movement.** Alt-Left/Right to jump by word. Cheap once cursor logic
    is in one place.

## Tier 5 — Bigger bets

These are real projects, not afternoon tasks. Decide whether you actually want them
rather than assuming you do.

19. **Syntax highlighting.** Tree-sitter is the modern choice but pulls in a lot.
    Alternative: `syntect` (TextMate grammars, pure Rust, smaller). Start with
    `syntect` for a handful of languages you care about.
20. **Find & replace.** Builds on search. Prompt with confirm-each-match.
21. **Mouse support.** Click to move cursor, drag to select, scroll wheel. Crossterm
    gives you the events, but wiring selection through mouse needs care.
22. **Split panes.** Horizontal/vertical splits showing different buffers. Complexity
    jumps once you have two cursors and two viewports.
23. **Per-project socket / multiple instances.** Change `/tmp/te-$USER.sock` to
    something keyed on `$PWD` or a walk-upward for a `.te-root` marker. Useful if
    you want one editor per project.
24. **Session persistence.** Save open buffers + cursors on quit, restore on next
    launch. Feels great; not urgent.

## Deliberately not on this list

- **LSP / diagnostics.** Massive scope, belongs in a different editor.
- **Plugin system.** Premature. Add extension points only when a real second feature
  wants them.
- **Mouse drag-to-resize panes, floating windows, etc.** You're not building VSCode.
- **Vim or emacs emulation modes.** You're building this *because* you don't want
  them. Stay honest.

## Suggested next step

Tier 1 in one sitting — it's all small, all localized to `editor.rs`, and turns the
current v1 from "proof of concept" into "actually editable text without weird
glitches." Then pause, use it for a day, see which Tier 2 item you reach for first.
