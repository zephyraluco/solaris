//! Session-scoped pending questions. Client replies never execute model code.
use crate::AgentEvent;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use solaris_tools::{Cancel, ExecutionMode, Tool, ToolContext, ToolOutput};
use solaris_types::{Question, QuestionRequest, QuestionResponse, ToolCall};
use std::{
    collections::{BTreeMap, HashSet},
    sync::{Arc, Mutex},
};
use tokio::sync::{mpsc::UnboundedSender, oneshot};

/// Pending user interaction channel, supplied only by clients able to answer.
/// Clone within one session; each newly assembled session receives its own channel.
#[derive(Clone, Default)]
pub struct Questions(Arc<Mutex<BTreeMap<String, Pending>>>);
struct Pending {
    request: QuestionRequest,
    reply: oneshot::Sender<QuestionResponse>,
    cancel: Cancel,
}
impl Pending {
    fn is_available_to(&self, session: &str) -> bool {
        self.request.session_id == session && !self.cancel.is_cancelled()
    }
}

struct Registration {
    questions: Questions,
    id: String,
}
impl Drop for Registration {
    fn drop(&mut self) {
        self.questions
            .0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&self.id);
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    questions: Vec<Question>,
}

impl Questions {
    pub(crate) fn contains(&self, session: &str, id: &str) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(id)
            .is_some_and(|entry| entry.is_available_to(session))
    }

    pub(crate) fn pending(&self, session: &str) -> Vec<QuestionRequest> {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .values()
            .filter(|entry| entry.is_available_to(session))
            .map(|p| p.request.clone())
            .collect()
    }
    pub(crate) fn reply(
        &self,
        session: &str,
        id: &str,
        response: QuestionResponse,
    ) -> Result<(), String> {
        let mut pending = self.0.lock().unwrap_or_else(|p| p.into_inner());
        let entry = pending.get(id).ok_or("question is no longer pending")?;
        if !entry.is_available_to(session) {
            return Err("question is no longer pending in this session".into());
        }
        validate_response(&entry.request.questions, &response)?;
        let entry = pending.remove(id).expect("validated pending request");
        entry
            .reply
            .send(response)
            .map_err(|_| "question is no longer pending".into())
    }
    pub(crate) async fn ask(
        &self,
        session: &str,
        call: &ToolCall,
        cancel: &Cancel,
        tx: &UnboundedSender<AgentEvent>,
    ) -> ToolOutput {
        let input: Input = match serde_json::from_value(call.input.clone()) {
            Ok(input) => input,
            Err(error) => return ToolOutput::error(error.to_string()),
        };
        if let Err(error) = validate_questions(&input.questions) {
            return ToolOutput::error(error);
        }
        let request = QuestionRequest {
            request_id: crate::identity::new_id(),
            session_id: session.into(),
            tool_call_id: call.id.clone(),
            questions: input.questions,
        };
        let (reply, answer) = oneshot::channel();
        let id = request.request_id.clone();
        self.0.lock().unwrap_or_else(|p| p.into_inner()).insert(
            id.clone(),
            Pending {
                request: request.clone(),
                reply,
                cancel: cancel.clone(),
            },
        );
        let _guard = Registration {
            questions: self.clone(),
            id: id.clone(),
        };
        let questions = request.questions.clone();
        if tx.send(AgentEvent::QuestionRequested(request)).is_err() {
            return ToolOutput::error("question client disconnected");
        }
        let response = tokio::select! {
            biased;
            _ = crate::execution::wait_for_cancel(cancel) => return ToolOutput::error("question cancelled"),
            result = answer => match result { Ok(answer)=>answer, Err(_)=> return ToolOutput::error("question cancelled") },
            _ = tx.closed() => return ToolOutput::error("question client disconnected"),
        };
        let _ = tx.send(AgentEvent::QuestionResolved {
            request_id: id,
            response: response.clone(),
        });
        let mut output =
            ToolOutput::ok(serde_json::to_string(&response).expect("question response serializes"));
        output.metadata.details = json!({"question":{"questions":questions,"response":response}});
        output
    }
}
fn validate_questions(questions: &[Question]) -> Result<(), String> {
    if questions.is_empty() || questions.len() > 8 {
        return Err("provide between 1 and 8 questions".into());
    }
    let mut ids = HashSet::new();
    for q in questions {
        if q.id.trim().is_empty()
            || !ids.insert(&q.id)
            || q.question.trim().is_empty()
            || q.header.trim().is_empty()
        {
            return Err("questions require unique nonempty IDs, headers and text".into());
        }
        if q.options.len() > 20 || (q.options.is_empty() && !q.allow_custom) {
            return Err("provide at most 20 options and at least one answer method".into());
        }
        let mut options = HashSet::new();
        for o in &q.options {
            if o.id.trim().is_empty() || o.label.trim().is_empty() || !options.insert(&o.id) {
                return Err("options require unique nonempty IDs and labels".into());
            }
        }
    }
    Ok(())
}
fn validate_response(questions: &[Question], response: &QuestionResponse) -> Result<(), String> {
    let QuestionResponse::Answered { answers } = response else {
        return Ok(());
    };
    if answers.len() != questions.len() {
        return Err("answer every question exactly once".into());
    }
    let mut ids = HashSet::new();
    for answer in answers {
        if !ids.insert(&answer.question_id) {
            return Err("duplicate question answer".into());
        }
        let q = questions
            .iter()
            .find(|q| q.id == answer.question_id)
            .ok_or("unknown question ID")?;
        let mut selected = HashSet::new();
        for id in &answer.selected {
            if !selected.insert(id) || !q.options.iter().any(|o| &o.id == id) {
                return Err("unknown or duplicate option ID".into());
            }
        }
        let custom = answer.custom.as_ref().is_some_and(|s| !s.trim().is_empty());
        if answer.custom.is_some() && (!custom || !q.allow_custom) {
            return Err("custom answer is empty or unavailable".into());
        }
        let count = selected.len() + usize::from(custom);
        if count == 0 || (!q.multiple && count != 1) {
            return Err("invalid number of selected answers".into());
        }
    }
    Ok(())
}

