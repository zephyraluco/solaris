//! Filterable selection list with fuzzy matching.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};

use crate::component::{Component, KeyResult, MouseResult};
use crate::components::fuzzy::fuzzy_score;
use crate::util::{digit_count, display_width, rect_contains, truncate_to_width};

/// One selectable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    /// Value returned to the caller.
    pub value: String,
    /// Primary text.
    pub label: String,
    /// Secondary text shown dimmed after a ` · ` separator.
    pub description: String,
    /// Short marker pinned to the right edge (`FREE`, `LOCAL`).
    pub badge: Option<String>,
}

impl SelectItem {
    /// Item with a value and label.
    pub fn new(value: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            label: label.into(),
            description: String::new(),
            badge: None,
        }
    }

    /// Attach a description.
    pub fn description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// Attach a right-aligned badge.
    pub fn badge(mut self, badge: impl Into<String>) -> Self {
        self.badge = Some(badge.into());
        self
    }
}

/// Styles used while drawing the list.
#[derive(Debug, Clone, Copy)]
pub struct SelectListTheme {
    pub normal: Style,
    pub selected: Style,
    pub description: Style,
    /// Right-aligned badge text.
    pub badge: Style,
    pub empty: Style,
}

impl Default for SelectListTheme {
    fn default() -> Self {
        Self {
            normal: Style::default(),
            selected: Style::default().add_modifier(Modifier::BOLD),
            description: Style::default().add_modifier(Modifier::DIM),
            badge: Style::default().add_modifier(Modifier::BOLD),
            empty: Style::default().add_modifier(Modifier::DIM),
        }
    }
}

/// A fuzzy-filterable list of [`SelectItem`]s.
pub struct SelectList {
    items: Vec<SelectItem>,
    matches: Vec<usize>,
    query: String,
    selected: usize,
    max_visible: u16,
    scroll: usize,
    theme: SelectListTheme,
    last_area: Rect,
    version: u64,
}

impl SelectList {
    /// Build a list over `items`.
    pub fn new(items: Vec<SelectItem>) -> Self {
        let mut list = Self {
            items,
            matches: Vec::new(),
            query: String::new(),
            selected: 0,
            max_visible: 8,
            scroll: 0,
            theme: SelectListTheme::default(),
            last_area: Rect::default(),
            version: 0,
        };
        list.refilter();
        list
    }

    /// Override the drawing styles.
    pub fn theme(mut self, theme: SelectListTheme) -> Self {
        self.theme = theme;
        self
    }

    /// Maximum rows drawn at once.
    pub fn max_visible(mut self, rows: u16) -> Self {
        self.max_visible = rows.max(1);
        self
    }

