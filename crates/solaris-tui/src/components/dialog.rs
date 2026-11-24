//! Reusable modal dialog components built on borderless raised surfaces.
//!
//! Each dialog owns its own presentation and input handling and delivers its
//! outcome through an exactly-once callback (`Option<Box<dyn FnOnce(...)>>`),
//! so closing via keyboard, mouse, or dropping the overlay stack can never
//! lose a cancellation or fire twice.

use crossterm::event::{KeyCode, KeyEvent, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use crate::component::{Component, KeyResult, MouseResult};
use crate::components::select_list::{SelectItem, SelectList, SelectListTheme};
use crate::theme::Theme;
use crate::util::{display_width, rect_contains, truncate_to_width, wrap_text};

/// Hint shown at the bottom of a picker dialog.
pub const HINT_SELECT: &str = "Enter to select · Esc to cancel";

/// Hint shown at the bottom of a yes/no dialog.
pub const HINT_CONFIRM: &str = "Enter to confirm · Esc to cancel";

/// Styles shared by dialogs, derived from the active [`Theme`].
#[derive(Debug, Clone, Copy)]
pub struct DialogStyles {
    /// Fills the whole panel; the raised surface that replaces a border.
    pub surface: Style,
    pub title: Style,
    pub text: Style,
    pub muted: Style,
    pub hint: Style,
    pub accent: Style,
}

impl DialogStyles {
    /// Derive dialog styles from `theme`.
    pub fn from_theme(theme: &Theme) -> Self {
        Self {
            surface: Style::default().fg(theme.fg).bg(theme.surface),
            title: Style::default()
                .fg(theme.heading)
                .add_modifier(Modifier::BOLD),
            text: Style::default().fg(theme.fg),
            muted: Style::default().fg(theme.muted),
            hint: Style::default().fg(theme.dim),
            accent: Style::default().fg(theme.accent),
        }
    }

    pub fn list_theme(&self) -> SelectListTheme {
        SelectListTheme {
            normal: self.text,
            // Only the cursor row brightens; labels stay unaccented, as in the
            // Claude Code pickers where the accent is spent on the title.
            selected: self.text.add_modifier(Modifier::BOLD),
            description: self.hint,
            badge: self.accent.add_modifier(Modifier::BOLD),
            empty: self.hint,
        }
    }
}

/// Where a dialog may draw, once its chrome is painted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChromeLayout {
    /// Content region, already below the title block.
    pub content: Rect,
    /// Bottom row reserved for the hint line.
    pub hint: Rect,
}

/// Paint the flat, borderless dialog panel and return its regions.
///
/// Mirrors the Claude Code picker layout: accent title, blank line, then the
/// body/list, with a dimmed hint pinned to the bottom row. The panel is a
/// raised surface rather than a box, so the overlay reads as a card.
pub fn draw_chrome(
    buf: &mut Buffer,
    area: Rect,
    title: &str,
    styles: &DialogStyles,
) -> Option<ChromeLayout> {
    if area.width < 8 || area.height < 4 {
        return None;
    }

    buf.set_style(area, styles.surface);

    let inner = Rect {
        x: area.x + 1,
        y: area.y + 1,
        width: area.width.saturating_sub(2),
        height: area.height.saturating_sub(2),
    };
    if inner.width < 4 || inner.height < 3 {
        return None;
    }

    buf.set_string(
        inner.x,
        inner.y,
        truncate_to_width(title, inner.width as usize, "…"),
        styles.title,
    );

    // Title, blank line, content…, one blank row, then the hint.
    let content_top = inner.y + 2;
    let hint_y = inner.y + inner.height - 1;
    let content_bottom = hint_y.saturating_sub(1);

    Some(ChromeLayout {
        content: Rect {
            x: inner.x,
            y: content_top,
            width: inner.width,
            height: content_bottom.saturating_sub(content_top),
        },
        hint: Rect {
            x: inner.x,
            y: hint_y,
            width: inner.width,
            height: 1,
        },
    })
}

