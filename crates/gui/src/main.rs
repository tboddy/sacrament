//! sacrament 2.0 spike — validates the iced rebuild before committing to it.
//!
//! What this is testing, in order of risk:
//!
//! 1. Can a custom iced widget draw a styled monospace cell grid fast enough,
//!    inside `pane_grid`'s layout? (`grid_view`)
//! 2. Does `alacritty_terminal` slot in cleanly as the VT layer, replacing v1's
//!    `vt100`? (`term`)
//! 3. Can PTY output be push-driven through iced's subscription model instead of
//!    v1's 20ms poll loop? (`pty`)
//! 4. What does it cost in frame time and memory?
//!
//! Run it, type in the shell, then flood output (`yes | head -200000`, `htop`)
//! and watch the metrics line at the bottom. `Ctrl+R` resets the counters.
//!
//! Not in scope: editor surface, tabs, multiple panes, selection, scrollback
//! input, mouse forwarding, OSC handling. Those are cheap once the grid is
//! proven; they're the reason the grid is proven first.

mod blocks;
mod buffer;
mod grid;
mod ipc;
mod macos;
mod grid_view;
mod gutter;
mod metrics;
mod palette;
mod font;
mod pty;
mod read;
mod term;
mod theme_guard;
mod watch;

use std::os::unix::net::UnixListener;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use iced::widget::{
    column, container, mouse_area, pane_grid, row, rule, scrollable, text, text_input,
};
use iced::{Element, Font, Length, Subscription, Task};

use buffer::{Buffer, BufferSource};
use grid_view::{GridMouse, GridView};
use gutter::Gutter;
use metrics::Metrics;
use font::FontSpec;
use palette::Palette;
use pty::{PaneId, ShellKey, Spawn};
use term::{Terminal, TerminalSource};

/// What the command line asked for.
struct Args {
    /// Paths with their optional `:line` suffix already split off.
    files: Vec<(std::path::PathBuf, Option<usize>)>,
    syntax: Option<String>,
    /// Opened on behalf of a tool rather than a person — see `--review` below.
    review: bool,
}

fn parse_args(argv: &[String]) -> Result<Args, String> {
    let mut args = Args {
        files: Vec::new(),
        syntax: None,
        review: false,
    };
    let mut i = 0;
    while i < argv.len() {
        let a = &argv[i];
        match a.as_str() {
            "-s" | "--syntax" => {
                let v = argv.get(i + 1).ok_or_else(|| format!("{a} requires a value"))?;
                args.syntax = Some(v.clone());
                i += 2;
            }
            s if s.starts_with("--syntax=") => {
                args.syntax = Some(s["--syntax=".len()..].to_string());
                i += 1;
            }
            "--review" => {
                args.review = true;
                i += 1;
            }
            // macOS hands a process-serial argument to bundled apps on some
            // launch paths. Rejecting it as unknown would exit(2) before the
            // window opened, so launching from the Dock would do nothing at all
            // while running the same binary from a shell worked fine.
            s if s.starts_with("-psn_") => i += 1,
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(format!("unexpected argument: {s}"));
            }
            s => {
                args.files.push(split_line_suffix(s));
                i += 1;
            }
        }
    }
    Ok(args)
}

/// Split a trailing `:N` into a line number, as `sacrament2 src/main.rs:42`.
///
/// A file whose name genuinely ends in `:digits` wins over the suffix reading —
/// checked by asking the filesystem, since there's no other way to tell them
/// apart.
fn split_line_suffix(arg: &str) -> (std::path::PathBuf, Option<usize>) {
    if let Some((head, tail)) = arg.rsplit_once(':')
        && !tail.is_empty()
        && tail.chars().all(|c| c.is_ascii_digit())
        && let Ok(n) = tail.parse::<usize>()
    {
        if !std::path::Path::new(head).exists() && std::path::Path::new(arg).exists() {
            return (std::path::PathBuf::from(arg), None);
        }
        return (std::path::PathBuf::from(head), Some(n));
    }
    (std::path::PathBuf::from(arg), None)
}

fn main() -> iced::Result {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let args = match parse_args(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("sacrament: {e}");
            std::process::exit(2);
        }
    };

    // Single instance per user, per app id. If a server is already listening,
    // hand the files over and exit — that's what makes opening a file from any
    // terminal join the live window instead of starting a second editor.
    match hand_off(&args) {
        HandOff::Done => return Ok(()),
        HandOff::BeServer => {}
    }

    // No server, so this process becomes one. Bound before the window opens: a
    // failure here shouldn't be discovered after the UI is up.
    if let Some(listener) = bind_socket() {
        ipc::attach(listener);
    }
    // Started before the window so the first `sync_watches` has somewhere to go.
    watch::start();

    // Window size comes from the session, so the app reopens where it was left.
    // Read before `State::new` because the builder needs it up front.
    let geometry = sacrament_core::session::load(sacrament_core::APP_GUI)
        .map(|s| s.geometry.sanitized())
        .unwrap_or_default();

    iced::application(State::new, State::update, State::view)
        .title(State::title)
        .subscription(State::subscription)
        .theme(State::theme)
        .window_size((geometry.window_width, geometry.window_height))
        // Intercept the close so the session can be written before exiting.
        .exit_on_close_request(false)
        .default_font(Font::MONOSPACE)
        .antialiasing(true)
        .run()
}

/// Bind the listening socket, clearing a stale one first.
///
/// A leftover socket file is the normal case, not an error: on macOS `Cmd+Q`
/// terminates the process without unwinding, so nothing gets the chance to
/// unlink it. `bind` fails with `EADDRINUSE` on an existing path whether or not
/// anyone is listening, so the two have to be told apart by trying to connect.
///
/// `None` means don't serve. That's expected when another instance already owns
/// the socket — a bare `sacrament2` opens a second window rather than refusing to
/// start, and the first instance keeps the socket.
fn bind_socket() -> Option<UnixListener> {
    use std::os::unix::net::UnixStream;

    let sock = sacrament_core::paths::socket_path(sacrament_core::APP_GUI);
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            return None;
        }
        let _ = std::fs::remove_file(&sock);
    }
    match UnixListener::bind(&sock) {
        Ok(listener) => Some(listener),
        // Losing IPC costs the single-instance behavior, not the editor, so this
        // is a warning rather than a failure to start.
        Err(e) => {
            eprintln!("sacrament: not listening on {}: {e}", sock.display());
            None
        }
    }
}

enum HandOff {
    /// A running instance took the request; this process is finished.
    Done,
    /// Nothing was listening — become the server.
    BeServer,
}

/// Try to give the requested files to an already-running instance.
fn hand_off(args: &Args) -> HandOff {
    // `--review` is a tool's open (the Claude Code hook), and only means anything
    // against an editor someone is already looking at. It never creates files and
    // never boots a server — a hook firing in a repo with no editor open should
    // do nothing at all, not launch one.
    if args.review {
        for (path, line) in &args.files {
            let _ = sacrament_core::client::try_send_open(
                sacrament_core::APP_GUI,
                path,
                *line,
                args.syntax.as_deref(),
                true,
            );
        }
        return HandOff::Done;
    }

    if args.files.is_empty() {
        // A bare `sacrament2` with a server running would otherwise be a no-op
        // that looks like a crash. Becoming a second window is the honest
        // outcome; the running instance already has the session.
        return HandOff::BeServer;
    }

    // Create before sending: the server resolves the path with `canonicalize`,
    // which fails on a file that doesn't exist yet, so `sacrament2 new.rs` has to
    // create it either way — here or after becoming the server.
    for (path, _) in &args.files {
        if !path.exists() {
            let _ = std::fs::File::create(path);
        }
    }

    let mut handed = false;
    for (path, line) in &args.files {
        match sacrament_core::client::try_send_open(
            sacrament_core::APP_GUI,
            path,
            *line,
            args.syntax.as_deref(),
            false,
        ) {
            Ok(true) => handed = true,
            // No server listening. Stop trying and open everything locally
            // instead, rather than half here and half there.
            Ok(false) => return HandOff::BeServer,
            Err(e) => {
                eprintln!("sacrament: {e}");
                return HandOff::BeServer;
            }
        }
    }
    if handed { HandOff::Done } else { HandOff::BeServer }
}

#[derive(Debug, Clone)]
enum Message {
    Pty(ShellKey, pty::Event),
    /// The shell grid measured its bounds and wants this many rows/cols.
    GridResized(ShellKey, usize, usize),
    /// Same, for the editor pane. Separate because only the shell's size has to
    /// be pushed down to a PTY.
    EditorResized(usize, usize),
    /// A pane_grid splitter was dragged.
    PaneDragged(pane_grid::DragEvent),
    PaneResized(pane_grid::ResizeEvent),
    /// Key, physical key, modifiers, and the composed text iced resolved for us.
    Key(
        iced::keyboard::Key,
        iced::keyboard::key::Physical,
        iced::keyboard::Modifiers,
        Option<String>,
    ),
    /// A mouse gesture inside a pane's grid.
    Mouse(Focus, GridMouse),
    /// Clipboard read completed; insert it.
    Pasted(Option<String>),
    /// The pointer entered a tab.
    TabHovered(Option<(TabGroup, usize)>),
    /// The pointer left a *specific* tab. Which one matters — see the handler.
    TabExited(TabGroup, usize),
    /// A tab was pressed: it becomes active, and a reorder drag begins.
    TabPressed(TabGroup, usize),
    /// Pointer moved within a tab strip, in strip-relative x. Supplies the drag's
    /// direction of travel.
    TabPointerMoved(TabGroup, f32),
    /// Middle-click on a tab.
    TabClosed(TabGroup, usize),
    /// The left button came up anywhere. Ends a tab drag, and settles geometry.
    LeftReleased,
    /// The prompt's text changed.
    PromptInput(String),
    /// Another invocation asked this instance to open something.
    Remote(ipc::Command),
    /// A watched file changed on disk.
    FileChanged(std::path::PathBuf),
    /// A file was dragged in from the Finder and dropped on the window.
    FileDropped(std::path::PathBuf),
    /// An alert was acknowledged. Carries nothing — it exists so the dialog's
    /// future has somewhere to land, and so a repeat of the same message can
    /// stop being suppressed once the first one is gone.
    AlertDismissed,
    /// The save panel closed. `None` means it was cancelled.
    SaveAsPicked(Option<std::path::PathBuf>, AfterSave),
    /// The open panel closed.
    OpenPicked(Vec<std::path::PathBuf>),
    /// New empty buffer tab.
    NewBuffer,
    /// A gutter chevron was clicked.
    ToggleFold(usize),
    /// Answer to "save before closing this tab?".
    CloseTabAnswer(usize, Answer),
    /// Answer to "save before quitting?".
    QuitAnswer(Answer),
    /// Answer to a save that collided with a change on disk.
    ConflictAnswer(Conflict),
    WindowResized(iced::Size),
    CloseRequested,
    SpawnShell(PaneId),
    /// A pane was clicked somewhere that isn't a cell — its padding. Focus moves,
    /// the cursor doesn't. Without this, the padding band would be dead to
    /// clicks, which looks like the pane ignoring you.
    FocusPane(Focus),
    /// Reload the Jira dashboard. From the clickable "Refresh" heading, and from
    /// `Cmd+R` while the section is showing.
    JiraRefresh,
    /// Pointer entered or left the refresh control.
    JiraRefreshHovered(bool),
    /// A Jira fetch finished. `Err` carries a message for an alert — Jira's own
    /// words where it gave any, since it explains a bad query far better than a
    /// generic failure would.
    JiraLoaded(Result<sacrament_core::jira::Page, String>),
}

/// One pane's identity. Only `Shell` exists in the spike; `Editor` is the point
/// of the enum — it's where the same `GridView` gets a different data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PaneKind {
    Editor,
    Shell(PaneId),
}

/// Inset between a pane's edge and its content. Terminal text jammed against a
/// window border is uncomfortable to read — the same reason terminal emulators
/// ship a padding setting. Not configurable yet; one number doesn't justify a new
/// config section, but it's a small change if it wants tuning.
const PANE_PADDING: u16 = 10;

/// Height of the buffer tab strip, in pixels.
const TAB_BAR_HEIGHT: f32 = 26.0;

/// Thickness of the tab strip's underline and the separators between tabs.
///
/// Drawn as `rule` widgets rather than a `Border`, because `iced::Border` applies
/// to all four sides at once — there's no way to ask it for "bottom only" or
/// "right only". A 1px rule per edge is the way to get a single side.
const TAB_BORDER: f32 = 1.0;


/// Thickness of the divider between panes.
///
/// Implemented as `pane_grid`'s `spacing`, not as a border on each pane: a border
/// would outline every pane, and `pane_grid::Style` only draws its split line on
/// hover or drag, with no always-visible option. The gap `spacing` leaves shows
/// whatever is behind the grid, so painting *that* the divider color yields one
/// permanent line between panes and nothing around them.
///
/// Independent of the drag target — `on_resize`'s leeway is what you grab, so a
/// 1px divider is still easy to hit.
const PANE_DIVIDER: f32 = 1.0;

/// What the one-line prompt at the bottom is currently asking for.
///
/// Find and goto-line both need a single line of text, so they share one prompt
/// rather than growing two lookalike inputs. (Save-as used to be a third; it's a
/// native panel now.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptKind {
    Find,
    GotoLine,
}

impl PromptKind {
    fn label(self) -> &'static str {
        match self {
            PromptKind::Find => "find:",
            PromptKind::GotoLine => "line:",
        }
    }
}

/// The prompt's state while it's open.
#[derive(Debug, Clone)]
struct Prompt {
    kind: PromptKind,
    input: String,
    /// Caret position when the prompt opened. Find searches from here every time
    /// the query changes, so editing the query re-searches the same span instead
    /// of walking forward through the file one keystroke at a time.
    origin: buffer::Pos,
    /// Outcome of the last action — "no match", an IO error — shown after the
    /// input. Not a transient status: it stays until the next action changes it.
    note: Option<String>,
}

/// What to do once a save-as completes. Dialogs are async, so an action that
/// depends on one has to travel with it rather than following the call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AfterSave {
    Nothing,
    CloseTab(usize),
    Quit,
}

/// A three-way answer from a native confirmation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Answer {
    Save,
    Discard,
    Cancel,
}

/// What to do about a file that changed underneath an edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Conflict {
    Overwrite,
    Reload,
    Cancel,
}

/// What one tab displays. The markers carry their own colors, so they can't be
/// folded into the name.
struct TabLabel {
    name: String,
    dirty: bool,
    unreviewed: bool,
}

/// Marker colors, matching v1: bright yellow for unsaved, bright cyan for a file
/// an external tool touched. Theme slots, not literals — `[theme]` decides the
/// actual shade.
const DIRTY_SLOT: usize = 11;
const UNREVIEWED_SLOT: usize = 14;

/// The Jira section's refresh control: `blue` at rest, `bright_blue` under the
/// pointer. Theme slots, not literals — `[theme]` decides the actual shades.
///
/// A chrome decision rather than a markdown-derived one, which is why these are
/// slot constants here beside the tab markers rather than accessors on
/// `core::markdown`. The control only *looks* like a heading; it isn't one.
const REFRESH_SLOT: usize = 4;
const REFRESH_HOVER_SLOT: usize = 12;

/// Where the Jira API token is looked up. The Keychain is the real source; the
/// environment variable is the scriptable override. Never `config.toml` — that
/// file is plaintext and shared with v1. See `core::secret`.
const JIRA_KEYCHAIN_SERVICE: &str = "sacrament-jira";
const JIRA_TOKEN_ENV: &str = "SACRAMENT_JIRA_TOKEN";

/// The dashboard's columns and their share of the pane's width.
///
/// Portions rather than fixed pixels: the pane is resizable and the summary should
/// absorb the slack. Summary is last and much the widest — it's the only column
/// whose length is unbounded, and the short ones are what you scan.
const JIRA_COLUMNS: [(&str, u16); 5] = [
    ("Key", 3),
    ("Pri", 2),
    ("Type", 3),
    ("Updated", 3),
    ("Summary", 13),
];

/// Gaps inside a dashboard table.
const TABLE_COL_GAP: f32 = 12.0;
const TABLE_ROW_GAP: f32 = 4.0;

/// What the Jira section shows when it has nothing to connect to.
///
/// Rendered as the dashboard itself rather than raised as an alert: an
/// unconfigured integration isn't an error, and setup instructions are something
/// to read and copy from, which a dialog with an OK button is bad at.
///
/// No title heading — `jira_section` draws one as chrome above the grid.
fn jira_setup_help() -> String {
    format!(
        "Not configured yet. Add a `[jira]` section to `config.toml`:\n\n\
         ```toml\n\
         [jira]\n\
         site = \"your-company\"\n\
         email = \"you@your-company.com\"\n\
         query = \"assignee = currentUser() AND statusCategory != Done ORDER BY updated DESC\"\n\
         ```\n\n\
         Then store an API token in the Keychain:\n\n\
         ```\n{}\n```\n\n\
         Create the token at `id.atlassian.net/manage-profile/security/api-tokens`.\n\n\
         The token is kept out of `config.toml` on purpose: that file is plain \
         text and shared with v1.\n"
    ,
        sacrament_core::secret::store_hint(JIRA_KEYCHAIN_SERVICE, "you@your-company.com")
    )
}

/// A tab being dragged to a new position within its own strip.
///
/// The reorder happens **live**, as the pointer crosses each tab, so the strip
/// shows the result instead of describing it.
///
/// Moving live is what makes oscillation possible, and it is not hypothetical.
/// Drag a narrow tab past a wide one: the swap puts the narrow tab where the
/// wide one began, which leaves the pointer still inside the *wide* tab. The
/// very next pointer movement fires `on_enter` for it and swaps back, and the
/// pair flip-flops for as long as the mouse moves.
///
/// The fix is direction, not geometry: a tab only moves *forward* into a tab
/// ahead of it while the pointer is travelling right, and *backward* into one
/// behind it while travelling left. The rebound above asks to move backward
/// during a rightward drag, so it's refused. Undoing a move then requires
/// actually reversing direction, which is exactly the hysteresis a midpoint
/// rule would give — without needing to know where any midpoint is.
#[derive(Debug, Clone, Copy, PartialEq)]
struct TabDrag {
    group: TabGroup,
    /// Where the drag started, so a release can tell whether anything changed.
    origin: usize,
    /// Where the dragged tab sits *now* — it moves as the pointer does.
    at: usize,
    /// Pointer x within the strip, from the previous move.
    last_x: Option<f32>,
    /// Sign of the last horizontal movement: positive is rightward.
    dir: f32,
}

/// The prompt input, so opening the prompt can move keyboard focus into it.
static PROMPT_ID: std::sync::LazyLock<iced::widget::Id> =
    std::sync::LazyLock::new(iced::widget::Id::unique);

/// The native save panel.
///
/// Async, and driven through `Task::perform`, because a blocking dialog on the
/// UI thread deadlocks against the event loop that has to keep drawing it.
/// Overwrite confirmation comes from the panel itself — which is the main reason
/// this replaced a hand-rolled prompt, since "the file exists, are you sure" is
/// exactly the part that had to be reinvented before.
fn save_as_dialog_for(current: Option<std::path::PathBuf>, then: AfterSave) -> Task<Message> {
    Task::perform(
        async move {
            let mut dialog = rfd::AsyncFileDialog::new();
            if let Some(path) = &current {
                if let Some(dir) = path.parent() {
                    dialog = dialog.set_directory(dir);
                }
                if let Some(name) = path.file_name() {
                    dialog = dialog.set_file_name(name.to_string_lossy().into_owned());
                }
            }
            dialog.save_file().await.map(|h| h.path().to_path_buf())
        },
        move |picked| Message::SaveAsPicked(picked, then),
    )
}

/// The native open panel. Multi-select, since tabs are cheap.
fn open_dialog() -> Task<Message> {
    Task::perform(
        async {
            rfd::AsyncFileDialog::new()
                .pick_files()
                .await
                .map(|files| files.iter().map(|f| f.path().to_path_buf()).collect())
                .unwrap_or_default()
        },
        Message::OpenPicked,
    )
}

