use std::sync::Arc;

use http::HeaderMap;
use http::HeaderValue;
use uuid::Uuid;

use crate::auth::CopilotAuth;
use crate::auth::build_session_headers;

#[derive(Debug)]
struct CachedHeaders {
    base: HeaderMap,
}

#[derive(Debug)]
struct Inner {
    auth: Arc<CopilotAuth>,
    cache: CachedHeaders,
}

#[derive(Clone, Debug)]
pub struct CopilotHeaderSource {
    inner: Arc<Inner>,
}

impl CopilotHeaderSource {
    pub async fn new(auth: Arc<CopilotAuth>) -> anyhow::Result<Self> {
        let cache = CachedHeaders {
            base: build_cached_headers(&auth).await?,
        };
        Ok(Self {
            inner: Arc::new(Inner { auth, cache }),
        })
    }

    pub fn inject(&self, headers: &mut HeaderMap) {
        for (name, value) in &self.inner.cache.base {
            headers.entry(name.clone()).or_insert(value.clone());
        }

        // v7: these defaults activate when ModelsClient is re-enabled for Copilot.
        // For v6 they are overridden by pre_send_hook on every streaming call.
        headers
            .entry("x-initiator")
            .or_insert(HeaderValue::from_static("user"));
        headers
            .entry("x-interaction-type")
            .or_insert(HeaderValue::from_static("conversation-user"));
        headers
            .entry("x-vscode-user-agent-library-version")
            .or_insert(HeaderValue::from_static("electron-fetch"));

        let request_id = Uuid::new_v4().to_string();
        let request_id = HeaderValue::from_str(&request_id).expect("uuid must be a valid header");

        headers
            .entry("x-interaction-id")
            .or_insert(request_id.clone());
        headers.entry("x-request-id").or_insert(request_id.clone());
        headers.entry("x-agent-task-id").or_insert(request_id);
    }

    pub fn invalidate(&self) {
        let _ = self.inner.auth.invalidate_cached_copilot_token();
    }
}

async fn build_cached_headers(auth: &CopilotAuth) -> anyhow::Result<HeaderMap> {
    let token = auth.copilot_token(/* force_refresh */ false).await?;
    Ok(build_session_headers(
        &token,
        auth.machine_id(),
        auth.session_id(),
        auth.device_id(),
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use pretty_assertions::assert_eq;
    use serde_json::json;
    use tempfile::tempdir;
    use uuid::Uuid;
    use wiremock::MockServer;

    use super::CopilotHeaderSource;
    use crate::auth::CopilotAuth;
    use crate::paths::AppPaths;

    #[tokio::test]
    async fn inject_includes_all_session_headers() {
        let server = MockServer::start().await;
        let source = CopilotHeaderSource::new(Arc::new(test_auth(&server)))
            .await
            .expect("header source");

        let mut headers = http::HeaderMap::new();
        headers.insert("x-initiator", http::HeaderValue::from_static("agent"));

        source.inject(&mut headers);

        assert_eq!(
            headers
                .get(http::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer copilot-token"),
        );
        assert_eq!(
            headers
                .get(http::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
        );
        assert_eq!(
            headers
                .get(http::header::ACCEPT)
                .and_then(|value| value.to_str().ok()),
            Some("application/json"),
        );
        assert_eq!(
            headers
                .get("copilot-integration-id")
                .and_then(|value| value.to_str().ok()),
            Some("vscode-chat"),
        );
        assert_eq!(
            headers
                .get("editor-plugin-version")
                .and_then(|value| value.to_str().ok()),
            Some(crate::auth::EDITOR_PLUGIN_VERSION),
        );
        assert_eq!(
            headers
                .get("user-agent")
                .and_then(|value| value.to_str().ok()),
            Some(crate::auth::USER_AGENT),
        );
        assert_eq!(
            headers
                .get("openai-intent")
                .and_then(|value| value.to_str().ok()),
            Some("conversation-agent"),
        );
        assert_eq!(
            headers
                .get("x-github-api-version")
                .and_then(|value| value.to_str().ok()),
            Some(crate::auth::API_VERSION),
        );
        assert_eq!(
            headers
                .get("x-vscode-user-agent-library-version")
                .and_then(|value| value.to_str().ok()),
            Some("electron-fetch"),
        );
        assert_eq!(
            headers
                .get("x-initiator")
                .and_then(|value| value.to_str().ok()),
            Some("agent"),
        );
        assert_eq!(
            headers
                .get("x-interaction-type")
                .and_then(|value| value.to_str().ok()),
            Some("conversation-user"),
        );
        assert!(
            headers
                .get("editor-version")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("vscode/"))
        );
        assert!(
            headers
                .get("vscode-machineid")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            headers
                .get("vscode-sessionid")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.is_empty())
        );
        assert!(
            headers
                .get("x-codex-copilot-device-id")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| !value.is_empty())
        );

        let request_id = headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("x-request-id");
        assert!(Uuid::parse_str(request_id).is_ok());
        assert_eq!(
            headers
                .get("x-agent-task-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id),
        );
        assert_eq!(
            headers
                .get("x-interaction-id")
                .and_then(|value| value.to_str().ok()),
            Some(request_id),
        );
    }

    #[tokio::test]
    async fn inject_generates_fresh_uuid_per_call() {
        let server = MockServer::start().await;
        let source = CopilotHeaderSource::new(Arc::new(test_auth(&server)))
            .await
            .expect("header source");

        let mut first = http::HeaderMap::new();
        source.inject(&mut first);
        let mut second = http::HeaderMap::new();
        source.inject(&mut second);

        let first_id = first
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("first request id");
        let second_id = second
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("second request id");

        assert_ne!(first_id, second_id);
        assert_eq!(
            first
                .get("x-agent-task-id")
                .and_then(|value| value.to_str().ok()),
            Some(first_id),
        );
        assert_eq!(
            second
                .get("x-agent-task-id")
                .and_then(|value| value.to_str().ok()),
            Some(second_id),
        );
    }

    #[tokio::test]
    async fn invalidate_clears_disk_cache() {
        let server = MockServer::start().await;
        let auth = Arc::new(test_auth(&server));
        let token_path = auth.copilot_token_path().to_path_buf();
        assert!(token_path.exists());

        let source = CopilotHeaderSource::new(auth).await.expect("header source");
        source.invalidate();

        assert!(!token_path.exists());
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
