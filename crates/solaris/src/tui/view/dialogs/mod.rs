//! Product-specific modal dialogs wired to [`DialogMessage`].
//!
//! Generic dialog chrome and widgets live in [`solaris_tui::components::dialog`];
//! this module binds them to Solaris's message channel and re-exports the
//! product content builders from [`content`].

pub mod content;

pub use content::{
    KEY_HELP, buddy_lines, help_lines, skills_lines, skills_lines_for_width, stats_lines,
};
pub use solaris_tui::components::dialog::{
    ChromeLayout, DialogStyles, HINT_CONFIRM, HINT_SELECT, draw_chrome, draw_hint,
};

use crossterm::event::{KeyEvent, MouseEvent};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::dialog as base;
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::theme::Theme;
use tokio::sync::mpsc::UnboundedSender;

/// Result of an interaction with a dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DialogMessage {
    Question {
        session_id: String,
        request_id: String,
        response: solaris_agent::QuestionResponse,
    },
    /// Dismissed without a choice.
    Cancelled,
    /// A theme was chosen.
    Theme(String),
    /// A saved session was chosen.
    Session(String),
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
    /// A specific tool call the loop is waiting on, correlated by approval ID.
    RunTool(u64),
}

// ---------------------------------------------------------------------------
// Select dialog
// ---------------------------------------------------------------------------

/// A filterable list dialog reporting its choice over `DialogMessage`.
pub struct SelectDialog {
    inner: base::SelectDialog,
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
        Self {
            inner: base::SelectDialog::new(title, hint, items, theme, move |picked| {
                let message = match picked {
                    Some(value) => mapper(&value),
                    None => DialogMessage::Cancelled,
                };
                let _ = sender.send(message);
            }),
        }
    }

    /// Add the prompt line shown above the list, e.g. `Select login method:`.
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.inner = self.inner.label(label);
        self
    }

    /// Preselect the item carrying `value`.
    pub fn selected(mut self, value: &str) -> Self {
        self.inner = self.inner.selected(value);
        self
    }

    /// Add an explanatory paragraph under the title.
    pub fn body(mut self, body: impl Into<String>) -> Self {
        self.inner = self.inner.body(body);
        self
    }

    /// The currently highlighted item, if any.
    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.inner.selected_item()
    }

    /// Height needed for the title block, label, and up to 10 items.
    pub fn height_for(items: usize) -> u16 {
        base::SelectDialog::height_for(items)
    }
}

impl Component for SelectDialog {
    fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.inner.render(buf, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.inner.handle_key(key)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        self.inner.handle_mouse(event)
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        self.inner.desired_height(width)
    }
}

// ---------------------------------------------------------------------------
// Confirm dialog
// ---------------------------------------------------------------------------

/// A yes/no confirmation dialog reporting its decision over `DialogMessage`.
///
/// Dropping the dialog without an explicit answer sends `accepted: false`,
/// preventing tool approval callers from blocking indefinitely.
pub struct ConfirmDialog {
    inner: base::ConfirmDialog,
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
            inner: base::ConfirmDialog::new(title, body, theme, move |accepted| {
                let _ = sender.send(DialogMessage::Confirm { action, accepted });
            }),
        }
    }

    /// Height needed for the title, wrapped `body`, and the button row.
    pub fn height_for(body: &str, width: u16) -> u16 {
        base::ConfirmDialog::height_for(body, width)
    }
}

impl Component for ConfirmDialog {
    fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.inner.render(buf, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.inner.handle_key(key)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        self.inner.handle_mouse(event)
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        self.inner.desired_height(width)
    }
}

// ---------------------------------------------------------------------------
// Text dialog
// ---------------------------------------------------------------------------

/// A scrollable read-only text dialog.
pub struct TextDialog {
    inner: base::TextDialog,
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
            inner: base::TextDialog::new(title, hint, lines, theme, move || {
                let _ = sender.send(DialogMessage::Cancelled);
            }),
        }
    }

    /// Build a text dialog whose content reflows dynamically when resized.
    pub fn reflowable(
        title: impl Into<String>,
        hint: impl Into<String>,
        theme: &Theme,
        build: impl Fn(u16) -> Vec<Line<'static>> + 'static,
        sender: UnboundedSender<DialogMessage>,
    ) -> Self {
        Self {
            inner: base::TextDialog::reflowable(title, hint, theme, build, move || {
                let _ = sender.send(DialogMessage::Cancelled);
            }),
        }
    }
}

impl Component for TextDialog {
    fn is_finished(&self) -> bool {
        self.inner.is_finished()
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.inner.render(buf, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.inner.handle_key(key)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        self.inner.handle_mouse(event)
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        self.inner.desired_height(width)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyModifiers, MouseButton, MouseEventKind};
    use ratatui::text::Span;
    use tokio::sync::mpsc::unbounded_channel;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        }
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

