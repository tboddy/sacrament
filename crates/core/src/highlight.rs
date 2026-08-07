//! Syntax highlighting via `syntect`, with framework-independent colors.
//!
//! `syntect` is a parser, not a UI library, so this belongs in `core`. The only
//! thing that had to change moving it out of the tui crate was the color type:
//! it used to hand back `ratatui::style::Color` and `ratatui::style::Modifier`.
//!
//! ## Why colors are theme slots, not RGB
//!
//! v1's rule was that the terminal's 16-color palette *is* the theme, so
//! `style_for` maps TextMate scopes onto those 16 slots and never emits RGB.
//! That rule outlived the terminal: v2 has a real `[theme]` table with the same
//! 16 slots, so returning a [`Slot`] index means the user's theme drives syntax
//! highlighting too, for free. A frontend resolves `Slot` however it likes —
//! v1 to a named ANSI color, v2 through `theme::Theme::ansi()`.
//!
//! The constraint is deliberate. Sixteen colors is a small palette to design a
//! highlighter against, and that's the point: it stays coherent with whatever
//! terminal theme the user already chose.

use std::path::Path;

use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};

/// An index into the theme's 16 ANSI slots (0-7 normal, 8-15 bright).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Slot(pub u8);

impl Slot {
    pub const BLACK: Slot = Slot(0);
    pub const RED: Slot = Slot(1);
    pub const GREEN: Slot = Slot(2);
    pub const YELLOW: Slot = Slot(3);
    pub const BLUE: Slot = Slot(4);
    pub const MAGENTA: Slot = Slot(5);
    pub const CYAN: Slot = Slot(6);
    pub const WHITE: Slot = Slot(7);
    pub const BRIGHT_BLACK: Slot = Slot(8);
    pub const BRIGHT_RED: Slot = Slot(9);
    pub const BRIGHT_GREEN: Slot = Slot(10);
    pub const BRIGHT_YELLOW: Slot = Slot(11);
    pub const BRIGHT_BLUE: Slot = Slot(12);
    pub const BRIGHT_MAGENTA: Slot = Slot(13);
    pub const BRIGHT_CYAN: Slot = Slot(14);
    pub const BRIGHT_WHITE: Slot = Slot(15);

    /// Clamped into 0-15 so a `Slot` can always index a 16-element palette.
    pub fn index(self) -> usize {
        (self.0 as usize).min(15)
    }
}

/// Text emphasis, independent of any toolkit's modifier bitflags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Emphasis {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl Emphasis {
    pub const NONE: Emphasis = Emphasis {
        bold: false,
        italic: false,
        underline: false,
    };

    pub const fn italic() -> Emphasis {
        Emphasis {
            italic: true,
            ..Self::NONE
        }
    }

    pub const fn underline() -> Emphasis {
        Emphasis {
            underline: true,
            ..Self::NONE
        }
    }
}

/// A styled byte range within one line.
#[derive(Clone, Debug)]
pub struct HlSpan {
    pub color: Option<Slot>,
    pub emphasis: Emphasis,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Parser state *before* a given line. Highlighting is line-by-line and each
/// line's parse depends on the previous, which is what the per-buffer
/// `line_state_before` cache exists to avoid re-deriving.
#[derive(Clone)]
pub struct LineState {
    pub parse: ParseState,
    pub scopes: ScopeStack,
}

pub struct Highlighter {
    syntax_set: SyntaxSet,
}

impl Default for Highlighter {
    fn default() -> Self {
        Self::new()
    }
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
                "fab" => "rs",
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

        // Spans are clamped to the real line length: we appended a newline so
        // syntect's line-oriented grammars behave, but that byte isn't ours.
        let actual_len = line.len();
        let mut spans: Vec<HlSpan> = Vec::new();
        let mut last_byte = 0usize;

        for (byte_idx, op) in &ops {
            let clamped = (*byte_idx).min(actual_len);
            if clamped > last_byte {
                let (color, emphasis) = style_for(&state.scopes);
                spans.push(HlSpan {
                    color,
                    emphasis,
                    byte_start: last_byte,
                    byte_end: clamped,
                });
                last_byte = clamped;
            }
            state.scopes.apply(op).ok();
        }

        if last_byte < actual_len {
            let (color, emphasis) = style_for(&state.scopes);
            spans.push(HlSpan {
                color,
                emphasis,
                byte_start: last_byte,
                byte_end: actual_len,
            });
        }

        spans
    }
}

/// Map a TextMate scope stack to a theme slot. Innermost scope wins, so this
/// walks the stack in reverse and returns on the first match.
///
/// This is the whole color scheme — there is no other theme layer for syntax.
/// Edit here.
fn style_for(stack: &ScopeStack) -> (Option<Slot>, Emphasis) {
    for scope in stack.as_slice().iter().rev() {
        let name = format!("{scope}");
        if let Some(style) = slot_for_scope(&name) {
            return style;
        }
    }
    (None, Emphasis::NONE)
}

