use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

pub const SCROLLBACK: usize = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneFocus {
    Editor,
    Bottom,
    Right,
}

pub enum ShellMsg {
    Bytes { id: u64, data: Vec<u8> },
    Exited { id: u64 },
}

pub struct Shell {
    pub id: u64,
    pub cwd: PathBuf,
    pub label: String,
    pub parser: vt100::Parser,
    pub size: (u16, u16),
    pub alive: bool,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    reader_alive: Arc<AtomicBool>,
}

impl Shell {
    pub fn write(&mut self, bytes: &[u8]) {
        if !self.alive {
            return;
        }
        let _ = self.writer.write_all(bytes);
        let _ = self.writer.flush();
    }

    pub fn resize(&mut self, rows: u16, cols: u16) {
        if (rows, cols) == self.size || rows == 0 || cols == 0 {
            return;
        }
        let _ = self.master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        self.parser.screen_mut().set_size(rows, cols);
        self.size = (rows, cols);
    }

    pub fn kill(&mut self) {
        self.reader_alive.store(false, Ordering::SeqCst);
        let _ = self.child.kill();
    }

    pub fn pid(&self) -> Option<u32> {
        self.child.process_id()
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        self.kill();
    }
}

pub struct ShellPane {
    pub shells: Vec<Shell>,
    pub active: usize,
    pub tabs_scroll: usize,
}

impl ShellPane {
    pub fn new() -> Self {
        Self {
            shells: Vec::new(),
            active: 0,
            tabs_scroll: 0,
        }
    }

    pub fn active_shell(&self) -> Option<&Shell> {
        self.shells.get(self.active)
    }

    pub fn active_shell_mut(&mut self) -> Option<&mut Shell> {
        self.shells.get_mut(self.active)
    }
}

pub fn spawn_shell(
    id: u64,
    cwd: &Path,
    rows: u16,
    cols: u16,
    tx: Sender<ShellMsg>,
) -> Result<Shell> {
    let pty = native_pty_system();
    let pair = pty
        .openpty(PtySize {
            rows: rows.max(1),
            cols: cols.max(1),
            pixel_width: 0,
            pixel_height: 0,
        })
        .context("openpty failed")?;

    let shell_cmd = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = CommandBuilder::new(&shell_cmd);
    if cwd.is_dir() {
        cmd.cwd(cwd);
    }
    // Hint common TUIs to use 16-colour reasoning where possible; we only
    // render the ANSI 16 palette ratatui exposes anyway.
    cmd.env("TERM", "xterm-256color");

    let child = pair
        .slave
        .spawn_command(cmd)
        .context("spawn shell failed")?;

    let mut reader = pair
        .master
        .try_clone_reader()
        .context("clone pty reader failed")?;
    let writer = pair
        .master
        .take_writer()
        .context("take pty writer failed")?;

    let reader_alive = Arc::new(AtomicBool::new(true));
    let reader_alive_t = reader_alive.clone();
    let tx_t = tx.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 4096];
        while reader_alive_t.load(Ordering::SeqCst) {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx_t
                        .send(ShellMsg::Bytes {
                            id,
                            data: buf[..n].to_vec(),
                        })
                        .is_err()
                    {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx_t.send(ShellMsg::Exited { id });
    });

    let label = derive_label(cwd);
    let parser = vt100::Parser::new(rows.max(1), cols.max(1), SCROLLBACK);

    Ok(Shell {
        id,
        cwd: cwd.to_path_buf(),
        label,
        parser,
        size: (rows.max(1), cols.max(1)),
        alive: true,
        master: pair.master,
        writer,
        child,
        reader_alive,
    })
}

pub fn derive_label(cwd: &Path) -> String {
    cwd.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| {
            if cwd == Path::new("/") {
                "/".to_string()
            } else {
                cwd.display().to_string()
            }
        })
}

