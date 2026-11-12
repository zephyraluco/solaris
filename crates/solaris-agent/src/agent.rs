//! The session: a resolved backend, the conversation it extends, and the tools a
//! turn may offer.

use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use solaris_backend::{AgentEventStream, BackendError};
use solaris_tools::{Cancel, ToolRegistry, ToolSelection};

use crate::approver::{AlwaysApprove, Approver};
use crate::compaction::Compaction;
use crate::conversation::Conversation;
use crate::mode::{Mode, default_tool_names};
use crate::prompt;
use crate::queue::Queue;
use crate::session::{self, DEFAULT_MAX_PARALLEL, TurnInput};
use crate::skills::Skills;
use crate::subagent::{SubagentRunner, SubagentTool};
use crate::{AgentBackendConfig, InputError, PromptContext, ResourceSnapshot, expand_input};

/// What a session is told once, and keeps for every turn of it.
#[derive(Clone)]
pub struct AgentOptions {
    /// How the mode's default tool set is edited.
    pub selection: ToolSelection,
    /// Who decides whether a call may run.
    pub approver: Arc<dyn Approver>,
    /// Directory relative tool paths resolve against.
    pub cwd: PathBuf,
    /// Where a truncated shell output is written.
    pub temp_dir: PathBuf,
    /// Most tool calls from one round that run at the same time, and the shared
    /// limit on active foreground/background children across rounds.
    ///
    /// `1` runs every round one call at a time; a round holding a tool that
    /// declared itself sequential does that whatever this says.
    ///
    /// There is no cap on how many rounds a turn may take: the loop runs until
    /// the model stops asking, fails, or the turn is cancelled.
    pub max_parallel: usize,
    /// When the conversation is summarized to stay inside the context window.
    pub compaction: Compaction,
    /// The skills this session may draw on.
    ///
    /// Held here rather than in the application because the system prompt is
    /// written here: what the model is told exists and what it can be pointed at
    /// by name have to be the same list.
    pub skills: Skills,
    /// Global and project instructions used by every turn.
    pub prompt_context: PromptContext,
    /// Whether this agent may delegate. Children always disable delegation.
    pub subagents: bool,
    /// Permission profile and aggregate request/time/cost limits.
    pub execution: crate::ExecutionSettings,
    /// Existing turn scope when constructing a child; leave None for a root.
    pub shared_budget: Option<Arc<crate::ExecutionBudget>>,
    /// Optional factory for explicit child model overrides under the current provider.
    pub child_backend: Option<crate::ChildBackendFactory>,
    /// Handles for cancelling individual active/queued children.
    pub tasks: crate::TaskControls,
}

impl Default for AgentOptions {
    fn default() -> Self {
        Self {
            selection: ToolSelection::none(),
            approver: Arc::new(AlwaysApprove),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            temp_dir: std::env::temp_dir(),
            max_parallel: DEFAULT_MAX_PARALLEL,
            compaction: Compaction::default(),
            skills: Skills::empty(),
            prompt_context: PromptContext::default(),
            subagents: true,
            execution: crate::ExecutionSettings::default(),
            shared_budget: None,
            child_backend: None,
            tasks: crate::TaskControls::default(),
        }
    }
}

impl AgentOptions {
    /// Options rooted at `cwd`, with the default temp directory.
    pub fn in_directory(cwd: impl Into<PathBuf>) -> Self {
        let cwd = cwd.into();
        let cwd = std::path::absolute(&cwd).unwrap_or(cwd);
        Self {
            cwd,
            ..Default::default()
        }
    }

    /// The same options with `approver` deciding about each call.
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Self {
        self.approver = approver;
        self
    }

    /// The same options with `selection` editing the mode's default tool set.
    pub fn with_selection(mut self, selection: ToolSelection) -> Self {
        self.selection = selection;
        self
    }

    /// The same options with `compaction` deciding when to summarize.
    pub fn with_compaction(mut self, compaction: Compaction) -> Self {
        self.compaction = compaction;
        self
    }

    /// The same options with `skills` as what this session may draw on.
    pub fn with_skills(mut self, skills: Skills) -> Self {
        self.skills = skills;
        self
    }

    /// Append instruction sources to the built-in system prompt.
    pub fn with_prompt_context(mut self, context: PromptContext) -> Self {
        self.prompt_context = context;
        self
    }

    /// Enable or disable the runtime's subagent tool.
    pub fn with_subagents(mut self, enabled: bool) -> Self {
        self.subagents = enabled;
        self
    }
}

/// A session with a model behind it.
///
/// The application holds one of these and asks it for turns; everything else —
/// which tools a turn declares, running them, feeding the results back, asking
/// again — happens behind this type. The application assembles the backend
/// configuration and tool registry before constructing the session.
///
/// The conversation lives here rather than in the application because it is the
/// model's view, not the user's: a summary has to still stand on the next turn, a
/// steered message has to still be where it landed, and neither is something the
/// transcript can be asked to reconstruct. [`Agent::clear`] is what forgets it.
#[derive(Clone)]
struct CachedPrompt {
    text: String,
    header: crate::projection::Header,
}

pub struct Agent {
    prompts: RwLock<Vec<CachedPrompt>>,
    /// Application-resolved backend and execution metadata.
    choice: AgentBackendConfig,
    active: Arc<std::sync::atomic::AtomicBool>,
    /// Every tool this platform has, built once so `edit` and `write` keep
    /// serialising against the same per-file locks for the whole session.
    registry: Arc<ToolRegistry>,
    options: AgentOptions,
    /// Stops the turn in flight, tools included.
    cancel: Cancel,
    /// The conversation every turn extends.
    conversation: Arc<Conversation>,
    /// Messages that correct the turn that is running.
    steering: Queue,
    /// Messages to run as the turn after this one.
    follow_up: Queue,
    resources: Arc<RwLock<ResourceSnapshot>>,
}

