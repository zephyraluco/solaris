use std::num::NonZeroU32;
use std::path::PathBuf;

use clap::Parser;

/// A terminal AI assistant.
#[derive(Debug, Parser)]
#[command(name = "solaris", version, about = "A terminal AI assistant")]
pub(super) struct Args {
    /// Model identifier; omitted means the connected provider chooses.
    #[arg(long)]
    pub(super) model: Option<String>,

    /// Colour theme to start with; omitted means the remembered one, or `dark`.
    #[arg(long, value_parser = ["dark", "light"])]
    pub(super) theme: Option<String>,

    /// Operating mode (`default`, `plan`, `auto`, or `ask`); omitted means the remembered one, or `default`.
    #[arg(long, value_parser = ["default", "plan", "auto", "ask"], conflicts_with = "plan")]
    pub(super) mode: Option<String>,

    /// Start in plan mode (shorthand for `--mode plan`).
    #[arg(long)]
    pub(super) plan: bool,

    /// Provider to send requests to, overriding the one `/connect` activated.
    #[arg(long)]
    pub(super) provider: Option<String>,

    /// Tools to declare, replacing the mode's default set. Entries are tool
    /// names or patterns where `*` matches any characters; a list made only of
    /// `+name` and `-name` entries edits the default set instead.
    #[arg(short = 't', long, value_name = "LIST")]
    pub(super) tools: Option<String>,

    /// Tools to withdraw, after everything else has selected them.
    #[arg(long, value_name = "LIST")]
    pub(super) exclude_tools: Option<String>,

    /// Skill directory, or a single `SKILL.md`, to load as well as the usual
    /// locations. Repeat the flag for more than one.
    #[arg(long = "skill", value_name = "PATH")]
    pub(super) skill: Vec<PathBuf>,

    /// Print the resolved configuration and exit without starting the UI.
    #[arg(long)]
    pub(super) print_config: bool,

    /// Run one prompt without starting the terminal UI.
    #[arg(long, value_name = "PROMPT")]
    pub(super) print: Option<String>,

    /// Attach a PNG/JPEG/GIF/WebP image to a headless prompt; repeat for more.
    #[arg(long, requires = "print", value_name = "PATH")]
    pub(super) image: Vec<PathBuf>,

    /// Emit versioned JSON events (requires --print).
    #[arg(long, requires = "print")]
    pub(super) json: bool,

    /// Serve Agent Client Protocol v1 over stdin/stdout.
    #[arg(long, conflicts_with_all = ["print", "json", "print_config", "sessions", "export_session", "changes", "undo", "mcp_status", "resume", "continue_session", "fork", "ephemeral", "image"])]
    pub(super) acp: bool,

    /// MCP configuration; defaults to mcp.json in the user config directory.
    #[arg(long)]
    pub(super) mcp_config: Option<PathBuf>,

    /// Connect configured MCP servers, report status, and exit.
    #[arg(long)]
    pub(super) mcp_status: bool,

    /// Fork a saved session in this directory before running.
    #[arg(long, conflicts_with_all = ["resume", "continue_session", "ephemeral"])]
    pub(super) fork: Option<String>,

    /// Export a session journal as JSON without running it.
    #[arg(long, conflicts_with_all = ["print", "resume", "continue_session", "fork", "ephemeral"])]
    pub(super) export_session: Option<String>,

    /// Confirm a call that changes something before it runs.
    ///
    /// Off by default: a terminal agent is already acting on your behalf. Turn
    /// it on to be asked before a write, an edit or a command runs.
    #[arg(long)]
    pub(super) confirm_tools: bool,

    /// Execution profile. Tool policy does not provide an OS sandbox.
    #[arg(long, value_parser = ["trusted", "ask", "read-only"], default_value = "trusted")]
    pub(super) policy: String,

    /// Optional per-run model step limit; the last step produces a text-only report.
    #[arg(long)]
    pub(super) steps: Option<NonZeroU32>,

    /// Maximum nested subagent depth; zero disables delegation.
    #[arg(long, default_value_t = 1)]
    pub(super) subagent_depth: u32,

    /// Resume a saved session by ID. Interrupted tools are never automatically rerun.
    #[arg(long, conflicts_with = "continue_session")]
    pub(super) resume: Option<String>,

    /// Resume the most recent session in this directory.
    #[arg(long = "continue", conflicts_with = "resume")]
    pub(super) continue_session: bool,

    /// List saved sessions in this directory and exit.
    #[arg(long)]
    pub(super) sessions: bool,

    /// Keep this run entirely in memory.
    #[arg(long, conflicts_with_all = ["resume", "continue_session", "sessions"])]
    pub(super) ephemeral: bool,

    /// List recorded file changes and patches; shell side effects are not included.
    #[arg(long, conflicts_with = "undo")]
    pub(super) changes: bool,

    /// Undo a recorded file change only if its after-image still matches.
    #[arg(long, value_name = "CHANGE_ID", conflicts_with = "changes")]
    pub(super) undo: Option<String>,
}

impl Args {
    pub(super) fn connection_overrides(
        &self,
    ) -> solaris::application::history::ConnectionOverrides {
        solaris::application::history::ConnectionOverrides {
            model: self.model.is_some(),
            provider: self.provider.is_some(),
        }
    }

    pub(super) fn history_command(&self) -> Option<super::sessions::HistoryCommand<'_>> {
        use super::sessions::HistoryCommand;
        if let Some(id) = &self.undo {
            Some(HistoryCommand::Undo(id))
        } else if self.changes {
            Some(HistoryCommand::Changes)
        } else if self.acp || self.ephemeral || self.mcp_status {
            None
        } else if let Some(id) = &self.export_session {
            Some(HistoryCommand::Export(id))
        } else if self.sessions {
            Some(HistoryCommand::List)
        } else {
            None
        }
    }
}
