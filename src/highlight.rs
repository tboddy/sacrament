use std::path::Path;

use ratatui::style::{Color, Modifier};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

#[derive(Clone)]
pub struct HlSpan {
    pub color: Option<Color>,
    pub modifier: Modifier,
    pub byte_start: usize,
    pub byte_end: usize,
}

#[derive(Clone)]
pub struct LineState {
    pub parse: ParseState,
    pub scopes: ScopeStack,
}

pub struct Highlighter {
    syntax_set: SyntaxSet,
}

impl Highlighter {
    pub fn new() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
        }
    }

    pub fn syntax_for_path<'a>(&'a self, path: &Path) -> Option<&'a SyntaxReference> {
        let ext = path.extension().and_then(|e| e.to_str());
        if let Some(ext) = ext {
            // Extensions not in syntect's defaults — alias to a close match.
            let canonical = match ext {
                "vue" => "html",
                other => other,
            };
            if let Some(s) = self.syntax_set.find_syntax_by_extension(canonical) {
                return Some(s);
            }
        }
        let name = path.file_name().and_then(|n| n.to_str())?;
        self.syntax_set.find_syntax_by_extension(name)
    }

    pub fn syntax_by_name<'a>(&'a self, token: &str) -> Option<&'a SyntaxReference> {
        self.syntax_set
            .find_syntax_by_name(token)
            .or_else(|| self.syntax_set.find_syntax_by_token(token))
    }

    pub fn initial_state(&self, syntax: &SyntaxReference) -> LineState {
        LineState {
            parse: ParseState::new(syntax),
            scopes: ScopeStack::new(),
        }
    }

    pub fn highlight_line(&self, line: &str, state: &mut LineState) -> Vec<HlSpan> {
        let mut with_nl = String::with_capacity(line.len() + 1);
        with_nl.push_str(line);
        with_nl.push('\n');

        let ops = state
            .parse
            .parse_line(&with_nl, &self.syntax_set)
            .unwrap_or_default();

        let actual_len = line.len();
        let mut spans: Vec<HlSpan> = Vec::new();
        let mut last_byte = 0usize;

        for (byte_idx, op) in &ops {
            let clamped = (*byte_idx).min(actual_len);
            if clamped > last_byte {
                let (color, modifier) = style_for(&state.scopes);
                spans.push(HlSpan {
                    color,
                    modifier,
                    byte_start: last_byte,
                    byte_end: clamped,
                });
                last_byte = clamped;
            }
            state.scopes.apply(op).ok();
        }

        if last_byte < actual_len {
            let (color, modifier) = style_for(&state.scopes);
            spans.push(HlSpan {
                color,
                modifier,
                byte_start: last_byte,
                byte_end: actual_len,
            });
        }

        spans
    }
}

fn style_for(stack: &ScopeStack) -> (Option<Color>, Modifier) {
    for scope in stack.as_slice().iter().rev() {
        let name = format!("{}", scope);
        if starts_with_any(&name, &["comment"]) {
            return (Some(Color::DarkGray), Modifier::empty());
        }
        if starts_with_any(&name, &["string", "constant.character"]) {
            return (Some(Color::Green), Modifier::empty());
        }
        if starts_with_any(
            &name,
            &["constant.numeric", "constant.language", "constant.other"],
        ) {
            return (Some(Color::LightMagenta), Modifier::empty());
        }
        if starts_with_any(&name, &["variable.language"]) {
            return (Some(Color::Magenta), Modifier::empty());
        }
        if starts_with_any(&name, &["keyword.operator"]) {
            return (Some(Color::Cyan), Modifier::empty());
        }
        if starts_with_any(&name, &["keyword"]) {
            return (Some(Color::Red), Modifier::empty());
        }
        if starts_with_any(&name, &["storage"]) {
            return (Some(Color::Red), Modifier::empty());
        }
        if starts_with_any(&name, &["punctuation.definition.directive"]) {
            return (Some(Color::Magenta), Modifier::empty());
        }
        if starts_with_any(
            &name,
            &["entity.name.constant", "entity.name.preprocessor"],
        ) {
            return (Some(Color::LightYellow), Modifier::empty());
        }
        if starts_with_any(&name, &["entity.name.function"]) {
            return (Some(Color::Green), Modifier::empty());
        }
        if starts_with_any(
            &name,
            &[
                "support.function",
                "variable.function",
                "meta.function-call.identifier",
            ],
        ) {
            return (Some(Color::Cyan), Modifier::empty());
        }
        if starts_with_any(
            &name,
            &[
                "variable.other.member",
                "variable.other.property",
                "meta.property.object",
                "entity.name.tag",
            ],
        ) {
            return (Some(Color::LightBlue), Modifier::empty());
        }
        if starts_with_any(
            &name,
            &["entity.name.type", "support.type", "support.class"],
        ) {
            return (Some(Color::LightCyan), Modifier::empty());
        }
        if starts_with_any(&name, &["entity.other.attribute-name"]) {
            return (Some(Color::LightCyan), Modifier::empty());
        }
        if starts_with_any(&name, &["variable.parameter"]) {
            return (Some(Color::LightYellow), Modifier::empty());
        }
        if starts_with_any(&name, &["variable.other.constant"]) {
            return (Some(Color::LightMagenta), Modifier::empty());
        }
        if starts_with_any(&name, &["markup.heading"]) {
            return (Some(Color::LightBlue), Modifier::empty());
        }
        if starts_with_any(&name, &["markup.bold"]) {
            return (None, Modifier::empty());
        }
        if starts_with_any(&name, &["markup.italic"]) {
            return (None, Modifier::ITALIC);
        }
        if starts_with_any(&name, &["markup.underline.link"]) {
            return (Some(Color::LightBlue), Modifier::UNDERLINED);
        }
        if starts_with_any(&name, &["invalid"]) {
            return (Some(Color::Red), Modifier::empty());
        }
    }
    (None, Modifier::empty())
}

fn starts_with_any(name: &str, prefixes: &[&str]) -> bool {
    for p in prefixes {
        if name.starts_with(p) {
            return true;
        }
    }
    false
}

/// Line-comment prefix for a given syntect syntax name. Returns the bare
/// prefix without trailing space — the caller decides whether to pad.
pub fn line_comment_for(syntax_name: &str) -> Option<&'static str> {
    match syntax_name {
        "Rust" | "C" | "C++" | "Java" | "JavaScript" | "JavaScript (Babel)"
        | "TypeScript" | "TypeScriptReact" | "JSX" | "Go" | "Swift" | "Kotlin"
        | "Scala" | "C#" | "Dart" | "Objective-C" | "Objective-C++" | "Zig"
        | "Groovy" | "Rust Enhanced" | "JSON with Comments" | "PHP"
        | "PHP Source" | "F#" | "OCaml" => Some("//"),
        "Python" | "Ruby" | "Shell-Unix-Generic" | "Bourne Again Shell (bash)"
        | "Bash" | "YAML" | "TOML" | "R" | "Perl" | "Makefile" | "CMake"
        | "Dockerfile" | "Nix" | "Elixir" | "Julia" | "Tcl" | "CoffeeScript"
        | "Crystal" | "Fish" | "GDScript" => Some("#"),
        "SQL" | "Haskell" | "Lua" | "Ada" | "Elm" | "PureScript" => Some("--"),
        "Clojure" | "Lisp" | "Scheme" | "Assembly"
        | "Assembly x86 (NASM)" | "INI" => Some(";"),
        "LaTeX" | "TeX" | "Erlang" | "MATLAB" | "Matlab" => Some("%"),
        "Visual Basic" | "VBScript" => Some("'"),
        _ => None,
    }
}
