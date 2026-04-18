use std::convert::Infallible;
use std::sync::Arc;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::extract::ws::Message as AxumWsMessage;
use axum::extract::ws::WebSocket;
use axum::extract::ws::WebSocketUpgrade;
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use bytes::Bytes;
use codex_client::maybe_build_rustls_client_config_with_custom_ca;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotClient;
use codex_copilot::RequestContext;
use codex_copilot::payload::Initiator;
use codex_copilot::payload::is_probe_request;
use codex_copilot::payload::normalize_payload;
use codex_copilot::payload::request_initiator;
use codex_copilot::payload::request_vision;
use codex_copilot::payload::synthetic_empty_response;
use codex_utils_rustls_provider::ensure_rustls_crypto_provider;
use eventsource_stream::Eventsource;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::Value;
use tokio::net::TcpStream;
use tokio_tungstenite::Connector;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::connect_async_tls_with_config;
use tokio_tungstenite::tungstenite::Message as TungsteniteMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

#[derive(Clone)]
struct GatewayState {
    client: CopilotClient,
}

pub async fn run_server(port: u16) -> anyhow::Result<()> {
    let auth = CopilotAuth::new()?;
    let client = CopilotClient::new(auth)?;
    let state = GatewayState { client };
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("binding codex-copilot-gateway to 127.0.0.1:{port}"))?;
    axum::serve(listener, app(state)).await?;
    Ok(())
}

pub async fn login(force: bool) -> anyhow::Result<Option<String>> {
    let auth = CopilotAuth::new()?;
    auth.login(force).await
}

fn app(state: GatewayState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/models", get(models))
        .route("/v1/models", get(models))
        .route("/responses", get(responses_ws).post(responses_http))
        .route("/v1/responses", get(responses_ws).post(responses_http))
        .with_state(Arc::new(state))
}

async fn healthz() -> impl IntoResponse {
    Json(serde_json::json!({ "ok": true }))
}

