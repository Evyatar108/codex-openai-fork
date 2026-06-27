// SANDBOX PATCH: signed-CoT native Anthropic Messages (`/v1/messages`) stream driver.
//
// The signed sibling of `stream_chat_completions`. It POSTs an Anthropic Messages
// request (built by the overlay `build_messages_request_body`) to
// `api.githubcopilot.com/v1/messages`, streams the native Anthropic Messages SSE
// through the (shared, extended) `AnthropicSseParser`, and maps it to
// `ResponseEvent`s — capturing each SIGNED `thinking` block into its OWN
// `ResponseItem::Reasoning { content: Some(..), encrypted_content: Some(signature) }`.
//
// Unlike the unsigned chat driver (a single reasoning accumulator that collapses
// all CoT into one item), this opens/closes a reasoning item PER content-block so
// interleaved thinking yields one persisted signed item per block, each carrying
// its own signature bound to the following `tool_use`. Items are emitted in WIRE
// ORDER (reasoning / message / function-call interleaved as they close) so the
// persisted rollout faithfully reconstructs the signed `thinking`->`tool_use`
// adjacency the outbound builder replays. See `docs/implementation/patch-surface.md` §14.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_api::ResponseEvent;
use codex_copilot::ChatStreamEvent;
use codex_copilot::ChatUsage;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_copilot::build_messages_request_body;
use codex_copilot::payload::request_initiator;
use codex_copilot::send_messages_request;
use codex_login::default_client::build_reqwest_client;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ReasoningItemContent;
use codex_protocol::models::ResponseItem;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client_common::ResponseStream;

use super::AssistantMessageItem;
use super::CHANNEL_CAPACITY;
use super::COPILOT_BASE_URL;
use super::ToolAccumulator;
use super::anthropic_sse::AnthropicSseParser;
use super::anthropic_sse::TranslatedSseEvent;
use super::to_token_usage;

/// Accumulates ONE signed `thinking` block into the reasoning-item lifecycle the
/// session state machine requires: opened on its first delta
/// (`OutputItemAdded(Reasoning)` — the session needs an active item for the
/// `ReasoningContentDelta`), finalized at its `content_block_stop`
/// (`OutputItemDone(Reasoning { content: Some([text]), encrypted_content: Some(sig) })`).
/// `finish()` RESETS the accumulator so the NEXT thinking block (interleaved
/// thinking) opens a fresh item with its own signature.
#[derive(Default)]
struct SignedReasoningItem {
    id: Option<String>,
    text: String,
    signature: String,
}

impl SignedReasoningItem {
    fn is_open(&self) -> bool {
        self.id.is_some()
    }

    /// Marks the reasoning item open and returns the empty item to emit as
    /// `OutputItemAdded`. Text/signature are attached at `finish()`.
    fn open(&mut self) -> ResponseItem {
        let id = uuid::Uuid::new_v4().to_string();
        self.id = Some(id.clone());
        ResponseItem::Reasoning {
            id,
            summary: Vec::new(),
            content: None,
            encrypted_content: None,
            metadata: None,
        }
    }

    /// Returns the finalized reasoning item (full CoT text + signature) to emit as
    /// `OutputItemDone`, or `None` when no item is open. Resets the accumulator so
    /// the next signed block starts clean. The signature is persisted into
    /// `encrypted_content` (the slot the unsigned lifecycle reserved); a block with
    /// no signature persists plaintext only (`encrypted_content: None`).
    fn finish(&mut self) -> Option<ResponseItem> {
        let id = self.id.take()?;
        let text = std::mem::take(&mut self.text);
        let signature = std::mem::take(&mut self.signature);
        Some(ResponseItem::Reasoning {
            id,
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText { text }]),
            encrypted_content: (!signature.is_empty()).then_some(signature),
            metadata: None,
        })
    }
}

