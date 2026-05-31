use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_tools::JsonSchema;
use codex_tools::ResponsesApiTool;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;
use serde::Serialize;
use std::collections::BTreeMap;

const TOOL_NAME: &str = "spawn_top_level_session";
const AGENT_SPAWNER_ROLE: &str = "agent-spawner";
const HAPPY_CURRENT_SESSION_ID: &str = "HAPPY_CURRENT_SESSION_ID";
const HAPPY_DAEMON_CONTROL_URL: &str = "HAPPY_DAEMON_CONTROL_URL";
const PARENT_SESSION_ID_PATTERN: &str = "^[A-Za-z0-9_-]{1,128}$";

// SANDBOX PATCH: plugin-scope-axis — fork-only handler.
// Trait migrated from old ToolHandler+ToolKind to new ToolExecutor<ToolInvocation>+CoreToolRuntime
// shape (rebase-resume v0.135.0). Replant recipe: docs/implementation/patch-surface.md §15.
pub struct SpawnTopLevelSessionHandler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for SpawnTopLevelSessionHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_spawn_top_level_session_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        handle_spawn_top_level_session(invocation)
            .await
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for SpawnTopLevelSessionHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }
}

async fn handle_spawn_top_level_session(
    invocation: ToolInvocation,
) -> Result<FunctionToolOutput, FunctionCallError> {
    let ToolInvocation { turn, payload, .. } = invocation;
    if !is_agent_spawner_subagent(&turn.session_source) {
        return Err(FunctionCallError::RespondToModel(
            "spawn_top_level_session is only available to agent-spawner subagents".to_string(),
        ));
    }

    let arguments = match payload {
        ToolPayload::Function { arguments } => arguments,
        _ => {
            return Err(FunctionCallError::RespondToModel(
                "spawn_top_level_session received unsupported payload".to_string(),
            ));
        }
    };
    let args: SpawnTopLevelSessionArgs = parse_arguments(&arguments)?;
    let parent_session_id = std::env::var(HAPPY_CURRENT_SESSION_ID).map_err(|_| {
        FunctionCallError::RespondToModel(format!("missing {HAPPY_CURRENT_SESSION_ID}"))
    })?;
    if !is_valid_parent_session_id(&parent_session_id) {
        return Err(FunctionCallError::RespondToModel(format!(
            "{HAPPY_CURRENT_SESSION_ID} must match {PARENT_SESSION_ID_PATTERN}"
        )));
    }
    let control_url_str = std::env::var(HAPPY_DAEMON_CONTROL_URL).map_err(|_| {
        FunctionCallError::RespondToModel(format!("missing {HAPPY_DAEMON_CONTROL_URL}"))
    })?;
    // SANDBOX PATCH: plugin-scope-axis — validate loopback-only before posting
    let control_url = validate_loopback_control_url(&control_url_str)?;
    let endpoint = format!(
        "{}/spawn-session-from-session",
        control_url.as_str().trim_end_matches('/')
    );
    let body = SpawnTopLevelSessionRequest {
        parent_session_id,
        config: args,
    };

    let response = reqwest::Client::new()
        .post(endpoint)
        .json(&body)
        .send()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to call Happy daemon spawn endpoint: {err}"
            ))
        })?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(FunctionCallError::RespondToModel(format!(
            "Happy daemon spawn endpoint returned {status}: {text}"
        )));
    }

    match response
        .json::<SpawnTopLevelSessionResponse>()
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!(
                "failed to parse Happy daemon spawn response: {err}"
            ))
        })? {
        SpawnTopLevelSessionResponse::Success { session_id } => {
            let result = SpawnTopLevelSessionResult { session_id };
            let text = serde_json::to_string(&result).map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "failed to serialize spawn_top_level_session result: {err}"
                ))
            })?;
            Ok(FunctionToolOutput::from_text(text, Some(true)))
        }
        SpawnTopLevelSessionResponse::Error { error_message } => {
            Err(FunctionCallError::RespondToModel(error_message))
        }
    }
}