    /// Replace the items and clear the filter.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.items = items;
        self.query.clear();
        self.selected = 0;
        self.scroll = 0;
        self.refilter();
    }

    /// Current filter text.
    pub fn query(&self) -> &str {
        &self.query
    }

    /// Number of rows passing the filter.
    pub fn len(&self) -> usize {
        self.matches.len()
    }

    /// Whether no row passes the filter.
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Index of the highlighted row within the filtered view.
    pub fn selected_index(&self) -> usize {
        self.selected
    }

    /// The highlighted item.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.matches.get(self.selected).map(|idx| &self.items[*idx])
    }

    /// Position of the item carrying `value` in the filtered view.
    pub fn position_of(&self, value: &str) -> Option<usize> {
        self.matches
            .iter()
            .position(|index| self.items[*index].value == value)
    }

    /// Highlight the row at `index` in the filtered view.
    pub fn set_selected(&mut self, index: usize) {
        if self.matches.is_empty() {
            self.selected = 0;
        } else {
            self.selected = index.min(self.matches.len() - 1);
        }
    }

    /// Move the highlight down.
    pub fn select_next(&mut self) {
        if !self.matches.is_empty() {
            self.selected = (self.selected + 1).min(self.matches.len() - 1);
        }
    }

    /// Move the highlight up.
    pub fn select_prev(&mut self) {
        self.selected = self.selected.saturating_sub(1);
    }

    /// Highlight the first row.
    pub fn select_first(&mut self) {
        self.selected = 0;
    }

    /// Highlight the last row.
    pub fn select_last(&mut self) {
        self.selected = self.matches.len().saturating_sub(1);
    }

    /// Apply a printable character or backspace to the filter.
    ///
    /// Returns `true` when the key was consumed.
    pub fn handle_filter_key(&mut self, key: &KeyEvent) -> bool {
        match key.code {
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                self.refilter();
                true
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.query.push(c);
                self.selected = 0;
                self.refilter();
                true
            }
            _ => false,
        }
    }

    fn refilter(&mut self) {
        let query = self.query.trim().to_lowercase();
        let mut scored: Vec<(i32, usize)> = self
            .items
            .iter()
            .enumerate()
            .filter_map(|(index, item)| {
                if query.is_empty() {
                    return Some((0, index));
                }
                let haystack = format!("{} {}", item.label, item.description);
                fuzzy_score(&query, &haystack).map(|score| (score, index))
            })
            .collect();

        // Highest score first; stable for equal scores.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        self.matches = scored.into_iter().map(|(_, index)| index).collect();
        self.version = self.version.wrapping_add(1);

        if self.selected >= self.matches.len() {
            self.selected = self.matches.len().saturating_sub(1);
        }
    }

    fn visible_rows(&self) -> u16 {
        self.max_visible.min(self.last_area.height.max(1))
    }

    fn sync_scroll(&mut self) {
        let visible = self.visible_rows() as usize;
        if visible == 0 {
            self.scroll = 0;
            return;
        }
        if self.selected < self.scroll {
            self.scroll = self.selected;
        } else if self.selected >= self.scroll + visible {
            self.scroll = self.selected + 1 - visible;
        }
    }
}

impl Component for SelectList {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        self.sync_scroll();

        if self.matches.is_empty() {
            let message = if self.query.is_empty() {
                "(nothing to select)"
            } else {
                "(no matches)"
            };
            buf.set_string(area.x, area.y, message, self.theme.empty);
            return;
        }

        // Claude-Code-style rows: "❯ 3. label · description".
        //
        // The cursor occupies the two columns the numbering is indented by, and
        // the numbers are right-aligned so labels stay in one column even when
        // the filtered list grows past nine entries.
        let number_width = digit_count(self.matches.len());
        let cursor_width = 2u16;
        let label_column = cursor_width + number_width as u16 + 2;

