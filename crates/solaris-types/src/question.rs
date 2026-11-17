//! Serializable user questions, independent of a particular client.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub id: String,
    pub label: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub header: String,
    pub question: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multiple: bool,
    #[serde(default = "yes")]
    pub allow_custom: bool,
}
fn yes() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionAnswer {
    pub question_id: String,
    pub selected: Vec<String>,
    pub custom: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QuestionRequest {
    pub request_id: String,
    pub session_id: String,
    pub tool_call_id: String,
    pub questions: Vec<Question>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum QuestionResponse {
    Answered { answers: Vec<QuestionAnswer> },
    Dismissed,
}
