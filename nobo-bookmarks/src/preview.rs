// SPDX-FileCopyrightText: 2026 Dennis Burgermeister
// SPDX-License-Identifier: GPL-3.0-or-later

//! What a page says about itself -- read from its HTML, nothing fetched
//! here (see `fetch` in commands). Pure, so it is tested on saved pages.

use nobo_core::utils::html::{collapse_whitespace, decode_entities, first_tag_text, meta_content};

use crate::model::Preview;
use crate::url;

/// The page's title and description: Open Graph first (what the page
/// chose to show when shared), then `<title>` and `<meta name=
/// "description">`. Both on one line, entities decoded; an empty one is
/// none.
pub fn read(page_url: &str, html: &str) -> Preview {
    let clean = |text: String| Some(collapse_whitespace(&decode_entities(&text))).filter(|text| !text.is_empty());
    let title = meta_content(html, "og:title").and_then(clean).or_else(|| first_tag_text(html, "title").and_then(clean));
    let description = meta_content(html, "og:description").and_then(clean).or_else(|| meta_content(html, "description").and_then(clean));
    Preview { url: page_url.to_string(), host: url::host(page_url), title, description }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_graph_comes_first() {
        let html = r#"<html><head><title>reqwest - Rust</title>
            <meta property="og:title" content="reqwest &amp; friends">
            <meta name="description" content="An HTTP client.">
            </head></html>"#;
        let preview = read("https://docs.rs/reqwest", html);
        assert_eq!(preview.title.as_deref(), Some("reqwest & friends"));
        assert_eq!(preview.description.as_deref(), Some("An HTTP client."));
        assert_eq!(preview.host, "docs.rs");
    }

    #[test]
    fn the_title_tag_when_there_is_nothing_else() {
        let preview = read("https://x.org", "<title>\n  Ein\n  Titel </title>");
        assert_eq!(preview.title.as_deref(), Some("Ein Titel"));
        assert_eq!(preview.description, None);
    }

    #[test]
    fn a_page_without_either_has_none() {
        let preview = read("https://x.org/a", "<p>nichts</p>");
        assert_eq!(preview.title, None);
    }
}
