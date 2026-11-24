use super::*;
use crate::application::connection::OAuthPending;
use crossterm::event::MouseButton;
use solaris_agent::AgentEventStream;
use solaris_agent::test_support::TurnRequest;
use solaris_agent::{BackendError, SkillRoot, SkillSource, ToolCall, ToolResult, Usage};
use solaris_provider::BackendOptions;
use std::cell::RefCell;
use std::time::Duration;

#[tokio::test]
async fn switching_and_forking_sessions_replays_saved_turns_without_running_tools() {
    let directory = tempfile::tempdir().expect("directory");
    let store = solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite"))
        .expect("store");
    let cwd = std::env::current_dir()
        .expect("cwd")
        .canonicalize()
        .expect("canonical cwd");
    let first = store.create(&cwd, "one", None).expect("first");
    let second = store.create(&cwd, "two", None).expect("second");
    let id = second.id().to_string();
    second
        .append(&solaris_agent::SessionRecord::Prompt {
            text: "saved question".into(),
        })
        .expect("prompt");
    second
        .append(&solaris_agent::SessionRecord::Event {
            event: AgentEvent::TextDelta("saved answer".into()),
        })
        .expect("text");
    second
        .append(&solaris_agent::SessionRecord::Event {
            event: AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            },
        })
        .expect("complete");
    drop(second);
    let mut options = app_options();
    let first_id = first.id().to_owned();
    options.journal = Some(first);
    let mut app = App::new(options);
    app.switch_session(&id, false);
    for _ in 0..1000 {
        app.tick();
        if !app.sessions.navigation.is_pending() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(app.sessions.agent.journal().expect("journal").id(), id);
    assert_eq!(app.session.turns[0].reply, "saved answer");
    // Fork a session other than the visible one, including when the runtime
    // still owns the source lease from an earlier visit.
    app.switch_session(&first_id, false);
    for _ in 0..1000 {
        app.tick();
        if !app.sessions.navigation.is_pending() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    assert_eq!(
        app.sessions.agent.journal().expect("journal").id(),
        first_id
    );
    app.switch_session(&id, true);
    for _ in 0..1000 {
        app.tick();
        if !app.sessions.navigation.is_pending() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    let fork = app.sessions.agent.journal().expect("fork");
    assert_ne!(fork.id(), id);
    assert_eq!(app.session.turns[0].reply, "saved answer");
    assert!(
        store
            .list(&cwd)
            .expect("sessions")
            .iter()
            .any(|info| info.id == fork.id() && info.parent_id.as_deref() == Some(id.as_str()))
    );
}

#[tokio::test]
async fn switching_to_a_loaded_session_applies_the_current_model_override() {
    let directory = tempfile::tempdir().expect("directory");
    let store = solaris_agent::SessionStore::open(&directory.path().join("sessions.sqlite"))
        .expect("store");
    let cwd = std::env::current_dir().unwrap().canonicalize().unwrap();
    let first = store.create(&cwd, "model-a", None).unwrap();
    let first_id = first.id().to_owned();
    let second_id = store
        .create(&cwd, "saved-model", None)
        .unwrap()
        .id()
        .to_owned();

    let mut options = app_options();
    options.config.model = "model-a".into();
    options.journal = Some(first);
    let mut app = App::new(options).with_connection_overrides(
        crate::application::history::ConnectionOverrides {
            model: true,
            provider: false,
        },
    );

    app.switch_session(&second_id, false);
    tokio::time::timeout(Duration::from_secs(3), async {
        while app.sessions.navigation.is_pending() {
            app.tick();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(app.sessions.agent.snapshot().session_id, second_id);
    app.set_model("model-b");
    tokio::time::timeout(Duration::from_secs(3), async {
        while app.sessions.busy() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(app.sessions.agent.snapshot().model, "model-b");

    app.switch_session(&first_id, false);
    tokio::time::timeout(Duration::from_secs(3), async {
        while app.sessions.navigation.is_pending() {
            app.tick();
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    let snapshot = app.sessions.agent.snapshot();
    assert_eq!(snapshot.session_id, first_id);
    assert_eq!(app.model(), "model-b");
    assert_eq!(snapshot.model, "model-b");
}

/// A backend that answers with a prompt-echoing markdown reply, so the app's
/// own behaviour can be driven without a provider and without a network.
struct FakeBackend {
    /// What `models()` reports. `Err` stands for a provider that will not say.
    models: Result<Vec<String>, String>,
}

#[async_trait::async_trait]
impl ModelConnector for FakeBackend {
    async fn model_client(
        self: std::sync::Arc<Self>,
    ) -> Result<solaris_agent::ModelClient, solaris_agent::BackendError> {
        Ok(solaris_agent::test_support::client(
            self.capabilities(),
            move |request| {
                let fixture = std::sync::Arc::clone(&self);
                async move { fixture.respond(request).await }
            },
        ))
    }

    fn label(&self) -> &str {
        "fake"
    }
}
#[async_trait::async_trait]
impl solaris_provider::ModelSource for FakeBackend {
    async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.models.clone().map_err(BackendError::new)
    }
}
impl FakeBackend {
    async fn respond(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
        let reply = format!(
            "You said: {}\n\n## Reply\n\n- streamed chunk by chunk\n",
            request.last_user_text()
        );
        let events = vec![
            AgentEvent::ThinkingDelta("thinking ".to_string()),
            AgentEvent::Status("composing reply".to_string()),
            AgentEvent::TextDelta(reply),
            AgentEvent::TurnComplete {
                usage: Usage::new(4, 2),
                cost_usd: 0.0,
            },
        ];
        Ok(Box::pin(futures::stream::iter(events)))
    }
}

/// The fake, type-erased the way `AppOptions` wants it. It lists no models,
/// which is what a backend with nothing to say looks like.
fn fake_backend() -> Arc<FakeBackend> {
    Arc::new(FakeBackend {
        models: Err("this backend cannot list models".to_string()),
    })
}

/// The same, reporting `models` when asked.
fn fake_backend_reporting(models: &[&str]) -> Arc<FakeBackend> {
    Arc::new(FakeBackend {
        models: Ok(models.iter().map(|id| (*id).to_string()).collect()),
    })
}

/// A session resolved as `provider` while answering from the fake backend.
///
/// The provider id is what the model picker reads its catalogue with, so a
/// test that needs one has to resolve a backend that carries it.
fn fake_session(provider: &'static str) -> Arc<solaris_agent::SessionRuntime> {
    Arc::new(solaris_agent::SessionRuntime::new(Agent::new(
        crate::agent_backend(BackendChoice {
            models: Arc::new(solaris_provider::UnavailableModels),
            configuration: Default::default(),
            backend: fake_backend(),
            provider_id: Some(provider.into()),
            base_url: None,
            model: String::new(),
            source: CredentialSource::None,
        }),
        Arc::new(ToolRegistry::builtin()),
        AgentOptions::default(),
    )))
}

fn app_options() -> AppOptions {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    AppOptions::new(backend, Config::default(), quit, queue, flag)
}

fn app() -> App {
    App::new(app_options())
}

/// An app pinned to `backend` that is allowed to ask it for its models.
fn app_with_backend(backend: Arc<FakeBackend>) -> App {
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let models: Arc<dyn solaris_provider::ModelSource> = backend.clone();
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    let factory = options.backend_factory.clone();
    options.backend_factory = Arc::new(move |auth, model| {
        let mut choice = factory(auth, model);
        choice.models = Arc::clone(&models);
        choice
    });
    options.discover_models = true;
    App::new(options)
}

/// An app that answers model discovery with `models` while resolving as
/// `provider`. A gateway is exactly this: no catalogue, so its list can
/// only come from what the wire says.
fn app_reporting_models_as(provider: &'static str, models: &[&str]) -> App {
    let pinned = fake_backend_reporting(models);
    let resolved = Arc::clone(&pinned);
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(pinned, Config::default(), quit, queue, flag);
    options.discover_models = true;
    options.backend_factory = Arc::new(move |_, model| BackendChoice {
        models: resolved.clone(),
        configuration: Default::default(),
        backend: resolved.clone(),
        provider_id: Some(provider.into()),
        base_url: None,
        model: model.to_string(),
        source: CredentialSource::None,
    });
    App::new(options)
}

/// An app that looks connected to `provider` while answering from the fake,
/// so the catalogue has a provider to fall back to.
fn app_connected_to_but_faked(provider: &'static str) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.discover_models = true;
    options.backend_factory = Arc::new(move |_, model| BackendChoice {
        models: Arc::new(solaris_provider::UnavailableModels),
        configuration: Default::default(),
        backend: fake_backend(),
        provider_id: Some(provider.into()),
        base_url: None,
        model: model.to_string(),
        source: CredentialSource::None,
    });
    App::new(options)
}

/// Tick until the background model discovery has finished, or give up.
async fn settle_discovery(app: &mut App) {
    for _ in 0..500 {
        app.tick();
        if app.connection.models_rx.is_none() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// An app that persists credentials to `path`.
fn app_with_auth_path(path: std::path::PathBuf) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.auth_path = Some(path);
    App::new(options)
}

/// An app that persists recent activity to `path`.
fn app_with_recent_path(path: std::path::PathBuf) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.recent_path = Some(path);
    App::new(options)
}

/// An app that persists its preferences to `path`.
fn app_with_settings_path(path: std::path::PathBuf) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.settings_path = Some(path);
    App::new(options)
}

#[test]
fn late_clipboard_result_cannot_paste_into_a_changed_destination() {
    let mut app = app();
    let generation = app.input_generation;
    app.io.submit(move || crate::desktop::Completion::Pasted {
        generation,
        text: Some("secret".into()),
    });
    app.handle_key(key(KeyCode::Char('x')));
    app.poll_io(true);
    assert!(!rendered_text(&mut app, 80, 24).contains("secret"));
    assert!(rendered_text(&mut app, 80, 24).contains("paste destination changed"));
}

#[tokio::test]
async fn producer_eof_waits_for_the_bounded_transcript_backlog() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    let (events, rx) = tokio::sync::mpsc::channel(512);
    app.sessions.runtime_events = crate::application::session::SessionFeed::from_receiver(rx);
    for sequence in 1..=200 {
        events
            .try_send(Ok(solaris_agent::RuntimeEvent {
                prompt: None,
                completion: None,
                session_id: "session".into(),
                sequence: Some(sequence),
                task_id: Some(1),
                event: AgentEvent::TextDelta("x".into()),
            }))
            .unwrap();
    }
    events
        .try_send(Ok(solaris_agent::RuntimeEvent {
            prompt: None,
            completion: None,
            session_id: "session".into(),
            sequence: Some(201),
            task_id: Some(1),
            event: AgentEvent::TurnComplete {
                usage: Default::default(),
                cost_usd: 0.0,
            },
        }))
        .unwrap();
    drop(events);
    app.tick();
    assert!(!app.session.turns[0].complete);
    for _ in 0..10 {
        app.tick();
    }
    assert_eq!(app.session.turns.len(), 1);
    assert_eq!(app.session.turns[0].reply.len(), 200);
    assert!(app.session.turns[0].complete);
}

#[tokio::test]
async fn a_large_event_backlog_is_not_drained_in_one_tick() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    let (events, rx) = tokio::sync::mpsc::channel(1024);
    app.sessions.runtime_events = crate::application::session::SessionFeed::from_receiver(rx);
    for sequence in 1..=1000 {
        events
            .try_send(Ok(solaris_agent::RuntimeEvent {
                prompt: None,
                completion: None,
                session_id: "session".into(),
                sequence: Some(sequence),
                task_id: Some(1),
                event: AgentEvent::TextDelta("x".into()),
            }))
            .unwrap();
    }
    app.tick();
    assert!(app.session.turns[0].reply.len() <= 128);
    app.handle_key(key(KeyCode::Char('a')));
}

#[test]
fn the_theme_the_mode_and_the_model_are_all_remembered() {
    let dir = std::env::temp_dir().join(format!("solaris-settings-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("a temp dir");
    let settings_path = dir.join("settings.json");
    let auth_path = dir.join("auth.json");

    let mut app = app_with_settings_path(settings_path.clone());
    app.connection.auth_path = Some(auth_path.clone());
    app.sessions.agent = fake_session("opencode");
    app.set_model("minimax-m3");
    app.toggle_mode();
    app.set_theme("light");

    // Theme and mode are session preferences…
    app.poll_io(true);
    let text = std::fs::read_to_string(&settings_path).expect("settings were written");
    let stored = Preferences::from_json(&text).expect("decodes");
    assert_eq!(stored.theme.as_deref(), Some("light"));
    assert_eq!(stored.mode, Some(Mode::Plan));

    // …while the model is remembered beside the provider that offers it.
    app.poll_io(true);
    let text = std::fs::read_to_string(&auth_path).expect("credentials were written");
    let auth = AuthStore::from_json(&text).expect("decodes");
    assert_eq!(auth.active_provider(), Some("opencode"));
    assert_eq!(auth.active_model(), Some("minimax-m3"));

    let _ = std::fs::remove_dir_all(&dir);
}

/// An app whose companion is named and persisted to `path`.
fn app_with_buddy_path(path: std::path::PathBuf) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.buddy_path = Some(path);
    App::new(options)
}

/// An app that resolves backends from credentials, the way the binary does.
fn app_with_real_backends(auth: AuthStore) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.auth = auth;
    options.backend_factory = Arc::new(|auth: &AuthStore, model: &str| {
        solaris_provider::choose_backend(
            auth,
            model,
            BackendOptions {
                environment: solaris_provider::empty_environment(),
                ..Default::default()
            },
        )
    });
    App::new(options)
}

/// An app with one provider connected by API key, so a provider's own model
/// list and the resolved backend are both available.
fn app_connected_to(provider: &str) -> App {
    let mut auth = AuthStore::new();
    auth.store(
        provider,
        Credential::ApiKey {
            key: "sk-test".to_string(),
        },
    );
    auth.activate(provider);
    app_with_real_backends(auth)
}

/// The bottom line of a rendered frame.
fn footer_text(app: &mut App) -> String {
    let area = Rect::new(0, 0, 100, 8);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);

    (0..area.width)
        .map(|x| buf[(x, area.height - 1)].symbol())
        .collect()
}

/// Render the app into a buffer and return the visible text.
fn rendered_text(app: &mut App, width: u16, height: u16) -> String {
    let area = Rect::new(0, 0, width, height);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);

    let mut out = String::new();
    for y in 0..area.height {
        for x in 0..area.width {
            out.push_str(buf[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}

/// Type a provider API key into the wizard and confirm it.
fn connect_with_api_key(app: &mut App, key_text: &str) {
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Enter)); // row 1 — Anthropic
    for ch in key_text.chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));
}

/// Walk the wizard down the gateway path — row 5, New API — and confirm the
/// URL, which is the only thing it asks for.
fn connect_with_a_gateway_url(app: &mut App) {
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Char('5')));
    for ch in "https://gateway.test/v1".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::empty())
}

fn click_child_tool(app: &mut App, label: &str, column: u16) {
    let text = rendered_text(app, 80, 14);
    let row = text
        .lines()
        .position(|line| line.contains(label))
        .unwrap_or_else(|| panic!("missing {label}: {text}")) as u16;
    for kind in [
        MouseEventKind::Down(MouseButton::Left),
        MouseEventKind::Up(MouseButton::Left),
    ] {
        app.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        });
    }
}

