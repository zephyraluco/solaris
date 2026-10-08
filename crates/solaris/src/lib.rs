//! solaris chat application built on the solaris-tui framework.
//!
//! Layer map, mirroring opencode's organization:
//! - [`app`] — the root component (the app opens straight into the session)
//! - [`state`] — session, notifications, and dialog messages (the "contexts")
//! - [`dialogs`] — dialogs as components, resolved through one overlay path
//! - [`connect`] — the inline `/connect` wizard that takes over the prompt area
//! - [`commands`] / [`keymap`] — the command registry and global bindings
//! - [`transcript`] — turn views rendered into styled lines
//! - [`clipboard`] — the platform clipboard, behind an injectable pair
//!
//! The app never talks to a provider itself: [`BackendFactory`] resolves an
//! `AgentBackend` from the credentials and the model, and the app only knows
//! the label it reports.

pub mod app;
pub mod clipboard;
pub mod commands;
pub mod connect;
pub mod dialogs;
pub mod keymap;
pub mod model;
pub mod state;
pub mod transcript;

// `Inline` is an implementation detail of `App`, so it stays off the crate's
// surface.
mod inline;

pub use app::{App, AppOptions, BackendFactory};
pub use clipboard::{Clipboard, ClipboardReader, ClipboardWriter};
pub use connect::{ConnectFlow, ConnectOutcome, ConnectStep, ConnectSubmit};
pub use dialogs::{ConfirmAction, DialogMessage};
pub use state::{NoticeKind, NotificationQueue, SessionState, Turn};
