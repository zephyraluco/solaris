//! Inline question panel that takes over the prompt region.
//!
//! Runtime owns request validity and settlement. The panel displays question
//! tabs (plus a review/submit tab for multi-question prompts), bold prompt
//! copy, numbered choices with muted descriptions, and an inline custom-answer
//! editor without opening a modal overlay.

use std::ops::Range;
use std::sync::Arc;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Widget};
use solaris_agent::{
    QuestionAnswer, QuestionRequest, QuestionResponse, RuntimeCommand, SessionRuntime,
};
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::editor::Editor;
use solaris_tui::theme::Theme;
use solaris_tui::util::{digit_count, display_width, rect_contains, truncate_to_width, wrap_text};
use tokio::sync::mpsc::UnboundedSender;

use super::dialogs::DialogMessage;

#[derive(Debug, Clone, Copy)]
struct PanelStyles {
    border: Style,
    tab_active: Style,
    tab_inactive: Style,
    tab_done: Style,
    arrow: Style,
    question: Style,
    option_active: Style,
    option_inactive: Style,
    description: Style,
    placeholder: Style,
    hint: Style,
    warning: Style,
}

impl PanelStyles {
    fn from_theme(theme: &Theme) -> Self {
        Self {
            border: Style::default().fg(theme.border),
            tab_active: Style::default()
                .fg(theme.bg)
                .bg(theme.accent)
                .add_modifier(Modifier::BOLD),
            tab_inactive: Style::default().fg(theme.fg),
            tab_done: Style::default().fg(theme.success),
            arrow: Style::default().fg(theme.dim),
            question: Style::default().fg(theme.fg).add_modifier(Modifier::BOLD),
            option_active: Style::default()
                .fg(theme.accent)
                .add_modifier(Modifier::BOLD),
            option_inactive: Style::default().fg(theme.fg),
            description: Style::default().fg(theme.muted),
            placeholder: Style::default().fg(theme.muted),
            hint: Style::default().fg(theme.dim),
            warning: Style::default().fg(theme.warning),
        }
    }
}

struct BodyLayout {
    lines: Vec<Line<'static>>,
    focus: usize,
    targets: Vec<(usize, Range<usize>)>,
}

/// Interactive question panel rendered in place of the prompt editor.
pub struct QuestionPanel {
    request: QuestionRequest,
    session: Arc<SessionRuntime>,
    /// Active tab: `0..questions.len()` for a question, or `questions.len()`
    /// for the review/submit tab when there are multiple questions.
    current: usize,
    cursor: usize,
    answers: Vec<QuestionAnswer>,
    editing: bool,
    submitted: bool,
    editor: Editor,
    styles: PanelStyles,
    sender: UnboundedSender<DialogMessage>,
    last_area: Rect,
    tab_area: Rect,
    tab_hits: Vec<(usize, Range<u16>)>,
    content_area: Rect,
    scroll: u16,
    max_scroll: u16,
    manual_scroll: bool,
    row_targets: Vec<(usize, Range<usize>)>,
    pressed_target: Option<usize>,
    pressed_tab: Option<usize>,
}

impl QuestionPanel {
    pub fn new(
        request: QuestionRequest,
        session: Arc<SessionRuntime>,
        theme: &Theme,
        sender: UnboundedSender<DialogMessage>,
    ) -> Self {
        let answers = request
            .questions
            .iter()
            .map(|q| QuestionAnswer {
                question_id: q.id.clone(),
                selected: Vec::new(),
                custom: None,
            })
            .collect();
        let mut editor = Editor::with_placeholder("Type your answer…");
        editor.set_styles(crate::tui::app::editor_styles(theme));
        editor.set_focused(true);
        Self {
            request,
            session,
            current: 0,
            cursor: 0,
            answers,
            editing: false,
            submitted: false,
            editor,
            styles: PanelStyles::from_theme(theme),
            sender,
            last_area: Rect::default(),
            tab_area: Rect::default(),
            tab_hits: Vec::new(),
            content_area: Rect::default(),
            scroll: 0,
            max_scroll: 0,
            manual_scroll: false,
            row_targets: Vec::new(),
            pressed_target: None,
            pressed_tab: None,
        }
    }

    pub fn session_id(&self) -> &str {
        &self.request.session_id
    }

    pub fn request_id(&self) -> &str {
        &self.request.request_id
    }

    pub fn area(&self) -> Rect {
        self.last_area
    }