#[test]
fn the_app_opens_straight_into_the_session() {
    // There is no welcome screen: the first frame is the terminal, and
    // typing lands in the prompt without any key press to get there.
    let mut app = app();
    let text = rendered_text(&mut app, 80, 20);

    assert!(text.contains("Ask anything"), "{text}");

    for ch in "hi".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert_eq!(app.editor.text(), "hi");
}

#[test]
fn the_empty_transcript_shows_the_welcome_box() {
    let mut app = app();
    let text = rendered_text(&mut app, 90, 26);

    assert!(text.contains("solaris v"), "title missing:\n{text}");
    assert!(text.contains("Welcome back!"), "greeting missing:\n{text}");
    assert!(
        text.contains("Tips for getting started"),
        "tip heading missing:\n{text}"
    );
    assert!(
        text.contains("No recent activity"),
        "activity placeholder missing:\n{text}"
    );
    assert!(text.contains('╭') && text.contains('╯'), "{text}");

    // The companion is drawn from its own sprite table.
    let sprite = buddy::render_lines(&app.buddy.bones, app.buddy_step);
    for row in sprite {
        assert!(text.contains(row.trim_end()), "sprite row missing: {row:?}");
    }
}

#[test]
fn the_companion_fidgets_between_animation_frames() {
    let mut app = app();
    app.buddy_step = 0;
    let first = rendered_text(&mut app, 90, 26);

    // Frame 2 is reached at step 8 of the idle cycle.
    app.buddy_step = 8;
    let second = rendered_text(&mut app, 90, 26);

    assert_ne!(first, second, "the companion never moves");

    // Once the conversation starts there is nothing left to animate.
    app.session.turns.push(Turn {
        prompt: "hi".into(),
        reply: "there".into(),
        complete: true,
        ..Default::default()
    });
    app.buddy_step = 0;
    let first = rendered_text(&mut app, 90, 26);
    app.buddy_step = 8;
    let second = rendered_text(&mut app, 90, 26);
    assert_eq!(first, second, "the box is gone but the frame still changes");
}

#[test]
fn the_welcome_box_scrolls_away_with_the_first_turn() {
    let mut app = app();
    assert!(rendered_text(&mut app, 90, 26).contains("Welcome back!"));

    app.session.turns.push(Turn {
        prompt: "hi".into(),
        reply: "there".into(),
        complete: true,
        ..Default::default()
    });
    let text = rendered_text(&mut app, 90, 26);
    assert!(!text.contains("Welcome back!"), "{text}");
    assert!(text.contains("there"), "{text}");
}

#[tokio::test]
async fn submitting_a_prompt_records_recent_activity() {
    let dir = std::env::temp_dir().join("solaris-recent-test");
    let path = dir.join("recent.json");
    let _ = std::fs::remove_file(&path);

    let mut app = app_with_recent_path(path.clone());
    app.submit("  fix the parser\nand the lexer  ".to_string());

    assert_eq!(app.recent.len(), 1);
    assert_eq!(app.recent.entries()[0].label, "fix the parser");

    app.poll_io(true);
    let saved = std::fs::read_to_string(&path).expect("recent file");
    let reloaded = RecentActivity::from_json(&saved).expect("valid json");
    assert_eq!(reloaded.entries()[0].label, "fix the parser");

    // Slash commands are not activity.
    app.submit("/stats".to_string());
    assert_eq!(app.recent.len(), 1);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_recent_list_appears_in_the_welcome_box() {
    let mut app = app();
    app.recent.record("fix the parser", recent::now_ms());
    app.recent.record("add the welcome box", recent::now_ms());

    let text = rendered_text(&mut app, 90, 26);
    assert!(text.contains("Recent activity"), "{text}");
    assert!(text.contains("fix the parser"), "{text}");
    assert!(text.contains("just now"), "{text}");
    assert!(!text.contains("No recent activity"), "{text}");
}

#[test]
fn the_buddy_command_shows_the_card() {
    let mut app = app();
    app.submit("/buddy".to_string());

    let queued = app.overlay_queue.borrow();
    assert_eq!(queued.len(), 1, "the card did not open");
}

#[test]
fn the_buddy_command_names_the_companion() {
    let dir = std::env::temp_dir().join("solaris-buddy-test");
    let path = dir.join("companion.json");
    let _ = std::fs::remove_file(&path);

    let mut app = app_with_buddy_path(path.clone());
    let species = app.buddy.bones.species.as_str().to_string();
    app.submit("/buddy name Pip".to_string());

    assert_eq!(app.buddy.display_name(), "Pip");
    assert_eq!(app.buddy.bones.species.as_str(), species);

    app.poll_io(true);
    let saved = std::fs::read_to_string(&path).expect("companion file");
    let soul = Soul::from_json(&saved).expect("valid json");
    assert_eq!(soul.name, "Pip");

    // The welcome box greets with the new name.
    let text = rendered_text(&mut app, 90, 26);
    assert!(text.contains("Pip"), "{text}");

    // An empty name is rejected instead of clearing it.
    app.submit("/buddy name".to_string());
    assert_eq!(app.buddy.display_name(), "Pip");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn tab_toggles_mode_when_no_completion_is_open() {
    let mut app = app();
    assert_eq!(app.mode(), Mode::Default);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.mode(), Mode::Plan);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.mode(), Mode::Auto);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.mode(), Mode::Ask);
    app.handle_key(key(KeyCode::Tab));
    assert_eq!(app.mode(), Mode::Default);
}

#[test]
fn slash_mode_command_toggles_or_sets_mode() {
    let mut app = app();
    app.submit("/mode".to_string());
    assert_eq!(app.mode(), Mode::Plan);

    app.submit("/mode auto".to_string());
    assert_eq!(app.mode(), Mode::Auto);

    app.submit("/mode ask".to_string());
    assert_eq!(app.mode(), Mode::Ask);

    app.submit("/mode default".to_string());
    assert_eq!(app.mode(), Mode::Default);

    app.submit("/mode bogus".to_string());
    assert_eq!(app.mode(), Mode::Default);
    assert_eq!(
        app.notifications.current().map(|(kind, _)| kind),
        Some(NoticeKind::Error)
    );
}

#[test]
fn unknown_command_reports_an_error() {
    let mut app = app();
    app.submit("/nope".to_string());
    assert_eq!(
        app.notifications.current().map(|(kind, _)| kind),
        Some(NoticeKind::Error)
    );
}

#[test]
fn theme_and_model_arguments_apply_immediately() {
    let mut app = app();
    app.submit("/theme light".to_string());
    app.submit("/model claude-opus-4-1".to_string());
    assert_eq!(app.theme_name(), "light");
    assert_eq!(app.model(), "claude-opus-4-1");
}

#[test]
fn a_named_model_with_nothing_connected_is_kept() {
    // `/model <name>` is the user's explicit choice, so it is not second
    // guessed even when no provider could serve it.
    let mut app = app();
    app.submit("/model some-model-nobody-lists".to_string());
    assert_eq!(app.model(), "some-model-nobody-lists");
}

#[test]
fn theme_descriptions_exist_for_every_palette() {
    for name in Theme::NAMES {
        assert!(
            !theme_description(name).is_empty(),
            "{name} would render an empty description"
        );
    }
}

#[test]
fn bare_commands_open_their_surface() {
    let mut app = app();
    app.submit("/theme".to_string());
    assert_eq!(app.overlay_queue.borrow().len(), 1);
    app.submit("/stats".to_string());
    assert_eq!(app.overlay_queue.borrow().len(), 2);
}

#[tokio::test]
async fn the_providers_own_model_list_fills_the_picker() {
    // A gateway's catalogue is empty, so what the provider reports is the
    // only model list there is.
    let mut app = app_with_backend(fake_backend_reporting(&["glm-5.3", "kimi-k3"]));
    assert_eq!(app.model(), "", "nothing is named until the list lands");

    settle_discovery(&mut app).await;

    // The first of them takes the empty slot, so a turn can be sent without
    // the user naming a model first.
    assert_eq!(app.model(), "glm-5.3");

    app.submit("/model".to_string());
    let text = rendered_text(&mut app, 90, 24);
    assert!(text.contains("Select a model:"), "{text}");
    assert!(text.contains("glm-5.3"), "{text}");
    assert!(text.contains("kimi-k3"), "{text}");
}

#[tokio::test]
async fn a_gateway_model_does_not_inherit_another_providers_description() {
    let mut app = app_with_backend(fake_backend_reporting(&[
        "claude-sonnet-4-5",
        "acme-internal-1",
    ]));
    settle_discovery(&mut app).await;
    app.submit("/model".to_string());

    let text = rendered_text(&mut app, 100, 24);
    // This gateway has not declared an Anthropic model mapping.
    assert!(text.contains("claude-sonnet-4-5"), "{text}");
    assert!(!text.contains("balanced"), "{text}");
    // And one it has never heard of is offered all the same.
    assert!(text.contains("acme-internal-1"), "{text}");
}

#[tokio::test]
async fn a_model_list_that_never_lands_leaves_the_catalogue_to_answer() {
    let mut app = app_connected_to_but_faked("anthropic");
    settle_discovery(&mut app).await;

    // The provider would not say, and that is not the user's problem to
    // solve while the catalogue can answer.
    assert!(
        app.notifications.current().is_none(),
        "the failure was announced"
    );

    app.submit("/model".to_string());
    let text = rendered_text(&mut app, 90, 24);
    assert!(
        text.contains(&app.connection.provider_config.models("anthropic")[1].0),
        "{text}"
    );
}

#[tokio::test]
async fn a_failed_model_list_with_nothing_to_fall_back_on_says_so() {
    let mut app = app_with_backend(fake_backend());
    settle_discovery(&mut app).await;
    assert_eq!(
        app.model(),
        "",
        "nothing was reported, so nothing was adopted"
    );

    // No models and no catalogue: the notice has to say how to proceed.
    app.submit("/model".to_string());
    let (kind, text) = app.notifications.current().expect("a notice was shown");
    assert_eq!(kind, NoticeKind::Warning);
    assert!(text.contains("/model <name>"), "{text}");
}

#[tokio::test]
async fn a_provider_that_lists_no_models_says_so() {
    // A gateway has no catalogue to fall back on, so an empty answer leaves
    // `/model` with nothing — which is what a URL that misses the API looks
    // like from here. It has to be said, or the picker just comes up empty.
    let mut app = app_connected_to_but_faked("new-api");
    app.on_models_discovered(Ok(Vec::new()));

    let (kind, text) = app.notifications.current().expect("a notice was shown");
    assert_eq!(kind, NoticeKind::Warning);
    assert!(text.contains("listed no models"), "{text}");
    assert!(text.contains("/model <name>"), "{text}");
}

