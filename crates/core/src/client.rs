use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::paths::socket_path;
use crate::protocol::{Request, Response};

/// Try to hand an OPEN off to an already-running server for `app`
/// (`crate::APP_TUI` / `crate::APP_GUI`). `Ok(false)` means no server was
/// listening, so the caller should become the server itself.
pub fn try_send_open(
    app: &str,
    path: &Path,
    line: Option<usize>,
    syntax: Option<&str>,
    review: bool,
) -> Result<bool> {
    let sock = socket_path(app);
    if !sock.exists() {
        return Ok(false);
    }

    let mut stream = match UnixStream::connect(&sock) {
        Ok(s) => s,
        Err(_) => {
            let _ = fs::remove_file(&sock);
            return Ok(false);
        }
    };

    let abs: PathBuf = fs::canonicalize(path)
        .with_context(|| format!("cannot resolve path: {}", path.display()))?;

    let req = Request::Open {
        path: abs,
        line,
        syntax: syntax.map(|s| s.to_string()),
        review,
    };
    stream.write_all(req.encode().as_bytes())?;

    let mut reader = BufReader::new(&stream);
    let mut line_buf = String::new();
    reader.read_line(&mut line_buf)?;

    match Response::parse(&line_buf) {
        Response::Ok => Ok(true),
        Response::Err(m) => bail!("server: {m}"),
    }
}
