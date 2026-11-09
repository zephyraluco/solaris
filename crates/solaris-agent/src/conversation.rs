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
    headers: Mutex<Vec<crate::projection::Header>>,
    published: Mutex<std::collections::BTreeMap<String, solaris_backend::ContextUsage>>,
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
        self.publish();
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

    pub(crate) fn record_model(&self, provider: Option<&str>, model: &str) {
        self.persist(SessionRecord::Model {
            provider: provider.map(str::to_owned),
            model: model.to_owned(),
        });
    }

    fn persist(&self, record: SessionRecord) -> bool {
        if self.error().is_some() {
            return false;
        }
        if let Some(journal) = self.journal()
            && let Err(error) = journal.append(&record)
        {
            *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
            return false;
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
        drop(messages);
        self.publish();
    }

    /// Replace everything, which is what compaction does.
    pub(crate) fn replace_with(&self, mut messages: Vec<Message>) {
        for message in &mut messages {
            message.context = None;
        }
        if let Some(first) = messages.first_mut() {
            first.context = Some(solaris_backend::MessageContext::Boundary);
        }
        let mut current = self.lock();
        if self.persist(SessionRecord::Context {
            messages: messages.clone(),
        }) {
            *current = messages;
        }
        drop(current);
        self.publish();
    }

    /// Forget the conversation, for `/clear`.
    ///
    /// The next turn starts with nothing but its own prompt and a fresh system
    /// prompt, so nothing of the old conversation reaches the model again.
    pub(crate) fn clear(&self) {
        self.replace_with(Vec::new());
    }

    /// Complete missing results in the latest assistant round without replaying
    /// tools. Older rounds may reuse the same call IDs, so only results after
    /// this assistant message count as answers.
    pub(crate) fn repair(&self) {
        let mut messages = self.lock();
        let Some(round) = messages.iter().rposition(|m| m.role == Role::Assistant) else {
            return;
        };
        let calls: Vec<_> = messages[round].tool_calls().cloned().collect();
        let answered: std::collections::HashSet<_> = messages[round + 1..]
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
        drop(messages);
        self.publish();
    }

    pub(crate) fn set_headers(&self, headers: Vec<crate::projection::Header>) {
        *self.headers.lock().unwrap_or_else(|p| p.into_inner()) = headers;
        self.publish();
    }

    fn publish(&self) {
        let messages = self.lock();
        let headers = self.headers.lock().unwrap_or_else(|p| p.into_inner());
        let snapshots = headers
            .iter()
            .map(|header| {
                (
                    header.base_id.clone(),
                    crate::projection::estimate(&messages, header, false, 0).1,
                )
            })
            .collect();
        *self.published.lock().unwrap_or_else(|p| p.into_inner()) = snapshots;
    }

    pub(crate) fn context_usage(
        &self,
        base_id: &str,
        window: u32,
    ) -> solaris_backend::ContextUsage {
        let unknown = solaris_backend::ContextUsage {
            tokens: None,
            window,
            estimated: true,
        };
        self.published
            .try_lock()
            .ok()
            .and_then(|snapshots| snapshots.get(base_id).copied())
            .map_or(unknown, |context| solaris_backend::ContextUsage {
                window,
                ..context
            })
    }

    pub(crate) fn projected_tokens(&self, header: &crate::projection::Header) -> u32 {
        crate::projection::estimate(&self.lock(), header, true, 0).0
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

    fn interrupted_reused_id_history() -> Vec<Message> {
        vec![
            Message::user("old round"),
            Message::tool_use(call("reused")),
            Message::tool_result(ToolResult::ok(&call("reused"), "old result")),
            Message::user("new round"),
            Message::with_content(
                Role::Assistant,
                vec![
                    solaris_backend::Content::ToolUse {
                        call: call("reused"),
                    },
                    solaris_backend::Content::ToolUse {
                        call: call("other"),
                    },
                ],
            ),
            Message::tool_result(ToolResult::ok(&call("other"), "current partial result")),
        ]
    }

    #[test]
    fn a_reused_id_is_repaired_without_changing_older_or_current_results() {
        let conversation = Conversation::new();
        let before = interrupted_reused_id_history();
        conversation.replace_with(before);
        let before = conversation.snapshot();
        conversation.repair();
        let after = conversation.snapshot();
        assert_eq!(after.len(), before.len() + 1);
        assert_eq!(&after[..before.len()], before.as_slice());
        let repaired = after
            .last()
            .expect("repair")
            .tool_results()
            .next()
            .expect("result");
        assert_eq!(repaired.id, "reused");
        assert!(repaired.is_error);
        conversation.repair();
        assert_eq!(conversation.snapshot(), after, "repair must be idempotent");
    }

    #[test]
    fn a_reused_id_with_a_current_result_is_not_repaired() {
        let conversation = Conversation::new();
        let mut complete = interrupted_reused_id_history();
        complete.push(Message::tool_result(ToolResult::ok(
            &call("reused"),
            "current result",
        )));
        conversation.replace_with(complete);
        let before = conversation.snapshot();
        conversation.repair();
        assert_eq!(conversation.snapshot(), before);
    }

    #[test]
    fn reopening_a_session_repairs_and_persists_the_latest_reused_id_once() {
        let directory = tempfile::tempdir().expect("directory");
        let store =
            crate::SessionStore::open(&directory.path().join("sessions.sqlite")).expect("store");
        let journal = store
            .create(directory.path(), "model", None)
            .expect("session");
        let id = journal.id().to_owned();
        let before = interrupted_reused_id_history();
        journal
            .append(&SessionRecord::Context {
                messages: before.clone(),
            })
            .expect("interrupted context");
        drop(journal);
        let conversation = Conversation::new();
        conversation
            .attach(store.resume(&id).expect("resume"))
            .expect("attach");
        let repaired = conversation.snapshot();
        assert_eq!(repaired.len(), before.len() + 1);
        assert_eq!(&repaired[..before.len()], before.as_slice());
        assert!(
            repaired
                .last()
                .expect("repair")
                .tool_results()
                .next()
                .expect("result")
                .is_error
        );
        drop(conversation);

        let count = store.records(&id).expect("records").len();
        let reopened = Conversation::new();
        reopened
            .attach(store.resume(&id).expect("resume again"))
            .expect("attach again");
        assert_eq!(reopened.snapshot(), repaired);
        assert_eq!(
            store.records(&id).expect("records").len(),
            count,
            "resume must not append a duplicate repair"
        );
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
