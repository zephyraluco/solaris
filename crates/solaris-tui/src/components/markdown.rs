//! Markdown semantics belong to tui-markdown; Solaris supplies its presentation.
//!
//! Only official StyleSheet configuration is used. List markers, quotations,
//! fences and rules retain the library defaults; body text is never rewritten.

use crate::theme::Theme;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tui_markdown::{Options, StyleSheet};

use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// Theme styles supplied through the official renderer configuration.
#[derive(Debug, Clone, Copy)]
pub struct MarkdownStyle {
    pub normal: Style,
    pub heading: Style,
    pub code: Style,
    pub quote: Style,
    pub bullet: Style,
    pub rule: Style,
}

impl Default for MarkdownStyle {
    fn default() -> Self {
        Self {
            normal: Style::default(),
            heading: Style::default().add_modifier(Modifier::BOLD),
            code: Style::default().add_modifier(Modifier::DIM),
            quote: Style::default().add_modifier(Modifier::ITALIC),
            bullet: Style::default(),
            rule: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

impl MarkdownStyle {
    /// Derive the existing application palette without implementing Markdown syntax.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            normal: Style::default().fg(theme.assistant),
            heading: Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD),
            code: Style::default().fg(theme.accent).bg(theme.code_bg),
            quote: Style::default()
                .fg(theme.muted)
                .add_modifier(Modifier::ITALIC),
            bullet: Style::default().fg(theme.accent),
            rule: Style::default().fg(theme.border),
        }
    }
}

impl StyleSheet for MarkdownStyle {
    fn heading(&self, _level: u8) -> Style {
        self.heading
    }
    fn heading_marker(&self, _level: u8) -> &str {
        ""
    }
    fn code(&self) -> Style {
        self.code
    }
    fn blockquote(&self) -> Style {
        self.quote
    }
    fn list_marker(&self) -> Style {
        self.bullet
    }
    fn link(&self) -> Style {
        self.bullet.add_modifier(Modifier::UNDERLINED)
    }
    fn table_header(&self) -> Style {
        self.heading
    }
    fn table_border(&self) -> Style {
        self.rule
    }
}

/// Render Markdown into physical rows, shared by painting and text selection.
///
/// Code is plain styled text with visible fences, without syntax highlighting
/// or code-specific controls. An oversized grapheme remains intact on its row.
pub fn render_markdown(text: &str, width: u16, style: &MarkdownStyle) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let rendered =
        tui_markdown::from_str_with_options(text, &Options::new(*style).table_width(width));
    rendered
        .lines
        .into_iter()
        .flat_map(|line| {
            let base = style.normal.patch(rendered.style).patch(line.style);
            wrap(line, width, base)
        })
        .collect()
}