/// Draw the dim hint row, plus a right-aligned filter indicator when filtering.
pub fn draw_hint(
    buf: &mut Buffer,
    hint: Rect,
    text: &str,
    styles: &DialogStyles,
    filter: Option<&str>,
) {
    if hint.width == 0 {
        return;
    }

    buf.set_string(
        hint.x,
        hint.y,
        truncate_to_width(text, hint.width as usize, "…"),
        styles.hint,
    );

    if let Some(filter) = filter.filter(|value| !value.is_empty()) {
        let label = format!("filter: {filter}");
        let width = display_width(&label) as u16;
        if width + 2 < hint.width {
            buf.set_string(hint.x + hint.width - width, hint.y, label, styles.muted);
        }
    }
}

// ---------------------------------------------------------------------------
// Select dialog
// ---------------------------------------------------------------------------

type SelectCallback = Box<dyn FnOnce(Option<String>)>;

/// A filterable list dialog.
pub struct SelectDialog {
    title: String,
    label: Option<String>,
    body: Option<String>,
    hint: String,
    list: SelectList,
    total_items: usize,
    list_area: Rect,
    pressed_selected: Option<usize>,
    on_result: Option<SelectCallback>,
    styles: DialogStyles,
}

impl SelectDialog {
    /// Build a list dialog; `on_result` receives `Some(value)` on confirm or
    /// `None` on cancel/drop.
    pub fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        items: Vec<SelectItem>,
        theme: &Theme,
        on_result: impl FnOnce(Option<String>) + 'static,
    ) -> Self {
        let total_items = items.len();
        let styles = DialogStyles::from_theme(theme);
        Self {
            title: title.into(),
            label: None,
            body: None,
            hint: hint.into(),
            list: SelectList::new(items)
                .max_visible(10)
                .theme(styles.list_theme()),
            total_items,
            list_area: Rect::default(),
            pressed_selected: None,
            on_result: Some(Box::new(on_result)),
            styles,
        }
    }

    /// Add the prompt line shown above the list, e.g. `Select login method:`.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Preselect the item carrying `value`, so the cursor opens on the active
    /// choice (a no-op when the value is not in the list).
    pub fn selected(mut self, value: &str) -> Self {
        if let Some(position) = self.list.position_of(value) {
            self.list.set_selected(position);
        }
        self
    }

    /// Add an explanatory paragraph under the title.
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// The currently highlighted item, if any.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.list.selected_item()
    }

    /// Height needed for the title block, label, and up to 10 items.
    pub fn height_for(items: usize) -> u16 {
        // panel padding 2 + title 1 + blank 1 + label 1 + blank 1 + items
        // + blank 1 + hint 1
        (items.min(10) as u16) + 8
    }

    fn finish(&mut self, value: Option<String>) -> KeyResult {
        if let Some(callback) = self.on_result.take() {
            callback(value.clone());
        }
        if value.is_some() {
            KeyResult::Confirmed
        } else {
            KeyResult::Cancelled
        }
    }

    fn confirm(&mut self) -> KeyResult {
        match self.list.selected_item() {
            Some(item) => {
                let value = item.value.clone();
                self.finish(Some(value))
            }
            None => self.finish(None),
        }
    }
}

impl Drop for SelectDialog {
    fn drop(&mut self) {
        if let Some(callback) = self.on_result.take() {
            callback(None);
        }
    }
}

