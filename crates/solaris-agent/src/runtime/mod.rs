//! Persistent event boundary shared by interactive and headless clients.
pub(crate) mod jobs;

#[cfg(any(test, feature = "test-utils"))]
use crate::SessionRecord;
use crate::{AgentEvent, AgentEventStream, SessionJournal};
use futures::StreamExt;
use std::sync::Arc;

#[cfg(any(test, feature = "test-utils"))]
pub(crate) fn journal_stream(
    stream: AgentEventStream,
    journal: Option<Arc<SessionJournal>>,
) -> AgentEventStream {
    let Some(journal) = journal else {
        return stream;
    };
    Box::pin(futures::stream::unfold(
        (stream, journal, false),
        |(mut stream, journal, failed)| async move {
            if failed {
                return None;
            }
            let event = stream.next().await?;
            match journal.append(&SessionRecord::Event {
                event: event.clone(),
            }) {
                Ok(()) => Some((event, (stream, journal, false))),
                Err(error) => Some((
                    AgentEvent::Error(format!("cannot persist session: {error}")),
                    (stream, journal, true),
                )),
            }
        },
    ))
}

/// The executor owns the session until its task retires.
pub(crate) struct TurnLease {
    active: Arc<std::sync::atomic::AtomicBool>,
}

impl TurnLease {
    pub(crate) fn acquire(active: Arc<std::sync::atomic::AtomicBool>) -> Option<Self> {
        active
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .ok()?;
        Some(Self { active })
    }
}

