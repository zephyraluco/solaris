//! The tools a model can ask for.
//!
//! A model can only ask; something has to act. This crate is that something: the
//! built-in tools, the registry that holds them, the runner they shell out
//! through, the diff an edit produces, and the truncation that keeps a huge
//! output from swamping the conversation.
//!
//! Two things are worth knowing before reading further:
//!
//! - **A tool never fails the turn.** A missing file, a rejected command and
//!   unusable arguments all become an error result, because the model is the one
//!   that has to read the problem and decide what to do next.
//! - **Nothing here knows there is a provider at all.** A tool is declared as
//!   JSON Schema and run locally; nothing in this crate sends a request, reads a
//!   stream, or knows what a conversation is. Declaring the tools to a model,
//!   running a round of what it asks for and asking again is `solaris-agent`'s
//!   business.

mod diff;
mod mutation_queue;
mod output;
mod path_utils;
mod registry;
mod runner;
mod tool;
pub mod tools;
mod truncate;

pub use diff::{Edit, EditError};
pub use mutation_queue::FileMutationQueue;
pub use registry::{BUILTIN_TOOLS, SHELL_TOOL, ToolRegistry, ToolSelection};
pub use runner::{CommandOutput, CommandRunner, LocalRunner};
pub use tool::{Cancel, ExecutionMode, Tool, ToolContext, ToolOutput};
pub use truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, Limits, Removed, Truncation, format_size,
};