impl Component for SelectDialog {
    fn is_finished(&self) -> bool {
        self.on_result.is_none()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = draw_chrome(buf, area, &self.title, &self.styles) else {
            self.list_area = Rect::default();
            return;
        };
        let mut content = layout.content;

        if let Some(body) = &self.body {
            for line in wrap_text(body, content.width as usize) {
                if content.height == 0 {
                    break;
                }
                buf.set_string(
                    content.x,
                    content.y,
                    truncate_to_width(&line, content.width as usize, "…"),
                    self.styles.muted,
                );
                content.y += 1;
                content.height -= 1;
            }
            if content.height > 1 {
                content.y += 1;
                content.height -= 1;
            }
        }

        if let Some(label) = &self.label
            && content.height > 2
        {
            buf.set_string(
                content.x,
                content.y,
                truncate_to_width(label, content.width as usize, "…"),
                self.styles.text,
            );
            content.y += 2;
            content.height -= 2;
        }

        self.list_area = content;
        self.list.render(buf, content);
        draw_hint(
            buf,
            layout.hint,
            &self.hint,
            &self.styles,
            Some(self.list.query()),
        );
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.pressed_selected = None;
        match self.list.handle_key(key) {
            KeyResult::Confirmed => self.confirm(),
            KeyResult::Cancelled => self.finish(None),
            other => other,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let previous = self.list.selected_index();
                let handled = self.list.handle_mouse(event);
                if handled.is_handled() && rect_contains(self.list_area, event.column, event.row) {
                    let row = event.row.saturating_sub(self.list_area.y) as usize;
                    if row < self.list.len() && self.list.selected_index() == previous {
                        self.pressed_selected = Some(self.list.selected_index());
                    } else {
                        self.pressed_selected = None;
                    }
                } else {
                    self.pressed_selected = None;
                }
                handled
            }
            MouseEventKind::Drag(_) => {
                self.pressed_selected = None;
                MouseResult::Ignored
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed_selected.take();
                if let Some(index) = pressed
                    && rect_contains(self.list_area, event.column, event.row)
                    && self.list.selected_index() == index
                {
                    let _ = self.confirm();
                    return MouseResult::Handled;
                }
                MouseResult::Ignored
            }
            _ => self.list.handle_mouse(event),
        }
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let inner_width = width.saturating_sub(2).max(4) as usize;
        // panel padding 2 + title 1 + blank 1 + blank 1 + hint 1 = 6
        let mut rows: u16 = 6;
        if let Some(body) = &self.body {
            rows = rows.saturating_add(wrap_text(body, inner_width).len() as u16 + 1);
        }
        if self.label.is_some() {
            rows = rows.saturating_add(2);
        }
        let item_rows = self.total_items.clamp(1, 10) as u16;
        Some(rows.saturating_add(item_rows))
    }
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

type ConfirmCallback = Box<dyn FnOnce(bool)>;

/// A yes/no confirmation dialog with guaranteed exactly-once delivery.
pub struct ConfirmDialog {
    title: String,
    body: String,
    accept: bool,
    yes_rect: Rect,
    no_rect: Rect,
    pressed: Option<bool>,
    on_result: Option<ConfirmCallback>,
    styles: DialogStyles,
}

impl ConfirmDialog {
    /// Build a confirmation dialog; `on_result` receives `true` when accepted
    /// or `false` when declined, dismissed, or dropped.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        theme: &Theme,
        on_result: impl FnOnce(bool) + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            accept: false,
            yes_rect: Rect::default(),
            no_rect: Rect::default(),
            pressed: None,
            on_result: Some(Box::new(on_result)),
            styles: DialogStyles::from_theme(theme),
        }
    }

    /// Height needed for the title, wrapped `body`, and the button row.
    pub fn height_for(body: &str, width: u16) -> u16 {
        let wrap = width.saturating_sub(2).max(8) as usize;
        let rows = wrap_text(body, wrap).len().max(1) as u16;
        // panel padding 2 + title 1 + blank 1 + body + blank 1 + buttons 1
        // + blank 1 + hint 1
        rows + 8
    }

    fn finish(&mut self, accepted: bool) -> KeyResult {
        if let Some(callback) = self.on_result.take() {
            callback(accepted);
        }
        if accepted {
            KeyResult::Confirmed
        } else {
            KeyResult::Cancelled
        }
    }
}

impl Drop for ConfirmDialog {
    fn drop(&mut self) {
        if let Some(callback) = self.on_result.take() {
            callback(false);
        }
    }
}

