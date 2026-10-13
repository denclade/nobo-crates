//! Colouring code inside a `#+BEGIN_SRC` block.
//!
//! nobo's frontend hands this to Prism, which ships a grammar per language
//! and loads more on demand. A phone gets one small scanner instead: the
//! five things worth telling apart in a code block someone is reading on a
//! 5" screen -- comments, strings, numbers, keywords and call names -- for
//! the handful of languages a notes app actually sees. A language it does
//! not know comes back unhighlighted rather than wrong, which is exactly
//! what Prism does with a grammar it has not loaded.
//!
//! Offsets are UTF-16 code units, like everywhere else this crate talks to
//! a host (see [`crate::core::org_ast`]).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenKind {
    Comment,
    String,
    Number,
    Keyword,
    /// An identifier directly followed by `(` -- a call or a definition,
    /// which is what a reader scans a block for.
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Token {
    pub kind: TokenKind,
    pub from: u32,
    pub to: u32,
}

struct Grammar {
    line_comments: &'static [&'static str],
    block_comment: Option<(&'static str, &'static str)>,
    quotes: &'static [char],
    keywords: &'static [&'static str],
}

const C_LIKE: Option<(&str, &str)> = Some(("/*", "*/"));

fn grammar(language: &str) -> Option<Grammar> {
    let language = language.trim().to_lowercase();
    let language = match language.as_str() {
        "sh" | "shell" | "zsh" | "console" => "bash",
        "js" | "node" => "javascript",
        "ts" => "typescript",
        "py" => "python",
        "yml" => "yaml",
        "htm" | "xml" | "svg" => "html",
        "c++" | "cpp" | "cc" | "h" | "hpp" => "c",
        other => other,
    };
    Some(match language {
        "bash" => Grammar {
            line_comments: &["#"],
            block_comment: None,
            quotes: &['"', '\'', '`'],
            keywords: &[
                "if", "then", "else", "elif", "fi", "for", "while", "until", "do", "done", "case", "esac", "function", "in", "return",
                "local", "export", "source", "echo", "cd", "set", "read", "exit",
            ],
        },
        "python" => Grammar {
            line_comments: &["#"],
            block_comment: None,
            quotes: &['"', '\''],
            keywords: &[
                "def", "class", "return", "if", "elif", "else", "for", "while", "in", "not", "and", "or", "import", "from", "as", "with",
                "try", "except", "finally", "raise", "lambda", "yield", "pass", "break", "continue", "None", "True", "False", "self",
                "async", "await", "global",
            ],
        },
        "javascript" | "typescript" => Grammar {
            line_comments: &["//"],
            block_comment: C_LIKE,
            quotes: &['"', '\'', '`'],
            keywords: &[
                "const",
                "let",
                "var",
                "function",
                "return",
                "if",
                "else",
                "for",
                "while",
                "do",
                "switch",
                "case",
                "break",
                "continue",
                "new",
                "class",
                "extends",
                "import",
                "export",
                "from",
                "default",
                "async",
                "await",
                "try",
                "catch",
                "finally",
                "throw",
                "typeof",
                "instanceof",
                "null",
                "undefined",
                "true",
                "false",
                "this",
                "interface",
                "type",
                "enum",
                "public",
                "private",
            ],
        },
        "rust" => Grammar {
            line_comments: &["//"],
            block_comment: C_LIKE,
            quotes: &['"'],
            keywords: &[
                "fn", "let", "mut", "const", "static", "struct", "enum", "impl", "trait", "for", "while", "loop", "if", "else", "match",
                "return", "pub", "use", "mod", "crate", "self", "super", "where", "async", "await", "move", "ref", "dyn", "as", "in",
                "break", "continue", "true", "false", "Some", "None", "Ok", "Err",
            ],
        },
        "c" => Grammar {
            line_comments: &["//"],
            block_comment: C_LIKE,
            quotes: &['"', '\''],
            keywords: &[
                "int",
                "char",
                "void",
                "float",
                "double",
                "long",
                "short",
                "unsigned",
                "signed",
                "struct",
                "enum",
                "union",
                "typedef",
                "const",
                "static",
                "return",
                "if",
                "else",
                "for",
                "while",
                "do",
                "switch",
                "case",
                "break",
                "continue",
                "sizeof",
                "class",
                "public",
                "private",
                "namespace",
                "template",
                "auto",
                "bool",
                "true",
                "false",
                "nullptr",
                "new",
                "delete",
            ],
        },
        "sql" => Grammar {
            line_comments: &["--"],
            block_comment: C_LIKE,
            quotes: &['"', '\''],
            keywords: &[
                "select", "from", "where", "insert", "into", "values", "update", "set", "delete", "create", "table", "index", "drop",
                "alter", "join", "left", "right", "inner", "outer", "on", "group", "order", "by", "having", "limit", "offset", "and", "or",
                "not", "null", "as", "distinct",
            ],
        },
        "json" => Grammar { line_comments: &[], block_comment: None, quotes: &['"'], keywords: &["true", "false", "null"] },
        "yaml" => {
            Grammar { line_comments: &["#"], block_comment: None, quotes: &['"', '\''], keywords: &["true", "false", "null", "yes", "no"] }
        }
        "css" => Grammar {
            line_comments: &[],
            block_comment: C_LIKE,
            quotes: &['"', '\''],
            keywords: &["important", "media", "import", "keyframes", "supports", "font-face", "root"],
        },
        "html" => Grammar { line_comments: &[], block_comment: Some(("<!--", "-->")), quotes: &['"', '\''], keywords: &[] },
        "toml" | "ini" | "conf" => {
            Grammar { line_comments: &["#", ";"], block_comment: None, quotes: &['"', '\''], keywords: &["true", "false"] }
        }
        _ => return None,
    })
}

