//! `@home cat:Arbeit -(status:DONE|time:<15m)` -- the filter, as a
//! boolean expression over facets.
//!
//! This was the frontend's (`utils/filterQuery.ts`): a tokenizer, a
//! parser and an evaluator, plus the task schema that says which facets
//! exist. The grammar is not layout -- two front ends typing the same
//! query must get the same rows -- so it lives here, and each front end
//! only renders the chips.
//!
//! # The grammar
//!
//! ```text
//! query   := or
//! or      := and ('|' and)*        -- also the word OR
//! and     := unary+                -- juxtaposition is AND
//! unary   := '-' unary | '(' or ')' | term
//! term    := facet | word
//! facet   := '@' value             -- a GTD context tag
//!          | key ':' value         -- cat:, status:, time:, tag:
//! ```
//!
//! Everything else is a word, matched against the title, case-insensitively.
//! An unknown `key:` is a word too -- a query is typed by a person, and
//! refusing the whole thing because of one typo is worse than matching
//! nothing for that one term.
//!
//! # What it does not do
//!
//! No date facets yet (`due:`, `scheduled:`): the frontend has none either,
//! and a date language is its own decision. `time:` compares the `EFFORT`
//! property, which is what "how long does this take" means here.

use crate::core::models::{has_project_tag, Heading, ORG_CATEGORY_PROPERTY};
use crate::core::task_utils;
use crate::utils::effort_utils;

/// One thing a term can ask about a heading.
#[derive(Debug, Clone, PartialEq)]
pub enum Facet {
    /// `@laptop` -- a GTD context, which is a tag by another name.
    Context(String),
    /// `cat:arbeit` -- the `:CATEGORY:` property.
    Category(String),
    /// `status:next` -- the TODO keyword. `status:open` and
    /// `status:done` are the two groups, since "anything not finished" is
    /// what a person usually means.
    Status(String),
    /// `tag:blog`.
    Tag(String),
    /// `time:<15m`, `time:>1h`, `time:15m-30m`, `time:overtime`.
    Effort(EffortBound),
    /// Anything else: matched against the title.
    Word(String),
}

/// What a `time:` term compares against, in minutes.
#[derive(Debug, Clone, PartialEq)]
pub enum EffortBound {
    Less(u32),
    Greater(u32),
    Between(u32, u32),
    /// Clocked more than estimated -- the one bound that is not a number.
    Overtime,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Term(Facet),
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    /// An empty query matches everything, and saying so as a value keeps
    /// every caller from special-casing it.
    Always,
}

// ---------------------------------------------------------------------
// Tokenizer
// ---------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Minus,
    Or,
    Open,
    Close,
}

/// Splits a query into tokens, keeping quoted runs together.
///
/// `-`, `|`, `(` and `)` are structure only where they are not inside a
/// word: `e-mail` is one word, `-mail` is a negation, and a `(` in the
/// middle of a word (`plan(b)`) stays part of it. That is what a person
/// typing means, and it is why this is a hand-written scanner rather than
/// a split on punctuation.
fn tokenize(input: &str) -> Vec<Token> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;

    let flush = |current: &mut String, tokens: &mut Vec<Token>| {
        if !current.is_empty() {
            let word = std::mem::take(current);
            if word.eq_ignore_ascii_case("or") {
                tokens.push(Token::Or);
            } else if word.eq_ignore_ascii_case("and") {
                // Juxtaposition is already AND; the word is noise a person
                // may still type.
            } else {
                tokens.push(Token::Word(word));
            }
        }
    };

    for ch in input.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            _ if in_quotes => current.push(ch),
            c if c.is_whitespace() => flush(&mut current, &mut tokens),
            '|' => {
                flush(&mut current, &mut tokens);
                tokens.push(Token::Or);
            }
            '(' if current.is_empty() => tokens.push(Token::Open),
            ')' if current.is_empty() => {
                flush(&mut current, &mut tokens);
                tokens.push(Token::Close);
            }
            ')' => {
                // A close paren ends a word AND closes -- `(a b)` has no
                // space before the paren.
                flush(&mut current, &mut tokens);
                tokens.push(Token::Close);
            }
            '-' if current.is_empty() => tokens.push(Token::Minus),
            c => current.push(c),
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

// ---------------------------------------------------------------------
// Facets
// ---------------------------------------------------------------------

/// Reads `15m`, `1h`, `1:30` as minutes. Shares
/// [`effort_utils`](crate::utils::effort_utils) so a bound and an
/// `EFFORT:` property can never disagree about what "1h" is.
fn minutes(raw: &str) -> Option<u32> {
    effort_utils::parse_effort(raw.trim())
}

fn parse_effort(value: &str) -> Option<Facet> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("overtime") || value.eq_ignore_ascii_case("überzogen") {
        return Some(Facet::Effort(EffortBound::Overtime));
    }
    if let Some(rest) = value.strip_prefix('<') {
        return minutes(rest).map(|m| Facet::Effort(EffortBound::Less(m)));
    }
    if let Some(rest) = value.strip_prefix('>') {
        return minutes(rest).map(|m| Facet::Effort(EffortBound::Greater(m)));
    }
    if let Some((low, high)) = value.split_once('-') {
        if let (Some(low), Some(high)) = (minutes(low), minutes(high)) {
            return Some(Facet::Effort(EffortBound::Between(low, high)));
        }
    }
    None
}