    pub fn set_theme(&mut self, theme: &Theme) {
        self.styles = PanelStyles::from_theme(theme);
        self.editor
            .set_styles(crate::tui::app::editor_styles(theme));
    }

    fn has_submit_tab(&self) -> bool {
        self.request.questions.len() > 1
    }

    fn tab_count(&self) -> usize {
        self.request.questions.len() + usize::from(self.has_submit_tab())
    }

    fn on_submit_tab(&self) -> bool {
        self.has_submit_tab() && self.current == self.request.questions.len()
    }

    fn is_answered(&self, index: usize) -> bool {
        self.answers
            .get(index)
            .is_some_and(|answer| !answer.selected.is_empty() || answer.custom.is_some())
    }

    fn all_answered(&self) -> bool {
        (0..self.request.questions.len()).all(|index| self.is_answered(index))
    }

    fn first_unanswered(&self) -> Option<usize> {
        (0..self.request.questions.len()).find(|&index| !self.is_answered(index))
    }

    fn switch_tab(&mut self, tab: usize) {
        let clamped = tab.min(self.tab_count().saturating_sub(1));
        if clamped != self.current {
            self.current = clamped;
            self.cursor = 0;
            self.scroll = 0;
            self.manual_scroll = false;
            self.editing = false;
            self.editor.clear();
        }
    }

    fn finish(&mut self, response: QuestionResponse) -> KeyResult {
        self.submitted = true;
        let _ = self.sender.send(DialogMessage::Question {
            session_id: self.request.session_id.clone(),
            request_id: self.request.request_id.clone(),
            response,
        });
        KeyResult::Confirmed
    }

    fn submit_if_ready(&mut self) -> KeyResult {
        if let Some(missing) = self.first_unanswered() {
            self.switch_tab(missing);
            return KeyResult::Handled;
        }
        self.finish(QuestionResponse::Answered {
            answers: self.answers.clone(),
        })
    }

    fn advance(&mut self) -> KeyResult {
        if !self.is_answered(self.current) {
            return KeyResult::Handled;
        }
        if self.has_submit_tab() {
            self.switch_tab(self.current + 1);
            KeyResult::Handled
        } else {
            self.submit_if_ready()
        }
    }

    fn item_count(&self) -> usize {
        if self.on_submit_tab() {
            return 2;
        }
        let q = &self.request.questions[self.current];
        let needs_continue = q.multiple;
        q.options.len() + usize::from(q.allow_custom) + usize::from(needs_continue)
    }

    fn build_body(&self, width: usize) -> BodyLayout {
        if self.on_submit_tab() {
            return self.build_submit_body(width);
        }
        let q = &self.request.questions[self.current];
        let mut lines = Vec::new();
        let mut targets = Vec::new();

        for line in wrap_text(&q.question, width) {
            lines.push(Line::from(Span::styled(line, self.styles.question)));
        }
        lines.push(Line::default());

        let total_choices = q.options.len() + usize::from(q.allow_custom);
        let number_width = digit_count(total_choices.max(1));
        // "❯ " (2) + number + ". " (2) + optional "[x] " (4)
        let label_indent = 2 + number_width + 2 + if q.multiple { 4 } else { 0 };
        let desc_indent = " ".repeat(label_indent);
        let desc_width = width.saturating_sub(label_indent).max(8);

        let mut focus = lines.len();
        for (i, option) in q.options.iter().enumerate() {
            let start = lines.len();
            let focused = i == self.cursor;
            if focused {
                focus = start;
            }
            let selected = self.answers[self.current].selected.contains(&option.id);
            let marker = if focused { "❯ " } else { "  " };
            let number = format!("{:>w$}. ", i + 1, w = number_width);
            let check = if q.multiple {
                if selected { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            let suffix = if !q.multiple && selected { " ✓" } else { "" };
            let head = format!("{marker}{number}{check}{}{suffix}", option.label);
            let row_style = if focused {
                self.styles.option_active
            } else {
                self.styles.option_inactive
            };
            for (seg_idx, seg) in wrap_text(&head, width).into_iter().enumerate() {
                if seg_idx == 0 {
                    lines.push(Line::from(Span::styled(seg, row_style)));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{desc_indent}{seg}"),
                        row_style,
                    )));
                }
            }
            if !option.description.is_empty() {
                for seg in wrap_text(&option.description, desc_width) {
                    lines.push(Line::from(Span::styled(
                        format!("{desc_indent}{seg}"),
                        self.styles.description,
                    )));
                }
            }
            targets.push((i, start..lines.len()));
        }

