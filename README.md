# sacrament

A small terminal text editor in Rust. Fast, keyboard-first, no config required.

Built as a daily driver — the feature set is what I actually reach for, nothing more. Single binary, single server per user (open files from any shell, they join the live session as tabs). Syntax highlighting follows your terminal's color palette instead of shipping its own theme.

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
- **Syntax highlighting** via `syntect` (TextMate grammars), rendered in your terminal's ANSI 16-color palette. Swap your terminal theme, the editor follows.
- **Code folding** (indent-based) with a clickable gutter chevron.
- **Search** (`Ctrl+F`), **goto-line** (`Ctrl+G`), and `sacrament file.rs:42` CLI syntax.
- **Undo/redo** with coalesced character inserts.
- **Mouse**: click to move, drag to select, double-click to select a word, scroll to navigate.
- **External-change detection** via `notify` — file-on-disk changes reload automatically.
- **Session persistence** — open tabs, cursor positions, scroll, and fold state survive quit/relaunch.
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

Extend selection by holding `Shift` with any movement key.

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

- Colors are driven by your terminal's 16-color ANSI palette. Change your terminal theme (e.g. iTerm2 / Ghostty / Alacritty color preset) and syntax highlighting follows automatically.
- Clicking the `▾` / `▸` chevron in the gutter toggles that fold.
- The tab-bar dirty marker is the light-yellow `•` after the filename.

## Architecture

See [CLAUDE.md](./CLAUDE.md) for a tour of the code — client/server model, event loop, highlight cache, folding, session persistence.

## License

MIT.
