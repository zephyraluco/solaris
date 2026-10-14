//! Text width, wrapping and truncation, and the two helpers a list needs to
//! keep its columns and its row window straight.
//!
//! Widths are measured in terminal cells via `unicode-width`, so CJK and
//! emoji-containing text wraps correctly.

use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Terminal-cell width of `text`.
pub fn display_width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

/// Split `text` into chunks no wider than `width`, never splitting a grapheme
/// boundary if it can be avoided.
fn split_at_width(text: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut used = 0usize;
    for (idx, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width && !head.is_empty() {
            return (head, text[idx..].to_string());
        }
        head.push(ch);
        used += ch_width;
    }
    (head, String::new())
}

/// Word-wrap `text` to `width` cells. Explicit newlines are preserved, and
/// words longer than `width` are hard-split.
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return Vec::new();
    }

    let mut out: Vec<String> = Vec::new();
    for paragraph in text.split('\n') {
        if paragraph.is_empty() {
            out.push(String::new());
            continue;
        }

        let mut current = String::new();
        let mut current_width = 0usize;

        for word in paragraph.split(' ') {
            let mut word = word.to_string();
            let mut word_width = display_width(&word);

            let separator = usize::from(current_width > 0);

            if current_width + separator + word_width <= width {
                if separator == 1 {
                    current.push(' ');
                    current_width += 1;
                }
                current.push_str(&word);
                current_width += word_width;
                continue;
            }

            // Flush the current line and place the word at the start of the next.
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }

            while word_width > width {
                let (head, tail) = split_at_width(&word, width);
                if head.is_empty() {
                    break;
                }
                out.push(head);
                word = tail;
                word_width = display_width(&word);
            }

            current.push_str(&word);
            current_width = word_width;
        }

        out.push(current);
    }
    out
}

/// Truncate `text` to `width` cells, appending `ellipsis` when cut.
pub fn truncate_to_width(text: &str, width: usize, ellipsis: &str) -> String {
    if width == 0 {
        return String::new();
    }
    if display_width(text) <= width {
        return text.to_string();
    }

    let ellipsis_width = display_width(ellipsis);
    if ellipsis_width >= width {
        let (head, _) = split_at_width(ellipsis, width);
        return head;
    }

    let budget = width - ellipsis_width;
    let mut head = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        head.push(ch);
        used += ch_width;
    }
    head.push_str(ellipsis);
    head
}

/// Pad `text` with spaces on the right up to `width` cells.
pub fn pad_to_width(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    let current = display_width(text);
    if current < width {
        out.push_str(&" ".repeat(width - current));
    }
    out
}

/// Number of decimal digits in `value` (at least 1).
///
/// Numbered lists use this to size their number column, so labels stay aligned
/// once a list grows past nine entries without indenting short lists.
pub fn digit_count(value: usize) -> usize {
    value
        .checked_ilog10()
        .map_or(1, |digits| digits as usize + 1)
}

/// The first row a list of `total` rows should draw, so that `highlighted` is
/// among the `visible` rows on screen.
///
/// `offset` is the row the list started at last time. Passing it back keeps the
/// window where it is while the highlight moves inside it, so the list does not
/// jump back to the top the moment the highlight steps up a line; passing `0`
/// recomputes the window from the highlight on every frame.
///
/// Every list in the framework windows this way — the editor's command popup,
/// the inline selectors behind `/model` and `/connect`, and the overlay lists —
/// which is what makes sure a highlight can never walk off the bottom of one.
pub fn window_start(total: usize, visible: usize, highlighted: usize, offset: usize) -> usize {
    if visible == 0 || visible >= total {
        return 0;
    }

    let highlighted = highlighted.min(total - 1);
    let mut start = offset.min(total - visible);
    if highlighted < start {
        start = highlighted;
    }
    if highlighted >= start + visible {
        start = highlighted + 1 - visible;
    }
    start
}

/// Whether `(x, y)` falls inside `rect`.
pub fn rect_contains(rect: ratatui::layout::Rect, x: u16, y: u16) -> bool {
    rect.width > 0
        && rect.height > 0
        && x >= rect.x
        && x < rect.x.saturating_add(rect.width)
        && y >= rect.y
        && y < rect.y.saturating_add(rect.height)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_keeps_the_highlight_on_screen() {
        // Everything fits, so there is no window to move.
        assert_eq!(window_start(3, 8, 2, 0), 0);
        // The highlight is inside the first window.
        assert_eq!(window_start(13, 5, 4, 0), 0);
        // It reaches the last row, and then the window follows it.
        assert_eq!(window_start(13, 5, 5, 0), 1);
        assert_eq!(window_start(13, 5, 12, 0), 8);
        // The last window, a highlight past the end, and nothing to show.
        assert_eq!(window_start(13, 5, 99, 0), 8);
        assert_eq!(window_start(13, 0, 3, 0), 0);
        assert_eq!(window_start(0, 5, 0, 0), 0);
    }

    #[test]
    fn a_remembered_window_holds_still_until_the_highlight_reaches_its_edge() {
        // The highlight steps back up inside the window; the window stays.
        assert_eq!(window_start(13, 5, 6, 4), 4);
        // It reaches the top edge, so the window follows it up.
        assert_eq!(window_start(13, 5, 3, 4), 3);
        // A remembered window past the end is pulled back into the list.
        assert_eq!(window_start(13, 5, 12, 9), 8);
    }

    #[test]
    fn wraps_on_word_boundaries() {
        assert_eq!(
            wrap_text("the quick brown fox", 9),
            vec!["the quick", "brown fox"]
        );
    }

    #[test]
    fn preserves_explicit_newlines() {
        assert_eq!(wrap_text("a\nb", 10), vec!["a", "b"]);
        assert_eq!(wrap_text("a\n\nb", 10), vec!["a", "", "b"]);
    }

    #[test]
    fn hard_splits_overlong_words() {
        assert_eq!(wrap_text("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn counts_wide_characters_as_two_cells() {
        assert_eq!(display_width("中文"), 4);
        // Width 4 fits exactly one wide char per line.
        assert_eq!(wrap_text("中中", 4), vec!["中中"]);
    }

    #[test]
    fn truncates_with_ellipsis() {
        assert_eq!(truncate_to_width("hello world", 8, "..."), "hello...");
        assert_eq!(truncate_to_width("hi", 8, "..."), "hi");
        assert_eq!(truncate_to_width("hello", 0, "..."), "");
    }

    #[test]
    fn pads_to_width() {
        assert_eq!(pad_to_width("ab", 4), "ab  ");
        assert_eq!(pad_to_width("abcd", 2), "abcd");
    }
}
