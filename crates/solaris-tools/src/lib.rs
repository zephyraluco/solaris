//! The tools a model can ask for, and the loop that runs them.
//!
//! A model can only ask; something has to act. This crate is that something: the
//! built-in tools, the registry that decides which of them a turn offers, the
//! approval hook that can refuse a call, and [`ToolLoop`] — the multi-round loop
//! that runs what the model asks for, hands back the results, and asks again
//! until it stops asking.
//!
//! Two things are worth knowing before reading further:
//!
//! - **A tool never fails the turn.** A missing file, a rejected command and
//!   unusable arguments all become an error result that goes back to the model,
//!   because the model is the one that can react to it.
//! - **Nothing here knows which provider is in use.** Tools are declared as JSON
//!   Schema and run locally; `solaris-backend` turns the declarations into
//!   whichever request shape the wire wants.

mod agent_loop;
mod approver;
mod diff;
mod mutation_queue;
mod path_utils;
mod registry;
mod runner;
mod tool;
pub mod tools;
mod truncate;

pub use agent_loop::{DEFAULT_MAX_ROUNDS, ToolLoop, ToolLoopOptions};
pub use approver::{AlwaysApprove, Approval, Approver, DenyAll, FnApprover};
pub use diff::{Edit, EditError};
pub use mutation_queue::FileMutationQueue;
pub use registry::{BUILTIN_TOOLS, SHELL_TOOL, ToolRegistry, ToolSelection, default_tool_names};
pub use runner::{CommandOutput, CommandRunner, LocalRunner};
pub use tool::{Cancel, Tool, ToolContext, ToolOutput};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, Removed, Truncation, format_size,
};
