//! Applying exact-text edits to a file's contents.
//!
//! Everything here is a pure function of its inputs, so the fiddly parts — a
//! byte-order mark, CRLF line endings, two edits that overlap — are decided
//! once and tested from fixtures rather than from a real file.

/// One replacement the model asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Edit {
    /// Text that must match the file exactly once.
    pub old_text: String,
    /// What to put in its place.
    pub new_text: String,
}

/// Why an edit set could not be applied.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    /// An edit with nothing to look for.
    #[error("`oldText` must not be empty")]
    Empty,
    /// Nothing matched.
    #[error("`oldText` was not found in the file: {snippet}")]
    NotFound {
        /// The start of what was looked for.
        snippet: String,
    },
    /// More than one place matched, so there is no way to know which was meant.
    #[error(
        "`oldText` appears {count} times in the file — include more surrounding lines so it \
         matches once: {snippet}"
    )]
    Ambiguous {
        /// How many places matched.
        count: usize,
        /// The start of what was looked for.
        snippet: String,
    },
    /// Two edits claimed the same characters.
    #[error("two edits overlap — merge changes that touch the same block into one edit")]
    Overlap,
}

/// Split a leading byte-order mark from `text`.
///
/// The mark is invisible, so a model will never include it in `oldText`; it is
/// kept aside and written back so the file's encoding is unchanged.
pub fn split_bom(text: &str) -> (&str, &str) {
    match text.strip_prefix('\u{feff}') {
        Some(rest) => ("\u{feff}", rest),
        None => ("", text),
    }
}

/// The line ending most of `text` uses.
pub fn detect_line_ending(text: &str) -> &'static str {
    let crlf = text.matches("\r\n").count();
    let lf = text.matches('\n').count() - crlf;
    if crlf > lf { "\r\n" } else { "\n" }
}

/// `text` with every line ending reduced to `\n`.
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// `text` with its line endings restored to `ending`.
pub fn restore_line_endings(text: &str, ending: &str) -> String {
    if ending == "\r\n" {
        text.replace('\n', "\r\n")
    } else {
        text.to_string()
    }
}

/// Apply every edit to `content`, each matched against the original.
///
/// Matching against the original rather than incrementally is what lets the
/// model send several changes to one file in a single call without having to
/// predict how the earlier ones shifted the later offsets.
pub fn apply_edits(content: &str, edits: &[Edit]) -> Result<String, EditError> {
    let mut placed: Vec<(usize, usize, &Edit)> = Vec::with_capacity(edits.len());

    for edit in edits {
        if edit.old_text.is_empty() {
            return Err(EditError::Empty);
        }
        let matches = find_all(content, &edit.old_text);
        match matches.len() {
            0 => {
                return Err(EditError::NotFound {
                    snippet: snippet(&edit.old_text),
                });
            }
            1 => {
                let start = matches[0];
                placed.push((start, start + edit.old_text.len(), edit));
            }
            count => {
                return Err(EditError::Ambiguous {
                    count,
                    snippet: snippet(&edit.old_text),
                });
            }
        }
    }

    placed.sort_by_key(|(start, _, _)| *start);
    for pair in placed.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(EditError::Overlap);
        }
    }

    let mut updated = String::with_capacity(content.len());
    let mut cursor = 0;
    for (start, end, edit) in placed {
        updated.push_str(&content[cursor..start]);
        updated.push_str(&edit.new_text);
        cursor = end;
    }
    updated.push_str(&content[cursor..]);
    Ok(updated)
}

/// Every byte offset at which `needle` starts, overlapping ones included.
fn find_all(haystack: &str, needle: &str) -> Vec<usize> {
    haystack
        .char_indices()
        .filter(|(index, _)| haystack[*index..].starts_with(needle))
        .map(|(index, _)| index)
        .collect()
}

