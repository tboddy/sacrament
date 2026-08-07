//! Single-instance IPC: a Unix socket that hands `OPEN` requests to the running
//! editor.
//!
//! This is what makes `sacrament2 foo.rs` from any terminal join the live window
//! as a new tab instead of starting a second editor. The protocol itself lives in
//! `core::protocol` and is shared with v1 — only the plumbing differs, because v1
//! owns its event loop and v2 doesn't.
//!
//! **The listener is bound in `main`, not here.** Whether this process is the
//! server has to be decided *before* the window opens: a client run must be able
//! to send its request and exit without ever starting iced. So `main` binds and
//! calls [`attach`], and the subscription later collects the receiving end.
//!
//! `Subscription::run` takes a bare `fn() -> Stream` with no captures, which is
//! the same constraint `pty::stream` works around. Here the value is handed
//! *in* rather than out, through a `OnceLock`.
//!
//! The socket file is deliberately not cleaned up on exit. It can't be: on macOS
//! `Cmd+Q` terminates the process without unwinding, so a `Drop` guard would be
//! skipped anyway. `client::try_send_open` already treats a socket that refuses
//! connection as stale and removes it, which covers both that and a crash.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use iced::futures::channel::mpsc as async_mpsc;

use sacrament_core::protocol::{Request, Response};

/// A request from another invocation, plus the channel its reply goes back on.
///
/// The reply is threaded through rather than answered immediately in the
/// listener thread so the client learns whether the file actually opened — a
/// path that can't be read should be an error at the shell that asked, not a
/// silent no-op in a window that may not even be visible.
#[derive(Debug, Clone)]
pub enum Command {
    Open {
        path: PathBuf,
        line: Option<usize>,
        syntax: Option<String>,
        review: bool,
        reply: Sender<Response>,
    },
}

/// How long a client waits for the editor to answer before giving up. The editor
/// replies from `update`, so this only elapses if the UI thread is wedged.
const REPLY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

type Rx = async_mpsc::UnboundedReceiver<Command>;

/// Set by [`attach`], taken by [`stream`]. A `OnceLock` rather than a parameter
/// because the subscription's stream builder can't capture.
static INBOX: OnceLock<Mutex<Option<Rx>>> = OnceLock::new();

/// Start serving `listener` on a background thread.
///
/// Called from `main` once this process has established it's the server, so no
/// request can arrive before there's somewhere to put it.
pub fn attach(listener: UnixListener) {
    let (tx, rx) = async_mpsc::unbounded::<Command>();
    let _ = INBOX.set(Mutex::new(Some(rx)));
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let tx = tx.clone();
            // One thread per client: `handle` blocks waiting for the editor's
            // reply, and a slow or wedged reply must not stall the next client.
            std::thread::spawn(move || handle(stream, tx));
        }
    });
}

/// The subscription's stream. Yields every request that arrives.
///
/// Returns the receiver concretely rather than `impl Stream` for the same reason
/// `pty::stream` does — `run` wants one nameable type.
///
/// Only the first call gets the real receiver. A second would mean iced rebuilt a
/// subscription we always keep in the batch, which doesn't happen; it yields a
/// dead stream rather than panicking, since losing IPC is better than losing the
/// editor.
pub fn stream() -> Rx {
    INBOX
        .get()
        .and_then(|cell| cell.lock().ok().and_then(|mut slot| slot.take()))
        .unwrap_or_else(|| async_mpsc::unbounded().1)
}

fn handle(mut stream: UnixStream, mut tx: async_mpsc::UnboundedSender<Command>) {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        if reader.read_line(&mut line).is_err() {
            return;
        }
    }

    let response = match Request::parse(&line) {
        Some(Request::Open {
            path,
            line,
            syntax,
            review,
        }) => {
            let (reply, replies) = std::sync::mpsc::channel();
            if tx
                .start_send(Command::Open {
                    path,
                    line,
                    syntax,
                    review,
                    reply,
                })
                .is_err()
            {
                Response::Err("editor is gone".into())
            } else {
                replies
                    .recv_timeout(REPLY_TIMEOUT)
                    .unwrap_or_else(|_| Response::Err("editor timeout".into()))
            }
        }
        None => Response::Err("unknown request".into()),
    };
    let _ = stream.write_all(response.encode().as_bytes());
}
