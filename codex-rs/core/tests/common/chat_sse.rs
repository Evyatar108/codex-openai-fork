// SANDBOX PATCH: D-001 chat-completions e2e harness (US-008).
//
// Mirrors `responses.rs` for the fork-exclusive Claude-via-Copilot
// `/chat/completions` transport. Provides:
//   * a wiremock mock that mounts `POST .../chat/completions`, serves scripted
//     chat-SSE bodies in sequence, and captures every outbound request body;
//   * `ev_chat_*` builders + `sse(...)` for the OpenAI chat-completions wire
//     shape (including the non-standard `delta.reasoning_text` Copilot streams
//     for Anthropic models) and the `usage` trailing chunk;
//   * a hermetic `COPILOT_API_HOME` token-cache fixture so the transport's
//     `CopilotAuth::new()` / `CopilotHeaderSource::new()` resolve a cached token
//     with NO network (the test-only side-channel was removed in the v0.140.0
//     rebase; this rebuilds only the on-disk cache it needs; zero production
//     change).
//
// Test-only. Closes the gap behind `docs/implementation/patch-surface.md`
// invariant 37 (the named `core/tests/suite/chat_completions.rs` home).
#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::ffi::OsString;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::time::SystemTime;
use std::time::UNIX_EPOCH;

use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Match;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::Request;
use wiremock::Respond;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path_regex;

/// Wiremock mock + request log for the `/chat/completions` path. Mirrors
/// `responses::ResponseMock`: every matched POST body is captured so tests can
/// assert on the outbound `messages[]` / `reasoning_effort`.
#[derive(Clone, Debug)]
pub struct ChatMock {
    requests: Arc<Mutex<Vec<ChatRequest>>>,
}

impl ChatMock {
    fn new() -> Self {
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// All captured chat requests, in arrival order.
    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// The single captured request; panics unless exactly one was captured.
    pub fn single_request(&self) -> ChatRequest {
        let requests = self.requests.lock().unwrap();
        assert_eq!(
            requests.len(),
            1,
            "expected exactly 1 chat request, got {}",
            requests.len()
        );
        requests[0].clone()
    }

    /// The most recent captured request, if any.
    pub fn last_request(&self) -> Option<ChatRequest> {
        self.requests.lock().unwrap().last().cloned()
    }
}

impl Match for ChatMock {
    fn matches(&self, request: &Request) -> bool {
        self.requests
            .lock()
            .unwrap()
            .push(ChatRequest(request.clone()));
        true
    }
}

/// A captured outbound `/chat/completions` request.
#[derive(Clone, Debug)]
pub struct ChatRequest(Request);

impl ChatRequest {
    /// The decoded JSON request body.
    pub fn body_json(&self) -> Value {
        serde_json::from_slice(&self.0.body).expect("chat request body should be JSON")
    }

    /// The outbound `messages[]` array (the chat transport sends `messages`, NOT
    /// the Responses-API `input[]`).
    pub fn messages(&self) -> Vec<Value> {
        self.body_json()
            .get("messages")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    }

    /// The plaintext `content` of every assistant message that carries a string
    /// `content`. The transport replays a persisted reasoning item as a
    /// standalone `{"role":"assistant","content":<text>}` message, so the
    /// replayed chain-of-thought shows up here.
    pub fn assistant_message_contents(&self) -> Vec<String> {
        self.messages()
            .into_iter()
            .filter_map(|message| {
                if message.get("role").and_then(Value::as_str) == Some("assistant") {
                    message
                        .get("content")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                } else {
                    None
                }
            })
            .collect()
    }

    /// Every assistant `tool_calls[]` entry replayed into this chat request.
    pub fn assistant_tool_calls(&self) -> Vec<Value> {
        self.messages()
            .into_iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("assistant"))
            .filter_map(|message| message.get("tool_calls").and_then(Value::as_array).cloned())
            .flatten()
            .collect()
    }

    /// Every tool-role message replayed into this chat request.
    pub fn tool_messages(&self) -> Vec<Value> {
        self.messages()
            .into_iter()
            .filter(|message| message.get("role").and_then(Value::as_str) == Some("tool"))
            .collect()
    }

    /// The top-level `reasoning_effort` field (present when an effort is threaded
    /// to the chat body).
    pub fn reasoning_effort(&self) -> Option<String> {
        self.body_json()
            .get("reasoning_effort")
            .and_then(Value::as_str)
            .map(str::to_string)
    }

    /// The request URL path (e.g. `/v1/chat/completions`).
    pub fn path(&self) -> String {
        self.0.url.path().to_string()
    }
}

/// Serialize chat-SSE `data:` chunks plus the terminal `data: [DONE]` sentinel.
/// The chat-completions SSE shape carries no `event:` lines (unlike `/responses`);
/// each chunk is a single `data: {json}` line.
pub fn sse(chunks: Vec<Value>) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    for chunk in chunks {
        writeln!(&mut out, "data: {chunk}").unwrap();
    }
    out.push_str("data: [DONE]\n");
    out
}

/// A Claude chain-of-thought chunk: `delta.content` is the empty string and the
/// CoT rides the non-standard `delta.reasoning_text` field Copilot streams for
/// Anthropic models (decoded by `core/src/chat_transport/anthropic_sse.rs`).
pub fn ev_chat_reasoning_delta(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":"","role":"assistant","reasoning_text":text}}]})
}

/// An assistant content text delta (`choices[].delta.content`).
pub fn ev_chat_content_delta(text: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"content":text}}]})
}