        if q.allow_custom {
            let target_index = q.options.len();
            let start = lines.len();
            let focused = self.cursor == target_index;
            if focused {
                focus = start;
            }
            let custom = self.answers[self.current].custom.as_deref();
            let marker = if focused { "❯ " } else { "  " };
            let number = format!("{:>w$}. ", target_index + 1, w = number_width);
            let check = if q.multiple {
                if custom.is_some() { "[x] " } else { "[ ] " }
            } else {
                ""
            };
            let label = custom.unwrap_or("Type something.");
            let suffix = if !q.multiple && custom.is_some() {
                " ✓"
            } else {
                ""
            };
            let row_style = if focused {
                self.styles.option_active
            } else if custom.is_some() {
                self.styles.option_inactive
            } else {
                self.styles.placeholder
            };
            let head = format!("{marker}{number}{check}{label}{suffix}");
            for (seg_idx, seg) in wrap_text(&head, width).into_iter().enumerate() {
                if seg_idx == 0 {
                    lines.push(Line::from(Span::styled(seg, row_style)));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("{desc_indent}{seg}"),
                        row_style,
                    )));
                }
            }
            targets.push((target_index, start..lines.len()));
        }

        if q.multiple {
            let target_index = q.options.len() + usize::from(q.allow_custom);
            let start = lines.len();
            let focused = self.cursor == target_index;
            if focused {
                focus = start;
            }
            let marker = if focused { "❯ " } else { "  " };
            let label = if self.has_submit_tab() {
                "Continue"
            } else {
                "Submit"
            };
            let style = if focused {
                self.styles.option_active
            } else {
                self.styles.option_inactive
            };
            lines.push(Line::from(Span::styled(format!("{marker}{label}"), style)));
            targets.push((target_index, start..lines.len()));
        }

        BodyLayout {
            lines,
            focus,
            targets,
        }
    }

    fn build_submit_body(&self, width: usize) -> BodyLayout {
        let mut lines = Vec::new();
        let mut targets = Vec::new();

        lines.push(Line::from(Span::styled(
            "Review your answers",
            self.styles.question,
        )));
        lines.push(Line::default());

        for (index, question) in self.request.questions.iter().enumerate() {
            let answer = &self.answers[index];
            let mut summary: Vec<String> = answer
                .selected
                .iter()
                .filter_map(|id| {
                    question
                        .options
                        .iter()
                        .find(|opt| &opt.id == id)
                        .map(|opt| opt.label.clone())
                })
                .collect();
            if let Some(custom) = &answer.custom {
                summary.push(custom.clone());
            }
            let answered = !summary.is_empty();
            let value_text = if answered {
                summary.join(", ")
            } else {
                "(unanswered)".to_string()
            };
            let prefix = format!("  {}: ", question.header);
            let value_style = if answered {
                self.styles.option_inactive
            } else {
                self.styles.warning
            };
            let prefix_str = truncate_to_width(&prefix, width, "…");
            let remaining = width.saturating_sub(display_width(&prefix_str));
            let value_str = truncate_to_width(&value_text, remaining, "…");
            lines.push(Line::from(vec![
                Span::styled(prefix_str, self.styles.description),
                Span::styled(value_str, value_style),
            ]));
        }

        lines.push(Line::default());
        let mut focus = lines.len();

        let actions = [
            if self.all_answered() {
                "Submit answers"
            } else {
                "Jump to unanswered question"
            },
            "Cancel",
        ];
        for (i, label) in actions.iter().enumerate() {
            let start = lines.len();
            let focused = self.cursor == i;
            if focused {
                focus = start;
            }
            let marker = if focused { "❯ " } else { "  " };
            let style = if focused {
                self.styles.option_active
            } else {
                self.styles.option_inactive
            };
            lines.push(Line::from(Span::styled(
                format!("{marker}{}. {label}", i + 1),
                style,
            )));
            targets.push((i, start..lines.len()));
        }

        BodyLayout {
            lines,
            focus,
            targets,
        }
    }

    fn render_tabs(&mut self, buf: &mut Buffer, area: Rect) {
        self.tab_area = area;
        self.tab_hits.clear();
        if area.width == 0 || area.height == 0 {
            return;
        }

        // Build tab labels: each question header plus a trailing Submit tab when >1 question.
        let mut tabs: Vec<(usize, String, bool)> = self
            .request
            .questions
            .iter()
            .enumerate()
            .map(|(index, q)| {
                let done = self.is_answered(index);
                let glyph = if done { "☑" } else { "□" };
                (index, format!(" {glyph} {} ", q.header), done)
            })
            .collect();
        if self.has_submit_tab() {
            tabs.push((self.request.questions.len(), " ✓ Submit ".to_string(), true));
        }

        let show_arrows = tabs.len() > 1;
        let left_arrow = if show_arrows { "← " } else { "" };
        let right_arrow = if show_arrows { " →" } else { "" };
        let arrow_left_w = display_width(left_arrow) as u16;
        let arrow_right_w = display_width(right_arrow) as u16;
        let available = area
            .width
            .saturating_sub(arrow_left_w.saturating_add(arrow_right_w));

        // Choose a contiguous slice of tabs containing `self.current` that fits `available`.
        let widths: Vec<u16> = tabs
            .iter()
            .map(|(_, label, _)| display_width(label) as u16)
            .collect();
        let mut start = 0usize;
        let mut end = tabs.len();
        let total: u16 = widths.iter().sum::<u16>() + tabs.len().saturating_sub(1) as u16;
        if total > available && available > 0 {
            start = self.current;
            end = self.current + 1;
            let mut used = widths[self.current];
            loop {
                let mut grew = false;
                if end < tabs.len() {
                    let next_w = widths[end] + 1;
                    if used + next_w <= available {
                        used += next_w;
                        end += 1;
                        grew = true;
                    }
                }
                if start > 0 {
                    let prev_w = widths[start - 1] + 1;
                    if used + prev_w <= available {
                        used += prev_w;
                        start -= 1;
                        grew = true;
                    }
                }
                if !grew {
                    break;
                }
            }
        }

        let mut x = area.x;
        if show_arrows && x < area.right() {
            let style = if self.current > 0 {
                self.styles.option_inactive
            } else {
                self.styles.arrow
            };
            buf.set_string(x, area.y, left_arrow, style);
            x = x.saturating_add(arrow_left_w);
        }

        for (idx, label, done) in &tabs[start..end] {
            if x >= area.right().saturating_sub(arrow_right_w) {
                break;
            }
            let budget = area.right().saturating_sub(arrow_right_w).saturating_sub(x) as usize;
            let rendered = truncate_to_width(label, budget, "…");
            let w = display_width(&rendered) as u16;
            let style = if *idx == self.current {
                self.styles.tab_active
            } else if *done && *idx < self.request.questions.len() {
                self.styles.tab_done
            } else {
                self.styles.tab_inactive
            };
            buf.set_string(x, area.y, &rendered, style);
            self.tab_hits.push((*idx, x..x.saturating_add(w)));
            x = x.saturating_add(w).saturating_add(1);
        }

        if show_arrows && x <= area.right().saturating_sub(arrow_right_w) {
            let style = if self.current + 1 < tabs.len() {
                self.styles.option_inactive
            } else {
                self.styles.arrow
            };
            buf.set_string(x.saturating_sub(1), area.y, right_arrow, style);
        }
    }

    fn activate_cursor(&mut self, toggle: bool) -> KeyResult {
        if self.on_submit_tab() {
            return match self.cursor {
                0 => self.submit_if_ready(),
                _ => self.finish(QuestionResponse::Dismissed),
            };
        }

        let q = &self.request.questions[self.current];
        if self.cursor == q.options.len() && q.allow_custom {
            if q.multiple && toggle && self.answers[self.current].custom.is_some() {
                self.answers[self.current].custom = None;
                return KeyResult::Handled;
            }
            self.editor
                .set_text(self.answers[self.current].custom.as_deref().unwrap_or(""));
            self.editing = true;
        } else if q.multiple && self.cursor == q.options.len() + usize::from(q.allow_custom) {
            return self.advance();
        } else if let Some(option) = q.options.get(self.cursor) {
            let answer = &mut self.answers[self.current];
            if !q.multiple {
                answer.selected = vec![option.id.clone()];
                answer.custom = None;
                return self.advance();
            }
            if toggle {
                if answer.selected.contains(&option.id) {
                    answer.selected.retain(|id| id != &option.id);
                } else {
                    answer.selected.push(option.id.clone());
                }
            } else {
                return self.advance();
            }
        }
        KeyResult::Handled
    }

    fn hit_target(&self, column: u16, row: u16) -> Option<usize> {
        if !rect_contains(self.content_area, column, row) {
            return None;
        }
        let line = usize::from(self.scroll) + usize::from(row.saturating_sub(self.content_area.y));
        self.row_targets
            .iter()
            .find(|(_, range)| range.contains(&line))
            .map(|(target, _)| *target)
    }

    fn hit_tab(&self, column: u16, row: u16) -> Option<usize> {
        if !rect_contains(self.tab_area, column, row) {
            return None;
        }
        self.tab_hits
            .iter()
            .find(|(_, cols)| cols.contains(&column))
            .map(|(tab, _)| *tab)
    }
}

