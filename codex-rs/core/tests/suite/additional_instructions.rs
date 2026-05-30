//! Integration tests for launcher-injected `additional_instructions`.
//!
//! These tests assert the end-to-end seam: a Config-level
//! `additional_instructions` value reaches the outbound Responses API
//! `instructions` field at session start, composed with the stable
//! heading marker introduced by US-003.

use anyhow::Result;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_once;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;

const RAIL_TEXT: &str = "RAIL_TEXT_FROM_TEST_xyz_marker";
const HEADING_MARKER: &str = "--- launcher safety rails ---";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_instructions_reach_first_turn_request() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.additional_instructions = Some(RAIL_TEXT.to_string());
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = req.single_request();
    let instructions_text = request.instructions_text();
    assert!(
        instructions_text.contains(HEADING_MARKER),
        "expected outbound instructions to contain launcher safety rails heading, got: {instructions_text:?}"
    );
    assert!(
        instructions_text.contains(RAIL_TEXT),
        "expected outbound instructions to contain rail text, got: {instructions_text:?}"
    );
    // Heading immediately precedes rails.
    let composed_suffix = format!("{HEADING_MARKER}\n{RAIL_TEXT}");
    assert!(
        instructions_text.contains(&composed_suffix),
        "expected heading to be immediately followed by rail text, got: {instructions_text:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_instructions_none_matches_baseline() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let req = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.additional_instructions = None;
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: "hello".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = req.single_request();
    let instructions_text = request.instructions_text();
    assert!(
        !instructions_text.contains(HEADING_MARKER),
        "expected NO launcher safety rails heading when additional_instructions is None, got: {instructions_text:?}"
    );

    Ok(())
}

/// Idempotency at the outbound-request level: a second turn against the same
/// session reuses the composed `instructions` text and must still contain
/// exactly one heading marker.
///
/// TODO(US-004-4c): if/when the mock-Responses resume harness exposes a
/// simpler config-injection path for `additional_instructions` across
/// process restarts, upgrade this to a true resume assertion. Operator-
/// confirmed downgrade-to-documented-gap per plan US-004 4c escape hatch.
/// The helper-level idempotency assertion already lives in
/// `core/src/session/tests.rs::compose_base_with_rails_is_idempotent`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn additional_instructions_second_turn_is_idempotent() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let _req1 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-1"), ev_completed("resp-1")]),
    )
    .await;
    let req2 = mount_sse_once(
        &server,
        sse(vec![ev_response_created("resp-2"), ev_completed("resp-2")]),
    )
    .await;

    let test = test_codex()
        .with_config(|config| {
            config.additional_instructions = Some(RAIL_TEXT.to_string());
        })
        .build(&server)
        .await?;

    test.codex
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: "hello 1".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    test.codex
        .submit(Op::UserInput {
            environments: None,
            items: vec![UserInput::Text {
                text: "hello 2".into(),
                text_elements: Vec::new(),
            }],
            final_output_json_schema: None,
            responsesapi_client_metadata: None,
        })
        .await?;
    wait_for_event(&test.codex, |ev| matches!(ev, EventMsg::TurnComplete(_))).await;

    let request = req2.single_request();
    let instructions_text = request.instructions_text();
    assert_eq!(
        instructions_text.matches(HEADING_MARKER).count(),
        1,
        "expected exactly one launcher safety rails heading on second turn, got: {instructions_text:?}"
    );

    Ok(())
}

// TODO(US-004-4d): subagent inheritance — operator-confirmed downgrade-to-documented-gap
// per plan US-004 4c escape hatch. The subagent test fixture (see
// suite/spawn_agent_description.rs and suite/hierarchical_agents.rs) requires
// deeper fixture surgery to assert outbound instructions on a child agent
// turn. Deferred to follow-up. The US-002 + US-003 seam guarantees the parent
// SessionConfiguration value carries through to children (child sessions
// clone the parent SessionConfiguration in core/src/session/mod.rs).
