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

use futures::StreamExt;

use std::cell::Cell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use solaris_agent::{
    AgentEvent, AlwaysApprove, Approval, ApprovalRequest, Approver, ChannelApprover, Mode,
    ModelConnector, PromptContext, ResourcePaths, Skills, ToolCall, ToolRegistry, ToolSelection,
};
use solaris_provider::{
    AuthKind, AuthStore, BackendChoice, Credential, CredentialSource, DEFAULT_CONTEXT_WINDOW,
};
use solaris_tui::component::{Component, KeyResult, MouseResult};
use solaris_tui::components::editor::{Editor, EditorStyles};
use solaris_tui::components::select_list::SelectItem;
use solaris_tui::components::welcome::{WelcomeData, WelcomeEntry};
use solaris_tui::components::{CollapsibleAction, CollapsibleHit, CollapsibleInteraction};
use solaris_tui::keys::Keybindings;
use solaris_tui::layout::{self, Axis, Entry};
use solaris_tui::overlay::{OverlayOptions, SizeValue};
use solaris_tui::selection::{Selection, SelectionHandle};
use solaris_tui::theme::Theme;
use solaris_tui::tui::{OverlayQueue, QuitFlag};
use solaris_tui::util::{display_width, pad_to_width, rect_contains, truncate_to_width};
use tokio::sync::mpsc::{
    UnboundedReceiver, UnboundedSender, error::TryRecvError, unbounded_channel,
};
use tokio::sync::oneshot;

use crate::buddy::{self, Companion, Soul};
use crate::clipboard::Clipboard;
use crate::commands::{self, SKILL_PREFIX, SlashCommand, parse_slash_command};
use crate::config::{Config, Preferences};
use crate::connect::{ConnectFlow, ConnectOutcome, ConnectStep, ConnectSubmit};
use crate::dialogs::{
    ConfirmAction, ConfirmDialog, DialogMessage, HINT_SELECT, SelectDialog, TextDialog, help_lines,
    skills_lines, stats_lines,
};
use crate::inline::{Inline, InlineOutcome};
use crate::keymap;
use crate::model::{ModelOutcome, ModelPicker};
use crate::recent::{self, RecentActivity};
#[cfg(test)]
use crate::state::ToolStep;
use crate::state::{NoticeKind, NotificationQueue, SessionState, Turn};
use crate::transcript::{SPINNER, TranscriptView};
#[cfg(test)]
use solaris_agent::{Agent, AgentOptions};

mod command_handling;
mod conversation;
mod input;
mod navigation;
mod overlays;
mod preferences;
mod providers;
mod terminal;

#[cfg(test)]
mod tests;

#[path = "../view/session.rs"]
mod session_view;
use session_view::SubagentTerminal;

/// Program name shown in the welcome box title.
const APP_NAME: &str = "solaris";
/// Milliseconds between two idle animation frames of the companion, matching
/// claurst's fidget.
const BUDDY_STEP_MS: u64 = 500;

/// Two presses of `ctrl+c` — or of `ctrl+d` on an empty prompt — inside this
/// window quit the app. claurst uses the same two seconds.
const QUIT_CONFIRM_WINDOW: Duration = Duration::from_secs(2);

/// Builds the backend for the current credentials and model.
///
/// A closure rather than a fixed backend, because `/connect` and `/model` both
/// change the answer mid-session — and because it is the seam that lets the app
/// stay ignorant of which provider, if any, is behind it.
pub use crate::application::connection::BackendFactory;