/// A Save / Don't Save / Cancel confirmation.
fn confirm_unsaved(title: &str, body: String) -> impl std::future::Future<Output = Answer> {
    let dialog = rfd::AsyncMessageDialog::new()
        .set_level(rfd::MessageLevel::Warning)
        .set_title(title)
        .set_description(body)
        .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
            "Save".into(),
            "Don't Save".into(),
            "Cancel".into(),
        ));
    async move {
        match dialog.show().await {
            rfd::MessageDialogResult::Custom(label) if label == "Save" => Answer::Save,
            rfd::MessageDialogResult::Custom(label) if label == "Don't Save" => Answer::Discard,
            // Everything else — Cancel, or the dialog dismissed some other way —
            // is the safe reading: do nothing.
            _ => Answer::Cancel,
        }
    }
}

/// An informational alert with a single OK button.
///
/// Async through `Task::perform` for the same reason every other dialog here is:
/// a blocking dialog on the UI thread deadlocks against the event loop that has
/// to keep drawing it. `AlertDismissed` is where the future lands.
fn alert_dialog(body: String) -> Task<Message> {
    Task::perform(
        async move {
            rfd::AsyncMessageDialog::new()
                .set_level(rfd::MessageLevel::Warning)
                .set_title("Sacrament")
                .set_description(body)
                .set_buttons(rfd::MessageButtons::Ok)
                .show()
                .await;
        },
        |()| Message::AlertDismissed,
    )
}

/// Backslash-escape a path for insertion at a shell prompt.
///
/// This is what a terminal does when a file is dropped into it, and it's the
/// reason a path with a space in it still arrives as one word. Alphanumerics —
/// including non-ASCII letters, which aren't shell-special and would only be
/// turned into noise — plus the punctuation that can't be misread pass through
/// unchanged; everything else is escaped, which is the safe default for the
/// characters this list hasn't thought about.
/// Hand a URL to the system browser.
///
/// `spawn`, not `output`: the UI thread must not wait on a browser launching, and
/// there is nothing to read back. A failure is dropped because there is nothing
/// useful to say — the opener reports its own problems, and an alert here would
/// fire for a browser that merely took its time.
///
/// The scheme is already restricted to http/https by `Terminal::url_at`, which is
/// where that check belongs: `open` will launch a registered handler for *any*
/// scheme, and terminal output is frequently attacker-influenced.
fn open_url(url: &str) {
    #[cfg(target_os = "macos")]
    let opener = "open";
    #[cfg(not(target_os = "macos"))]
    let opener = "xdg-open";
    let _ = std::process::Command::new(opener).arg(url).spawn();
}

fn shell_escaped(path: &std::path::Path) -> String {
    let s = path.to_string_lossy();
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let safe = c.is_alphanumeric()
            || matches!(c, '/' | '.' | '_' | '-' | '+' | ',' | ':' | '@' | '%' | '=');
        if !safe {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Move one element of a `Vec` to another index, returning whether it happened.
///
/// Remove-then-insert, deliberately, rather than a swap: dragging a tab three
/// places left should slide the three it passes one step right, not exchange the
/// endpoints. Because `to` indexes the list *before* the removal, no adjustment
/// is needed in either direction — removing an earlier element shifts the target
/// left by exactly the one position the insert then accounts for.
fn move_item<T>(items: &mut Vec<T>, from: usize, to: usize) -> bool {
    if from == to || from >= items.len() || to >= items.len() {
        return false;
    }
    let item = items.remove(from);
    items.insert(to, item);
    true
}

/// Is this the app's non-`Cmd` chord?
///
/// Two bindings sit outside the Cmd scheme, both because the macOS convention for
/// their key is already spoken for: pane focus (`Cmd+1..9` is "select tab N") and
/// goto-line (`Cmd+G` is "find next"). Ctrl is where editors put both.
///
/// Off macOS `Modifiers::command()` *is* `Ctrl`, so plain `Ctrl+G` would be
/// ambiguous with the Cmd table; `Ctrl+Alt` is the fallback there.
fn is_app_ctrl(mods: iced::keyboard::Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        mods.control() && !mods.alt()
    } else {
        mods.control() && mods.alt()
    }
}

/// Which tab strip a hover belongs to. Hover has to be tracked in app state
/// because the tabs aren't `button`s — see `tab_strip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TabGroup {
    /// The editor pane's *outer* strip — which `Section` is showing. The file
    /// tabs below it are `TabGroup::Editor`, one level in.
    Section,
    Editor,
    Shell(PaneId),
}

impl TabGroup {
    /// Can this strip's tabs be dragged into a new order?
    ///
    /// Sections are a fixed set the app defines, not a collection the user
    /// opened, so there is nothing to reorder — and a drag would have to persist
    /// an order that means nothing on the next launch. Answered by the group
    /// rather than by a parameter to `tab_strip`, so the fact lives in one place
    /// instead of at every call site.
    fn reorderable(self) -> bool {
        !matches!(self, TabGroup::Section)
    }
}

/// What the editor pane is showing.
///
/// The pane is no longer just the editor: it holds several *sections*, of which
/// the editor is one, and the open files are subtabs of that one. Adding a
/// section means a variant, a `label`, and an arm in `view` — `ALL` drives the
/// strip, so nothing else has to learn that the set grew.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Section {
    Editor,
    Jira,
}

impl Section {
    /// Strip order, left to right. A tab press carries an index into this, so it
    /// is the single definition of both the set and its order — `section_bar`
    /// renders from it and `select_section` reads back through it.
    const ALL: [Section; 2] = [Section::Editor, Section::Jira];

    fn label(self) -> &'static str {
        match self {
            Section::Editor => "Editor",
            Section::Jira => "Jira",
        }
    }
}

/// The Jira section's state.
///
/// The dashboard is built from **real widgets**, not from the markdown renderer:
/// its tables have to be responsive, and a table drawn as text can only be sized
/// for one pane width. `core::jira` therefore hands over structured `Issue`s and
/// the frontend lays them out. See `docs/jira-integration.md`.
enum JiraView {
    /// A fetch is in flight.
    Loading,
    /// Issues to draw as tables.
    Ready(sacrament_core::jira::Page),
    /// Something to say instead: setup instructions, or why a fetch failed.
    Note(String),
}

struct JiraPane {
    view: JiraView,
    /// A fetch is in flight. Gates a second one — the refresh key is easy to
    /// lean on, and Jira rate-limits. Kept alongside `JiraView::Loading` because
    /// it must stay true across the whole request, including while an error note
    /// from a previous attempt is still on screen.
    loading: bool,
    /// Whether a fetch has ever been attempted. Drives the first-show fetch, and
    /// keeps a failed attempt from re-firing every time the section is selected.
    attempted: bool,
    /// Pointer is over the refresh control.
    ///
    /// A plain `bool` rather than the `Option<(group, index)>` the tab strips need:
    /// there is exactly one hoverable control here, so `on_exit` can't be clearing
    /// a hover that a sibling's `on_enter` just set. That tree-order hazard is real
    /// on the strips and simply absent with one control.
    hovered: bool,
}

impl JiraPane {
    fn new() -> Self {
        Self {
            view: JiraView::Loading,
            loading: false,
            attempted: false,
            hovered: false,
        }
    }
}

/// Which pane receives keystrokes. There is exactly one, always — v1 has the
/// same rule (`PaneFocus`), and without it every key went to the PTY while the
/// editor drew a caret it couldn't honor.
///
/// `Editor` names the **pane**, not the section inside it. Since the pane gained
/// sections, focus alone no longer says the text surface is on screen — that's
/// `State::editing`, and every command that touches the buffer asks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    Editor,
    Shell(PaneId),
}

/// One shell: its own PTY, VT parser, and input handle.
struct Shell {
    key: ShellKey,
    /// Directory to start in. Only set for restored shells; `None` means the
    /// process cwd. Part of the subscription data, so it must not change after the
    /// shell starts.
    start_cwd: Option<std::path::PathBuf>,
    terminal: Arc<Mutex<Terminal>>,
    handle: Option<pty::Handle>,
    /// Viewport rows, reported by that pane's grid.
    rows: usize,
    /// The shell's pid, once spawned. Needed to read its working directory.
    pid: Option<u32>,
    /// Tab label: the cwd's basename, following `cd`.
    label: String,
    /// Throttles the cwd syscall — output can arrive thousands of times a second
    /// during a flood, and the directory changes at human speed.
    last_cwd_check: Option<Instant>,
    /// Sub-row scroll offset for this shell's grid, in pixels. See
    /// `State::editor_scroll_px`. Forced to zero unless scrolled into scrollback:
    /// the extra grid line a partial row needs only exists above the live screen,
    /// and live output should not sit half a row out of line anyway.
    scroll_px: f32,
}

impl Shell {
    /// A brand-new shell, started at home.
    ///
    /// `None` would mean "wherever the editor was launched from", which is the
    /// last project or `/` depending on how it was started — unpredictable from
    /// the user's side. Restored shells go through `in_dir` with their saved
    /// directory and are unaffected.
    fn new(key: ShellKey, size: Option<(usize, usize)>) -> Self {
        Self::in_dir(key, sacrament_core::paths::home_dir(), size)
    }

    /// `size` is the pane's measured body size, or `None` before any grid has
    /// reported one.
    ///
    /// **A new tab in an already-measured pane must start at that size, not at the
    /// placeholder**, because nothing will correct it: `GridView` publishes a size
    /// only when it *changes* (`State::reported`), and iced reuses one widget state
    /// for the pane's grid however many tabs come and go — so a new tab in a pane
    /// whose size is already published sees no `GridResized` at all. Its PTY is
    /// still told the truth (`Event::Attached` reads the pane), which is precisely
    /// what makes the mismatch visible: zsh writes `COLUMNS` cells into a grid
    /// 80 wide, they wrap, and its partial-line marker is stranded on the row above
    /// the prompt.
    fn in_dir(
        key: ShellKey,
        start_cwd: Option<std::path::PathBuf>,
        size: Option<(usize, usize)>,
    ) -> Self {
        let (rows, cols) = size.unwrap_or((24, 80));
        Self {
            key,
            label: start_cwd
                .as_deref()
                .map(sacrament_core::proc::dir_label)
                .unwrap_or_else(|| {
                    std::env::current_dir()
                        .map(|p| sacrament_core::proc::dir_label(&p))
                        .unwrap_or_else(|_| "shell".to_string())
                }),
            start_cwd,
            terminal: Arc::new(Mutex::new(Terminal::new(rows, cols))),
            handle: None,
            rows,
            pid: None,
            last_cwd_check: None,
            scroll_px: 0.0,
        }
    }
}

/// A shell pane: several shells with one active, like v1's `ShellPane`.
struct ShellPane {
    shells: Vec<Shell>,
    active: usize,
    /// The body size every shell in this pane is drawn at, once a grid has
    /// measured it. `None` until the first frame.
    ///
    /// **Held per pane rather than per shell because only the active shell's grid
    /// exists.** `view` builds a `GridView` for `active()` alone, so an inactive
    /// tab never lays out and never reports a size — and a restored session's
    /// inactive tabs would therefore wait out `pty::SIZE_WAIT` and spawn their
    /// shells at the 24x80 placeholder, which is what they were still showing when
    /// you switched to them. Every tab in a pane shares the pane's geometry, so
    /// the size one grid measures is the right size for all of them.
    size: Option<(usize, usize)>,
}

impl ShellPane {
    /// The active shell, or `None` when the pane is empty. A pane *is* allowed to
    /// be empty — closing the last tab leaves it blank until `+` is clicked, which
    /// is v1's behavior and documented in its README.
    fn active(&self) -> Option<&Shell> {
        self.shells.get(self.active)
    }


    fn find_mut(&mut self, key: ShellKey) -> Option<&mut Shell> {
        self.shells.iter_mut().find(|s| s.key == key)
    }
}

struct State {
    /// Shared with the widget, which reads it during `draw`. `Arc<Mutex<_>>`
    /// because `draw` takes `&self` while `update` needs to mutate on every
    /// output chunk. Contention is nil in practice — both run on the UI thread.
    bottom: ShellPane,
    right: ShellPane,
    /// Never-reused counter behind `ShellKey::serial`. See its docs for why reuse
    /// would be a bug rather than an optimization.
    next_shell_serial: u64,
    /// Open buffers, in tab order. `Arc<Mutex<_>>` per buffer because the gutter
    /// widget and the grid's source both need the active one, and each locks
    /// independently at draw time.
    buffers: Vec<Arc<Mutex<Buffer>>>,
    /// Index into `buffers`. Kept valid by `close_tab`, which is the only thing
    /// that can invalidate it.
    active: usize,
    /// Which section the editor pane is showing. Not persisted: a section has no
    /// state of its own to restore, and landing on the editor is the right
    /// default for a launch that was given files to open.
    section: Section,
    /// The Jira section's dashboard and fetch state.
    jira: JiraPane,
    /// `None` when `syntax_highlighting = false`.
    highlighter: Option<Arc<sacrament_core::highlight::Highlighter>>,
    panes: pane_grid::State<PaneKind>,
    metrics: Metrics,
    /// Resolved once at startup from `[theme]` in config.toml.
    palette: Palette,
    /// Resolved once at startup from `[font]` in config.toml.
    font: FontSpec,
    focus: Focus,
    /// The whole loaded config, so options are read where they're used rather
     /// than copied into a field each. v1 and v2 share `config.toml`.
    config: sacrament_core::config::Config,
    /// Live geometry, written to the session on close. Pane ratios are tracked here
    /// because `pane_grid::State` doesn't expose them for reading.
    geometry: sacrament_core::session::Geometry,
    /// The two splits, captured when created so a `ResizeEvent` can be attributed
    /// to the right one — `ResizeEvent` carries a `Split` id and nothing else.
    split_vertical: Option<pane_grid::Split>,
    split_horizontal: Option<pane_grid::Split>,
    /// Which tab the pointer is over, for hover styling.
    hovered_tab: Option<(TabGroup, usize)>,
    /// An in-progress tab reorder.
    tab_drag: Option<TabDrag>,
    /// Window or pane sizes changed since the last session write. Flushed on
    /// mouse-up rather than per event — a splitter drag emits one per frame.
    geometry_dirty: bool,
    /// The bottom prompt, when one is open.
    prompt: Option<Prompt>,
    /// Last find query, so `Cmd+G` can repeat it after the prompt has closed.
    last_query: Option<String>,
    /// Editor viewport height in rows, reported by the grid. Needed so keyboard
    /// scrolling and cursor-following can clamp against the real viewport
    /// rather than a guess.
    editor_rows: usize,
    /// Sub-row scroll offset for the editor grid, in pixels the content is shifted
    /// **up** by. Always in `[0, cell_height)`.
    ///
    /// In `State` rather than in `Buffer` or the widget: the grid and the gutter are
    /// separate widgets that must shift identically, and neither can read the
    /// other's state. Not persisted — it is a fraction of a row, and a restored
    /// session landing on a row boundary is right.
    editor_scroll_px: f32,
    /// Columns the editor grid last reported. Read mode wraps to the real pane
    /// width regardless of `word_wrap`, so it needs this even when the editor's
    /// own wrap width is zero.
    editor_cols: usize,
    /// Messages queued to become native alerts, drained by `update` once the
    /// message that produced them has been fully handled.
    ///
    /// A queue rather than a `Task` returned from each site: the things that need
    /// to say something — a failed save, a search that found nothing, a comment
    /// key with no comment syntax — are plain `&mut self` methods, and threading
    /// a `Task` back out of every one of them would restructure half the file to
    /// deliver a dialog.
    alerts: Vec<String>,
    /// What the alert currently on screen says. A burst of identical messages is
    /// real — the file watcher emits several events for one write, and a refused
    /// reload reports on each — and stacking a dialog per event would bury the
    /// window. Cleared when the alert is acknowledged, so the same message can be
    /// raised again later.
    showing_alerts: Vec<String>,
}

impl State {
    fn new() -> (Self, Task<Message>) {
        // v1's arrangement: a left column of editor-over-shell, and a full-height
        // shell down the right. Split the right off first so it spans both.
        let (mut panes, editor) = pane_grid::State::new(PaneKind::Editor);
        let split_vertical = panes
            .split(
                pane_grid::Axis::Vertical,
                editor,
                PaneKind::Shell(PaneId::Right),
            )
            .map(|(_, split)| split);
        let split_horizontal = panes
            .split(
                pane_grid::Axis::Horizontal,
                editor,
                PaneKind::Shell(PaneId::Bottom),
            )
            .map(|(_, split)| split);

        let saved = sacrament_core::session::load(sacrament_core::APP_GUI);

        // Restore the shell panes. A PTY isn't serializable, so what persists is
        // each shell's *directory* — restore re-spawns there. A pane with nothing
        // saved gets one shell in the process cwd, which is also the first-run path.
        let mut serial = 0u64;
        let mut restore_pane = |id: PaneId, saved_shells: &[sacrament_core::session::ShellTabSession], active: usize| {
            let mut shells: Vec<Shell> = saved_shells
                .iter()
                .map(|s| {
                    let key = ShellKey { pane: id, serial };
                    serial += 1;
                    // No grid has laid out yet at startup, so there is no size to
                    // pass; the first `GridResized` reaches every tab in the pane.
                    Shell::in_dir(key, Some(s.cwd.clone()), None)
                })
                .collect();
            if shells.is_empty() {
                let key = ShellKey { pane: id, serial };
                serial += 1;
                shells.push(Shell::new(key, None));
            }
            let active = active.min(shells.len() - 1);
            ShellPane {
                shells,
                active,
                size: None,
            }
        };
        let (bottom, right) = match &saved {
            Some(sess) => (
                restore_pane(PaneId::Bottom, &sess.bottom_shells, sess.bottom_active),
                restore_pane(PaneId::Right, &sess.right_shells, sess.right_active),
            ),
            None => (
                restore_pane(PaneId::Bottom, &[], 0),
                restore_pane(PaneId::Right, &[], 0),
            ),
        };
        let geometry = saved
            .as_ref()
            .map(|s| s.geometry.sanitized())
            .unwrap_or_default();
        // Restore the split positions the window was left at.
        if let Some(split) = split_vertical {
            panes.resize(split, geometry.vertical_split);
        }
        if let Some(split) = split_horizontal {
            panes.resize(split, geometry.horizontal_split);
        }
        let config = sacrament_core::config::load();
        let tab_width = config.tab_width.max(1);
        let syntax_on = config.syntax_highlighting;
        // Validate the requested family against what iced can actually load, so
        // a typo degrades to the default monospace instead of drawing nothing
        // (Shaping::Basic has no font fallback).
        // One font-database load feeds both family validation and glyph coverage.
        let fonts = font::SystemFonts::load();
        let (font, font_warning) = font::resolve(&config.font, &fonts.families());
        // Leaked once at startup so `FontSpec` stays `Copy` — same rationale as
        // the family name. Without coverage, a character the font lacks draws as
        // nothing, since `Shaping::Basic` does no fallback.
        let coverage = fonts
            .coverage(&font.font.family)
            .map(|c| &*Box::leak(Box::new(c)));
        // The chain that draws what the configured font can't. Leaked for the same
        // reason, and it consumes `fonts` because its lookups happen at draw time
        // rather than here — see `font::Fallback`.
        let fallback: &'static font::Fallback = Box::leak(Box::new(fonts.into_fallback()));
        let font = font
            .with_coverage(coverage)
            .with_fallback(Some(fallback));

        // Open a file if one was given on the command line, else an empty
        // buffer. Real argument parsing and the client/server open flow come
        // with the socket work; this is enough to see a file on screen.
        // `syntax_highlighting = false` skips building the highlighter at all,
        // not just skipping its output: `Highlighter::new` loads syntect's whole
        // default syntax set, which is the startup cost and memory the option
        // exists to avoid.
        let highlighter =
            syntax_on.then(|| Arc::new(sacrament_core::highlight::Highlighter::new()));
        let palette = Palette::from_theme(&config.theme);
        // Every path on the command line opens as a tab. An empty buffer only
        // when nothing was given, so `sacrament2 a.rs b.rs` does the obvious thing.
        //
        // Parsed with the same `parse_args` `main` used, rather than treating
        // every argument as a path — otherwise `--syntax=Rust` becomes a request
        // to open a file by that name.
        let argv: Vec<String> = std::env::args().skip(1).collect();
        let cli = parse_args(&argv).unwrap_or(Args {
            files: Vec::new(),
            syntax: None,
            review: false,
        });
        let mut buffers: Vec<Arc<Mutex<Buffer>>> = cli
            .files
            .iter()
            .filter_map(|(path, line)| {
                let mut b = Buffer::load(path, highlighter.as_deref()).ok()?;
                b.tab_width = tab_width;
                if let Some(name) = &cli.syntax
                    && let Some(hl) = highlighter.as_deref()
                {
                    b.set_syntax_override(name, hl);
                }
                if let Some(n) = line {
                    b.goto_line(*n);
                }
                Some(Arc::new(Mutex::new(b)))
            })
            .collect();
        // Only restore when nothing was named on the command line — an explicit
        // `sacrament2 foo.rs` means "open this", not "and also everything from
        // last time". Same rule as v1.
        let mut active = 0;
        if buffers.is_empty()
            && let Some(saved) = &saved
        {
            for sb in &saved.buffers {
                if let Ok(mut b) = Buffer::load(&sb.path, highlighter.as_deref()) {
                    b.tab_width = tab_width;
                    if let Some(name) = &sb.syntax_override
                        && let Some(hl) = highlighter.as_deref()
                    {
                        b.set_syntax_override(name, hl);
                    }
                    // Clamp: the file may have shrunk since it was saved.
                    b.cursor_row = sb.cursor_row.min(b.line_count().saturating_sub(1));
                    b.cursor_col = sb.cursor_col;
                    b.scroll_row = sb.scroll_row.min(b.line_count().saturating_sub(1));
                    b.scroll_col = sb.scroll_col;
                    b.clamp_to_content();
                    // After the text is in place: restoring a fold needs the
                    // line count to validate against.
                    b.set_fold_ranges(&sb.folds);
                    if sb.read_mode {
                        b.set_read_mode();
                    }
                    buffers.push(Arc::new(Mutex::new(b)));
                }
            }
            active = saved.active.min(buffers.len().saturating_sub(1));
        }
        if buffers.is_empty() {
            let mut b = Buffer::empty();
            b.tab_width = tab_width;
            buffers.push(Arc::new(Mutex::new(b)));
        }
        let state = Self {
            bottom,
            right,
            next_shell_serial: serial,
            buffers,
            active,
            section: Section::Editor,
            jira: JiraPane::new(),
            highlighter,
            config,
            panes,
            geometry,
            split_vertical,
            split_horizontal,
            hovered_tab: None,
            tab_drag: None,
            geometry_dirty: false,
            prompt: None,
            last_query: None,
            metrics: Metrics::new(),
            palette,
            font,
            focus: Focus::Editor,
            editor_rows: 24,
            editor_cols: 80,
            editor_scroll_px: 0.0,
            alerts: Vec::new(),
            showing_alerts: Vec::new(),
        };
        // `persist` keeps these in step later, but it hasn't run yet.
        state.sync_watches();
        // A font family that didn't resolve is a config error, so it's said once
        // at startup rather than queued — nothing has happened yet to queue it
        // behind.
        let boot = match font_warning {
            Some(w) => alert_dialog(w),
            None => Task::none(),
        };
        (state, boot)
    }

