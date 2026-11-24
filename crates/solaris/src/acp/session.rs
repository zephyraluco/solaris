//! ACP v1 transport over the application session runtime.

use agent_client_protocol::v1::schema::*;
use agent_client_protocol::{Error, v1::ConnectionTo};
use futures::StreamExt;
use solaris::application::{
    execution::with_approval,
    history::{ConnectionOverrides, find_session},
    session::{SessionTarget, build_agent, child_backend, open_journal},
};
use solaris_agent::{
    AgentEvent, AgentOptions, Approval, ApprovalRequest, ChannelApprover, ExecutionSettings, Mode,
    ResourcePaths, Runtime, RuntimeCommand, SessionRecord, SessionRuntime, SessionStore,
    ToolRegistry, ToolSelection,
};
use solaris_provider::{AuthStore, BackendOptions};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::{Mutex, mpsc};

use super::{input, questions};

pub(crate) struct Settings {
    pub store: Arc<SessionStore>,
    pub resources: ResourcePaths,
    pub registry: ToolRegistry,
    pub selection: ToolSelection,
    pub execution: ExecutionSettings,
    pub auth: AuthStore,
    pub auth_path: Option<PathBuf>,
    pub backend_options: BackendOptions,
    pub model: String,
    pub mode: Mode,
    pub overrides: ConnectionOverrides,
    pub confirm_tools: bool,
    pub mcp: solaris_mcp::McpConfig,
}

pub(super) struct Server {
    pub(super) settings: Settings,
    pub(super) runtime: Runtime,
    pub(super) sessions: Mutex<HashMap<String, Arc<Session>>>,
    pub(super) forms: std::sync::atomic::AtomicBool,
}

pub(super) struct Session {
    _mcp: solaris_mcp::McpManager,
    pub(super) runtime: SessionRuntime,
    config: Mutex<Config>,
    pub(super) operation: Arc<Mutex<()>>,
    pub(super) forms: bool,
}

struct Config {
    mode: Mode,
    model: String,
    models: Vec<(String, String)>,
    factory: Arc<dyn Fn(&str) -> solaris_provider::BackendChoice + Send + Sync>,
}

impl Config {
    fn options(&self) -> Vec<SessionConfigOption> {
        let mut options = vec![
            SessionConfigOption::select(
                "mode",
                "Mode",
                mode_id(self.mode),
                vec![
                    SessionConfigSelectOption::new("default", "Default"),
                    SessionConfigSelectOption::new("plan", "Plan"),
                    SessionConfigSelectOption::new("auto", "Auto"),
                    SessionConfigSelectOption::new("ask", "Ask"),
                ],
            )
            .category(SessionConfigOptionCategory::Mode),
        ];
        if !self.model.is_empty() {
            options.push(
                SessionConfigOption::select(
                    "model",
                    "Model",
                    self.model.clone(),
                    self.models
                        .iter()
                        .map(|(id, name)| SessionConfigSelectOption::new(id.clone(), name.clone()))
                        .collect::<Vec<_>>(),
                )
                .category(SessionConfigOptionCategory::Model),
            );
        }
        options
    }
}

fn mode_id(mode: Mode) -> &'static str {
    mode.id()
}

pub(super) fn invalid(message: impl ToString) -> Error {
    Error::invalid_params().data(message.to_string())
}

pub(super) fn failure(message: impl ToString) -> Error {
    Error::internal_error().data(message.to_string())
}

pub(super) type ClientConnection = ConnectionTo<agent_client_protocol::Client>;

impl Server {
    pub(super) async fn session(&self, id: &SessionId) -> Result<Arc<Session>, Error> {
        self.sessions
            .lock()
            .await
            .get(id.0.as_ref())
            .cloned()
            .ok_or_else(|| invalid("unknown Solaris session"))
    }

