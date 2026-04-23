# sacrament

A small terminal text editor in Rust. Fast, keyboard-first, no config required.

Built as a daily driver — the feature set is what I actually reach for, nothing more. Single binary, single server per user (open files from any shell, they join the live session as tabs). Ships with two built-in shell panes (bottom + right) so editing and terminal work share one window. Syntax highlighting follows your terminal's color palette instead of shipping its own theme.

## Install

Requires Rust (stable, 2024 edition or newer).

```sh
git clone https://github.com/tboddy/sacrament
cd sacrament
cargo install --path .
```

Then run `sacrament <file>[:line]`.

## Features

- **Multiple buffers** with a clickable tab bar; `Alt+1..9` jumps directly.
- **Remote open**: a second `sacrament foo.rs` in another terminal opens as a new tab in the already-running instance.
- **Integrated shells**: a bottom pane and a right-side pane, each with its own shell tabs. `Ctrl+1/2/3` moves focus between editor / bottom / right. Tab labels track the shell's cwd as you `cd` around.
- **Syntax highlighting** via `syntect` (TextMate grammars), rendered in your terminal's ANSI 16-color palette. Swap your terminal theme, the editor follows.
- **Code folding** (indent-based) with a clickable gutter chevron.
- **Search** (`Ctrl+F`), **goto-line** (`Ctrl+G`), and `sacrament file.rs:42` CLI syntax.
- **Undo/redo** with coalesced character inserts.
- **Mouse**: click to move, drag to select, double-click to select a word, scroll to navigate. Mouse in shell panes passes through to TUIs that opt into mouse reporting.
- **External-change detection** via `notify` — file-on-disk changes reload automatically.
- **Session persistence** — open tabs, cursor positions, scroll, fold state, and each shell pane's tab cwds survive quit/relaunch.
- **Bracketed paste** and **kitty keyboard protocol** — `Cmd+V` pastes the system clipboard; `Cmd+Shift+S`, `Cmd+Option+[`, etc. all disambiguate properly on capable terminals.

## Keybindings

Where shown, `Ctrl` and `Cmd` are interchangeable (macOS-friendly).

| Action | Keys |
|--------|------|
| Save | `Ctrl+S` |
| Save as | `Ctrl+Shift+S` / `Alt+S` |
| Open new tab (remote) | run `sacrament <file>` in another shell |
| Close tab | `Ctrl+W` (press twice if dirty) |
| Next / prev tab | `Ctrl+Tab` / `Shift+Tab` |
| Jump to tab 1–9 | `Alt+1` … `Alt+9` |
| Quit | `Ctrl+Q` (press twice if any buffer dirty) |
| Undo / redo | `Ctrl+Z` / `Ctrl+Shift+Z` (or `Ctrl+Y`) |
| Cut / copy / paste | `Ctrl+X` / `Ctrl+C` / `Ctrl+V` |
| Find | `Ctrl+F` |
| Goto line | `Ctrl+G` |
| Word-wise move | `Alt+←` / `Alt+→` (or `Alt+B` / `Alt+F`) |
| Indent / outdent | `Ctrl+]` / `Ctrl+[` (selection-aware) |
| Fold / unfold at cursor | `Cmd+Option+[` / `Cmd+Option+]` |
| Fold / unfold all | `Cmd+Option+Shift+[` / `Cmd+Option+Shift+]` |
| Focus editor / bottom shell / right shell | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` |
| New shell tab in focused pane | `Ctrl+Shift+T` |
| Close shell tab in focused pane | `Ctrl+Shift+W` |
| Switch shell tab in focused pane | `Alt+1` … `Alt+9` |

Extend selection by holding `Shift` with any movement key. When a shell pane is focused, keystrokes pass through to the shell (so your shell's own bindings still work); the table rows above are the editor-global ones that stay reserved.

## Config

TOML at `$XDG_CONFIG_HOME/sacrament/config.toml` (or `~/.config/sacrament/config.toml`). All fields are optional.

```toml
tab_width = 4
indent_with_tabs = false
line_numbers = true
status_timeout_ms = 2000
syntax_highlighting = true
```

## Tips

- Colors are driven by your terminal's 16-color ANSI palette. Change your terminal theme (e.g. iTerm2 / Ghostty / Alacritty color preset) and syntax highlighting follows automatically. This applies to shell output too — anything your shell would render in truecolor gets folded back to the nearest ANSI slot.
- Clicking the `▾` / `▸` chevron in the gutter toggles that fold.
- The tab-bar dirty marker is the light-yellow `•` after the filename.
- Click a shell tab to switch, or the `+` at the end of the strip to spawn a new shell in that pane. Closing the last tab in a pane is fine — the pane stays empty until you open a new one.

## Architecture

See [CLAUDE.md](./CLAUDE.md) for a tour of the code — client/server model, event loop, highlight cache, folding, session persistence.

## License

MIT.
