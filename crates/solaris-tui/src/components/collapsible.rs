//! A reusable disclosure row with styled content and selection-safe interaction.
//!
//! Retained layouts use [`Collapsible`] as a normal [`Component`]. Line-based
//! transcripts use the same [`Collapsible::lines`] renderer and keep a copy of
//! [`CollapsibleState`] with each record. Rebuilding content never resets it.
//!
//! ```
//! use ratatui::text::Line;
//! use solaris_tui::components::{Collapsible, CollapsibleStatus};
//!
//! let mut output = Collapsible::new("Read src/lib.rs");
//! output.set_status(CollapsibleStatus::Running);
//! output.set_content(vec![Line::raw("Tool output can go here.")]);
//! assert_eq!(output.lines(60).len(), 1); // initially folded
//! output.set_expanded(true);
//! assert_eq!(output.lines(60).len(), 2);
//! output.set_status(CollapsibleStatus::Idle); // stays expanded
//! assert!(output.state().is_expanded());
//!
//! let mut group = Collapsible::new("Tools");
//! group.push_child(output);
//! group.set_expanded(true);
//! assert!(group.lines(60).len() > 2);
//! ```

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Widget};
use unicode_segmentation::UnicodeSegmentation;

use crate::component::{Component, KeyResult, MouseResult};
use crate::selection::SelectionHandle;
use crate::theme::Theme;
use crate::util::{display_width, rect_contains};

/// Persistent disclosure state, independent of content, execution, and layout.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollapsibleState {
    expanded: bool,
}

impl CollapsibleState {
    pub fn is_expanded(self) -> bool {
        self.expanded
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        self.expanded = expanded;
    }

    /// Toggle and return the new expansion state.
    pub fn toggle(&mut self) -> bool {
        self.expanded = !self.expanded;
        self.expanded
    }
}

/// Generic activity state; the caller supplies any domain-specific title.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CollapsibleStatus {
    #[default]
    Idle,
    Running,
    Failed,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CollapsibleStyles {
    pub title: Style,
    pub marker: Style,
    pub running: Style,
    pub failed: Style,
}

impl CollapsibleStyles {
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            title: Style::default().fg(theme.muted),
            marker: Style::default().fg(theme.muted),
            running: Style::default().fg(theme.accent),
            failed: Style::default().fg(theme.error),
        }
    }
}

/// Screen-space hit target for either a retained component or a virtual row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollapsibleHit<T> {
    target: T,
    header: Rect,
    marker_offset: u16,
}

impl<T> CollapsibleHit<T> {
    pub fn new(target: T, header: Rect, marker_offset: u16) -> Self {
        Self {
            target,
            header,
            marker_offset,
        }
    }
}

/// A pointer gesture may be consumed without changing expansion (e.g. selection).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollapsibleAction<T> {
    Ignored,
    Handled,
    Toggle(T),
}

/// Shared click/drag recognition for any number of caller-identified records.
pub struct CollapsibleInteraction<T = ()> {
    pressed: Option<(u16, u16, T)>,
}

impl<T> Default for CollapsibleInteraction<T> {
    fn default() -> Self {
        Self { pressed: None }
    }
}

