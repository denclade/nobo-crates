//! Markdown in, org out -- for the one moment it matters: a paste.
//!
//! Lifted from nobo's `components/editor/paste`, minus the half that
//! walks HTML: a phone's clipboard hands over text, and the desktop's
//! rich-text path (turndown, a DOM parse) has nothing to run on here. The
//! line-based converter is the same one, and it stays deliberately
//! partial -- headings, emphasis, code, links, lists and quotes. Tables,
//! reference links and footnotes fall through as the text they are,
//! which reads better than a half-converted table.
//!
//! Detection is separate from conversion on purpose: pasting is the
//! user's action, and converting it silently would be the editor deciding
//! what they meant.

/// Signals that are vanishingly unlikely in accidental prose -- any one
/// is enough.
fn has_strong_signal(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        trimmed.starts_with("```")
            || atx_heading(line).is_some()
    }) || has_markdown_link(text)
}

/// Signals that happen constantly in normal writing -- two different ones
/// are needed before this counts as Markdown.
fn weak_signals(text: &str) -> usize {
    let lines: Vec<&str> = text.lines().collect();
    let mut count = 0;
    if wrapped(text, "**") {
        count += 1;
    }
    if wrapped(text, "~~") {
        count += 1;
    }
    if wrapped(text, "`") {
        count += 1;
    }
    if consecutive(&lines, |line| unordered_item(line).is_some()) {
        count += 1;
    }
    if consecutive(&lines, |line| ordered_item(line).is_some()) {
        count += 1;
    }
    if lines.iter().any(|line| line.trim_start().starts_with('>')) {
        count += 1;
    }
    count
}

/// Whether a run of at least two lines in a row matches.
fn consecutive(lines: &[&str], matches: impl Fn(&str) -> bool) -> bool {
    lines.windows(2).any(|pair| matches(pair[0]) && matches(pair[1]))
}

/// `MARKER text MARKER` somewhere in the text, with something between.
fn wrapped(text: &str, marker: &str) -> bool {
    let mut rest = text;
    while let Some(open) = rest.find(marker) {
        let after = &rest[open + marker.len()..];
        match after.find(marker) {
            Some(close) if close > 0 && !after[..close].contains('\n') => return true,
            Some(close) => rest = &after[close + marker.len()..],
            None => return false,
        }
    }
    false
}

fn has_markdown_link(text: &str) -> bool {
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let after = &rest[open + 1..];
        if let Some(close) = after.find(']') {
            if after[close + 1..].starts_with('(') && after[close + 2..].contains(')') && close > 0 {
                return true;
            }
            rest = &after[close + 1..];
        } else {
            return false;
        }
    }
    false
}

/// Pasted text this app would offer to convert.
///
/// Above 200 kB it says no without looking: whatever was pasted at that
/// size, nobody is waiting for a conversion of it.
pub fn looks_like_markdown(text: &str) -> bool {
    if text.len() > 200_000 {
        return false;
    }
    has_strong_signal(text) || weak_signals(text) >= 2
}

fn atx_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 6 {
        return None;
    }
    let rest = &line[hashes..];
    let body = rest.strip_prefix(' ')?;
    (!body.trim().is_empty()).then(|| (hashes, body.trim_start()))
}

fn unordered_item(line: &str) -> Option<(String, &str)> {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = &line[indent.len()..];
    let body = rest.strip_prefix("- ").or_else(|| rest.strip_prefix("* ")).or_else(|| rest.strip_prefix("+ "))?;
    (!body.trim().is_empty()).then(|| (indent, body))
}

