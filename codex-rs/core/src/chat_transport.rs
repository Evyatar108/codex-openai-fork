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
use codex_protocol::models::ReasoningItemContent;
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

/// SANDBOX PATCH: D-001 reasoning capture. Accumulates streamed Claude
/// chain-of-thought (Copilot's non-standard `delta.reasoning_text`) into the
/// reasoning-item lifecycle the session state machine requires: an
/// `OutputItemAdded(Reasoning)` (active reasoning item opened) BEFORE the first
/// `ReasoningContentDelta` — without an open item the turn loop hits
/// `error_or_panic("ReasoningRawContentDelta without active item")`
/// (`session/turn.rs`) — and a closing `OutputItemDone(Reasoning)` carrying the
/// full CoT as a `ReasoningItemContent::ReasoningText` so it persists to the
/// rollout (`should_serialize_reasoning_content`). The reasoning item MUST close
/// before the assistant `Message` (or any `FunctionCall`) item opens, since
/// `OutputItemAdded` replaces the active item. The same `id` is reused for open
/// and close. `encrypted_content` is `None` on this unsigned plaintext path but is
/// never assumed plaintext-only, so the future signed fast-follow can populate it
/// without reworking this lifecycle. Once closed the item never reopens, so a late
/// straggler reasoning chunk (one network read straddling the last reasoning line
/// and the first content line) can never resurrect it after the message opened.
#[derive(Default)]
struct ReasoningMessageItem {
    id: Option<String>,
    text: String,
    closed: bool,
}

impl ReasoningMessageItem {
    fn is_open(&self) -> bool {
        self.id.is_some() && !self.closed
    }

    /// Marks the reasoning item open and returns the empty reasoning item to send
    /// as `OutputItemAdded`. `content`/`encrypted_content` are `None` at open; the
    /// full CoT text is attached at `finish()`.
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

