use std::env;
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/sacrament-{user}.sock"))
}

pub enum Request {
    Open {
        path: PathBuf,
        line: Option<usize>,
        syntax: Option<String>,
    },
}

impl Request {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches('\n');
        let (cmd, rest) = line.split_once(' ')?;
        match cmd {
            "OPEN" => {
                let mut parts = rest.split('\t');
                let path_str = parts.next()?;
                let line_field = parts.next().unwrap_or("");
                let syntax_field = parts.next().unwrap_or("");
                let line_num = if line_field.is_empty() {
                    None
                } else {
                    line_field.parse::<usize>().ok()
                };
                let syntax = if syntax_field.is_empty() {
                    None
                } else {
                    Some(syntax_field.to_string())
                };
                Some(Request::Open {
                    path: PathBuf::from(path_str),
                    line: line_num,
                    syntax,
                })
            }
            _ => None,
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Request::Open {
                path,
                line,
                syntax,
            } => {
                let line_field = line.map(|n| n.to_string()).unwrap_or_default();
                match syntax {
                    Some(s) => format!("OPEN {}\t{}\t{}\n", path.display(), line_field, s),
                    None if line.is_some() => format!("OPEN {}\t{}\n", path.display(), line_field),
                    None => format!("OPEN {}\n", path.display()),
                }
            }
        }
    }
}

pub enum Response {
    Ok,
    Err(String),
}

impl Response {
    pub fn parse(line: &str) -> Self {
        let line = line.trim_end_matches('\n');
        if line == "ok" {
            Response::Ok
        } else if let Some(msg) = line.strip_prefix("err ") {
            Response::Err(msg.to_string())
        } else {
            Response::Err(format!("malformed response: {line}"))
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Response::Ok => "ok\n".to_string(),
            Response::Err(m) => format!("err {m}\n"),
        }
    }
}
