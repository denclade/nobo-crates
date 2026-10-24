//! One grammar for org text: what a line is, what markup it carries, and
//! how a document groups into the blocks a reader sees.
//!
//! This used to live in the frontend (`src/components/editor/orgAST/`),
//! where CodeMirror both parsed and drew. A second frontend cannot reuse a
//! CodeMirror plugin, and the two would have drifted the day one of them
//! learned a new block type -- so the grammar moved here and the frontends
//! keep only their drawing.
//!
//! Three shapes, three consumers:
//! * [`parse`] groups a document into render blocks (a read-only view).
//! * [`segments`] flattens one line's markup into runs of styled text (the
//!   same view, one line at a time).
//! * [`inline_spans`] keeps the offsets instead, for a syntax highlighter
//!   that colours the source rather than replacing it.
//!
//! Every offset handed out is a **UTF-16 code unit** index, not a byte
//! index: the only consumer that asks for offsets is Qt, whose `QString`
//! is UTF-16, and a German note is full of characters where the two
//! disagree.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::core::models::TODO_STATES;
use crate::core::org_parser::parse_heading_line;

/// Inline markup this grammar knows, matching org's own set as far as the
/// editor supports it. Deliberately not extended beyond what nobo's
/// frontend renders -- one grammar means one answer, not a superset here
/// and a subset there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InlineKind {
    Bold,
    Italic,
    Strikethrough,
    Monospace,
    Underline,
    Link,
}

/// One piece of markup with its position in the source line.
///
/// `from`/`to` bracket the whole thing including its markers; `marker_len`
/// says how much of each end is marker, so a highlighter can dim the
/// markers and style what is between them without matching anything twice.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InlineSpan {
    pub kind: InlineKind,
    pub from: u32,
    pub to: u32,
    pub marker_len: u32,
    /// Link only: what `[[target]]` points at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Link only: the `[[target][description]]` half, when there is one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A run of text that shares one set of attributes -- what a renderer
/// draws, with the markers already gone.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Segment {
    pub text: String,
    #[serde(default, skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub italic: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub strike: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub mono: bool,
    #[serde(default, skip_serializing_if = "is_false")]
    pub underline: bool,
    /// Set on the visible part of a link; carries its raw target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// What a single line is, judged on the line alone.
///
/// Lines that only mean something in context -- the inside of a drawer or
/// of a `#+BEGIN_…` block -- are not in here; that is what [`parse`] and
/// the highlighter's carried state are for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LineType {
    Empty,
    Text,
    Heading,
    Hr,
    List,
    Keyword,
    Table,
    Planning,
    DrawerBegin,
    DrawerEnd,
    Property,
    Logbook,
    BlockBegin,
    BlockEnd,
}

/// What a reader sees as one thing.
///
/// Coarser than a line: a paragraph is its lines reflowed, a table is all
/// of its rows, and everything org files under a heading (planning line,
/// properties, logbook, any other drawer) is one [`BlockKind::Metadata`]
/// entry, because "what is filed under this title?" is one question.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BlockKind {
    Keyword,
    Heading,
    Paragraph,
    ListItem,
    Table,
    Src,
    Example,
    Quote,
    Verse,
    Block,
    Results,
    Image,
    Metadata,
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableCell {
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<Segment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct TableRow {
    /// A `|---+---|` rule rather than data.
    #[serde(default, skip_serializing_if = "is_false")]
    pub separator: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cells: Vec<TableCell>,
}

/// One entry of [`parse`]'s output.
///
/// One struct for every kind rather than an enum with payloads: the
/// consumer is a list model whose roles are columns, and a tagged union
/// would only be taken apart again on the other side of the JSON.
/// Everything not meaningful for a kind is skipped when serializing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Block {
    pub kind: BlockKind,
    /// 1-based line this block starts on, and the one it ends on. What
    /// makes a checkbox in a read-only view writable: the toggle knows
    /// which line to rewrite without re-parsing anything.
    pub line: usize,
    pub end_line: usize,
    /// The block's own text, reflowed for paragraphs and verbatim for
    /// everything that is meant to keep its line breaks.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<Segment>,

    // -- heading --
    #[serde(default, skip_serializing_if = "is_zero")]
    pub level: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_id: Option<String>,

    // -- list item --
    #[serde(default, skip_serializing_if = "is_zero")]
    pub depth: usize,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub bullet: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkbox: Option<String>,
    /// A checkbox whose state is the aggregate of its children's, which is
    /// why it must not be toggled directly.
    #[serde(default, skip_serializing_if = "is_false")]
    pub derived: bool,

    // -- blocks, images --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,

    // -- table --
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<TableRow>,

    // -- keyword --
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,

    // -- metadata --
    /// The `SCHEDULED:`/`DEADLINE:`/`CLOSED:` line's body, if there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planning: Option<String>,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub property_count: usize,
    /// Names of the drawers folded into this block, `PROPERTIES` included.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub drawers: Vec<String>,
    /// Lines behind the fold -- what a "12 lines" hint counts.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub line_count: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

/// A default so [`Block`] can derive `Default` and every construction site
/// sets only the handful of fields its own kind uses.
impl Default for BlockKind {
    fn default() -> Self {
        BlockKind::Paragraph
    }
}

// ---------------------------------------------------------------------
// Offsets
// ---------------------------------------------------------------------

fn utf16_len(text: &str) -> u32 {
    text.encode_utf16().count() as u32
}

/// Characters of a line paired with the UTF-16 offset each one starts at,
/// so a match found by character index can be reported in Qt's units.
fn chars_with_offsets(text: &str) -> (Vec<char>, Vec<u32>) {
    let mut chars = Vec::new();
    let mut offsets = Vec::new();
    let mut offset = 0u32;
    for ch in text.chars() {
        chars.push(ch);
        offsets.push(offset);
        offset += ch.len_utf16() as u32;
    }
    offsets.push(offset);
    (chars, offsets)
}

// ---------------------------------------------------------------------
// Inline markup
// ---------------------------------------------------------------------

const MARKERS: [(char, InlineKind); 6] = [
    ('*', InlineKind::Bold),
    ('/', InlineKind::Italic),
    ('+', InlineKind::Strikethrough),
    ('~', InlineKind::Monospace),
    // Org's other two, which were missing: `=verbatim=` is monospace
    // like `~code~` (org keeps them apart for export; on a screen they
    // read the same), and `_underline_` had no rendering at all.
    ('=', InlineKind::Monospace),
    ('_', InlineKind::Underline),
];

/// A marker only opens after a line start or whitespace, only closes after
/// a non-blank character, and what follows the closing one must end the
/// line or be whitespace or punctuation. Without those guards a file path
/// (`a/b/c`), a date range (`2026-01-01+2`) or a bullet list of products
/// would light up as markup.
fn closes_ok(next: Option<char>) -> bool {
    match next {
        None => true,
        Some(ch) => ch.is_whitespace() || ch.is_ascii_punctuation() || !ch.is_alphanumeric(),
    }
}

/// Markup and links in one line, sorted by position.
pub fn inline_spans(text: &str) -> Vec<InlineSpan> {
    let (chars, offsets) = chars_with_offsets(text);
    let mut spans = Vec::new();

    for (marker, kind) in MARKERS {
        let mut i = 0usize;
        while i < chars.len() {
            if chars[i] != marker {
                i += 1;
                continue;
            }
            let opens = i == 0 || chars[i - 1].is_whitespace();
            let content_starts = chars.get(i + 1).is_some_and(|c| !c.is_whitespace() && *c != marker);
            if !(opens && content_starts) {
                i += 1;
                continue;
            }
            // The nearest marker that can close, and no crossing of
            // another one: `*a* b *c*` is two spans, never one.
            let mut end = None;
            let mut j = i + 2;
            while j < chars.len() {
                if chars[j] == marker {
                    if !chars[j - 1].is_whitespace() && closes_ok(chars.get(j + 1).copied()) {
                        end = Some(j);
                    }
                    break;
                }
                j += 1;
            }
            match end {
                Some(j) => {
                    spans.push(InlineSpan { kind, from: offsets[i], to: offsets[j + 1], marker_len: 1, target: None, description: None });
                    i = j + 1;
                }
                None => i += 1,
            }
        }
    }

    spans.extend(link_spans(&chars, &offsets));
    spans.sort_by_key(|span| (span.from, span.to));
    spans
}

/// `[[target]]` and `[[target][description]]`. Its own matcher rather than
/// another marker rule: two possible halves, and no repeated character to
/// key off.
fn link_spans(chars: &[char], offsets: &[u32]) -> Vec<InlineSpan> {
    let mut spans = Vec::new();
    let mut i = 0usize;
    while i + 3 < chars.len() {
        if !(chars[i] == '[' && chars[i + 1] == '[') {
            i += 1;
            continue;
        }
        let Some(target_end) = find(chars, i + 2, ']') else { break };
        if target_end == i + 2 {
            i += 1;
            continue;
        }
        let target: String = chars[i + 2..target_end].iter().collect();
        let (description, end) = match chars.get(target_end + 1) {
            Some(']') => (None, target_end + 2),
            Some('[') => {
                let Some(desc_end) = find(chars, target_end + 2, ']') else {
                    i += 1;
                    continue;
                };
                if chars.get(desc_end + 1) != Some(&']') {
                    i += 1;
                    continue;
                }
                (Some(chars[target_end + 2..desc_end].iter().collect::<String>()), desc_end + 2)
            }
            _ => {
                i += 1;
                continue;
            }
        };
        spans.push(InlineSpan {
            kind: InlineKind::Link,
            from: offsets[i],
            to: offsets[end],
            marker_len: 2,
            target: Some(target),
            description,
        });
        i = end;
    }
    spans
}

fn find(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|&i| chars[i] == needle)
}

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CharStyle {
    bold: bool,
    italic: bool,
    strike: bool,
    mono: bool,
    underline: bool,
    link: Option<usize>,
    hidden: bool,
}

