use super::*;

impl App {
    pub(super) fn on_dialog_message(&mut self, message: DialogMessage) {
        match message {
            DialogMessage::Question {
                session_id,
                request_id,
                response,
            } => {
                self.sessions
                    .answer_question(&session_id, request_id, response);
            }
            DialogMessage::Cancelled => {}
            DialogMessage::Theme(name) => self.set_theme(&name),
            DialogMessage::Session(id) => self.switch_session(&id, false),
            DialogMessage::Command(text) => self.submit(text),
            DialogMessage::Confirm {
                action: ConfirmAction::RunTool,
                accepted,
            } => {
                // The loop is still waiting on this answer, so it is sent even
                // when the prompt was dismissed: a denial is a real answer.
                if let Some(reply) = self.pending_approvals.pop_front() {
                    let answer = if accepted {
                        Approval::Approved
                    } else {
                        Approval::Denied
                    };
                    let _ = reply.send(answer);
                }
            }
            DialogMessage::Confirm {
                action: ConfirmAction::ClearTranscript,
                accepted: true,
            } => {
                self.sessions.dispatch_clear();
            }

            DialogMessage::Confirm {
                accepted: false, ..
            } => {}
        }
    }

    // ----------------------------------------------------------------- dialogs

    pub(super) fn push_overlay(&mut self, component: Box<dyn Component>, options: OverlayOptions) {
        self.overlay_queue.borrow_mut().push((component, options));
    }

    /// Answer a call the loop is waiting on.
    ///
    /// The policy layer has already decided this call needs confirmation,
    /// including read-only calls covered by an explicit ask rule.
    pub(super) fn request_approval(&mut self, request: ApprovalRequest) {
        let ApprovalRequest { call, reply } = request;

        self.pending_approvals.push_back(reply);
        let dialog = ConfirmDialog::new(
            format!("Run {}?", call.name),
            describe_call(&call),
            ConfirmAction::RunTool,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered().width(SizeValue::Percent(64)),
        );
    }

    pub(super) fn open_help(&mut self) {
        let lines = help_lines(&self.theme, crate::tui::commands::PROMPT_SLASH_COMMANDS);
        let dialog = TextDialog::new(
            "Help",
            "↑↓ scroll · enter or esc closes",
            lines,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(74))
                .max_height(SizeValue::Percent(85)),
        );
    }

    /// Show the skills this session found, and where each one lives.
    pub(super) fn open_skills(&mut self) {
        if self.resources.skills.is_empty() {
            self.notifications
                .info("no skills found — a skill is a directory holding a SKILL.md".to_string());
            return;
        }

        let lines = skills_lines(&self.resources.skills, &self.theme);
        let dialog = TextDialog::new(
            "Skills",
            "↑↓ scroll · enter or esc closes",
            lines,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(74))
                .max_height(SizeValue::Percent(85)),
        );
    }

    pub(super) fn open_stats(&mut self) {
        let snapshot = self.sessions.agent.snapshot();
        self.session.billing = snapshot.billing.or(self.session.billing);
        self.session.context = snapshot.context(self.config.mode);
        let backend = snapshot.label;
        let model = if snapshot.model.is_empty() {
            "(none yet)".to_string()
        } else {
            snapshot.model
        };
        let lines = stats_lines(
            &self.theme,
            &self.session,
            &model,
            &backend,
            self.config.mode,
            u64::from(
                snapshot
                    .capabilities
                    .context_window
                    .unwrap_or(DEFAULT_CONTEXT_WINDOW),
            ),
            snapshot.capabilities.price_known,
        );
        let dialog = TextDialog::new(
            "Stats",
            "enter or esc closes",
            lines,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered().width(SizeValue::Percent(60)),
        );
    }

    /// `/buddy` shows the companion; `/buddy name <name>` names it.
    pub(super) fn open_buddy(&mut self, args: &str) {
        if let Some(name) = args.strip_prefix("name") {
            self.name_buddy(name.trim());
            return;
        }

        let lines = buddy_lines(&self.buddy, &self.theme);
        let dialog = TextDialog::new(
            "Buddy",
            "↑↓ scroll · enter or esc closes",
            lines,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered().width(SizeValue::Percent(60)),
        );
    }

    /// Give the companion a name and remember it.
    fn name_buddy(&mut self, name: &str) {
        if name.is_empty() {
            self.notifications
                .error("usage: /buddy name <name>".to_string());
            return;
        }

        let name = name.chars().take(24).collect::<String>();
        self.buddy.soul = Some(Soul::new(
            name.clone(),
            self.buddy
                .soul
                .as_ref()
                .map(|soul| soul.personality.clone())
                .unwrap_or_default(),
            recent::now_ms(),
        ));
        self.persist_buddy();
        self.notifications.info(format!(
            "buddy: {name} the {}",
            self.buddy.bones.species.as_str()
        ));
    }

    pub(super) fn open_palette(&mut self) {
        let items = commands::palette_items();
        let count = items.len();
        let dialog = SelectDialog::new(
            "Commands",
            HINT_SELECT,
            items,
            &self.theme,
            self.dialog_tx.clone(),
            |value| DialogMessage::Command(format!("/{value}")),
        )
        .label("Select a command:");
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(72))
                .height(SizeValue::Cells(SelectDialog::height_for(count))),
        );
    }

    pub(super) fn open_theme_dialog(&mut self) {
        let current = self.config.theme.clone();
        let items = Theme::NAMES
            .iter()
            .map(|name| SelectItem::new(*name, *name).description(theme_description(name)))
            .collect::<Vec<_>>();
        let count = Theme::NAMES.len();
        let dialog = SelectDialog::new(
            "Theme",
            HINT_SELECT,
            items,
            &self.theme,
            self.dialog_tx.clone(),
            |value| DialogMessage::Theme(value.to_string()),
        )
        .label("Select a theme:")
        .selected(&current);
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(50))
                .height(SizeValue::Cells(SelectDialog::height_for(count))),
        );
    }

    pub(super) fn open_clear_confirm(&mut self) {
        let body = "This removes every turn from the transcript. It cannot be undone.";
        let dialog = ConfirmDialog::new(
            "Clear transcript",
            body,
            ConfirmAction::ClearTranscript,
            &self.theme,
            self.dialog_tx.clone(),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(50))
                .height(SizeValue::Cells(ConfirmDialog::height_for(body, 60))),
        );
    }

    // ----------------------------------------------------------------- connect
}
