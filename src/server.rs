use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use crate::config::Config;
use crate::editor::Editor;
use crate::protocol::{Request, Response, socket_path};
use crate::session;

pub struct InitialOpen {
    pub path: PathBuf,
    pub line: Option<usize>,
    pub syntax: Option<String>,
}

pub enum RemoteCommand {
    Open {
        path: PathBuf,
        line: Option<usize>,
        syntax: Option<String>,
        reply: Sender<Response>,
    },
}

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub fn run(initial: Option<InitialOpen>, config: Config) -> Result<()> {
    let sock_path = socket_path();
    if sock_path.exists() {
        let _ = fs::remove_file(&sock_path);
    }
    let listener =
        UnixListener::bind(&sock_path).with_context(|| format!("bind {}", sock_path.display()))?;
    let _guard = SocketGuard(sock_path.clone());

    let (tx, rx) = mpsc::channel::<RemoteCommand>();
    spawn_listener_thread(listener, tx);

    let mut editor = Editor::new(config);
    if let Some(open) = initial {
        if let Err(e) = editor.load(&open.path, open.syntax.as_deref()) {
            editor.set_status(format!("load failed: {e}"));
        } else if let Some(n) = open.line {
            editor.goto_line(n);
        }
    } else if let Some(sess) = session::load() {
        editor.restore_session(sess);
    }

    let result = run_ui(&mut editor, rx);

    let snapshot = editor.capture_session();
    let _ = session::save(&snapshot);

    result
}

fn spawn_listener_thread(listener: UnixListener, tx: Sender<RemoteCommand>) {
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(stream) = conn else { continue };
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = handle_client(stream, tx);
            });
        }
    });
}

fn handle_client(mut stream: UnixStream, tx: Sender<RemoteCommand>) -> Result<()> {
    let mut line = String::new();
    {
        let mut reader = BufReader::new(&stream);
        reader.read_line(&mut line)?;
    }

    let response = match Request::parse(&line) {
        Some(Request::Open { path, line, syntax }) => {
            let (reply_tx, reply_rx) = mpsc::channel();
            tx.send(RemoteCommand::Open {
                path,
                line,
                syntax,
                reply: reply_tx,
            })
            .ok();
            reply_rx
                .recv_timeout(Duration::from_secs(2))
                .unwrap_or_else(|_| Response::Err("editor timeout".into()))
        }
        None => Response::Err("unknown request".into()),
    };

    stream.write_all(response.encode().as_bytes())?;
    Ok(())
}

fn run_ui(editor: &mut Editor, rx: Receiver<RemoteCommand>) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    execute!(
        stdout,
        EnterAlternateScreen,
        EnableMouseCapture,
        EnableBracketedPaste
    )?;
    let pushed_flags = execute!(
        stdout,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALL_KEYS_AS_ESCAPE_CODES,
        )
    )
    .is_ok();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let result = event_loop(&mut terminal, editor, &rx);

    if pushed_flags {
        let _ = execute!(terminal.backend_mut(), PopKeyboardEnhancementFlags);
    }
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )?;
    terminal.show_cursor()?;

    result
}

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    editor: &mut Editor,
    rx: &Receiver<RemoteCommand>,
) -> Result<()> {
    loop {
        terminal.draw(|f| editor.render(f))?;

        // Drain PTY output from background reader threads; vt100 parsers
        // update and OSC 7 cwd updates are applied in apply_shell_output.
        editor.drain_shell_output();

        if event::poll(Duration::from_millis(20))? {
            loop {
                let ev = event::read()?;
                if let Event::Key(key) = &ev {
                    if key.kind == KeyEventKind::Press
                        && key.code == KeyCode::Esc
                        && key.modifiers.is_empty()
                        && event::poll(Duration::from_millis(0))?
                    {
                        let peek = event::read()?;
                        if is_csi_intro(&peek) {
                            drain_escape_tail()?;
                        } else {
                            editor.handle_key(*key);
                            dispatch_event(editor, peek);
                        }
                        if !event::poll(Duration::from_millis(0))? {
                            break;
                        }
                        continue;
                    }
                }
                dispatch_event(editor, ev);
                if !event::poll(Duration::from_millis(0))? {
                    break;
                }
            }
        }

        while let Ok(cmd) = rx.try_recv() {
            apply_remote(editor, cmd);
        }

        if editor.should_quit {
            return Ok(());
        }
    }
}

fn dispatch_event(editor: &mut Editor, ev: Event) {
    match ev {
        Event::Key(key) => {
            if key.kind == KeyEventKind::Press {
                editor.handle_key(key);
            }
        }
        Event::Mouse(m) => editor.handle_mouse(m),
        Event::Paste(text) => editor.handle_paste(text),
        _ => {}
    }
}

fn is_csi_intro(ev: &Event) -> bool {
    if let Event::Key(k) = ev {
        if k.kind == KeyEventKind::Press && k.modifiers.is_empty() {
            return matches!(k.code, KeyCode::Char('[') | KeyCode::Char('O'));
        }
    }
    false
}

// Swallow the tail of a leaked CSI/SS3 escape sequence: consume chars
// until we reach a final byte (ASCII letter or `~`). Bounded so we can't
// spin on pathological input.
fn drain_escape_tail() -> Result<()> {
    for _ in 0..64 {
        if !event::poll(Duration::from_millis(0))? {
            return Ok(());
        }
        if let Event::Key(k) = event::read()? {
            if let KeyCode::Char(c) = k.code {
                if c.is_ascii_alphabetic() || c == '~' {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

fn apply_remote(editor: &mut Editor, cmd: RemoteCommand) {
    match cmd {
        RemoteCommand::Open {
            path,
            line,
            syntax,
            reply,
        } => {
            let response = match editor.try_load_remote(&path, syntax.as_deref()) {
                Ok(()) => {
                    if let Some(n) = line {
                        editor.goto_line(n);
                    }
                    Response::Ok
                }
                Err(e) => Response::Err(e.to_string()),
            };
            let _ = reply.send(response);
        }
    }
}
