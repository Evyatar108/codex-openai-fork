// SANDBOX PATCH: Copilot-aware ModelProvider. Routes api_auth() through
// CopilotHeaderSource + CoreAuthProvider.with_copilot, so call sites can use
// provider.api_auth().await? uniformly without branching on is_copilot().

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
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use tokio::sync::OnceCell;
use tracing::warn;

use crate::provider::ModelProvider;

/// Runtime model provider for Copilot sessions. Owns the session-scoped
/// `CopilotAuth` (which caches GitHub/Copilot tokens on disk + in memory) and
/// builds a fresh `CopilotHeaderSource` on every `api_auth()` call so that
/// `CoreAuthProvider::on_unauthorized()` -> `invalidate()` semantics remain
/// correct: after a 401/403 the caller drops the old `SharedAuthProvider`, and
/// the next `api_auth()` rebuilds from a fresh header source (token re-fetched
/// from GitHub because `invalidate` cleared the disk cache).
pub(crate) struct CopilotModelProvider {
    info: ModelProviderInfo,
    auth_manager: Option<Arc<AuthManager>>,
    copilot_auth: OnceCell<Arc<CopilotAuth>>,
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
            copilot_auth: OnceCell::new(),
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

    async fn api_provider(&self) -> CodexResult<Provider> {
        // Copilot sessions do not require a CodexAuth for provider
        // construction, so skip the default trait path's self.auth().await.
        self.info().to_api_provider(/*auth_mode*/ None)
    }

    async fn api_auth(&self) -> CodexResult<SharedAuthProvider> {
        if !self.info.is_copilot_trusted() {
            warn!(
                "Copilot provider base_url override detected; not injecting Copilot credentials"
            );
            return Ok(Arc::new(CoreAuthProvider::new_legacy(None, None)));
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
