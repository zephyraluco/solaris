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
//! Official memory owns messages and saves successful SDK tasks and explicit
//! safe-boundary handoffs. Failed or cancelled phases leave previously committed
//! history unchanged. Product
//! records (including tool intents and billing) are persisted independently.

use crate::{SessionJournal, SessionRecord, StoreError};
use std::sync::{
    Arc, Mutex, RwLock,
    atomic::{AtomicBool, Ordering},
};

use solaris_backend::{Message, Role};

/// The messages a turn is built from and appends to.
pub(crate) struct Conversation {
    storage: RwLock<Arc<dyn rig_memory::Store + Send + Sync>>,
    headers: Mutex<Vec<crate::projection::Header>>,
    projections: Mutex<std::collections::BTreeMap<String, crate::projection::Projection>>,
    projecting: AtomicBool,
    published: Mutex<std::collections::BTreeMap<String, solaris_backend::ContextUsage>>,
    journal: RwLock<Option<Arc<SessionJournal>>>,
    failure: Mutex<Option<String>>,
    billing: Mutex<crate::UsageTotals>,
}

pub(crate) struct ProjectionGuard<'a>(&'a Conversation);
impl Drop for ProjectionGuard<'_> {
    fn drop(&mut self) {
        self.0.projecting.store(false, Ordering::Release);
        self.0.publish();
    }
}

