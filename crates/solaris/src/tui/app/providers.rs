use super::*;

impl App {
    /// Provider the current session was resolved to, when one was.
    ///
    /// The model picker lists this provider's models, so it needs the id rather
    /// than the backend's display label.
    pub(super) fn provider_id(&self) -> Option<String> {
        self.sessions.agent.snapshot().provider_id
    }

    /// Resolve the backend again after the credentials or the model changed.
    ///
    /// Returns where the credential came from, so a caller can explain a choice
    /// that did not land where the user pointed.
    pub(super) fn rebuild_backend(&mut self) -> CredentialSource {
        let choice = resolve_backend(
            &self.connection.backend_factory,
            &self.connection.auth,
            &mut self.config,
        );
        self.connection.provider_config = Arc::clone(&choice.configuration);
        let source = choice.source;
        self.refresh_transport(choice);
        source
    }

    /// Ask the backend which models the provider offers, in the background.
    ///
    /// Discovery never blocks the UI. ChatGPT uses only the remote catalog;
    /// other providers may still use their bundled catalog. Replacing the
    /// receiver prevents a stale answer from an earlier request being adopted.
    pub(super) fn refresh_models(&mut self) {
        self.connection.refresh_models();
    }

    fn refresh_transport(&mut self, choice: BackendChoice) {
        self.connection.model_source = Arc::clone(&choice.models);
        let factory = Arc::clone(&self.connection.backend_factory);
        let auth = self.connection.auth.clone();
        self.runtime_command(solaris_agent::RuntimeCommand::ReplaceConnection {
            connection: crate::agent_backend(choice),
            children: Some(Arc::new(move |model| {
                Ok(crate::agent_backend(factory(&auth, model)))
            })),
        });
    }

    /// Adopt the model list the provider reported.
    pub(super) fn on_models_discovered(&mut self, answer: Result<Vec<String>, String>) {
        let catalog_notice = self.connection.model_source.notice();
        match answer {
            Ok(models) if !models.is_empty() => {
                // The provider's own list is the one that knows which models
                // this credential can actually reach, so an unnamed model takes
                // the first of them — the same substitution the catalogue gets.
                let adopt = self.config.model.is_empty()
                    || (self.uses_chatgpt() && !models.contains(&self.config.model));
                self.connection.models_error = None;
                self.connection.discovered = models;

                if adopt && let Some(first) = self.connection.discovered.first().cloned() {
                    self.config.model = first;
                    if self.uses_chatgpt() {
                        let choice = resolve_backend(
                            &self.connection.backend_factory,
                            &self.connection.auth,
                            &mut self.config,
                        );
                        self.refresh_transport(choice);
                    } else {
                        self.rebuild_backend();
                    }
                }
                let count = self.connection.discovered.len();
                if self.connect_is_choosing_a_model() {
                    // The list is on screen; pointing at `/model` would only
                    // send the user somewhere they already are.
                    self.notifications
                        .info(format!("{count} model(s) reported"));
                } else {
                    self.notifications
                        .info(format!("{count} model(s) available — /model to choose"));
                }
            }
            Ok(_) => {
                if self.uses_chatgpt() {
                    self.connection.discovered.clear();
                    self.connection.models_error = Some(
                        "OpenAI returned no visible models for this account. Press r to retry."
                            .into(),
                    );
                }
                // An empty list is an answer rather than a failure — the
                // provider says this credential reaches no models — but with no
                // catalogue to fall back on (a gateway, a custom endpoint) the
                // picker would come up empty with no explanation of why.
                if self.model_choices().is_empty() && self.provider_id().is_some() {
                    self.notifications
                        .warning("the provider listed no models — name one with /model <name>");
                }
            }
            Err(message) => {
                if self.uses_chatgpt() {
                    self.connection.discovered.clear();
                    self.connection.models_error = Some(format!(
                        "Could not list OpenAI models: {message}. Press r to retry."
                    ));
                }
                // Only worth saying when there was no catalogue to fall back on;
                // otherwise a provider that will not answer is not the user's
                // problem to solve.
                if self.model_choices().is_empty() && self.provider_id().is_some() {
                    self.notifications.warning(format!(
                        "could not list models: {message} — use /model <name>"
                    ));
                }
            }
        }

        self.fill_in_models();
        if let Some(notice) = catalog_notice {
            self.notifications.warning(notice);
        }
    }

