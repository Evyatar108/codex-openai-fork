// SANDBOX PATCH: Copilot-aware ModelProvider. Routes api_auth() through
// CopilotHeaderSource + CoreAuthProvider.with_copilot, so call sites can use
// provider.api_auth().await? uniformly without branching on is_copilot().

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use codex_api::CoreAuthProvider;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::collaboration_mode_presets::CollaborationModesConfig;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::openai_models::ModelsResponse;
use tokio::sync::OnceCell;
use tracing::warn;

use crate::copilot_models_endpoint::CopilotModelsEndpoint;
use crate::provider::ModelProvider;
use crate::provider::ProviderAccountResult;
use crate::provider::ProviderAccountState;

/// Runtime model provider for Copilot sessions. Owns the session-scoped
/// `CopilotAuth` (which caches GitHub/Copilot tokens on disk + in memory) and
/// builds a fresh `CopilotHeaderSource` on every `api_auth()` call so that
/// `CoreAuthProvider::on_unauthorized()` -> `invalidate()` semantics remain
/// correct: after a 401/403 the caller drops the old `SharedAuthProvider`, and
/// the next `api_auth()` rebuilds from a fresh header source (token re-fetched
/// from GitHub because `invalidate` cleared the disk cache).
///
/// NOTE: the `OnceCell<Arc<CopilotAuth>>` itself is NEVER invalidated on 401.
/// Retry correctness depends entirely on `CopilotHeaderSource::invalidate()`
/// clearing the on-disk Copilot token so the next
/// `CopilotHeaderSource::new(auth.clone())` re-fetches from GitHub. If a future
/// change to `CopilotAuth` caches the token in memory beyond the disk layer,
/// the retry loop will silently reuse the stale token — add an explicit
/// `OnceCell::take()` in `on_unauthorized` or a new trait hook in that case.
pub(crate) struct CopilotModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
    copilot_auth: Arc<OnceCell<Arc<CopilotAuth>>>,
}

impl std::fmt::Debug for CopilotModelProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CopilotModelProvider")
            .field("info", &self.info)
            .finish()
    }
}

impl CopilotModelProvider {
    pub(crate) fn new(
        info: ModelProviderInfo,
        auth_manager: Option<Arc<AuthManager>>,
    ) -> Self {
        Self {
            info,
            auth_manager,
            copilot_auth: Arc::new(OnceCell::new()),
        }
    }

    async fn get_or_init_copilot_auth(&self) -> CodexResult<Arc<CopilotAuth>> {
        self.copilot_auth
            .get_or_try_init(|| async {
                CopilotAuth::new()
                    .map(Arc::new)
                    .map_err(|e| CodexErr::Fatal(e.to_string()))
            })
            .await
            .cloned()
    }
}

#[async_trait]
impl ModelProvider for CopilotModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    async fn auth(&self) -> Option<CodexAuth> {
        // Copilot sessions do not surface a CodexAuth; api_auth() attaches the
        // Copilot-specific authorization header directly.
        None
    }

    fn account_state(&self) -> ProviderAccountResult {
        // Copilot session auth is not surfaced as a `ProviderAccount` variant; the
        // app-visible state has no email/plan, and we never gate on OpenAI auth.
        Ok(ProviderAccountState {
            account: None,
            requires_openai_auth: false,
        })
    }

    fn models_manager(
        &self,
        codex_home: PathBuf,
        config_model_catalog: Option<ModelsResponse>,
        collaboration_modes_config: CollaborationModesConfig,
    ) -> SharedModelsManager {
        // SANDBOX PATCH: route the Copilot session's model catalog through
        // `OpenAiModelsManager` driven by `CopilotModelsEndpoint`, which fetches
        // `/models` from `api.githubcopilot.com` using `CopilotHeaderSource` and
        // translates the response into `ModelInfo` entries (preferring bundled
        // metadata for known slugs; synthesizing a minimal entry for new
        // Copilot-only slugs). When the caller provides an explicit
        // `model_catalog`, honor it as authoritative.
        if let Some(model_catalog) = config_model_catalog {
            return Arc::new(StaticModelsManager::new(
                self.auth_manager.clone(),
                model_catalog,
                collaboration_modes_config,
            ));
        }

        let base_url = self
            .info
            .base_url
            .clone()
            .unwrap_or_else(|| "https://api.githubcopilot.com".to_string());
        let endpoint = Arc::new(CopilotModelsEndpoint::new(
            base_url,
            Arc::clone(&self.copilot_auth),
        ));
        Arc::new(OpenAiModelsManager::new(
            codex_home,
            endpoint,
            self.auth_manager.clone(),
            collaboration_modes_config,
        ))
    }

    async fn api_provider(&self) -> CodexResult<Provider> {
        // Copilot sessions do not require a CodexAuth for provider
        // construction, so skip the default trait path's self.auth().await.
        self.info().to_api_provider(/*auth_mode*/ None)
    }

    async fn api_auth(&self) -> CodexResult<SharedAuthProvider> {
        if !self.info.is_copilot_trusted() {
            // SANDBOX PATCH: fail-closed. The copilot wire still sends Copilot-session
            // metadata headers (x-initiator, copilot-integration-id, editor-plugin-version,
            // ...) via CopilotHeaderSource on every request. If the base_url points at a
            // non-Copilot host, silently dropping only the Authorization header would leak
            // those metadata headers to an untrusted origin. Refuse to proceed.
            warn!(
                "Copilot provider base_url override detected; refusing to build auth (would leak Copilot session headers to non-Copilot host)"
            );
            return Err(CodexErr::Fatal(
                "Copilot provider base_url override not allowed: must point to api.githubcopilot.com".to_string(),
            ));
        }

        let copilot_auth = self.get_or_init_copilot_auth().await?;
        let source = CopilotHeaderSource::new(copilot_auth)
            .await
            .map_err(|e| CodexErr::Fatal(e.to_string()))?;
        Ok(Arc::new(
            CoreAuthProvider::new_legacy(None, None).with_copilot(Arc::new(source)),
        ))
    }

    #[cfg(any(test, feature = "test-support"))]
    fn inject_copilot_auth_for_tests(
        &self,
        auth: Arc<CopilotAuth>,
    ) -> Result<(), &'static str> {
        self.copilot_auth
            .set(auth)
            .map_err(|_| "copilot auth was already initialized")
    }
}

