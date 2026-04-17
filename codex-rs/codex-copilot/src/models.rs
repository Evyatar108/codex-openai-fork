use serde_json::Value;

pub fn filter_models_response(mut value: Value) -> Value {
    let Some(data) = value.get_mut("data").and_then(Value::as_array_mut) else {
        return value;
    };

    data.retain(|model| {
        model
            .get("model_picker_enabled")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            || model
                .pointer("/capabilities/type")
                .and_then(Value::as_str)
                .is_some_and(|kind| kind == "embeddings")
    });
    value
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;

    use super::filter_models_response;

    #[test]
    fn filters_to_model_picker_and_embeddings_models() {
        let filtered = filter_models_response(json!({
            "object": "list",
            "data": [
                {"id": "keep-1", "model_picker_enabled": true, "capabilities": {"type": "chat"}},
                {"id": "keep-2", "model_picker_enabled": false, "capabilities": {"type": "embeddings"}},
                {"id": "drop-1", "model_picker_enabled": false, "capabilities": {"type": "chat"}}
            ]
        }));

        assert_eq!(
            filtered
                .get("data")
                .and_then(Value::as_array)
                .map(std::vec::Vec::len),
            Some(2),
        );
    }
}
