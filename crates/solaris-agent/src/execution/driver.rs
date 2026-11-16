//! Full SDK runner integration. The host coordinates tasks, never model/tool rounds.
use super::*;
use rig_agent::agent::{
    CompletionCallAction, CompletionCallEvent, InvalidToolCallAction, ModelTurnAction,
    ModelTurnFinished, ObservationAction, OutcomeAction, OutcomeEvent, ReasoningDelta,
    RequestPatch, TextDelta,
};
use rig_agent::{AgentBuilder, AgentHook, HookContext};
use rig_core::{
    effect::{Answered, EffectKind, FamilyDescriptor, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::{self as native, AssistantContent},
    serve::{Dispatch, Reply, Serve},
};
use std::sync::Mutex;

const TRUNCATED_CALL: &str = "model output was truncated; emit complete tool arguments again";
const STEP_REPORT: &str = "The maximum number of steps for this run has been reached. Do not call tools. Return a concise report of completed work, findings, verification, remaining tasks and how to continue. State that this is a partial result if work remains.";
const PHASE_BOUNDARY: &str = "solaris internal phase boundary";
#[derive(Clone, Copy, PartialEq, Eq)]
enum Boundary {
    Steering,
    Background,
    Compaction,
}

/// The exact model connection and tool/header view fixed for one SDK request.
#[derive(Clone)]
struct RequestSnapshot {
    client: solaris_backend::ModelClient,
    header: crate::context::projection::Header,
}
#[derive(Default)]
struct Phase {
    boundary: Option<Boundary>,
    checkpoint: Option<Vec<native::Message>>,
    reject_truncated: bool,
    totals: crate::UsageTotals,
    request: Option<RequestSnapshot>,
    text: String,
    input_tokens: u32,
    sequential: Option<Arc<tokio::sync::Semaphore>>,
    calls: std::collections::HashMap<rig_core::effect::EffectId, ToolCall>,
}
#[derive(Clone)]
struct Policy {
    runner: Arc<Runner>,
    tx: UnboundedSender<AgentEvent>,
    phase: Arc<Mutex<Phase>>,
}
fn failure(error: impl std::fmt::Display) -> ErrorReport {
    ErrorReport::new(ErrorKind::Provider, error.to_string())
}
impl AgentHook for Policy {
    async fn on_dispatch(
        &self,
        _: &HookContext,
        event: rig_agent::agent::DispatchEvent<'_>,
    ) -> rig_agent::agent::DispatchAction {
        if let (Some(name), Some(id)) = (event.tool_name(), event.call_id) {
            let input = match serde_json::from_str(event.tool_args().unwrap_or("{}")) {
                Ok(input) => input,
                Err(error) => return rig_agent::agent::DispatchAction::deny(failure(error)),
            };
            let call = ToolCall::new(id.wire().into_owned(), name, input);
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            if phase.reject_truncated {
                let _ = self.tx.send(AgentEvent::ToolResult {
                    result: ToolResult::error(&call, TRUNCATED_CALL),
                    duration_ms: 0,
                });
                return rig_agent::agent::DispatchAction::skip(TRUNCATED_CALL);
            }
            phase.calls.insert(event.id, call);
        }
        rig_agent::agent::DispatchAction::proceed()
    }
    async fn on_completion_call(
        &self,
        _: &HookContext,
        event: CompletionCallEvent<'_>,
    ) -> CompletionCallAction {
        if self.runner.cancel.is_cancelled() || self.tx.is_closed() {
            return CompletionCallAction::stop("the turn was cancelled");
        }
        if let Some(error) = self.runner.conversation.error() {
            return CompletionCallAction::stop(error);
        }
        {
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            phase.text.clear();
            phase.request = None;
            phase.reject_truncated = false;
        }
        let final_step = self.runner.steps.is_some_and(|limit| {
            self.runner
                .model_steps
                .load(std::sync::atomic::Ordering::Relaxed)
                + 1
                >= limit.get()
        });
        if self.runner.steps.is_some_and(|limit| {
            self.runner
                .model_steps
                .load(std::sync::atomic::Ordering::Relaxed)
                >= limit.get()
        }) {
            return CompletionCallAction::stop("model step limit reached after final report");
        }
        let report_prompt = format!("{}\n\n{STEP_REPORT}", self.runner.system);
        let header = if final_step {
            let mut header = crate::context::projection::Header::new(
                &self.runner.model_key,
                &report_prompt,
                &[],
            );
            header.base_id.clone_from(&self.runner.base_id);
            header
        } else {
            self.runner.request_header()
        };
        let mut messages = event.history.to_vec();
        messages.push(Arc::new(event.prompt.clone()));
        let mut context = self
            .runner
            .conversation
            .publish_projection(&messages, &header, false);
        context.window = self
            .runner
            .inner
            .capabilities()
            .context_window
            .unwrap_or(self.runner.context_window);
        let boundary = if self.runner.steering.has_pending()
            || !self
                .runner
                .jobs
                .pending_inputs(&self.runner.owner, false)
                .is_empty()
        {
            Some(Boundary::Steering)
        } else if !self.runner.jobs.pending(&self.runner.owner).is_empty() {
            Some(Boundary::Background)
        } else if event.turn > 1 && self.runner.over_budget_tokens(context.tokens.unwrap_or(0)) {
            Some(Boundary::Compaction)
        } else {
            None
        };
        if let Some(boundary) = boundary {
            let _ = self.tx.send(AgentEvent::ContextUsageUpdated { context });
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            phase.boundary = Some(boundary);
            phase.checkpoint = Some(
                messages
                    .iter()
                    .map(|message| message.as_ref().clone())
                    .collect(),
            );
            return CompletionCallAction::stop(PHASE_BOUNDARY);
        }
        // Resolve exactly once, before projecting the request. The same client
        // supplies capabilities, the actual model call and its price snapshot.
        let client = match Arc::clone(&self.runner.inner).model_client().await {
            Ok(client) => client,
            Err(error) => return CompletionCallAction::stop(error.to_string()),
        };
        let notices = self.tx.clone();
        let client = client.with_status_observer(move |text| {
            let _ = notices.send(AgentEvent::Status(text));
        });
        let capabilities = client.capabilities();
        self.phase.lock().unwrap_or_else(|p| p.into_inner()).request = Some(RequestSnapshot {
            client,
            header: header.clone(),
        });
        let context = self
            .runner
            .conversation
            .publish_projection(&messages, &header, true);
        let _ = self.tx.send(AgentEvent::ContextUsageUpdated {
            context: solaris_backend::ContextUsage {
                window: capabilities
                    .context_window
                    .unwrap_or(self.runner.context_window),
                ..context
            },
        });
        let patch = RequestPatch::new().active_tools(
            header
                .tools
                .iter()
                .map(|tool| tool.name.as_str().to_owned()),
        );
        CompletionCallAction::patch(if final_step {
            patch
                .preamble(report_prompt)
                .tool_choice(rig_core::message::ToolChoice::None)
        } else {
            patch
        })
    }

    async fn on_text_delta(&self, _: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        if phase.request.is_none() {
            return ObservationAction::continue_run();
        }
        phase.text.push_str(event.delta);
        drop(phase);
        let _ = self.tx.send(AgentEvent::TextDelta(event.delta.into()));
        ObservationAction::continue_run()
    }
    async fn on_reasoning_delta(
        &self,
        _: &HookContext,
        event: ReasoningDelta<'_>,
    ) -> ObservationAction {
        let _ = self.tx.send(AgentEvent::ThinkingDelta(event.delta.into()));
        ObservationAction::continue_run()
    }
    async fn on_model_turn_finished(
        &self,
        _: &HookContext,
        event: ModelTurnFinished<'_>,
    ) -> ModelTurnAction {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        let text: String = event
            .content
            .iter()
            .filter_map(|part| match part {
                AssistantContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .collect();
        if let Some(suffix) = text.strip_prefix(&phase.text)
            && !suffix.is_empty()
        {
            let _ = self.tx.send(AgentEvent::TextDelta(suffix.into()));
        }
        let mut sequential = false;
        for part in event.content {
            if let AssistantContent::ToolCall(call) = part {
                let _ = self.tx.send(AgentEvent::ToolCall(business_call(call)));
                sequential |= self.runner.tools.iter().any(|tool| {
                    tool.name() == call.function.name.as_str()
                        && tool.execution_mode() == ExecutionMode::Sequential
                });
            }
        }
        phase.sequential = sequential.then(|| Arc::new(tokio::sync::Semaphore::new(1)));
        if matches!(
            event.finish_reason,
            Some(rig_core::completion::FinishReason::Length)
        ) {
            let _ = self.tx.send(AgentEvent::OutputTruncated);
        }
        phase.reject_truncated = matches!(
            event.finish_reason,
            Some(rig_core::completion::FinishReason::Length)
        );
        if !event
            .content
            .iter()
            .any(|part| matches!(part, AssistantContent::ToolCall(_)))
            && !self.runner.cancel.is_cancelled()
            && !self.tx.is_closed()
        {
            let inputs = self.runner.steering.drain();
            if !inputs.is_empty() {
                for text in &inputs {
                    let _ = self.tx.send(AgentEvent::Steered { text: text.clone() });
                }
                return ModelTurnAction::retry_with_feedback(inputs.join("\n\n"));
            }
        }
        ModelTurnAction::continue_run()
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(response) = event.completion() {
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            let Some(request) = &phase.request else {
                return OutcomeAction::replace(Err(failure("model connection was not prepared")));
            };
            let measured = crate::context::estimate::usage(phase.input_tokens, response);
            match request.client.settle(measured, response.usage.cost) {
                Ok(_) => {}
                Err(error) => return OutcomeAction::replace(Err(failure(error))),
            };
            self.runner
                .conversation
                .observe_usage(&request.header, measured);
            phase.totals.add(measured, response.usage.cost);
        }
        OutcomeAction::proceed()
    }
    async fn on_invalid_tool_call(
        &self,
        _: &HookContext,
        event: &rig_agent::run::InvalidToolCallContext,
    ) -> Option<InvalidToolCallAction> {
        let call = ToolCall::new(
            event
                .tool_call_id
                .as_ref()
                .map(|id| id.wire().into_owned())
                .unwrap_or_default(),
            &event.tool_name,
            serde_json::Value::String(event.args.clone().unwrap_or_default()),
        );
        let reason = if self
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .reject_truncated
        {
            TRUNCATED_CALL.to_string()
        } else {
            match event.reason {
                rig_agent::run::policy::InvalidToolCallReason::UnknownTool => format!(
                    "there is no tool called `{}` in this session",
                    event.tool_name
                ),
                _ => format!(
                    "invalid tool call `{}`: {:?}",
                    event.tool_name, event.reason
                ),
            }
        };
        let _ = self.tx.send(AgentEvent::ToolCall(call.clone()));
        let _ = self.tx.send(AgentEvent::ToolResult {
            result: ToolResult::error(&call, &reason),
            duration_ms: 0,
        });
        Some(InvalidToolCallAction::skip(reason))
    }
}
fn business_call(call: &native::ToolCall) -> ToolCall {
    ToolCall::new(
        call.id.wire().into_owned(),
        call.function.name.as_str(),
        call.function.arguments_value(),
    )
}
impl Serve for Policy {
    type Family = family::Completion;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: "model".into(),
            family: FamilyDescriptor::Completion {
                model: "default".into(),
                capabilities: rig_core::completion::ProviderCapabilities {
                    ..Default::default()
                },
            },
            layers: Vec::new(),
        }
    }
    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        let EffectKind::Completion { request, .. } = &kind else {
            return Reply::Outcome(Err(failure("expected model request")));
        };
        if let Some(error) = self.runner.conversation.error() {
            return Reply::Outcome(Err(failure(error)));
        }
        if self.runner.cancel.is_cancelled() {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::Cancelled,
                "the turn was cancelled",
            )));
        }
        self.runner
            .model_steps
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .input_tokens = crate::context::estimate::request_tokens(request);
        let client = self
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .request
            .as_ref()
            .map(|request| request.client.clone());
        let permit = match self.runner.jobs.execution_slot(&self.runner.cancel).await {
            Ok(permit) => permit,
            Err(error) => return Reply::Outcome(Err(failure(error))),
        };
        match client {
            Some(client) => match client.serve(kind, dispatch).await {
                Reply::Stream(stream) => Reply::Stream(Box::pin(futures::stream::unfold(
                    (stream, permit),
                    |(mut stream, permit)| async move {
                        stream.next().await.map(|event| (event, (stream, permit)))
                    },
                ))),
                outcome => outcome,
            },
            None => Reply::Outcome(Err(failure("model connection was not prepared"))),
        }
    }
}
struct ToolBridge {
    spec: ToolSpec,
    policy: Policy,
}
impl Serve for ToolBridge {
    type Family = family::Tool;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            &self.spec.name,
            &self.spec.description,
            self.spec.parameters.clone(),
        )
    }
    async fn serve(&self, _: EffectKind, dispatch: Dispatch) -> Reply {
        if let Some(error) = self.policy.runner.conversation.error() {
            return Reply::Outcome(Err(failure(error)));
        }
        let call = self
            .policy
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .calls
            .remove(&dispatch.id());
        let Some(call) = call else {
            return Reply::Outcome(Err(failure("tool dispatch has no correlated call")));
        };
        let sequential = self
            .policy
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .sequential
            .clone();
        let _permit = match sequential {
            Some(semaphore) => match semaphore.acquire_owned().await {
                Ok(permit) => Some(permit),
                Err(error) => return Reply::Outcome(Err(failure(error))),
            },
            None => None,
        };
        let Some(outcome) = self
            .policy
            .runner
            .run_and_report(&call, &self.policy.tx)
            .await
        else {
            return Reply::Outcome(Err(ErrorReport::new(
                ErrorKind::Cancelled,
                "the turn was cancelled",
            )));
        };
        self.policy
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .totals
            .merge(outcome.totals);
        let content = match super::content::tool_result(&outcome.result) {
            Ok(content) => content,
            Err(error) => return Reply::Outcome(Err(failure(error))),
        };
        let output = match rig_core::tool::ToolOutput::content(content) {
            Ok(output) => output,
            Err(error) => return Reply::Outcome(Err(failure(error))),
        };
        let result = if outcome.result.is_error {
            rig_core::tool::ToolResult::failed(
                rig_core::tool::ToolExecutionError::other(outcome.result.output)
                    .with_model_output(output),
            )
        } else {
            rig_core::tool::ToolResult::success(output)
        };
        Reply::Outcome(Ok(Answered::ToolResult { result }))
    }
}
impl Runner {
    pub(super) async fn drive(
        self: &Arc<Self>,
        mut prompt: String,
        tx: UnboundedSender<AgentEvent>,
    ) {
        let mut total = Usage::default();
        let mut cost_usd = 0.0;
        let mut force_compaction = false;
        loop {
            if self.absorb_inputs(false, &tx) {
                self.model_steps
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            }
            self.absorb_background(&mut total, &mut cost_usd, &tx);
            if self.cancel.is_cancelled() {
                self.give_up("the turn was cancelled".into(), &tx);
                return;
            }
            if let Some(error) = self.conversation.error() {
                let _ = tx.send(AgentEvent::Error(error));
                return;
            }
            self.compact(force_compaction, &tx, &mut total, &mut cost_usd)
                .await;
            let mut base = match self.conversation.native() {
                Ok(base) => base,
                Err(error) => {
                    self.give_up(error.to_string(), &tx);
                    return;
                }
            };
            let phase = Arc::new(Mutex::new(Phase::default()));
            let policy = Policy {
                runner: Arc::clone(self),
                tx: tx.clone(),
                phase: Arc::clone(&phase),
            };
            let handlers = rig_agent::handlers::Handlers::default();
            if let Err(error) = handlers.register("model", policy.clone()) {
                self.give_up(error.to_string(), &tx);
                return;
            }
            let mut builder = AgentBuilder::over_handlers(handlers, "session", "model".into())
                .preamble(&self.system)
                .memory(self.conversation.storage())
                .conversation("conversation")
                .add_hook(policy)
                .record_to(crate::storage::recording::ExecutionRecorder::new(
                    Arc::clone(&self.conversation),
                ));
            let server = rig_agent::tool::server::ToolServer::new().run();
            for tool in &self.tools {
                let bridge = ToolBridge {
                    spec: self
                        .specs
                        .iter()
                        .find(|spec| spec.name == tool.name())
                        .expect("registered tool specification")
                        .clone(),
                    policy: Policy {
                        runner: Arc::clone(self),
                        tx: tx.clone(),
                        phase: Arc::clone(&phase),
                    },
                };
                let registered =
                    match rig_agent::tool::registry::RegisteredTool::from_handler(bridge) {
                        Ok(tool) => tool,
                        Err(error) => {
                            self.give_up(error.to_string(), &tx);
                            return;
                        }
                    };
                server.add_registered_tool(registered);
            }
            builder = builder.tool_server_handle(server);
            let agent = builder.build();
            let projection_guard = self.conversation.projecting();
            let (run, mut events) = agent
                .prompt(&prompt)
                .max_turns(usize::MAX)
                .tool_concurrency(self.max_parallel)
                .run_channel();
            tokio::pin!(run);
            let mut events_open = true;
            let result = loop {
                tokio::select! {
                    biased;
                    _ = wait_for_cancel(&self.cancel) => { self.give_up("the turn was cancelled".into(), &tx); return; },
                    _ = tx.closed() => return,
                    result = &mut run => break result,
                    event = events.next(), if events_open => { events_open = event.is_some(); },
                }
            };
            let boundary = phase.lock().unwrap_or_else(|p| p.into_inner()).boundary;
            let handed_off = match result {
                Ok(_) => false,
                Err(rig_agent::completion::PromptError::Cancelled {
                    chat_history,
                    reason,
                }) if boundary.is_some() && reason == PHASE_BOUNDARY => {
                    if self.cancel.is_cancelled() {
                        self.give_up("the turn was cancelled".into(), &tx);
                        return;
                    }
                    if tx.is_closed() {
                        return;
                    }
                    // Cancellation canonicalizes adjacent user messages. Preserve
                    // the SDK's raw request-boundary view so original message IDs
                    // and the saved prefix remain intact, and verify both views
                    // describe exactly the same canonical history.
                    let checkpoint = phase
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .checkpoint
                        .take();
                    let Some(checkpoint) = checkpoint else {
                        self.give_up("SDK phase boundary has no checkpoint".into(), &tx);
                        return;
                    };
                    if rig_core::conversation::pairing::repair(checkpoint.clone()).messages
                        != chat_history
                    {
                        self.give_up(
                            "SDK phase checkpoint does not match its cancellation history".into(),
                            &tx,
                        );
                        return;
                    }
                    if let Err(error) = self.conversation.commit_phase(&mut base, checkpoint) {
                        self.conversation.failed(&error);
                        self.give_up(error, &tx);
                        return;
                    }
                    true
                }
                Err(error) => {
                    drop(projection_guard);
                    self.conversation.refresh();
                    self.give_up(error.to_string(), &tx);
                    return;
                }
            };
            drop(projection_guard);
            force_compaction = handed_off && boundary == Some(Boundary::Compaction);
            {
                let phase = phase.lock().unwrap_or_else(|p| p.into_inner());
                total = total.merge(phase.totals.usage);
                cost_usd += phase.totals.cost_usd;
            }
            self.conversation.refresh();
            self.emit_context(&tx);
            let notifications = self.absorb_background(&mut total, &mut cost_usd, &tx);
            let received_input = self.absorb_inputs(!handed_off, &tx);
            let background = notifications || received_input;
            let mut inputs = self.steering.drain();
            for text in &inputs {
                let _ = tx.send(AgentEvent::Steered { text: text.clone() });
            }
            if inputs.is_empty() && !background && !handed_off {
                inputs = self.follow_up.drain();
                for prompt in &inputs {
                    let _ = tx.send(AgentEvent::FollowUp {
                        prompt: prompt.clone(),
                    });
                }
            }
            if !inputs.is_empty() {
                prompt = inputs.join("\n\n");
            } else if background {
                prompt = String::new();
            } else if handed_off {
                prompt = "Continue the current task using the available context.".into();
            } else {
                *self.outcome.lock().unwrap_or_else(|p| p.into_inner()) = Some(
                    if self.steps.is_some_and(|steps| {
                        self.model_steps.load(std::sync::atomic::Ordering::Relaxed) >= steps.get()
                    }) {
                        crate::StopReason::StepLimit
                    } else {
                        crate::StopReason::Completed
                    },
                );
                let _ = tx.send(AgentEvent::TurnComplete {
                    usage: total,
                    cost_usd,
                });
                return;
            }
            if received_input || !inputs.is_empty() {
                self.model_steps
                    .store(0, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }
}
