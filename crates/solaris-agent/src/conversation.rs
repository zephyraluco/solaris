//! The conversation, from the model's point of view.
//!
//! The agent owns this rather than being handed it turn by turn, and that is the
//! point: everything that changes the conversation for the *model* has to stay
//! changed. A summary that only existed for one turn would be paid for again on
//! the next one, and a steered message would have to be reconstructed from the
//! transcript rather than simply still being there.
//!
//! It is deliberately not the transcript. The transcript is what the user reads —
//! turns, tool steps, timings, thinking — and it lives in the application. What
//! is here is only what goes on the wire: the system prompt is *not* in it (the
//! agent writes one per turn, so it can never be summarized away), and nothing
//! here knows about turns at all.
//!
//! Locking is per operation, never held across an `await`: the loop appends after
//! each step it takes, so a turn abandoned halfway leaves the conversation with
//! exactly the parts that really happened.

use std::sync::{Mutex, MutexGuard};

use solaris_backend::{Message, Role, tokens_for_characters};

/// The messages a turn is built from and appends to.
#[derive(Debug, Default)]
pub(crate) struct Conversation {
    messages: Mutex<Vec<Message>>,
}

impl Conversation {
    /// An empty conversation.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// The messages as they are now, for building one request.
    ///
    /// A copy rather than a guard because the caller goes on to await with it,
    /// and holding the lock across that would let one turn block the next for as
    /// long as a command takes to run.
    pub(crate) fn snapshot(&self) -> Vec<Message> {
        self.lock().clone()
    }

    /// How many messages there are.
    pub(crate) fn len(&self) -> usize {
        self.lock().len()
    }

    /// Append one message.
    pub(crate) fn push(&self, message: Message) {
        self.lock().push(message);
    }

    /// Replace everything, which is what compaction does.
    pub(crate) fn replace_with(&self, messages: Vec<Message>) {
        *self.lock() = messages;
    }

    /// Forget the conversation, for `/clear`.
    ///
    /// The next turn starts with nothing but its own prompt and a fresh system
    /// prompt, so nothing of the old conversation reaches the model again.
    pub(crate) fn clear(&self) {
        self.lock().clear();
    }

    /// Drop a trailing request for calls that never got answers.
    ///
    /// A turn that is cancelled or fails can leave the conversation ending on an
    /// assistant message that asked for tools whose results never arrived, and
    /// every wire rejects a call with no result. Removing that one message is
    /// enough: results are appended immediately after the request, so an earlier
    /// request always has its answers.
    pub(crate) fn repair(&self) {
        let mut messages = self.lock();
        let unanswered = messages
            .last()
            .is_some_and(|last| last.role == Role::Assistant && last.tool_calls().next().is_some());
        if unanswered {
            messages.pop();
        }
    }

    /// Roughly what the conversation costs the model, by character count.
    ///
    /// Only a fallback and a budget: a provider that reports usage gives the real
    /// number, and this is what is left when none has been heard from yet.
    pub(crate) fn estimated_tokens(&self) -> u32 {
        tokens_for_characters(self.lock().iter().map(Message::characters).sum())
    }

    /// The lock, recovering from a panic in another holder rather than panicking
    /// here: a poisoned conversation must not take down the turn that is running.
    fn lock(&self) -> MutexGuard<'_, Vec<Message>> {
        self.messages
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use solaris_backend::{ToolCall, ToolResult};

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id, "read", json!({ "path": "a.txt" }))
    }

    #[test]
    fn what_is_pushed_is_what_a_request_is_built_from() {
        let conversation = Conversation::new();
        assert!(conversation.snapshot().is_empty());

        conversation.push(Message::user("hello"));
        conversation.push(Message::assistant("hi"));

        assert_eq!(conversation.len(), 2);
        let snapshot = conversation.snapshot();
        assert_eq!(snapshot[0].text(), "hello");
        assert_eq!(snapshot[1].text(), "hi");
    }

    #[test]
    fn clearing_forgets_everything() {
        let conversation = Conversation::new();
        conversation.push(Message::user("hello"));
        conversation.push(Message::assistant("hi"));
        conversation.clear();

        assert_eq!(conversation.len(), 0);
        assert!(conversation.snapshot().is_empty());
        assert_eq!(conversation.estimated_tokens(), 0);
    }

    #[test]
    fn a_request_for_calls_whose_answers_never_came_is_taken_back() {
        let conversation = Conversation::new();
        conversation.push(Message::user("read it"));
        conversation.push(Message::tool_use(call("call-1")));

        conversation.repair();

        // The prompt stays — it really was said — and the unanswered call goes,
        // because every wire rejects a call with no result.
        assert_eq!(conversation.len(), 1);
        assert_eq!(conversation.snapshot()[0].text(), "read it");
    }

    #[test]
    fn a_consistent_conversation_is_left_alone() {
        let conversation = Conversation::new();
        conversation.push(Message::user("read it"));
        conversation.push(Message::tool_use(call("call-1")));
        conversation.push(Message::tool_result(ToolResult::ok(
            &call("call-1"),
            "contents",
        )));
        conversation.push(Message::assistant("it says hello"));

        conversation.repair();

        assert_eq!(conversation.len(), 4, "nothing to take back");
    }

    #[test]
    fn repairing_an_empty_conversation_is_harmless() {
        let conversation = Conversation::new();
        conversation.repair();
        assert_eq!(conversation.len(), 0);
    }

    #[test]
    fn the_estimate_grows_with_what_is_in_it() {
        let conversation = Conversation::new();
        conversation.push(Message::user("x".repeat(400)));
        let small = conversation.estimated_tokens();

        conversation.push(Message::assistant("y".repeat(400)));
        assert!(conversation.estimated_tokens() > small);
    }
}
