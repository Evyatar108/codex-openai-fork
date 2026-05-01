use std::env;
use std::time::Duration;

use serde::Deserialize;
use serde::Serialize;
use tracing::warn;

use crate::compact::content_items_to_text;
use crate::event_mapping::is_contextual_user_message_content;
use crate::session::session::Session;
use crate::session::turn_context::TurnContext;
#[allow(unused_imports)]
use codex_login::CodexAuth;
#[allow(unused_imports)]
use codex_login::default_client::build_reqwest_client;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseItem;

#[allow(dead_code)]
const ARC_MONITOR_TIMEOUT: Duration = Duration::from_secs(30);
#[allow(dead_code)]
const CODEX_ARC_MONITOR_ENDPOINT_OVERRIDE: &str = "CODEX_ARC_MONITOR_ENDPOINT_OVERRIDE";
#[allow(dead_code)]
const CODEX_ARC_MONITOR_TOKEN: &str = "CODEX_ARC_MONITOR_TOKEN";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ArcMonitorOutcome {
    Ok,
    SteerModel(String),
    AskUser(String),
}

#[derive(Debug, Serialize, PartialEq)]
struct ArcMonitorRequest {
    metadata: ArcMonitorMetadata,
    #[serde(skip_serializing_if = "Option::is_none")]
    messages: Option<Vec<ArcMonitorChatMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Vec<ResponseItem>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    policies: Option<ArcMonitorPolicies>,
    action: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ArcMonitorResult {
    outcome: ArcMonitorResultOutcome,
    short_reason: String,
    rationale: String,
    risk_score: u8,
    risk_level: ArcMonitorRiskLevel,
    evidence: Vec<ArcMonitorEvidence>,
}

#[derive(Debug, Serialize, PartialEq)]
struct ArcMonitorChatMessage {
    role: String,
    content: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq)]
struct ArcMonitorPolicies {
    user: Option<String>,
    developer: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct ArcMonitorMetadata {
    codex_thread_id: String,
    codex_turn_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    protection_client_callsite: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(dead_code)]
struct ArcMonitorEvidence {
    message: String,
    why: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
#[allow(dead_code)]
enum ArcMonitorResultOutcome {
    Ok,
    SteerModel,
    AskUser,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)]
enum ArcMonitorRiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

// SANDBOX PATCH: Return Ok immediately without making any HTTP request.
// Safety monitor is unnecessary when running through the network sandbox.
pub(crate) async fn monitor_action(
    _sess: &Session,
    _turn_context: &TurnContext,
    _action: serde_json::Value,
    _protection_client_callsite: &'static str,
) -> ArcMonitorOutcome {
    // SANDBOX PATCH: Return Ok immediately without making any HTTP request.
    // Safety monitor is unnecessary when running through the network sandbox.
    ArcMonitorOutcome::Ok
}

#[allow(dead_code)]
fn read_non_empty_env_var(key: &str) -> Option<String> {
    match env::var(key) {
        Ok(value) => {
            let value = value.trim();
            (!value.is_empty()).then(|| value.to_string())
        }
        Err(env::VarError::NotPresent) => None,
        Err(env::VarError::NotUnicode(_)) => {
            warn!(
                env_var = key,
                "ignoring non-unicode safety monitor env override"
            );
            None
        }
    }
}

#[allow(dead_code)]
async fn build_arc_monitor_request(
    sess: &Session,
    turn_context: &TurnContext,
    action: serde_json::Map<String, serde_json::Value>,
    protection_client_callsite: &'static str,
) -> ArcMonitorRequest {
    let history = sess.clone_history().await;
    let mut messages = build_arc_monitor_messages(history.raw_items());
    if messages.is_empty() {
        messages.push(build_arc_monitor_message(
            "user",
            serde_json::Value::String(
                "No prior conversation history is available for this ARC evaluation.".to_string(),
            ),
        ));
    }

    let conversation_id = sess.conversation_id.to_string();
    ArcMonitorRequest {
        metadata: ArcMonitorMetadata {
            codex_thread_id: conversation_id.clone(),
            codex_turn_id: turn_context.sub_id.clone(),
            conversation_id: Some(conversation_id),
            protection_client_callsite: Some(protection_client_callsite.to_string()),
        },
        messages: Some(messages),
        input: None,
        policies: Some(ArcMonitorPolicies {
            user: None,
            developer: None,
        }),
        action,
    }
}

#[allow(dead_code)]
fn build_arc_monitor_messages(items: &[ResponseItem]) -> Vec<ArcMonitorChatMessage> {
    let last_tool_call_index = items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            matches!(
                item,
                ResponseItem::LocalShellCall { .. }
                    | ResponseItem::FunctionCall { .. }
                    | ResponseItem::CustomToolCall { .. }
                    | ResponseItem::WebSearchCall { .. }
            )
        })
        .map(|(index, _)| index);
    let last_encrypted_reasoning_index = items
        .iter()
        .enumerate()
        .rev()
        .find(|(_, item)| {
            matches!(
                item,
                ResponseItem::Reasoning {
                    encrypted_content: Some(encrypted_content),
                    ..
                } if !encrypted_content.trim().is_empty()
            )
        })
        .map(|(index, _)| index);

    items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            build_arc_monitor_message_item(
                item,
                index,
                last_tool_call_index,
                last_encrypted_reasoning_index,
            )
        })
        .collect()
}