impl Component for QuestionPanel {
    fn is_finished(&self) -> bool {
        self.submitted || !self.session.is_question_pending(&self.request.request_id)
    }

    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_area = area;
        self.content_area = Rect::default();
        self.tab_area = Rect::default();
        self.tab_hits.clear();
        self.row_targets.clear();
        if area.width < 4 || area.height < 3 {
            return;
        }

        // Row 0: top horizontal divider above the inline question block.
        buf.set_string(
            area.x,
            area.y,
            "─".repeat(area.width as usize),
            self.styles.border,
        );

        // Row 1: header / tab bar.
        let tab_rect = Rect::new(area.x, area.y + 1, area.width, 1);
        self.render_tabs(buf, tab_rect);

        // Last row: dim hint line.
        let hint_y = area.y + area.height - 1;
        let hint = if self.editing {
            "Enter save · Esc back"
        } else if self.on_submit_tab() {
            "Enter submit · ←/→ switch tab · Esc cancel"
        } else if self.request.questions[self.current].multiple {
            if self.has_submit_tab() {
                "↑↓ move · Space toggle · Enter next · ←/→ tabs · Esc cancel"
            } else {
                "↑↓ move · Space toggle · Enter submit · Esc cancel"
            }
        } else if self.has_submit_tab() {
            "↑↓ move · Enter select · ←/→ tabs · Esc cancel"
        } else {
            "↑↓ move · Enter select · Esc cancel"
        };
        buf.set_string(
            area.x,
            hint_y,
            truncate_to_width(hint, area.width as usize, "…"),
            self.styles.hint,
        );

