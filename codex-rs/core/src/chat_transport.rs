// SANDBOX PATCH: D-001 Claude-via-Copilot chat-completions transport (core seam).
//
// This is the core half of the chat transport. The single new dispatch arm in
// `client.rs` (`WireApi::ChatCompletions => ...`) builds the same
// `ResponsesApiRequest` core builds for the `/responses` path, serializes it to
// JSON, and hands it here. We:
//   1. build a Copilot header source from the on-disk token cache (the same
//      tokens the Copilot provider uses),
//   2. translate the Responses JSON into a chat-completions body via the overlay
//      `codex_copilot::build_chat_request_body` (which hard-errors on Responses
//      features chat cannot serve — never a silent drop),
//   3. spawn a task that POSTs, reads the streamed chat SSE through the overlay
//      `ChatSseParser`, and MAPS the neutral `ChatStreamEvent`s into
//      `codex_api::ResponseEvent`s (per-`call_id` partial-JSON assembly keyed on
//      the chat content-block `index` — see the US-001 spike: Claude indices are
//      non-zero / non-contiguous),
//   4. return a core `ResponseStream` wrapping the mpsc receiver.
//
// The overlay (`codex-copilot`) yields NEUTRAL events (no `codex_api` types) to
// avoid a Cargo cycle; this module owns the neutral->`ResponseEvent` mapping.
// See `docs/implementation/patch-surface.md` §14.

use std::collections::BTreeMap;
use std::sync::Arc;

use codex_api::ResponseEvent;
use codex_copilot::ChatSseParser;
use codex_copilot::ChatStreamEvent;
use codex_copilot::ChatUsage;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_copilot::build_chat_request_body;
use codex_copilot::payload::Initiator;
use codex_copilot::payload::request_initiator;
use codex_copilot::send_chat_request;
use codex_login::default_client::build_reqwest_client;
use codex_model_provider::anthropic_models_resolved;
use codex_model_provider_info::WireApi;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelWireRoute;
use codex_protocol::protocol::TokenUsage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client_common::ResponseStream;

mod anthropic_sse;

use anthropic_sse::AnthropicSseParser;
use anthropic_sse::TranslatedSseEvent;

const CHANNEL_CAPACITY: usize = 1600;
const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";

/// Maps a model's protocol-local route hint to the EFFECTIVE wire protocol at the
/// dispatch boundary. This is the single place `ModelWireRoute` becomes a `WireApi`:
/// a chat-hinted model routes to `ChatCompletions` only when Anthropic transport
/// is opted in, everything else to the provider's wire. See
/// `docs/implementation/patch-surface.md` §14 invariant 35.
pub(crate) fn effective_wire_api(route: ModelWireRoute, provider_wire: WireApi) -> WireApi {
    effective_wire_api_gated(route, provider_wire, anthropic_models_resolved())
}

pub(crate) fn effective_wire_api_gated(
    route: ModelWireRoute,
    provider_wire: WireApi,
    anthropic_enabled: bool,
) -> WireApi {
    match route {
        ModelWireRoute::ChatCompletions if anthropic_enabled => WireApi::ChatCompletions,
        ModelWireRoute::ChatCompletions | ModelWireRoute::ProviderDefault => provider_wire,
    }
}

/// Per-tool-call accumulator. Keyed by the chat content-block `index` (which is
/// NOT 0-based contiguous for Claude), so parallel calls never collapse/reorder.
#[derive(Default)]
struct ToolAccumulator {
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

/// Accumulates the streamed assistant message so the chat transport can emit the
/// item lifecycle the session state machine requires: an `OutputItemAdded`
/// (assistant message opened) BEFORE the first `OutputTextDelta` — without an open
/// item the turn loop hits `error_or_panic("OutputTextDelta without active item")`
/// (`session/turn.rs`) — and a closing `OutputItemDone` carrying the full text so
/// the assistant message is recorded. The `/responses` path gets these item events
/// from the provider; the chat path must synthesize them. The same `id` is reused
/// for open and close so both refer to one item.
#[derive(Default)]
struct AssistantMessageItem {
    id: Option<String>,
    text: String,
}

impl AssistantMessageItem {
    fn is_open(&self) -> bool {
        self.id.is_some()
    }

