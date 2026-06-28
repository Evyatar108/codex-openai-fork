// SANDBOX PATCH: D-001 Claude `/chat/completions` transport end-to-end test (US-008).
//
// Drives a full multi-turn agentic conversation over the UNSIGNED Claude
// `/chat/completions` transport against a mounted chat-SSE mock and asserts the
// `capture -> persist -> replay` chain end-to-end PLUS the dual-gate routing
// decision: with the parent Anthropic gate ON and the signed sub-gate OFF, a
// `ChatCompletions`-route model must egress to `/chat/completions`, NOT the signed
// `/v1/messages` arm. Closes the gap behind `docs/implementation/patch-surface.md`
// invariant 37 (this named home previously did not exist).
//
// Network-gated via `skip_if_no_network!`: the wiremock server needs localhost
// TCP, which the codex sandbox disables. Run OUTSIDE the sandbox via nextest /
// `just test` so the PROCESS-GLOBAL anthropic gates + `COPILOT_API_HOME` env are
// isolated per test (constraint 8 in the plan). The deterministic gated assertions
// for this feature live in `codex-copilot`'s inline tests (US-001); this is the
// integration capstone.

use anyhow::Result;
use codex_model_provider::install_anthropic_gate;
use codex_model_provider::install_anthropic_signed_messages_gate;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ModelWireRoute;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TokenUsageInfo;
use codex_protocol::user_input::UserInput;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::TempDirExt;
use core_test_support::chat_sse;
use core_test_support::chat_sse::CopilotAuthFixture;
use core_test_support::chat_sse::mount_chat_sse_sequence;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::local_selections;
use core_test_support::test_codex::test_codex;
use core_test_support::test_codex::turn_permission_fields;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_completions_captures_then_replays_reasoning_and_routes_unsigned() -> Result<()> {
    skip_if_no_network!(Ok(()));

    // Hermetic Copilot auth so `CopilotAuth::new()` resolves a cached token offline.
    let _auth = CopilotAuthFixture::install();
    let server = start_mock_server().await;

    // Shape an EXISTING bundled GPT slug like a synthesized Claude chat row: the
    // bundled test catalog is GPT-only, so a `claude-*` slug would panic
    // `with_model_info_override`. Routing depends on `wire_route` + the gates, not
    // the slug. `supports_reasoning_summaries = false` is load-bearing; it makes
    // core strip the `reasoning` object and thread `reasoning_effort` explicitly
    // (the path US-008 exercises), and a non-`None` `default_reasoning_level`
    // guarantees the effort reaches the chat body.
    let mut builder = test_codex().with_model_info_override("gpt-5.2", |model_info| {
        model_info.wire_route = ModelWireRoute::ChatCompletions;
        model_info.supports_reasoning_summaries = false;
        model_info.default_reasoning_level = Some(ReasoningEffort::High);
    });
    let test = builder.build(&server).await?;
    let codex = &test.codex;
    let cwd_path = test.cwd.abs();
    let session_model = test.session_configured.model.clone();

    // Parent gate ON, signed sub-gate OFF => a `ChatCompletions`-route model routes
    // to the unsigned `/chat/completions` path (and an `AnthropicMessages`-route
    // model would DEGRADE to it). Installed AFTER `build()` because config build
    // re-derives both gates from `Feature::*`; a bare install before `build()` is
    // silently reset.
    install_anthropic_gate(true);
    install_anthropic_signed_messages_gate(false);

    // Turn 1: Claude chain-of-thought (reasoning_text deltas) + a fragmented
    // typed tool call + usage. The model loop then sends a chat follow-up carrying
    // the assembled tool call/output, receives an assistant answer, and a later
    // user turn proves the same reasoning is replayed from persisted history.
    let reasoning_deltas = ["Let me ", "work through ", "this step by step."];
    let reasoning_text = reasoning_deltas.concat();
    let tool_call_id = "call_shell_chat";
    let tool_args = serde_json::to_string(&json!({
        "command": "echo chat tool",
        "login": false,
    }))?;

    let turn1 = chat_sse::sse(vec![
        chat_sse::ev_chat_reasoning_delta(reasoning_deltas[0]),
        chat_sse::ev_chat_reasoning_delta(reasoning_deltas[1]),
        chat_sse::ev_chat_reasoning_delta(reasoning_deltas[2]),
        chat_sse::ev_chat_tool_call(7, tool_call_id, "shell_command"),
        chat_sse::ev_chat_tool_call_args(7, "{\"command\":\"echo chat "),
        chat_sse::ev_chat_tool_call_args(7, "tool\",\"login\":false}"),
        chat_sse::ev_chat_finish("tool_calls"),
        chat_sse::ev_chat_usage(19, 4, 23),
    ]);
    let turn1_followup = chat_sse::sse(vec![
        chat_sse::ev_chat_content_delta("first answer"),
        chat_sse::ev_chat_finish("stop"),
        chat_sse::ev_chat_usage(29, 5, 34),
    ]);
    let turn2 = chat_sse::sse(vec![
        chat_sse::ev_chat_content_delta("second answer"),
        chat_sse::ev_chat_finish("stop"),
        chat_sse::ev_chat_usage(25, 3, 28),
    ]);
    let chat_mock = mount_chat_sse_sequence(&server, vec![turn1, turn1_followup, turn2]).await;

    let first_turn_usage = submit_user_turn(
        codex,
        "first question",
        turn_settings(&session_model, cwd_path.clone()),
    )
    .await?
    .expect("chat usage should surface after turn 1");
    submit_user_turn(
        codex,
        "follow up question",
        turn_settings(&session_model, cwd_path),
    )
    .await?;

    let requests = chat_mock.requests();
    assert_eq!(
        requests.len(),
        3,
        "expected exactly three /chat/completions requests, got {}",
        requests.len()
    );

    // Turn-1 egress hits the chat path and carries `reasoning_effort` (the explicit
    // threading path for `supports_reasoning_summaries = false` rows).
    let turn1_request = &requests[0];
    assert_eq!(turn1_request.path(), "/v1/chat/completions");
    assert!(
        turn1_request.reasoning_effort().is_some(),
        "turn-1 body must carry reasoning_effort; got {:?}",
        turn1_request.body_json().get("reasoning_effort")
    );

    // Usage from chat-completions trailing chunks is surfaced as TokenUsage and
    // accumulated across the tool-driving first turn.
    assert_eq!(first_turn_usage.last_token_usage.input_tokens, 29);
    assert_eq!(first_turn_usage.last_token_usage.output_tokens, 5);
    assert_eq!(first_turn_usage.last_token_usage.total_tokens, 34);
    assert_eq!(first_turn_usage.total_token_usage.input_tokens, 48);
    assert_eq!(first_turn_usage.total_token_usage.output_tokens, 9);
    assert_eq!(first_turn_usage.total_token_usage.total_tokens, 57);

    // The tool-driving follow-up request proves typed `function_call` assembly: the
    // streamed id/name + split argument fragments round-trip as one assistant
    // `tool_calls[]` entry, followed by the matching tool output message.
    let tool_followup_request = &requests[1];
    assert_eq!(
        tool_followup_request.assistant_tool_calls(),
        vec![json!({
            "id": tool_call_id,
            "type": "function",
            "function": {
                "name": "shell_command",
                "arguments": tool_args,
            },
        })]
    );
    let tool_messages = tool_followup_request.tool_messages();
    assert_eq!(tool_messages.len(), 1);
    assert_eq!(tool_messages[0]["tool_call_id"], tool_call_id);
    assert!(
        tool_messages[0]["content"]
            .as_str()
            .is_some_and(|content| content.contains("chat") && content.contains("tool")),
        "expected tool output to include command stdout; got {tool_messages:?}"
    );

    // Turn-2 outbound `messages[]` replays the captured turn-1 reasoning as a
    // standalone assistant message equal to the concatenated mock deltas. This is
    // the load-bearing capture -> persist -> replay proof: the reasoning is only in
    // the turn-2 request because it was captured in turn 1 AND persisted to history.
    let turn2_request = &requests[2];
    let assistant_contents = turn2_request.assistant_message_contents();
    assert!(
        assistant_contents
            .iter()
            .any(|content| content == &reasoning_text),
        "turn-2 messages[] must replay the captured reasoning {reasoning_text:?}; \
         got assistant contents {assistant_contents:?}"
    );

    // Routing proof at the SERVER level (NOT the chat-mock `path()`, which is
    // tautological; a misroute to `/v1/messages` would 404 against the unmounted
    // route and never be captured by the chat mock). The dual-gate decision must
    // land on `/chat/completions` and never touch the signed `/v1/messages` arm.
    let received = server.received_requests().await.unwrap_or_default();
    let paths: Vec<String> = received
        .iter()
        .map(|request| request.url.path().to_string())
        .collect();
    assert!(
        paths.iter().any(|path| path.ends_with("/chat/completions")),
        "expected at least one /chat/completions egress; got {paths:?}"
    );
    assert!(
        !paths.iter().any(|path| path.ends_with("/messages")),
        "unsigned route must NOT egress to the signed /v1/messages arm; got {paths:?}"
    );

    Ok(())
}

