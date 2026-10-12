//! Just enough HTML to read a web page's meta data, and no more.
//!
//! Not a parser, and not meant to become one. What reads pages here -- the
//! cookbook's recipe import, the bookmarks' preview -- wants a page's
//! title and description, its JSON-LD blocks, an absolute address for a
//! relative one, and now and then one element a site refiner knows by
//! its class. For that, finding an element and the end that matches it
//! is the whole job -- a DOM would be a large dependency for a few
//! lookups that a page redesign breaks either way.
//!
//! In the core because it is pure (text in, text out) and because two
//! features need it, which may not reach into each other.

/// One element found by its class: the markup between its start and end
/// tags.
#[derive(Debug, Clone, Copy)]
pub struct Element<'a> {
    pub inner: &'a str,
}

impl<'a> Element<'a> {
    /// Its text, the way a reader sees it (see `text_of`).
    pub fn text(&self) -> String {
        text_of(self.inner)
    }

    /// The elements inside this one carrying CLASS.
    pub fn find(&self, class: &str) -> Vec<Element<'a>> {
        elements(self.inner, class)
    }

    /// The first element inside this one carrying CLASS, as text.
    pub fn text_of_first(&self, class: &str) -> Option<String> {
        self.find(class).first().map(Element::text).filter(|text| !text.is_empty())
    }
}

/// The body of every `<script type="application/ld+json">` on the page.
pub fn json_ld_blocks(html: &str) -> Vec<&str> {
    // Lowercased once, for the searching; ASCII case does not move a
    // byte, so every offset found in it holds in the original too.
    let lower = html.to_ascii_lowercase();
    let mut blocks = Vec::new();
    let mut at = 0;
    while let Some(start) = lower[at..].find("<script").map(|s| at + s) {
        let Some(open_end) = lower[start..].find('>').map(|e| start + e) else { break };
        let body = open_end + 1;
        let Some(close) = lower[body..].find("</script").map(|c| body + c) else { break };
        if lower[start..open_end].contains("application/ld+json") {
            blocks.push(html[body..close].trim());
        }
        at = close;
    }
    blocks
}

/// The `content` of `<meta property="NAME">` (or `name="NAME"`) --
/// Open Graph's `og:image` and friends.
pub fn meta_content(html: &str, name: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let mut at = 0;
    while let Some(start) = lower[at..].find("<meta").map(|s| at + s) {
        let end = lower[start..].find('>').map(|e| start + e)?;
        let tag = &html[start..end];
        let names_it = [format!("property=\"{name}\""), format!("name=\"{name}\""), format!("property='{name}'")]
            .iter()
            .any(|attribute| tag.contains(attribute.as_str()));
        if names_it {
            return attribute(tag, "content").map(|value| decode_entities(&value));
        }
        at = end;
    }
    None
}

/// HREF as an absolute web address, read relative to the page at BASE --
/// the way a browser reads `src="/bilder/suppe.jpg"`.
///
/// Pages give their picture as a path more often than one would think,
/// in the recipe data and in `og:image` alike; dropping those lost the
/// picture of a recipe that plainly had one. `None` for anything that is
/// not a web address once resolved (`data:`, `javascript:`), and for a
/// BASE that is not one either.
pub fn absolute_url(base: &str, href: &str) -> Option<String> {
    let href = href.trim();
    let (scheme, rest) = base.split_once("://")?;
    if !(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https")) || href.is_empty() {
        return None;
    }
    let lower = href.to_ascii_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Some(href.to_string());
    }
    if let Some(after) = href.strip_prefix("//") {
        return Some(format!("{scheme}://{after}"));
    }
    // Any other scheme: not something to download.
    let has_scheme = href.find(':').is_some_and(|colon| {
        let candidate = &href[..colon];
        candidate.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && candidate.chars().all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    });
    if has_scheme {
        return None;
    }

    let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let origin = format!("{scheme}://{}", &rest[..host_end]);
    let full_path = &rest[host_end..];
    let path = &full_path[..full_path.find(['?', '#']).unwrap_or(full_path.len())];

    if href.starts_with('?') || href.starts_with('#') {
        return Some(format!("{origin}{path}{href}"));
    }
    let joined = if href.starts_with('/') {
        href.to_string()
    } else {
        let directory = &path[..path.rfind('/').map(|slash| slash + 1).unwrap_or(0)];
        let directory = if directory.is_empty() { "/" } else { directory };
        format!("{directory}{href}")
    };
    Some(format!("{origin}{}", without_dot_segments(&joined)))
}