impl<T: PartialEq> CollapsibleInteraction<T> {
    /// `selection_active` lets a containing TUI retain word/paragraph selection.
    /// The marker itself remains a repeat-clickable control. A drag never toggles.
    pub fn handle_mouse(
        &mut self,
        event: MouseEvent,
        hit: Option<CollapsibleHit<T>>,
        selection_active: bool,
    ) -> CollapsibleAction<T> {
        let hit = hit.filter(|hit| rect_contains(hit.header, event.column, event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                self.pressed = hit.map(|hit| (event.column, event.row, hit.target));
                if self.pressed.is_some() {
                    CollapsibleAction::Handled
                } else {
                    CollapsibleAction::Ignored
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let Some((x, y, target)) = self.pressed.take() else {
                    return CollapsibleAction::Ignored;
                };
                if let Some(hit) = hit {
                    let marker = hit.marker_offset < hit.header.width
                        && event.column == hit.header.x.saturating_add(hit.marker_offset);
                    if (event.column, event.row) == (x, y)
                        && target == hit.target
                        && (!selection_active || marker)
                    {
                        return CollapsibleAction::Toggle(target);
                    }
                }
                CollapsibleAction::Handled
            }
            MouseEventKind::Drag(MouseButton::Left)
            | MouseEventKind::ScrollUp
            | MouseEventKind::ScrollDown
            | MouseEventKind::ScrollLeft
            | MouseEventKind::ScrollRight => {
                self.cancel();
                CollapsibleAction::Ignored
            }
            _ => CollapsibleAction::Ignored,
        }
    }

    pub fn cancel(&mut self) {
        self.pressed = None;
    }
}

/// Collapsed by default, with `→` when folded and `↓` when expanded, regardless
/// of execution status. The body can contain text and nested disclosures;
/// collapsing an ancestor preserves every descendant's state.
pub struct Collapsible {
    title: Line<'static>,
    content: Vec<Line<'static>>,
    children: Vec<Collapsible>,
    state: CollapsibleState,
    status: CollapsibleStatus,
    styles: CollapsibleStyles,
    indent: u16,
    body_indent: u16,
    revision: u64,
    last_area: Rect,
    interaction: CollapsibleInteraction<Vec<usize>>,
    focused: Vec<usize>,
    selection: Option<SelectionHandle>,
}

impl Collapsible {
    pub fn new(title: impl Into<Line<'static>>) -> Self {
        Self {
            title: title.into(),
            content: Vec::new(),
            children: Vec::new(),
            state: CollapsibleState::default(),
            status: CollapsibleStatus::Idle,
            styles: CollapsibleStyles::default(),
            indent: 0,
            body_indent: 2,
            revision: 1,
            last_area: Rect::default(),
            interaction: CollapsibleInteraction::default(),
            focused: Vec::new(),
            selection: None,
        }
    }

    /// Rehydrate a record's expansion state in a line-based view.
    pub fn with_state(mut self, state: CollapsibleState) -> Self {
        self.set_state(state);
        self
    }

    pub fn styles(mut self, styles: CollapsibleStyles) -> Self {
        self.set_styles(styles);
        self
    }

    /// Update the palette without recreating a retained component.
    pub fn set_styles(&mut self, styles: CollapsibleStyles) {
        if self.styles != styles {
            self.styles = styles;
            self.bump();
        }
    }

    /// Absolute indentation of the header and body, respectively.
    pub fn indentation(mut self, header: u16, body: u16) -> Self {
        self.set_indentation(header, body);
        self
    }

    pub fn set_indentation(&mut self, header: u16, body: u16) {
        if (self.indent, self.body_indent) != (header, body) {
            self.indent = header;
            self.body_indent = body;
            self.interaction.cancel();
            self.bump();
        }
    }

    /// Share the enclosing TUI's selection to distinguish clicks from word picks.
    pub fn selection(mut self, selection: SelectionHandle) -> Self {
        self.selection = Some(selection);
        self
    }

    pub fn state(&self) -> CollapsibleState {
        self.state
    }

    pub fn set_state(&mut self, state: CollapsibleState) {
        if self.state != state {
            self.state = state;
            if let Some(selection) = &self.selection {
                selection.borrow_mut().clear();
            }
            self.bump();
        }
    }

    pub fn set_expanded(&mut self, expanded: bool) {
        let mut state = self.state;
        state.set_expanded(expanded);
        self.set_state(state);
    }

    pub fn set_title(&mut self, title: impl Into<Line<'static>>) {
        let title = title.into();
        if self.title != title {
            self.title = title;
            self.bump();
        }
    }

    pub fn set_content(&mut self, content: Vec<Line<'static>>) {
        if self.content != content {
            self.content = content;
            self.bump();
        }
    }

    /// Nested disclosures follow the text body and keep their own expansion state.
    /// Their indentation is relative to this component's body indentation.
    pub fn set_children(&mut self, children: Vec<Collapsible>) {
        self.children = children;
        self.focused.clear();
        self.interaction.cancel();
        self.bump();
    }