/// Tokens in CODE, in order and non-overlapping.
///
/// An unknown language gives an empty list -- the caller draws the code as
/// it is, which is the honest answer and never a wrong one.
pub fn highlight(language: &str, code: &str) -> Vec<Token> {
    let Some(grammar) = grammar(language) else { return Vec::new() };
    let chars: Vec<char> = code.chars().collect();
    let mut offsets: Vec<u32> = Vec::with_capacity(chars.len() + 1);
    let mut offset = 0u32;
    for ch in &chars {
        offsets.push(offset);
        offset += ch.len_utf16() as u32;
    }
    offsets.push(offset);

    let mut tokens = Vec::new();
    let mut i = 0usize;
    let rest = |i: usize| -> String { chars[i..].iter().take(4).collect() };

    while i < chars.len() {
        // Comments first: everything else can appear inside one.
        if let Some(prefix) = grammar.line_comments.iter().find(|p| rest(i).starts_with(**p)) {
            let _ = prefix;
            let end = (i..chars.len()).find(|j| chars[*j] == '\n').unwrap_or(chars.len());
            tokens.push(Token { kind: TokenKind::Comment, from: offsets[i], to: offsets[end] });
            i = end;
            continue;
        }
        if let Some((open, close)) = grammar.block_comment {
            if rest(i).starts_with(open) {
                let end = find_from(&chars, i + open.chars().count(), close).map(|j| j + close.chars().count()).unwrap_or(chars.len());
                tokens.push(Token { kind: TokenKind::Comment, from: offsets[i], to: offsets[end] });
                i = end;
                continue;
            }
        }
        if grammar.quotes.contains(&chars[i]) {
            let quote = chars[i];
            let mut j = i + 1;
            while j < chars.len() {
                if chars[j] == '\\' {
                    j += 2;
                    continue;
                }
                if chars[j] == quote || chars[j] == '\n' {
                    break;
                }
                j += 1;
            }
            let end = (j + 1).min(chars.len());
            tokens.push(Token { kind: TokenKind::String, from: offsets[i], to: offsets[end] });
            i = end;
            continue;
        }
        if chars[i].is_ascii_digit() && (i == 0 || !is_word(chars[i - 1])) {
            let end = (i..chars.len())
                .find(|j| !(chars[*j].is_ascii_digit() || chars[*j] == '.' || chars[*j] == 'x' || chars[*j].is_ascii_hexdigit()))
                .unwrap_or(chars.len());
            tokens.push(Token { kind: TokenKind::Number, from: offsets[i], to: offsets[end] });
            i = end;
            continue;
        }
        if is_word_start(chars[i]) {
            let end = (i..chars.len()).find(|j| !is_word(chars[*j])).unwrap_or(chars.len());
            let word: String = chars[i..end].iter().collect();
            if grammar.keywords.contains(&word.as_str()) {
                tokens.push(Token { kind: TokenKind::Keyword, from: offsets[i], to: offsets[end] });
            } else if chars.get(end) == Some(&'(') {
                tokens.push(Token { kind: TokenKind::Function, from: offsets[i], to: offsets[end] });
            }
            i = end;
            continue;
        }
        i += 1;
    }
    tokens
}

/// Whether a language is one this scanner knows -- what a language picker
/// offers first, and what tells "no colours because it is plain text" from
/// "no colours because nobody taught it this one".
pub fn is_known_language(language: &str) -> bool {
    grammar(language).is_some()
}

