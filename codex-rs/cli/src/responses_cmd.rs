use std::sync::Arc;

use clap::Parser;
use codex_api::CoreAuthProvider;
use codex_api::Provider as ApiProvider;
use codex_copilot::CopilotAuth;
use codex_core::config::Config;
use codex_core::copilot_transport;
use codex_model_provider::create_model_provider;
use codex_utils_cli::CliConfigOverrides;
use serde_json::Value;
use serde_json::json;
use tokio::io::AsyncReadExt;

#[derive(Debug, Parser)]
pub(crate) struct ResponsesCommand {}

pub(crate) async fn run_responses_command(
    root_config_overrides: CliConfigOverrides,
) -> anyhow::Result<()> {
    let mut payload_text = String::new();
    tokio::io::stdin().read_to_string(&mut payload_text).await?;
    if payload_text.trim().is_empty() {
        anyhow::bail!("expected Responses API JSON payload on stdin");
    }

    let payload: serde_json::Value = serde_json::from_str(&payload_text)
        .map_err(|err| anyhow::anyhow!("failed to parse Responses API JSON payload: {err}"))?;
    if payload.get("stream").and_then(serde_json::Value::as_bool) != Some(true) {
        anyhow::bail!("codex responses expects a streaming payload with `\"stream\": true`");
    }

    let cli_overrides = root_config_overrides
        .parse_overrides()
        .map_err(anyhow::Error::msg)?;
    let config = Config::load_with_cli_overrides(cli_overrides).await?;
    let base_auth_manager = codex_login::AuthManager::shared_from_config(
        &config, /*enable_codex_api_key_env*/ true,
    );
    let is_copilot = config.model_provider.is_copilot();
    let model_provider = create_model_provider(config.model_provider, Some(base_auth_manager));
    let api_provider = model_provider.api_provider().await?;
    let api_auth = model_provider.api_auth().await?;
    let copilot_auth = if is_copilot {
        Some(Arc::new(
            CopilotAuth::new().map_err(|e| anyhow::anyhow!(e))?,
        ))
    } else {
        None
    };
    run_responses_with_auth(payload, api_provider, api_auth, copilot_auth).await
}

pub async fn run_responses_with_auth(
    payload: Value,
    api_provider: ApiProvider,
    api_auth: Arc<dyn codex_api::ApiAuthProvider>,
    copilot_auth: Option<Arc<CopilotAuth>>,
) -> anyhow::Result<()> {
    let transport =
        codex_api::ReqwestTransport::new(codex_login::default_client::build_reqwest_client());
    // SANDBOX PATCH: delegate Copilot sessions to the shared transport factory so the
    // per-request pre_send_hook (normalize_payload + copilot-vision-request) stays consistent
    // with the streaming path in `core::client::stream_responses_api`.
    let client = if let Some(copilot_auth) = copilot_auth {
        // TODO(copilot): add retry-on-401/403 once the CLI path shares the main client retry loop.
        let header_source = Arc::new(
            codex_copilot::CopilotHeaderSource::new(copilot_auth)
                .await
                .map_err(|err| anyhow::anyhow!(err))?,
        );
        let copilot_auth_provider = CoreAuthProvider::default().with_copilot(header_source);
        copilot_transport::build_copilot_client(api_provider, copilot_auth_provider, transport)
    } else {
        codex_api::ResponsesClient::new(transport, api_provider, api_auth)
    };

    let mut stream = client
        .stream(
            payload,
            Default::default(),
            codex_api::Compression::None,
            /*turn_state*/ None,
        )
        .await?;
    while let Some(event) = stream.rx_event.recv().await {
        let event = event?;
        println!("{}", serde_json::to_string(&response_event_to_json(event))?);
    }

    Ok(())
}