    pub fn push_child(&mut self, child: Collapsible) {
        self.children.push(child);
        self.bump();
    }

    /// Resolve an index path; an empty path addresses this component.
    pub fn descendant(&self, path: &[usize]) -> Option<&Collapsible> {
        match path.split_first() {
            None => Some(self),
            Some((&index, rest)) => self.children.get(index)?.descendant(rest),
        }
    }

    pub fn descendant_mut(&mut self, path: &[usize]) -> Option<&mut Collapsible> {
        match path.split_first() {
            None => Some(self),
            Some((&index, rest)) => {
                if index >= self.children.len() {
                    return None;
                }
                // Mutable access invalidates every ancestor's render cache.
                self.bump();
                self.children.get_mut(index)?.descendant_mut(rest)
            }
        }
    }

    pub fn set_status(&mut self, status: CollapsibleStatus) {
        if self.status != status {
            self.status = status;
            self.bump();
        }
    }

    pub fn marker_offset(&self) -> u16 {
        self.indent
    }

    /// Common renderer used by both `Component::render` and transcript adapters.
    /// Lines are clipped in terminal cells while preserving span styles.
    pub fn lines(&self, width: u16) -> Vec<Line<'static>> {
        self.layout(width).lines
    }

    /// Render lines and visible header paths using the same recursive layout.
    pub fn layout(&self, width: u16) -> CollapsibleLayout {
        if width == 0 {
            return CollapsibleLayout::default();
        }
        let expanded = self.state.is_expanded();
        let glyph = if expanded { "↓" } else { "→" };
        let marker_style = match self.status {
            CollapsibleStatus::Failed => self.styles.failed,
            CollapsibleStatus::Running if !expanded => self.styles.running,
            _ => self.styles.marker,
        };
        let title_style = if self.status == CollapsibleStatus::Failed {
            self.styles.failed
        } else {
            self.styles.title
        };
        let mut title = self.title.clone();
        title.style = title_style.patch(title.style);
        title.spans.insert(
            0,
            Span::styled(
                format!(
                    "{}{glyph} ",
                    " ".repeat(usize::from(self.indent.min(width)))
                ),
                marker_style,
            ),
        );
        let mut lines = vec![clip_line(title, usize::from(width))];
        let mut headers = Vec::new();
        if expanded {
            for line in self.content.iter().take(usize::from(u16::MAX) - 1) {
                let mut line = line.clone();
                line.spans.insert(
                    0,
                    Span::raw(" ".repeat(usize::from(self.body_indent.min(width)))),
                );
                lines.push(clip_line(line, usize::from(width)));
            }
            let indent = self.body_indent.min(width);
            for (index, child) in self.children.iter().enumerate() {
                let child_layout = child.layout(width.saturating_sub(indent));
                let offset = lines.len();
                for mut header in child_layout.headers {
                    header.path.insert(0, index);
                    header.row += offset;
                    header.marker_offset = header.marker_offset.saturating_add(indent);
                    headers.push(header);
                }
                for mut line in child_layout.lines {
                    line.spans
                        .insert(0, Span::raw(" ".repeat(usize::from(indent))));
                    lines.push(line);
                }
            }
        }
        lines.truncate(usize::from(u16::MAX));
        headers.retain(|header| header.row < lines.len());
        headers.insert(
            0,
            CollapsibleRow {
                path: Vec::new(),
                row: 0,
                marker_offset: self.indent,
                body_rows: lines.len().saturating_sub(1),
            },
        );
        CollapsibleLayout { lines, headers }
    }

    fn bump(&mut self) {
        // Zero means "unknown" to Component caches; an owned component is known.
        self.revision = self.revision.wrapping_add(1).max(1);
    }
}

/// One visible disclosure header, relative to the root component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollapsibleRow {
    pub path: Vec<usize>,
    pub row: usize,
    pub marker_offset: u16,
    pub body_rows: usize,
}