/// One line's markup flattened into runs of styled text, markers removed
/// and links reduced to what they show.
///
/// Attributes are collected per character, so overlapping markup (`*bold
/// /and italic*/`) composes instead of one span winning -- which also
/// means a renderer never has to think about nesting.
pub fn segments(text: &str) -> Vec<Segment> {
    let spans = inline_spans(text);
    let (chars, offsets) = chars_with_offsets(text);
    let mut styles = vec![CharStyle::default(); chars.len()];
    let mut targets: Vec<String> = Vec::new();

    let index_of = |offset: u32| -> usize { offsets.iter().position(|o| *o == offset).unwrap_or(chars.len()) };

    for span in &spans {
        let from = index_of(span.from);
        let to = index_of(span.to);
        if from >= to {
            continue;
        }
        match span.kind {
            InlineKind::Link => {
                let target = span.target.clone().unwrap_or_default();
                let target_len = target.chars().count();
                targets.push(target);
                let link = Some(targets.len() - 1);
                // `[[`, the target, and either `][` or `]]` are markup;
                // what is left is the description, or the target itself
                // when the link has none.
                let visible = match &span.description {
                    Some(description) => {
                        let start = from + 2 + target_len + 2;
                        start..start + description.chars().count()
                    }
                    None => from + 2..from + 2 + target_len,
                };
                for (i, style) in styles.iter_mut().enumerate().take(to).skip(from) {
                    style.hidden = !visible.contains(&i);
                    style.link = link;
                }
            }
            _ => {
                let marker = span.marker_len as usize;
                for (i, style) in styles.iter_mut().enumerate().take(to).skip(from) {
                    if i < from + marker || i >= to - marker {
                        style.hidden = true;
                        continue;
                    }
                    match span.kind {
                        InlineKind::Bold => style.bold = true,
                        InlineKind::Italic => style.italic = true,
                        InlineKind::Strikethrough => style.strike = true,
                        InlineKind::Monospace => style.mono = true,
                        InlineKind::Underline => style.underline = true,
                        InlineKind::Link => {}
                    }
                }
            }
        }
    }

    let mut out: Vec<Segment> = Vec::new();
    let mut current: Option<(CharStyle, String)> = None;
    for (i, ch) in chars.iter().enumerate() {
        if styles[i].hidden {
            continue;
        }
        let style = styles[i];
        match &mut current {
            Some((open, buffer)) if *open == style => buffer.push(*ch),
            _ => {
                if let Some((open, buffer)) = current.take() {
                    out.push(finish_segment(open, buffer, &targets));
                }
                current = Some((style, ch.to_string()));
            }
        }
    }
    if let Some((open, buffer)) = current {
        out.push(finish_segment(open, buffer, &targets));
    }
    out
}

fn finish_segment(style: CharStyle, text: String, targets: &[String]) -> Segment {
    Segment {
        text,
        bold: style.bold,
        italic: style.italic,
        strike: style.strike,
        mono: style.mono,
        underline: style.underline,
        link: style.link.and_then(|i| targets.get(i).cloned()),
    }
}

// ---------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------

/// Leading whitespace, in characters.
pub fn indent_of(text: &str) -> usize {
    text.chars().take_while(|c| c.is_whitespace()).count()
}

/// `*` … `******` followed by a space. The space is what keeps `*bold*` at
/// the start of a line from becoming a first-level heading.
pub fn heading_level(text: &str) -> Option<usize> {
    let stars = text.chars().take_while(|c| *c == '*').count();
    if stars == 0 || stars > 6 {
        return None;
    }
    match text.chars().nth(stars) {
        Some(ch) if ch == ' ' || ch == '\t' => Some(stars),
        _ => None,
    }
}

