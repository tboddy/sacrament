//! PTY plumbing: spawn a shell, pump its bytes into the iced runtime, and take
//! keystrokes back out.
//!
//! Shape of the data flow, and why:
//!
//! `Subscription::run` takes a bare `fn() -> Stream`, so the stream builder
//! can't capture anything — the PTY has to be created *inside* it. That leaves
//! the app with no handle to write to. The fix is for the stream's first item to
//! hand the writer back out (`Event::Attached`), which the app stashes in its
//! state. After that, input is a plain channel send.
//!
//! Reader bytes cross the thread → async boundary through
//! `futures::channel::mpsc::unbounded`, whose sender is usable from a plain std
//! thread and whose receiver *is* a `Stream`. That means no async runtime
//! assumptions and no polling tick — v1 had to poll `event::poll(20ms)` because
//! it owned the event loop; here output is genuinely push-driven.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result};
use iced::futures::channel::mpsc as async_mpsc;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

/// Bytes from the PTY, plus the lifecycle events around it.
#[derive(Debug, Clone)]
pub enum Event {
    /// The PTY is live. Carries the input side and the resize control.
    Attached(Handle),
    /// The shell process exists. Carries its pid, which is what makes cwd tracking
    /// possible; it can't ride on `Attached` because that fires *before* the spawn
    /// (the spawn waits for a size — see `run`).
    Started { pid: Option<u32> },
    Output(Vec<u8>),
    Exited,
    Failed(String),
}

/// The app's handle to a running PTY. Cloneable so it can live in the app state
/// and be used from `update` without borrowing the stream.
#[derive(Debug, Clone)]
pub struct Handle {
    input: mpsc::Sender<Msg>,
}

#[derive(Debug)]
enum Msg {
    Write(Vec<u8>),
    Resize { rows: u16, cols: u16 },
}

impl Handle {
    pub fn write(&self, bytes: Vec<u8>) {
        let _ = self.input.send(Msg::Write(bytes));
    }

    pub fn resize(&self, rows: u16, cols: u16) {
        let _ = self.input.send(Msg::Resize { rows, cols });
    }
}

/// Placeholder PTY size, used only for the window between `openpty` and the
/// first real size arriving. The shell is not started until then, so it never
/// draws at this size.
const INITIAL_ROWS: u16 = 24;
const INITIAL_COLS: u16 = 80;

/// How long to wait for the grid to report its real size before starting the
/// shell anyway. A bound rather than an unconditional wait: if a size never
/// arrives, a shell at the placeholder size beats no shell at all.
const SIZE_WAIT: Duration = Duration::from_millis(500);

/// Which shell pane a shell lives in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PaneId {
    Bottom,
    Right,
}

impl PaneId {
    pub const ALL: [PaneId; 2] = [PaneId::Bottom, PaneId::Right];
}

/// Identity of one shell. Doubles as its subscription identity —
/// `Subscription::run_with` hashes this, so each shell gets its own PTY instead of
/// collapsing into a shared one.
///
/// `serial` is a never-reused counter, which is what makes shells spawnable at
/// runtime: adding a key to the subscription list starts a PTY, removing it stops
/// one. Reusing serials would let a closed shell's subscription be mistaken for a
/// new one's and hand back the dead stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShellKey {
    pub pane: PaneId,
    pub serial: u64,
}

/// Where a restored shell should start.
///
/// Threaded through `ShellKey`'s subscription data rather than a field on the key,
/// because the key is hashed for subscription identity and two shells differing only
/// by starting directory must still be two shells. `Spawn` carries both.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spawn {
    pub key: ShellKey,
    /// `None` starts in the process cwd.
    pub cwd: Option<std::path::PathBuf>,
}