    /// Marks the message open and returns the empty assistant-message item to send
    /// as `OutputItemAdded`.
    fn open(&mut self) -> ResponseItem {
        let id = uuid::Uuid::new_v4().to_string();
        self.id = Some(id.clone());
        ResponseItem::Message {
            id: Some(id),
            role: "assistant".to_string(),
            content: Vec::new(),
            phase: None,
            metadata: None,
        }
    }

    /// Returns the finalized assistant-message item (full accumulated text) to send
    /// as `OutputItemDone`, or `None` when no text was ever streamed.
    fn finish(&self) -> Option<ResponseItem> {
        let id = self.id.clone()?;
        Some(ResponseItem::Message {
            id: Some(id),
            role: "assistant".to_string(),
            content: vec![ContentItem::OutputText {
                text: self.text.clone(),
            }],
            phase: None,
            metadata: None,
        })
    }
}

/// Stream a turn for a chat-routed (Claude-via-Copilot) model.
///
/// `responses_body` is the serialized `ResponsesApiRequest` core already builds;
/// `base_url` is the Copilot endpoint (defaults to `api.githubcopilot.com`).
/// `reasoning_effort`, when `Some`, is threaded to the overlay builder and
/// emitted as a top-level `reasoning_effort` on the chat body — core strips it
/// from `responses_body` for synthesized Claude rows
/// (`supports_reasoning_summaries=false`), so the caller passes it explicitly.
pub(crate) async fn stream_chat_completions(
    responses_body: serde_json::Value,
    model_slug: &str,
    base_url: Option<&str>,
    reasoning_effort: Option<String>,
) -> Result<ResponseStream> {
    let chat_body = build_chat_request_body(&responses_body, model_slug, reasoning_effort)
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
        run_chat_stream(
            client,
            header_source,
            base_url,
            chat_body,
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

async fn run_chat_stream(
    client: reqwest::Client,
    header_source: CopilotHeaderSource,
    base_url: String,
    chat_body: codex_copilot::ChatRequestBody,
    initiator: Initiator,
    tx_event: mpsc::Sender<Result<ResponseEvent>>,
    consumer_dropped: CancellationToken,
) {
    if tx_event.send(Ok(ResponseEvent::Created)).await.is_err() {
        return;
    }

    let mut response =
        match send_chat_request(&client, &header_source, &base_url, &chat_body, initiator).await {
            Ok(response) => response,
            Err(err) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(err.to_string(), None)))
                    .await;
                return;
            }
        };

    let mut parser = ChatSseParser::new();
    let mut anthropic_parser = AnthropicSseParser::new();
    let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
    let mut message = AssistantMessageItem::default();
    let mut usage: Option<ChatUsage> = None;
    let mut finish_reason: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            _ = consumer_dropped.cancelled() => return,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(bytes)) => {
                for event in parser.push(&bytes) {
                    if !handle_event(
                        event,
                        &mut tools,
                        &mut message,
                        &mut usage,
                        &mut finish_reason,
                        &tx_event,
                    )
                    .await
                    {
                        return;
                    }
                }
                // SANDBOX PATCH: D-001. Also decode the Anthropic Messages-API SSE
                // shape (the overlay `ChatSseParser` only understands the OpenAI
                // chat shape and silently drops Anthropic text blocks). The two
                // shapes are disjoint, so feeding the same bytes to both yields at
                // most one decoder's events per stream.
                if !handle_translated_events(
                    anthropic_parser.push(&bytes),
                    &mut tools,
                    &mut message,
                    &mut usage,
                    &mut finish_reason,
                    &tx_event,
                )
                .await
                {
                    return;
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
        if !handle_event(
            event,
            &mut tools,
            &mut message,
            &mut usage,
            &mut finish_reason,
            &tx_event,
        )
        .await
        {
            return;
        }
    }
    if !handle_translated_events(
        anthropic_parser.finish(),
        &mut tools,
        &mut message,
        &mut usage,
        &mut finish_reason,
        &tx_event,
    )
    .await
    {
        return;
    }

    // SANDBOX PATCH: D-001. Close the streamed assistant message (opened on its
    // first text delta) so its full text is recorded, before finalizing tool calls.
    if let Some(item) = message.finish()
        && tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_err()
    {
        return;
    }

    // Finalize assembled tool calls as typed function_call items (ordered by index).
    for accum in tools.into_values() {
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
            return;
        }
    }

    let end_turn = finish_reason
        .as_deref()
        .map(|reason| matches!(reason, "stop" | "tool_calls"));
    let _ = tx_event
        .send(Ok(ResponseEvent::Completed {
            response_id: String::new(),
            token_usage: usage.map(to_token_usage),
            end_turn,
        }))
        .await;
}