/// `#+KEY:` / `#+KEY[args]:`, with the key upper-cased and org's
/// `#+HEADERS:` folded into `#+HEADER:` the way org itself reads it.
pub fn keyword_of(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix("#+")?;
    let mut key_end = 0usize;
    for (i, ch) in rest.char_indices() {
        if ch.is_alphanumeric() || ch == '_' || ch == '-' || ch == '@' {
            key_end = i + ch.len_utf8();
        } else {
            break;
        }
    }
    if key_end == 0 {
        return None;
    }
    let key = rest[..key_end].to_uppercase();
    let mut after = &rest[key_end..];
    if after.starts_with('[') {
        let close = after.find(']')?;
        after = &after[close + 1..];
    }
    let value = after.strip_prefix(':')?;
    let key = if key == "HEADERS" { "HEADER".to_string() } else { key };
    Some((key, value.trim().to_string()))
}

/// The bullet and checkbox of a list line: `- `, `+ `, `12. `, `12) `,
/// `a. `, `a) `, optionally followed by `[ ]`, `[X]` or `[-]`.
///
/// Both delimiters org knows for an ordered list (`org-plain-list-ordered-
/// item-terminator`): a file written with `1)` is as much a list as one
/// with `1.`, and reading it as text left it without continuation,
/// renumbering or sorting.
pub fn list_of(text: &str) -> Option<(String, Option<String>)> {
    let indent = indent_of(text);
    let rest: String = text.chars().skip(indent).collect();
    let mut chars = rest.chars();
    let first = chars.next()?;
    let terminator = |at: usize| rest.chars().nth(at).filter(|c| *c == '.' || *c == ')');

    let bullet: String = if first == '-' || first == '+' {
        first.to_string()
    } else if first.is_ascii_digit() {
        let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        format!("{digits}{}", terminator(digits.len())?)
    } else if first.is_ascii_alphabetic() {
        format!("{first}{}", terminator(1)?)
    } else {
        return None;
    };

    let after: String = rest.chars().skip(bullet.chars().count()).collect();
    if !after.starts_with(' ') && !after.starts_with('\t') && !after.is_empty() {
        return None;
    }
    let body = after.trim_start();
    let checkbox = match body.as_bytes() {
        [b'[', state @ (b' ' | b'x' | b'X' | b'-'), b']', rest @ ..] if rest.first().is_none_or(|c| *c == b' ' || *c == b'\t') => {
            Some(match state {
                b'x' | b'X' => "X".to_string(),
                b'-' => "-".to_string(),
                _ => " ".to_string(),
            })
        }
        _ => None,
    };
    Some((bullet, checkbox))
}

/// `:LOGBOOK:` and friends -- a drawer opener carries nothing after the
/// closing colon, which is what tells it from a `:CUSTOM_ID: x` property.
pub fn drawer_name(trimmed: &str) -> Option<String> {
    let inner = trimmed.strip_prefix(':')?.strip_suffix(':')?;
    if inner.is_empty() || inner.contains(':') || inner.contains(char::is_whitespace) {
        return None;
    }
    if !inner.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-') {
        return None;
    }
    Some(inner.to_uppercase())
}

/// `:KEY: value` inside a drawer.
pub fn property_of(trimmed: &str) -> Option<(String, String)> {
    let rest = trimmed.strip_prefix(':')?;
    let end = rest.find(':')?;
    let key = &rest[..end];
    if key.is_empty() || key.contains(char::is_whitespace) {
        return None;
    }
    Some((key.to_uppercase(), rest[end + 1..].trim().to_string()))
}

fn is_planning(trimmed: &str) -> bool {
    trimmed.starts_with("SCHEDULED:") || trimmed.starts_with("DEADLINE:") || trimmed.starts_with("CLOSED:")
}

fn is_logbook_state(trimmed: &str) -> bool {
    let Some(rest) = trimmed.strip_prefix("- State \"") else { return false };
    let Some(close) = rest.find('"') else { return false };
    rest[close + 1..].trim_start().starts_with("from \"")
}

fn is_horizontal_rule(trimmed: &str) -> bool {
    trimmed.len() >= 3 && trimmed.chars().all(|c| c == '-')
}

/// `#+BEGIN_KIND rest` -- the kind upper-cased, the rest unparsed.
///
/// Not a [`keyword_of`] case: org writes these without the colon every
/// other `#+KEY:` line has.
fn block_delimiter<'a>(trimmed: &'a str, word: &str) -> Option<(String, &'a str)> {
    let rest = trimmed.strip_prefix("#+")?;
    if !rest.get(..word.len()).is_some_and(|start| start.eq_ignore_ascii_case(word)) {
        return None;
    }
    let rest = &rest[word.len()..];
    let kind_len = rest
        .char_indices()
        .take_while(|(_, ch)| ch.is_alphanumeric() || *ch == '_' || *ch == '-')
        .map(|(i, ch)| i + ch.len_utf8())
        .last()
        .unwrap_or(0);
    if kind_len == 0 {
        return None;
    }
    Some((rest[..kind_len].to_uppercase(), rest[kind_len..].trim()))
}

fn block_begin(trimmed: &str) -> Option<(String, String)> {
    block_delimiter(trimmed, "BEGIN_").map(|(kind, params)| (kind, params.to_string()))
}

fn block_end(trimmed: &str) -> Option<String> {
    let (kind, rest) = block_delimiter(trimmed, "END_")?;
    rest.is_empty().then_some(kind)
}

/// What a line is on its own. Context-dependent lines (inside a drawer or
/// a block) are reported as what they look like; the caller carries the
/// state that overrides it.
pub fn classify_line(text: &str) -> LineType {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return LineType::Empty;
    }
    if block_begin(trimmed).is_some() {
        return LineType::BlockBegin;
    }
    if block_end(trimmed).is_some() {
        return LineType::BlockEnd;
    }
    if keyword_of(trimmed).is_some() {
        return LineType::Keyword;
    }
    if heading_level(text).is_some() {
        return LineType::Heading;
    }
    if is_horizontal_rule(trimmed) {
        return LineType::Hr;
    }
    if is_planning(trimmed) {
        return LineType::Planning;
    }
    if is_logbook_state(trimmed) {
        return LineType::Logbook;
    }
    if let Some(name) = drawer_name(trimmed) {
        return if name == "END" { LineType::DrawerEnd } else { LineType::DrawerBegin };
    }
    if list_of(text).is_some() {
        return LineType::List;
    }
    if trimmed.starts_with('|') {
        return LineType::Table;
    }
    if property_of(trimmed).is_some() {
        return LineType::Property;
    }
    LineType::Text
}

// ---------------------------------------------------------------------
// Document
// ---------------------------------------------------------------------