    /// Window title carries the filename and the dirty marker. That's the save
    /// feedback: there's no status bar, and a dirty dot that disappears on save
    /// tells you more, continuously, than a message that flashes once.
    fn title(&self) -> String {
        let (name, dirty) = self
            .buf()
            .lock()
            .map(|b| (b.display_name(), b.dirty))
            .unwrap_or_else(|_| ("[no file]".to_string(), false));
        if dirty {
            format!("• {name} — sacrament")
        } else {
            format!("{name} — sacrament")
        }
    }

    /// The active buffer. `active` is always in range — `close_tab` is the only
    /// operation that can shrink `buffers`, and it clamps.
    fn buf(&self) -> &Arc<Mutex<Buffer>> {
        &self.buffers[self.active]
    }

    fn select_tab(&mut self, index: usize) {
        if index < self.buffers.len() {
            self.active = index;
            self.focus = Focus::Editor;
            self.mark_active_reviewed();
            self.persist();
        }
    }

    /// Is the editor's text surface the thing on screen and holding the keyboard?
    ///
    /// `Focus::Editor` names the pane, and the pane holds sections now, so focus
    /// alone no longer answers this: with Jira showing there is no text and no
    /// caret, and routing a keystroke — or a `Cmd+Z`, or a comment toggle — to the
    /// active buffer would edit a file the user cannot see. That's the same class
    /// of hole as read mode, so it's closed the same way: at the routing layer,
    /// once, rather than inside each command.
    ///
    /// Read mode is deliberately *not* folded in here. It's a different question
    /// — the text is on screen, it just can't be typed into — and the two have
    /// different answers for navigation keys.
    fn editing(&self) -> bool {
        self.focus == Focus::Editor && self.section == Section::Editor
    }

    /// Put the editor section in front of the user and give it the keyboard.
    ///
    /// Called by the commands whose whole purpose is to show something —
    /// opening a file, a new buffer, a search, read mode. Those have to bring the
    /// editor back into view, or they act on a surface that isn't being displayed.
    /// The commands that instead act on *what is already on screen* (typing,
    /// undo, save, close tab) stay inert while another section shows; see
    /// `editing`.
    fn show_editor(&mut self) {
        self.focus = Focus::Editor;
        self.section = Section::Editor;
    }

    /// Switch which section the editor pane shows. `index` is into `Section::ALL`.
    fn select_section(&mut self, index: usize) -> Task<Message> {
        let Some(&section) = Section::ALL.get(index) else {
            return Task::none();
        };
        self.section = section;
        // The strip belongs to the editor pane, so pressing it is interacting
        // with that pane — the same reason `select_tab` takes focus.
        self.focus = Focus::Editor;
        // Fetch on first show rather than at startup: an app launched to edit a
        // file shouldn't make a network request nobody asked for. `attempted`
        // rather than "is it empty" so a failure doesn't re-fire on every visit.
        if section == Section::Jira && !self.jira.attempted {
            return self.jira_refresh();
        }
        Task::none()
    }

    /// Fetch the Jira dashboard in the background.
    ///
    /// `Task::perform` rather than the channel-and-subscription pattern `pty` and
    /// `watch` use: this is one request and one response, not a stream. The
    /// blocking call occupies a thread-pool thread for its duration, which is fine
    /// at human pace and keeps `tokio` out of the tree.
    fn jira_refresh(&mut self) -> Task<Message> {
        if self.jira.loading {
            return Task::none();
        }
        let config = self.config.jira.clone();
        if !config.is_configured() {
            // Not an error — an unconfigured integration is a normal state. Say
            // what to add rather than reporting a failure.
            self.jira.attempted = true;
            self.jira.view = JiraView::Note(jira_setup_help());
            return Task::none();
        }
        self.jira.loading = true;
        self.jira.attempted = true;
        // Show the loading state *before* the fetch starts, so a refresh looks
        // exactly like the first visit. Without this the previous dashboard stayed
        // on screen for the length of the request and the click read as a no-op —
        // the control's colour doesn't change on press, so replacing the body is
        // the only feedback there is.
        self.jira.view = JiraView::Loading;
        Task::perform(
            async move {
                let Some(token) = sacrament_core::secret::lookup(
                    JIRA_TOKEN_ENV,
                    JIRA_KEYCHAIN_SERVICE,
                    &config.email,
                ) else {
                    return Err(format!(
                        "No Jira API token for {}. Store one with:\n\n{}",
                        config.email,
                        sacrament_core::secret::store_hint(
                            JIRA_KEYCHAIN_SERVICE,
                            &config.email
                        )
                    ));
                };
                sacrament_core::jira::fetch(&config, &token)
            },
            Message::JiraLoaded,
        )
    }

    /// Ask the system where to save, then save there.
    fn save_as_dialog(&self) -> Task<Message> {
        let current = self
            .buf()
            .lock()
            .ok()
            .and_then(|b| b.path().map(|p| p.to_path_buf()));
        save_as_dialog_for(current, AfterSave::Nothing)
    }

    /// Fold or unfold. `shift` widens it to the whole file.
    fn fold_command(&mut self, unfold: bool, all: bool) {
        if !self.editing() {
            return;
        }
        let rows = self.editor_rows;
        if let Ok(mut b) = self.buf().lock() {
            match (all, unfold) {
                (true, true) => b.unfold_all(),
                (true, false) => b.fold_all(),
                // On a line that heads no block, walk outward to the block this
                // line is *inside* — folding "here" should work from the body,
                // not only from the header.
                (false, _) => {
                    let row = b.enclosing_fold_head(b.cursor_row);
                    if unfold {
                        if b.fold_mark(row) == Some(buffer::FoldMark::Closed) {
                            b.toggle_fold(row);
                        }
                    } else if b.fold_mark(row) == Some(buffer::FoldMark::Open) {
                        b.toggle_fold(row);
                    }
                }
            }
            b.ensure_cursor_visible(rows);
        }
    }

    /// Indent or outdent the selected lines in the editor.
    fn reindent(&mut self, deeper: bool) {
        if !self.editing() {
            return;
        }
        let (tw, tabs) = (self.config.tab_width.max(1), self.config.indent_with_tabs);
        let rows = self.editor_rows;
        if let Ok(mut b) = self.buf().lock() {
            if deeper {
                b.indent_selection(tw, tabs);
            } else {
                b.outdent_selection(tw);
            }
            b.ensure_cursor_visible(rows);
        }
    }

    /// Switch the active buffer between source and rendered markdown.
    ///
    /// Shows the editor first: this is a command whose result is something to
    /// look at, so it has to bring the surface it renders into view.
    fn toggle_read_mode(&mut self) {
        self.show_editor();
        let ok = self
            .buf()
            .lock()
            .map(|mut b| b.toggle_read_mode())
            .unwrap_or(false);
        if !ok {
            self.alert("Not a markdown file.");
        }
    }

    /// Comment or uncomment the selected lines.
    ///
    /// The marker comes from the buffer's syntax, so a file the highlighter
    /// doesn't recognise has none — that's reported rather than silently doing
    /// nothing, since an unresponsive key reads as broken.
    fn toggle_comment(&mut self) {
        if !self.editing() {
            return;
        }
        let rows = self.editor_rows;
        let Ok(mut b) = self.buf().lock() else { return };
        let Some(syntax) = b.syntax_name().map(str::to_string) else {
            drop(b);
            self.alert("No syntax for this file — nothing to comment with.");
            return;
        };
        let Some(prefix) = sacrament_core::highlight::line_comment_for(&syntax) else {
            drop(b);
            self.alert(format!("No comment style known for {syntax}."));
            return;
        };
        b.toggle_comment(prefix);
        b.ensure_cursor_visible(rows);
    }

    /// Looking at a tab counts as reviewing it.
    ///
    /// Called from the paths where the *user* chose a tab, not from every place
    /// `active` moves: closing a tab or reordering one shifts the index without
    /// anyone having read what's in it.
    fn mark_active_reviewed(&mut self) {
        if let Ok(mut b) = self.buf().lock() {
            b.set_unreviewed(false);
        }
    }

    fn cycle_tab(&mut self, forward: bool) {
        let n = self.buffers.len();
        if n < 2 {
            return;
        }
        self.active = if forward {
            (self.active + 1) % n
        } else {
            (self.active + n - 1) % n
        };
        self.focus = Focus::Editor;
        self.mark_active_reviewed();
        self.persist();
    }

    /// Close a tab. Closing the last one leaves an empty untitled buffer rather
    /// than zero buffers, so `buf()` never has to handle an empty list.
    /// Close a tab, asking about unsaved work first.
    ///
    /// Previously this just refused and left a message, which put the burden on
    /// the user to notice it. The system dialog is the right shape for the
    /// question: it's modal because the answer decides whether work survives.
    fn close_tab(&mut self, index: usize) -> Task<Message> {
        if index >= self.buffers.len() {
            return Task::none();
        }
        let (dirty, name) = self.buffers[index]
            .lock()
            .map(|b| (b.dirty, b.display_name()))
            .unwrap_or((false, "this file".to_string()));
        if dirty {
            return Task::perform(
                confirm_unsaved(
                    "Unsaved changes",
                    format!("Save changes to {name} before closing?"),
                ),
                move |answer| Message::CloseTabAnswer(index, answer),
            );
        }
        self.discard_tab(index);
        Task::none()
    }

    /// Close a tab without asking. Every path that decides it's safe ends here.
    ///
    /// Closing the last one leaves an empty untitled buffer rather than zero
    /// buffers, so `buf()` never has to handle an empty list.
    fn discard_tab(&mut self, index: usize) {
        if index >= self.buffers.len() {
            return;
        }
        self.buffers.remove(index);
        if self.buffers.is_empty() {
            let mut b = Buffer::empty();
            b.tab_width = self.config.tab_width.max(1);
            self.buffers.push(Arc::new(Mutex::new(b)));
        }
        self.active = self.active.min(self.buffers.len() - 1);
        self.persist();
    }

    /// Quit, asking about unsaved work first.
    fn request_quit(&mut self) -> Task<Message> {
        let dirty: Vec<String> = self
            .buffers
            .iter()
            .filter_map(|b| b.lock().ok())
            .filter(|b| b.dirty)
            .map(|b| b.display_name())
            .collect();
        if dirty.is_empty() {
            return self.quit_now();
        }
        let body = if dirty.len() == 1 {
            format!("Save changes to {} before quitting?", dirty[0])
        } else {
            format!(
                "Save changes to {} files before quitting?\n\n{}",
                dirty.len(),
                dirty.join(", ")
            )
        };
        Task::perform(confirm_unsaved("Unsaved changes", body), Message::QuitAnswer)
    }

    /// Save the session and go.
    fn quit_now(&mut self) -> Task<Message> {
        self.save_session();
        iced::exit()
    }

    /// Save every dirty buffer, then quit.
    ///
    /// Untitled buffers need a path, so each one opens a save panel whose
    /// completion re-enters here — the loop runs through the message system
    /// rather than blocking on a dialog.
    fn save_all_then_quit(&mut self) -> Task<Message> {
        for i in 0..self.buffers.len() {
            let Ok(mut b) = self.buffers[i].lock() else {
                continue;
            };
            if !b.dirty {
                continue;
            }
            if b.path().is_none() {
                drop(b);
                self.active = i;
                return save_as_dialog_for(None, AfterSave::Quit);
            }
            if let Err(e) = b.save() {
                drop(b);
                // Stop rather than quit: a failed save here means quitting would
                // throw away exactly what the user asked to keep.
                self.active = i;
                self.alert(e.to_string());
                return Task::none();
            }
        }
        self.quit_now()
    }

    fn pane_mut(&mut self, id: PaneId) -> &mut ShellPane {
        match id {
            PaneId::Bottom => &mut self.bottom,
            PaneId::Right => &mut self.right,
        }
    }

    fn pane(&self, id: PaneId) -> &ShellPane {
        match id {
            PaneId::Bottom => &self.bottom,
            PaneId::Right => &self.right,
        }
    }

    /// Locate a shell by key across both panes — how PTY events find their target,
    /// since a key is unique app-wide.
    fn shell_by_key(&mut self, key: ShellKey) -> Option<&mut Shell> {
        self.pane_mut(key.pane).find_mut(key)
    }

    /// Every live shell key, in pane order. Drives the subscription list, so this
    /// growing spawns a PTY and shrinking stops one.
    fn spawns(&self) -> Vec<Spawn> {
        PaneId::ALL
            .iter()
            .flat_map(|id| {
                self.pane(*id).shells.iter().map(|s| Spawn {
                    key: s.key,
                    cwd: s.start_cwd.clone(),
                })
            })
            .collect()
    }

    /// Update a shell's tab label from its process cwd.
    ///
    /// Polling the process rather than parsing OSC 7: v1 shipped both and concluded
    /// only this one was reliable (see `core::proc`).
    ///
    /// Deliberately **not** throttled. An earlier version skipped checks inside a
    /// 150ms window, which lost the update entirely whenever a `cd` produced its
    /// only output inside that window — nothing re-checked afterwards. The PTY
    /// coalescer already bounds `Output` to roughly one message per frame, so the
    /// syscall runs at frame rate at worst, which is nothing.
    fn refresh_cwd(&mut self, key: ShellKey) {
        let Some(shell) = self.shell_by_key(key) else {
            return;
        };
        let Some(pid) = shell.pid else {
            return;
        };
        let mut moved = false;
        if let Some(cwd) = sacrament_core::proc::cwd_of(pid) {
            let label = sacrament_core::proc::dir_label(&cwd);
            if shell.label != label {
                shell.label = label;
                moved = true;
            }
        }
        // Only on an actual `cd`. The poll itself runs every frame a shell
        // produces output, so writing unconditionally here would be a file write
        // per frame.
        if moved {
            self.persist();
        }
    }

    fn spawn_shell(&mut self, id: PaneId) {
        let key = ShellKey {
            pane: id,
            serial: self.next_shell_serial,
        };
        self.next_shell_serial += 1;
        let pane = self.pane_mut(id);
        let size = pane.size;
        pane.shells.push(Shell::new(key, size));
        pane.active = pane.shells.len() - 1;
        self.focus = Focus::Shell(id);
        self.persist();
    }

    /// Close a shell tab. Dropping it removes its key from the subscription list,
    /// which drops the stream and kills the child (see `pty::run`).
    fn close_shell(&mut self, id: PaneId, index: usize) {
        let pane = self.pane_mut(id);
        if index >= pane.shells.len() {
            return;
        }
        pane.shells.remove(index);
        pane.active = pane.active.min(pane.shells.len().saturating_sub(1));
        self.persist();
    }

    fn select_shell(&mut self, id: PaneId, index: usize) {
        let pane = self.pane_mut(id);
        if index < pane.shells.len() {
            pane.active = index;
        }
        self.focus = Focus::Shell(id);
        self.persist();
    }

    fn theme(&self) -> iced::Theme {
        self.palette.iced_theme()
    }