        assert_eq!(dialog.selected_item().unwrap().value, "light");
    }

    #[test]
    fn select_dialog_renders_an_optional_body_paragraph() {
        let (tx, _rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = SelectDialog::new("Login", "hint", items(), &theme, tx, |value| {
            DialogMessage::Theme(value.to_string())
        })
        .body("Sign in with an API key, billed per API call.")
        .label("Select a method:");

        let area = Rect::new(0, 0, 52, 14);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);
        let text = rows(&buf).join("\n");

        assert!(text.contains("Login"), "{text}");
        assert!(
            text.contains("Sign in with an API key, billed per API call."),
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
        let value = dialog.selected_item().unwrap().value.clone();
        assert_eq!(value, "light");
    }

    #[test]
    fn select_dialog_mouse_selects_then_confirms_highlighted_row() {
        let (mut dialog, mut rx) = theme_dialog();
        let area = Rect::new(0, 0, 48, SelectDialog::height_for(2));
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);

        // First click on row 6 ("light") moves selection without confirming.
        dialog.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 6));
        dialog.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 6));
        assert_eq!(dialog.selected_item().unwrap().value, "light");
        assert!(!dialog.is_finished());

        // Clicking the already-selected row confirms it.
        dialog.handle_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 4, 6));
        dialog.handle_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 6));
        assert!(dialog.is_finished());
        assert_eq!(rx.try_recv().unwrap(), DialogMessage::Theme("light".into()));
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
    fn confirm_dialog_sends_denial_on_drop_without_double_sending() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let dialog = ConfirmDialog::new("Run?", "cmd", ConfirmAction::RunTool(7), &theme, tx);
        drop(dialog);
        assert_eq!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                action: ConfirmAction::RunTool(7),
                accepted: false,
            }
        );
        assert!(rx.try_recv().is_err());

        let (tx, mut rx) = unbounded_channel();
        let mut dialog = ConfirmDialog::new("Run?", "cmd", ConfirmAction::RunTool(8), &theme, tx);
        dialog.handle_key(key(KeyCode::Char('y')));
        drop(dialog);
        assert_eq!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                action: ConfirmAction::RunTool(8),
                accepted: true,
            }
        );
        assert!(
            rx.try_recv().is_err(),
            "dropping an already-answered dialog must not send a second reply"
        );
    }

    #[test]
    fn confirm_dialog_supports_mouse_clicks_on_yes_and_no() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let mut dialog = ConfirmDialog::new("Run?", "cmd", ConfirmAction::RunTool(1), &theme, tx);
        let height = dialog.desired_height(40).unwrap();
        let area = Rect::new(0, 0, 40, height);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);
        let rendered = rows(&buf);
        let button_row = rendered
            .iter()
            .position(|row| row.contains("Yes") && row.contains("No"))
            .unwrap() as u16;
        let yes_col = rendered[button_row as usize].find("Yes").unwrap() as u16;

        dialog.handle_mouse(mouse(
            MouseEventKind::Down(MouseButton::Left),
            yes_col,
            button_row,
        ));
        dialog.handle_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            yes_col,
            button_row,
        ));
        assert!(dialog.is_finished());
        assert_eq!(
            rx.try_recv().unwrap(),
            DialogMessage::Confirm {
                action: ConfirmAction::RunTool(1),
                accepted: true,
            }
        );
    }

    #[test]
    fn text_dialog_scrolls_and_reflows_with_width() {
        let (tx, mut rx) = unbounded_channel();
        let theme = Theme::dark();
        let lines = (0..100)
            .map(|i| Line::from(Span::raw(format!("line {i}"))))
            .collect();
        let mut dialog = TextDialog::new("Help", "hint", lines, &theme, tx);

        let area = Rect::new(0, 0, 40, 10);
        let mut buf = Buffer::empty(area);
        dialog.render(&mut buf, area);

        assert_eq!(dialog.inner.scroll(), 0);
        assert!(dialog.inner.max_scroll() > 0);

        dialog.handle_key(key(KeyCode::PageDown));
        assert!(dialog.inner.scroll() > 0);
        assert_eq!(dialog.handle_key(key(KeyCode::Esc)), KeyResult::Cancelled);
        assert_eq!(rx.try_recv().unwrap(), DialogMessage::Cancelled);

        // Reflowable text dialogs recalculate desired height at each width.
        let (tx, _rx) = unbounded_channel();
        let mut reflow = TextDialog::reflowable(
            "Notes",
            "hint",
            &theme,
            |width| {
                solaris_tui::util::wrap_text(&"word ".repeat(30), width as usize)
                    .into_iter()
                    .map(Line::from)
                    .collect()
            },
            tx,
        );
        let wide = reflow.desired_height(80).unwrap();
        let narrow = reflow.desired_height(24).unwrap();
        assert!(narrow > wide, "narrow={narrow} wide={wide}");
    }
}