/// `/a/b/../c/./d` as `/a/c/d`, query and fragment left alone.
fn without_dot_segments(path: &str) -> String {
    let split = path.find(['?', '#']).unwrap_or(path.len());
    let (path, tail) = path.split_at(split);
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = path.split('/').skip(1).collect();
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        match *part {
            "." => {
                if last {
                    segments.push("");
                }
            }
            ".." => {
                segments.pop();
                if last {
                    segments.push("");
                }
            }
            other => segments.push(other),
        }
    }
    format!("/{}{tail}", segments.join("/"))
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let at = tag.find(&format!("{name}="))? + name.len() + 1;
    let value = &tag[at..];
    let quote = value.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    value[1..].split(quote).next().map(str::to_string)
}

/// The text of the first element named TAG (`h1`), whatever its
/// attributes -- for the one element on a page that needs no class to be
/// found.
pub fn first_tag_text(html: &str, tag: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let start = find_tag(&lower, &open)?;
    let content = start + lower[start..].find('>')? + 1;
    let end = matching_end(html, content, tag);
    Some(text_of(&html[content..end])).filter(|text| !text.is_empty())
}

/// Every element in HTML whose class list contains CLASS -- the whole
/// word, so `wprm-recipe-ingredient` does not also find
/// `wprm-recipe-ingredients`.
pub fn elements<'a>(html: &'a str, class: &str) -> Vec<Element<'a>> {
    let mut found = Vec::new();
    let mut at = 0;
    while let Some(offset) = html[at..].find('<') {
        let start = at + offset;
        let Some(tag) = open_tag(&html[start..]) else {
            at = start + 1;
            continue;
        };
        let content_start = start + tag.length;
        if has_class(tag.attributes, class) {
            let end = matching_end(html, content_start, &tag.name);
            found.push(Element { inner: &html[content_start..end] });
            // Not past the end: an element of the same class can sit
            // inside this one and is a match of its own.
        }
        at = content_start;
    }
    found
}

/// Markup as the text a reader sees: tags gone, entities decoded,
/// whitespace collapsed to single spaces.
///
/// A block tag counts as a space and an inline one as nothing, so
/// `<li>A</li><li>B</li>` reads "A B" and `<strong>T</strong>ipp` reads
/// "Tipp".
pub fn text_of(markup: &str) -> String {
    let mut text = String::with_capacity(markup.len());
    let mut rest = markup;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        let after = &rest[start..];
        let Some(end) = after.find('>') else {
            // A lone `<` in prose ("a < b"): it is text.
            text.push_str(after);
            rest = "";
            break;
        };
        let name = tag_name(&after[1..end]);
        if is_block(&name) {
            text.push(' ');
        }
        rest = &after[end + 1..];
    }
    text.push_str(rest);
    collapse_whitespace(&decode_entities(&text))
}