fn ordered_item(line: &str) -> Option<(String, String, &str)> {
    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let rest = &line[indent.len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    let body = rest[digits.len()..].strip_prefix(". ")?;
    (!body.trim().is_empty()).then(|| (indent, digits, body))
}

/// The shallowest heading in the text, so a fragment that starts at `###`
/// does not come out three levels deep.
///
/// Relative nesting is kept either way, since every heading shifts by the
/// same amount. Fenced code is skipped: a `# comment` in a snippet would
/// otherwise pull the base level back to one.
fn base_level(lines: &[&str]) -> usize {
    let mut fenced = false;
    let mut levels = Vec::new();
    for line in lines {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some((level, _)) = atx_heading(line) {
            levels.push(level);
        }
    }
    levels.into_iter().min().unwrap_or(1)
}

/// Inline markup, one span at a time.
///
/// Each conversion stashes its finished org text behind a placeholder
/// before the next runs, so the italic pass never re-matches inside
/// something the bold pass just wrote -- `**bold**` becomes `*bold*`,
/// which is exactly what the italic rule is looking for.
fn convert_inline(text: &str) -> String {
    let mut stash: Vec<String> = Vec::new();
    let mut out = text.to_string();

    // Code first: nothing inside it is markup.
    out = replace_wrapped(&out, "`", "`", |inner| format!("~{inner}~"), &mut stash);
    out = replace_links(&out, &mut stash);
    out = replace_wrapped(&out, "**", "**", |inner| format!("*{inner}*"), &mut stash);
    out = replace_wrapped(&out, "__", "__", |inner| format!("*{inner}*"), &mut stash);
    out = replace_wrapped(&out, "~~", "~~", |inner| format!("+{inner}+"), &mut stash);
    out = replace_wrapped(&out, "*", "*", |inner| format!("/{inner}/"), &mut stash);
    out = replace_wrapped(&out, "_", "_", |inner| format!("/{inner}/"), &mut stash);

    for (index, rendered) in stash.iter().enumerate() {
        out = out.replace(&placeholder(index), rendered);
    }
    out
}

fn placeholder(index: usize) -> String {
    format!("\u{0}{index}\u{0}")
}

fn replace_wrapped(
    text: &str,
    open: &str,
    close: &str,
    render: impl Fn(&str) -> String,
    stash: &mut Vec<String>,
) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after = &rest[start + open.len()..];
        let Some(end) = after.find(close) else { break };
        let inner = &after[..end];
        if inner.is_empty() || inner.contains('\n') || inner.contains('\u{0}') {
            out.push_str(&rest[..start + open.len()]);
            rest = after;
            continue;
        }
        out.push_str(&rest[..start]);
        stash.push(render(inner));
        out.push_str(&placeholder(stash.len() - 1));
        rest = &after[end + close.len()..];
    }
    out.push_str(rest);
    out
}

fn replace_links(text: &str, stash: &mut Vec<String>) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find('[') {
        let after = &rest[start + 1..];
        let Some(label_end) = after.find(']') else { break };
        let tail = &after[label_end + 1..];
        if !tail.starts_with('(') {
            out.push_str(&rest[..start + 1]);
            rest = after;
            continue;
        }
        let Some(url_end) = tail.find(')') else { break };
        let label = &after[..label_end];
        let url = &tail[1..url_end];
        out.push_str(&rest[..start]);
        stash.push(format!("[[{url}][{label}]]"));
        out.push_str(&placeholder(stash.len() - 1));
        rest = &tail[url_end + 1..];
    }
    out.push_str(rest);
    out
}

