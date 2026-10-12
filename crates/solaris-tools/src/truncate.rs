//! Keeping tool output inside a size the model can actually read.
//!
//! A `read` of a minified file or a `bash` command that prints a build log can
//! produce megabytes. Sending that costs more than the turn is worth and buries
//! the part the model needed, so every tool cuts its output and says what it
//! cut. Reads keep the head — the beginning of a file is what tells you what it
//! is — and command output keeps the tail, because that is where a failure
//! prints.

/// Most lines an output keeps.
pub const DEFAULT_MAX_LINES: usize = 2000;

/// Most bytes an output keeps; whichever limit is reached first wins.
pub const DEFAULT_MAX_BYTES: usize = 50 * 1024;

/// The limits one truncation runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Most lines to keep.
    pub max_lines: usize,
    /// Most bytes to keep.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
        }
    }
}

/// Which end of the text was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Removed {
    /// Nothing.
    None,
    /// The end was dropped, so the head was kept.
    Back,
    /// The beginning was dropped, so the tail was kept.
    Front,
}

/// What a truncation did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncation {
    /// The content that survives.
    pub content: String,
    /// How many lines went.
    pub removed_lines: usize,
    /// Which end they went from.
    pub removed: Removed,
}

impl Truncation {
    /// Whether anything was dropped.
    pub fn truncated(&self) -> bool {
        self.removed != Removed::None
    }

    /// A notice to append, so the model knows what it is not seeing.
    ///
    /// `None` when nothing was dropped: a notice that always appears spends the
    /// model's attention on saying nothing.
    pub fn notice(&self, limits: Limits) -> Option<String> {
        let budget = format!(
            "{} lines / {}",
            limits.max_lines,
            format_size(limits.max_bytes)
        );
        match self.removed {
            Removed::None => None,
            Removed::Back => Some(format!(
                "[output truncated to the first {budget}; {} more line(s) follow]",
                self.removed_lines
            )),
            Removed::Front => Some(format!(
                "[output truncated to the last {budget}; {} earlier line(s) dropped]",
                self.removed_lines
            )),
        }
    }
}

/// Keep the head of `text`.
pub fn head(text: &str, limits: Limits) -> Truncation {
    let mut content = String::new();
    let mut kept = 0usize;
    let mut total = 0usize;

    for line in text.lines() {
        total += 1;
        if kept >= limits.max_lines {
            continue;
        }
        let separator = usize::from(!content.is_empty());
        if content.len() + separator + line.len() <= limits.max_bytes {
            if separator == 1 {
                content.push('\n');
            }
            content.push_str(line);
            kept += 1;
        } else if content.is_empty() {
            // Not even one line fits whole: keep the part that does rather than
            // handing back nothing at all.
            content = clip_to_bytes(text, limits.max_bytes);
            kept += 1;
        }
    }

    let mut truncation = Truncation {
        content,
        removed_lines: total - kept,
        removed: Removed::None,
    };
    if truncation.removed_lines > 0 {
        truncation.removed = Removed::Back;
    }
    truncation
}

/// Keep the tail of `text`.
pub fn tail(text: &str, limits: Limits) -> Truncation {
    let lines: Vec<&str> = text.lines().collect();
    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;

    for line in lines.iter().rev() {
        if kept.len() >= limits.max_lines {
            break;
        }
        let extra = line.len() + usize::from(!kept.is_empty());
        if bytes + extra > limits.max_bytes {
            break;
        }
        bytes += extra;
        kept.push(line);
    }
    kept.reverse();

    let mut content = kept.join("\n");
    if kept.is_empty() && !text.is_empty() {
        // A single line longer than the byte budget: the end of it is still
        // worth more than nothing.
        content = clip_from_tail(text, limits.max_bytes);
    }

    let mut truncation = Truncation {
        content,
        removed_lines: lines.len() - kept.len(),
        removed: Removed::None,
    };
    if truncation.removed_lines > 0 {
        truncation.removed = Removed::Front;
    }
    truncation
}

/// A byte count someone can read, for the notices above.
pub fn format_size(bytes: usize) -> String {
    const KB: usize = 1024;
    const MB: usize = 1024 * 1024;

    if bytes >= MB {
        format!("{:.1}MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{}KB", bytes / KB)
    } else {
        format!("{bytes}B")
    }
}

/// The longest prefix of `text` that fits in `max` bytes, cut on a character
/// boundary so a multi-byte character is never split.
fn clip_to_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The longest suffix of `text` that fits in `max` bytes, cut the same way.
fn clip_from_tail(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut start = text.len() - max;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(lines: usize, bytes: usize) -> Limits {
        Limits {
            max_lines: lines,
            max_bytes: bytes,
        }
    }

    #[test]
    fn short_output_is_left_alone() {
        let limits = Limits::default();
        let result = head("one\ntwo", limits);
        assert_eq!(result.content, "one\ntwo");
        assert!(!result.truncated());
        assert_eq!(result.notice(limits), None);

        let result = tail("one\ntwo", limits);
        assert_eq!(result.content, "one\ntwo");
        assert!(!result.truncated());
    }

    #[test]
    fn the_head_keeps_the_beginning_and_counts_what_it_dropped() {
        let result = head("one\ntwo\nthree\nfour", small(2, 1024));
        assert_eq!(result.content, "one\ntwo");
        assert_eq!(result.removed_lines, 2);
        assert_eq!(result.removed, Removed::Back);

        let notice = result.notice(small(2, 1024)).expect("a notice");
        assert!(notice.contains("2 more line(s)"), "{notice}");
    }

    #[test]
    fn the_tail_keeps_the_end_and_says_so() {
        let result = tail("one\ntwo\nthree\nfour", small(2, 1024));
        assert_eq!(result.content, "three\nfour");
        assert_eq!(result.removed_lines, 2);
        assert_eq!(result.removed, Removed::Front);

        let notice = result.notice(small(2, 1024)).expect("a notice");
        assert!(notice.contains("earlier line(s)"), "{notice}");
    }

    #[test]
    fn the_byte_budget_can_bind_before_the_line_budget() {
        // `aaaa` fills the eight bytes exactly; `bbbb` would need a separator
        // plus four more, so the output stops on a line boundary.
        let result = head("aaaa\nbbbb\ncccc", small(100, 8));
        assert_eq!(result.content, "aaaa");
        assert_eq!(result.removed_lines, 2);
    }

    #[test]
    fn one_very_long_line_still_says_something() {
        let text = "x".repeat(100);
        let result = head(&text, small(10, 10));
        assert_eq!(result.content, "x".repeat(10));

        let result = tail(&text, small(10, 10));
        assert_eq!(result.content, "x".repeat(10));
    }

    #[test]
    fn a_multi_byte_character_is_never_split() {
        // Each of these is three bytes, so a ten-byte budget cannot take four.
        let text = "中中中中";
        let result = head(text, small(10, 10));
        assert_eq!(result.content, "中中中");
        assert!(result.content.len() <= 10);

        let result = tail(text, small(10, 10));
        assert_eq!(result.content, "中中中");
    }

    #[test]
    fn empty_output_truncates_to_nothing_and_says_nothing() {
        let limits = Limits::default();
        let result = head("", limits);
        assert_eq!(result.content, "");
        assert!(!result.truncated());
        assert_eq!(result.notice(limits), None);

        let result = tail("", limits);
        assert_eq!(result.content, "");
        assert!(!result.truncated());
    }

    #[test]
    fn sizes_read_the_way_a_person_would_say_them() {
        assert_eq!(format_size(512), "512B");
        assert_eq!(format_size(2048), "2KB");
        assert_eq!(format_size(50 * 1024), "50KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
