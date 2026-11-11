//! Solaris model capability policy applied to Rig-generated request bodies.
use serde_json::Value;
fn strict_schema(schema: &Value) -> bool {
    if schema["type"] != "object" {
        return false;
    }
    let Some(properties) = schema["properties"].as_object() else {
        return false;
    };
    let Some(required) = schema["required"].as_array() else {
        return false;
    };
    schema["additionalProperties"] == false
        && properties
            .keys()
            .all(|name| required.iter().any(|value| value.as_str() == Some(name)))
        && properties.values().all(strict_member)
}
fn strict_member(schema: &Value) -> bool {
    if schema.get("$ref").is_some() || schema.get("oneOf").is_some() {
        return false;
    }
    if let Some(variants) = schema["anyOf"].as_array() {
        return variants.iter().all(strict_member);
    }
    match schema["type"].as_str() {
        Some("object") => strict_schema(schema),
        Some("array") => schema.get("items").is_some_and(strict_member),
        Some("string" | "number" | "integer" | "boolean" | "null") => true,
        _ => false,
    }
}

pub(crate) fn apply_capabilities(body: &mut Value, capabilities: &crate::ModelCapabilities) {
    if capabilities.streaming_usage == crate::Support::Unsupported
        && let Some(body) = body.as_object_mut()
    {
        body.remove("stream_options");
    }
    if capabilities.prompt_caching == crate::Support::Unsupported {
        if let Some(body) = body.as_object_mut() {
            body.remove("prompt_cache_key");
        }
        if let Some(system) = body.get_mut("system").and_then(Value::as_array_mut) {
            for block in system {
                if let Some(block) = block.as_object_mut() {
                    block.remove("cache_control");
                }
            }
        }
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if let Some(blocks) = message["content"].as_array_mut() {
                    for block in blocks {
                        if let Some(block) = block.as_object_mut() {
                            block.remove("cache_control");
                        }
                    }
                }
            }
        }
    }
    if capabilities.structured_output == crate::Support::Supported
        && let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut)
    {
        for tool in tools {
            let definition = if tool.get("function").is_some() {
                &mut tool["function"]
            } else {
                tool
            };
            let schema = definition
                .get("parameters")
                .or_else(|| definition.get("input_schema"));
            if schema.is_some_and(strict_schema) {
                definition["strict"] = Value::Bool(true);
            }
        }
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    #[test]
    fn strict_sampling_is_only_enabled_for_explicit_support_and_compatible_schemas() {
        let mut body = serde_json::json!({"tools":[{"type":"function","function":{"name":"test","parameters":{"type":"object","properties":{"value":{"type":"string"}},"required":["value"],"additionalProperties":false}}}],"stream_options":{"include_usage":true}});
        apply_capabilities(&mut body, &crate::ModelCapabilities::default());
        assert!(body["tools"][0]["function"].get("strict").is_none());
        apply_capabilities(
            &mut body,
            &crate::ModelCapabilities {
                structured_output: crate::Support::Supported,
                streaming_usage: crate::Support::Unsupported,
                ..Default::default()
            },
        );
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert!(body.get("stream_options").is_none());
    }
}

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
