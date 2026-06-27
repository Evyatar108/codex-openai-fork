use super::MessagesStreamState;
use super::SignedReasoningItem;
use super::TranslatedSseEvent;
use super::close_message;
use super::close_reasoning;
use super::flush_tools;
use super::handle_messages_event;

use codex_api::ResponseEvent;
use codex_copilot::ChatStreamEvent;
use codex_copilot::build_messages_request_body;
use codex_protocol::error::Result;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::sync::mpsc;

/// Runs a sequence of decoded SSE events through the signed driver's reduction
/// (`handle_messages_event` + end-of-stream finalization) and returns the
/// persisted `OutputItemDone` items in emission (wire) order.
async fn reduce_to_items(events: Vec<TranslatedSseEvent>) -> Vec<ResponseItem> {
    let (tx, mut rx) = mpsc::channel::<Result<ResponseEvent>>(256);
    let mut state = MessagesStreamState::default();
    for event in events {
        assert!(handle_messages_event(event, &mut state, &tx).await);
    }
    assert!(close_reasoning(&mut state.reasoning, &tx).await);
    assert!(close_message(&mut state.message, &tx).await);
    assert!(flush_tools(&mut state.tools, &tx).await);
    drop(tx);

    let mut items = Vec::new();
    while let Some(event) = rx.recv().await {
        if let Ok(ResponseEvent::OutputItemDone(item)) = event {
            items.push(item);
        }
    }
    items
}

#[test]
fn signed_reasoning_item_persists_plaintext_and_signature() {
    let mut item = SignedReasoningItem::default();
    let _ = item.open();
    item.text.push_str("let me think");
    item.signature.push_str("sig-xyz");
    let finished = item.finish().expect("item open");

    match finished {
        ResponseItem::Reasoning {
            content,
            encrypted_content,
            ..
        } => {
            assert_eq!(
                content,
                Some(vec![ReasoningItemContent::ReasoningText {
                    text: "let me think".to_string()
                }])
            );
            assert_eq!(encrypted_content.as_deref(), Some("sig-xyz"));
        }
        other => panic!("expected Reasoning, got {other:?}"),
    }

    // After finish() the accumulator is reset so the next block opens clean.
    assert!(!item.is_open());
    assert!(item.text.is_empty());
    assert!(item.signature.is_empty());
}

#[tokio::test]
async fn signed_turn_reduces_to_reasoning_then_function_call() {
    let events = vec![
        TranslatedSseEvent::SignedThinkingStart { index: 0 },
        TranslatedSseEvent::SignedThinkingDelta {
            index: 0,
            text: "add the numbers".to_string(),
        },
        TranslatedSseEvent::SignedThinkingSignature {
            index: 0,
            signature: "sig-abc".to_string(),
        },
        TranslatedSseEvent::SignedThinkingStop { index: 0 },
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index: 1,
            id: Some("call-1".to_string()),
            name: Some("calc".to_string()),
            arguments: String::new(),
        }),
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index: 1,
            id: None,
            name: None,
            arguments: "{\"x\":2}".to_string(),
        }),
        TranslatedSseEvent::Chat(ChatStreamEvent::Finish {
            reason: Some("tool_calls".to_string()),
        }),
    ];

    let items = reduce_to_items(events).await;
    assert_eq!(items.len(), 2);
    // Reasoning persisted FIRST (bound to the following function_call), with its
    // signature in `encrypted_content`.
    match &items[0] {
        ResponseItem::Reasoning {
            content,
            encrypted_content,
            ..
        } => {
            assert_eq!(
                content.as_ref().expect("content")[0],
                ReasoningItemContent::ReasoningText {
                    text: "add the numbers".to_string()
                }
            );
            assert_eq!(encrypted_content.as_deref(), Some("sig-abc"));
        }
        other => panic!("expected Reasoning first, got {other:?}"),
    }
    match &items[1] {
        ResponseItem::FunctionCall {
            name,
            arguments,
            call_id,
            ..
        } => {
            assert_eq!(name, "calc");
            assert_eq!(arguments, "{\"x\":2}");
            assert_eq!(call_id, "call-1");
        }
        other => panic!("expected FunctionCall second, got {other:?}"),
    }
}