    pub(super) async fn open(
        &self,
        cwd: &Path,
        resume: Option<&SessionId>,
        mcp_servers: Vec<McpServer>,
        connection: &ClientConnection,
    ) -> Result<(SessionId, Vec<SessionConfigOption>), Error> {
        if !cwd.is_absolute() {
            return Err(invalid("cwd must be absolute"));
        }
        let cwd = cwd.canonicalize().map_err(invalid)?;
        if !cwd.is_dir() {
            return Err(invalid("cwd must be a directory"));
        }

        let settings = &self.settings;
        let mcp_config = input::merge_mcp(settings.mcp.clone(), mcp_servers)?;
        let mut auth = settings.auth.clone();
        let mut model = settings.model.clone();
        let journal = if let Some(id) = resume {
            let info = find_session(&settings.store, &cwd, id.0.as_ref())
                .map_err(failure)?
                .ok_or_else(|| invalid("session not found in this directory"))?;
            settings.overrides.restore(info, &mut model, &mut auth);
            Some(
                open_journal(
                    &settings.store,
                    &cwd,
                    &model,
                    SessionTarget::Resume(id.0.as_ref()),
                    None,
                )
                .map_err(failure)?,
            )
        } else {
            None
        };
        let factory = Arc::new(solaris_provider::refreshing_backend_factory(
            auth,
            settings.auth_path.clone(),
            settings.backend_options.clone(),
        ));
        let choice = factory(&model);
        let mut models = choice.provider_id.as_deref().map_or_else(Vec::new, |id| {
            settings.backend_options.configuration.models(id)
        });
        if !choice.model.is_empty() && !models.iter().any(|(id, _)| id == &choice.model) {
            models.insert(0, (choice.model.clone(), choice.model.clone()));
        }
        let journal = match journal {
            Some(journal) => journal,
            None => open_journal(
                &settings.store,
                &cwd,
                &choice.model,
                SessionTarget::New,
                None,
            )
            .map_err(failure)?,
        };
        let id = SessionId::new(journal.id());
        let history = if resume.is_some() {
            journal.history().map_err(failure)?.0
        } else {
            Vec::new()
        };
        let mut resources = settings.resources.clone();
        resources.cwd.clone_from(&cwd);
        let resources = resources.load().map_err(failure)?;
        let mcp = solaris_mcp::McpManager::connect(mcp_config, &cwd).await;
        let mut registry = settings.registry.clone();
        for tool in mcp.tools() {
            registry.add(Arc::clone(tool));
        }
        for status in &mcp.status {
            if !status.connected {
                eprintln!("MCP {}: {}", status.name, status.message);
            }
        }
        let (approval_tx, approval_rx) = mpsc::unbounded_channel();
        let mut options =
            AgentOptions::in_directory(cwd).with_selection(settings.selection.clone());
        if settings.confirm_tools {
            options.approver = Arc::new(ChannelApprover::new(approval_tx));
        }
        options.execution = with_approval(settings.execution.clone(), settings.confirm_tools);
        options.questions = Some(Default::default());
        let child_backend = factory.clone();
        let config = Config {
            mode: settings.mode,
            model: choice.model.clone(),
            models,
            factory,
        };
        let config_options = config.options();
        let agent = build_agent(
            choice,
            Arc::new(registry),
            options,
            resources,
            move |model| child_backend(model),
        );
        agent.attach_journal(journal).map_err(failure)?;
        let runtime = self.runtime.open(agent).map_err(failure)?;

        replay(connection, &id, history)?;
        let approval_connection = connection.clone();
        let approval_session = runtime.clone();
        let approval_id = id.clone();
        connection.spawn(async move {
            approve_calls(
                approval_connection,
                approval_id,
                approval_session,
                approval_rx,
            )
            .await;
            Ok(())
        })?;
        self.sessions.lock().await.insert(
            id.0.to_string(),
            Arc::new(Session {
                _mcp: mcp,
                runtime,
                config: Mutex::new(config),
                operation: Arc::default(),
                forms: self.forms.load(std::sync::atomic::Ordering::Relaxed),
            }),
        );
        Ok((id, config_options))
    }
}