        // Body region between the blank row after tabs (y + 3) and the hint row.
        let body_top = area.y.saturating_add(3);
        let body_bottom = hint_y.saturating_sub(1);
        if body_top >= body_bottom {
            return;
        }
        let body_area = Rect::new(area.x, body_top, area.width, body_bottom - body_top);
        let BodyLayout {
            lines,
            focus,
            targets,
        } = self.build_body(body_area.width.max(1) as usize);
        self.row_targets = targets;

        let editor_height = if self.editing {
            body_area.height.saturating_sub(1).min(4)
        } else {
            0
        };
        let list_height = body_area.height.saturating_sub(editor_height);
        self.content_area = Rect {
            height: list_height,
            ..body_area
        };
        self.max_scroll = (lines.len() as u16).saturating_sub(list_height);
        if self.manual_scroll {
            self.scroll = self.scroll.min(self.max_scroll);
        } else {
            self.scroll = focus
                .saturating_sub(list_height.saturating_sub(1) as usize)
                .min(self.max_scroll as usize) as u16;
        }
        if list_height > 0 {
            Paragraph::new(lines)
                .scroll((self.scroll, 0))
                .render(self.content_area, buf);
        }
        if self.editing && editor_height > 0 {
            self.editor.render(
                buf,
                Rect {
                    y: body_area.y + list_height,
                    height: editor_height,
                    ..body_area
                },
            );
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.pressed_target = None;
        self.pressed_tab = None;
        if self.is_finished() {
            return KeyResult::Cancelled;
        }
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            let _ = self.session.dispatch(RuntimeCommand::Cancel);
            return KeyResult::Cancelled;
        }
        if self.editing {
            if key.code == KeyCode::Esc {
                self.editing = false;
                self.editor.clear();
                return KeyResult::Handled;
            }
            self.editor.handle_key(key);
            if let Some(text) = self.editor.take_submitted()
                && !text.trim().is_empty()
            {
                self.answers[self.current].custom = Some(text.trim().into());
                self.editing = false;
                if !self.request.questions[self.current].multiple {
                    self.answers[self.current].selected.clear();
                    return self.advance();
                }
            }
            return KeyResult::Handled;
        }