        let visible = self.visible_rows() as usize;
        for (row, index) in self
            .matches
            .iter()
            .enumerate()
            .skip(self.scroll)
            .take(visible)
        {
            let y = area.y + (row - self.scroll) as u16;
            let item = &self.items[*index];
            let is_selected = row == self.selected;

            let base_style = if is_selected {
                self.theme.selected
            } else {
                self.theme.normal
            };

            let cursor = if is_selected { "❯ " } else { "  " };
            buf.set_string(area.x, y, cursor, base_style);

            let number = format!("{:>width$}. ", row + 1, width = number_width);
            buf.set_string(area.x + cursor_width, y, number, base_style);

            // A badge is pinned to the right edge and takes its width (plus a
            // gap) out of the description's budget, so the two never collide.
            let badge = item.badge.as_deref().filter(|text| !text.is_empty());
            let badge_width = badge.map_or(0, display_width) as u16;
            let reserve = if badge_width > 0 && badge_width + 2 < area.width {
                badge_width + 2
            } else {
                0
            };
            if let Some(badge) = badge.filter(|_| reserve > 0) {
                buf.set_string(
                    area.x + area.width - badge_width,
                    y,
                    badge,
                    self.theme.badge,
                );
            }

            let label_budget =
                (area.width.saturating_sub(reserve)).saturating_sub(label_column) as usize;
            let label = truncate_to_width(&item.label, label_budget, "…");
            let label_cells = display_width(&label) as u16;
            buf.set_string(area.x + label_column, y, &label, base_style);

            if item.description.is_empty() {
                continue;
            }

            // The description trails the label after a middle dot, dimmed.
            let separator = " · ";
            let separator_width = display_width(separator) as u16;
            let after_label = area.x + label_column + label_cells;
            let remaining =
                (area.x + area.width - reserve).saturating_sub(after_label + separator_width);
            if remaining < 4 {
                continue;
            }

            buf.set_string(after_label, y, separator, self.theme.description);
            let description = truncate_to_width(&item.description, remaining as usize, "…");
            buf.set_string(
                after_label + separator_width,
                y,
                description,
                self.theme.description,
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
        {
            return KeyResult::Ignored;
        }

        match key.code {
            KeyCode::Up => {
                self.select_prev();
                KeyResult::Handled
            }
            KeyCode::Down => {
                self.select_next();
                KeyResult::Handled
            }
            KeyCode::Home => {
                self.select_first();
                KeyResult::Handled
            }
            KeyCode::End => {
                self.select_last();
                KeyResult::Handled
            }
            KeyCode::PageUp => {
                let step = self.visible_rows().max(1) as usize;
                self.set_selected(self.selected.saturating_sub(step));
                KeyResult::Handled
            }
            KeyCode::PageDown => {
                let step = self.visible_rows().max(1) as usize;
                self.set_selected(self.selected.saturating_add(step));
                KeyResult::Handled
            }
            KeyCode::Enter => KeyResult::Confirmed,
            KeyCode::Esc => KeyResult::Cancelled,
            KeyCode::Backspace | KeyCode::Char(_) => {
                self.handle_filter_key(&key);
                KeyResult::Handled
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        if !rect_contains(self.last_area, event.column, event.row) {
            return MouseResult::Ignored;
        }
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.select_prev();
                MouseResult::Handled
            }
            MouseEventKind::ScrollDown => {
                self.select_next();
                MouseResult::Handled
            }
            MouseEventKind::Down(crossterm::event::MouseButton::Left) => {
                let row = event.row.saturating_sub(self.last_area.y) as usize;
                let index = self.scroll + row;
                if index < self.matches.len() {
                    self.selected = index;
                }
                MouseResult::Handled
            }
            _ => MouseResult::Ignored,
        }
    }

    fn desired_height(&mut self, _width: u16) -> Option<u16> {
        Some(self.matches.len().min(self.max_visible as usize) as u16)
    }

    fn version(&self) -> u64 {
        self.version
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("help", "help").description("show help"),
            SelectItem::new("theme", "theme").description("switch theme"),
            SelectItem::new("model", "model").description("switch model"),
        ]
    }

    fn rows(buffer: &Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn rendered(list: &mut SelectList, width: u16, height: u16) -> Vec<String> {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        list.render(&mut buffer, area);
        rows(&buffer)
    }

    #[test]
    fn renders_numbered_rows_with_a_cursor_and_dot_description() {
        let mut list = SelectList::new(items());
        let rows = rendered(&mut list, 40, 4);

        // The first row is selected by default, so it carries the cursor.
        assert_eq!(rows[0], "❯ 1. help · show help");
        assert_eq!(rows[1], "  2. theme · switch theme");
        assert_eq!(rows[2], "  3. model · switch model");
    }

    #[test]
    fn the_cursor_follows_the_selection() {
        let mut list = SelectList::new(items());
        list.select_next();
        let rows = rendered(&mut list, 40, 4);

        assert_eq!(rows[0], "  1. help · show help");
        assert_eq!(rows[1], "❯ 2. theme · switch theme");
    }

    #[test]
    fn numbering_tracks_the_filtered_view() {
        let mut list = SelectList::new(items());
        list.handle_filter_key(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()));
        let rows = rendered(&mut list, 40, 4);

        // Two matches, renumbered from the top of the filtered list.
        assert_eq!(rows[0], "❯ 1. model · switch model");
        assert_eq!(rows[1], "  2. theme · switch theme");
        assert!(rows[2].is_empty());
    }