/// Routes the Anthropic decoder's [`TranslatedSseEvent`]s: chat events reuse the
/// shared [`handle_event`] mapping, while an in-band stream error aborts the turn
/// (so an Anthropic `error` event surfaces as a visible error instead of a silent
/// blank). Returns `false` when the consumer dropped or the stream errored, so
/// the caller can stop.
async fn handle_translated_events(
    events: Vec<TranslatedSseEvent>,
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    message: &mut AssistantMessageItem,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    for event in events {
        match event {
            TranslatedSseEvent::Chat(chat) => {
                if !handle_event(chat, tools, message, usage, finish_reason, tx_event).await {
                    return false;
                }
            }
            TranslatedSseEvent::StreamError(error_message) => {
                let _ = tx_event
                    .send(Err(CodexErr::Stream(error_message, None)))
                    .await;
                return false;
            }
        }
    }
    true
}

/// Maps one neutral [`ChatStreamEvent`] into `ResponseEvent`(s). Returns `false`
/// when the consumer dropped (send failed) so the caller can stop.
async fn handle_event(
    event: ChatStreamEvent,
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    message: &mut AssistantMessageItem,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    match event {
        ChatStreamEvent::ContentDelta(text) => {
            // SANDBOX PATCH: D-001. Open the assistant message item on the first
            // text delta so the session state machine has an active item to attach
            // the `OutputTextDelta` to (otherwise `session/turn.rs` hits
            // `error_or_panic("OutputTextDelta without active item")`).
            if !message.is_open() {
                let item = message.open();
                if tx_event
                    .send(Ok(ResponseEvent::OutputItemAdded(item)))
                    .await
                    .is_err()
                {
                    return false;
                }
            }
            message.text.push_str(&text);
            tx_event
                .send(Ok(ResponseEvent::OutputTextDelta(text)))
                .await
                .is_ok()
        }
        ChatStreamEvent::ToolCallDelta {
            index,
            id,
            name,
            arguments,
        } => {
            let accum = tools.entry(index).or_default();
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
                .unwrap_or_else(|| format!("chat_item_{index}"));
            tx_event
                .send(Ok(ResponseEvent::ToolCallInputDelta {
                    item_id,
                    call_id: accum.call_id.clone(),
                    delta: arguments,
                }))
                .await
                .is_ok()
        }
        ChatStreamEvent::Finish { reason } => {
            if reason.is_some() {
                *finish_reason = reason;
            }
            true
        }
        ChatStreamEvent::Usage(chat_usage) => {
            // Ignore content-free usage events. This also neutralizes a benign
            // cross-shape false positive: the overlay `ChatSseParser` decodes the
            // Anthropic `message_delta` line's top-level `usage` object as an
            // all-`None` `ChatUsage`, which must not clobber the real token counts
            // the Anthropic parser reports for the same line.
            if chat_usage.prompt_tokens.is_none()
                && chat_usage.completion_tokens.is_none()
                && chat_usage.total_tokens.is_none()
            {
                return true;
            }
            *usage = Some(chat_usage);
            true
        }
    }
}

