// SANDBOX PATCH: D-001 Anthropic Messages-API SSE decoding for the chat transport.
//
// The overlay `codex_copilot::ChatSseParser` only decodes the OpenAI
// chat-completions SSE shape (`choices[].delta.content` / `.tool_calls`). When
// the Anthropic-gated Copilot endpoint streams the **Anthropic Messages-API SSE
// shape** instead (`event: content_block_delta` / `text_delta`), every `data:`
// line parses as a `ChatChunk` with empty `choices`, so the overlay parser emits
// no `ContentDelta` and assistant TEXT is silently dropped (tool calls emitted in
// the OpenAI `tool_calls` shape still surface — hence the "tools work, text
// blank" symptom).
//
// This module lives in `codex-core` (not the overlay) only because the overlay
// crate is off-limits to this change; it decodes the Anthropic shape into the
// SAME neutral `codex_copilot::ChatStreamEvent`s the overlay already yields, so
// `chat_transport::handle_event` maps them through the existing path with no new
// `ResponseEvent` surface. `run_chat_stream` feeds each raw byte chunk to BOTH
// this parser and the overlay parser; the two wire shapes are disjoint (OpenAI
// lines carry `choices` and no top-level `type`; Anthropic lines carry `type`
// and no `choices`), so at most one parser yields meaningful events per stream
// and the OpenAI path is unchanged.
//
// As a defensive hedge it also recovers assistant text from OpenAI chunks whose
// `delta.content` is a structured ARRAY of content parts (`[{"type":"text",
// "text":"..."}]`) — the overlay parser hard-drops those whole chunks because its
// `content: Option<String>` fails to deserialize an array. This branch fires only
// for array-shaped content, so string content / pure tool-call chunks stay with
// the overlay parser and never double-emit.

use codex_copilot::ChatStreamEvent;
use codex_copilot::ChatUsage;
use serde_json::Value;

/// One decoded event from the Anthropic Messages-API SSE stream. Either a neutral
/// chat event (mapped downstream exactly like the overlay parser's events) or an
/// in-band stream error that must abort the turn instead of silently blanking.
pub(crate) enum TranslatedSseEvent {
    Chat(ChatStreamEvent),
    StreamError(String),
}

/// Incremental parser for an Anthropic Messages-API SSE byte stream.
///
/// Feed raw response bytes via [`AnthropicSseParser::push`] as they arrive; it
/// returns any [`TranslatedSseEvent`]s decodable from newly-completed `data:`
/// lines. Bytes are buffered until a `\n` so multi-byte UTF-8 sequences split
/// across chunks are never decoded mid-character. The terminal `data: [DONE]`
/// sentinel and `event:`/comment lines are ignored.
#[derive(Debug, Default)]
pub(crate) struct AnthropicSseParser {
    buffer: Vec<u8>,
    /// `input_tokens` captured from `message_start` so the `message_delta` usage
    /// event (which only carries `output_tokens`) can report a complete `Usage`.
    input_tokens: Option<i64>,
}