const IMAGE_EXTENSIONS: [&str; 7] = ["png", "jpg", "jpeg", "gif", "webp", "svg", "avif"];

/// The image a line points at, when the whole line is one image link.
fn image_link(trimmed: &str) -> Option<String> {
    let spans = inline_spans(trimmed);
    let span = spans.first()?;
    if span.kind != InlineKind::Link || span.from != 0 || span.to != utf16_len(trimmed) {
        return None;
    }
    let target = span.target.clone()?;
    let path = target.strip_prefix("file:").unwrap_or(&target);
    let extension = path.rsplit('.').next()?.to_lowercase();
    IMAGE_EXTENSIONS.contains(&extension.as_str()).then(|| path.to_string())
}

fn verbatim_kind(kind: &str) -> bool {
    matches!(kind, "SRC" | "EXAMPLE" | "EXPORT" | "COMMENT" | "ASCII" | "LATEX")
}

fn block_kind_of(kind: &str) -> BlockKind {
    match kind {
        "SRC" => BlockKind::Src,
        "EXAMPLE" => BlockKind::Example,
        "QUOTE" => BlockKind::Quote,
        "VERSE" => BlockKind::Verse,
        _ => BlockKind::Block,
    }
}

/// A document as the blocks a reader sees.
///
/// Parsing and grouping in one pass: a second pass over a "lines" array
/// would be the same walk twice, and nothing here needs to look further
/// ahead than the line it is on -- except a checkbox's children, which is
/// the one thing `mark_derived_checkboxes` does at the end.
pub fn parse(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.split('\n').collect();
    let mut blocks: Vec<Block> = Vec::new();
    let mut meta: HashMap<String, String> = HashMap::new();
    let mut paragraph: Vec<String> = Vec::new();
    let mut paragraph_start = 0usize;
    let mut i = 0usize;

    macro_rules! flush_paragraph {
        () => {
            if !paragraph.is_empty() {
                let text = paragraph.join(" ");
                blocks.push(Block {
                    kind: BlockKind::Paragraph,
                    line: paragraph_start,
                    end_line: paragraph_start + paragraph.len() - 1,
                    segments: segments(&text),
                    text,
                    ..Block::default()
                });
                paragraph.clear();
            }
        };
    }

    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim();
        let line_no = i + 1;

        if trimmed.is_empty() {
            flush_paragraph!();
            meta.clear();
            i += 1;
            continue;
        }

        // `#+BEGIN_…` … `#+END_…`, taken whole: everything between is the
        // block's own content, whatever it looks like.
        if let Some((kind, params)) = block_begin(trimmed) {
            flush_paragraph!();
            let (language, _) = split_block_params(&params);
            let mut content: Vec<&str> = Vec::new();
            let mut j = i + 1;
            let mut closed = false;
            while j < lines.len() {
                if block_end(lines[j].trim()).as_deref() == Some(kind.as_str()) {
                    closed = true;
                    break;
                }
                content.push(lines[j]);
                j += 1;
            }
            let end = if closed { j } else { j.saturating_sub(1).max(i) };
            let body = content.join("\n");
            let mut block = Block {
                kind: block_kind_of(&kind),
                line: line_no,
                end_line: end + 1,
                segments: if verbatim_kind(&kind) { Vec::new() } else { segments(&body) },
                text: body,
                language,
                caption: meta.get("CAPTION").cloned(),
                name: meta.get("NAME").cloned(),
                line_count: content.len(),
                ..Block::default()
            };
            if block.kind == BlockKind::Block {
                block.key = Some(kind);
            }
            blocks.push(block);
            meta.clear();
            i = if closed { j + 1 } else { j };
            continue;
        }

        if let Some(level) = heading_level(line) {
            flush_paragraph!();
            meta.clear();
            let parsed = parse_heading_line(line);
            let title = parsed.as_ref().map(|p| p.title.clone()).unwrap_or_default();
            blocks.push(Block {
                kind: BlockKind::Heading,
                line: line_no,
                end_line: line_no,
                segments: segments(&title),
                text: title,
                level,
                state: parsed.as_ref().and_then(|p| p.state.clone()),
                priority: parsed.as_ref().and_then(|p| p.priority.clone()),
                tags: parsed.map(|p| p.tags).unwrap_or_default(),
                ..Block::default()
            });
            let heading = blocks.len() - 1;
            i += 1;

            // Planning line, properties, logbook, any other drawer: one
            // fold, because they answer one question together.
            let (metadata, next) = collect_metadata(&lines, i);
            if let Some(mut metadata) = metadata {
                blocks[heading].custom_id = metadata.value.take();
                blocks.push(metadata);
            }
            i = next;
            continue;
        }

        // A drawer standing on its own -- the file-level one above the
        // first heading -- is a block of its own.
        if drawer_name(trimmed).is_some_and(|name| name != "END") {
            flush_paragraph!();
            let (metadata, next) = collect_metadata(&lines, i);
            if let Some(mut metadata) = metadata {
                metadata.value = None;
                blocks.push(metadata);
            }
            meta.clear();
            i = next;
            continue;
        }

        if let Some((key, value)) = keyword_of(trimmed) {
            flush_paragraph!();
            if key == "RESULTS" {
                let (block, next) = collect_results(&lines, i);
                blocks.push(block);
                meta.clear();
                i = next;
                continue;
            }
            meta.insert(key.clone(), value.clone());
            blocks.push(Block {
                kind: BlockKind::Keyword,
                line: line_no,
                end_line: line_no,
                key: Some(key),
                text: value.clone(),
                value: Some(value),
                ..Block::default()
            });
            i += 1;
            continue;
        }

        if is_horizontal_rule(trimmed) {
            flush_paragraph!();
            meta.clear();
            blocks.push(Block { kind: BlockKind::Rule, line: line_no, end_line: line_no, ..Block::default() });
            i += 1;
            continue;
        }

        if is_planning(trimmed) {
            flush_paragraph!();
            let (metadata, next) = collect_metadata(&lines, i);
            if let Some(mut metadata) = metadata {
                metadata.value = None;
                blocks.push(metadata);
            }
            i = next;
            continue;
        }

        if let Some(url) = image_link(trimmed) {
            flush_paragraph!();
            blocks.push(Block {
                kind: BlockKind::Image,
                line: line_no,
                end_line: line_no,
                url: Some(url),
                caption: meta.get("CAPTION").cloned(),
                name: meta.get("NAME").cloned(),
                text: meta.get("CAPTION").cloned().unwrap_or_default(),
                ..Block::default()
            });
            meta.clear();
            i += 1;
            continue;
        }

        if let Some((bullet, checkbox)) = list_of(line) {
            flush_paragraph!();
            meta.clear();
            let indent = indent_of(line);
            let body = list_body(line, &bullet, checkbox.is_some());
            blocks.push(Block {
                kind: BlockKind::ListItem,
                line: line_no,
                end_line: line_no,
                segments: segments(&body),
                text: body,
                depth: indent,
                bullet,
                checkbox,
                ..Block::default()
            });
            i += 1;
            continue;
        }

        if trimmed.starts_with('|') {
            flush_paragraph!();
            meta.clear();
            let mut rows = Vec::new();
            let start = line_no;
            while i < lines.len() && lines[i].trim().starts_with('|') {
                rows.push(table_row(lines[i].trim()));
                i += 1;
            }
            blocks.push(Block { kind: BlockKind::Table, line: start, end_line: i, rows, ..Block::default() });
            continue;
        }

        if paragraph.is_empty() {
            paragraph_start = line_no;
        }
        paragraph.push(trimmed.to_string());
        i += 1;
    }

    flush_paragraph!();
    normalize_depths(&mut blocks);
    mark_derived_checkboxes(&mut blocks);
    blocks
}