    /// Returns the finalized reasoning item (full accumulated CoT) to send as
    /// `OutputItemDone`, or `None` when the item is not open (never opened, or
    /// already closed). Marks the item closed so it never reopens.
    fn finish(&mut self) -> Option<ResponseItem> {
        if !self.is_open() {
            return None;
        }
        let id = self.id.clone()?;
        self.closed = true;
        Some(ResponseItem::Reasoning {
            id,
            summary: Vec::new(),
            content: Some(vec![ReasoningItemContent::ReasoningText {
                text: self.text.clone(),
            }]),
            encrypted_content: None,
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
    // SANDBOX PATCH: D-001 reasoning capture. Accumulates Claude chain-of-thought
    // streamed off `delta.reasoning_text`, emitting the reasoning-item lifecycle.
    let mut reasoning = ReasoningMessageItem::default();
    let mut usage: Option<ChatUsage> = None;
    let mut finish_reason: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            _ = consumer_dropped.cancelled() => return,
            chunk = response.chunk() => chunk,
        };
        match chunk {
            Ok(Some(bytes)) => {
                if !drain_chat_chunk(
                    parser.push(&bytes),
                    anthropic_parser.push(&bytes),
                    &mut tools,
                    &mut message,
                    &mut reasoning,
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
    if !drain_chat_chunk(
        parser.finish(),
        anthropic_parser.finish(),
        &mut tools,
        &mut message,
        &mut reasoning,
        &mut usage,
        &mut finish_reason,
        &tx_event,
    )
    .await
    {
        return;
    }

    // SANDBOX PATCH: D-001 reasoning capture. Close any reasoning item still open at
    // stream finish (a pure-reasoning turn with no content/tool would leave it open),
    // before the assistant message and tool-call items are finalized.
    if !close_reasoning_if_open(&mut reasoning, &tx_event).await {
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

/// SANDBOX PATCH: D-001. Drains one raw byte chunk through BOTH SSE parsers (the
/// overlay `ChatSseParser`, which understands the OpenAI chat shape, and the core
/// Anthropic decoder) in WIRE-FAITHFUL order. The two parsers split the wire by
/// shape: assistant text / tool calls come from the overlay parser, while reasoning
/// (`delta.reasoning_text`, with `delta.content:""`) and the Anthropic Messages
/// shape come from the Anthropic decoder — so feeding the same bytes to both yields
/// at most one decoder's meaningful events per stream.
///
/// The Anthropic decoder's events are handled FIRST because reasoning precedes
/// assistant content/tool calls on the wire. When a single network read buffers BOTH
/// the (first) reasoning line and the (first) content line, draining the overlay
/// parser first would emit the content delta — opening the assistant message — before
/// the reasoning item ever opened, violating the reasoning-before-content ordering
/// invariant (and later nulling the active item, tripping
/// `error_or_panic("OutputTextDelta without active item")`). Anthropic-first opens the
/// reasoning item, then the overlay content delta closes it in wire order; it also
/// preserves a trailing reasoning fragment that shares a read with the first content.
/// Returns `false` when the consumer dropped or the stream errored, so the caller can
/// stop.
async fn drain_chat_chunk(
    overlay_events: Vec<ChatStreamEvent>,
    translated_events: Vec<TranslatedSseEvent>,
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    message: &mut AssistantMessageItem,
    reasoning: &mut ReasoningMessageItem,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    if !handle_translated_events(
        translated_events,
        tools,
        message,
        reasoning,
        usage,
        finish_reason,
        tx_event,
    )
    .await
    {
        return false;
    }
    for event in overlay_events {
        if !handle_event(
            event,
            tools,
            message,
            reasoning,
            usage,
            finish_reason,
            tx_event,
        )
        .await
        {
            return false;
        }
    }
    true
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
    reasoning: &mut ReasoningMessageItem,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    for event in events {
        match event {
            TranslatedSseEvent::Chat(chat) => {
                if !handle_event(
                    chat,
                    tools,
                    message,
                    reasoning,
                    usage,
                    finish_reason,
                    tx_event,
                )
                .await
                {
                    return false;
                }
            }
            // SANDBOX PATCH: D-001 reasoning capture. Open the reasoning item on the
            // first delta (so the session has an active item for the
            // `ReasoningContentDelta`), accumulate the full CoT, and stream the chunk.
            // A late straggler after the item has been closed (a content/tool event
            // already finalized it) is dropped rather than reopened — closing the
            // reasoning item before the assistant message is the load-bearing
            // ordering invariant.
            TranslatedSseEvent::ReasoningDelta(text) => {
                if reasoning.closed {
                    continue;
                }
                if !reasoning.is_open() {
                    let item = reasoning.open();
                    if tx_event
                        .send(Ok(ResponseEvent::OutputItemAdded(item)))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
                reasoning.text.push_str(&text);
                if tx_event
                    .send(Ok(ResponseEvent::ReasoningContentDelta {
                        delta: text,
                        content_index: 0,
                    }))
                    .await
                    .is_err()
                {
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

/// SANDBOX PATCH: D-001 reasoning capture. Closes-and-SEALS the reasoning item at a
/// content/tool boundary (or stream finish): emits `OutputItemDone(Reasoning)` if an
/// item is open, then marks the accumulator permanently sealed so a reasoning delta
/// arriving AFTERWARDS can never (re)open a reasoning item behind the assistant
/// message. `drain_chat_chunk`'s anthropic-first ordering already guarantees reasoning
/// opens before same-read content; the unconditional seal is defense-in-depth for a
/// wire that violates the reasoning-precedes-content contract (it turns a would-be
/// ordering violation — and the consumer-side `error_or_panic` — into a safe drop of
/// the stray late reasoning). Returns `false` when the consumer dropped (send failed)
/// so the caller can stop.
async fn close_reasoning_if_open(
    reasoning: &mut ReasoningMessageItem,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    let sent = if let Some(item) = reasoning.finish() {
        tx_event
            .send(Ok(ResponseEvent::OutputItemDone(item)))
            .await
            .is_ok()
    } else {
        true
    };
    // `finish()` already sets `closed` when an item was open; seal the never-opened
    // case too, so a stray reasoning delta after content/tool is dropped, not opened.
    reasoning.closed = true;
    sent
}

/// Maps one neutral [`ChatStreamEvent`] into `ResponseEvent`(s). Returns `false`
/// when the consumer dropped (send failed) so the caller can stop.
async fn handle_event(
    event: ChatStreamEvent,
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    message: &mut AssistantMessageItem,
    reasoning: &mut ReasoningMessageItem,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    match event {
        ChatStreamEvent::ContentDelta(text) => {
            // SANDBOX PATCH: D-001 reasoning capture. Close any open reasoning item
            // BEFORE the assistant message opens — `OutputItemAdded(Message)` replaces
            // the active item, so the reasoning item must already be finalized.
            if !close_reasoning_if_open(reasoning, tx_event).await {
                return false;
            }
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
            // SANDBOX PATCH: D-001 reasoning capture. Close any open reasoning item
            // BEFORE a function-call item is finalized (tool-only turns produce no
            // assistant text, so the reasoning item would otherwise stay active when
            // the `FunctionCall` item arrives).
            if !close_reasoning_if_open(reasoning, tx_event).await {
                return false;
            }
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
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        for delta in ["Hel", "lo"] {
            assert!(
                handle_event(
                    ChatStreamEvent::ContentDelta(delta.to_string()),
                    &mut tools,
                    &mut message,
                    &mut reasoning,
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

    // SANDBOX PATCH: D-001 reasoning capture tests.

    /// AC5: the closing reasoning item carries the full CoT as a
    /// `ReasoningItemContent::ReasoningText` (so `should_serialize_reasoning_content`
    /// persists it), with `encrypted_content: None` on the unsigned path. The open
    /// item carries no content yet, and the item never reopens once closed.
    #[test]
    fn reasoning_item_close_carries_reasoning_text_for_persistence() {
        let mut reasoning = ReasoningMessageItem::default();
        match reasoning.open() {
            ResponseItem::Reasoning {
                content,
                encrypted_content,
                summary,
                ..
            } => {
                assert_eq!(content, None);
                assert_eq!(encrypted_content, None);
                assert!(summary.is_empty());
            }
            other => panic!("expected Reasoning open item, got {other:?}"),
        }
        reasoning.text.push_str("step one ");
        reasoning.text.push_str("step two");
        match reasoning.finish().expect("reasoning should be open") {
            ResponseItem::Reasoning {
                content,
                encrypted_content,
                ..
            } => {
                assert_eq!(
                    content,
                    Some(vec![ReasoningItemContent::ReasoningText {
                        text: "step one step two".to_string(),
                    }])
                );
                assert_eq!(encrypted_content, None);
            }
            other => panic!("expected Reasoning done item, got {other:?}"),
        }
        // Already closed: never reopens.
        assert!(reasoning.finish().is_none());
    }

    /// AC3: a reasoning-then-content turn emits the reasoning lifecycle
    /// (`OutputItemAdded(Reasoning)` → `ReasoningContentDelta`(s) →
    /// `OutputItemDone(Reasoning)`) entirely BEFORE the assistant message
    /// (`OutputItemAdded(Message)` → `OutputTextDelta` → `OutputItemDone(Message)`).
    #[tokio::test]
    async fn reasoning_then_content_emits_reasoning_lifecycle_before_message() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(32);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        // Reasoning arrives first (anthropic-decoder path).
        assert!(
            handle_translated_events(
                vec![
                    TranslatedSseEvent::ReasoningDelta("Let me ".to_string()),
                    TranslatedSseEvent::ReasoningDelta("think.".to_string()),
                ],
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        // Then assistant text (overlay-decoder path) closes the reasoning item first.
        assert!(
            handle_event(
                ChatStreamEvent::ContentDelta("Answer".to_string()),
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        // End-of-stream finalize, mirroring `run_chat_stream`.
        assert!(close_reasoning_if_open(&mut reasoning, &tx_event).await);
        let done = message.finish().expect("assistant message should be open");
        tx_event
            .send(Ok(ResponseEvent::OutputItemDone(done)))
            .await
            .expect("send done");
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }

        assert!(matches!(
            &events[0],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        ));
        assert!(matches!(
            &events[1],
            ResponseEvent::ReasoningContentDelta { delta, content_index: 0 } if delta == "Let me "
        ));
        assert!(matches!(
            &events[2],
            ResponseEvent::ReasoningContentDelta { delta, content_index: 0 } if delta == "think."
        ));
        match &events[3] {
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning {
                content,
                encrypted_content,
                ..
            }) => {
                assert_eq!(
                    content,
                    &Some(vec![ReasoningItemContent::ReasoningText {
                        text: "Let me think.".to_string(),
                    }])
                );
                assert_eq!(encrypted_content, &None);
            }
            other => panic!("expected OutputItemDone(Reasoning), got {other:?}"),
        }
        assert!(matches!(
            &events[4],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        ));
        assert!(matches!(&events[5], ResponseEvent::OutputTextDelta(t) if t == "Answer"));
        assert!(matches!(
            &events[6],
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
        ));
        // The reasoning item is opened exactly once.
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(
                    event,
                    ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
                ))
                .count(),
            1,
        );
    }

    /// AC14: a reasoning-then-tool-call turn (no assistant text) closes the
    /// reasoning item BEFORE the `FunctionCall` item is finalized, so
    /// `session/turn.rs` never sees a function item while the reasoning item is
    /// still the active item. No assistant message item is synthesized.
    #[tokio::test]
    async fn reasoning_then_tool_call_closes_reasoning_before_function_call() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(32);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        assert!(
            handle_translated_events(
                vec![TranslatedSseEvent::ReasoningDelta(
                    "plan the call".to_string()
                )],
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        assert!(
            handle_event(
                ChatStreamEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("get_time".to_string()),
                    arguments: "{}".to_string(),
                },
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        // Finalize exactly as `run_chat_stream` does at end of stream.
        assert!(close_reasoning_if_open(&mut reasoning, &tx_event).await);
        assert!(message.finish().is_none());
        for accum in tools.into_values() {
            let item = ResponseItem::FunctionCall {
                id: None,
                name: accum.name.unwrap_or_default(),
                namespace: None,
                arguments: accum.arguments,
                call_id: accum.call_id.unwrap_or_default(),
                metadata: None,
            };
            tx_event
                .send(Ok(ResponseEvent::OutputItemDone(item)))
                .await
                .expect("send function call");
        }
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }

        let reasoning_done = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. })
                )
            })
            .expect("reasoning item closed");
        let function_done = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    ResponseEvent::OutputItemDone(ResponseItem::FunctionCall { .. })
                )
            })
            .expect("function call finalized");
        assert!(
            reasoning_done < function_done,
            "reasoning must close before the function call; events: {events:?}"
        );
        assert!(!events.iter().any(|event| matches!(
            event,
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
                | ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
        )));
    }

    /// AC4 (transport level): a turn with zero reasoning chunks emits NO reasoning
    /// events at all.
    #[tokio::test]
    async fn no_reasoning_chunks_emit_no_reasoning_events() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        assert!(
            handle_event(
                ChatStreamEvent::ContentDelta("Hi".to_string()),
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        assert!(close_reasoning_if_open(&mut reasoning, &tx_event).await);
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
                    | ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. })
                    | ResponseEvent::ReasoningContentDelta { .. }
            )),
            "no reasoning events expected, got {events:?}"
        );
    }

    /// Regression for the same-read ordering hazard: when ONE network read buffers
    /// both the first reasoning line and the first content line (reasoning not opened
    /// in any prior read), the reasoning item MUST still open and close BEFORE the
    /// assistant message. `drain_chat_chunk` guarantees this by draining the Anthropic
    /// decoder (reasoning) before the overlay parser (content). With overlay-first
    /// draining this fails — the message opens before the reasoning item, and a later
    /// content delta would null the active item and trip `error_or_panic`.
    #[tokio::test]
    async fn single_read_with_reasoning_and_content_keeps_reasoning_first() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(32);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        // One buffer: a reasoning chunk (content:"" + reasoning_text) immediately
        // followed by a content chunk (string content) — the realistic single-read
        // coalesced case where reasoning has never been opened before.
        let bytes = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"\",\"role\":\"assistant\",\"reasoning_text\":\"think\"}}]}\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Answer\"}}]}\n",
        )
        .as_bytes();

        let mut overlay = ChatSseParser::new();
        let mut anthropic = AnthropicSseParser::new();
        assert!(
            drain_chat_chunk(
                overlay.push(bytes),
                anthropic.push(bytes),
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        assert!(close_reasoning_if_open(&mut reasoning, &tx_event).await);
        if let Some(done) = message.finish() {
            tx_event
                .send(Ok(ResponseEvent::OutputItemDone(done)))
                .await
                .expect("send done");
        }
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }

        // The reasoning lifecycle fully precedes the assistant message.
        assert!(matches!(
            &events[0],
            ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
        ));
        assert!(matches!(
            &events[1],
            ResponseEvent::ReasoningContentDelta { delta, content_index: 0 } if delta == "think"
        ));
        assert!(matches!(
            &events[2],
            ResponseEvent::OutputItemDone(ResponseItem::Reasoning { .. })
        ));
        assert!(matches!(
            &events[3],
            ResponseEvent::OutputItemAdded(ResponseItem::Message { .. })
        ));
        assert!(matches!(&events[4], ResponseEvent::OutputTextDelta(t) if t == "Answer"));
        assert!(matches!(
            &events[5],
            ResponseEvent::OutputItemDone(ResponseItem::Message { .. })
        ));
    }

    /// Defense-in-depth for the seal: a reasoning delta arriving AFTER content has
    /// started (a wire that violates the reasoning-precedes-content contract) is
    /// DROPPED, not opened behind the assistant message — so `session/turn.rs` never
    /// sees a reasoning item replace the active message item mid-text.
    #[tokio::test]
    async fn reasoning_delta_after_content_is_dropped_not_opened() {
        let (tx_event, mut rx_event) = mpsc::channel::<Result<ResponseEvent>>(16);
        let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
        let mut message = AssistantMessageItem::default();
        let mut reasoning = ReasoningMessageItem::default();
        let mut usage: Option<ChatUsage> = None;
        let mut finish_reason: Option<String> = None;

        // Content first (opens the message and seals reasoning), with NO prior reasoning.
        assert!(
            handle_event(
                ChatStreamEvent::ContentDelta("ans".to_string()),
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        // A stray reasoning delta afterwards must be dropped (the item is sealed).
        assert!(
            handle_translated_events(
                vec![TranslatedSseEvent::ReasoningDelta("late".to_string())],
                &mut tools,
                &mut message,
                &mut reasoning,
                &mut usage,
                &mut finish_reason,
                &tx_event,
            )
            .await
        );
        drop(tx_event);

        let mut events = Vec::new();
        while let Some(event) = rx_event.recv().await {
            events.push(event.expect("event"));
        }
        assert!(
            !events.iter().any(|event| matches!(
                event,
                ResponseEvent::OutputItemAdded(ResponseItem::Reasoning { .. })
                    | ResponseEvent::ReasoningContentDelta { .. }
            )),
            "stray late reasoning must be dropped, got {events:?}"
        );
    }
}
