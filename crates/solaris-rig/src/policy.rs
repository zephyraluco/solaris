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

pub(crate) fn apply_capabilities(
    body: &mut Value,
    capabilities: &solaris_backend::ModelCapabilities,
) {
    if capabilities.streaming_usage == solaris_backend::Support::Unsupported
        && let Some(body) = body.as_object_mut()
    {
        body.remove("stream_options");
    }
    if capabilities.prompt_caching == solaris_backend::Support::Unsupported {
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
    if capabilities.structured_output == solaris_backend::Support::Supported
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
        apply_capabilities(&mut body, &solaris_backend::ModelCapabilities::default());
        assert!(body["tools"][0]["function"].get("strict").is_none());
        apply_capabilities(
            &mut body,
            &solaris_backend::ModelCapabilities {
                structured_output: solaris_backend::Support::Supported,
                streaming_usage: solaris_backend::Support::Unsupported,
                ..Default::default()
            },
        );
        assert_eq!(body["tools"][0]["function"]["strict"], true);
        assert!(body.get("stream_options").is_none());
    }
}