/// Language and switches of a `#+BEGIN_SRC` parameter string. Everything
/// from the first `:key` on is header arguments, which nothing on this
/// side of the port reads (babel is a desktop affair).
fn split_block_params(params: &str) -> (Option<String>, Vec<String>) {
    let head = match params.find(" :") {
        Some(index) => &params[..index],
        None if params.starts_with(':') => "",
        None => params,
    };
    let mut tokens = head.split_whitespace().map(|t| t.to_string()).collect::<Vec<_>>();
    if tokens.first().is_some_and(|t| !t.starts_with('-')) {
        let language = tokens.remove(0);
        (Some(language), tokens)
    } else {
        (None, tokens)
    }
}

/// The text of a list item, without bullet and checkbox.
fn list_body(line: &str, bullet: &str, has_checkbox: bool) -> String {
    let indent = indent_of(line);
    let rest: String = line.chars().skip(indent + bullet.chars().count()).collect();
    let rest = rest.trim_start();
    if has_checkbox {
        rest.chars().skip(3).collect::<String>().trim_start().to_string()
    } else {
        rest.to_string()
    }
}

fn table_row(trimmed: &str) -> TableRow {
    let inner = trimmed.trim_matches('|');
    if inner.chars().all(|c| c == '-' || c == '+' || c == '|') {
        return TableRow { separator: true, cells: Vec::new() };
    }
    TableRow {
        separator: false,
        cells: inner
            .split('|')
            .map(|cell| {
                let text = cell.trim().to_string();
                TableCell { segments: segments(&text), text }
            })
            .collect(),
    }
}

/// Planning line plus every drawer that follows it, as one block.
///
/// Returns the block (if there was anything) and the line to continue at.
/// The `value` field carries the `CUSTOM_ID` property out to the heading
/// above -- the caller takes it and clears it, because a fold has no use
/// for it but a heading link does.
fn collect_metadata(lines: &[&str], start: usize) -> (Option<Block>, usize) {
    let mut i = start;
    let mut planning = None;
    let mut drawers: Vec<String> = Vec::new();
    let mut properties = 0usize;
    let mut custom_id = None;
    let mut raw: Vec<&str> = Vec::new();

    loop {
        let Some(line) = lines.get(i) else { break };
        let trimmed = line.trim();
        if is_planning(trimmed) {
            if planning.is_none() {
                planning = Some(trimmed.to_string());
            }
            raw.push(line);
            i += 1;
            continue;
        }
        match drawer_name(trimmed) {
            Some(name) if name != "END" => {
                drawers.push(name);
                raw.push(line);
                i += 1;
                // Everything up to `:END:`, or up to whatever ends the
                // entry: an unterminated drawer must not swallow the rest
                // of the file the way an early version did.
                while let Some(line) = lines.get(i) {
                    let trimmed = line.trim();
                    if heading_level(line).is_some() {
                        break;
                    }
                    raw.push(line);
                    i += 1;
                    if drawer_name(trimmed).as_deref() == Some("END") {
                        break;
                    }
                    if let Some((key, value)) = property_of(trimmed) {
                        properties += 1;
                        if key == "CUSTOM_ID" {
                            custom_id = Some(value);
                        }
                    }
                }
            }
            _ => break,
        }
    }

    if raw.is_empty() {
        return (None, start);
    }
    let line_count = raw.len();
    (
        Some(Block {
            kind: BlockKind::Metadata,
            line: start + 1,
            end_line: i,
            text: raw.join("\n"),
            planning,
            property_count: properties,
            drawers,
            line_count,
            value: custom_id,
            ..Block::default()
        }),
        i,
    )
}

/// `#+RESULTS:` and whatever it produced -- a block, a table, or a run of
/// `:`-prefixed lines.
fn collect_results(lines: &[&str], start: usize) -> (Block, usize) {
    let mut i = start + 1;
    let mut body: Vec<&str> = Vec::new();

    if let Some(line) = lines.get(i) {
        let trimmed = line.trim();
        if let Some(kind) = block_begin(trimmed).map(|(kind, _)| kind) {
            i += 1;
            while let Some(line) = lines.get(i) {
                i += 1;
                if block_end(line.trim()).as_deref() == Some(kind.as_str()) {
                    break;
                }
                body.push(line);
            }
        } else {
            while let Some(line) = lines.get(i) {
                let trimmed = line.trim();
                if !(trimmed.starts_with(':') || trimmed.starts_with('|')) {
                    break;
                }
                body.push(trimmed.strip_prefix(": ").or_else(|| trimmed.strip_prefix(':')).unwrap_or(trimmed));
                i += 1;
            }
        }
    }

    let line_count = body.len();
    (Block { kind: BlockKind::Results, line: start + 1, end_line: i, text: body.join("\n"), line_count, ..Block::default() }, i)
}

/// Turns each list item's indent in characters into a nesting depth, so a
/// renderer can indent by steps instead of by however many spaces the
/// author happened to use.
fn normalize_depths(blocks: &mut [Block]) {
    let mut stack: Vec<usize> = Vec::new();
    let mut previous_was_item = false;
    for block in blocks.iter_mut() {
        if block.kind != BlockKind::ListItem {
            if block.kind != BlockKind::Paragraph || !previous_was_item {
                stack.clear();
            }
            previous_was_item = false;
            continue;
        }
        previous_was_item = true;
        let indent = block.depth;
        while stack.last().is_some_and(|last| *last > indent) {
            stack.pop();
        }
        if stack.last() != Some(&indent) {
            stack.push(indent);
        }
        block.depth = stack.len() - 1;
    }
}

