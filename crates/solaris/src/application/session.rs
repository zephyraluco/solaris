use solaris_agent::{Runtime, RuntimeCommand, RuntimeEvent, SessionRuntime};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::sync::mpsc;

/// Backpressure reaches the subscription, which repairs lag from durable events.
pub struct SessionFeed {
    subscription: Option<solaris_agent::RuntimeSubscription>,
    receiver: Option<mpsc::Receiver<Result<RuntimeEvent, String>>>,
    task: Option<tokio::task::AbortHandle>,
    failed: bool,
}
impl SessionFeed {
    pub fn new(session: &SessionRuntime, sequence: u64) -> Self {
        Self {
            subscription: Some(session.subscribe_from(sequence)),
            receiver: None,
            task: None,
            failed: false,
        }
    }
    pub fn is_failed(&self) -> bool {
        self.failed
    }
    pub fn try_recv(&mut self) -> Result<Result<RuntimeEvent, String>, mpsc::error::TryRecvError> {
        if self.receiver.is_none()
            && let Ok(handle) = tokio::runtime::Handle::try_current()
        {
            let mut subscription = self.subscription.take().expect("unstarted feed");
            let (tx, rx) = mpsc::channel(128);
            self.receiver = Some(rx);
            self.task = Some(
                handle
                    .spawn(async move {
                        loop {
                            let next = subscription.next().await;
                            let failed = next.is_err();
                            if tx.send(next).await.is_err() || failed {
                                break;
                            }
                        }
                    })
                    .abort_handle(),
            );
        }
        let result = self
            .receiver
            .as_mut()
            .map_or(Err(mpsc::error::TryRecvError::Empty), |rx| rx.try_recv());
        if matches!(result, Ok(Err(_))) {
            self.failed = true;
        }
        result
    }
    #[cfg(test)]
    pub fn from_receiver(receiver: mpsc::Receiver<Result<RuntimeEvent, String>>) -> Self {
        Self {
            subscription: None,
            receiver: Some(receiver),
            task: None,
            failed: false,
        }
    }
}
impl Drop for SessionFeed {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub enum CommandTarget {
    Prompt(usize),
    Clear,
    Question(String),
}
pub struct CommandOutcome {
    pub session_id: String,
    pub target: CommandTarget,
    pub result: Result<solaris_agent::RuntimeSnapshot, String>,
}
pub struct SessionController {
    pub navigation: super::navigation::Navigation,
    outcomes: mpsc::UnboundedReceiver<CommandOutcome>,
    outcome_tx: mpsc::UnboundedSender<CommandOutcome>,
    pub agent: Arc<SessionRuntime>,
    pub runtime: Arc<Runtime>,
    pub runtime_events: SessionFeed,
    pub runtime_errors: mpsc::UnboundedReceiver<String>,
    pub runtime_error_tx: mpsc::UnboundedSender<String>,
    pub runtime_pending: Arc<AtomicUsize>,
}
impl SessionController {
    pub fn new(agent: Arc<SessionRuntime>, runtime: Arc<Runtime>) -> Self {
        let (runtime_error_tx, runtime_errors) = mpsc::unbounded_channel();
        let (outcome_tx, outcomes) = mpsc::unbounded_channel();
        let sequence = agent.snapshot().sequence;
        Self {
            navigation: Default::default(),
            outcome_tx,
            outcomes,
            runtime_events: SessionFeed::new(&agent, sequence),
            agent,
            runtime,
            runtime_errors,
            runtime_error_tx,
            runtime_pending: Default::default(),
        }
    }
    pub fn bind(&mut self, agent: Arc<SessionRuntime>, sequence: u64) {
        self.runtime_events = SessionFeed::new(&agent, sequence);
        self.agent = agent;
    }
    pub fn poll_outcome(&mut self) -> Option<CommandOutcome> {
        self.outcomes.try_recv().ok()
    }
    pub fn dispatch(&self, command: RuntimeCommand) {
        self.send(command, None);
    }
    pub fn dispatch_prompt(&self, command: RuntimeCommand, turn: usize) {
        self.send(command, Some(CommandTarget::Prompt(turn)));
    }
    pub fn dispatch_clear(&self) {
        self.send(RuntimeCommand::Clear, Some(CommandTarget::Clear));
    }
    pub fn answer_question(
        &self,
        session_id: &str,
        request_id: String,
        response: solaris_agent::QuestionResponse,
    ) {
        if let Some(session) = self.runtime.session(session_id) {
            self.send_to(
                &session,
                RuntimeCommand::AnswerQuestion {
                    request_id: request_id.clone(),
                    response,
                },
                Some(CommandTarget::Question(request_id)),
            );
        }
    }
    fn send(&self, command: RuntimeCommand, target: Option<CommandTarget>) {
        self.send_to(&self.agent, command, target);
    }
    fn send_to(
        &self,
        session: &SessionRuntime,
        command: RuntimeCommand,
        target: Option<CommandTarget>,
    ) {
        let session_id = session.snapshot().session_id;
        let outcomes = self.outcome_tx.clone();
        let receipt = match session.dispatch(command) {
            Ok(receipt) => receipt,
            Err(error) => {
                if let Some(target) = target {
                    let _ = outcomes.send(CommandOutcome {
                        session_id,
                        target,
                        result: Err(error),
                    });
                } else {
                    let _ = self.runtime_error_tx.send(error);
                }
                return;
            }
        };
        let errors = self.runtime_error_tx.clone();
        let pending = self.runtime_pending.clone();
        pending.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            let result = receipt.accepted().await;
            if let Some(target) = target {
                let _ = outcomes.send(CommandOutcome {
                    session_id,
                    target,
                    result,
                });
            } else if let Err(error) = result {
                let _ = errors.send(error);
            }
            pending.fetch_sub(1, Ordering::AcqRel);
        });
    }
    pub fn busy(&self) -> bool {
        self.runtime_pending.load(Ordering::Acquire) > 0 || self.agent.snapshot().task_id.is_some()
    }
}
