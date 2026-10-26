//! What the model is told before the conversation starts.
//!
//! One system prompt is written for every platform, assembled from four parts:
//!
//! - **what solaris is and what this turn may do** — the identity and the mode,
//!   which is the one part that changes turn to turn;
//! - **the tools declared for this turn**, each contributing its own
//!   [`Tool::guideline`](solaris_tools::Tool::guideline) — a tool knows how it
//!   should be used, so it says so itself rather than being described here;
//! - **the skills on offer**, listed by name, description and file so the model
//!   can ask for the instructions of the one it needs
//!   ([`Skills::prompt_section`]) — the instructions are not in here, which is
//!   the point of a skill;
//! - **this endpoint's own note**, when the platform needs one
//!   ([`ProviderSpec::prompt`](solaris_provider::ProviderSpec::prompt)) — a
//!   gateway that routes to any family, or a small model on this machine.
//!
//! Everything else a model might want to know is already on the wire: the
//! endpoint, the model id, and the tool schemas. Repeating any of it here would
//! be a second source of truth for a fact that cannot disagree.
//!
//! [`COMPACT_PROMPT`] is the other prompt in here, and it is a different kind of
//! thing: not what the model is told about its job, but what it is asked when
//! the conversation has to be summarized.

use std::fmt::Write as _;
use std::sync::Arc;

use solaris_backend::{Content, Message, Role};
use solaris_provider::provider_spec;
use solaris_tools::Tool;

use crate::mode::Mode;
use crate::resources::PromptContext;
use crate::skills::Skills;
use std::path::Path;

/// The tool a model opens a file with, which is the one a skill is read by.
const READ_TOOL: &str = "read";

/// What the model is asked for when a conversation is summarized.
///
/// Written as a fixed set of sections rather than as prose because a summary is
/// read by a model that has to *act* on it: "Goal" and "Next Steps" are what it
/// picks up, "Critical Context" is what it cannot re-derive from the files, and
/// the Done / In Progress / Blocked split is what stops it redoing work. A
/// free-form summary reliably loses one of those, and which one it loses is not
/// predictable.
pub const COMPACT_PROMPT: &str = "\
The messages above are a conversation to summarize: the earlier part of a coding session, being \
replaced because it outgrew the model's context window. Write a checkpoint that lets the session \
continue without them.

Use exactly these sections:

## Goal
What the user is trying to accomplish — several items if the session covers several tasks.

## Constraints & Preferences
What the user asked for or ruled out, including style and tooling. Write \"(none)\" if none.

## Progress
### Done
- [x] What is finished
### In Progress
- [ ] What is being worked on now
### Blocked
What is stuck, and why. Write \"(none)\" if nothing is.

## Key Decisions
- **Decision**: why it was taken

## Next Steps
1. What should happen next, in order

## Critical Context
Facts needed to continue that cannot be re-read from the files: exact paths, symbol names, error \
messages, and command output that still matters. Write \"(none)\" if none.

Keep each section short. Never invent anything the conversation does not contain, and never drop \
an exact path, symbol or error message: those are what the rest of the session is built on.";

/// What the model is asked for when the conversation already carries a summary.
///
/// Re-summarizing from the messages alone would make each summary a summary of a
/// summary, losing a little more of the beginning every time. Updating instead
/// means the tenth compaction still knows what the first one recorded.
pub const UPDATE_COMPACT_PROMPT: &str = "\
The messages above are new parts of a coding session that was already summarized, and that \
summary is below. Update it.

Rules:
- Keep every fact already in the summary; add what these messages establish.
- Move items from In Progress to Done when they are finished, and update Next Steps for what is \
left.
- Keep every exact path, symbol name and error message, both the old ones and the new ones.
- Write the whole summary again, in the same sections.
- Never invent anything the messages do not contain.

The existing summary:

";

/// The opening of the message that stands in for the messages it replaced.
///
/// It is a marker as much as a sentence: it is how a later compaction knows it is
/// updating a summary rather than writing the first one, and how a reader of the
/// conversation knows why it starts in the middle.
pub(crate) const SUMMARY_PREFIX: &str =
    "[Summary of the earlier conversation, replaced to stay inside the context window]";

