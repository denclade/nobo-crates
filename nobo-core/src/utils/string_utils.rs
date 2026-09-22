/*
LINE is e.g. CLOSED: [2025-07-28 Mon 15:17] SCHEDULED: <2025-07-26 Sa>
searches for OPEN_CHAR after KEYWORD. If found searches for CLOSE_CHAR in the string after start string.
Then cuts the string from OPEN_IDX+1 to close_idx.
 */
pub fn extract_bracket_value(line: &str, keyword: &str, open_char: char, close_char: char) -> Option<String> {
    let start_idx = line.find(keyword)?;
    let rest = &line[start_idx + keyword.len()..];

    let open_idx = rest.find(open_char)?;
    let close_idx = rest[open_idx + 1..].find(close_char)?;
    Some(rest[open_idx + 1 .. open_idx + 1 + close_idx].to_string())
}

/// Number of leading `*` characters, e.g. for org heading-level detection.
/// Shared so the definition of "a star run" lives in exactly one place.
pub fn count_leading_stars(line: &str) -> usize {
    line.chars().take_while(|&c| c == '*').count()
}

/* --------- Test -------- */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_closed_value() {
        let line = "CLOSED: [2025-07-28 Mon 15:17] SCHEDULED: <2025-07-26 Sa>";
        let result = extract_bracket_value(line, "CLOSED:", '[', ']');
        assert_eq!(result, Some("2025-07-28 Mon 15:17".to_string()));
    }

    #[test]
    fn extracts_scheduled_value_with_angle_brackets() {
        let line = "CLOSED: [2025-07-28 Mon 15:17] SCHEDULED: <2025-07-26 Sa>";
        let result = extract_bracket_value(line, "SCHEDULED:", '<', '>');
        assert_eq!(result, Some("2025-07-26 Sa".to_string()));
    }

    #[test]
    fn missing_keyword_returns_none() {
        let line = "SCHEDULED: <2025-07-26 Sa>";
        assert_eq!(extract_bracket_value(line, "DEADLINE:", '<', '>'), None);
    }

    #[test]
    fn keyword_without_open_char_returns_none() {
        let line = "CLOSED: 2025-07-28 Mon 15:17";
        assert_eq!(extract_bracket_value(line, "CLOSED:", '[', ']'), None);
    }

    #[test]
    fn open_char_without_close_char_returns_none() {
        let line = "CLOSED: [2025-07-28 Mon 15:17";
        assert_eq!(extract_bracket_value(line, "CLOSED:", '[', ']'), None);
    }

    #[test]
    fn empty_brackets_return_empty_string() {
        let line = "CLOSED: []";
        assert_eq!(extract_bracket_value(line, "CLOSED:", '[', ']'), Some("".to_string()));
    }

    #[test]
    fn only_searches_after_keyword_not_before() {
        // Bracket VOR dem Keyword darf nicht fälschlich gematcht werden.
        let line = "[irrelevant] CLOSED: [2025-07-28 Mon 15:17]";
        let result = extract_bracket_value(line, "CLOSED:", '[', ']');
        assert_eq!(result, Some("2025-07-28 Mon 15:17".to_string()));
    }

    #[test]
    fn keyword_not_at_start_of_line_still_works() {
        let line = "   CLOSED: [done]";
        assert_eq!(extract_bracket_value(line, "CLOSED:", '[', ']'), Some("done".to_string()));
    }
}