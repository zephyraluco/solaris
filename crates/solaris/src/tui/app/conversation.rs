use super::*;

impl App {
    /// Run a skill as the next turn's prompt.
    ///
    /// The instructions travel as the prompt because that is what the command
    /// means: the user has decided the skill applies, and the model should not
    /// have to spend a round working that out. The transcript shows the
    /// invocation rather than the instructions it stands for.
    pub(super) fn run_skill(&mut self, name: &str, args: &str) {
        let invocation = if args.is_empty() {
            format!("/skill:{name}")
        } else {
            format!("/skill:{name} {args}")
        };
        if self.session.is_streaming() {
            self.steer(invocation);
            return;
        }
        self.start_turn(invocation.clone(), invocation);
    }

    /// Put `text` in line to run as the turn after the one in flight.
    ///
    /// This is what makes `steering` a choice rather than the only option: a
    /// message typed while a turn runs is steered into it, and `/queue` is how to
    /// say "this one after that one instead".
    pub(super) fn queue_follow_up(&mut self, text: &str) {
        if text.is_empty() {
            self.notifications
                .error("usage: /queue <prompt>".to_string());
            return;
        }
        if !self.session.is_streaming() {
            // Nothing to wait behind, so it is simply the next turn.
            let prompt = text.to_string();
            self.start_turn(prompt.clone(), prompt);
            return;
        }
        match self.sessions.agent.expand_input(text) {
            Ok(prepared) if !prepared.trim().is_empty() => {
                self.runtime_command(solaris_agent::RuntimeCommand::PreparedQueue {
                    text: prepared,
                });
                self.notifications.info(format!(
                    "after this turn: {}",
                    crate::tui::view::transcript::summarize(text)
                ));
            }
            Ok(_) => {}
            Err(error) => self.notifications.error(error.to_string()),
        }
    }

    pub(super) fn reload_resources(&mut self) {
        match self
            .resources
            .reload(self.sessions.busy() || self.compact_rx.is_some())
        {
            crate::application::resources::Reload::Unavailable => self
                .notifications
                .warning("no resource locations configured for this session"),
            crate::application::resources::Reload::Queued => self
                .notifications
                .info("resources will reload when the active turn finishes"),
            crate::application::resources::Reload::Started => {}
        }
    }

    pub(super) fn publish_resources(
        &mut self,
        result: std::io::Result<solaris_agent::ResourceSnapshot>,
    ) {
        match result {
            Ok(snapshot) => self
                .resources
                .publish(self.sessions.agent.clone(), snapshot),
            Err(error) => self.adopt_resources(Err(error)),
        }
    }
    pub(super) fn adopt_resources(
        &mut self,
        result: std::io::Result<solaris_agent::ResourceSnapshot>,
    ) {
        match result {
            Ok(snapshot) => {
                let warnings = snapshot.warnings.iter().chain(snapshot.skills.warnings());
                let diagnostics = warnings
                    .map(|warning| format!("{} — {}", warning.path.display(), warning.message))
                    .collect::<Vec<_>>();
                self.resources.skills = snapshot.skills.clone();
                self.editor
                    .set_hints(commands::hints(&self.resources.skills));
                if diagnostics.is_empty() {
                    self.notifications.info(format!(
                        "reloaded {} skills and prompt instructions",
                        self.resources.skills.len()
                    ));
                } else {
                    self.notifications.warning(format!(
                        "resources reloaded with diagnostics:\n{}",
                        diagnostics.join("\n")
                    ));
                }
            }
            Err(error) => self
                .notifications
                .error(format!("resources were not reloaded: {error}")),
        }
    }

    /// Summarize the conversation now.
    ///
    /// It runs off the event loop because it is a model call of its own, and its
    /// answer comes back the same way model discovery's does.
    pub(super) fn compact_now(&mut self) {
        if self.compact_rx.is_some() {
            self.notifications.info("already compacting".to_string());
            return;
        }
        let agent = Arc::clone(&self.sessions.agent);
        let (tx, rx) = unbounded_channel();
        self.compact_rx = Some(rx);
        self.notifications.clear();
        self.notifications.info("compacting…".to_string());

        let errors = self.sessions.runtime_error_tx.clone();
        tokio::spawn(async move {
            let mut dropped = None;
            match agent.execute(solaris_agent::RuntimeCommand::Compact).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        if let AgentEvent::Compacted { dropped: count } = event.event {
                            dropped = Some(count);
                        }
                    }
                }
                Err(error) => {
                    let _ = errors.send(error);
                }
            }
            let _ = tx.send(dropped);
        });
    }

    /// Report what the manual compaction did.
    pub(super) fn on_compacted(&mut self, dropped: Option<usize>) {
        self.session.context = self.sessions.agent.snapshot().context(self.config.mode);
        match dropped {
            Some(dropped) => self.notifications.info(format!(
                "summarized {dropped} messages; {} left in context",
                self.sessions.agent.conversation_len()
            )),
            None => self
                .notifications
                .warning("nothing to summarize, or the model would not produce a summary"),
        }
    }

    pub(super) fn start_turn(&mut self, submitted: String, prompt: String) {
        if self.sessions.runtime_events.is_failed() {
            self.editor.set_text(&submitted);
            self.notifications
                .error("cancel active work and use /clear to restart the event subscription");
            return;
        }
        if self.sessions.navigation.is_pending() || self.sessions.agent.snapshot().task_id.is_some()
        {
            self.editor.set_text(&submitted);
            self.notifications
                .info("wait for the current session operation to finish");
            return;
        }
        if self.connection.oauth.is_some() {
            self.notifications.warning(
                "OpenAI authorization is in progress; wait before submitting another prompt",
            );
            return;
        }
        let prompt = match self.sessions.agent.expand_input(&prompt) {
            Ok(prompt) => prompt,
            Err(error) => {
                self.notifications.error(error.to_string());
                return;
            }
        };
        // The session owns the conversation and writes its own system prompt, so
        // the app hands over the new words and nothing else.
        self.session.turns.push(Turn {
            prompt: submitted,
            ..Default::default()
        });
        self.session.follow_end = true;
        self.session.bump();
        self.spinner_frame = 0;

        self.sessions.dispatch_prompt(
            solaris_agent::RuntimeCommand::Prompt {
                text: prompt,
                mode: self.config.mode,
            },
            self.session.turns.len() - 1,
        );
    }

    pub(super) fn apply_effects(&mut self, effects: crate::tui::presentation::session::Effects) {
        if effects.follow_end {
            self.session.follow_end = true;
            self.spinner_frame = 0;
        }
        for (level, text) in effects.notices {
            match level {
                NoticeKind::Info => self.notifications.info(text),
                NoticeKind::Warning => self.notifications.warning(text),
                NoticeKind::Error => self.notifications.error(text),
            }
        }
    }
    #[cfg(test)]
    pub(super) fn apply_event(&mut self, event: AgentEvent) {
        let effects = self.session.projection.apply(event);
        self.apply_effects(effects);
    }
    pub(super) fn apply_runtime_event(&mut self, event: solaris_agent::RuntimeEvent) {
        match self.session.projection.apply_runtime(event) {
            Ok(effects) => self.apply_effects(effects),
            Err(error) => self.notifications.error(error),
        }
    }
}