    /// Whether `/connect` is sitting on the step that picks a model.
    fn connect_is_choosing_a_model(&self) -> bool {
        self.inline
            .as_ref()
            .and_then(Inline::step)
            .is_some_and(|step| step == ConnectStep::Model)
    }

    /// Put the answer to model discovery into the wizard's model step.
    ///
    /// `/connect` opens that step the moment the credential lands, before the
    /// provider has said what it offers — a gateway has no catalogue to hand it
    /// over from — so the list can only arrive this way. A provider that names
    /// nothing and has no catalogue behind it leaves the step with nothing to
    /// pick, and the notice the answer produced already says how to name a model
    /// by hand.
    fn fill_in_models(&mut self) {
        if self.uses_chatgpt() {
            let models = self.model_choices();
            let status = self.model_discovery_status();
            let current = self.config.model.clone();
            match self.inline.as_mut() {
                Some(Inline::Connect(flow)) if flow.step() == ConnectStep::Model => {
                    let empty = models.is_empty();
                    flow.enter_models(models, Some(&current));
                    if empty {
                        flow.model_status(status);
                    }
                }
                Some(Inline::Model(picker)) => {
                    *picker = if models.is_empty() {
                        ModelPicker::new(&self.theme, models, None).with_status(status)
                    } else {
                        ModelPicker::new(&self.theme, models, Some(&current))
                    };
                }
                _ => {}
            }
            return;
        }
        if !self.connect_is_choosing_a_model() {
            return;
        }

        let models = self.model_choices();
        if models.is_empty() {
            self.close_inline();
            return;
        }

        let current = self.config.model.clone();
        if let Some(flow) = self.inline.as_mut() {
            flow.enter_models(models, Some(&current));
        }
    }

    /// The models the picker should offer.
    ///
    /// What the provider reported comes first and is annotated from the
    /// catalogue where the model is known, which keeps descriptions, prices and
    /// context windows for the models we ship while leaving a gateway's own
    /// list intact. With nothing reported, the catalogue is the whole answer.
    pub(super) fn model_choices(&self) -> Vec<SelectItem> {
        let catalogue = self
            .provider_id()
            .map(|id| self.connection.provider_config.models(&id))
            .unwrap_or_default();
        if self.connection.discovered.is_empty() {
            if self.uses_chatgpt() {
                return Vec::new();
            }
            return catalogue
                .into_iter()
                .map(|(id, name)| SelectItem::new(id.clone(), id).description(name))
                .collect();
        }
        self.connection
            .discovered
            .iter()
            .map(|id| {
                let description = catalogue
                    .iter()
                    .find(|(model, _)| model == id)
                    .map_or("", |(_, name)| name.as_str());
                SelectItem::new(id.as_str(), id.as_str()).description(description)
            })
            .collect()
    }

    /// Open the model picker in the prompt region — the `/connect` look rather
    /// than a modal — with the active model highlighted.
    ///
    /// The list is whatever the connected provider offers, since no other
    /// provider's models can be asked for. With nothing connected there is
    /// nothing to choose between, so the user is told what to do instead.
    pub(super) fn open_model_picker(&mut self) {
        if self.uses_chatgpt() {
            self.refresh_models();
            self.inline = Some(Inline::Model(
                ModelPicker::new(&self.theme, Vec::new(), None)
                    .with_status(self.model_discovery_status()),
            ));
            self.notifications.clear();
            return;
        }
        let models = self.model_choices();

        if models.is_empty() {
            self.notifications
                .warning("no models to choose from — run /connect, or name one with /model <name>");
            return;
        }

        let current = self.config.model.clone();
        let picker = ModelPicker::new(&self.theme, models, Some(&current));
        self.inline = Some(Inline::Model(picker));
        self.notifications.clear();
    }

    fn uses_chatgpt(&self) -> bool {
        self.connection.uses_chatgpt()
    }

    fn model_discovery_status(&self) -> String {
        self.connection
            .models_error
            .clone()
            .unwrap_or_else(|| "Loading OpenAI account models…".into())
    }

    fn begin_openai_login(&mut self) {
        if let Some(Inline::Connect(flow)) = self.inline.as_mut() {
            flow.enter_browser_auth("Starting browser authorization…".into());
        }
        self.connection.begin_login();
    }

