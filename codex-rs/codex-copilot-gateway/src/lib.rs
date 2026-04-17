use std::convert::Infallible;
use std::sync::Arc;

use anyhow::Context;
use axum::Json;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::HeaderValue;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::response::IntoResponse;
use axum::response::Response;
use axum::routing::get;
use axum::routing::post;
use bytes::Bytes;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotClient;
use codex_copilot::RequestContext;
use codex_copilot::payload::is_probe_request;
use codex_copilot::payload::normalize_payload;
use codex_copilot::payload::request_initiator;
use codex_copilot::payload::request_vision;
use codex_copilot::payload::synthetic_empty_response;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::Value;

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
        .route("/responses", post(responses))
        .route("/v1/responses", post(responses))
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

async fn responses(
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
        return error_response(status, body);
    }

    if stream_requested {
        return streaming_response(response);
    }

    match response.json::<Value>().await {
        Ok(value) => Json(value).into_response(),
        Err(error) => error_response(StatusCode::BAD_GATEWAY, error.to_string()),
    }
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
