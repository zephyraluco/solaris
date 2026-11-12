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

#[derive(Default)]
struct Phase {
    totals: crate::UsageTotals,
    client: Option<solaris_backend::ModelClient>,
    synthetic: Option<(Usage, f64)>,
    text: String,
    thinking: String,
    input_characters: usize,
    sequential: Option<Arc<tokio::sync::Semaphore>>,
    measurements: Vec<(Message, solaris_backend::MessageContext)>,
    last_usage: Option<Usage>,
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
fn usage(value: rig_core::completion::Usage) -> Usage {
    let read = value.cached_input_tokens.unwrap_or(0);
    let write = value.cache_creation_input_tokens.unwrap_or(0);
    let bounded = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
    Usage::new(
        bounded(
            value
                .input_tokens
                .unwrap_or(0)
                .saturating_sub(read.saturating_add(write)),
        ),
        bounded(value.output_tokens.unwrap_or(0)),
    )
    .with_cache(bounded(read), bounded(write))
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
            self.phase
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .calls
                .insert(event.id, ToolCall::new(id.wire().into_owned(), name, input));
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
            phase.thinking.clear();
            phase.client = None;
            phase.synthetic = None;
        }
        let header = self.runner.request_header();
        let mut messages: Vec<_> = event
            .history
            .iter()
            .map(|message| Message::from(message.as_ref()))
            .collect();
        messages.push(Message::from(event.prompt));
        if let Some((measured, context)) = self
            .phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .measurements
            .last()
            && let Some(message) = messages.iter_mut().rev().find(|message| {
                message.role == measured.role && message.content == measured.content
            })
        {
            message.context = Some(context.clone());
        }
        let mut context = self
            .runner
            .conversation
            .publish_projection(&messages, &header);
        context.window = self
            .runner
            .inner
            .capabilities()
            .context_window
            .unwrap_or(self.runner.context_window);
        let _ = self.tx.send(AgentEvent::ContextUsageUpdated { context });
        CompletionCallAction::patch(
            RequestPatch::new().active_tools(
                self.runner
                    .request_tools()
                    .into_iter()
                    .map(|tool| tool.name),
            ),
        )
    }
    async fn on_text_delta(&self, _: &HookContext, event: TextDelta<'_>) -> ObservationAction {
        let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
        if phase.client.is_none() {
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
        self.phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .thinking
            .push_str(event.delta);
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
        if let Some(usage) = phase.last_usage
            && !usage.estimated
            && !usage.is_empty()
        {
            let mut message = Message::from(&native::Message::Assistant(
                native::AssistantMessage::new(event.content.clone()),
            ));
            self.runner.request_header().annotate(&mut message, usage);
            if let Some(context) = message.context.take() {
                phase.measurements.push((message, context));
            }
        }

        if matches!(
            event.finish_reason,
            Some(rig_core::completion::FinishReason::Length)
        ) {
            let _ = self.tx.send(AgentEvent::OutputTruncated);
        }
        if matches!(
            event.finish_reason,
            Some(rig_core::completion::FinishReason::Length)
        ) && event
            .content
            .iter()
            .any(|part| matches!(part, AssistantContent::ToolCall(_)))
        {
            return ModelTurnAction::stop(
                "model output limit reached; incomplete tool calls were not executed",
            );
        }
        ModelTurnAction::continue_run()
    }
    async fn on_outcome(&self, _: &HookContext, event: OutcomeEvent<'_>) -> OutcomeAction {
        if let Some(response) = event.completion() {
            let mut phase = self.phase.lock().unwrap_or_else(|p| p.into_inner());
            let (measured, cost) = if let Some(client) = &phase.client {
                let measured = if response.usage.input_tokens.is_some()
                    || response.usage.output_tokens.is_some()
                {
                    usage(response.usage)
                } else {
                    let output = response
                        .choice
                        .iter()
                        .filter_map(|part| match part {
                            AssistantContent::Text(text) => Some(text.text.chars().count()),
                            _ => None,
                        })
                        .sum();
                    Usage::estimate(phase.input_characters, output)
                };
                let cost = client.cost(measured);
                if let Err(error) = self.runner.inner.observe_usage(measured, cost) {
                    return OutcomeAction::replace(Err(failure(error)));
                }
                (measured, cost)
            } else {
                phase
                    .synthetic
                    .take()
                    .unwrap_or_else(|| (usage(response.usage), 0.0))
            };
            phase.last_usage = Some(measured);
            phase.totals.add(measured, cost);
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
        let reason = match event.reason {
            rig_agent::run::policy::InvalidToolCallReason::UnknownTool => format!(
                "there is no tool called `{}` in this session",
                event.tool_name
            ),
            _ => format!(
                "invalid tool call `{}`: {:?}",
                event.tool_name, event.reason
            ),
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
        let caps = self.runner.inner.capabilities();
        HandlerDescriptor {
            key: "model".into(),
            family: FamilyDescriptor::Completion {
                model: "default".into(),
                capabilities: rig_core::completion::ProviderCapabilities {
                    ..Default::default()
                },
            },
            layers: {
                let _ = caps;
                Vec::new()
            },
        }
    }
    async fn serve(&self, kind: EffectKind, dispatch: Dispatch) -> Reply {
        let EffectKind::Completion { request, .. } = &kind else {
            return Reply::Outcome(Err(failure("expected model request")));
        };
        self.phase
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .input_characters = request
            .chat_history
            .iter()
            .map(|message| Message::from(message.as_ref()).characters())
            .sum();
        match self.runner.inner.model_client().await {
            Ok(Some(client)) => {
                self.phase.lock().unwrap_or_else(|p| p.into_inner()).client = Some(client.clone());
                client.serve(kind, dispatch).await
            }
            Ok(None) => self.synthetic(request).await,
            Err(error) => Reply::Outcome(Err(failure(error))),
        }
    }
}
impl Policy {
    // Compatibility for injected business-event backends (tests and application
    // extensions). Production providers always take the native connection path.
    async fn synthetic(&self, request: &rig_core::completion::CompletionRequest) -> Reply {
        let history = request
            .chat_history
            .iter()
            .map(|message| Message::from(message.as_ref()))
            .collect();
        let tools = request
            .tools
            .iter()
            .map(|tool| {
                ToolSpec::new(
                    tool.name.as_str(),
                    &tool.description,
                    tool.parameters.clone(),
                )
            })
            .collect();
        let result = self
            .runner
            .inner
            .run_turn(TurnRequest {
                history,
                prompt: String::new(),
                tools,
            })
            .await;
        let mut stream = match result {
            Ok(stream) => stream,
            Err(error) => return Reply::Outcome(Err(failure(error))),
        };
        let mut content = Vec::new();
        let mut text = String::new();
        let mut truncated = false;
        while let Some(event) = stream.next().await {
            match event {
                AgentEvent::TextDelta(delta) => {
                    text.push_str(&delta);
                    self.phase
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .text
                        .push_str(&delta);
                    let _ = self.tx.send(AgentEvent::TextDelta(delta));
                }
                AgentEvent::ToolCall(call) => {
                    let name = match native::ToolName::new(call.name) {
                        Ok(name) => name,
                        Err(error) => return Reply::Outcome(Err(failure(error))),
                    };
                    content.push(AssistantContent::tool_call(call.id, name, call.input));
                }
                AgentEvent::OutputTruncated => {
                    truncated = true;
                }
                AgentEvent::Error(error) => return Reply::Outcome(Err(failure(error))),
                AgentEvent::TurnComplete {
                    usage: measured,
                    cost_usd,
                } => {
                    if !text.is_empty() {
                        content.insert(0, AssistantContent::text(text));
                    }
                    self.phase
                        .lock()
                        .unwrap_or_else(|p| p.into_inner())
                        .synthetic = Some((measured, cost_usd));
                    let mut native_usage = rig_core::completion::Usage::new();
                    native_usage.input_tokens = Some(
                        u64::from(measured.input_tokens)
                            + u64::from(measured.cache_read_tokens)
                            + u64::from(measured.cache_write_tokens),
                    );
                    native_usage.output_tokens = Some(u64::from(measured.output_tokens));
                    native_usage.cached_input_tokens = Some(u64::from(measured.cache_read_tokens));
                    native_usage.cache_creation_input_tokens =
                        Some(u64::from(measured.cache_write_tokens));
                    let response = rig_core::completion::CompletionResponse::new(
                        content,
                        native_usage,
                        native::Origin::new("synthetic", "synthetic", "synthetic"),
                        serde_json::Value::Null,
                    )
                    .with_finish_reason(if truncated {
                        rig_core::completion::FinishReason::Length
                    } else {
                        rig_core::completion::FinishReason::Stop
                    });
                    return Reply::Outcome(Ok(Answered::Completion(Arc::new(response))));
                }
                other => {
                    let _ = self.tx.send(other);
                }
            }
        }
        Reply::Outcome(Err(failure(
            "the model's response ended without completing",
        )))
    }
}
struct ToolBridge {
    tool: Arc<dyn Tool>,
    policy: Policy,
}
impl Serve for ToolBridge {
    type Family = family::Tool;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor::tool(
            self.tool.name(),
            self.tool.description(),
            self.tool.parameters(),
        )
    }
    async fn serve(&self, _: EffectKind, dispatch: Dispatch) -> Reply {
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
            .add(outcome.usage, outcome.cost_usd);
        let message = match native::Message::try_from(&Message::tool_result(outcome.result.clone()))
        {
            Ok(message) => message,
            Err(error) => return Reply::Outcome(Err(failure(error))),
        };
        let native::Message::User { content } = message else {
            return Reply::Outcome(Err(failure("invalid tool result")));
        };
        let content = content
            .into_iter()
            .find_map(|part| match part {
                native::UserContent::ToolResult(result) => Some(result.content),
                _ => None,
            })
            .unwrap_or_default();
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
        loop {
            if self.cancel.is_cancelled() {
                self.give_up("the turn was cancelled".into(), &tx);
                return;
            }
            if let Some(error) = self.conversation.error() {
                let _ = tx.send(AgentEvent::Error(error));
                return;
            }
            self.compact(false, &tx, &mut total, &mut cost_usd).await;
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
                .add_hook(policy);
            let server = rig_agent::tool::server::ToolServer::new().run();
            for tool in &self.tools {
                let bridge = ToolBridge {
                    tool: Arc::clone(tool),
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
            let mut stream = agent
                .prompt(&prompt)
                .max_turns(usize::MAX)
                .tool_concurrency(self.max_parallel)
                .stream();
            let mut success = false;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = wait_for_cancel(&self.cancel) => { self.give_up("the turn was cancelled".into(), &tx); return; },
                    _ = tx.closed() => return,
                    event = stream.next() => event,
                };
                match event {
                    Some(Ok(rig_agent::agent::MultiTurnStreamItem::FinalResponse(_))) => {
                        success = true;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(error)) => {
                        self.conversation.refresh();
                        self.give_up(error.to_string(), &tx);
                        return;
                    }
                    None => break,
                }
            }
            if !success {
                self.give_up("the task ended without completing".into(), &tx);
                return;
            }
            {
                let phase = phase.lock().unwrap_or_else(|p| p.into_inner());
                total = total.merge(phase.totals.usage);
                cost_usd += phase.totals.cost_usd;
            }
            self.conversation
                .annotate_completed(&phase.lock().unwrap_or_else(|p| p.into_inner()).measurements);
            self.conversation.refresh();
            self.emit_context(&tx);
            let mut background = self.absorb_background(&mut total, &mut cost_usd);
            if !background && !self.steering.has_pending() && !self.background.is_empty() {
                if !self
                    .wait_for_background(&tx, &mut total, &mut cost_usd)
                    .await
                {
                    return;
                }
                background |= self.absorb_background(&mut total, &mut cost_usd);
                // wait_for_background records the completion it receives.
                background |= !self.steering.has_pending();
            }
            let mut inputs = self.steering.drain();
            for text in &inputs {
                let _ = tx.send(AgentEvent::Steered { text: text.clone() });
            }
            if inputs.is_empty() && !background {
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
                prompt = "Process the completed background task results.".into();
            } else {
                let _ = tx.send(AgentEvent::TurnComplete {
                    usage: total,
                    cost_usd,
                });
                return;
            }
        }
    }
}
