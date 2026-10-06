//! Subsequence fuzzy matching for filterable lists.

/// Score a candidate: `None` when `query` is not a subsequence of `text`.
///
/// Higher is better. Consecutive matches and early matches score higher.
pub fn fuzzy_score(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }

    let needle: Vec<char> = query.to_lowercase().chars().collect();
    let haystack: Vec<char> = text.to_lowercase().chars().collect();

    let mut score = 0i32;
    let mut matched = 0usize;
    let mut previous: Option<usize> = None;

    for (index, ch) in haystack.iter().enumerate() {
        if matched >= needle.len() {
            break;
        }
        if *ch != needle[matched] {
            continue;
        }

        score += if previous == Some(index.saturating_sub(1)) {
            5
        } else {
            1
        };
        if index < 12 {
            score += 2;
        }
        // Strongly prefer matches that start at the beginning of the candidate.
        if index == 0 {
            score += 8;
        }
        previous = Some(index);
        matched += 1;
    }

    if matched == needle.len() {
        // Prefer shorter candidates when scores are otherwise equal.
        Some(score - (haystack.len() as i32 / 8))
    } else {
        None
    }
}

/// Whether `query` is a subsequence of `text`.
pub fn fuzzy_match(query: &str, text: &str) -> bool {
    fuzzy_score(query, text).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_query_matches_everything() {
        assert_eq!(fuzzy_score("", "anything"), Some(0));
    }

    #[test]
    fn matches_subsequences() {
        assert!(fuzzy_match("hlp", "help"));
        assert!(fuzzy_match("mdl", "model"));
        assert!(!fuzzy_match("xyz", "help"));
        assert!(!fuzzy_match("plh", "help"));
    }

    #[test]
    fn prefers_contiguous_matches() {
        let contiguous = fuzzy_score("the", "theme").unwrap();
        let scattered = fuzzy_score("the", "t-h-e").unwrap();
        assert!(contiguous > scattered);
    }

    #[test]
    fn is_case_insensitive() {
        assert!(fuzzy_match("HELP", "help"));
        assert!(fuzzy_match("help", "HELP"));
    }
}
