use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Initiator {
    Agent,
    User,
}

impl Initiator {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Agent => "agent",
            Self::User => "user",
        }
    }
}

pub fn is_probe_request(payload: &Value) -> bool {
    let has_input = payload
        .get("input")
        .map(|value| match value {
            Value::Array(items) => !items.is_empty(),
            Value::Null => false,
            Value::String(text) => !text.is_empty(),
            _ => true,
        })
        .unwrap_or(false);
    let has_previous_response = payload
        .get("previous_response_id")
        .and_then(Value::as_str)
        .is_some();
    !has_input && !has_previous_response
}

pub fn synthetic_empty_response(model: Option<&str>) -> Value {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    serde_json::json!({
        "id": format!("resp_probe_{millis}"),
        "object": "response",
        "status": "completed",
        "output": [],
        "output_text": "",
        "model": model.unwrap_or("unknown"),
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0,
            "total_tokens": 0,
        },
    })
}

pub fn normalize_payload(payload: &mut Value) {
    if let Some(object) = payload.as_object_mut() {
        object.remove("previous_response_id");
        object.insert("service_tier".to_string(), Value::Null);
    }
    replace_apply_patch_tool(payload);
    remove_web_search_tool(payload);
    compact_input_by_latest_compaction(payload);
}

pub fn request_vision(payload: &Value) -> bool {
    payload
        .get("input")
        .and_then(Value::as_array)
        .is_some_and(|items| items.iter().any(contains_vision_content))
}

pub fn request_initiator(payload: &Value) -> Initiator {
    let Some(last_item) = payload
        .get("input")
        .and_then(Value::as_array)
        .and_then(|items| items.last())
    else {
        return Initiator::User;
    };

    let Some(role) = last_item.get("role") else {
        return Initiator::Agent;
    };

    match role.as_str().map(str::to_ascii_lowercase).as_deref() {
        Some("assistant") => Initiator::Agent,
        _ => Initiator::User,
    }
}

pub fn compact_input_by_latest_compaction(payload: &mut Value) {
    let Some(input) = payload.get_mut("input").and_then(Value::as_array_mut) else {
        return;
    };

    let latest_compaction_index = input.iter().rposition(|item| {
        item.get("type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind == "compaction")
    });

    if let Some(index) = latest_compaction_index {
        let compacted = input.split_off(index);
        *input = compacted;
    }
}

fn remove_web_search_tool(payload: &mut Value) {
    let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    tools.retain(|tool| tool.get("type").and_then(Value::as_str) != Some("web_search"));
}

fn replace_apply_patch_tool(payload: &mut Value) {
    let Some(tools) = payload.get_mut("tools").and_then(Value::as_array_mut) else {
        return;
    };

    for tool in tools {
        let is_apply_patch = tool.get("type").and_then(Value::as_str) == Some("custom")
            && tool.get("name").and_then(Value::as_str) == Some("apply_patch");
        if is_apply_patch {
            *tool = serde_json::json!({
                "type": "function",
                "name": "apply_patch",
                "description": "Use the `apply_patch` tool to edit files",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "input": {
                            "type": "string",
                            "description": "The entire contents of the apply_patch command"
                        }
                    },
                    "required": ["input"]
                },
                "strict": false
            });
        }
    }
}

fn contains_vision_content(value: &Value) -> bool {
    match value {
        Value::Array(values) => values.iter().any(contains_vision_content),
        Value::Object(object) => {
            if object.get("type").and_then(Value::as_str) == Some("input_image") {
                return true;
            }
            object
                .get("content")
                .and_then(Value::as_array)
                .is_some_and(|content| content.iter().any(contains_vision_content))
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;

    use super::Initiator;
    use super::compact_input_by_latest_compaction;
    use super::is_probe_request;
    use super::normalize_payload;
    use super::request_initiator;
    use super::request_vision;

    #[test]
    fn probe_requests_require_neither_input_nor_previous_response() {
        assert!(is_probe_request(&json!({"model": "gpt-5.4"})));
        assert!(!is_probe_request(&json!({"input": "hello"})));
        assert!(!is_probe_request(
            &json!({"previous_response_id": "resp-1"})
        ));
    }

    #[test]
    fn normalize_payload_applies_codex_specific_transforms() {
        let mut payload = json!({
            "previous_response_id": "resp-1",
            "tools": [
                {"type": "web_search", "name": "search"},
                {"type": "custom", "name": "apply_patch"}
            ],
            "input": [
                {"type": "message", "role": "user", "content": "old"},
                {"type": "compaction", "id": "cmp-1", "encrypted_content": "abc"},
                {"type": "message", "role": "user", "content": "new"}
            ]
        });

        normalize_payload(&mut payload);

        assert_eq!(payload.get("previous_response_id"), None);
        assert_eq!(payload.get("service_tier"), Some(&Value::Null));
        assert_eq!(
            payload
                .get("tools")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len),
            Some(1),
        );
        assert_eq!(
            payload.pointer("/tools/0/type").and_then(Value::as_str),
            Some("function"),
        );
        assert_eq!(
            payload
                .get("input")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len),
            Some(2),
        );
    }

    #[test]
    fn compact_input_keeps_latest_compaction_suffix() {
        let mut payload = json!({
            "input": [
                {"type": "message", "role": "user"},
                {"type": "compaction", "id": "cmp-1"},
                {"type": "message", "role": "assistant"},
                {"type": "compaction", "id": "cmp-2"},
                {"type": "message", "role": "user"}
            ]
        });

        compact_input_by_latest_compaction(&mut payload);

        assert_eq!(
            payload.pointer("/input/0/id").and_then(Value::as_str),
            Some("cmp-2"),
        );
        assert_eq!(
            payload
                .get("input")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len),
            Some(2),
        );
    }

    #[test]
    fn request_context_detects_agent_turns_and_vision() {
        let payload = json!({
            "input": [
                {"type": "message", "role": "assistant", "content": [{"type": "input_image", "image_url": "file://image.png"}]}
            ]
        });

        assert_eq!(request_initiator(&payload), Initiator::Agent);
        assert!(request_vision(&payload));
    }
}
