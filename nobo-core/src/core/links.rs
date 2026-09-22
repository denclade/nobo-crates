//! What an org link points at.
//!
//! Lifted from the frontend (`components/editor/links/resolveLink.ts`) for
//! the same reason as the rest of the grammar: a link's syntax is part of
//! the file format, not of whichever editor is showing it.
//!
//! Notes link to notes by org's own `id:` type, resolved through this
//! app's Denote-style identifier rather than by path, so a rename does not
//! break them. Denote's own `denote:` type means the same thing and is
//! read the same way -- a compatibility affordance for notes written in
//! Emacs; links this app writes are always `id:`.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LinkKind {
    /// Another note, by Denote identifier.
    Note,
    /// A file next to the vault -- an attachment, usually.
    File,
    /// Something the system browser should open.
    Url,
    /// Anything else. Not an error: a link this app cannot follow is still
    /// text somebody wrote on purpose.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub kind: LinkKind,
    /// The raw target, exactly as written between `[[` and `]`.
    pub raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub identifier: Option<String>,
    /// A `::#h:…` suffix: the CUSTOM_ID of the heading to jump to once the
    /// note is open. The `h:` prefix belongs to the id itself.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub heading: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

fn split_heading_anchor(target: &str) -> (String, Option<String>) {
    match target.split_once("::#") {
        Some((identifier, heading)) if !heading.is_empty() => {
            (identifier.to_string(), Some(heading.to_string()))
        }
        _ => (target.to_string(), None),
    }
}

/// Classifies a raw link target. Never fails: an unrecognized target is a
/// kind, not an error.
pub fn classify(target: &str) -> Link {
    let raw = target.trim().to_string();
    let base = Link { kind: LinkKind::Unknown, raw: raw.clone(), identifier: None, heading: None, path: None, url: None };

    for prefix in ["id:", "denote:"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            let (identifier, heading) = split_heading_anchor(rest);
            return Link { kind: LinkKind::Note, identifier: Some(identifier), heading, ..base };
        }
    }
    let lower = raw.to_lowercase();
    if lower.starts_with("http://") || lower.starts_with("https://") {
        return Link { kind: LinkKind::Url, url: Some(raw.clone()), ..base };
    }
    if let Some(path) = raw.strip_prefix("file:") {
        return Link { kind: LinkKind::File, path: Some(path.to_string()), ..base };
    }
    base
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_note_link_keeps_its_heading_anchor() {
        let link = classify("id:20240118T060608::#h:eed0fb8e");
        assert_eq!(link.kind, LinkKind::Note);
        assert_eq!(link.identifier.as_deref(), Some("20240118T060608"));
        assert_eq!(link.heading.as_deref(), Some("h:eed0fb8e"));
    }

    #[test]
    fn denote_links_read_like_id_links() {
        assert_eq!(classify("denote:20240118T060608").kind, LinkKind::Note);
        assert_eq!(classify("denote:20240118T060608").identifier.as_deref(), Some("20240118T060608"));
    }

    #[test]
    fn urls_files_and_the_rest() {
        assert_eq!(classify("https://example.org").kind, LinkKind::Url);
        assert_eq!(classify("file:.attachments/a.png").path.as_deref(), Some(".attachments/a.png"));
        assert_eq!(classify("mailto:someone@example.org").kind, LinkKind::Unknown);
    }
}
