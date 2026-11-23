//! Interactive question presentation. Runtime owns request validity and settlement.
use super::dialogs::{DialogMessage, DialogStyles};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
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
    component::{Component, KeyResult},
    components::editor::Editor,
    theme::Theme,
    util::wrap_text,
};
use std::sync::Arc;
use tokio::sync::mpsc::UnboundedSender;

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
        editor.set_styles(crate::app::editor_styles(theme));
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
        KeyResult::Handled
    }
}
impl Component for QuestionDialog {
    fn is_finished(&self) -> bool {
        !self.session.is_question_pending(&self.request.request_id)
    }
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        buf.set_style(area, self.styles.surface);
        let area = Rect {
            x: area.x.saturating_add(1),
            y: area.y.saturating_add(1),
            width: area.width.saturating_sub(2),
            height: area.height.saturating_sub(2),
        };
        if area.width == 0 || area.height == 0 {
            return;
        }
        let q = &self.request.questions[self.current];
        let mut lines = Vec::new();
        let mut add = |text: String, style| {
            for line in wrap_text(&text, area.width as usize) {
                lines.push(Line::from(Span::styled(line, style)));
            }
        };
        add(
            format!(
                "Waiting for your answer · {} ({}/{})",
                q.header,
                self.current + 1,
                self.request.questions.len()
            ),
            self.styles.title,
        );
        add(q.question.clone(), self.styles.text);
        add(String::new(), self.styles.text);
        let mut focus = lines.len();
        for (i, o) in q.options.iter().enumerate() {
            if i == self.cursor {
                focus = lines.len();
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
            for line in wrap_text(&text, area.width as usize) {
                lines.push(Line::from(Span::styled(
                    line,
                    if i == self.cursor {
                        self.styles.accent
                    } else {
                        self.styles.text
                    },
                )));
            }
            if !o.description.is_empty() {
                for line in wrap_text(&format!("    {}", o.description), area.width as usize) {
                    lines.push(Line::from(Span::styled(line, self.styles.muted)));
                }
            }
        }
        if q.allow_custom {
            if self.cursor == q.options.len() {
                focus = lines.len();
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "{} Custom: {}",
                    if self.cursor == q.options.len() {
                        ">"
                    } else {
                        " "
                    },
                    self.answers[self.current]
                        .custom
                        .as_deref()
                        .unwrap_or("Type your own answer")
                ),
                self.styles.accent,
            )));
        }
        if q.multiple {
            let selected = self.cursor == q.options.len() + usize::from(q.allow_custom);
            if selected {
                focus = lines.len();
            }
            lines.push(Line::from(Span::styled(
                format!("{} Continue", if selected { ">" } else { " " }),
                self.styles.accent,
            )));
        }
        let editor_height = if self.editing {
            area.height.saturating_sub(2).min(5)
        } else {
            0
        };
        let height = area.height.saturating_sub(2 + editor_height);
        let scroll = focus
            .saturating_sub(height.saturating_sub(1) as usize)
            .min(u16::MAX as usize) as u16;
        Paragraph::new(lines)
            .scroll((scroll, 0))
            .render(Rect { height, ..area }, buf);
        let hint = if self.editing {
            "Enter save · Esc back"
        } else if q.multiple {
            "↑↓ move · Space select · Enter next · Shift+Tab back · Esc dismiss"
        } else {
            "↑↓ move · Enter choose · Shift+Tab back · Esc dismiss"
        };
        if self.editing {
            self.editor.render(
                buf,
                Rect {
                    y: area.y + height,
                    height: editor_height,
                    ..area
                },
            );
        }
        Paragraph::new(hint).style(self.styles.hint).render(
            Rect {
                y: area.y + height + editor_height,
                height: area.height - height - editor_height,
                ..area
            },
            buf,
        );
    }
    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
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
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => self.cursor = (self.cursor + 1).min(count.saturating_sub(1)),
            KeyCode::BackTab if self.current > 0 => {
                self.current -= 1;
                self.cursor = 0;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.cursor == q.options.len() && q.allow_custom {
                    if q.multiple
                        && key.code == KeyCode::Char(' ')
                        && self.answers[self.current].custom.is_some()
                    {
                        self.answers[self.current].custom = None;
                        return KeyResult::Handled;
                    }
                    self.editor
                        .set_text(self.answers[self.current].custom.as_deref().unwrap_or(""));
                    self.editing = true;
                } else if q.multiple && self.cursor == q.options.len() + usize::from(q.allow_custom)
                {
                    return self.next();
                } else if let Some(o) = q.options.get(self.cursor) {
                    let answer = &mut self.answers[self.current];
                    if !q.multiple {
                        answer.selected = vec![o.id.clone()];
                        answer.custom = None;
                        return self.next();
                    }
                    if key.code == KeyCode::Char(' ') {
                        if answer.selected.contains(&o.id) {
                            answer.selected.retain(|id| id != &o.id);
                        } else {
                            answer.selected.push(o.id.clone());
                        }
                    } else {
                        return self.next();
                    }
                }
            }
            _ => {}
        }
        KeyResult::Handled
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
            Ok(crate::test_support::client(Default::default(), move |_| {
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
            }))
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
}
