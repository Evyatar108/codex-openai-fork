use std::sync::Arc;
#[cfg(test)]
use std::sync::atomic::AtomicBool;
#[cfg(test)]
use std::sync::atomic::Ordering;

use codex_login::AuthManager;
use codex_models_manager::ModelsManagerConfig;
use codex_models_manager::manager::ModelsManager;
use codex_models_manager::manager::ModelsManagerFuture;
use codex_models_manager::manager::RefreshStrategy;
use codex_models_manager::manager::SharedModelsManager;
use codex_models_manager::model_info;
use codex_protocol::config_types::CollaborationModeMask;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelPreset;
use codex_protocol::openai_models::ModelWireRoute;
use codex_protocol::openai_models::ModelsResponse;
use tokio::sync::TryLockError;

use crate::anthropic_gate::anthropic_models_resolved;

#[derive(Debug)]
pub(crate) struct GatedModelsManager {
    inner: SharedModelsManager,
    anthropic_gate: AnthropicGate,
}

#[derive(Debug)]
enum AnthropicGate {
    Global,
    #[cfg(test)]
    Test(Arc<AtomicBool>),
}

impl AnthropicGate {
    fn enabled(&self) -> bool {
        match self {
            // SANDBOX PATCH: Read the process-global Anthropic feature gate at
            // filter time so `/experimental` changes apply to existing managers.
            Self::Global => anthropic_models_resolved(),
            #[cfg(test)]
            Self::Test(gate) => gate.load(Ordering::Relaxed),
        }
    }
}

impl GatedModelsManager {
    pub(crate) fn wrap(inner: SharedModelsManager) -> SharedModelsManager {
        Arc::new(Self {
            inner,
            anthropic_gate: AnthropicGate::Global,
        })
    }

    fn keep_model_slug(&self, model: &str) -> bool {
        self.anthropic_enabled() || !is_anthropic_model_slug(model)
    }

    fn keep_model(&self, model: &ModelInfo) -> bool {
        self.keep_model_slug(&model.slug)
            && (self.anthropic_enabled() || model.wire_route != ModelWireRoute::ChatCompletions)
    }

    fn anthropic_enabled(&self) -> bool {
        self.anthropic_gate.enabled()
    }

    fn filter_model_infos(&self, models: Vec<ModelInfo>) -> Vec<ModelInfo> {
        models
            .into_iter()
            .filter(|model| self.keep_model(model))
            .collect()
    }

    fn default_model_from_presets(models: Vec<ModelPreset>) -> String {
        models
            .iter()
            .find(|model| model.is_default)
            .or_else(|| models.first())
            .map(|model| model.model.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    fn wrap_with_test_gate(
        inner: SharedModelsManager,
        gate: Arc<AtomicBool>,
    ) -> SharedModelsManager {
        Arc::new(Self {
            inner,
            anthropic_gate: AnthropicGate::Test(gate),
        })
    }
}

impl ModelsManager for GatedModelsManager {
    fn list_models(
        &self,
        refresh_strategy: RefreshStrategy,
    ) -> ModelsManagerFuture<'_, Vec<ModelPreset>> {
        Box::pin(async move {
            let catalog = self.raw_model_catalog(refresh_strategy).await;
            self.build_available_models(catalog.models)
        })
    }

    fn raw_model_catalog(
        &self,
        refresh_strategy: RefreshStrategy,
    ) -> ModelsManagerFuture<'_, ModelsResponse> {
        Box::pin(async move {
            let mut catalog = self.inner.raw_model_catalog(refresh_strategy).await;
            catalog.models = self.filter_model_infos(catalog.models);
            catalog
        })
    }

