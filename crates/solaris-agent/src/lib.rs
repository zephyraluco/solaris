//! The agent session: the model, the tools it may ask for, and the loop between
//! them.
//!
//! A model can only ask; something has to act, and something has to carry the
//! answer back and ask again. That is this crate. It sits above the three layers
//! it composes and is the only door the application needs:
//!
//! - [`Agent`] wraps a resolved backend ([`solaris_backend`]) in the multi-round
//!   loop, declaring the tools ([`solaris_tools`]) a turn's [`Mode`] offers and
//!   running what comes back.
//! - [`Mode`] is what a session is doing — building, or only looking — which
//!   decides the default tool set and nothing else.
//! - [`Approver`] is the hook that can refuse a call before it runs.
//!
//! Because the app talks to this crate alone, the vocabulary it needs comes from
//! here too: the messages and events a turn is made of, the tool calls and
//! results, and the usage a turn is billed for are re-exported from
//! [`solaris_backend`], and the tools and their registry from [`solaris_tools`].

mod agent;
mod approver;
mod background;
mod compaction;
mod conversation;
mod input;
mod mode;
mod overflow;
mod prompt;
mod queue;
mod resources;
mod session;
mod skills;
mod subagent;

pub use agent::{Agent, AgentOptions};
pub use approver::{
    AlwaysApprove, Approval, ApprovalRequest, Approver, ChannelApprover, DenyAll, FnApprover,
};
pub use compaction::Compaction;
pub use input::{InputError, expand_input};
pub use mode::{Mode, default_tool_names};
pub use queue::Queue;
pub use resources::{PromptContext, PromptFile, ResourcePaths, ResourceSnapshot};
pub use session::DEFAULT_MAX_PARALLEL;
pub use skills::{Skill, SkillRoot, SkillSource, SkillWarning, Skills};
pub use subagent::{SUBAGENT_TOOL, agent_tool_registry};

// The conversation: what a turn is written in and what it streams back.
pub use solaris_backend::{
    AgentBackend, AgentEvent, AgentEventStream, BackendError, Content, Message, Price, Role,
    ToolCall, ToolResult, ToolSpec, TurnRequest, Usage, tokens_for_characters,
};
// The tools, as the application sees them: what to hold, and what to hand in.
pub use solaris_tools::{
    BUILTIN_TOOLS, Cancel, CommandOutput, CommandRunner, LocalRunner, SHELL_TOOL, Tool,
    ToolContext, ToolOutput, ToolRegistry, ToolSelection,
};

mod store;
pub use store::{SessionInfo, SessionJournal, SessionRecord, SessionStore, StoreError};

mod runtime;

mod budget;
mod policy;
pub use budget::{BudgetLimits, ExecutionBudget};
pub use policy::{PermissionEffect, PermissionRule, PolicyApprover, PolicyProfile};

/// Per-session execution settings; children inherit these ceilings.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ExecutionSettings {
    #[serde(default)]
    pub profile: PolicyProfile,
    #[serde(default)]
    pub rules: Vec<PermissionRule>,
    #[serde(default)]
    pub budget: BudgetLimits,
}

pub use runtime::{RuntimeCommand, SessionRuntime};

pub type ChildBackendFactory =
    std::sync::Arc<dyn Fn(&str) -> Result<solaris_provider::BackendChoice, String> + Send + Sync>;
pub use subagent::TaskControls;
