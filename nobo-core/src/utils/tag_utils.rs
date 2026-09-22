use std::collections::HashMap;

pub fn search(tag_counts: &HashMap<String, usize>, query: &str, selected: &[String], limit: usize) -> Vec<String> {
    let query_lower = query.to_lowercase();
    let selected_lower: Vec<String> = selected.iter().map(|s| s.to_lowercase()).collect();

    let mut matched: Vec<(String, usize)> = tag_counts.iter()
        .filter(|(t, _)| t.to_lowercase().contains(&query_lower))
        .filter(|(t, _)| !selected_lower.contains(&t.to_lowercase()))
        .map(|(t, c)| (t.clone(), *c))
        .collect();

    matched.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    matched.into_iter().map(|(t, _)| t).take(limit).collect()
}

/* --------- Test -------- */

#[cfg(test)]
mod tests {
    use super::*;

    fn counts(pairs: &[(&str, usize)]) -> HashMap<String, usize> {
        pairs.iter().map(|(t, c)| (t.to_string(), *c)).collect()
    }

    #[test]
    fn search_excludes_selected_and_respects_limit() {
        let tags = counts(&[("rust", 3), ("rustacean", 1), ("go", 2)]);
        let result = search(&tags, "rust", &["rust".to_string()], 10);
        assert_eq!(result, vec!["rustacean"]);
    }
}