//! Request views and token anchors delegated to rig-memory. Never a message author.
use rig_core::{completion::ToolDefinition, conversation::Conversation, message::Message};
use serde::Serialize;
use sha2::{Digest, Sha256};
use solaris_backend::{ContextUsage, ToolSpec, Usage};
use std::sync::Arc;

#[derive(Debug, Clone)]
pub(crate) struct Header {
    pub base_id: String,
    pub request_id: String,
    model: String,
    system: String,
    pub tools: Vec<Arc<ToolDefinition>>,
}
fn digest(value: &impl Serialize) -> String {
    serde_json::to_vec(value)
        .map(|bytes| format!("{:x}", Sha256::digest(bytes)))
        .unwrap_or_default()
}
impl Header {
    pub fn new(model: &str, system: &str, tools: &[ToolSpec]) -> Self {
        let request_id = digest(&(model, system, tools));
        Self {
            base_id: request_id.clone(),
            request_id,
            model: model.into(),
            system: system.into(),
            tools: tools
                .iter()
                .filter_map(|tool| {
                    rig_core::message::ToolName::new(&tool.name)
                        .ok()
                        .map(|name| {
                            Arc::new(ToolDefinition::new(
                                name,
                                &tool.description,
                                tool.parameters.clone(),
                            ))
                        })
                })
                .collect(),
        }
    }
}

#[derive(Default)]
pub(crate) struct Projection {
    history: Conversation,
    initialized: bool,
    anchor: Option<rig_memory::Anchor>,
    sent: Option<rig_memory::RequestStart>,
    sent_len: usize,
    sent_header: String,
    pending: Option<rig_core::completion::Usage>,
}
impl Projection {
    pub fn initialized(&self) -> bool {
        self.initialized
    }
    /// Keep shared native messages stable while the SDK extends its history.
    /// Any edit, replacement or rollback invalidates the process-local anchor.
    pub fn sync(&mut self, messages: &[Arc<Message>]) {
        let prefix = self.history.messages();
        if prefix.len() > messages.len()
            || !prefix
                .iter()
                .zip(messages)
                .all(|(left, right)| left == right)
        {
            *self = Self::default();
        }
        self.initialized = true;
        self.history.extend(
            messages
                .iter()
                .skip(self.history.messages().len())
                .cloned()
                .collect::<Vec<_>>(),
        );
        if let Some(usage) = self.pending.take() {
            let through = if matches!(
                self.history
                    .messages()
                    .get(self.sent_len)
                    .map(AsRef::as_ref),
                Some(Message::Assistant(_))
            ) {
                self.history.id(self.sent_len)
            } else {
                self.sent_len
                    .checked_sub(1)
                    .and_then(|at| self.history.id(at))
            };
            self.anchor = through.map(|through| rig_memory::Anchor { usage, through });
        }
    }
    pub fn observe(&mut self, usage: Usage) {
        if usage.estimated || self.sent.is_none() {
            self.anchor = None;
            self.pending = None;
            self.sent = None;
            return;
        }
        let mut measured = rig_core::completion::Usage::new();
        measured.input_tokens = Some(
            u64::from(usage.input_tokens)
                + u64::from(usage.cache_read_tokens)
                + u64::from(usage.cache_write_tokens),
        );
        measured.output_tokens = Some(u64::from(usage.output_tokens));
        measured.cached_input_tokens = Some(u64::from(usage.cache_read_tokens));
        measured.cache_creation_input_tokens = Some(u64::from(usage.cache_write_tokens));
        self.pending = Some(measured);
    }
    pub fn context(&mut self, header: &Header, sent: bool) -> ContextUsage {
        let request = rig_memory::Prepared {
            model: &header.model,
            prompt: (!header.system.is_empty()).then_some(header.system.as_str()),
            tools: &header.tools,
            history: &self.history,
        };
        let anchored = self
            .anchor
            .as_ref()
            .and_then(|anchor| self.history.index_of(anchor.through))
            .is_some_and(|at| {
                self.sent.is_some()
                    && self.sent_header == header.request_id
                    && (at == self.sent_len || at + 1 == self.sent_len)
            });
        let tokens = rig_memory::next_request_tokens(self.anchor.as_ref(), self.sent, &request);
        let tail = self
            .anchor
            .as_ref()
            .and_then(|anchor| self.history.index_of(anchor.through))
            .is_some_and(|at| at + 1 < self.history.messages().len());
        if sent {
            self.sent = Some(request.start());
            self.sent_len = self.history.messages().len();
            self.sent_header.clone_from(&header.request_id);
        }
        ContextUsage {
            tokens: Some(u32::try_from(tokens).unwrap_or(u32::MAX)),
            window: 0,
            estimated: !anchored || tail,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn messages(text: &str) -> Vec<Arc<Message>> {
        vec![Arc::new(Message::user(text))]
    }
    #[test]
    fn anchors_include_cache_traffic_once_and_estimate_only_the_tail() {
        let header = Header::new("model", "system", &[]);
        let mut projection = Projection::default();
        let mut history = messages("question");
        projection.sync(&history);
        projection.context(&header, true);
        projection.observe(Usage::new(100, 20).with_cache(1000, 0));
        history.push(Arc::new(Message::assistant("answer")));
        projection.sync(&history);
        assert_eq!(
            projection.context(&header, false),
            ContextUsage {
                tokens: Some(1120),
                window: 0,
                estimated: false
            }
        );
        history.push(Arc::new(Message::user("tail")));
        projection.sync(&history);
        let expected_tail = rig_memory::TokenCounter::count(
            &rig_memory::HeuristicTokenCounter::default(),
            &history[2],
        );
        assert_eq!(
            projection.context(&header, false).tokens,
            Some(1120 + expected_tail as u32)
        );
    }
    #[test]
    fn request_changes_and_replacements_invalidate_measurements() {
        let header = Header::new("model", "system", &[]);
        let mut projection = Projection::default();
        let mut history = messages("question");
        projection.sync(&history);
        projection.context(&header, true);
        projection.observe(Usage::new(10000, 20));
        history.push(Arc::new(Message::assistant("reply")));
        projection.sync(&history);
        assert!(!projection.context(&header, false).estimated);
        let changed = Header::new(
            "model",
            "system",
            &[ToolSpec::new(
                "read",
                "Read",
                serde_json::json!({"type":"object"}),
            )],
        );
        assert!(projection.context(&changed, false).estimated);
        projection.sync(&messages("summary"));
        let context = projection.context(&header, false);
        assert!(context.estimated && context.tokens.unwrap() < 1000);
    }
    #[test]
    fn restored_views_estimate_instead_of_reusing_process_local_anchors() {
        let header = Header::new("model", "system", &[]);
        let mut projection = Projection::default();
        projection.sync(&messages("restored"));
        assert!(projection.context(&header, false).estimated);
    }
}