/// Build the output stream for one pane. Passed to `Subscription::run_with`, so
/// it must be a plain `fn` — captures aren't allowed, which is why the PTY is
/// created inside.
// Returns the receiver *concretely* rather than `impl Stream`. `run_with` wants
// `fn(&D) -> S` for a single `S`, and an opaque return type can't unify with that
// higher-ranked signature — the error is an unhelpful "one type is more general
// than the other". Naming the type sidesteps it. `UnboundedReceiver` already
// implements `Stream`, which is the whole reason this design works.
// Events are tagged with the id *inside* the stream rather than by a `.map()` on
// the subscription: `Subscription::map` requires a non-capturing closure, and one
// that captures the id isn't. The id is already here, so tagging at the source is
// both simpler and free.
pub fn stream(spawn: &Spawn) -> async_mpsc::UnboundedReceiver<(ShellKey, Event)> {
    let (tx, rx) = async_mpsc::unbounded::<(ShellKey, Event)>();
    let id = spawn.key;
    let start_cwd = spawn.cwd.clone();

    // The spawn itself can fail (no PTY available, shell missing). Report it
    // through the stream rather than panicking on a background thread.
    std::thread::spawn(move || {
        if let Err(e) = run(id, start_cwd, tx.clone()) {
            let mut tx = tx;
            let _ = tx.start_send((id, Event::Failed(e.to_string())));
        }
    });

    rx
}