#[test]
fn the_model_command_opens_the_inline_picker() {
    let mut app = app_connected_to("anthropic");
    // Nothing was named, so the provider's first model was adopted.
    let current = app.model().to_string();
    assert_eq!(
        current,
        app.connection.provider_config.models("anthropic")[0].0
    );

    app.submit("/model".to_string());

    // It takes the prompt region like `/connect`, and leaves the overlay
    // stack alone.
    assert!(app.overlay_queue.borrow().is_empty());
    assert!(app.inline.is_some());
    let text = rendered_text(&mut app, 80, 24);
    assert!(text.contains("Select a model:"), "{text}");
    assert!(text.contains(current.as_str()), "{text}");
    // Exactly what the connected provider offers, and nothing else.
    assert!(
        text.contains(&app.connection.provider_config.models("anthropic")[1].0),
        "{text}"
    );
    assert!(text.contains("claude-haiku-4-5"), "{text}");

    // The active model is the highlighted row, so Enter keeps it.
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.model(), current);
    assert!(app.inline.is_none(), "the picker stayed open");
}

#[test]
fn a_model_picker_with_nothing_connected_says_so() {
    let mut app = app();
    app.submit("/model".to_string());

    // Nothing to choose between, so the picker stays shut and the notice
    // says what to do about it.
    assert!(app.inline.is_none());
    let (kind, text) = app.notifications.current().expect("a notice was shown");
    assert_eq!(kind, NoticeKind::Warning);
    assert!(text.contains("/connect"), "{text}");
}

#[test]
fn the_inline_model_picker_switches_the_model() {
    let mut app = app_connected_to("anthropic");
    let names: Vec<String> = app
        .connection
        .provider_config
        .models("anthropic")
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let start = names
        .iter()
        .position(|name| name == app.model())
        .expect("the active model is on the list");

    app.submit("/model".to_string());
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.model(), names[(start + 1) % names.len()]);
    assert!(app.inline.is_none(), "the picker stayed open");
}

#[test]
fn escaping_the_model_picker_keeps_the_model() {
    let mut app = app_connected_to("anthropic");
    let current = app.model().to_string();

    app.submit("/model".to_string());
    app.handle_key(key(KeyCode::Esc));

    assert_eq!(app.model(), current);
    assert!(app.inline.is_none());
}

#[test]
fn dialog_choice_updates_the_theme() {
    let mut app = app();
    app.on_dialog_message(DialogMessage::Theme("light".into()));
    assert_eq!(app.theme_name(), "light");
}

#[tokio::test]
async fn confirm_message_clears_the_transcript() {
    let mut app = app();
    app.session.turns.push(Turn {
        prompt: "hi".into(),
        reply: "there".into(),
        complete: true,
        ..Default::default()
    });
    app.on_dialog_message(DialogMessage::Confirm {
        action: ConfirmAction::ClearTranscript,
        accepted: true,
    });
    for _ in 0..1000 {
        app.tick();
        if app.session.turns.is_empty() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(app.session.turns.is_empty());
}

#[tokio::test]
async fn expired_temporary_feed_requires_clear_and_then_accepts_new_work() {
    let mut app = app();
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    app.sessions.runtime_events = crate::application::session::SessionFeed::from_receiver(rx);
    tx.send(Err("history expired".into())).await.unwrap();
    app.tick();
    assert!(app.sessions.runtime_events.is_failed());
    app.submit("not sent".into());
    assert!(app.session.turns.is_empty());
    app.on_dialog_message(DialogMessage::Confirm {
        action: ConfirmAction::ClearTranscript,
        accepted: true,
    });
    for _ in 0..1000 {
        app.tick();
        if !app.sessions.runtime_events.is_failed() {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(!app.sessions.runtime_events.is_failed());
    app.submit("next".into());
    settle_turn(&mut app).await;
    assert!(app.session.turns[0].reply.contains("next"));
}

#[tokio::test]
async fn a_rejected_clear_preserves_the_displayed_history() {
    let mut app = app();
    app.session.turns.push(Turn {
        prompt: "keep".into(),
        reply: "answer".into(),
        complete: true,
        ..Default::default()
    });
    app.sessions.agent.shutdown();
    app.on_dialog_message(DialogMessage::Confirm {
        action: ConfirmAction::ClearTranscript,
        accepted: true,
    });
    app.tick();
    assert_eq!(app.session.turns[0].reply, "answer");
}

#[tokio::test]
async fn explicit_ask_rules_for_read_only_tools_wait_for_user_confirmation() {
    for name in ["read", "mcp__server__lookup"] {
        let mut app = app();
        let (tx, mut rx) = unbounded_channel();
        let policy = solaris_agent::PolicyApprover {
            profile: solaris_agent::PolicyProfile::Trusted,
            read_only_tools: [name.to_owned()].into_iter().collect(),
            cwd: app.cwd.clone(),
            rules: vec![solaris_agent::PermissionRule {
                tool: name.into(),
                resource: "*".into(),
                effect: solaris_agent::PermissionEffect::Ask,
            }],
            fallback: Arc::new(ChannelApprover::new(tx)),
        };
        let cwd = policy.cwd.clone();
        tokio::task::spawn_blocking(move || {
            let path = solaris_tools::environment::resolve_path(&cwd, "a.txt");
            solaris_tools::environment::ExecutionEnvironment
                .canonical_path(&path)
                .unwrap_or_else(|error| {
                    panic!(
                        "approval resource normalization failed for {path:?} (cwd {cwd:?}): {error}"
                    )
                })
        })
        .await
        .expect("resource worker");
        let answer = tokio::spawn(async move {
            policy
                .approve(&ToolCall::new(
                    "call-1",
                    name,
                    serde_json::json!({"path":"a.txt"}),
                ))
                .await
        });
        app.request_approval(rx.recv().await.expect("policy requested confirmation"));
        assert_eq!(app.pending_approvals.len(), 1);
        let id = *app.pending_approvals.keys().next().unwrap();
        assert!(!answer.is_finished(), "read must wait for the user");
        app.on_dialog_message(DialogMessage::Confirm {
            action: ConfirmAction::RunTool(id),
            accepted: false,
        });
        assert_eq!(answer.await.expect("approval task"), Approval::Denied);
    }
}

#[test]
fn interactive_confirmation_uses_ask_semantics_without_raising_read_only_limits() {
    use solaris_agent::PolicyProfile;
    for (profile, expected) in [
        (PolicyProfile::Trusted, PolicyProfile::Ask),
        (PolicyProfile::Ask, PolicyProfile::Ask),
        (PolicyProfile::ReadOnly, PolicyProfile::ReadOnly),
    ] {
        let mut options = AppOptions::new(
            fake_backend(),
            Config::default(),
            Rc::new(Cell::new(false)),
            Rc::new(RefCell::new(Vec::new())),
            Rc::new(Cell::new(false)),
        );
        options.ask_approval = true;
        options.execution.profile = profile;
        let app = App::new(options);
        assert_eq!(app.execution.profile, expected);
    }
}

#[tokio::test]
async fn a_mutating_call_waits_for_the_users_answer() {
    let mut app = app();
    let (reply, mut answer) = oneshot::channel();
    app.request_approval(ApprovalRequest {
        call: ToolCall::new(
            "call-1",
            "write",
            serde_json::json!({ "path": "a.txt", "content": "hi" }),
        ),
        reply,
    });

    // The loop is parked until the prompt is answered.
    assert_eq!(app.pending_approvals.len(), 1);
    let id = *app.pending_approvals.keys().next().unwrap();
    assert!(
        answer.try_recv().is_err(),
        "the call must not be answered before the user is"
    );

    app.on_dialog_message(DialogMessage::Confirm {
        action: ConfirmAction::RunTool(id),
        accepted: true,
    });

    assert_eq!(answer.await.expect("an answer"), Approval::Approved);
    assert!(app.pending_approvals.is_empty());
}

#[tokio::test]
async fn a_declined_prompt_denies_the_call() {
    let mut app = app();
    let (reply, answer) = oneshot::channel();
    app.request_approval(ApprovalRequest {
        call: ToolCall::new("call-1", "bash", serde_json::json!({ "command": "ls" })),
        reply,
    });

    let id = *app.pending_approvals.keys().next().unwrap();
    app.on_dialog_message(DialogMessage::Confirm {
        action: ConfirmAction::RunTool(id),
        accepted: false,
    });

    assert_eq!(answer.await.expect("an answer"), Approval::Denied);
}

#[tokio::test]
async fn stacked_approval_dialogs_resolve_their_own_tool_even_out_of_order_and_on_dismiss() {
    let mut app = app();
    let (first_reply, first_answer) = oneshot::channel();
    let (second_reply, second_answer) = oneshot::channel();
    app.request_approval(ApprovalRequest {
        call: ToolCall::new("call-1", "write", serde_json::json!({ "path": "a.txt" })),
        reply: first_reply,
    });
    app.request_approval(ApprovalRequest {
        call: ToolCall::new("call-2", "bash", serde_json::json!({ "command": "rm -rf" })),
        reply: second_reply,
    });
    assert_eq!(app.pending_approvals.len(), 2);

    // Pop the top (second) dialog first as the LIFO overlay stack does, approve it,
    // then drop the bottom (first) dialog without answering (simulating outside click).
    let mut overlays = app.overlay_queue.borrow_mut().drain(..).collect::<Vec<_>>();
    assert_eq!(overlays.len(), 2);
    let (mut top_second, _) = overlays.pop().unwrap();
    let (bottom_first, _) = overlays.pop().unwrap();

    assert_eq!(
        top_second.handle_key(key(KeyCode::Char('y'))),
        KeyResult::Confirmed
    );
    drop(top_second);
    drop(bottom_first);
    app.tick();

    assert_eq!(
        second_answer.await.expect("second answered"),
        Approval::Approved
    );
    assert_eq!(
        first_answer.await.expect("first denied on drop"),
        Approval::Denied
    );
    assert!(app.pending_approvals.is_empty());
}

#[test]
fn tool_events_become_one_step_per_call() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.session.bump();

    let first = ToolCall::new("call-1", "read", serde_json::json!({ "path": "a.txt" }));
    let second = ToolCall::new("call-2", "bash", serde_json::json!({ "command": "ls" }));
    app.apply_event(AgentEvent::ToolCall(first.clone()));
    app.apply_event(AgentEvent::ToolCall(second.clone()));

    assert_eq!(app.session.turns[0].steps.len(), 2);
    assert!(!app.session.turns[0].steps[0].is_done(), "still running");

    app.apply_event(AgentEvent::ToolResult {
        result: ToolResult::ok(&first, "hello"),
        duration_ms: 7,
    });
    app.apply_event(AgentEvent::ToolResult {
        result: ToolResult::error(&second, "exit code 1"),
        duration_ms: 3,
    });

    let steps = &app.session.turns[0].steps;
    assert!(steps[0].is_done() && !steps[0].is_error());
    assert_eq!(steps[0].duration_ms(), Some(7));
    assert!(steps[1].is_error());
    assert_eq!(steps[1].duration_ms(), Some(3));
}

#[test]
fn a_result_for_a_call_that_was_never_made_is_ignored() {
    // A backend that lost a ToolCall event must not invent a step for it.
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.session.bump();

    let call = ToolCall::new("call-1", "read", serde_json::json!({}));
    app.apply_event(AgentEvent::ToolResult {
        result: ToolResult::ok(&call, "hello"),
        duration_ms: 1,
    });

    assert!(app.session.turns[0].steps.is_empty());
}

#[test]
fn background_reports_finish_the_launch_group_and_notify_on_success_or_failure() {
    for is_error in [false, true] {
        let mut app = app();
        app.session.turns.push(Turn::default());
        let call = ToolCall::new("launch", "subagent", serde_json::json!({"background":true}));
        app.apply_event(AgentEvent::ToolCall(call.clone()));
        app.apply_event(AgentEvent::ToolResult {
            result: ToolResult::ok(&call, r#"{"task_id":"subagent-1","status":"started"}"#),
            duration_ms: 1,
        });
        assert!(
            app.session.turns[0].steps[0]
                .subagent
                .as_ref()
                .unwrap()
                .result
                .is_none(),
            "a background launch acknowledgement must keep its spinner running"
        );
        app.apply_event(AgentEvent::SubagentComplete {
            task_id: "subagent-1".into(),
            output: "background report".into(),
            is_error,
            duration_ms: 123,
        });
        let turn = &app.session.turns[0];
        assert_eq!(turn.steps.len(), 1);
        assert!(
            turn.steps[0]
                .result
                .as_ref()
                .unwrap()
                .output
                .contains("started")
        );
        let child = turn.steps[0].subagent.as_ref().unwrap();
        assert_eq!(child.result.as_ref().unwrap().output, "background report");
        assert_eq!(child.result.as_ref().unwrap().is_error, is_error);
        assert_eq!(child.duration_ms, Some(123));
        assert!(!turn.steps[0].disclosure.is_expanded());
        assert!(!turn.complete);
    }
}

#[test]
fn a_background_completion_before_the_launch_ack_updates_one_group_without_losing_the_report() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    let call = ToolCall::new(
        "launch",
        "subagent",
        serde_json::json!({"agent":"explore","background":true}),
    );
    app.apply_event(AgentEvent::ToolCall(call.clone()));
    app.apply_event(AgentEvent::SubagentStarted {
        call_id: "launch".into(),
        task_id: "subagent-1".into(),
    });
    app.session.turns[0].steps[0].disclosure.set_expanded(true);
    app.apply_event(AgentEvent::SubagentComplete {
        task_id: "subagent-1".into(),
        output: "fast child report".into(),
        is_error: false,
        duration_ms: 5,
    });
    app.apply_event(AgentEvent::ToolResult {
        result: ToolResult::ok(&call, r#"{"task_id":"subagent-1","status":"started"}"#),
        duration_ms: 1,
    });
    let turn = &app.session.turns[0];
    assert_eq!(turn.steps.len(), 1);
    let child = turn.steps[0].subagent.as_ref().unwrap();
    assert_eq!(child.result.as_ref().unwrap().output, "fast child report");
    assert_eq!(child.duration_ms, Some(5));
    assert!(
        turn.steps[0].disclosure.is_expanded(),
        "completion must preserve the user's expansion choice"
    );
}

#[test]
fn parallel_child_tool_events_are_nested_under_the_right_launch_even_with_reused_tool_ids() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    for id in ["one", "two"] {
        app.apply_event(AgentEvent::ToolCall(ToolCall::new(
            id,
            "subagent",
            serde_json::json!({"agent":"explore"}),
        )));
        app.apply_event(AgentEvent::SubagentStarted {
            call_id: id.into(),
            task_id: format!("task-{id}"),
        });
        app.apply_event(AgentEvent::SubagentToolCall {
            task_id: format!("task-{id}"),
            call: ToolCall::new("same-id", "read", serde_json::json!({"path":id})),
        });
    }
    for id in ["two", "one"] {
        let call = ToolCall::new("same-id", "read", serde_json::json!({"path":id}));
        app.apply_event(AgentEvent::SubagentToolResult {
            task_id: format!("task-{id}"),
            result: ToolResult::ok(&call, format!("data-{id}")),
            duration_ms: 2,
        });
    }
    let steps = &app.session.turns[0].steps;
    assert_eq!(
        steps.len(),
        2,
        "child calls must not become top-level steps"
    );
    for (step, id) in steps.iter().zip(["one", "two"]) {
        let child = step.subagent.as_ref().unwrap();
        assert!(!step.disclosure.is_expanded() && child.result.is_none());
        assert_eq!(child.steps.len(), 1);
        assert_eq!(
            child.steps[0].result.as_ref().unwrap().output,
            format!("data-{id}")
        );
    }
}

#[test]
fn child_title_click_opens_terminal_and_escape_restores_main_view() {
    let mut app = app();
    app.session.turns.push(Turn {
        prompt: (0..20).map(|n| format!("line {n}\n")).collect(),
        complete: true,
        ..Default::default()
    });
    app.session.turns.push(Turn {
        prompt: "Delegate".into(),
        complete: true,
        ..Default::default()
    });
    // Insert into the completed turn directly so this also tests history rows.
    let call = ToolCall::new("launch", "subagent", serde_json::json!({"agent":"explore"}));
    let mut step = ToolStep::pending(call.clone());
    step.subagent
        .as_mut()
        .unwrap()
        .steps
        .push(ToolStep::pending(ToolCall::new(
            "read",
            "read",
            serde_json::json!({"path":"inside.txt"}),
        )));
    step.finish(ToolResult::ok(&call, "report"), 3);
    app.session.turns[1].steps.push(step);
    let text = rendered_text(&mut app, 80, 14);
    assert!(app.session.scroll > 0);
    let row = text
        .lines()
        .position(|line| line.contains("SubAgent:"))
        .unwrap() as u16;
    let pointer = |kind, column| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::empty(),
    };
    app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 2));
    app.handle_mouse(pointer(MouseEventKind::Drag(MouseButton::Left), 10));
    app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 10));
    assert!(app.subagent_terminal.is_none());
    assert!(!app.session.turns[1].steps[0].disclosure.is_expanded());
    let main_scroll = app.session.scroll;
    let main_follow = app.session.follow_end;
    app.editor.set_text("unfinished prompt");
    app.handle_mouse(pointer(MouseEventKind::Down(MouseButton::Left), 2));
    app.handle_mouse(pointer(MouseEventKind::Up(MouseButton::Left), 2));
    assert_eq!(app.subagent_terminal.as_ref().unwrap().target, (1, 0));
    assert!(!app.session.turns[1].steps[0].disclosure.is_expanded());
    click_child_tool(&mut app, "→ Explored:", 2);
    let expanded = rendered_text(&mut app, 80, 14);
    assert!(
        expanded.contains("Subagent 1/1")
            && expanded.contains("Read inside.txt")
            && expanded.contains("report"),
        "{expanded}"
    );
    app.handle_key(key(KeyCode::Esc));
    assert!(app.subagent_terminal.is_none());
    assert_eq!(app.session.scroll, main_scroll);
    assert_eq!(app.session.follow_end, main_follow);
    let restored = rendered_text(&mut app, 80, 14);
    assert!(restored.contains("unfinished prompt"), "{restored}");
    assert!(!restored.contains("Read inside.txt"), "{restored}");
    assert!(!app.session.turns[1].steps[0].disclosure.is_expanded());
}

