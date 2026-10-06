//! Overlay dialogs.
//!
//! Every dialog is an ordinary [`Component`] pushed onto the framework overlay
//! stack, so there is exactly one modal path: the framework captures all input
//! while an overlay is visible, and each dialog reports its result through an
//! `mpsc` channel instead of reaching back into the application.

use crossterm::event::{KeyCode, KeyEvent};
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::markdown::{MarkdownStyle, render_markdown};
use solaris_tui::components::select_list::{SelectItem, SelectList, SelectListTheme};
use solaris_tui::theme::Theme;
use solaris_tui::util::{display_width, rect_contains, truncate_to_width, wrap_text};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::UnboundedSender;

/// Result of an interaction with a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogMessage {
    /// Dismissed without a choice.
    Cancelled,
    /// A theme was chosen.
    Theme(String),
    /// A model was chosen.
    Model(String),
    /// A slash command should be executed (from the palette).
    Command(String),
    /// A confirmation was answered.
    Confirm {
        action: ConfirmAction,
        accepted: bool,
    },
}

/// Which destructive action a confirmation guards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    ClearTranscript,
}

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

    fn list_theme(&self) -> SelectListTheme {
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
struct ChromeLayout {
    /// Content region, already below the title block.
    content: Rect,
    /// Bottom row reserved for the hint line.
    hint: Rect,
}

/// Paint the flat, borderless dialog panel and return its regions.
///
/// Mirrors the Claude Code picker layout: accent title, blank line, then the
/// body/list, with a dimmed hint pinned to the bottom row. The panel is a
/// raised surface rather than a box, so the overlay reads as a card.
fn chrome(
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
fn draw_hint(
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

/// A filterable list dialog.
pub struct SelectDialog {
    title: String,
    label: Option<String>,
    body: Option<String>,
    hint: String,
    list: SelectList,
    sender: UnboundedSender<DialogMessage>,
    mapper: fn(&str) -> DialogMessage,
    styles: DialogStyles,
}

impl SelectDialog {
    /// Build a list dialog; `mapper` converts the chosen value into a message.
    pub fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        items: Vec<SelectItem>,
        theme: &Theme,
        sender: UnboundedSender<DialogMessage>,
        mapper: fn(&str) -> DialogMessage,
    ) -> Self {
        let styles = DialogStyles::from_theme(theme);
        Self {
            title: title.into(),
            label: None,
            body: None,
            hint: hint.into(),
            list: SelectList::new(items)
                .max_visible(10)
                .theme(styles.list_theme()),
            sender,
            mapper,
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

    /// Height needed for the title block, optional body, label, and every item.
    pub fn height_for(items: usize) -> u16 {
        // panel padding 2 + title 1 + blank 1 + label 1 + blank 1 + items
        // + blank 1 + hint 1
        (items.min(10) as u16) + 8
    }

    fn confirm(&mut self) -> KeyResult {
        if let Some(item) = self.list.selected_item() {
            let value = item.value.clone();
            let _ = self.sender.send((self.mapper)(&value));
            KeyResult::Confirmed
        } else {
            let _ = self.sender.send(DialogMessage::Cancelled);
            KeyResult::Cancelled
        }
    }
}

impl Component for SelectDialog {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = chrome(buf, area, &self.title, &self.styles) else {
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

        if let Some(label) = &self.label {
            if content.height > 2 {
                buf.set_string(
                    content.x,
                    content.y,
                    truncate_to_width(label, content.width as usize, "…"),
                    self.styles.text,
                );
                content.y += 2;
                content.height -= 2;
            }
        }

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
        match self.list.handle_key(key) {
            KeyResult::Confirmed => self.confirm(),
            KeyResult::Cancelled => {
                let _ = self.sender.send(DialogMessage::Cancelled);
                KeyResult::Cancelled
            }
            other => other,
        }
    }

    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent) -> MouseResult {
        self.list.handle_mouse(event)
    }

    fn desired_height(&mut self, _width: u16) -> Option<u16> {
        Some(Self::height_for(self.list.len()))
    }
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

/// A yes/no confirmation dialog.
pub struct ConfirmDialog {
    title: String,
    body: String,
    action: ConfirmAction,
    accept: bool,
    sender: UnboundedSender<DialogMessage>,
    styles: DialogStyles,
}

impl ConfirmDialog {
    /// Build a confirmation dialog.
    pub fn new(
        title: impl Into<String>,
        body: impl Into<String>,
        action: ConfirmAction,
        theme: &Theme,
        sender: UnboundedSender<DialogMessage>,
    ) -> Self {
        Self {
            title: title.into(),
            body: body.into(),
            action,
            accept: false,
            sender,
            styles: DialogStyles::from_theme(theme),
        }
    }

    /// Height needed for the title, wrapped `body`, and the button row.
    pub fn height_for(body: &str, width: u16) -> u16 {
        let wrap = width.saturating_sub(6).max(8) as usize;
        let rows = wrap_text(body, wrap).len() as u16;
        // panel padding 2 + title 1 + blank 1 + body + blank 1 + buttons 1
        // + blank 1 + hint 1
        rows + 8
    }

    fn finish(&mut self, accepted: bool) -> KeyResult {
        let _ = self.sender.send(DialogMessage::Confirm {
            action: self.action,
            accepted,
        });
        if accepted {
            KeyResult::Confirmed
        } else {
            KeyResult::Cancelled
        }
    }
}

impl Component for ConfirmDialog {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = chrome(buf, area, &self.title, &self.styles) else {
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
            buf.set_string(content.x, buttons_y, yes, yes_style);
            buf.set_string(content.x + yes.len() as u16 + 2, buttons_y, no, no_style);
        }

        draw_hint(buf, layout.hint, HINT_CONFIRM, &self.styles, None);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
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

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        Some(Self::height_for(&self.body, width))
    }
}

// ---------------------------------------------------------------------------
// Text dialog (help, stats, …)
// ---------------------------------------------------------------------------

/// A scrollable read-only text dialog.
pub struct TextDialog {
    title: String,
    hint: String,
    lines: Vec<Line<'static>>,
    scroll: u16,
    max_scroll: u16,
    last_content: Rect,
    sender: UnboundedSender<DialogMessage>,
    styles: DialogStyles,
}

impl TextDialog {
    /// Build a text dialog from pre-rendered lines.
    pub fn new(
        title: impl Into<String>,
        hint: impl Into<String>,
        lines: Vec<Line<'static>>,
        theme: &Theme,
        sender: UnboundedSender<DialogMessage>,
    ) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            lines,
            scroll: 0,
            max_scroll: 0,
            last_content: Rect::default(),
            sender,
            styles: DialogStyles::from_theme(theme),
        }
    }

    fn close(&mut self) -> KeyResult {
        let _ = self.sender.send(DialogMessage::Cancelled);
        KeyResult::Cancelled
    }

    fn scroll_by(&mut self, delta: i32) {
        let target = (self.scroll as i32 + delta).clamp(0, self.max_scroll as i32);
        self.scroll = target as u16;
    }
}

impl Component for TextDialog {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let Some(layout) = chrome(buf, area, &self.title, &self.styles) else {
            return;
        };
        let content = layout.content;
        self.last_content = content;

        self.max_scroll = (self.lines.len() as u16).saturating_sub(content.height);
        self.scroll = self.scroll.min(self.max_scroll);

        for (row, line) in self
            .lines
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

    fn handle_mouse(&mut self, event: crossterm::event::MouseEvent) -> MouseResult {
        if !rect_contains(self.last_content, event.column, event.row) {
            return MouseResult::Ignored;
        }
        match event.kind {
            crossterm::event::MouseEventKind::ScrollUp => {
                self.scroll_by(-3);
                MouseResult::Handled
            }
            crossterm::event::MouseEventKind::ScrollDown => {
                self.scroll_by(3);
                MouseResult::Handled
            }
            _ => MouseResult::Ignored,
        }
    }
}

// ---------------------------------------------------------------------------
// Content builders
// ---------------------------------------------------------------------------

/// Keyboard shortcuts shown in the help dialog.
pub const KEY_HELP: &[(&str, &str)] = &[
    ("enter", "send the prompt — or run the command under `/`"),
    ("alt+enter", "insert a newline"),
    (
        "tab",
        "toggle build / plan mode — or fill in the `/` command",
    ),
    ("ctrl+k", "command palette"),
    ("ctrl+t", "cycle the theme"),
    ("ctrl+o", "toggle thinking blocks"),
    ("ctrl+l", "clear the transcript"),
    ("page up / down", "scroll the transcript"),
    ("?", "this help (when the prompt is empty)"),
    ("ctrl+c", "quit"),
];

/// Build the help dialog contents.
pub fn help_lines(theme: &Theme, commands: &[solaris_core::SlashCommandSpec]) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let key_style = Style::default().fg(theme.accent);
    let text_style = Style::default().fg(theme.fg);
    let muted = Style::default().fg(theme.muted);

    let mut lines = vec![
        Line::from(Span::styled("Keyboard", heading)),
        Line::default(),
    ];
    let width = KEY_HELP
        .iter()
        .map(|(key, _)| display_width(key))
        .max()
        .unwrap_or(0);
    for (key, description) in KEY_HELP {
        let padding = " ".repeat(width - display_width(key) + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("  {key}"), key_style),
            Span::styled(padding, text_style),
            Span::styled(*description, text_style),
        ]));
    }

    lines.push(Line::default());
    lines.push(Line::from(Span::styled("Commands", heading)));
    lines.push(Line::default());
    let name_width = commands
        .iter()
        .map(|spec| display_width(spec.name) + spec.args.len() + 2)
        .max()
        .unwrap_or(0);
    for spec in commands {
        let invocation = if spec.args.is_empty() {
            format!("/{}", spec.name)
        } else {
            format!("/{} {}", spec.name, spec.args)
        };
        let padding = " ".repeat(name_width.saturating_sub(display_width(&invocation)) + 2);
        lines.push(Line::from(vec![
            Span::styled(format!("  {invocation}"), key_style),
            Span::styled(padding, muted),
            Span::styled(spec.description, text_style),
        ]));
    }

    lines
}