impl AnthropicSseParser {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Feed a chunk of response bytes; returns events from completed SSE lines.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Vec<TranslatedSseEvent> {
        self.buffer.extend_from_slice(bytes);
        let mut events = Vec::new();
        while let Some(newline) = self.buffer.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buffer.drain(..=newline).collect();
            self.decode_line(&line, &mut events);
        }
        events
    }

    /// Flush any trailing line left unterminated at stream end.
    pub(crate) fn finish(&mut self) -> Vec<TranslatedSseEvent> {
        let mut events = Vec::new();
        if !self.buffer.is_empty() {
            let line = std::mem::take(&mut self.buffer);
            self.decode_line(&line, &mut events);
        }
        events
    }

    fn decode_line(&mut self, line: &[u8], events: &mut Vec<TranslatedSseEvent>) {
        let line = String::from_utf8_lossy(line);
        let line = line.trim_end_matches(['\r', '\n']);
        let Some(data) = line.strip_prefix("data:") else {
            return;
        };
        let data = data.trim();
        if data.is_empty() || data == "[DONE]" {
            return;
        }
        let Ok(value) = serde_json::from_str::<Value>(data) else {
            // Unknown/garbled chunk: skip rather than abort the stream.
            return;
        };
        if let Some(event_type) = value.get("type").and_then(Value::as_str) {
            self.decode_anthropic_event(event_type, &value, events);
        } else if value.get("choices").is_some() {
            decode_openai_structured_content(&value, events);
        }
    }

    fn decode_anthropic_event(
        &mut self,
        event_type: &str,
        value: &Value,
        events: &mut Vec<TranslatedSseEvent>,
    ) {
        match event_type {
            "message_start" => {
                if let Some(input) = value
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_i64)
                {
                    self.input_tokens = Some(input);
                }
            }
            "content_block_start" => {
                let block = value.get("content_block");
                let is_tool_use =
                    block.and_then(|b| b.get("type")).and_then(Value::as_str) == Some("tool_use");
                if is_tool_use {
                    let index = value
                        .get("index")
                        .and_then(Value::as_i64)
                        .unwrap_or_default();
                    let id = block
                        .and_then(|b| b.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    let name = block
                        .and_then(|b| b.get("name"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    events.push(TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments: String::new(),
                    }));
                }
            }
            "content_block_delta" => {
                let index = value
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or_default();
                let delta = value.get("delta");
                let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);
                match delta_type {
                    Some("text_delta") => {
                        if let Some(text) =
                            delta.and_then(|d| d.get("text")).and_then(Value::as_str)
                            && !text.is_empty()
                        {
                            events.push(TranslatedSseEvent::Chat(ChatStreamEvent::ContentDelta(
                                text.to_string(),
                            )));
                        }
                    }
                    Some("input_json_delta") => {
                        if let Some(partial) = delta
                            .and_then(|d| d.get("partial_json"))
                            .and_then(Value::as_str)
                        {
                            events.push(TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
                                index,
                                id: None,
                                name: None,
                                arguments: partial.to_string(),
                            }));
                        }
                    }
                    // `thinking_delta` / `signature_delta` are reasoning content and
                    // are intentionally not surfaced as assistant text here.
                    _ => {}
                }
            }
            "message_delta" => {
                let reason = value
                    .pointer("/delta/stop_reason")
                    .and_then(Value::as_str)
                    .map(map_stop_reason);
                events.push(TranslatedSseEvent::Chat(ChatStreamEvent::Finish { reason }));

                let output_tokens = value
                    .pointer("/usage/output_tokens")
                    .and_then(Value::as_i64);
                if output_tokens.is_some() || self.input_tokens.is_some() {
                    let total_tokens = match (self.input_tokens, output_tokens) {
                        (Some(prompt), Some(output)) => Some(prompt + output),
                        _ => None,
                    };
                    events.push(TranslatedSseEvent::Chat(ChatStreamEvent::Usage(
                        ChatUsage {
                            prompt_tokens: self.input_tokens,
                            completion_tokens: output_tokens,
                            total_tokens,
                        },
                    )));
                }
            }
            "error" => {
                let message = value
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("anthropic stream error");
                events.push(TranslatedSseEvent::StreamError(message.to_string()));
            }
            // `ping`, `content_block_stop`, `message_stop` carry no renderable data.
            _ => {}
        }
    }
}

/// Maps an Anthropic `stop_reason` to the finish-reason vocabulary
/// `run_chat_stream` expects (`"stop"` / `"tool_calls"` are treated as a clean
/// end-of-turn). Unknown and refusal reasons map to the terminal `"stop"` so a
/// surprise value never accidentally signals "continue".
fn map_stop_reason(reason: &str) -> String {
    match reason {
        "tool_use" => "tool_calls",
        "max_tokens" => "length",
        // `end_turn`, `stop_sequence`, `refusal`, `pause_turn`, and any unknown
        // reason terminate the turn.
        _ => "stop",
    }
    .to_string()
}