/// Stream a turn for a signed-messages-routed (Claude-via-Copilot) model over the
/// native Anthropic Messages transport. `responses_body` is the serialized
/// `ResponsesApiRequest` core already builds; `base_url` is the Copilot endpoint
/// (defaults to `api.githubcopilot.com`); `reasoning_effort`, when `Some`, sizes
/// the Anthropic `thinking.budget_tokens`.
pub(crate) async fn stream_anthropic_messages(
    responses_body: serde_json::Value,
    model_slug: &str,
    base_url: Option<&str>,
    reasoning_effort: Option<String>,
) -> Result<ResponseStream> {
    let messages_body = build_messages_request_body(&responses_body, model_slug, reasoning_effort)
        .map_err(|err| CodexErr::Fatal(err.to_string()))?;
    let initiator = request_initiator(&responses_body);

    let copilot_auth =
        Arc::new(CopilotAuth::new().map_err(|err| CodexErr::Fatal(err.to_string()))?);
    let header_source = CopilotHeaderSource::new(copilot_auth)
        .await
        .map_err(|err| CodexErr::Fatal(err.to_string()))?;
    let client = build_reqwest_client();
    let base_url = base_url.unwrap_or(COPILOT_BASE_URL).to_string();

    let (tx_event, rx_event) = mpsc::channel::<Result<ResponseEvent>>(CHANNEL_CAPACITY);
    let consumer_dropped = CancellationToken::new();
    let consumer_dropped_task = consumer_dropped.clone();

    tokio::spawn(async move {
        run_anthropic_messages_stream(
            client,
            header_source,
            base_url,
            messages_body,
            initiator,
            tx_event,
            consumer_dropped_task,
        )
        .await;
    });

    Ok(ResponseStream {
        rx_event,
        consumer_dropped,
    })
}

/// Per-stream mutable state for the signed messages driver.
#[derive(Default)]
struct MessagesStreamState {
    tools: BTreeMap<i64, ToolAccumulator>,
    message: AssistantMessageItem,
    reasoning: SignedReasoningItem,
    usage: Option<ChatUsage>,
    finish_reason: Option<String>,
}

async fn run_anthropic_messages_stream(
    client: reqwest::Client,
    header_source: CopilotHeaderSource,
    base_url: String,
    messages_body: codex_copilot::AnthropicMessagesRequestBody,
    initiator: codex_copilot::payload::Initiator,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    let mut response = match send_messages_request(
        &client,
        &header_source,
        &base_url,
        &messages_body,
        initiator,
    )
    .await
    {
        Ok(response) => response,
        Err(err) => {
            let _ = tx_event
                .send(Err(CodexErr::Stream(err.to_string(), None)))
                .await;
            return;
        }
    };

    let mut parser = AnthropicSseParser::new();
    let mut state = MessagesStreamState::default();

    loop {
        let chunk = tokio::select! {
            _ = consumer_dropped.cancelled() => return,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(bytes)) => {
                for event in parser.push(&bytes) {
                    if !handle_messages_event(event, &mut state, &tx_event).await {
                        return;
                    }
                }
            }
            Ok(None) => break,
            Err(err) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(err.to_string(), None)))
                    .await;
                return;
            }
        }
    }
    for event in parser.finish() {
        if !handle_messages_event(event, &mut state, &tx_event).await {
            return;
        }
    }

    // Close any reasoning/message still open, then flush remaining tool calls in
    // index order, then signal turn completion.
    if !close_reasoning(&mut state.reasoning, &tx_event).await {
        return;
    }
    if !close_message(&mut state.message, &tx_event).await {
        return;
    }
    if !flush_tools(&mut state.tools, &tx_event).await {
        return;
    }

    let end_turn = state
        .finish_reason
        .as_deref()
        .map(|reason| matches!(reason, "stop" | "tool_calls"));
    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: state.usage.take().map(to_token_usage),
            end_turn,
        }))
        .await;
}