#[test]
fn arrow_keys_cycle_child_terminals_across_turns_without_editing_the_prompt() {
    let mut app = app();
    for id in ["first", "second"] {
        app.session.turns.push(Turn {
            steps: vec![ToolStep::pending(ToolCall::new(
                id,
                "subagent",
                serde_json::json!({"prompt":id}),
            ))],
            ..Default::default()
        });
    }
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.subagent_terminal.as_ref().unwrap().target, (0, 0));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.subagent_terminal.as_ref().unwrap().target, (1, 0));
    app.handle_key(key(KeyCode::Right));
    assert_eq!(app.subagent_terminal.as_ref().unwrap().target, (0, 0));
    app.handle_key(key(KeyCode::Left));
    assert_eq!(app.subagent_terminal.as_ref().unwrap().target, (1, 0));
    app.handle_paste("should not reach the main prompt");
    app.handle_key(key(KeyCode::Char('x')));
    app.handle_key(key(KeyCode::Esc));
    assert!(app.editor.is_empty());
    app.editor.set_text("draft");
    app.handle_key(key(KeyCode::Left));
    assert!(app.subagent_terminal.is_none());
}

#[test]
fn child_terminal_scrolls_and_receives_live_events_without_moving_main_view() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.apply_event(AgentEvent::ToolCall(ToolCall::new(
        "launch",
        "subagent",
        serde_json::json!({"prompt":"investigate"}),
    )));
    app.apply_event(AgentEvent::SubagentStarted {
        call_id: "launch".into(),
        task_id: "child".into(),
    });
    app.open_subagent((0, 0));
    let call = ToolCall::new("read", "read", serde_json::json!({"path":"live.txt"}));
    app.apply_event(AgentEvent::SubagentToolCall {
        task_id: "child".into(),
        call: call.clone(),
    });
    click_child_tool(&mut app, "→ Exploring:", 2);
    click_child_tool(&mut app, "→ Read live.txt", 4);
    app.apply_event(AgentEvent::SubagentToolResult {
        task_id: "child".into(),
        result: ToolResult::ok(
            &call,
            (0..40).map(|i| format!("output {i}\n")).collect::<String>(),
        ),
        duration_ms: 2,
    });
    app.handle_key(key(KeyCode::End));
    assert!(rendered_text(&mut app, 80, 14).contains("output 39"));
    app.handle_key(key(KeyCode::Home));
    assert!(rendered_text(&mut app, 80, 14).contains("investigate"));
    assert!(!app.subagent_terminal.as_ref().unwrap().follow_end);
    app.handle_key(key(KeyCode::PageDown));
    assert!(app.subagent_terminal.as_ref().unwrap().scroll > 0);
    app.handle_key(key(KeyCode::End));
    app.apply_event(AgentEvent::SubagentComplete {
        task_id: "child".into(),
        output: "Live final report".into(),
        is_error: false,
        duration_ms: 3,
    });
    assert!(rendered_text(&mut app, 80, 14).contains("Live final report"));
    assert_eq!(app.session.scroll, 0);
}

#[test]
fn child_disclosures_preserve_state_across_agent_switches_and_ignore_drags() {
    let mut app = app();
    let mut launches = Vec::new();
    for name in ["first", "second"] {
        let launch = ToolCall::new(name, "subagent", serde_json::json!({"name":name}));
        let mut step = ToolStep::pending(launch.clone());
        for (id, tool, input) in [
            ("read", "read", serde_json::json!({"path":"child.txt"})),
            (
                "shell",
                "shell",
                serde_json::json!({"command":"echo child"}),
            ),
        ] {
            let call = ToolCall::new(id, tool, input);
            let mut child_tool = ToolStep::pending(call.clone());
            child_tool.finish(ToolResult::ok(&call, format!("{name} {tool} output")), 1);
            step.subagent.as_mut().unwrap().steps.push(child_tool);
        }
        step.finish(ToolResult::ok(&launch, format!("{name} report")), 2);
        launches.push(step);
    }
    app.session.turns.push(Turn {
        steps: launches,
        complete: true,
        ..Default::default()
    });
    app.open_subagent((0, 0));
    click_child_tool(&mut app, "→ Explored:", 2);
    let text = rendered_text(&mut app, 80, 14);
    let row = text
        .lines()
        .position(|line| line.contains("→ Read child.txt"))
        .unwrap() as u16;
    for (kind, column) in [
        (MouseEventKind::Down(MouseButton::Left), 4),
        (MouseEventKind::Drag(MouseButton::Left), 12),
        (MouseEventKind::Up(MouseButton::Left), 4),
    ] {
        app.handle_mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::empty(),
        });
    }
    assert!(!rendered_text(&mut app, 80, 14).contains("first read output"));
    click_child_tool(&mut app, "→ Read child.txt", 4);
    click_child_tool(&mut app, "→ Shell:", 2);
    let text = rendered_text(&mut app, 80, 14);
    assert!(
        text.contains("first read output") && text.contains("first shell output"),
        "{text}"
    );
    app.handle_key(key(KeyCode::Right));
    let text = rendered_text(&mut app, 80, 14);
    assert!(
        !text.contains("first read output") && !text.contains("second read output"),
        "{text}"
    );
    app.handle_key(key(KeyCode::Left));
    let text = rendered_text(&mut app, 80, 14);
    assert!(
        text.contains("first read output") && text.contains("first shell output"),
        "{text}"
    );
    app.handle_key(key(KeyCode::Esc));
    let text = rendered_text(&mut app, 80, 14);
    assert!(!text.contains("first read output"), "{text}");
    assert!(!app.session.turns[0].steps[0].disclosure.is_expanded());
}

#[test]
fn tools_inside_a_group_expand_independently_and_survive_parent_collapse() {
    let mut app = app();
    let steps = ["a.txt", "b.txt"]
        .into_iter()
        .map(|path| {
            let call = ToolCall::new(path, "read", serde_json::json!({"path":path}));
            let mut step = ToolStep::pending(call.clone());
            step.finish(ToolResult::ok(&call, format!("contents of {path}")), 1);
            step
        })
        .collect();
    app.session.turns.push(Turn {
        steps,
        complete: true,
        ..Default::default()
    });
    let click = |app: &mut App, label: &str, column| {
        let text = rendered_text(app, 80, 20);
        let row = text.lines().position(|line| line.contains(label)).unwrap() as u16;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::empty(),
            });
        }
    };
    click(&mut app, "→ Explored:", 2);
    click(&mut app, "→ Read a.txt", 4);
    let text = rendered_text(&mut app, 80, 20);
    assert!(
        text.contains("↓ Read a.txt") && text.contains("contents of a.txt"),
        "{text}"
    );
    assert!(!text.contains("contents of b.txt"), "{text}");
    click(&mut app, "→ Read b.txt", 4);
    let text = rendered_text(&mut app, 80, 20);
    assert!(
        text.contains("contents of a.txt") && text.contains("contents of b.txt"),
        "{text}"
    );
    click(&mut app, "↓ Read a.txt", 4);
    assert!(
        !app.session.turns[0].steps[0]
            .detail_disclosure
            .is_expanded()
    );
    assert!(app.session.turns[0].steps[0].disclosure.is_expanded());
    assert!(
        app.session.turns[0].steps[1]
            .detail_disclosure
            .is_expanded()
    );
    click(&mut app, "↓ Explored:", 2);
    assert!(!rendered_text(&mut app, 80, 20).contains("Read b.txt"));
    click(&mut app, "→ Explored:", 2);
    let text = rendered_text(&mut app, 80, 20);
    assert!(
        text.contains("contents of b.txt") && !text.contains("contents of a.txt"),
        "{text}"
    );
}

#[test]
fn expanding_tool_disclosures_at_the_viewport_bottom_reveals_their_bodies() {
    for shell in [false, true] {
        let mut app = app();
        let call = if shell {
            ToolCall::new("tool", "shell", serde_json::json!({"command":"cargo test"}))
        } else {
            ToolCall::new("tool", "read", serde_json::json!({"path":"inside.txt"}))
        };
        let mut step = ToolStep::pending(call.clone());
        step.finish(
            ToolResult::ok(&call, "first output line\nsecond output line"),
            3,
        );
        app.session.turns.push(Turn {
            prompt: (0..20).map(|n| format!("line {n}\n")).collect(),
            steps: if shell {
                vec![step]
            } else {
                vec![step.clone(), step]
            },
            complete: true,
            ..Default::default()
        });
        let text = rendered_text(&mut app, 80, 14);
        assert!(app.session.scroll > 0);
        let title = if shell { "Shell:" } else { "Explored:" };
        let row = text.lines().position(|line| line.contains(title)).unwrap() as u16;
        assert_eq!(row, app.last_transcript.bottom() - 1);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            app.handle_mouse(MouseEvent {
                kind,
                column: 2,
                row,
                modifiers: KeyModifiers::empty(),
            });
        }
        let expanded = rendered_text(&mut app, 80, 14);
        assert!(expanded.contains(&format!("↓ {title}")), "{expanded}");
        let detail = if shell {
            "second output line"
        } else {
            "Read inside.txt"
        };
        assert!(expanded.contains(detail), "{expanded}");
        assert!(app.session.turns[0].steps[0].disclosure.is_expanded());
    }
}