impl Agent {
    /// A session over `choice`, offering the registry's tools.
    pub fn new(
        choice: AgentBackendConfig,
        registry: Arc<ToolRegistry>,
        mut options: AgentOptions,
    ) -> Self {
        options.approver = Arc::new(crate::PolicyApprover {
            profile: options.execution.profile,
            read_only_tools: registry
                .names()
                .into_iter()
                .filter(|name| registry.get(name).is_some_and(|tool| tool.is_read_only()))
                .collect(),
            cwd: options.cwd.clone(),
            rules: options.execution.rules.clone(),
            fallback: Arc::clone(&options.approver),
        });
        let registry = if options.subagents {
            let mut registry = registry.as_ref().clone();
            registry.add(Arc::new(SubagentTool));
            Arc::new(registry)
        } else {
            registry
        };
        let registry = if registry
            .names()
            .iter()
            .filter(|name| name.starts_with("mcp_"))
            .count()
            > 8
        {
            let mut registry = registry.as_ref().clone();
            registry.add(Arc::new(crate::tool_search::ToolSearch));
            Arc::new(registry)
        } else {
            registry
        };
        let mut prompt = options.prompt_context.clone();
        prompt.prepare_paths(&options.cwd);
        let resources = Arc::new(RwLock::new(ResourceSnapshot {
            skills: options.skills.clone(),
            prompt,
            warnings: Vec::new(),
        }));
        let steering = Queue::for_resources(Arc::clone(&resources));
        let follow_up = Queue::for_resources(Arc::clone(&resources));
        let agent = Self {
            prompts: RwLock::new(Vec::new()),
            choice,
            active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            registry,
            options,
            cancel: Cancel::new(),
            conversation: Arc::new(Conversation::new()),
            steering,
            follow_up,
            resources,
        };
        agent.rebuild_prompts();
        agent
    }

    /// Attach a persistent session at an idle boundary and restore its context.
    pub fn attach_journal(
        &self,
        journal: Arc<crate::SessionJournal>,
    ) -> Result<(), crate::StoreError> {
        let _lease =
            crate::runtime::TurnLease::acquire(Arc::clone(&self.active)).ok_or_else(|| {
                crate::StoreError::Invalid("cannot attach a journal during a turn".into())
            })?;
        self.conversation.attach(journal)?;
        self.conversation
            .record_model(self.choice.provider_id.as_deref(), &self.choice.model);
        self.conversation
            .error()
            .map_or(Ok(()), |error| Err(crate::StoreError::Invalid(error)))
    }

    /// Attached persistent session, if enabled by the caller.
    pub fn journal(&self) -> Option<Arc<crate::SessionJournal>> {
        self.conversation.journal()
    }

    /// Attach an image at an idle boundary. Unsupported/unknown models fail locally.
    pub fn attach_image(&self, image: solaris_backend::InlineImage) -> Result<(), String> {
        let _lease = crate::runtime::TurnLease::acquire(Arc::clone(&self.active))
            .ok_or_else(|| "cannot attach an image during a turn".to_string())?;
        if self.capabilities().image_input != solaris_backend::Support::Supported {
            return Err("model image support is not confirmed; configure models.json or choose a vision model".into());
        }
        self.conversation
            .push(solaris_backend::Message::with_content(
                solaris_backend::Role::User,
                vec![solaris_backend::Content::Image { image }],
            ));
        self.conversation.error().map_or(Ok(()), Err)
    }

    pub fn active_tasks(&self) -> Vec<String> {
        self.options.tasks.active()
    }
    pub fn cancel_task(&self, id: &str) -> bool {
        self.options.tasks.cancel(id)
    }

    /// A handle that stops a turn in flight: a running command is killed and no
    /// further model call is made.
    ///
    /// Every turn of this session shares one, so a stop reaches whichever turn
    /// is running.
    pub fn cancel_handle(&self) -> Cancel {
        self.cancel.clone()
    }

    /// Replace transport credentials at an idle boundary without losing the
    /// conversation, queued input, resources, tools or cancellation handle.
    pub fn with_refreshed_backend(&self, choice: AgentBackendConfig) -> Self {
        // Persistence failures are retained by the conversation and block the
        // next request, including callers whose refresh API cannot return errors.
        self.conversation
            .record_model(choice.provider_id.as_deref(), &choice.model);
        let agent = Self {
            prompts: RwLock::new(Vec::new()),
            choice,
            active: Arc::clone(&self.active),
            registry: Arc::clone(&self.registry),
            options: self.options.clone(),
            cancel: self.cancel.clone(),
            conversation: Arc::clone(&self.conversation),
            steering: self.steering.clone(),
            follow_up: self.follow_up.clone(),
            resources: Arc::clone(&self.resources),
        };
        agent.rebuild_prompts();
        agent
    }

    /// Replace the factory used by subsequent child model overrides.
    pub fn with_child_backend(mut self, factory: crate::ChildBackendFactory) -> Self {
        self.options.child_backend = Some(factory);
        self
    }

    /// A handle for messages that correct the turn that is running.
    ///
    /// Queued, not delivered: the loop folds a message into the conversation
    /// before it asks the model again, so one sent while the model is answering
    /// or while a command runs reaches it at the start of the next round rather
    /// than in the middle of one. A message that arrives after the model's last
    /// word still counts — the loop reads the queue once more before deciding the
    /// turn is over, which is how "stop, do this instead" is heard.
    pub fn steering_handle(&self) -> Queue {
        self.steering.clone()
    }

    /// A handle for messages to run as the turn *after* the one that is running.
    ///
    /// The difference from [`steering_handle`](Agent::steering_handle) is when the
    /// model reads it: a steered message joins the turn in flight, while a
    /// follow-up becomes a turn of its own. Both reach the same conversation, so
    /// the choice is about attribution and not about capability — "then run the
    /// tests" wants its own turn and its own cost line.
    pub fn follow_up_handle(&self) -> Queue {
        self.follow_up.clone()
    }

    /// Forget the conversation, for `/clear`.
    ///
    /// The transcript is the application's to clear; what this clears is what the
    /// model would be sent, which is the part that costs money to carry.
    pub fn clear(&self) {
        self.conversation.clear();
    }

    /// How many messages the model will be sent next turn.
    ///
    /// A count rather than a token total because it is the one number the
    /// application has that the agent cannot state more honestly: what the
    /// conversation costs is the provider's to say, and it says it per turn in
    /// the usage.
    pub fn conversation_len(&self) -> usize {
        self.conversation.len()
    }

