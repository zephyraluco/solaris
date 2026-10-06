//! Domain types and pure logic for solaris.
//!
//! This crate is the bottom of the dependency graph: it knows nothing about
//! terminals, rendering, or network backends.

pub mod auth;
pub mod buddy;
pub mod command;
pub mod config;
pub mod event;
pub mod message;
pub mod provider;
pub mod recent;
pub mod tips;

pub use auth::{AuthError, AuthStore, Credential, mask_secret};
pub use buddy::{Bones, BuddyError, Companion, Hat, Rarity, Soul, Species};
pub use command::{
    PROMPT_SLASH_COMMANDS, SlashCommand, SlashCommandSpec, matching_slash_commands,
    parse_slash_command,
};
pub use config::{Config, Mode};
pub use event::{AgentEvent, BackendError, TurnRequest};
pub use message::{Message, Role};
pub use provider::{AuthKind, PROVIDERS, ProviderSpec, provider};
pub use recent::{RecentActivity, RecentEntry};
pub use tips::{TIPS, Tip};