async fn models(State(state): State<Arc<GatewayState>>) -> Response {
    match state.client.get_models().await {
        Ok(models) => Json(models).into_response(),
        Err(error) => error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn responses_http(
    State(state): State<Arc<GatewayState>>,
    Json(mut payload): Json<Value>,
) -> Response {
    if is_probe_request(&payload) {
        let model = payload.get("model").and_then(Value::as_str);
        return Json(synthetic_empty_response(model)).into_response();
    }

    normalize_payload(&mut payload);
    let stream_requested = payload
        .get("stream")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let request_context =
        RequestContext::new(request_initiator(&payload), request_vision(&payload));

    let response = match state
        .client
        .create_response(&payload, &request_context)
        .await
    {
        Ok(response) => response,
        Err(error) => return error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    };

    if !response.status().is_success() {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "(unable to read body)".to_string());
        eprintln!(
            "codex-copilot-gateway: upstream responses call failed with status {status}: {body}"
        );
        return upstream_error_response(status);
    }

    if stream_requested {
        return streaming_response(response);
    }

    match response.json::<Value>().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

async fn responses_ws(
    ws: WebSocketUpgrade,
    State(state): State<Arc<GatewayState>>,
    client_headers: HeaderMap,
) -> Response {
    let auth = state.client.auth().clone();

    let (upstream_stream, _resp) = match connect_upstream_ws(&auth, &client_headers, false).await {
        Ok(value) => value,
        Err(WsConnectError::Unauthorized) => {
            if let Err(error) = auth.invalidate_cached_copilot_token() {
                return error_response(StatusCode::BAD_GATEWAY, error.to_string());
            }
            match connect_upstream_ws(&auth, &client_headers, true).await {
                Ok(value) => value,
                Err(WsConnectError::Unauthorized) => {
                    return error_response(
                        StatusCode::UNAUTHORIZED,
                        "upstream rejected websocket handshake".to_string(),
                    );
                }
                Err(WsConnectError::Other(error)) => {
                    return error_response(StatusCode::BAD_GATEWAY, error.to_string());
                }
            }
        }
        Err(WsConnectError::Other(error)) => {
            return error_response(StatusCode::BAD_GATEWAY, error.to_string());
        }
    };

    ws.on_upgrade(move |client_ws| async move {
        pump_bidirectional(client_ws, upstream_stream).await;
    })
}

enum WsConnectError {
    Unauthorized,
    Other(anyhow::Error),
}

impl From<anyhow::Error> for WsConnectError {
    fn from(value: anyhow::Error) -> Self {
        WsConnectError::Other(value)
    }
}

async fn connect_upstream_ws(
    auth: &CopilotAuth,
    client_headers: &HeaderMap,
    force_token_refresh: bool,
) -> Result<
    (
        WebSocketStream<MaybeTlsStream<TcpStream>>,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    WsConnectError,
> {
    ensure_rustls_crypto_provider();

    let context = RequestContext::new(Initiator::Agent, /*vision*/ false);
    let auth_headers = auth
        .request_headers(
            context.initiator.clone(),
            &context.request_id,
            context.interaction_id.as_deref(),
            /*vision*/ false,
            force_token_refresh,
        )
        .await
        .context("building Copilot auth headers for websocket")?;

    let base = auth.copilot_base_url();
    let ws_url = if let Some(host) = base.strip_prefix("https://") {
        format!("wss://{host}/responses")
    } else if let Some(host) = base.strip_prefix("http://") {
        format!("ws://{host}/responses")
    } else {
        format!("{base}/responses")
    };

    let mut request = ws_url
        .as_str()
        .into_client_request()
        .context("building websocket client request")?;

    let headers_mut = request.headers_mut();
    for (name, value) in &auth_headers {
        // Skip JSON content-negotiation headers; they belong to HTTP body, not the WS handshake.
        if name == axum::http::header::CONTENT_TYPE || name == axum::http::header::ACCEPT {
            continue;
        }
        headers_mut.insert(name.clone(), value.clone());
    }
    // Forward Responses-protocol headers only. Do NOT forward
    // `Sec-WebSocket-Extensions` — axum's server leg doesn't negotiate
    // `permessage-deflate`, so if we let upstream turn it on we'd receive
    // compressed frames our tungstenite client can't decode, and the stream
    // hangs until codex-core's idle timeout retries over HTTP (~5 min).
    for forwarded in ["openai-beta", "x-codex-turn-state", "sec-websocket-protocol"] {
        if let Some(value) = client_headers.get(forwarded) {
            headers_mut.insert(forwarded, value.clone());
        }
    }

    let connector = maybe_build_rustls_client_config_with_custom_ca()
        .context("configuring websocket TLS")?
        .map(Connector::Rustls);

    match connect_async_tls_with_config(request, None, false, connector).await {
        Ok(value) => Ok(value),
        Err(tokio_tungstenite::tungstenite::Error::Http(response))
            if response.status() == StatusCode::UNAUTHORIZED && !force_token_refresh =>
        {
            Err(WsConnectError::Unauthorized)
        }
        Err(error) => {
            eprintln!("codex-copilot-gateway: upstream websocket connect failed: {error}");
            Err(WsConnectError::Other(anyhow::anyhow!(
                "upstream websocket connect failed: {error}"
            )))
        }
    }
}

async fn pump_bidirectional(
    mut client_ws: WebSocket,
    mut upstream_ws: WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    // Phase 1: intercept codex-core's startup WebSocket prewarm. The prewarm sends
    // `response.create` with `generate=false` and no input/previous_response_id.
    // OpenAI accepts this; Copilot rejects it with
    // `bad_request: One of "input" or ... must be provided`. That error is a
    // non-terminal event in codex-core's parser, so the prewarm stream_request
    // blocks on the idle timeout (~5 min) before the first real turn can run.
    //
    // We stay in this serial loop ONLY while the client keeps sending warmup frames.
    // As soon as a real frame arrives, we forward it to upstream and fall through to
    // the full-duplex split pump below so that backpressure on one direction cannot
    // block the other.
    loop {
        let Some(Ok(msg)) = client_ws.recv().await else {
            let _ = client_ws.close().await;
            let _ = upstream_ws.close(None).await;
            return;
        };

        if let AxumWsMessage::Text(ref text) = msg
            && is_warmup_request(text.as_str())
        {
            // Use an empty `id` so codex-core's `prepare_websocket_request` falls
            // through its "no previous response id" guard
            // (core/src/client.rs: `if last_response.response_id.is_empty()`) and
            // does NOT chain the next turn with `previous_response_id`, which would
            // fail Copilot's `resp*` prefix check.
            let synthetic = "{\"type\":\"response.completed\",\"response\":{\"id\":\"\"}}";
            if client_ws
                .send(AxumWsMessage::Text(synthetic.into()))
                .await
                .is_err()
            {
                let _ = upstream_ws.close(None).await;
                return;
            }
            continue;
        }

        let Some(forwarded) = axum_to_tungstenite(msg) else { continue };
        let is_close = matches!(forwarded, TungsteniteMessage::Close(_));
        if upstream_ws.send(forwarded).await.is_err() {
            let _ = client_ws.close().await;
            return;
        }
        if is_close {
            // Client shut down before a real turn; drain any in-flight upstream
            // frames to the client until upstream closes too.
            drain_upstream_until_close(&mut client_ws, &mut upstream_ws).await;
            let _ = client_ws.close().await;
            let _ = upstream_ws.close(None).await;
            return;
        }
        break;
    }

    // Phase 2: full-duplex pump. Split each socket into independent sink/stream halves
    // so reads in one direction never wait on writes in the other.
    let (mut client_tx, mut client_rx) = client_ws.split();
    let (mut upstream_tx, mut upstream_rx) = upstream_ws.split();

    let client_to_upstream = async {
        while let Some(Ok(msg)) = client_rx.next().await {
            let Some(forwarded) = axum_to_tungstenite(msg) else { continue };
            let is_close = matches!(forwarded, TungsteniteMessage::Close(_));
            if upstream_tx.send(forwarded).await.is_err() { break; }
            if is_close { break; }
        }
        let _ = upstream_tx.close().await;
    };

    let upstream_to_client = async {
        while let Some(Ok(msg)) = upstream_rx.next().await {
            let Some(forwarded) = tungstenite_to_axum(msg) else { continue };
            let is_close = matches!(forwarded, AxumWsMessage::Close(_));
            if client_tx.send(forwarded).await.is_err() { break; }
            if is_close { break; }
        }
        let _ = client_tx.close().await;
    };

    // Each half runs to EOF independently so that a close frame in one direction
    // still gets its reply drained through the proxy rather than dropped.
    tokio::join!(client_to_upstream, upstream_to_client);
}

async fn drain_upstream_until_close(
    client_ws: &mut WebSocket,
    upstream_ws: &mut WebSocketStream<MaybeTlsStream<TcpStream>>,
) {
    while let Some(Ok(msg)) = upstream_ws.next().await {
        let Some(forwarded) = tungstenite_to_axum(msg) else { continue };
        let is_close = matches!(forwarded, AxumWsMessage::Close(_));
        if client_ws.send(forwarded).await.is_err() {
            break;
        }
        if is_close {
            break;
        }
    }
}

fn is_warmup_request(text: &str) -> bool {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return false;
    };
    if value.get("type").and_then(Value::as_str) != Some("response.create") {
        return false;
    }
    if value.get("generate").and_then(Value::as_bool) != Some(false) {
        return false;
    }
    // Any of these fields indicates a real turn, not the startup prewarm.
    let input_empty = match value.get("input") {
        None => true,
        Some(v) => v.as_array().is_some_and(|a| a.is_empty()),
    };
    let no_previous_response_id = value
        .get("previous_response_id")
        .is_none_or(Value::is_null);
    let no_prompt = value.get("prompt").is_none_or(Value::is_null);
    let no_conversation_id = value.get("conversation_id").is_none_or(Value::is_null);
    input_empty && no_previous_response_id && no_prompt && no_conversation_id
}

fn axum_to_tungstenite(message: AxumWsMessage) -> Option<TungsteniteMessage> {
    use tokio_tungstenite::tungstenite::Utf8Bytes as TsUtf8Bytes;
    use tokio_tungstenite::tungstenite::protocol::CloseFrame as TsCloseFrame;
    use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as TsCloseCode;

    Some(match message {
        AxumWsMessage::Text(text) => {
            let bytes: Bytes = text.into();
            let utf8 = TsUtf8Bytes::try_from(bytes).ok()?;
            TungsteniteMessage::Text(utf8)
        }
        AxumWsMessage::Binary(bytes) => TungsteniteMessage::Binary(bytes),
        AxumWsMessage::Ping(bytes) => TungsteniteMessage::Ping(bytes),
        AxumWsMessage::Pong(bytes) => TungsteniteMessage::Pong(bytes),
        AxumWsMessage::Close(frame) => TungsteniteMessage::Close(frame.map(|f| {
            let reason_bytes: Bytes = f.reason.into();
            TsCloseFrame {
                code: TsCloseCode::from(f.code),
                reason: TsUtf8Bytes::try_from(reason_bytes).unwrap_or_default(),
            }
        })),
    })
}

fn tungstenite_to_axum(message: TungsteniteMessage) -> Option<AxumWsMessage> {
    use axum::extract::ws::CloseFrame as AxCloseFrame;
    use axum::extract::ws::Utf8Bytes as AxUtf8Bytes;

    Some(match message {
        TungsteniteMessage::Text(text) => AxumWsMessage::Text(AxUtf8Bytes::from(text.as_str())),
        TungsteniteMessage::Binary(bytes) => AxumWsMessage::Binary(bytes),
        TungsteniteMessage::Ping(bytes) => AxumWsMessage::Ping(bytes),
        TungsteniteMessage::Pong(bytes) => AxumWsMessage::Pong(bytes),
        TungsteniteMessage::Close(frame) => AxumWsMessage::Close(frame.map(|f| AxCloseFrame {
            code: u16::from(f.code),
            reason: AxUtf8Bytes::from(f.reason.as_str()),
        })),
        TungsteniteMessage::Frame(_) => return None,
    })
}

fn streaming_response(response: reqwest::Response) -> Response {
    let stream = async_stream::stream! {
        let mut upstream_events = response
            .bytes_stream()
            .map(|result| result.map_err(std::io::Error::other))
            .eventsource();

        while let Some(event) = upstream_events.next().await {
            match event {
                Ok(event) => {
                    yield Ok::<Bytes, Infallible>(encode_sse_event(&event.event, &event.data, &event.id));
                }
                Err(error) => {
                    let payload = serde_json::json!({
                        "message": format!("upstream SSE error: {error}"),
                    })
                    .to_string();
                    yield Ok::<Bytes, Infallible>(encode_sse_event("error", &payload, ""));
                    break;
                }
            }
        }
    };

    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("text/event-stream"));
    response
}

fn encode_sse_event(event: &str, data: &str, id: &str) -> Bytes {
    let mut body = String::new();
    if !id.is_empty() {
        body.push_str("id: ");
        body.push_str(id);
        body.push('\n');
    }
    if !event.is_empty() {
        body.push_str("event: ");
        body.push_str(event);
        body.push('\n');
    }
    if data.is_empty() {
        body.push_str("data:\n\n");
        return Bytes::from(body);
    }
    for line in data.lines() {
        body.push_str("data: ");
        body.push_str(line);
        body.push('\n');
    }
    body.push('\n');
    Bytes::from(body)
}

fn error_response(status: StatusCode, message: String) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": message,
            }
        })),
    )
        .into_response()
}