/// One word into the facet it names.
///
/// An unrecognised `key:` falls through to [`Facet::Word`] with the whole
/// text -- see the module doc on why a typo must not break the query.
pub fn parse_facet(word: &str) -> Facet {
    if let Some(context) = word.strip_prefix('@') {
        if !context.is_empty() {
            return Facet::Context(context.to_lowercase());
        }
    }
    if let Some((key, value)) = word.split_once(':') {
        if !value.is_empty() {
            match key.to_lowercase().as_str() {
                "cat" | "category" | "bereich" => return Facet::Category(value.to_lowercase()),
                "status" | "state" => return Facet::Status(value.to_uppercase()),
                "tag" => return Facet::Tag(value.to_lowercase()),
                "time" | "effort" | "dauer" => {
                    if let Some(facet) = parse_effort(value) {
                        return facet;
                    }
                }
                _ => {}
            }
        }
    }
    Facet::Word(word.to_lowercase())
}

// ---------------------------------------------------------------------
// Parser
// ---------------------------------------------------------------------

struct Parser {
    tokens: Vec<Token>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.at)
    }

    fn or(&mut self) -> Expr {
        let mut branches = vec![self.and()];
        while matches!(self.peek(), Some(Token::Or)) {
            self.at += 1;
            branches.push(self.and());
        }
        if branches.len() == 1 {
            branches.pop().expect("one branch")
        } else {
            Expr::Or(branches)
        }
    }

    fn and(&mut self) -> Expr {
        let mut parts = Vec::new();
        while let Some(token) = self.peek() {
            if matches!(token, Token::Or | Token::Close) {
                break;
            }
            parts.push(self.unary());
        }
        match parts.len() {
            0 => Expr::Always,
            1 => parts.pop().expect("one part"),
            _ => Expr::And(parts),
        }
    }

    fn unary(&mut self) -> Expr {
        match self.peek() {
            Some(Token::Minus) => {
                self.at += 1;
                Expr::Not(Box::new(self.unary()))
            }
            Some(Token::Open) => {
                self.at += 1;
                let inner = self.or();
                // A missing `)` is forgiven: the query is being typed, and
                // refusing to filter halfway through `(a b` would make the
                // list flicker empty at every keystroke.
                if matches!(self.peek(), Some(Token::Close)) {
                    self.at += 1;
                }
                inner
            }
            Some(Token::Word(_)) => {
                let Some(Token::Word(word)) = self.tokens.get(self.at).cloned() else { unreachable!() };
                self.at += 1;
                Expr::Term(parse_facet(&word))
            }
            // A stray `)` or `|`: skip it rather than stop, same reason.
            Some(_) => {
                self.at += 1;
                Expr::Always
            }
            None => Expr::Always,
        }
    }
}

/// Parses a query. Never fails: a half-typed query filters by what is
/// already readable (see `Parser::unary`).
pub fn parse(query: &str) -> Expr {
    let tokens = tokenize(query);
    if tokens.is_empty() {
        return Expr::Always;
    }
    Parser { tokens, at: 0 }.or()
}

// ---------------------------------------------------------------------
// Evaluation
// ---------------------------------------------------------------------

fn category_of(heading: &Heading) -> Option<&String> {
    heading.properties.iter().find(|(key, _)| key.eq_ignore_ascii_case(ORG_CATEGORY_PROPERTY)).map(|(_, value)| value)
}

fn matches_status(heading: &Heading, wanted: &str) -> bool {
    match wanted {
        // The two groups a person actually asks for, rather than seven
        // keywords they would have to remember.
        "OPEN" | "OFFEN" => !task_utils::is_finished(heading.state.as_deref()),
        "DONE" | "ERLEDIGT" if heading.state.is_none() => false,
        "ANY" => heading.state.is_some(),
        "NONE" | "KEIN" => heading.state.is_none(),
        _ => heading.state.as_deref().is_some_and(|state| state.eq_ignore_ascii_case(wanted)),
    }
}

fn matches_effort(heading: &Heading, bound: &EffortBound) -> bool {
    match bound {
        EffortBound::Overtime => match (heading.effort_minutes, heading.total_clocked_minutes) {
            (Some(effort), Some(clocked)) => clocked > effort,
            _ => false,
        },
        _ => {
            let Some(effort) = heading.effort_minutes else { return false };
            match bound {
                EffortBound::Less(limit) => effort < *limit,
                EffortBound::Greater(limit) => effort > *limit,
                EffortBound::Between(low, high) => effort >= *low && effort <= *high,
                EffortBound::Overtime => unreachable!(),
            }
        }
    }
}

