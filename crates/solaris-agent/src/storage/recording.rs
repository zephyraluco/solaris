//! Persist official executor lifecycle observations without copying message history.
use std::sync::Arc;

use rig_core::{
    effect::{Answered, EffectId, EffectKind, HandlerDescriptor, HandlerKey},
    error::ErrorReport,
    message::Origin,
    serve::{DispatchContext, Recorder},
    streaming::{Item, StreamEvent},
    tool::ToolResultContext,
};
use serde_json::json;

use crate::storage::memory::Conversation;

pub(crate) struct ExecutionRecorder {
    conversation: Arc<Conversation>,
    task: String,
}
impl ExecutionRecorder {
    pub fn new(conversation: Arc<Conversation>) -> Self {
        Self {
            conversation,
            task: crate::identity::new_id(),
        }
    }
    fn record(&self, detail: serde_json::Value) {
        self.conversation.record_execution(&self.task, detail);
    }
}
impl Recorder for ExecutionRecorder {
    fn handlers(&self, handlers: Vec<HandlerDescriptor>) {
        self.record(json!({"event":"handlers", "keys":handlers.iter().map(|handler| handler.key.as_str()).collect::<Vec<_>>() }));
    }
    fn begin(&self, id: EffectId, key: HandlerKey, kind: EffectKind, origin: DispatchContext) {
        self.record(json!({"event":"begin", "id":id, "handler":key.as_str(),
            "family":kind.family(), "parent":origin.parent, "scope":origin.scope }));
    }
    fn discard(&self, id: EffectId) {
        self.record(json!({"event":"discard", "id":id}));
    }
    fn patch(&self, id: EffectId, kind: EffectKind) {
        self.record(json!({"event":"patch", "id":id, "family":kind.family()}));
    }
    fn keep_events(&self) -> bool {
        false
    }
    fn event(&self, _: EffectId, _: &Item<StreamEvent>) {}
    fn origin(&self, _: EffectId, _: &Origin) {}
    fn tool_output(&self, _: EffectId, _: ToolResultContext) {}
    fn resolve(&self, id: EffectId, outcome: Result<Answered, ErrorReport>) {
        let detail = match outcome {
            Ok(Answered::Completion(response)) => json!({"event":"complete", "id":id,
                "family":"completion", "reported_usage":response.usage,
                "finish_reason":response.finish_reason(), "request_id":response.provider_request_id }),
            Ok(Answered::ToolResult { result }) => json!({"event":"complete", "id":id,
                "family":"tool", "status":result.status_name()}),
            Ok(other) => json!({"event":"complete", "id":id, "family":other.family()}),
            Err(error) => json!({"event":"error", "id":id, "kind":error.kind,
                "status":error.http_status, "request_id":error.request_id}),
        };
        self.record(detail);
    }
}
