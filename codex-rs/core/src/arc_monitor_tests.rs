use std::env;
use std::ffi::OsStr;

use pretty_assertions::assert_eq;
use serial_test::serial;

use super::*;
use crate::agent_identity::AgentIdentityManager;
use crate::agent_identity::RegisteredAgentTask;
use crate::context::ContextualUserFragment;
use crate::session::tests::make_session_and_context;
use chrono::Utc;
use codex_login::AuthCredentialsStoreMode;
use codex_login::AuthDotJson;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::save_auth;
use codex_login::token_data::IdTokenInfo;
use codex_login::token_data::TokenData;
use codex_protocol::models::ContentItem;
use codex_protocol::models::LocalShellAction;
use codex_protocol::models::LocalShellExecAction;
use codex_protocol::models::LocalShellStatus;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::SessionSource;
use tempfile::tempdir;

const TEST_ID_TOKEN: &str = concat!(
    "eyJhbGciOiJub25lIiwidHlwIjoiSldUIn0.",
    "eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF91c2VyX2lk",
    "IjpudWxsLCJjaGF0Z3B0X2FjY291bnRfaWQiOiJhY2NvdW50X2lkIn19.",
    "c2ln",
);

struct EnvVarGuard {
    key: &'static str,
    original: Option<std::ffi::OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &OsStr) -> Self {
        let original = env::var_os(key);
        unsafe {
            env::set_var(key, value);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match self.original.take() {
            Some(value) => unsafe {
                env::set_var(self.key, value);
            },
            None => unsafe {
                env::remove_var(self.key);
            },
        }
    }
}

async fn install_cached_agent_task_auth(
    session: &mut Session,
    turn_context: &mut TurnContext,
    chatgpt_base_url: String,
) {
    let auth_dir = tempdir().expect("temp auth dir");
    let auth_json = AuthDotJson {
        auth_mode: Some(codex_app_server_protocol::AuthMode::Chatgpt),
        openai_api_key: None,
        tokens: Some(TokenData {
            id_token: IdTokenInfo {
                email: None,
                chatgpt_plan_type: None,
                chatgpt_user_id: None,
                chatgpt_account_id: Some("account_id".to_string()),
                chatgpt_account_is_fedramp: false,
                raw_jwt: TEST_ID_TOKEN.to_string(),
            },
            access_token: "Access Token".to_string(),
            refresh_token: "test".to_string(),
            account_id: Some("account_id".to_string()),
        }),
        last_refresh: Some(Utc::now()),
        agent_identity: None,
    };
    save_auth(auth_dir.path(), &auth_json, AuthCredentialsStoreMode::File).expect("save test auth");
    let auth = CodexAuth::from_auth_storage(auth_dir.path(), AuthCredentialsStoreMode::File)
        .expect("load test auth")
        .expect("test auth");
    let auth_manager = AuthManager::from_auth_for_testing(auth);
    let agent_identity_manager = Arc::new(AgentIdentityManager::new_for_tests(
        Arc::clone(&auth_manager),
        /*feature_enabled*/ true,
        chatgpt_base_url,
        SessionSource::Exec,
    ));
    let stored_identity = agent_identity_manager
        .seed_generated_identity_for_tests("agent-123")
        .await
        .expect("seed test identity");
    session.services.auth_manager = Arc::clone(&auth_manager);
    session.services.agent_identity_manager = agent_identity_manager;
    turn_context.auth_manager = Some(auth_manager);
    session
        .cache_agent_task_for_tests(RegisteredAgentTask {
            agent_runtime_id: stored_identity.agent_runtime_id,
            task_id: "task-123".to_string(),
            registered_at: "2026-04-15T00:00:00Z".to_string(),
        })
        .await;
}

