use super::*;

impl App {
    /// Stop the turn that is streaming; whatever it produced stays on screen.
    ///
    /// Dropping the receiver ends the backend task as well, because its next
    /// send fails. Claude Code answers an interrupt by sending anything queued
    /// next, so the queue keeps running in order instead of stalling.
    pub(super) fn cancel_turn(&mut self) {
        // Cancellation is an execution command, independent of view subscriptions.
        self.sessions
            .dispatch(solaris_agent::RuntimeCommand::Cancel);
        if let Some(turn) = self.session.active_turn_mut() {
            turn.complete = true;
            self.session.status = None;
            self.session.bump();
        }
        self.notifications.info("cancelled");
    }

    pub(super) fn set_mode(&mut self, mode: Mode) {
        self.config.mode = mode;
        self.persist_settings();
        self.notifications
            .info(format!("{} mode", self.config.mode.label()));
    }

    pub(super) fn toggle_mode(&mut self) {
        self.set_mode(self.config.mode.next());
    }

    pub(super) fn cycle_theme(&mut self) {
        let next = Theme::cycle_name(&self.config.theme);
        self.set_theme(next);
    }

    pub(super) fn set_theme(&mut self, name: &str) {
        let theme = Theme::by_name(name);
        self.config.theme = theme.name.clone();
        self.persist_settings();
        self.theme = theme;
        self.selection
            .borrow_mut()
            .set_style(self.theme.selection_fg, self.theme.selection_bg);
        self.session_page.set_theme(&self.theme);
        self.notifications
            .info(format!("theme: {}", self.config.theme));
    }

    pub(super) fn set_model(&mut self, name: &str) {
        self.config.model = name.to_string();
        // A model belongs to the provider that offers it, so it is remembered
        // beside that provider rather than in the settings file.
        if let Some(provider) = self.provider_id() {
            self.connection.auth.activate(&provider);
        }
        self.connection.auth.remember_model(name);
        self.persist_auth();
        // The model decides the payload, the price and the size of the context
        // window, so the backend is resolved again.
        self.rebuild_backend();
        self.notifications
            .info(format!("model: {}", self.config.model));
    }

    pub(super) fn toggle_thinking(&mut self) {
        let expand = !self.session.turns.iter().any(|turn| turn.thinking_expanded);
        for turn in &mut self.session.turns {
            turn.thinking_expanded = expand;
            turn.layout_revision = turn.layout_revision.wrapping_add(1);
        }
        self.session.bump();
    }

    // ---------------------------------------------------------------- commands

    /// Submit prompt text: a slash command runs, anything else starts a turn.
    ///
    /// A prompt submitted while a turn is streaming joins that turn instead of
    /// waiting behind it: starting a second turn concurrently would attribute
    /// the in-flight deltas to the wrong turn, and waiting means the correction
    /// arrives after the model has done the thing being corrected. The loop
    /// folds it in before it asks again, so the message reaches the model as
    /// soon as the current round finishes.
    pub(crate) fn submit(&mut self, text: String) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        // A fresh prompt is the natural point to drop stale banners.
        self.notifications.clear();

        // Persist every submitted prompt and slash command in global history.
        self.session_page.bottom.editor_mut().push_history(&trimmed);
        self.persist_history();

        if let Some(command) = parse_slash_command(&trimmed) {
            self.execute_command(command);
            return;
        }

        if self.session.is_streaming() {
            self.steer(trimmed);
            return;
        }

        self.start_turn(trimmed.clone(), trimmed);
    }

    /// Add `text` to the turn that is running.
    ///
    /// The message goes into the agent's steering queue, and the agent reports
    /// it back as a [`AgentEvent::Steered`] event once the loop has actually
    /// folded it in — that is when the turn's record gets the note, so a
    /// message that arrives just too late to be read by this turn is recorded
    /// against the turn that does read it rather than the one it was typed into.
    pub(super) fn runtime_command(&self, command: solaris_agent::RuntimeCommand) {
        self.sessions.dispatch(command);
    }

    pub(super) fn steer(&mut self, text: String) {
        match self.sessions.agent.expand_input(&text) {
            Ok(prepared) if !prepared.trim().is_empty() => self
                .runtime_command(solaris_agent::RuntimeCommand::PreparedSteer { text: prepared }),
            Ok(_) => return,
            Err(error) => {
                self.notifications.error(error.to_string());
                return;
            }
        }
        self.notifications.info(format!(
            "steered: {}",
            crate::tui::view::transcript::summarize(&text)
        ));
    }

    fn execute_command(&mut self, command: SlashCommand) {
        let args = command.args.trim();

        // A skill is invoked as `/skill:<name>`: the colon keeps the name part of
        // the same word, which is what lets the editor offer every skill once
        // `/skill` has been typed, and the parser hands that word back whole.
        if let Some(name) = command.name.strip_prefix(SKILL_PREFIX) {
            if name.is_empty() {
                // `/skill:` on its own is the prefix a completion is built out
                // of rather than a command; listing is what was about to be
                // picked from anyway.
                self.open_skills();
            } else {
                self.run_skill(name, args);
            }
            return;
        }

        match command.name.as_str() {
            "help" => self.open_help(),
            "connect" => self.open_connect(),
            "theme" => {
                if args.is_empty() {
                    self.open_theme_dialog();
                } else {
                    self.set_theme(args);
                }
            }
            "model" => {
                if args.is_empty() {
                    self.open_model_picker();
                } else {
                    self.set_model(args);
                }
            }
            "mode" => {
                if args.is_empty() {
                    self.toggle_mode();
                } else if let Some(mode) = Mode::parse(args) {
                    self.set_mode(mode);
                } else {
                    self.notifications.error(format!(
                        "unknown mode `{args}` — expected default, plan, auto, or ask"
                    ));
                }
            }
            "clear" => self.open_clear_confirm(),
            "compact" => self.compact_now(),
            "queue" => self.queue_follow_up(args),
            "skills" => self.open_skills(),
            "sessions" => self.open_sessions(false),
            "tree" => self.open_sessions(true),
            "resume" => {
                if args.is_empty() {
                    self.open_sessions(false);
                } else {
                    self.switch_session(args, false);
                }
            }
            "fork" => {
                if let Some(journal) = self.sessions.agent.journal() {
                    self.switch_session(journal.id(), true);
                } else {
                    self.notifications.warning("fork requires a saved session");
                }
            }
            "reload" => self.reload_resources(),
            // A skill's name is taken on the colon rather than after a space; a
            // reminder that says so is worth more than "unknown command".
            "skill" => self.notifications.error(format!(
                "a skill is loaded as `{SKILL_PREFIX}<name>` — /skills lists them"
            )),
            "stats" => self.open_stats(),
            "subagents" => self.notifications.info(format!(
                "Active tasks: {}",
                self.sessions.agent.snapshot().active_tasks.join(", ")
            )),
            "stop" => {
                if self
                    .sessions
                    .agent
                    .snapshot()
                    .active_tasks
                    .iter()
                    .any(|id| id == command.args.trim())
                {
                    self.runtime_command(solaris_agent::RuntimeCommand::CancelTask {
                        id: command.args.trim().into(),
                    });
                    self.notifications.info("stopping child task");
                } else {
                    self.notifications
                        .warning("no active child with that task ID");
                }
            }
            "buddy" => self.open_buddy(args),
            "quit" | "exit" => self.quit.set(true),
            other => self
                .notifications
                .error(format!("unknown command /{other} — try /help")),
        }
    }
}
