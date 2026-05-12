use crate::function_tool::FunctionCallError;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::ToolHandler;
use crate::tools::registry::ToolKind;
use crate::unified_exec::AwaitBackgroundCompletionRequest;
use codex_tools::ToolName;
use serde::Deserialize;

use super::effective_max_output_tokens;
use super::post_unified_exec_tool_use_payload;

#[derive(Debug, Deserialize)]
struct AwaitBackgroundCompletionArgs {
    // The model is trained on `session_id`.
    session_id: i32,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    max_output_tokens: Option<usize>,
}

pub struct AwaitBackgroundCompletionHandler;

impl ToolHandler for AwaitBackgroundCompletionHandler {
    type Output = ExecCommandToolOutput;

    fn tool_name(&self) -> ToolName {
        ToolName::plain("await_background_completion")
    }

    fn kind(&self) -> ToolKind {
        ToolKind::Function
    }

    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    async fn is_mutating(&self, _invocation: &ToolInvocation) -> bool {
        // Waiting for a background process to finish does not mutate the
        // environment by itself, but the underlying process may be mutating.
        // Treat as non-mutating for tool planning purposes; the original
        // exec_command invocation already established mutation status.
        false
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &Self::Output,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }

    async fn handle(&self, invocation: ToolInvocation) -> Result<Self::Output, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(
                    "await_background_completion handler received unsupported payload".to_string(),
                ));
            }
        };

        let args: AwaitBackgroundCompletionArgs = parse_arguments(&arguments)?;
        let max_output_tokens =
            effective_max_output_tokens(args.max_output_tokens, turn.truncation_policy);
        let response = session
            .services
            .unified_exec_manager
            .await_background_completion(AwaitBackgroundCompletionRequest {
                process_id: args.session_id,
                timeout_ms: args.timeout_ms,
                max_output_tokens: Some(max_output_tokens),
            })
            .await
            .map_err(|err| {
                FunctionCallError::RespondToModel(format!(
                    "await_background_completion failed: {err}"
                ))
            })?;

        Ok(response)
    }
}