    fn subscription(&self) -> Subscription<Message> {
        // Push-driven: the PTY reader thread feeds an unbounded channel whose
        // receiver *is* the stream. No polling tick, unlike v1's event loop.
        // One subscription per shell. `run_with` hashes the id, so the two get
        // independent PTYs instead of being deduplicated into one.
        // One subscription per live shell. Because the list is rebuilt from state
        // each frame, adding a key spawns a PTY and removing one stops it — no
        // imperative spawn/kill calls anywhere.
        let ptys: Vec<_> = self
            .spawns()
            .into_iter()
            .map(|spawn| {
                Subscription::run_with(spawn, pty::stream).map(|(key, ev)| Message::Pty(key, ev))
            })
            .collect();
        // `listen_with` rather than `keyboard::listen()`, because the latter
        // yields only events the widget tree *ignored* — and the prompt's
        // `text_input` captures `Escape` (it unfocuses on it, calling
        // `shell.capture_event()`). With `listen()` the prompt could therefore
        // never be closed with `Escape`: the input would silently unfocus, every
        // later key would arrive uncaptured, and `update` would swallow them all
        // because a prompt was still open. It reads exactly like a frozen editor.
        //
        // Receiving captured events means `update` must not act on keys the
        // `text_input` already handled — see the prompt arm there, which handles
        // `Enter`/`Escape` and nothing else.
        let keys = iced::event::listen_with(|event, _status, _window| match event {
            iced::Event::Keyboard(iced::keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                text,
                ..
            }) => Some(Message::Key(
                key,
                physical_key,
                modifiers,
                text.map(|t| t.to_string()),
            )),
            // A tab drag has to end on *any* left release, not just one over a
            // tab: releasing past the end of the strip, or outside the window,
            // would otherwise leave the drag armed and reorder on the next click.
            iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                iced::mouse::Button::Left,
            )) => Some(Message::LeftReleased),
            // Files dragged in from the Finder. One event per file.
            iced::Event::Window(iced::window::Event::FileDropped(path)) => {
                Some(Message::FileDropped(path))
            }
            _ => None,
        });
        let resizes = iced::window::resize_events().map(|(_id, size)| Message::WindowResized(size));
        let closes = iced::window::close_requests().map(|_id| Message::CloseRequested);
        // Requests from other invocations. The listener was started in `main`,
        // before the window existed, so nothing queued in between is lost.
        let remote = Subscription::run(ipc::stream).map(Message::Remote);
        let files = Subscription::run(watch::stream).map(Message::FileChanged);

        Subscription::batch(
            ptys.into_iter()
                .chain([keys, resizes, closes, remote, files]),
        )
    }

    /// Handle a message, then raise anything it asked to tell the user.
    ///
    /// Flushing in one place is what lets every site queue a message with a plain
    /// assignment instead of returning a `Task`, and it's also the only point that
    /// can guarantee a queued message is shown exactly once.
    fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.handle(message);
        if self.alerts.is_empty() {
            return task;
        }
        // Everything one message produced goes in one dialog. Two dialogs from
        // one keystroke would be a queue the user has to clear.
        let queued = std::mem::take(&mut self.alerts);
        let body = queued.join("\n");
        self.showing_alerts = queued;
        Task::batch([task, alert_dialog(body)])
    }

    /// Queue a message to be shown as a native alert.
    ///
    /// Suppressed if it's already on screen or already queued — see
    /// `showing_alerts`.
    fn alert(&mut self, msg: impl Into<String>) {
        let msg = msg.into();
        if self.showing_alerts.contains(&msg) || self.alerts.contains(&msg) {
            return;
        }
        self.alerts.push(msg);
    }

    fn handle(&mut self, message: Message) -> Task<Message> {
        // Done here rather than in `main`: the menu doesn't exist until winit has
        // finished launching the application, and this has to run on the main
        // thread — which is where iced drives `update` from.
        static QUIT_ROUTED: std::sync::Once = std::sync::Once::new();
        QUIT_ROUTED.call_once(macos::route_quit_through_window_close);

        match message {
            Message::Pty(key, pty::Event::Attached(handle)) => {
                let bottom_first = key
                    == ShellKey {
                        pane: PaneId::Bottom,
                        serial: 0,
                    };
                // Only push a size the grid actually measured. Sending the
                // terminal's placeholder here would let the PTY spawn its shell at
                // 24x80, and the real size arriving later would trigger the
                // redraw this whole path exists to avoid. If nothing is measured
                // yet, `GridResized` pushes it the moment it is.
                //
                // Read from the *pane*, not this shell: a tab that has never been
                // active has no grid and so no size of its own, but its pane's
                // size applies to it just the same.
                let measured = self.pane(key.pane).size;
                let Some(shell) = self.shell_by_key(key) else {
                    return Task::none();
                };
                if let Some((rows, cols)) = measured {
                    handle.resize(rows as u16, cols as u16);
                }
                // SACRAMENT_SPIKE_CMD runs a command on attach so throughput can
                // be measured without typing. Only the bottom pane, or it would
                // run twice.
                if bottom_first
                    && let Ok(cmd) = std::env::var("SACRAMENT_SPIKE_CMD")
                {
                    handle.write(format!("{cmd}\n").into_bytes());
                }
                shell.handle = Some(handle);
            }
            Message::Pty(key, pty::Event::Started { pid }) => {
                if let Some(shell) = self.shell_by_key(key) {
                    shell.pid = pid;
                    shell.last_cwd_check = None;
                }
                self.refresh_cwd(key);
                // The pid is what makes this shell's directory readable, so the
                // session entry written at spawn time was a fallback until now.
                self.persist();
            }
            Message::Pty(key, pty::Event::Output(bytes)) => {
                let n = bytes.len();
                let t0 = std::time::Instant::now();
                if let Some(shell) = self.shell_by_key(key)
                    && let Ok(mut term) = shell.terminal.lock()
                {
                    term.feed(&bytes);
                }
                self.metrics.record_feed(n, t0.elapsed());
                // Output is exactly when a `cd` would have happened — the prompt
                // gets redrawn — so this is event-driven rather than a timer, and
                // costs nothing at idle. Throttled inside.
                self.refresh_cwd(key);
                if self.metrics.should_log() {
                    eprintln!("[metrics] {}", self.metrics.render());
                }
            }
            Message::Pty(key, pty::Event::Exited) => {
                // The shell's process ended (`exit`, or it was killed). Remove the
                // tab, matching v1: a dead shell isn't something to look at.
                let pane = self.pane_mut(key.pane);
                if let Some(i) = pane.shells.iter().position(|s| s.key == key) {
                    pane.shells.remove(i);
                    pane.active = pane.active.min(pane.shells.len().saturating_sub(1));
                }
            }
            Message::Pty(_, pty::Event::Failed(e)) => {
                self.alert(format!("The shell couldn't be started: {e}"));
            }
            Message::GridResized(key, rows, cols) => {
                let (rows, cols) = (rows.max(1), cols.max(1));
                // Applied to **every shell in the pane**, not just the one whose
                // grid reported. Only the active tab has a grid, so an inactive
                // tab would otherwise never learn its size — and on a restored
                // session that meant its PTY waited out `SIZE_WAIT` and spawned
                // zsh at 24x80. Tabs in a pane all share the pane's geometry, so
                // one measurement is the right answer for all of them.
                let pane = self.pane_mut(key.pane);
                pane.size = Some((rows, cols));
                for shell in &mut pane.shells {
                    shell.rows = rows;
                    // Grid and shell are resized together, every frame — no
                    // throttling. Keeping them in lockstep is what makes a drag
                    // look right: any delay leaves the grid holding content
                    // wrapped for a width the shell no longer has, which renders
                    // as fragments of adjacent lines until the shell catches up.
                    let changed = shell
                        .terminal
                        .lock()
                        .map(|mut t| t.resize(rows, cols))
                        .unwrap_or(false);
                    if changed && let Some(h) = &shell.handle {
                        h.resize(rows as u16, cols as u16);
                    }
                }
            }
            Message::EditorResized(rows, cols) => {
                self.editor_rows = rows.max(1);
                self.editor_cols = cols.max(1);
                // The buffer needs the viewport width to derive wrap segments.
                // `word_wrap = false` becomes width 0, which `text::wrap_line`
                // treats as "one segment" — no second code path.
                let width = if self.config.word_wrap { cols.max(1) } else { 0 };
                if let Ok(mut b) = self.buf().lock()
                    && b.wrap_width != width
                {
                    b.wrap_width = width;
                    b.ensure_cursor_visible(self.editor_rows);
                }
            }
            Message::FocusPane(focus) => self.focus = focus,
            Message::SpawnShell(id) => self.spawn_shell(id),
            Message::Mouse(focus, gesture) => return self.mouse(focus, gesture),
            Message::Pasted(text) => {
                if let Some(text) = text {
                    match self.focus {
                        // `editing()` and `reading()` are both required, and a
                        // paste is exactly where that gets forgotten: it arrives
                        // as its own message rather than through `keymap`, so
                        // neither the section arm nor the read-mode arm in the key
                        // dispatch covers it. v1 has this hole — pasting into read
                        // mode edits the source behind the rendering.
                        Focus::Editor if !self.editing() || self.reading() => {}
                        Focus::Editor => {
                            if let Ok(mut b) = self.buf().lock() {
                                b.insert_str(&text);
                                b.ensure_cursor_visible(self.editor_rows);
                            }
                        }
                        Focus::Shell(id) => self.paste_to_shell(id, &text),
                    }
                }
            }
            Message::PaneResized(pane_grid::ResizeEvent { split, ratio }) => {
                self.panes.resize(split, ratio);
                // Record it: `pane_grid::State` has no getter for a split's ratio,
                // so this event is the only place it can be observed.
                if Some(split) == self.split_vertical {
                    self.geometry.vertical_split = ratio;
                } else if Some(split) == self.split_horizontal {
                    self.geometry.horizontal_split = ratio;
                }
                self.geometry_dirty = true;
            }
            Message::TabHovered(which) => {
                // A drag follows the pointer by hover, which is why the drop
                // target costs no extra event plumbing.
                self.hovered_tab = which;
            }
            Message::TabExited(group, i) => {
                // Only clear if this is still the tab we think is hovered.
                //
                // Widgets publish in tree order, so moving the pointer *left*
                // from one tab to its neighbour emits the neighbour's `on_enter`
                // before the departed tab's `on_exit` — and an unconditional
                // clear then wiped the hover that had just been set. Rightward
                // moves happened to emit them in the useful order, which is why
                // this surfaced as "dragging left does nothing" rather than as a
                // hover bug.
                if self.hovered_tab == Some((group, i)) {
                    self.hovered_tab = None;
                }
            }
            Message::TabPointerMoved(group, x) => {
                let Some(drag) = &mut self.tab_drag else {
                    return Task::none();
                };
                if drag.group != group {
                    return Task::none();
                }
                // Measured against the strip, not against a tab: a position
                // relative to whichever tab is under the pointer jumps when the
                // pointer crosses between them, which would read as a direction
                // change that never happened.
                if let Some(prev) = drag.last_x
                    && (x - prev).abs() > f32::EPSILON
                {
                    drag.dir = x - prev;
                }
                drag.last_x = Some(x);
                let (at, dir) = (drag.at, drag.dir);

                // Reconsidered on every pointer move rather than only when the
                // pointer crosses into a tab. Crossing publishes exactly one
                // `on_enter`, and the direction is still unknown on the very
                // first one of a drag — deciding there meant a refused move was
                // never retried and the tab simply never followed the pointer.
                if let Some((hover_group, target)) = self.hovered_tab
                    && hover_group == group
                    && target != usize::MAX
                    && target != at
                {
                    let forward = target > at;
                    if (forward && dir > 0.0) || (!forward && dir < 0.0) {
                        self.move_tab(group, at, target);
                        if let Some(d) = &mut self.tab_drag {
                            d.at = target;
                        }
                    }
                }
            }
            Message::TabPressed(group, i) => {
                let task = match group {
                    TabGroup::Section => self.select_section(i),
                    TabGroup::Editor => {
                        self.select_tab(i);
                        Task::none()
                    }
                    TabGroup::Shell(id) => {
                        self.select_shell(id, i);
                        Task::none()
                    }
                };
                // A section press begins no drag — the set is fixed, so there is
                // no order to change. Without this the drag machinery would run
                // over a strip whose `move_tab` has nothing to move.
                if group.reorderable() {
                    self.tab_drag = Some(TabDrag {
                        group,
                        origin: i,
                        at: i,
                        last_x: None,
                        dir: 0.0,
                    });
                }
                return task;
            }
            Message::TabClosed(group, i) => match group {
                // Middle-click on a section does nothing: sections aren't things
                // you opened, so there is nothing to close.
                TabGroup::Section => {}
                TabGroup::Editor => return self.close_tab(i),
                TabGroup::Shell(id) => self.close_shell(id, i),
            },
            Message::LeftReleased => {
                // The list is already in its final order; releasing only ends the
                // gesture. Persisting here rather than per crossed tab keeps a
                // drag across five tabs to one file write, not five.
                if let Some(drag) = self.tab_drag.take()
                    && drag.origin != drag.at
                {
                    self.persist();
                }
                // A window resize and a splitter drag both *end* with the button
                // coming up, so this is where geometry settles — no timer, and
                // nothing written while the pointer is still moving.
                if self.geometry_dirty {
                    self.save_session();
                }
            }
            Message::PromptInput(value) => {
                if let Some(p) = &mut self.prompt {
                    p.input = value;
                    p.note = None;
                    // Find searches as you type, always from the origin.
                    if p.kind == PromptKind::Find {
                        let (query, origin) = (p.input.clone(), p.origin);
                        self.search(&query, origin, true);
                    }
                }
            }
            Message::Remote(ipc::Command::Open {
                path,
                line,
                syntax,
                review,
                reply,
            }) => {
                let result = self.open_path(&path, line, syntax.as_deref(), review);
                // Answer whatever happened. The client is a shell command that
                // exits on this, so a failure has to travel back rather than be
                // left in a window nobody is looking at.
                let _ = reply.send(match &result {
                    Ok(()) => sacrament_core::protocol::Response::Ok,
                    Err(e) => sacrament_core::protocol::Response::Err(e.clone()),
                });
                if let Err(e) = result {
                    self.alert(e);
                }
            }
            Message::JiraRefresh => return self.jira_refresh(),
            Message::JiraRefreshHovered(over) => self.jira.hovered = over,
            Message::JiraLoaded(result) => {
                self.jira.loading = false;
                match result {
                    Ok(page) => self.jira.view = JiraView::Ready(page),
                    Err(e) => {
                        // Both: the alert can't be missed, and the pane keeps the
                        // text after the dialog is dismissed — otherwise the
                        // failure is gone the moment it's acknowledged, and the
                        // section is left showing a stale or empty dashboard.
                        self.jira.view = JiraView::Note(format!("Could not load.\n\n{e}"));
                        self.alert(e);
                    }
                }
            }
            Message::FileChanged(path) => self.reload_changed(&path),
            Message::FileDropped(path) => self.drop_file(path),
            Message::AlertDismissed => self.showing_alerts.clear(),
            Message::SaveAsPicked(None, _) => {}
            Message::SaveAsPicked(Some(path), then) => {
                let hl = self.highlighter.clone();
                let result = self
                    .buf()
                    .lock()
                    .map(|mut b| b.save_as(path, hl.as_deref()))
                    .ok();
                match result {
                    Some(Ok(())) => {
                        self.persist();
                        // Whatever was waiting on the save can happen now.
                        match then {
                            AfterSave::Nothing => {}
                            AfterSave::CloseTab(i) => self.discard_tab(i),
                            AfterSave::Quit => return self.quit_now(),
                        }
                    }
                    Some(Err(e)) => self.alert(e.to_string()),
                    None => self.alert("Buffer lock poisoned — nothing was written."),
                }
            }
            Message::NewBuffer => self.new_buffer(),
            Message::ToggleFold(row) => {
                let rows = self.editor_rows;
                self.focus = Focus::Editor;
                if let Ok(mut b) = self.buf().lock() {
                    b.toggle_fold(row);
                    b.ensure_cursor_visible(rows);
                }
            }
            Message::OpenPicked(paths) => {
                for path in paths {
                    if let Err(e) = self.open_path(&path, None, None, false) {
                        self.alert(e);
                    }
                }
            }
            Message::CloseTabAnswer(i, answer) => match answer {
                Answer::Cancel => {}
                Answer::Discard => self.discard_tab(i),
                Answer::Save => {
                    let path = self
                        .buffers
                        .get(i)
                        .and_then(|b| b.lock().ok())
                        .and_then(|b| b.path().map(|p| p.to_path_buf()));
                    // An untitled buffer has nowhere to save yet, so the save
                    // panel comes first and the close waits on it.
                    if path.is_none() {
                        self.active = i.min(self.buffers.len().saturating_sub(1));
                        return save_as_dialog_for(None, AfterSave::CloseTab(i));
                    }
                    // The guard has to be gone before `alert` can borrow `self`,
                    // hence taking the error out first rather than reporting
                    // inside the `if let` chain.
                    let failed = self
                        .buffers
                        .get(i)
                        .and_then(|buf| buf.lock().ok().and_then(|mut b| b.save().err()));
                    if let Some(e) = failed {
                        // Don't close on a failed save — that's the case where
                        // closing would destroy the very edits being rescued.
                        self.alert(e.to_string());
                        return Task::none();
                    }
                    self.discard_tab(i);
                }
            },
            Message::QuitAnswer(answer) => match answer {
                Answer::Cancel => {}
                Answer::Discard => return self.quit_now(),
                Answer::Save => return self.save_all_then_quit(),
            },
            Message::ConflictAnswer(choice) => match choice {
                Conflict::Cancel => {}
                Conflict::Overwrite => {
                    let result = self.buf().lock().map(|mut b| b.save_overwriting()).ok();
                    match result {
                        Some(Ok(())) => {}
                        Some(Err(e)) => self.alert(e.to_string()),
                        None => self.alert("Buffer lock poisoned — nothing was written."),
                    }
                }
                Conflict::Reload => {
                    let rows = self.editor_rows;
                    let hl = self.highlighter.clone();
                    if let Ok(mut b) = self.buf().lock() {
                        b.discard_and_reload(hl.as_deref());
                        b.ensure_cursor_visible(rows);
                    }
                }
            },
            Message::WindowResized(size) => {
                self.geometry.window_width = size.width;
                self.geometry.window_height = size.height;
                self.geometry_dirty = true;
            }
            Message::CloseRequested => return self.request_quit(),
            Message::PaneDragged(pane_grid::DragEvent::Dropped { pane, target }) => {
                self.panes.drop(pane, target);
            }
            Message::PaneDragged(_) => {}
            Message::Key(key, physical, mods, composed) => {
                // An open prompt owns the keyboard. Its `text_input` has already
                // handled typing, editing, selection and paste by the time this
                // runs, so acting on those again would double-apply them — only
                // the two keys the input doesn't handle are claimed here.
                if self.prompt.is_some() {
                    return self.prompt_key(&key, mods);
                }
                // Pane focus first: it's the one binding that isn't Cmd, and it
                // has to win before the shell sees anything.
                if is_app_ctrl(mods)
                    && let iced::keyboard::Key::Character(c) = &key
                {
                    match c.as_str() {
                        "1" => {
                            self.focus = Focus::Editor;
                            return Task::none();
                        }
                        "2" => {
                            self.focus = Focus::Shell(PaneId::Bottom);
                            return Task::none();
                        }
                        "3" => {
                            self.focus = Focus::Shell(PaneId::Right);
                            return Task::none();
                        }
                        // Sublime's binding. `Cmd+G` can't be goto-line because
                        // macOS spends it on find-next.
                        "g" | "G" => return self.open_prompt(PromptKind::GotoLine),
                        _ => {}
                    }
                }
                if let Some(task) = self.command_key(&key, physical, mods) {
                    return task;
                }
                match self.focus {
                    Focus::Shell(id) => {
                        if let Some(bytes) = keymap(&key, mods, composed.as_deref())
                            && let Some(shell) = self.pane(id).active()
                        {
                            // Typing snaps back to live output and drops the
                            // selection — what every terminal does.
                            if let Ok(mut t) = shell.terminal.lock() {
                                t.scroll_to_bottom();
                                t.clear_selection();
                            }
                            if let Some(h) = &shell.handle {
                                h.write(bytes);
                            }
                        }
                    }
                    // Another section has no caret and nothing to type into, but
                    // it does have a scrollable surface — so navigation keys go
                    // through `read_key`, which claims only those and drops the
                    // rest. Same funnel read mode uses, one level up.
                    Focus::Editor if self.section != Section::Editor => {
                        self.read_key(&key)
                    }
                    // Read mode takes navigation only. Gating here rather than
                    // inside `edit_key` covers every editing path at once —
                    // v1 gated its key handler and left `Event::Paste` free to
                    // edit the source behind the rendering.
                    Focus::Editor if self.reading() => self.read_key(&key),
                    Focus::Editor => self.edit_key(&key, mods, composed.as_deref()),
                }
            }
        }
        Task::none()
    }

    /// The application shortcut table: every binding is `Cmd` (`Ctrl` off macOS),
    /// which is what keeps `Ctrl` free for the shell.
    ///
    /// Returns `None` when nothing matched, so the caller can route the key on to
    /// the editor or the PTY. Returning `Some` — even `Some(Task::none())` — means
    /// the key was consumed.
    ///
    /// **Tab and window commands act on the focused pane**, not on the buffer
    /// list: `Cmd+T` in a shell pane opens a shell, `Cmd+W` closes whatever tab
    /// you're looking at. One binding per concept rather than v1's split of
    /// `Ctrl+W` for buffers and `Ctrl+Shift+W` for shells.
    fn command_key(
        &mut self,
        key: &iced::keyboard::Key,
        physical: iced::keyboard::key::Physical,
        mods: iced::keyboard::Modifiers,
    ) -> Option<Task<Message>> {
        use iced::keyboard::Key;
        use iced::keyboard::key::{Code, Named, Physical};

        if !mods.command() {
            return None;
        }
        let shift = mods.shift();

        // Folding is `Cmd+Option+[` / `]`, Sublime's binding, matched on the
        // *physical* key. With Option held, macOS composes those keys into `“`
        // and `‘`, so matching the character would bind the US layout only —
        // which is the class of bug v1's `apply_shift` table existed to paper
        // over. The physical key sidesteps it entirely.
        if mods.alt() {
            let bracket = match physical {
                Physical::Code(Code::BracketLeft) => Some(false),
                Physical::Code(Code::BracketRight) => Some(true),
                _ => None,
            };
            if let Some(open) = bracket {
                self.fold_command(open, shift);
                return Some(Task::none());
            }
        }

        // Cursor movement, macOS style. Editor only — in a shell these are the
        // app's, not the PTY's, and are simply swallowed.
        if let Key::Named(named) = key
            && matches!(
                named,
                Named::ArrowLeft | Named::ArrowRight | Named::ArrowUp | Named::ArrowDown
            )
        {
            if self.editing() {
                let rows = self.editor_rows;
                if let Ok(mut b) = self.buf().lock() {
                    match named {
                        Named::ArrowLeft => b.move_home(shift),
                        Named::ArrowRight => b.move_end(shift),
                        Named::ArrowUp => b.move_doc_start(shift),
                        _ => b.move_doc_end(shift),
                    }
                    b.ensure_cursor_visible(rows);
                }
            }
            return Some(Task::none());
        }
        // Ctrl+Tab cycles tabs everywhere it exists, so it's kept alongside the
        // macOS-native Cmd+Shift+[ and Cmd+Shift+].
        if let Key::Named(Named::Tab) = key {
            self.cycle_focused_tab(!shift);
            return Some(Task::none());
        }

        let Key::Character(c) = key else {
            return None;
        };
        // Shifted punctuation arrives composed — `Cmd+Shift+[` is `{` on a US
        // layout — so both forms are matched rather than reading the raw key.
        match c.as_str() {
            "[" | "{" if shift => self.cycle_focused_tab(false),
            "]" | "}" if shift => self.cycle_focused_tab(true),
            // Indent / outdent, the Sublime and VS Code binding on macOS.
            "]" => self.reindent(true),
            "[" => self.reindent(false),
            // Toggle comment. `/` needs no shift, so there's no second spelling.
            "/" => self.toggle_comment(),
            // `Cmd+Shift+M`, because plain `Cmd+M` is Minimize on macOS.
            "m" | "M" if shift => self.toggle_read_mode(),
            // Save and save-as act on what's on screen, so they're inert while
            // another section shows — the buffer stays dirty and saveable, and the
            // quit prompt still catches it. `Cmd+O` is the opposite case: its
            // whole purpose is to put a file in front of you, so it switches back
            // (in `open_path`).
            "s" | "S" if shift => {
                if self.editing() {
                    return Some(self.save_as_dialog());
                }
            }
            "o" | "O" => return Some(open_dialog()),
            "s" | "S" => {
                if self.editing() {
                    return Some(self.save());
                }
            }
            "f" | "F" => return Some(self.open_prompt(PromptKind::Find)),
            // macOS find-next. Repeats the last query with no prompt in the way.
            "g" | "G" => return Some(self.find_next(shift)),
            "n" | "N" => self.new_buffer(),
            "t" | "T" => match self.focus {
                Focus::Editor => self.new_buffer(),
                Focus::Shell(id) => self.spawn_shell(id),
            },
            "w" | "W" => return Some(self.close_focused_tab()),
            "q" | "Q" => return Some(self.request_quit()),
            "z" | "Z" => {
                if self.editing() {
                    self.history(shift);
                }
            }
            "c" | "C" => {
                return Some(match self.focus {
                    Focus::Editor if !self.editing() => Task::none(),
                    Focus::Editor => self.copy(false),
                    Focus::Shell(_) => self.copy_shell(),
                });
            }
            "x" | "X" => {
                if self.editing() {
                    return Some(self.copy(true));
                }
            }
            "v" | "V" => return Some(iced::clipboard::read().map(Message::Pasted)),
            "a" | "A" => {
                if self.editing()
                    && let Ok(mut b) = self.buf().lock()
                {
                    b.select_all();
                }
            }
            // Diagnostics only, and visible only under SACRAMENT_METRICS.
            "r" | "R" if shift => self.metrics.reset(),
            // Refresh the section that has something to refresh. The editor's
            // content comes from disk and is kept current by the watcher, so
            // there's nothing for `Cmd+R` to do there.
            "r" | "R" => {
                if self.section == Section::Jira {
                    return Some(self.jira_refresh());
                }
            }
            // Jump to a tab in the focused pane.
            d if d.len() == 1 && matches!(d.as_bytes()[0], b'1'..=b'9') => {
                let n = (d.as_bytes()[0] - b'1') as usize;
                match self.focus {
                    // The file tabs aren't on screen, so there's no tab N to jump
                    // to. Sections have their own strip and no binding yet.
                    Focus::Editor if !self.editing() => {}
                    Focus::Editor => self.select_tab(n),
                    Focus::Shell(id) => self.select_shell(id, n),
                }
            }
            // Any other Cmd combination is still consumed: letting it through
            // would type a bare character into the shell or the buffer.
            _ => {}
        }
        Some(Task::none())
    }

    /// Persist now, because the tab set or its order just changed.
    ///
    /// The session used to be written *only* on close, which turned out to mean
    /// "only when quit via the window button": on macOS, `Cmd+Q` is handled by
    /// AppKit, which terminates the process without the key ever reaching the
    /// application — no `CloseRequested`, no save. Writing on the changes
    /// themselves makes persistence independent of how the app goes away, which
    /// also covers a crash or a `SIGTERM`.
    ///
    /// Deliberately *not* used for geometry: a splitter drag emits an event per
    /// frame. That flushes on mouse-up instead.
    fn persist(&mut self) {
        self.save_session();
        // The watch set changes on the same events the session does — a tab
        // opening, closing, or being renamed by save-as — so it's kept in step
        // here rather than from each of those call sites.
        self.sync_watches();
    }

    /// Move one tab within its strip.
    ///
    /// The dragged tab stays active, which is what makes a drag feel like moving
    /// *this* tab rather than shuffling the strip underneath it — pressing it
    /// already made it active, so `active` simply follows it.
    ///
    /// No `persist` here: this runs once per tab crossed during a drag, and the
    /// release writes the result.
    fn move_tab(&mut self, group: TabGroup, from: usize, to: usize) {
        match group {
            // Unreachable in practice — `TabPressed` starts no drag for a strip
            // that isn't `reorderable`, and a drag is the only route here. The arm
            // states it rather than leaving a `_` that would silently absorb a
            // future group that *should* reorder.
            TabGroup::Section => {}
            TabGroup::Editor => {
                if move_item(&mut self.buffers, from, to) {
                    self.active = to;
                }
            }
            TabGroup::Shell(id) => {
                let pane = self.pane_mut(id);
                if move_item(&mut pane.shells, from, to) {
                    pane.active = to;
                }
            }
        }
    }

    /// Re-read every buffer showing `path`.
    ///
    /// Reached from the watcher, so it fires for our *own* saves too — the mtime
    /// check inside `Buffer::reload` is what makes those a no-op, and it's also
    /// why no debouncing is needed: once reloaded, the buffer's mtime matches
    /// disk and the duplicate events notify emits for a single write do nothing.
    fn reload_changed(&mut self, path: &std::path::Path) {
        let rows = self.editor_rows;
        let hl = self.highlighter.clone();
        let mut conflict = None;
        for buf in &self.buffers {
            let Ok(mut b) = buf.lock() else { continue };
            if b.path() != Some(path) {
                continue;
            }
            match b.reload(hl.as_deref()) {
                Ok(true) => b.ensure_cursor_visible(rows),
                Ok(false) => {}
                // Unsaved edits *and* a changed file. Nothing is discarded either
                // way, so this only has to be said out loud — and it has to name
                // the escape hatch, because plain Cmd+S will refuse too.
                Err(buffer::ReloadError::Dirty) => {
                    conflict = Some(format!(
                        "{} changed on disk. Your unsaved edits are kept — press Cmd+S to \
                         choose between them.",
                        b.display_name()
                    ));
                }
                Err(buffer::ReloadError::Io(e)) => {
                    conflict = Some(format!("{}: {e}", path.display()));
                }
            }
        }
        // One write produces several watch events, and a refused reload reports
        // on every one of them — `alert` collapses the repeats.
        if let Some(msg) = conflict {
            self.alert(msg);
        }
    }

    /// A file dropped on the window from the Finder.
    ///
    /// Routed by **focus**, not by where the pointer landed: the drop event
    /// carries no position — winit exposes none, so neither does iced — and
    /// nothing else is available to decide with, since the OS owns the pointer
    /// for the length of a drag and no `CursorMoved` arrives during one. v1
    /// behaved the same way for a different reason: the emulator handed the path
    /// over as pasted text, which went wherever focus was.
    ///
    /// A shell gets the escaped path, which is what every terminal puts there —
    /// and what Claude Code reads a dropped image from. The editor opens it as a
    /// tab, the same as `Cmd+O`: a text editor's answer to a dropped file is to
    /// show it, and a file it can't read reports that rather than opening blank.
    fn drop_file(&mut self, path: std::path::PathBuf) {
        match self.focus {
            Focus::Shell(id) => {
                // Trailing space, so a second drop or typing afterwards doesn't
                // run into the path.
                let text = format!("{} ", shell_escaped(&path));
                self.paste_to_shell(id, &text);
            }
            Focus::Editor => {
                if let Err(e) = self.open_path(&path, None, None, false) {
                    self.alert(e);
                }
            }
        }
    }

    /// Tell the watcher which files are open.
    ///
    /// Declared as a whole set rather than added and removed one at a time, so
    /// a path can't be left watched after its tab closes.
    fn sync_watches(&self) {
        let paths: Vec<std::path::PathBuf> = self
            .buffers
            .iter()
            .filter_map(|b| b.lock().ok().and_then(|b| b.path().map(|p| p.to_path_buf())))
            .collect();
        watch::sync(paths);
    }

    /// Open a file as a tab, or focus it if it's already open.
    ///
    /// `review` marks a tool's open (the Claude Code hook): the tab appears but
    /// does *not* become active, because something writing files in the
    /// background must not yank the cursor out of what you're typing.
    fn open_path(
        &mut self,
        path: &std::path::Path,
        line: Option<usize>,
        syntax: Option<&str>,
        review: bool,
    ) -> Result<(), String> {
        let rows = self.editor_rows;
        // Already open? Reuse the tab rather than stacking duplicates — an agent
        // touching the same file repeatedly would otherwise fill the strip.
        let existing = self.buffers.iter().position(|b| {
            b.lock()
                .map(|b| b.path() == Some(path))
                .unwrap_or(false)
        });
        let index = match existing {
            Some(i) => {
                if let Some(n) = line
                    && let Ok(mut b) = self.buffers[i].lock()
                {
                    b.goto_line(n);
                    b.ensure_cursor_visible(rows);
                }
                i
            }
            None => {
                let mut buf = Buffer::load(path, self.highlighter.as_deref())
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                buf.tab_width = self.config.tab_width.max(1);
                if let Some(name) = syntax
                    && let Some(hl) = self.highlighter.as_deref()
                {
                    buf.set_syntax_override(name, hl);
                }
                if let Some(n) = line {
                    buf.goto_line(n);
                    buf.ensure_cursor_visible(rows);
                } else {
                    // Markdown opens **rendered**, because that's what a markdown
                    // file is for; `Cmd+Shift+M` gets to the source. `set_read_mode`
                    // gates on the extension itself, so this needs no check of its
                    // own — a non-markdown file is left alone.
                    //
                    // Only when no line was asked for. `sacrament NOTES.md:42` is a
                    // request for a specific *source* line, and read mode has no
                    // such thing — its rows are rendered ones, which don't
                    // correspond. Honoring the line means showing the source.
                    buf.set_read_mode();
                }
                // An untouched, untitled, unmodified buffer is the placeholder
                // from startup — replace it rather than leaving an empty tab
                // beside the file that was just asked for.
                let placeholder = self.buffers.len() == 1
                    && self.buffers[0]
                        .lock()
                        .map(|b| b.path().is_none() && !b.dirty)
                        .unwrap_or(false);
                if placeholder {
                    self.buffers.clear();
                }
                self.buffers.push(Arc::new(Mutex::new(buf)));
                self.buffers.len() - 1
            }
        };
        if review {
            // Mark it so the tab says an agent touched it — unless it's the tab
            // already on screen, where the mark could never be cleared without
            // navigating away and back.
            if index != self.active
                && let Ok(mut b) = self.buffers[index].lock()
            {
                b.set_unreviewed(true);
            }
        } else {
            self.active = index;
            // A user-initiated open shows the editor, since a tab nobody can see
            // isn't an answer to "open this". A *review* open deliberately does
            // not: it's a tool writing in the background, and it doesn't take the
            // active tab either.
            self.show_editor();
            self.mark_active_reviewed();
        }
        self.persist();
        Ok(())
    }

    /// A new empty buffer, made active. `Cmd+N`.
    ///
    /// Shows the editor: an empty buffer exists to be typed into, so making one
    /// while another section is up has to bring the editor back.
    fn new_buffer(&mut self) {
        self.buffers.push(Arc::new(Mutex::new(Buffer::empty())));
        self.active = self.buffers.len() - 1;
        self.show_editor();
        self.persist();
    }

    /// Close the active tab of whichever pane has focus. `Cmd+W`.
    fn close_focused_tab(&mut self) -> Task<Message> {
        match self.focus {
            // Nothing to close: the file tabs aren't on screen, and a section
            // isn't something you opened.
            Focus::Editor if !self.editing() => Task::none(),
            Focus::Editor => self.close_tab(self.active),
            Focus::Shell(id) => {
                let i = self.pane(id).active;
                self.close_shell(id, i);
                Task::none()
            }
        }
    }

    /// Cycle tabs within the focused pane.
    fn cycle_focused_tab(&mut self, forward: bool) {
        match self.focus {
            // No file tabs on screen to cycle through.
            Focus::Editor if !self.editing() => {}
            Focus::Editor => self.cycle_tab(forward),
            Focus::Shell(id) => {
                let pane = self.pane(id);
                let n = pane.shells.len();
                if n == 0 {
                    return;
                }
                let next = if forward {
                    (pane.active + 1) % n
                } else {
                    (pane.active + n - 1) % n
                };
                self.select_shell(id, next);
            }
        }
    }

    /// Open the bottom prompt.
    ///
    /// The editor is shown first: both prompts act on the buffer, so running one
    /// while a shell has focus — or while another section is up — would otherwise
    /// leave the result invisible.
    fn open_prompt(&mut self, kind: PromptKind) -> Task<Message> {
        self.show_editor();
        let (origin, selection) = match self.buf().lock() {
            Ok(b) => ((b.cursor_row, b.cursor_col), b.selected_text()),
            Err(_) => ((0, 0), None),
        };
        let input = match kind {
            // Prefill from the selection, the way every find bar does. Multi-line
            // selections are skipped since `find` only matches within a line.
            PromptKind::Find => selection.filter(|s| !s.contains('\n')).unwrap_or_default(),
            PromptKind::GotoLine => String::new(),
        };
        self.prompt = Some(Prompt {
            kind,
            input,
            origin,
            note: None,
        });
        // Focus, then select: opening save-as over an existing path should let a
        // single keystroke replace it rather than append to it.
        iced::widget::operation::focus(PROMPT_ID.clone())
            .chain(iced::widget::operation::select_all(PROMPT_ID.clone()))
    }

    /// The only keys an open prompt acts on.
    ///
    /// Deliberately a very short list. Everything else — characters, arrows,
    /// `Backspace`, `Cmd+V` — is the `text_input`'s, and this subscription sees
    /// those events *after* it has handled them. Adding an arm here for anything
    /// the input already does would apply it twice.
    ///
    /// `Cmd+Q` is the exception worth keeping: an open prompt shouldn't be able
    /// to trap the application.
    fn prompt_key(
        &mut self,
        key: &iced::keyboard::Key,
        mods: iced::keyboard::Modifiers,
    ) -> Task<Message> {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        match key {
            Key::Named(Named::Escape) => self.prompt = None,
            // Shift+Enter searches backwards. The `text_input` is given no
            // `on_submit` precisely so both cases can be told apart here.
            Key::Named(Named::Enter) => return self.prompt_confirm(mods.shift()),
            // Advancing with Cmd+G while the prompt is open should work too,
            // rather than being swallowed as "not one of my keys".
            Key::Character(c) if mods.command() && c.eq_ignore_ascii_case("g") => {
                return self.prompt_confirm(mods.shift());
            }
            Key::Character(c) if mods.command() && c.eq_ignore_ascii_case("q") => {
                return self.request_quit();
            }
            _ => {}
        }
        Task::none()
    }

    /// Act on the prompt's contents.
    fn prompt_confirm(&mut self, reverse: bool) -> Task<Message> {
        let Some(prompt) = self.prompt.clone() else {
            return Task::none();
        };
        match prompt.kind {
            PromptKind::Find => {
                // Search on from the current match rather than the origin, or
                // Enter would return the same hit forever. Forward continues from
                // the match's end, backward from its start.
                let from = match self.buf().lock() {
                    Ok(b) => {
                        let (start, end) = b
                            .selection_range()
                            .unwrap_or(((b.cursor_row, b.cursor_col), (b.cursor_row, b.cursor_col)));
                        if reverse { start } else { end }
                    }
                    Err(_) => prompt.origin,
                };
                self.search(&prompt.input, from, !reverse);
            }
            PromptKind::GotoLine => match prompt.input.trim().parse::<usize>() {
                Ok(line) => {
                    let rows = self.editor_rows;
                    if let Ok(mut b) = self.buf().lock() {
                        b.goto_line(line);
                        b.ensure_cursor_visible(rows);
                    }
                    self.prompt = None;
                }
                Err(_) => {
                    if let Some(p) = &mut self.prompt {
                        p.note = Some("not a line number".to_string());
                    }
                }
            },
        }
        Task::none()
    }

    /// Repeat the last search without opening anything. `Cmd+G`.
    ///
    /// Falls back to opening the find prompt when there's no query yet — the
    /// alternative is a key that does nothing the first time you press it.
    fn find_next(&mut self, reverse: bool) -> Task<Message> {
        let Some(query) = self.last_query.clone() else {
            return self.open_prompt(PromptKind::Find);
        };
        // A match is shown by selecting it, so the editor has to be the thing on
        // screen — otherwise `Cmd+G` silently moves a caret nobody can see.
        self.show_editor();
        // Continue from the current match, not the caret: forward from its end,
        // backward from its start, or the same hit comes back every time.
        let from = match self.buf().lock() {
            Ok(b) => {
                let here = (b.cursor_row, b.cursor_col);
                let (start, end) = b.selection_range().unwrap_or((here, here));
                if reverse { start } else { end }
            }
            Err(_) => (0, 0),
        };
        self.search(&query, from, !reverse);
        Task::none()
    }

    /// Run a search and select the hit, or report that there wasn't one.
    fn search(&mut self, query: &str, from: buffer::Pos, forward: bool) {
        if query.is_empty() {
            if let Some(p) = &mut self.prompt {
                p.note = None;
            }
            return;
        }
        // Remembered here rather than at the call sites, so every route into a
        // search — typing, Enter, Cmd+G — keeps `Cmd+G` working afterwards.
        self.last_query = Some(query.to_string());
        let rows = self.editor_rows;
        let found = match self.buf().lock() {
            Ok(mut b) => match b.find(query, from, forward) {
                Some((start, end)) => {
                    b.select_range(start, end);
                    b.ensure_cursor_visible(rows);
                    true
                }
                None => false,
            },
            Err(_) => false,
        };
        match &mut self.prompt {
            // With the prompt open the note belongs next to the query: it's the
            // one message that arrives *while typing*, so it can't be a dialog —
            // one would have to be dismissed between keystrokes.
            Some(p) => p.note = (!found).then(|| "no match".to_string()),
            // `Cmd+G` with no prompt open has nothing on screen to say what was
            // searched for, so the alert names the query. A silent no-op there
            // reads as a broken key.
            None if !found => self.alert(format!("No match for “{query}”.")),
            None => {}
        }
    }

    /// Undo, or redo when `forward`. Scrolls to follow the restored cursor,
    /// since an undo can land far from where you're looking.
    fn history(&mut self, forward: bool) {
        let rows = self.editor_rows;
        if let Ok(mut b) = self.buf().lock() {
            if forward { b.redo() } else { b.undo() };
            b.ensure_cursor_visible(rows);
        }
    }

    /// Route a grid gesture. Press/drag drive selection; scroll moves the
    /// viewport without touching the cursor.
    fn mouse(&mut self, focus: Focus, gesture: GridMouse) -> Task<Message> {
        self.focus = focus;
        // Starting a selection anywhere ends every other one. Two highlighted
        // regions on screen claim to be "the selection" at once, and `Cmd+C` can
        // only take one of them — which one being decided by focus, invisibly.
        if matches!(gesture, GridMouse::Press { .. }) {
            self.clear_selections_except(focus);
        }
        match (focus, gesture) {
            // Read mode has nothing to select or put a caret in — but it does
            // scroll, so this must not swallow the wheel.
            //
            // `!self.editing()` covers the Jira section, whose grid is a read-mode
            // surface too: without it a click there would fall through and move the
            // caret in a file that isn't on screen.
            (
                Focus::Editor,
                GridMouse::Press { .. } | GridMouse::Drag { .. } | GridMouse::Release,
            ) if !self.editing() || self.reading() => {}
            // `shift` is unread here: the editor has no link handling yet, and
            // shift-extending a selection isn't implemented either.
            (Focus::Editor, GridMouse::Press {
                row,
                col,
                count,
                shift: _,
            }) => {
                let rows = self.editor_rows;
                if let Ok(mut b) = self.buf().lock() {
                    let pos = b.screen_to_doc(row, col, rows);
                    if count >= 2 {
                        // Double click selects a word; falling back to a plain
                        // caret placement when there's no word under the pointer.
                        if !b.select_word_at(pos) {
                            b.clear_selection();
                            b.cursor_row = pos.0;
                            b.cursor_col = pos.1;
                            b.clamp_to_content();
                        }
                    } else {
                        b.clear_selection();
                        b.cursor_row = pos.0;
                        b.cursor_col = pos.1;
                        b.clamp_to_content();
                        // Anchor here so the drag that may follow has an origin.
                        b.selection_anchor = Some((b.cursor_row, b.cursor_col));
                    }
                }
            }
            (Focus::Editor, GridMouse::Drag { row, col }) => {
                let rows = self.editor_rows;
                if let Ok(mut b) = self.buf().lock() {
                    let (r, c) = b.screen_to_doc(row, col, rows);
                    b.cursor_row = r;
                    b.cursor_col = c;
                    b.clamp_to_content();
                }
            }
            (Focus::Editor, GridMouse::Release) => {
                // A click with no movement leaves a collapsed selection; drop the
                // anchor so it isn't reported as a selection.
                if let Ok(mut b) = self.buf().lock()
                    && !b.has_selection()
                {
                    b.clear_selection();
                }
            }
            (Focus::Editor, GridMouse::Scroll { dy, cols: dx }) => {
                let (rows, cols) = (self.editor_rows, self.editor_cols);
                let ch = self.font.cell_height().max(1.0);
                // Negated: a positive delta means "toward the start of the
                // content", which is a smaller index.
                let advance = -dy;
                let dx = -dx as isize;

                // Accumulate pixels, spend whole rows, keep the remainder for the
                // renderer. This is what makes the view move by pixels instead of
                // jumping a row at a time.
                let target = self.editor_scroll_px + advance;
                let whole = (target / ch).floor();
                let remainder = target - whole * ch;

                let mut clamped = false;
                if let Ok(mut b) = self.read_target().lock() {
                    if b.view_mode() == buffer::ViewMode::Read {
                        let before = b.read_scroll();
                        if whole != 0.0 {
                            b.scroll_read(whole as isize, rows, cols);
                            clamped = b.read_scroll() == before;
                        }
                        b.scroll_read_cols(dx, cols);
                    } else {
                        let before = (b.scroll_row, b.scroll_seg);
                        if whole != 0.0 {
                            b.scroll_by(whole as isize, rows);
                            clamped = (b.scroll_row, b.scroll_seg) == before;
                        }
                        b.scroll_cols(dx, cols);
                    }
                }
                // A clamped row move means an end of the content: sit exactly on
                // the boundary rather than leaving a partial row of background
                // showing that nothing can scroll away.
                self.editor_scroll_px = if clamped { 0.0 } else { remainder };
            }
            // A terminal reflows to its width, so it has nothing off to the
            // side; the horizontal component is dropped rather than ignored
            // silently in the widget, which would cost the editor its own.
            (Focus::Shell(id), GridMouse::Scroll { dy, .. }) => {
                let ch = self.font.cell_height().max(1.0);
                let active = self.pane(id).active;
                let Some(shell) = self.pane_mut(id).shells.get_mut(active) else {
                    return Task::none();
                };
                let target = shell.scroll_px + -dy;
                let whole = (target / ch).floor();
                let remainder = target - whole * ch;
                let terminal = shell.terminal.clone();

                let mut offset = 0;
                if let Ok(mut t) = terminal.lock() {
                    if whole != 0.0 {
                        // `Terminal::scroll` takes lines toward *older* content, so
                        // the sign flips back here.
                        t.scroll(-(whole as i32));
                    }
                    offset = t.display_offset();
                }
                // A partial row needs one grid line below the viewport, and that
                // only exists while scrolled into scrollback. At the live screen
                // there is nothing below to reveal — and live output should not be
                // drawn half a row out of line — so it snaps to the boundary.
                shell.scroll_px = if offset > 0 { remainder } else { 0.0 };
            }
            (Focus::Shell(id), GridMouse::Press {
                row,
                col,
                count,
                shift,
            }) => {
                if let Some(t) = self.pane(id).active().map(|s| s.terminal.clone())
                    && let Ok(mut t) = t.lock()
                {
                    // Shift+click opens a URL, the terminal convention. It falls
                    // through to a normal selection when there isn't one under the
                    // pointer, so the gesture is never simply dead.
                    if shift
                        && let Some(url) = t.url_at(row, col)
                    {
                        open_url(&url);
                        return Task::none();
                    }
                    match count {
                        1 => t.begin_selection(row, col, false),
                        2 => t.begin_selection(row, col, true),
                        _ => t.begin_line_selection(row, col),
                    }
                }
            }
            (Focus::Shell(id), GridMouse::Drag { row, col }) => {
                if let Some(t) = self.pane(id).active().map(|s| s.terminal.clone())
                    && let Ok(mut t) = t.lock()
                {
                    t.update_selection(row, col);
                }
            }
            (Focus::Shell(id), GridMouse::Release) => {
                // A click with no drag leaves an empty selection; drop it so the
                // highlight doesn't linger on a single cell.
                if let Some(t) = self.pane(id).active().map(|s| s.terminal.clone())
                    && let Ok(mut t) = t.lock()
                    && t.selected_text().is_none()
                {
                    t.clear_selection();
                }
            }
        }
        Task::none()
    }

    /// Drop every selection except the one in `keep`.
    ///
    /// Shell tabs that aren't on screen are cleared too: a selection left in a
    /// background tab would reappear the moment you switched to it, which is the
    /// same surprise arriving later.
    fn clear_selections_except(&mut self, keep: Focus) {
        if keep != Focus::Editor
            && let Ok(mut b) = self.buf().lock()
        {
            b.clear_selection();
        }
        for id in PaneId::ALL {
            let active = self.pane(id).active;
            for (i, shell) in self.pane(id).shells.iter().enumerate() {
                if keep == Focus::Shell(id) && i == active {
                    continue;
                }
                if let Ok(mut t) = shell.terminal.lock() {
                    t.clear_selection();
                }
            }
        }
    }

    /// Copy the focused shell's selection.
    fn copy_shell(&mut self) -> Task<Message> {
        let Focus::Shell(id) = self.focus else {
            return Task::none();
        };
        let text = self
            .pane(id)
            .active()
            .and_then(|s| s.terminal.lock().ok().and_then(|t| t.selected_text()));
        match text {
            Some(t) => iced::clipboard::write(t),
            None => Task::none(),
        }
    }

    fn copy(&mut self, cut: bool) -> Task<Message> {
        let text = {
            let Ok(mut b) = self.buf().lock() else {
                return Task::none();
            };
            let text = b.selected_text();
            if cut && text.is_some() {
                b.delete_selection();
                b.ensure_cursor_visible(self.editor_rows);
            }
            text
        };
        match text {
            Some(t) => iced::clipboard::write(t),
            None => Task::none(),
        }
    }

    fn paste_to_shell(&mut self, id: PaneId, text: &str) {
        // Bracketed paste, so a shell that supports it treats the whole thing as
        // literal input rather than interpreting newlines as submits.
        if let Some(h) = self.pane(id).active().and_then(|s| s.handle.as_ref()) {
            let mut bytes = Vec::with_capacity(text.len() + 12);
            bytes.extend_from_slice(b"\x1b[200~");
            bytes.extend_from_slice(text.as_bytes());
            bytes.extend_from_slice(b"\x1b[201~");
            h.write(bytes);
        }
    }

    /// Write the session: open files, shell directories, and geometry.
    ///
    /// Untitled buffers are dropped — there's no path to reopen them from — and the
    /// active index is remapped past the ones removed, or reopening would land on
    /// the wrong tab. v1 does the same remapping for the same reason.
    fn save_session(&mut self) {
        use sacrament_core::session::{Session, SessionBuffer, ShellTabSession};

        let mut buffers = Vec::new();
        let mut active = 0;
        for (i, buf) in self.buffers.iter().enumerate() {
            let Ok(b) = buf.lock() else { continue };
            let Some(path) = b.path() else { continue };
            if i == self.active {
                active = buffers.len();
            }
            buffers.push(SessionBuffer {
                path: path.to_path_buf(),
                cursor_row: b.cursor_row,
                cursor_col: b.cursor_col,
                scroll_row: b.scroll_row,
                scroll_col: b.scroll_col,
                // Folding isn't ported yet; an empty list restores cleanly.
                folds: b.fold_ranges(),
                syntax_override: b.syntax_override().map(str::to_string),
                read_mode: b.view_mode() == buffer::ViewMode::Read,
            });
        }

        // A shell's *directory* is what persists — a PTY isn't serializable, so
        // restore re-spawns a shell there rather than reviving one.
        // Every shell produces an entry, so the tab *count* survives even when a
        // cwd can't be read. Filtering on the pid instead silently dropped tabs:
        // a shell spawned moments ago has no pid yet (it arrives with
        // `Event::Started`, after the deferred spawn), so a session written in
        // that window lost the tab entirely rather than just its directory.
        let shells = |id: PaneId| -> Vec<ShellTabSession> {
            self.pane(id)
                .shells
                .iter()
                .map(|sh| ShellTabSession {
                    cwd: sh
                        .pid
                        .and_then(sacrament_core::proc::cwd_of)
                        .or_else(|| sh.start_cwd.clone())
                        .or_else(|| std::env::current_dir().ok())
                        .unwrap_or_else(|| std::path::PathBuf::from("/")),
                })
                .collect()
        };

        let session = Session {
            active,
            buffers,
            bottom_shells: shells(PaneId::Bottom),
            bottom_active: self.bottom.active,
            right_shells: shells(PaneId::Right),
            right_active: self.right.active,
            geometry: self.geometry.clone(),
        };
        // Reported rather than discarded. A silently-swallowed error here is
        // exactly how "the session is never written" stayed invisible: the write
        // was failing to even be attempted, and nothing said so.
        if let Err(e) = sacrament_core::session::save(sacrament_core::APP_GUI, &session) {
            self.alert(format!("The session couldn't be saved: {e}"));
        }
        self.geometry_dirty = false;
    }

    /// Save the active buffer.
    ///
    /// A file that changed underneath us asks what to do rather than refusing:
    /// there are two reasonable answers and the editor can't pick. Replaced the
    /// "press Cmd+S twice" idiom, which had to be discovered from a message and
    /// offered no way to take the other side.
    fn save(&mut self) -> Task<Message> {
        let untitled = self
            .buf()
            .lock()
            .map(|b| b.path().is_none())
            .unwrap_or(false);
        if untitled {
            // Nowhere to write yet, so Cmd+S *is* save-as.
            return save_as_dialog_for(None, AfterSave::Nothing);
        }
        // `.ok()` before matching: a `PoisonError` holds the guard, which keeps
        // `self` borrowed and blocks queueing an alert.
        let result = self.buf().lock().map(|mut b| b.save()).ok();
        match result {
            Some(Ok(())) => {}
            Some(Err(buffer::SaveError::ChangedOnDisk)) => {
                let name = self
                    .buf()
                    .lock()
                    .map(|b| b.display_name())
                    .unwrap_or_else(|_| "This file".to_string());
                let dialog = rfd::AsyncMessageDialog::new()
                    .set_level(rfd::MessageLevel::Warning)
                    .set_title("File changed on disk")
                    .set_description(format!(
                        "{name} changed on disk since it was opened. \
                         Overwrite it with this version, or reload and lose these edits?"
                    ))
                    .set_buttons(rfd::MessageButtons::YesNoCancelCustom(
                        "Overwrite".into(),
                        "Reload".into(),
                        "Cancel".into(),
                    ));
                return Task::perform(
                    async move {
                        match dialog.show().await {
                            rfd::MessageDialogResult::Custom(l) if l == "Overwrite" => {
                                Conflict::Overwrite
                            }
                            rfd::MessageDialogResult::Custom(l) if l == "Reload" => {
                                Conflict::Reload
                            }
                            _ => Conflict::Cancel,
                        }
                    },
                    Message::ConflictAnswer,
                );
            }
            Some(Err(e)) => self.alert(e.to_string()),
            None => self.alert("Buffer lock poisoned — nothing was written."),
        }
        Task::none()
    }

    /// Is the active buffer showing rendered markdown?
    fn reading(&self) -> bool {
        self.buf()
            .lock()
            .map(|b| b.view_mode() == buffer::ViewMode::Read)
            .unwrap_or(false)
    }

    /// Which buffer read-mode navigation and scrolling act on.
    ///
    /// The editor pane hosts more than one read-mode surface now — a markdown
    /// file and the Jira dashboard — and they scroll independently. Deciding it
    /// here means the key handler and the wheel handler cannot disagree about
    /// which one moved, which is the bug they would otherwise take turns having.
    fn read_target(&self) -> Arc<Mutex<Buffer>> {
        self.buf().clone()
    }

    /// Navigation only — read mode has nothing to type into.
    fn read_key(&mut self, key: &iced::keyboard::Key) {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;
        let rows = self.editor_rows;
        let cols = self.editor_cols;
        let delta = match key {
            Key::Named(Named::ArrowDown) => 1,
            Key::Named(Named::ArrowUp) => -1,
            Key::Named(Named::PageDown) => rows as isize,
            Key::Named(Named::PageUp) => -(rows as isize),
            Key::Named(Named::Home) => isize::MIN / 2,
            Key::Named(Named::End) => isize::MAX / 2,
            _ => return,
        };
        if let Ok(mut b) = self.read_target().lock() {
            b.scroll_read(delta, rows, cols);
        }
    }

    /// Editor keystrokes. Deliberately small: no undo, no selection, no
    /// clipboard yet — each is a self-contained port from v1 and none of them
    /// changes the primitives underneath.
    fn edit_key(
        &mut self,
        key: &iced::keyboard::Key,
        mods: iced::keyboard::Modifiers,
        composed: Option<&str>,
    ) {
        use iced::keyboard::Key;
        use iced::keyboard::key::Named;

        let extend = mods.shift();
        let Ok(mut b) = self.buf().lock() else {
            return;
        };
        match key {
            // Esc drops the selection. The prompt claims Esc before this runs,
            // and a focused shell gets it as a real escape byte, so this only
            // fires for a plain editor keypress.
            Key::Named(Named::Escape) => b.clear_selection(),
            Key::Named(Named::Enter) => b.insert_newline(),
            Key::Named(Named::Backspace) => b.backspace(),
            Key::Named(Named::Delete) => b.delete_forward(),
            // Honour both indent options: a literal tab when `indent_with_tabs`,
            // otherwise `tab_width` spaces. Previously hardcoded to four spaces,
            // which ignored a config the user had set.
            Key::Named(Named::Tab) => {
                if self.config.indent_with_tabs {
                    b.insert_char('\t');
                } else {
                    b.insert_str(&" ".repeat(self.config.tab_width.max(1)));
                }
            }
            Key::Named(Named::Space) => b.insert_char(' '),
            // Shift with any movement extends the selection; without it, moving
            // drops the selection. One flag, threaded through every mover.
            // Option+arrow is word movement on macOS. Checked before the plain
            // arrows or the modifier would be ignored.
            Key::Named(Named::ArrowLeft) if mods.alt() => b.move_word_left(extend),
            Key::Named(Named::ArrowRight) if mods.alt() => b.move_word_right(extend),
            Key::Named(Named::ArrowLeft) => b.move_left(extend),
            Key::Named(Named::ArrowRight) => b.move_right(extend),
            Key::Named(Named::ArrowUp) => b.move_up(extend),
            Key::Named(Named::ArrowDown) => b.move_down(extend),
            Key::Named(Named::Home) => b.move_home(extend),
            Key::Named(Named::End) => b.move_end(extend),
            Key::Named(Named::PageUp) => b.move_page(-(self.editor_rows as isize), extend),
            Key::Named(Named::PageDown) => b.move_page(self.editor_rows as isize, extend),
            // Ctrl/Cmd combos are reserved; don't type them into the buffer.
            Key::Character(_) if mods.control() || mods.command() => {}
            Key::Character(_) => {
                if let Some(text) = composed {
                    b.insert_str(text);
                }
            }
            _ => {}
        }
        // Horizontal too: with wrapping off, typing past the right edge has to
        // bring the caret back into view or you're editing something you can't
        // see.
        b.ensure_cursor_visible_in(self.editor_rows, self.editor_cols);
    }

    /// One tab marker — the dirty dot or the unreviewed diamond.
    ///
    /// Shaped with fallback only when the configured font lacks the glyph, the
    /// same rule `GridView` applies per cell. This is not hypothetical: `◇`
    /// (U+25C7) is absent from Envy Code R, and with `Shaping::Basic` it would
    /// draw as nothing at all — an invisible marker being strictly worse than no
    /// marker, since it silently reports "reviewed".
    fn marker(&self, glyph: &'static str, color: iced::Color) -> Element<'_, Message> {
        let drawable = glyph.chars().all(|c| self.font.can_draw(c));
        text(glyph)
            .size(self.font.size)
            .font(self.font.font)
            .color(color)
            .shaping(if drawable {
                iced::widget::text::Shaping::Basic
            } else {
                iced::widget::text::Shaping::Advanced
            })
            .into()
    }

    /// One tab strip. Shared by the editor and both shell panes, so the three read
    /// as the same control rather than three lookalikes.
    ///
    /// `plus` adds a trailing `+` when the pane can spawn (shells can; buffers need
    /// a file, which needs an open dialog v2 doesn't have yet).
    fn tab_strip<'a>(
        &'a self,
        group: TabGroup,
        labels: Vec<TabLabel>,
        active: usize,
        plus: Option<Message>,
    ) -> Element<'a, Message> {
        let strip_bg = self.palette.background;
        let divider = self.palette.dim();

        // One cell of the strip, carrying its own slice of the underline.
        //
        // Per-cell rather than one rule across the whole strip, because only this
        // way can the line break under the active tab. Nothing else could place
        // that break: tab widths come from their text, so a separate strip-wide
        // rule has no way to know where the boundaries fall.
        //
        // **The line is background-plus-padding, not a `rule`.** A
        // `rule::horizontal` is `width: Fill`, so one inside each cell made every
        // cell demand the full width and the row split itself evenly between them —
        // all the tabs came out the same size. Painting the outer container the
        // line colour and insetting the content by `TAB_BORDER` at the bottom
        // leaves exactly the same 1px line, while the cell still sizes to its text.
        // (`iced::Border` can't do this: it applies to all four sides at once.)
        //
        // An unlit cell paints `background` rather than dropping the inset, so every
        // cell keeps identical geometry and nothing shifts by a pixel as the
        // selection moves. Background rather than transparency because transparency
        // isn't a theme colour — `theme_guard` enforces that.
        // One cell of the strip. `lit` means the underline shows beneath it.
        //
        // **The line is not drawn here.** It's a single full-width `rule` behind
        // the whole strip (see `line` below), and a cell either covers it or
        // doesn't: the active tab is `Fill` height and paints over it, everything
        // else stops `TAB_BORDER` short and lets it through.
        //
        // Painting a per-cell line as a container background instead is what
        // introduced the subpixel blur. Tab widths come from measured text and so
        // land on fractional positions; where two backgrounds abut at a fraction,
        // rounding leaves a sliver of whatever is behind, and a line-coloured
        // backdrop bled through those seams as hairlines — crisp at boundaries that
        // happened to fall on whole pixels, blurry at the ones that didn't. A
        // `rule` carries `snap: true`, which pins it to the physical pixel grid;
        // container backgrounds have no equivalent. So: exactly one rule, snapped,
        // and no cell ever painted the line colour.
        // **Only the active cell paints.** Every other cell is transparent and lets
        // the strip's own background through.
        //
        // This is a subpixel fix, not a tidy-up. The window runs at scale 1, so a
        // logical pixel is a physical pixel and there is no supersampling to hide
        // anything. Tab widths come from measured text, so a cell's edges land on
        // fractional x — and a filled rect with a fractional edge is antialiased
        // across the neighbouring column, which is exactly where the 1px separator
        // rule sits. Each background therefore ate part of the rule beside it and
        // it rendered as less than a full pixel. `Rule::draw` rounds its *own*
        // position to a whole pixel, but nothing rounds a container background, so
        // the rule was crisp and then partly painted over.
        //
        // With only the active tab painting, there is one fractional fill in the
        // strip instead of one per cell.
        // `covers`: this cell paints over the underline running beneath the strip
        // instead of letting it through. True for the active tab and the separators
        // flanking it; false for everything else.
        let cell = |content: Element<'a, Message>, covers: bool| {
            container(content)
                .height(if covers {
                    Length::Fill
                } else {
                    Length::Fixed(TAB_BAR_HEIGHT - TAB_BORDER)
                })
                .style(move |_theme| container::Style {
                    background: covers.then(|| strip_bg.into()),
                    ..container::Style::default()
                })
        };

        let mut tabs: Vec<Element<'a, Message>> = Vec::new();

        for (i, tab_label) in labels.into_iter().enumerate() {
            let TabLabel {
                name,
                dirty,
                unreviewed,
            } = tab_label;
            // The dragged tab reads as active for the whole gesture. Pressing it
            // already made it active and `move_tab` keeps `active` following it,
            // so this is belt-and-braces — but it states the intent rather than
            // depending on that chain holding.
            let dragged = self
                .tab_drag
                .is_some_and(|d| d.group == group && d.at == i);
            let is_active = i == active || dragged;
            let fg = if is_active {
                self.palette.foreground
            } else {
                self.palette.dim()
            };
            let foreground = self.palette.foreground;

            // Not a `button`: `button` hardcodes `Interaction::Pointer` on hover and
            // offers no way to opt out, and a hand cursor over a tab strip is wrong —
            // it's not a link. A `container` reports no interaction, so the pointer
            // stays the normal arrow. The cost is tracking hover ourselves, which is
            // what `hovered_tab` is for.
            //
            // Hover styling is suppressed for the duration of a drag. The tabs
            // are sliding under the pointer, so lighting up whichever one it
            // happens to be over reads as a second, competing highlight — and it
            // lands on tabs the pointer never deliberately visited. The hover is
            // still *tracked* throughout, because it's what tells the drag where
            // the pointer is; only the styling is dropped.
            let hovered = self.tab_drag.is_none() && self.hovered_tab == Some((group, i));
            // Markers are their own widgets so they can keep their own color.
            // Baking them into the label string would tint them with the tab's
            // active/inactive color, which is the whole point of having them.
            let mut line = row![
                text(name)
                    .size(self.font.size)
                    .font(self.font.font)
                    .color(if hovered { foreground } else { fg })
            ];
            if dirty {
                line = line.push(self.marker(" •", self.palette.ansi_slot(DIRTY_SLOT)));
            }
            if unreviewed {
                line = line.push(self.marker(" ◇", self.palette.ansi_slot(UNREVIEWED_SLOT)));
            }
            // No background: the strip paints one, and an extra fill here would be
            // another fractional edge against the separator rule. See `cell`.
            let tab = container(line).padding([4, 10]);
            // The active tab is the cell that covers the underline; that break is
            // what joins it to the pane below.
            tabs.push(cell(
                mouse_area(tab)
                    .on_press(Message::TabPressed(group, i))
                    // Middle-click closes, the usual tab-bar gesture.
                    .on_middle_press(Message::TabClosed(group, i))
                    .on_enter(Message::TabHovered(Some((group, i))))
                    .on_exit(Message::TabExited(group, i))
                    .into(),
                is_active,
            ).into());
            // Separators run the **full** height of the strip, always, and never go
            // through `cell`.
            //
            // Full height because the separator is the tab's side border, and the
            // bottom pixel of the strip is part of it. A separator that stopped at
            // `TAB_BAR_HEIGHT - TAB_BORDER` left the border visibly 1px short at
            // both bottom corners of every tab.
            //
            // Never covering because it doesn't need to: at a separator's column the
            // underline and the rule are the same colour, so a full-height rule
            // hides the line underneath it by painting over it identically. That is
            // also what makes the active tab's break look right — it is bounded by
            // two divider columns rather than by a stub of underline, which is the
            // 1px artifact that started all this.
            //
            // Uniform, so the row's child count never changes with the selection.
            // Removing separators near the active tab did change it, and the strip
            // shifted every time the selection moved.
            tabs.push(
                container(vertical_divider(divider))
                    .height(Length::Fill)
                    .into(),
            );
        }

        if let Some(msg) = plus {
            let foreground = self.palette.foreground;
            let dim = self.palette.dim();
            // `usize::MAX` as the hover slot: the `+` isn't a tab index, and this
            // keeps one hover field rather than a second variant for it.
            let hovered =
                self.tab_drag.is_none() && self.hovered_tab == Some((group, usize::MAX));
            tabs.push(cell(
                mouse_area(
                    container(text("+").size(self.font.size).font(self.font.font))
                        .padding([4, 10])
                        .style(move |_theme| container::Style {
                            text_color: Some(if hovered { foreground } else { dim }),
                            ..container::Style::default()
                        }),
                )
                .on_press(msg)
                .on_enter(Message::TabHovered(Some((group, usize::MAX))))
                .on_exit(Message::TabExited(group, usize::MAX))
                .into(),
                false,
            ).into());
            // No trailing divider. Separators sit *between* tabs, and the `+` is
            // the last thing in the strip — a rule after it would be dividing it
            // from empty space.
        }

        // The strip-wide `mouse_area` exists only to report the pointer's x in
        // one coordinate space. Per-tab `on_move` would give a position relative
        // to whichever tab is under the pointer, which jumps at every boundary
        // and would read as a direction reversal.
        // The underline: one snapped rule spanning the strip, pinned to its
        // bottom edge, drawn *behind* the tabs. The active tab covers its slice of
        // it, which is what makes the line break there; the empty space past the
        // last tab needs no filler cell, because the line already runs the full
        // width underneath.
        let line = column![
            container(text("")).height(Length::Fill),
            rule::horizontal(TAB_BORDER).style(move |_theme| rule::Style {
                color: divider,
                radius: 0.0.into(),
                fill_mode: rule::FillMode::Full,
                snap: true,
            })
        ]
        .width(Length::Fill);

        // The tabs scroll horizontally, and the `scrollable` is doing two jobs.
        //
        // It **clips**, which is the bug it fixes: nothing in iced clips a child to
        // its parent by default, so once the tabs were wider than the pane the
        // strip simply drew over the pane beside it — editor tab names appearing
        // on top of a shell.
        //
        // And it makes the overflow reachable, which v1 had (`tabs_scroll`) and v2
        // had lost. The wheel over the strip scrolls it.
        //
        // Scrollbar suppressed to zero width, as in the Jira pane: a bar under a
        // 26px strip would be most of its height, and the strip is chrome.
        let tabs_bar = scrollable::Scrollbar::new().width(0).scroller_width(0);
        container(iced::widget::stack![
            line,
            scrollable(
                mouse_area(row(tabs).height(Length::Fill))
                    .on_move(move |p| Message::TabPointerMoved(group, p.x)),
            )
            .direction(scrollable::Direction::Horizontal(tabs_bar))
            .height(Length::Fill),
        ])
        .width(Length::Fill)
        .height(Length::Fixed(TAB_BAR_HEIGHT))
        .style(move |_theme| container::Style {
            background: Some(strip_bg.into()),
            ..container::Style::default()
        })
        .into()
    }

    /// Buffer tabs for the editor pane.
    fn tab_bar(&self) -> Element<'_, Message> {
        let labels: Vec<TabLabel> = self
            .buffers
            .iter()
            .map(|b| match b.lock() {
                Ok(b) => TabLabel {
                    name: b.display_name(),
                    dirty: b.dirty,
                    unreviewed: b.unreviewed(),
                },
                Err(_) => TabLabel {
                    name: "[locked]".to_string(),
                    dirty: false,
                    unreviewed: false,
                },
            })
            .collect();
        // `+` makes a new empty tab, the same as `Cmd+N` — a `+` on a tab strip
        // means "one more of these", not "go find me a file". Opening is `Cmd+O`.
        self.tab_strip(TabGroup::Editor, labels, self.active, Some(Message::NewBuffer))
    }

    /// The editor pane's outer strip: which section is showing.
    ///
    /// No `+` — the set of sections belongs to the app, not to something you add
    /// to — and no markers, since a section has no file behind it to be dirty or
    /// unreviewed. Otherwise it's the same control as the strips below and beside
    /// it, deliberately: it's a tab strip, and it should read as one.
    fn section_bar(&self) -> Element<'_, Message> {
        let labels: Vec<TabLabel> = Section::ALL
            .iter()
            .map(|s| TabLabel {
                name: s.label().to_string(),
                dirty: false,
                unreviewed: false,
            })
            .collect();
        let active = Section::ALL
            .iter()
            .position(|s| *s == self.section)
            .unwrap_or(0);
        self.tab_strip(TabGroup::Section, labels, active, None)
    }

    /// The editor section: file tabs over the text surface.
    ///
    /// Everything below the section strip, so the file tabs are subtabs of this
    /// section rather than chrome the pane always carries — switch section and
    /// they go with it, because they describe *this* section's contents.
    fn editor_section(&self) -> Element<'_, Message> {
        // Read mode has no gutter — no source line numbers to show and nothing
        // to fold — and swaps the source for the rendered one. Same widget
        // either way.
        let reading = self
            .buf()
            .lock()
            .map(|b| b.view_mode() == buffer::ViewMode::Read)
            .unwrap_or(false);
        // `GridView` owns its source, so the choice is made by building the
        // widget rather than by boxing twice.
        let grid = if reading {
            GridView::new(
                read::ReadSource {
                    buffer: self.buf().clone(),
                },
                &self.palette,
                self.font,
                Message::EditorResized,
            )
        } else {
            GridView::new(
                BufferSource {
                    buffer: self.buf().clone(),
                    rows: self.editor_rows,
                    highlighter: self.highlighter.clone(),
                    focused: self.focus == Focus::Editor,
                },
                &self.palette,
                self.font,
                Message::EditorResized,
            )
        }
        .offset(self.editor_scroll_px)
        .on_mouse(|g| Message::Mouse(Focus::Editor, g));
        // `line_numbers = false` drops the gutter entirely rather than drawing
        // an empty one.
        let body: Element<'_, Message> = if reading {
            grid.into()
        } else if self.config.line_numbers {
            row![
                Gutter::new(self.buf().clone(), self.font, &self.palette)
                    // Same value the grid gets, or the numbers slide out of line
                    // with their text while the view sits between rows.
                    .offset(self.editor_scroll_px)
                    .on_fold(Message::ToggleFold),
                grid
            ]
            .into()
        } else {
            grid.into()
        };
        // The tab strip is flush to the pane edges; only the content below it is
        // inset. Padding the whole pane would leave the strip floating with a gap
        // on three sides, and its underline would stop short of the pane's width
        // instead of reading as a division of it.
        column![self.tab_bar(), pad_content(body)].into()
    }

    /// The Jira dashboard: generated markdown through the read-mode renderer,
    /// under a title and a clickable refresh control.
    ///
    /// The same `GridView` the editor and the shells use, with a third source.
    /// No gutter and no tab strip — there are no source lines to number and
    /// nothing to fold, and the dashboard is one view rather than a set of them.
    ///
    /// **The two headings are chrome, not markdown**, because a cell grid has no
    /// hit-testing for the text inside it — `GridMouse` reports a row and a column,
    /// not "you clicked the heading". Drawing them as `text` widgets is what makes
    /// one of them clickable. They still look like headings: heading level in this
    /// renderer is carried entirely by colour, so the same slot at the same size is
    /// indistinguishable from a rendered `#`/`##`. The slots come from
    /// `core::markdown` rather than being written out again here, so they cannot
    /// drift from what the renderer would have produced.
    fn jira_section(&self) -> Element<'_, Message> {
        use sacrament_core::markdown;

        let heading = |label: &'static str, slot: usize| {
            text(label)
                .size(self.font.size)
                .font(self.font.font)
                .color(self.palette.ansi_slot(slot))
        };

        // Brightening on hover *is* the affordance, and the cursor stays an arrow —
        // the same choice the tab strips make. A hand cursor would claim this
        // navigates somewhere, and no other control in the app shows one.
        let slot = if self.jira.hovered {
            REFRESH_HOVER_SLOT
        } else {
            REFRESH_SLOT
        };
        let refresh = mouse_area(heading("Refresh", slot))
            .on_press(Message::JiraRefresh)
            .on_enter(Message::JiraRefreshHovered(true))
            .on_exit(Message::JiraRefreshHovered(false));
        // Hidden while a fetch is in flight: it can't do anything then (a second
        // press is refused by the `loading` guard), and a control that responds to
        // nothing is worse than no control. Its absence is also a second, quieter
        // signal that something is happening, alongside the body going to
        // `JIRA_LOADING`.
        //
        // Deliberately *not* hidden on an error or when unconfigured: those are
        // exactly the states where retrying — after fixing config.toml, or after
        // the network came back — is the thing you want to do.
        let loading = self.jira.loading;

        let body: Element<'_, Message> = match &self.jira.view {
            JiraView::Loading => self.jira_note("Loading..."),
            JiraView::Note(msg) => self.jira_note(msg),
            JiraView::Ready(page) => self.jira_tables(page),
        };

        // Spacing is one cell height, matching the gap a markdown block break used
        // to leave. Padding is applied once around the whole column rather than to
        // the body alone, which would leave the headings hanging outside the inset
        // the content below them observes.
        let mut stack = iced::widget::Column::new()
            .spacing(self.font.cell_height())
            .push(heading("Summary", markdown::h1_slot().index()));
        if !loading {
            stack = stack.push(refresh);
        }

        // The **whole** section scrolls, headings included, rather than a fixed
        // header over a scrolling list. One scroll region reads as one document;
        // pinning the title and the refresh control would spend two rows of a
        // short pane on chrome that has nothing to say once you're reading.
        //
        // Content keeps its natural height inside — `Fill` there would clamp it to
        // the viewport and there would be nothing to scroll.
        // No scrollbar. The app has no other visible scroll affordance — the
        // editor and the shells have none either — and one permanent bar down the
        // side of a pane would be the only piece of chrome of its kind. Width and
        // scroller both zero so it takes no space, rather than being painted
        // `background`: a hidden bar that still reserved a column would inset the
        // content for nothing.
        let bar = scrollable::Scrollbar::new().width(0).scroller_width(0);
        // Padding sits **inside** the scroll region, on the content, so it scrolls
        // with it. Outside, the inset is a fixed frame and the first row is jammed
        // against the top edge the moment you scroll.
        container(
            scrollable(container(stack.push(body)).padding(PANE_PADDING))
                .direction(scrollable::Direction::Vertical(bar))
                .height(Length::Fill),
        )
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
    }

    /// Plain prose for the Jira section: loading, setup instructions, an error.
    fn jira_note(&self, message: &str) -> Element<'_, Message> {
        text(message.to_string())
            .size(self.font.size)
            .font(self.font.font)
            .color(self.palette.foreground)
            .into()
    }

    /// The dashboard: one real table per status group.
    ///
    /// Widgets rather than the markdown renderer, because a table drawn as *text*
    /// cannot be responsive — its columns are measured in characters, so they can
    /// only be sized for one pane width. `markdown::emit_table` scales columns
    /// down when they don't fit but never truncates a cell, so a narrow pane
    /// produced rows that each overflowed by a different amount and no column
    /// lined up. Real columns size themselves to whatever width the pane has.
    fn jira_tables(&self, page: &sacrament_core::jira::Page) -> Element<'_, Message> {
        use sacrament_core::markdown;

        let mut body = iced::widget::Column::new().spacing(self.font.cell_height());
        for (status, issues) in sacrament_core::jira::group_by_status(&page.issues) {
            body = body
                .push(
                    text(format!("{status} ({})", issues.len()))
                        .size(self.font.size)
                        .font(self.font.font)
                        .color(self.palette.ansi_slot(markdown::h2_slot().index())),
                )
                .push(self.issue_table(&issues));
        }
        if page.more {
            // The one thing that has to be said when it applies: a dashboard
            // showing part of the result set without saying so is misleading.
            body = body.push(
                text("More issues matched than were fetched.")
                    .size(self.font.size)
                    .font(self.font.font)
                    .color(self.palette.dim()),
            );
        }
        body.into()
    }

    /// One status group's issues, as a table that fills the pane's width.
    ///
    /// **Every column is a `FillPortion`, and that is what keeps the rows aligned.**
    /// Each row is its own widget, so a `Shrink` column would size independently
    /// per row and the table would come out ragged. Portions give every row the
    /// same proportional split at whatever width the pane happens to be.
    ///
    /// Summary takes the lion's share and wraps rather than truncating — the
    /// character budget the text version needed is gone, along with the truncation
    /// it forced.
    fn issue_table(&self, issues: &[&sacrament_core::jira::Issue]) -> Element<'_, Message> {
        let dim = self.palette.dim();
        let fg = self.palette.foreground;
        let cell = |content: String, portion: u16, color: iced::Color| {
            text(content)
                .size(self.font.size)
                .font(self.font.font)
                .color(color)
                .width(Length::FillPortion(portion))
        };

        let mut table = iced::widget::Column::new().spacing(TABLE_ROW_GAP);

        let mut header = iced::widget::Row::new().spacing(TABLE_COL_GAP);
        for (label, portion) in JIRA_COLUMNS {
            header = header.push(cell(label.to_string(), portion, dim));
        }
        table = table.push(header).push(rule::horizontal(TAB_BORDER).style(
            move |_theme| rule::Style {
                color: dim,
                radius: 0.0.into(),
                fill_mode: rule::FillMode::Full,
                snap: true,
            },
        ));

        for issue in issues {
            let p = |i: usize| JIRA_COLUMNS[i].1;
            table = table.push(
                iced::widget::Row::new()
                    .spacing(TABLE_COL_GAP)
                    // Top, so a wrapped summary doesn't drag its row's other cells
                    // down to the middle of it.
                    .align_y(iced::Alignment::Start)
                    .push(cell(issue.key.clone(), p(0), fg))
                    .push(cell(
                        issue.priority.clone().unwrap_or_else(|| "-".into()),
                        p(1),
                        dim,
                    ))
                    .push(cell(issue.kind.clone(), p(2), dim))
                    .push(cell(
                        issue.updated.clone().unwrap_or_else(|| "-".into()),
                        p(3),
                        dim,
                    ))
                    .push(cell(issue.summary.clone(), p(4), fg)),
            );
        }
        table.into()
    }

    /// Shell tabs for one pane, labelled by each shell's cwd basename.
    fn shell_tab_bar(&self, id: PaneId) -> Element<'_, Message> {
        let pane = self.pane(id);
        let labels: Vec<TabLabel> = pane
            .shells
            .iter()
            .map(|s| TabLabel {
                name: s.label.clone(),
                dirty: false,
                unreviewed: false,
            })
            .collect();
        self.tab_strip(
            TabGroup::Shell(id),
            labels,
            pane.active,
            Some(Message::SpawnShell(id)),
        )
    }

    fn view(&self) -> Element<'_, Message> {
        // Captured before the closure so it doesn't borrow `self` inside a
        // `move` context alongside the other palette uses.
        let pane_background = self.palette.background;
        // The divider comes from the theme's muted slot — the only one of the 16
        // that reads against the background in a typical dark scheme (`black` is
        // usually the background itself). Same source as the inactive line
        // numbers, so chrome stays one family.
        let divider = self.palette.dim();

        let grid = pane_grid(&self.panes, move |_pane, kind, _maximized| {
            let inner: Element<'_, Message> = match kind {
                PaneKind::Shell(id) => {
                    let body: Element<'_, Message> = match self.pane(*id).active() {
                        Some(shell) => {
                            let key = shell.key;
                            pad_content(
                                GridView::new(
                                    TerminalSource {
                                        terminal: shell.terminal.clone(),
                                        show_cursor: self.focus == Focus::Shell(*id),
                                    },
                                    &self.palette,
                                    self.font,
                                    move |r, c| Message::GridResized(key, r, c),
                                )
                                .offset(shell.scroll_px)
                                .on_mouse(move |g| Message::Mouse(Focus::Shell(*id), g)),
                            )
                        }
                        // An empty pane renders blank until `+` is used. v1 does
                        // the same; it's intended, not a missing case.
                        None => container(text(""))
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into(),
                    };
                    column![self.shell_tab_bar(*id), body].into()
                }
                // The pane holds several sections, of which the editor is one.
                // The outer strip picks between them; the section decides what
                // sits under it, including whether there are file subtabs at all.
                PaneKind::Editor => {
                    let body = match self.section {
                        Section::Editor => self.editor_section(),
                        Section::Jira => self.jira_section(),
                    };
                    column![self.section_bar(), body].into()
                }
            };
            let focus = match kind {
                PaneKind::Shell(id) => Focus::Shell(*id),
                PaneKind::Editor => Focus::Editor,
            };
            // The pane itself carries no padding — its children decide what gets
            // inset. Background is painted here so the inset area reads as part of
            // the pane rather than a gap behind it. No border: the divider between
            // panes is the gap behind them, not an outline around each one.
            let padded = container(inner)
                .width(Length::Fill)
                .height(Length::Fill)
                .style(move |_theme| container::Style {
                    background: Some(pane_background.into()),
                    ..container::Style::default()
                });
            // Catches clicks in the padding. The grid captures clicks on cells, so
            // this only fires for the band around them.
            pane_grid::Content::new(mouse_area(padded).on_press(Message::FocusPane(focus)))
        })
        .width(Length::Fill)
        .height(Length::Fill)
        .spacing(PANE_DIVIDER)
        // Suppress iced's hover/drag split highlights. The divider is visible at
        // all times and the resize mouse cursor already signals grabbability, so
        // the highlight was redundant motion. Width 0 rather than a transparent
        // color: transparency isn't a theme color, and `theme_guard` enforces that.
        .style(move |theme| pane_grid::Style {
            hovered_split: pane_grid::Line {
                color: divider,
                width: 0.0,
            },
            picked_split: pane_grid::Line {
                color: divider,
                width: 0.0,
            },
            // Everything else keeps iced's default. Only the split lines were
            // asked to go; `hovered_region` belongs to pane *dragging*, which is a
            // different interaction.
            ..pane_grid::default(theme)
        })
        .on_resize(8, Message::PaneResized)
        .on_drag(Message::PaneDragged);

        // This background is what shows through `pane_grid`'s spacing, so it *is*
        // the divider.
        let grid = container(grid)
            .width(Length::Fill)
            .height(Length::Fill)
            .style(move |_theme| container::Style {
                background: Some(divider.into()),
                ..container::Style::default()
            });

        // No status bar of any kind — the window is the grid, edge to edge. The
        // bottom row appears only while a prompt is open, and a prompt is an
        // *input*, not a message: everything the app has to say goes through
        // `alert` and a native dialog, which can't be missed and can't be left
        // sitting on screen. Throughput/memory numbers aren't UI either — they go
        // to stderr under SACRAMENT_METRICS.
        match self.prompt_row() {
            Some(rowel) => column![grid, rowel].into(),
            None => grid.into(),
        }
    }

    /// The bottom row, which exists only while a prompt is open.
    fn prompt_row(&self) -> Option<Element<'_, Message>> {
        let foreground = self.palette.foreground;
        let dim = self.palette.dim();
        let background = self.palette.background;

        if let Some(prompt) = &self.prompt {
            let label = text(prompt.kind.label())
                .size(self.font.size)
                .font(self.font.font)
                .color(dim);
            // A real `text_input`, not a hand-rolled line: it brings the caret,
            // selection, arrow keys and Cmd+V with it. Every color still comes
            // from the palette, and the border is zero-width because the strip's
            // own presence is the affordance.
            let field = text_input("", &prompt.input)
                .id(PROMPT_ID.clone())
                .on_input(Message::PromptInput)
                .size(self.font.size)
                .font(self.font.font)
                .padding(0)
                .style(move |_theme, _status| text_input::Style {
                    background: background.into(),
                    border: iced::Border {
                        color: dim,
                        width: 0.0,
                        radius: 0.0.into(),
                    },
                    icon: dim,
                    placeholder: dim,
                    value: foreground,
                    selection: self.palette.selection_background,
                });
            let mut line = row![label, field].spacing(8);
            if let Some(note) = &prompt.note {
                line = line.push(
                    text(note.clone())
                        .size(self.font.size)
                        .font(self.font.font)
                        .color(dim),
                );
            }
            return Some(
                container(line)
                    .padding([2, 6])
                    .width(Length::Fill)
                    .style(move |_theme| container::Style {
                        background: Some(background.into()),
                        ..container::Style::default()
                    })
                    .into(),
            );
        }
        None
    }
}

