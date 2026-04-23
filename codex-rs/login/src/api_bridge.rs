use std::sync::Arc;

use codex_api::CoreAuthProvider;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_model_provider_info::ModelProviderInfo;
use codex_protocol::error::CodexErr;
use tracing::warn;

use crate::CodexAuth;

pub async fn auth_provider_from_auth(
    auth: Option<CodexAuth>,
    provider: &ModelProviderInfo,
    copilot_auth: Option<Arc<CopilotAuth>>,
) -> codex_protocol::error::Result<CoreAuthProvider> {
    if provider.is_copilot() {
        if !provider.is_copilot_trusted() {
            warn!("Copilot provider base_url override detected; not injecting Copilot credentials");
            return Ok(CoreAuthProvider::new_legacy(None, None));
        }

        let copilot_auth = copilot_auth
            .ok_or_else(|| CodexErr::Fatal("Copilot session missing Arc<CopilotAuth>".into()))?;
        let source = CopilotHeaderSource::new(copilot_auth)
            .await
            .map_err(|e| CodexErr::Fatal(e.to_string()))?;
        return Ok(CoreAuthProvider::new_legacy(None, None).with_copilot(Arc::new(source)));
    }

    if let Some(api_key) = provider.api_key()? {
        return Ok(CoreAuthProvider::new_legacy(Some(api_key), None));
    }

    if let Some(token) = provider.experimental_bearer_token.clone() {
        return Ok(CoreAuthProvider::new_legacy(Some(token), None));
    }

    if let Some(auth) = auth {
        let token = auth.get_token()?;
        Ok(CoreAuthProvider::new_legacy(
            Some(token),
            auth.get_account_id(),
        ))
    } else {
        Ok(CoreAuthProvider::new_legacy(None, None))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Arc;

    use codex_api::AuthProvider;
    use codex_copilot::CopilotAuth;
    use codex_copilot::paths::AppPaths;
    use codex_model_provider_info::create_copilot_provider;
    use pretty_assertions::assert_eq;
    use reqwest::header::AUTHORIZATION;
    use reqwest::header::HeaderMap;
    use tempfile::TempDir;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::auth_provider_from_auth;
    use crate::CodexAuth;

    #[tokio::test]
    async fn auth_provider_from_auth_copilot_wins_over_chatgpt_token() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/copilot_internal/v2/token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "token": "copilot-token",
                "expires_at": 4_102_444_800u64,
                "refresh_in": 3600u64
            })))
            .expect(1)
            .mount(&server)
            .await;

        let (_tmp, copilot_auth) = test_copilot_auth(&server);
        let provider = create_copilot_provider();

        let auth_provider = auth_provider_from_auth(
            Some(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            &provider,
            Some(copilot_auth),
        )
        .await
        .expect("copilot auth provider");

        assert_eq!(auth_provider.token, None);
        assert_eq!(auth_provider.account_id, None);
        assert!(auth_provider.copilot.is_some());

        let mut headers = HeaderMap::new();
        AuthProvider::add_auth_headers(&auth_provider, &mut headers);

        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer copilot-token"),
        );
        assert_eq!(headers.get("ChatGPT-Account-ID"), None);

        server.verify().await;
    }

    #[tokio::test]
    async fn auth_provider_from_auth_rejects_copilot_with_overridden_base_url() {
        let mut provider = create_copilot_provider();
        provider.base_url = Some("http://evil.example.com".to_string());

        let auth_provider = auth_provider_from_auth(
            Some(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            &provider,
            None,
        )
        .await
        .expect("untrusted copilot provider should no-op");

        assert_eq!(auth_provider.token, None);
        assert_eq!(auth_provider.account_id, None);
        assert!(auth_provider.copilot.is_none());
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
}
