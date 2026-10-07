//! The solaris application: input handling and rendering.
//!
//! `App` is the root [`Component`]. It owns domain-shaped sub-structures
//! (`session`, `editor`, `notifications`, `auth`) instead of one flat field
//! bag, and it never drives the terminal itself: [`crate::App::tick`] drains
//! background channels and the framework owns the event loop.
//!
//! The app opens straight into the session — there is no separate welcome
//! screen. While the transcript is empty the first rows are the welcome box,
//! which scrolls away with the first turn.

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use solaris_backend::AgentBackend;
use solaris_core::{
    AgentEvent, AuthKind, AuthStore, Companion, Config, Credential, Mode, RecentActivity,
    SlashCommand, Soul, TurnRequest, buddy, parse_slash_command, recent,
};
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::editor::{Editor, EditorStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::components::welcome::{WelcomeData, WelcomeEntry};
use solaris_tui::keys::Keybindings;
use solaris_tui::layout::{self, Axis, Entry};
use solaris_tui::overlay::{OverlayOptions, SizeValue};
use solaris_tui::selection::{Selection, SelectionHandle};
use solaris_tui::theme::Theme;
use solaris_tui::tui::{OverlayQueue, QuitFlag};
use solaris_tui::util::{display_width, pad_to_width, rect_contains, truncate_to_width};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use tokio::sync::mpsc::{
    UnboundedReceiver, UnboundedSender, error::TryRecvError, unbounded_channel,
};

use crate::clipboard::{ClipboardWriter, system_writer};
use crate::commands;
use crate::connect::{ConnectFlow, ConnectOutcome, ConnectStep, ConnectSubmit, DeviceAuthEvent};
use crate::dialogs::{
    ConfirmAction, ConfirmDialog, DialogMessage, HINT_SELECT, SelectDialog, TextDialog, help_lines,
    stats_lines,
};
use crate::keymap;
use crate::state::{NoticeKind, NotificationQueue, SessionState, Turn};
use crate::transcript::{SPINNER, TranscriptView};

/// Program name shown in the welcome box title.
const APP_NAME: &str = "solaris";
/// Milliseconds between two idle animation frames of the companion, matching
/// claurst's fidget.
const BUDDY_STEP_MS: u64 = 500;

/// Everything [`App::new`] needs to start.
pub struct AppOptions {
    pub backend: Arc<dyn AgentBackend>,
    pub config: Config,
    /// Credentials already on disk, if any.
    pub auth: AuthStore,
    /// Where credentials are persisted; `None` keeps them session-scoped.
    pub auth_path: Option<PathBuf>,
    /// Program version, shown in the welcome box title.
    pub version: String,
    /// Greeting line for the welcome box.
    pub greeting: String,
    /// The companion drawn in the welcome box.
    pub buddy: Companion,
    /// Where the companion's name is persisted; `None` keeps it session-scoped.
    pub buddy_path: Option<PathBuf>,
    /// The tip shown under "Tips for getting started".
    pub tip: String,
    /// Prompts recorded by earlier sessions.
    pub recent: RecentActivity,
    /// Where recent activity is persisted; `None` keeps it session-scoped.
    pub recent_path: Option<PathBuf>,
    pub quit: QuitFlag,
    pub overlay_queue: OverlayQueue,
    pub overlay_flag: Rc<Cell<bool>>,
    /// The screen selection the framework tracks for a drag.
    pub selection: SelectionHandle,
    /// Where a finished selection is copied.
    pub clipboard: ClipboardWriter,
}

impl AppOptions {
    /// Options with no stored credentials and no persistence — handy in tests.
    pub fn new(
        backend: Arc<dyn AgentBackend>,
        config: Config,
        quit: QuitFlag,
        overlay_queue: OverlayQueue,
        overlay_flag: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            backend,
            config,
            auth: AuthStore::new(),
            auth_path: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            greeting: "Welcome back!".to_string(),
            buddy: Companion::new("solaris", None),
            buddy_path: None,
            tip: solaris_core::tips::select(0).content.to_string(),
            recent: RecentActivity::new(),
            recent_path: None,
            quit,
            overlay_queue,
            overlay_flag,
            selection: Selection::handle(),
            clipboard: system_writer(),
        }
    }
}

/// The solaris application root component.
pub struct App {
    pub(crate) backend: Arc<dyn AgentBackend>,
    pub(crate) config: Config,
    pub(crate) theme: Theme,
    pub(crate) session: SessionState,
    pub(crate) notifications: NotificationQueue,
    transcript: TranscriptView,
    editor: Editor,
    keybindings: Keybindings,
    quit: QuitFlag,
    overlay_queue: OverlayQueue,
    overlay_flag: Rc<Cell<bool>>,
    /// The screen selection the framework tracks, shared with `Tui`.
    selection: SelectionHandle,
    /// Where a finished selection is copied.
    clipboard: ClipboardWriter,
    dialog_tx: UnboundedSender<DialogMessage>,
    dialog_rx: UnboundedReceiver<DialogMessage>,
    events_rx: Option<UnboundedReceiver<AgentEvent>>,
    /// Prompts typed while a turn was streaming; run in order afterwards.
    queued_prompts: VecDeque<String>,
    spinner_frame: u16,
    frame: u64,
    last_transcript: Rect,
    /// Credentials collected by `/connect`.
    pub(crate) auth: AuthStore,
    /// Where credentials are saved after a change.
    auth_path: Option<PathBuf>,
    /// The companion shown in the welcome box and by `/buddy`.
    pub(crate) buddy: Companion,
    /// Where the companion's name is saved once it has one.
    buddy_path: Option<PathBuf>,
    /// Program version, greeting and tip for the welcome box.
    version: String,
    greeting: String,
    tip: String,
    /// Prompts recorded by earlier sessions, and where they are saved.
    recent: RecentActivity,
    recent_path: Option<PathBuf>,
    /// Idle animation step of the companion, counted in 500 ms beats.
    buddy_step: u64,
    buddy_started: Instant,
    /// The inline `/connect` wizard while it owns the prompt region.
    inline: Option<ConnectFlow>,
    /// Events from the background device-auth task.
    device_rx: Option<UnboundedReceiver<DeviceAuthEvent>>,
}