#[cfg(test)]
mod tests {
    //! SANDBOX PATCH: These tests cover the Copilot branch of `api_auth()` that
    //! previously lived in `login::api_bridge::auth_provider_from_auth`. The old
    //! tests were deleted in the F-2 port; this module re-homes the equivalent
    //! coverage against `CopilotModelProvider::api_auth`.
    use std::fs;
    use std::sync::Arc;

    use codex_copilot::CopilotAuth;
    use codex_copilot::paths::AppPaths;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
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

    use super::CopilotModelProvider;
    use crate::provider::ModelProvider;

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
            .expect("test copilot auth");

        (temp, Arc::new(auth))
    }

    #[tokio::test]
    async fn api_auth_copilot_injects_bearer_and_omits_chatgpt_account_id() {
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
        let model_provider = CopilotModelProvider::new(provider, None);
        model_provider
            .inject_copilot_auth_for_tests(copilot_auth)
            .expect("seed test copilot auth");

        let api_auth = model_provider
            .api_auth()
            .await
            .expect("copilot api_auth should build");

        let mut headers = HeaderMap::new();
        api_auth.add_auth_headers(&mut headers);

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
    async fn api_auth_copilot_wins_over_chatgpt_auth_manager() {
        // SANDBOX PATCH: Locks in the invariant that even if a ChatGPT `CodexAuth` is
        // reachable through the `AuthManager`, a Copilot session uses only the
        // Copilot-session bearer. A future refactor that surfaces `auth()` from the
        // manager would break this test — catching the regression where
        // `ChatGPT-Account-ID` could leak onto a Copilot request.
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
        let chatgpt_auth_manager = AuthManager::from_auth_for_testing(
            CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        );
        let provider = create_copilot_provider();
        let model_provider = CopilotModelProvider::new(provider, Some(chatgpt_auth_manager));
        model_provider
            .inject_copilot_auth_for_tests(copilot_auth)
            .expect("seed test copilot auth");

        // SANDBOX PATCH: lock in `ModelProvider::auth()` returning `None` for Copilot
        // sessions. `core/src/client.rs::current_client_setup` reads `provider.auth()`
        // to decide whether to attach a ChatGPT bearer via `AuthorizationHeaderAuthProvider`;
        // a regression here would leak ChatGPT auth onto a Copilot request even when
        // `api_auth()` itself is correct.
        assert!(
            model_provider.auth().await.is_none(),
            "CopilotModelProvider::auth() must surface None even when an AuthManager carries ChatGPT auth",
        );

        let api_auth = model_provider
            .api_auth()
            .await
            .expect("copilot api_auth should build even with a ChatGPT AuthManager present");

        let mut headers = HeaderMap::new();
        api_auth.add_auth_headers(&mut headers);

        assert_eq!(
            headers
                .get(AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer copilot-token"),
        );
        assert_eq!(
            headers.get("ChatGPT-Account-ID"),
            None,
            "ChatGPT-Account-ID must not leak onto a Copilot request",
        );

        server.verify().await;
    }

    #[tokio::test]
    async fn api_auth_rejects_copilot_with_overridden_base_url() {
        let mut provider = create_copilot_provider();
        provider.base_url = Some("http://evil.example.com".to_string());
        let model_provider = CopilotModelProvider::new(provider, None);

        let err = model_provider
            .api_auth()
            .await
            .err()
            .expect("untrusted base_url must fail closed");

        let message = err.to_string();
        assert!(
            message.contains("Copilot provider base_url override not allowed"),
            "unexpected error: {message}",
        );
    }
}