        let count = self.item_count();
        match key.code {
            KeyCode::Esc => return self.finish(QuestionResponse::Dismissed),
            KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                self.manual_scroll = false;
            }
            KeyCode::Down => {
                self.cursor = (self.cursor + 1).min(count.saturating_sub(1));
                self.manual_scroll = false;
            }
            KeyCode::Left | KeyCode::BackTab if self.current > 0 => {
                self.switch_tab(self.current - 1);
            }
            KeyCode::Right | KeyCode::Tab if self.current + 1 < self.tab_count() => {
                self.switch_tab(self.current + 1);
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                return self.activate_cursor(key.code == KeyCode::Char(' '));
            }
            _ => {}
        }
        KeyResult::Handled
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        if self.is_finished() {
            return MouseResult::Ignored;
        }
        if self.editing {
            return self.editor.handle_mouse(event);
        }
        match event.kind {
            MouseEventKind::ScrollUp
                if rect_contains(self.content_area, event.column, event.row) =>
            {
                self.scroll = self.scroll.saturating_sub(3);
                self.manual_scroll = true;
                MouseResult::Handled
            }
            MouseEventKind::ScrollDown
                if rect_contains(self.content_area, event.column, event.row) =>
            {
                self.scroll = self.scroll.saturating_add(3).min(self.max_scroll);
                self.manual_scroll = true;
                MouseResult::Handled
            }
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(tab) = self.hit_tab(event.column, event.row) {
                    self.pressed_tab = Some(tab);
                    self.pressed_target = None;
                    return MouseResult::Handled;
                }
                if let Some(target) = self.hit_target(event.column, event.row) {
                    self.cursor = target;
                    self.pressed_target = Some(target);
                    self.pressed_tab = None;
                    self.manual_scroll = false;
                    return MouseResult::Handled;
                }
                self.pressed_target = None;
                self.pressed_tab = None;
                MouseResult::Ignored
            }
            MouseEventKind::Drag(_) => {
                self.pressed_target = None;
                self.pressed_tab = None;
                MouseResult::Ignored
            }
            MouseEventKind::Up(MouseButton::Left) => {
                if let Some(tab) = self.pressed_tab.take()
                    && self.hit_tab(event.column, event.row) == Some(tab)
                {
                    self.switch_tab(tab);
                    return MouseResult::Handled;
                }
                if let Some(target) = self.pressed_target.take()
                    && self.hit_target(event.column, event.row) == Some(target)
                {
                    self.cursor = target;
                    let toggle = if self.on_submit_tab() {
                        false
                    } else {
                        let q = &self.request.questions[self.current];
                        let is_continue =
                            q.multiple && target == q.options.len() + usize::from(q.allow_custom);
                        q.multiple && !is_continue
                    };
                    let _ = self.activate_cursor(toggle);
                    return MouseResult::Handled;
                }
                MouseResult::Ignored
            }
            _ => MouseResult::Ignored,
        }
    }

    fn cursor_position(&self) -> Option<(u16, u16)> {
        if self.editing {
            self.editor.cursor_position()
        } else {
            None
        }
    }

    fn handle_paste(&mut self, text: &str) -> KeyResult {
        if self.editing {
            self.editor.insert_paste(text);
        }
        KeyResult::Handled
    }

    fn desired_height(&mut self, width: u16) -> Option<u16> {
        let built = self.build_body(width.max(8) as usize);
        let editor_rows = if self.editing { 4u16 } else { 0 };
        // divider (1) + tabs (1) + blank (1) + body + editor + blank (1) + hint (1) = +5
        Some(
            (built.lines.len().max(1) as u16)
                .saturating_add(editor_rows)
                .saturating_add(5),
        )
    }
}

