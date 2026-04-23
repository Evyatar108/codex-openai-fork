use super::*;
use crate::ModelClient;
use codex_copilot::CopilotAuth;
use codex_copilot::paths::AppPaths;
use codex_model_provider_info::create_copilot_provider;
use codex_otel::SessionTelemetry;
use codex_protocol::ThreadId;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::protocol::SessionSource;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::tempdir;
use wiremock::MockServer;

#[test]
fn normalize_trace_items_handles_payload_wrapper_and_message_role_filtering() {
    let items = vec![
        serde_json::json!({
            "type": "response_item",
            "payload": {"type": "message", "role": "assistant", "content": []}
        }),
        serde_json::json!({
            "type": "response_item",
            "payload": [
                {"type": "message", "role": "user", "content": []},
                {"type": "message", "role": "tool", "content": []},
                {"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"}
            ]
        }),
        serde_json::json!({
            "type": "not_response_item",
            "payload": {"type": "message", "role": "assistant", "content": []}
        }),
        serde_json::json!({
            "type": "message",
            "role": "developer",
            "content": []
        }),
    ];

    let normalized = normalize_trace_items(items, Path::new("trace.json")).expect("normalize");
    let expected = vec![
        serde_json::json!({"type": "message", "role": "assistant", "content": []}),
        serde_json::json!({"type": "message", "role": "user", "content": []}),
        serde_json::json!({"type": "function_call", "name": "shell", "arguments": "{}", "call_id": "c1"}),
        serde_json::json!({"type": "message", "role": "developer", "content": []}),
    ];
    assert_eq!(normalized, expected);
}

#[test]
fn load_trace_items_supports_jsonl_arrays_and_objects() {
    let text = r#"
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[]}}
[{"type":"message","role":"user","content":[]},{"type":"message","role":"tool","content":[]}]
"#;
    let loaded = load_trace_items(Path::new("trace.jsonl"), text).expect("load");
    let expected = vec![
        serde_json::json!({"type":"message","role":"assistant","content":[]}),
        serde_json::json!({"type":"message","role":"user","content":[]}),
    ];
    assert_eq!(loaded, expected);
}

#[tokio::test]
async fn load_trace_text_decodes_utf8_sig() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("trace.json");
    tokio::fs::write(
        &path,
        [
            0xEF, 0xBB, 0xBF, b'[', b'{', b'"', b't', b'y', b'p', b'e', b'"', b':', b'"', b'm',
            b'e', b's', b's', b'a', b'g', b'e', b'"', b',', b'"', b'r', b'o', b'l', b'e', b'"',
            b':', b'"', b'u', b's', b'e', b'r', b'"', b',', b'"', b'c', b'o', b'n', b't', b'e',
            b'n', b't', b'"', b':', b'[', b']', b'}', b']',
        ],
    )
    .await
    .expect("write");

    let text = load_trace_text(&path).await.expect("decode");
    assert!(text.starts_with('['));
}

fn test_model_info() -> ModelInfo {
    serde_json::from_value(serde_json::json!({
        "slug": "gpt-5.4",
        "display_name": "gpt-5.4",
        "description": "desc",
        "default_reasoning_level": "medium",
        "supported_reasoning_levels": [
            {"effort": "medium", "description": "medium"}
        ],
        "shell_type": "shell_command",
        "visibility": "list",
        "supported_in_api": true,
        "priority": 1,
        "upgrade": null,
        "base_instructions": "base instructions",
        "model_messages": null,
        "supports_reasoning_summaries": false,
        "support_verbosity": false,
        "default_verbosity": null,
        "apply_patch_tool_type": null,
        "truncation_policy": {"mode": "bytes", "limit": 10000},
        "supports_parallel_tool_calls": false,
        "supports_image_detail_original": false,
        "context_window": 272000,
        "auto_compact_token_limit": null,
        "experimental_supported_tools": []
    }))
    .expect("deserialize test model info")
}

fn test_session_telemetry() -> SessionTelemetry {
    SessionTelemetry::new(
        ThreadId::new(),
        "gpt-5.4",
        "gpt-5.4",
        /*account_id*/ None,
        /*account_email*/ None,
        /*auth_mode*/ None,
        "test-originator".to_string(),
        /*log_user_prompts*/ false,
        "test-terminal".to_string(),
        SessionSource::Cli,
    )
}

fn test_copilot_auth(server: &MockServer) -> (tempfile::TempDir, Arc<CopilotAuth>) {
    let temp = tempdir().expect("temp dir");
    let app_dir = temp.path().join("copilot-home");
    std::fs::create_dir_all(&app_dir).expect("create app dir");
    std::fs::write(app_dir.join("github_token"), "github-token\n").expect("write github token");

    let paths = AppPaths {
        app_dir: app_dir.clone(),
        github_token_path: app_dir.join("github_token"),
        copilot_token_path: app_dir.join("copilot_token"),
        device_id_path: app_dir.join("device_id"),
        machine_id_path: app_dir.join("machine_id"),
    };
    let auth = CopilotAuth::new_for_tests(paths, server.uri(), server.uri(), server.uri())
        .expect("test copilot auth");

    (temp, Arc::new(auth))
}

#[tokio::test]
async fn memory_trace_returns_empty_for_copilot() {
    let server = MockServer::start().await;
    let client = ModelClient::new(
        /*auth_manager*/ None,
        ThreadId::new(),
        /*installation_id*/ "11111111-1111-4111-8111-111111111111".to_string(),
        create_copilot_provider(),
        SessionSource::Cli,
        /*model_verbosity*/ None,
        /*enable_request_compression*/ false,
        /*include_timing_metrics*/ false,
        /*beta_features_header*/ None,
    );
    let (_copilot_home, copilot_auth) = test_copilot_auth(&server);
    client.configure_copilot_session_for_tests(copilot_auth, format!("{}/v1", server.uri()));

    let dir = tempdir().expect("temp dir");
    let trace_path = dir.path().join("trace.json");
    tokio::fs::write(
        &trace_path,
        serde_json::json!([{
            "type": "message",
            "role": "assistant",
            "content": []
        }])
        .to_string(),
    )
    .await
    .expect("write trace");

    let memories = build_memories_from_trace_files(
        &client,
        &[trace_path],
        &test_model_info(),
        /*effort*/ None,
        &test_session_telemetry(),
    )
    .await
    .expect("copilot memory trace should short-circuit");

    assert_eq!(memories, Vec::<BuiltMemory>::new());
    assert_eq!(
        server.received_requests().await.unwrap_or_default().len(),
        0
    );
}