fn run(
    id: ShellKey,
    start_cwd: Option<std::path::PathBuf>,
    mut tx: async_mpsc::UnboundedSender<(ShellKey, Event)>,
) -> Result<()> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: INITIAL_ROWS,
            cols: INITIAL_COLS,
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;

    // A **login** shell, which is what every terminal emulator starts and what
    // `new_default_prog` gives: it resolves `$SHELL` (falling back to the password
    // database rather than to `/bin/sh`) and sets argv0 to `-zsh`, the leading
    // dash being how a shell knows it's a login shell.
    //
    // `CommandBuilder::new(shell)` was here and it's the reason `docker`, `brew`
    // and anything else outside `/usr/bin` couldn't be found. Without the dash,
    // zsh reads `~/.zshrc` and nothing else — no `/etc/zprofile`, so
    // `/usr/libexec/path_helper` never runs and `/etc/paths.d` is never read, and
    // no `~/.zprofile`, which is where `brew shellenv` and most credential
    // helpers are set up. It went unnoticed because it isn't visible when the app
    // is started *from* a terminal: the full PATH is inherited from the shell
    // that launched it, so only a Dock launch — where the parent environment is
    // launchd's `/usr/bin:/bin:/usr/sbin:/sbin` — shows what's missing. That's
    // also why v1 never had the bug: it only ever ran inside an emulator.
    //
    // Measured, launched from the Dock: the non-login PATH ends
    // `…/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin` — no `/usr/local/bin`,
    // where Docker Desktop puts both its CLI and
    // `docker-credential-osxkeychain`, and no `/opt/local/bin`. The login shell
    // has all of them.
    //
    // Note `new_default_prog` panics if `arg` is called on it — it's the "just run
    // the user's shell" constructor, and there are no arguments to add.
    let mut cmd = CommandBuilder::new_default_prog();
    // A restored directory that no longer exists falls back to the process cwd,
    // rather than failing to spawn.
    let cwd = start_cwd
        .filter(|p| p.is_dir())
        .or_else(|| std::env::current_dir().ok())
        .unwrap_or_else(|| Path::new(".").to_path_buf());
    if cwd.is_dir() {
        cmd.cwd(&cwd);
    }
    // Unlike v1, this matches what we actually render. v1 advertised
    // `xterm-256color` while collapsing everything above index 15 to the
    // default color, so 256-color TUIs drew blank; palette.rs resolves the
    // full cube and truecolor, so the claim is now honest.
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone pty reader failed")?;
    let mut writer = pair.master.take_writer().context("take pty writer failed")?;

    // Input side: a std channel drained on its own thread. Keeps `update`
    // non-blocking — a full PTY buffer can't stall the UI.
    let (in_tx, in_rx) = mpsc::channel::<Msg>();
    let master = pair.master;

    // Hand the input side out *before* starting the shell, so the app can push
    // the real size down first.
    // Handed out before the shell is spawned, which is why the pid rides on
    // `Event::Started` instead of here — it doesn't exist yet.
    tx.start_send((id, Event::Attached(Handle { input: in_tx })))?;

    // Wait for that size before spawning.
    //
    // This is the whole reason the shell isn't started above. The grid can only
    // report its size once it has been laid out, which is after the PTY exists.
    // Starting the shell first meant it drew its prompt at the placeholder width
    // and then took a SIGWINCH — and zsh's redraw scattered fragments of the
    // prompt across the grid and left a reverse-video `%` behind.
    //
    // Writes that arrive before the size are stashed rather than dropped; there
    // shouldn't be any this early, but losing keystrokes would be worse than
    // holding them for a few milliseconds.
    let mut stashed: Vec<Vec<u8>> = Vec::new();
    let deadline = std::time::Instant::now() + SIZE_WAIT;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match in_rx.recv_timeout(remaining) {
            Ok(Msg::Resize { rows, cols }) => {
                let _ = master.resize(PtySize {
                    rows,
                    cols,
                    pixel_width: 0,
                    pixel_height: 0,
                });
                break;
            }
            Ok(Msg::Write(bytes)) => stashed.push(bytes),
            Err(_) => break,
        }
    }

    let mut child = pair.slave.spawn_command(cmd).context("spawn shell failed")?;
    drop(pair.slave);

    let _ = tx.start_send((
        id,
        Event::Started {
            pid: child.process_id(),
        },
    ));

    for bytes in stashed {
        let _ = writer.write_all(&bytes);
    }
    let _ = writer.flush();
    std::thread::spawn(move || {
        while let Ok(msg) = in_rx.recv() {
            match msg {
                Msg::Write(bytes) => {
                    if writer.write_all(&bytes).is_err() || writer.flush().is_err() {
                        break;
                    }
                }
                Msg::Resize { rows, cols } => {
                    let _ = master.resize(PtySize {
                        rows,
                        cols,
                        pixel_width: 0,
                        pixel_height: 0,
                    });
                }
            }
        }
    });

    // Output side, in two stages. A PTY hands back whatever is available, which
    // during a flood measured ~138 bytes per read against an 8 KiB buffer. One
    // iced message per read meant thousands of `update` + redraw cycles per
    // second for a few MiB, and since parsing costs only ~3µs the runtime
    // overhead — not the VT parser — was the whole bottleneck.
    //
    // So: a raw reader thread feeds a plain channel, and a coalescer drains
    // everything currently queued into a single message. While the UI is busy
    // rendering a frame, reads pile up and collapse into one batch, which turns
    // "one message per read" into roughly "one message per frame".
    let (raw_tx, raw_rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if raw_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // Caps a single batch so one message can't grow without bound if the UI
    // stalls. The channel behind it is still unbounded — real back-pressure is
    // a later problem, not a spike problem.
    const MAX_BATCH: usize = 256 * 1024;
    loop {
        let Ok(first) = raw_rx.recv() else { break };
        let mut batch = first;
        while batch.len() < MAX_BATCH {
            match raw_rx.try_recv() {
                Ok(more) => batch.extend_from_slice(&more),
                Err(_) => break,
            }
        }
        if tx.start_send((id, Event::Output(batch))).is_err() {
            break;
        }
    }

    // Kill before waiting. The loop above also exits when the *subscription* is
    // dropped — i.e. the user closed the tab — and in that case the shell is still
    // alive; without this it would keep running with nothing reading it, and
    // `wait` would block this thread forever. When the shell exited on its own,
    // `kill` is a harmless no-op.
    let _ = child.kill();
    let _ = child.wait();
    let _ = tx.start_send((id, Event::Exited));
    Ok(())
}