impl Drop for QuestionPanel {
    fn drop(&mut self) {
        if !self.submitted && !self.is_finished() {
            let _ = self.sender.send(DialogMessage::Question {
                session_id: self.request.session_id.clone(),
                request_id: self.request.request_id.clone(),
                response: QuestionResponse::Dismissed,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;
    use solaris_agent::{
        Agent, AgentEvent, AgentEventStream, AgentOptions, BackendError, ConnectionConfig, Mode,
        ModelClient, ModelConnector, Questions, ToolCall, ToolRegistry, Usage,
    };

    struct Model(Vec<solaris_agent::Question>);
    #[async_trait::async_trait]
    impl ModelConnector for Model {
        async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
            Ok(solaris_agent::test_support::client(
                Default::default(),
                move |_| {
                    let questions = self.0.clone();
                    async move {
                        Ok(Box::pin(futures::stream::iter([
                            AgentEvent::ToolCall(ToolCall::new(
                                "ask",
                                "question",
                                serde_json::json!({"questions": questions}),
                            )),
                            AgentEvent::TurnComplete {
                                usage: Usage::new(1, 1),
                                cost_usd: 0.0,
                            },
                        ])) as AgentEventStream)
                    }
                },
            ))
        }
    }

    fn question(multiple: bool) -> solaris_agent::Question {
        serde_json::from_value(serde_json::json!({
            "id": "q",
            "header": "Storage",
            "question": "选择存储方式？",
            "options": [
                {"id": "a", "label": "SQLite", "description": "本地"},
                {"id": "b", "label": "PostgreSQL", "description": "共享"}
            ],
            "multiple": multiple,
            "allow_custom": true
        }))
        .unwrap()
    }

    async fn panel(
        questions: Vec<solaris_agent::Question>,
    ) -> (
        QuestionPanel,
        tokio::sync::mpsc::UnboundedReceiver<DialogMessage>,
        solaris_agent::RuntimeEventStream,
    ) {
        let options = AgentOptions {
            questions: Some(Questions::default()),
            ..AgentOptions::default().with_subagents(false)
        };
        let session = Arc::new(SessionRuntime::new(Agent::new(
            ConnectionConfig {
                backend: Arc::new(Model(questions)),
                model: "fixture".into(),
                provider_id: None,
                fallback_context_window: 128_000,
                instructions: None,
            },
            Arc::new(ToolRegistry::with_tools(vec![])),
            options,
        )));
        let mut stream = session
            .execute(RuntimeCommand::Prompt {
                text: "ask".into(),
                mode: Mode::Default,
            })
            .await
            .unwrap();
        let request = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let AgentEvent::QuestionRequested(q) = stream.next().await.unwrap().event {
                    break q;
                }
            }
        })
        .await
        .unwrap();
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        (
            QuestionPanel::new(request, session, &Theme::dark(), tx),
            rx,
            stream,
        )
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn render_lines(panel: &mut QuestionPanel, width: u16, height: u16) -> (Buffer, Vec<String>) {
        let area = Rect::new(0, 0, width, height);
        let mut buf = Buffer::empty(area);
        panel.render(&mut buf, area);
        let lines = (0..area.height)
            .map(|y| {
                let mut row = String::new();
                let mut x = 0;
                while x < area.width {
                    let sym = buf[(x, y)].symbol();
                    row.push_str(sym);
                    x = x.saturating_add(display_width(sym).max(1) as u16);
                }
                row
            })
            .collect();
        (buf, lines)
    }

    #[tokio::test]
    async fn initial_focus_is_not_an_answer_and_single_choice_uses_option_id() {
        let (mut view, mut rx, _stream) = panel(vec![question(false)]).await;
        let _ = render_lines(&mut view, 60, 16);
        assert!(rx.try_recv().is_err());
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("answer");
        };
        assert_eq!(answers[0].selected, ["b"]);
        drop(view);
        assert!(rx.try_recv().is_err(), "no dismissal after submission");
    }

    #[tokio::test]
    async fn multiple_questions_use_tabs_and_submit_review_tab() {
        let first = question(false);
        let mut second = question(true);
        second.id = "second".into();
        second.header = "Extras".into();
        let (mut view, mut rx, _stream) = panel(vec![first, second]).await;

        let (_, lines) = render_lines(&mut view, 80, 16);
        assert!(lines[0].starts_with("────"), "top divider: {}", lines[0]);
        assert!(
            lines[1].contains("Storage")
                && lines[1].contains("Extras")
                && lines[1].contains("Submit"),
            "tabs row: {}",
            lines[1]
        );

        // Answer question 0 -> advances to question 1.
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Handled);
        assert_eq!(view.current, 1);