/// Everything [`App::new`] needs to start.
pub struct AppOptions {
    /// Resolves the backend at start-up and again after every change to the
    /// credentials or the model.
    pub backend_factory: BackendFactory,
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
    /// What the last session left behind: theme, mode and the model chosen for
    /// its provider.
    pub preferences: Preferences,
    /// Where the preferences are persisted; `None` keeps them session-scoped.
    pub settings_path: Option<PathBuf>,
    pub quit: QuitFlag,
    pub overlay_queue: OverlayQueue,
    pub overlay_flag: Rc<Cell<bool>>,
    /// The screen selection the framework tracks for a drag.
    pub selection: SelectionHandle,
    /// Where a finished selection is copied, and where a paste comes from.
    pub clipboard: Clipboard,
    /// Whether the app asks the provider for its model list.
    ///
    /// On in the binary, where the answer fills the model picker; off in
    /// [`AppOptions::new`], so a test that pins its own backend makes no
    /// request of its own and the catalogue is the only source.
    pub discover_models: bool,
    /// Which tools a turn declares, on top of what the mode offers by default.
    pub tools: ToolSelection,
    /// The skills this session found, which are listed in the system prompt and
    /// loadable by name with `/skill:<name>`.
    ///
    /// Empty in tests, and in a session whose directories hold no skill.
    pub skills: Skills,
    /// Loaded global/project prompt instructions.
    pub prompt_context: PromptContext,
    /// Filesystem locations to revisit on /reload; absent in isolated callers.
    pub resource_paths: Option<ResourcePaths>,
    /// Whether a call that changes something is confirmed before it runs.
    ///
    /// Off in tests, which drive the loop themselves and have nobody to ask; on
    /// in the binary, where a prompt is the point.
    pub ask_approval: bool,
    /// Optional persistent transcript and model context.
    pub journal: Option<Arc<solaris_agent::SessionJournal>>,
    pub execution: solaris_agent::ExecutionSettings,
    /// Optional registry populated with external tools before UI startup.
    pub registry: Option<ToolRegistry>,
}

impl AppOptions {
    /// Options with no stored credentials and no persistence — handy in tests.
    ///
    /// The factory pinned here always returns `backend`, so a test drives the
    /// backend it handed in whatever the wizard does to the credentials. Model
    /// discovery is left off for the same reason: whatever backend it was handed
    /// is the only thing this app should talk to.
    pub fn new(
        backend: Arc<dyn ModelConnector>,
        config: Config,
        quit: QuitFlag,
        overlay_queue: OverlayQueue,
        overlay_flag: Rc<Cell<bool>>,
    ) -> Self {
        Self {
            backend_factory: Arc::new(move |_, model| BackendChoice {
                models: Arc::new(solaris_provider::UnavailableModels),
                configuration: Default::default(),
                backend: Arc::clone(&backend),
                provider_id: None,
                base_url: None,
                model: model.to_string(),
                source: CredentialSource::None,
            }),
            config,
            auth: AuthStore::new(),
            auth_path: None,
            version: env!("CARGO_PKG_VERSION").to_string(),
            greeting: "Welcome back!".to_string(),
            buddy: Companion::new("solaris", None),
            buddy_path: None,
            tip: crate::tips::select(0).content.to_string(),
            recent: RecentActivity::new(),
            recent_path: None,
            preferences: Preferences::default(),
            settings_path: None,
            quit,
            overlay_queue,
            overlay_flag,
            selection: Selection::handle(),
            clipboard: Clipboard::system(),
            discover_models: false,
            tools: ToolSelection::none(),
            skills: Skills::empty(),
            prompt_context: PromptContext::default(),
            resource_paths: None,
            ask_approval: false,
            journal: None,
            execution: solaris_agent::ExecutionSettings::default(),
            registry: None,
        }
    }
}

use crate::assembly::{resolve_backend, with_tools};