fn wrap(line: Line<'_>, width: u16, base: Style) -> Vec<Line<'static>> {
    let text = line.to_string();
    let mut spans = line.spans.iter();
    let mut current = spans.next();
    let mut span_end = current.map_or(0, |span| span.content.len());
    let mut glyphs = Vec::new();
    let mut column = 0;
    for (offset, glyph) in text.grapheme_indices(true) {
        while offset >= span_end {
            current = spans.next();
            let Some(span) = current else {
                break;
            };
            span_end += span.content.len();
        }
        let style = current.map_or(base, |span| base.patch(span.style));
        if glyph == "\t" {
            let spaces = 4 - column % 4;
            glyphs.extend(std::iter::repeat_n((" ", style, 1), spaces));
            column += spaces;
        } else {
            let size = glyph.width();
            glyphs.push((glyph, style, size));
            column += size;
        }
    }
    let mut rows = Vec::new();
    let mut start = 0;
    while start < glyphs.len() {
        let available = usize::from(width);
        let mut end = start;
        let mut cells = 0;
        let mut word_break = None;
        while end < glyphs.len() {
            let (glyph, _, size) = glyphs[end];
            if cells + size > available && end > start {
                break;
            }
            cells += size;
            end += 1;
            if glyph.chars().all(char::is_whitespace) {
                word_break = Some(end);
            }
        }
        if end < glyphs.len()
            && let Some(boundary) = word_break
        {
            end = boundary;
        }
        let mut output: Vec<Span<'static>> = Vec::new();
        for &(glyph, style, _) in &glyphs[start..end] {
            if let Some(last) = output.last_mut()
                && last.style == style
            {
                last.content.to_mut().push_str(glyph);
            } else {
                output.push(Span::styled(glyph.to_owned(), style));
            }
        }
        rows.push(Line::from(output).style(base));
        start = end;
    }
    if rows.is_empty() {
        rows.push(Line::default().style(base));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    fn render(source: &str, width: u16) -> Vec<Line<'static>> {
        render_markdown(source, width, &MarkdownStyle::default())
    }
    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn official_dependency_preserves_default_markers() {
        let output =
            tui_markdown::from_str("- item\n\n> quoted\n\n---\n\n```text\ncode\n```").to_string();
        assert_eq!(output, "- item\n\n> quoted\n\n---\n\n```text\ncode\n```");
    }

    #[test]
    fn headings_hide_prefixes_while_lists_quotes_and_rules_use_defaults() {
        let output = text(&render(
            "## Title\n\n- one\n  - two\n\n3. third\n\n> quote\n\n---",
            40,
        ));
        assert!(output.starts_with("Title\n"), "{output}");
        assert!(
            output.contains("- one") && output.contains("    - two"),
            "{output}"
        );
        assert!(output.contains("3. third"), "{output}");
        assert!(output.contains("> quote"), "{output}");
        assert!(output.ends_with("---"), "{output}");
    }

    #[test]
    fn nested_inline_styles_compose_without_changing_word_spacing() {
        let lines = render("a**bold *both***z ~~gone~~", 80);
        assert_eq!(text(&lines), "abold bothz gone");
        assert!(lines[0].spans.iter().any(|span| {
            span.content.contains("both")
                && span
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::ITALIC)
        }));
    }

    #[test]
    fn escaped_bullet_before_emphasis_is_literal() {
        assert_eq!(text(&render("\\- **literal**", 80)), "- literal");
    }

    #[test]
    fn escaped_text_and_code_are_not_rewritten() {
        let output = text(&render(
            "\\- literal\n\n\\> literal\n\n\\---\n\n`- `\n\n```text\n- item\n> quoted\n---\n```",
            80,
        ));
        assert_eq!(
            output,
            "- literal\n\n> literal\n\n---\n\n- \n\n```text\n- item\n> quoted\n---\n```"
        );
    }

    #[test]
    fn task_lists_links_and_nested_quotes_use_library_semantics() {
        let output = text(&render(
            "- [x] done\n- [ ] todo\n\n> > deep\n>\n> - [site](https://example.com)",
            80,
        ));
        assert!(
            output.contains("- [x] done") && output.contains("- [ ] todo"),
            "{output}"
        );
        assert!(output.contains(">> deep"), "{output}");
        assert!(output.contains("> - site"), "{output}");
        assert!(output.contains("https://example.com"));
    }

    #[test]
    fn fences_remain_visible_and_code_has_no_syntax_highlighting() {
        let lines = render("```rust\nlet value = 42;\n```", 80);
        assert_eq!(text(&lines), "```rust\nlet value = 42;\n```");
        assert!(
            lines[1]
                .spans
                .iter()
                .all(|span| span.style == MarkdownStyle::default().code)
        );
        assert_eq!(
            text(&render("```rust\nunfinished", 80)),
            "```rust\nunfinished\n```"
        );
    }

    #[test]
    fn long_list_items_and_quotes_fit_the_content_width() {
        for source in [
            "- alpha beta gamma delta",
            "12. alpha beta gamma delta",
            "> alpha beta gamma delta",
        ] {
            let lines = render(source, 12);
            assert!(lines.len() > 1);
            assert!(lines.iter().all(|line| line.width() <= 12));
            assert_eq!(
                lines.iter().map(ToString::to_string).collect::<String>(),
                source
            );
        }
    }

    #[test]
    fn tables_and_long_code_keep_content_at_narrow_widths() {
        let lines = render(
            "| Name | Value |\n| --- | --- |\n| alpha beta gamma | 123 |",
            24,
        );
        assert!(text(&lines).contains('┌') && text(&lines).contains("gamma"));
        assert!(lines.iter().all(|line| line.width() <= 24));
        let code = "    let value = 1234567890;  ";
        let lines = render(&format!("```rust\n{code}\n```"), 8);
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<String>(),
            format!("```rust{code}```")
        );
    }

    #[test]
    fn tiny_list_viewports_keep_wide_graphemes_whole() {
        use unicode_segmentation::UnicodeSegmentation;
        for width in [2, 3, 4] {
            let lines = render("- 中文👩‍💻", width);
            assert!(lines.iter().all(|line| line.width() <= usize::from(width)));
            let glyphs: Vec<_> = lines
                .iter()
                .flat_map(|line| &line.spans)
                .flat_map(|span| span.content.graphemes(true))
                .filter(|glyph| !glyph.chars().all(char::is_whitespace))
                .collect();
            assert_eq!(glyphs, ["-", "中", "文", "👩‍💻"]);
        }
    }

    #[test]
    fn both_themes_apply_heading_and_inline_code_colors() {
        for theme in [Theme::dark(), Theme::light()] {
            let lines =
                render_markdown("# Title\n\n`code`", 80, &MarkdownStyle::from_theme(&theme));
            assert_eq!(lines[0].spans[0].style.fg, Some(theme.heading));
            assert_eq!(
                lines.last().expect("code").spans[0].style.bg,
                Some(theme.code_bg)
            );
        }
    }

    #[test]
    fn empty_content_and_zero_width_have_no_visible_output() {
        assert!(text(&render("", 40)).is_empty());
        assert!(render("content", 0).is_empty());
    }

    #[test]
    fn unicode_clusters_styles_and_code_spaces_survive_wrapping() {
        let input = "    中文 é 👩‍💻  end  ";
        for width in [2, 5, 12, 80] {
            let rows = wrap(Line::from(input), width, Style::default());
            assert_eq!(
                rows.iter().map(ToString::to_string).collect::<String>(),
                input
            );
            let actual: Vec<_> = rows
                .iter()
                .flat_map(|row| &row.spans)
                .flat_map(|span| span.content.graphemes(true))
                .collect();
            assert_eq!(actual, input.graphemes(true).collect::<Vec<_>>());
            assert!(rows.iter().all(|row| row.width() <= usize::from(width)));
        }
    }
    #[test]
    fn tabs_expand_at_terminal_column_stops_before_wrapping() {
        let rows = wrap(Line::from("中\té\tx"), 5, Style::default());
        assert_eq!(
            rows.iter().map(ToString::to_string).collect::<String>(),
            "中  é   x"
        );
    }
}