impl Component for ConfirmDialog {
    fn is_finished(&self) -> bool {
        self.on_result.is_none()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = draw_chrome(buf, area, &self.title, &self.styles) else {
            self.yes_rect = Rect::default();
            self.no_rect = Rect::default();
            return;
        };
        let content = layout.content;

        // Body text fills everything above the button row.
        for (offset, segment) in wrap_text(&self.body, content.width as usize)
            .into_iter()
            .enumerate()
        {
            if offset as u16 >= content.height.saturating_sub(1) {
                break;
            }
            buf.set_string(
                content.x,
                content.y + offset as u16,
                truncate_to_width(&segment, content.width as usize, "…"),
                self.styles.text,
            );
        }

        // Button row pinned to the last content row.
        if content.height > 1 {
            let buttons_y = content.y + content.height - 1;
            let yes = if self.accept { "[ Yes ]" } else { "  Yes  " };
            let no = if self.accept { "  No  " } else { "[ No ]" };
            let bold = self.styles.text.add_modifier(Modifier::BOLD);
            let yes_style = if self.accept { bold } else { self.styles.muted };
            let no_style = if self.accept { self.styles.muted } else { bold };
            let yes_w = yes.len() as u16;
            let no_x = content.x + yes_w + 2;
            let no_w = no.len() as u16;
            buf.set_string(content.x, buttons_y, yes, yes_style);
            buf.set_string(no_x, buttons_y, no, no_style);
            self.yes_rect = Rect::new(content.x, buttons_y, yes_w.min(content.width), 1);
            self.no_rect = Rect::new(
                no_x,
                buttons_y,
                no_w.min(content.right().saturating_sub(no_x)),
                1,
            );
        } else {
            self.yes_rect = Rect::default();
            self.no_rect = Rect::default();
        }

        draw_hint(buf, layout.hint, HINT_CONFIRM, &self.styles, None);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.pressed = None;
        match key.code {
            KeyCode::Esc => self.finish(false),
            KeyCode::Enter => self.finish(self.accept),
            KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                self.accept = !self.accept;
                KeyResult::Handled
            }
            KeyCode::Char('y') | KeyCode::Char('Y') if key.modifiers.is_empty() => {
                self.finish(true)
            }
            KeyCode::Char('n') | KeyCode::Char('N') if key.modifiers.is_empty() => {
                self.finish(false)
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if rect_contains(self.yes_rect, event.column, event.row) {
                    self.accept = true;
                    self.pressed = Some(true);
                    return MouseResult::Handled;
                }
                if rect_contains(self.no_rect, event.column, event.row) {
                    self.accept = false;
                    self.pressed = Some(false);
                    return MouseResult::Handled;
                }
                self.pressed = None;
                MouseResult::Ignored
            }
            MouseEventKind::Drag(_) => {
                self.pressed = None;
                MouseResult::Ignored
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed.take();
                if pressed == Some(true) && rect_contains(self.yes_rect, event.column, event.row) {
                    let _ = self.finish(true);
                    return MouseResult::Handled;
                }
                if pressed == Some(false) && rect_contains(self.no_rect, event.column, event.row) {
                    let _ = self.finish(false);
                    return MouseResult::Handled;
                }
                MouseResult::Ignored
            }
            _ => MouseResult::Ignored,
        }
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        Some(Self::height_for(&self.body, width))
    }
}

// ---------------------------------------------------------------------------
// Text dialog (help, stats, skills, buddy, …)
// ---------------------------------------------------------------------------

type CloseCallback = Box<dyn FnOnce()>;
type LineBuilder = Box<dyn Fn(u16) -> Vec<Line<'static>>>;

