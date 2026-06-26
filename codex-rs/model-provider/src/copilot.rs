// SANDBOX PATCH: Copilot-aware ModelProvider. Routes api_auth() through
// CopilotHeaderSource + CoreAuthProvider.with_copilot, so call sites can use
// provider.api_auth().await? uniformly without branching on is_copilot().

mod gated_models_manager;

use std::path::PathBuf;
use std::sync::Arc;

use codex_api::CoreAuthProvider;
use codex_api::Provider;
use codex_api::SharedAuthProvider;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_model_provider_info::ModelProviderInfo;
use codex_models_manager::manager::OpenAiModelsManager;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::manager::StaticModelsManager;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::openai_models::ModelsResponse;
use tokio::sync::OnceCell;
use tracing::warn;

use crate::copilot::gated_models_manager::GatedModelsManager;
use crate::copilot_models_endpoint::CopilotModelsEndpoint;
use crate::provider::ModelProvider;
use crate::provider::ModelProviderFuture;
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
    pub(crate) fn new(info: ModelProviderInfo, auth_manager: Option<Arc<AuthManager>>) -> Self {
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

impl ModelProvider for CopilotModelProvider {
    fn info(&self) -> &ModelProviderInfo {
        &self.info
    }

    fn auth_manager(&self) -> Option<Arc<AuthManager>> {
        self.auth_manager.clone()
    }

    fn auth(&self) -> ModelProviderFuture<'_, Option<CodexAuth>> {
        // Copilot sessions do not surface a CodexAuth; api_auth() attaches the
        // Copilot-specific authorization header directly.
        Box::pin(async move { None })
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
    ) -> SharedModelsManager {
        // SANDBOX PATCH: route the Copilot session's model catalog through
        // `OpenAiModelsManager` driven by `CopilotModelsEndpoint`, which fetches
        // `/models` from `api.githubcopilot.com` using `CopilotHeaderSource` and
        // translates the response into `ModelInfo` entries (preferring bundled
        // metadata for known slugs; synthesizing a minimal entry for new
        // Copilot-only slugs). When the caller provides an explicit
        // `model_catalog`, honor it as authoritative.
        if let Some(model_catalog) = config_model_catalog {
            // SANDBOX PATCH: `GatedModelsManager` reads the Anthropic feature
            // gate live so `/experimental` can update an existing TUI session.
            return GatedModelsManager::wrap(Arc::new(StaticModelsManager::new(
                self.auth_manager.clone(),
                model_catalog,
            )));
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
        // SANDBOX PATCH: `GatedModelsManager` reads the Anthropic feature gate
        // live so `/experimental` can update an existing TUI session.
        GatedModelsManager::wrap(Arc::new(OpenAiModelsManager::new(
            codex_home,
            endpoint,
            self.auth_manager.clone(),
        )))
    }

    fn api_provider(&self) -> ModelProviderFuture<'_, CodexResult<Provider>> {
        // Copilot sessions do not require a CodexAuth for provider
        // construction, so skip the default trait path's self.auth().await.
        Box::pin(async move {
            self.info().to_api_provider(/*auth_mode*/ None)
        })
    }

    fn api_auth(&self) -> ModelProviderFuture<'_, CodexResult<SharedAuthProvider>> {
        Box::pin(async move {
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
            let provider: SharedAuthProvider =
                Arc::new(CoreAuthProvider::new_legacy(None, None).with_copilot(Arc::new(source)));
            Ok(provider)
        })
    }

    #[cfg(any(test, feature = "test-support"))]
    fn inject_copilot_auth_for_tests(&self, auth: Arc<CopilotAuth>) -> Result<(), &'static str> {
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
    use std::path::Path;
    use std::path::PathBuf;
    use std::sync::Arc;

    use codex_copilot::CopilotAuth;
    use codex_copilot::paths::AppPaths;
    use codex_login::AuthManager;
    use codex_login::CodexAuth;
    use codex_model_provider_info::create_copilot_provider;
    use codex_models_manager::ModelsManagerConfig;
    use codex_models_manager::manager::RefreshStrategy;
    use codex_protocol::openai_models::ConfigShellToolType;
    use codex_protocol::openai_models::ModelInfo;
    use codex_protocol::openai_models::ModelPreset;
    use codex_protocol::openai_models::ModelVisibility;
    use codex_protocol::openai_models::ModelWireRoute;
    use codex_protocol::openai_models::ModelsResponse;
    use codex_protocol::openai_models::ReasoningEffortPreset;
    use codex_protocol::openai_models::TruncationPolicyConfig;
    use codex_protocol::openai_models::WebSearchToolType;
    use codex_protocol::openai_models::default_input_modalities;
    use pretty_assertions::assert_eq;
    use reqwest::header::AUTHORIZATION;
    use reqwest::header::HeaderMap;
    use wiremock::Mock;
    use wiremock::MockServer;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    use super::CopilotModelProvider;
    use crate::anthropic_gate::install_anthropic_gate;
    use crate::provider::ModelProvider;

    struct TestTempDir {
        path: PathBuf,
    }

    impl TestTempDir {
        fn new() -> Self {
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system time should be after unix epoch")
                .as_nanos();
            let path = std::env::temp_dir().join(format!(
                "codex-copilot-auth-test-{}-{}",
                std::process::id(),
                nonce
            ));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }
    }

    impl Drop for TestTempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    fn test_copilot_auth(server: &MockServer) -> (TestTempDir, Arc<CopilotAuth>) {
        let temp = TestTempDir::new();
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

    fn test_model(slug: &str, wire_route: ModelWireRoute) -> ModelInfo {
        ModelInfo {
            slug: slug.to_string(),
            display_name: slug.to_string(),
            description: None,
            default_reasoning_level: None,
            supported_reasoning_levels: Vec::<ReasoningEffortPreset>::new(),
            shell_type: ConfigShellToolType::ShellCommand,
            visibility: ModelVisibility::List,
            supported_in_api: true,
            priority: 50,
            additional_speed_tiers: Vec::new(),
            service_tiers: Vec::new(),
            default_service_tier: None,
            availability_nux: None,
            upgrade: None,
            base_instructions: String::new(),
            model_messages: None,
            supports_reasoning_summaries: false,
            default_reasoning_summary: codex_protocol::config_types::ReasoningSummary::Auto,
            support_verbosity: false,
            default_verbosity: None,
            apply_patch_tool_type: None,
            web_search_tool_type: WebSearchToolType::Text,
            truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
            supports_parallel_tool_calls: false,
            supports_image_detail_original: false,
            context_window: Some(100_000),
            max_context_window: Some(100_000),
            auto_compact_token_limit: None,
            effective_context_window_percent: 95,
            experimental_supported_tools: Vec::new(),
            input_modalities: default_input_modalities(),
            used_fallback_model_metadata: false,
            supports_search_tool: false,
            wire_route,
            comp_hash: None,
            use_responses_lite: false,
            auto_review_model_override: None,
            tool_mode: None,
            multi_agent_version: None,
        }
    }

    fn test_catalog() -> ModelsResponse {
        ModelsResponse {
            models: vec![
                test_model("gpt-5.5", ModelWireRoute::ProviderDefault),
                test_model("claude-opus-4.8", ModelWireRoute::ChatCompletions),
            ],
        }
    }

    fn preset_slugs(models: &[ModelPreset]) -> Vec<&str> {
        models.iter().map(|model| model.model.as_str()).collect()
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn models_manager_replaces_persisted_anthropic_model_with_provider_default_when_gate_off()
    {
        install_anthropic_gate(false);
        let temp = TestTempDir::new();
        let model_provider = CopilotModelProvider::new(create_copilot_provider(), None);
        let models_manager =
            model_provider.models_manager(temp.path().to_path_buf(), Some(test_catalog()));

        let configured_model = Some("claude-opus-4.8".to_string());
        let selected_model = models_manager
            .get_default_model(&configured_model, RefreshStrategy::Offline)
            .await;
        let model_info = models_manager
            .get_model_info(&selected_model, &ModelsManagerConfig::default())
            .await;

        install_anthropic_gate(false);

        assert_eq!(selected_model, "gpt-5.5");
        assert_eq!(model_info.slug, "gpt-5.5");
        assert_eq!(model_info.wire_route, ModelWireRoute::ProviderDefault);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn models_manager_catalog_branch_filters_anthropic_models_when_gate_off() {
        install_anthropic_gate(false);
        let temp = TestTempDir::new();
        let model_provider = CopilotModelProvider::new(create_copilot_provider(), None);
        let models_manager =
            model_provider.models_manager(temp.path().to_path_buf(), Some(test_catalog()));

        assert_eq!(
            preset_slugs(&models_manager.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            preset_slugs(&models_manager.try_list_models().expect("try list")),
            vec!["gpt-5.5"]
        );

        install_anthropic_gate(false);
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
        let chatgpt_auth_manager =
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing());
        let provider = create_copilot_provider();
        let model_provider = CopilotModelProvider::new(provider, Some(chatgpt_auth_manager));
        model_provider
            .inject_copilot_auth_for_tests(copilot_auth)
            .expect("seed test copilot auth");

        // SANDBOX PATCH: lock in `ModelProvider::auth()` returning `None` for Copilot
        // sessions. `core/src/client.rs::current_client_setup` reads `provider.auth()`
        // to decide whether to attach a ChatGPT bearer via `BearerAuthProvider`;
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

    #[tokio::test]
    async fn api_auth_rejects_copilot_with_untrusted_https_base_url() {
        let mut provider = create_copilot_provider();
        provider.base_url = Some("https://evil.example.com".to_string());
        let model_provider = CopilotModelProvider::new(provider, None);

        let err = model_provider
            .api_auth()
            .await
            .err()
            .expect("untrusted https base_url must fail closed");

        let message = err.to_string();
        assert!(
            message.contains("Copilot provider base_url override not allowed"),
            "unexpected error: {message}",
        );
    }
}
