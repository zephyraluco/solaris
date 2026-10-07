//! Reusable terminal UI framework for solaris.
//!
//! The design follows `@earendil-works/pi-tui`: a retained component tree with a
//! [`Component`] trait, application-owned focus and overlays, and flex-like
//! [`layout`] stacks. Rendering is delegated to ratatui, whose double-buffered
//! diffing provides the flicker-free differential updates pi-tui gets from
//! synchronized output.
//!
//! [`Tui::run`] owns the event loop, so an application only implements
//! [`Component`] and hands it to [`Tui::set_root`].

pub mod component;
pub mod components;
pub mod keys;
pub mod layout;
pub mod overlay;
pub mod selection;
pub mod terminal;
pub mod theme;
pub mod tui;
pub mod util;

pub use component::{Component, KeyResult, MouseResult};
pub use components::{
    CommandHint, Editor, Loader, MarkdownStyle, Panel, ScrollView, SelectItem, SelectList, Spacer,
    Text, render_markdown,
};
pub use keys::Keybindings;
pub use layout::{Axis, Basis, Entry, split};
pub use overlay::{Anchor, OverlayOptions, SizeValue, resolve};
pub use selection::{Selection, SelectionHandle};
pub use terminal::{PiTerminal, install_panic_hook, restore_terminal, setup_terminal};
pub use theme::Theme;
pub use tui::{OverlayQueue, QuitFlag, Tui};
pub use util::{display_width, truncate_to_width, wrap_text};