enum TextSource {
    Static(Vec<Line<'static>>),
    Reflowable {
        build: LineBuilder,
        cached_width: u16,
        cached_lines: Vec<Line<'static>>,
    },
}

impl TextSource {
    fn lines_for(&mut self, width: u16) -> &[Line<'static>] {
        match self {
            TextSource::Static(lines) => lines,
            TextSource::Reflowable {
                build,
                cached_width,
                cached_lines,
            } => {
                let width = width.max(8);
                if *cached_width != width || cached_lines.is_empty() {
                    *cached_lines = build(width);
                    *cached_width = width;
                }
                cached_lines
            }
        }
    }
}

/// A scrollable read-only text dialog, supporting both fixed lines and
/// width-aware reflowable builders.
pub struct TextDialog {
    title: String,
    hint: String,
    source: TextSource,
    scroll: u16,
    max_scroll: u16,
    last_content: Rect,
    on_close: Option<CloseCallback>,
    styles: DialogStyles,
}

impl TextDialog {
    /// Build a text dialog from pre-rendered lines.
    pub fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        lines: Vec<Line<'static>>,
        theme: &Theme,
        on_close: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            source: TextSource::Static(lines),
            scroll: 0,
            max_scroll: 0,
            last_content: Rect::default(),
            on_close: Some(Box::new(on_close)),
            styles: DialogStyles::from_theme(theme),
        }
    }

    /// Build a text dialog whose lines reflow when the overlay width changes.
    pub fn reflowable(
        title: impl Into<String>,
        hint: impl Into<String>,
        theme: &Theme,
        build: impl Fn(u16) -> Vec<Line<'static>> + 'static,
        on_close: impl FnOnce() + 'static,
    ) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            source: TextSource::Reflowable {
                build: Box::new(build),
                cached_width: 0,
                cached_lines: Vec::new(),
            },
            scroll: 0,
            max_scroll: 0,
            last_content: Rect::default(),
            on_close: Some(Box::new(on_close)),
            styles: DialogStyles::from_theme(theme),
        }
    }

    /// Current vertical scroll offset (in rows).
    pub fn scroll(&self) -> u16 {
        self.scroll
    }

    /// Maximum vertical scroll offset from the most recent render.
    pub fn max_scroll(&self) -> u16 {
        self.max_scroll
    }

    fn close(&mut self) -> KeyResult {
        if let Some(callback) = self.on_close.take() {
            callback();
        }
        KeyResult::Cancelled
    }

    fn scroll_by(&mut self, delta: i32) {
        let target = (self.scroll as i32 + delta).clamp(0, self.max_scroll as i32);
        self.scroll = target as u16;
    }
}

impl Drop for TextDialog {
    fn drop(&mut self) {
        if let Some(callback) = self.on_close.take() {
            callback();
        }
    }
}

impl Component for TextDialog {
    fn is_finished(&self) -> bool {
        self.on_close.is_none()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = draw_chrome(buf, area, &self.title, &self.styles) else {
            return;
        };
        let content = layout.content;
        self.last_content = content;

        let lines = self.source.lines_for(content.width);
        self.max_scroll = (lines.len() as u16).saturating_sub(content.height);
        self.scroll = self.scroll.min(self.max_scroll);

        for (row, line) in lines
            .iter()
            .skip(self.scroll as usize)
            .take(content.height as usize)
            .enumerate()
        {
            buf.set_line(content.x, content.y + row as u16, line, content.width);
        }

        draw_hint(buf, layout.hint, &self.hint, &self.styles, None);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        let page = self.last_content.height.max(1) as i32;
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => self.close(),
            KeyCode::Up => {
                self.scroll_by(-1);
                KeyResult::Handled
            }
            KeyCode::Down => {
                self.scroll_by(1);
                KeyResult::Handled
            }
            KeyCode::PageUp => {
                self.scroll_by(-page);
                KeyResult::Handled
            }
            KeyCode::PageDown => {
                self.scroll_by(page);
                KeyResult::Handled
            }
            KeyCode::Home => {
                self.scroll = 0;
                KeyResult::Handled
            }
            KeyCode::End => {
                self.scroll = self.max_scroll;
                KeyResult::Handled
            }
            _ => KeyResult::Ignored,
        }
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        if !rect_contains(self.last_content, event.column, event.row) {
            return MouseResult::Ignored;
        }
        match event.kind {
            MouseEventKind::ScrollUp => {
                self.scroll_by(-3);
                MouseResult::Handled
            }
            MouseEventKind::ScrollDown => {
                self.scroll_by(3);
                MouseResult::Handled
            }
            _ => MouseResult::Ignored,
        }
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let inner_width = width.saturating_sub(2).max(4);
        let rows = self.source.lines_for(inner_width).len().max(1) as u16;
        // panel padding 2 + title 1 + blank 1 + content + blank 1 + hint 1 = rows + 6
        Some(rows.saturating_add(6))
    }
}
