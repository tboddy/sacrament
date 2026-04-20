use std::env;
use std::path::PathBuf;

pub fn socket_path() -> PathBuf {
    let user = env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    PathBuf::from(format!("/tmp/sacrament-{user}.sock"))
}

pub enum Request {
    Open { path: PathBuf, line: Option<usize> },
}

impl Request {
    pub fn parse(line: &str) -> Option<Self> {
        let line = line.trim_end_matches('\n');
        let (cmd, rest) = line.split_once(' ')?;
        match cmd {
            "OPEN" => {
                let (path_str, line_num) = match rest.split_once('\t') {
                    Some((p, n)) => (p, n.parse::<usize>().ok()),
                    None => (rest, None),
                };
                Some(Request::Open {
                    path: PathBuf::from(path_str),
                    line: line_num,
                })
            }
            _ => None,
        }
    }

    pub fn encode(&self) -> String {
        match self {
            Request::Open { path, line: Some(n) } => {
                format!("OPEN {}\t{}\n", path.display(), n)
            }
            Request::Open { path, line: None } => format!("OPEN {}\n", path.display()),
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