/// The languages worth offering in a picker, most likely first.
pub fn known_languages() -> Vec<String> {
    ["bash", "python", "javascript", "typescript", "json", "yaml", "rust", "c", "sql", "css", "html", "toml"]
        .iter()
        .map(|l| l.to_string())
        .collect()
}

fn is_word_start(ch: char) -> bool {
    ch.is_alphabetic() || ch == '_' || ch == '$'
}

fn is_word(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_' || ch == '$' || ch == '-'
}

fn find_from(chars: &[char], from: usize, needle: &str) -> Option<usize> {
    let needle: Vec<char> = needle.chars().collect();
    (from..chars.len().saturating_sub(needle.len() - 1)).find(|i| chars[*i..*i + needle.len()] == needle[..])
}

// ---------------------------------------------------------------------
// Colouring the org source itself
// ---------------------------------------------------------------------

/// What one stretch of a line is, for a host that paints it.
///
/// Named rather than coloured: which colour a heading gets is the host's
/// decision (and on Sailfish it changes with the ambience), while *that*
/// this stretch is a heading is the grammar's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Span {
    pub style: String,
    pub from: u32,
    pub to: u32,
}

/// One line's spans, plus the state to carry into the next line.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LineHighlight {
    pub spans: Vec<Span>,
    /// Opaque to the host: hand it back with the next line. Fits in an int
    /// because that is what a QSyntaxHighlighter carries between blocks,
    /// which is the whole reason this is a per-line function rather than a
    /// pass over the document.
    pub state: i32,
}

const MODE_NORMAL: i32 = 0;
const MODE_DRAWER: i32 = 1;
const MODE_VERBATIM: i32 = 2;
const MODE_PROSE: i32 = 3;

/// The carried state is `mode | (language + 1) << 8` -- one int, because
/// the host has one int to give.
fn encode_state(mode: i32, language: Option<&str>) -> i32 {
    let index = language
        .and_then(|language| known_languages().iter().position(|known| known == &language.to_lowercase()))
        .map(|index| index as i32 + 1)
        .unwrap_or(0);
    mode | (index << 8)
}

fn decode_state(state: i32) -> (i32, Option<String>) {
    let mode = state & 0xff;
    let index = (state >> 8) - 1;
    let language = (index >= 0).then(|| known_languages().get(index as usize).cloned()).flatten();
    (mode, language)
}