/// Character references, named and numeric.
///
/// The named ones are the ones recipe pages actually use -- fractions,
/// umlauts, typographic quotes and dashes -- rather than HTML's two
/// thousand. Cookidoo writes `&frac12;` into its JSON-LD, which is how
/// this ended up mattering outside of markup too. An unknown one stays
/// as it was written: a visible `&foo;` is a better failure than a
/// silently dropped word.
pub fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(';').filter(|end| *end <= 10);
        let decoded = end.and_then(|end| decode_one(&after[..end]).map(|c| (c, end)));
        match decoded {
            Some((c, end)) => {
                out.push(c);
                rest = &after[end + 1..];
            }
            None => {
                out.push('&');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

fn decode_one(name: &str) -> Option<char> {
    if let Some(number) = name.strip_prefix('#') {
        let code = match number.strip_prefix('x').or_else(|| number.strip_prefix('X')) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => number.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        "shy" => '\u{ad}',
        "frac12" => '½',
        "frac14" => '¼',
        "frac34" => '¾',
        "frac13" => '⅓',
        "frac23" => '⅔',
        "auml" => 'ä',
        "ouml" => 'ö',
        "uuml" => 'ü',
        "Auml" => 'Ä',
        "Ouml" => 'Ö',
        "Uuml" => 'Ü',
        "szlig" => 'ß',
        "eacute" => 'é',
        "egrave" => 'è',
        "agrave" => 'à',
        "ccedil" => 'ç',
        "deg" => '°',
        "times" => '×',
        "ndash" => '–',
        "mdash" => '—',
        "hellip" => '…',
        "lsquo" => '‘',
        "rsquo" => '’',
        "ldquo" => '“',
        "rdquo" => '”',
        "bdquo" => '„',
        "euro" => '€',
        _ => return None,
    })
}

pub fn collapse_whitespace(text: &str) -> String {
    // A soft hyphen is invisible where it was meant to be; in a recipe
    // line it is only noise.
    text.split_whitespace().collect::<Vec<_>>().join(" ").replace('\u{ad}', "")
}

struct OpenTag<'a> {
    name: String,
    attributes: &'a str,
    /// Up to and including the `>`.
    length: usize,
}

fn open_tag(at: &str) -> Option<OpenTag<'_>> {
    let first = at[1..].chars().next()?;
    if !first.is_ascii_alphabetic() {
        // A closing tag, a comment, a doctype -- or a `<` in text.
        return None;
    }
    let end = at.find('>')?;
    let inside = &at[1..end];
    let name = tag_name(inside);
    Some(OpenTag { attributes: &inside[name.len()..], name, length: end + 1 })
}

fn tag_name(inside: &str) -> String {
    inside
        .trim_start_matches('/')
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '-')
        .collect::<String>()
        .to_ascii_lowercase()
}

fn has_class(attributes: &str, class: &str) -> bool {
    let Some(at) = find_ascii_ci(attributes, "class=") else { return false };
    let value = &attributes[at + "class=".len()..];
    let list = match value.chars().next() {
        Some(quote @ ('"' | '\'')) => value[1..].split(quote).next().unwrap_or(""),
        _ => value.split(|c: char| c.is_whitespace() || c == '>').next().unwrap_or(""),
    };
    list.split_whitespace().any(|word| word == class)
}

/// Where the element opened just before FROM ends: the start of its
/// matching end tag, counting nested elements of the same name. The end
/// of the input when it never closes.
fn matching_end(html: &str, from: usize, name: &str) -> usize {
    let open = format!("<{name}");
    let close = format!("</{name}");
    let mut depth = 1;
    // Offsets below are relative to FROM (see json_ld_blocks on why the
    // lowercased copy can be indexed like the original).
    let lower = html[from..].to_ascii_lowercase();
    let mut at = 0;
    while depth > 0 {
        let next_open = find_tag(&lower[at..], &open).map(|o| at + o);
        let Some(next_close) = find_tag(&lower[at..], &close).map(|c| at + c) else {
            return html.len();
        };
        match next_open {
            Some(o) if o < next_close => {
                depth += 1;
                at = o + open.len();
            }
            _ => {
                depth -= 1;
                if depth == 0 {
                    return from + next_close;
                }
                at = next_close + close.len();
            }
        }
    }
    html.len()
}

/// `<div` but not `<divider`: the name must end where the match does.
fn find_tag(haystack: &str, tag: &str) -> Option<usize> {
    let mut at = 0;
    while let Some(offset) = haystack[at..].find(tag) {
        let found = at + offset;
        let next = haystack[found + tag.len()..].chars().next();
        if matches!(next, None | Some('>' | '/' | ' ' | '\t' | '\n' | '\r')) {
            return Some(found);
        }
        at = found + tag.len();
    }
    None
}

