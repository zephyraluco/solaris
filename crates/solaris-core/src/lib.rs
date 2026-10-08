//! Domain types and pure logic for solaris.
//!
//! This crate is the bottom of the dependency graph: it knows nothing about
//! terminals, rendering, network backends, or which platforms exist. Credentials
//! and the provider catalogue live in `solaris-provider`, the wire protocols in
//! `solaris-backend`, and running a tool that the model asked for in
//! `solaris-tools`. What lives here is only the vocabulary they share — messages
//! and their content blocks, tool declarations, calls and results, and the event
//! stream a backend produces.

pub mod buddy;
pub mod command;
pub mod config;
pub mod event;
pub mod message;
pub mod recent;
pub mod tips;
pub mod tool;
pub mod usage;

pub use buddy::{Bones, BuddyError, Companion, Hat, Rarity, Soul, Species};
pub use command::{
    PROMPT_SLASH_COMMANDS, SlashCommand, SlashCommandSpec, matching_slash_commands,
    parse_slash_command,
};
pub use config::{Config, Mode, Preferences};
pub use event::{AgentEvent, BackendError, TurnRequest};
pub use message::{Content, Message, Role};
pub use recent::{RecentActivity, RecentEntry};
pub use tips::{TIPS, Tip};
pub use tool::{ToolCall, ToolResult, ToolSpec};
pub use usage::{Price, Usage, tokens_for_characters};
