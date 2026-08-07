# sacrament

A small text editor in Rust. Fast, keyboard-first, no config required.

Built as a daily driver — the feature set is what I actually reach for, nothing more. It's a native GUI app (via [iced](https://iced.rs)) that keeps a terminal's feel without running inside one: the editor and the shells are one styled cell grid, edge to edge, with no status bar and no chrome it doesn't need. **Two built-in shell panes** (bottom + right) sit beside the editor, so editing and terminal work share one window. Colors come from 16 ANSI slots you set in `config.toml`, so the editor and the shells are one palette.

The editor pane holds **sections**, chosen by a tab strip above the file tabs: the editor is one, and a read-only [Jira dashboard](#jira) is another. The file tabs belong to the editor section rather than to the pane, so switching section takes them with it.

One process per user: `sacrament <file>` from any shell opens a tab in the window that's already running.

## Install

Requires Rust (stable, 2024 edition or newer).

```sh
git clone https://github.com/tboddy/sacrament
cd sacrament
scripts/install-gui.sh          # sacrament — the editor
scripts/bundle-mac.sh           # Sacrament.app in ~/Applications (icon, Dock, Spotlight)
```

Then run `sacrament <file>[:line]`.

The `.app` and the `$PATH` binary are separate installs and update separately, but they reach the *same* running editor through the same socket — so `sacrament foo.rs` in a terminal opens a tab in the window you launched from the Dock.

> **1.x is still here.** The repo is a Cargo workspace: `crates/gui` is the
> editor described here and installs as `sacrament`; `crates/tui` is the original
> terminal editor, frozen but still built, and installs as `sacrament1`
> (`scripts/install-gui.sh --tui`). `crates/core` is the shared,
> framework-independent core. The two keep separate sockets and sessions, so both
> can run at once. See [the terminal version](#the-terminal-version) below.

## Features

- **Buffer tabs** — clickable, closeable (middle-click), and reorderable by dragging. `Cmd+1..9` jumps directly. A strip that overflows its pane scrolls sideways under the wheel rather than spilling over the pane beside it.
- **Sections** — the editor pane's outer tab strip. The editor is one section; the [Jira dashboard](#jira) is another. Commands that act on what's on screen go quiet while another section shows, so `Cmd+Z` can't edit a file you can't see; commands whose job is to *show* you something (`Cmd+O`, `Cmd+N`, `Cmd+F`) switch back to the editor.
- **Remote open** — a second `sacrament foo.rs` in another terminal opens as a new tab in the already-running instance instead of starting a rival window. A path that's already open reuses its tab.
- **Integrated shells** — a bottom pane and a right-side pane, each with its own shell tabs, backed by `alacritty_terminal`. `Ctrl+1/2/3` moves focus between editor / bottom / right. Tab labels track each shell's cwd as you `cd` around. Full 256-color and truecolor. Shells start as **login shells**, so your `PATH`, credential helpers and profile are exactly what a terminal would give you.
- **Ctrl belongs to the shell.** Every application binding is `Cmd`, so `Ctrl+C` is SIGINT, `Ctrl+R` is reverse history search, `Ctrl+W` is kill-word. `Shift+Enter` sends a newline (for TUIs that compose multi-line input) and `Option+←/→` moves by word.
- **Drag and drop** — drop a file on a shell pane and its escaped path is inserted, the way a terminal does it; drop one on the editor and it opens as a tab.
- **Syntax highlighting** via `syntect` (TextMate grammars), rendered in the 16 ANSI slots from `[theme]` — plus a hand-written TOML highlighter, since syntect ships no grammar for it.
- **Soft wrap** with hanging indentation, so a wrapped continuation lines up under its own statement instead of returning to the margin.
- **Code folding** (indent-based) with a clickable gutter chevron.
- **Markdown read mode** — `.md` / `.markdown` / `.mdx` files **open rendered** (headings, lists, code blocks, tables, links, emphasis); `Cmd+Shift+M` toggles back to the source. Opening with a line — `sacrament NOTES.md:42` — gives you the source instead, since a rendered view has no source lines to jump to. Wide tables scroll sideways rather than wrapping, which would destroy their alignment.
- **Review surface for Claude Code** — a PostToolUse hook opens every file the agent edits as an *unreviewed* background tab (cyan `◇` marker), cleared when you look at it. See [Reviewing AI-written code](#reviewing-ai-written-code).
- **Find** (`Cmd+F`, searches as you type, smart case), **find next/prev** (`Cmd+G` / `Cmd+Shift+G`), **goto-line** (`Ctrl+G`), and `sacrament file.rs:42` CLI syntax.
- **Undo/redo** storing deltas rather than snapshots, with runs of typing coalesced into one step.
- **Mouse** — click to move, drag to select, double-click for a word, scroll to navigate. In a shell, selection works in grid coordinates so it survives scrolling, and double/triple click select word and line.
- **Smooth scrolling** — the editor and the shells scroll by the pixel, not a row at a time, so a trackpad glides instead of stepping. Snapped to whole pixels, so glyphs stay crisp. (In a shell this applies to scrollback; live output stays line-aligned, where it belongs.)
- **Clickable links** — `Shift+click` a URL in a shell opens it in your browser, reading the whole logical line so a wrapped URL still resolves. `http`/`https` only, deliberately: `open` will launch a handler for *any* scheme, and terminal output is often not yours.
- **External-change detection** via `notify` — a clean buffer reloads itself when the file changes underneath it, keeping your caret and scroll; a *dirty* buffer refuses and tells you, rather than silently discarding unsaved edits.
- **Save that can't lose work** — writes go to a temp file and get renamed, and a file that changed on disk since it was read asks whether to overwrite or reload instead of clobbering it.
- **Session persistence** — open tabs, cursor positions, scroll, fold state, read mode, each shell pane's tab cwds, window size and pane ratios all survive quit and relaunch. Written when state changes, not only on a clean exit.
- **Native dialogs** for everything that's a real question: save, open, "save before closing?", "the file changed on disk". Anything the app merely has to *report* is a system alert with an OK button — there's no status strip to go stale.
- **Themes and fonts** from `config.toml` — 16 ANSI slots plus background/foreground/cursor/selection, and any installed monospace family at any size. A missing glyph falls back to a monochrome system font rather than a colour emoji, so a TUI's box drawing and bullets stay text.

Deliberately absent, compared to the terminal version: git change-bars, the diff view, and linting.

## Keybindings

Every application binding is `Cmd`. The only two exceptions are `Ctrl`, because their `Cmd` chord is already spoken for by the macOS convention: pane focus and goto-line.

| Action | Keys |
|--------|------|
| Save | `Cmd+S` |
| Save as | `Cmd+Shift+S` |
| Open | `Cmd+O` |
| New buffer tab | `Cmd+N` |
| New tab in the focused pane | `Cmd+T` |
| Close the focused pane's tab | `Cmd+W` (or middle-click) |
| Next / prev tab in the focused pane | `Cmd+Tab`, `Cmd+Shift+]` / `Cmd+Shift+[` |
| Jump to tab 1–9 in the focused pane | `Cmd+1` … `Cmd+9` |
| Quit | `Cmd+Q` |
| Undo / redo | `Cmd+Z` / `Cmd+Shift+Z` |
| Cut / copy / paste | `Cmd+X` / `Cmd+C` / `Cmd+V` |
| Select all / clear selection | `Cmd+A` / `Esc` |
| Find | `Cmd+F` |
| Find next / previous | `Cmd+G` / `Cmd+Shift+G` |
| Goto line | `Ctrl+G` |
| Toggle line comment | `Cmd+/` |
| Indent / outdent | `Cmd+]` / `Cmd+[` (selection-aware) |
| Word-wise move | `Option+←` / `Option+→` |
| Start / end of screen row | `Cmd+←` / `Cmd+→` |
| Start / end of document | `Cmd+↑` / `Cmd+↓` |
| Fold / unfold at cursor | `Cmd+Option+[` / `Cmd+Option+]` |
| Fold / unfold all | `Cmd+Option+Shift+[` / `Cmd+Option+Shift+]` |
| Toggle markdown read mode | `Cmd+Shift+M` on `.md` buffers |
| Refresh the section | `Cmd+R` (Jira; the editor is kept current by the file watcher) |
| Focus editor / bottom shell / right shell | `Ctrl+1` / `Ctrl+2` / `Ctrl+3` |

Extend selection by holding `Shift` with any movement key.

When a shell pane is focused, keystrokes pass through to the shell — all of `Ctrl` included, so your shell's own bindings work untouched. `Shift+Enter` inserts a newline (and still submits at a plain shell prompt), `Option+←` / `Option+→` move by word, and `Option+Backspace` deletes one.

## Config

TOML at `$XDG_CONFIG_HOME/sacrament/config.toml` (or `~/.config/sacrament/config.toml`). All fields are optional, and both versions read the same file.

```toml
tab_width = 2
indent_with_tabs = true
line_numbers = true
syntax_highlighting = true
word_wrap = true

[font]
# Unset means the platform's default monospace. An unknown name says so
# rather than silently drawing nothing.
family = "Envy Code R"
size = 13
line_height = 1.15      # a multiple of size, and it *is* the cell height

[theme]
# The 16 ANSI slots are the whole palette: syntax highlighting, shell output
# and the app's own chrome all resolve to these. Paste them from your
# terminal's config — Ghostty, kitty and Alacritty keep theirs in plain text.
background           = "#282828"
foreground           = "#ebdbb2"
cursor               = "#a89984"
selection_background = "#ebdbb2"
selection_foreground = "#282828"

black   = "#282828"
red     = "#cc241d"
green   = "#98971a"
yellow  = "#d79921"
blue    = "#458588"
magenta = "#b16286"
cyan    = "#689d6a"
white   = "#a89984"

bright_black   = "#928374"
bright_red     = "#fb4934"
bright_green   = "#b8bb26"
bright_yellow  = "#fabd2f"
bright_blue    = "#83a598"
bright_magenta = "#d3869b"
bright_cyan    = "#8ec07c"
bright_white   = "#ebdbb2"

[jira]
# Optional — omit the table and the Jira section just shows setup instructions.
site  = "your-company"          # or your-company.atlassian.net, or the full URL
email = "you@your-company.com"
query = "assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC"
max_results = 50
```

The Jira **API token is deliberately not in here** — this file is plain text and
shared with the terminal version. It lives in the macOS Keychain; see
[Jira](#jira).

Window geometry is *not* config — it lives in the session file, because rewriting `config.toml` on every resize would delete your own comments.

`status_timeout_ms` and `[lint.linters]` are read by the terminal version only.

## Tips

- Save feedback is the window title: `• name` while dirty, `name` once written. A marker that disappears tells you more, continuously, than a message that flashes once.
- Clicking the `▾` / `▸` chevron in the gutter toggles that fold. Clicking a line *number* deliberately does nothing.
- The tab dirty marker is the bright-yellow `•` after the filename; a bright-cyan `◇` means "touched by a tool, not yet reviewed" and clears when you switch to that tab.
- Drag a tab within its strip to reorder it. The strip rearranges as you move, so there's no drop marker to aim at.
- Click a shell tab to switch, or the `+` at the end of any strip to add one more of that thing — a shell in a shell pane, an empty buffer in the editor. Closing the last tab in a shell pane is fine; it stays empty until you press `+`.
- The tab strip's underline breaks under the active tab, which is what joins it to the pane below.
- The 1px line between panes is a splitter — drag it. Window size and both ratios are restored next launch.
- `SACRAMENT_METRICS=1` prints feed time, throughput and memory footprint to stderr. Diagnostics are never drawn in the window.

## Jira

The Jira section shows what you're meant to be working on without leaving the
editor: issues grouped by status, in real tables that follow the pane's width.
It's read-only — nothing here writes to Jira.

Two pieces of setup. The non-secret half goes in `config.toml` (see
[Config](#config)); the token goes in the Keychain:

```sh
security add-generic-password -s sacrament-jira -a you@your-company.com -w '<token>'
```

Create the token at
[id.atlassian.com/manage-profile/security/api-tokens](https://id.atlassian.com/manage-profile/security/api-tokens).
`SACRAMENT_JIRA_TOKEN` overrides the Keychain if you'd rather pass it in from a
shell. With nothing configured the section shows these instructions instead of a
dashboard, so you can see it working before you go and mint anything.

Click **Jira** to load, **Refresh** or `Cmd+R` to reload. The fetch happens the
first time you open the section, not at startup — an editor launched to open a
file shouldn't make a network request nobody asked for.

One thing worth knowing about the default query: it's `statusCategory != Done`,
not the more obvious `resolution = Unresolved`. Plenty of workflows have terminal
statuses that never set a resolution, so Unresolved quietly matches finished
work — on the instance this was built against it returned 50 issues of which 35
were released or rejected. `statusCategory` keys off the fixed To Do / In Progress
/ Done grouping that exists underneath whatever custom status names a project
invented.

[docs/jira-integration.md](./docs/jira-integration.md) has the design notes and
the planned next steps (ticket detail, the linked Confluence spec, status
transitions, branch-and-PR).

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
marked unreviewed (cyan `◇`) without stealing focus — they pile up while you keep
working, and a `--review` open never yanks the cursor out of what you're typing.
Switch to one and the mark clears. To use this in another project, copy both
files there (the hook path is project-relative via `$CLAUDE_PROJECT_DIR`), or
lift it into your global `~/.claude/settings.json`. `sacrament` must be on your
`PATH` (or set `SACRAMENT_BIN` in the script).

## The terminal version

`crates/tui` is the original: the same three-pane workspace drawn with
`ratatui` + `crossterm`, running inside your terminal emulator. It's frozen —
it gets fixes, not features — and installs alongside as `sacrament1`
(`scripts/install-gui.sh --tui`), with its own socket and session file so both
can run at once.

Three things it has that the GUI doesn't:

- **git change-bars and a diff view** — `Alt+D` shows the current file's diff vs
  `HEAD`, and the gutter carries always-on bars (green added, cyan modified).
- **Linting** — `Alt+L` runs a configured linter for the file's language and
  marks problems in the gutter; `Alt+]` / `Alt+[` jump between them.
- **Your terminal's palette as the theme** — it ignores `[theme]` by design and
  renders through whatever your emulator provides.

Its bindings are `Ctrl`-based (`Ctrl+S`, `Ctrl+W`, `Ctrl+F`, `Alt+1..9`, and
`Ctrl+1/2/3` for pane focus), since it had to fit around whatever the host
emulator already claimed. [CLAUDE.md](./CLAUDE.md) documents it in full.

## Architecture

See [CLAUDE.md](./CLAUDE.md) for a tour of the code — the grid widget and its
sources, PTY plumbing, the highlight cache, folding, the client/server model, and
session persistence.

## License

MIT.
