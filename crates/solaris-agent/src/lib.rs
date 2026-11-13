//! Session runtime and policy over the official Rig executor and memory.
//!
//! [`SessionRuntime`] serializes commands and publishes sequenced events and
//! snapshots. [`Agent`] constructs the execution state supplied to that runtime;
//! model connections and tools are injected by the application. Provider selection
//! and authentication never enter this crate. SDK message history has one owner:
//! native memory. Product events and request settlements live in the journal.

pub use storage::billing::{UsageKind, UsageTotals};

pub use context::compaction::Compaction;
pub use context::resources::{PromptContext, PromptFile, ResourcePaths, ResourceSnapshot};
pub use context::skills::{Skill, SkillRoot, SkillSource, SkillWarning, Skills};
pub use execution::DEFAULT_MAX_PARALLEL;
pub use policy::approver::{
    AlwaysApprove, Approval, ApprovalRequest, Approver, ChannelApprover, DenyAll, FnApprover,
};
pub use runtime::agent::{Agent, AgentOptions};
pub use runtime::connection::ConnectionConfig;
pub use runtime::input::{InputError, expand_input};
pub use runtime::mode::{Mode, default_tool_names};
pub use runtime::queue::Queue;
pub use tasks::{SUBAGENT_TOOL, agent_tool_registry};

// The conversation: what a turn is written in and what it streams back.
pub use solaris_backend::{
    BackendError, Content, ContextUsage, Message, ModelClient, ModelConnector, Role, Usage,
    tokens_for_characters,
};
pub use solaris_types::{ToolCall, ToolResult, ToolSpec};
// The tools, as the application sees them: what to hold, and what to hand in.
pub use solaris_tools::{
    BUILTIN_TOOLS, Cancel, CommandOutput, CommandRunner, LocalRunner, SHELL_TOOL, Tool,
    ToolContext, ToolOutput, ToolRegistry, ToolSelection,
};

pub use storage::{SessionInfo, SessionJournal, SessionRecord, SessionStore, StoreError};

pub use policy::budget::{BudgetLimits, ExecutionBudget};
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

pub use runtime::{
    CommandReceipt, RuntimeCommand, RuntimeEvent, RuntimeEventStream, RuntimeSnapshot,
    SessionRuntime,
};

pub type ChildBackendFactory =
    std::sync::Arc<dyn Fn(&str) -> Result<ConnectionConfig, String> + Send + Sync>;
pub use tasks::TaskControls;

pub use runtime::event::AgentEvent;
pub type AgentEventStream = std::pin::Pin<Box<dyn futures::Stream<Item = AgentEvent> + Send>>;
#[cfg(feature = "test-utils")]
pub mod testing;

mod context;
mod execution;
mod policy;
mod runtime;
mod storage;
mod tasks;

mod identity;