/// Marks every checkbox that has checkbox children as derived.
///
/// Org computes such a parent from below, so it is shown but not
/// toggleable -- tapping it would be overwritten by the next aggregation.
fn mark_derived_checkboxes(blocks: &mut [Block]) {
    for i in 0..blocks.len() {
        if blocks[i].kind != BlockKind::ListItem || blocks[i].checkbox.is_none() {
            continue;
        }
        let depth = blocks[i].depth;
        for j in i + 1..blocks.len() {
            let next = &blocks[j];
            if next.kind != BlockKind::ListItem {
                break;
            }
            if next.depth <= depth {
                break;
            }
            if next.checkbox.is_some() {
                blocks[i].derived = true;
                break;
            }
        }
    }
}

/// The state of a checkbox aggregated from its children: every child
/// ticked is `X`, none is a space, anything in between is org's `-`.
pub fn aggregate_checkbox(children: &[String]) -> String {
    if children.iter().all(|state| state == "X") {
        "X".to_string()
    } else if children.iter().all(|state| state == " ") {
        " ".to_string()
    } else {
        "-".to_string()
    }
}

/// Character range of the `[ ]` marker on a list line, measured rather
/// than assumed: an item written `-  [ ] x` has two spaces after the
/// bullet, and rewriting three characters at the wrong column produced
/// `-  [X]] x` the one time this was guessed.
fn checkbox_range(line: &str) -> Option<(usize, usize)> {
    let (bullet, checkbox) = list_of(line)?;
    checkbox?;
    let start = indent_of(line) + bullet.chars().count();
    let gap = line.chars().skip(start).take_while(|c| c.is_whitespace()).count();
    Some((start + gap, start + gap + 3))
}

fn replace_range(line: &str, from: usize, to: usize, insert: &str) -> String {
    let head: String = line.chars().take(from).collect();
    let tail: String = line.chars().skip(to).collect();
    format!("{head}{insert}{tail}")
}

/// Rewrites every parent checkbox to match the children below it.
///
/// Org derives a parent's state from its subtree, so this is the one place
/// that decides what a `[-]` means: all children ticked is `X`, none is a
/// space, anything else is `-`. A heading, a paragraph, a table -- anything
/// that is not part of the list -- ends it, so a change here can never
/// reach a checkbox under a different heading.
pub fn aggregate_checkboxes(text: &str) -> String {
    struct Frame {
        indent: usize,
        state: String,
        line: usize,
        children: Vec<String>,
    }

    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    let mut stack: Vec<Frame> = Vec::new();
    let mut changes: Vec<(usize, String)> = Vec::new();

    fn flush(stack: &mut Vec<Frame>, down_to: usize, changes: &mut Vec<(usize, String)>) {
        while stack.len() > down_to {
            let mut frame = stack.pop().expect("checked by the loop condition");
            if !frame.children.is_empty() {
                let expected = aggregate_checkbox(&frame.children);
                if frame.state != expected {
                    changes.push((frame.line, expected.clone()));
                    frame.state = expected;
                }
            }
            if let Some(parent) = stack.last_mut() {
                parent.children.push(frame.state);
            }
        }
    }

    for (index, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Some((_, checkbox)) = list_of(line) else {
            flush(&mut stack, 0, &mut changes);
            continue;
        };
        let Some(state) = checkbox else { continue };
        let indent = indent_of(line);
        let mut depth = stack.len();
        while depth > 0 && stack[depth - 1].indent >= indent {
            depth -= 1;
        }
        flush(&mut stack, depth, &mut changes);
        stack.push(Frame { indent, state, line: index, children: Vec::new() });
    }
    flush(&mut stack, 0, &mut changes);

    for (index, state) in changes {
        if let Some((from, to)) = checkbox_range(&lines[index]) {
            lines[index] = replace_range(&lines[index], from, to, &format!("[{state}]"));
        }
    }
    lines.join("\n")
}

/// The document with the checkbox on LINE_NO (1-based) ticked or cleared,
/// and every parent above it brought up to date.
///
/// `None` when that line has no checkbox of its own to set: a parent whose
/// state is aggregated is shown but not toggled, because the next
/// aggregation would undo the tap.
pub fn toggle_checkbox(text: &str, line_no: usize) -> Option<String> {
    let lines: Vec<&str> = text.split('\n').collect();
    let line = lines.get(line_no.checked_sub(1)?)?;
    let (_, checkbox) = list_of(line)?;
    let current = checkbox?;
    if parse(text).iter().any(|block| block.line == line_no && block.derived) {
        return None;
    }
    let (from, to) = checkbox_range(line)?;
    let next = if current == "X" { " " } else { "X" };
    let mut updated: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    updated[line_no - 1] = replace_range(line, from, to, &format!("[{next}]"));
    Some(aggregate_checkboxes(&updated.join("\n")))
}

/// What the caret is sitting in, as a toolbar needs to know it.
///
/// Whether the buttons offered are the list ones or the table ones is a
/// question about the text, not about the editor -- so it is answered
/// here, once, rather than by a second pile of regular expressions on the
/// host's side.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    /// What the line is, with the surrounding state applied: a line inside
    /// a `#+BEGIN_SRC` is block content, whatever it looks like.
    pub kind: String,
    /// 1-based.
    pub line: usize,
    pub column: u32,
    #[serde(skip_serializing_if = "is_zero")]
    pub level: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bullet: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkbox: Option<String>,
    /// The kind of `#+BEGIN_…` block the caret is inside, if any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub block: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub language: Option<String>,
    #[serde(default, skip_serializing_if = "is_false")]
    pub in_drawer: bool,
    /// The whole line, so a host can show or measure it without slicing
    /// the document again.
    pub text: String,
}