fn create_spawn_top_level_session_tool() -> ToolSpec {
    let properties = BTreeMap::from([
        (
            "agent".to_string(),
            JsonSchema::string(Some(
                "Required top-level agent or command profile to start.".to_string(),
            )),
        ),
        (
            "path".to_string(),
            JsonSchema::string(Some("Optional working directory path.".to_string())),
        ),
        (
            "model".to_string(),
            JsonSchema::string(Some("Optional model override.".to_string())),
        ),
        (
            "permissionMode".to_string(),
            JsonSchema::string(Some("Optional permission mode override.".to_string())),
        ),
        (
            "effortLevel".to_string(),
            JsonSchema::string(Some("Optional reasoning effort level.".to_string())),
        ),
        (
            "initialMessage".to_string(),
            JsonSchema::string(Some(
                "Optional initial message for the new session.".to_string(),
            )),
        ),
    ]);

    ToolSpec::Function(ResponsesApiTool {
        name: TOOL_NAME.to_string(),
        description: "Spawn a new top-level Happy session through the local daemon.".to_string(),
        strict: false,
        defer_loading: None,
        parameters: JsonSchema::object(
            properties,
            /*required*/ Some(vec!["agent".to_string()]),
            Some(false.into()),
        ),
        output_schema: None,
    })
}

fn is_agent_spawner_subagent(session_source: &SessionSource) -> bool {
    matches!(
        session_source,
        SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            agent_role: Some(role),
            ..
        }) if role == AGENT_SPAWNER_ROLE
    )
}

fn validate_loopback_control_url(raw: &str) -> Result<reqwest::Url, FunctionCallError> {
    let url = reqwest::Url::parse(raw).map_err(|_| {
        FunctionCallError::RespondToModel(format!(
            "{HAPPY_DAEMON_CONTROL_URL} is not a valid URL: {raw}"
        ))
    })?;
    if url.scheme() != "http" {
        return Err(FunctionCallError::RespondToModel(format!(
            "{HAPPY_DAEMON_CONTROL_URL} must use the http scheme (got: {})",
            url.scheme()
        )));
    }
    match url.host_str() {
        Some("127.0.0.1") | Some("::1") | Some("localhost") => {}
        other => {
            return Err(FunctionCallError::RespondToModel(format!(
                "{HAPPY_DAEMON_CONTROL_URL} must point at a loopback host (got: {})",
                other.unwrap_or("<none>")
            )));
        }
    }
    Ok(url)
}

fn is_valid_parent_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpawnTopLevelSessionArgs {
    agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    permission_mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    effort_level: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    initial_message: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpawnTopLevelSessionRequest {
    parent_session_id: String,
    config: SpawnTopLevelSessionArgs,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum SpawnTopLevelSessionResponse {
    #[serde(rename_all = "camelCase")]
    Success { session_id: String },
    #[serde(rename_all = "camelCase")]
    Error { error_message: String },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SpawnTopLevelSessionResult {
    session_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parent_session_id_validation_matches_expected_shape() {
        assert!(is_valid_parent_session_id("abc_123-XYZ"));
        assert!(!is_valid_parent_session_id(""));
        assert!(!is_valid_parent_session_id("machine:session"));
        assert!(!is_valid_parent_session_id(&"a".repeat(129)));
    }

    #[test]
    fn loopback_url_valid_127_0_0_1() {
        assert!(validate_loopback_control_url("http://127.0.0.1:8080").is_ok());
    }

    #[test]
    fn loopback_url_valid_localhost() {
        assert!(validate_loopback_control_url("http://localhost:9000").is_ok());
    }

    #[test]
    fn loopback_url_valid_ipv6_loopback() {
        assert!(validate_loopback_control_url("http://[::1]:8080").is_ok());
    }

    #[test]
    fn loopback_url_rejects_https_scheme() {
        let err = validate_loopback_control_url("https://127.0.0.1:8080").unwrap_err();
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("http scheme"), "unexpected msg: {msg}");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn loopback_url_rejects_non_loopback_host() {
        let err = validate_loopback_control_url("http://example.com/path").unwrap_err();
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("loopback host"), "unexpected msg: {msg}");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn loopback_url_rejects_malformed_url() {
        let err = validate_loopback_control_url("not-a-url").unwrap_err();
        match err {
            FunctionCallError::RespondToModel(msg) => {
                assert!(msg.contains("valid URL"), "unexpected msg: {msg}");
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }
}
