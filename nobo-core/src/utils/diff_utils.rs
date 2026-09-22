use crate::core::models::DiffChunk;
use similar::{ChangeTag, TextDiff};

pub fn compute_word_diff(original: &str, changed: &str) -> Vec<DiffChunk> {
    let diff = TextDiff::from_words(original, changed);
    diff.iter_all_changes().map(|change| {
        let tag = match change.tag() {
            ChangeTag::Equal => "Equal",
            ChangeTag::Delete => "Delete",
            ChangeTag::Insert => "Insert",
        };
        DiffChunk { tag: tag.to_string(), value: change.value().to_string() }
    }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_texts_produce_only_equal_chunks() {
        let result = compute_word_diff("hello world", "hello world");
        assert!(result.iter().all(|c| c.tag == "Equal"));
    }

    #[test]
    fn detects_insertion() {
        let result = compute_word_diff("hello", "hello world");
        assert!(result.iter().any(|c| c.tag == "Insert"));
    }
}