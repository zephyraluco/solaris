//! The inline selector: a list that takes over the prompt region instead of
//! opening an overlay.
//!
//! This is the look `/connect` and `/model` share — a heading, an optional
//! note, a question, the `❯` rows and a hint pinned to the bottom. Callers own
//! the policy: which items are offered, what a pick means, and when the
//! surface closes.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::components::select_list::SelectItem;
use crate::theme::Theme;
use crate::util::{digit_count, display_width, truncate_to_width, wrap_text};

/// Rows the list shows at once; longer lists scroll.
const MAX_ROWS: u16 = 8;
/// Rows reserved for the pinned hint line.
const HINT_ROWS: u16 = 1;

/// Colours and weights the inline surfaces draw with.
///
/// Derived from the theme in one place so a heading, a list row and a text
/// field can never drift apart.
#[derive(Debug, Clone, Copy)]
pub struct InlineStyles {
    pub title: Style,
    pub text: Style,
    pub muted: Style,
    pub dim: Style,
    pub tip: Style,
    pub ok: Style,
    pub warn: Style,
    pub error: Style,
}

impl InlineStyles {
    /// Derive the palette from the active theme.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            title: Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD),
            text: Style::default().fg(theme.fg),
            muted: Style::default().fg(theme.muted),
            dim: Style::default().fg(theme.dim),
            tip: Style::default()
                .fg(theme.success)
                .add_modifier(Modifier::BOLD),
            ok: Style::default().fg(theme.success),
            warn: Style::default().fg(theme.warning),
            error: Style::default().fg(theme.error),
        }
    }
}

/// What a key or mouse event did to the selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InlineSelectOutcome {
    /// Consumed — the highlight may have moved.
    Handled,
    /// A row was confirmed; the caller decides what picking it means.
    Picked(usize),
}

/// An inline list: heading, note, question, rows, hint.
pub struct InlineSelect {
    heading: String,
    note: Option<String>,
    question: String,
    items: Vec<SelectItem>,
    selected: usize,
    scroll: usize,
    styles: InlineStyles,
    /// Where the selector last drew, so mouse rows can be resolved.
    last_area: Rect,
    item_rows: Vec<(u16, usize)>,
}

impl InlineSelect {
    /// A selector showing `items`.
    pub fn new(
        styles: InlineStyles,
        heading: impl Into<String>,
        question: impl Into<String>,
        items: Vec<SelectItem>,
    ) -> Self {
        Self {
            heading: heading.into(),
            note: None,
            question: question.into(),
            items,
            selected: 0,
            scroll: 0,
            styles,
            last_area: Rect::default(),
            item_rows: Vec::new(),
        }
    }

    /// Add a wrapped explanation line under the heading.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    /// Repaint with another theme's palette.
    pub fn set_styles(&mut self, styles: InlineStyles) {
        self.styles = styles;
    }

    /// The rows on offer.
    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// How many rows are on offer.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there is nothing to pick.
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The highlight's place in the list.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The value of the highlighted row.
    pub fn selected_value(&self) -> Option<&str> {
        self.items
            .get(self.selected)
            .map(|item| item.value.as_str())
    }

    /// Highlight the row carrying `value`; the first row when it is absent.
    pub fn select_value(&mut self, value: &str) {
        self.selected = self
            .items
            .iter()
            .position(|item| item.value == value)
            .unwrap_or(0);
        self.scroll = 0;
    }

    /// The area the selector last drew into.
    pub fn area(&self) -> Rect {
        self.last_area
    }

    /// Rows (body plus hint) the selector wants at `width`.
    pub fn desired_height(&self, width: u16) -> u16 {
        let list_rows = self.items.len().min(MAX_ROWS as usize) as u16;
        self.fixed_rows(width) + list_rows + HINT_ROWS
    }