async fn approve_calls(
    connection: ClientConnection,
    id: SessionId,
    runtime: SessionRuntime,
    mut requests: mpsc::UnboundedReceiver<ApprovalRequest>,
) {
    loop {
        let request = tokio::select! {
            _ = connection.incoming_closed() => return,
            request = requests.recv() => request,
        };
        let Some(ApprovalRequest { call, mut reply }) = request else {
            return;
        };
        if reply.is_closed() {
            continue;
        }
        let request = RequestPermissionRequest::new(
            id.clone(),
            ToolCallUpdate::new(
                call_id(runtime.snapshot().task_id, &call.id),
                ToolCallUpdateFields::new()
                    .title(call.name)
                    .raw_input(call.input),
            ),
            vec![
                PermissionOption::new("allow", "Allow once", PermissionOptionKind::AllowOnce),
                PermissionOption::new("reject", "Reject", PermissionOptionKind::RejectOnce),
            ],
        );
        let response = tokio::select! {
            _ = connection.incoming_closed() => return,
            _ = reply.closed() => continue,
            response = connection.send_request(request).block_task() => response,
        };
        let allowed = matches!(response, Ok(response) if matches!(response.outcome,
            RequestPermissionOutcome::Selected(ref outcome) if outcome.option_id.0.as_ref() == "allow"));
        let _ = reply.send(if allowed {
            Approval::Approved
        } else {
            Approval::Denied
        });
    }
}

pub(super) fn call_id(run: Option<u64>, id: &str) -> ToolCallId {
    ToolCallId::new(match run {
        Some(run) => format!("{run}:{id}"),
        None => id.to_owned(),
    })
}

fn notify(
    connection: &ClientConnection,
    id: &SessionId,
    update: SessionUpdate,
) -> Result<(), Error> {
    connection.send_notification(SessionNotification::new(id.clone(), update))
}

fn emit(
    connection: &ClientConnection,
    id: &SessionId,
    run: Option<u64>,
    event: AgentEvent,
) -> Result<(), Error> {
    let update = match event {
        AgentEvent::TextDelta(text) => {
            SessionUpdate::AgentMessageChunk(ContentChunk::new(text.into()))
        }
        AgentEvent::ThinkingDelta(text) => {
            SessionUpdate::AgentThoughtChunk(ContentChunk::new(text.into()))
        }
        AgentEvent::ToolCall(call) => SessionUpdate::ToolCall(
            ToolCall::new(call_id(run, &call.id), call.name)
                .raw_input(call.input)
                .status(ToolCallStatus::InProgress),
        ),
        AgentEvent::ToolResult { result, .. } => {
            SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                call_id(run, &result.id),
                ToolCallUpdateFields::new()
                    .status(if result.is_error {
                        ToolCallStatus::Failed
                    } else {
                        ToolCallStatus::Completed
                    })
                    .content(vec![ToolCallContent::Content(Content::new(result.output))]),
            ))
        }
        AgentEvent::FollowUp { prompt } | AgentEvent::Steered { text: prompt } => {
            SessionUpdate::UserMessageChunk(ContentChunk::new(prompt.into()))
        }
        _ => return Ok(()),
    };
    notify(connection, id, update)
}

fn replay(
    connection: &ClientConnection,
    id: &SessionId,
    mut history: Vec<SessionRecord>,
) -> Result<(), Error> {
    if let Some(cleared) = history
        .iter()
        .rposition(|record| matches!(record, SessionRecord::Cleared))
    {
        history.drain(..=cleared);
    }
    // A durable Runtime prompt follows the expanded legacy Prompt record.
    // Defer the legacy text until the next display event so it is emitted once.
    let mut legacy_prompt = None;
    let mut user_index = 0;
    let mut user_message = |text: String| {
        user_index += 1;
        notify(
            connection,
            id,
            SessionUpdate::UserMessageChunk(
                ContentChunk::new(text.into())
                    .message_id(MessageId::new(format!("user:{user_index}"))),
            ),
        )
    };
    for record in history {
        let (run, event, prompt) = match record {
            SessionRecord::RuntimeEvent { envelope } => {
                (envelope.task_id, envelope.event, envelope.prompt)
            }
            SessionRecord::Event { event } => (None, event, None),
            SessionRecord::Prompt { text } => {
                if let Some(previous) = legacy_prompt.replace(text) {
                    user_message(previous)?;
                }
                continue;
            }
            _ => continue,
        };
        let pending = legacy_prompt.take();
        if let Some(text) = prompt.or(pending) {
            user_message(text)?;
        }
        match event {
            AgentEvent::FollowUp { prompt } | AgentEvent::Steered { text: prompt } => {
                user_message(prompt)?
            }
            event => emit(connection, id, run, event)?,
        }
    }
    if let Some(text) = legacy_prompt {
        user_message(text)?;
    }
    Ok(())
}