/// The solaris application root component.
pub struct App {
    connection: crate::application::connection::ConnectionController,
    resources: crate::application::resources::ResourceController,
    sessions: crate::application::session::SessionController,
    io: crate::desktop::DesktopIo,
    input_generation: u64,
    /// The session every turn goes through, and the backend behind it.
    /// Resolves the backend again whenever the credentials or model change.
    pub(crate) config: Config,
    pub(crate) theme: Theme,
    pub(crate) session: SessionState,
    pub(crate) notifications: NotificationQueue,
    transcript: TranscriptView,
    /// A child terminal has its own viewport, leaving the main session intact.
    subagent_terminal: Option<SubagentTerminal>,
    editor: Editor,
    keybindings: Keybindings,
    quit: QuitFlag,
    overlay_queue: OverlayQueue,
    overlay_flag: Rc<Cell<bool>>,
    /// The screen selection the framework tracks, shared with `Tui`.
    selection: SelectionHandle,
    /// Where a finished selection is copied, and where a paste comes from.
    clipboard: Clipboard,
    dialog_tx: UnboundedSender<DialogMessage>,
    dialog_rx: UnboundedReceiver<DialogMessage>,
    spinner_frame: u16,
    last_transcript: Rect,
    /// Shared disclosure interaction; targets identify transcript records.
    collapsible_mouse: CollapsibleInteraction<(usize, usize, bool)>,
    /// Credentials collected by `/connect`.
    /// Where credentials are saved after a change.
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
    /// Theme, mode and model as the last session left them, and where they are
    /// saved.
    preferences: Preferences,
    settings_path: Option<PathBuf>,
    /// Idle animation step of the companion, counted in 500 ms beats.
    buddy_step: u64,
    buddy_started: Instant,
    /// Which quit key was pressed first, and when — the two-press quit.
    quit_press: Option<(char, Instant)>,
    /// The inline `/connect` wizard while it owns the prompt region.
    inline: Option<Inline>,
    /// Models the active provider reported, once it has reported any. Empty
    /// until then, which is what makes the catalogue the fallback rather than
    /// the only answer.
    /// Answer from the background model-discovery task, while one is in flight.
    /// Answer from a `/compact` that is in flight.
    compact_rx: Option<UnboundedReceiver<Option<usize>>>,
    /// Whether asking the provider for its models is allowed at all.
    /// Which tools a turn declares, on top of what the mode offers by default.
    tools: ToolSelection,
    /// The skills this session may draw on, found before the app was built.
    /// Every built-in tool this platform has.
    ///
    /// Built once, so `edit` and `write` keep serialising against the same
    /// per-file locks for the whole session.
    registry: Arc<ToolRegistry>,
    /// Directory relative tool paths resolve against.
    cwd: PathBuf,
    connection_overrides: crate::application::history::ConnectionOverrides,
    /// The approver every rebuilt loop is wired with.
    approver: Arc<dyn Approver>,
    execution: solaris_agent::ExecutionSettings,
    /// Stops the turn in flight, tools included.
    /// Calls waiting for the user's answer; `None` when the session approves
    /// everything without asking.
    approval_rx: Option<UnboundedReceiver<ApprovalRequest>>,
    /// Answers the loop is still waiting for, oldest first.
    pending_approvals: VecDeque<oneshot::Sender<Approval>>,
    question_dialog: Option<(String, String)>,
}

