//! `solaris` — a terminal AI assistant built on the solaris TUI framework.
//!
//! This binary is a thin assembly layer: it parses arguments, resolves where
//! credentials live, builds a backend and configuration, installs the
//! terminal, and hands control to [`solaris_tui::Tui::run`], which owns the event
//! loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use solaris::{App, AppOptions, BackendFactory, Clipboard};
use solaris_core::{Companion, Config, Mode, Preferences, RecentActivity, Soul, tips};
use solaris_provider::{AuthStore, context_window_for, provider_spec};
use solaris_provider::{BackendOptions, CredentialSource, choose_backend};
use solaris_tools::{ToolRegistry, ToolSelection};
use solaris_tui::Tui;

/// A terminal AI assistant.
#[derive(Debug, Parser)]
#[command(name = "solaris", version, about = "A terminal AI assistant")]
struct Args {
    /// Model identifier; omitted means the connected provider chooses.
    #[arg(long)]
    model: Option<String>,

    /// Colour theme to start with; omitted means the remembered one, or `dark`.
    #[arg(long, value_parser = ["dark", "light"])]
    theme: Option<String>,

    /// Start in plan mode instead of build mode.
    #[arg(long)]
    plan: bool,

    /// Provider to send requests to, overriding the one `/connect` activated.
    #[arg(long)]
    provider: Option<String>,

    /// Tools to declare, replacing the mode's default set. Entries are tool
    /// names or patterns where `*` matches any characters; a list made only of
    /// `+name` and `-name` entries edits the default set instead.
    #[arg(short = 't', long, value_name = "LIST")]
    tools: Option<String>,

    /// Tools to withdraw, after everything else has selected them.
    #[arg(long, value_name = "LIST")]
    exclude_tools: Option<String>,

    /// Print the resolved configuration and exit without starting the UI.
    #[arg(long)]
    print_config: bool,

    /// Confirm a call that changes something before it runs.
    ///
    /// Off by default: a terminal agent is already acting on your behalf. Turn
    /// it on to be asked before a write, an edit or a command runs.
    #[arg(long)]
    confirm_tools: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let mut config = Config {
        theme: args
            .theme
            .clone()
            .unwrap_or_else(|| Config::default().theme),
        mode: if args.plan { Mode::Plan } else { Mode::Build },
        ..Config::default()
    };
    if let Some(model) = &args.model {
        config.model = model.clone();
    }

    let tools = match ToolSelection::parse(args.tools.as_deref(), args.exclude_tools.as_deref()) {
        Ok(selection) => selection,
        Err(message) => anyhow::bail!("{message}"),
    };
    // A name that matches nothing is almost always a typo, and silently
    // declaring nothing would look like the tool simply refused to run.
    let registry = ToolRegistry::builtin();
    for entry in registry.unmatched(&tools) {
        eprintln!(
            "solaris: no tool named `{entry}` on this platform — available: {}",
            registry.names().join(", ")
        );
    }

    let state_dir = config_dir();
    let auth_path = state_dir.as_ref().map(|dir| dir.join("auth.json"));
    let buddy_path = state_dir.as_ref().map(|dir| dir.join("companion.json"));
    let recent_path = state_dir.as_ref().map(|dir| dir.join("recent.json"));
    let settings_path = state_dir.as_ref().map(|dir| dir.join("settings.json"));

    let mut auth = load_auth(auth_path.as_deref());
    let user = user_id();
    let buddy = Companion::new(&user, load_soul(buddy_path.as_deref()));
    let recent = load_recent(recent_path.as_deref());
    let settings = load_settings(settings_path.as_deref());

    // `--provider` moves this run without rewriting the saved choice.
    if let Some(id) = &args.provider {
        if provider_spec(id).is_none() {
            anyhow::bail!("unknown provider `{id}` — one of: {}", provider_ids());
        }
        auth.activate(id.clone());
    }

    // What the last session left, unless this run overrides it: a flag always
    // wins, so `--plan` in a script keeps meaning plan. The model follows the
    // provider it was chosen for, and is kept in `auth.json` beside it.
    if args.theme.is_none() {
        if let Some(theme) = &settings.theme {
            config.theme = theme.clone();
        }
    }
    if !args.plan {
        if let Some(mode) = settings.mode {
            config.mode = mode;
        }
    }

    // Without `--model`, a session starts on the model this provider was last
    // used with: the catalogue's first entry is a fallback, not a decision, and
    // reconnecting should not quietly move the user onto it.
    if args.model.is_none() {
        if let Some(model) = auth.active_model() {
            config.model = model.to_string();
        }
    }

    let backend_options = BackendOptions::default();
    // Resolved here for reporting, and again by the app whenever the
    // credentials or the model change.
    let choice = choose_backend(&auth, &config.model, backend_options.clone());

    if args.provider.is_some() && choice.provider_id.is_none() {
        eprintln!(
            "solaris: {} has no usable credentials — run /connect, or set its key in the environment",
            args.provider.as_deref().unwrap_or_default()
        );
    }