    /// Summarize the conversation now, at the user's request.
    ///
    /// Returns how many messages the summary replaced, or `None` when there was
    /// nothing worth summarizing or the model would not produce one.
    pub async fn compact(&self) -> Option<usize> {
        let _lease = crate::runtime::TurnLease::acquire(Arc::clone(&self.active))?;
        let cancel = self.cancel.begin_turn();
        let budget = crate::ExecutionBudget::new(self.options.execution.budget.clone());
        let inner: Arc<dyn solaris_backend::AgentBackend> =
            Arc::new(crate::budget::BudgetedBackend {
                inner: Arc::new(crate::billing::AccountingBackend {
                    inner: Arc::clone(&self.choice.backend),
                    conversation: Arc::clone(&self.conversation),
                    kind: crate::UsageKind::Manual,
                }),
                budget: Arc::clone(&budget),
            });
        tokio::select! {
            biased;
            _ = tokio::time::sleep_until(budget.deadline()) => { cancel.cancel(); None },
            result = session::compact_now(&inner, &self.conversation, self.options.compaction, &cancel) => result,
        }
    }

    /// Short human-readable label shown in the status bar.
    pub fn label(&self) -> &str {
        self.choice.backend.label()
    }

    /// The provider requests go to, when one was picked at all.
    pub fn provider_id(&self) -> Option<&str> {
        self.choice.provider_id.as_deref()
    }

    /// The model that will really be asked for, which is not always the one that
    /// was requested: an unnamed model is filled in with the provider's first
    /// offer.
    pub fn model(&self) -> &str {
        &self.choice.model
    }

    pub fn capabilities(&self) -> solaris_backend::ModelCapabilities {
        self.choice.backend.capabilities()
    }
    pub fn model_catalog_notice(&self) -> Option<String> {
        self.choice.backend.model_catalog_notice()
    }

    fn rebuild_prompts(&self) {
        let resources = self.resources.read().unwrap_or_else(|p| p.into_inner());
        let mut prompts = Vec::new();
        for mode in [Mode::Build, Mode::Plan] {
            let tools = self.turn_tools(mode);
            let text = prompt::build_system_prompt(
                mode,
                &tools,
                &resources.skills,
                self.choice.instructions.as_deref(),
                &self.options.cwd,
                &resources.prompt,
            );
            let specs: Vec<_> = tools
                .iter()
                .map(|tool| {
                    solaris_backend::ToolSpec::new(
                        tool.name(),
                        tool.description(),
                        tool.parameters(),
                    )
                })
                .collect();
            let mut header = crate::projection::Header::new(&self.model_key(mode), &text, &specs);
            if specs.iter().any(|spec| spec.name == "tool_search")
                && specs
                    .iter()
                    .filter(|spec| spec.name.starts_with("mcp_"))
                    .count()
                    > 8
            {
                let declared: Vec<_> = specs
                    .into_iter()
                    .filter(|spec| !spec.name.starts_with("mcp_"))
                    .collect();
                let initial =
                    crate::projection::Header::new(&self.model_key(mode), &text, &declared);
                header.overhead = initial.overhead;
                header.request_id = initial.request_id;
            }
            prompts.push(CachedPrompt { text, header });
        }
        self.conversation
            .set_headers(prompts.iter().map(|prompt| prompt.header.clone()).collect());
        *self.prompts.write().unwrap_or_else(|p| p.into_inner()) = prompts;
    }

    fn model_key(&self, mode: Mode) -> String {
        format!(
            "{:?}:{}:{mode:?}",
            self.choice.provider_id, self.choice.model
        )
    }

    pub fn context_usage(&self, mode: Mode) -> solaris_backend::ContextUsage {
        let unknown = solaris_backend::ContextUsage {
            tokens: None,
            window: self.context_window(),
            estimated: true,
        };
        let Ok(prompts) = self.prompts.try_read() else {
            return unknown;
        };
        prompts
            .get(if mode == Mode::Build { 0 } else { 1 })
            .map_or(unknown, |prompt| {
                self.conversation
                    .context_usage(&prompt.header.base_id, unknown.window)
            })
    }

    pub fn usage_totals(&self) -> Option<crate::UsageTotals> {
        self.conversation.usage_totals()
    }

    /// The models the endpoint offers, for the picker to choose from.
    pub async fn models(&self) -> Result<Vec<String>, BackendError> {
        self.choice.backend.models().await
    }

    /// The context window of the model this session will ask.
    fn context_window(&self) -> u32 {
        self.capabilities()
            .context_window
            .unwrap_or(self.choice.fallback_context_window)
    }

