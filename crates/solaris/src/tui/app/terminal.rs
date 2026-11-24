use super::*;

impl Component for App {
    fn shutdown(&mut self) -> std::io::Result<()> {
        self.sessions.navigation.cancel();
        self.sessions.runtime.shutdown();
        self.io.shutdown().map_err(std::io::Error::other)
    }
    fn cursor_position(&self) -> Option<(u16, u16)> {
        if self.overlay_open() || self.subagent_terminal.is_some() {
            return None;
        }
        self.session_page.bottom.cursor_position()
    }
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        if self.subagent_terminal.is_some() {
            self.render_subagent(buf, area);
        } else {
            self.render_session(buf, area);
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        if !(key.code == KeyCode::Char('v') && key.modifiers.contains(KeyModifiers::CONTROL)) {
            self.input_generation = self.input_generation.wrapping_add(1);
        }
        if self.subagent_terminal.is_some() {
            if self.keybindings.matches("quit", &key) {
                self.copy_selection_if_any();
            }
            return self.handle_subagent_key(key);
        }
        // Paste first: a terminal that forwards ctrl+v expects it to work
        // wherever text is typed, the wizard included.
        if self.keybindings.matches("paste", &key) {
            self.paste_from_clipboard();
            return KeyResult::Handled;
        }

        // Ctrl+C copies first, wherever the range was drawn — over the wizard's
        // own text too — so "copy" outranks the "cancel" and "interrupt" below.
        if self.keybindings.matches("quit", &key) && self.copy_selection_if_any() {
            return KeyResult::Handled;
        }

        // Peer surfaces in BottomDock (`Inline` wizard / `QuestionPanel`) own
        // the keyboard while active — including Ctrl+C.
        if !self.session_page.bottom.is_prompt_active() {
            let (_, action) = self.session_page.bottom.handle_key(key, false);
            self.apply_bottom_action(action);
            return KeyResult::Handled;
        }

        if self.keybindings.matches("quit", &key) {
            self.interrupt();
            return KeyResult::Handled;
        }

        // Ctrl+D is the other half of the same gesture, and only from an empty
        // prompt: with text in it the key belongs to the editor.
        if self.keybindings.matches("exit", &key) && self.session_page.bottom.editor().is_empty() {
            self.confirm_quit('d');
            return KeyResult::Handled;
        }

        self.handle_session_key(key)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        self.input_generation = self.input_generation.wrapping_add(1);
        if self.subagent_terminal.is_some() {
            if self.handle_collapsible_mouse(event) {
                return MouseResult::Handled;
            }
            match event.kind {
                MouseEventKind::ScrollUp => self.scroll_subagent(-3),
                MouseEventKind::ScrollDown => self.scroll_subagent(3),
                _ => {}
            }
            return MouseResult::Handled;
        }
        let selected = self.selection.borrow().is_active();
        let (summary_handled, target) = self.session_page.transcript.handle_subagent_mouse(
            event,
            self.session.scroll,
            selected,
        );
        let tool_handled = self.handle_collapsible_mouse(event);
        if let Some(target) = target {
            self.open_subagent(target);
        }
        if summary_handled || tool_handled {
            return MouseResult::Handled;
        }

        // Non-prompt peer surfaces (`Inline`, `QuestionPanel`) take mouse
        // events inside their own area; the transcript above keeps scrolling.
        if !self.session_page.bottom.is_prompt_active() {
            let (result, action) = self.session_page.bottom.handle_mouse(event);
            self.apply_bottom_action(action);
            if result.is_handled() {
                return MouseResult::Handled;
            }
        }

        if rect_contains(self.session_page.transcript.area(), event.column, event.row) {
            match event.kind {
                MouseEventKind::ScrollUp => {
                    self.scroll_transcript(-3);
                    return MouseResult::Handled;
                }
                MouseEventKind::ScrollDown => {
                    self.scroll_transcript(3);
                    return MouseResult::Handled;
                }
                _ => {}
            }
        }
        if !self.session_page.bottom.is_prompt_active() {
            return MouseResult::Ignored;
        }
        let (result, action) = self.session_page.bottom.handle_mouse(event);
        self.apply_bottom_action(action);
        result
    }

    fn handle_paste(&mut self, text: &str) -> KeyResult {
        self.input_generation = self.input_generation.wrapping_add(1);
        if self.subagent_terminal.is_some() {
            return KeyResult::Handled;
        }
        self.insert_pasted(text);
        KeyResult::Handled
    }