fn response_event_to_json(event: codex_api::ResponseEvent) -> serde_json::Value {
    match event {
        codex_api::ResponseEvent::Created => {
            json!({ "type": "response.created", "response": {} })
        }
        codex_api::ResponseEvent::OutputItemDone(item) => {
            json!({ "type": "response.output_item.done", "item": item })
        }
        codex_api::ResponseEvent::OutputItemAdded(item) => {
            json!({ "type": "response.output_item.added", "item": item })
        }
        codex_api::ResponseEvent::ServerModel(model) => {
            json!({ "type": "response.server_model", "model": model })
        }
        codex_api::ResponseEvent::ServerReasoningIncluded(included) => {
            json!({ "type": "response.server_reasoning_included", "included": included })
        }
        codex_api::ResponseEvent::Completed {
            response_id,
            token_usage,
        } => {
            let response = match token_usage {
                Some(token_usage) => json!({
                    "id": response_id,
                    "usage": {
                        "input_tokens": token_usage.input_tokens,
                        "input_tokens_details": {
                            "cached_tokens": token_usage.cached_input_tokens,
                        },
                        "output_tokens": token_usage.output_tokens,
                        "output_tokens_details": {
                            "reasoning_tokens": token_usage.reasoning_output_tokens,
                        },
                        "total_tokens": token_usage.total_tokens,
                    },
                }),
                None => json!({ "id": response_id }),
            };
            json!({ "type": "response.completed", "response": response })
        }
        codex_api::ResponseEvent::OutputTextDelta(delta) => {
            json!({ "type": "response.output_text.delta", "delta": delta })
        }
        codex_api::ResponseEvent::ToolCallInputDelta {
            item_id,
            call_id,
            delta,
        } => {
            json!({
                "type": "response.tool_call_input.delta",
                "item_id": item_id,
                "call_id": call_id,
                "delta": delta,
            })
        }
        codex_api::ResponseEvent::ReasoningSummaryDelta {
            delta,
            summary_index,
        } => json!({
            "type": "response.reasoning_summary_text.delta",
            "delta": delta,
            "summary_index": summary_index,
        }),
        codex_api::ResponseEvent::ReasoningContentDelta {
            delta,
            content_index,
        } => json!({
            "type": "response.reasoning_text.delta",
            "delta": delta,
            "content_index": content_index,
        }),
        codex_api::ResponseEvent::ReasoningSummaryPartAdded { summary_index } => {
            json!({
                "type": "response.reasoning_summary_part.added",
                "summary_index": summary_index,
            })
        }
        codex_api::ResponseEvent::RateLimits(rate_limits) => {
            json!({ "type": "response.rate_limits", "rate_limits": rate_limits })
        }
        codex_api::ResponseEvent::ModelsEtag(etag) => {
            json!({ "type": "response.models_etag", "etag": etag })
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use super::response_event_to_json;
    use super::run_responses_with_auth;
    use std::time::Duration;

    use codex_api::CoreAuthProvider;
    use codex_api::Provider as ApiProvider;
    use codex_api::RetryConfig;
    use codex_copilot::CopilotAuth;
    use codex_copilot::CopilotHeaderSource;
    use codex_copilot::paths::AppPaths;
    use codex_protocol::protocol::TokenUsage;
    use pretty_assertions::assert_eq;
    use serde_json::Value;
    use serde_json::json;
    use tempfile::TempDir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    #[test]
    fn response_events_keep_replayable_response_envelopes() {
        let created = response_event_to_json(codex_api::ResponseEvent::Created);
        assert_eq!(created, json!({"type": "response.created", "response": {}}));

        let completed = response_event_to_json(codex_api::ResponseEvent::Completed {
            response_id: "resp-1".to_string(),
            token_usage: Some(TokenUsage {
                input_tokens: 10,
                cached_input_tokens: 4,
                output_tokens: 7,
                reasoning_output_tokens: 3,
                total_tokens: 17,
            }),
        });
        assert_eq!(
            completed,
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp-1",
                    "usage": {
                        "input_tokens": 10,
                        "input_tokens_details": {
                            "cached_tokens": 4,
                        },
                        "output_tokens": 7,
                        "output_tokens_details": {
                            "reasoning_tokens": 3,
                        },
                        "total_tokens": 17,
                    },
                },
            })
        );

        let completed_without_usage = response_event_to_json(codex_api::ResponseEvent::Completed {
            response_id: "resp-2".to_string(),
            token_usage: None,
        });
        assert_eq!(
            completed_without_usage,
            json!({"type": "response.completed", "response": {"id": "resp-2"}})
        );
    }

    #[test]
    fn reasoning_deltas_use_responses_event_names() {
        let summary = response_event_to_json(codex_api::ResponseEvent::ReasoningSummaryDelta {
            delta: "plan".to_string(),
            summary_index: 1,
        });
        assert_eq!(
            summary,
            json!({
                "type": "response.reasoning_summary_text.delta",
                "delta": "plan",
                "summary_index": 1,
            })
        );

        let content = response_event_to_json(codex_api::ResponseEvent::ReasoningContentDelta {
            delta: "detail".to_string(),
            content_index: 2,
        });
        assert_eq!(
            content,
            json!({
                "type": "response.reasoning_text.delta",
                "delta": "detail",
                "content_index": 2,
            })
        );
    }

    #[tokio::test]
    async fn responses_cmd_copilot_wires_pre_send_hook() {
        let responses_server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("content-type", "text/event-stream")
                    .set_body_string(completed_sse("resp-cli-1")),
            )
            .expect(1)
            .mount(&responses_server)
            .await;

        let token_server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "token": "copilot-token",
                "expires_at": 4_102_444_800u64,
                "refresh_in": 3600u64
            })))
            .expect(1)
            .mount(&token_server)
            .await;

        let (_copilot_home, copilot_auth) = test_copilot_auth(&token_server);
        let header_source = CopilotHeaderSource::new(copilot_auth)
            .await
            .expect("copilot header source");
        let api_auth =
            CoreAuthProvider::new_legacy(None, None).with_copilot(Arc::new(header_source));
        let api_provider = test_api_provider(&responses_server);

        let payload = json!({
            "stream": true,
            "model": "gpt-5.4",
            "previous_response_id": "resp-old",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": "normalize me"
                        }
                    ]
                }
            ],
            "tools": [
                { "type": "web_search", "name": "search" },
                { "type": "function", "name": "keep", "parameters": { "type": "object" } }
            ]
        });

        let noop_auth: Arc<dyn codex_api::ApiAuthProvider> = Arc::new(api_auth.clone());
        let copilot_auth = api_auth
            .copilot
            .as_ref()
            .expect("copilot header source present")
            .clone();
        // The Copilot branch of run_responses_with_auth rebuilds its own CoreAuthProvider from
        // CopilotAuth; in this test we pre-materialized the header source, so invoke the
        // transport factory directly to exercise the pre_send_hook wiring.
        let transport =
            codex_api::ReqwestTransport::new(codex_login::default_client::build_reqwest_client());
        let client = copilot_transport::build_copilot_client(api_provider, api_auth, transport);
        let mut stream = client
            .stream(
                payload,
                Default::default(),
                codex_api::Compression::None,
                /*turn_state*/ None,
            )
            .await
            .expect("copilot responses stream");
        while let Some(event) = stream.rx_event.recv().await {
            let _ = event.expect("event ok");
        }
        let _ = noop_auth;
        let _ = copilot_auth;

        let requests = responses_server
            .received_requests()
            .await
            .expect("captured requests");
        assert_eq!(requests.len(), 1);
        let request = &requests[0];
        let body = request.body_json::<Value>().expect("request body json");

        assert_eq!(request.url.path(), "/responses");
        assert_eq!(body.get("previous_response_id"), None);
        assert_eq!(body.get("service_tier"), Some(&Value::Null));
        assert_eq!(
            body.pointer("/tools/0/type").and_then(Value::as_str),
            Some("function")
        );
        assert!(
            body.get("tools")
                .and_then(Value::as_array)
                .is_some_and(|tools| {
                    tools
                        .iter()
                        .all(|tool| tool.get("type").and_then(Value::as_str) != Some("web_search"))
                }),
            "web_search tool should be stripped for Copilot payloads",
        );
        assert_eq!(
            request
                .headers
                .get("copilot-integration-id")
                .and_then(|value| value.to_str().ok()),
            Some("vscode-chat"),
        );

        token_server.verify().await;
    }

    fn completed_sse(response_id: &str) -> String {
        format!(
            "event: response.created\ndata: {{\"type\":\"response.created\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\nevent: response.completed\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"{response_id}\"}}}}\n\n"
        )
    }

    fn test_api_provider(server: &MockServer) -> ApiProvider {
        ApiProvider {
            name: "copilot".to_string(),
            base_url: server.uri(),
            query_params: None,
            headers: Default::default(),
            retry: RetryConfig {
                max_attempts: 1,
                base_delay: Duration::from_millis(1),
                retry_429: false,
                retry_5xx: false,
                retry_transport: true,
            },
            stream_idle_timeout: Duration::from_secs(5),
        }
    }

    fn test_copilot_auth(server: &MockServer) -> (TempDir, Arc<CopilotAuth>) {
        let temp = tempfile::tempdir().expect("temp dir");
        let app_dir = temp.path().join("copilot-home");
        fs::create_dir_all(&app_dir).expect("create app dir");
        fs::write(app_dir.join("github_token"), "github-token\n").expect("write github token");

        let paths = AppPaths {
            app_dir: app_dir.clone(),
            github_token_path: app_dir.join("github_token"),
            copilot_token_path: app_dir.join("copilot_token"),
            device_id_path: app_dir.join("device_id"),
            machine_id_path: app_dir.join("machine_id"),
        };
        let auth = CopilotAuth::new_for_tests(paths, server.uri(), server.uri(), server.uri())
            .expect("test auth");

        (temp, Arc::new(auth))
    }

    #[test]
    fn tool_call_input_delta_uses_responses_event_name() {
        let delta = response_event_to_json(codex_api::ResponseEvent::ToolCallInputDelta {
            item_id: "item-1".to_string(),
            call_id: Some("call-1".to_string()),
            delta: "patch".to_string(),
        });
        assert_eq!(
            delta,
            json!({
                "type": "response.tool_call_input.delta",
                "item_id": "item-1",
                "call_id": "call-1",
                "delta": "patch",
            })
        );
    }
}