impl Default for Conversation {
    fn default() -> Self {
        Self {
            storage: RwLock::new(Arc::new(rig_memory::MemoryStore::default())),
            headers: Mutex::new(Vec::new()),
            published: Mutex::new(Default::default()),
            projections: Mutex::new(Default::default()),
            projecting: AtomicBool::new(false),
            journal: RwLock::new(None),
            failure: Mutex::new(None),
            billing: Mutex::new(Default::default()),
        }
    }
}
impl Conversation {
    /// An empty conversation.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    pub(crate) fn projecting(&self) -> ProjectionGuard<'_> {
        self.projecting.store(true, Ordering::Release);
        ProjectionGuard(self)
    }
    pub(crate) fn attach(&self, journal: Arc<SessionJournal>) -> Result<(), StoreError> {
        self.projections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let context = match journal.context() {
            Ok(context) => context,
            Err(error) => {
                *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
                return Err(error);
            }
        };
        let billing = crate::UsageTotals::restore(&journal.store().records(journal.id())?);
        *self.billing.lock().unwrap_or_else(|p| p.into_inner()) = billing;
        let directory = journal
            .store()
            .artifact_dir()
            .parent()
            .ok_or_else(|| StoreError::Invalid("missing session directory".into()))?
            .join("memory")
            .join(journal.id());
        let storage = rig_memory::DirStore::open(directory)
            .map_err(|error| StoreError::Invalid(error.to_string()))?;
        let imported = rig_memory::Store::logs(&storage)
            .map_err(StoreError::Io)?
            .iter()
            .any(|name| name == "conversation");
        *self.storage.write().unwrap_or_else(|p| p.into_inner()) = Arc::new(storage);
        if !imported {
            self.replace_with(context);
        }
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

    pub(crate) fn record_execution(&self, task: &str, detail: serde_json::Value) {
        self.persist(SessionRecord::Execution {
            task: task.into(),
            detail,
        });
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
    pub(crate) fn storage(&self) -> Arc<dyn rig_memory::Store + Send + Sync> {
        Arc::clone(&self.storage.read().unwrap_or_else(|p| p.into_inner()))
    }
    pub(crate) fn native(
        &self,
    ) -> Result<rig_core::conversation::Conversation, rig_memory::SessionError> {
        rig_memory::load(&self.storage(), "conversation")
    }
    /// Commit a SDK-owned safe-boundary snapshot, retaining the saved prefix,
    /// native message IDs and asides. The store's append check rejects races.
    pub(crate) fn commit_phase(
        &self,
        base: &mut rig_core::conversation::Conversation,
        history: Vec<rig_core::message::Message>,
    ) -> Result<(), String> {
        let prefix = base.messages();
        if history.len() < prefix.len()
            || !prefix
                .iter()
                .map(|message| message.as_ref())
                .eq(history[..prefix.len()].iter())
        {
            return Err("SDK phase history does not extend its saved conversation".into());
        }
        rig_core::conversation::pairing::validate(&history).map_err(|error| error.to_string())?;
        let suffix = history.into_iter().skip(prefix.len()).collect::<Vec<_>>();
        base.extend(suffix);
        rig_memory::save(&self.storage(), "conversation", base).map_err(|error| error.to_string())
    }

    pub(crate) fn failed(&self, error: impl std::fmt::Display) {
        *self.failure.lock().unwrap_or_else(|p| p.into_inner()) = Some(error.to_string());
    }
    pub(crate) fn snapshot(&self) -> Vec<Message> {
        match self.native() {
            Ok(conversation) => conversation
                .messages()
                .iter()
                .map(|message| Message::from(message.as_ref()))
                .collect(),
            Err(error) => {
                self.failed(error);
                Vec::new()
            }
        }
    }
    pub(crate) fn len(&self) -> usize {
        self.snapshot().len()
    }
    pub(crate) fn push(&self, message: Message) {
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut conversation = self.native()?;
            conversation.extend([rig_core::message::Message::try_from(&message)?]);
            rig_memory::save(&self.storage(), "conversation", &mut conversation)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.failed(error);
        }
        self.publish();
    }
    pub(crate) fn replace_with(&self, messages: Vec<Message>) {
        self.projections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clear();
        let result = (|| -> Result<(), Box<dyn std::error::Error>> {
            let mut conversation = self.native()?;
            let converted = messages
                .iter()
                .map(rig_core::message::Message::try_from)
                .collect::<Result<Vec<_>, _>>()?;
            conversation.splice(
                ..,
                converted
                    .into_iter()
                    .map(rig_core::conversation::Entry::new),
                rig_core::conversation::Author::plugin("solaris"),
            );
            rig_memory::save(&self.storage(), "conversation", &mut conversation)?;
            Ok(())
        })();
        if let Err(error) = result {
            self.failed(error);
        }
        self.publish();
    }
    pub(crate) fn publish_projection(
        &self,
        messages: &[Arc<rig_core::message::Message>],
        header: &crate::projection::Header,
        sent: bool,
    ) -> solaris_backend::ContextUsage {
        let mut projections = self.projections.lock().unwrap_or_else(|p| p.into_inner());
        let projection = projections.entry(header.base_id.clone()).or_default();
        projection.sync(messages);
        let context = projection.context(header, sent);
        self.published
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .insert(header.base_id.clone(), context);
        context
    }
    pub(crate) fn observe_usage(
        &self,
        header: &crate::projection::Header,
        usage: solaris_backend::Usage,
    ) {
        if let Some(projection) = self
            .projections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get_mut(&header.base_id)
        {
            projection.observe(usage);
        }
    }
    pub(crate) fn refresh(&self) {
        self.publish();
    }

    /// Forget the conversation, for `/clear`.
    ///
    /// The next turn starts with nothing but its own prompt and a fresh system
    /// prompt, so nothing of the old conversation reaches the model again.
    pub(crate) fn clear(&self) {
        self.replace_with(Vec::new());
        self.persist(SessionRecord::Context {
            messages: Vec::new(),
        });
        *self.billing.lock().unwrap_or_else(|p| p.into_inner()) = Default::default();
    }

    /// Complete missing results in the latest assistant round without replaying
    /// tools. Older rounds may reuse the same call IDs, so only results after
    /// this assistant message count as answers.
    pub(crate) fn repair(&self) {
        let messages = self.snapshot();
        let Some(round) = messages
            .iter()
            .rposition(|message| message.role == Role::Assistant)
        else {
            return;
        };
        let answered: std::collections::HashSet<_> = messages[round + 1..]
            .iter()
            .flat_map(|message| message.tool_results())
            .map(|result| result.id.as_str())
            .collect();
        let missing: Vec<_> = messages[round]
            .tool_calls()
            .filter(|call| !answered.contains(call.id.as_str()))
            .cloned()
            .collect();
        for call in missing {
            self.push(Message::tool_result(solaris_backend::ToolResult::error(
                &call,
                "interrupted; execution outcome is unknown; verify side effects before retrying",
            )));
        }
    }

    pub(crate) fn record_billing(
        &self,
        usage: solaris_backend::Usage,
        cost_usd: f64,
        kind: crate::UsageKind,
    ) -> bool {
        if usage.is_empty() && cost_usd == 0.0 {
            return true;
        }
        self.billing
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .add(usage, cost_usd);
        self.persist(SessionRecord::Usage {
            usage,
            cost_usd,
            kind,
        })
    }

    pub(crate) fn usage_totals(&self) -> Option<crate::UsageTotals> {
        self.billing.try_lock().ok().map(|totals| *totals)
    }

    pub(crate) fn set_headers(&self, headers: Vec<crate::projection::Header>) {
        self.projections
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|key, _| headers.iter().any(|header| &header.base_id == key));
        *self.headers.lock().unwrap_or_else(|p| p.into_inner()) = headers;
        self.publish();
    }

    pub(crate) fn update_request_header(&self, header: crate::projection::Header) {
        {
            let mut headers = self.headers.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(current) = headers
                .iter_mut()
                .find(|current| current.base_id == header.base_id)
            {
                if current.request_id == header.request_id {
                    return;
                }
                *current = header;
            } else {
                headers.push(header);
            }
        }
        self.publish();
    }

    fn publish(&self) {
        let native = match self.native() {
            Ok(native) => native,
            Err(error) => {
                self.failed(error);
                return;
            }
        };
        let headers = self.headers.lock().unwrap_or_else(|p| p.into_inner());
        let mut projections = self.projections.lock().unwrap_or_else(|p| p.into_inner());
        let snapshots = headers
            .iter()
            .map(|header| {
                let projection = projections.entry(header.base_id.clone()).or_default();
                if !self.projecting.load(Ordering::Acquire) || !projection.initialized() {
                    projection.sync(native.messages());
                }
                (header.base_id.clone(), projection.context(header, false))
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
        self.update_request_header(header.clone());
        self.context_usage(&header.base_id, 0).tokens.unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use solaris_backend::{ToolCall, ToolResult};

    fn tokens(conversation: &Conversation) -> u32 {
        let header = crate::projection::Header::new("", "", &[]);
        conversation.projected_tokens(&header)
    }

    fn call(id: &str) -> ToolCall {
        ToolCall::new(id, "read", json!({ "path": "a.txt" }))
    }

    #[test]
    fn a_phase_checkpoint_refuses_a_concurrent_writer() {
        let conversation = Conversation::new();
        conversation.push(Message::user("original"));
        let mut base = conversation.native().unwrap();
        let mut checkpoint = base
            .messages()
            .iter()
            .map(|message| message.as_ref().clone())
            .collect::<Vec<_>>();
        checkpoint.push(rig_core::message::Message::assistant("completed phase"));
        conversation.push(Message::user("concurrent input"));
        assert!(conversation.commit_phase(&mut base, checkpoint).is_err());
        let messages = conversation.snapshot();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].text(), "concurrent input");
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
        assert_eq!(tokens(&conversation), 0);
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
        let small = tokens(&conversation);

        conversation.push(Message::assistant("y".repeat(400)));
        assert!(tokens(&conversation) > small);
    }
    #[test]
    fn tool_changes_reestimate_the_same_projection_for_ui_and_budget() {
        let conversation = Conversation::new();
        let initial = crate::projection::Header::new("model", "system", &[]);
        conversation.set_headers(vec![initial.clone()]);
        conversation.publish_projection(&[], &initial, true);
        conversation.observe_usage(&initial, solaris_backend::Usage::new(9000, 100));
        conversation.push(Message::assistant("previous response"));
        for schema in [r#"{"type":"string"}"#, r#"{"type":"number"}"#, "null"] {
            let tools = if schema == "null" {
                vec![]
            } else {
                vec![solaris_backend::ToolSpec {
                    name: "lookup".into(),
                    description: "Find an item".into(),
                    parameters: serde_json::from_str(schema).expect("schema"),
                }]
            };
            let mut header = crate::projection::Header::new("model", "system", &tools);
            header.base_id.clone_from(&initial.base_id);
            conversation.update_request_header(header.clone());
            let display = conversation.context_usage(&initial.base_id, 10000);
            assert_eq!(display.tokens, Some(conversation.projected_tokens(&header)));
            assert!(display.estimated);
            assert!(display.tokens.expect("estimate") < 9000);
            conversation.publish_projection(
                conversation.native().unwrap().messages(),
                &header,
                true,
            );
            conversation.observe_usage(&header, solaris_backend::Usage::new(200, 20));
            conversation.push(Message::assistant("new response"));
            let display = conversation.context_usage(&initial.base_id, 10000);
            assert_eq!(display.tokens, Some(220));
            assert!(!display.estimated);
        }
    }

    #[test]
    fn changing_tools_during_a_run_uses_sdk_working_messages_not_saved_memory() {
        let conversation = Conversation::new();
        let initial = crate::projection::Header::new("model", "system", &[]);
        conversation.set_headers(vec![initial.clone()]);
        let guard = conversation.projecting();
        let working = vec![Arc::new(rig_core::message::Message::user(
            "x".repeat(20_000),
        ))];
        conversation.publish_projection(&working, &initial, true);
        let tools = [solaris_backend::ToolSpec::new(
            "read",
            "Read",
            json!({"type":"object"}),
        )];
        let mut changed = crate::projection::Header::new("model", "system", &tools);
        changed.base_id.clone_from(&initial.base_id);
        conversation.update_request_header(changed.clone());
        let live = conversation.context_usage(&initial.base_id, 10000);
        assert!(live.estimated && live.tokens.unwrap() > 1000);
        assert_eq!(live.tokens, Some(conversation.projected_tokens(&changed)));
        assert_eq!(
            conversation.len(),
            0,
            "a projection does not author memory messages"
        );
        drop(guard);
        assert!(
            conversation
                .context_usage(&initial.base_id, 10000)
                .tokens
                .unwrap()
                < 1000,
            "cancellation restores the saved view"
        );
    }

    #[test]
    fn ui_reads_published_projection_without_waiting_for_message_lock() {
        let conversation = Arc::new(Conversation::new());
        let header = crate::projection::Header::new("model", "system", &[]);
        let base = header.base_id.clone();
        conversation.set_headers(vec![header.clone()]);
        conversation.publish_projection(&[], &header, true);
        conversation.observe_usage(&header, solaris_backend::Usage::new(100, 10));
        conversation.push(Message::assistant("reply"));
        let lock = conversation.storage.write().unwrap();
        let reader = Arc::clone(&conversation);
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(reader.context_usage(&base, 1000)).expect("send");
        });
        let result = rx.recv_timeout(std::time::Duration::from_secs(1));
        drop(lock);
        thread.join().expect("reader");
        assert_eq!(result.expect("nonblocking snapshot").tokens, Some(110));
    }
}
