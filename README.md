# sacrament

A small terminal text editor in Rust. Fast, keyboard-first, no config required.

Built as a daily driver — the feature set is what I actually reach for, nothing more. Single binary, single server per user (open files from any shell, they join the live session as tabs). Ships with two built-in shell panes (bottom + right) so editing and terminal work share one window. Syntax highlighting follows your terminal's color palette instead of shipping its own theme.

## Install

Requires Rust (stable, 2024 edition or newer).

```sh
git clone https://github.com/tboddy/sacrament
cd sacrament
cargo install --path crates/tui
```

Then run `sacrament <file>[:line]`.

> **2.0 in progress.** The repo is a Cargo workspace: `crates/tui` is the
> shipping terminal editor described below, `crates/core` is the shared
> framework-independent core, and `crates/gui` is an in-progress rewrite as a
> native GUI app (via [iced](https://iced.rs)) that keeps the terminal-ish feel
> without running in a terminal. The two install side by side as `sacrament` and
> `sacrament2` and keep separate sockets and sessions, so nothing below changes
> until the cutover.

## Features

- **Multiple buffers** with a clickable tab bar; `Alt+1..9` jumps directly.
- **Remote open**: a second `sacrament foo.rs` in another terminal opens as a new tab in the already-running instance.
- **Integrated shells**: a bottom pane and a right-side pane, each with its own shell tabs. `Ctrl+1/2/3` moves focus between editor / bottom / right. Tab labels track the shell's cwd as you `cd` around.
- **Syntax highlighting** via `syntect` (TextMate grammars), rendered in your terminal's ANSI 16-color palette. Swap your terminal theme, the editor follows.
- **Code folding** (indent-based) with a clickable gutter chevron.
- **Markdown read mode** — `Alt+M` toggles a `.md` / `.markdown` / `.mdx` buffer between source editing and a rendered, read-only view (headings, lists, code blocks, tables, links, emphasis).
- **Diff / review view** — `Alt+D` shows the current file's git diff (added lines green, removed red, hunks cyan); the gutter also carries always-on change-bars (`▎` green = added, cyan = modified) vs `HEAD`. New, untracked files render as all-added.
- **Linting** — `Alt+L` runs a configured linter for the file's language and marks problems in the gutter (`●` red error / yellow warning); `Alt+]` / `Alt+[` jump between them, and the message shows in the status strip when the cursor sits on a flagged line.
- **Review surface for Claude Code** — a PostToolUse hook opens every file Claude edits as an *unreviewed* background tab (cyan `◇` marker), cleared when you view it. See [Reviewing AI-written code](#reviewing-ai-written-code).
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
| Toggle markdown read mode | `Alt+M` (or `Ctrl+Shift+M`) on `.md` buffers |
| Toggle diff view (vs git `HEAD`) | `Alt+D` (or `Ctrl+Shift+D`) |
| Lint current file | `Alt+L` (or `Ctrl+Shift+L`) |
| Jump to next / prev diagnostic | `Alt+]` / `Alt+[` |
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
word_wrap = true

# Optional: per-language linters for Alt+L. Key by the syntect language name
# (e.g. "Rust", "Python") or a file extension. {file} is replaced with the file
# name; the command runs in the file's directory. Output is scanned for
# `path:line:col: message` lines (use formatters that emit that — clippy short,
# ruff, eslint -f unix, shellcheck, tsc, gcc/clang).
[lint.linters]
Rust = { command = "cargo clippy --message-format=short" }
Python = { command = "ruff check {file}" }
JavaScript = { command = "eslint -f unix {file}" }
```

## Tips

- Colors are driven by your terminal's 16-color ANSI palette. Change your terminal theme (e.g. iTerm2 / Ghostty / Alacritty color preset) and syntax highlighting follows automatically. This applies to shell output too — anything your shell would render in truecolor gets folded back to the nearest ANSI slot.
- Clicking the `▾` / `▸` chevron in the gutter toggles that fold.
- The tab-bar dirty marker is the light-yellow `•` after the filename; a cyan `◇` means "touched, not yet reviewed" and clears when you switch to that tab.
- Gutter markers sit just left of the text: a `▎` bar flags a line changed vs git (green added / cyan modified), and a `●` flags a lint problem (red error / yellow warning).
- Click a shell tab to switch, or the `+` at the end of the strip to spawn a new shell in that pane. Closing the last tab in a pane is fine — the pane stays empty until you open a new one.

## Reviewing AI-written code

sacrament can double as a review surface for code an agent writes. Because any
`sacrament <file>` joins the live session, a [Claude Code](https://claude.ai/code)
hook can pop every file the agent touches into the editor for you to look over.

This repo ships the wiring:

- `scripts/claude-open-hook.sh` — reads the hook payload from stdin and runs
  `sacrament --review <file>` (a no-op unless a sacrament server is running, so
  it's harmless when the editor is closed).
- `.claude/settings.json` — a `PostToolUse` hook matching `Edit|Write|MultiEdit`
  that invokes the script.

With sacrament open in this repo, edits Claude makes appear as background tabs
marked unreviewed (cyan `◇`) without stealing focus — they pile up while you
keep working. Switch to one and the mark clears; the gutter shows what changed
(or `Alt+D` for the full diff), and `Alt+L` lints it. To use this in another
project, copy both files there (the hook path is project-relative via
`$CLAUDE_PROJECT_DIR`), or lift it into your global `~/.claude/settings.json`.
`sacrament` must be on your `PATH` (or set `SACRAMENT_BIN` in the script).

## Architecture

See [CLAUDE.md](./CLAUDE.md) for a tour of the code — client/server model, event loop, highlight cache, folding, session persistence.

## License

MIT.
