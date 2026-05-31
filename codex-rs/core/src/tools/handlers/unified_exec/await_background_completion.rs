use crate::function_tool::FunctionCallError;
use crate::tools::context::ExecCommandToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::PostToolUsePayload;
use crate::tools::registry::ToolExecutor;
use crate::unified_exec::AwaitBackgroundCompletionRequest;
use crate::unified_exec::resolve_max_tokens;
use codex_protocol::protocol::TruncationPolicy;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

use super::super::shell_spec::create_await_background_completion_tool;
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

// SANDBOX PATCH: D-002 — fork-only handler.
// Trait migrated from old ToolHandler+ToolKind to new ToolExecutor<ToolInvocation>+CoreToolRuntime
// shape (rebase-resume v0.135.0). The truncation-policy clamp helper was inlined
// (`effective_max_output_tokens` had only one caller post-rebase).
// Replant recipe: docs/implementation/patch-surface.md §15.
pub struct AwaitBackgroundCompletionHandler;

#[async_trait::async_trait]
impl ToolExecutor<ToolInvocation> for AwaitBackgroundCompletionHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("await_background_completion")
    }

    fn spec(&self) -> ToolSpec {
        create_await_background_completion_tool()
    }

    async fn handle(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        handle_await_background_completion(invocation)
            .await
            .map(boxed_tool_output)
    }
}

impl CoreToolRuntime for AwaitBackgroundCompletionHandler {
    fn matches_kind(&self, payload: &ToolPayload) -> bool {
        matches!(payload, ToolPayload::Function { .. })
    }

    fn post_tool_use_payload(
        &self,
        invocation: &ToolInvocation,
        result: &dyn crate::tools::context::ToolOutput,
    ) -> Option<PostToolUsePayload> {
        post_unified_exec_tool_use_payload(invocation, result)
    }
}

async fn handle_await_background_completion(
    invocation: ToolInvocation,
) -> Result<ExecCommandToolOutput, FunctionCallError> {
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
            truncation_policy: turn.truncation_policy,
        })
        .await
        .map_err(|err| {
            FunctionCallError::RespondToModel(format!("await_background_completion failed: {err}"))
        })?;

    Ok(response)
}

fn effective_max_output_tokens(
    max_output_tokens: Option<usize>,
    truncation_policy: TruncationPolicy,
) -> usize {
    resolve_max_tokens(max_output_tokens).min(truncation_policy.token_budget())
}