    /// The tools a turn in `mode` would declare.
    pub fn tool_names(&self, mode: Mode) -> Vec<String> {
        self.turn_tools(mode)
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    fn turn_tools(&self, mode: Mode) -> Vec<Arc<dyn solaris_tools::Tool>> {
        if self.capabilities().tool_calling == solaris_backend::Support::Unsupported {
            return Vec::new();
        }
        let mut defaults: Vec<&str> = default_tool_names(mode).to_vec();
        let external: Vec<String> = self
            .registry
            .names()
            .into_iter()
            .filter(|name| {
                name.starts_with("mcp_")
                    && self
                        .registry
                        .get(name)
                        .is_some_and(|tool| mode == Mode::Build || tool.is_read_only())
            })
            .collect();
        defaults.extend(external.iter().map(String::as_str));
        if self.registry.get("tool_search").is_some() {
            defaults.push("tool_search");
        }
        self.registry
            .select(&defaults, &self.options.selection)
            .into_iter()
            .filter(|tool| self.options.subagents || tool.name() != crate::SUBAGENT_TOOL)
            .filter(|tool| {
                mode == Mode::Build || !tool.name().starts_with("mcp_") || tool.is_read_only()
            })
            .collect()
    }

    /// The skills this session may draw on.
    pub fn skills(&self) -> Skills {
        self.resources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .skills
            .clone()
    }

    /// Replace resources for future turns without clearing conversation or queues.
    /// Call at an idle boundary; an active turn keeps its existing system prompt.
    /// ResourcePaths::load prepares paths on the loader worker. Hand-built
    /// snapshots are prepared here before publication, outside the resource lock.
    pub fn replace_resources(&self, mut snapshot: ResourceSnapshot) {
        {
            let current = self.resources.read().unwrap_or_else(|p| p.into_inner());
            for path in std::iter::once(&self.options.cwd).chain(
                snapshot
                    .prompt
                    .project_rules
                    .iter()
                    .chain(&snapshot.prompt.append_system)
                    .map(|file| &file.path),
            ) {
                if let Some(prepared) = current.prompt.model_paths.get(path) {
                    snapshot
                        .prompt
                        .model_paths
                        .entry(path.clone())
                        .or_insert_with(|| prepared.clone());
                }
            }
        }
        snapshot.prompt.prepare_paths(&self.options.cwd);
        *self
            .resources
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = snapshot;
        self.rebuild_prompts();
    }

    /// Current instructions, also used when the application rebuilds a backend.
    pub fn prompt_context(&self) -> PromptContext {
        self.resources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .prompt
            .clone()
    }

    /// Validate and expand input using the current resource snapshot.
    pub fn expand_input(&self, text: &str) -> Result<String, InputError> {
        let resources = self
            .resources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        expand_input(text, &resources.skills)
    }

    /// Run one turn over `prompt`, streaming what it produces.
    ///
    /// The turn runs in its own task, so the caller can keep drawing while a
    /// command runs; dropping the stream abandons it. A failure to answer is an
    /// [`AgentEvent::Error`](crate::AgentEvent::Error) in the stream rather than
    /// a start-up error, because the loop is what finds out.
    ///
    /// The prompt is recorded in the conversation before anything is asked, so a
    /// turn that is abandoned still leaves what the user said.
    pub fn run_turn(&self, prompt: impl Into<String>, mode: Mode) -> AgentEventStream {
        let Some(lease) = crate::runtime::TurnLease::acquire(Arc::clone(&self.active)) else {
            return Box::pin(futures::stream::once(async {
                crate::AgentEvent::Error("session already has an active turn".into())
            }));
        };
        let resources = self
            .resources
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let prompt = match expand_input(&prompt.into(), &resources.skills) {
            Ok(prompt) => prompt,
            Err(error) => {
                return Box::pin(futures::stream::once(async move {
                    crate::AgentEvent::Error(error.to_string())
                }));
            }
        };
        let mut turn_options = self.options.clone();
        turn_options.shared_budget =
            Some(self.options.shared_budget.clone().unwrap_or_else(|| {
                crate::ExecutionBudget::new(self.options.execution.budget.clone())
            }));
        let tools = self.turn_tools(mode);
        let subagent = tools
            .iter()
            .any(|tool| tool.name() == crate::SUBAGENT_TOOL)
            .then(|| {
                Arc::new(SubagentRunner {
                    choice: self.choice.clone(),
                    parent: self.journal(),
                    billing: Arc::clone(&self.conversation),
                    registry: Arc::clone(&self.registry),
                    options: turn_options.clone(),
                    resources: resources.clone(),
                    mode,
                    slots: tokio::sync::Semaphore::new(self.options.max_parallel.max(1)),
                })
            });

        let cached = self.prompts.read().unwrap_or_else(|p| p.into_inner())
            [if mode == Mode::Build { 0 } else { 1 }]
        .clone();
        let system = cached.text;
        drop(resources);

        let journal = self.journal();
        if let Some(journal) = &journal
            && let Err(error) = journal.append(&crate::SessionRecord::Prompt {
                text: prompt.clone(),
            })
        {
            return Box::pin(futures::stream::once(async move {
                crate::AgentEvent::Error(error.to_string())
            }));
        }
        let stream = session::start(TurnInput {
            inner: Arc::clone(&self.choice.backend),
            tools,
            options: turn_options,
            cancel: self.cancel.clone(),
            steering: self.steering.clone(),
            follow_up: self.follow_up.clone(),
            conversation: Arc::clone(&self.conversation),
            system,
            prompt,
            context_window: self.context_window(),
            model_key: self.model_key(mode),
            base_id: cached.header.base_id,
            subagent,
        });
        crate::runtime::leased_stream(crate::runtime::journal_stream(stream, journal), lease)
    }
}

impl std::fmt::Debug for Agent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Agent")
            .field("backend", &self.label())
            .field("tools", &self.registry.len())
            .field("messages", &self.conversation.len())
            .field("compaction", &self.options.compaction)
            .finish()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use futures::StreamExt;
    use serde_json::{Value, json};
    use solaris_backend::{
        AgentBackend, AgentEvent, AgentEventStream, BackendError, ToolCall, TurnRequest, Usage,
    };
    use solaris_tools::{SHELL_TOOL, Tool, ToolContext, ToolOutput};

    #[tokio::test]
    async fn saved_context_is_restored_and_transcript_events_survive_restart() {
        let path =
            std::env::temp_dir().join(format!("solaris-agent-resume-{}", std::process::id()));
        let store = crate::SessionStore::open(&path.join("sessions.sqlite")).expect("store");
        let journal = store
            .create(std::path::Path::new("/project"), "model", None)
            .expect("session");
        let id = journal.id().to_string();
        let complete = || {
            vec![
                AgentEvent::TextDelta("answer".into()),
                AgentEvent::TurnComplete {
                    usage: Usage::new(1, 1),
                    cost_usd: 0.0,
                },
            ]
        };
        let inner = Scripted::new(vec![complete(), complete()]);
        let first = agent(inner.clone());
        first.attach_journal(journal).expect("attach");
        let events = drain(first.run_turn("remember this", Mode::Plan)).await;
        assert!(matches!(
            events.last(),
            Some(AgentEvent::TurnComplete { .. })
        ));
        drop(first);
        let second = agent(inner.clone());
        second
            .attach_journal(store.resume(&id).expect("resume"))
            .expect("attach again");
        drain(second.run_turn("continue", Mode::Plan)).await;
        let requests = inner.requests();
        assert!(
            requests[1]
                .history
                .iter()
                .any(|m| m.text() == "remember this")
        );
        assert!(requests[1].history.iter().any(|m| m.text() == "answer"));
        assert_eq!(
            store
                .records(&id)
                .expect("records")
                .iter()
                .filter(|r| matches!(r, crate::SessionRecord::Prompt { .. }))
                .count(),
            2
        );
        drop(second);
        drop(store);
        std::fs::remove_dir_all(path).expect("cleanup");
    }