/// The first line of `text`, shortened, for an error message.
fn snippet(text: &str) -> String {
    const WIDTH: usize = 60;

    let first = text.lines().next().unwrap_or_default();
    if first.chars().count() <= WIDTH {
        return first.to_string();
    }
    let clipped: String = first.chars().take(WIDTH).collect();
    format!("{clipped}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old: &str, new: &str) -> Edit {
        Edit {
            old_text: old.to_string(),
            new_text: new.to_string(),
        }
    }

    #[test]
    fn one_replacement_lands_where_it_matched() {
        let updated = apply_edits("one two three", &[edit("two", "2")]).expect("applied");
        assert_eq!(updated, "one 2 three");
    }

    #[test]
    fn several_edits_all_match_the_original() {
        // The second edit's `oldText` is unaffected by the first edit having
        // changed the length of the file.
        let updated =
            apply_edits("aa bb cc", &[edit("aa", "aaaa"), edit("cc", "c")]).expect("applied");
        assert_eq!(updated, "aaaa bb c");
    }

    #[test]
    fn an_edit_that_matches_nothing_is_reported_with_the_text() {
        let error = apply_edits("one two", &[edit("three", "3")]).expect_err("no match");
        assert_eq!(
            error,
            EditError::NotFound {
                snippet: "three".to_string()
            }
        );
        assert!(error.to_string().contains("not found"), "{error}");
    }

    #[test]
    fn an_ambiguous_edit_says_how_many_times_it_matched() {
        let error = apply_edits("x\ny\nx", &[edit("x", "z")]).expect_err("ambiguous");
        assert_eq!(
            error,
            EditError::Ambiguous {
                count: 2,
                snippet: "x".to_string()
            }
        );
        assert!(error.to_string().contains("2 times"), "{error}");
    }

    #[test]
    fn empty_text_is_refused_rather_than_matching_everywhere() {
        assert_eq!(apply_edits("abc", &[edit("", "x")]), Err(EditError::Empty));
    }

    #[test]
    fn overlapping_edits_are_refused() {
        let error =
            apply_edits("abcdef", &[edit("abc", "1"), edit("cde", "2")]).expect_err("overlap");
        assert_eq!(error, EditError::Overlap);
    }

    #[test]
    fn adjacent_edits_are_fine() {
        let updated =
            apply_edits("abcdef", &[edit("abc", "1"), edit("def", "2")]).expect("applied");
        assert_eq!(updated, "12");
    }

    #[test]
    fn a_long_snippet_is_shortened_for_the_message() {
        let long = "z".repeat(200);
        let error = apply_edits("nothing like it", &[edit(&long, "x")]).expect_err("no match");
        let EditError::NotFound { snippet } = error else {
            panic!("expected NotFound");
        };
        assert_eq!(
            snippet.chars().count(),
            61,
            "60 characters plus an ellipsis"
        );
    }

    #[test]
    fn a_bom_is_kept_aside_and_put_back() {
        let text = "\u{feff}hello";
        let (bom, body) = split_bom(text);
        assert_eq!(bom, "\u{feff}");
        assert_eq!(body, "hello");

        let (bom, body) = split_bom("hello");
        assert_eq!(bom, "");
        assert_eq!(body, "hello");
    }

    #[test]
    fn the_dominant_line_ending_wins() {
        assert_eq!(detect_line_ending("a\nb\nc"), "\n");
        assert_eq!(detect_line_ending("a\r\nb\r\nc"), "\r\n");
        assert_eq!(detect_line_ending("a\r\nb\r\nc\nd"), "\r\n");
        assert_eq!(detect_line_ending("no newline at all"), "\n");
    }

    #[test]
    fn line_endings_survive_a_round_trip() {
        let crlf = "a\r\nb\r\n";
        let normalized = normalize_to_lf(crlf);
        assert_eq!(normalized, "a\nb\n");
        assert_eq!(restore_line_endings(&normalized, "\r\n"), crlf);
        assert_eq!(restore_line_endings(&normalized, "\n"), normalized);
    }

    #[test]
    fn a_model_that_sent_crlf_still_matches() {
        let normalized = normalize_to_lf("one\r\ntwo");
        let updated = apply_edits(&normalized, &[edit("one\ntwo", "three")]).expect("applied");
        assert_eq!(updated, "three");
    }
}