/// The message a summary travels in.
pub(crate) fn summary_message(summary: &str) -> Message {
    Message::user(format!("{SUMMARY_PREFIX}\n{summary}"))
}

/// The summary the conversation already carries, if it carries one.
///
/// The newest one wins: each compaction replaces the previous note along with the
/// messages around it, so more than one can only appear if that ever changes.
pub(crate) fn existing_summary(messages: &[Message]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        let text = message.text();
        text.starts_with(SUMMARY_PREFIX)
            .then(|| text[SUMMARY_PREFIX.len()..].trim().to_string())
            .filter(|summary| !summary.is_empty())
    })
}

/// The system prompt for one turn.
///
/// `tools` is what the turn will declare, so a tool that is withdrawn for the
/// mode contributes neither a declaration nor a guideline; `skills` is what a
/// `SKILL.md` scan turned up, of which only the names, descriptions and files
/// are written here; `provider` is the id the turn is resolved to, whose note is
/// appended last.
#[cfg(test)]
pub fn system_prompt(
    mode: Mode,
    tools: &[Arc<dyn Tool>],
    skills: &Skills,
    provider: Option<&str>,
) -> String {
    build_system_prompt(
        mode,
        tools,
        skills,
        provider,
        Path::new("."),
        &PromptContext::default(),
    )
}

/// Build the prompt with the session's actual directory and instruction sources.
pub(crate) fn build_system_prompt(
    mode: Mode,
    tools: &[Arc<dyn Tool>],
    skills: &Skills,
    provider: Option<&str>,
    cwd: &Path,
    context: &PromptContext,
) -> String {
    let mut prompt = String::new();

    prompt.push_str(
        "You are solaris, a terminal assistant. You work in the directory solaris was started in, \
         you act by calling the tools declared to you, and your replies are read in a terminal.\n",
    );
    let _ = writeln!(prompt, "\n# Working directory\n{}", cwd.display());

    prompt.push_str("\n# Working mode\n");
    prompt.push_str(mode.instructions());

    let guidelines: Vec<(&str, &str)> = tools
        .iter()
        .filter_map(|tool| {
            let tool = tool.as_ref();
            tool.guideline().map(|guideline| (tool.name(), guideline))
        })
        .collect();
    if !guidelines.is_empty() {
        prompt.push_str("\n# Using the tools\n");
        for (name, guideline) in guidelines {
            let _ = writeln!(prompt, "- `{name}`: {guideline}");
        }
    }

    // A skill is opened with whichever tool plays that part, which is only worth
    // naming when the turn declares one; a mode that withdraws `read` leaves the
    // section pointing at nothing in particular.
    let reader = tools
        .iter()
        .any(|tool| tool.name() == READ_TOOL)
        .then_some(READ_TOOL);
    prompt.push_str(&skills.prompt_section(reader));

    prompt.push_str(
        "\n# How to work\n\
         - Investigate before you change anything: read the file, or search for the callers, \
         rather than assuming what is there.\n\
         - Make the smallest change that solves the problem, in the style the surrounding code \
         is already written in.\n\
         - Leave alone what you were not asked to touch, including formatting.\n\
         - When a tool fails, read what it said and adjust; do not repeat the same call.\n\
         - Say what you did and what it means for the user, briefly. A tool's output is not your \
         answer unless it is the answer.\n",
    );

    if let Some(note) = provider_note(provider) {
        prompt.push_str("\n# This endpoint\n");
        prompt.push_str(note);
        prompt.push('\n');
    }

    for (title, files) in [
        ("Project instructions", &context.project_rules),
        ("Additional instructions", &context.append_system),
    ] {
        if !files.is_empty() {
            let _ = writeln!(prompt, "\n# {title}");
            for file in files {
                let _ = writeln!(
                    prompt,
                    "\n--- Begin instructions: {} ---\n{}\n--- End instructions ---",
                    file.path.display(),
                    file.content.trim()
                );
            }
        }
    }

    prompt
}

/// The note the platform behind this turn contributes, if it has one.
fn provider_note(provider: Option<&str>) -> Option<&'static str> {
    provider_spec(provider?).and_then(|spec| spec.prompt)
}