fn span(style: &str, from: u32, to: u32) -> Span {
    Span { style: style.to_string(), from, to }
}

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Colours one line of org source.
///
/// `state` is what the previous line returned (0 for the first). Per line
/// rather than per document because that is the shape a syntax
/// highlighter works in: it is handed one changed paragraph at a time and
/// the state of the one before it, and re-parsing the whole note on every
/// keystroke is exactly what makes an editor feel slow.
pub fn highlight_line(line: &str, state: i32) -> LineHighlight {
    use crate::core::models::TODO_STATES;
    use crate::core::org_ast::{classify_line, drawer_name, heading_level, indent_of, list_of, LineType};

    let (mode, language) = decode_state(state);
    let trimmed = line.trim();
    let length = utf16_len(line);
    let mut spans = Vec::new();

    // Inside a block: the delimiter ends it, everything else is content.
    if mode == MODE_VERBATIM || mode == MODE_PROSE {
        if is_block_end(trimmed) {
            spans.push(span("blockDelimiter", 0, length));
            return LineHighlight { spans, state: MODE_NORMAL };
        }
        if mode == MODE_VERBATIM {
            spans.push(span("code", 0, length));
            if let Some(language) = &language {
                for token in highlight(language, line) {
                    spans.push(span(token_style(token.kind), token.from, token.to));
                }
            }
        } else {
            spans.extend(markup_spans(line));
        }
        return LineHighlight { spans, state };
    }

    if mode == MODE_DRAWER {
        spans.push(span("drawer", 0, length));
        let ends = drawer_name(trimmed).as_deref() == Some("END") || heading_level(line).is_some();
        if ends && heading_level(line).is_some() {
            return highlight_line(line, MODE_NORMAL);
        }
        return LineHighlight { spans, state: if ends { MODE_NORMAL } else { MODE_DRAWER } };
    }

    match classify_line(line) {
        LineType::BlockBegin => {
            spans.push(span("blockDelimiter", 0, length));
            let (kind, params) = split_begin(trimmed);
            let verbatim = matches!(kind.as_str(), "SRC" | "EXAMPLE" | "EXPORT" | "COMMENT" | "ASCII" | "LATEX");
            let language = params.split_whitespace().next().filter(|token| !token.starts_with(':'));
            return LineHighlight { spans, state: encode_state(if verbatim { MODE_VERBATIM } else { MODE_PROSE }, language) };
        }
        LineType::BlockEnd | LineType::Keyword => {
            spans.push(span("blockDelimiter", 0, length));
        }
        LineType::Heading => {
            let level = heading_level(line).unwrap_or(1);
            spans.push(span(&format!("heading{}", level.min(6)), 0, length));
            let after_stars = level as u32 + 1;
            // The keyword, if this heading carries one.
            let rest: String = line.chars().skip(level + 1).collect();
            let first = rest.split_whitespace().next().unwrap_or("");
            if TODO_STATES.contains(&first) {
                let finished = matches!(first, "DONE" | "OBE" | "WONT-DO");
                let lead = utf16_len(&rest.chars().take_while(|c| c.is_whitespace()).collect::<String>());
                let from = after_stars + lead;
                spans.push(span(if finished { "done" } else { "todo" }, from, from + utf16_len(first)));
            }
            // Trailing `:tag:tag:`.
            if let Some(index) = line.rfind(char::is_whitespace) {
                let tail = line[index..].trim();
                if tail.len() > 1 && tail.starts_with(':') && tail.ends_with(':') {
                    let from = utf16_len(&line[..index + 1]);
                    spans.push(span("tag", from, length));
                }
            }
            spans.extend(markup_spans(line));
        }
        LineType::List => {
            let indent = indent_of(line) as u32;
            if let Some((bullet, checkbox)) = list_of(line) {
                let bullet_len = utf16_len(&bullet);
                spans.push(span("bullet", indent, indent + bullet_len));
                if checkbox.is_some() {
                    let gap = line.chars().skip(indent as usize + bullet.chars().count()).take_while(|c| c.is_whitespace()).count() as u32;
                    let from = indent + bullet_len + gap;
                    spans.push(span("checkbox", from, from + 3));
                }
            }
            spans.extend(markup_spans(line));
        }
        LineType::Table => {
            let mut offset = 0u32;
            for ch in line.chars() {
                if ch == '|' {
                    spans.push(span("tableSeparator", offset, offset + 1));
                }
                offset += ch.len_utf16() as u32;
            }
            spans.extend(markup_spans(line));
        }
        LineType::Planning | LineType::Logbook | LineType::Property => {
            spans.push(span("drawer", 0, length));
        }
        LineType::DrawerBegin => {
            spans.push(span("drawer", 0, length));
            return LineHighlight { spans, state: MODE_DRAWER };
        }
        LineType::DrawerEnd => {
            spans.push(span("drawer", 0, length));
        }
        LineType::Hr => {
            spans.push(span("rule", 0, length));
        }
        LineType::Empty | LineType::Text => {
            spans.extend(markup_spans(line));
        }
    }

    LineHighlight { spans, state: MODE_NORMAL }
}

/// Inline markup, with the markers dimmed rather than hidden: this is the
/// source, and an editor that hides what the caret is about to step over
/// makes it impossible to aim.
fn markup_spans(line: &str) -> Vec<Span> {
    use crate::core::org_ast::InlineKind;

    let mut spans = Vec::new();
    for found in crate::core::org_ast::inline_spans(line) {
        let marker = found.marker_len;
        let style = match found.kind {
            InlineKind::Bold => "bold",
            InlineKind::Italic => "italic",
            InlineKind::Strikethrough => "strike",
            InlineKind::Monospace => "mono",
            InlineKind::Underline => "underline",
            InlineKind::Link => "link",
        };
        spans.push(span("marker", found.from, found.from + marker));
        spans.push(span(style, found.from + marker, found.to - marker));
        spans.push(span("marker", found.to - marker, found.to));
    }
    spans
}

fn token_style(kind: TokenKind) -> &'static str {
    match kind {
        TokenKind::Comment => "comment",
        TokenKind::String => "string",
        TokenKind::Number => "number",
        TokenKind::Keyword => "codeKeyword",
        TokenKind::Function => "function",
    }
}

fn is_block_end(trimmed: &str) -> bool {
    trimmed.len() > 6 && trimmed.get(..2).is_some_and(|start| start == "#+") && trimmed[2..].to_uppercase().starts_with("END_")
}