fn upstream_error_response(status: StatusCode) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "message": "upstream request failed",
                "code": status.as_u16(),
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::net::SocketAddr;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::GatewayState;
    use super::app;
    use super::encode_sse_event;
    use codex_copilot::CopilotAuth;
    use codex_copilot::CopilotClient;
    use codex_copilot::paths::AppPaths;

    #[test]
    fn encodes_named_sse_events() {
        let encoded = encode_sse_event("response.completed", "{\"ok\":true}", "evt-1");
        assert_eq!(
            String::from_utf8(encoded.to_vec()).expect("valid UTF-8"),
            "id: evt-1\nevent: response.completed\ndata: {\"ok\":true}\n\n"
        );
    }

    #[tokio::test]
    async fn models_endpoint_returns_filtered_models() {
        let upstream = MockServer::start().await;
        let address = spawn_gateway(test_state(&upstream)).await;
        let http = reqwest::Client::new();

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    { "id": "keep", "model_picker_enabled": true, "capabilities": { "type": "chat" } },
                    { "id": "drop", "model_picker_enabled": false, "capabilities": { "type": "chat" } }
                ]
            })))
            .mount(&upstream)
            .await;

        let response = http
            .get(format!("http://{address}/v1/models"))
            .send()
            .await
            .expect("models response");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response
            .json::<serde_json::Value>()
            .await
            .expect("JSON body");
        assert_eq!(
            body.get("data")
                .and_then(serde_json::Value::as_array)
                .map(std::vec::Vec::len),
            Some(1)
        );
    }

    #[tokio::test]
    async fn responses_endpoint_returns_probe_response_without_upstream_call() {
        let upstream = MockServer::start().await;
        let address = spawn_gateway(test_state(&upstream)).await;
        let http = reqwest::Client::new();

        let response = http
            .post(format!("http://{address}/v1/responses"))
            .json(&json!({
                "model": "gpt-5.4",
                "stream": false
            }))
            .send()
            .await
            .expect("probe response");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response
            .json::<serde_json::Value>()
            .await
            .expect("JSON body");
        assert_eq!(
            body.get("status").and_then(serde_json::Value::as_str),
            Some("completed")
        );
        let requests = upstream
            .received_requests()
            .await
            .expect("captured requests");
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn responses_endpoint_streams_upstream_sse() {
        let upstream = MockServer::start().await;
        let address = spawn_gateway(test_state(&upstream)).await;
        let http = reqwest::Client::new();

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_raw(
                        concat!(
                            "event: response.created\n",
                            "data: {\"id\":\"resp-1\"}\n\n",
                            "event: response.completed\n",
                            "data: {\"id\":\"resp-1\",\"status\":\"completed\"}\n\n"
                        ),
                        "text/event-stream",
                    ),
            )
            .mount(&upstream)
            .await;

        let response = http
            .post(format!("http://{address}/v1/responses"))
            .json(&json!({
                "model": "gpt-5.4",
                "stream": true,
                "input": [{ "type": "message", "role": "user", "content": "hello" }]
            }))
            .send()
            .await
            .expect("streaming response");

        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("text/event-stream")
        );
        let body = response.text().await.expect("stream body");
        assert!(body.contains("event: response.created"));
        assert!(body.contains("event: response.completed"));
    }

    #[test]
    fn is_warmup_request_matches_generate_false() {
        use super::is_warmup_request;
        assert!(is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4"}"#
        ));
    }

    #[test]
    fn is_warmup_request_rejects_non_warmup() {
        use super::is_warmup_request;
        // generate=true — real turn
        assert!(!is_warmup_request(
            r#"{"type":"response.create","generate":true,"model":"gpt-5.4"}"#
        ));
        // no `generate` field — standard real turn
        assert!(!is_warmup_request(
            r#"{"type":"response.create","model":"gpt-5.4","input":[]}"#
        ));
        // wrong top-level type
        assert!(!is_warmup_request(r#"{"type":"response.cancel"}"#));
        // garbage
        assert!(!is_warmup_request("not json"));
        // generate=false but carries real input — NOT the prewarm, must not intercept
        assert!(!is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#
        ));
        // generate=false but continues a prior response — real turn chained to a previous id
        assert!(!is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4","previous_response_id":"resp_123"}"#
        ));
        // generate=false with a non-null prompt field
        assert!(!is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4","prompt":"hi"}"#
        ));
        // generate=false with a non-null conversation_id
        assert!(!is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4","conversation_id":"conv_1"}"#
        ));
    }

    #[test]
    fn is_warmup_request_accepts_explicit_empty_input() {
        use super::is_warmup_request;
        // Explicit input=[] with generate=false is still the prewarm.
        assert!(is_warmup_request(
            r#"{"type":"response.create","generate":false,"model":"gpt-5.4","input":[]}"#
        ));
    }

    #[tokio::test]
    async fn responses_endpoint_intercepts_warmup_without_calling_upstream() {
        use futures::SinkExt;
        use futures::StreamExt;
        use std::sync::Arc;
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream listener");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let upstream_frames = Arc::new(AtomicUsize::new(0));
        let upstream_frames_clone = Arc::clone(&upstream_frames);
        tokio::spawn(async move {
            let (stream, _) = upstream_listener
                .accept()
                .await
                .expect("accept upstream connection");
            let mut ws = accept_async(stream).await.expect("ws handshake");
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    WsMessage::Text(_) | WsMessage::Binary(_) => {
                        upstream_frames_clone.fetch_add(1, Ordering::SeqCst);
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        });

        let upstream_url = format!("http://{upstream_addr}");
        let state = GatewayState {
            client: CopilotClient::new(test_auth_with_upstream(&upstream_url))
                .expect("test client"),
        };
        let gateway_addr = spawn_gateway(state).await;

        let ws_url = format!("ws://{gateway_addr}/v1/responses");
        let (mut client_ws, _) = connect_async(&ws_url)
            .await
            .expect("client handshake against gateway");
        client_ws
            .send(WsMessage::Text(
                r#"{"type":"response.create","generate":false,"model":"gpt-5.4"}"#.into(),
            ))
            .await
            .expect("send warmup frame");

        let reply = client_ws
            .next()
            .await
            .expect("receive synthetic frame")
            .expect("frame ok");
        match reply {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(text.as_str()).expect("valid json");
                assert_eq!(
                    parsed.get("type").and_then(serde_json::Value::as_str),
                    Some("response.completed"),
                );
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        // Give the pump a moment so we can assert upstream never saw the warmup.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(upstream_frames.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn responses_endpoint_forwards_non_warmup_response_create() {
        // Regression: a `response.create` with `generate=false` but a real `input`
        // array must be forwarded to upstream verbatim (not intercepted as a warmup).
        use futures::SinkExt;
        use futures::StreamExt;
        use std::sync::Arc;
        use tokio::sync::Mutex;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream listener");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let captured: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        tokio::spawn(async move {
            let (stream, _) = upstream_listener
                .accept()
                .await
                .expect("accept upstream connection");
            let mut ws = accept_async(stream).await.expect("ws handshake");
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    WsMessage::Text(text) => {
                        captured_clone.lock().await.push(text.as_str().to_string());
                        let _ = ws
                            .send(WsMessage::Text("{\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}".into()))
                            .await;
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        });

        let upstream_url = format!("http://{upstream_addr}");
        let state = GatewayState {
            client: CopilotClient::new(test_auth_with_upstream(&upstream_url))
                .expect("test client"),
        };
        let gateway_addr = spawn_gateway(state).await;

        let ws_url = format!("ws://{gateway_addr}/v1/responses");
        let (mut client_ws, _) = connect_async(&ws_url)
            .await
            .expect("client handshake against gateway");
        let payload = r#"{"type":"response.create","generate":false,"model":"gpt-5.4","input":[{"type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]}]}"#;
        client_ws
            .send(WsMessage::Text(payload.into()))
            .await
            .expect("send real response.create frame");

        let reply = client_ws
            .next()
            .await
            .expect("receive reply frame")
            .expect("frame ok");
        match reply {
            WsMessage::Text(text) => {
                let parsed: serde_json::Value =
                    serde_json::from_str(text.as_str()).expect("valid json");
                assert_eq!(
                    parsed.get("type").and_then(serde_json::Value::as_str),
                    Some("response.completed"),
                );
                // The upstream, not the gateway, produced this response_id. Intercept
                // would have replaced it with an empty id.
                assert_eq!(
                    parsed
                        .pointer("/response/id")
                        .and_then(serde_json::Value::as_str),
                    Some("resp_1"),
                );
            }
            other => panic!("unexpected frame: {other:?}"),
        }
        let captured_frames = captured.lock().await;
        assert_eq!(captured_frames.len(), 1);
        assert_eq!(captured_frames[0], payload);
    }

    #[tokio::test]
    async fn responses_endpoint_drains_upstream_frames_before_client_close() {
        // Regression for codex review finding #2: when the client sends Close, the
        // pump must drain any in-flight upstream frames back to the client before
        // tearing down. A naive implementation broke out of the loop on first close.
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream listener");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let (stream, _) = upstream_listener
                .accept()
                .await
                .expect("accept upstream connection");
            let mut ws = accept_async(stream).await.expect("ws handshake");
            // Accept one turn-starting frame, then emit two events + close.
            if let Some(Ok(_)) = ws.next().await {
                let _ = ws
                    .send(WsMessage::Text("{\"type\":\"response.created\"}".into()))
                    .await;
                let _ = ws
                    .send(WsMessage::Text(
                        "{\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}".into(),
                    ))
                    .await;
                let _ = ws.close(None).await;
            }
        });

        let upstream_url = format!("http://{upstream_addr}");
        let state = GatewayState {
            client: CopilotClient::new(test_auth_with_upstream(&upstream_url))
                .expect("test client"),
        };
        let gateway_addr = spawn_gateway(state).await;

        let ws_url = format!("ws://{gateway_addr}/v1/responses");
        let (mut client_ws, _) = connect_async(&ws_url)
            .await
            .expect("client handshake against gateway");
        client_ws
            .send(WsMessage::Text(
                r#"{"type":"response.create","generate":true,"model":"gpt-5.4","input":[]}"#.into(),
            ))
            .await
            .expect("send turn frame");

        // Collect every frame the gateway forwards until the client-side stream ends.
        let mut events = Vec::new();
        let mut saw_close = false;
        while let Some(Ok(msg)) = client_ws.next().await {
            match msg {
                WsMessage::Text(t) => events.push(t.as_str().to_string()),
                WsMessage::Close(_) => {
                    saw_close = true;
                    break;
                }
                _ => {}
            }
        }
        assert_eq!(events.len(), 2, "expected both upstream events relayed");
        assert!(events[0].contains("response.created"));
        assert!(events[1].contains("response.completed"));
        assert!(saw_close, "upstream close frame should be relayed to client");
    }

    #[tokio::test]
    async fn responses_endpoint_does_not_offer_permessage_deflate_to_upstream() {
        // Regression for the .3 fix: the gateway must strip
        // `Sec-WebSocket-Extensions: permessage-deflate` when dialing upstream, even
        // when the inbound client offers it. Otherwise upstream enables deflate and
        // our tungstenite client receives compressed frames it cannot decode.
        use futures::StreamExt;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream listener");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        let captured_headers: Arc<Mutex<Option<http::HeaderMap>>> = Arc::new(Mutex::new(None));
        let captured_clone = Arc::clone(&captured_headers);
        tokio::spawn(async move {
            let (stream, _) = upstream_listener
                .accept()
                .await
                .expect("accept upstream connection");
            let callback = |req: &tokio_tungstenite::tungstenite::handshake::server::Request,
                            resp: tokio_tungstenite::tungstenite::handshake::server::Response|
             -> std::result::Result<
                tokio_tungstenite::tungstenite::handshake::server::Response,
                tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
            > {
                let headers = req.headers().clone();
                let captured = Arc::clone(&captured_clone);
                tokio::spawn(async move {
                    *captured.lock().await = Some(headers);
                });
                Ok(resp)
            };
            let mut ws = tokio_tungstenite::accept_hdr_async(stream, callback)
                .await
                .expect("ws handshake");
            while let Some(Ok(_)) = ws.next().await {}
        });

        let upstream_url = format!("http://{upstream_addr}");
        let state = GatewayState {
            client: CopilotClient::new(test_auth_with_upstream(&upstream_url))
                .expect("test client"),
        };
        let gateway_addr = spawn_gateway(state).await;

        // Build a client handshake request that explicitly offers permessage-deflate.
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
        let ws_url = format!("ws://{gateway_addr}/v1/responses");
        let mut request = ws_url.into_client_request().expect("client request");
        request
            .headers_mut()
            .insert("Sec-WebSocket-Extensions", "permessage-deflate".parse().unwrap());
        let _ = connect_async(request).await.expect("client handshake");

        // Give the upstream listener a moment to record headers.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let captured = captured_headers.lock().await;
        let headers = captured.as_ref().expect("upstream received handshake");
        assert!(
            !headers.contains_key("sec-websocket-extensions"),
            "gateway must not forward `Sec-WebSocket-Extensions` to upstream, saw: {:?}",
            headers.get("sec-websocket-extensions"),
        );
    }

    #[tokio::test]
    async fn responses_endpoint_forwards_websocket_frames() {
        use futures::SinkExt;
        use futures::StreamExt;
        use tokio_tungstenite::accept_async;
        use tokio_tungstenite::connect_async;
        use tokio_tungstenite::tungstenite::Message as WsMessage;

        let upstream_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind upstream listener");
        let upstream_addr = upstream_listener.local_addr().expect("upstream addr");
        tokio::spawn(async move {
            let (stream, _) = upstream_listener
                .accept()
                .await
                .expect("accept upstream connection");
            let mut ws = accept_async(stream).await.expect("ws handshake");
            while let Some(Ok(message)) = ws.next().await {
                match message {
                    WsMessage::Text(text) => {
                        let reply = format!("echo:{text}");
                        if ws.send(WsMessage::Text(reply.into())).await.is_err() {
                            break;
                        }
                    }
                    WsMessage::Close(_) => break,
                    _ => {}
                }
            }
        });

        let upstream_url = format!("http://{upstream_addr}");
        let state = GatewayState {
            client: CopilotClient::new(test_auth_with_upstream(&upstream_url))
                .expect("test client"),
        };
        let gateway_addr = spawn_gateway(state).await;

        let ws_url = format!("ws://{gateway_addr}/v1/responses");
        let (mut client_ws, _response) = connect_async(&ws_url)
            .await
            .expect("client handshake against gateway");
        client_ws
            .send(WsMessage::Text("hello".into()))
            .await
            .expect("send text frame");
        let reply = client_ws
            .next()
            .await
            .expect("receive frame")
            .expect("frame ok");
        match reply {
            WsMessage::Text(text) => assert_eq!(text.as_str(), "echo:hello"),
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    fn test_auth_with_upstream(upstream_url: &str) -> CopilotAuth {
        let temp = tempdir().expect("temp dir");
        let app_dir = temp.keep().join("copilot-home");
        fs::create_dir_all(&app_dir).expect("create app dir");
        fs::write(app_dir.join("github_token"), "github-token\n").expect("write github token");
        fs::write(
            app_dir.join("copilot_token"),
            serde_json::to_vec(&json!({
                "token": "copilot-token",
                "expires_at": 4_102_444_800u64,
                "refresh_in": 3600u64
            }))
            .expect("encode cached token"),
        )
        .expect("write cached token");
        let paths = AppPaths {
            app_dir: app_dir.clone(),
            github_token_path: app_dir.join("github_token"),
            copilot_token_path: app_dir.join("copilot_token"),
            device_id_path: app_dir.join("device_id"),
            machine_id_path: app_dir.join("machine_id"),
        };
        CopilotAuth::new_for_tests(
            paths,
            upstream_url.to_string(),
            upstream_url.to_string(),
            upstream_url.to_string(),
        )
        .expect("test auth")
    }

    async fn spawn_gateway(state: GatewayState) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let address = listener.local_addr().expect("listener addr");
        tokio::spawn(async move {
            axum::serve(listener, app(state))
                .await
                .expect("serve test gateway");
        });
        address
    }

    fn test_state(server: &MockServer) -> GatewayState {
        GatewayState {
            client: CopilotClient::new(test_auth(server)).expect("test client"),
        }
    }

    fn test_auth(server: &MockServer) -> CopilotAuth {
        let temp = tempdir().expect("temp dir");
        let app_dir = temp.keep().join("copilot-home");
        fs::create_dir_all(&app_dir).expect("create app dir");
        fs::write(app_dir.join("github_token"), "github-token\n").expect("write github token");
        fs::write(
            app_dir.join("copilot_token"),
            serde_json::to_vec(&json!({
                "token": "copilot-token",
                "expires_at": 4_102_444_800u64,
                "refresh_in": 3600u64
            }))
            .expect("encode cached token"),
        )
        .expect("write cached token");

        let paths = AppPaths {
            app_dir: app_dir.clone(),
            github_token_path: app_dir.join("github_token"),
            copilot_token_path: app_dir.join("copilot_token"),
            device_id_path: app_dir.join("device_id"),
            machine_id_path: app_dir.join("machine_id"),
        };
        CopilotAuth::new_for_tests(paths, server.uri(), server.uri(), server.uri())
            .expect("test auth")
    }
}
