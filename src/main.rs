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

use crate::server::InitialOpen;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let parsed = match parse_args(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("sacrament: {e}");
            process::exit(2);
        }
    };

    let target = parsed
        .file
        .as_deref()
        .map(parse_file_line)
        .map(|(path, line)| InitialOpen {
            path,
            line,
            syntax: parsed.syntax.clone(),
        });

    if let Some(open) = &target {
        if !open.path.exists() {
            if let Err(e) = std::fs::File::create(&open.path) {
                eprintln!("sacrament: cannot create {}: {e}", open.path.display());
                process::exit(1);
            }
        }
        match client::try_send_open(&open.path, open.line, open.syntax.as_deref()) {
            Ok(true) => return Ok(()),
            Ok(false) => {}
            Err(e) => {
                eprintln!("sacrament: {e}");
                process::exit(1);
            }
        }
    }

    let cfg = config::load();
    server::run(target, cfg)
}

struct ParsedArgs {
    file: Option<String>,
    syntax: Option<String>,
}

fn parse_args(args: &[String]) -> Result<ParsedArgs, String> {
    let mut file: Option<String> = None;
    let mut syntax: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        match a.as_str() {
            "-s" | "--s" | "--syntax" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| format!("{a} requires a value"))?;
                syntax = Some(v.clone());
                i += 2;
            }
            s if s.starts_with("--syntax=") => {
                syntax = Some(s["--syntax=".len()..].to_string());
                i += 1;
            }
            _ if file.is_none() => {
                file = Some(a.clone());
                i += 1;
            }
            _ => return Err(format!("unexpected argument: {a}")),
        }
    }
    Ok(ParsedArgs { file, syntax })
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