/// The conversation as text, for a summarizer to read.
///
/// Every message is rendered with its role, because what the user asked and what
/// the assistant claimed are different kinds of fact and a summary that cannot
/// tell them apart misreports the session. Tool blocks are included but their
/// bodies are cut short: a summary needs to know *that* a file was read, not the
/// thousand lines it contained.
pub(crate) fn conversation_text(history: &[Message]) -> String {
    let mut text = String::new();
    for message in history {
        for block in &message.content {
            match block {
                Content::Image { image } => {
                    text.push_str(&format!("[{} image attachment; binary content omitted from this textual checkpoint]\n", image.media_type));
                }
                Content::Text { text: body } => {
                    let _ = writeln!(text, "{}: {body}", role_label(message.role));
                }
                Content::ToolUse { call } => {
                    let _ = writeln!(
                        text,
                        "{}: called `{}` with {}",
                        role_label(message.role),
                        call.name,
                        clipped(&call.input.to_string(), 400)
                    );
                }
                Content::ToolResult { result } => {
                    let _ = writeln!(
                        text,
                        "tool `{}` returned{}: {}",
                        result.name,
                        if result.is_error { " an error" } else { "" },
                        clipped(&result.output, 600)
                    );
                }
            }
        }
    }
    text
}

/// What a role is called in the text handed to a summarizer.
fn role_label(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
    }
}