/// Where CURSOR (a UTF-16 offset) is.
pub fn context(text: &str, cursor: u32) -> Context {
    let mut offset = 0u32;
    let mut block: Option<(String, Option<String>)> = None;
    let mut in_drawer = false;
    let lines: Vec<&str> = text.split('\n').collect();

    for (index, line) in lines.iter().enumerate() {
        let length = utf16_len(line);
        let last = index + 1 == lines.len();
        let inside = cursor <= offset + length || last;
        let trimmed = line.trim();

        // The state as it is *entering* this line decides what the line
        // is; a BEGIN line itself is still a delimiter, not content.
        let kind = if let Some((kind, _)) = &block {
            if block_end(trimmed).as_deref() == Some(kind.as_str()) {
                "block-end"
            } else {
                "block-content"
            }
        } else if in_drawer {
            if drawer_name(trimmed).as_deref() == Some("END") {
                "drawer-end"
            } else {
                "property"
            }
        } else {
            match classify_line(line) {
                LineType::Empty => "empty",
                LineType::Text => "text",
                LineType::Heading => "heading",
                LineType::Hr => "hr",
                LineType::List => "list",
                LineType::Keyword => "keyword",
                LineType::Table => "table",
                LineType::Planning => "planning",
                LineType::DrawerBegin => "drawer-begin",
                LineType::DrawerEnd => "drawer-end",
                LineType::Property => "property",
                LineType::Logbook => "logbook",
                LineType::BlockBegin => "block-begin",
                LineType::BlockEnd => "block-end",
            }
        };

        if inside {
            let (bullet, checkbox) = match list_of(line) {
                Some((bullet, checkbox)) => (Some(bullet), checkbox),
                None => (None, None),
            };
            let heading = parse_heading_line(line).filter(|_| heading_level(line).is_some());
            return Context {
                kind: kind.to_string(),
                line: index + 1,
                column: cursor.saturating_sub(offset).min(length),
                level: heading_level(line).unwrap_or(0),
                state: heading.and_then(|h| h.state),
                bullet,
                checkbox,
                block: block.as_ref().map(|(kind, _)| kind.clone()),
                language: block.as_ref().and_then(|(_, language)| language.clone()),
                in_drawer,
                text: line.to_string(),
            };
        }

        // Carry the state into the next line.
        match &block {
            Some((kind, _)) if block_end(trimmed).as_deref() == Some(kind.as_str()) => block = None,
            Some(_) => {}
            None => {
                if let Some((kind, params)) = block_begin(trimmed) {
                    let (language, _) = split_block_params(&params);
                    block = Some((kind, language));
                } else if in_drawer {
                    if drawer_name(trimmed).as_deref() == Some("END") || heading_level(line).is_some() {
                        in_drawer = false;
                    }
                } else if drawer_name(trimmed).is_some_and(|name| name != "END") {
                    in_drawer = true;
                }
            }
        }
        offset += length + 1;
    }

    Context { kind: "empty".to_string(), line: 1, ..Context::default() }
}