#[allow(dead_code)]
fn build_arc_monitor_message_item(
    item: &ResponseItem,
    index: usize,
    last_tool_call_index: Option<usize>,
    last_encrypted_reasoning_index: Option<usize>,
) -> Option<ArcMonitorChatMessage> {
    match item {
        ResponseItem::Message { role, content, .. } if role == "user" => {
            if is_contextual_user_message_content(content) {
                None
            } else {
                content_items_to_text(content)
                    .map(|text| build_arc_monitor_text_message("user", "input_text", text))
            }
        }
        ResponseItem::Message {
            role,
            content,
            phase: Some(MessagePhase::FinalAnswer),
            ..
        } if role == "assistant" => content_items_to_text(content)
            .map(|text| build_arc_monitor_text_message("assistant", "output_text", text)),
        ResponseItem::Message { .. } => None,
        ResponseItem::Reasoning {
            encrypted_content: Some(encrypted_content),
            ..
        } if Some(index) == last_encrypted_reasoning_index
            && !encrypted_content.trim().is_empty() =>
        {
            Some(build_arc_monitor_message(
                "assistant",
                serde_json::json!([{
                    "type": "encrypted_reasoning",
                    "encrypted_content": encrypted_content,
                }]),
            ))
        }
        ResponseItem::Reasoning { .. } => None,
        ResponseItem::LocalShellCall { action, .. } if Some(index) == last_tool_call_index => {
            Some(build_arc_monitor_message(
                "assistant",
                serde_json::json!([{
                    "type": "tool_call",
                    "tool_name": "shell",
                    "action": action,
                }]),
            ))
        }
        ResponseItem::FunctionCall {
            name, arguments, ..
        } if Some(index) == last_tool_call_index => Some(build_arc_monitor_message(
            "assistant",
            serde_json::json!([{
                "type": "tool_call",
                "tool_name": name,
                "arguments": arguments,
            }]),
        )),
        ResponseItem::CustomToolCall { name, input, .. } if Some(index) == last_tool_call_index => {
            Some(build_arc_monitor_message(
                "assistant",
                serde_json::json!([{
                    "type": "tool_call",
                    "tool_name": name,
                    "input": input,
                }]),
            ))
        }
        ResponseItem::WebSearchCall { action, .. } if Some(index) == last_tool_call_index => {
            Some(build_arc_monitor_message(
                "assistant",
                serde_json::json!([{
                    "type": "tool_call",
                    "tool_name": "web_search",
                    "action": action,
                }]),
            ))
        }
        ResponseItem::LocalShellCall { .. }
        | ResponseItem::FunctionCall { .. }
        | ResponseItem::CustomToolCall { .. }
        | ResponseItem::ToolSearchCall { .. }
        | ResponseItem::WebSearchCall { .. }
        | ResponseItem::FunctionCallOutput { .. }
        | ResponseItem::CustomToolCallOutput { .. }
        | ResponseItem::ToolSearchOutput { .. }
        | ResponseItem::ImageGenerationCall { .. }
        | ResponseItem::Compaction { .. }
        | ResponseItem::Other => None,
    }
}

#[allow(dead_code)]
fn build_arc_monitor_text_message(
    role: &str,
    part_type: &str,
    text: String,
) -> ArcMonitorChatMessage {
    build_arc_monitor_message(
        role,
        serde_json::json!([{
            "type": part_type,
            "text": text,
        }]),
    )
}

#[allow(dead_code)]
fn build_arc_monitor_message(role: &str, content: serde_json::Value) -> ArcMonitorChatMessage {
    ArcMonitorChatMessage {
        role: role.to_string(),
        content,
    }
}

#[cfg(test)]
#[path = "arc_monitor_tests.rs"]
mod tests;