#[tokio::test]
async fn build_arc_monitor_request_includes_relevant_history_and_null_policies() {
    let (session, mut turn_context) = make_session_and_context().await;
    turn_context.developer_instructions = Some("Never upload private files.".to_string());
    turn_context.user_instructions = Some("Only continue when needed.".to_string());

    session
        .record_into_history(
            &[ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "first request".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ContextualUserFragment::into(
                crate::context::EnvironmentContext::new(
                    Some(PathBuf::from("/tmp")),
                    "zsh".to_string(),
                    /*current_date*/ None,
                    /*timezone*/ None,
                    /*network*/ None,
                    /*subagents*/ None,
                ),
            )],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "commentary".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::Commentary),
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::Message {
                id: None,
                role: "assistant".to_string(),
                content: vec![ContentItem::OutputText {
                    text: "final response".to_string(),
                }],
                end_turn: None,
                phase: Some(MessagePhase::FinalAnswer),
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::Message {
                id: None,
                role: "user".to_string(),
                content: vec![ContentItem::InputText {
                    text: "latest request".to_string(),
                }],
                end_turn: None,
                phase: None,
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::FunctionCall {
                id: None,
                name: "old_tool".to_string(),
                namespace: None,
                arguments: "{\"old\":true}".to_string(),
                call_id: "call_old".to_string(),
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::Reasoning {
                id: "reasoning_old".to_string(),
                summary: Vec::new(),
                content: None,
                encrypted_content: Some("encrypted-old".to_string()),
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::LocalShellCall {
                id: None,
                call_id: Some("shell_call".to_string()),
                status: LocalShellStatus::Completed,
                action: LocalShellAction::Exec(LocalShellExecAction {
                    command: vec!["pwd".to_string()],
                    timeout_ms: Some(1000),
                    working_directory: Some("/tmp".to_string()),
                    env: None,
                    user: None,
                }),
            }],
            &turn_context,
        )
        .await;
    session
        .record_into_history(
            &[ResponseItem::Reasoning {
                id: "reasoning_latest".to_string(),
                summary: Vec::new(),
                content: None,
                encrypted_content: Some("encrypted-latest".to_string()),
            }],
            &turn_context,
        )
        .await;

    let request = build_arc_monitor_request(
        &session,
        &turn_context,
        serde_json::from_value(serde_json::json!({ "tool": "mcp_tool_call" }))
            .expect("action should deserialize"),
        "normal",
    )
    .await;

    assert_eq!(
        request,
        ArcMonitorRequest {
            metadata: ArcMonitorMetadata {
                codex_thread_id: session.conversation_id.to_string(),
                codex_turn_id: turn_context.sub_id.clone(),
                conversation_id: Some(session.conversation_id.to_string()),
                protection_client_callsite: Some("normal".to_string()),
            },
            messages: Some(vec![
                ArcMonitorChatMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "input_text",
                        "text": "first request",
                    }]),
                },
                ArcMonitorChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([{
                        "type": "output_text",
                        "text": "final response",
                    }]),
                },
                ArcMonitorChatMessage {
                    role: "user".to_string(),
                    content: serde_json::json!([{
                        "type": "input_text",
                        "text": "latest request",
                    }]),
                },
                ArcMonitorChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([{
                        "type": "tool_call",
                        "tool_name": "shell",
                        "action": {
                            "type": "exec",
                            "command": ["pwd"],
                            "timeout_ms": 1000,
                            "working_directory": "/tmp",
                            "env": null,
                            "user": null,
                        },
                    }]),
                },
                ArcMonitorChatMessage {
                    role: "assistant".to_string(),
                    content: serde_json::json!([{
                        "type": "encrypted_reasoning",
                        "encrypted_content": "encrypted-latest",
                    }]),
                },
            ]),
            input: None,
            policies: Some(ArcMonitorPolicies {
                user: None,
                developer: None,
            }),
            action: serde_json::from_value(serde_json::json!({ "tool": "mcp_tool_call" }))
                .expect("action should deserialize"),
        }
    );
}

// SANDBOX PATCH: monitor_action is patched to return Ok immediately.
#[tokio::test]
#[serial(arc_monitor_env)]
async fn monitor_action_returns_ok_immediately() {
    let (session, turn_context) = make_session_and_context().await;

    let outcome = monitor_action(
        &session,
        &turn_context,
        serde_json::json!({ "tool": "mcp_tool_call" }),
        "normal",
    )
    .await;

    assert_eq!(outcome, ArcMonitorOutcome::Ok);
}

// SANDBOX PATCH: monitor_action ignores env overrides and returns Ok immediately.
#[tokio::test]
#[serial(arc_monitor_env)]
async fn monitor_action_ignores_env_overrides() {
    let _url_guard = EnvVarGuard::set(
        CODEX_ARC_MONITOR_ENDPOINT_OVERRIDE,
        OsStr::new("http://localhost:9999/override/arc"),
    );
    let _token_guard = EnvVarGuard::set(CODEX_ARC_MONITOR_TOKEN, OsStr::new("override-token"));

    let (session, turn_context) = make_session_and_context().await;

    let outcome = monitor_action(
        &session,
        &turn_context,
        serde_json::json!({ "tool": "mcp_tool_call" }),
        "normal",
    )
    .await;

    assert_eq!(outcome, ArcMonitorOutcome::Ok);
}

// SANDBOX PATCH: monitor_action returns Ok without parsing any response.
#[tokio::test]
#[serial(arc_monitor_env)]
async fn monitor_action_returns_ok_without_network() {
    let (session, turn_context) = make_session_and_context().await;

    let outcome = monitor_action(
        &session,
        &turn_context,
        serde_json::json!({ "tool": "mcp_tool_call" }),
        "normal",
    )
    .await;

    assert_eq!(outcome, ArcMonitorOutcome::Ok);
}