/// Markdown as org.
pub fn markdown_to_org(markdown: &str) -> String {
    let normalized = markdown.replace("\r\n", "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    let base = base_level(&lines);
    let mut out: Vec<String> = Vec::new();
    let mut index = 0usize;

    while index < lines.len() {
        let line = lines[index];
        let trimmed = line.trim_start();

        if let Some(language) = trimmed.strip_prefix("```") {
            let language = language.trim();
            index += 1;
            let mut body = Vec::new();
            while index < lines.len() && !lines[index].trim_start().starts_with("```") {
                body.push(lines[index].to_string());
                index += 1;
            }
            index += 1; // the closing fence
            out.push(if language.is_empty() {
                "#+BEGIN_SRC".to_string()
            } else {
                format!("#+BEGIN_SRC {language}")
            });
            out.extend(body);
            out.push("#+END_SRC".to_string());
            continue;
        }

        if trimmed.starts_with('>') {
            let mut body = Vec::new();
            while index < lines.len() && lines[index].trim_start().starts_with('>') {
                let stripped = lines[index].trim_start().trim_start_matches('>');
                body.push(convert_inline(stripped.strip_prefix(' ').unwrap_or(stripped)));
                index += 1;
            }
            out.push("#+BEGIN_QUOTE".to_string());
            out.extend(body);
            out.push("#+END_QUOTE".to_string());
            continue;
        }

        if let Some((level, body)) = atx_heading(line) {
            let stars = "*".repeat(level.saturating_sub(base) + 1);
            out.push(format!("{stars} {}", convert_inline(body)));
            index += 1;
            continue;
        }

        if let Some((indent, body)) = unordered_item(line) {
            out.push(format!("{indent}- {}", convert_inline(body)));
            index += 1;
            continue;
        }

        if let Some((indent, number, body)) = ordered_item(line) {
            out.push(format!("{indent}{number}. {}", convert_inline(body)));
            index += 1;
            continue;
        }

        out.push(convert_inline(line));
        index += 1;
    }

    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fenced_block_or_a_heading_is_enough_on_its_own() {
        assert!(looks_like_markdown("# Title\n\ntext"));
        assert!(looks_like_markdown("```js\nlet a = 1\n```"));
        assert!(looks_like_markdown("see [the docs](https://example.org)"));
    }

    #[test]
    fn one_stray_asterisk_is_not_markdown() {
        assert!(!looks_like_markdown("a *word* in passing"));
        assert!(!looks_like_markdown("Dear Sir,\n\nthanks for the 5 * 3 = 15."));
        // Two different weak signals together are.
        assert!(looks_like_markdown("**bold** and `code`"));
    }

    #[test]
    fn a_shopping_list_counts_as_one_signal_not_two() {
        assert!(!looks_like_markdown("- milk\n- bread\n- eggs"));
        assert!(looks_like_markdown("- milk\n- bread\n\n> and a note"));
    }

    #[test]
    fn headings_are_rebased_to_the_shallowest_one() {
        // A fragment copied out of the middle of a page starts at ###; as
        // an absolute level it would arrive three deep.
        assert_eq!(markdown_to_org("### One\n#### Two"), "* One\n** Two");
        assert_eq!(markdown_to_org("# One\n### Three"), "* One\n*** Three");
    }

    #[test]
    fn inline_markup_does_not_convert_twice() {
        // `**bold**` becomes `*bold*`, which is exactly what the italic
        // pass looks for -- the placeholder is what stops it.
        assert_eq!(markdown_to_org("**bold** and *slanted*"), "*bold* and /slanted/");
        assert_eq!(markdown_to_org("~~gone~~ and `kept`"), "+gone+ and ~kept~");
    }

    #[test]
    fn code_spans_keep_their_contents() {
        assert_eq!(markdown_to_org("use `a * b` here"), "use ~a * b~ here");
    }

    #[test]
    fn links_lists_and_quotes() {
        assert_eq!(markdown_to_org("[docs](https://x.org)"), "[[https://x.org][docs]]");
        assert_eq!(markdown_to_org("* one\n2. two"), "- one\n2. two");
        assert_eq!(markdown_to_org("> quoted\n> more"), "#+BEGIN_QUOTE\nquoted\nmore\n#+END_QUOTE");
    }

    #[test]
    fn a_fence_keeps_its_language_and_its_body_verbatim() {
        assert_eq!(
            markdown_to_org("```python\nprint(\"**hi**\")\n```"),
            "#+BEGIN_SRC python\nprint(\"**hi**\")\n#+END_SRC"
        );
    }
}