/// The TODO keywords a heading line may carry, for a host that offers them
/// as a menu.
pub fn todo_states() -> Vec<String> {
    TODO_STATES.iter().map(|state| state.to_string()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(blocks: &[Block]) -> Vec<BlockKind> {
        blocks.iter().map(|b| b.kind).collect()
    }

    #[test]
    fn bold_italic_and_links_become_segments() {
        let out = segments("a *bold* and [[id:20240101T000000][a note]] end");
        let rendered: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "a bold and a note end");
        assert!(out.iter().any(|s| s.bold && s.text == "bold"));
        let link = out.iter().find(|s| s.link.is_some()).expect("link segment");
        assert_eq!(link.text, "a note");
        assert_eq!(link.link.as_deref(), Some("id:20240101T000000"));
    }

    /// Org's other two inline markers, which had no rendering at all:
    /// `=verbatim=` came out with its equals signs showing, and
    /// `_underline_` with its underscores.
    #[test]
    fn verbatim_is_monospace_like_code() {
        let out = segments("try =python3= and ~bash~");
        let rendered: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "try python3 and bash");
        assert!(out.iter().any(|s| s.mono && s.text == "python3"));
        assert!(out.iter().any(|s| s.mono && s.text == "bash"));
    }

    #[test]
    fn underscores_around_a_word_underline_it() {
        let out = segments("that is _important_ here");
        let rendered: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "that is important here");
        assert!(out.iter().any(|s| s.underline && s.text == "important"));
    }

    /// The guard that keeps a marker from eating ordinary text: an
    /// underscore inside a word opens nothing, the same way a slash in a
    /// path does not start italics.
    #[test]
    fn an_underscore_inside_a_word_is_just_an_underscore() {
        let out = segments("the file_path_name stays");
        let rendered: String = out.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(rendered, "the file_path_name stays");
        assert!(out.iter().all(|s| !s.underline));
    }

    #[test]
    fn a_bare_link_shows_its_target() {
        let out = segments("see [[https://example.org]]");
        assert_eq!(out.last().unwrap().text, "https://example.org");
        assert_eq!(out.last().unwrap().link.as_deref(), Some("https://example.org"));
    }

    #[test]
    fn a_file_path_is_not_italic() {
        // The markup rules exist for exactly this: `/usr/bin` would
        // otherwise swallow half the line into one italic run.
        assert!(inline_spans("see /usr/bin/env for details").is_empty());
    }

    #[test]
    fn offsets_are_utf16_code_units() {
        let spans = inline_spans("Grüße *fett*");
        assert_eq!(spans.len(), 1);
        // "Grüße " is six code units even though it is seven bytes.
        assert_eq!(spans[0].from, 6);
    }

    #[test]
    fn a_starred_word_is_not_a_heading() {
        assert_eq!(classify_line("*bold* start"), LineType::Text);
        assert_eq!(classify_line("** Real heading"), LineType::Heading);
    }

    #[test]
    fn paragraph_lines_reflow_into_one_block() {
        let blocks = parse("one line\nand its continuation\n\nsecond");
        assert_eq!(kinds(&blocks), vec![BlockKind::Paragraph, BlockKind::Paragraph]);
        assert_eq!(blocks[0].text, "one line and its continuation");
        assert_eq!(blocks[0].line, 1);
        assert_eq!(blocks[0].end_line, 2);
    }

    #[test]
    fn heading_metadata_folds_into_one_block() {
        let blocks = parse(
            "* TODO Buy milk :shop:\nSCHEDULED: <2026-09-22 Tue>\n:PROPERTIES:\n:CUSTOM_ID: h:abc\n:END:\n:LOGBOOK:\nCLOCK: [2026-09-22 Tue 10:00]\n:END:\ntext",
        );
        assert_eq!(kinds(&blocks), vec![BlockKind::Heading, BlockKind::Metadata, BlockKind::Paragraph]);
        assert_eq!(blocks[0].state.as_deref(), Some("TODO"));
        assert_eq!(blocks[0].tags, vec!["shop".to_string()]);
        assert_eq!(blocks[0].custom_id.as_deref(), Some("h:abc"));
        assert_eq!(blocks[1].planning.as_deref(), Some("SCHEDULED: <2026-09-22 Tue>"));
        assert_eq!(blocks[1].property_count, 1);
        assert_eq!(blocks[1].drawers, vec!["PROPERTIES".to_string(), "LOGBOOK".to_string()]);
    }

    #[test]
    fn an_unclosed_drawer_stops_at_the_next_heading() {
        // A single missing `:END:` used to turn the rest of the file into
        // property lines, hiding real work behind one folded row.
        let blocks = parse("* One\n:PROPERTIES:\n:ID: x\n* Two\ntext");
        assert_eq!(kinds(&blocks), vec![BlockKind::Heading, BlockKind::Metadata, BlockKind::Heading, BlockKind::Paragraph]);
    }

    #[test]
    fn nested_list_depths_count_steps_not_spaces() {
        let blocks = parse("- [ ] parent\n    - [X] child\n    - [ ] other\n- plain");
        let items: Vec<(usize, Option<&str>, bool)> =
            blocks.iter().filter(|b| b.kind == BlockKind::ListItem).map(|b| (b.depth, b.checkbox.as_deref(), b.derived)).collect();
        assert_eq!(items, vec![(0, Some(" "), true), (1, Some("X"), false), (1, Some(" "), false), (0, None, false)]);
        assert_eq!(blocks[0].text, "parent");
    }

    #[test]
    fn numbered_and_lettered_bullets_are_lists() {
        let blocks = parse("1. first\n2. second\na. letter");
        assert_eq!(blocks.iter().filter(|b| b.kind == BlockKind::ListItem).count(), 3);
        assert_eq!(blocks[0].bullet, "1.");
        assert_eq!(blocks[2].bullet, "a.");
    }

    #[test]
    fn a_parenthesis_ends_a_bullet_as_well_as_a_dot() {
        assert_eq!(list_of("1) first"), Some(("1)".to_string(), None)));
        assert_eq!(list_of("  12) [X] done"), Some(("12)".to_string(), Some("X".to_string()))));
        assert_eq!(list_of("b) letter"), Some(("b)".to_string(), None)));
        // Not a bullet: no space after it, or nothing ordered about it.
        assert_eq!(list_of("1)x"), None);
        assert_eq!(list_of("(1) x"), None);
        assert_eq!(list_of("ab) x"), None);
    }

    #[test]
    fn a_source_block_keeps_its_lines_and_language() {
        let blocks = parse("#+BEGIN_SRC python :results output\nprint(1)\n\nprint(2)\n#+END_SRC\nafter");
        assert_eq!(kinds(&blocks), vec![BlockKind::Src, BlockKind::Paragraph]);
        assert_eq!(blocks[0].language.as_deref(), Some("python"));
        assert_eq!(blocks[0].text, "print(1)\n\nprint(2)");
        assert_eq!(blocks[0].end_line, 5);
        assert_eq!(blocks[1].line, 6);
    }

    #[test]
    fn an_unclosed_block_ends_at_the_file() {
        let blocks = parse("#+BEGIN_QUOTE\nstill quoted\n");
        assert_eq!(kinds(&blocks), vec![BlockKind::Quote]);
    }

    #[test]
    fn a_caption_belongs_to_the_image_below_it() {
        let blocks = parse("#+CAPTION: A photo\n[[file:.attachments/x.png]]");
        assert_eq!(kinds(&blocks), vec![BlockKind::Keyword, BlockKind::Image]);
        assert_eq!(blocks[1].url.as_deref(), Some(".attachments/x.png"));
        assert_eq!(blocks[1].caption.as_deref(), Some("A photo"));
    }

    #[test]
    fn a_table_is_one_block_of_rows() {
        let blocks = parse("| a | b |\n|---+---|\n| 1 | 2 |\nafter");
        assert_eq!(kinds(&blocks), vec![BlockKind::Table, BlockKind::Paragraph]);
        assert_eq!(blocks[0].rows.len(), 3);
        assert!(blocks[0].rows[1].separator);
        assert_eq!(blocks[0].rows[2].cells[1].text, "2");
    }

    #[test]
    fn results_collapse_with_their_block() {
        let blocks = parse("#+RESULTS:\n#+begin_example\n1\n2\n#+end_example\nafter");
        assert_eq!(kinds(&blocks), vec![BlockKind::Results, BlockKind::Paragraph]);
        assert_eq!(blocks[0].line_count, 2);
        assert_eq!(blocks[0].text, "1\n2");
    }

    #[test]
    fn colon_results_are_collected_too() {
        let blocks = parse("#+RESULTS:\n: hello\n: world\nafter");
        assert_eq!(blocks[0].text, "hello\nworld");
        assert_eq!(kinds(&blocks), vec![BlockKind::Results, BlockKind::Paragraph]);
    }

    #[test]
    fn file_keywords_stay_as_keywords() {
        let blocks = parse("#+TITLE: A note\n#+FILETAGS: :demo:\n\ntext");
        assert_eq!(kinds(&blocks), vec![BlockKind::Keyword, BlockKind::Keyword, BlockKind::Paragraph]);
        assert_eq!(blocks[0].key.as_deref(), Some("TITLE"));
        assert_eq!(blocks[0].value.as_deref(), Some("A note"));
    }

    #[test]
    fn toggling_a_child_updates_its_parent() {
        let text = "- [ ] parent\n  - [ ] one\n  - [ ] two";
        let once = toggle_checkbox(text, 2).expect("a real checkbox");
        assert_eq!(once, "- [-] parent\n  - [X] one\n  - [ ] two");
        let twice = toggle_checkbox(&once, 3).expect("a real checkbox");
        assert_eq!(twice, "- [X] parent\n  - [X] one\n  - [X] two");
    }

    #[test]
    fn a_derived_parent_cannot_be_toggled() {
        assert!(toggle_checkbox("- [ ] parent\n  - [ ] child", 1).is_none());
        assert!(toggle_checkbox("- plain item", 1).is_none());
    }

    #[test]
    fn aggregation_stops_at_the_end_of_a_list() {
        // The stack used to survive across headings, so typing in one list
        // could rewrite a checkbox in an unrelated one further down.
        let text = "- [ ] first\n  - [X] child\n\n* Heading\n- [ ] untouched";
        assert_eq!(aggregate_checkboxes(text), "- [X] first\n  - [X] child\n\n* Heading\n- [ ] untouched");
    }

    #[test]
    fn a_wide_gap_after_the_bullet_is_measured() {
        assert_eq!(toggle_checkbox("-  [ ] spaced", 1).unwrap(), "-  [X] spaced");
    }

    #[test]
    fn the_context_knows_what_the_caret_is_in() {
        let text = "* TODO Task\n- [ ] item\n#+BEGIN_SRC python\nprint(1)\n#+END_SRC\ntail";
        assert_eq!(context(text, 3).kind, "heading");
        assert_eq!(context(text, 3).state.as_deref(), Some("TODO"));
        assert_eq!(context(text, 14).kind, "list");
        assert_eq!(context(text, 14).checkbox.as_deref(), Some(" "));
        let inside = context(text, 46);
        assert_eq!(inside.kind, "block-content");
        assert_eq!(inside.language.as_deref(), Some("python"));
        assert_eq!(context(text, 63).kind, "text");
        assert!(context(text, 63).block.is_none());
    }

    #[test]
    fn the_context_knows_a_drawer_from_a_property() {
        let text = "* One\n:PROPERTIES:\n:ID: x\n:END:\nafter";
        assert!(context(text, 20).in_drawer);
        assert_eq!(context(text, 20).kind, "property");
        assert!(!context(text, 36).in_drawer);
    }

    #[test]
    fn aggregate_follows_orgs_three_states() {
        assert_eq!(aggregate_checkbox(&["X".into(), "X".into()]), "X");
        assert_eq!(aggregate_checkbox(&[" ".into(), " ".into()]), " ");
        assert_eq!(aggregate_checkbox(&["X".into(), " ".into()]), "-");
    }
}