/// Translate a crossterm KeyEvent into the byte sequence a shell in a PTY
/// expects. Covers the common cases; exotic keys fall through to an empty
/// Vec which just drops the event.
pub fn key_to_bytes(key: KeyEvent, apply_shift: fn(char) -> char) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);

    let mut out: Vec<u8> = Vec::new();
    if alt {
        out.push(0x1b);
    }

    match key.code {
        KeyCode::Char(c) => {
            if ctrl {
                let byte = match c {
                    ' ' => 0x00,
                    'a'..='z' => (c as u8) - b'a' + 1,
                    'A'..='Z' => (c as u8) - b'A' + 1,
                    '[' | '{' => 0x1b,
                    '\\' | '|' => 0x1c,
                    ']' | '}' => 0x1d,
                    '^' => 0x1e,
                    '_' | '?' => 0x1f,
                    '/' => 0x1f,
                    _ => return Vec::new(),
                };
                out.push(byte);
                return out;
            }
            let ch = if shift { apply_shift(c) } else { c };
            let mut buf = [0u8; 4];
            out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            out
        }
        KeyCode::Enter => {
            out.push(b'\r');
            out
        }
        KeyCode::Tab => {
            out.push(b'\t');
            out
        }
        KeyCode::BackTab => {
            out.extend_from_slice(b"\x1b[Z");
            out
        }
        KeyCode::Backspace => {
            out.push(0x7f);
            out
        }
        KeyCode::Esc => {
            out.push(0x1b);
            out
        }
        KeyCode::Up => arrow(&mut out, b'A', shift, alt, ctrl),
        KeyCode::Down => arrow(&mut out, b'B', shift, alt, ctrl),
        KeyCode::Right => arrow(&mut out, b'C', shift, alt, ctrl),
        KeyCode::Left => arrow(&mut out, b'D', shift, alt, ctrl),
        KeyCode::Home => {
            out.extend_from_slice(b"\x1b[H");
            out
        }
        KeyCode::End => {
            out.extend_from_slice(b"\x1b[F");
            out
        }
        KeyCode::PageUp => {
            out.extend_from_slice(b"\x1b[5~");
            out
        }
        KeyCode::PageDown => {
            out.extend_from_slice(b"\x1b[6~");
            out
        }
        KeyCode::Delete => {
            out.extend_from_slice(b"\x1b[3~");
            out
        }
        KeyCode::Insert => {
            out.extend_from_slice(b"\x1b[2~");
            out
        }
        KeyCode::F(n) => {
            let seq: &[u8] = match n {
                1 => b"\x1bOP",
                2 => b"\x1bOQ",
                3 => b"\x1bOR",
                4 => b"\x1bOS",
                5 => b"\x1b[15~",
                6 => b"\x1b[17~",
                7 => b"\x1b[18~",
                8 => b"\x1b[19~",
                9 => b"\x1b[20~",
                10 => b"\x1b[21~",
                11 => b"\x1b[23~",
                12 => b"\x1b[24~",
                _ => b"",
            };
            out.extend_from_slice(seq);
            out
        }
        _ => Vec::new(),
    }
}

fn arrow(out: &mut Vec<u8>, letter: u8, shift: bool, alt: bool, ctrl: bool) -> Vec<u8> {
    let modflag = 1
        + if shift { 1 } else { 0 }
        + if alt { 2 } else { 0 }
        + if ctrl { 4 } else { 0 };
    if modflag == 1 {
        out.extend_from_slice(&[0x1b, b'[', letter]);
    } else {
        out.extend_from_slice(b"\x1b[1;");
        out.extend_from_slice(modflag.to_string().as_bytes());
        out.push(letter);
    }
    std::mem::take(out)
}

