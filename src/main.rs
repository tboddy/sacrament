mod client;
mod config;
mod editor;
mod highlight;
mod protocol;
mod server;
mod session;

use std::path::{Path, PathBuf};
use std::process;

use anyhow::Result;

fn main() -> Result<()> {
    let raw_arg = std::env::args().nth(1);
    let target = raw_arg.as_deref().map(parse_file_line);

    if let Some((path, line)) = &target {
        match client::try_send_open(path, *line) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => {
                eprintln!("te: {e}");
                process::exit(1);
            }
        }
    }

    let cfg = config::load();
    server::run(target, cfg)
}

fn parse_file_line(arg: &str) -> (PathBuf, Option<usize>) {
    if let Some((head, tail)) = arg.rsplit_once(':') {
        if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(n) = tail.parse::<usize>() {
                let p = PathBuf::from(head);
                if !Path::new(head).exists() && Path::new(arg).exists() {
                    return (PathBuf::from(arg), None);
                }
                return (p, Some(n));
            }
        }
    }
    (PathBuf::from(arg), None)
}
