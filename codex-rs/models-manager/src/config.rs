use codex_protocol::openai_models::ContextWindowTier;
use codex_protocol::openai_models::ModelsResponse;

#[derive(Debug, Clone, Default)]
pub struct ModelsManagerConfig {
    pub model_context_window: Option<i64>,
    pub model_auto_compact_token_limit: Option<i64>,
    pub tool_output_token_limit: Option<usize>,
    pub base_instructions: Option<String>,
    pub personality_enabled: bool,
    pub model_supports_reasoning_summaries: Option<bool>,
    pub model_catalog: Option<ModelsResponse>,
    // SANDBOX PATCH: Knob B context-window tier. The user-selected tier
    // (`default | long_context`). Applied in `with_config_overrides` before the
    // numeric `model_context_window` clamp. See patch-surface §14.
    pub model_context_tier: Option<ContextWindowTier>,
}