pub(crate) struct QuestionTool;
#[async_trait]
impl Tool for QuestionTool {
    fn name(&self) -> &str {
        "question"
    }
    fn description(&self) -> &str {
        "Ask the user for a preference, clarification or decision and wait for their answer. Supply concise options with stable IDs. The UI adds custom input when allow_custom is true; do not add an Other option. Dismissal is not approval. Do not repeat a dismissed question without a reason."
    }
    fn parameters(&self) -> Value {
        json!({"type":"object","additionalProperties":false,"required":["questions"],"properties":{"questions":{"type":"array","minItems":1,"maxItems":8,"items":{"type":"object","additionalProperties":false,"required":["id","header","question","options"],"properties":{"id":{"type":"string"},"header":{"type":"string"},"question":{"type":"string"},"multiple":{"type":"boolean"},"allow_custom":{"type":"boolean","default":true},"options":{"type":"array","maxItems":20,"items":{"type":"object","additionalProperties":false,"required":["id","label","description"],"properties":{"id":{"type":"string"},"label":{"type":"string"},"description":{"type":"string"}}}}}}}}})
    }
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Sequential
    }
    fn is_read_only(&self) -> bool {
        true
    }
    async fn run(&self, _: Value, _: &ToolContext) -> ToolOutput {
        ToolOutput::error("question requires an interactive session")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solaris_types::{QuestionAnswer, QuestionOption};
    fn question(multiple: bool, allow_custom: bool) -> Question {
        Question {
            id: "q".into(),
            header: "Choice".into(),
            question: "Choose".into(),
            multiple,
            allow_custom,
            options: vec![
                QuestionOption {
                    id: "a".into(),
                    label: "A".into(),
                    description: "First".into(),
                },
                QuestionOption {
                    id: "b".into(),
                    label: "B".into(),
                    description: "Second".into(),
                },
            ],
        }
    }
    fn response(ids: &[&str], custom: Option<&str>) -> QuestionResponse {
        QuestionResponse::Answered {
            answers: vec![QuestionAnswer {
                question_id: "q".into(),
                selected: ids.iter().map(|s| s.to_string()).collect(),
                custom: custom.map(str::to_string),
            }],
        }
    }
    #[test]
    fn validates_single_multiple_custom_and_dismissal_without_consuming_request() {
        for (multiple, custom, ids, text, valid) in [
            (false, false, vec!["a"], None, true),
            (false, false, vec!["a", "b"], None, false),
            (true, false, vec!["a", "b"], None, true),
            (true, false, vec!["a", "a"], None, false),
            (true, false, vec!["unknown"], None, false),
            (false, true, vec![], Some("free text"), true),
            (false, true, vec!["a"], Some("free text"), false),
            (true, true, vec!["a"], Some("free text"), true),
            (true, false, vec![], Some("free text"), false),
            (true, true, vec![], Some("  "), false),
            (true, true, vec![], None, false),
        ] {
            assert_eq!(
                validate_response(&[question(multiple, custom)], &response(&ids, text)).is_ok(),
                valid
            );
        }
        assert!(validate_response(&[question(false, false)], &QuestionResponse::Dismissed).is_ok());
        let mut answer = response(&["a"], None);
        if let QuestionResponse::Answered { answers } = &mut answer {
            answers[0].question_id = "unknown".into();
        }
        assert!(validate_response(&[question(false, false)], &answer).is_err());
    }
    #[test]
    fn rejects_ambiguous_questions_and_allows_free_text_only() {
        let q = question(false, true);
        assert!(validate_questions(&[]).is_err());
        assert!(validate_questions(&[q.clone(), q.clone()]).is_err());
        let mut duplicate = q.clone();
        duplicate.options.push(duplicate.options[0].clone());
        assert!(validate_questions(&[duplicate]).is_err());
        let mut free = q;
        free.options.clear();
        assert!(validate_questions(&[free.clone()]).is_ok());
        free.allow_custom = false;
        assert!(validate_questions(&[free]).is_err());
    }
    #[tokio::test]
    async fn dropping_a_waiting_tool_unregisters_it() {
        let service = Questions::default();
        let call = ToolCall::new(
            "call",
            "question",
            json!({"questions":[question(false,true)]}),
        );
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let cancel = Cancel::new();
        let task = {
            let service = service.clone();
            tokio::spawn(async move { service.ask("session", &call, &cancel, &tx).await })
        };
        assert!(matches!(
            rx.recv().await,
            Some(AgentEvent::QuestionRequested(_))
        ));
        assert_eq!(service.pending("session").len(), 1);
        task.abort();
        let _ = task.await;
        assert!(service.pending("session").is_empty());
    }
}