/// Build the statistics dialog contents.
pub fn stats_lines(
    theme: &Theme,
    session: &crate::state::SessionState,
    model: &str,
    mode: solaris_core::Mode,
    context_window: u64,
) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme.muted);
    let value = Style::default().fg(theme.fg);

    let tokens = session.total_tokens();
    let used_ratio = if context_window == 0 {
        0.0
    } else {
        tokens as f64 / context_window as f64
    };

    let row = |name: &str, value_text: String| {
        Line::from(vec![
            Span::styled(format!("  {name:<16}"), label),
            Span::styled(value_text, value),
        ])
    };

    vec![
        Line::from(Span::styled("Session", heading)),
        Line::default(),
        row("model", model.to_string()),
        row("mode", mode.label().to_string()),
        row("turns", session.turns.len().to_string()),
        row("tokens", tokens.to_string()),
        row("cost", format!("${:.4}", session.total_cost())),
        row("context window", context_window.to_string()),
        row("context used", format!("{:.1}%", used_ratio * 100.0)),
        Line::default(),
        Line::from(Span::styled(
            "  Token and cost figures come from the active backend.",
            label,
        )),
    ]
}

/// Render markdown into dialog lines (used by future text dialogs).
pub fn markdown_lines(text: &str, width: u16, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown(text, width, &MarkdownStyle::from_theme(theme))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyModifiers;
    use tokio::sync::mpsc::unbounded_channel;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn items() -> Vec<SelectItem> {
        vec![
            SelectItem::new("dark", "dark").description("dark background"),
            SelectItem::new("light", "light").description("light background"),
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

    fn theme_dialog() -> (
        SelectDialog,
        tokio::sync::mpsc::UnboundedReceiver<DialogMessage>,
    ) {
        let (tx, rx) = unbounded_channel();
        let theme = Theme::dark();
        let dialog = SelectDialog::new(
            "Theme",
            "Enter to select · Esc to cancel",
            items(),
            &theme,
            tx,
            |value| DialogMessage::Theme(value.to_string()),
        )
        .label("Select a theme:");
        (dialog, rx)
    }

    #[test]
    fn select_dialog_renders_the_claude_picker_layout() {
        let (mut dialog, _rx) = theme_dialog();
        let height = SelectDialog::height_for(2);
        let area = Rect::new(0, 0, 48, height);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);
        let rows = rows(&buf);

        // Title, blank, label, blank, numbered options, blank, dim hint, padding.
        assert_eq!(rows[0], "");
        assert_eq!(rows[1], " Theme");
        assert_eq!(rows[2], "");
        assert_eq!(rows[3], " Select a theme:");
        assert_eq!(rows[4], "");
        assert_eq!(rows[5], " ❯ 1. dark · dark background");
        assert_eq!(rows[6], "   2. light · light background");
        assert_eq!(rows[7], "", "the hint needs a blank row above it");
        assert_eq!(
            rows[height as usize - 2],
            " Enter to select · Esc to cancel"
        );
        assert_eq!(rows[height as usize - 1], "", "panel bottom padding");
    }

    #[test]
    fn select_dialog_panel_has_no_border_glyphs() {
        let (mut dialog, _rx) = theme_dialog();
        let area = Rect::new(0, 0, 48, SelectDialog::height_for(2));
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);

        let text: String = rows(&buf).join("\n");
        for glyph in ['╭', '╮', '╰', '╯', '│', '─'] {
            assert!(!text.contains(glyph), "border glyph {glyph:?} in:\n{text}");
        }
        // The panel is a raised surface, so its cells carry a background.
        assert!(
            buf[(0, 0)].style().bg.is_some(),
            "panel has no surface fill"
        );
    }

    #[test]
    fn select_dialog_height_fits_the_whole_layout() {
        // panel 2 + title 1 + blank 1 + label 1 + blank 1 + items + blank 1 + hint 1
        assert_eq!(SelectDialog::height_for(2), 10);
        assert_eq!(SelectDialog::height_for(7), 15);
    }

    #[test]
    fn select_dialog_filters_and_shows_the_filter_indicator() {
        let (mut dialog, _rx) = theme_dialog();
        dialog.handle_key(key(KeyCode::Char('l')));

        let area = Rect::new(0, 0, 48, SelectDialog::height_for(2));
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);
        let text = rows(&buf).join("\n");

        assert!(text.contains("1. light · light background"), "{text}");
        assert!(text.contains("filter: l"), "{text}");
    }

    #[test]
    fn select_dialog_preselects_the_active_value() {
        let (tx, _rx) = unbounded_channel();
        let theme = Theme::dark();
        let dialog = SelectDialog::new("Theme", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        })
        .label("Select a theme:")
        .selected("light");

        assert_eq!(dialog.list.selected_item().unwrap().value, "light");
    }

    #[test]
    fn select_dialog_renders_an_optional_body_paragraph() {
        let (tx, _rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = SelectDialog::new("Login", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        })
        .body("Sign in with a subscription, or bill per API call.")
        .label("Select a method:");

        let area = Rect::new(0, 0, 52, 14);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);
        let text = rows(&buf).join("\n");

        assert!(text.contains("Login"), "{text}");
        assert!(
            text.contains("Sign in with a subscription, or bill per API call."),
            "{text}"
        );
        assert!(text.contains("Select a method:"), "{text}");
    }

    #[test]
    fn select_dialog_sends_the_mapped_value_on_enter() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = SelectDialog::new("Theme", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        });

        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        assert_eq!(rx.try_recv().unwrap(), DialogMessage::Theme("dark".into()));
    }

    #[test]
    fn select_dialog_reports_cancellation_on_escape() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = SelectDialog::new("Theme", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        });

        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), KeyResult::Cancelled);
        assert_eq!(rx.try_recv().unwrap(), DialogMessage::Cancelled);
    }

    #[test]
    fn select_dialog_filters_with_typed_characters() {
        let (tx, _rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = SelectDialog::new("Theme", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        });

        dialog.handle_key(key(KeyCode::Char('l')));
        let value = dialog.list.selected_item().unwrap().value.clone();
        assert_eq!(value, "light");
    }

    #[test]
    fn confirm_dialog_defaults_to_no() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = ConfirmDialog::new(
            "Clear",
            "Clear the transcript?",
            ConfirmAction::ClearTranscript,
            &theme,
            tx,
        );

        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), KeyResult::Cancelled);
        assert_eq!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                action: ConfirmAction::ClearTranscript,
                accepted: false
            }
        );
    }

    #[test]
    fn confirm_dialog_toggles_to_yes() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog =
            ConfirmDialog::new("Clear", "Sure?", ConfirmAction::ClearTranscript, &theme, tx);

        dialog.handle_key(key(KeyCode::Right));
        assert_eq!(dialog.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        assert_eq!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                action: ConfirmAction::ClearTranscript,
                accepted: true
            }
        );
    }

    #[test]
    fn confirm_dialog_accepts_y_and_n() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog =
            ConfirmDialog::new("Clear", "Sure?", ConfirmAction::ClearTranscript, &theme, tx);
        dialog.handle_key(key(KeyCode::Char('y')));
        assert!(matches!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm { accepted: true, .. }
        ));

        let (tx, mut rx) = unbounded_channel();
        let mut dialog =
            ConfirmDialog::new("Clear", "Sure?", ConfirmAction::ClearTranscript, &theme, tx);
        dialog.handle_key(key(KeyCode::Char('n')));
        assert!(matches!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                accepted: false,
                ..
            }
        ));
    }

    #[test]
    fn text_dialog_scrolls_and_reports_a_full_page() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let lines = (0..100)
            .map(|i| Line::from(Span::raw(format!("line {i}"))))
            .collect();
        let mut dialog = TextDialog::new("Help", "hint", lines, &theme, tx);

        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);

        // Paging must have room to move in both directions.
        assert_eq!(dialog.scroll, 0);
        assert!(dialog.max_scroll > 0);

        dialog.handle_key(key(KeyCode::PageDown));
        assert!(dialog.scroll > 0);
        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), KeyResult::Cancelled);
        assert_eq!(rx.try_recv().unwrap(), DialogMessage::Cancelled);
    }

    #[test]
    fn help_lines_mention_every_command() {
        let theme = Theme::dark();
        let lines = help_lines(&theme, solaris_core::PROMPT_SLASH_COMMANDS);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        for spec in solaris_core::PROMPT_SLASH_COMMANDS {
            assert!(
                text.contains(&format!("/{}", spec.name)),
                "missing {}",
                spec.name
            );
        }
        assert!(text.contains("ctrl+k"));
    }

    #[test]
    fn stats_lines_report_totals() {
        let theme = Theme::dark();
        let mut session = crate::state::SessionState::new();
        session.turns.push(crate::state::Turn {
            tokens: 120,
            cost_usd: 0.01,
            complete: true,
            ..Default::default()
        });

        let lines = stats_lines(&theme, &session, "solaris-mock-1", solaris_core::Mode::Build, 1000);
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter().map(|s| s.content.to_string()))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("solaris-mock-1"));
        assert!(text.contains("120"));
        assert!(text.contains("BUILD"));
        assert!(text.contains("12.0%"));
    }
}
