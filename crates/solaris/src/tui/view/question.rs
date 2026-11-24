//! Interactive question presentation. Runtime owns request validity and settlement.
use super::dialogs::{DialogMessage, DialogStyles, draw_chrome, draw_hint};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    text::{Line, Span},
    widgets::{Paragraph, Widget},
};
use solaris_agent::{
    QuestionAnswer, QuestionRequest, QuestionResponse, RuntimeCommand, SessionRuntime,
};
use solaris_tui::{
    component::{Component, KeyResult, MouseResult},
    components::editor::Editor,
    theme::Theme,
    util::{rect_contains, wrap_text},
};
use std::ops::Range;
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

struct QuestionLines {
    lines: Vec<Line<'static>>,
    focus: usize,
    targets: Vec<(usize, Range<usize>)>,
}

pub struct QuestionDialog {
    request: QuestionRequest,
    session: Arc<SessionRuntime>,
    current: usize,
    cursor: usize,
    answers: Vec<QuestionAnswer>,
    editing: bool,
    submitted: bool,
    editor: Editor,
    styles: DialogStyles,
    sender: UnboundedSender<DialogMessage>,
    content_area: Rect,
    scroll: u16,
    max_scroll: u16,
    manual_scroll: bool,
    row_targets: Vec<(usize, Range<usize>)>,
    pressed_target: Option<usize>,
}
impl QuestionDialog {
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
        let mut editor = Editor::with_placeholder("Your answer…");
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
            styles: DialogStyles::from_theme(theme),
            sender,
            content_area: Rect::default(),
            scroll: 0,
            max_scroll: 0,
            manual_scroll: false,
            row_targets: Vec::new(),
            pressed_target: None,
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
    fn next(&mut self) -> KeyResult {
        let answer = &self.answers[self.current];
        if answer.selected.is_empty() && answer.custom.is_none() {
            return KeyResult::Handled;
        }
        if self.current + 1 == self.request.questions.len() {
            return self.finish(QuestionResponse::Answered {
                answers: self.answers.clone(),
            });
        }
        self.current += 1;
        self.cursor = 0;
        self.scroll = 0;
        self.manual_scroll = false;
        KeyResult::Handled
    }
    fn build_lines(&self, width: usize) -> QuestionLines {
        let q = &self.request.questions[self.current];
        let mut lines = Vec::new();
        let mut targets = Vec::new();
        for line in wrap_text(&q.question, width) {
            lines.push(Line::from(Span::styled(line, self.styles.text)));
        }
        lines.push(Line::default());

        let mut focus = lines.len();
        for (i, o) in q.options.iter().enumerate() {
            let start = lines.len();
            if i == self.cursor {
                focus = start;
            }
            let checked = if self.answers[self.current].selected.contains(&o.id) {
                "x"
            } else {
                " "
            };
            let text = format!(
                "{} [{}] {}",
                if i == self.cursor { ">" } else { " " },
                checked,
                o.label
            );
            let style = if i == self.cursor {
                self.styles.accent
            } else {
                self.styles.text
            };
            for line in wrap_text(&text, width) {
                lines.push(Line::from(Span::styled(line, style)));
            }
            if !o.description.is_empty() {
                for line in wrap_text(&format!("    {}", o.description), width) {
                    lines.push(Line::from(Span::styled(line, self.styles.muted)));
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
            let style = if focused {
                self.styles.accent
            } else {
                self.styles.text
            };
            let text = format!(
                "{} Custom: {}",
                if focused { ">" } else { " " },
                self.answers[self.current]
                    .custom
                    .as_deref()
                    .unwrap_or("Type your own answer")
            );
            for line in wrap_text(&text, width) {
                lines.push(Line::from(Span::styled(line, style)));
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
            let style = if focused {
                self.styles.accent
            } else {
                self.styles.text
            };
            lines.push(Line::from(Span::styled(
                format!("{} Continue", if focused { ">" } else { " " }),
                style,
            )));
            targets.push((target_index, start..lines.len()));
        }
        QuestionLines {
            lines,
            focus,
            targets,
        }
    }
    fn activate_cursor(&mut self, toggle: bool) -> KeyResult {
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
            return self.next();
        } else if let Some(o) = q.options.get(self.cursor) {
            let answer = &mut self.answers[self.current];
            if !q.multiple {
                answer.selected = vec![o.id.clone()];
                answer.custom = None;
                return self.next();
            }
            if toggle {
                if answer.selected.contains(&o.id) {
                    answer.selected.retain(|id| id != &o.id);
                } else {
                    answer.selected.push(o.id.clone());
                }
            } else {
                return self.next();
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
}
impl Component for QuestionDialog {
    fn is_finished(&self) -> bool {
        self.submitted || !self.session.is_question_pending(&self.request.request_id)
    }
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        let q = &self.request.questions[self.current];
        let title = format!(
            "Waiting for your answer · {} ({}/{})",
            q.header,
            self.current + 1,
            self.request.questions.len()
        );
        let Some(layout) = draw_chrome(buf, area, &title, &self.styles) else {
            self.content_area = Rect::default();
            self.row_targets.clear();
            return;
        };
        let content = layout.content;
        let QuestionLines {
            lines,
            focus,
            targets,
        } = self.build_lines(content.width.max(1) as usize);
        self.row_targets = targets;

        let editor_height = if self.editing {
            content.height.saturating_sub(1).min(5)
        } else {
            0
        };
        let list_height = content.height.saturating_sub(editor_height);
        self.content_area = Rect {
            height: list_height,
            ..content
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
                    y: content.y + list_height,
                    height: editor_height,
                    ..content
                },
            );
        }
        let hint = if self.editing {
            "Enter save · Esc back"
        } else if q.multiple {
            "↑↓ move · Space select · Enter next · Shift+Tab back · Esc dismiss"
        } else {
            "↑↓ move · Enter choose · Shift+Tab back · Esc dismiss"
        };
        draw_hint(buf, layout.hint, hint, &self.styles, None);
    }
    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        self.pressed_target = None;
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
                    return self.next();
                }
            }
            return KeyResult::Handled;
        }
        let q = &self.request.questions[self.current];
        let count = q.options.len() + usize::from(q.allow_custom) + usize::from(q.multiple);
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
            KeyCode::BackTab if self.current > 0 => {
                self.current -= 1;
                self.cursor = 0;
                self.scroll = 0;
                self.manual_scroll = false;
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
                if let Some(target) = self.hit_target(event.column, event.row) {
                    self.cursor = target;
                    self.pressed_target = Some(target);
                    self.manual_scroll = false;
                    return MouseResult::Handled;
                }
                self.pressed_target = None;
                MouseResult::Ignored
            }
            MouseEventKind::Drag(_) => {
                self.pressed_target = None;
                MouseResult::Ignored
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let pressed = self.pressed_target.take();
                if let Some(target) = pressed
                    && self.hit_target(event.column, event.row) == Some(target)
                {
                    self.cursor = target;
                    let q = &self.request.questions[self.current];
                    let is_continue =
                        q.multiple && target == q.options.len() + usize::from(q.allow_custom);
                    let toggle = q.multiple && !is_continue;
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
        let inner_width = width.saturating_sub(2).max(4) as usize;
        let built = self.build_lines(inner_width);
        let editor_rows = if self.editing { 5u16 } else { 0 };
        // panel padding 2 + title 1 + blank 1 + content + editor + blank 1 + hint 1 = +6
        Some(
            (built.lines.len().max(1) as u16)
                .saturating_add(editor_rows)
                .saturating_add(6),
        )
    }
}