/// `text`, cut to `limit` characters with a marker when it was longer.
///
/// Counted in characters rather than bytes so a multi-byte character is never
/// split in half.
fn clipped(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let head: String = text.chars().take(limit).collect();
    format!("{head}… [cut here]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::Skill;
    use solaris_tools::ToolRegistry;

    /// The registry's tools as a turn would declare them.
    fn tools(mode: Mode) -> Vec<std::sync::Arc<dyn Tool>> {
        let registry = ToolRegistry::builtin();
        registry.select(crate::default_tool_names(mode), &Default::default())
    }

    #[test]
    fn the_prompt_names_what_solaris_is_and_what_it_may_do() {
        let build = system_prompt(Mode::Build, &tools(Mode::Build), &Skills::empty(), None);
        assert!(build.contains("You are solaris"));
        assert!(build.contains("build mode"));
        assert!(build.contains("# Using the tools"), "{build}");
        assert!(
            build.contains("`read`:"),
            "tool guidance is listed: {build}"
        );
        assert!(build.contains("# How to work"), "{build}");
        assert!(
            !build.contains("# This endpoint"),
            "no note without a provider"
        );
    }

    #[test]
    fn plan_mode_says_it_may_not_change_anything() {
        let plan = system_prompt(Mode::Plan, &tools(Mode::Plan), &Skills::empty(), None);
        assert!(plan.contains("plan mode"));
        assert!(
            plan.contains("Withdrawn") || plan.contains("propose"),
            "plan mode explains what it cannot do: {plan}"
        );
    }

    #[test]
    fn only_the_tools_of_this_turn_are_described() {
        let build = system_prompt(Mode::Build, &tools(Mode::Build), &Skills::empty(), None);
        let plan = system_prompt(Mode::Plan, &tools(Mode::Plan), &Skills::empty(), None);

        // The shell is declared in build mode and withdrawn in plan mode.
        assert!(
            build.contains(&format!("`{}`:", solaris_tools::SHELL_TOOL)),
            "{build}"
        );
        assert!(
            !plan.contains(&format!("`{}`:", solaris_tools::SHELL_TOOL)),
            "a withdrawn tool is not described: {plan}"
        );
        assert!(plan.contains("`grep`:"), "plan mode's tools are: {plan}");
    }

    #[test]
    fn a_platforms_own_note_is_appended_when_it_has_one() {
        let local = system_prompt(
            Mode::Build,
            &tools(Mode::Build),
            &Skills::empty(),
            Some("local"),
        );
        assert!(local.contains("# This endpoint"), "{local}");
        assert!(local.contains("small model"), "{local}");

        // Most platforms have nothing to add, so nothing is added.
        let anthropic = system_prompt(
            Mode::Build,
            &tools(Mode::Build),
            &Skills::empty(),
            Some("anthropic"),
        );
        assert!(!anthropic.contains("# This endpoint"), "{anthropic}");

        // An id the catalogue does not know is not an error.
        let unknown = system_prompt(
            Mode::Build,
            &tools(Mode::Build),
            &Skills::empty(),
            Some("nope"),
        );
        assert!(!unknown.contains("# This endpoint"), "{unknown}");
    }

    #[test]
    fn skills_are_listed_without_their_instructions() {
        let skills = Skills::from_skills(vec![
            Skill::sample(
                "pdf-tools",
                "Extract text from PDFs.",
                "/skills/pdf-tools/SKILL.md",
            ),
            Skill::sample("release", "Cut a release.", "/skills/release/SKILL.md"),
        ]);

        let prompt = system_prompt(Mode::Build, &tools(Mode::Build), &skills, None);
        assert!(prompt.contains("# Skills"), "{prompt}");
        assert!(
            prompt.contains("- `pdf-tools`: Extract text from PDFs."),
            "{prompt}"
        );
        assert!(prompt.contains("- `release`: Cut a release."), "{prompt}");
        assert!(
            prompt.contains("/skills/pdf-tools/SKILL.md"),
            "a skill says where its instructions are: {prompt}"
        );
        assert!(
            prompt.contains("`read` tool"),
            "a declared reader is named: {prompt}"
        );
    }

    #[test]
    fn a_session_without_skills_says_nothing_about_them() {
        let prompt = system_prompt(Mode::Build, &tools(Mode::Build), &Skills::empty(), None);
        assert!(!prompt.contains("# Skills"), "{prompt}");
    }

    #[test]
    fn a_turn_that_cannot_read_a_file_does_not_name_a_reader() {
        let skills =
            Skills::from_skills(vec![Skill::sample("a", "Something.", "/skills/a/SKILL.md")]);

        let prompt = system_prompt(Mode::Build, &[], &skills, None);
        assert!(prompt.contains("read its file"), "{prompt}");
        assert!(!prompt.contains("`read` tool"), "{prompt}");
    }

    #[test]
    fn a_tool_without_a_guideline_contributes_nothing() {
        /// A tool that only implements the required methods.
        struct Bare;

        #[async_trait::async_trait]
        impl Tool for Bare {
            fn name(&self) -> &str {
                "bare"
            }
            fn description(&self) -> &str {
                "Does something"
            }
            fn parameters(&self) -> serde_json::Value {
                serde_json::json!({ "type": "object" })
            }
            async fn run(
                &self,
                _input: serde_json::Value,
                _ctx: &solaris_tools::ToolContext,
            ) -> solaris_tools::ToolOutput {
                solaris_tools::ToolOutput::ok("")
            }
        }

        let tools: Vec<std::sync::Arc<dyn Tool>> = vec![std::sync::Arc::new(Bare)];
        let prompt = system_prompt(Mode::Build, &tools, &Skills::empty(), None);
        assert!(!prompt.contains("`bare`"), "{prompt}");
        assert!(
            !prompt.contains("# Using the tools"),
            "no section when no tool has guidance: {prompt}"
        );
    }

    #[test]
    fn the_summary_input_names_every_speaker() {
        let history = vec![
            Message::user("add a flag"),
            Message::assistant("I will read the parser."),
            Message::tool_use(solaris_backend::ToolCall::new(
                "call-1",
                "read",
                serde_json::json!({ "path": "src/parser.rs" }),
            )),
            Message::tool_result(solaris_backend::ToolResult::ok(
                &solaris_backend::ToolCall::new("call-1", "read", serde_json::json!({})),
                "fn main() {}",
            )),
        ];

        let text = conversation_text(&history);
        assert!(text.contains("user: add a flag"), "{text}");
        assert!(text.contains("assistant: I will read"), "{text}");
        assert!(text.contains("called `read`"), "{text}");
        assert!(text.contains("tool `read` returned"), "{text}");
        assert!(text.contains("src/parser.rs"), "{text}");
    }

    #[test]
    fn a_long_tool_body_is_cut_rather_than_repeated_in_full() {
        let call = solaris_backend::ToolCall::new("call-1", "read", serde_json::json!({}));
        let output = "é".repeat(700);
        let history = vec![Message::tool_result(solaris_backend::ToolResult::ok(
            &call, &output,
        ))];

        let text = conversation_text(&history);
        assert!(text.contains("[cut here]"), "{text}");
        assert!(
            text.chars().count() < 700,
            "the body was clipped: {}",
            text.chars().count()
        );
    }
}