    if args.print_config {
        println!("backend: {}", choice.backend.label());
        println!("model: {}", describe_model(&choice.model));
        println!("theme: {}", config.theme);
        println!("mode: {}", config.mode.label());
        println!(
            "tools: {}",
            registry.selected_names(config.mode, &tools).join(", ")
        );
        println!("context window: {}", context_window_for(&choice.model));
        match &auth_path {
            Some(path) => println!("credentials: {}", path.display()),
            None => println!("credentials: (unsaved — no config directory)"),
        }
        println!(
            "connected: {}",
            if auth.is_empty() {
                "none".to_string()
            } else {
                auth.connected().join(", ")
            }
        );
        println!(
            "active provider: {}",
            auth.active_provider().unwrap_or("none")
        );
        println!(
            "endpoint: {}",
            choice.base_url.as_deref().unwrap_or("(not connected)")
        );
        println!("credential source: {}", describe_source(choice.source));
        let buddy_name = match &buddy.soul {
            Some(soul) => format!("{} the {}", soul.name, buddy.bones.species.as_str()),
            None => format!("{} (unnamed)", buddy.bones.species.as_str()),
        };
        println!(
            "buddy: {buddy_name} — {}, eyes {}, hat {}",
            buddy.bones.rarity.as_str(),
            buddy.bones.eye.glyph(),
            buddy.bones.hat.as_str()
        );
        match &buddy_path {
            Some(path) => println!("companion: {}", path.display()),
            None => println!("companion: (unsaved — no config directory)"),
        }
        match &recent_path {
            Some(path) => println!("recent activity: {}", path.display()),
            None => println!("recent activity: (unsaved — no config directory)"),
        }
        println!(
            "recent prompts: {}",
            if recent.is_empty() {
                "none".to_string()
            } else {
                recent
                    .entries()
                    .iter()
                    .map(|entry| entry.label.as_str())
                    .collect::<Vec<_>>()
                    .join(" | ")
            }
        );
        println!("tip: {}", tips::select(recent.len()).content);
        return Ok(());
    }

    // The backend runs on a runtime; the UI loop stays on the main thread and
    // only ever drains a channel, so nothing blocks the terminal.
    let runtime = tokio::runtime::Runtime::new()?;
    let _guard = runtime.enter();

    solaris_tui::install_panic_hook();
    let mut terminal = solaris_tui::setup_terminal()?;

    let mut tui = Tui::new();
    let factory: BackendFactory = Arc::new(move |auth: &AuthStore, model: &str| {
        choose_backend(auth, model, backend_options.clone())
    });
    let app = App::new(AppOptions {
        backend_factory: factory,
        config,
        auth,
        auth_path,
        version: env!("CARGO_PKG_VERSION").to_string(),
        greeting: greeting(&user),
        buddy,
        buddy_path,
        tip: tips::select(recent.len()).content.to_string(),
        recent,
        recent_path,
        preferences: settings,
        settings_path,
        quit: tui.quit_flag(),
        overlay_queue: tui.overlay_queue(),
        overlay_flag: tui.overlay_flag(),
        selection: tui.selection(),
        clipboard: Clipboard::system(),
        // The binary wants the provider's own model list in the picker; the
        // catalogue is what it falls back to.
        discover_models: true,
        tools,
        ask_approval: args.confirm_tools,
    });
    tui.set_root(Box::new(app));

    let outcome = tui.run(&mut terminal);

    // Always restore, even when the loop failed.
    solaris_tui::restore_terminal()?;
    outcome?;

    Ok(())
}

/// Every provider id the catalogue knows, for the `--provider` error message.
fn provider_ids() -> String {
    solaris_provider::PROVIDERS
        .iter()
        .map(|spec| spec.id)
        .collect::<Vec<_>>()
        .join(", ")
}

/// The model a run will ask for, or an explanation when none was named and no
/// provider was around to supply one.
fn describe_model(model: &str) -> &str {
    if model.is_empty() {
        "(none yet — run /connect)"
    } else {
        model
    }
}

/// Where the credential a backend will use came from, for `--print-config`.
fn describe_source(source: CredentialSource) -> String {
    match source {
        CredentialSource::Stored => "auth.json".to_string(),
        CredentialSource::Env(name) => format!("environment ({name})"),
        CredentialSource::None => "none needed".to_string(),
        CredentialSource::Missing => "none — not connected".to_string(),
        CredentialSource::NoModel => "present — no model named for it yet".to_string(),
    }
}

/// Where solaris keeps its state: `$SOLARIS_CONFIG_DIR` when set, otherwise the
/// platform's per-user configuration directory.
fn config_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("SOLARIS_CONFIG_DIR") {
        return Some(PathBuf::from(dir));
    }

    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);

    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")));

    base.map(|dir| dir.join("solaris"))
}

/// Read the credential store, treating a missing or unreadable file as empty:
/// losing the file must never stop the app from starting.
fn load_auth(path: Option<&Path>) -> AuthStore {
    let Some(path) = path else {
        return AuthStore::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| AuthStore::from_json(&text).ok())
        .unwrap_or_default()
}

/// The companion's saved name, if it has one.
fn load_soul(path: Option<&Path>) -> Option<Soul> {
    let path = path?;
    let text = std::fs::read_to_string(path).ok()?;
    Soul::from_json(&text).ok()
}

/// Read the recorded prompts, treating a missing or corrupt file as empty.
fn load_recent(path: Option<&Path>) -> RecentActivity {
    let Some(path) = path else {
        return RecentActivity::new();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| RecentActivity::from_json(&text).ok())
        .unwrap_or_default()
}

/// The preferences saved last time, or none when there is no usable record.
fn load_settings(path: Option<&Path>) -> Preferences {
    let Some(path) = path else {
        return Preferences::default();
    };
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| Preferences::from_json(&text).ok())
        .unwrap_or_default()
}

/// The current user: names the companion's roll and the welcome greeting.
fn user_id() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "solaris".to_string())
}

/// The greeting line for the welcome box.
fn greeting(user: &str) -> String {
    if user == "solaris" {
        "Welcome back!".to_string()
    } else {
        format!("Welcome back {user}!")
    }
}