    fn get_remote_models(&self) -> ModelsManagerFuture<'_, Vec<ModelInfo>> {
        Box::pin(async move { self.filter_model_infos(self.inner.get_remote_models().await) })
    }

    fn try_get_remote_models(&self) -> Result<Vec<ModelInfo>, TryLockError> {
        self.inner
            .try_get_remote_models()
            .map(|models| self.filter_model_infos(models))
    }

    fn auth_manager(&self) -> Option<&AuthManager> {
        self.inner.auth_manager()
    }

    fn list_collaboration_modes(&self) -> Vec<CollaborationModeMask> {
        self.inner.list_collaboration_modes()
    }

    fn try_list_models(&self) -> Result<Vec<ModelPreset>, TryLockError> {
        let remote_models = self.try_get_remote_models()?;
        Ok(self.build_available_models(remote_models))
    }

    fn get_default_model<'a>(
        &'a self,
        model: &'a Option<String>,
        refresh_strategy: RefreshStrategy,
    ) -> ModelsManagerFuture<'a, String> {
        Box::pin(async move {
            if let Some(model) = model.as_ref() {
                if self.keep_model_slug(model) {
                    let info = self
                        .inner
                        .get_model_info(model, &ModelsManagerConfig::default())
                        .await;
                    if self.keep_model(&info) {
                        return model.clone();
                    }
                }
            }

            Self::default_model_from_presets(self.list_models(refresh_strategy).await)
        })
    }

    fn get_model_info<'a>(
        &'a self,
        model: &'a str,
        config: &'a ModelsManagerConfig,
    ) -> ModelsManagerFuture<'a, ModelInfo> {
        Box::pin(async move {
            if self.keep_model_slug(model) {
                let info = self.inner.get_model_info(model, config).await;
                if self.keep_model(&info) {
                    return info;
                }
            }
            let fallback =
                Self::default_model_from_presets(self.list_models(RefreshStrategy::Offline).await);
            model_info::with_config_overrides(model_info::model_info_from_slug(&fallback), config)
        })
    }

    fn refresh_if_new_etag(&self, etag: String) -> ModelsManagerFuture<'_, ()> {
        Box::pin(async move {
            self.inner.refresh_if_new_etag(etag).await;
        })
    }
}

fn is_anthropic_model_slug(model: &str) -> bool {
    let model = model.to_ascii_lowercase();
    model.starts_with("claude") || model.contains("anthropic")
}

// The decorator gates every remote-model read method, each of which backs a
// distinct user-facing surface (D-002 defense-in-depth, gate 2):
//   - `list_models`     -> sub-agent v1/v2 `available_models` (US-005) + cache reads.
//   - `try_list_models` -> the `/model` picker catalog (US-006).
//   - `raw_model_catalog`/`get_remote_models`/`try_get_remote_models` -> stale
//     `models_cache.json` + config-supplied catalog reads (US-003).
//   - `get_default_model`/`get_model_info` -> an inherited/pinned Claude model
//     (US-005 inherited path; the routing gate in `core::chat_transport` is the
//     companion fail-closed defense).
#[cfg(test)]
mod tests {
    use super::*;
    use codex_models_manager::manager::StaticModelsManager;
    use codex_protocol::openai_models::ConfigShellToolType;
    use codex_protocol::openai_models::ModelVisibility;
    use codex_protocol::openai_models::ReasoningEffortPreset;
    use codex_protocol::openai_models::TruncationPolicyConfig;
    use codex_protocol::openai_models::WebSearchToolType;
    use codex_protocol::openai_models::default_input_modalities;
    use pretty_assertions::assert_eq;

    fn model(slug: &str, wire_route: ModelWireRoute) -> ModelInfo {
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

    fn manager(anthropic_enabled: bool) -> (SharedModelsManager, Arc<AtomicBool>) {
        let models = ModelsResponse {
            models: vec![
                model("gpt-5.5", ModelWireRoute::ProviderDefault),
                model("claude-sonnet-4.6", ModelWireRoute::ChatCompletions),
            ],
        };
        let gate = Arc::new(AtomicBool::new(anthropic_enabled));
        let manager = GatedModelsManager::wrap_with_test_gate(
            Arc::new(StaticModelsManager::new(/*auth_manager*/ None, models)),
            Arc::clone(&gate),
        );
        (manager, gate)
    }

    fn slugs(models: &[ModelInfo]) -> Vec<&str> {
        models.iter().map(|model| model.slug.as_str()).collect()
    }

    fn preset_slugs(models: &[ModelPreset]) -> Vec<&str> {
        models.iter().map(|model| model.model.as_str()).collect()
    }

    #[tokio::test]
    async fn off_filters_chat_completions_from_all_catalog_reads() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ false);

        assert_eq!(slugs(&manager.get_remote_models().await), vec!["gpt-5.5"]);
        assert_eq!(
            slugs(
                &manager
                    .raw_model_catalog(RefreshStrategy::Offline)
                    .await
                    .models
            ),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            slugs(&manager.try_get_remote_models().expect("try remote")),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            preset_slugs(&manager.try_list_models().expect("try list")),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            preset_slugs(&manager.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5"]
        );
    }

