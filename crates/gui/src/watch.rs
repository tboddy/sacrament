//! Watching open files for changes made outside the editor.
//!
//! This is what makes an agent-edited file refresh on screen instead of going
//! quietly stale — the same job v1 gives its `notify` watcher. Without it a
//! buffer can drift from the file it claims to show, and `Buffer::save`'s
//! changed-on-disk guard then refuses to write, which leaves the editor stuck
//! with no way out.
//!
//! **The watch set is declared, not managed.** Callers hand over the current list
//! of open paths and the watcher thread diffs it against what it's already
//! watching. That mirrors how the PTY subscriptions work — the live set is
//! derived from state rather than maintained by paired add/remove calls, so
//! there's no way to leak a watch by forgetting one half.
//!
//! Plumbing matches `ipc`: the thread is started once and the receiving end is
//! collected later by the subscription, because `Subscription::run` takes a bare
//! `fn() -> Stream` that can't capture.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};

use iced::futures::channel::mpsc as async_mpsc;
use notify::{RecursiveMode, Watcher};

type Rx = async_mpsc::UnboundedReceiver<PathBuf>;

static INBOX: OnceLock<Mutex<Option<Rx>>> = OnceLock::new();
static COMMANDS: OnceLock<Sender<Vec<PathBuf>>> = OnceLock::new();

/// Start the watcher thread. Called once, before the window exists.
pub fn start() {
    let (events_tx, events_rx) = async_mpsc::unbounded::<PathBuf>();
    let (cmd_tx, cmd_rx) = mpsc::channel::<Vec<PathBuf>>();
    let _ = INBOX.set(Mutex::new(Some(events_rx)));
    let _ = COMMANDS.set(cmd_tx);

    std::thread::spawn(move || {
        let mut tx = events_tx;
        // The callback runs on notify's own thread. It forwards paths and does
        // no filtering beyond the event kind — deciding whether a path matters
        // needs the buffer list, which lives in the app.
        let watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            let Ok(event) = res else { return };
            if matches!(
                event.kind,
                notify::EventKind::Modify(_)
                    | notify::EventKind::Create(_)
                    | notify::EventKind::Remove(_)
            ) {
                for path in event.paths {
                    let _ = tx.start_send(path);
                }
            }
        });
        let Ok(mut watcher) = watcher else { return };

        let mut watched: HashSet<PathBuf> = HashSet::new();
        while let Ok(wanted) = cmd_rx.recv() {
            let wanted: HashSet<PathBuf> = wanted.into_iter().collect();
            for path in wanted.difference(&watched) {
                // Watch the file itself, non-recursively, as v1 does. On macOS
                // the backend is FSEvents, which tracks a *path* rather than an
                // inode, so this survives the write-temp-then-rename that both
                // our own `save` and most other tools use.
                let _ = watcher.watch(path, RecursiveMode::NonRecursive);
            }
            for path in watched.difference(&wanted) {
                let _ = watcher.unwatch(path);
            }
            watched = wanted;
        }
    });
}

/// Declare the full set of paths to watch. Anything absent is unwatched.
pub fn sync(paths: Vec<PathBuf>) {
    if let Some(tx) = COMMANDS.get() {
        let _ = tx.send(paths);
    }
}

/// The subscription's stream of changed paths.
///
/// Only the first call gets the real receiver; a second yields a dead stream
/// rather than panicking, on the same reasoning as `ipc::stream`.
pub fn stream() -> Rx {
    INBOX
        .get()
        .and_then(|cell| cell.lock().ok().and_then(|mut slot| slot.take()))
        .unwrap_or_else(|| async_mpsc::unbounded().1)
}
