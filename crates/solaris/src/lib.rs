//! Solaris application services and terminal adapter.
//!
//! [`application`] prepares settings, connections and sessions for all entry
//! points. [`tui`] owns terminal input, display state and rendering. The
//! executable also provides headless and ACP adapters over the same runtime.

pub mod application;
pub mod desktop;
pub mod tui;

pub use application::assembly::agent_backend;
pub use application::config::{Config, Preferences};
pub use application::connection::BackendFactory;
pub use application::recent::{RecentActivity, RecentEntry};
pub use desktop::clipboard::{Clipboard, ClipboardReader, ClipboardWriter};
pub use tui::app::{App, AppOptions};

// The vocabulary a turn is written in comes from the agent, which is the app's
// only door to a backend and to the tools.
pub use solaris_agent::Mode;