#[test]
fn streamed_events_record_interleaved_display_order() {
    use crate::tui::state::TurnPart;
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.apply_event(AgentEvent::ThinkingDelta("先想".into()));
    app.apply_event(AgentEvent::TextDelta("先".into()));
    app.apply_event(AgentEvent::TextDelta("说明".into()));
    let call = ToolCall::new("call", "read", serde_json::json!({"path":"file.rs"}));
    app.apply_event(AgentEvent::ToolCall(call.clone()));
    app.apply_event(AgentEvent::ToolResult {
        result: solaris_agent::ToolResult::ok(&call, "data"),
        duration_ms: 1,
    });
    app.apply_event(AgentEvent::Steered {
        text: "再检查".into(),
    });
    app.apply_event(AgentEvent::ThinkingDelta("再想".into()));
    app.apply_event(AgentEvent::TextDelta("结论".into()));
    let turn = &app.session.turns[0];
    assert_eq!(
        turn.parts,
        [
            TurnPart::Thinking(0..6),
            TurnPart::Text(0..9),
            TurnPart::Tool(0),
            TurnPart::Steering(0),
            TurnPart::Thinking(6..12),
            TurnPart::Text(9..15)
        ]
    );
    assert_eq!(turn.reply, "先说明结论");
    assert_eq!(turn.thinking, "先想再想");
    assert_eq!(turn.steps[0].result.as_ref().unwrap().output, "data");
}

#[test]
fn edit_events_accumulate_into_the_active_turn() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.session.bump();

    app.apply_event(AgentEvent::ThinkingDelta("hmm ".into()));
    app.apply_event(AgentEvent::TextDelta("hello ".into()));
    app.apply_event(AgentEvent::TextDelta("world".into()));
    app.apply_event(AgentEvent::TurnComplete {
        usage: Usage::new(5, 2),
        cost_usd: 0.001,
    });

    let turn = &app.session.turns[0];
    assert_eq!(turn.thinking, "hmm ");
    assert_eq!(turn.reply, "hello world");
    assert_eq!(turn.tokens(), 7);
    assert_eq!(turn.usage.input_tokens, 5);
    assert_eq!(turn.usage.output_tokens, 2);
    assert!(turn.complete);
    assert!(!app.session.is_streaming());
}

#[test]
fn error_event_finishes_the_turn_and_notifies() {
    let mut app = app();
    app.session.turns.push(Turn::default());
    app.apply_event(AgentEvent::Error("boom".into()));

    assert!(app.session.turns[0].complete);
    assert!(app.session.turns[0].reply.contains("boom"));
    assert_eq!(
        app.notifications.current().map(|(kind, _)| kind),
        Some(NoticeKind::Error)
    );
}

#[test]
fn rendering_rearms_follow_end_at_the_bottom() {
    let mut app = app();
    app.scroll_transcript(-5);
    assert!(!app.session.follow_end);

    app.session.scroll = 0;
    let area = Rect::new(0, 0, 60, 6);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    assert!(app.session.follow_end);
}

#[test]
fn footer_slots_do_not_overlap() {
    let mut app = app();

    // Narrow enough that the status segment would run into the hint.
    let width = 45u16;
    let area = Rect::new(0, 0, width, 8);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);

    let footer: String = (0..area.width)
        .map(|x| buf[(x, area.height - 1)].symbol())
        .collect();

    // The hint owns the right slot in full…
    let hint_width = display_width(keymap::FOOTER_HINT) as u16 + 1;
    let split = (width - hint_width) as usize;
    let right: String = footer.chars().skip(split).collect();
    assert!(right.starts_with(keymap::FOOTER_HINT), "{footer:?}");

    // …and the status segment ellipsises instead of being overwritten.
    let left: String = footer.chars().take(split).collect();
    assert!(
        left.ends_with('…'),
        "status segment not ellipsised: {left:?}"
    );
}

#[test]
fn footer_shows_context_percentage_bar_with_status_colors() {
    let mut app = app();
    let area = Rect::new(0, 0, 100, 8);
    let read_row = |buf: &Buffer, y: u16| -> String {
        let mut out = String::new();
        let mut x = 0u16;
        while x < area.width {
            let sym = buf[(x, y)].symbol();
            out.push_str(sym);
            x += (display_width(sym).max(1)) as u16;
        }
        out
    };

    // Zero context tokens renders 0% with 10 light shade blocks in success colour,
    // without raw token count or cost.
    app.session.context = solaris_agent::ContextUsage {
        tokens: Some(0),
        window: 1000,
        estimated: false,
    };
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    let footer = read_row(&buf, area.height - 1);
    assert!(
        footer.contains("上下文 ░░░░░░░░░░ 0%"),
        "unexpected empty context gauge: {footer:?}"
    );
    assert!(
        !footer.contains("tok"),
        "raw token counter should be gone: {footer:?}"
    );
    assert!(
        !footer.contains('$'),
        "cost should only be shown in /stats: {footer:?}"
    );
    let empty_bar_x = (0..area.width)
        .find(|&x| buf[(x, area.height - 1)].symbol() == "░")
        .expect("empty bar cell");
    assert_eq!(buf[(empty_bar_x, area.height - 1)].fg, app.theme.success);

    // 50% fills half the bar in success colour.
    app.session.context.tokens = Some(500);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    let footer = read_row(&buf, area.height - 1);
    assert!(
        footer.contains("上下文 █████░░░░░ 50%"),
        "unexpected 50% context gauge: {footer:?}"
    );
    let bar_x = (0..area.width)
        .find(|&x| buf[(x, area.height - 1)].symbol() == "█")
        .expect("filled bar cell");
    assert_eq!(buf[(bar_x, area.height - 1)].fg, app.theme.success);

    // 75% switches to warning colour; 92% switches to error colour.
    for (tokens, expected, expected_fg) in [
        (750, "上下文 ████████░░ 75%", app.theme.warning),
        (920, "上下文 █████████░ 92%", app.theme.error),
    ] {
        app.session.context.tokens = Some(tokens);
        let mut buf = Buffer::empty(area);
        app.render(&mut buf, area);
        let footer = read_row(&buf, area.height - 1);
        assert!(footer.contains(expected), "{footer:?}");
        let bar_x = (0..area.width)
            .find(|&x| buf[(x, area.height - 1)].symbol() == "█")
            .expect("filled bar cell");
        assert_eq!(buf[(bar_x, area.height - 1)].fg, expected_fg);
    }
}

#[test]
fn a_notice_wider_than_the_footer_is_ellipsised_not_dropped() {
    let mut app = app();
    app.notifications.warning(
        "the provider listed no models — check its URL in /connect, or name one with \
             /model <name>",
    );

    // Narrower than the notice: the facts give way, and the message is cut
    // short rather than hidden — it is the only place the failure is said.
    let area = Rect::new(0, 0, 40, 8);
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);

    let footer: String = (0..area.width)
        .map(|x| buf[(x, area.height - 1)].symbol())
        .collect();
    assert!(footer.starts_with("! the provider listed no"), "{footer:?}");
    assert!(footer.ends_with('…'), "{footer:?}");
}

// ------------------------------------------------------------- /connect

fn chatgpt_credential() -> Credential {
    Credential::ChatGpt {
        access_token: "access".into(),
        refresh_token: "refresh".into(),
        id_token: "id".into(),
        account_id: "account".into(),
        expires_at: u64::MAX,
    }
}

fn connect_chatgpt_for_test(app: &mut App) {
    let mut flow = ConnectFlow::new(&app.theme);
    flow.enter_browser_auth("https://auth.openai.com/oauth/authorize".into());
    app.inline = Some(Inline::Connect(flow));
    app.finish_connect("openai".into(), "OpenAI".into(), Some(chatgpt_credential()));
}

#[tokio::test]
async fn chatgpt_picker_waits_for_account_catalog_without_any_bundled_models() {
    let mut app = app_reporting_models_as("openai", &["account-a", "account-b", "account-c"]);
    connect_chatgpt_for_test(&mut app);
    let loading = rendered_text(&mut app, 90, 22);
    assert!(loading.contains("Loading OpenAI account models"));
    assert!(app.model_choices().is_empty());
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));
    settle_discovery(&mut app).await;
    assert_eq!(
        app.model_choices()
            .iter()
            .map(|model| model.value.as_str())
            .collect::<Vec<_>>(),
        vec!["account-a", "account-b", "account-c"]
    );
    assert_eq!(app.config.model, "account-a");
}

#[tokio::test]
async fn chatgpt_picker_keeps_catalog_failure_visible_and_can_retry() {
    let mut app = app_connected_to_but_faked("openai");
    connect_chatgpt_for_test(&mut app);
    settle_discovery(&mut app).await;
    let text = rendered_text(&mut app, 100, 22);
    assert!(text.contains("Could not list OpenAI models"));
    assert!(text.contains("model discovery is unavailable"));
    assert!(app.model_choices().is_empty());
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));
    app.handle_key(key(KeyCode::Char('r')));
    assert!(app.connection.models_rx.is_some());
    assert!(app.connection.models_error.is_none());
    assert!(rendered_text(&mut app, 100, 22).contains("Loading OpenAI account models"));
}

#[tokio::test]
async fn chatgpt_empty_account_catalog_is_not_replaced_by_public_api_models() {
    let mut app = app_reporting_models_as("openai", &[]);
    connect_chatgpt_for_test(&mut app);
    settle_discovery(&mut app).await;
    assert!(app.model_choices().is_empty());
    assert!(rendered_text(&mut app, 100, 22).contains("OpenAI returned no visible models"));
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));
}

#[tokio::test]
async fn reopening_chatgpt_model_picker_requests_a_fresh_account_catalog() {
    let mut app = app_reporting_models_as("openai", &["account-current"]);
    connect_chatgpt_for_test(&mut app);
    settle_discovery(&mut app).await;
    app.close_inline();
    app.open_model_picker();
    assert!(app.model_choices().is_empty());
    assert!(rendered_text(&mut app, 100, 22).contains("Loading OpenAI account models"));
    settle_discovery(&mut app).await;
    let text = rendered_text(&mut app, 100, 22);
    assert!(text.contains("account-current"));
    assert!(!text.contains("Loading"));
}

#[tokio::test]
async fn chatgpt_discovery_delegates_refresh_to_its_provider() {
    let mut app = app_reporting_models_as("openai", &["account-current"]);
    let mut credential = chatgpt_credential();
    if let Credential::ChatGpt { expires_at, .. } = &mut credential {
        *expires_at = 0;
    }
    app.connection.auth.store("openai", credential);
    app.connection.auth.activate("openai");
    app.refresh_models();
    assert!(app.connection.oauth.is_none());
    assert!(app.connection.models_rx.is_some());
    settle_discovery(&mut app).await;
    assert_eq!(app.connection.discovered, vec!["account-current"]);
}

#[tokio::test]
async fn browser_authorization_completion_stores_credentials_and_opens_model_picker() {
    let mut app = app();
    let mut flow = ConnectFlow::new(&app.theme);
    flow.enter_browser_auth("https://auth.openai.com/oauth/authorize".into());
    app.inline = Some(Inline::Connect(flow));
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(async move {
        let _ = tx.send(Ok(chatgpt_credential()));
    });
    app.connection.oauth = Some(OAuthPending {
        url_rx: None,
        rx,
        task,
    });
    tokio::task::yield_now().await;
    assert!(app.poll_oauth());
    assert_eq!(app.connection.auth.active_provider(), Some("openai"));
    assert_eq!(
        app.connection.auth.credential("openai"),
        Some(&chatgpt_credential())
    );
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));
    assert!(app.model_choices().is_empty());
    assert!(rendered_text(&mut app, 80, 20).contains("Model discovery is disabled"));
    app.on_models_discovered(Ok(vec![
        "account-model-one".into(),
        "account-model-two".into(),
        "account-model-three".into(),
    ]));
    let text = rendered_text(&mut app, 80, 20);
    assert!(text.contains("account-model-three"));
    assert!(!text.contains("gpt-5-codex"));
}

#[tokio::test]
async fn model_preparation_does_not_create_a_ui_refresh_workflow() {
    let mut app = app_connected_to_but_faked("openai");
    app.start_turn("hello".into(), "hello".into());
    assert!(app.connection.oauth.is_none());
    settle_turn(&mut app).await;
    assert_eq!(app.session.turns[0].prompt, "hello");
}