#[tokio::test]
async fn interleaved_signed_blocks_keep_wire_order_and_own_signatures() {
    // thinking0 -> tool1 -> thinking2 -> tool3: each reasoning item carries its OWN
    // signature and is emitted immediately before its following function_call.
    let events = vec![
        TranslatedSseEvent::SignedThinkingStart { index: 0 },
        TranslatedSseEvent::SignedThinkingDelta {
            index: 0,
            text: "first".to_string(),
        },
        TranslatedSseEvent::SignedThinkingSignature {
            index: 0,
            signature: "sig-one".to_string(),
        },
        TranslatedSseEvent::SignedThinkingStop { index: 0 },
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index: 1,
            id: Some("c1".to_string()),
            name: Some("t1".to_string()),
            arguments: "{}".to_string(),
        }),
        TranslatedSseEvent::SignedThinkingStart { index: 2 },
        TranslatedSseEvent::SignedThinkingDelta {
            index: 2,
            text: "second".to_string(),
        },
        TranslatedSseEvent::SignedThinkingSignature {
            index: 2,
            signature: "sig-two".to_string(),
        },
        TranslatedSseEvent::SignedThinkingStop { index: 2 },
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index: 3,
            id: Some("c3".to_string()),
            name: Some("t3".to_string()),
            arguments: "{}".to_string(),
        }),
    ];

    let items = reduce_to_items(events).await;
    let signatures: Vec<Option<String>> = items
        .iter()
        .map(|item| match item {
            ResponseItem::Reasoning {
                encrypted_content, ..
            } => encrypted_content.clone(),
            ResponseItem::FunctionCall { call_id, .. } => Some(format!("call:{call_id}")),
            other => panic!("unexpected item {other:?}"),
        })
        .collect();
    assert_eq!(
        signatures,
        vec![
            Some("sig-one".to_string()),
            Some("call:c1".to_string()),
            Some("sig-two".to_string()),
            Some("call:c3".to_string()),
        ]
    );
}

/// AC12 round-trip: parse signed thinking -> persist into encrypted_content ->
/// replay through the outbound builder -> request body carries the signed thinking
/// block BEFORE the tool_use, signature preserved end-to-end.
#[tokio::test]
async fn signed_round_trip_preserves_signature_before_tool_use() {
    let events = vec![
        TranslatedSseEvent::SignedThinkingStart { index: 0 },
        TranslatedSseEvent::SignedThinkingDelta {
            index: 0,
            text: "reason".to_string(),
        },
        TranslatedSseEvent::SignedThinkingSignature {
            index: 0,
            signature: "the-signature".to_string(),
        },
        TranslatedSseEvent::SignedThinkingStop { index: 0 },
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index: 1,
            id: Some("call-1".to_string()),
            name: Some("calc".to_string()),
            arguments: "{\"x\":1}".to_string(),
        }),
    ];
    let persisted = reduce_to_items(events).await;

    // Build the Responses input the next turn would carry: the user prompt, the
    // persisted reasoning + function_call, and the tool result.
    let mut input = vec![json!({"type": "message", "role": "user", "content": "q"})];
    for item in &persisted {
        input.push(serde_json::to_value(item).expect("serialize item"));
    }
    input.push(json!({"type": "function_call_output", "call_id": "call-1", "output": "2"}));
    let responses = json!({"input": Value::Array(input)});

    let body =
        build_messages_request_body(&responses, "claude-sonnet-4.6", None).expect("build body");

    let assistant = body
        .messages
        .iter()
        .find(|m| m["role"] == json!("assistant"))
        .expect("assistant message");
    let content = assistant["content"].as_array().expect("content array");
    // Signed thinking block first, carrying the preserved signature, then tool_use.
    assert_eq!(content[0]["type"], json!("thinking"));
    assert_eq!(content[0]["thinking"], json!("reason"));
    assert_eq!(content[0]["signature"], json!("the-signature"));
    assert_eq!(content[1]["type"], json!("tool_use"));
    assert_eq!(content[1]["id"], json!("call-1"));
}