impl Drop for QuestionDialog {
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
    use solaris_tui::{overlay::OverlayOptions, tui::Tui};
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
                                serde_json::json!({"questions":questions}),
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
        serde_json::from_value(serde_json::json!({"id":"q","header":"Storage","question":"选择存储方式？","options":[{"id":"a","label":"SQLite","description":"本地"},{"id":"b","label":"PostgreSQL","description":"共享"}],"multiple":multiple,"allow_custom":true})).unwrap()
    }
    async fn dialog(
        questions: Vec<solaris_agent::Question>,
    ) -> (
        QuestionDialog,
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
                mode: Mode::Build,
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
            QuestionDialog::new(request, session, &Theme::dark(), tx),
            rx,
            stream,
        )
    }
    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    #[tokio::test]
    async fn initial_focus_is_not_an_answer_and_single_choice_uses_option_id() {
        let (mut view, mut rx, _stream) = dialog(vec![question(false)]).await;
        let area = Rect::new(0, 0, 60, 18);
        let mut buf = Buffer::empty(area);
        view.render(&mut buf, area);
        assert!(rx.try_recv().is_err());
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("answer")
        };
        assert_eq!(answers[0].selected, ["b"]);
        drop(view);
        assert!(rx.try_recv().is_err(), "no dismissal after submission");
    }
    #[tokio::test]
    async fn multiple_questions_preserve_answers_and_multiselect_supports_custom_editor() {
        let first = question(false);
        let mut second = question(true);
        second.id = "second".into();
        let (mut view, mut rx, _stream) = dialog(vec![first, second]).await;
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Handled);
        assert_eq!(view.current, 1);
        view.handle_key(key(KeyCode::Char(' ')));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Char(' ')));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Enter));
        assert!(view.editing);
        view.handle_paste("自定义数据库");
        let area = Rect::new(0, 0, 30, 14);
        let mut buf = Buffer::empty(area);
        view.render(&mut buf, area);
        assert!(view.cursor_position().is_some());
        view.handle_key(key(KeyCode::Enter));
        assert!(!view.editing);
        view.handle_key(key(KeyCode::BackTab));
        assert_eq!(view.current, 0);
        view.handle_key(key(KeyCode::Enter));
        assert_eq!(view.current, 1);
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        view.handle_key(key(KeyCode::Down));
        assert_eq!(view.handle_key(key(KeyCode::Enter)), KeyResult::Confirmed);
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("answer")
        };
        assert_eq!(answers[0].selected, ["a"]);
        assert_eq!(answers[1].selected, ["a", "b"]);
        assert_eq!(answers[1].custom.as_deref(), Some("自定义数据库"));
    }
    #[tokio::test]
    async fn editor_escape_returns_to_choices_and_overlay_drop_dismisses_question() {
        let (mut view, mut rx, _stream) = dialog(vec![question(false)]).await;
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
    async fn cancellation_removes_the_overlay_without_another_keypress() {
        let (view, mut rx, _stream) = dialog(vec![question(false)]).await;
        let session = view.session.clone();
        let mut tui = Tui::new();
        tui.overlay_queue()
            .borrow_mut()
            .push((Box::new(view), OverlayOptions::centered()));
        let area = Rect::new(0, 0, 60, 18);
        tui.render(&mut Buffer::empty(area), area);
        assert!(tui.overlay_flag().get());
        session
            .dispatch(RuntimeCommand::Cancel)
            .unwrap()
            .accepted()
            .await
            .unwrap();
        assert!(tui.tick());
        assert!(!tui.overlay_flag().get());
        assert!(
            rx.try_recv().is_err(),
            "task cancellation does not send a dismissal reply"
        );
    }
    #[tokio::test]
    async fn free_text_only_multiselect_can_clear_custom_answer_and_submit() {
        let mut q = question(true);
        q.options.clear();
        let (mut view, mut rx, _stream) = dialog(vec![q]).await;
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
            panic!("answer")
        };
        assert_eq!(answers[0].custom.as_deref(), Some("final"));
    }

    #[tokio::test]
    async fn narrow_and_zero_sized_terminals_render_without_panicking() {
        let (mut view, _rx, _stream) = dialog(vec![question(true)]).await;
        for (width, height) in [(0, 0), (1, 1), (3, 3), (8, 5), (25, 10)] {
            let area = Rect::new(0, 0, width, height);
            view.render(&mut Buffer::empty(area), area);
        }
    }

    #[tokio::test]
    async fn question_dialog_uses_shared_chrome_focus_styling_and_mouse_clicks() {
        let (mut view, mut rx, _stream) = dialog(vec![question(false)]).await;
        let height = view.desired_height(60).unwrap();
        assert!(height < 18, "short question should shrink: {height}");
        let area = Rect::new(0, 0, 60, height);
        let mut buf = Buffer::empty(area);
        view.render(&mut buf, area);

        let lines: Vec<String> = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .map(|x| buf[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        let custom_row = lines
            .iter()
            .position(|row| row.contains("Custom:"))
            .expect("custom row") as u16;
        let custom_col = lines[custom_row as usize].find("Custom:").unwrap() as u16;
        // Cursor starts on option 0, so unfocused Custom row must use normal text colour, not accent.
        assert_eq!(buf[(custom_col, custom_row)].fg, Theme::dark().fg);

        let pg_row = lines
            .iter()
            .position(|row| row.contains("PostgreSQL"))
            .expect("option b row") as u16;
        let pg_col = lines[pg_row as usize].find("PostgreSQL").unwrap() as u16;
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
        assert!(view.is_finished());
        let DialogMessage::Question {
            response: QuestionResponse::Answered { answers },
            ..
        } = rx.try_recv().unwrap()
        else {
            panic!("expected mouse answer");
        };
        assert_eq!(answers[0].selected, ["b"]);
    }
}