pub(super) async fn start_prompt(
    session: &Session,
    request: PromptRequest,
) -> Result<solaris_agent::RuntimeEventStream, Error> {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in request.prompt {
        match part {
            ContentBlock::Text(part) => text.push(part.text),
            ContentBlock::Image(image) => {
                if images.len() == 8 {
                    return Err(invalid("at most eight images per prompt"));
                }
                images.push(input::image(image)?);
            }
            _ => return Err(invalid("Solaris ACP accepts text and image prompts")),
        }
    }
    let text = text.join("\n");
    if text.trim().is_empty() && images.is_empty() {
        return Err(invalid("prompt is empty"));
    }
    let text = if text.trim().is_empty() {
        "See the attached image.".into()
    } else {
        text
    };
    let mode = session.config.lock().await.mode;
    let command = if images.is_empty() {
        RuntimeCommand::Prompt { text, mode }
    } else {
        RuntimeCommand::PromptWithImages { text, mode, images }
    };
    session.runtime.execute(command).await.map_err(failure)
}

pub(super) async fn finish_prompt(
    session: &Session,
    id: &SessionId,
    connection: &ClientConnection,
    mut events: solaris_agent::RuntimeEventStream,
) -> Result<PromptResponse, Error> {
    let mut terminal = None;
    let mut truncated = false;
    loop {
        let event = tokio::select! {
            _ = connection.incoming_closed() => {
                session.runtime.cancel_handle().cancel();
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            event = events.next() => event,
        };
        let Some(event) = event else {
            break;
        };
        match &event.event {
            AgentEvent::TurnComplete { .. } => {
                terminal = Some(Ok(if truncated {
                    StopReason::MaxTokens
                } else {
                    StopReason::EndTurn
                }))
            }
            AgentEvent::Error(error) => terminal = Some(Err(failure(error))),
            AgentEvent::OutputTruncated => truncated = true,
            AgentEvent::QuestionRequested(question) => {
                let question = question.clone();
                let client = connection.clone();
                let runtime = session.runtime.clone();
                let id = id.clone();
                let forms = session.forms;
                let run = event.task_id;
                connection.spawn(async move {
                    questions::answer(client, runtime, id, run, question, forms).await;
                    Ok(())
                })?;
            }
            _ => {}
        }
        emit(connection, id, event.task_id, event.event)?;
    }
    match session.runtime.snapshot().stop_reason {
        Some(solaris_agent::StopReason::UserCancelled) => {
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }
        Some(solaris_agent::StopReason::StepLimit) => {
            return Ok(PromptResponse::new(StopReason::MaxTurnRequests));
        }
        _ => {}
    }
    terminal
        .unwrap_or_else(|| Err(failure("execution ended without a terminal event")))
        .map(PromptResponse::new)
}

pub(super) async fn configure(
    session: Arc<Session>,
    request: SetSessionConfigOptionRequest,
) -> Result<SetSessionConfigOptionResponse, Error> {
    let _operation = session
        .operation
        .try_lock()
        .map_err(|_| invalid("session is busy"))?;
    let mut config = session.config.lock().await;
    let SessionConfigOptionValue::ValueId { value } = request.value else {
        return Err(invalid("configuration requires a selection"));
    };
    match request.config_id.0.as_ref() {
        "mode" => {
            config.mode = match value.0.as_ref() {
                "default" | "build" => Mode::Default,
                "plan" => Mode::Plan,
                "auto" => Mode::Auto,
                "ask" => Mode::Ask,
                _ => return Err(invalid("unknown mode")),
            }
        }
        "model" if config.models.iter().any(|(id, _)| id == value.0.as_ref()) => {
            if session.runtime.snapshot().task_id.is_some() {
                return Err(invalid("session is busy"));
            }
            let choice = (config.factory)(value.0.as_ref());
            let model = choice.model.clone();
            session
                .runtime
                .dispatch(RuntimeCommand::ReplaceConnection {
                    connection: solaris::agent_backend(choice),
                    children: Some({
                        let resolve = config.factory.clone();
                        child_backend(move |model| resolve(model))
                    }),
                })
                .map_err(failure)?
                .accepted()
                .await
                .map_err(failure)?;
            config.model = model;
        }
        _ => return Err(invalid("unknown configuration option or value")),
    }
    Ok(SetSessionConfigOptionResponse::new(config.options()))
}