impl App {
    /// Build the application.
    pub fn new(options: AppOptions) -> Self {
        let AppOptions {
            backend,
            config,
            auth,
            auth_path,
            version,
            greeting,
            buddy,
            buddy_path,
            tip,
            recent,
            recent_path,
            quit,
            overlay_queue,
            overlay_flag,
            selection,
            clipboard,
        } = options;

        let theme = Theme::by_name(&config.theme);
        selection
            .borrow_mut()
            .set_style(theme.selection_fg, theme.selection_bg);
        let (dialog_tx, dialog_rx) = unbounded_channel();

        let mut editor = Editor::with_placeholder("Ask anything…  /help for commands");
        editor.set_hints(commands::hints());
        editor.set_styles(editor_styles(&theme));

        Self {
            backend,
            config,
            theme,
            session: SessionState::new(),
            notifications: NotificationQueue::new(),
            transcript: TranscriptView::new(),
            editor,
            keybindings: keymap::default_bindings(),
            quit,
            overlay_queue,
            overlay_flag,
            selection,
            clipboard,
            dialog_tx,
            dialog_rx,
            events_rx: None,
            queued_prompts: VecDeque::new(),
            spinner_frame: 0,
            frame: 0,
            last_transcript: Rect::default(),
            auth,
            auth_path,
            buddy,
            buddy_path,
            version,
            greeting,
            tip,
            recent,
            recent_path,
            buddy_step: 0,
            buddy_started: Instant::now(),
            inline: None,
            device_rx: None,
        }
    }

    /// The active provider, once one has been connected.
    pub fn active_provider(&self) -> Option<&str> {
        self.auth.active_provider()
    }

    /// Whether the `/connect` wizard is on screen.
    pub fn connect_open(&self) -> bool {
        self.inline.is_some()
    }

    /// The wizard's current step, when it is open.
    pub fn connect_step(&self) -> Option<ConnectStep> {
        self.inline.as_ref().map(|flow| flow.step())
    }

    /// The active agent mode.
    pub fn mode(&self) -> Mode {
        self.config.mode
    }

    /// The active theme name.
    pub fn theme_name(&self) -> &str {
        &self.config.theme
    }

    /// The active model name.
    pub fn model(&self) -> &str {
        &self.config.model
    }

    /// Whether an overlay is currently visible.
    pub fn overlay_open(&self) -> bool {
        self.overlay_flag.get()
    }

    /// Frames rendered so far.
    pub fn frame_count(&self) -> u64 {
        self.frame
    }

    // ------------------------------------------------------------------- input

    fn handle_session_key(&mut self, key: KeyEvent) -> KeyResult {
        // Global shortcuts work regardless of the editor contents.
        if self.keybindings.matches("palette", &key) {
            self.open_palette();
            return KeyResult::Handled;
        }
        if self.keybindings.matches("help", &key)
            || (self.editor.is_empty() && key.code == KeyCode::Char('?'))
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
        let page = self.last_transcript.height.max(1) as i32;
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

        let result = self.editor.handle_key(key);

        if let Some(text) = self.editor.take_submitted() {
            self.submit(text);
            return KeyResult::Handled;
        }

        if result.is_handled() {
            return result;
        }

        // The editor ignores Tab when its completion popup is closed.
        if self.keybindings.matches("toggle_mode", &key) {
            self.toggle_mode();
            return KeyResult::Handled;
        }

        result
    }

    fn scroll_transcript(&mut self, delta: i32) {
        self.session.scroll = (self.session.scroll as i32 + delta).max(0) as u16;
        self.session.follow_end = false;
    }

    /// Copy a selection the user just dragged out, and say so either way.
    fn copy_selection(&mut self, text: &str) {
        let characters = text.chars().count();
        if characters == 0 {
            return;
        }

        if (self.clipboard)(text) {
            self.notifications
                .info(format!("copied {characters} characters"));
        } else {
            self.notifications
                .warning("could not reach the system clipboard");
        }
    }

    fn toggle_mode(&mut self) {
        self.config.mode = self.config.mode.next();
        self.notifications
            .info(format!("{} mode", self.config.mode.label()));
    }

    fn cycle_theme(&mut self) {
        let next = Theme::cycle_name(&self.config.theme);
        self.set_theme(next);
    }

    fn set_theme(&mut self, name: &str) {
        let theme = Theme::by_name(name);
        self.config.theme = theme.name.clone();
        self.theme = theme;
        self.editor.set_styles(editor_styles(&self.theme));
        self.selection
            .borrow_mut()
            .set_style(self.theme.selection_fg, self.theme.selection_bg);
        self.transcript.invalidate();
        if let Some(flow) = self.inline.as_mut() {
            flow.set_theme(&self.theme);
        }
        self.notifications
            .info(format!("theme: {}", self.config.theme));
    }

    fn set_model(&mut self, name: &str) {
        self.config.model = name.to_string();
        self.notifications
            .info(format!("model: {}", self.config.model));
    }

    fn toggle_thinking(&mut self) {
        let expand = !self.session.turns.iter().any(|turn| turn.thinking_expanded);
        for turn in &mut self.session.turns {
            turn.thinking_expanded = expand;
        }
        self.session.bump();
    }

    // ---------------------------------------------------------------- commands

