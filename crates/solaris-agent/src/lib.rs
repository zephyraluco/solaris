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
mod compaction;
mod conversation;
mod mode;
mod overflow;
mod prompt;
mod queue;
mod session;
mod skills;

pub use agent::{Agent, AgentOptions};
pub use approver::{
    AlwaysApprove, Approval, ApprovalRequest, Approver, ChannelApprover, DenyAll, FnApprover,
};
pub use compaction::Compaction;
pub use mode::{Mode, default_tool_names};
pub use queue::Queue;
pub use session::DEFAULT_MAX_PARALLEL;
pub use skills::{Skill, SkillRoot, SkillSource, SkillWarning, Skills};

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
