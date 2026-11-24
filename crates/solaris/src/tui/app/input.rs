use super::*;

impl App {
    // ------------------------------------------------------------------- input

    pub(super) fn handle_session_key(&mut self, key: KeyEvent) -> KeyResult {
        if self.session_page.bottom.is_prompt_active()
            && self.session_page.bottom.editor().is_empty()
            && key.modifiers.is_empty()
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
            && !self.subagent_targets().is_empty()
        {
            self.switch_subagent(key.code == KeyCode::Right);
            return KeyResult::Handled;
        }
        // Global shortcuts work regardless of the editor contents.
        if self.keybindings.matches("help", &key)
            || (self.session_page.bottom.editor().is_empty() && key.code == KeyCode::Char('?'))
        {
            self.open_help();
            return KeyResult::Handled;
        }
        if self.keybindings.matches("theme", &key) {
            self.cycle_theme();
            return KeyResult::Handled;
        }
        if self.keybindings.matches("clear", &key) {
            self.open_clear_confirm();
            return KeyResult::Handled;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('o') {
            self.toggle_thinking();
            return KeyResult::Handled;
        }

        // Transcript scrolling stays available while the editor has focus.
        let page = self.session_page.transcript.area().height.max(1) as i32;
        match key.code {
            KeyCode::PageUp => {
                self.scroll_transcript(-page);
                return KeyResult::Handled;
            }
            KeyCode::PageDown => {
                self.scroll_transcript(page);
                return KeyResult::Handled;
            }
            _ => {}
        }

        let is_toggle_mode = self.keybindings.matches("toggle_mode", &key);
        let (result, action) = self.session_page.bottom.handle_key(key, is_toggle_mode);
        self.apply_bottom_action(action);
        result
    }

    pub(super) fn apply_bottom_action(&mut self, action: crate::tui::containers::BottomAction) {
        use crate::tui::containers::{BottomAction, PromptAction};
        match action {
            BottomAction::None | BottomAction::QuestionClosed => {}
            BottomAction::Prompt(PromptAction::None) => {}
            BottomAction::Prompt(PromptAction::Submit(text)) => self.submit(text),
            BottomAction::Prompt(PromptAction::ToggleMode) => self.toggle_mode(),
            BottomAction::Inline(InlineOutcome::Connect(outcome)) => {
                self.apply_connect_outcome(outcome)
            }
            BottomAction::Inline(InlineOutcome::Model(outcome)) => {
                self.apply_model_outcome(outcome)
            }
            BottomAction::Inline(InlineOutcome::Skill(outcome)) => {
                self.apply_skill_outcome(outcome)
            }
            BottomAction::Approval { id, accepted } => self.resolve_approval(id, accepted),
        }
    }

    pub(super) fn apply_skill_outcome(&mut self, outcome: crate::tui::skill::SkillOutcome) {
        use crate::tui::skill::SkillOutcome;
        match outcome {
            SkillOutcome::Handled => {}
            SkillOutcome::Closed => self.close_inline(),
            SkillOutcome::Picked { skill_name } => {
                self.close_inline();
                self.session_page
                    .bottom
                    .editor_mut()
                    .insert_paste(&format!("@{skill_name} "));
            }
        }
    }

    pub(super) fn scroll_transcript(&mut self, delta: i32) {
        self.session.scroll = self.session.scroll.saturating_add_signed(delta as isize);
        self.session.follow_end = false;
    }

    pub(super) fn copy_text(&mut self, text: &str, success: String, failure: &str) {
        if self.clipboard.background {
            let text = text.to_owned();
            let failure = failure.to_owned();
            self.io
                .submit_clipboard(move || crate::desktop::Completion::Copied {
                    ok: crate::desktop::clipboard::system_write(&text),
                    success,
                    failure,
                });
        } else if (self.clipboard.write)(text) {
            self.notifications.info(success);
        } else {
            self.notifications.warning(failure);
        }
    }

    /// Copy the current selection to the system clipboard, saying so either way.
    fn copy_selection(&mut self, text: &str) {
        let characters = text.chars().count();
        if characters == 0 {
            return;
        }

        let plural = if characters == 1 { "" } else { "s" };
        self.copy_text(
            text,
            format!("copied {characters} character{plural}"),
            "could not reach the system clipboard",
        );
    }

    /// Put the clipboard into whatever is taking text, saying so when there is
    /// nothing to put.
    ///
    /// claurst reads the clipboard on the same key and warns about an empty
    /// one.
    pub(super) fn paste_from_clipboard(&mut self) {
        if self.clipboard.background {
            let generation = self.input_generation;
            self.io
                .submit_clipboard(move || crate::desktop::Completion::Pasted {
                    generation,
                    text: crate::desktop::clipboard::system_read(),
                });
            return;
        }
        match (self.clipboard.read)() {
            Some(text) if !text.is_empty() => self.insert_pasted(&text),
            _ => self.notifications.warning("clipboard is empty"),
        }
    }

    /// Insert pasted text where it belongs: the wizard's field while it is up,
    /// the prompt otherwise. The wizard drops a paste when its current step has
    /// no field, rather than leaking the payload into the prompt behind it.
    pub(super) fn insert_pasted(&mut self, text: &str) {
        self.session_page.bottom.handle_paste(text);
    }

    /// Copy the selection when a range was drawn, reporting whether there was one.
    ///
    /// This is the first rung of the ctrl+C ladder the reference terminals use:
    /// pi-tui binds `ctrl+c` to a copy action, Claude Code and claurst interrupt
    /// first, and only a key with nothing left to do asks about quitting. Copy
    /// sits on top of all of them, so a range drawn over the transcript, over a
    /// streaming turn or over the wizard is always what the key does.
    ///
    /// A range holding nothing but blanks still counts as answered: the user drew
    /// it, so the key must not fall through to clearing the prompt or arming the
    /// quit.
    pub(super) fn copy_selection_if_any(&mut self) -> bool {
        let selected = {
            let selection = self.selection.borrow();
            if !selection.is_active() {
                return false;
            }
            selection.selected_text()
        };

        self.copy_selection(&selected);
        true
    }

    /// Ctrl+C with nothing drawn: stop a turn that is streaming, or start the
    /// two-press quit.
    ///
    /// [`App::copy_selection_if_any`] settles the copy case before this is
    /// reached, so a drawn range never cancels a turn or clears the prompt.
    pub(super) fn interrupt(&mut self) {
        if self.session.is_streaming() {
            self.cancel_turn();
            return;
        }

        // With nothing running, the prompt is what the key clears — Claude
        // Code and claurst both drop the typed text on the first press.
        if !self.session_page.bottom.editor().is_empty() {
            self.session_page.bottom.editor_mut().clear();
        }
        self.confirm_quit('c');
    }

    /// Two presses of the same key inside [`QUIT_CONFIRM_WINDOW`] quit; the
    /// first one arms the gesture and says so.
    pub(super) fn confirm_quit(&mut self, key_char: char) {
        let now = Instant::now();
        let armed = self.quit_press.is_some_and(|(armed_key, at)| {
            armed_key == key_char && now.duration_since(at) <= QUIT_CONFIRM_WINDOW
        });

        if armed {
            self.quit.set(true);
            return;
        }

        self.quit_press = Some((key_char, now));
        self.notifications
            .info(format!("press ctrl+{key_char} again to quit"));
    }
}
