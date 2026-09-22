use crate::t;

/// Consistent "couldn't find X at line N" message, used across the heading
/// commands wherever a LINE_NO lookup can fail because the underlying
/// heading has meanwhile moved, been deleted, or was never there.
pub fn not_found_at_line(subject: &str, line_no: usize) -> String {
    t!(
        "{subject} bei Zeile {line_no} nicht (mehr) gefunden",
        "{subject} is no longer at line {line_no}"
    )
}

/// What a write is about, for the messages that name it.
///
/// `heading_identity::resolve_target` and friends take this as a plain
/// `&str` and format it into a sentence, so it has to be in the same
/// language as the sentence -- these are the four things it can be, in one
/// place, instead of the same word written out at sixteen call sites.
pub mod subject {
    use crate::t;

    pub fn heading() -> String {
        t!("Überschrift", "heading")
    }

    pub fn parent_heading() -> String {
        t!("Übergeordnete Überschrift", "parent heading")
    }

    pub fn source_heading() -> String {
        t!("Quell-Überschrift", "source heading")
    }

    pub fn target_heading() -> String {
        t!("Ziel-Überschrift", "target heading")
    }

    pub fn sibling_heading() -> String {
        t!("Nachbar-Überschrift", "neighbouring heading")
    }
}