fn turn_settings(session_model: &str, cwd_path: AbsolutePathBuf) -> ThreadSettingsOverrides {
    let (sandbox_policy, permission_profile) =
        turn_permission_fields(PermissionProfile::Disabled, cwd_path.as_path());
    ThreadSettingsOverrides {
        environments: Some(local_selections(cwd_path)),
        approval_policy: Some(AskForApproval::Never),
        sandbox_policy: Some(sandbox_policy),
        permission_profile,
        collaboration_mode: Some(codex_protocol::config_types::CollaborationMode {
            mode: codex_protocol::config_types::ModeKind::Default,
            settings: codex_protocol::config_types::Settings {
                model: session_model.to_string(),
                reasoning_effort: None,
                developer_instructions: None,
            },
        }),
        ..Default::default()
    }
}

/// Submit a user message that starts a turn and block until that turn completes.
async fn submit_user_turn(
    codex: &std::sync::Arc<codex_core::CodexThread>,
    text: &str,
    thread_settings: ThreadSettingsOverrides,
) -> Result<Option<TokenUsageInfo>> {
    codex
        .submit(Op::UserInput {
            items: vec![UserInput::Text {
                text: text.to_string(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
            additional_context: Default::default(),
            thread_settings,
        })
        .await?;

    let mut last_usage = None;
    loop {
        match wait_for_event(codex, |_| true).await {
            EventMsg::TokenCount(event) if event.info.is_some() => {
                last_usage = event.info;
            }
            EventMsg::TurnComplete(_) => break,
            EventMsg::Error(event) => anyhow::bail!("turn failed: {event:?}"),
            _ => {}
        }
    }
    Ok(last_usage)
}