/// The first fragment of a streamed tool call: carries `id` + `name`. `index` is
/// the content-block position (non-zero / non-contiguous for Claude), keyed on by
/// the overlay parser's per-call assembly.
pub fn ev_chat_tool_call(index: i64, id: &str, name: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"name":name},"id":id,"index":index,"type":"function"}]}}]})
}

/// A later fragment of a streamed tool call: an `arguments` chunk of partial JSON
/// for the call at `index`.
pub fn ev_chat_tool_call_args(index: i64, arguments_fragment: &str) -> Value {
    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"function":{"arguments":arguments_fragment},"index":index}]}}]})
}

/// The choice's terminal `finish_reason` (e.g. `"stop"`, `"tool_calls"`).
pub fn ev_chat_finish(reason: &str) -> Value {
    json!({"choices":[{"index":0,"finish_reason":reason,"delta":{}}]})
}

/// Token usage on a trailing chunk with empty `choices` (the wire reality: usage
/// rides a final chunk after the content/tool deltas).
pub fn ev_chat_usage(prompt_tokens: i64, completion_tokens: i64, total_tokens: i64) -> Value {
    json!({"choices":[],"usage":{"prompt_tokens":prompt_tokens,"completion_tokens":completion_tokens,"total_tokens":total_tokens}})
}

/// Mount a single chat-SSE response. Captures the outbound body on the returned
/// [`ChatMock`].
pub async fn mount_chat_sse_once(server: &MockServer, body: String) -> ChatMock {
    mount_chat_sse_sequence(server, vec![body]).await
}

/// Mount a SEQUENCE of chat-SSE responses served in order, one per `POST
/// .../chat/completions`. Uses a single mock + single request log so every turn's
/// outbound body is captured on the same [`ChatMock`] (`requests()[0]` is turn 1,
/// `requests()[1]` is turn 2, ...). Mirrors `responses::mount_sse_sequence`.
pub async fn mount_chat_sse_sequence(server: &MockServer, bodies: Vec<String>) -> ChatMock {
    struct SeqResponder {
        num_calls: AtomicUsize,
        responses: Vec<String>,
    }

    impl Respond for SeqResponder {
        fn respond(&self, _: &Request) -> ResponseTemplate {
            let call_num = self.num_calls.fetch_add(1, Ordering::SeqCst);
            let body = self
                .responses
                .get(call_num)
                .unwrap_or_else(|| panic!("no chat response for call {call_num}"));
            ResponseTemplate::new(200)
                .insert_header("content-type", "text/event-stream")
                .set_body_string(body.clone())
        }
    }

    let num_calls = bodies.len() as u64;
    let responder = SeqResponder {
        num_calls: AtomicUsize::new(0),
        responses: bodies,
    };

    let chat_mock = ChatMock::new();
    Mock::given(method("POST"))
        .and(path_regex(r".*/chat/completions$"))
        .and(chat_mock.clone())
        .respond_with(responder)
        .up_to_n_times(num_calls)
        .mount(server)
        .await;
    chat_mock
}

/// Hermetic Copilot-auth fixture (US-008 Option A). Points `COPILOT_API_HOME` at a
/// temp dir seeded with a `copilot_token` whose `expires_at` is far in the future,
/// so `CopilotAuth::copilot_token(false)` short-circuits to the cached token with
/// NO network and `CopilotHeaderSource::new()` succeeds fully offline. Restores the
/// prior env value on drop.
///
/// `COPILOT_API_HOME` (and the anthropic routing gates) are PROCESS-GLOBAL, so any
/// test using this fixture MUST run in its own process. The repo's `cargo-nextest`
/// runner guarantees process-per-test isolation; do NOT run such a test under bare
/// `cargo test`.
pub struct CopilotAuthFixture {
    _dir: TempDir,
    prev: Option<OsString>,
}

impl CopilotAuthFixture {
    /// Seed the cache and point `COPILOT_API_HOME` at it.
    pub fn install() -> Self {
        let dir = TempDir::new().expect("create temp COPILOT_API_HOME");
        // `CachedCopilotToken` has NO serde defaults: all three fields are
        // required or `read_cached_copilot_token` fails with "decoding cached
        // Copilot token". Deliberately write NO `github_token` so the cached-token
        // short-circuit is the only path taken.
        let token = json!({
            "token": "test-copilot-token",
            "expires_at": far_future_epoch_seconds(),
            "refresh_in": 1800u64,
        });
        std::fs::write(
            dir.path().join("copilot_token"),
            serde_json::to_vec(&token).unwrap(),
        )
        .expect("write seeded copilot_token");

        let prev = std::env::var_os("COPILOT_API_HOME");
        // SAFETY: a single-test process under nextest owns its environment; the
        // value is restored on drop. (Process-global env mutation is `unsafe` in
        // edition 2024.)
        unsafe {
            std::env::set_var("COPILOT_API_HOME", dir.path());
        }
        Self { _dir: dir, prev }
    }
}

impl Drop for CopilotAuthFixture {
    fn drop(&mut self) {
        // SAFETY: restoring the prior value in the same single-test process.
        unsafe {
            match &self.prev {
                Some(value) => std::env::set_var("COPILOT_API_HOME", value),
                None => std::env::remove_var("COPILOT_API_HOME"),
            }
        }
    }
}

fn far_future_epoch_seconds() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    // +10 years: comfortably past the `now + 60` freshness check.
    now + 10 * 365 * 24 * 60 * 60
}
