//! Persistent event boundary shared by interactive and headless clients.

use crate::{AgentEvent, AgentEventStream, SessionJournal, SessionRecord};
use futures::StreamExt;
use std::sync::Arc;

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

/// A single active turn owns the session until its stream is dropped.
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
    pub sequence: u64,
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
    pub context: crate::ContextUsage,
    pub billing: Option<crate::UsageTotals>,
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

/// One command owner for every interactive and headless session.
#[derive(Clone)]
pub struct SessionRuntime {
    core: Arc<Core>,
}
impl SessionRuntime {
    pub fn new(agent: crate::Agent) -> Self {
        let session_id = agent
            .journal()
            .map_or_else(crate::identity::new_id, |j| j.id().into());
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
            context: agent.context_usage(crate::Mode::Build),
            billing: agent.usage_totals(),
        };
        Self {
            core: Arc::new(Core {
                view: std::sync::RwLock::new(Arc::new(agent)),
                sender: Default::default(),
                events: tokio::sync::broadcast::channel(1024).0,
                snapshot: std::sync::RwLock::new(snapshot),
            }),
        }
    }
    fn view(&self) -> Arc<crate::Agent> {
        Arc::clone(&self.core.view.read().unwrap_or_else(|p| p.into_inner()))
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
    fn send(&self, request: Request) -> Result<(), String> {
        let mut sender = self.core.sender.lock().unwrap_or_else(|p| p.into_inner());
        if sender.is_none() {
            let runtime = tokio::runtime::Handle::try_current()
                .map_err(|_| "session commands require an async runtime")?;
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            *sender = Some(tx);
            runtime.spawn(serve(Arc::downgrade(&self.core), rx));
        }
        sender
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
    /// Configure before the first execution. Running sessions use execute().
    pub fn configure(&self, command: RuntimeCommand) -> Result<(), String> {
        self.idle(|agent| match command {
            RuntimeCommand::Clear => {
                agent.clear();
                Ok(())
            }
            RuntimeCommand::ReplaceResources { resources } => {
                agent.replace_resources(resources);
                Ok(())
            }
            RuntimeCommand::ReplaceConnection {
                connection,
                children,
            } => {
                let mut replacement = agent.with_refreshed_backend(connection);
                if let Some(factory) = children {
                    replacement = replacement.with_child_backend(factory);
                }
                *self.core.view.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(replacement);
                Ok(())
            }
            _ => Err("command requires execute()".into()),
        })
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
    pub fn label(&self) -> String {
        self.view().label().into()
    }
    pub fn provider_id(&self) -> Option<String> {
        self.view().provider_id().map(str::to_owned)
    }
    pub fn capabilities(&self) -> solaris_types::ModelCapabilities {
        self.view().capabilities()
    }
    pub fn context_usage(&self, mode: crate::Mode) -> crate::ContextUsage {
        self.view().context_usage(mode)
    }
    pub fn usage_totals(&self) -> Option<crate::UsageTotals> {
        self.view().usage_totals()
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
    pub fn active_tasks(&self) -> Vec<String> {
        self.view().active_tasks()
    }
    pub fn steering_pending(&self) -> bool {
        self.view().steering_handle().has_pending()
    }
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
    snapshot.context = view.context_usage(snapshot.mode);
    snapshot.billing = view.usage_totals();
}
fn publish(
    core: &Core,
    task_id: Option<u64>,
    mode: crate::Mode,
    event: AgentEvent,
) -> RuntimeEvent {
    refresh(core, if event.is_terminal() { None } else { task_id }, mode);
    let mut event = event;
    let sequence = core
        .view
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .journal()
        .map(|journal| journal.next_sequence());
    let mut snapshot = core.snapshot.write().unwrap_or_else(|p| p.into_inner());
    snapshot.sequence = match sequence {
        Some(Ok(sequence)) => sequence,
        Some(Err(error)) => {
            let agent = core.view.read().unwrap_or_else(|p| p.into_inner());
            agent.fail(&error);
            agent.cancel_handle().cancel();
            event = AgentEvent::Error(format!("cannot persist event sequence: {error}"));
            snapshot.sequence + 1
        }
        None => snapshot.sequence + 1,
    };
    let event = RuntimeEvent {
        session_id: snapshot.session_id.clone(),
        sequence: snapshot.sequence,
        task_id,
        event,
    };
    let _ = core.events.send(event.clone());
    event
}
struct Active {
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
    let mut active: Option<Active> = None;
    let mut next_id = 0;
    loop {
        let abandoned_output = active
            .as_ref()
            .filter(|run| !run.abandoned)
            .map(|run| run.output.clone());
        tokio::select! {
            biased;
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
                        active = Some(Active { id: next_id, mode, stream, output: output.clone(), abandoned: false, terminal: None });
                        refresh(&core, Some(next_id), mode);
                    }
                    RuntimeCommand::Cancel => agent.cancel_handle().cancel(),
                    RuntimeCommand::Steer { text } => { result = agent.steering_handle().try_push(text).map(|_| ()).map_err(|e| e.to_string()); }
                    RuntimeCommand::Queue { text } => { result = agent.follow_up_handle().try_push(text).map(|_| ()).map_err(|e| e.to_string()); }
                    RuntimeCommand::PreparedSteer { text } => agent.steering_handle().push_prepared(text),
                    RuntimeCommand::PreparedQueue { text } => agent.follow_up_handle().push_prepared(text),
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
                        active = Some(Active { id: next_id, mode: crate::Mode::Build, stream: receiver(rx), output: output.clone(), abandoned: false, terminal: None });
                    }
                    _ => result = Err("session already has an active operation".into()),
                }
                let _ = accepted.send(result.clone());
                if let Err(error) = result { let event = publish(&core, None, crate::Mode::Build, AgentEvent::Error(error)); let _ = output.send(event); }
                refresh(&core, active.as_ref().map(|a| a.id), active.as_ref().map_or(crate::Mode::Build, |a| a.mode));
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
            assert_eq!(observed.sequence, previous + 1);
            previous = observed.sequence;
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
        let sequence = events.last().unwrap().sequence;
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
        assert_eq!(events[0].sequence, sequence + 1);
        assert_eq!(events[0].session_id, id);
    }
}

pub(crate) mod agent;
pub(crate) mod connection;
pub(crate) mod event;
pub(crate) mod input;
pub(crate) mod mode;
pub(crate) mod queue;
pub(crate) mod services;