    /// Submit prompt text: a slash command runs, anything else starts a turn.
    ///
    /// Prompts submitted while a turn is streaming are queued and run in order
    /// once it finishes; starting a second turn concurrently would attribute
    /// the in-flight deltas to the wrong turn.
    pub(crate) fn submit(&mut self, text: String) {
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }

        // A fresh prompt is the natural point to drop stale banners.
        self.notifications.clear();

        if let Some(command) = parse_slash_command(&trimmed) {
            self.execute_command(command);
            return;
        }

        // Only real prompts count as activity, not slash commands.
        self.record_activity(&trimmed);

        if self.session.is_streaming() {
            self.queued_prompts.push_back(trimmed.clone());
            self.notifications.info(format!(
                "queued: {}",
                crate::transcript::summarize(&trimmed)
            ));
            return;
        }

        self.start_turn(trimmed);
    }

    fn execute_command(&mut self, command: SlashCommand) {
        let args = command.args.trim();
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
                    self.open_model_dialog();
                } else {
                    self.set_model(args);
                }
            }
            "mode" => self.toggle_mode(),
            "clear" => self.open_clear_confirm(),
            "stats" => self.open_stats(),
            "buddy" => self.open_buddy(args),
            "quit" | "exit" => self.quit.set(true),
            other => self
                .notifications
                .error(format!("unknown command /{other} — try /help")),
        }
    }

    fn start_turn(&mut self, prompt: String) {
        let request = TurnRequest {
            history: self.session.history(self.config.mode),
            prompt: prompt.clone(),
            mode: self.config.mode,
        };

        self.session.turns.push(Turn {
            prompt,
            ..Default::default()
        });
        self.session.follow_end = true;
        self.session.bump();
        self.spinner_frame = 0;

        let (tx, rx) = unbounded_channel();
        self.events_rx = Some(rx);

        let backend = Arc::clone(&self.backend);
        tokio::spawn(async move {
            use futures::StreamExt;
            match backend.run_turn(request).await {
                Ok(mut stream) => {
                    while let Some(event) = stream.next().await {
                        if tx.send(event).is_err() {
                            break;
                        }
                    }
                }
                Err(error) => {
                    let _ = tx.send(AgentEvent::Error(error.to_string()));
                }
            }
        });
    }

    fn apply_event(&mut self, event: AgentEvent) {
        match event {
            AgentEvent::ThinkingDelta(chunk) => {
                if let Some(turn) = self.session.active_turn_mut() {
                    turn.thinking.push_str(&chunk);
                }
            }
            AgentEvent::TextDelta(chunk) => {
                if let Some(turn) = self.session.active_turn_mut() {
                    turn.reply.push_str(&chunk);
                }
            }
            AgentEvent::Status(text) => self.session.status = Some(text),
            AgentEvent::TurnComplete { tokens, cost_usd } => {
                if let Some(turn) = self.session.active_turn_mut() {
                    turn.complete = true;
                    turn.tokens = tokens;
                    turn.cost_usd = cost_usd;
                }
                self.session.status = None;
            }
            AgentEvent::Error(message) => {
                if let Some(turn) = self.session.active_turn_mut() {
                    turn.complete = true;
                    if turn.reply.trim().is_empty() {
                        turn.reply = format!("**Error:** {message}");
                    }
                }
                self.session.status = None;
                self.notifications.error(message);
            }
        }
        self.session.bump();
    }

    fn on_dialog_message(&mut self, message: DialogMessage) {
        match message {
            DialogMessage::Cancelled => {}
            DialogMessage::Theme(name) => self.set_theme(&name),
            DialogMessage::Model(name) => self.set_model(&name),
            DialogMessage::Command(text) => self.submit(text),
            DialogMessage::Confirm {
                action: ConfirmAction::ClearTranscript,
                accepted: true,
            } => {
                self.session.turns.clear();
                self.session.scroll = 0;
                self.session.follow_end = true;
                self.session.bump();
                self.notifications.info("transcript cleared");
            }
            DialogMessage::Confirm {
                accepted: false, ..
            } => {}
        }
    }

    // ----------------------------------------------------------------- dialogs

    fn push_overlay(&mut self, component: Box<dyn Component>, options: OverlayOptions) {
        self.overlay_queue.borrow_mut().push((component, options));
    }

    fn open_help(&mut self) {
        let lines = help_lines(&self.theme, solaris_core::PROMPT_SLASH_COMMANDS);
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

    fn open_stats(&mut self) {
        let lines = stats_lines(
            &self.theme,
            &self.session,
            &self.config.model,
            self.config.mode,
            self.config.context_window,
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
    fn open_buddy(&mut self, args: &str) {
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

    fn open_palette(&mut self) {
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

    fn open_theme_dialog(&mut self) {
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

    fn open_model_dialog(&mut self) {
        let current = self.config.model.clone();
        let items = Config::MODEL_OPTIONS
            .iter()
            .map(|(name, description)| SelectItem::new(*name, *name).description(*description))
            .collect::<Vec<_>>();
        let count = Config::MODEL_OPTIONS.len();
        let dialog = SelectDialog::new(
            "Model",
            HINT_SELECT,
            items,
            &self.theme,
            self.dialog_tx.clone(),
            |value| DialogMessage::Model(value.to_string()),
        )
        .label("Select a model:")
        .selected(&current);
        self.push_overlay(
            Box::new(dialog),
            OverlayOptions::centered()
                .width(SizeValue::Percent(66))
                .height(SizeValue::Cells(SelectDialog::height_for(count))),
        );
    }

    fn open_clear_confirm(&mut self) {
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

    /// Open the `/connect` wizard in the prompt region.
    fn open_connect(&mut self) {
        self.inline = Some(ConnectFlow::new(&self.theme));
        self.device_rx = None;
        self.notifications.clear();
    }

    /// Drop the wizard and any device-auth task feeding it.
    fn close_connect(&mut self) {
        self.inline = None;
        self.device_rx = None;
    }

    fn handle_inline_key(&mut self, key: KeyEvent) {
        let outcome = match self.inline.as_mut() {
            Some(flow) => flow.on_key(key),
            None => return,
        };
        self.apply_connect_outcome(outcome);
    }

    fn apply_connect_outcome(&mut self, outcome: ConnectOutcome) {
        match outcome {
            ConnectOutcome::Handled => {}
            ConnectOutcome::Closed => self.close_connect(),
            ConnectOutcome::ProviderPicked { id, name } => self.begin_provider_setup(&id, &name),
            ConnectOutcome::Submit(submit) => self.apply_connect_submit(submit),
            ConnectOutcome::ModelPicked { model_id } => {
                self.config.model = model_id.clone();
                self.notifications.info(format!("model: {model_id}"));
                self.close_connect();
            }
        }
    }

    /// Route a picked provider to the step its auth kind needs.
    fn begin_provider_setup(&mut self, provider_id: &str, name: &str) {
        let Some(spec) = solaris_core::provider(provider_id) else {
            self.notifications
                .error(format!("unknown provider {provider_id}"));
            self.close_connect();
            return;
        };

        let id = provider_id.to_string();
        let provider_name = name.to_string();

        match spec.auth {
            // Nothing to collect: activate it straight away.
            AuthKind::Local => {
                self.apply_connect_outcome(ConnectOutcome::Submit(ConnectSubmit::Local {
                    provider_id: id,
                    provider_name,
                }));
            }
            AuthKind::ApiKey => {
                if let Some(flow) = self.inline.as_mut() {
                    flow.enter_api_key(id, provider_name);
                }
            }
            AuthKind::ApiKeyWithUrl => {
                let current =
                    self.auth
                        .credential(provider_id)
                        .and_then(|credential| match credential {
                            Credential::Endpoint { base_url, .. } => Some(base_url.clone()),
                            _ => None,
                        });
                if let Some(flow) = self.inline.as_mut() {
                    flow.enter_custom_provider(id, provider_name, current);
                }
            }
            AuthKind::DeviceCode => {
                self.device_rx = Some(spawn_device_auth(provider_id));
                if let Some(flow) = self.inline.as_mut() {
                    flow.enter_device_auth(id, provider_name);
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
            ConnectSubmit::DeviceAuthToken {
                provider_id,
                provider_name,
                token,
            } => (
                provider_id,
                provider_name,
                Some(Credential::Token { token }),
            ),
        };

        if let Some(credential) = credential {
            self.auth.store(provider_id.clone(), credential);
        }
        self.auth.activate(provider_id.clone());
        self.persist_auth();
        self.notifications
            .info(format!("connected to {provider_name}"));

        // Continue into the model picker, the way claurst's wizard does.
        let models: Vec<SelectItem> = solaris_core::provider(&provider_id)
            .map(|spec| {
                spec.models
                    .iter()
                    .map(|(id, description)| SelectItem::new(*id, *id).description(*description))
                    .collect()
            })
            .unwrap_or_default();

        match self.inline.as_mut() {
            Some(flow) if !models.is_empty() => flow.enter_models(models),
            _ => self.close_connect(),
        }
    }

    /// Best-effort persistence of the credential store.
    fn persist_auth(&mut self) {
        let Some(path) = self.auth_path.clone() else {
            return;
        };

        let written = self
            .auth
            .to_json()
            .map_err(|error| error.to_string())
            .and_then(|json| {
                if let Some(parent) = path.parent() {
                    std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
                }
                std::fs::write(&path, json).map_err(|error| error.to_string())
            });

        if let Err(error) = written {
            self.notifications
                .warning(format!("could not save credentials: {error}"));
        }
    }

    /// Record a submitted prompt and persist the history, best-effort.
    fn record_activity(&mut self, prompt: &str) {
        self.recent.record(prompt, recent::now_ms());
        self.persist_recent();
    }

    fn persist_recent(&mut self) {
        let Some(path) = self.recent_path.clone() else {
            return;
        };

        let written = self
            .recent
            .to_json()
            .map_err(|error| error.to_string())
            .and_then(|json| write_json(&path, &json));

        if let Err(error) = written {
            self.notifications
                .warning(format!("could not save recent activity: {error}"));
        }
    }

    /// Best-effort persistence of the companion's name.
    fn persist_buddy(&mut self) {
        let Some(soul) = self.buddy.soul.clone() else {
            return;
        };
        let Some(path) = self.buddy_path.clone() else {
            return;
        };

        let written = soul
            .to_json()
            .map_err(|error| error.to_string())
            .and_then(|json| write_json(&path, &json));

        if let Err(error) = written {
            self.notifications
                .warning(format!("could not save the companion: {error}"));
        }
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
            recent: self
                .recent
                .rows(recent::now_ms())
                .into_iter()
                .map(|(label, when)| WelcomeEntry { label, when })
                .collect(),
        }
    }

    // ------------------------------------------------------------------ render

    /// Render the transcript, the prompt region, and the status footer.
    fn render_session(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width < 12 || area.height < 4 {
            return;
        }

        let max_editor = area.height.saturating_sub(2).max(3);
        // The inline wizard takes over the prompt region, the way claurst's
        // `/connect` does: it is not an overlay, it replaces the input line.
        let prompt_height = match self.inline.as_mut() {
            Some(flow) => flow.desired_height(area.width),
            None => self.editor.desired_height(area.width).unwrap_or(3),
        };
        let prompt_height = prompt_height.clamp(3, max_editor);

        let rows = layout::split(
            area,
            Axis::Vertical,
            &[
                Entry::auto().grow(1).min(1),
                Entry::px(prompt_height),
                Entry::px(1),
            ],
        );

        self.render_transcript(buf, rows[0]);
        match self.inline.as_mut() {
            Some(flow) => flow.render(buf, rows[1]),
            None => {
                self.editor.set_focused(!self.overlay_open());
                self.editor.render(buf, rows[1]);
            }
        }
        self.render_footer(buf, rows[2]);
    }

    fn render_transcript(&mut self, buf: &mut Buffer, area: Rect) {
        self.last_transcript = area;
        if area.width == 0 || area.height == 0 {
            return;
        }

        let spinner = self.session.is_streaming().then_some(self.spinner_frame);
        let welcome = self.welcome_data();
        let lines = self
            .transcript
            .lines(&self.session, area, &self.theme, spinner, &welcome);

        let visible = area.height as usize;
        let max_scroll = lines.len().saturating_sub(visible) as u16;

        if self.session.follow_end {
            self.session.scroll = max_scroll;
        }
        self.session.scroll = self.session.scroll.min(max_scroll);
        // Re-arm following once the view is back at the bottom.
        if self.session.scroll >= max_scroll {
            self.session.follow_end = true;
        }

        for (row, line) in lines
            .iter()
            .skip(self.session.scroll as usize)
            .take(visible)
            .enumerate()
        {
            buf.set_line(area.x, area.y + row as u16, line, area.width);
        }
    }

    fn render_footer(&mut self, buf: &mut Buffer, area: Rect) {
        if area.width == 0 || area.height == 0 {
            return;
        }

        let (right, style) = if let Some((kind, text)) = self.notifications.current() {
            let style = match kind {
                NoticeKind::Error => Style::default()
                    .fg(self.theme.error)
                    .add_modifier(Modifier::BOLD),
                NoticeKind::Warning => Style::default().fg(self.theme.warning),
                NoticeKind::Info => Style::default().fg(self.theme.muted),
            };
            (format!("{} {} ", kind.marker(), text), style)
        } else if self.session.is_streaming() {
            let glyph = SPINNER[self.spinner_frame as usize % SPINNER.len()];
            let status = self.session.status.as_deref().unwrap_or("working");
            (
                format!("{glyph} {status} "),
                Style::default().fg(self.theme.accent),
            )
        } else {
            (
                format!("{} ", keymap::FOOTER_HINT),
                Style::default().fg(self.theme.dim),
            )
        };

        // The right slot wins the space it needs; the left one is truncated to
        // whatever remains so the two never overlap.
        let right_width = display_width(&right) as u16;
        let show_right = right_width + 4 < area.width;
        let left_budget = if show_right {
            area.width.saturating_sub(right_width)
        } else {
            area.width
        };

        let left = format!(
            " {} · {} · {} tok · ${:.4} ",
            self.config.model,
            self.config.mode.label(),
            self.session.total_tokens(),
            self.session.total_cost()
        );
        // A connected provider is prepended so the footer says where requests
        // would go, even though the backend is still the mock one.
        let left = match self.auth.active_provider() {
            Some(provider) => format!(" {provider} · {}", &left[1..]),
            None => left,
        };
        buf.set_string(
            area.x,
            area.y,
            truncate_to_width(&left, left_budget as usize, "…"),
            Style::default().fg(self.theme.muted),
        );

        if show_right {
            buf.set_string(
                area.x + area.width - right_width,
                area.y,
                truncate_to_width(&right, area.width as usize, "…"),
                style,
            );
        }
    }
}

impl Component for App {
    fn render(&mut self, buf: &mut Buffer, area: Rect) {
        self.frame = self.frame.wrapping_add(1);
        self.render_session(buf, area);
    }

    fn handle_key(&mut self, key: KeyEvent) -> KeyResult {
        // The inline wizard owns the keyboard while it is up — including
        // Ctrl+C, which cancels the wizard rather than the application.
        if self.inline.is_some() {
            self.handle_inline_key(key);
            return KeyResult::Handled;
        }

        if self.keybindings.matches("quit", &key) {
            self.quit.set(true);
            return KeyResult::Handled;
        }

        self.handle_session_key(key)
    }

    fn handle_mouse(&mut self, event: MouseEvent) -> MouseResult {
        // The wizard only takes mouse events inside its own area; the
        // transcript keeps scrolling everywhere else.
        let over_wizard = self
            .inline
            .as_ref()
            .is_some_and(|flow| rect_contains(flow.area(), event.column, event.row));
        if over_wizard {
            let outcome = match self.inline.as_mut() {
                Some(flow) => flow.on_mouse(event),
                None => ConnectOutcome::Handled,
            };
            self.apply_connect_outcome(outcome);
            return MouseResult::Handled;
        }

        if rect_contains(self.last_transcript, event.column, event.row) {
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
        self.editor.handle_mouse(event)
    }

    fn handle_paste(&mut self, text: &str) -> KeyResult {
        // The wizard takes a paste only when its current step has a field;
        // otherwise the payload is dropped rather than leaking into the prompt.
        if let Some(flow) = self.inline.as_mut() {
            flow.insert_paste(text);
            return KeyResult::Handled;
        }

        self.editor.handle_paste(text);
        KeyResult::Handled
    }

    fn tick(&mut self) -> bool {
        let mut dirty = false;

        // A drag that finished since the last frame is waiting in the shared
        // handle. Taking it here rather than in a mouse handler means the copy
        // happens whoever consumed the event — the app or a dialog on top.
        let selected = self.selection.borrow_mut().take_finalized();
        if let Some(text) = selected {
            self.copy_selection(&text);
            dirty = true;
        }

        // Drain backend events without holding a borrow across the handler.
        let mut events = Vec::new();
        let mut disconnected = false;
        if let Some(rx) = self.events_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }

        let terminal = events.iter().any(AgentEvent::is_terminal);
        for event in events {
            dirty = true;
            self.apply_event(event);
        }

        if terminal || disconnected {
            self.events_rx = None;
            // A stream that ends without a terminal event must still stop the spinner.
            if let Some(turn) = self.session.active_turn_mut() {
                turn.complete = true;
                self.session.status = None;
                self.session.bump();
            }
            dirty = true;

            // Run the next queued prompt now that the turn is over.
            if let Some(next) = self.queued_prompts.pop_front() {
                self.start_turn(next);
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

        // Device-auth progress from the background task.
        let mut device_events = Vec::new();
        let mut device_done = false;
        if let Some(rx) = self.device_rx.as_mut() {
            loop {
                match rx.try_recv() {
                    Ok(event) => device_events.push(event),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        device_done = true;
                        break;
                    }
                }
            }
        }
        if device_done {
            self.device_rx = None;
        }
        for event in device_events {
            dirty = true;
            if let Some(flow) = self.inline.as_mut() {
                match event {
                    DeviceAuthEvent::GotCode {
                        user_code,
                        verification_uri,
                    } => flow.device_set_code(user_code, verification_uri),
                    DeviceAuthEvent::TokenReceived(token) => flow.device_set_success(token),
                    DeviceAuthEvent::Error(message) => flow.device_set_error(message),
                }
            }
        }

        if self.notifications.tick() {
            dirty = true;
        }
        if self.session.is_streaming() {
            self.spinner_frame = self.spinner_frame.wrapping_add(1);
            if self.spinner_frame % 6 == 0 {
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

/// Write `json` to `path`, creating the directory when needed.
fn write_json(path: &Path, json: &str) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    std::fs::write(path, json).map_err(|error| error.to_string())
}

/// Lines for the `/buddy` card: the sprite with its name and face, then every
/// trait.
fn buddy_lines(companion: &Companion, theme: &Theme) -> Vec<Line<'static>> {
    let heading = Style::default()
        .fg(theme.heading)
        .add_modifier(Modifier::BOLD);
    let label = Style::default().fg(theme.muted);
    let value = Style::default().fg(theme.fg);
    let accent = Style::default().fg(theme.accent);

    let sprite = buddy::render_lines(&companion.bones, 0);
    let sprite_width = sprite
        .iter()
        .map(|row| display_width(row))
        .max()
        .unwrap_or(0)
        + 4;

    let mut lines = Vec::new();
    for (index, row) in sprite.iter().enumerate() {
        let mut spans = vec![Span::styled(
            pad_to_width(&format!("  {row}"), sprite_width),
            accent,
        )];
        match index {
            0 => spans.push(Span::styled(companion.display_name().to_string(), heading)),
            1 => spans.push(Span::styled(buddy::render_face(&companion.bones), accent)),
            _ => {}
        }
        lines.push(Line::from(spans));
    }

    lines.push(Line::default());

    // Everything but the trait summary, which is split over two rows so it fits
    // the card's width instead of being cut mid-value.
    let mut card = companion.card().into_iter().skip(1);
    for _ in 0..5 {
        if let Some((name, text)) = card.next() {
            lines.push(Line::from(vec![
                Span::styled(format!("  {:<10}", format!("{name}:")), label),
                Span::styled(text, value),
            ]));
        }
    }

    let traits: Vec<String> = companion
        .bones
        .stats
        .rows()
        .iter()
        .map(|(name, value)| format!("{name} {value}"))
        .collect();
    for (index, chunk) in traits.chunks(3).enumerate() {
        let name = if index == 0 { "stats:" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(format!("  {name:<10}"), label),
            Span::styled(chunk.join(" · "), value),
        ]));
    }

    lines
}

/// One-line description shown after the ` · ` separator in the theme picker.
fn theme_description(name: &str) -> &'static str {
    match name {
        "dark" => "dark background",
        "light" => "light background",
        _ => "",
    }
}

/// How long the stand-in device-auth task "waits for the code".
const DEVICE_CODE_DELAY: Duration = Duration::from_millis(300);
/// How long it "polls" before reporting success.
const DEVICE_POLL_DELAY: Duration = Duration::from_millis(1_500);

/// Start the device-code task for `provider_id`.
///
/// The network half of OAuth is not implemented — no provider can actually be
/// contacted yet — so this stands in with the same event shape a real
/// implementation emits: a code is issued, then a token arrives. It is the
/// single seam to replace when a provider client lands.
fn spawn_device_auth(provider_id: &str) -> UnboundedReceiver<DeviceAuthEvent> {
    let (tx, rx) = unbounded_channel();
    let verification_uri = format!("https://{provider_id}.example.test/device");

    tokio::spawn(async move {
        tokio::time::sleep(DEVICE_CODE_DELAY).await;
        let issued = tx.send(DeviceAuthEvent::GotCode {
            user_code: "SO-4F7Q-9X2M".to_string(),
            verification_uri,
        });
        if issued.is_err() {
            return;
        }

        tokio::time::sleep(DEVICE_POLL_DELAY).await;
        let _ = tx.send(DeviceAuthEvent::TokenReceived(
            "simulated-oauth-token".to_string(),
        ));
    });

    rx
}

/// Editor styles derived from the active theme.
pub fn editor_styles(theme: &Theme) -> EditorStyles {
    EditorStyles {
        border: Style::default().fg(theme.border),
        border_focused: Style::default().fg(theme.accent),
        prompt: Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        text: Style::default().fg(theme.fg),
        placeholder: Style::default().fg(theme.dim),
        completion: Style::default().fg(theme.muted),
        completion_selected: Style::default()
            .fg(theme.accent)
            .add_modifier(Modifier::BOLD),
        cursor: Style::default().add_modifier(Modifier::REVERSED),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connect::DeviceAuthStatus;
    use crossterm::event::MouseButton;
    use solaris_backend::MockBackend;
    use std::cell::RefCell;
    use std::time::Duration;

    fn app() -> App {
        let backend = Arc::new(MockBackend::with_delay(Duration::ZERO));
        let quit: QuitFlag = Rc::new(Cell::new(false));
        let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
        let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        App::new(AppOptions::new(
            backend,
            Config::default(),
            quit,
            queue,
            flag,
        ))
    }

    /// An app that persists credentials to `path`.
    fn app_with_auth_path(path: std::path::PathBuf) -> App {
        let backend = Arc::new(MockBackend::with_delay(Duration::ZERO));
        let quit: QuitFlag = Rc::new(Cell::new(false));
        let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
        let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
        options.auth_path = Some(path);
        App::new(options)
    }

    /// An app that persists recent activity to `path`.
    fn app_with_recent_path(path: std::path::PathBuf) -> App {
        let backend = Arc::new(MockBackend::with_delay(Duration::ZERO));
        let quit: QuitFlag = Rc::new(Cell::new(false));
        let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
        let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
        options.recent_path = Some(path);
        App::new(options)
    }

    /// An app whose companion is named and persisted to `path`.
    fn app_with_buddy_path(path: std::path::PathBuf) -> App {
        let backend = Arc::new(MockBackend::with_delay(Duration::ZERO));
        let quit: QuitFlag = Rc::new(Cell::new(false));
        let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
        let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
        options.buddy_path = Some(path);
        App::new(options)
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

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
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
        assert_eq!(app.mode(), Mode::Build);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.mode(), Mode::Plan);
        app.handle_key(key(KeyCode::Tab));
        assert_eq!(app.mode(), Mode::Build);
    }

    #[test]
    fn slash_mode_command_toggles_mode() {
        let mut app = app();
        app.submit("/mode".to_string());
        assert_eq!(app.mode(), Mode::Plan);
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
        app.submit("/model solaris-mock-reason".to_string());
        assert_eq!(app.theme_name(), "light");
        assert_eq!(app.model(), "solaris-mock-reason");
    }

    #[test]
    fn model_dialog_options_come_from_the_model_table() {
        // The picker must offer exactly the models the config advertises.
        assert_eq!(Config::MODEL_OPTIONS.len(), 3);
        for (name, description) in Config::MODEL_OPTIONS {
            assert!(!name.is_empty());
            assert!(!description.is_empty(), "{name} has no picker description");
        }
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
    fn bare_commands_queue_dialogs() {
        let mut app = app();
        app.submit("/theme".to_string());
        assert_eq!(app.overlay_queue.borrow().len(), 1);
        app.submit("/model".to_string());
        assert_eq!(app.overlay_queue.borrow().len(), 2);
        app.submit("/stats".to_string());
        assert_eq!(app.overlay_queue.borrow().len(), 3);
    }

    #[test]
    fn dialog_choice_updates_theme_and_model() {
        let mut app = app();
        app.on_dialog_message(DialogMessage::Theme("light".into()));
        app.on_dialog_message(DialogMessage::Model("solaris-mock-1-mini".into()));
        assert_eq!(app.theme_name(), "light");
        assert_eq!(app.model(), "solaris-mock-1-mini");
    }

    #[test]
    fn confirm_message_clears_the_transcript() {
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
        assert!(app.session.turns.is_empty());
    }

    #[test]
    fn palette_choice_runs_the_command() {
        let mut app = app();
        app.on_dialog_message(DialogMessage::Command("/mode".into()));
        assert_eq!(app.mode(), Mode::Plan);
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
            tokens: 7,
            cost_usd: 0.001,
        });

        let turn = &app.session.turns[0];
        assert_eq!(turn.thinking, "hmm ");
        assert_eq!(turn.reply, "hello world");
        assert_eq!(turn.tokens, 7);
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
        let width = 60u16;
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

    // ------------------------------------------------------------- /connect

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

        // Ctrl+K would open the palette for the session screen.
        app.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::CONTROL));

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
        assert!(app.auth.is_connected("anthropic"));
        assert_eq!(
            app.auth
                .credential("anthropic")
                .and_then(Credential::secret),
            Some("sk-test")
        );
        // The wizard hands over to the provider's model list.
        assert_eq!(app.connect_step(), Some(ConnectStep::Model));

        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.model(), "claude-sonnet-4-5");
        assert!(!app.connect_open());
    }

    #[test]
    fn a_local_provider_connects_without_collecting_anything() {
        let mut app = app();
        app.submit("/connect".to_string());

        // Row 7 is the local runtime: activation is immediate.
        app.handle_key(key(KeyCode::Char('7')));

        assert_eq!(app.active_provider(), Some("local"));
        assert!(app.auth.credential("local").is_none());
        assert_eq!(app.connect_step(), Some(ConnectStep::Model));
    }

    #[test]
    fn the_custom_endpoint_step_requires_a_url() {
        let mut app = app();
        app.submit("/connect".to_string());
        app.handle_key(key(KeyCode::Char('8')));

        // Enter with an empty URL does not connect.
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.auth.is_connected("custom"));
        assert_eq!(app.connect_step(), Some(ConnectStep::CustomProvider));

        for ch in "https://example.test/v1".chars() {
            app.handle_key(key(KeyCode::Char(ch)));
        }
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(app.active_provider(), Some("custom"));
        assert!(matches!(
            app.auth.credential("custom"),
            Some(Credential::Endpoint { .. })
        ));
        // The custom provider has no model list, so the wizard closes.
        assert!(!app.connect_open());
    }

    #[tokio::test]
    async fn device_auth_stores_the_token_once_it_completes() {
        let mut app = app();
        app.submit("/connect".to_string());

        // Row 2 is the subscription provider, which uses device auth.
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.connect_step(), Some(ConnectStep::DeviceAuth));

        // The background task issues a code, then a token.
        for _ in 0..600 {
            app.tick();
            let succeeded = app
                .inline
                .as_ref()
                .is_some_and(|flow| matches!(flow.device_status(), DeviceAuthStatus::Success(_)));
            if succeeded {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        // Any key continues and stores the token.
        app.handle_key(key(KeyCode::Char('x')));
        assert!(app.auth.is_connected("claude-subscription"));
        assert_eq!(app.active_provider(), Some("claude-subscription"));
    }

    #[test]
    fn pasting_an_api_key_lands_in_the_field() {
        let mut app = app();
        app.submit("/connect".to_string());
        app.handle_key(key(KeyCode::Enter));

        app.handle_paste("sk-pasted");
        app.handle_key(key(KeyCode::Enter));

        assert_eq!(
            app.auth
                .credential("anthropic")
                .and_then(Credential::secret),
            Some("sk-pasted")
        );
    }

    #[tokio::test]
    async fn credentials_are_written_to_disk() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!("solaris-auth-{nanos}.json"));
        let _ = std::fs::remove_file(&path);

        let mut app = app_with_auth_path(path.clone());
        connect_with_api_key(&mut app, "sk-on-disk");

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
    fn the_footer_names_the_connected_provider() {
        let mut app = app();
        app.auth
            .store("anthropic", Credential::ApiKey { key: "k".into() });
        app.auth.activate("anthropic");

        let area = Rect::new(0, 0, 100, 8);
        let mut buf = Buffer::empty(area);
        app.render(&mut buf, area);
        let footer: String = (0..area.width)
            .map(|x| buf[(x, area.height - 1)].symbol())
            .collect();

        assert!(footer.contains("anthropic"), "{footer:?}");
        assert!(footer.contains(" anthropic · "), "{footer:?}");
    }

    #[test]
    fn ctrl_c_raises_the_quit_flag() {
        let mut app = app();
        app.handle_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(app.quit.get());
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
        assert!(app.session.turns[0].tokens > 0);
    }

    #[tokio::test]
    async fn prompts_submitted_while_streaming_are_queued_and_run_in_order() {
        let mut app = app();

        app.submit("first".to_string());
        assert!(app.session.is_streaming());
        // A second prompt must not start a concurrent turn.
        app.submit("second".to_string());
        assert_eq!(app.session.turns.len(), 1, "concurrent turn started");
        assert_eq!(app.queued_prompts.len(), 1);

        for _ in 0..1000 {
            app.tick();
            if app.queued_prompts.is_empty()
                && !app.session.is_streaming()
                && app.session.turns.len() == 2
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }

        assert_eq!(
            app.session
                .turns
                .iter()
                .map(|turn| turn.prompt.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        assert!(
            app.session.turns.iter().all(|turn| turn.complete),
            "a queued turn never completed"
        );
        // Deltas were attributed to the right turn.
        assert!(app.session.turns[0].reply.contains("first"));
        assert!(app.session.turns[1].reply.contains("second"));
    }

    /// An app whose copies are recorded instead of written to the real
    /// clipboard, plus the log they land in.
    fn app_with_clipboard(succeeds: bool) -> (App, Rc<RefCell<Vec<String>>>) {
        let backend = Arc::new(MockBackend::with_delay(Duration::ZERO));
        let quit: QuitFlag = Rc::new(Cell::new(false));
        let queue: OverlayQueue = Rc::new(RefCell::new(Vec::new()));
        let flag: Rc<Cell<bool>> = Rc::new(Cell::new(false));
        let copied: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));

        let log = Rc::clone(&copied);
        let mut options = AppOptions::new(backend, Config::default(), quit, queue, flag);
        options.clipboard = Rc::new(move |text: &str| {
            log.borrow_mut().push(text.to_string());
            succeeds
        });
        (App::new(options), copied)
    }

    /// Drag the screen the way `Tui` does: draw a frame, then press, move and
    /// release over the app's own selection handle.
    fn drag_select(app: &mut App, area: Rect, from: (u16, u16), to: (u16, u16)) {
        let mut buf = Buffer::empty(area);
        app.render(&mut buf, area);

        let mut selection = app.selection.borrow_mut();
        selection.set_area(area);
        selection.highlight(&mut buf);
        for (kind, (column, row)) in [
            (MouseEventKind::Down(MouseButton::Left), from),
            (MouseEventKind::Drag(MouseButton::Left), to),
            (MouseEventKind::Up(MouseButton::Left), to),
        ] {
            selection.handle_mouse(MouseEvent {
                kind,
                column,
                row,
                modifiers: KeyModifiers::empty(),
            });
        }
    }

    #[tokio::test]
    async fn a_finished_selection_reaches_the_clipboard() {
        let (mut app, copied) = app_with_clipboard(true);
        app.submit("hello world".to_string());

        // The prompt echo is the first transcript row.
        drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (12, 0));
        app.tick();

        assert_eq!(copied.borrow().as_slice(), ["› hello world".to_string()]);
        assert!(
            rendered_text(&mut app, 60, 12).contains("copied 13 characters"),
            "the footer never announced the copy"
        );
    }

    #[tokio::test]
    async fn a_press_without_a_drag_copies_nothing() {
        let (mut app, copied) = app_with_clipboard(true);
        app.submit("hello world".to_string());

        drag_select(&mut app, Rect::new(0, 0, 60, 12), (3, 0), (3, 0));
        app.tick();

        assert!(copied.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_clipboard_that_refuses_is_reported() {
        let (mut app, copied) = app_with_clipboard(false);
        app.submit("hello world".to_string());

        drag_select(&mut app, Rect::new(0, 0, 60, 12), (0, 0), (12, 0));
        app.tick();

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
}
