use anyhow::Context;
use reqwest::Response;
use reqwest::StatusCode;
use serde_json::Value;
use uuid::Uuid;

use crate::auth::CopilotAuth;
use crate::models::filter_models_response;
use crate::payload::Initiator;

#[derive(Clone)]
pub struct CopilotClient {
    auth: CopilotAuth,
    client: reqwest::Client,
}

#[derive(Clone, Debug)]
pub struct RequestContext {
    pub initiator: Initiator,
    pub request_id: String,
    pub interaction_id: Option<String>,
    pub vision: bool,
}

impl RequestContext {
    pub fn new(initiator: Initiator, vision: bool) -> Self {
        let request_id = Uuid::new_v4().to_string();
        Self {
            interaction_id: Some(request_id.clone()),
            initiator,
            request_id,
            vision,
        }
    }
}

impl CopilotClient {
    pub fn new(auth: CopilotAuth) -> anyhow::Result<Self> {
        let client = codex_client::build_reqwest_client_with_custom_ca(reqwest::Client::builder())
            .context("building Copilot client")?;
        Ok(Self { auth, client })
    }

    pub fn auth(&self) -> &CopilotAuth {
        &self.auth
    }

    pub async fn get_models(&self) -> anyhow::Result<Value> {
        for attempt in 0..2 {
            let force_refresh = attempt > 0;
            let request_id = Uuid::new_v4().to_string();
            let headers = self
                .auth
                .request_headers(
                    Initiator::User,
                    &request_id,
                    Some(&request_id),
                    /*vision*/ false,
                    force_refresh,
                )
                .await?;
            let response = self
                .client
                .get(format!("{}/models", self.auth.copilot_base_url()))
                .headers(headers)
                .send()
                .await
                .context("requesting Copilot models")?;
            if should_retry_with_fresh_token(response.status(), force_refresh) {
                self.auth.invalidate_cached_copilot_token()?;
                continue;
            }
            let value = response
                .json::<Value>()
                .await
                .context("decoding Copilot models response")?;
            return Ok(filter_models_response(value));
        }

        unreachable!("token refresh loop should return on success or final failure")
    }

    pub async fn create_response(
        &self,
        payload: &Value,
        request_context: &RequestContext,
    ) -> anyhow::Result<Response> {
        for attempt in 0..2 {
            let force_refresh = attempt > 0;
            let headers = self
                .auth
                .request_headers(
                    request_context.initiator.clone(),
                    &request_context.request_id,
                    request_context.interaction_id.as_deref(),
                    request_context.vision,
                    force_refresh,
                )
                .await?;
            let response = self
                .client
                .post(format!("{}/responses", self.auth.copilot_base_url()))
                .headers(headers)
                .json(payload)
                .send()
                .await
                .context("sending Copilot responses request")?;
            if should_retry_with_fresh_token(response.status(), force_refresh) {
                self.auth.invalidate_cached_copilot_token()?;
                continue;
            }
            return Ok(response);
        }

        unreachable!("token refresh loop should return on success or final failure")
    }
}

fn should_retry_with_fresh_token(status: StatusCode, force_refresh: bool) -> bool {
    !force_refresh && matches!(status, StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::Request;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CopilotClient;
    use super::RequestContext;
    use crate::auth::CopilotAuth;
    use crate::paths::AppPaths;
    use crate::payload::Initiator;

    #[tokio::test]
    async fn get_models_uses_copilot_headers_and_filters_response() {
        let server = MockServer::start().await;
        let auth = test_auth(&server).expect("test auth");
        let client = CopilotClient::new(auth).expect("client");

        Mock::given(method("GET"))
            .and(path("/models"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "object": "list",
                "data": [
                    { "id": "keep", "model_picker_enabled": true, "capabilities": { "type": "chat" } },
                    { "id": "drop", "model_picker_enabled": false, "capabilities": { "type": "chat" } }
                ]
            })))
            .mount(&server)
            .await;

        let response = client.get_models().await.expect("models response");

        assert_eq!(
            response
                .get("data")
                .and_then(serde_json::Value::as_array)
                .map(std::vec::Vec::len),
            Some(1),
        );
    }

    #[tokio::test]
    async fn create_response_posts_to_copilot_responses_endpoint() {
        let server = MockServer::start().await;
        let auth = test_auth(&server).expect("test auth");
        let client = CopilotClient::new(auth).expect("client");
        let payload = json!({
            "model": "gpt-5.4",
            "input": [{ "type": "message", "role": "user", "content": "hello" }],
            "stream": false
        });

        Mock::given(method("POST"))
            .and(path("/responses"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "resp-1",
                "object": "response",
                "status": "completed",
                "output": [],
                "output_text": "",
                "model": "gpt-5.4",
                "usage": { "input_tokens": 1, "output_tokens": 1, "total_tokens": 2 }
            })))
            .mount(&server)
            .await;

        let request_context = RequestContext::new(Initiator::User, /*vision*/ false);
        let response = client
            .create_response(&payload, &request_context)
            .await
            .expect("responses request");

        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let requests = server.received_requests().await.expect("captured requests");
        let request = requests
            .iter()
            .find(|request| request.url.path() == "/responses")
            .expect("responses request recorded");
        assert_eq!(
            request
                .headers
                .get("copilot-integration-id")
                .and_then(|value| value.to_str().ok()),
            Some("vscode-chat"),
        );
        assert_eq!(
            parse_json_body(request)
                .get("model")
                .and_then(serde_json::Value::as_str),
            Some("gpt-5.4"),
        );
    }

    fn test_auth(server: &MockServer) -> anyhow::Result<CopilotAuth> {
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
    }

    fn parse_json_body(request: &Request) -> serde_json::Value {
        serde_json::from_slice(&request.body).expect("valid JSON body")
    }
}
