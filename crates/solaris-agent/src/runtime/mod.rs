//! Persistent event boundary shared by interactive and headless clients.
pub(crate) mod jobs;
mod subscription;
pub use subscription::RuntimeSubscription;

use crate::{AgentEvent, AgentEventStream, SessionJournal};
use futures::StreamExt;
use std::sync::Arc;

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

/// Summary metadata recorded when a turn finishes.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TurnCompletion {
    /// Working duration of the turn in milliseconds.
    pub duration_ms: u64,
    /// Wall-clock completion timestamp in Unix epoch milliseconds.
    pub completed_at_ms: u64,
    /// Backend or provider display label used for the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Model identifier used for the turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

/// Ordered product event. Sequence numbers belong to the session, not a client.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct RuntimeEvent {
    pub session_id: String,
    pub sequence: Option<u64>,
    pub task_id: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completion: Option<TurnCompletion>,
    pub event: AgentEvent,
}

#[derive(Debug, Clone)]
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

#[derive(Debug, Clone, Copy)]
pub struct ModeContexts {
    pub default: crate::ContextUsage,
    pub plan: crate::ContextUsage,
    pub auto: crate::ContextUsage,
    pub ask: crate::ContextUsage,
}
impl ModeContexts {
    fn from_agent(agent: &crate::Agent) -> Self {
        Self {
            default: agent.context_usage(crate::Mode::Default),
            plan: agent.context_usage(crate::Mode::Plan),
            auto: agent.context_usage(crate::Mode::Auto),
            ask: agent.context_usage(crate::Mode::Ask),
        }
    }
}
impl RuntimeSnapshot {
    pub fn context(&self, mode: crate::Mode) -> crate::ContextUsage {
        match mode {
            crate::Mode::Default => self.contexts.default,
            crate::Mode::Plan => self.contexts.plan,
            crate::Mode::Auto => self.contexts.auto,
            crate::Mode::Ask => self.contexts.ask,
        }
    }
}

pub type RuntimeEventStream = std::pin::Pin<Box<dyn futures::Stream<Item = RuntimeEvent> + Send>>;

pub struct DispatchReceipt {
    ready: tokio::sync::oneshot::Receiver<Result<RuntimeSnapshot, String>>,
}
impl DispatchReceipt {
    pub async fn accepted(self) -> Result<RuntimeSnapshot, String> {
        self.ready
            .await
            .map_err(|_| "session runtime stopped".to_string())?
    }
}

