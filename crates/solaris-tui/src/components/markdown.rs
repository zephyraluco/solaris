//! A small markdown renderer producing styled ratatui lines.
//!
//! Supports the subset the transcript needs: ATX headings, bold, italic,
//! inline code, fenced code blocks, bullet and numbered lists, blockquotes,
//! horizontal rules and paragraphs. Wrapping preserves inline styling by
//! wrapping at word boundaries and carrying each word's style along.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::theme::Theme;
use crate::util::display_width;

/// Styles used by [`render_markdown`].
#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyle {
    pub normal: Style,
    pub heading: Style,
    pub bold: Style,
    pub italic: Style,
    pub code: Style,
    pub code_block: Style,
    pub quote: Style,
    pub bullet: Style,
    pub rule: Style,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            normal: Style::default(),
            heading: Style::default().add_modifier(Modifier::BOLD),
            bold: Style::default().add_modifier(Modifier::BOLD),
            italic: Style::default().add_modifier(Modifier::ITALIC),
            code: Style::default().add_modifier(Modifier::DIM),
            code_block: Style::default(),
            quote: Style::default().add_modifier(Modifier::ITALIC),
            bullet: Style::default(),
            rule: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

impl MarkdownStyle {
    /// Derive a palette from a [`Theme`].
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            normal: Style::default().fg(theme.assistant),
            heading: Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD),
            bold: Style::default()
                .fg(theme.assistant)
                .add_modifier(Modifier::BOLD),
            italic: Style::default()
                .fg(theme.assistant)
                .add_modifier(Modifier::ITALIC),
            code: Style::default().fg(theme.accent).bg(theme.code_bg),
            code_block: Style::default().fg(theme.muted).bg(theme.code_bg),
            quote: Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC),
            bullet: Style::default().fg(theme.accent),
            rule: Style::default().fg(theme.border),
        }
    }
}

#[derive(Debug, Clone)]
struct Styled {
    text: String,
    style: Style,
}

/// Render `text` as markdown constrained to `width` cells.
pub fn render_markdown(text: &str, width: u16, style: &MarkdownStyle) -> Vec<Line<'static>> {
    let width = width as usize;
    if width == 0 {
        return Vec::new();
    }

    let mut out: Vec<Line<'static>> = Vec::new();
    let mut code_buffer: Vec<String> = Vec::new();
    let mut in_code = false;

    for raw in text.split('\n') {
        let trimmed = raw.trim_end();

        if trimmed.trim_start().starts_with("```") {
            if in_code {
                for code_line in code_buffer.drain(..) {
                    out.push(code_line_line(&code_line, width, style));
                }
                in_code = false;
            } else {
                in_code = true;
            }
            continue;
        }

        if in_code {
            code_buffer.push(raw.to_string());
            continue;
        }

        if trimmed.trim().is_empty() {
            out.push(Line::default());
            continue;
        }

        if let Some(content) = heading_content(trimmed) {
            let spans = parse_inline(content, style.heading, style);
            for line in wrap_styled(&spans, width) {
                out.push(to_line(&line));
            }
            continue;
        }

        if is_rule(trimmed) {
            out.push(Line::from(Span::styled(
                "─".repeat(width.min(48)),
                style.rule,
            )));
            continue;
        }

        if let Some(rest) = quote_content(trimmed) {
            let prefix = vec![Styled {
                text: "▎ ".to_string(),
                style: style.quote,
            }];
            let spans = parse_inline(rest, style.quote, style);
            for line in wrap_with_prefix(&spans, width, &prefix) {
                out.push(to_line(&line));
            }
            continue;
        }

        if let Some((marker, rest)) = bullet_content(trimmed) {
            let prefix = vec![Styled {
                text: marker,
                style: style.bullet,
            }];
            let spans = parse_inline(rest, style.normal, style);
            for line in wrap_with_prefix(&spans, width, &prefix) {
                out.push(to_line(&line));
            }
            continue;
        }

        let spans = parse_inline(trimmed, style.normal, style);
        for line in wrap_styled(&spans, width) {
            out.push(to_line(&line));
        }
    }

    if in_code {
        for code_line in code_buffer.drain(..) {
            out.push(code_line_line(&code_line, width, style));
        }
    }

    out
}