fn split_begin(trimmed: &str) -> (String, String) {
    let rest = &trimmed[2 + 6..]; // "#+BEGIN_"
    let kind_len = rest.find(char::is_whitespace).unwrap_or(rest.len());
    (rest[..kind_len].to_uppercase(), rest[kind_len..].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(language: &str, code: &str) -> Vec<(TokenKind, String)> {
        let units: Vec<u16> = code.encode_utf16().collect();
        highlight(language, code)
            .into_iter()
            .map(|token| (token.kind, String::from_utf16_lossy(&units[token.from as usize..token.to as usize])))
            .collect()
    }

    fn styles(line: &str, state: i32) -> (Vec<String>, i32) {
        let out = highlight_line(line, state);
        (out.spans.iter().map(|span| span.style.clone()).collect(), out.state)
    }

    #[test]
    fn a_heading_carries_its_level_keyword_and_tags() {
        let (styles, state) = styles("** TODO Buy milk :shop:", 0);
        assert_eq!(styles, vec!["heading2", "todo", "tag"]);
        assert_eq!(state, 0);
        assert_eq!(styles_only("** DONE Done"), vec!["heading2", "done"]);
    }

    fn styles_only(line: &str) -> Vec<String> {
        highlight_line(line, 0).spans.into_iter().map(|span| span.style).collect()
    }

    #[test]
    fn a_source_block_colours_its_code_until_the_end_line() {
        let (_, state) = styles("#+BEGIN_SRC python", 0);
        assert_ne!(state, 0);
        let (inside, carried) = styles("    print(1)", state);
        assert_eq!(inside, vec!["code", "function", "number"]);
        assert_eq!(carried, state);
        let (end, after) = styles("#+END_SRC", state);
        assert_eq!(end, vec!["blockDelimiter"]);
        assert_eq!(after, 0);
    }

    #[test]
    fn a_drawer_dims_until_its_end() {
        let (_, state) = styles(":PROPERTIES:", 0);
        assert_eq!(state, 1);
        let (inside, carried) = styles(":CUSTOM_ID: h:abc", state);
        assert_eq!(inside, vec!["drawer"]);
        assert_eq!(carried, 1);
        assert_eq!(styles(":END:", state).1, 0);
    }

    #[test]
    fn an_unterminated_drawer_ends_at_the_next_heading() {
        let (styles, state) = styles("* Next", 1);
        assert_eq!(styles, vec!["heading1"]);
        assert_eq!(state, 0);
    }

    #[test]
    fn markup_markers_are_their_own_spans() {
        assert_eq!(styles_only("a *b* c"), vec!["marker", "bold", "marker"]);
    }

    #[test]
    fn a_list_marks_its_bullet_and_checkbox() {
        assert_eq!(styles_only("  - [X] done"), vec!["bullet", "checkbox"]);
    }

    #[test]
    fn python_keywords_strings_and_calls() {
        assert_eq!(
            kinds("python", "def greet(n):\n    print(\"hi\")  # done"),
            vec![
                (TokenKind::Keyword, "def".into()),
                (TokenKind::Function, "greet".into()),
                (TokenKind::Function, "print".into()),
                (TokenKind::String, "\"hi\"".into()),
                (TokenKind::Comment, "# done".into()),
            ]
        );
    }

    #[test]
    fn a_comment_swallows_what_looks_like_code() {
        assert_eq!(kinds("bash", "# echo \"x\" 42"), vec![(TokenKind::Comment, "# echo \"x\" 42".into())]);
    }

    #[test]
    fn an_unterminated_string_ends_at_the_line() {
        assert_eq!(kinds("python", "x = \"open\ny = 1"), vec![(TokenKind::String, "\"open\n".into()), (TokenKind::Number, "1".into())]);
    }

    #[test]
    fn an_unknown_language_is_left_alone() {
        assert!(highlight("brainfuck", "+++[->+<]").is_empty());
        assert!(highlight("", "anything").is_empty());
        assert!(!is_known_language("brainfuck"));
    }

    #[test]
    fn aliases_reach_the_same_grammar() {
        assert_eq!(kinds("sh", "if true; then"), kinds("bash", "if true; then"));
        assert!(is_known_language("JS"));
    }

    #[test]
    fn offsets_survive_non_ascii_code() {
        // One "ü" is two bytes but one UTF-16 unit; a host slicing by the
        // wrong one paints the next token half a character off.
        let tokens = highlight("python", "s = \"grüße\"\nn = 1");
        let units: Vec<u16> = "s = \"grüße\"\nn = 1".encode_utf16().collect();
        let number = tokens.last().expect("the number");
        assert_eq!(String::from_utf16_lossy(&units[number.from as usize..number.to as usize]), "1");
    }
}
