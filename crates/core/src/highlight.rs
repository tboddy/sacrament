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

/// A language syntect's bundled set doesn't ship.
///
/// Sublime's default packages have no TOML — and no INI, cfg or conf either —
/// so `Cargo.toml` and the app's own `config.toml` came out unhighlighted. The
/// alternatives were vendoring a third-party `.sublime-syntax` and enabling
/// syntect's YAML loader (a dependency and a startup parse for one language), or
/// this: a small line-wise highlighter for a format that is line-wise anyway.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    Toml,
}

/// Parser state *before* a given line. Highlighting is line-by-line and each
/// line's parse depends on the previous, which is what the per-buffer
/// `line_state_before` cache exists to avoid re-deriving.
#[derive(Clone)]
pub enum LineState {
    /// syntect's incremental parse.
    Syntect {
        parse: ParseState,
        scopes: ScopeStack,
    },
    /// A built-in language. `in_block_string` is the only state TOML carries
    /// across lines — a `"""` or `'''` block.
    Builtin { kind: Builtin, in_block_string: bool },
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
        LineState::Syntect {
            parse: ParseState::new(syntax),
            scopes: ScopeStack::new(),
        }
    }

    /// The syntax name and starting state for a path, syntect's or ours.
    ///
    /// One call so callers don't have to know that some languages come from
    /// syntect and some don't — the built-ins would otherwise have to be checked
    /// at every site that seeds a buffer.
    pub fn seed_for_path(&self, path: &Path) -> Option<(String, LineState)> {
        if let Some(syntax) = self.syntax_for_path(path) {
            return Some((syntax.name.clone(), self.initial_state(syntax)));
        }
        let kind = match path.extension().and_then(|e| e.to_str())? {
            "toml" => Builtin::Toml,
            _ => return None,
        };
        // The name matters beyond display: `line_comment_for` keys off it, so
        // Cmd+/ works in a built-in language too.
        Some((
            "TOML".to_string(),
            LineState::Builtin {
                kind,
                in_block_string: false,
            },
        ))
    }

    pub fn highlight_line(&self, line: &str, state: &mut LineState) -> Vec<HlSpan> {
        let (parse, scopes) = match state {
            LineState::Syntect { parse, scopes } => (parse, scopes),
            LineState::Builtin {
                kind: Builtin::Toml,
                in_block_string,
            } => return toml_syntax::highlight(line, in_block_string),
        };
        self.highlight_syntect(line, parse, scopes)
    }

    fn highlight_syntect(
        &self,
        line: &str,
        state_parse: &mut ParseState,
        state_scopes: &mut ScopeStack,
    ) -> Vec<HlSpan> {
        let mut with_nl = String::with_capacity(line.len() + 1);
        with_nl.push_str(line);
        with_nl.push('\n');

        let ops = state_parse
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
                let (color, emphasis) = style_for(state_scopes);
                spans.push(HlSpan {
                    color,
                    emphasis,
                    byte_start: last_byte,
                    byte_end: clamped,
                });
                last_byte = clamped;
            }
            state_scopes.apply(op).ok();
        }

        if last_byte < actual_len {
            let (color, emphasis) = style_for(state_scopes);
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
    //
    // `markup.bold` renders italic rather than bold: the app uses no bold faces,
    // and this is the editing view of the same `**text**` the read mode shows,
    // so the two should agree.
    if name.starts_with("markup.bold") {
        return Some((None, Emphasis::italic()));
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


/// A small line-wise highlighter for TOML.
///
/// syntect's bundled set has no TOML, and TOML is line-oriented enough that a
/// grammar is more machinery than it needs: the only state crossing a line
/// boundary is whether a block string is open. Colours match what `style_for`
/// gives the equivalent scopes elsewhere, so a `.toml` file sits beside a `.rs`
/// one without looking like a different program rendered it.
mod toml_syntax {
    use super::{Emphasis, HlSpan, Slot};

    const COMMENT: Slot = Slot::BRIGHT_BLACK;
    const STRING: Slot = Slot::GREEN;
    /// Numbers, booleans and datetimes: all `constant.*` elsewhere.
    const CONSTANT: Slot = Slot::BRIGHT_MAGENTA;
    /// Table headers, which are the file's structure.
    const HEADER: Slot = Slot::BRIGHT_YELLOW;
    /// Keys, matching the `variable.other.member` family.
    const KEY: Slot = Slot::BRIGHT_BLUE;
    const OP: Slot = Slot::CYAN;

    fn push(out: &mut Vec<HlSpan>, color: Slot, start: usize, end: usize) {
        if end > start {
            out.push(HlSpan {
                color: Some(color),
                emphasis: Emphasis::NONE,
                byte_start: start,
                byte_end: end,
            });
        }
    }

    pub fn highlight(line: &str, in_block_string: &mut bool) -> Vec<HlSpan> {
        let mut out = Vec::new();
        let b = line.as_bytes();
        let mut i = 0;

        // Continuing a block string opened on an earlier line.
        if *in_block_string {
            match block_end(line, 0) {
                Some(end) => {
                    push(&mut out, STRING, 0, end);
                    *in_block_string = false;
                    i = end;
                }
                None => {
                    push(&mut out, STRING, 0, line.len());
                    return out;
                }
            }
        }

        while i < b.len() && (b[i] == b' ' || b[i] == b'\t') {
            i += 1;
        }
        if i >= b.len() {
            return out;
        }

        if b[i] == b'#' {
            push(&mut out, COMMENT, i, line.len());
            return out;
        }

        if b[i] == b'[' {
            // Scan to the closing bracket rather than the last one on the line,
            // so a `]` inside a trailing comment doesn't swallow it.
            let mut j = i;
            while j < b.len() && b[j] != b']' {
                j += 1;
            }
            if j < b.len() {
                j += 1;
                if j < b.len() && b[j] == b']' {
                    j += 1;
                }
            }
            push(&mut out, HEADER, i, j);
            i = j;
        } else if let Some(eq) = key_end(line, i) {
            let mut key_stop = eq;
            while key_stop > i && (b[key_stop - 1] == b' ' || b[key_stop - 1] == b'\t') {
                key_stop -= 1;
            }
            push(&mut out, KEY, i, key_stop);
            push(&mut out, OP, eq, eq + 1);
            i = eq + 1;
        }

        values(line, i, in_block_string, &mut out);
        out
    }

    /// Byte index of the `=` separating key from value, if this line has one.
    ///
    /// Quote-aware, because a key may be quoted and contain anything — a key of
    /// `"a=b"` is legal and its first `=` isn't the separator.
    fn key_end(line: &str, from: usize) -> Option<usize> {
        let b = line.as_bytes();
        let mut quote: Option<u8> = None;
        for (i, &c) in b.iter().enumerate().skip(from) {
            match quote {
                Some(q) => {
                    if c == q {
                        quote = None;
                    }
                }
                None => match c {
                    b'"' | b'\'' => quote = Some(c),
                    b'=' => return Some(i),
                    b'#' => return None,
                    _ => {}
                },
            }
        }
        None
    }

    /// Colour whatever follows the `=`: strings, constants and a trailing
    /// comment. Array and inline-table punctuation is left alone, which lets
    /// their contents be coloured without special-casing either.
    fn values(line: &str, mut i: usize, in_block_string: &mut bool, out: &mut Vec<HlSpan>) {
        let b = line.as_bytes();
        while i < b.len() {
            match b[i] {
                b' ' | b'\t' | b',' | b'[' | b']' | b'{' | b'}' => i += 1,
                b'#' => {
                    push(out, COMMENT, i, line.len());
                    return;
                }
                b'"' | b'\'' => {
                    let quote = b[i];
                    // A tripled quote opens a block that may not close here.
                    if b[i..].starts_with(&[quote, quote, quote]) {
                        match block_end(line, i + 3) {
                            Some(end) => {
                                push(out, STRING, i, end);
                                i = end;
                            }
                            None => {
                                push(out, STRING, i, line.len());
                                *in_block_string = true;
                                return;
                            }
                        }
                    } else {
                        let end = string_end(line, i, quote);
                        push(out, STRING, i, end);
                        i = end;
                    }
                }
                c if c.is_ascii_digit() || c == b'+' || c == b'-' => {
                    let start = i;
                    // Datetimes are as much a constant as numbers and share
                    // their characters, so one scan covers both.
                    while i < b.len()
                        && (b[i].is_ascii_alphanumeric()
                            || matches!(b[i], b'+' | b'-' | b'.' | b':' | b'_'))
                    {
                        i += 1;
                    }
                    push(out, CONSTANT, start, i);
                }
                c if c.is_ascii_alphabetic() => {
                    let start = i;
                    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
                        i += 1;
                    }
                    if matches!(&line[start..i], "true" | "false" | "inf" | "nan") {
                        push(out, CONSTANT, start, i);
                    }
                }
                _ => i += 1,
            }
        }
    }

    /// End of a quoted string, just past its closer.
    fn string_end(line: &str, start: usize, quote: u8) -> usize {
        let b = line.as_bytes();
        let mut i = start + 1;
        while i < b.len() {
            // Only basic strings take escapes; a literal string doesn't, so a
            // trailing backslash inside one mustn't swallow the closer.
            if quote == b'"' && b[i] == b'\\' {
                i += 2;
                continue;
            }
            if b[i] == quote {
                return i + 1;
            }
            i += 1;
        }
        line.len()
    }

    /// End of a block string that closes on this line, just past its delimiter.
    fn block_end(line: &str, from: usize) -> Option<usize> {
        let b = line.as_bytes();
        let mut i = from;
        while i + 3 <= b.len() {
            if &b[i..i + 3] == b"\"\"\"" || &b[i..i + 3] == b"'''" {
                return Some(i + 3);
            }
            i += 1;
        }
        None
    }
}

#[cfg(test)]
mod toml_tests {
    use super::*;

    fn spans(line: &str, open: &mut bool) -> Vec<(Option<Slot>, String)> {
        toml_syntax::highlight(line, open)
            .into_iter()
            .map(|s| (s.color, line[s.byte_start..s.byte_end].to_string()))
            .collect()
    }

    fn one(line: &str) -> Vec<(Option<Slot>, String)> {
        spans(line, &mut false)
    }

    #[test]
    fn toml_is_recognised_even_though_syntect_has_no_grammar_for_it() {
        let hl = Highlighter::new();
        assert!(
            hl.syntax_for_path(Path::new("a.toml")).is_none(),
            "if syntect ever ships TOML, prefer its grammar over ours"
        );
        let (name, state) = hl.seed_for_path(Path::new("Cargo.toml")).expect("seeded");
        assert_eq!(name, "TOML");
        assert!(matches!(state, LineState::Builtin { .. }));
        // The name is what Cmd+/ keys off, so comment toggling works too.
        assert_eq!(line_comment_for(&name), Some("#"));
    }

    #[test]
    fn a_table_header_is_coloured_whole() {
        assert_eq!(one("[dependencies]"), vec![(Some(Slot::BRIGHT_YELLOW), "[dependencies]".into())]);
        assert_eq!(one("[[bin]]"), vec![(Some(Slot::BRIGHT_YELLOW), "[[bin]]".into())]);
    }

    #[test]
    fn a_header_stops_at_its_bracket_not_at_one_in_a_comment() {
        let got = one("[bin]  # array[0]");
        assert_eq!(got[0], (Some(Slot::BRIGHT_YELLOW), "[bin]".into()));
        assert_eq!(got[1], (Some(Slot::BRIGHT_BLACK), "# array[0]".into()));
    }

    #[test]
    fn a_key_value_pair_splits_into_key_operator_and_value() {
        let got = one(r#"name = "sacrament""#);
        assert_eq!(got[0], (Some(Slot::BRIGHT_BLUE), "name".into()));
        assert_eq!(got[1], (Some(Slot::CYAN), "=".into()));
        assert_eq!(got[2], (Some(Slot::GREEN), "\"sacrament\"".into()));
    }

    #[test]
    fn an_equals_inside_a_quoted_key_is_not_the_separator() {
        let got = one(r#""a=b" = 1"#);
        assert_eq!(got[0], (Some(Slot::BRIGHT_BLUE), "\"a=b\"".into()));
        assert_eq!(got[1], (Some(Slot::CYAN), "=".into()));
        assert_eq!(got[2], (Some(Slot::BRIGHT_MAGENTA), "1".into()));
    }

    #[test]
    fn constants_cover_numbers_booleans_and_datetimes() {
        for (src, want) in [
            ("a = 42", "42"),
            ("a = -1.5e3", "-1.5e3"),
            ("a = true", "true"),
            ("a = 1979-05-27T07:32:00Z", "1979-05-27T07:32:00Z"),
        ] {
            let got = one(src);
            assert_eq!(
                got.last().unwrap(),
                &(Some(Slot::BRIGHT_MAGENTA), want.to_string()),
                "for {src}"
            );
        }
    }

    #[test]
    fn array_and_inline_table_contents_are_coloured() {
        let got = one(r#"features = ["a", "b"]"#);
        let strings: Vec<&String> = got
            .iter()
            .filter(|(c, _)| *c == Some(Slot::GREEN))
            .map(|(_, t)| t)
            .collect();
        assert_eq!(strings, vec!["\"a\"", "\"b\""]);

        let got = one(r#"dep = { version = "1", optional = true }"#);
        assert!(got.iter().any(|(c, t)| *c == Some(Slot::GREEN) && t == "\"1\""));
        assert!(got.iter().any(|(c, t)| *c == Some(Slot::BRIGHT_MAGENTA) && t == "true"));
    }

    #[test]
    fn a_comment_after_a_value_is_still_a_comment() {
        let got = one("edition = 2021 # the newer one");
        assert_eq!(got.last().unwrap(), &(Some(Slot::BRIGHT_BLACK), "# the newer one".into()));
    }

    #[test]
    fn an_escaped_quote_does_not_end_a_string() {
        let got = one(r#"a = "say \"hi\" now""#);
        assert_eq!(got[2], (Some(Slot::GREEN), r#""say \"hi\" now""#.into()));
    }

    #[test]
    fn a_block_string_carries_across_lines() {
        // The one piece of state TOML has, and the reason `LineState` keeps a
        // flag for built-ins at all.
        let mut open = false;
        let first = spans(r#"text = """start"#, &mut open);
        assert!(open, "block left open");
        assert_eq!(first[2].0, Some(Slot::GREEN));

        let middle = spans("still inside # not a comment", &mut open);
        assert!(open, "still open");
        assert_eq!(middle, vec![(Some(Slot::GREEN), "still inside # not a comment".into())]);

        let last = spans(r#"end""" "#, &mut open);
        assert!(!open, "closed");
        assert_eq!(last[0], (Some(Slot::GREEN), "end\"\"\"".into()));
    }
}