    #[tokio::test]
    async fn on_preserves_chat_completions_models() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ true);

        assert_eq!(
            slugs(&manager.get_remote_models().await),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            preset_slugs(&manager.try_list_models().expect("try list")),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
    }

    #[tokio::test]
    async fn off_does_not_resolve_chat_completions_metadata_for_explicit_model() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ false);

        let info = manager
            .get_model_info("claude-sonnet-4.6", &ModelsManagerConfig::default())
            .await;

        assert_eq!(info.slug, "gpt-5.5");
        assert_eq!(info.wire_route, ModelWireRoute::ProviderDefault);
        assert!(info.used_fallback_model_metadata);
    }

    #[tokio::test]
    async fn off_replaces_inherited_chat_completions_model_with_filtered_default() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ false);

        let selected = manager
            .get_default_model(
                &Some("claude-sonnet-4.6".to_string()),
                RefreshStrategy::Offline,
            )
            .await;

        assert_eq!(selected, "gpt-5.5");
    }

    #[tokio::test]
    async fn off_replaces_unavailable_anthropic_slug_with_filtered_default() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ false);

        let selected = manager
            .get_default_model(
                &Some("claude-not-in-catalog".to_string()),
                RefreshStrategy::Offline,
            )
            .await;

        assert_eq!(selected, "gpt-5.5");
    }

    #[tokio::test]
    async fn off_preserves_custom_non_anthropic_slug() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ false);

        let selected = manager
            .get_default_model(&Some("custom-model".to_string()), RefreshStrategy::Offline)
            .await;

        assert_eq!(selected, "custom-model");
    }

    #[tokio::test]
    async fn on_preserves_inherited_chat_completions_model() {
        let (manager, _gate) = manager(/*anthropic_enabled*/ true);

        let selected = manager
            .get_default_model(
                &Some("claude-sonnet-4.6".to_string()),
                RefreshStrategy::Offline,
            )
            .await;

        assert_eq!(selected, "claude-sonnet-4.6");
    }

    /// US-005: sub-agent model selection — v1 `multi_agent_v1.spawn_agent` AND v2
    /// `spawn_agent` — is gated through this decorator. Both call sites share
    /// `core/src/tools/handlers/multi_agents_common.rs::apply_requested_spawn_agent_model_overrides`,
    /// which sources `available_models` from `models_manager.list_models(Offline)` and
    /// rejects an unknown explicit `model` via `find_spawn_agent_model_name`
    /// ("Unknown model `..` for spawn_agent"). When off, the gated `list_models` omits
    /// every `ChatCompletions` (Claude) row, so an explicit Claude `model` is rejected
    /// for BOTH v1 and v2; when on, Claude is selectable. (The inherited-model path is
    /// covered by `off_replaces_inherited_chat_completions_model_with_filtered_default`
    /// plus the `core::chat_transport` routing fail-closed test.)
    #[tokio::test]
    async fn off_excludes_claude_from_spawn_agent_available_models() {
        let (off, _off_gate) = manager(/*anthropic_enabled*/ false);
        assert_eq!(
            preset_slugs(&off.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5"]
        );

        let (on, _on_gate) = manager(/*anthropic_enabled*/ true);
        assert_eq!(
            preset_slugs(&on.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
    }

    #[tokio::test]
    async fn test_gate_flips_after_construction_for_all_catalog_reads() {
        let (manager, gate) = manager(/*anthropic_enabled*/ false);

        assert_eq!(
            preset_slugs(&manager.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            manager
                .get_default_model(
                    &Some("claude-sonnet-4.6".to_string()),
                    RefreshStrategy::Offline,
                )
                .await,
            "gpt-5.5"
        );
        assert_eq!(
            manager
                .get_model_info("claude-sonnet-4.6", &ModelsManagerConfig::default())
                .await
                .slug,
            "gpt-5.5"
        );

        gate.store(true, Ordering::Relaxed);

        assert_eq!(
            slugs(&manager.get_remote_models().await),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            slugs(
                &manager
                    .raw_model_catalog(RefreshStrategy::Offline)
                    .await
                    .models
            ),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            slugs(&manager.try_get_remote_models().expect("try remote")),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            preset_slugs(&manager.try_list_models().expect("try list")),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            preset_slugs(&manager.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5", "claude-sonnet-4.6"]
        );
        assert_eq!(
            manager
                .get_default_model(
                    &Some("claude-sonnet-4.6".to_string()),
                    RefreshStrategy::Offline,
                )
                .await,
            "claude-sonnet-4.6"
        );
        assert_eq!(
            manager
                .get_model_info("claude-sonnet-4.6", &ModelsManagerConfig::default())
                .await
                .slug,
            "claude-sonnet-4.6"
        );

        gate.store(false, Ordering::Relaxed);

        assert_eq!(
            preset_slugs(&manager.list_models(RefreshStrategy::Offline).await),
            vec!["gpt-5.5"]
        );
        assert_eq!(
            manager
                .get_default_model(
                    &Some("claude-sonnet-4.6".to_string()),
                    RefreshStrategy::Offline,
                )
                .await,
            "gpt-5.5"
        );
    }
}