/// Recovers assistant text (and any co-located tool calls / finish reason) from an
/// OpenAI chat chunk whose `delta.content` is a structured ARRAY of content parts.
/// The overlay `ChatSseParser` hard-drops such a chunk because its
/// `content: Option<String>` rejects an array, so this branch is purely additive:
/// it fires only for array-shaped content, leaving string content and pure
/// tool-call chunks to the overlay parser (no double-emit).
fn decode_openai_structured_content(value: &Value, events: &mut Vec<TranslatedSseEvent>) {
    let Some(choice) = value.pointer("/choices/0") else {
        return;
    };
    let delta = choice.get("delta");
    let Some(parts) = delta
        .and_then(|d| d.get("content"))
        .and_then(Value::as_array)
    else {
        return;
    };

    let mut text = String::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) == Some("text")
            && let Some(chunk) = part.get("text").and_then(Value::as_str)
        {
            text.push_str(chunk);
        }
    }
    if !text.is_empty() {
        events.push(TranslatedSseEvent::Chat(ChatStreamEvent::ContentDelta(
            text,
        )));
    }

    if let Some(tool_calls) = delta
        .and_then(|d| d.get("tool_calls"))
        .and_then(Value::as_array)
    {
        for tool_call in tool_calls {
            events.push(TranslatedSseEvent::Chat(ChatStreamEvent::ToolCallDelta {
                index: tool_call
                    .get("index")
                    .and_then(Value::as_i64)
                    .unwrap_or_default(),
                id: tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                name: tool_call
                    .pointer("/function/name")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                arguments: tool_call
                    .pointer("/function/arguments")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            }));
        }
    }

    if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
        events.push(TranslatedSseEvent::Chat(ChatStreamEvent::Finish {
            reason: Some(reason.to_string()),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Collects only the `Chat` events, panicking on any in-band stream error.
    fn parse_chat(sse: &str) -> Vec<ChatStreamEvent> {
        let mut parser = AnthropicSseParser::new();
        let mut events = parser.push(sse.as_bytes());
        events.extend(parser.finish());
        events
            .into_iter()
            .map(|event| match event {
                TranslatedSseEvent::Chat(chat) => chat,
                TranslatedSseEvent::StreamError(message) => {
                    panic!("unexpected stream error: {message}")
                }
            })
            .collect()
    }

    fn collected_text(events: &[ChatStreamEvent]) -> String {
        events
            .iter()
            .filter_map(|event| match event {
                ChatStreamEvent::ContentDelta(text) => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn text_only_anthropic_stream_surfaces_assistant_text() {
        let sse = concat!(
            "event: message_start\n",
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"usage\":{\"input_tokens\":7,\"output_tokens\":0}}}\n",
            "event: content_block_start\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n",
            "event: content_block_delta\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n",
            "event: content_block_stop\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "event: message_delta\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":4}}\n",
            "event: message_stop\n",
            "data: {\"type\":\"message_stop\"}\n",
        );
        let events = parse_chat(sse);
        assert_eq!(collected_text(&events), "Hello");
        assert!(events.contains(&ChatStreamEvent::Finish {
            reason: Some("stop".to_string()),
        }));
        assert!(events.contains(&ChatStreamEvent::Usage(ChatUsage {
            prompt_tokens: Some(7),
            completion_tokens: Some(4),
            total_tokens: Some(11),
        })));
    }

    #[test]
    fn mixed_text_and_tool_use_surfaces_both() {
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_2\",\"usage\":{\"input_tokens\":3,\"output_tokens\":0}}}\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Let me check.\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n",
            "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\"\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\": \\\"Paris\\\"}\"}}\n",
            "data: {\"type\":\"content_block_stop\",\"index\":1}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":9}}\n",
            "data: {\"type\":\"message_stop\"}\n",
        );
        let events = parse_chat(sse);

        // Text is surfaced.
        assert_eq!(collected_text(&events), "Let me check.");

        // Tool call assembles to valid JSON, keyed on its (non-zero) block index.
        let mut id = None;
        let mut name = None;
        let mut args = String::new();
        for event in &events {
            if let ChatStreamEvent::ToolCallDelta {
                index,
                id: ev_id,
                name: ev_name,
                arguments,
            } = event
            {
                assert_eq!(*index, 1);
                if ev_id.is_some() {
                    id = ev_id.clone();
                }
                if ev_name.is_some() {
                    name = ev_name.clone();
                }
                args.push_str(arguments);
            }
        }
        assert_eq!(id.as_deref(), Some("toolu_1"));
        assert_eq!(name.as_deref(), Some("get_weather"));
        let parsed: Value = serde_json::from_str(&args).expect("valid JSON args");
        assert_eq!(parsed["location"], "Paris");
        assert!(events.contains(&ChatStreamEvent::Finish {
            reason: Some("tool_calls".to_string()),
        }));
    }

    #[test]
    fn two_tool_use_blocks_keep_non_contiguous_indices_distinct() {
        let sse = concat!(
            "data: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_a\",\"name\":\"get_weather\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\":\\\"Paris\\\"}\"}}\n",
            "data: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_b\",\"name\":\"get_weather\"}}\n",
            "data: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"location\\\":\\\"Tokyo\\\"}\"}}\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n",
        );
        let events = parse_chat(sse);

        let mut by_index: std::collections::BTreeMap<i64, (Option<String>, String)> =
            std::collections::BTreeMap::new();
        for event in &events {
            if let ChatStreamEvent::ToolCallDelta {
                index,
                id,
                arguments,
                ..
            } = event
            {
                let entry = by_index.entry(*index).or_default();
                if id.is_some() {
                    entry.0 = id.clone();
                }
                entry.1.push_str(arguments);
            }
        }
        assert_eq!(by_index.len(), 2);
        assert_eq!(by_index[&2].0.as_deref(), Some("toolu_a"));
        assert_eq!(by_index[&3].0.as_deref(), Some("toolu_b"));
        let v2: Value = serde_json::from_str(&by_index[&2].1).unwrap();
        let v3: Value = serde_json::from_str(&by_index[&3].1).unwrap();
        assert_eq!(v2["location"], "Paris");
        assert_eq!(v3["location"], "Tokyo");
    }

    #[test]
    fn openai_shaped_stream_yields_no_anthropic_events() {
        // OpenAI chat-completions chunks (string content + tool_calls) carry no
        // top-level `type`; the Anthropic parser must ignore them entirely so the
        // overlay parser stays the sole decoder for that shape (no double-emit).
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"date\",\"arguments\":\"{}\"}}]}}]}\n",
            "data: {\"choices\":[{\"index\":0,\"finish_reason\":\"stop\",\"delta\":{}}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n",
            "data: [DONE]\n",
        );
        let events = parse_chat(sse);
        assert!(events.is_empty(), "expected no events, got {events:?}");
    }

    #[test]
    fn openai_structured_content_array_recovers_text_and_tools() {
        // A `delta.content` ARRAY is dropped wholesale by the overlay parser
        // (its `content: Option<String>` rejects an array), so this parser must
        // recover the text (and any co-located tool call) from such a chunk.
        let sse = concat!(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"Hi there\"}]}}]}\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":[{\"type\":\"text\",\"text\":\"!\"}],\"tool_calls\":[{\"index\":0,\"id\":\"call_9\",\"function\":{\"name\":\"date\",\"arguments\":\"{}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
        );
        let events = parse_chat(sse);
        assert_eq!(collected_text(&events), "Hi there!");
        assert!(events.iter().any(|event| matches!(
            event,
            ChatStreamEvent::ToolCallDelta { id, name, .. }
                if id.as_deref() == Some("call_9") && name.as_deref() == Some("date")
        )));
        assert!(events.contains(&ChatStreamEvent::Finish {
            reason: Some("tool_calls".to_string()),
        }));
    }

    #[test]
    fn openai_string_content_is_left_to_overlay_parser() {
        // A plain string `delta.content` must NOT be decoded here (the overlay
        // parser owns it); decoding it too would double-render every OpenAI turn.
        let sse = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"Hello\"}}]}\n";
        let events = parse_chat(sse);
        assert!(events.is_empty(), "expected no events, got {events:?}");
    }

    #[test]
    fn anthropic_error_event_surfaces_stream_error() {
        let sse = concat!(
            "event: error\n",
            "data: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n",
        );
        let mut parser = AnthropicSseParser::new();
        let mut events = parser.push(sse.as_bytes());
        events.extend(parser.finish());
        assert_eq!(events.len(), 1);
        match &events[0] {
            TranslatedSseEvent::StreamError(message) => assert_eq!(message, "Overloaded"),
            TranslatedSseEvent::Chat(event) => panic!("expected stream error, got {event:?}"),
        }
    }

    #[test]
    fn multibyte_text_split_across_push_boundaries_is_not_corrupted() {
        // The smiley "😀" is 4 UTF-8 bytes; split the `data:` line mid-character
        // across two pushes and assert the reassembled text is intact.
        let line = "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"😀\"}}\n";
        let bytes = line.as_bytes();
        let split = bytes.len() - 6; // mid multi-byte char + before newline
        let mut parser = AnthropicSseParser::new();
        let mut events = parser.push(&bytes[..split]);
        assert!(events.is_empty());
        events.extend(parser.push(&bytes[split..]));
        let chat: Vec<ChatStreamEvent> = events
            .into_iter()
            .map(|event| match event {
                TranslatedSseEvent::Chat(chat) => chat,
                TranslatedSseEvent::StreamError(message) => panic!("unexpected error: {message}"),
            })
            .collect();
        assert_eq!(chat, vec![ChatStreamEvent::ContentDelta("😀".to_string())]);
    }

    #[test]
    fn stop_reason_mapping_is_terminal_safe() {
        assert_eq!(map_stop_reason("end_turn"), "stop");
        assert_eq!(map_stop_reason("stop_sequence"), "stop");
        assert_eq!(map_stop_reason("refusal"), "stop");
        assert_eq!(map_stop_reason("pause_turn"), "stop");
        assert_eq!(map_stop_reason("something_new"), "stop");
        assert_eq!(map_stop_reason("tool_use"), "tool_calls");
        assert_eq!(map_stop_reason("max_tokens"), "length");
    }
}