    pub(super) fn poll_oauth(&mut self) -> bool {
        let mut updated = false;
        if let Some(urls) = self
            .connection
            .oauth
            .as_mut()
            .and_then(|pending| pending.url_rx.as_mut())
        {
            while let Ok(url) = urls.try_recv() {
                if let Some(Inline::Connect(flow)) = self.inline.as_mut() {
                    flow.enter_browser_auth(url);
                    updated = true;
                }
            }
        }
        let result = match self
            .connection
            .oauth
            .as_mut()
            .map(|pending| pending.rx.try_recv())
        {
            Some(Ok(result)) => result,
            Some(Err(oneshot::error::TryRecvError::Closed)) => {
                Err("OpenAI authorization was interrupted".into())
            }
            _ => return updated,
        };
        let Some(_pending) = self.connection.oauth.take() else {
            return false;
        };
        match result {
            Ok(credential) => {
                self.finish_connect("openai".into(), "OpenAI".into(), Some(credential))
            }
            Err(error) => {
                self.notifications.error(error);
                self.close_inline();
            }
        }
        true
    }

    /// Open the `/connect` wizard in the prompt region.
    pub(super) fn open_connect(&mut self) {
        self.connection.oauth = None;
        self.inline = Some(Inline::Connect(ConnectFlow::new(&self.theme)));
        self.notifications.clear();
    }

    /// Drop the wizard.
    pub(super) fn close_inline(&mut self) {
        self.input_generation = self.input_generation.wrapping_add(1);
        // Credential refresh belongs to the pending request, not to the wizard.
        if self.connection.oauth.is_some() {
            self.connection.oauth = None;
        }
        self.inline = None;
    }

    pub(super) fn handle_inline_key(&mut self, key: KeyEvent) {
        let outcome = match self.inline.as_mut() {
            Some(inline) => inline.on_key(key),
            None => return,
        };
        match outcome {
            InlineOutcome::Connect(outcome) => self.apply_connect_outcome(outcome),
            InlineOutcome::Model(outcome) => self.apply_model_outcome(outcome),
        }
    }

    pub(super) fn apply_connect_outcome(&mut self, outcome: ConnectOutcome) {
        match outcome {
            ConnectOutcome::Handled => {}
            ConnectOutcome::Closed => self.close_inline(),
            ConnectOutcome::ProviderPicked { id, name } => self.begin_provider_setup(&id, &name),
            ConnectOutcome::Submit(submit) => self.apply_connect_submit(submit),
            ConnectOutcome::CopyBrowserUrl(url) => {
                self.copy_text(
                    &url,
                    "OpenAI authorization URL copied".into(),
                    "Could not copy URL; open the displayed URL manually",
                );
            }
            ConnectOutcome::RetryModels => {
                self.refresh_models();
                self.fill_in_models();
            }
            ConnectOutcome::ModelPicked { model_id } => {
                self.set_model(&model_id);
                self.close_inline();
            }
        }
    }

    /// Apply what the `/model` picker decided.
    pub(super) fn apply_model_outcome(&mut self, outcome: ModelOutcome) {
        match outcome {
            ModelOutcome::Handled => {}
            ModelOutcome::Closed => self.close_inline(),
            ModelOutcome::Retry => {
                self.refresh_models();
                self.fill_in_models();
            }
            ModelOutcome::Picked { model_id } => {
                self.set_model(&model_id);
                self.close_inline();
            }
        }
    }

    /// The secret already stored for an API-key provider, so `/connect` shows
    /// it (masked) instead of asking for it again.
    fn stored_api_key(&self, provider_id: &str) -> Option<String> {
        match self.connection.auth.credential(provider_id) {
            Some(Credential::ApiKey { key }) => Some(key.clone()),
            _ => None,
        }
    }

    /// The gateway URL and key already stored for an endpoint provider.
    fn stored_endpoint(&self, provider_id: &str) -> (Option<String>, Option<String>) {
        match self.connection.auth.credential(provider_id) {
            Some(Credential::Endpoint { base_url, api_key }) => {
                (Some(base_url.clone()), Some(api_key.clone()))
            }
            _ => (None, None),
        }
    }