    /// Route a key.
    pub fn on_key(&mut self, key: KeyEvent) -> InlineSelectOutcome {
        match key.code {
            KeyCode::Up => self.move_selection(-1),
            KeyCode::Down => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-(MAX_ROWS as isize)),
            KeyCode::PageDown => self.move_selection(MAX_ROWS as isize),
            KeyCode::Home => {
                self.selected = 0;
                InlineSelectOutcome::Handled
            }
            KeyCode::End => {
                self.selected = self.items.len().saturating_sub(1);
                InlineSelectOutcome::Handled
            }
            KeyCode::Enter => self.pick(),
            // Digits 1-9 jump straight to that row and confirm it, matching the
            // numbers on screen. Zero is not a shortcut.
            KeyCode::Char(c) if key.modifiers.is_empty() && c.is_ascii_digit() && c != '0' => {
                let index = (c as u8 - b'1') as usize;
                if index >= self.items.len() {
                    return InlineSelectOutcome::Handled;
                }
                self.selected = index;
                self.pick()
            }
            _ => InlineSelectOutcome::Handled,
        }
    }

    /// Route a mouse event, resolving rows against the last drawn frame.
    pub fn on_mouse(&mut self, mouse: MouseEvent) -> InlineSelectOutcome {
        if !rect_contains(self.last_area, mouse.column, mouse.row) {
            return InlineSelectOutcome::Handled;
        }

        match mouse.kind {
            MouseEventKind::ScrollUp => self.move_selection(-1),
            MouseEventKind::ScrollDown => self.move_selection(1),
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(index) = self.item_at_row(mouse.row) {
                    self.selected = index;
                    return InlineSelectOutcome::Picked(index);
                }
                InlineSelectOutcome::Handled
            }
            _ => InlineSelectOutcome::Handled,
        }
    }

    /// Draw the selector into `area`.
    pub fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        self.item_rows.clear();

        if area.width == 0 || area.height < 2 {
            return;
        }

        let width = area.width;
        // The hint row is pinned to the bottom so a squeezed layout still tells
        // the user how to get out.
        let body_area = Rect {
            height: area.height - HINT_ROWS,
            ..area
        };
        let hint_area = Rect {
            y: area.y + area.height - HINT_ROWS,
            height: HINT_ROWS,
            ..area
        };

        let list_rows = body_area.height.saturating_sub(self.fixed_rows(width)) as usize;
        let (lines, rows) = self.body(width, list_rows);

        for (row, line) in lines.iter().take(body_area.height as usize).enumerate() {
            buf.set_line(body_area.x, body_area.y + row as u16, line, body_area.width);
        }
        buf.set_line(hint_area.x, hint_area.y, &self.hint_line(), hint_area.width);

        // Hit-testing works on absolute rows, so translate what `body` recorded.
        self.item_rows = rows
            .into_iter()
            .map(|(row, index)| (body_area.y.saturating_add(row as u16), index))
            .collect();
    }

    // ------------------------------------------------------------- internals

    fn pick(&self) -> InlineSelectOutcome {
        match self.items.get(self.selected) {
            Some(_) => InlineSelectOutcome::Picked(self.selected),
            None => InlineSelectOutcome::Handled,
        }
    }

    /// Move the highlight, wrapping around both ends.
    fn move_selection(&mut self, delta: isize) -> InlineSelectOutcome {
        if self.items.is_empty() {
            return InlineSelectOutcome::Handled;
        }
        self.selected =
            (self.selected as isize + delta).rem_euclid(self.items.len() as isize) as usize;
        InlineSelectOutcome::Handled
    }

    fn item_at_row(&self, row: u16) -> Option<usize> {
        self.item_rows
            .iter()
            .find(|(r, _)| *r == row)
            .map(|(_, index)| *index)
    }

    /// Rows the heading, note and question occupy at `width`.
    fn fixed_rows(&self, width: u16) -> u16 {
        self.body(width, 0).0.len() as u16
    }

    /// The block, with row indices relative to its first line.
    fn body(&self, width: u16, list_rows: usize) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
        let mut lines: Vec<Line<'static>> = Vec::new();
        let mut item_rows: Vec<(usize, usize)> = Vec::new();

        lines.push(Line::from(Span::styled(
            self.heading.clone(),
            self.styles.title,
        )));

        if let Some(note) = &self.note {
            lines.push(Line::from(""));
            for line in wrap_text(note, width.saturating_sub(2).max(8) as usize) {
                lines.push(Line::from(Span::styled(line, self.styles.muted)));
            }
        }

        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            self.question.clone(),
            self.styles.text,
        )));
        lines.push(Line::from(""));

        self.push_item_rows(width, list_rows, &mut lines, &mut item_rows);

        (lines, item_rows)
    }

    /// The list, windowed so the highlighted row is always visible.
    fn push_item_rows(
        &self,
        width: u16,
        list_rows: usize,
        lines: &mut Vec<Line<'static>>,
        item_rows: &mut Vec<(usize, usize)>,
    ) {
        let total = self.items.len();
        if total == 0 || list_rows == 0 {
            return;
        }

        let visible = list_rows.min(MAX_ROWS as usize).min(total);
        let mut start = self.scroll.min(total.saturating_sub(visible));
        if self.selected < start {
            start = self.selected;
        }
        if self.selected >= start + visible {
            start = self.selected + 1 - visible;
        }

        for index in start..start + visible {
            item_rows.push((lines.len(), index));
            lines.push(self.item_row(width, index));
        }
    }

    /// One option row: `❯ 3. Title · description   BADGE`.
    fn item_row(&self, width: u16, index: usize) -> Line<'static> {
        let item = &self.items[index];
        let selected = index == self.selected;

        let marker = if selected { "❯ " } else { "  " };
        // Numbers are right-aligned across the list, matching the pickers.
        let number = format!(
            "{:>width$}. ",
            index + 1,
            width = digit_count(self.items.len())
        );
        let title_style = if selected {
            self.styles.text.add_modifier(Modifier::BOLD)
        } else {
            self.styles.muted
        };
        let desc_style = if selected {
            self.styles.text
        } else {
            self.styles.dim
        };

        let prefix_width = 2 + display_width(&number);
        let badge = item.badge.as_deref().filter(|text| !text.is_empty());
        let badge_width = badge.map_or(0, display_width);
        let badge_space = if badge_width > 0 && badge_width + 3 < width as usize {
            badge_width + 2
        } else {
            0
        };

        let mut text = item.label.clone();
        if !item.description.is_empty() {
            text.push_str(" · ");
            text.push_str(&item.description);
        }
        let text = truncate_to_width(
            &text,
            (width as usize)
                .saturating_sub(prefix_width)
                .saturating_sub(badge_space),
            "…",
        );

        // Split the truncated text back so title and description keep their own
        // colours.
        let (title, desc) = match text.split_once(" · ") {
            Some((title, desc)) => (title.to_string(), Some(desc.to_string())),
            None => (text, None),
        };

        let mut spans = vec![
            Span::styled(marker.to_string(), self.styles.title),
            Span::styled(number, self.styles.dim),
            Span::styled(title, title_style),
        ];
        if let Some(desc) = desc {
            spans.push(Span::styled(" · ", self.styles.dim));
            spans.push(Span::styled(desc, desc_style));
        }
        if let Some(badge) = badge.filter(|_| badge_space > 0) {
            let used: usize = spans.iter().map(|span| display_width(&span.content)).sum();
            let gap = (width as usize).saturating_sub(used + badge_width);
            spans.push(Span::styled(" ".repeat(gap), Style::default()));
            spans.push(Span::styled(badge.to_string(), self.styles.tip));
        }

        Line::from(spans)
    }

    /// Keys the selector answers to, and where the highlight sits.
    fn hint_line(&self) -> Line<'static> {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for hint in ["↑/↓ select", "enter confirm", "esc cancel"] {
            if !spans.is_empty() {
                spans.push(Span::styled("   ", self.styles.dim));
            }
            spans.push(Span::styled(hint.to_string(), self.styles.dim));
        }
        if !self.items.is_empty() {
            spans.push(Span::styled(
                format!("   {}/{}", self.selected + 1, self.items.len()),
                self.styles.dim,
            ));
        }
        Line::from(spans)
    }
}

