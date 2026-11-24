use super::*;

impl App {
    pub(super) fn poll_io(&mut self, wait: bool) -> bool {
        use crate::desktop::Completion;
        let events = self.io.drain(wait);
        let dirty = !events.is_empty();
        for event in events {
            match event {
                Completion::Saved {
                    label,
                    result: Err(error),
                } => self
                    .notifications
                    .warning(format!("could not save {label}: {error}")),
                Completion::Saved { .. } => {}
                Completion::Auth { submitted, result } => match result {
                    Ok(saved) if self.connection.auth == submitted => {
                        self.connection.auth_base = saved.clone();
                        self.connection.auth = saved;
                    }
                    Ok(_) => {}
                    Err(error) => self
                        .notifications
                        .warning(format!("could not save credentials: {error}")),
                },
                Completion::Copied {
                    success,
                    failure,
                    ok,
                } => {
                    if ok {
                        self.notifications.info(success)
                    } else {
                        self.notifications.warning(failure)
                    }
                }
                Completion::Pasted { generation, text } => {
                    if generation != self.input_generation {
                        self.notifications
                            .warning("paste destination changed; paste again");
                    } else if let Some(text) = text {
                        self.insert_pasted(&text);
                    } else {
                        self.notifications.warning("clipboard is empty");
                    }
                }
            }
        }
        dirty
    }

    /// Persist global input history, best-effort.
    pub(super) fn persist_history(&mut self) {
        let Some(path) = self.history_path.clone() else {
            return;
        };

        let entries = self.session_page.bottom.editor().history().to_vec();
        let json = serde_json::to_string_pretty(&entries).map_err(|error| error.to_string());
        self.io.submit(move || crate::desktop::Completion::Saved {
            label: "input history",
            result: json.and_then(|json| write_json(&path, &json)),
        });
    }

    /// Remember the theme and the mode, best-effort.
    ///
    /// Only choices the user made reach here: a resolved default belongs to a
    /// flag, and writing one down would turn a fallback into a decision. The
    /// model is not written here — it is remembered with the provider it was
    /// chosen for, in `auth.json`.
    pub(super) fn persist_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else {
            return;
        };

        self.preferences.theme = Some(self.config.theme.clone());
        self.preferences.mode = Some(self.config.mode);

        let json = self
            .preferences
            .to_json()
            .map_err(|error| error.to_string());
        self.io.submit(move || crate::desktop::Completion::Saved {
            label: "preferences",
            result: json.and_then(|json| write_json(&path, &json)),
        });
    }

    /// Best-effort persistence of the companion's name.
    pub(super) fn persist_buddy(&mut self) {
        let Some(soul) = self.buddy.soul.clone() else {
            return;
        };
        let Some(path) = self.buddy_path.clone() else {
            return;
        };

        let json = soul.to_json().map_err(|error| error.to_string());
        self.io.submit(move || crate::desktop::Completion::Saved {
            label: "the companion",
            result: json.and_then(|json| write_json(&path, &json)),
        });
    }

    /// Everything the welcome box shows, rebuilt per frame so the companion can
    /// fidget while the transcript is empty.
    pub(crate) fn welcome_data(&self) -> WelcomeData {
        WelcomeData {
            app_name: APP_NAME.to_string(),
            version: self.version.clone(),
            greeting: self.greeting.clone(),
            hint: "/help for commands  ·  ? for shortcuts".to_string(),
            mascot: buddy::render_lines(&self.buddy.bones, self.buddy_step),
            tip: self.tip.clone(),
        }
    }

    // ------------------------------------------------------------------ render
}
