//! Compatibility repairs for explicit gateway rejections before output.
use serde_json::Value;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Repair {
    /// Drop `stream_options.include_usage`, which older servers reject as an
    /// unknown field. The cost is losing the reported token counts.
    DropStreamOptions,
    /// Rename `max_tokens` to `max_completion_tokens`, which the reasoning
    /// models require.
    UseMaxCompletionTokens,
}

/// The repair a rejected request is asking for, if it names a field we know.
pub fn repair_for(body: &str) -> Option<Repair> {
    if body.contains("stream_options") {
        return Some(Repair::DropStreamOptions);
    }
    if body.contains("max_completion_tokens") {
        return Some(Repair::UseMaxCompletionTokens);
    }
    None
}

/// Apply `repair` to a request body.
pub fn apply_repair(body: &mut Value, repair: Repair) {
    match repair {
        Repair::DropStreamOptions => {
            let Some(object) = body.as_object_mut() else {
                return;
            };
            object.remove("stream_options");
        }
        Repair::UseMaxCompletionTokens => {
            let Some(object) = body.as_object_mut() else {
                return;
            };
            let Some(max_tokens) = object.remove("max_tokens") else {
                return;
            };
            object.insert("max_completion_tokens".to_string(), max_tokens);
        }
    }
}

#[cfg(test)]
mod repair_tests {
    use super::{Repair, apply_repair, repair_for};
    use serde_json::json;

    #[test]
    fn token_budget_repair_preserves_the_budget_and_other_fields() {
        let mut body = json!({"max_tokens":8192,"model":"custom","messages":[]});
        apply_repair(&mut body, Repair::UseMaxCompletionTokens);
        assert_eq!(
            body,
            json!({"max_completion_tokens":8192,"model":"custom","messages":[]})
        );
        apply_repair(&mut body, Repair::UseMaxCompletionTokens);
        assert_eq!(body["max_completion_tokens"], 8192);
        assert_eq!(
            repair_for("use max_completion_tokens"),
            Some(Repair::UseMaxCompletionTokens)
        );
        assert_eq!(repair_for("invalid credentials"), None);
    }
}