fn code_line_line(line: &str, width: usize, style: &MarkdownStyle) -> Line<'static> {
    let mut text = String::new();
    let mut used = 0usize;
    for ch in line.chars() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width {
            break;
        }
        text.push(ch);
        used += ch_width;
    }
    Line::from(Span::styled(text, style.code_block))
}

fn heading_content(line: &str) -> Option<&str> {
    let hashes = line.chars().take_while(|c| *c == '#').count();
    if hashes == 0 || hashes > 4 {
        return None;
    }
    line[hashes..].strip_prefix(' ').map(|rest| rest.trim_end())
}

fn is_rule(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 3
        && (trimmed.chars().all(|c| c == '-')
            || trimmed.chars().all(|c| c == '*')
            || trimmed.chars().all(|c| c == '_'))
}

fn quote_content(line: &str) -> Option<&str> {
    line.strip_prefix('>')
        .map(|rest| rest.strip_prefix(' ').unwrap_or(rest))
}

fn bullet_content(line: &str) -> Option<(String, &str)> {
    for marker in ["- ", "* ", "+ "] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some(("• ".to_string(), rest));
        }
    }

    let digits: String = line.chars().take_while(|c| c.is_ascii_digit()).collect();
    if !digits.is_empty() && digits.len() <= 3 {
        if let Some(rest) = line[digits.len()..].strip_prefix(". ") {
            return Some((format!("{digits}. "), rest));
        }
    }
    None
}

fn parse_inline(text: &str, base: Style, style: &MarkdownStyle) -> Vec<Styled> {
    let chars: Vec<char> = text.chars().collect();
    let mut out: Vec<Styled> = Vec::new();
    let mut buffer = String::new();
    let mut index = 0usize;

    while index < chars.len() {
        // Inline code: `…`
        if chars[index] == '`' {
            if let Some(end) = find_char(&chars, index + 1, '`') {
                flush(&mut out, &mut buffer, base);
                out.push(Styled {
                    text: chars[index + 1..end].iter().collect(),
                    style: style.code,
                });
                index = end + 1;
                continue;
            }
        }

        // Bold: **…** and italic: *…*
        if chars[index] == '*' {
            let double = index + 1 < chars.len() && chars[index + 1] == '*';
            let marker_len = if double { 2 } else { 1 };
            let search_from = index + marker_len;
            if let Some(end) = find_marker(&chars, search_from, marker_len) {
                flush(&mut out, &mut buffer, base);
                out.push(Styled {
                    text: chars[search_from..end].iter().collect(),
                    style: if double { style.bold } else { style.italic },
                });
                index = end + marker_len;
                continue;
            }
        }

        buffer.push(chars[index]);
        index += 1;
    }

    flush(&mut out, &mut buffer, base);
    out
}

fn flush(out: &mut Vec<Styled>, buffer: &mut String, style: Style) {
    if !buffer.is_empty() {
        out.push(Styled {
            text: std::mem::take(buffer),
            style,
        });
    }
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    (from..chars.len()).find(|idx| chars[*idx] == needle)
}