struct Request {
    command: RuntimeCommand,
    output: Option<tokio::sync::mpsc::UnboundedSender<RuntimeEvent>>,
    accepted: tokio::sync::oneshot::Sender<Result<RuntimeSnapshot, String>>,
}
struct Core {
    closed: tokio::sync::watch::Sender<bool>,
    history: std::sync::Mutex<std::collections::VecDeque<RuntimeEvent>>,
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
    stopped: std::sync::atomic::AtomicBool,
    jobs: crate::Jobs,
    sessions: std::sync::Mutex<std::collections::HashMap<String, SessionRuntime>>,
}
impl Runtime {
    pub fn open(&self, mut agent: crate::Agent) -> Result<SessionRuntime, crate::StoreError> {
        let mut sessions = self.sessions.lock().unwrap_or_else(|p| p.into_inner());
        if self.stopped.load(std::sync::atomic::Ordering::Acquire) {
            return Err(crate::StoreError::Invalid(
                "application runtime stopped".into(),
            ));
        }
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
        self.stopped
            .store(true, std::sync::atomic::Ordering::Release);
        for session in self
            .sessions
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
        {
            session.shutdown();
        }
        self.jobs.shutdown();
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
            mode: crate::Mode::Default,
            label: agent.label().into(),
            provider_id: agent.provider_id().map(str::to_owned),
            capabilities: agent.capabilities(),
            contexts: ModeContexts::from_agent(&agent),
            active_tasks: agent.active_tasks(),
            billing: agent.usage_totals(),
            stop_reason: agent.stop_reason(),
        };
        Self {
            core: Arc::new(Core {
                closed: tokio::sync::watch::channel(false).0,
                history: Default::default(),
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
    /// Whether a live request still accepts a reply in this session.
    pub fn is_question_pending(&self, request_id: &str) -> bool {
        self.view()
            .questions()
            .is_some_and(|questions| questions.contains(&self.snapshot().session_id, request_id))
    }
    /// Current live requests; historical events do not recreate pending tools.
    pub fn pending_questions(&self) -> Vec<crate::QuestionRequest> {
        self.view().questions().map_or_else(Vec::new, |questions| {
            questions.pending(&self.snapshot().session_id)
        })
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
    pub fn shutdown(&self) {
        self.core.closed.send_replace(true);
        self.cancel_handle().cancel();
    }
    pub fn activate(&self) -> Result<(), String> {
        if *self.core.closed.borrow() {
            return Err("session runtime stopped".into());
        }
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
    /// Submit an application-owned operation. Lifetime is independent of observers;
    /// use Cancel explicitly. The receipt contains no duplicate event stream.
    pub fn dispatch(&self, command: RuntimeCommand) -> Result<DispatchReceipt, String> {
        let (accepted, ready) = tokio::sync::oneshot::channel();
        self.send(Request {
            command,
            output: None,
            accepted,
        })?;
        Ok(DispatchReceipt { ready })
    }

    pub async fn execute(&self, command: RuntimeCommand) -> Result<RuntimeEventStream, String> {
        let (output, rx) = tokio::sync::mpsc::unbounded_channel();
        let (accepted, ready) = tokio::sync::oneshot::channel();
        self.send(Request {
            command,
            output: Some(output),
            accepted,
        })?;
        ready
            .await
            .map_err(|_| "session runtime stopped".to_string())??;
        Ok(Box::pin(futures::stream::unfold(rx, |mut rx| async {
            rx.recv().await.map(|event| (event, rx))
        })))
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
        refresh(&self.core, None, crate::Mode::Default);
        Ok(())
    }
    /// Attach a journal before the session starts.
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
    let contexts = ModeContexts::from_agent(&view);
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
fn build_completion(core: &Core, started_at: std::time::Instant) -> TurnCompletion {
    let snapshot = core.snapshot.read().unwrap_or_else(|p| p.into_inner());
    let provider = (!snapshot.label.is_empty()).then(|| snapshot.label.clone());
    let model = (!snapshot.model.is_empty()).then(|| snapshot.model.clone());
    let duration_ms = started_at.elapsed().as_millis().min(u64::MAX as u128) as u64;
    let completed_at_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    TurnCompletion {
        duration_ms,
        completed_at_ms,
        provider,
        model,
    }
}
fn publish(
    core: &Core,
    task_id: Option<u64>,
    mode: crate::Mode,
    event: AgentEvent,
) -> RuntimeEvent {
    publish_with_meta(core, task_id, mode, event, None, None)
}
fn publish_with_meta(
    core: &Core,
    task_id: Option<u64>,
    mode: crate::Mode,
    event: AgentEvent,
    prompt: Option<String>,
    completion: Option<TurnCompletion>,
) -> RuntimeEvent {
    refresh(core, if event.is_terminal() { None } else { task_id }, mode);
    let mut event = event;
    let journal = core
        .view
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .journal();
    let persisted = journal.as_ref().map(|journal| {
        journal.publish_runtime_event(task_id, &event, prompt.clone(), completion.clone())
    });
    let mut history = core.history.lock().unwrap_or_else(|p| p.into_inner());
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
        prompt,
        completion,
        event,
    };
    history.push_back(event.clone());
    if history.len() > 1024 {
        history.pop_front();
    }
    drop(history);
    let _ = core.events.send(event.clone());
    event
}
struct Active {
    id: u64,
    mode: crate::Mode,
    stream: AgentEventStream,
    output: Option<tokio::sync::mpsc::UnboundedSender<RuntimeEvent>>,
    terminal: Option<(AgentEvent, Option<TurnCompletion>)>,
    turn_started_at: std::time::Instant,
}
async fn serve(
    weak: std::sync::Weak<Core>,
    mut commands: tokio::sync::mpsc::UnboundedReceiver<Request>,
) {
    let Some(core) = weak.upgrade() else { return };
    let jobs = core.view.read().unwrap_or_else(|p| p.into_inner()).jobs();
    let mut closed = core.closed.subscribe();
    let mut deferred = std::collections::VecDeque::<Request>::new();
    let mut job_changes = jobs.subscribe();
    let mut progress = jobs.progress();
    let mut next_id = core
        .snapshot
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .sequence;
    drop(core);
    let mut active: Option<Active> = None;
    loop {
        if *closed.borrow() {
            break;
        }
        if active.is_none() && deferred.is_empty() {
            let Some(core) = weak.upgrade() else { break };
            let agent = Arc::clone(&core.view.read().unwrap_or_else(|p| p.into_inner()));
            if core.auto_wake && agent.has_notifications() {
                let mode = core.snapshot.read().unwrap_or_else(|p| p.into_inner()).mode;
                next_id += 1;
                active = Some(Active {
                    id: next_id,
                    mode,
                    stream: agent.run_pending(mode),
                    output: None,
                    terminal: None,
                    turn_started_at: std::time::Instant::now(),
                });
            }
        }
        let abandoned_output = active.as_ref().and_then(|run| run.output.clone());
        let idle = active.is_none();
        tokio::select! {
            biased;
            _ = closed.changed()=>break,
            request = async {if idle && !deferred.is_empty(){deferred.pop_front()}else{commands.recv().await}} => {
                let Some(Request { command, output, accepted }) = request else { break };
                let Some(core) = weak.upgrade() else { break };
                let agent = Arc::clone(&core.view.read().unwrap_or_else(|p| p.into_inner()));
                let busy = active.is_some();
                if busy && matches!(&command,RuntimeCommand::ReplaceResources {..} | RuntimeCommand::ReplaceConnection {..}) {
                    deferred.push_back(Request {command,output,accepted});continue;
                }
                let mut result = Ok(());
                let command = match command {
                    RuntimeCommand::PromptWithImages { text, mode, images } if !busy => {
                        let attached = agent.expand_input(&text).map_err(|error| error.to_string())
                            .and_then(|_| agent.attach_images(images));
                        if let Err(error) = attached {
                            let _ = accepted.send(Err(error));
                            continue;
                        }
                        RuntimeCommand::Prompt { text, mode }
                    }
                    command => command,
                };
                match command {
                    RuntimeCommand::Prompt { text, mode } if !busy => {
                        next_id += 1;
                        let display=text.clone();
                        let stream = agent.run_turn(text, mode);
                        active = Some(Active {
                            id: next_id,
                            mode,
                            stream,
                            output: output.clone(),
                            terminal: None,
                            turn_started_at: std::time::Instant::now(),
                        });
                        refresh(&core, Some(next_id), mode);
                        let event=publish_with_meta(&core,Some(next_id),mode,AgentEvent::Status("working".into()),Some(display),None);
                        if let Some(output)=&output {let _=output.send(event);}
                    }
                    RuntimeCommand::Cancel => agent.cancel_handle().cancel(),
                    RuntimeCommand::PreparedSteer { text } => result=agent.jobs().enqueue(&agent.session_id(),&text,false,agent.journal()).map_err(|error|error.to_string()),
                    RuntimeCommand::PreparedQueue { text } => result=agent.jobs().enqueue(&agent.session_id(),&text,true,agent.journal()).map_err(|error|error.to_string()),
                    RuntimeCommand::CancelTask { id } => { if !agent.cancel_task(&id) { result = Err("task is not active".into()); } }
                    RuntimeCommand::AnswerQuestion { request_id, response } => {
                        result = agent.questions().ok_or_else(|| "interactive questions are unavailable".to_string()).and_then(|questions| questions.reply(&agent.session_id(), &request_id, response));
                    }
                    RuntimeCommand::Clear if !busy => result=agent.clear_checked(),
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
                        active = Some(Active {
                            id: next_id,
                            mode,
                            stream: receiver(rx),
                            output: output.clone(),
                            terminal: None,
                            turn_started_at: std::time::Instant::now(),
                        });
                    }
                    _ => result = Err("session already has an active operation".into()),
                }
                refresh(&core, active.as_ref().map(|a| a.id), active.as_ref().map_or(crate::Mode::Default, |a| a.mode));
                let _ = accepted.send(result.map(|()|core.snapshot.read().unwrap_or_else(|p|p.into_inner()).clone()));
            }
            event = progress.recv() => {
                let Some(core) = weak.upgrade() else { break };
                let state = core.snapshot.read().unwrap_or_else(|p| p.into_inner()).clone();
                match event {
                    Ok((owner, event)) if owner == state.session_id => {
                        let event = publish(&core, active.as_ref().map(|run|run.id), state.mode, event);
                        if let Some(run) = &active && let Some(output) = &run.output { let _ = output.send(event); }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        publish(&core, None, state.mode, AgentEvent::Error("background progress subscriber lagged; consult the child session journal".into()));
                    }
                    _ => {}
                }
            }
            _ = job_changes.changed() => {},
            _ = async { match &abandoned_output {
                Some(output) => output.closed().await,
                _ => std::future::pending().await,
            }} => {
                let Some(core) = weak.upgrade() else { break };
                core.view.read().unwrap_or_else(|p| p.into_inner()).cancel_handle().cancel();
                if let Some(run) = active.as_mut() { run.output = None; }
            }
            event = async { match active.as_mut() {
                Some(active) => active.stream.next().await,
                None => std::future::pending().await,
            }} => {
                let Some(core) = weak.upgrade() else { break };
                match event {
                    Some(event) => {
                        let run = active.as_mut().unwrap();
                        if event.is_terminal() {
                            let completion = matches!(event, AgentEvent::TurnComplete { .. })
                                .then(|| build_completion(&core, run.turn_started_at));
                            run.terminal = Some((event, completion));
                            continue;
                        }
                        let completion = if matches!(event, AgentEvent::FollowUp { .. }) {
                            let meta = build_completion(&core, run.turn_started_at);
                            run.turn_started_at = std::time::Instant::now();
                            Some(meta)
                        } else {
                            None
                        };
                        let event = publish_with_meta(&core, Some(run.id), run.mode, event, None, completion);
                        if let Some(output) = &run.output && output.send(event).is_err() { core.view.read().unwrap_or_else(|p| p.into_inner()).cancel_handle().cancel(); }
                    }
                    None => {
                        let run = active.take().unwrap();
                        if let Some((terminal, completion)) = run.terminal {
                            let event = publish_with_meta(&core, Some(run.id), run.mode, terminal, None, completion);
                            if let Some(output) = &run.output { let _ = output.send(event); }
                        }
                        refresh(&core, None, crate::Mode::Default);
                    }
                }
            }
        }
    }
    let running = active.is_some();
    drop(active);
    if let Some(core) = weak.upgrade() {
        refresh(&core, None, crate::Mode::Default);
        if running && *closed.borrow() {
            core.snapshot
                .write()
                .unwrap_or_else(|p| p.into_inner())
                .stop_reason = Some(crate::StopReason::Shutdown);
        }
    }
}

#[derive(Clone)]
pub enum RuntimeCommand {
    AnswerQuestion {
        request_id: String,
        response: crate::QuestionResponse,
    },
    Prompt {
        text: String,
        mode: crate::Mode,
    },
    PromptWithImages {
        text: String,
        mode: crate::Mode,
        images: Vec<solaris_backend::InlineImage>,
    },
    Cancel,
    Clear,
    Compact,
    PreparedSteer {
        text: String,
    },
    PreparedQueue {
        text: String,
    },
    CancelTask {
        id: String,
    },
    ReplaceResources {
        resources: crate::ResourceSnapshot,
    },
    ReplaceConnection {
        connection: crate::ConnectionConfig,
        children: Option<crate::ChildBackendFactory>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::messages::MessageFixture;
    struct Connector;
    #[async_trait::async_trait]
    impl crate::ModelConnector for Connector {
        async fn model_client(self: Arc<Self>) -> Result<crate::ModelClient, crate::BackendError> {
            Ok(crate::test_support::client(
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
    async fn stopping_a_session_wakes_subscribers_and_refuses_new_commands() {
        let session = runtime();
        let mut events = session.subscribe_from(0);
        session.shutdown();
        assert!(
            session
                .dispatch(RuntimeCommand::Prompt {
                    text: "no".into(),
                    mode: crate::Mode::Default
                })
                .is_err()
        );
        assert!(events.next().await.unwrap_err().contains("stopped"));
    }
    #[tokio::test]
    async fn resource_replacement_waits_for_the_execution_boundary_before_acknowledging() {
        let session = runtime();
        session
            .dispatch(RuntimeCommand::Prompt {
                text: "wait".into(),
                mode: crate::Mode::Default,
            })
            .unwrap()
            .accepted()
            .await
            .unwrap();
        let receipt = session
            .dispatch(RuntimeCommand::ReplaceResources {
                resources: crate::ResourceSnapshot::default(),
            })
            .unwrap();
        let accepted = receipt.accepted();
        tokio::pin!(accepted);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut accepted)
                .await
                .is_err()
        );
        session
            .dispatch(RuntimeCommand::Cancel)
            .unwrap()
            .accepted()
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), accepted)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn detached_dispatch_outlives_subscribers_and_cancels_only_explicitly() {
        let runtime = runtime();
        let observer = runtime.subscribe();
        let accepted = runtime
            .dispatch(RuntimeCommand::Prompt {
                text: "wait".into(),
                mode: crate::Mode::Default,
            })
            .unwrap()
            .accepted()
            .await
            .unwrap();
        assert!(accepted.task_id.is_some());
        drop(observer);
        for _ in 0..10 {
            tokio::task::yield_now().await;
        }
        assert!(runtime.snapshot().task_id.is_some());
        let mut observer = runtime.subscribe_from(runtime.snapshot().sequence);
        runtime
            .dispatch(RuntimeCommand::Cancel)
            .unwrap()
            .accepted()
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                if matches!(observer.next().await.unwrap().event, AgentEvent::Error(_)) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        assert!(runtime.snapshot().task_id.is_none());
    }

    #[tokio::test]
    async fn lagging_durable_subscription_replays_pages_without_gaps_or_duplicates() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "fixture", None).unwrap();
        let runtime = runtime();
        runtime.attach_journal(journal.clone()).unwrap();
        let mut observer = runtime.subscribe_from(0);
        for index in 0..1500 {
            publish(
                &runtime.core,
                Some(8),
                crate::Mode::Default,
                AgentEvent::TextDelta(index.to_string()),
            );
        }
        for sequence in 1..=1500 {
            let event = tokio::time::timeout(std::time::Duration::from_secs(3), observer.next())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.sequence, Some(sequence));
            assert_eq!(event.task_id, Some(8));
        }
        let (records, sequence) = journal.history().unwrap();
        assert_eq!(sequence, 1500);
        assert_eq!(
            records
                .iter()
                .filter(|record| matches!(record, crate::SessionRecord::RuntimeEvent { .. }))
                .count(),
            1500
        );
    }

    #[tokio::test]
    async fn expired_ephemeral_history_requires_explicit_resynchronization() {
        let runtime = runtime();
        for _ in 0..1100 {
            publish(
                &runtime.core,
                None,
                crate::Mode::Default,
                AgentEvent::Status("status".into()),
            );
        }
        assert!(
            runtime
                .replay_after(0)
                .await
                .unwrap_err()
                .contains("reload")
        );
    }

    #[tokio::test]
    async fn forked_event_history_preserves_waterline_and_rebinds_session_identity() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::SessionStore::open(&dir.path().join("sessions.sqlite")).unwrap();
        let journal = store.create(dir.path(), "fixture", None).unwrap();
        let source = runtime();
        source.attach_journal(journal.clone()).unwrap();
        publish(
            &source.core,
            Some(1),
            crate::Mode::Default,
            AgentEvent::TextDelta("saved".into()),
        );
        let fork = journal.fork(dir.path(), "fixture").unwrap();
        assert_eq!(fork.history().unwrap().1, 1);
        let branch = runtime();
        branch.attach_journal(fork.clone()).unwrap();
        let events = collect(
            branch
                .execute(RuntimeCommand::Prompt {
                    text: "next".into(),
                    mode: crate::Mode::Default,
                })
                .await
                .unwrap(),
        )
        .await;
        assert!(events.iter().all(|event| event.sequence.unwrap() > 1));
        assert!(
            events
                .iter()
                .filter_map(|event| event.task_id)
                .all(|id| id > 1)
        );
        assert!(
            fork.events_after(0)
                .unwrap()
                .iter()
                .all(|event| event.session_id == fork.id())
        );
    }

    #[tokio::test]
    async fn cancellation_keeps_controls_responsive_and_cannot_poison_the_next_task() {
        let runtime = runtime();
        let running = runtime
            .execute(RuntimeCommand::Prompt {
                text: "wait".into(),
                mode: crate::Mode::Default,
            })
            .await
            .unwrap();
        assert!(runtime.execute(RuntimeCommand::Clear).await.is_err());
        assert!(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "other".into(),
                    mode: crate::Mode::Default
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
                    mode: crate::Mode::Default,
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
                    mode: crate::Mode::Default,
                })
                .await
                .unwrap(),
        )
        .await;
        let second = collect(
            runtime
                .execute(RuntimeCommand::Prompt {
                    text: "two".into(),
                    mode: crate::Mode::Default,
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
                mode: crate::Mode::Default,
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
                    mode: crate::Mode::Default,
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
                    mode: crate::Mode::Default,
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
            assert_eq!(snapshot.context(crate::Mode::Default).window, window);
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