    fn tick(&mut self) -> bool {
        let event_started = std::time::Instant::now();
        let mut dirty = false;
        let errors = std::iter::from_fn(|| self.sessions.runtime_errors.try_recv().ok())
            .take(32)
            .collect::<Vec<_>>();
        for error in errors {
            dirty = true;
            if !self.sessions.busy()
                && let Some(turn) = self.session.active_turn_mut()
            {
                turn.complete = true;
                turn.push_text(&format!("**Error:** {error}"));
                self.session.bump();
            }
            self.notifications.error(error);
        }
        while let Some(outcome) = self.sessions.poll_outcome() {
            dirty = true;
            if outcome.session_id != self.sessions.agent.snapshot().session_id {
                continue;
            }
            use crate::application::session::CommandTarget;
            match (outcome.target, outcome.result) {
                (CommandTarget::Prompt(turn), Ok(snapshot)) => {
                    if let Some(run) = snapshot.task_id {
                        self.session.runs.insert(run, turn);
                    }
                }
                (CommandTarget::Prompt(index), Err(error)) => {
                    if let Some(turn) = self.session.turns.get_mut(index) {
                        turn.complete = true;
                        turn.push_text(&format!("**Error:** {error}"));
                    }
                    self.session.bump();
                    self.notifications.error(error);
                }
                (CommandTarget::Clear, Ok(snapshot)) => {
                    self.session_page.bottom.clear_question();
                    self.session = SessionState::new();
                    self.session.session_id = snapshot.session_id;
                    self.session.sequence = snapshot.sequence;
                    self.sessions.runtime_events = crate::application::session::SessionFeed::new(
                        &self.sessions.agent,
                        snapshot.sequence,
                    );
                    self.session_page.transcript.invalidate();
                    self.selection.borrow_mut().clear();
                    self.notifications.info("transcript cleared");
                }
                (CommandTarget::Clear, Err(error)) => self.notifications.error(error),
                (CommandTarget::Question(id), Err(error)) => {
                    if self
                        .session_page
                        .bottom
                        .question()
                        .is_some_and(|panel| panel.request_id() == id)
                    {
                        self.session_page.bottom.clear_question();
                    }
                    self.notifications.error(error);
                }
                (CommandTarget::Question(_), Ok(_)) => {}
            }
        }
        dirty |= self.poll_navigation() | self.poll_io(false) | self.poll_oauth();
        let snapshot = self.sessions.agent.snapshot();
        let context = snapshot.context(self.config.mode);
        if self.session.context != context {
            self.session.context = context;
            dirty = true;
        }

        if let Some(billing) = snapshot.billing
            && self.session.billing != Some(billing)
        {
            self.session.billing = Some(billing);
            dirty = true;
        }

        for _ in 0..128 {
            if event_started.elapsed() >= Duration::from_millis(3) {
                break;
            }
            match self.sessions.runtime_events.try_recv() {
                Ok(Ok(event)) => {
                    self.apply_runtime_event(event);
                    dirty = true;
                }
                Ok(Err(error)) => {
                    self.notifications.warning(error);
                    self.recover_projection();
                    dirty = true;
                    break;
                }
                Err(_) => break,
            }
        }
        if let Some(result) = self.resources.poll_publication() {
            self.adopt_resources(result);
            dirty = true;
        }
        if self.resources.reload_pending
            && !self.sessions.busy()
            && self
                .sessions
                .runtime_pending
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
            && self.compact_rx.is_none()
            && self.resources.resource_rx.is_none()
            && !self.resources.publication_pending()
        {
            self.reload_resources();
            dirty = true;
        }

        // One loader runs at a time. A ready result waits for an idle publication boundary.
        if !self.sessions.busy() && self.compact_rx.is_none() {
            let result = self
                .resources
                .resource_rx
                .as_mut()
                .and_then(|rx| match rx.try_recv() {
                    Ok(result) => Some(result),
                    Err(TryRecvError::Disconnected) => {
                        Some(Err(std::io::Error::other("resource loader stopped")))
                    }
                    Err(TryRecvError::Empty) => None,
                });
            if let Some(result) = result {
                self.resources.resource_rx = None;
                self.publish_resources(result);
                dirty = true;
            }
        }

        // Dialog results.
        let mut messages = Vec::new();
        while let Ok(message) = self.dialog_rx.try_recv() {
            messages.push(message);
        }
        for message in messages {
            dirty = true;
            self.on_dialog_message(message);
        }

        if self.session_page.bottom.question().is_some_and(|panel| {
            panel.session_id() != self.sessions.agent.snapshot().session_id || panel.is_finished()
        }) {
            self.session_page.bottom.clear_question();
            dirty = true;
        }
        if self.session_page.bottom.question().is_none()
            && let Some(request) = self.sessions.agent.pending_questions().into_iter().next()
        {
            self.session_page.bottom.set_question(Some(
                crate::tui::view::question::QuestionPanel::new(
                    request,
                    Arc::clone(&self.sessions.agent),
                    &self.theme,
                    self.dialog_tx.clone(),
                ),
            ));
            dirty = true;
        }

        // Calls the loop is waiting on: answer each one, asking the user where a
        // prompt is warranted.
        let mut requests = Vec::new();
        if let Some(rx) = self.approval_rx.as_mut() {
            while let Ok(request) = rx.try_recv() {
                requests.push(request);
            }
        }
        for request in requests {
            dirty = true;
            self.request_approval(request);
        }

        // Models the provider reported to the background discovery task.
        let mut discovered = Vec::new();
        let mut discovery_done = false;
        if let Some(rx) = self.connection.models_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(answer) => discovered.push(answer),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        discovery_done = true;
                        break;
                    }
                }
            }
        }
        if discovery_done {
            self.connection.models_rx = None;
        }
        for answer in discovered {
            dirty = true;
            self.on_models_discovered(answer);
        }

        // Answer from a `/compact` that was running.
        let mut compacted = None;
        if let Some(rx) = self.compact_rx.as_mut() {
            match rx.try_recv() {
                Ok(answer) => compacted = Some(answer),
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => compacted = Some(None),
            }
        }
        if let Some(answer) = compacted {
            self.compact_rx = None;
            dirty = true;
            self.on_compacted(answer);
        }

        if self.notifications.tick() {
            dirty = true;
        }
        if self.session.is_streaming() {
            let frame = (self.buddy_started.elapsed().as_millis() / 100) as u16;
            if frame != self.spinner_frame {
                self.spinner_frame = frame;
                dirty = true;
            }
        }

        // The companion fidgets while the welcome box is on screen; once the
        // first turn starts there is nothing left to animate.
        if self.session.turns.is_empty() {
            let step = self.buddy_started.elapsed().as_millis() as u64 / BUDDY_STEP_MS;
            if step != self.buddy_step {
                self.buddy_step = step;
                dirty = true;
            }
        }

        dirty
    }
}