impl App {
    /// Build the application.
    pub fn new(options: AppOptions) -> Self {
        let AppOptions {
            backend_factory,
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
            preferences,
            settings_path,
            quit,
            overlay_queue,
            overlay_flag,
            selection,
            clipboard,
            discover_models,
            tools,
            skills,
            prompt_context,
            resource_paths,
            ask_approval,
            journal,
            execution,
            registry,
        } = options;

        let mut config = config;
        // Enabling the TUI confirmation hook selects Ask semantics for default
        // reads. Explicit rules still reach the hook; Trusted library callers
        // retain their custom approver's decisions for every tool.
        let execution = crate::application::execution::with_approval(execution, ask_approval);
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let registry = Arc::new(registry.unwrap_or_else(ToolRegistry::builtin));

        // A session that asks before acting gets a channel the UI drains; one
        // that does not approves everything, which is what a test wants.
        let (approver, approval_rx): (Arc<dyn Approver>, _) = if ask_approval {
            let (tx, rx) = unbounded_channel();
            (Arc::new(ChannelApprover::new(tx)), Some(rx))
        } else {
            (Arc::new(AlwaysApprove), None)
        };

        let runtime = Arc::new(solaris_agent::Runtime::default());
        let choice = resolve_backend(&backend_factory, &auth, &mut config);
        let provider_config = Arc::clone(&choice.configuration);
        let model_source = Arc::clone(&choice.models);
        let agent = with_tools(
            choice,
            Arc::clone(&registry),
            &tools,
            &skills,
            &prompt_context,
            &cwd,
            Arc::clone(&approver),
            &execution,
            &backend_factory,
            &auth,
            &runtime,
        )
        .expect("fresh application runtime can register its first session");

        if let Some(journal) = &journal {
            // Attach errors are retained by the agent and prevent tool execution.
            let _ = agent.attach_journal(Arc::clone(journal));
        }
        let theme = Theme::by_name(&config.theme);
        selection
            .borrow_mut()
            .set_style(theme.selection_fg, theme.selection_bg);
        let (dialog_tx, dialog_rx) = unbounded_channel();

        let mut editor = Editor::with_placeholder("Ask anything…  /help for commands");
        editor.set_hints(commands::hints(&skills));
        editor.set_styles(editor_styles(&theme));

        let sessions = crate::application::session::SessionController::new(agent, runtime);
        let _ = sessions.agent.activate();
        let connection = crate::application::connection::ConnectionController::new(
            backend_factory,
            auth,
            auth_path,
            model_source,
            provider_config,
            discover_models,
        );
        let resources =
            crate::application::resources::ResourceController::new(skills, resource_paths);
        let mut app = Self {
            connection,
            resources,
            sessions,
            io: Default::default(),
            input_generation: 0,
            config,
            theme,
            session: SessionState::new(),
            notifications: NotificationQueue::new(),
            transcript: TranscriptView::new(),
            subagent_terminal: None,
            editor,
            keybindings: keymap::default_bindings(),
            quit,
            overlay_queue,
            overlay_flag,
            selection,
            clipboard,
            dialog_tx,
            dialog_rx,
            spinner_frame: 0,
            last_transcript: Rect::default(),
            collapsible_mouse: CollapsibleInteraction::default(),
            buddy,
            buddy_path,
            version,
            greeting,
            tip,
            recent,
            recent_path,
            preferences,
            settings_path,
            buddy_step: 0,
            buddy_started: Instant::now(),
            quit_press: None,
            inline: None,
            compact_rx: None,
            tools,
            registry,
            cwd,
            connection_overrides: Default::default(),
            approver,
            execution,
            approval_rx,
            pending_approvals: VecDeque::new(),
            question_dialog: None,
        };

        if let Some(journal) = journal {
            app.sessions.navigation.history(journal);
        }
        app.refresh_models();
        app
    }

    /// Preserve process-level provider and model overrides when opening history.
    pub fn with_connection_overrides(
        mut self,
        overrides: crate::application::history::ConnectionOverrides,
    ) -> Self {
        self.connection_overrides = overrides;
        self
    }

    /// The active provider, once one has been connected.
    pub fn active_provider(&self) -> Option<&str> {
        self.connection.auth.active_provider()
    }

    /// Whether the `/connect` wizard is on screen.
    pub fn connect_open(&self) -> bool {
        self.inline.as_ref().is_some_and(Inline::is_connect)
    }

    /// The wizard's current step, when it is open.
    pub fn connect_step(&self) -> Option<ConnectStep> {
        self.inline.as_ref().and_then(|inline| inline.step())
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

    /// The skills this session may draw on.
    pub fn skills(&self) -> &Skills {
        &self.resources.skills
    }

    /// The label of the backend answering turns right now.
    pub fn backend_label(&self) -> String {
        self.sessions.agent.snapshot().label
    }
    /// Whether an overlay is currently visible.
    pub fn overlay_open(&self) -> bool {
        self.overlay_flag.get()
    }
}

/// The arguments of a call, as the confirmation prompt shows them.
fn describe_call(call: &ToolCall) -> String {
    // `Value`'s own display is the compact JSON the model sent, which is what
    // the user needs to judge the call by.
    truncate_to_width(&call.input.to_string(), 400, "…")
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