fn find_marker(chars: &[char], from: usize, marker_len: usize) -> Option<usize> {
    let mut index = from;
    while index + marker_len <= chars.len() {
        if (0..marker_len).all(|offset| chars[index + offset] == '*') {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn to_line(spans: &[Styled]) -> Line<'static> {
    Line::from(
        spans
            .iter()
            .map(|span| Span::styled(span.text.clone(), span.style))
            .collect::<Vec<_>>(),
    )
}

fn wrap_styled(spans: &[Styled], width: usize) -> Vec<Vec<Styled>> {
    wrap_with_prefix(spans, width, &[])
}

fn wrap_with_prefix(spans: &[Styled], width: usize, prefix: &[Styled]) -> Vec<Vec<Styled>> {
    if width == 0 {
        return Vec::new();
    }

    let prefix_width: usize = prefix.iter().map(|s| display_width(&s.text)).sum();
    let body_width = width.saturating_sub(prefix_width).max(1);

    let mut result: Vec<Vec<Styled>> = Vec::new();
    for (index, mut line) in wrap_body(spans, body_width).into_iter().enumerate() {
        let mut prefixed: Vec<Styled> = Vec::new();
        if !prefix.is_empty() {
            if index == 0 {
                prefixed.extend(prefix.iter().cloned());
            } else {
                prefixed.push(Styled {
                    text: " ".repeat(prefix_width),
                    style: Style::default(),
                });
            }
        }
        prefixed.append(&mut line);
        result.push(prefixed);
    }
    result
}

fn wrap_body(spans: &[Styled], width: usize) -> Vec<Vec<Styled>> {
    let mut lines: Vec<Vec<Styled>> = Vec::new();
    let mut line: Vec<Styled> = Vec::new();
    let mut line_width = 0usize;

    for span in spans {
        for word in span.text.split_whitespace() {
            let mut remaining: String = word.to_string();
            loop {
                let word_width = display_width(&remaining);
                let separator = usize::from(line_width > 0);

                if line_width + separator + word_width <= width {
                    if separator == 1 {
                        line.push(Styled {
                            text: " ".to_string(),
                            style: span.style,
                        });
                        line_width += 1;
                    }
                    line.push(Styled {
                        text: remaining,
                        style: span.style,
                    });
                    line_width += word_width;
                    break;
                }

                if line_width > 0 {
                    lines.push(std::mem::take(&mut line));
                    line_width = 0;
                    continue;
                }

                // A single word wider than the line: hard split it.
                let (head, tail) = split_cells(&remaining, width);
                if head.is_empty() {
                    break;
                }
                line.push(Styled {
                    text: head,
                    style: span.style,
                });
                lines.push(std::mem::take(&mut line));
                line_width = 0;
                remaining = tail;
                if remaining.is_empty() {
                    break;
                }
            }
        }
    }

    if !line.is_empty() || lines.is_empty() {
        lines.push(line);
    }
    lines
}

fn split_cells(text: &str, width: usize) -> (String, String) {
    let mut head = String::new();
    let mut used = 0usize;
    for (index, ch) in text.char_indices() {
        let ch_width = ch.width().unwrap_or(0);
        if used + ch_width > width && !head.is_empty() {
            return (head, text[index..].to_string());
        }
        head.push(ch);
        used += ch_width;
    }
    (head, String::new())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain() -> MarkdownStyle {
        MarkdownStyle::default()
    }

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn renders_headings_and_paragraphs() {
        let lines = render_markdown("# Title\n\nBody text", 40, &plain());
        assert_eq!(line_text(&lines[0]), "Title");
        assert_eq!(line_text(&lines[1]), "");
        assert_eq!(line_text(&lines[2]), "Body text");
    }

    #[test]
    fn renders_bullets_and_numbered_lists() {
        let lines = render_markdown("- one\n- two\n\n1. first", 40, &plain());
        assert_eq!(line_text(&lines[0]), "• one");
        assert_eq!(line_text(&lines[1]), "• two");
        assert_eq!(line_text(&lines[3]), "1. first");
    }

    #[test]
    fn renders_code_blocks_verbatim() {
        let input = "```rust\nlet x = 1;\n```";
        let lines = render_markdown(input, 40, &plain());
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "let x = 1;");
    }

    #[test]
    fn wraps_long_paragraphs() {
        let lines = render_markdown("one two three four five six", 10, &plain());
        assert!(lines.len() >= 3);
        for line in &lines {
            assert!(display_width(&line_text(line)) <= 10);
        }
    }

    #[test]
    fn wraps_list_items_with_hanging_indent() {
        let lines = render_markdown("- alpha beta gamma delta", 12, &plain());
        assert!(lines.len() >= 2);
        assert!(line_text(&lines[0]).starts_with("• "));
        assert!(line_text(&lines[1]).starts_with("  "));
        for line in &lines {
            assert!(display_width(&line_text(line)) <= 12);
        }
    }

    #[test]
    fn applies_inline_styles() {
        let lines = render_markdown("say **hi** now", 40, &plain());
        let styles: Vec<_> = lines[0]
            .spans
            .iter()
            .map(|s| (s.content.to_string(), s.style))
            .collect();
        assert!(
            styles
                .iter()
                .any(|(text, style)| text == "hi" && *style == plain().bold)
        );
    }

    #[test]
    fn unterminated_code_fence_still_renders() {
        let lines = render_markdown("```\nabc", 40, &plain());
        assert_eq!(lines.len(), 1);
        assert_eq!(line_text(&lines[0]), "abc");
    }

    #[test]
    fn empty_input_produces_nothing_visible() {
        let lines = render_markdown("", 20, &plain());
        assert!(lines.iter().all(|l| line_text(l).is_empty()));
    }
}
