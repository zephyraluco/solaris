//! Solaris application services and terminal adapter.
//!
//! [`application`] prepares settings, connections and sessions for all entry
//! points. [`tui`] owns terminal input, display state and rendering. The
//! executable also provides headless and ACP adapters over the same runtime.

pub mod application;
pub mod config;
pub mod desktop;
pub mod recent;
pub mod tui;

pub use desktop::clipboard;
pub(crate) use tui::inline;
pub use tui::view as ui;
pub use tui::view::{dialogs, transcript};
pub use tui::{app, buddy, commands, connect, keymap, model, presentation, state, tips};

pub use app::{App, AppOptions, BackendFactory};
pub use buddy::{Companion, Soul};
pub use clipboard::{Clipboard, ClipboardReader, ClipboardWriter};
pub use commands::{PROMPT_SLASH_COMMANDS, SlashCommand, SlashCommandSpec, parse_slash_command};
pub use config::{Config, Preferences};
pub use connect::{ConnectFlow, ConnectOutcome, ConnectStep, ConnectSubmit};
pub use dialogs::{ConfirmAction, DialogMessage};
pub use recent::{RecentActivity, RecentEntry};
pub use state::{NoticeKind, NotificationQueue, SessionState, SteeringNote, Turn};

// The vocabulary a turn is written in comes from the agent, which is the app's
// only door to a backend and to the tools.
pub use solaris_agent::Mode;

mod assembly;
pub use assembly::agent_backend;

#[cfg(test)]
#[path = "../../solaris-agent/tests/support/mod.rs"]
mod test_support;