    #[tokio::test]
    async fn invalid_arguments_are_rejected_before_a_tool_can_write() {
        let dir = std::env::temp_dir().join(format!("solaris-schema-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("directory");
        let call = ToolCall::new(
            "bad-write",
            "write",
            json!({"path": "should-not-exist", "content": 42}),
        );
        let inner = Scripted::new(vec![
            vec![
                AgentEvent::ToolCall(call),
                AgentEvent::TurnComplete {
                    usage: Usage::new(1, 1),
                    cost_usd: 0.0,
                },
            ],
            vec![AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            }],
        ]);
        let agent = Agent::new(
            choice(inner),
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::in_directory(&dir),
        );
        let events = drain(agent.run_turn("write", Mode::Build)).await;
        assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolResult { result, .. } if result.is_error && result.output.contains("invalid tool arguments"))));
        assert!(!dir.join("should-not-exist").exists());
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn request_budget_stops_a_tool_loop_before_another_model_call() {
        let inner = Scripted::new(vec![vec![
            AgentEvent::ToolCall(ToolCall::new("read", "read", json!({"path":"Cargo.toml"}))),
            AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            },
        ]]);
        let mut options = AgentOptions::default();
        options.execution.budget.max_requests = 1;
        let agent = Agent::new(
            choice(inner.clone()),
            Arc::new(ToolRegistry::builtin()),
            options,
        );
        let events = drain(agent.run_turn("read", Mode::Plan)).await;
        assert_eq!(inner.requests().len(), 1);
        assert!(
            matches!(events.last(), Some(AgentEvent::Error(message)) if message.contains("request budget"))
        );
    }

    #[tokio::test]
    async fn a_large_external_catalog_is_declared_only_after_search() {
        struct External(String);
        #[async_trait]
        impl Tool for External {
            fn name(&self) -> &str {
                &self.0
            }
            fn description(&self) -> &str {
                "external fixture tool"
            }
            fn parameters(&self) -> Value {
                json!({"type":"object"})
            }
            async fn run(&self, _: Value, _: &ToolContext) -> ToolOutput {
                ToolOutput::ok("external reply")
            }
        }
        let done = || AgentEvent::TurnComplete {
            usage: Usage::new(1, 1),
            cost_usd: 0.0,
        };
        let inner = Scripted::new(vec![
            vec![
                AgentEvent::ToolCall(ToolCall::new(
                    "search",
                    "tool_search",
                    json!({"query":"mcp__fixture__tool_4"}),
                )),
                done(),
            ],
            vec![
                AgentEvent::ToolCall(ToolCall::new("external", "mcp__fixture__tool_4", json!({}))),
                done(),
            ],
            vec![done()],
        ]);
        let tools = (0..9)
            .map(|index| Arc::new(External(format!("mcp__fixture__tool_{index}"))) as Arc<dyn Tool>)
            .collect();
        let agent = Agent::new(
            choice(inner.clone()),
            Arc::new(ToolRegistry::with_tools(tools)),
            AgentOptions::default(),
        );
        let events = drain(agent.run_turn("use external tool", Mode::Build)).await;
        let requests = inner.requests();
        assert!(
            requests[0]
                .tools
                .iter()
                .all(|tool| !tool.name.starts_with("mcp_"))
        );
        assert!(
            requests[1]
                .tools
                .iter()
                .any(|tool| tool.name == "mcp__fixture__tool_4")
        );
        assert!(events.iter().any(|event| matches!(event, AgentEvent::ToolResult { result, .. } if result.output == "external reply")));
    }

    /// A backend that plays a scripted list of rounds and records what it was
    /// asked.
    struct Scripted {
        rounds: Mutex<VecDeque<Vec<AgentEvent>>>,
        seen: Mutex<Vec<TurnRequest>>,
    }

    impl Scripted {
        fn new(rounds: Vec<Vec<AgentEvent>>) -> Arc<Self> {
            Arc::new(Self {
                rounds: Mutex::new(rounds.into()),
                seen: Mutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<TurnRequest> {
            self.seen.lock().expect("lock").clone()
        }
    }

    #[async_trait]
    impl AgentBackend for Scripted {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
            Ok(solaris_backend::ModelClient::from_events(self))
        }

        async fn run_turn(&self, request: TurnRequest) -> Result<AgentEventStream, BackendError> {
            self.seen.lock().expect("lock").push(request);
            let events = self
                .rounds
                .lock()
                .expect("lock")
                .pop_front()
                .unwrap_or_else(|| vec![AgentEvent::Error("the script ran out".to_string())]);
            Ok(Box::pin(futures::stream::iter(events)))
        }

        fn label(&self) -> &str {
            "scripted"
        }
    }

    /// A backend that lists one model, for the pass-through checks.
    struct Listed;

    #[async_trait]
    impl AgentBackend for Listed {
        async fn model_client(
            self: std::sync::Arc<Self>,
        ) -> Result<solaris_backend::ModelClient, solaris_backend::BackendError> {
            Ok(solaris_backend::ModelClient::from_events(self))
        }

        async fn run_turn(&self, _request: TurnRequest) -> Result<AgentEventStream, BackendError> {
            Ok(Box::pin(futures::stream::iter(Vec::new())))
        }

        fn label(&self) -> &str {
            "listed"
        }

        async fn models(&self) -> Result<Vec<String>, BackendError> {
            Ok(vec!["first".to_string()])
        }
    }

    /// A tool that stops the turn it is running in.
    #[derive(Default)]
    struct Cancelling {
        cancel: Mutex<Option<Cancel>>,
        runs: AtomicUsize,
    }

    #[async_trait]
    impl Tool for Cancelling {
        fn name(&self) -> &str {
            "cancel-now"
        }

        fn description(&self) -> &str {
            "Cancel the turn"
        }

        fn parameters(&self) -> Value {
            json!({ "type": "object" })
        }

        async fn run(&self, _input: Value, _ctx: &ToolContext) -> ToolOutput {
            self.runs.fetch_add(1, Ordering::SeqCst);
            if let Some(cancel) = self.cancel.lock().expect("lock").take() {
                cancel.cancel();
            }
            ToolOutput::ok("done")
        }
    }

    /// A backend as the choice carries it, with a provider behind it.
    fn choice(backend: Arc<dyn AgentBackend>) -> AgentBackendConfig {
        AgentBackendConfig {
            backend,
            provider_id: Some("anthropic".into()),
            model: "claude-sonnet-4-5".to_string(),
            fallback_context_window: 128_000,
            instructions: None,
        }
    }

    /// A session over `backend`, offering every built-in tool.
    fn agent(backend: Arc<dyn AgentBackend>) -> Agent {
        Agent::new(
            choice(backend),
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        )
    }

    #[test]
    fn refreshing_backend_keeps_conversation_and_resource_identity() {
        let original = agent(Scripted::new(Vec::new()));
        original
            .conversation
            .push(solaris_backend::Message::user("remember this"));
        let refreshed = original.with_refreshed_backend(choice(Scripted::new(Vec::new())));
        assert_eq!(refreshed.conversation_len(), 1);
        assert!(Arc::ptr_eq(&original.conversation, &refreshed.conversation));
        assert!(Arc::ptr_eq(&original.resources, &refreshed.resources));
    }

    #[test]
    fn session_metadata_tracks_the_resolved_backend_on_attach_and_refresh() {
        let directory = tempfile::tempdir().expect("directory");
        let database = directory.path().join("sessions.sqlite");
        let store = crate::SessionStore::open(&database).expect("store");
        let journal = store
            .create(directory.path(), "unresolved-model", None)
            .expect("session");
        let original = agent(Scripted::new(Vec::new()));
        original.attach_journal(journal).expect("attach");
        let first = store.list(directory.path()).expect("sessions").remove(0);
        assert_eq!(first.provider.as_deref(), Some("anthropic"));
        assert_eq!(first.model, "claude-sonnet-4-5");
        original
            .conversation
            .push(crate::Message::user("remember this"));

        let mut next = choice(Scripted::new(Vec::new()));
        next.provider_id = Some("openai".into());
        next.model = "next-model".into();
        let refreshed = original.with_refreshed_backend(next);
        assert_eq!(refreshed.conversation_len(), 1);
        drop(original);
        drop(refreshed);
        drop(store);

        let store = crate::SessionStore::open(&database).expect("reopen store");
        let saved = store.list(directory.path()).expect("sessions").remove(0);
        assert_eq!(saved.provider.as_deref(), Some("openai"));
        assert_eq!(saved.model, "next-model");
        let resumed = store.resume(&saved.id).expect("resume");
        assert_eq!(
            resumed.context().expect("context")[0].text(),
            "remember this"
        );
    }

    #[tokio::test]
    async fn a_failed_backend_metadata_update_prevents_sending_history() {
        let directory = tempfile::tempdir().expect("directory");
        let database = directory.path().join("sessions.sqlite");
        let store = crate::SessionStore::open(&database).expect("store");
        let journal = store
            .create(directory.path(), "initial", None)
            .expect("session");
        let original = agent(Scripted::new(Vec::new()));
        original.attach_journal(journal).expect("attach");
        original
            .conversation
            .push(crate::Message::user("private history"));
        rusqlite::Connection::open(&database)
            .expect("connection")
            .execute_batch(
                "CREATE TRIGGER reject_model BEFORE UPDATE OF provider,model ON sessions
             BEGIN SELECT RAISE(ABORT, 'model persistence unavailable'); END;",
            )
            .expect("inject write failure");
        let backend = Scripted::new(Vec::new());
        let refreshed = original.with_refreshed_backend(choice(backend.clone()));
        let events = drain(refreshed.run_turn("next prompt", Mode::Build)).await;
        assert!(events.iter().any(|event| matches!(event, AgentEvent::Error(error) if error.contains("model persistence unavailable"))));
        assert!(
            backend.requests().is_empty(),
            "history must not reach a backend whose identity could not be persisted"
        );
    }

    #[test]
    fn refreshing_child_factory_replaces_credentials_without_resetting_the_session() {
        let original = agent(Scripted::new(Vec::new()))
            .with_child_backend(Arc::new(|_| Err("expired credential".into())));
        original
            .conversation
            .push(crate::Message::user("remember this"));
        let refreshed = original
            .with_refreshed_backend(choice(Scripted::new(Vec::new())))
            .with_child_backend(Arc::new(|model| {
                let mut next = choice(Scripted::new(Vec::new()));
                next.model = model.into();
                Ok(next)
            }));
        assert_eq!(
            refreshed.options.child_backend.as_ref().expect("factory")("child-model")
                .expect("fresh credential")
                .model,
            "child-model"
        );
        assert!(
            original
                .options
                .child_backend
                .as_ref()
                .expect("old factory")("child-model")
            .is_err()
        );
        assert!(Arc::ptr_eq(&original.conversation, &refreshed.conversation));
    }

    #[test]
    fn attachment_boundaries_take_the_same_lease_as_a_turn() {
        let original = agent(Scripted::new(Vec::new()));
        let lease =
            crate::runtime::TurnLease::acquire(Arc::clone(&original.active)).expect("lease");
        let directory = tempfile::tempdir().expect("directory");
        let store =
            crate::SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("journal");
        assert!(original.attach_journal(Arc::clone(&journal)).is_err());
        drop(lease);
        original.attach_journal(journal).expect("idle attach");
        assert!(!original.active.load(std::sync::atomic::Ordering::Acquire));
    }

    /// A tool as the registry and the loop want it.
    fn as_tool<T: Tool + 'static>(tool: Arc<T>) -> Arc<dyn Tool> {
        tool
    }

    /// Options that declare exactly `name` — the registry in a test holds tools
    /// the mode's default set does not name.
    fn only(name: &str) -> AgentOptions {
        AgentOptions::default().with_selection(ToolSelection {
            only: vec![name.to_string()],
            ..Default::default()
        })
    }

    /// Read a turn's stream to its end.
    async fn drain(stream: AgentEventStream) -> Vec<AgentEvent> {
        let mut stream = stream;
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn skill_input_expands_in_turns_and_both_queues_with_current_body() {
        let dir = std::env::temp_dir().join(format!("solaris-agent-input-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("pdf")).expect("directory");
        let path = dir.join("pdf/SKILL.md");
        std::fs::write(
            &path,
            "---\ndescription: PDF work\ndisable-model-invocation: true\n---\nOld body",
        )
        .expect("skill");
        let skills = Skills::load(
            &[crate::SkillRoot::new(&dir, crate::SkillSource::Project)],
            &[],
        );
        let completion = || {
            vec![
                AgentEvent::TextDelta("done".to_string()),
                AgentEvent::TurnComplete {
                    usage: Usage::new(1, 1),
                    cost_usd: 0.0,
                },
            ]
        };
        let inner = Scripted::new(vec![completion(), completion()]);
        let agent = Agent::new(
            choice(inner.clone()),
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::in_directory(&dir).with_skills(skills),
        );
        std::fs::write(&path, "---\ndescription: PDF work\ndisable-model-invocation: true\n---\nNew body: read references/formats.md").expect("update");
        agent
            .steering_handle()
            .try_push("/skill:pdf steer")
            .expect("steering");
        agent
            .follow_up_handle()
            .try_push("/skill:pdf follow")
            .expect("follow-up");
        drain(agent.run_turn("/skill:pdf initial", Mode::Plan)).await;
        let requests = inner.requests();
        assert_eq!(requests.len(), 2);
        let user_text: Vec<_> = requests
            .last()
            .expect("request")
            .history
            .iter()
            .filter(|message| message.role == crate::Role::User)
            .map(crate::Message::text)
            .collect();
        assert_eq!(user_text.len(), 3);
        for (text, request) in user_text.iter().zip(["initial", "steer", "follow"]) {
            assert!(text.contains("New body"), "{text}");
            assert!(
                text.contains(&solaris_tools::environment::display_path(&dir.join("pdf"))),
                "{text}"
            );
            assert!(text.contains(&format!("Request: {request}")), "{text}");
            assert!(!text.contains("Old body"));
        }
        assert!(
            !requests[0].history[0].text().contains("PDF work"),
            "hidden skill is not advertised"
        );
        std::fs::remove_dir_all(dir).expect("cleanup");
    }

    #[tokio::test]
    async fn invalid_skill_input_never_reaches_backend_or_conversation() {
        let inner = Scripted::new(Vec::new());
        let agent = agent(inner.clone());
        let events = drain(agent.run_turn("/skill:missing request", Mode::Build)).await;
        assert!(
            matches!(events.as_slice(), [AgentEvent::Error(message)] if message.contains("no skill named"))
        );
        assert!(inner.requests().is_empty());
        assert_eq!(agent.conversation_len(), 0);
        assert!(agent.follow_up_handle().try_push("/skill:missing").is_err());
        assert!(agent.follow_up_handle().is_empty());
    }

    #[tokio::test]
    async fn replacing_resources_preserves_history_and_updates_next_system_prompt() {
        let completion = || {
            vec![
                AgentEvent::TextDelta("done".to_string()),
                AgentEvent::TurnComplete {
                    usage: Usage::new(1, 1),
                    cost_usd: 0.0,
                },
            ]
        };
        let inner = Scripted::new(vec![completion(), completion()]);
        let agent = agent(inner.clone());
        drain(agent.run_turn("first request", Mode::Plan)).await;
        let before = agent.conversation_len();
        agent.replace_resources(ResourceSnapshot {
            prompt: PromptContext {
                project_rules: vec![crate::PromptFile {
                    path: agent.options.cwd.join("AGENTS.md"),
                    content: "Project rule marker".to_string(),
                }],
                append_system: vec![crate::PromptFile {
                    path: agent.options.cwd.join("APPEND_SYSTEM.md"),
                    content: "Addendum marker".to_string(),
                }],
                ..Default::default()
            },
            ..Default::default()
        });
        assert_eq!(agent.conversation_len(), before);
        let prepared = agent.prompt_context();
        for path in [
            &agent.options.cwd,
            &prepared.project_rules[0].path,
            &prepared.append_system[0].path,
        ] {
            assert!(
                prepared.model_paths.contains_key(path),
                "replacement must prepare {path:?}"
            );
        }
        drain(agent.run_turn("second request", Mode::Plan)).await;
        let requests = inner.requests();
        let system = requests[1].history[0].text();
        assert!(system.contains("Project rule marker"));
        assert!(system.contains("Addendum marker"));
        assert!(system.contains(&solaris_tools::environment::display_path(
            &agent.options.cwd
        )));
        assert!(
            requests[1]
                .history
                .iter()
                .any(|message| message.text() == "first request")
        );
    }

    #[test]
    fn build_mode_offers_more_than_plan_mode() {
        let agent = agent(Scripted::new(Vec::new()));

        let build = agent.tool_names(Mode::Build);
        assert!(build.contains(&"read".to_string()));
        assert!(build.contains(&SHELL_TOOL.to_string()));
        assert!(build.contains(&"edit".to_string()));
        assert!(build.contains(&"write".to_string()));

        assert_eq!(
            agent.tool_names(Mode::Plan),
            ["find", "grep", "ls", "read", "subagent"]
        );
    }

    #[test]
    fn an_allowlist_overrides_what_the_mode_offers() {
        let agent = Agent::new(
            choice(Scripted::new(Vec::new())),
            Arc::new(ToolRegistry::builtin()),
            only("read"),
        );

        assert_eq!(agent.tool_names(Mode::Build), ["read"]);
        assert_eq!(agent.tool_names(Mode::Plan), ["read"]);
    }

    #[tokio::test]
    async fn a_turn_declares_the_tools_its_mode_offers() {
        let inner = Scripted::new(vec![Vec::new()]);
        let agent = agent(inner.clone());

        drain(agent.run_turn("hi", Mode::Plan)).await;

        let mut names: Vec<String> = inner.requests()[0]
            .tools
            .iter()
            .map(|spec| spec.name.clone())
            .collect();
        names.sort();
        assert_eq!(names, ["find", "grep", "ls", "read", "subagent"]);
    }

    #[tokio::test]
    async fn a_turn_declares_what_the_tool_describes_itself_as() {
        let inner = Scripted::new(vec![Vec::new()]);
        let tool = Arc::new(Cancelling::default());
        let agent = Agent::new(
            choice(inner.clone()),
            Arc::new(ToolRegistry::with_tools(vec![as_tool(Arc::clone(&tool))])),
            only("cancel-now"),
        );

        drain(agent.run_turn("hi", Mode::Build)).await;

        let spec = &inner.requests()[0].tools[0];
        assert_eq!(spec.name, tool.name());
        assert_eq!(spec.description, tool.description());
        assert_eq!(spec.parameters, tool.parameters());
    }

    #[tokio::test]
    async fn the_backends_label_and_models_pass_through() {
        let agent = agent(Arc::new(Listed));

        assert_eq!(agent.label(), "listed");
        assert_eq!(agent.models().await.expect("a list"), ["first"]);
    }

    #[test]
    fn the_session_reports_the_choice_it_was_built_from() {
        let agent = agent(Scripted::new(Vec::new()));

        assert_eq!(agent.provider_id(), Some("anthropic"));
        assert_eq!(agent.model(), "claude-sonnet-4-5");
    }

    #[tokio::test]
    async fn refreshing_backend_replaces_injected_instructions_and_window() {
        let first = Scripted::new(Vec::new());
        let mut initial = choice(first);
        initial.instructions = Some("Initial caller instructions".into());
        initial.fallback_context_window = 32_000;
        let original = Agent::new(
            initial,
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        );
        assert_eq!(original.context_window(), 32_000);
        let backend = Scripted::new(vec![vec![AgentEvent::TextDelta("done".into())]]);
        let mut next = choice(backend.clone());
        next.instructions = Some("Replacement caller instructions".into());
        next.fallback_context_window = 64_000;
        let refreshed = original.with_refreshed_backend(next);
        assert_eq!(refreshed.context_window(), 64_000);
        drain(refreshed.run_turn("hi", Mode::Build)).await;
        let requests = backend.requests();
        let system = requests[0].history[0].text();
        assert!(
            system.contains("Replacement caller instructions"),
            "{system}"
        );
        assert!(!system.contains("Initial caller instructions"), "{system}");
    }

    #[test]
    fn a_session_nothing_was_connected_for_says_so() {
        let mut choice = choice(Scripted::new(Vec::new()));
        choice.provider_id = None;
        choice.model = String::new();

        let agent = Agent::new(
            choice,
            Arc::new(ToolRegistry::builtin()),
            AgentOptions::default(),
        );
        assert_eq!(agent.provider_id(), None);
        assert_eq!(agent.model(), "");
    }

    #[tokio::test]
    async fn the_handle_a_session_hands_out_stops_the_turn_it_is_running() {
        let tool = Arc::new(Cancelling::default());
        let inner = Scripted::new(vec![vec![
            AgentEvent::ToolCall(ToolCall::new("call-1", "cancel-now", json!({}))),
            AgentEvent::TurnComplete {
                usage: Usage::new(1, 1),
                cost_usd: 0.0,
            },
        ]]);
        let agent = Agent::new(
            choice(inner),
            Arc::new(ToolRegistry::with_tools(vec![as_tool(Arc::clone(&tool))])),
            only("cancel-now"),
        );

        // The tool stops the turn through the handle the application holds, so
        // the stop has to reach the loop that is running.
        *tool.cancel.lock().expect("lock") = Some(agent.cancel_handle());

        let events = drain(agent.run_turn("hi", Mode::Build)).await;

        assert_eq!(tool.runs.load(Ordering::SeqCst), 1);
        let Some(AgentEvent::Error(message)) = events.last() else {
            panic!("a cancelled turn ends in an error: {events:?}");
        };
        assert!(message.contains("cancelled"), "{message}");
    }
    #[tokio::test]
    async fn restored_projection_reports_usage_without_background_validation_or_model_calls() {
        let directory = tempfile::tempdir().expect("directory");
        let store =
            crate::SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("journal");
        let first = agent(Scripted::new(vec![vec![
            AgentEvent::TextDelta("reply".into()),
            AgentEvent::TurnComplete {
                usage: Usage::new(100, 10),
                cost_usd: 0.0,
            },
        ]]));
        first.attach_journal(Arc::clone(&journal)).expect("attach");
        drain(first.run_turn("hello", Mode::Build)).await;
        let backend = Scripted::new(vec![]);
        let restored = agent(backend.clone());
        restored.attach_journal(journal).expect("restore");
        assert_eq!(restored.context_usage(Mode::Build).tokens, Some(110));
        assert!(backend.requests().is_empty());
    }

    #[test]
    fn context_queries_do_not_rebuild_prompts_or_call_tool_metadata() {
        struct ObservedTool(Arc<AtomicUsize>);
        #[async_trait]
        impl Tool for ObservedTool {
            fn name(&self) -> &str {
                "read"
            }
            fn description(&self) -> &str {
                "Read"
            }
            fn parameters(&self) -> Value {
                self.0.fetch_add(1, Ordering::SeqCst);
                json!({})
            }
            fn guideline(&self) -> Option<&'static str> {
                self.0.fetch_add(1, Ordering::SeqCst);
                Some("Read a file")
            }
            async fn run(&self, _: Value, _: &ToolContext) -> ToolOutput {
                ToolOutput::ok("done")
            }
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let agent = Agent::new(
            choice(Scripted::new(vec![])),
            Arc::new(ToolRegistry::with_tools(vec![Arc::new(ObservedTool(
                Arc::clone(&calls),
            ))])),
            AgentOptions::default(),
        );
        let before = calls.load(Ordering::SeqCst);
        for _ in 0..20 {
            agent.context_usage(Mode::Build);
        }
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }
    #[tokio::test]
    async fn request_billing_survives_failure_without_a_terminal_completion() {
        let directory = tempfile::tempdir().expect("directory");
        let store =
            crate::SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("journal");
        let backend = Scripted::new(vec![
            vec![
                AgentEvent::ToolCall(ToolCall::new("one", "missing", json!({}))),
                AgentEvent::TurnComplete {
                    usage: Usage::new(100, 10),
                    cost_usd: 0.01,
                },
            ],
            vec![AgentEvent::Error("next request failed".into())],
        ]);
        let original = agent(backend);
        original
            .attach_journal(Arc::clone(&journal))
            .expect("attach");
        let events = drain(original.run_turn("work", Mode::Build)).await;
        assert!(matches!(events.last(), Some(AgentEvent::Error(_))));
        assert_eq!(original.usage_totals().expect("totals").usage.total(), 110);
        let restored = agent(Scripted::new(vec![]));
        restored.attach_journal(journal).expect("restore");
        assert_eq!(restored.usage_totals(), original.usage_totals());
    }

    #[tokio::test]
    async fn manual_summaries_are_billed_without_changing_the_context_baseline() {
        let backend = Scripted::new(vec![vec![
            AgentEvent::TextDelta("summary".into()),
            AgentEvent::TurnComplete {
                usage: Usage::new(500, 50),
                cost_usd: 0.02,
            },
        ]]);
        let mut agent = agent(backend);
        agent.options.compaction.keep_recent_tokens = 1;
        agent
            .conversation
            .push(solaris_backend::Message::user("old context"));
        agent
            .conversation
            .push(solaris_backend::Message::assistant("old reply"));
        agent
            .conversation
            .push(solaris_backend::Message::user("new question"));
        assert!(agent.compact().await.is_some());
        assert_eq!(agent.usage_totals().expect("totals").usage.total(), 550);
        let context = agent.context_usage(Mode::Build);
        assert!(
            context.estimated,
            "summary billing is not a context measurement"
        );
        assert_ne!(context.tokens, Some(550));
    }
}