#[tokio::test]
async fn cancelling_browser_authorization_discards_late_credentials() {
    let mut app = app();
    let mut flow = ConnectFlow::new(&app.theme);
    flow.enter_browser_auth("https://auth.openai.com/oauth/authorize".into());
    app.inline = Some(Inline::Connect(flow));
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(std::future::pending());
    app.connection.oauth = Some(OAuthPending {
        url_rx: None,
        rx,
        task,
    });
    app.handle_key(key(KeyCode::Esc));
    assert!(app.connection.oauth.is_none());
    assert!(tx.send(Ok(chatgpt_credential())).is_err());
    assert!(app.connection.auth.credential("openai").is_none());
}

#[test]
fn connect_command_opens_the_wizard() {
    let mut app = app();
    app.submit("/connect".to_string());

    assert!(app.connect_open());
    assert_eq!(app.connect_step(), Some(ConnectStep::Provider));
}

#[test]
fn picking_a_provider_routes_to_its_auth_step() {
    let mut app = app();
    app.submit("/connect".to_string());

    // Row 1 is Anthropic, which wants an API key.
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.connect_step(), Some(ConnectStep::ApiKey));

    // Row 8 is the custom endpoint, which wants a URL.
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Char('8')));
    assert_eq!(app.connect_step(), Some(ConnectStep::CustomProvider));
}

#[test]
fn the_wizard_owns_the_keyboard() {
    let mut app = app();
    app.submit("/connect".to_string());

    // F1 would open the help overlay for the session screen.
    app.handle_key(KeyEvent::new(KeyCode::F(1), KeyModifiers::empty()));

    assert!(
        app.overlay_queue.borrow().is_empty(),
        "key leaked to the app"
    );
    assert!(app.connect_open(), "the wizard should still be up");
}

#[test]
fn ctrl_c_cancels_the_wizard_instead_of_quitting() {
    let mut app = app();
    app.submit("/connect".to_string());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(!app.connect_open());
    assert!(
        !app.quit.get(),
        "ctrl+c must cancel the wizard, not the app"
    );
}

#[test]
fn escape_closes_the_wizard() {
    let mut app = app();
    app.submit("/connect".to_string());

    app.handle_key(key(KeyCode::Esc));
    assert!(!app.connect_open());
}

#[tokio::test]
async fn an_api_key_is_stored_then_the_model_picker_follows() {
    let mut app = app();
    connect_with_api_key(&mut app, "sk-test");

    assert_eq!(app.active_provider(), Some("anthropic"));
    assert!(app.connection.auth.credential("anthropic").is_some());
    assert_eq!(
        app.connection
            .auth
            .credential("anthropic")
            .and_then(Credential::secret),
        Some("sk-test")
    );
    // The wizard hands over to the provider's model list.
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.model(),
        app.connection.provider_config.models("anthropic")[0].0
    );
    assert!(!app.connect_open());
}

#[tokio::test]
async fn a_gateway_reaches_the_model_step_before_its_list_arrives() {
    // A gateway has no catalogue to hand the wizard, so its model step can
    // only be filled from the wire. It opens the moment the credential
    // lands, and the provider's answer is what completes it.
    let mut app = app_reporting_models_as("new-api", &["gpt-4o", "gpt-4o-mini"]);
    connect_with_a_gateway_url(&mut app);

    assert_eq!(app.connect_step(), Some(ConnectStep::Model));
    let text = rendered_text(&mut app, 80, 24);
    assert!(text.contains("Select a model:"), "{text}");
    assert!(text.contains("Asking New API"), "{text}");

    settle_discovery(&mut app).await;

    let text = rendered_text(&mut app, 80, 24);
    assert!(text.contains("gpt-4o-mini"), "{text}");
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));

    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.model(), "gpt-4o");
    assert!(!app.connect_open());
}

#[tokio::test]
async fn a_gateway_that_names_no_models_leaves_nothing_to_pick() {
    // An empty list is an answer, not a failure: the gateway is saying this
    // credential reaches no models. The step has nothing to offer, so it
    // goes away and the notice says how to name one by hand.
    let mut app = app_reporting_models_as("new-api", &[]);
    connect_with_a_gateway_url(&mut app);
    assert_eq!(app.connect_step(), Some(ConnectStep::Model));

    settle_discovery(&mut app).await;

    assert!(!app.connect_open());
    let (kind, text) = app.notifications.current().expect("a notice was shown");
    assert_eq!(kind, NoticeKind::Warning);
    assert!(text.contains("listed no models"), "{text}");
}

#[tokio::test]
async fn a_connected_gateway_is_named_in_the_footer_before_a_model_is_chosen() {
    // The credential is stored and the endpoint is resolved even though no
    // model is named yet, so the session is not "unconnected": the footer
    // names the platform, and reconnecting says what is still missing.
    let mut auth = AuthStore::new();
    auth.store(
        "new-api",
        Credential::Endpoint {
            base_url: "https://gateway.test/v1".to_string(),
            api_key: "k".to_string(),
        },
    );
    auth.activate("new-api");
    let mut app = app_with_real_backends(auth);

    let footer = footer_text(&mut app);
    assert!(footer.contains(" new-api · "), "{footer:?}");

    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Char('5'))); // row 5 — New API
    app.handle_key(key(KeyCode::Enter));

    let (kind, text) = app.notifications.current().expect("a notice was shown");
    assert_eq!(kind, NoticeKind::Info);
    assert!(text.contains("/model <name>"), "{text}");
}

#[test]
fn a_local_provider_connects_without_collecting_anything() {
    let mut app = app();
    app.submit("/connect".to_string());

    // Row 7 is the local runtime: activation is immediate.
    app.handle_key(key(KeyCode::Char('7')));

    assert_eq!(app.active_provider(), Some("local"));
    assert!(app.connection.auth.credential("local").is_none());
    assert_eq!(
        app.connect_step(),
        None,
        "local models require endpoint discovery"
    );
}

#[test]
fn the_custom_endpoint_step_requires_a_url() {
    let mut app = app();
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Char('8')));

    // Enter with an empty URL does not connect.
    app.handle_key(key(KeyCode::Enter));
    assert!(app.connection.auth.credential("custom").is_none());
    assert_eq!(app.connect_step(), Some(ConnectStep::CustomProvider));

    for ch in "https://example.test/v1".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(app.active_provider(), Some("custom"));
    assert!(matches!(
        app.connection.auth.credential("custom"),
        Some(Credential::Endpoint { .. })
    ));
    // The custom provider has no model list, so the wizard closes.
    assert!(!app.connect_open());
}

#[test]
fn reconnecting_a_gateway_shows_the_stored_url_and_key() {
    let mut auth = AuthStore::new();
    auth.store(
        "new-api",
        Credential::Endpoint {
            base_url: "http://one-api.test".to_string(),
            api_key: "sk-stored-1234".to_string(),
        },
    );
    let mut app = app_with_real_backends(auth);

    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Char('5'))); // row 5 — New API
    assert_eq!(app.connect_step(), Some(ConnectStep::CustomProvider));

    let text = rendered_text(&mut app, 100, 24);
    assert!(text.contains("http://one-api.test"), "{text}");
    assert!(
        text.contains(&format!("{}1234", "\u{2022}".repeat(8))),
        "the stored key should show masked:\n{text}"
    );
    assert!(!text.contains("sk-stored-1234"), "the key leaked:\n{text}");
}

#[test]
fn reconnecting_an_api_key_provider_keeps_the_stored_key() {
    let mut app = app_connected_to("anthropic");

    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Enter)); // row 1 — Anthropic

    // `mask_secret("sk-test")` — three bullets and the tail.
    let text = rendered_text(&mut app, 100, 24);
    assert!(
        text.contains(&format!("{}test", "\u{2022}".repeat(3))),
        "the stored key should show masked:\n{text}"
    );
    assert!(!text.contains("sk-test"), "the key leaked:\n{text}");

    // Confirming the untouched field stores the same key back.
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.connection
            .auth
            .credential("anthropic")
            .and_then(Credential::secret),
        Some("sk-test")
    );
}

#[test]
fn pasting_an_api_key_lands_in_the_field() {
    let mut app = app();
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Enter));

    app.handle_paste("sk-pasted");
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.connection
            .auth
            .credential("anthropic")
            .and_then(Credential::secret),
        Some("sk-pasted")
    );
}

#[tokio::test]
async fn ctrl_v_pastes_the_clipboard_into_the_prompt() {
    let (mut app, _) = app_with_clipboard_text(Some("pasted text"), true);

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

    assert_eq!(app.editor.text(), "pasted text");
}

#[test]
fn pasting_a_key_over_a_stored_one_replaces_it() {
    let mut auth = AuthStore::new();
    auth.store(
        "anthropic",
        Credential::ApiKey {
            key: "sk-stored".to_string(),
        },
    );
    auth.activate("anthropic");

    let mut app = app_with_real_backends(auth);
    app.clipboard = Clipboard {
        background: false,
        write: Rc::new(|_| true),
        read: Rc::new(|| Some("sk-pasted".to_string())),
    };

    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Enter)); // row 1 — Anthropic
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.connection
            .auth
            .credential("anthropic")
            .and_then(Credential::secret),
        Some("sk-pasted"),
        "the paste should replace the stored key, not extend it"
    );
}

#[tokio::test]
async fn ctrl_v_pastes_into_the_connect_wizard() {
    let (mut app, _) = app_with_clipboard_text(Some("sk-pasted"), true);
    app.submit("/connect".to_string());
    app.handle_key(key(KeyCode::Enter));

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));

    assert_eq!(
        app.connection
            .auth
            .credential("anthropic")
            .and_then(Credential::secret),
        Some("sk-pasted")
    );
}

#[tokio::test]
async fn a_blank_cell_survives_a_copy_then_paste_round_trip() {
    let mut app = app_with_live_clipboard();
    let area = Rect::new(0, 0, 60, 12);

    // Copy two blank cells, wherever this layout keeps them.
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    let blank_run = |row: u16| {
        (0..area.width - 4)
            .find(|start| (*start..*start + 5).all(|x| buf[(x, row)].symbol() == " "))
    };
    let (row, column) = (0..area.height - 1)
        .find_map(|row| blank_run(row).map(|column| (row, column)))
        .expect("the frame has no blank run to drag over");

    drag_select(&mut app, area, (column, row), (column + 1, row));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    // Paste them back: the prompt holds the blanks, invisible as they are.
    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

    assert_eq!(app.editor.text(), "  ", "the blanks did not round-trip");
    assert!(!app.editor.is_empty(), "the prompt still counts as empty");
}

#[tokio::test]
async fn a_whitespace_only_prompt_is_not_sent() {
    let (mut app, _) = app_with_clipboard_text(Some("   "), true);

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));
    app.handle_key(key(KeyCode::Enter));

    // Enter takes the text out of the prompt, and `submit` drops a prompt
    // that holds nothing but blanks — the same as typing spaces by hand.
    assert!(app.session.turns.is_empty(), "a blank prompt was sent");
}

#[tokio::test]
async fn an_empty_clipboard_says_so() {
    let (mut app, _) = app_with_clipboard_text(None, true);

    app.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL));

    assert!(app.editor.is_empty(), "something was pasted");
    assert!(
        rendered_text(&mut app, 60, 12).contains("clipboard is empty"),
        "an empty clipboard went unmentioned"
    );
}

#[tokio::test]
async fn credentials_are_written_to_disk() {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("solaris-provider-{nanos}.json"));
    let _ = std::fs::remove_file(&path);

    let mut app = app_with_auth_path(path.clone());
    connect_with_api_key(&mut app, "sk-on-disk");

    app.poll_io(true);
    let text = std::fs::read_to_string(&path).expect("credentials file");
    let store = AuthStore::from_json(&text).expect("valid json");
    assert_eq!(
        store.credential("anthropic").and_then(Credential::secret),
        Some("sk-on-disk")
    );
    assert_eq!(store.active_provider(), Some("anthropic"));

    let _ = std::fs::remove_file(&path);
}

#[test]
fn the_footer_names_the_backend_that_answers() {
    // Nothing connected: the footer says `unconnected` rather than naming a
    // provider that would never be called.
    let mut app = app_with_real_backends(AuthStore::new());
    assert_eq!(app.backend_label(), "unconnected");
    let footer = footer_text(&mut app);
    assert!(footer.contains(" unconnected · "), "{footer:?}");
    // No model was named, so the footer leaves the slot out entirely.
    assert!(!footer.contains(" ·  · "), "{footer:?}");

    // A stored key turns the same app into a real client.
    let mut auth = AuthStore::new();
    auth.store("anthropic", Credential::ApiKey { key: "k".into() });
    auth.activate("anthropic");

    let mut app = app_with_real_backends(auth);
    assert_eq!(app.backend_label(), "anthropic");
    let footer = footer_text(&mut app);
    assert!(footer.contains(" anthropic · "), "{footer:?}");
    assert!(footer.contains(app.model()), "{footer:?}");
}