#[derive(Default)]
pub struct CollapsibleLayout {
    pub lines: Vec<Line<'static>>,
    pub headers: Vec<CollapsibleRow>,
}

impl Component for Collapsible {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if self.last_area != area {
            self.interaction.cancel();
        }
        self.last_area = area;
        if area.width == 0 || area.height == 0 {
            return;
        }
        Clear.render(area, buf);
        for (row, line) in self
            .lines(area.width)
            .iter()
            .take(usize::from(area.height))
            .enumerate()
        {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        if key.modifiers != KeyModifiers::empty() {
            return KeyResult::Ignored;
        }
        let width = if self.last_area.width == 0 {
            u16::MAX
        } else {
            self.last_area.width
        };
        let headers = self.layout(width).headers;
        if !headers.iter().any(|header| header.path == self.focused) {
            self.focused.clear();
        }
        if matches!(key.code, KeyCode::Up | KeyCode::Down) {
            let current = headers
                .iter()
                .position(|header| header.path == self.focused)
                .unwrap_or(0);
            let next = if key.code == KeyCode::Down {
                (current + 1).min(headers.len() - 1)
            } else {
                current.saturating_sub(1)
            };
            self.focused = headers[next].path.clone();
            return KeyResult::Handled;
        }
        let path = self.focused.clone();
        let Some(node) = self.descendant_mut(&path) else {
            return KeyResult::Ignored;
        };
        let expanded = match key.code {
            KeyCode::Enter | KeyCode::Char(' ') => !node.state.is_expanded(),
            KeyCode::Left => false,
            KeyCode::Right => true,
            _ => return KeyResult::Ignored,
        };
        node.set_expanded(expanded);
        if let Some(selection) = &self.selection {
            selection.borrow_mut().clear();
        }
        KeyResult::Handled
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        let hit = self
            .layout(self.last_area.width)
            .headers
            .into_iter()
            .find(|header| {
                header.row < usize::from(self.last_area.height)
                    && event.row == self.last_area.y.saturating_add(header.row as u16)
            })
            .map(|header| {
                CollapsibleHit::new(
                    header.path,
                    Rect::new(self.last_area.x, event.row, self.last_area.width, 1),
                    header.marker_offset,
                )
            });
        let selected = self
            .selection
            .as_ref()
            .is_some_and(|selection| selection.borrow().is_active());
        match self.interaction.handle_mouse(event, hit, selected) {
            CollapsibleAction::Ignored => MouseResult::Ignored,
            CollapsibleAction::Handled => MouseResult::Handled,
            CollapsibleAction::Toggle(path) => {
                if let Some(node) = self.descendant_mut(&path) {
                    node.set_expanded(!node.state.is_expanded());
                }
                self.focused = path;
                if let Some(selection) = &self.selection {
                    selection.borrow_mut().clear();
                }
                MouseResult::Handled
            }
        }
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        Some(u16::try_from(self.lines(width).len()).unwrap_or(u16::MAX))
    }

    fn version(&self) -> u64 {
        self.revision
    }

    fn invalidate(&mut self) {
        self.interaction.cancel();
        self.last_area = Rect::default();
        self.bump();
    }
}