    /// Route a picked provider to the step its auth kind needs.
    fn begin_provider_setup(&mut self, provider_id: &str, name: &str) {
        let Some(spec) = solaris_provider::provider_spec(provider_id) else {
            self.notifications
                .error(format!("unknown provider {provider_id}"));
            self.close_inline();
            return;
        };

        let id = provider_id.to_string();
        let provider_name = name.to_string();

        match spec.auth {
            AuthKind::ChatGpt => self.begin_openai_login(),
            // Nothing to collect: activate it straight away.
            AuthKind::Local => {
                self.apply_connect_outcome(ConnectOutcome::Submit(ConnectSubmit::Local {
                    provider_id: id,
                    provider_name,
                }));
            }
            AuthKind::ApiKey => {
                let current = self.stored_api_key(provider_id);
                if let Some(flow) = self.inline.as_mut() {
                    flow.enter_api_key(id, provider_name, current);
                }
            }
            AuthKind::ApiKeyWithUrl => {
                let (url, key) = self.stored_endpoint(provider_id);
                if let Some(flow) = self.inline.as_mut() {
                    flow.enter_custom_provider(id, provider_name, url, key);
                }
            }
        }
    }

    /// Apply a confirmed credential, then hand over to the model picker.
    fn apply_connect_submit(&mut self, submit: ConnectSubmit) {
        let (provider_id, provider_name, credential) = match submit {
            ConnectSubmit::Local {
                provider_id,
                provider_name,
            } => (provider_id, provider_name, None),
            ConnectSubmit::ApiKey {
                provider_id,
                provider_name,
                key,
            } => (provider_id, provider_name, Some(Credential::ApiKey { key })),
            ConnectSubmit::CustomProvider {
                provider_id,
                provider_name,
                base_url,
                api_key,
            } => (
                provider_id,
                provider_name,
                Some(Credential::Endpoint { base_url, api_key }),
            ),
        };

        self.finish_connect(provider_id, provider_name, credential);
    }

    pub(super) fn finish_connect(
        &mut self,
        provider_id: String,
        provider_name: String,
        credential: Option<Credential>,
    ) {
        if let Some(credential) = credential {
            self.connection.auth.store(provider_id.clone(), credential);
        }
        self.connection.auth.activate(provider_id.clone());
        self.persist_auth();

        // Connecting is what resolves a real client, so the backend is rebuilt
        // before the model step — and the new provider is asked what it offers.
        match self.rebuild_backend() {
            CredentialSource::Missing => self
                .notifications
                .warning("no usable credential — turns will say so until /connect succeeds"),
            CredentialSource::NoModel => self.notifications.info(format!(
                "connected to {provider_name} — no model named yet, use /model <name>"
            )),
            _ => self
                .notifications
                .info(format!("connected to {provider_name}")),
        }
        self.refresh_models();

        // Continue into the model picker, the way claurst's wizard does. The
        // list can still be empty — a gateway or a custom endpoint publishes its
        // models over the wire rather than in the catalogue, and discovery is a
        // background task — so the step opens anyway and is filled in when the
        // answer lands. With nothing to offer and nothing on its way there is no
        // step to take, so the wizard closes rather than parking the user on an
        // empty screen.
        let models: Vec<SelectItem> = if matches!(
            self.connection.auth.credential(&provider_id),
            Some(Credential::ChatGpt { .. })
        ) {
            Vec::new()
        } else {
            self.connection
                .provider_config
                .models(&provider_id)
                .into_iter()
                .map(|(id, name)| SelectItem::new(id.clone(), id).description(name))
                .collect()
        };
        let pending = self.connection.models_rx.is_some();
        let current = self.config.model.clone();
        let chatgpt = self.uses_chatgpt();

        match self.inline.as_mut() {
            Some(flow) if !models.is_empty() || pending || chatgpt => {
                flow.enter_models(models, Some(&current));
            }
            _ => self.close_inline(),
        }
        if chatgpt {
            self.fill_in_models();
        }
    }

    /// Best-effort persistence of the credential store.
    pub(super) fn persist_auth(&mut self) {
        let Some(path) = self.connection.auth_path.clone() else {
            return;
        };

        let submitted = self.connection.auth.clone();
        let base = self.connection.auth_base.clone();
        self.io.submit(move || {
            let result = submitted
                .save_changes(&base, &path)
                .map_err(|error| error.to_string());
            crate::desktop::Completion::Auth { submitted, result }
        });
    }
}