fn slot_for_scope(name: &str) -> Option<(Option<Slot>, Emphasis)> {
    // Ordered most-specific first within each family; `starts_with` means
    // "comment" also catches "comment.line.double-slash".
    const RULES: &[(&[&str], Slot)] = &[
        (&["comment"], Slot::BRIGHT_BLACK),
        (&["string", "constant.character"], Slot::GREEN),
        (
            &["constant.numeric", "constant.language", "constant.other"],
            Slot::BRIGHT_MAGENTA,
        ),
        (&["variable.language"], Slot::MAGENTA),
        (&["keyword.operator"], Slot::CYAN),
        (&["keyword"], Slot::RED),
        (&["storage"], Slot::RED),
        (&["punctuation.definition.directive"], Slot::MAGENTA),
        (
            &["entity.name.constant", "entity.name.preprocessor"],
            Slot::BRIGHT_YELLOW,
        ),
        (&["entity.name.function"], Slot::BRIGHT_GREEN),
        (
            &[
                "support.function",
                "variable.function",
                "meta.function-call.identifier",
            ],
            Slot::CYAN,
        ),
        (
            &[
                "variable.other.member",
                "variable.other.property",
                "meta.property.object",
                "entity.name.tag",
            ],
            Slot::BRIGHT_BLUE,
        ),
        (
            &["entity.name.type", "support.type", "support.class"],
            Slot::BRIGHT_CYAN,
        ),
        (&["entity.other.attribute-name"], Slot::BRIGHT_CYAN),
        (&["variable.parameter"], Slot::BRIGHT_YELLOW),
        (&["variable.other.constant"], Slot::BRIGHT_MAGENTA),
        (&["markup.heading"], Slot::BRIGHT_BLUE),
        (&["invalid"], Slot::RED),
    ];

    for (prefixes, slot) in RULES {
        if prefixes.iter().any(|p| name.starts_with(p)) {
            return Some((Some(*slot), Emphasis::NONE));
        }
    }

    // Emphasis-only rules, which carry no color of their own.
    if name.starts_with("markup.bold") {
        return Some((
            None,
            Emphasis {
                bold: true,
                ..Emphasis::NONE
            },
        ));
    }
    if name.starts_with("markup.italic") {
        return Some((None, Emphasis::italic()));
    }
    if name.starts_with("markup.underline.link") {
        return Some((Some(Slot::BRIGHT_BLUE), Emphasis::underline()));
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_index_is_clamped() {
        assert_eq!(Slot::BLACK.index(), 0);
        assert_eq!(Slot::BRIGHT_WHITE.index(), 15);
        assert_eq!(Slot(200).index(), 15);
    }

    #[test]
    fn scope_prefixes_match_subscopes() {
        let (color, _) = slot_for_scope("comment.line.double-slash").unwrap();
        assert_eq!(color, Some(Slot::BRIGHT_BLACK));
        let (color, _) = slot_for_scope("string.quoted.double").unwrap();
        assert_eq!(color, Some(Slot::GREEN));
    }

    #[test]
    fn keyword_operator_beats_bare_keyword() {
        // Ordering in RULES matters: the more specific scope must win.
        let (op, _) = slot_for_scope("keyword.operator.arithmetic").unwrap();
        let (kw, _) = slot_for_scope("keyword.control").unwrap();
        assert_eq!(op, Some(Slot::CYAN));
        assert_eq!(kw, Some(Slot::RED));
    }

    #[test]
    fn emphasis_only_scopes_carry_no_color() {
        let (color, emph) = slot_for_scope("markup.italic").unwrap();
        assert_eq!(color, None);
        assert!(emph.italic);
    }

    #[test]
    fn unknown_scope_has_no_style() {
        assert!(slot_for_scope("meta.whatever.nonsense").is_none());
    }

    #[test]
    fn highlights_a_rust_line_into_spans() {
        let hl = Highlighter::new();
        let syntax = hl.syntax_by_name("Rust").expect("Rust syntax");
        let mut state = hl.initial_state(syntax);
        let spans = hl.highlight_line("let x = 1; // hi", &mut state);
        assert!(!spans.is_empty());
        // Spans must tile the line without exceeding it.
        assert!(spans.iter().all(|s| s.byte_end <= "let x = 1; // hi".len()));
        assert_eq!(spans.first().unwrap().byte_start, 0);
    }

    #[test]
    fn comment_prefixes() {
        assert_eq!(line_comment_for("Rust"), Some("//"));
        assert_eq!(line_comment_for("Python"), Some("#"));
        assert_eq!(line_comment_for("Nonexistent Lang"), None);
    }
}
