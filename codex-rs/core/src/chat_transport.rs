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
use codex_copilot::ChatStreamEvent;
use codex_copilot::ChatSseParser;
use codex_copilot::ChatUsage;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_copilot::build_chat_request_body;
use codex_copilot::payload::Initiator;
use codex_copilot::payload::request_initiator;
use codex_copilot::send_chat_request;
use codex_login::default_client::build_reqwest_client;
use codex_model_provider_info::WireApi;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ModelWireRoute;
use codex_protocol::protocol::TokenUsage;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::client_common::ResponseStream;

const CHANNEL_CAPACITY: usize = 1600;
const COPILOT_BASE_URL: &str = "https://api.githubcopilot.com";

/// Maps a model's protocol-local route hint to the EFFECTIVE wire protocol at the
/// dispatch boundary. This is the single place `ModelWireRoute` becomes a `WireApi`:
/// a chat-hinted model routes to `ChatCompletions`, everything else to the provider's
/// wire. See `docs/implementation/patch-surface.md` §14 invariant 35.
pub(crate) fn effective_wire_api(route: ModelWireRoute, provider_wire: WireApi) -> WireApi {
    match route {
        ModelWireRoute::ChatCompletions => WireApi::ChatCompletions,
        ModelWireRoute::ProviderDefault => provider_wire,
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

/// Stream a turn for a chat-routed (Claude-via-Copilot) model.
///
/// `responses_body` is the serialized `ResponsesApiRequest` core already builds;
/// `base_url` is the Copilot endpoint (defaults to `api.githubcopilot.com`).
pub(crate) async fn stream_chat_completions(
    responses_body: serde_json::Value,
    model_slug: &str,
    base_url: Option<&str>,
) -> Result<ResponseStream> {
    let chat_body = build_chat_request_body(&responses_body, model_slug)
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
    let mut tools: BTreeMap<i64, ToolAccumulator> = BTreeMap::new();
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
                    if !handle_event(event, &mut tools, &mut usage, &mut finish_reason, &tx_event)
                        .await
                    {
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
        if !handle_event(event, &mut tools, &mut usage, &mut finish_reason, &tx_event).await {
            return;
        }
    }

    // Finalize assembled tool calls as typed function_call items (ordered by index).
    for accum in tools.into_values() {
        let item = ResponseItem::FunctionCall {
            id: None,
            name: accum.name.unwrap_or_default(),
            namespace: None,
            arguments: accum.arguments,
            call_id: accum.call_id.unwrap_or_default(),
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

/// Maps one neutral [`ChatStreamEvent`] into `ResponseEvent`(s). Returns `false`
/// when the consumer dropped (send failed) so the caller can stop.
async fn handle_event(
    event: ChatStreamEvent,
    tools: &mut BTreeMap<i64, ToolAccumulator>,
    usage: &mut Option<ChatUsage>,
    finish_reason: &mut Option<String>,
    tx_event: &mpsc::Sender<Result<ResponseEvent>>,
) -> bool {
    match event {
        ChatStreamEvent::ContentDelta(text) => {
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
    fn routing_maps_hint_to_effective_wire() {
        // A chat-hinted model routes to ChatCompletions (never Responses).
        assert_eq!(
            effective_wire_api(ModelWireRoute::ChatCompletions, WireApi::Responses),
            WireApi::ChatCompletions,
        );
        // A provider-default model keeps the provider's wire (Responses for GPT).
        assert_eq!(
            effective_wire_api(ModelWireRoute::ProviderDefault, WireApi::Responses),
            WireApi::Responses,
        );
        // A Claude row never reaches the Responses transport.
        assert_ne!(
            effective_wire_api(ModelWireRoute::ChatCompletions, WireApi::Responses),
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
}