fn clip_line(mut line: Line<'static>, width: usize) -> Line<'static> {
    if line.width() <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut spans = Vec::new();
    let mut used = 0;
    'spans: for span in &line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let cells = display_width(grapheme);
            if used + cells > budget {
                if !text.is_empty() {
                    spans.push(Span::styled(text, span.style));
                }
                break 'spans;
            }
            text.push_str(grapheme);
            used += cells;
        }
        if !text.is_empty() {
            spans.push(Span::styled(text, span.style));
        }
    }
    if width != 0 {
        spans.push(Span::raw("…"));
    }
    line.spans = spans;
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Tui;
    use crate::components::ScrollView;
    use ratatui::style::{Color, Modifier};

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn pointer(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn output() -> Collapsible {
        let mut output = Collapsible::new("Tool output");
        output.set_content(vec![Line::raw("first result"), Line::raw("second result")]);
        output
    }

    #[test]
    fn nested_disclosures_expand_independently_and_keep_state_when_parent_closes() {
        let mut grandchild = Collapsible::new("Grandchild");
        grandchild.set_content(vec![Line::raw("deep output")]);
        let mut child = Collapsible::new("Child");
        child.set_content(vec![Line::raw("child output")]);
        child.push_child(grandchild);
        let mut root = Collapsible::new("Parent");
        root.push_child(child);
        root.set_expanded(true);
        let area = Rect::new(0, 0, 40, 20);
        let mut buf = Buffer::empty(area);
        root.render(&mut buf, area);
        let mut click = |root: &mut Collapsible, column, row| {
            root.handle_mouse(pointer(
                MouseEventKind::Down(MouseButton::Left),
                column,
                row,
            ));
            root.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), column, row));
            root.render(&mut buf, area);
        };
        assert_eq!(root.desired_height(40), Some(2));
        click(&mut root, 2, 1);
        assert!(root.state().is_expanded());
        assert!(root.descendant(&[0]).unwrap().state().is_expanded());
        assert_eq!(root.desired_height(40), Some(4));
        let version = root.version();
        click(&mut root, 4, 3);
        assert!(root.version() > version);
        assert_eq!(
            root.lines(40).iter().map(text).collect::<Vec<_>>(),
            [
                "↓ Parent",
                "  ↓ Child",
                "    child output",
                "    ↓ Grandchild",
                "      deep output",
            ]
        );
        click(&mut root, 0, 0);
        assert_eq!(root.desired_height(40), Some(1));
        click(&mut root, 0, 0);
        assert_eq!(root.desired_height(40), Some(5));
        let layout = root.layout(40);
        assert_eq!(layout.headers[2].path, [0, 0]);
        assert_eq!(layout.headers[2].row, 3);
        assert_eq!(layout.headers[2].marker_offset, 4);
    }

    #[test]
    fn keyboard_can_focus_and_toggle_a_child_without_closing_the_parent() {
        let mut root = Collapsible::new("Parent");
        root.push_child(output());
        root.handle_key(key(KeyCode::Right));
        root.handle_key(key(KeyCode::Down));
        root.handle_key(key(KeyCode::Right));
        assert!(root.descendant(&[0]).unwrap().state().is_expanded());
        root.handle_key(key(KeyCode::Left));
        assert!(!root.descendant(&[0]).unwrap().state().is_expanded());
        assert!(root.state().is_expanded());
    }

    #[test]
    fn nested_layout_clips_to_width_and_ignores_hidden_or_clipped_headers() {
        let mut root = Collapsible::new("Parent").indentation(2, 4);
        root.push_child(output());
        root.set_expanded(true);
        for width in [0, 1, 4, 8, 20] {
            assert!(
                root.lines(width)
                    .iter()
                    .all(|line| line.width() <= usize::from(width))
            );
        }
        let area = Rect::new(0, 0, 40, 1);
        let mut buf = Buffer::empty(area);
        root.render(&mut buf, area);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            root.handle_mouse(pointer(kind, 4, 1));
        }
        assert!(!root.descendant(&[0]).unwrap().state().is_expanded());
    }

    #[test]
    fn dragging_a_nested_header_never_toggles_the_child_or_parent() {
        let mut root = Collapsible::new("Parent");
        root.push_child(output());
        root.set_expanded(true);
        let area = Rect::new(0, 0, 40, 10);
        root.render(&mut Buffer::empty(area), area);
        root.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 2, 1));
        root.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 10, 1));
        root.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 2, 1));
        assert!(!root.descendant(&[0]).unwrap().state().is_expanded());
        assert!(root.state().is_expanded());
    }

    #[test]
    fn any_content_defaults_to_a_single_folded_row() {
        let mut output = output();
        assert!(!output.state().is_expanded());
        assert_eq!(output.desired_height(40), Some(1));
        assert_eq!(
            output.lines(40).iter().map(text).collect::<Vec<_>>(),
            ["→ Tool output"]
        );
    }

    #[test]
    fn opening_reveals_the_body_and_closing_hides_it() {
        let mut output = output();
        output.set_expanded(true);
        assert_eq!(output.desired_height(40), Some(3));
        assert_eq!(
            output.lines(40).iter().map(text).collect::<Vec<_>>(),
            ["↓ Tool output", "  first result", "  second result"]
        );
        output.set_expanded(false);
        assert_eq!(output.lines(40).len(), 1);
    }

    #[test]
    fn status_and_content_updates_preserve_expansion_and_can_be_rehydrated() {
        let mut output = output();
        output.set_expanded(true);
        output.set_status(CollapsibleStatus::Running);
        output.set_title("Updated title");
        output.set_content(vec![Line::raw("new content")]);
        output.set_status(CollapsibleStatus::Idle);
        let mut rebuilt = Collapsible::new("Another presentation").with_state(output.state());
        rebuilt.set_content(vec![Line::raw("still open")]);
        assert!(rebuilt.state().is_expanded());
        assert_eq!(rebuilt.lines(40).len(), 2);
    }

    #[test]
    fn retained_content_and_theme_updates_advance_versions_without_resetting_state() {
        let mut output = output();
        output.set_expanded(true);
        let mut version = output.version();
        output.set_title("Updated output");
        assert!(output.version() > version);
        version = output.version();
        output.set_content(vec![Line::raw("Replacement result")]);
        assert!(output.version() > version);
        version = output.version();
        let styles = CollapsibleStyles::from_theme(&Theme::light());
        output.set_styles(styles);
        assert!(output.version() > version);
        version = output.version();
        output.set_styles(styles);
        assert_eq!(output.version(), version);
        output.set_indentation(2, 4);
        assert!(output.version() > version);
        let lines = output.lines(40);
        assert_eq!(text(&lines[0]), "  ↓ Updated output");
        assert_eq!(text(&lines[1]), "    Replacement result");
        assert!(output.state().is_expanded());
    }

    #[test]
    fn all_statuses_keep_static_arrows_without_animation_redraws() {
        let mut output = output();
        for status in [
            CollapsibleStatus::Idle,
            CollapsibleStatus::Running,
            CollapsibleStatus::Failed,
        ] {
            output.set_status(status);
            for expanded in [false, true] {
                output.set_expanded(expanded);
                let lines = output.lines(40);
                let glyph = if expanded { "↓" } else { "→" };
                assert_eq!(text(&lines[0]), format!("{glyph} Tool output"));
                let version = output.version();
                for _ in 0..120 {
                    assert!(!output.tick());
                }
                assert_eq!(output.version(), version);
                assert_eq!(output.lines(40), lines);
            }
        }
    }

    #[test]
    fn styled_spans_and_indentation_survive_cell_width_clipping() {
        let title = Line::from(vec![
            Span::styled("Read ", Style::default().fg(Color::Yellow)),
            Span::styled("source", Style::default().add_modifier(Modifier::BOLD)),
        ]);
        let mut output = Collapsible::new(title).indentation(2, 4);
        output.set_content(vec![Line::styled(
            "彩色结果",
            Style::default().fg(Color::Blue),
        )]);
        output.set_expanded(true);
        let lines = output.lines(12);
        assert!(text(&lines[0]).starts_with("  ↓ Read "));
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.content == "Read " && span.style.fg == Some(Color::Yellow))
        );
        assert_eq!(lines[1].style.fg, Some(Color::Blue));
        assert_eq!(text(&lines[1]), "    彩色结果");
        assert!(lines.iter().all(|line| line.width() <= 12));
    }

    #[test]
    fn cjk_and_emoji_graphemes_fit_even_extremely_narrow_widths() {
        let mut output = Collapsible::new("状态 e\u{301} 👩‍💻 日志").indentation(2, 4);
        output.set_content(vec![Line::raw("👩‍💻 e\u{301} 中文")]);
        output.set_expanded(true);
        for width in 0..30 {
            let lines = output.lines(width);
            assert!(
                lines.iter().all(|line| line.width() <= usize::from(width)),
                "width={width}: {lines:?}"
            );
            for line in &lines {
                let text = text(line);
                assert!(!text.contains('👩') || text.contains("👩‍💻"), "{text}");
            }
            assert_eq!(
                output.desired_height(width),
                Some(if width == 0 { 0 } else { 2 })
            );
        }
    }

    #[test]
    fn failed_status_uses_error_styles_without_changing_the_disclosure_glyphs() {
        let styles = CollapsibleStyles {
            failed: Style::default().fg(Color::Red),
            ..Default::default()
        };
        let mut output = output().styles(styles);
        output.set_status(CollapsibleStatus::Failed);
        let lines = output.lines(40);
        assert!(text(&lines[0]).starts_with('→'));
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::Red));
        output.set_expanded(true);
        assert!(text(&output.lines(40)[0]).starts_with('↓'));
    }

    #[test]
    fn zero_sized_render_ignores_pointer_events() {
        let mut output = output();
        let area = Rect::default();
        output.render(&mut Buffer::empty(area), area);
        assert_eq!(
            output.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 0, 0)),
            MouseResult::Ignored
        );
    }

    #[test]
    fn render_and_line_adapter_share_identical_layout_and_clear_hidden_content() {
        let mut output = output().indentation(2, 4);
        output.set_expanded(true);
        let area = Rect::new(3, 5, 30, 4);
        let mut buf = Buffer::empty(area);
        output.render(&mut buf, area);
        for (row, line) in output.lines(area.width).iter().enumerate() {
            let rendered: String = (area.x..area.right())
                .map(|x| buf[(x, area.y + row as u16)].symbol())
                .collect();
            assert_eq!(rendered.trim_end(), text(line));
        }
        output.set_expanded(false);
        output.render(&mut buf, area);
        assert!((area.x..area.right()).all(|x| buf[(x, area.y + 1)].symbol() == " "));
    }

    #[test]
    fn component_keyboard_controls_invalidate_version_only_when_state_changes() {
        let mut output = output();
        let initial = output.version();
        assert_ne!(initial, 0);
        assert_eq!(output.handle_key(key(KeyCode::Right)), KeyResult::Handled);
        assert!(output.state().is_expanded());
        let opened = output.version();
        assert_ne!(opened, initial);
        output.handle_key(key(KeyCode::Right));
        assert_eq!(output.version(), opened);
        output.handle_key(key(KeyCode::Enter));
        assert!(!output.state().is_expanded());
        output.handle_key(key(KeyCode::Char(' ')));
        assert!(output.state().is_expanded());
        output.handle_key(key(KeyCode::Left));
        assert!(!output.state().is_expanded());
        assert_eq!(
            output.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::CONTROL)),
            KeyResult::Ignored
        );
    }

    #[test]
    fn content_updates_and_expansion_refresh_a_scroll_view_cache() {
        let output = output();
        let mut scroll = ScrollView::new(Box::new(output)).follow_end(false);
        let area = Rect::new(0, 0, 30, 3);
        let mut buf = Buffer::empty(area);
        scroll.render(&mut buf, area);
        assert_eq!(buf[(2, 1)].symbol(), " ");
        scroll.content_mut().handle_key(key(KeyCode::Right));
        scroll.render(&mut buf, area);
        assert_eq!(buf[(2, 1)].symbol(), "f");
        scroll.content_mut().handle_key(key(KeyCode::Left));
        scroll.render(&mut buf, area);
        assert_eq!(buf[(2, 1)].symbol(), " ");
    }

    #[test]
    fn a_real_tui_can_repeatedly_click_the_marker_without_leaving_a_selection() {
        let mut tui = Tui::new();
        tui.set_root(Box::new(output().selection(tui.selection())));
        let area = Rect::new(0, 0, 30, 4);
        let mut buf = Buffer::empty(area);
        tui.render(&mut buf, area);
        for expanded in [true, false, true] {
            tui.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 0, 0));
            // Real terminal loops may redraw between the press and the release.
            tui.render(&mut buf, area);
            tui.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 0, 0));
            tui.render(&mut buf, area);
            assert_eq!(buf[(0, 0)].symbol(), if expanded { "↓" } else { "→" });
            assert!(!tui.selection().borrow().is_active());
        }
    }

    #[test]
    fn a_real_tui_preserves_drag_selection_over_a_disclosure_title() {
        let mut tui = Tui::new();
        tui.set_root(Box::new(output().selection(tui.selection())));
        let area = Rect::new(0, 0, 30, 4);
        let mut buf = Buffer::empty(area);
        tui.render(&mut buf, area);
        tui.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 2, 0));
        tui.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 5, 0));
        tui.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 5, 0));
        tui.render(&mut buf, area);
        assert_eq!(buf[(0, 0)].symbol(), "→");
        assert_eq!(tui.selection().borrow().selected_text(), "Tool");
    }

    #[test]
    fn virtual_hit_targets_do_not_toggle_on_release_over_another_record() {
        let mut interaction = CollapsibleInteraction::<u32>::default();
        let hit = |target| Some(CollapsibleHit::new(target, Rect::new(5, 7, 30, 1), 2));
        assert_eq!(
            interaction.handle_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left), 7, 7),
                hit(10),
                false
            ),
            CollapsibleAction::Handled
        );
        assert_eq!(
            interaction.handle_mouse(
                pointer(MouseEventKind::Up(MouseButton::Left), 7, 7),
                hit(11),
                false
            ),
            CollapsibleAction::Handled
        );
        assert_eq!(
            interaction.handle_mouse(
                pointer(MouseEventKind::Up(MouseButton::Left), 7, 7),
                hit(10),
                false
            ),
            CollapsibleAction::Ignored
        );
    }

    #[test]
    fn dragging_or_scrolling_cancels_virtual_header_presses() {
        for kind in [
            MouseEventKind::Drag(MouseButton::Left),
            MouseEventKind::ScrollUp,
            MouseEventKind::ScrollDown,
            MouseEventKind::ScrollLeft,
            MouseEventKind::ScrollRight,
        ] {
            let mut interaction = CollapsibleInteraction::<u32>::default();
            let hit = Some(CollapsibleHit::new(10, Rect::new(5, 7, 30, 1), 2));
            interaction.handle_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left), 7, 7),
                hit,
                false,
            );
            interaction.handle_mouse(pointer(kind, 7, 7), hit, false);
            assert_eq!(
                interaction.handle_mouse(
                    pointer(MouseEventKind::Up(MouseButton::Left), 7, 7),
                    hit,
                    false
                ),
                CollapsibleAction::Ignored
            );
        }
    }

    #[test]
    fn selection_blocks_virtual_text_clicks_but_not_the_marker() {
        let mut interaction = CollapsibleInteraction::<u32>::default();
        let hit = Some(CollapsibleHit::new(10, Rect::new(5, 7, 30, 1), 2));
        for (column, expected) in [
            (9, CollapsibleAction::Handled),
            (7, CollapsibleAction::Toggle(10)),
        ] {
            interaction.handle_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left), column, 7),
                hit,
                true,
            );
            assert_eq!(
                interaction.handle_mouse(
                    pointer(MouseEventKind::Up(MouseButton::Left), column, 7),
                    hit,
                    true
                ),
                expected
            );
        }
    }

    #[test]
    fn blank_areas_and_release_outside_the_header_never_toggle() {
        let mut interaction = CollapsibleInteraction::<u32>::default();
        let hit = Some(CollapsibleHit::new(10, Rect::new(5, 7, 30, 1), 2));
        assert_eq!(
            interaction.handle_mouse(
                pointer(MouseEventKind::Down(MouseButton::Left), 7, 8),
                hit,
                false
            ),
            CollapsibleAction::Ignored
        );
        interaction.handle_mouse(
            pointer(MouseEventKind::Down(MouseButton::Left), 7, 7),
            hit,
            false,
        );
        assert_eq!(
            interaction.handle_mouse(
                pointer(MouseEventKind::Up(MouseButton::Left), 7, 8),
                hit,
                false
            ),
            CollapsibleAction::Handled
        );
    }
}