/// Minimal key → bytes mapping, enough to drive a shell.
///
/// Note what's *absent* compared to v1's `shell::key_to_bytes`: no kitty
/// protocol negotiation, no `apply_shift` US-layout table, no CSI leak guard.
/// iced hands over real modifier state and already-composed text, so that whole
/// class of terminal workaround disappears.
fn keymap(
    key: &iced::keyboard::Key,
    mods: iced::keyboard::Modifiers,
    composed: Option<&str>,
) -> Option<Vec<u8>> {
    use iced::keyboard::Key;
    use iced::keyboard::key::Named;

    // Cmd belongs to the application and never reaches the shell. Without this
    // an unbound `Cmd+K` would send a bare "k" down the PTY, because the branch
    // below falls through to the composed text.
    if cfg!(target_os = "macos") && mods.logo() {
        return None;
    }
    match key {
        Key::Character(s) => {
            let c = s.chars().next()?;
            if mods.control() {
                // Ctrl+A..Z → 0x01..0x1a, plus the standard punctuation cases.
                let byte = match c.to_ascii_lowercase() {
                    'a'..='z' => (c.to_ascii_lowercase() as u8) - b'a' + 1,
                    '[' => 0x1b,
                    '\\' => 0x1c,
                    ']' => 0x1d,
                    '^' => 0x1e,
                    '_' | '?' | '/' => 0x1f,
                    ' ' => 0x00,
                    _ => return None,
                };
                return Some(vec![byte]);
            }
            let mut out = Vec::new();
            if mods.alt() {
                out.push(0x1b);
            }
            // Use the composed text when available — it has shift, IME, and
            // dead-key composition already applied. This is the whole reason
            // v1's `apply_shift` US-layout table isn't needed here.
            out.extend_from_slice(composed.unwrap_or(s.as_str()).as_bytes());
            Some(out)
        }
        Key::Named(named) => {
            // Word-wise editing, checked before the plain arrows because those
            // ignore modifiers. `Esc`-prefixed `b`/`f`/`DEL` is the macOS
            // terminal convention — iTerm2's "natural text editing" preset and
            // Ghostty's defaults both send exactly this, and it's what zsh and
            // readline already bind to backward-word / forward-word /
            // backward-kill-word. The CSI form (`\x1b[1;3D`) is xterm's and
            // needs the shell to have bound it.
            if mods.alt() {
                let seq: Option<&[u8]> = match named {
                    Named::ArrowLeft => Some(b"\x1bb"),
                    Named::ArrowRight => Some(b"\x1bf"),
                    Named::Backspace => Some(b"\x1b\x7f"),
                    _ => None,
                };
                if let Some(seq) = seq {
                    return Some(seq.to_vec());
                }
            }
            let seq: &[u8] = match named {
                // Shift+Enter is a newline rather than a submit, which is what a
                // TUI composing multi-line input wants. Sent as `\n` (what
                // `Ctrl+J` produces) rather than the `\x1b\r` that Claude Code's
                // own `/terminal-setup` installs elsewhere, because `\n` is
                // *also* `accept-line` in zsh and readline — so a plain shell
                // prompt still submits on Shift+Enter, while `\x1b\r` there is
                // unbound and would do nothing.
                Named::Enter if mods.shift() => b"\n",
                Named::Enter => b"\r",
                // Back-tab (CSI Z). Without the shift arm the modifier is simply
                // dropped and a plain tab goes down the PTY, so a TUI binding
                // Shift+Tab sees the unshifted key — indistinguishable from Tab.
                // `\x1b[Z` is what every terminal sends and what terminfo calls
                // `kcbt`; readline and zsh already have it.
                Named::Tab if mods.shift() => b"\x1b[Z",
                Named::Tab => b"\t",
                Named::Backspace => b"\x7f",
                Named::Escape => b"\x1b",
                Named::Space => b" ",
                Named::ArrowUp => b"\x1b[A",
                Named::ArrowDown => b"\x1b[B",
                Named::ArrowRight => b"\x1b[C",
                Named::ArrowLeft => b"\x1b[D",
                Named::Home => b"\x1b[H",
                Named::End => b"\x1b[F",
                Named::PageUp => b"\x1b[5~",
                Named::PageDown => b"\x1b[6~",
                Named::Delete => b"\x1b[3~",
                _ => return None,
            };
            Some(seq.to_vec())
        }
        _ => None,
    }
}