        // Select both options + custom answer on question 1.
        view.handle_key(key(KeyCode::Char(' ')));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char(' ')));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.editing);
        view.handle_paste("自定义数据库");
        let _ = render_lines(&mut view, 40, 16);
        assert!(view.cursor_position().is_some());
        view.handle_key(key(KeyCode::Enter));
        assert!(!view.editing);

        // Navigate back to tab 0 via BackTab, then forward via Right.
        view.handle_key(key(KeyCode::BackTab));
        assert_eq!(view.current, 0);
        view.handle_key(key(KeyCode::Right));
        assert_eq!(view.current, 1);

        // Continue advances to Submit review tab.
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Handled);
        assert!(view.on_submit_tab());

        let (_, review) = render_lines(&mut view, 80, 16);
        let review_text = review.join("\n");
        assert!(review_text.contains("Review your answers"), "{review_text}");
        assert!(review_text.contains("SQLite"), "{review_text}");
        assert!(review_text.contains("自定义数据库"), "{review_text}");

        // Confirm on Submit tab sends all answers.
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("answer");
        };
        assert_eq!(answers[0].selected, ["a"]);
        assert_eq!(answers[1].selected, ["a", "b"]);
        assert_eq!(answers[1].custom.as_deref(), Some("自定义数据库"));
    }

    #[tokio::test]
    async fn editor_escape_returns_to_choices_and_drop_dismisses_question() {
        let (mut view, mut rx, _stream) = panel(vec![question(false)]).await;
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.editing);
        assert_eq!(view.handle_key(key(KeyCode::Esc)), KeyResult::Handled);
        assert!(rx.try_recv().is_err());
        drop(view);
        assert!(matches!(
            rx.try_recv(),
            Ok(DialogMessage::Question {
                response: QuestionResponse::Dismissed,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn cancellation_marks_panel_finished_without_dismissal_reply() {
        let (view, mut rx, _stream) = panel(vec![question(false)]).await;
        let session = view.session.clone();
        assert!(!view.is_finished());
        session
            .dispatch(RuntimeCommand::Cancel)
            .unwrap()
            .accepted()
            .await
            .unwrap();
        assert!(view.is_finished());
        drop(view);
        assert!(
            rx.try_recv().is_err(),
            "task cancellation does not send a dismissal reply"
        );
    }

    #[tokio::test]
    async fn free_text_only_multiselect_can_clear_custom_answer_and_submit() {
        let mut q = question(true);
        q.options.clear();
        let (mut view, mut rx, _stream) = panel(vec![q]).await;
        view.handle_key(key(KeyCode::Enter));
        view.handle_paste("first");
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.answers[0].custom.as_deref(), Some("first"));
        view.handle_key(key(KeyCode::Char(' ')));
        assert!(view.answers[0].custom.is_none());
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Handled);
        view.handle_key(key(KeyCode::Up));
        view.handle_key(key(KeyCode::Enter));
        view.handle_paste("final");
        view.handle_key(key(KeyCode::Enter));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("answer");
        };
        assert_eq!(answers[0].custom.as_deref(), Some("final"));
    }

    #[tokio::test]
    async fn narrow_and_zero_sized_terminals_render_without_panicking() {
        let (mut view, _rx, _stream) = panel(vec![question(true)]).await;
        for (width, height) in [(0, 0), (1, 1), (3, 3), (8, 5), (25, 10)] {
            let area = Rect::new(0, 0, width, height);
            view.render(&mut Buffer::empty(area), area);
        }
    }

    #[tokio::test]
    async fn inline_question_supports_tab_and_option_mouse_clicks() {
        let first = question(false);
        let mut second = question(false);
        second.id = "second".into();
        second.header = "Region".into();
        let (mut view, mut rx, _stream) = panel(vec![first, second]).await;
        let height = view.desired_height(70).unwrap();
        let (buf, lines) = render_lines(&mut view, 70, height);

        // Unfocused placeholder "Type something." uses muted styling, not accent.
        let custom_row = lines
            .iter()
            .position(|row| row.contains("Type something."))
            .expect("custom row") as u16;
        let custom_byte = lines[custom_row as usize].find("Type something.").unwrap();
        let custom_col = lines[custom_row as usize][..custom_byte].chars().count() as u16;
        assert_eq!(buf[(custom_col, custom_row)].fg, Theme::dark().muted);

        // Clicking the "Region" tab switches directly to tab 1.
        let region_byte = lines[1].find("Region").unwrap();
        let region_col = lines[1][..region_byte].chars().count() as u16;
        view.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: region_col,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        view.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: region_col,
            row: 1,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(view.current, 1);

        // Click back to tab 0, click PostgreSQL -> advances to tab 1, click SQLite -> Submit tab.
        view.switch_tab(0);
        let (_, lines) = render_lines(&mut view, 70, height);
        let pg_row = lines
            .iter()
            .position(|row| row.contains("PostgreSQL"))
            .unwrap() as u16;
        let pg_byte = lines[pg_row as usize].find("PostgreSQL").unwrap();
        let pg_col = lines[pg_row as usize][..pg_byte].chars().count() as u16;
        view.handle_mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: pg_col,
            row: pg_row,
            modifiers: KeyModifiers::NONE,
        });
        view.handle_mouse(MouseEvent {
            kind: MouseEventKind::Up(MouseButton::Left),
            column: pg_col,
            row: pg_row,
            modifiers: KeyModifiers::NONE,
        });
        assert_eq!(view.current, 1);
        assert_eq!(view.answers[0].selected, ["b"]);
        assert!(rx.try_recv().is_err());
    }
}