impl Drop for TurnLease {
    fn drop(&mut self) {
        self.active
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Ordered product event. Sequence numbers belong to the session, not a client.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeEvent {
    pub session_id: String,
    pub sequence: Option<u64>,
    pub task_id: Option<u64>,
    pub event: AgentEvent,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeSnapshot {
    pub session_id: String,
    pub sequence: u64,
    pub task_id: Option<u64>,
    pub model: String,
    pub mode: crate::Mode,
    pub label: String,
    pub provider_id: Option<String>,
    pub capabilities: solaris_types::ModelCapabilities,
    pub contexts: ModeContexts,
    pub active_tasks: Vec<String>,
    pub billing: Option<crate::UsageTotals>,
    pub stop_reason: Option<crate::StopReason>,
}

#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct ModeContexts {
    pub build: crate::ContextUsage,
    pub plan: crate::ContextUsage,
}
impl RuntimeSnapshot {
    pub fn context(&self, mode: crate::Mode) -> crate::ContextUsage {
        match mode {
            crate::Mode::Build => self.contexts.build,
            crate::Mode::Plan => self.contexts.plan,
        }
    }
}

pub type RuntimeEventStream = std::pin::Pin<Box<dyn futures::Stream<Item = RuntimeEvent> + Send>>;

pub struct CommandReceipt {
    ready: tokio::sync::oneshot::Receiver<Result<(), String>>,
    events: RuntimeEventStream,
}
impl CommandReceipt {
    pub async fn accepted(self) -> Result<RuntimeEventStream, String> {
        self.ready
            .await
            .map_err(|_| "session runtime stopped".to_string())??;
        Ok(self.events)
    }
}

struct Request {
    command: RuntimeCommand,
    output: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    accepted: tokio::sync::oneshot::Sender<Result<(), String>>,
}
struct Core {
    auto_wake: bool,
    view: std::sync::RwLock<Arc<crate::Agent>>,
    sender: std::sync::Mutex<Option<tokio::sync::mpsc::UnboundedSender<Request>>>,
    events: tokio::sync::broadcast::Sender<RuntimeEvent>,
    snapshot: std::sync::RwLock<RuntimeSnapshot>,
}
impl Drop for Core {
    fn drop(&mut self) {
        self.view
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .cancel_handle()
            .cancel();
    }
}

/// Application lifetime owner of loaded sessions and their independent jobs.
#[derive(Default)]
pub struct Runtime {
    jobs: crate::Jobs,
    sessions: std::sync::Mutex<std::collections::HashMap<String, SessionRuntime>>,
}
impl Runtime {
    pub fn open(&self, mut agent: crate::Agent) -> Result<SessionRuntime, crate::StoreError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        let id = agent.session_id();
        if let Some(session) = sessions
            .values()
            .find(|session| session.snapshot().session_id == id)
        {
            return Ok(session.clone());
        }
        agent.install_jobs(self.jobs.clone())?;
        let session = SessionRuntime::new(agent);
        sessions.insert(id, session.clone());
        Ok(session)
    }
    pub fn session(&self, id: &str) -> Option<SessionRuntime> {
        self.sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .find(|session| session.snapshot().session_id == id)
            .cloned()
    }
    pub fn jobs(&self) -> crate::Jobs {
        self.jobs.clone()
    }
    pub fn shutdown(&self) {
        self.jobs.shutdown();
        for session in self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            session.cancel_handle().cancel();
        }
    }
}
impl Drop for Runtime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// One command owner for every interactive and headless session.
#[derive(Clone)]
pub struct SessionRuntime {
    core: Arc<Core>,
}
impl SessionRuntime {
    pub fn new(agent: crate::Agent) -> Self {
        Self::from_agent(Arc::new(agent), true)
    }
    pub(crate) fn for_job(agent: Arc<crate::Agent>) -> Self {
        Self::from_agent(agent, false)
    }
    fn from_agent(agent: Arc<crate::Agent>, auto_wake: bool) -> Self {
        let session_id = agent.session_id();
        let sequence = agent
            .journal()
            .map(|journal| journal.event_sequence())
            .transpose()
            .unwrap_or_else(|error| {
                agent.fail(error);
                None
            })
            .unwrap_or(0);
        let snapshot = RuntimeSnapshot {
            session_id,
            sequence,
            task_id: None,
            model: agent.model().into(),
            mode: crate::Mode::Build,
            label: agent.label().into(),
            provider_id: agent.provider_id().map(str::to_owned),
            capabilities: agent.capabilities(),
            contexts: ModeContexts {
                build: agent.context_usage(crate::Mode::Build),
                plan: agent.context_usage(crate::Mode::Plan),
            },
            active_tasks: agent.active_tasks(),
            billing: agent.usage_totals(),
            stop_reason: agent.stop_reason(),
        };
        Self {
            core: Arc::new(Core {
                auto_wake,
                view: std::sync::RwLock::new(agent),
                sender: Default::default(),
                events: tokio::sync::broadcast::channel(1024).0,
                snapshot: std::sync::RwLock::new(snapshot),
            }),
        }
    }
    fn view(&self) -> Arc<crate::Agent> {
        Arc::clone(&self.core.view.read().unwrap_or_else(|p| p.into_inner()))
    }
    pub fn jobs(&self) -> crate::Jobs {
        self.view().jobs()
    }
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.core
            .snapshot
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
    /// Subscribe before submitting commands. Lag requires resynchronizing from snapshot().
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RuntimeEvent> {
        self.core.events.subscribe()
    }
    pub fn activate(&self) -> Result<(), String> {
        let mut sender = self.core.sender.lock().unwrap_or_else(|p| p.into_inner());
        if sender.is_none() {
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| "session commands require an async runtime")?;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *sender = Some(tx);
            runtime.spawn(serve(Arc::downgrade(&self.core), rx));
        }
        Ok(())
    }
    fn send(&self, request: Request) -> Result<(), String> {
        self.activate()?;
        self.core
            .sender
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .as_ref()
            .unwrap()
            .send(request)
            .map_err(|_| "session runtime stopped".into())
    }
    /// Enqueue synchronously so UI actions preserve submission order.
    pub fn submit(&self, command: RuntimeCommand) -> Result<CommandReceipt, String> {
        let (output, rx) = tokio::sync::mpsc::unbounded_channel();
        let (accepted, ready) = tokio::sync::oneshot::channel();
        self.send(Request {
            command,
            output,
            accepted,
        })?;
        Ok(CommandReceipt {
            ready,
            events: Box::pin(futures::stream::unfold(rx, |mut rx| async {
                rx.recv().await.map(|event| (event, rx))
            })),
        })
    }
    pub async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeEventStream, String> {
        self.submit(command)?.accepted().await
    }
    fn idle(
        &self,
        operation: impl FnOnce(&crate::Agent) -> Result<(), String>,
    ) -> Result<(), String> {
        let sender = self.core.sender.lock().unwrap_or_else(|p| p.into_inner());
        if sender.is_some() {
            return Err("use a runtime command after session startup".into());
        }
        operation(&self.view())?;
        refresh(&self.core, None, crate::Mode::Build);
        Ok(())
    }
    /// Initial configuration only. Once started, use AttachJournal.
    pub fn attach_journal(&self, journal: Arc<SessionJournal>) -> Result<(), crate::StoreError> {
        self.idle(|agent| agent.attach_journal(journal).map_err(|e| e.to_string()))
            .map_err(crate::StoreError::Invalid)
    }
    pub fn attach_image(&self, image: solaris_backend::InlineImage) -> Result<(), String> {
        self.idle(|agent| agent.attach_image(image))
    }
    pub fn journal(&self) -> Option<Arc<SessionJournal>> {
        self.view().journal()
    }

    pub fn conversation_len(&self) -> usize {
        self.view().conversation_len()
    }
    pub fn prompt_context(&self) -> crate::PromptContext {
        self.view().prompt_context()
    }
    pub fn expand_input(&self, text: &str) -> Result<String, crate::InputError> {
        self.view().expand_input(text)
    }
    pub fn cancel_handle(&self) -> crate::Cancel {
        self.view().cancel_handle()
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn steering_pending(&self) -> bool {
        self.view().steering_handle().has_pending()
    }
    #[cfg(any(test, feature = "test-utils"))]
    pub fn follow_up_pending(&self) -> bool {
        self.view().follow_up_handle().has_pending()
    }
}
fn receiver(rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> AgentEventStream {
    Box::pin(futures::stream::unfold(rx, |mut rx| async {
        rx.recv().await.map(|event| (event, rx))
    }))
}
fn refresh(core: &Core, task_id: Option<u64>, mode: crate::Mode) {
    let view = core.view.read().unwrap_or_else(|p| p.into_inner());
    // Compute services before taking the UI snapshot lock: Jobs may be waiting
    // for persistence, but drawing must still be able to read the previous view.
    let contexts = ModeContexts {
        build: view.context_usage(crate::Mode::Build),
        plan: view.context_usage(crate::Mode::Plan),
    };
    let tasks = view.active_tasks();
    let billing = view.usage_totals();
    let mut snapshot = core.snapshot.write().unwrap_or_else(|p| p.into_inner());
    if let Some(journal) = view.journal() {
        if snapshot.session_id != journal.id() {
            snapshot.sequence = journal.event_sequence().unwrap_or_else(|error| {
                view.fail(error);
                snapshot.sequence
            });
        }
        snapshot.session_id = journal.id().into();
    }
    snapshot.task_id = task_id;
    snapshot.model = view.model().into();
    if task_id.is_some() {
        snapshot.mode = mode;
    }
    snapshot.label = view.label().into();
    snapshot.provider_id = view.provider_id().map(str::to_owned);
    snapshot.capabilities = view.capabilities();
    snapshot.contexts = contexts;
    snapshot.active_tasks = tasks;
    snapshot.stop_reason = view.stop_reason();
    if let Some(billing) = billing {
        snapshot.billing = Some(billing);
    }
}
fn publish(
    core: &Core,
    task_id: Option<u64>,
    mode: crate::Mode,
    event: AgentEvent,
) -> RuntimeEvent {
    refresh(core, if event.is_terminal() { None } else { task_id }, mode);
    let mut event = event;
    let journal = core
        .view
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .journal();
    let persisted = journal
        .as_ref()
        .map(|journal| journal.publish_event(&event));
    let mut snapshot = core.snapshot.write().unwrap_or_else(|p| p.into_inner());
    let sequence = match persisted {
        Some(Ok(sequence)) => {
            snapshot.sequence = sequence;
            Some(sequence)
        }
        Some(Err(error)) => {
            let agent = core.view.read().unwrap_or_else(|p| p.into_inner());
            agent.fail(&error);
            agent.cancel_handle().cancel();
            event = AgentEvent::Error(format!("cannot persist session event: {error}"));
            None
        }
        None => {
            snapshot.sequence += 1;
            Some(snapshot.sequence)
        }
    };
    let event = RuntimeEvent {
        session_id: snapshot.session_id.clone(),
        sequence,
        task_id,
        event,
    };
    let _ = core.events.send(event.clone());
    event
}
struct Active {
    _output_guard: Option<tokio::sync::mpsc::UnboundedReceiver<RuntimeEvent>>,
    id: u64,
    mode: crate::Mode,
    stream: AgentEventStream,
    output: tokio::sync::mpsc::UnboundedSender<RuntimeEvent>,
    abandoned: bool,
    terminal: Option<AgentEvent>,
}
async fn serve(
    weak: std::sync::Weak<Core>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Request>,
) {
    let Some(core) = weak.upgrade() else { return };
    let jobs = core.view.read().unwrap_or_else(|p| p.into_inner()).jobs();
    let mut job_changes = jobs.subscribe();
    let mut progress = jobs.progress();
    drop(core);
    let mut active: Option<Active> = None;
    let mut next_id = 0;
    loop {
        if active.is_none() {
            let Some(core) = weak.upgrade() else { break };
            let agent = Arc::clone(&core.view.read().unwrap_or_else(|p| p.into_inner()));
            if core.auto_wake && agent.has_notifications() {
                let mode = core.snapshot.read().unwrap_or_else(|p| p.into_inner()).mode;
                let (output, guard) = tokio::sync::mpsc::unbounded_channel();
                next_id += 1;
                active = Some(Active {
                    _output_guard: Some(guard),
                    id: next_id,
                    mode,
                    stream: agent.run_pending(mode),
                    output,
                    abandoned: false,
                    terminal: None,
                });
            }
        }
        let abandoned_output = active
            .as_ref()
            .filter(|run| !run.abandoned)
            .map(|run| run.output.clone());
        tokio::select! {
            biased;
            event = progress.recv() => {
                let Some(core) = weak.upgrade() else { break };
                let state = core.snapshot.read().unwrap_or_else(|p| p.into_inner()).clone();
                match event {
                    Ok((owner, event)) if owner == state.session_id => {
                        let event = publish(&core, active.as_ref().map(|run|run.id), state.mode, event);
                        if let Some(run) = &active { let _ = run.output.send(event); }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        publish(&core, None, state.mode, AgentEvent::Error("background progress subscriber lagged; consult the child session journal".into()));
                    }
                    _ => {}
                }
            }
            _ = job_changes.changed() => {},
            request = commands.recv() => {
                let Some(Request { command, output, accepted }) = request else { break };
                let Some(core) = weak.upgrade() else { break };
                let agent = Arc::clone(&core.view.read().unwrap_or_else(|p| p.into_inner()));
                let busy = active.is_some();
                let mut result = Ok(());
                match command {
                    RuntimeCommand::Prompt { text, mode } if !busy => {
                        next_id += 1;
                        let stream = agent.run_turn(text, mode);
                        active = Some(Active { _output_guard:None, id: next_id, mode, stream, output: output.clone(), abandoned: false, terminal: None });
                        refresh(&core, Some(next_id), mode);
                    }
                    RuntimeCommand::Cancel => agent.cancel_handle().cancel(),
                    RuntimeCommand::Steer { text } => { result = agent.enqueue_input(&text,false); }
                    RuntimeCommand::Queue { text } => { result = agent.enqueue_input(&text,true); }
                    RuntimeCommand::PreparedSteer { text } => result=agent.jobs().enqueue(&agent.session_id(),&text,false,agent.journal()).map_err(|error|error.to_string()),
                    RuntimeCommand::PreparedQueue { text } => result=agent.jobs().enqueue(&agent.session_id(),&text,true,agent.journal()).map_err(|error|error.to_string()),
                    RuntimeCommand::CancelTask { id } => { if !agent.cancel_task(&id) { result = Err("task is not active".into()); } }
                    RuntimeCommand::Clear if !busy => agent.clear(),
                    RuntimeCommand::AttachImage { image } if !busy => result = agent.attach_image(image),
                    RuntimeCommand::AttachJournal { journal } if !busy => result = agent.attach_journal(journal).map_err(|e| e.to_string()),
                    RuntimeCommand::ReplaceResources { resources } if !busy => agent.replace_resources(resources),
                    RuntimeCommand::ReplaceConnection { connection, children } if !busy => {
                        let mut replacement = agent.with_refreshed_backend(connection);
                        if let Some(factory) = children { replacement = replacement.with_child_backend(factory); }
                        *core.view.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(replacement);
                    }
                    RuntimeCommand::Compact if !busy => {
                        let mode = core.snapshot.read().unwrap_or_else(|p| p.into_inner()).mode;
                        next_id += 1;
                        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
                        let (started, ready) = tokio::sync::oneshot::channel();
                        tokio::spawn(async move {
                            let work = agent.compact();
                            tokio::pin!(work);
                            let first = futures::future::poll_fn(|cx| std::task::Poll::Ready(std::future::Future::poll(work.as_mut(), cx))).await;
                            let _ = started.send(());
                            let outcome = match first { std::task::Poll::Ready(value) => value, std::task::Poll::Pending => work.await };
                            match outcome {
                                Some(dropped) => { let _ = tx.send(AgentEvent::Compacted { dropped }); }
                                None => { let _ = tx.send(AgentEvent::Status("nothing compacted".into())); }
                            }
                        });
                        let _ = ready.await;
                        active = Some(Active { _output_guard:None, id: next_id, mode, stream: receiver(rx), output: output.clone(), abandoned: false, terminal: None });
                    }
                    _ => result = Err("session already has an active operation".into()),
                }
                if let Err(error) = &result { let event = publish(&core, None, crate::Mode::Build, AgentEvent::Error(error.clone())); let _ = output.send(event); }
                refresh(&core, active.as_ref().map(|a| a.id), active.as_ref().map_or(crate::Mode::Build, |a| a.mode));
                let _ = accepted.send(result);
            }
            _ = async { match &abandoned_output {
                Some(output) => output.closed().await,
                _ => std::future::pending().await,
            }} => {
                let Some(core) = weak.upgrade() else { break };
                if core.events.receiver_count() == 0 { core.view.read().unwrap_or_else(|p| p.into_inner()).cancel_handle().cancel(); }
                if let Some(run) = active.as_mut() { run.abandoned = true; }
            }
            event = async { match active.as_mut() {
                Some(active) => active.stream.next().await,
                None => std::future::pending().await,
            }} => {
                let Some(core) = weak.upgrade() else { break };
                match event {
                    Some(event) => {
                        let run = active.as_ref().unwrap();
                        if event.is_terminal() {
                            active.as_mut().unwrap().terminal = Some(event);
                            continue;
                        }
                        let event = publish(&core, Some(run.id), run.mode, event);
                        if run.output.send(event).is_err() && core.events.receiver_count() == 0 { core.view.read().unwrap_or_else(|p| p.into_inner()).cancel_handle().cancel(); }
                    }
                    None => {
                        let run = active.take().unwrap();
                        if let Some(terminal) = run.terminal {
                            let event = publish(&core, Some(run.id), run.mode, terminal);
                            let _ = run.output.send(event);
                        }
                        refresh(&core, None, crate::Mode::Build);
                    }
                }
            }
        }
    }
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeCommand {
    Prompt {
        text: String,
        mode: crate::Mode,
    },
    Cancel,
    Steer {
        text: String,
    },
    Queue {
        text: String,
    },
    Clear,
    Compact,
    #[serde(skip)]
    PreparedSteer {
        text: String,
    },
    #[serde(skip)]
    PreparedQueue {
        text: String,
    },
    CancelTask {
        id: String,
    },
    #[serde(skip)]
    AttachImage {
        image: solaris_backend::InlineImage,
    },
    #[serde(skip)]
    AttachJournal {
        journal: Arc<SessionJournal>,
    },
    #[serde(skip)]
    ReplaceResources {
        resources: crate::ResourceSnapshot,
    },
    #[serde(skip)]
    ReplaceConnection {
        connection: crate::ConnectionConfig,
        children: Option<crate::ChildBackendFactory>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Connector;
    #[async_trait::async_trait]
    impl crate::ModelConnector for Connector {
        async fn model_client(self: Arc<Self>) -> Result<crate::ModelClient, crate::BackendError> {
            Ok(crate::testing::client(
                Default::default(),
                |request| async move {
                    if request
                        .history
                        .last()
                        .is_some_and(|message| message.text() == "wait")
                    {
                        Ok(Box::pin(futures::stream::pending()) as AgentEventStream)
                    } else {
                        Ok(Box::pin(futures::stream::iter([
                            AgentEvent::TextDelta("answer".into()),
                            AgentEvent::TurnComplete {
                                usage: crate::Usage::new(10, 1),
                                cost_usd: 0.0,
                            },
                        ])) as AgentEventStream)
                    }
                },
            ))
        }
    }
    fn runtime() -> SessionRuntime {
        SessionRuntime::new(crate::Agent::new(
            crate::ConnectionConfig {
                backend: Arc::new(Connector),
                model: "fixture".into(),
                provider_id: None,
                fallback_context_window: 128_000,
                instructions: None,
            },
            Arc::new(crate::ToolRegistry::with_tools(Vec::new())),
            crate::AgentOptions::default().with_subagents(false),
        ))
    }
    async fn collect(stream: RuntimeEventStream) -> Vec<RuntimeEvent> {
        tokio::time::timeout(std::time::Duration::from_secs(3), stream.collect())
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn cancellation_keeps_controls_responsive_and_cannot_poison_the_next_task() {
        let runtime = runtime();
        let running = runtime
            .execute(RuntimeCommand::Prompt {
                text: "wait".into(),
                mode: crate::Mode::Build,
            })
            .await
            .unwrap();
        assert!(runtime.execute(RuntimeCommand::Clear).await.is_err());
        assert!(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "other".into(),
                    mode: crate::Mode::Build
                })
                .await
                .is_err()
        );
        drop(runtime.execute(RuntimeCommand::Cancel).await.unwrap());
        let cancelled = collect(running).await;
        assert!(
            matches!(&cancelled.last().unwrap().event, AgentEvent::Error(error) if error.contains("cancelled"))
        );
        let first_id = cancelled.last().unwrap().task_id;
        let next = collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "next".into(),
                    mode: crate::Mode::Build,
                })
                .await
                .unwrap(),
        )
        .await;
        assert!(matches!(
            next.last().unwrap().event,
            AgentEvent::TurnComplete { .. }
        ));
        assert_ne!(first_id, next.last().unwrap().task_id);
        assert!(runtime.snapshot().task_id.is_none());
    }
    #[tokio::test]
    async fn all_clients_share_session_identity_sequence_and_billing_snapshots() {
        let runtime = runtime();
        let mut subscriber = runtime.subscribe();
        let first = collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "one".into(),
                    mode: crate::Mode::Build,
                })
                .await
                .unwrap(),
        )
        .await;
        let second = collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "two".into(),
                    mode: crate::Mode::Build,
                })
                .await
                .unwrap(),
        )
        .await;
        let mut previous = 0;
        for expected in first.iter().chain(&second) {
            let observed = subscriber.recv().await.unwrap();
            assert_eq!(observed.session_id, runtime.snapshot().session_id);
            assert_eq!(observed.sequence, expected.sequence);
            assert_eq!(observed.sequence, Some(previous + 1));
            previous = observed.sequence.unwrap();
        }
        assert_eq!(runtime.snapshot().sequence, previous);
        assert_eq!(runtime.snapshot().billing.unwrap().usage.total(), 22);
        collect(runtime.execute(RuntimeCommand::Clear).await.unwrap()).await;
        assert_eq!(runtime.conversation_len(), 0);
        assert_eq!(runtime.snapshot().billing.unwrap().usage.total(), 0);
    }
    #[tokio::test]
    async fn dropping_the_command_stream_cancels_a_stalled_task() {
        let runtime = runtime();
        let stream = runtime
            .execute(RuntimeCommand::Prompt {
                text: "wait".into(),
                mode: crate::Mode::Build,
            })
            .await
            .unwrap();
        drop(stream);
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            while runtime.snapshot().task_id.is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let next = collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "next".into(),
                    mode: crate::Mode::Build,
                })
                .await
                .unwrap(),
        )
        .await;
        assert!(matches!(
            next.last().unwrap().event,
            AgentEvent::TurnComplete { .. }
        ));
    }
    #[tokio::test]
    async fn event_sequence_continues_after_reopening_a_persistent_session() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::SessionStore::open(&directory.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(directory.path(), "fixture", None).unwrap();
        let id = journal.id().to_owned();
        let first = runtime();
        first.attach_journal(journal).unwrap();
        let events = collect(
            first
                .execute(RuntimeCommand::Prompt {
                    text: "first".into(),
                    mode: crate::Mode::Plan,
                })
                .await
                .unwrap(),
        )
        .await;
        let sequence = events.last().unwrap().sequence.unwrap();
        assert_eq!(first.snapshot().mode, crate::Mode::Plan);
        drop(first);
        let second = runtime();
        second.attach_journal(store.resume(&id).unwrap()).unwrap();
        assert_eq!(second.snapshot().sequence, sequence);
        let events = collect(
            second
                .execute(RuntimeCommand::Prompt {
                    text: "second".into(),
                    mode: crate::Mode::Build,
                })
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(events[0].sequence, Some(sequence + 1));
        assert_eq!(events[0].session_id, id);
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn connection_command_updates_the_snapshot_before_acknowledging() {
        let runtime = runtime();
        for index in 0..16 {
            let model = format!("model-{index}");
            let provider = format!("provider-{index}");
            let window = 32_000 + index;
            let _events = runtime
                .execute(RuntimeCommand::ReplaceConnection {
                    connection: crate::ConnectionConfig {
                        backend: Arc::new(Connector),
                        model: model.clone(),
                        provider_id: Some(provider.clone()),
                        fallback_context_window: window,
                        instructions: None,
                    },
                    children: None,
                })
                .await
                .unwrap();
            let snapshot = runtime.snapshot();
            assert_eq!(snapshot.model, model);
            assert_eq!(snapshot.provider_id.as_deref(), Some(provider.as_str()));
            assert_eq!(snapshot.context(crate::Mode::Build).window, window);
            assert_eq!(snapshot.context(crate::Mode::Plan).window, window);
            assert!(snapshot.task_id.is_none());
        }
    }
    #[tokio::test]
    async fn manual_compaction_preserves_the_session_mode_in_the_snapshot() {
        let runtime = runtime();
        collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "question".into(),
                    mode: crate::Mode::Plan,
                })
                .await
                .unwrap(),
        )
        .await;
        collect(runtime.execute(RuntimeCommand::Compact).await.unwrap()).await;
        let snapshot = runtime.snapshot();
        assert_eq!(snapshot.mode, crate::Mode::Plan);
        assert!(snapshot.task_id.is_none());
        assert_eq!(snapshot.context(crate::Mode::Plan).window, 128_000);
    }
}

pub(crate) mod agent;
pub(crate) mod connection;
pub(crate) mod event;
pub(crate) mod input;
pub(crate) mod mode;
pub(crate) mod queue;
pub(crate) mod services;