fn to_token_usage(usage: ChatUsage) -> TokenUsage {
    let input_tokens = usage.prompt_tokens.unwrap_or(0);
    let output_tokens = usage.completion_tokens.unwrap_or(0);
    TokenUsage {
        input_tokens,
        cached_input_tokens: 0,
        output_tokens,
        reasoning_output_tokens: 0,
        total_tokens: usage
            .total_tokens
            .unwrap_or_else(|| input_tokens + output_tokens),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn routing_maps_hint_to_effective_wire_when_anthropic_enabled() {
        assert_eq!(
            effective_wire_api_gated(
                ModelWireRoute::ChatCompletions,
                WireApi::Responses,
                /*anthropic_enabled*/ true,
            ),
            WireApi::ChatCompletions,
        );
        assert_eq!(
            effective_wire_api_gated(
                ModelWireRoute::ProviderDefault,
                WireApi::Responses,
                /*anthropic_enabled*/ true,
            ),
            WireApi::Responses,
        );
    }

    #[test]
    fn routing_falls_back_to_responses_when_anthropic_disabled() {
        assert_eq!(
            effective_wire_api_gated(
                ModelWireRoute::ChatCompletions,
                WireApi::Responses,
                /*anthropic_enabled*/ false,
            ),
            WireApi::Responses,
        );
        assert_eq!(
            effective_wire_api_gated(
                ModelWireRoute::ProviderDefault,
                WireApi::Responses,
                /*anthropic_enabled*/ false,
            ),
            WireApi::Responses,
        );
    }

    #[test]
    fn usage_maps_to_token_usage() {
        let usage = to_token_usage(ChatUsage {
            prompt_tokens: Some(19),
            completion_tokens: Some(4),
            total_tokens: Some(23),
        });
        assert_eq!(usage.input_tokens, 19);
        assert_eq!(usage.output_tokens, 4);
        assert_eq!(usage.total_tokens, 23);
    }

    /// Regression: the chat path MUST open an assistant message item
    /// (`OutputItemAdded`) before the first `OutputTextDelta`. Without it the
    /// session turn loop hits `error_or_panic("OutputTextDelta without active
    /// item")` (it panics in debug builds). The open event must be emitted exactly
    /// once, and `finish()` must yield the full accumulated text as the closing
    /// `OutputItemDone` item.
    #[tokio::test]
    async fn content_delta_opens_assistant_item_before_text_then_finishes() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        for delta in ["Hel", "lo"] {
            assert!(
                handle_event(
                    ChatStreamEvent::ContentDelta(delta.to_string()),
                    &mut tools,
                    &mut message,
                    &mut usage,
                    &mut finish_reason,
                    &tx_event,
                )
                .await
            );
        }
        // Close the message exactly as `run_chat_stream` does at end of stream.
        let done_item = message.finish().expect("assistant message should be open");
        tx_event
            .send(Ok(ResponseEvent::OutputItemDone(done_item)))
            .await
            .expect("send done");
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }

        // First event opens an empty assistant message item.
        match &events[0] {
            ResponseEvent::OutputItemAdded(ResponseItem::Message { role, content, .. }) => {
                assert_eq!(role, "assistant");
                assert!(content.is_empty());
            }
            other => panic!("expected OutputItemAdded(assistant message), got {other:?}"),
        }
        // Then the text deltas, in order.
        assert!(matches!(&events[1], ResponseEvent::OutputTextDelta(t) if t == "Hel"));
        assert!(matches!(&events[2], ResponseEvent::OutputTextDelta(t) if t == "lo"));
        // The item is opened exactly once across multiple deltas.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, ResponseEvent::OutputItemAdded(_)))
                .count(),
            1,
        );
        // Last event closes the message with the full accumulated text.
        match events.last() {
            Some(ResponseEvent::OutputItemDone(ResponseItem::Message {
                role, content, ..
            })) => {
                assert_eq!(role, "assistant");
                assert_eq!(
                    content,
                    &vec![ContentItem::OutputText {
                        text: "Hello".to_string()
                    }]
                );
            }
            other => panic!("expected OutputItemDone(assistant message), got {other:?}"),
        }
    }

    /// A turn with no assistant text (pure tool call) must NOT synthesize an
    /// assistant message item.
    #[test]
    fn no_text_leaves_assistant_message_unopened() {
        let message = AssistantMessageItem::default();
        assert!(!message.is_open());
        assert!(message.finish().is_none());
    }
}
