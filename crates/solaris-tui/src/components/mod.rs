//! Built-in components.
//!
//! Ported from pi-tui's component set: text, layout primitives, an editor, a
//! fuzzy select list, markdown rendering, a scrolling viewport, and the
//! welcome box.

pub mod editor;
pub mod fuzzy;
pub mod inline_select;
pub mod loader;
pub mod markdown;
pub mod panel;
pub mod scroll_view;
pub mod select_list;
pub mod spacer;
pub mod text;
pub mod welcome;

pub use editor::{CommandHint, Editor};
pub use fuzzy::{fuzzy_match, fuzzy_score};
pub use inline_select::{InlineSelect, InlineSelectOutcome, InlineStyles};
pub use loader::Loader;
pub use markdown::{MarkdownStyle, render_markdown};
pub use panel::Panel;
pub use scroll_view::ScrollView;
pub use select_list::{SelectItem, SelectList};
pub use spacer::Spacer;
pub use text::Text;
pub use welcome::{WelcomeData, WelcomeEntry, WelcomeStyles, render_welcome};
