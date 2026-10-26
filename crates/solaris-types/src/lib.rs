//! Provider-independent metadata. Model-facing text and UI details stay separate.
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;

/// Final state of an execution, independent of its display text.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    #[default]
    Success,
    Failed,
    Cancelled,
    TimedOut,
    Interrupted,
}

/// An output retained outside the model context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artifact {
    pub path: PathBuf,
    pub media_type: String,
}

/// Why the returned text is only part of the result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TruncationInfo {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub kept: String,
}

/// Structured execution data preserved in events and session journals.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolMetadata {
    #[serde(default)]
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub details: Value,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<Artifact>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncation: Option<TruncationInfo>,
}