async fn handle_messages_event(
    event: TranslatedSseEvent,
    state: &mut MessagesStreamState,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    match event {
        // A signed thinking block opens its own reasoning item. Close any open
        // message and flush completed tools FIRST so the persisted item order stays
        // wire-faithful (reasoning -> tool_use -> reasoning ...).
        TranslatedSseEvent::SignedThinkingStart { .. } => {
            if !close_message(&mut state.message, tx_event).await
                || !flush_tools(&mut state.tools, tx_event).await
            {
                return false;
            }
            if !state.reasoning.is_open() {
                let item = state.reasoning.open();
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(item)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            true
        }
        TranslatedSseEvent::SignedThinkingDelta { text, .. } => {
            if !state.reasoning.is_open() {
                let item = state.reasoning.open();
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(item)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            state.reasoning.text.push_str(&text);
            tx_event
                .send(Ok(ResponseEvent::ReasoningContentDelta {
                    delta: text,
                    content_index: 0,
                }))
                .await
                .is_ok()
        }
        TranslatedSseEvent::SignedThinkingSignature { signature, .. } => {
            state.reasoning.signature.push_str(&signature);
            true
        }
        TranslatedSseEvent::SignedThinkingStop { .. } => {
            close_reasoning(&mut state.reasoning, tx_event).await
        }
        // Assistant text: close any open reasoning, flush completed tools, then open
        // the message item before streaming its text deltas.
        TranslatedSseEvent::Chat(ChatStreamEvent::ContentDelta(text)) => {
            if !close_reasoning(&mut state.reasoning, tx_event).await {
                return false;
            }
            if !state.message.is_open() {
                if !flush_tools(&mut state.tools, tx_event).await {
                    return false;
                }
                let item = state.message.open();
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(item)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            state.message.text.push_str(&text);
            tx_event
                .send(Ok(ResponseEvent::OutputTextDelta(text)))
                .await
                .is_ok()
        }
        TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        }) => {
            let accum = state.tools.entry(index).or_default();
            if id.is_some() {
                accum.call_id = id;
            }
            if name.is_some() {
                accum.name = name;
            }
            if arguments.is_empty() {
                return true;
            }
            accum.arguments.push_str(&arguments);
            let item_id = accum
                .call_id
                .clone()
                .unwrap_or_else(|| format!("messages_item_{index}"));
            tx_event
                .send(Ok(ResponseEvent::ToolCallInputDelta {
                    item_id,
                    call_id: accum.call_id.clone(),
                    delta: arguments,
                }))
                .await
                .is_ok()
        }
        TranslatedSseEvent::Chat(ChatStreamEvent::Finish { reason }) => {
            if reason.is_some() {
                state.finish_reason = reason;
            }
            true
        }
        TranslatedSseEvent::Chat(ChatStreamEvent::Usage(chat_usage)) => {
            if chat_usage.prompt_tokens.is_none()
                && chat_usage.completion_tokens.is_none()
                && chat_usage.total_tokens.is_none()
            {
                return true;
            }
            state.usage = Some(chat_usage);
            true
        }
        // The native messages shape never carries the unsigned `reasoning_text`
        // (`ReasoningDelta`); ignore it defensively rather than mixing paths.
        TranslatedSseEvent::ReasoningDelta(_) => true,
        TranslatedSseEvent::StreamError(error_message) => {
            let _ = tx_event
                .send(Err(CodexErr::Stream(error_message, None)))
                .await;
            false
        }
    }
}

/// Finalizes the open signed reasoning item (if any) as `OutputItemDone`. Returns
/// `false` only when the consumer dropped (send failed).
async fn close_reasoning(
    reasoning: &mut SignedReasoningItem,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    match reasoning.finish() {
        Some(item) => tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_ok(),
        None => true,
    }
}

/// Finalizes the open assistant message item (if any) as `OutputItemDone`.
async fn close_message(
    message: &mut AssistantMessageItem,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    let Some(item) = message.finish() else {
        return true;
    };
    let sent = tx_event
        .send(Ok(ResponseEvent::OutputItemDone(item)))
        .await
        .is_ok();
    *message = AssistantMessageItem::default();
    sent
}

/// Drains accumulated tool calls in index order, emitting each as a finalized
/// `FunctionCall` `OutputItemDone`. Returns `false` only when the consumer dropped.
async fn flush_tools(
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    for (_, accum) in std::mem::take(tools) {
        let item = ResponseItem::FunctionCall {
            id: None,
            name: accum.name.unwrap_or_default(),
            namespace: None,
            arguments: accum.arguments,
            call_id: accum.call_id.unwrap_or_default(),
            metadata: None,
        };
        if tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_err()
        {
            return false;
        }
    }
    true
}

#[cfg(test)]
#[path = "messages_stream_tests.rs"]
mod tests;
