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

use crate::{SessionJournal, SessionRecord, StoreError};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use solaris_backend::{Message, Role, tokens_for_characters};

/// The messages a turn is built from and appends to.
#[derive(Default)]
pub(crate) struct Conversation {
    messages: Mutex<Vec<Message>>,
    journal: RwLock<Option<Arc<SessionJournal>>>,
    failure: Mutex<Option<String>>,
}

impl Conversation {
    /// An empty conversation.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn attach(&self, journal: Arc<SessionJournal>) -> Result<(), StoreError> {
        let context = match journal.context() {
            Ok(context) => context,
            Err(error) => {
                *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                return Err(error);
            }
        };
        *self.lock() = context;
        *self
            .journal
            .write()
            .map_err(|_| StoreError::Invalid("journal lock poisoned".into()))? = Some(journal);
        self.repair();
        if let Some(error) = self.error() {
            return Err(StoreError::Invalid(error));
        }
        Ok(())
    }

    pub(crate) fn journal(&self) -> Option<Arc<SessionJournal>> {
        self.journal
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    pub(crate) fn error(&self) -> Option<String> {
        self.failure
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn persist(&self, record: SessionRecord) -> bool {
        if self.error().is_some() {
            return false;
        }
        if let Some(journal) = self.journal() {
            if let Err(error) = journal.append(&record) {
                *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                return false;
            }
        }
        true
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
        let mut messages = self.lock();
        if self.persist(SessionRecord::Message {
            message: message.clone(),
        }) {
            messages.push(message);
        }
    }

    /// Replace everything, which is what compaction does.
    pub(crate) fn replace_with(&self, messages: Vec<Message>) {
        let mut current = self.lock();
        if self.persist(SessionRecord::Context {
            messages: messages.clone(),
        }) {
            *current = messages;
        }
    }

    /// Forget the conversation, for `/clear`.
    ///
    /// The next turn starts with nothing but its own prompt and a fresh system
    /// prompt, so nothing of the old conversation reaches the model again.
    pub(crate) fn clear(&self) {
        self.replace_with(Vec::new());
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
        let calls: Vec<_> = messages
            .iter()
            .rev()
            .find(|m| m.role == Role::Assistant)
            .into_iter()
            .flat_map(|m| m.tool_calls())
            .cloned()
            .collect();
        let answered: std::collections::HashSet<_> = messages
            .iter()
            .flat_map(|m| m.tool_results())
            .map(|r| r.id.as_str())
            .collect();
        let missing: Vec<_> = calls
            .into_iter()
            .filter(|call| !answered.contains(call.id.as_str()))
            .collect();
        for call in missing {
            let message = Message::tool_result(solaris_backend::ToolResult::error(
                &call,
                "interrupted; execution outcome is unknown; verify side effects before retrying",
            ));
            if self.persist(SessionRecord::Message {
                message: message.clone(),
            }) {
                messages.push(message);
            }
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
    fn unanswered_calls_get_explicit_interruption_results() {
        let conversation = Conversation::new();
        conversation.push(Message::user("read it"));
        conversation.push(Message::tool_use(call("call-1")));

        conversation.repair();

        assert_eq!(conversation.len(), 3);
        assert!(
            conversation.snapshot()[2]
                .tool_results()
                .next()
                .expect("result")
                .is_error
        );
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