/// Encode a crossterm mouse event as an SGR mouse report relative to the
/// shell pane body origin.
pub fn mouse_to_bytes(ev: MouseEvent, origin: (u16, u16)) -> Option<Vec<u8>> {
    let col = ev.column.saturating_sub(origin.0) + 1;
    let row = ev.row.saturating_sub(origin.1) + 1;

    let (cb, final_byte): (u32, u8) = match ev.kind {
        MouseEventKind::Down(MouseButton::Left) => (0, b'M'),
        MouseEventKind::Down(MouseButton::Middle) => (1, b'M'),
        MouseEventKind::Down(MouseButton::Right) => (2, b'M'),
        MouseEventKind::Up(MouseButton::Left) => (0, b'm'),
        MouseEventKind::Up(MouseButton::Middle) => (1, b'm'),
        MouseEventKind::Up(MouseButton::Right) => (2, b'm'),
        MouseEventKind::Drag(MouseButton::Left) => (32, b'M'),
        MouseEventKind::Drag(MouseButton::Middle) => (33, b'M'),
        MouseEventKind::Drag(MouseButton::Right) => (34, b'M'),
        MouseEventKind::ScrollUp => (64, b'M'),
        MouseEventKind::ScrollDown => (65, b'M'),
        MouseEventKind::Moved => return None,
        _ => return None,
    };

    let mut modified = cb;
    if ev.modifiers.contains(KeyModifiers::SHIFT) {
        modified |= 4;
    }
    if ev.modifiers.contains(KeyModifiers::ALT) {
        modified |= 8;
    }
    if ev.modifiers.contains(KeyModifiers::CONTROL) {
        modified |= 16;
    }

    Some(format!("\x1b[<{};{};{}{}", modified, col, row, final_byte as char).into_bytes())
}

/// Scan the stream for an OSC 7 cwd sequence; returns the latest cwd
/// found (if any). Leaves the bytes themselves unmodified.
pub fn extract_osc7_cwd(bytes: &[u8]) -> Option<PathBuf> {
    const MARKER: &[u8] = b"\x1b]7;";
    let mut result: Option<PathBuf> = None;
    let mut i = 0;
    while i + MARKER.len() < bytes.len() {
        if &bytes[i..i + MARKER.len()] == MARKER {
            let start = i + MARKER.len();
            // Find ST: \x1b\\ or \x07 (BEL).
            let mut end = start;
            while end < bytes.len() {
                if bytes[end] == 0x07 {
                    break;
                }
                if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                    break;
                }
                end += 1;
            }
            if end <= bytes.len() {
                let body = &bytes[start..end];
                if let Ok(s) = std::str::from_utf8(body) {
                    if let Some(path) = parse_file_url(s) {
                        result = Some(path);
                    }
                }
            }
            i = end + 1;
        } else {
            i += 1;
        }
    }
    result
}

/// Query the current working directory of a running process. Used to
/// update shell tab labels as the user `cd`s around inside the shell —
/// works regardless of OSC 7 support.
#[cfg(target_os = "macos")]
pub fn query_process_cwd(pid: u32) -> Option<PathBuf> {
    use std::os::raw::{c_int, c_void};
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: c_int,
            flavor: c_int,
            arg: u64,
            buffer: *mut c_void,
            buffersize: c_int,
        ) -> c_int;
    }
    const PROC_PIDVNODEPATHINFO: c_int = 9;
    const BUF_SIZE: usize = 2352; // sizeof(struct proc_vnodepathinfo)
    const CWD_PATH_OFFSET: usize = 152; // offset of pvi_cdir.vip_path
    const MAXPATHLEN: usize = 1024;

    let mut buf = vec![0u8; BUF_SIZE];
    let ret = unsafe {
        proc_pidinfo(
            pid as c_int,
            PROC_PIDVNODEPATHINFO,
            0,
            buf.as_mut_ptr() as *mut c_void,
            BUF_SIZE as c_int,
        )
    };
    if ret <= 0 {
        return None;
    }
    let slice = &buf[CWD_PATH_OFFSET..CWD_PATH_OFFSET + MAXPATHLEN];
    let nul = slice.iter().position(|&b| b == 0)?;
    let s = std::str::from_utf8(&slice[..nul]).ok()?;
    if s.is_empty() {
        None
    } else {
        Some(PathBuf::from(s))
    }
}

#[cfg(target_os = "linux")]
pub fn query_process_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{}/cwd", pid)).ok()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn query_process_cwd(_pid: u32) -> Option<PathBuf> {
    None
}

fn parse_file_url(s: &str) -> Option<PathBuf> {
    let rest = s.strip_prefix("file://")?;
    let path_start = rest.find('/').unwrap_or(0);
    let encoded = &rest[path_start..];
    let mut out = String::with_capacity(encoded.len());
    let bytes = encoded.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok()?;
            if let Ok(b) = u8::from_str_radix(hex, 16) {
                out.push(b as char);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    Some(PathBuf::from(out))
}
