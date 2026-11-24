//! Composable TUI containers for the main session screen.
//!
//! Layout hierarchy:
//! - [`SessionContainer`] splits the viewport vertically into two top-level
//!   containers: [`TranscriptContainer`] (top, grows) and [`BottomDock`]
//!   (bottom, content-sized).
//! - [`BottomDock`] hosts peer bottom surfaces at the same level:
//!   [`PromptContainer`] (retained default composer = `Editor` + [`StatusBar`]),
//!   [`QuestionPanel`](crate::tui::view::question::QuestionPanel), and
//!   [`Inline`](crate::tui::inline::Inline) (`/connect`, `/model`).

pub(crate) mod approval;
pub(crate) mod bottom_dock;
pub(crate) mod prompt;
pub(crate) mod session_page;
pub(crate) mod status_bar;
pub(crate) mod transcript_pane;

pub(crate) use approval::ApprovalPanel;
pub(crate) use bottom_dock::BottomAction;
pub(crate) use prompt::PromptAction;
pub(crate) use session_page::SessionContainer;
pub(crate) use status_bar::StatusBarState;
