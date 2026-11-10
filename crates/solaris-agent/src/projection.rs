//! In-memory request headers and message-linked measurements, shared by budgets and UI.
use serde::Serialize;
use sha2::{Digest, Sha256};
use solaris_backend::{
    ContextUsage, Message, MessageContext, ToolSpec, Usage, tokens_for_characters,
};

#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub base_id: String,
    pub request_id: String,
    pub overhead: u32,
}

fn digest(value: &impl Serialize) -> String {
    serde_json::to_vec(value)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .unwrap_or_default()
}

impl Header {
    pub fn new(model: &str, system: &str, tools: &[ToolSpec]) -> Self {
        let request_id = digest(&(model, system, tools));
        let chars = system
            .chars()
            .count()
            .saturating_add(serde_json::to_string(tools).map_or(0, |text| text.chars().count()));
        Self {
            base_id: request_id.clone(),
            request_id,
            overhead: tokens_for_characters(chars),
        }
    }

    pub fn annotate(&self, message: &mut Message, usage: Usage) {
        if !usage.estimated && !usage.is_empty() {
            message.context = Some(MessageContext::Measurement {
                usage,
                base_id: self.base_id.clone(),
                request_id: self.request_id.clone(),
            });
        }
    }
}

/// Reuse measurements only for the current request configuration.
pub(crate) fn estimate(messages: &[Message], header: &Header, window: u32) -> (u32, ContextUsage) {
    let mut after_boundary = false;
    for (index, message) in messages.iter().enumerate().rev() {
        match &message.context {
            Some(MessageContext::Boundary) => {
                after_boundary = true;
                break;
            }
            Some(MessageContext::Measurement {
                usage,
                base_id,
                request_id,
            }) => {
                if base_id == &header.base_id
                    && request_id == &header.request_id
                    && !usage.estimated
                    && !usage.is_empty()
                {
                    let tail = tokens_for_characters(
                        messages[index + 1..].iter().map(Message::characters).sum(),
                    );
                    let tokens = usage.total().saturating_add(tail);
                    return (
                        tokens,
                        ContextUsage {
                            tokens: Some(tokens),
                            window,
                            estimated: tail > 0,
                        },
                    );
                }
                // A configuration change invalidates older measurements as well.
                break;
            }
            None => {}
        }
    }
    let tokens = header.overhead.saturating_add(tokens_for_characters(
        messages.iter().map(Message::characters).sum(),
    ));
    (
        tokens,
        ContextUsage {
            tokens: (!after_boundary).then_some(tokens),
            window,
            estimated: true,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn measurements_are_per_request_and_trailing_messages_are_estimated() {
        let header = Header::new("model", "system", &[]);
        let mut first = Message::assistant("first");
        header.annotate(&mut first, Usage::new(100, 20).with_cache(1000, 0));
        let mut second = Message::assistant("second");
        header.annotate(&mut second, Usage::new(200, 30).with_cache(2000, 0));
        let messages = [first, second, Message::user("x".repeat(4000))];
        let (_, context) = estimate(&messages, &header, 10000);
        assert_eq!(context.tokens, Some(3230));
        assert!(context.estimated);
    }
    #[test]
    fn boundaries_and_configuration_changes_reject_old_usage() {
        let header = Header::new("model", "system", &[]);
        let mut measured = Message::assistant("reply");
        header.annotate(&mut measured, Usage::new(10000, 20));
        let changed = Header::new("other", "system", &[]);
        assert!(estimate(&[measured.clone()], &changed, 1000).1.estimated);
        let mut boundary = Message::user("summary");
        boundary.context = Some(MessageContext::Boundary);
        let (budget, display) = estimate(&[measured, boundary], &header, 1000);
        assert_eq!(display.tokens, None);
        assert!(budget < 100);
    }
    #[test]
    fn journal_round_trip_retains_measurement_without_recovery_tasks() {
        let header = Header::new("model", "system", &[]);
        let mut message = Message::assistant("reply");
        header.annotate(&mut message, Usage::new(100, 10));
        let decoded: Vec<Message> =
            serde_json::from_str(&serde_json::to_string(&vec![message]).expect("encode"))
                .expect("decode");
        assert_eq!(estimate(&decoded, &header, 1000).1.tokens, Some(110));
    }
}