    #[test]
    fn position_of_finds_values_in_the_filtered_view() {
        let mut list = SelectList::new(items());
        assert_eq!(list.position_of("theme"), Some(1));
        assert_eq!(list.position_of("missing"), None);

        // After filtering, positions are relative to the filtered list.
        list.handle_filter_key(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()));
        assert_eq!(list.position_of("model"), Some(0));
        assert_eq!(list.position_of("help"), None);
    }

    #[test]
    fn numbers_align_once_the_list_passes_nine_entries() {
        let items: Vec<_> = (0..12)
            .map(|i| SelectItem::new(format!("v{i}"), format!("opt{i}")))
            .collect();
        let mut list = SelectList::new(items).max_visible(12);
        let rows = rendered(&mut list, 20, 12);

        // Two-digit numbering keeps every label in the same column.
        assert_eq!(rows[0], "❯  1. opt0");
        assert_eq!(rows[9], "  10. opt9");
        assert_eq!(rows[11], "  12. opt11");
    }

    #[test]
    fn long_descriptions_are_truncated_not_overflowed() {
        let mut list = SelectList::new(vec![
            SelectItem::new("a", "alpha").description("a description that will not fit"),
        ]);
        let rows = rendered(&mut list, 24, 2);
        assert_eq!(display_width(&rows[0]), 24);
        assert!(rows[0].ends_with('…'), "{:?}", rows[0]);
    }

    #[test]
    fn a_label_without_a_description_has_no_separator() {
        let mut list = SelectList::new(vec![SelectItem::new("a", "alpha")]);
        assert_eq!(rendered(&mut list, 20, 2)[0], "❯ 1. alpha");
    }

    #[test]
    fn badges_are_pinned_to_the_right_edge() {
        let mut list = SelectList::new(vec![
            SelectItem::new("groq", "Groq")
                .description("fast")
                .badge("FREE"),
            SelectItem::new("local", "Local runtime").badge("LOCAL"),
        ]);
        let rows = rendered(&mut list, 40, 3);

        assert!(rows[0].starts_with("❯ 1. Groq · fast"));
        assert!(rows[1].starts_with("  2. Local runtime"));
        // Both badges are flush with the right edge.
        assert!(rows[0].ends_with("FREE"), "{:?}", rows[0]);
        assert!(rows[1].ends_with("LOCAL"), "{:?}", rows[1]);
        assert_eq!(display_width(&rows[0]), 40);
        assert_eq!(display_width(&rows[1]), 40);
    }

    #[test]
    fn a_badge_never_overlaps_the_description() {
        // Narrow enough that the badge would collide without reservation.
        let mut list = SelectList::new(vec![
            SelectItem::new("groq", "Groq")
                .description("open models on fast hardware")
                .badge("FREE"),
        ]);
        let rows = rendered(&mut list, 30, 2);

        assert_eq!(display_width(&rows[0]), 30);
        assert!(rows[0].ends_with("FREE"), "{:?}", rows[0]);
        assert!(
            rows[0].contains('…'),
            "description should be truncated: {:?}",
            rows[0]
        );
    }

    #[test]
    fn an_overlong_badge_the_width_of_the_row_is_skipped() {
        let mut list = SelectList::new(vec![
            SelectItem::new("x", "x").badge("AN-EXTREMELY-LONG-BADGE"),
        ]);
        let rows = rendered(&mut list, 12, 2);
        assert!(!rows[0].contains("AN-EXTREMELY"), "{:?}", rows[0]);
        assert_eq!(rows[0], "❯ 1. x");
    }

    #[test]
    fn filters_by_subsequence() {
        let mut list = SelectList::new(items());
        assert_eq!(list.len(), 3);

        list.handle_filter_key(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()));
        let values: Vec<_> = list
            .matches
            .iter()
            .map(|i| list.items[*i].value.as_str())
            .collect();
        assert_eq!(values, vec!["model", "theme"]);

        list.handle_filter_key(&KeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty()));
        assert!(list.is_empty());
        assert!(list.selected_item().is_none());
    }

    #[test]
    fn backspace_restores_matches() {
        let mut list = SelectList::new(items());
        list.handle_filter_key(&KeyEvent::new(KeyCode::Char('m'), KeyModifiers::empty()));
        list.handle_filter_key(&KeyEvent::new(KeyCode::Backspace, KeyModifiers::empty()));
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn navigation_clamps_to_bounds() {
        let mut list = SelectList::new(items());
        list.select_prev();
        assert_eq!(list.selected_index(), 0);
        for _ in 0..10 {
            list.select_next();
        }
        assert_eq!(list.selected_index(), 2);
        assert_eq!(list.selected_item().unwrap().value, "model");
    }

    #[test]
    fn enter_confirms_and_escape_cancels() {
        let mut list = SelectList::new(items());
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::empty())),
            KeyResult::Confirmed
        );
        assert_eq!(
            list.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::empty())),
            KeyResult::Cancelled
        );
    }
}
