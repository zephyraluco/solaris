//! Explicit adapter for injected event sources. Production providers use native models.
use crate::{AgentBackend, AgentEvent, AgentEventStream, Message, ToolSpec, TurnRequest};
use futures::StreamExt;
use rig_core::{
    completion::{CompletionResponse, FinishReason, Usage as NativeUsage},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::{AssistantContent, Origin, ToolName},
    serve::{Dispatch, Reply, Serve},
    streaming::{Item, Part, PartKind, Relayed, StreamEvent},
};
use std::collections::VecDeque;
use std::sync::Arc;

pub(crate) struct EventModel<T: ?Sized> {
    pub backend: Arc<T>,
    pub reported: crate::connection::ReportedUsage,
}
fn error(message: impl ToString) -> ErrorReport {
    ErrorReport::new(ErrorKind::Provider, message.to_string())
}
impl<T: AgentBackend + ?Sized + 'static> Serve for EventModel<T> {
    type Family = family::Completion;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: "model:injected".into(),
            family: FamilyDescriptor::Completion {
                model: "injected".into(),
                capabilities: Default::default(),
            },
            layers: Vec::new(),
        }
    }
    async fn serve(&self, kind: EffectKind, _: Dispatch) -> Reply {
        let EffectKind::Completion { request, .. } = kind else {
            return Reply::Outcome(Err(error("expected a model request")));
        };
        let history: Vec<_> = request
            .chat_history
            .iter()
            .map(|message| Message::from(message.as_ref()))
            .collect();
        let request = TurnRequest {
            history,
            prompt: String::new(),
            tools: request
                .tools
                .iter()
                .map(|tool| {
                    ToolSpec::new(
                        tool.name.as_str(),
                        &tool.description,
                        tool.parameters.clone(),
                    )
                })
                .collect(),
        };
        let stream = match self.backend.run_turn(request).await {
            Ok(stream) => stream,
            Err(value) => return Reply::Outcome(Err(error(value))),
        };
        let state = Events {
            stream,
            reported: Arc::clone(&self.reported),
            content: Vec::new(),
            active: None,
            pending: VecDeque::from([Ok(Relayed::Origin(origin()))]),
            done: false,
            truncated: false,
        };
        Reply::Stream(Box::pin(futures::stream::unfold(
            state,
            |mut state| async move { state.next().await.map(|event| (event, state)) },
        )))
    }
}
fn origin() -> Origin {
    Origin::new("injected", "injected", "injected")
}
struct Events {
    stream: AgentEventStream,
    reported: crate::connection::ReportedUsage,
    content: Vec<AssistantContent>,
    active: Option<(PartKind, String)>,
    pending: VecDeque<Result<Relayed, ErrorReport>>,
    done: bool,
    truncated: bool,
}
impl Events {
    fn emit(&mut self, event: StreamEvent) {
        self.pending
            .push_back(Ok(Relayed::Item(Item::Event(event))));
    }
    fn close_part(&mut self) {
        if let Some((kind, text)) = self.active.take() {
            let content = if kind == PartKind::Text {
                AssistantContent::text(text)
            } else {
                AssistantContent::reasoning(text)
            };
            let part = Part::new(self.content.len() as u32);
            self.content.push(content.clone());
            self.emit(StreamEvent::End { part, content });
        }
    }
    fn delta(&mut self, kind: PartKind, text: String) {
        if self
            .active
            .as_ref()
            .is_some_and(|(active, _)| *active != kind)
        {
            self.close_part();
        }
        let part = Part::new(self.content.len() as u32);
        if self.active.is_none() {
            self.active = Some((kind, String::new()));
            self.emit(StreamEvent::Start {
                part,
                kind,
                name: None,
            });
        }
        if let Some((_, value)) = self.active.as_mut() {
            value.push_str(&text);
        }
        self.emit(if kind == PartKind::Text {
            StreamEvent::Text { part, text }
        } else {
            StreamEvent::Reasoning { part, text }
        });
    }
    async fn next(&mut self) -> Option<Result<Relayed, ErrorReport>> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            match self.stream.next().await {
                Some(AgentEvent::TextDelta(text)) => self.delta(PartKind::Text, text),
                Some(AgentEvent::ThinkingDelta(text)) => self.delta(PartKind::Reasoning, text),
                Some(AgentEvent::ToolCall(call)) => {
                    self.close_part();
                    let name = match ToolName::new(call.name) {
                        Ok(name) => name,
                        Err(value) => {
                            self.done = true;
                            return Some(Err(error(value)));
                        }
                    };
                    let part = Part::new(self.content.len() as u32);
                    self.emit(StreamEvent::Start {
                        part,
                        kind: PartKind::ToolCall,
                        name: Some(name.clone()),
                    });
                    let content = AssistantContent::tool_call(call.id, name, call.input);
                    self.content.push(content.clone());
                    self.emit(StreamEvent::End { part, content });
                }
                Some(AgentEvent::OutputTruncated) => self.truncated = true,
                Some(AgentEvent::TurnComplete { usage, cost_usd }) => {
                    self.close_part();
                    *self.reported.lock().unwrap_or_else(|p| p.into_inner()) =
                        Some((usage, cost_usd));
                    let mut measured = NativeUsage::new();
                    measured.input_tokens = Some(
                        u64::from(usage.input_tokens)
                            + u64::from(usage.cache_read_tokens)
                            + u64::from(usage.cache_write_tokens),
                    );
                    measured.output_tokens = Some(u64::from(usage.output_tokens));
                    measured.cached_input_tokens = Some(u64::from(usage.cache_read_tokens));
                    measured.cache_creation_input_tokens =
                        Some(u64::from(usage.cache_write_tokens));
                    let response = CompletionResponse::new(
                        std::mem::take(&mut self.content),
                        measured,
                        origin(),
                        serde_json::Value::Null,
                    )
                    .with_finish_reason(if self.truncated {
                        FinishReason::Length
                    } else {
                        FinishReason::Stop
                    });
                    self.pending
                        .push_back(Ok(Relayed::Done(Arc::new(response))));
                    self.done = true;
                }
                Some(AgentEvent::Error(value)) => {
                    self.done = true;
                    return Some(Err(error(value)));
                }
                Some(_) => {}
                None => {
                    self.done = true;
                    return Some(Err(error("the model's response ended without completing")));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BackendError, ModelClient, Usage};
    struct Source;
    #[async_trait::async_trait]
    impl AgentBackend for Source {
        async fn model_client(self: Arc<Self>) -> Result<ModelClient, BackendError> {
            Ok(ModelClient::from_events(self))
        }
        async fn run_turn(&self, _: TurnRequest) -> Result<AgentEventStream, BackendError> {
            Ok(Box::pin(futures::stream::iter([
                AgentEvent::TextDelta("ok".into()),
                AgentEvent::TurnComplete {
                    usage: Usage {
                        estimated: true,
                        ..Usage::new(7, 2).with_cache(3, 1)
                    },
                    cost_usd: 0.25,
                },
            ])))
        }
    }
    #[tokio::test]
    async fn explicit_event_connection_preserves_estimates_cache_usage_and_accounting() {
        let observed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let records = Arc::clone(&observed);
        let client = Arc::new(Source)
            .model_client()
            .await
            .unwrap()
            .with_usage_observer(move |usage, cost| {
                records.lock().unwrap().push((usage, cost));
                Ok(())
            });
        let reply = client
            .serve(
                EffectKind::Completion {
                    request: Arc::new(rig_core::completion::CompletionRequest::new("hello")),
                    stream: true,
                },
                Dispatch::new(rig_core::effect::EffectId::from_raw(1), true),
            )
            .await;
        let rig_core::effect::Answered::Completion(response) = reply.into_outcome().await.unwrap()
        else {
            panic!("completion expected")
        };
        assert_eq!(response.text(), "ok");
        assert_eq!(response.usage.input_tokens, Some(11));
        let usage = client.usage(crate::convert::usage(response.usage));
        assert!(usage.estimated);
        assert_eq!(
            usage,
            Usage {
                estimated: true,
                ..Usage::new(7, 2).with_cache(3, 1)
            }
        );
        assert_eq!(client.settle(usage).unwrap(), 0.25);
        assert_eq!(*observed.lock().unwrap(), [(usage, 0.25)]);
    }
}
