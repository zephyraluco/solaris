//! Scripted test replies exposed as native Rig Serve handlers.
use super::TurnRequest;
use crate::{AgentEvent, AgentEventStream, BackendError, Message, ModelClient, ToolSpec};
use futures::StreamExt;
use rig_core::{
    completion::{CompletionResponse, FinishReason, Usage as NativeUsage},
    effect::{EffectKind, FamilyDescriptor, HandlerDescriptor, family},
    error::{ErrorKind, ErrorReport},
    message::{AssistantContent, Origin, ToolName},
    serve::{Dispatch, Reply, Serve},
    streaming::{Item, Part, PartKind, Relayed, StreamEvent},
};
use std::{collections::VecDeque, future::Future, pin::Pin, sync::Arc};
type Response = Pin<Box<dyn Future<Output = Result<AgentEventStream, BackendError>> + Send>>;
struct Fixture {
    reply: Box<dyn Fn(TurnRequest) -> Response + Send + Sync>,
}
pub fn client<F, Fut>(capabilities: crate::ModelCapabilities, reply: F) -> ModelClient
where
    F: Fn(TurnRequest) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<AgentEventStream, BackendError>> + Send + 'static,
{
    ModelClient::fixture(
        Fixture {
            reply: Box::new(move |request| Box::pin(reply(request))),
        },
        capabilities,
    )
}
fn error(value: impl ToString) -> ErrorReport {
    ErrorReport::new(ErrorKind::Provider, value.to_string())
}
fn origin() -> Origin {
    Origin::new("fixture", "fixture", "fixture")
}
impl Serve for Fixture {
    type Family = family::Completion;
    fn descriptor(&self) -> HandlerDescriptor {
        HandlerDescriptor {
            key: "model:fixture".into(),
            family: FamilyDescriptor::Completion {
                model: "fixture".into(),
                capabilities: Default::default(),
            },
            layers: Vec::new(),
        }
    }
    async fn serve(&self, kind: EffectKind, _: Dispatch) -> Reply {
        let EffectKind::Completion { request, .. } = kind else {
            return Reply::Outcome(Err(error("expected a completion")));
        };
        let request = TurnRequest {
            history: request
                .chat_history
                .iter()
                .map(|message| Message::from(message.as_ref()))
                .collect(),
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
        let stream = match (self.reply)(request).await {
            Ok(stream) => stream,
            Err(err) => return Reply::Outcome(Err(error(err))),
        };
        let events = Events {
            stream,
            content: Vec::new(),
            active: None,
            pending: VecDeque::from([Ok(Relayed::Origin(origin()))]),
            done: false,
            truncated: false,
        };
        Reply::Stream(Box::pin(futures::stream::unfold(
            events,
            |mut events| async move { events.next().await.map(|item| (item, events)) },
        )))
    }
}
struct Events {
    stream: AgentEventStream,
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
                    if usage.estimated {
                        measured = NativeUsage::new();
                    }
                    measured.cost = Some(rig_core::completion::Cost::from_parts(
                        cost_usd, 0.0, 0.0, 0.0,
                    ));
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