/// Insets a pane's *content*. Applied per child rather than to the pane, so
/// chrome like the tab strips can sit flush while text stays off the edge.
fn pad_content<'a>(content: impl Into<Element<'a, Message>>) -> Element<'a, Message> {
    container(content)
        .padding(PANE_PADDING)
        .width(Length::Fill)
        .height(Length::Fill)
        .into()
}

/// A 1px full-height separator in the given color.
///
/// `iced::Border` applies to all four sides at once, so a single edge has to be a
/// `rule` widget. Extracted because the tab strip draws one after every tab.
fn vertical_divider(color: iced::Color) -> Element<'static, Message> {
    rule::vertical(TAB_BORDER)
        .style(move |_theme| rule::Style {
            color,
            radius: 0.0.into(),
            fill_mode: rule::FillMode::Full,
            snap: true,
        })
        .into()
}

#[cfg(test)]
mod shell_tests {
    use super::*;

    #[test]
    fn a_new_tab_starts_at_its_panes_measured_size() {
        // Nothing corrects this later: `GridView` publishes a size only when it
        // *changes*, and one widget state serves the pane's grid however many tabs
        // come and go — so a tab added to an already-measured pane never sees a
        // `GridResized`. Its PTY is told the pane's real size regardless
        // (`Event::Attached`), and that gap is what put zsh's partial-line marker
        // on the row above the prompt: `COLUMNS` cells wrapping against a grid
        // still 80 wide.
        let key = ShellKey {
            pane: PaneId::Bottom,
            serial: 7,
        };
        let shell = Shell::new(key, Some((40, 173)));
        let mut term = shell.terminal.lock().unwrap();
        assert!(
            !term.resize(40, 173),
            "the terminal should already be at the pane's size"
        );
        assert!(term.resize(40, 174), "a real change must still register");
    }

