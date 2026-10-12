//! solaris chat application built on the solaris-tui framework.
//!
//! Layer map, mirroring opencode's organization:
//! - [`app`] — the root component (the app opens straight into the session)
//! - [`state`] — session, notifications, and dialog messages (the "contexts")
//! - [`dialogs`] — dialogs as components, resolved through one overlay path
//! - [`connect`] — the inline `/connect` wizard that takes over the prompt area
//! - [`commands`] / [`keymap`] — the slash-command table with the views over it,
//!   and the global bindings
//! - [`transcript`] — turn views rendered into styled lines
//! - [`clipboard`] — the platform clipboard, behind an injectable pair
//! - [`config`] — the saved preferences
//! - [`buddy`] / [`tips`] / [`recent`] — the companion, what it says, and what
//!   earlier sessions were about
//!
//! The app never talks to a model itself: [`BackendFactory`] resolves a backend
//! from the credentials and the model, `solaris-agent` wraps it in the tool loop,
//! and all the app ever learns is the label the backend reports.

pub mod app;
pub mod buddy;
pub mod clipboard;
pub mod commands;
pub mod config;
pub mod connect;
pub mod dialogs;
pub mod keymap;
pub mod model;
pub mod recent;
pub mod state;
pub mod tips;
pub mod transcript;

// `Inline` is an implementation detail of `App`, so it stays off the crate's
// surface.
mod inline;

pub use app::{App, AppOptions, BackendFactory};
pub use buddy::{Companion, Soul};
pub use clipboard::{Clipboard, ClipboardReader, ClipboardWriter};
pub use commands::{PROMPT_SLASH_COMMANDS, SlashCommand, SlashCommandSpec, parse_slash_command};
pub use config::{Config, Preferences};
pub use connect::{ConnectFlow, ConnectOutcome, ConnectStep, ConnectSubmit};
pub use dialogs::{ConfirmAction, DialogMessage};
pub use recent::{RecentActivity, RecentEntry};
pub use state::{NoticeKind, NotificationQueue, SessionState, Turn};

// The vocabulary a turn is written in comes from the agent, which is the app's
// only door to a backend and to the tools.
pub use solaris_agent::Mode;
