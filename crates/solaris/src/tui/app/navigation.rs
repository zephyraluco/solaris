use super::*;

impl App {
    fn restore_records(&mut self, records: Vec<solaris_agent::SessionRecord>) {
        let id = self.sessions.agent.snapshot().session_id;
        let (projection, effects) =
            crate::tui::presentation::session::SessionProjection::restore(id, records);
        self.session = SessionState::new();
        self.session.projection = projection;
        self.apply_effects(effects);
        self.transcript.invalidate();
    }

    pub(super) fn recover_projection(&mut self) {
        if let Some(journal) = self.sessions.agent.journal() {
            self.sessions.navigation.history(journal);
        } else {
            self.notifications.error(
                "temporary session history expired; cancel active work and use /clear to restart",
            );
        }
    }

    pub(super) fn open_sessions(&mut self, tree: bool) {
        let Some(journal) = self.sessions.agent.journal() else {
            self.notifications
                .warning("session navigation is unavailable in ephemeral mode");
            return;
        };
        self.sessions
            .navigation
            .list(journal, self.cwd.clone(), tree);
    }

    fn show_sessions(
        &mut self,
        sessions: Vec<solaris_agent::SessionInfo>,
        current: String,
        tree: bool,
    ) {
        let items = sessions
            .into_iter()
            .map(|session| {
                let title = session
                    .title
                    .as_deref()
                    .unwrap_or("New session")
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" ");
                let label = format!(
                    "{}{}",
                    if session.id == current {
                        "● "
                    } else if session.parent_id.is_some() {
                        "↳ "
                    } else {
                        "  "
                    },
                    title
                );
                let description = format!(
                    "{}{}",
                    session.status,
                    session
                        .parent_id
                        .map(|parent| format!(" · parent {parent}"))
                        .unwrap_or_default()
                );
                solaris_tui::components::select_list::SelectItem::new(session.id, label)
                    .description(description)
            })
            .collect();
        let dialog = SelectDialog::new(
            if tree { "Session tree" } else { "Sessions" },
            HINT_SELECT,
            items,
            &self.theme,
            self.dialog_tx.clone(),
            |id| DialogMessage::Session(id.to_string()),
        );
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered().width(SizeValue::Percent(85)),
        );
    }

    pub(super) fn switch_session(&mut self, id: &str, fork: bool) {
        if self.sessions.busy()
            || self.compact_rx.is_some()
            || self.resources.resource_rx.is_some()
            || self.connection.oauth.is_some()
            || self
                .sessions
                .runtime_pending
                .load(std::sync::atomic::Ordering::Acquire)
                > 0
        {
            self.notifications
                .warning("finish or cancel the active operation before changing sessions");
            return;
        }
        let Some(current) = self.sessions.agent.journal() else {
            self.notifications
                .warning("resume requires a saved session");
            return;
        };
        if current.id() == id && !fork {
            self.notifications.info("this session is already open");
            return;
        }
        self.sessions.navigation.open_with_overrides(
            crate::application::navigation::OpenSession {
                current,
                id: id.to_owned(),
                fork,
                cwd: self.cwd.clone(),
                runtime: self.sessions.runtime.clone(),
                config: self.config.clone(),
                auth: self.connection.auth.clone(),
                factory: self.connection.backend_factory.clone(),
                registry: self.registry.clone(),
                tools: self.tools.clone(),
                skills: self.resources.skills.clone(),
                prompt: self.sessions.agent.prompt_context(),
                approver: self.approver.clone(),
                execution: self.execution.clone(),
            },
            self.connection_overrides,
        );
    }

    pub(super) fn poll_navigation(&mut self) -> bool {
        use crate::application::navigation::NavigationResult;
        let Some(result) = self.sessions.navigation.poll() else {
            return false;
        };
        match result {
            Err(error) => self.notifications.error(error),
            Ok(NavigationResult::List {
                items,
                current,
                tree,
            }) => self.show_sessions(items, current, tree),
            Ok(NavigationResult::History { records, sequence }) => {
                self.restore_records(records);
                self.session.sequence = sequence;
                self.sessions.runtime_events =
                    crate::application::session::SessionFeed::new(&self.sessions.agent, sequence);
            }
            Ok(NavigationResult::Open {
                session,
                records,
                sequence,
                config,
                auth,
                choice,
                fork,
            }) => {
                if let Err(error) = session.activate() {
                    self.notifications.error(error);
                    return true;
                }
                self.connection.provider_config = choice.configuration;
                self.connection.model_source = choice.models;
                self.sessions.bind(session.clone(), sequence);
                self.config = config;
                self.connection.auth = auth;
                self.subagent_terminal = None;
                self.question = None;
                self.selection.borrow_mut().clear();
                self.notifications.clear();
                self.restore_records(records);
                self.session.sequence = sequence;
                self.refresh_models();
                self.notifications.info(format!(
                    "{} session {}",
                    if fork { "forked" } else { "resumed" },
                    session.snapshot().session_id
                ));
            }
        }
        true
    }
}