#[tokio::test]
async fn connecting_switches_the_backend_away_from_unconnected() {
    let mut app = app_with_real_backends(AuthStore::new());
    assert_eq!(app.backend_label(), "unconnected");
    assert_eq!(app.model(), "");

    connect_with_api_key(&mut app, "sk-test");
    for _ in 0..10 {
        tokio::task::yield_now().await;
        app.tick();
    }

    // Connecting a provider replaces both the backend and the model: with
    // nothing named, the provider's own first model is adopted.
    assert_eq!(app.backend_label(), "anthropic");
    assert_eq!(
        app.model(),
        app.connection.provider_config.models("anthropic")[0].0
    );
    let expected = app
        .connection
        .provider_config
        .resolve("anthropic", app.model())
        .parameters
        .capabilities()
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    assert_eq!(
        app.sessions
            .agent
            .snapshot()
            .context(app.config.mode)
            .window,
        expected
    );

    // The wizard's model step can still change it.
    app.handle_key(key(KeyCode::Down));
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(
        app.model(),
        app.connection.provider_config.models("anthropic")[1].0
    );
    for _ in 0..10 {
        tokio::task::yield_now().await;
        app.tick();
    }
    // The official catalog also supplies the selected model's limits.
    let expected = app
        .connection
        .provider_config
        .resolve("anthropic", app.model())
        .parameters
        .capabilities()
        .context_window
        .unwrap_or(DEFAULT_CONTEXT_WINDOW);
    assert_eq!(
        app.sessions
            .agent
            .snapshot()
            .context(app.config.mode)
            .window,
        expected
    );
}

#[tokio::test]
async fn a_session_without_credentials_stays_unconnected() {
    let mut app = app_with_real_backends(AuthStore::new());
    app.tick();

    // Nothing to connect: no model is named and no provider answers.
    assert_eq!(app.backend_label(), "unconnected");
    assert_eq!(app.model(), "");
    assert_eq!(
        app.sessions
            .agent
            .snapshot()
            .context(app.config.mode)
            .window,
        DEFAULT_CONTEXT_WINDOW
    );
}

#[test]
fn ctrl_c_twice_quits_while_one_press_only_asks() {
    let mut app = app();
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);

    app.handle_key(ctrl_c);
    assert!(!app.quit.get(), "one press must not quit");
    assert!(app.notifications.current().is_some(), "no hint was shown");

    app.handle_key(ctrl_c);
    assert!(app.quit.get());
}

#[test]
fn a_stale_quit_confirmation_does_not_quit() {
    let mut app = app();
    app.quit_press = Some(('c', Instant::now() - QUIT_CONFIRM_WINDOW));

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(!app.quit.get(), "the window had expired");
}

#[test]
fn ctrl_c_clears_the_prompt_before_it_means_quit() {
    let mut app = app();
    for ch in "half typed".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    assert!(!app.editor.is_empty());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(app.editor.is_empty(), "the prompt survived");
    assert!(!app.quit.get());
}

#[test]
fn ctrl_d_only_quits_from_an_empty_prompt() {
    let ctrl_d = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);

    let mut empty = app();
    empty.handle_key(ctrl_d);
    assert!(!empty.quit.get());
    empty.handle_key(ctrl_d);
    assert!(empty.quit.get());

    // With text in the prompt the key belongs to the editor instead.
    let mut typing = app();
    typing.handle_key(key(KeyCode::Char('x')));
    typing.handle_key(ctrl_d);
    typing.handle_key(ctrl_d);
    assert!(!typing.quit.get());
    assert_eq!(typing.editor.text(), "x");
}

#[tokio::test]
async fn ctrl_c_stops_the_turn_that_is_streaming() {
    let mut app = app();
    app.submit("hello".to_string());
    assert!(app.session.is_streaming());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(!app.session.is_streaming(), "the turn kept streaming");
    assert!(!app.quit.get(), "interrupting must not quit");
    assert!(app.session.turns.iter().all(|turn| turn.complete));
    assert!(app.notifications.current().is_some(), "no notice was shown");
}

#[tokio::test]
async fn interrupting_a_turn_leaves_nothing_waiting_behind_it() {
    let mut app = app();
    app.submit("first".to_string());
    // A second prompt joins the running turn rather than queueing behind it.
    app.submit("second".to_string());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert!(!app.session.is_streaming(), "the turn kept streaming");
    assert_eq!(
        app.session.turns.len(),
        1,
        "an interrupt must not start another turn"
    );
    assert!(app.session.turns.iter().all(|turn| turn.complete));
}

#[tokio::test]
async fn a_prompt_submitted_while_streaming_steers_the_running_turn() {
    let mut app = app();

    app.submit("first".to_string());
    assert!(app.session.is_streaming());
    app.submit("second".to_string());

    // No concurrent turn, and the message is waiting for the loop that is
    // running to read it rather than being run as a turn of its own.
    assert_eq!(app.session.turns.len(), 1, "a concurrent turn was started");
    settle_turn(&mut app).await;
    assert!(
        app.session.turns[0]
            .steering
            .iter()
            .any(|note| note.text == "second")
    );
    assert!(
        app.notifications
            .current()
            .is_some_and(|(_, text)| text.contains("steered")),
        "the user is told what happened to it"
    );
}

#[tokio::test]
async fn a_steered_message_is_recorded_when_the_loop_says_it_was_read() {
    let call = ToolCall::new("call-1", "read", serde_json::json!({}));
    let mut app = app();
    app.session.turns.push(Turn {
        prompt: "read it".into(),
        steps: vec![
            ToolStep {
                call: call.clone(),
                result: Some(ToolResult::ok(&call, "one")),
                duration_ms: None,
                disclosure: Default::default(),
                detail_disclosure: Default::default(),
                subagent: None,
            },
            ToolStep::pending(call),
        ],
        ..Default::default()
    });

    app.apply_event(AgentEvent::Steered {
        text: "also check the tests".into(),
    });

    // The note is recorded against the turn that read it, and remembers how
    // much had already run — only the finished call counts.
    let note = &app.session.turns[0].steering[0];
    assert_eq!(note.text, "also check the tests");
    assert_eq!(note.after_steps, 1);
}

#[test]
fn help_shortcut_opens_an_overlay() {
    let mut app = app();
    app.handle_key(key(KeyCode::F(1)));
    assert_eq!(app.overlay_queue.borrow().len(), 1);
}

#[test]
fn question_mark_opens_help_only_when_the_prompt_is_empty() {
    let mut empty = app();
    empty.handle_key(key(KeyCode::Char('?')));
    assert_eq!(empty.overlay_queue.borrow().len(), 1);

    let mut typing = app();
    typing.handle_key(key(KeyCode::Char('a')));
    typing.handle_key(key(KeyCode::Char('?')));
    assert!(typing.overlay_queue.borrow().is_empty());
}

/// An app that found `skills` before it was built.
fn app_with_skills(skills: Skills) -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.skills = skills;
    App::new(options)
}

/// A skills directory holding one skill, found the way the binary finds it.
fn skills_holding(tag: &str, name: &str, description: &str, body: &str) -> Skills {
    let dir = std::env::temp_dir().join(format!("solaris-skills-{tag}-{}", std::process::id()));
    let path = dir.join(name).join("SKILL.md");
    std::fs::create_dir_all(path.parent().expect("a skill directory")).expect("skills dir");
    std::fs::write(
        &path,
        format!("---\ndescription: {description}\n---\n{body}\n"),
    )
    .expect("the skill was written");
    Skills::load(&[SkillRoot::new(&dir, SkillSource::Project)], &[])
}