fn rect_contains(rect: Rect, x: u16, y: u16) -> bool {
    x >= rect.x && x < rect.right() && y >= rect.y && y < rect.bottom()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("one", "One").description("first"),
            SelectItem::new("two", "Two").description("second"),
            SelectItem::new("three", "Three"),
        ]
    }

    fn selector() -> InlineSelect {
        InlineSelect::new(
            InlineStyles::from_theme(&Theme::dark()),
            "Heading",
            "Pick one:",
            items(),
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn render(selector: &mut InlineSelect, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("terminal");
        terminal
            .draw(|frame| {
                let area = frame.area();
                selector.render(frame.buffer_mut(), area);
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .map(|col| buffer[(col, row)].symbol().to_string())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn arrows_wrap_and_enter_picks() {
        let mut selector = selector();
        assert_eq!(
            selector.on_key(key(KeyCode::Up)),
            InlineSelectOutcome::Handled
        );
        assert_eq!(selector.selected_value(), Some("three"));

        selector.on_key(key(KeyCode::Down));
        assert_eq!(selector.selected_value(), Some("one"));

        assert_eq!(
            selector.on_key(key(KeyCode::Enter)),
            InlineSelectOutcome::Picked(0)
        );
    }

    #[test]
    fn digits_jump_and_zero_is_not_a_shortcut() {
        let mut selector = selector();
        assert_eq!(
            selector.on_key(key(KeyCode::Char('0'))),
            InlineSelectOutcome::Handled
        );
        assert_eq!(selector.selected_value(), Some("one"));

        assert_eq!(
            selector.on_key(key(KeyCode::Char('2'))),
            InlineSelectOutcome::Picked(1)
        );
        assert_eq!(selector.selected_value(), Some("two"));

        // A digit past the end of the list is ignored.
        selector.select_value("one");
        assert_eq!(
            selector.on_key(key(KeyCode::Char('9'))),
            InlineSelectOutcome::Handled
        );
        assert_eq!(selector.selected_value(), Some("one"));
    }

    #[test]
    fn the_body_renders_heading_note_and_question() {
        let mut selector = selector().with_note("a note about the list");
        let text = render(&mut selector, 40, 12);

        assert!(text.starts_with("Heading"), "{text}");
        assert!(text.contains("a note about the list"), "{text}");
        assert!(text.contains("Pick one:"), "{text}");
        // The first row is the highlighted one, with its description inline.
        assert!(text.contains("❯ 1. One"), "{text}");
    }

    #[test]
    fn a_long_list_scrolls_so_the_highlighted_row_stays_visible() {
        let items: Vec<SelectItem> = (0..20)
            .map(|i| SelectItem::new(format!("p{i}"), format!("provider {i}")))
            .collect();
        let mut selector = InlineSelect::new(
            InlineStyles::from_theme(&Theme::dark()),
            "Heading",
            "Pick one:",
            items,
        );

        for _ in 0..12 {
            selector.on_key(key(KeyCode::Down));
        }
        let text = render(&mut selector, 60, 12);
        assert!(text.contains("❯ 13. provider 12"), "{text}");
        assert!(!text.contains("1. provider 0"), "{text}");
    }

    #[test]
    fn clicking_a_row_picks_it() {
        let mut selector = selector();
        let text = render(&mut selector, 40, 12);
        let row = text
            .lines()
            .position(|line| line.contains("Two"))
            .expect("the second row") as u16;

        let click = MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: 3,
            row,
            modifiers: KeyModifiers::empty(),
        };
        assert_eq!(selector.on_mouse(click), InlineSelectOutcome::Picked(1));
    }

    #[test]
    fn selecting_a_value_highlights_it() {
        let mut selector = selector();
        selector.select_value("three");
        assert_eq!(selector.selected_value(), Some("three"));

        selector.select_value("gone");
        assert_eq!(selector.selected_value(), Some("one"));
    }
}
