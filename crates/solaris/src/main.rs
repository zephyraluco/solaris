//! `solaris` — a terminal AI assistant built on the solaris TUI framework.
//!
//! This binary is a thin assembly layer: it parses arguments, resolves where
//! credentials live, builds a backend and configuration, installs the
//! terminal, and hands control to [`solaris_tui::Tui::run`], which owns the event
//! loop.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::Parser;
use solaris::{App, AppOptions};
use solaris_backend::{AgentBackend, MockBackend};
use solaris_core::{AuthStore, Companion, Config, Mode, RecentActivity, Soul, tips};
use solaris_tui::Tui;

/// A terminal AI assistant.
#[derive(Debug, Parser)]
#[command(name = "solaris", version, about = "A terminal AI assistant")]
struct Args {
    /// Model identifier.
    #[arg(long, default_value = "solaris-mock-1")]
    model: String,

    /// Colour theme to start with.
    #[arg(long, default_value = "dark", value_parser = ["dark", "light"])]
    theme: String,

    /// Start in plan mode instead of build mode.
    #[arg(long)]
    plan: bool,

    /// Per-chunk streaming delay for the mock backend, in milliseconds.
    #[arg(long, default_value_t = 24)]
    chunk_delay_ms: u64,

    /// Print the resolved configuration and exit without starting the UI.
    #[arg(long)]
    print_config: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let config = Config {
        model: args.model.clone(),
        theme: args.theme.clone(),
        mode: if args.plan { Mode::Plan } else { Mode::Build },
        ..Config::default()
    };

    let state_dir = config_dir();
    let auth_path = state_dir.as_ref().map(|dir| dir.join("auth.json"));
    let buddy_path = state_dir.as_ref().map(|dir| dir.join("companion.json"));
    let recent_path = state_dir.as_ref().map(|dir| dir.join("recent.json"));

    let auth = load_auth(auth_path.as_deref());
    let user = user_id();
    let buddy = Companion::new(&user, load_soul(buddy_path.as_deref()));
    let recent = load_recent(recent_path.as_deref());

    let backend: Arc<dyn AgentBackend> = Arc::new(MockBackend::with_delay(Duration::from_millis(
        args.chunk_delay_ms,
    )));

    if args.print_config {
        println!("backend: {}", backend.label());
        println!("model: {}", config.model);
        println!("theme: {}", config.theme);
        println!("mode: {}", config.mode.label());
        println!("context window: {}", config.context_window);
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
    let app = App::new(AppOptions {
        backend,
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
        quit: tui.quit_flag(),
        overlay_queue: tui.overlay_queue(),
        overlay_flag: tui.overlay_flag(),
    });
    tui.set_root(Box::new(app));

    let outcome = tui.run(&mut terminal);

    // Always restore, even when the loop failed.
    solaris_tui::restore_terminal()?;
    outcome?;

    Ok(())
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