fn matches_facet(heading: &Heading, facet: &Facet) -> bool {
    match facet {
        Facet::Context(value) | Facet::Tag(value) => heading.tags.iter().any(|tag| tag.to_lowercase() == *value),
        Facet::Category(value) => category_of(heading).is_some_and(|c| c.to_lowercase() == *value),
        Facet::Status(value) => matches_status(heading, value),
        Facet::Effort(bound) => matches_effort(heading, bound),
        Facet::Word(value) => {
            heading.title.to_lowercase().contains(value.as_str())
                // A project is often looked for by its tag alone.
                || (value == "project" && has_project_tag(&heading.tags))
        }
    }
}

/// Whether one heading matches a parsed query.
pub fn matches(expr: &Expr, heading: &Heading) -> bool {
    match expr {
        Expr::Always => true,
        Expr::Term(facet) => matches_facet(heading, facet),
        Expr::Not(inner) => !matches(inner, heading),
        Expr::And(parts) => parts.iter().all(|part| matches(part, heading)),
        Expr::Or(parts) => parts.iter().any(|part| matches(part, heading)),
    }
}

/// Parse once, filter a list -- what a caller with a text field wants.
pub fn filter_headings(headings: Vec<Heading>, query: &str) -> Vec<Heading> {
    let expr = parse(query);
    if expr == Expr::Always {
        return headings;
    }
    headings.into_iter().filter(|heading| matches(&expr, heading)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn task(title: &str, state: Option<&str>, tags: &[&str], category: Option<&str>, effort: Option<u32>) -> Heading {
        let mut properties = HashMap::new();
        if let Some(category) = category {
            properties.insert(ORG_CATEGORY_PROPERTY.to_string(), category.to_string());
        }
        Heading {
            title: title.to_string(),
            state: state.map(str::to_string),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            properties,
            effort_minutes: effort,
            ..Default::default()
        }
    }

    #[test]
    fn juxtaposition_is_and() {
        let expr = parse("cat:Arbeit status:NEXT");
        let hit = task("x", Some("NEXT"), &[], Some("Arbeit"), None);
        let miss = task("x", Some("TODO"), &[], Some("Arbeit"), None);

        assert!(matches(&expr, &hit));
        assert!(!matches(&expr, &miss));
    }

    #[test]
    fn a_leading_minus_negates_and_a_pipe_alternates() {
        let expr = parse("-(status:DONE|status:WONT-DO)");

        assert!(matches(&expr, &task("x", Some("NEXT"), &[], None, None)));
        assert!(!matches(&expr, &task("x", Some("DONE"), &[], None, None)));
        assert!(!matches(&expr, &task("x", Some("WONT-DO"), &[], None, None)));
    }

    /// The one thing a hand-written scanner is for: a hyphen inside a word
    /// is not a negation.
    #[test]
    fn a_hyphen_inside_a_word_stays_in_the_word() {
        assert_eq!(tokenize("e-mail"), vec![Token::Word("e-mail".into())]);
        assert_eq!(tokenize("-mail"), vec![Token::Minus, Token::Word("mail".into())]);
    }

    #[test]
    fn a_context_is_a_tag_and_status_open_is_a_group() {
        let expr = parse("@laptop status:open");

        assert!(matches(&expr, &task("x", Some("NEXT"), &["laptop"], None, None)));
        assert!(!matches(&expr, &task("x", Some("DONE"), &["laptop"], None, None)));
        assert!(!matches(&expr, &task("x", Some("NEXT"), &["buero"], None, None)));
    }

    #[test]
    fn effort_bounds_read_the_same_units_as_the_property() {
        assert!(matches(&parse("time:<30m"), &task("x", None, &[], None, Some(15))));
        assert!(!matches(&parse("time:<30m"), &task("x", None, &[], None, Some(45))));
        assert!(matches(&parse("time:>1h"), &task("x", None, &[], None, Some(90))));
        assert!(matches(&parse("time:15m-30m"), &task("x", None, &[], None, Some(30))));
        assert!(!matches(&parse("time:<30m"), &task("x", None, &[], None, None)), "no estimate is not 'under 30m'");
    }

    /// A query is typed one character at a time; every intermediate state
    /// has to filter something rather than blow up.
    #[test]
    fn a_half_typed_query_still_filters() {
        assert_eq!(parse(""), Expr::Always);
        assert!(matches(&parse("(cat:Arbeit"), &task("x", None, &[], Some("Arbeit"), None)));
        assert!(matches(&parse("cat:"), &task("cat: something", None, &[], None, None)), "a bare key is a word");
    }

    #[test]
    fn an_unknown_key_is_a_word_not_an_error() {
        let expr = parse("wurst:brot");
        assert!(matches(&expr, &task("Wurst:Brot kaufen", None, &[], None, None)));
        assert!(!matches(&expr, &task("Brot kaufen", None, &[], None, None)));
    }

    #[test]
    fn quoted_words_stay_together() {
        let expr = parse("\"neues modul\"");
        assert!(matches(&expr, &task("Neues Modul: KI im Alltag", None, &[], None, None)));
        assert!(!matches(&expr, &task("Neues Konzept, Modul folgt", None, &[], None, None)));
    }
}