fn is_block(name: &str) -> bool {
    matches!(
        name,
        "br" | "p" | "div" | "li" | "ul" | "ol" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "tr" | "td" | "section"
    )
}

fn find_ascii_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_class_is_matched_as_a_whole_word() {
        let html = r#"<ul class="wprm-recipe-ingredients"><li class="wprm-recipe-ingredient">A</li><li class="x wprm-recipe-ingredient">B</li></ul>"#;
        let found: Vec<String> = elements(html, "wprm-recipe-ingredient").iter().map(Element::text).collect();
        assert_eq!(found, ["A", "B"]);
    }

    #[test]
    fn an_element_ends_at_its_own_end_tag_not_a_nested_one() {
        let html = r#"<div class="group"><div>inner</div><span>tail</span></div><div>after</div>"#;
        let found = elements(html, "group");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].text(), "inner tail");
    }

    #[test]
    fn a_relative_address_is_read_against_the_page() {
        let page = "https://example.org/rezepte/suppe/?ref=x#top";
        assert_eq!(absolute_url(page, "/bilder/a.jpg").as_deref(), Some("https://example.org/bilder/a.jpg"));
        assert_eq!(absolute_url(page, "a.jpg").as_deref(), Some("https://example.org/rezepte/suppe/a.jpg"));
        assert_eq!(absolute_url(page, "../../a.jpg?w=800").as_deref(), Some("https://example.org/a.jpg?w=800"));
        assert_eq!(absolute_url(page, "//cdn.example.org/a.jpg").as_deref(), Some("https://cdn.example.org/a.jpg"));
        assert_eq!(absolute_url("https://example.org", "a.jpg").as_deref(), Some("https://example.org/a.jpg"));
        assert_eq!(absolute_url(page, "https://cdn.example/b.png").as_deref(), Some("https://cdn.example/b.png"));
    }

    #[test]
    fn what_is_not_a_web_address_stays_out() {
        assert_eq!(absolute_url("https://example.org/", "data:image/png;base64,AAAA"), None);
        assert_eq!(absolute_url("https://example.org/", "javascript:void(0)"), None);
        assert_eq!(absolute_url("https://example.org/", "  "), None);
        assert_eq!(absolute_url("file:///tmp/x.html", "a.jpg"), None);
    }

    #[test]
    fn text_keeps_inline_markup_together_and_separates_blocks() {
        assert_eq!(text_of("<em><strong>*</strong></em><u>Tipp</u>: x"), "*Tipp: x");
        assert_eq!(text_of("<li>A</li><li>B</li>"), "A B");
        assert_eq!(text_of("a < b"), "a < b");
    }

    #[test]
    fn entities_decode_named_and_numeric_and_leave_unknown_ones() {
        assert_eq!(decode_entities("&frac12; TL &amp; &#32;&#x41;"), "½ TL &  A");
        assert_eq!(decode_entities("&foo; R&D"), "&foo; R&D");
    }

    #[test]
    fn the_open_graph_image_is_read_whatever_attribute_order() {
        let html = r#"<meta content="https://x/a.jpg" property="og:image"><meta property="og:title" content="T">"#;
        assert_eq!(meta_content(html, "og:image").as_deref(), Some("https://x/a.jpg"));
        assert_eq!(meta_content(html, "og:description"), None);
    }

    #[test]
    fn the_first_heading_is_found_without_a_class() {
        assert_eq!(first_tag_text("<header><h1 data-v-1>Bihun-Suppe <small>x</small></h1></header>", "h1").as_deref(), Some("Bihun-Suppe x"));
        assert_eq!(first_tag_text("<h2>no</h2>", "h1"), None);
    }

    #[test]
    fn json_ld_is_found_whatever_else_the_script_tag_says() {
        let html = r#"<script>var x;</script><script type="application/ld+json" class="yoast">{"a":1}</script>"#;
        assert_eq!(json_ld_blocks(html), [r#"{"a":1}"#]);
    }
}