    #[test]
    fn an_unmeasured_pane_leaves_the_placeholder_alone() {
        // First launch: no grid has laid out, so there is no size to adopt and the
        // placeholder stands until the first `GridResized` reaches the whole pane.
        let shell = Shell::new(
            ShellKey {
                pane: PaneId::Right,
                serial: 0,
            },
            None,
        );
        let mut term = shell.terminal.lock().unwrap();
        assert!(!term.resize(24, 80));
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use iced::keyboard::{Key, Modifiers};

    fn ch(c: &str) -> Key {
        Key::Character(c.into())
    }

    fn args(v: &[&str]) -> super::Args {
        super::parse_args(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn flags_are_not_mistaken_for_filenames() {
        // The bug this pins: `State::new` used to treat every argument as a path,
        // so `--syntax=Rust` became a request to open a file by that name.
        let a = args(&["--syntax=Rust", "src/main.rs", "--review"]);
        assert_eq!(a.syntax.as_deref(), Some("Rust"));
        assert!(a.review);
        assert_eq!(a.files.len(), 1);
        assert_eq!(a.files[0].0, std::path::PathBuf::from("src/main.rs"));

        let a = args(&["-s", "Python", "x.txt"]);
        assert_eq!(a.syntax.as_deref(), Some("Python"));
        assert_eq!(a.files.len(), 1);
    }

    #[test]
    fn several_files_open_as_several_tabs() {
        let a = args(&["a.rs", "b.rs", "c.rs"]);
        assert_eq!(a.files.len(), 3);
        assert!(a.files.iter().all(|(_, line)| line.is_none()));
    }

    #[test]
    fn the_finder_process_serial_argument_is_ignored() {
        // Without this the app exits(2) before opening a window, so launching
        // from the Dock silently does nothing while the same binary run from a
        // shell works.
        let a = args(&["-psn_0_774binary", "notes.md"]);
        assert_eq!(a.files.len(), 1);
        assert_eq!(a.files[0].0, std::path::PathBuf::from("notes.md"));
    }

    #[test]
    fn an_unknown_flag_is_an_error_rather_than_a_filename() {
        let argv = vec!["--nope".to_string()];
        assert!(super::parse_args(&argv).is_err());
        // But a bare `-` is a plausible filename, not a flag.
        assert!(super::parse_args(&["-".to_string()]).is_ok());
    }

    #[test]
    fn a_line_suffix_is_split_off() {
        assert_eq!(
            super::split_line_suffix("src/main.rs:42"),
            (std::path::PathBuf::from("src/main.rs"), Some(42))
        );
        // No suffix, and a colon that isn't a line number, both stay whole.
        assert_eq!(
            super::split_line_suffix("src/main.rs"),
            (std::path::PathBuf::from("src/main.rs"), None)
        );
        assert_eq!(
            super::split_line_suffix("weird:name"),
            (std::path::PathBuf::from("weird:name"), None)
        );
    }

    #[test]
    fn moving_a_tab_slides_the_ones_it_passes() {
        // Remove-then-insert, not swap: dragging A to C's slot must leave B and C
        // in order, not exchange A and C.
        let mut v = vec!['A', 'B', 'C', 'D'];
        assert!(move_item(&mut v, 0, 2));
        assert_eq!(v, vec!['B', 'C', 'A', 'D']);

        let mut v = vec!['A', 'B', 'C', 'D'];
        assert!(move_item(&mut v, 3, 1));
        assert_eq!(v, vec!['A', 'D', 'B', 'C']);
    }

    #[test]
    fn moving_a_tab_onto_itself_or_out_of_range_does_nothing() {
        let mut v = vec!['A', 'B'];
        assert!(!move_item(&mut v, 1, 1), "a plain click must not reorder");
        assert!(!move_item(&mut v, 0, 9));
        assert!(!move_item(&mut v, 9, 0));
        assert_eq!(v, vec!['A', 'B']);
        // An empty strip can be released over without panicking.
        let mut empty: Vec<char> = Vec::new();
        assert!(!move_item(&mut empty, 0, 0));
    }

    #[test]
    fn a_moved_tab_ends_up_where_the_pointer_left_it() {
        // The property the drop relies on: after the move, index `to` holds the
        // dragged item. `active` is set to `to` on that basis.
        for (from, to) in [(0, 3), (3, 0), (1, 2), (2, 1)] {
            let mut v = vec!['A', 'B', 'C', 'D'];
            let dragged = v[from];
            move_item(&mut v, from, to);
            assert_eq!(v[to], dragged, "moving {from} -> {to}");
            assert_eq!(v.len(), 4, "nothing lost or duplicated");
        }
    }

    #[test]
    fn only_the_strips_holding_user_opened_tabs_reorder() {
        // The section strip is a fixed set, so a press there must start no drag —
        // `move_tab` has nothing to move for it, and a drag would be asking to
        // persist an order that means nothing next launch.
        assert!(!TabGroup::Section.reorderable());
        assert!(TabGroup::Editor.reorderable());
        assert!(TabGroup::Shell(PaneId::Bottom).reorderable());
    }

    #[test]
    fn every_section_is_reachable_from_its_strip_position() {
        // `select_section` indexes `ALL` and `section_bar` finds the active one
        // with `position`, which returns the *first* match — so a duplicate in
        // `ALL` would leave one tab permanently unselectable.
        for (i, section) in Section::ALL.iter().enumerate() {
            assert_eq!(
                Section::ALL.iter().position(|s| s == section),
                Some(i),
                "{} appears twice in ALL",
                section.label()
            );
        }
    }

    #[test]
    fn app_ctrl_is_plain_ctrl_on_macos() {
        assert_eq!(is_app_ctrl(Modifiers::CTRL), cfg!(target_os = "macos"));
        assert_eq!(
            is_app_ctrl(Modifiers::CTRL | Modifiers::ALT),
            !cfg!(target_os = "macos")
        );
        assert!(!is_app_ctrl(Modifiers::empty()));
        assert!(!is_app_ctrl(Modifiers::SHIFT));
    }

    #[test]
    fn command_keys_never_reach_the_shell() {
        // Without the guard, an unbound Cmd chord falls through to the composed
        // text and types a bare letter into the PTY.
        if cfg!(target_os = "macos") {
            assert_eq!(keymap(&ch("k"), Modifiers::LOGO, Some("k")), None);
            assert_eq!(keymap(&ch("s"), Modifiers::LOGO, Some("s")), None);
        }
    }

    #[test]
    fn ctrl_still_produces_control_codes_for_the_shell() {
        // The whole point of moving the app onto Cmd: Ctrl belongs to the shell
        // again. Ctrl+C must be SIGINT, Ctrl+R reverse search, Ctrl+W kill-word.
        assert_eq!(keymap(&ch("c"), Modifiers::CTRL, Some("c")), Some(vec![0x03]));
        assert_eq!(keymap(&ch("r"), Modifiers::CTRL, Some("r")), Some(vec![0x12]));
        assert_eq!(keymap(&ch("w"), Modifiers::CTRL, Some("w")), Some(vec![0x17]));
        assert_eq!(keymap(&ch("z"), Modifiers::CTRL, Some("z")), Some(vec![0x1a]));
    }

    #[test]
    fn plain_typing_still_reaches_the_shell() {
        assert_eq!(keymap(&ch("k"), Modifiers::empty(), Some("k")), Some(b"k".to_vec()));
    }

    fn named(n: iced::keyboard::key::Named) -> Key {
        Key::Named(n)
    }

    #[test]
    fn shift_enter_is_a_newline_and_plain_enter_still_submits() {
        use iced::keyboard::key::Named;
        assert_eq!(
            keymap(&named(Named::Enter), Modifiers::SHIFT, None),
            Some(b"\n".to_vec())
        );
        assert_eq!(
            keymap(&named(Named::Enter), Modifiers::empty(), None),
            Some(b"\r".to_vec())
        );
    }

    #[test]
    fn shift_tab_is_a_back_tab_the_shell_can_tell_apart() {
        use iced::keyboard::key::Named;
        // The modifier used to be dropped, so a TUI binding Shift+Tab received a
        // plain tab and had no way to know the difference. `cat -v` shows `^[[Z`.
        assert_eq!(
            keymap(&named(Named::Tab), Modifiers::SHIFT, None),
            Some(b"\x1b[Z".to_vec())
        );
        assert_eq!(
            keymap(&named(Named::Tab), Modifiers::empty(), None),
            Some(b"\t".to_vec())
        );
    }

    #[test]
    fn option_arrows_move_by_word_in_the_shell() {
        use iced::keyboard::key::Named;
        // Esc+b / Esc+f, the convention zsh and readline already bind. Without
        // the modifier these must stay the bare cursor keys.
        assert_eq!(
            keymap(&named(Named::ArrowLeft), Modifiers::ALT, None),
            Some(b"\x1bb".to_vec())
        );
        assert_eq!(
            keymap(&named(Named::ArrowRight), Modifiers::ALT, None),
            Some(b"\x1bf".to_vec())
        );
        assert_eq!(
            keymap(&named(Named::Backspace), Modifiers::ALT, None),
            Some(b"\x1b\x7f".to_vec())
        );
        assert_eq!(
            keymap(&named(Named::ArrowLeft), Modifiers::empty(), None),
            Some(b"\x1b[D".to_vec())
        );
    }

    #[test]
    fn a_dropped_path_is_escaped_for_the_shell() {
        use std::path::Path;
        assert_eq!(
            shell_escaped(Path::new("/tmp/a b.png")),
            "/tmp/a\\ b.png",
            "a space has to be escaped or the shell reads two words"
        );
        assert_eq!(shell_escaped(Path::new("/tmp/plain.png")), "/tmp/plain.png");
        assert_eq!(
            shell_escaped(Path::new("/tmp/it's $HOME(1).png")),
            "/tmp/it\\'s\\ \\$HOME\\(1\\).png"
        );
        // Non-ASCII letters aren't shell-special, so they pass through as
        // themselves rather than being backslashed into noise.
        assert_eq!(shell_escaped(Path::new("/tmp/café.png")), "/tmp/café.png");
    }
}
