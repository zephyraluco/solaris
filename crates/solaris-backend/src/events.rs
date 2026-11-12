//! Display-only adaptation of native executor replies. No HTTP or retry logic.
use std::collections::{HashMap, VecDeque};

use futures::StreamExt;
use rig_core::completion::FinishReason;
use rig_core::message::AssistantContent;
use rig_core::streaming::{Item, Relayed, StreamEvent, StreamEvents};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::{AgentEvent, AgentEventStream, BackendError, ModelClient, Usage, convert};

pub(crate) fn adapt(
    client: ModelClient,
    stream: StreamEvents,
    status: UnboundedReceiver<AgentEvent>,
    input_characters: usize,
) -> AgentEventStream {
    let state = Events {
        client,
        stream,
        status,
        input_characters,
        output_characters: 0,
        parts: HashMap::new(),
        pending: VecDeque::new(),
        done: false,
    };
    Box::pin(futures::stream::unfold(state, |mut state| async move {
        state.next().await.map(|event| (event, state))
    }))
}
struct Events {
    client: ModelClient,
    stream: StreamEvents,
    status: UnboundedReceiver<AgentEvent>,
    input_characters: usize,
    output_characters: usize,
    parts: HashMap<rig_core::streaming::Part, String>,
    pending: VecDeque<AgentEvent>,
    done: bool,
}
impl Events {
    async fn next(&mut self) -> Option<AgentEvent> {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return Some(event);
            }
            if self.done {
                return None;
            }
            let next = tokio::select! {
                biased;
                Some(status) = self.status.recv() => return Some(status),
                next = self.stream.next() => next,
            };
            match next {
                Some(Ok(Relayed::Item(Item::Event(event)))) => {
                    let event = match event {
                        StreamEvent::Text { part, text } => {
                            self.parts.entry(part).or_default().push_str(&text);
                            self.output_characters += text.chars().count();
                            Some(AgentEvent::TextDelta(text))
                        }
                        StreamEvent::Reasoning { part, text } => {
                            self.parts.entry(part).or_default().push_str(&text);
                            Some(AgentEvent::ThinkingDelta(text))
                        }
                        StreamEvent::End {
                            part,
                            content: AssistantContent::Text(text),
                        } => {
                            let previous = self.parts.remove(&part).unwrap_or_default();
                            match final_suffix(&previous, &text.text) {
                                Ok(Some(text)) => {
                                    self.output_characters += text.chars().count();
                                    Some(AgentEvent::TextDelta(text))
                                }
                                Ok(None) => None,
                                Err(error) => {
                                    self.done = true;
                                    Some(AgentEvent::Error(error.to_string()))
                                }
                            }
                        }
                        StreamEvent::End {
                            part,
                            content: AssistantContent::Reasoning(reasoning),
                        } => {
                            let previous = self.parts.remove(&part).unwrap_or_default();
                            match final_suffix(&previous, &reasoning.text) {
                                Ok(Some(text)) => Some(AgentEvent::ThinkingDelta(text)),
                                Ok(None) => None,
                                Err(error) => {
                                    self.done = true;
                                    Some(AgentEvent::Error(error.to_string()))
                                }
                            }
                        }
                        StreamEvent::End {
                            content: AssistantContent::ToolCall(call),
                            ..
                        } => {
                            let arguments = call.function.invalid_arguments.as_ref().map_or_else(
                                || call.function.arguments_value(),
                                |invalid| serde_json::Value::String(invalid.clone()),
                            );
                            Some(AgentEvent::ToolCall(crate::ToolCall::new(
                                call.id.wire().into_owned(),
                                call.function.name.as_str(),
                                arguments,
                            )))
                        }
                        _ => None,
                    };
                    if event.is_some() {
                        return event;
                    }
                }
                Some(Ok(Relayed::Done(response))) => {
                    self.done = true;
                    if let Some(error) = response.error.as_ref().or(response.aborted.as_ref()) {
                        return Some(AgentEvent::Error(error.clone()));
                    }
                    let usage = if response.usage.input_tokens.is_some()
                        || response.usage.output_tokens.is_some()
                    {
                        convert::usage(response.usage)
                    } else {
                        Usage::estimate(self.input_characters, self.output_characters)
                    };
                    let complete = AgentEvent::TurnComplete {
                        usage,
                        cost_usd: self.client.cost(usage),
                    };
                    if matches!(response.finish_reason(), Some(FinishReason::Length)) {
                        self.pending.push_back(complete);
                        return Some(AgentEvent::OutputTruncated);
                    }
                    return Some(complete);
                }
                Some(Ok(_)) => {}
                Some(Err(error)) => {
                    self.done = true;
                    return Some(AgentEvent::Error(self.client.backend.as_ref().map_or_else(
                        || error.to_string(),
                        |backend| backend.describe_error(&error).to_string(),
                    )));
                }
                None => {
                    self.done = true;
                    return Some(AgentEvent::Error(
                        "model stream ended without a complete response".into(),
                    ));
                }
            }
        }
    }
}
fn final_suffix(previous: &str, final_text: &str) -> Result<Option<String>, BackendError> {
    let suffix = final_text
        .strip_prefix(previous)
        .ok_or_else(|| BackendError::new("provider changed previously streamed text"))?;
    Ok((!suffix.is_empty()).then(|| suffix.to_string()))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn snapshots_emit_only_missing_text() {
        assert_eq!(
            final_suffix("hello", "hello world").unwrap().as_deref(),
            Some(" world")
        );
        assert_eq!(final_suffix("hello", "hello").unwrap(), None);
        assert!(final_suffix("hello", "different").is_err());
    }
}