/// Tick until the turn in flight has finished, or give up.
async fn settle_turn(app: &mut App) {
    for _ in 0..500 {
        app.tick();
        if !app.session.is_streaming() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
    panic!("the turn never completed");
}

#[tokio::test]
async fn invoking_a_skill_asks_the_model_for_its_instructions() {
    let skills = skills_holding(
        "invoke",
        "pdf-tools",
        "Extract text from PDFs.",
        "Read the reference first.",
    );
    let mut app = app_with_skills(skills);

    app.submit("/skill:pdf-tools extract report.pdf".to_string());

    // The transcript shows what was typed, not the block it stood for.
    assert_eq!(
        app.session.turns[0].prompt,
        "/skill:pdf-tools extract report.pdf"
    );

    settle_turn(&mut app).await;

    let reply = &app.session.turns[0].reply;
    assert!(reply.contains("Read the reference first."), "{reply}");
    assert!(reply.contains("extract report.pdf"), "{reply}");
}

async fn settle_resources(app: &mut App) {
    for _ in 0..1000 {
        app.tick();
        if app.resources.resource_rx.is_none()
            && !app.resources.publication_pending()
            && !app.resources.reload_pending
            && app
                .sessions
                .runtime_pending
                .load(std::sync::atomic::Ordering::Acquire)
                == 0
        {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("resource load did not finish");
}

#[tokio::test]
async fn reload_waits_for_stream_and_keeps_history_and_expanded_queue_content() {
    let skills = skills_holding("reload", "pdf-tools", "PDF work", "Original instructions");
    let path = skills.get("pdf-tools").expect("skill").path().to_path_buf();
    let cwd = path
        .parent()
        .expect("skill directory")
        .parent()
        .expect("root")
        .to_path_buf();
    std::fs::create_dir_all(cwd.join(".git")).expect("repo marker");
    let mut app = app_with_skills(skills);
    app.resources.resource_paths = Some(ResourcePaths {
        cwd: cwd.clone(),
        config_dir: None,
        home_dir: None,
        extra_skills: vec![cwd.clone()],
    });
    app.submit("first".to_string());
    app.submit("/queue /skill:pdf-tools extract report.pdf".to_string());
    app.submit("/reload".to_string());
    assert!(app.resources.reload_pending);
    std::fs::write(
        &path,
        "---\ndescription: Updated PDF work\n---\nNew instructions",
    )
    .expect("update skill");
    std::fs::write(cwd.join("AGENTS.md"), "Reloaded project rules").expect("rules");
    settle_turn(&mut app).await;
    settle_resources(&mut app).await;
    assert!(!app.resources.reload_pending);
    assert_eq!(
        app.resources
            .skills
            .get("pdf-tools")
            .expect("skill")
            .description(),
        "Updated PDF work"
    );
    assert_eq!(
        app.sessions.agent.prompt_context().project_rules[0].content,
        "Reloaded project rules"
    );
    assert_eq!(app.session.turns.len(), 2);
    assert!(
        app.session.turns[1].reply.contains("Original instructions"),
        "already queued messages keep their expanded text"
    );
    assert!(
        app.sessions.agent.conversation_len() >= 4,
        "reload retains conversation"
    );
    app.submit("/skill:pdf-tools use new body".to_string());
    settle_turn(&mut app).await;
    assert!(app.session.turns[2].reply.contains("New instructions"));
    std::fs::remove_file(path).expect("remove skill");
    app.submit("/reload".to_string());
    settle_resources(&mut app).await;
    assert!(app.resources.skills.is_empty());
    assert_eq!(app.session.turns.len(), 3);
    std::fs::remove_dir_all(cwd).expect("cleanup");
}

#[tokio::test]
async fn failed_reload_preserves_previous_resources() {
    let skills = skills_holding("failed-reload", "pdf-tools", "PDF work", "Body");
    let path = skills.get("pdf-tools").expect("skill").path().to_path_buf();
    let mut app = app_with_skills(skills);
    app.resources.resource_paths = Some(ResourcePaths {
        cwd: path.join("missing"),
        config_dir: None,
        home_dir: None,
        extra_skills: Vec::new(),
    });
    app.submit("/reload".to_string());
    settle_resources(&mut app).await;
    assert!(app.resources.skills.get("pdf-tools").is_some());
    assert!(
        app.notifications
            .current()
            .is_some_and(|(kind, text)| kind == NoticeKind::Error && text.contains("not reloaded"))
    );
}

#[test]
fn hidden_skill_is_offered_to_user_but_not_advertised_to_model() {
    let skills = skills_holding("hidden-completion", "manual", "Manual work", "Body");
    let path = skills.get("manual").expect("skill").path().to_path_buf();
    std::fs::write(
        &path,
        "---\ndescription: Manual work\ndisable-model-invocation: true\n---\nBody",
    )
    .expect("skill");
    let skills = Skills::load(&[], &[path]);
    let hints = commands::hints(&skills);
    assert!(
        hints
            .iter()
            .any(|hint| hint.name == "skill:manual" && hint.description.contains("explicit only"))
    );
    assert_eq!(skills.visible().count(), 0);
}

#[tokio::test]
async fn the_skill_prefix_offers_every_skill_by_name() {
    let skills = skills_holding(
        "offered",
        "pdf-tools",
        "Extract text from PDFs.",
        "Read the reference first.",
    );
    let mut app = app_with_skills(skills);

    for character in "/skill".chars() {
        app.handle_key(key(KeyCode::Char(character)));
    }

    let text = rendered_text(&mut app, 100, 30);
    assert!(text.contains("› /skills"), "{text}");
    assert!(
        text.contains("  /skill:pdf-tools [p] Extract text from PDFs."),
        "every skill is offered, with what it is for and where it came from: {text}"
    );
    assert!(
        !text.lines().any(|line| line.trim_end() == "› /skill"),
        "the bare prefix is a prefix, not a row of its own: {text}"
    );
}

#[test]
fn a_skill_named_after_a_space_is_a_reminder_rather_than_a_run() {
    let mut app = app_with_skills(skills_holding(
        "space",
        "pdf-tools",
        "Extract text from PDFs.",
        "Body.",
    ));

    app.submit("/skill pdf-tools do it".to_string());

    assert!(app.session.turns.is_empty());
    let (kind, text) = app.notifications.current().expect("a notice");
    assert_eq!(kind, NoticeKind::Error);
    assert!(text.contains("skill:<name>"), "{text}");
}

#[test]
fn the_skill_prefix_on_its_own_lists_what_there_is() {
    let mut app = app_with_skills(skills_holding(
        "prefix",
        "pdf-tools",
        "Extract text from PDFs.",
        "Body.",
    ));

    app.submit("/skill:".to_string());

    assert_eq!(app.overlay_queue.borrow().len(), 1);
    assert!(app.session.turns.is_empty());
}

#[test]
fn an_unknown_skill_is_reported_rather_than_run() {
    let mut app = app_with_skills(skills_holding(
        "unknown",
        "pdf-tools",
        "Extract text from PDFs.",
        "Body.",
    ));

    app.submit("/skill:nope do it".to_string());

    assert!(app.session.turns.is_empty());
    let (kind, text) = app.notifications.current().expect("a notice");
    assert_eq!(kind, NoticeKind::Error);
    assert!(text.contains("no skill named"), "{text}");
}

#[test]
fn listing_skills_without_any_says_none_were_found() {
    let mut app = app();

    app.submit("/skills".to_string());

    assert!(app.overlay_queue.borrow().is_empty());
    let (kind, text) = app.notifications.current().expect("a notice");
    assert_eq!(kind, NoticeKind::Info);
    assert!(text.contains("no skills found"), "{text}");
}

#[test]
fn listing_skills_shows_the_ones_that_were_found() {
    let mut app = app_with_skills(skills_holding(
        "list",
        "pdf-tools",
        "Extract text from PDFs.",
        "Body.",
    ));

    app.submit("/skills".to_string());

    assert_eq!(app.overlay_queue.borrow().len(), 1);
    assert_eq!(app.resources.skills.len(), 1);
}

#[tokio::test]
async fn typing_then_enter_starts_a_turn_through_the_event_loop() {
    let mut app = app();
    for ch in "hello".chars() {
        app.handle_key(key(KeyCode::Char(ch)));
    }
    app.handle_key(key(KeyCode::Enter));
    assert_eq!(app.session.turns.len(), 1);
    assert_eq!(app.session.turns[0].prompt, "hello");
}

#[tokio::test]
async fn prompt_completes_once_events_are_drained() {
    let mut app = app();
    app.submit("hello".to_string());
    assert!(app.session.is_streaming());

    for _ in 0..500 {
        app.tick();
        if !app.session.is_streaming() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert!(!app.session.is_streaming(), "turn never completed");
    assert!(app.session.turns[0].complete);
    assert!(app.session.turns[0].reply.contains("hello"));
    assert_eq!(app.session.turns[0].usage, Usage::new(4, 2));
}

#[tokio::test]
async fn a_queued_prompt_is_run_as_its_own_turn_when_the_loop_reports_it() {
    let mut app = app();
    app.submit("first".to_string());

    // The loop reports that it has taken the queued prompt and is starting a
    // turn with it: everything that streams from here belongs to that turn.
    app.apply_event(AgentEvent::FollowUp {
        prompt: "run the tests".into(),
    });

    assert_eq!(app.session.turns.len(), 2, "a turn was not opened for it");
    assert!(app.session.turns[0].complete, "the old turn is finished");
    assert_eq!(app.session.turns[1].prompt, "run the tests");
    assert!(!app.session.turns[1].complete, "and the new one is running");
}

#[tokio::test]
async fn queueing_while_streaming_waits_for_the_turn_in_flight() {
    let mut app = app();
    app.submit("first".to_string());

    app.submit("/queue run the tests".to_string());

    // No second turn yet, and no steering either: this is the queue for after.
    assert_eq!(app.session.turns.len(), 1);
    settle_turn(&mut app).await;
    assert_eq!(app.session.turns.len(), 2);
    assert_eq!(app.session.turns[1].prompt, "run the tests");
    assert!(
        !app.sessions.agent.steering_pending(),
        "a queued prompt is not a correction"
    );
    assert!(
        app.notifications
            .current()
            .is_some_and(|(_, text)| text.contains("after this turn")),
        "the user is told when it will run"
    );
}

#[tokio::test]
async fn queueing_with_nothing_in_flight_just_runs_it() {
    let mut app = app();
    app.submit("/queue do it now".to_string());

    assert_eq!(app.session.turns.len(), 1, "it started straight away");
    assert_eq!(app.session.turns[0].prompt, "do it now");
    assert!(!app.sessions.agent.follow_up_pending());
}

#[tokio::test]
async fn queueing_nothing_says_how_to_queue() {
    let mut app = app();
    app.submit("/queue".to_string());

    assert!(app.session.turns.is_empty());
    assert!(
        app.notifications
            .current()
            .is_some_and(|(_, text)| text.contains("/queue <prompt>")),
    );
}

#[tokio::test]
async fn compacting_an_empty_conversation_says_there_was_nothing_to_do() {
    let mut app = app();
    app.submit("/compact".to_string());

    for _ in 0..200 {
        app.tick();
        if app.compact_rx.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(1)).await;
    }

    assert!(app.compact_rx.is_none(), "the answer never came back");
    assert!(
        app.notifications
            .current()
            .is_some_and(|(_, text)| text.contains("nothing to summarize")),
        "an empty conversation has nothing to summarize"
    );
}

#[tokio::test]
async fn a_compaction_is_reported_and_recorded_on_the_turn() {
    let mut app = app();
    app.session.turns.push(Turn {
        prompt: "long one".into(),
        ..Default::default()
    });

    app.apply_event(AgentEvent::Compacted { dropped: 12 });

    assert_eq!(app.session.turns[0].compacted, 12);
    assert!(
        app.notifications
            .current()
            .is_some_and(|(_, text)| text.contains("12")),
        "the user is told the model now remembers less"
    );
}

/// An app whose copies are recorded instead of written to the real
/// clipboard, plus the log they land in. Its clipboard holds no text.
fn app_with_clipboard(succeeds: bool) -> (App, Rc<RefCell<Vec<String>>>) {
    app_with_clipboard_text(None, succeeds)
}

/// The same, with `reads` sitting on the clipboard for a paste.
fn app_with_clipboard_text(reads: Option<&str>, succeeds: bool) -> (App, Rc<RefCell<Vec<String>>>) {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let copied: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

    let log = Rc::clone(&copied);
    let pasted = reads.map(str::to_string);
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.clipboard = Clipboard {
        background: false,
        write: Rc::new(move |text: &str| {
            log.borrow_mut().push(text.to_string());
            succeeds
        }),
        read: Rc::new(move || pasted.clone()),
    };
    (App::new(options), copied)
}

/// An app whose clipboard is a single cell: what a copy writes is what a
/// paste reads back.
fn app_with_live_clipboard() -> App {
    let backend = fake_backend();
    let quit: QuitFlag = Rc::new(Cell::new(false));
    let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
    let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
    let cell: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));

    let writer = Rc::clone(&cell);
    let reader = Rc::clone(&cell);
    let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
    options.clipboard = Clipboard {
        background: false,
        write: Rc::new(move |text: &str| {
            *writer.borrow_mut() = text.to_string();
            true
        }),
        read: Rc::new(move || {
            let text = reader.borrow().clone();
            (!text.is_empty()).then_some(text)
        }),
    };
    App::new(options)
}

/// Drive the app's own selection handle the way `Tui` does: draw a frame,
/// then replay a pointer gesture over it.
fn gesture(app: &mut App, area: Rect, steps: &[(MouseEventKind, (u16, u16))]) {
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);

    let mut selection = app.selection.borrow_mut();
    selection.set_area(area);
    selection.highlight(&mut buf);
    for (kind, (column, row)) in steps {
        let event = MouseEvent {
            kind: *kind,
            column: *column,
            row: *row,
            modifiers: KeyModifiers::empty(),
        };
        selection.handle_mouse(event);
    }
}

/// Press, drag, release — the gesture behind a mouse selection.
fn drag_select(app: &mut App, area: Rect, from: (u16, u16), to: (u16, u16)) {
    gesture(
        app,
        area,
        &[
            (MouseEventKind::Down(MouseButton::Left), from),
            (MouseEventKind::Drag(MouseButton::Left), to),
            (MouseEventKind::Up(MouseButton::Left), to),
        ],
    );
}

#[tokio::test]
async fn ctrl_c_copies_instead_of_cancelling_a_streaming_turn() {
    let (mut app, copied) = app_with_clipboard(true);
    app.submit("hello".to_string());
    assert!(app.session.is_streaming());

    drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (12, 0));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().as_slice(), ["› hello".to_string()]);
    assert!(
        app.session.is_streaming(),
        "a copy must not stop the turn that is running"
    );
    assert!(!app.quit.get(), "copying a selection must not quit");
}

#[tokio::test]
async fn ctrl_c_copies_over_the_wizard_instead_of_cancelling_it() {
    let (mut app, copied) = app_with_clipboard(true);
    app.submit("/connect".to_string());
    assert!(app.connect_open());

    // The transcript is still on screen behind the wizard, and the range can
    // be drawn over either.
    drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (5, 0));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().len(), 1, "the range was not copied");
    assert!(app.connect_open(), "the wizard was cancelled instead");
    assert!(!app.quit.get(), "copying a selection must not quit");
}

#[tokio::test]
async fn ctrl_c_copies_what_was_dragged() {
    let (mut app, copied) = app_with_clipboard(true);
    app.submit("hello world".to_string());

    // The prompt echo is the first transcript row.
    drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (12, 0));
    app.tick();

    assert!(
        copied.borrow().is_empty(),
        "a drag must not copy anything by itself"
    );
    assert!(
        app.selection.borrow().is_active(),
        "the selection outlives the release"
    );

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().as_slice(), ["› hello world".to_string()]);
    assert!(!app.quit.get(), "copying a selection must not quit");
    assert!(
        rendered_text(&mut app, 60, 12).contains("copied 13 characters"),
        "the footer never announced the copy"
    );
}

#[tokio::test]
async fn a_single_cell_drag_copies_that_cell() {
    let (mut app, copied) = app_with_clipboard(true);
    let area = Rect::new(0, 0, 60, 12);

    // The footer opens with a space, so its second cell holds the first
    // character of the model name.
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    let row = area.height - 1;
    let expected = buf[(1, row)].symbol().to_string();
    assert_ne!(expected, " ", "the footer moved");

    // Press, drag and release without ever leaving the cell.
    drag_select(&mut app, area, (1, row), (1, row));
    assert!(app.selection.borrow().is_active());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().as_slice(), [expected]);
    assert!(!app.quit.get(), "a one-cell selection asked to quit");
}

#[tokio::test]
async fn a_blank_selection_is_copied_like_any_other() {
    let (mut app, copied) = app_with_clipboard(true);
    let area = Rect::new(0, 0, 60, 12);

    // Drag over a run of blank cells — wherever this layout keeps them —
    // so the test says nothing about what is on screen.
    let mut buf = Buffer::empty(area);
    app.render(&mut buf, area);
    let blank_run = |row: u16| {
        (0..area.width - 4)
            .find(|start| (*start..*start + 5).all(|x| buf[(x, row)].symbol() == " "))
    };
    let (row, column) = (0..area.height - 1)
        .find_map(|row| blank_run(row).map(|column| (row, column)))
        .expect("the frame has no blank run to drag over");

    drag_select(&mut app, area, (column, row), (column + 4, row));
    assert!(app.selection.borrow().is_active());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().as_slice(), [" ".repeat(5)]);
    assert!(!app.quit.get(), "a blank selection asked to quit");
    assert!(
        app.quit_press.is_none(),
        "a blank selection armed the quit sequence"
    );
}

#[tokio::test]
async fn a_press_without_a_drag_is_not_a_copy() {
    let (mut app, copied) = app_with_clipboard(true);
    app.submit("hello world".to_string());

    // A press and release with no drag in between is a click: it moves the
    // editor's caret and selects nothing.
    let area = Rect::new(0, 0, 60, 12);
    gesture(
        &mut app,
        area,
        &[
            (MouseEventKind::Down(MouseButton::Left), (3, 0)),
            (MouseEventKind::Up(MouseButton::Left), (3, 0)),
        ],
    );
    assert!(!app.selection.borrow().is_active());

    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
    assert!(copied.borrow().is_empty(), "there was nothing to copy");
}

#[tokio::test]
async fn a_clipboard_that_refuses_is_reported() {
    let (mut app, copied) = app_with_clipboard(false);
    app.submit("hello world".to_string());

    drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (12, 0));
    app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));

    assert_eq!(copied.borrow().len(), 1, "the write was attempted");
    assert!(
        rendered_text(&mut app, 60, 12).contains("clipboard"),
        "a failed copy went unreported"
    );
}

#[test]
fn the_selection_is_painted_in_the_theme_colours() {
    let (app, _) = app_with_clipboard(true);
    let theme = Theme::by_name(&app.config.theme);

    let style = app.selection.borrow().style();
    assert_eq!(style.fg, Some(theme.selection_fg));
    assert_eq!(style.bg, Some(theme.selection_bg));
}

#[test]
fn a_theme_switch_repaints_the_selection() {
    let (mut app, _) = app_with_clipboard(true);
    app.set_theme("light");

    let style = app.selection.borrow().style();
    assert_eq!(style.fg, Some(Theme::light().selection_fg));
    assert_eq!(style.bg, Some(Theme::light().selection_bg));
}
