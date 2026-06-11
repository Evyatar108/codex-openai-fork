use codex_utils_absolute_path::AbsolutePathBuf;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

use crate::config_toml::ToolsToml;
use crate::types::AnalyticsConfigToml;
use crate::types::ApprovalsReviewer;
use crate::types::Personality;
use crate::types::SessionPickerViewMode;
use crate::types::WindowsToml;
use codex_features::FeaturesToml;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::config_types::SandboxMode;
use codex_protocol::config_types::Verbosity;
use codex_protocol::config_types::WebSearchMode;
use codex_protocol::openai_models::ContextWindowTier;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::AskForApproval;

/// Collection of common configuration options that a user can define as a unit
/// in `config.toml`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[schemars(deny_unknown_fields)]
pub struct ConfigProfile {
    pub model: Option<String>,
    /// Optional explicit service tier request id for new turns (for example
    /// `default`, `priority`, or `flex`; legacy `fast` also works).
    pub service_tier: Option<String>,
    /// The key in the `model_providers` map identifying the
    /// [`ModelProviderInfo`] to use.
    pub model_provider: Option<String>,
    pub approval_policy: Option<AskForApproval>,
    pub approvals_reviewer: Option<ApprovalsReviewer>,
    pub sandbox_mode: Option<SandboxMode>,
    pub model_reasoning_effort: Option<ReasoningEffort>,
    pub plan_mode_reasoning_effort: Option<ReasoningEffort>,
    pub model_reasoning_summary: Option<ReasoningSummary>,
    pub model_verbosity: Option<Verbosity>,
    // SANDBOX PATCH: Knob B context-window tier. Profile parity with the
    // top-level `model_context_tier` key so a profile-selected model carries
    // its tier (Finding-4). See patch-surface §14.
    pub model_context_tier: Option<ContextWindowTier>,
    /// Optional path to a JSON model catalog (applied on startup only).
    pub model_catalog_json: Option<AbsolutePathBuf>,
    pub personality: Option<Personality>,
    pub chatgpt_base_url: Option<String>,
    /// Optional path to a file containing model instructions.
    pub model_instructions_file: Option<AbsolutePathBuf>,
    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_path: Option<AbsolutePathBuf>,
    /// Deprecated: ignored.
    #[schemars(skip)]
    pub js_repl_node_module_dirs: Option<Vec<AbsolutePathBuf>>,
    pub experimental_compact_prompt_file: Option<AbsolutePathBuf>,
    pub include_permissions_instructions: Option<bool>,
    pub include_apps_instructions: Option<bool>,
    pub include_collaboration_mode_instructions: Option<bool>,
    pub include_environment_context: Option<bool>,
    pub experimental_use_unified_exec_tool: Option<bool>,
    pub tools: Option<ToolsToml>,
    pub web_search: Option<WebSearchMode>,
    pub analytics: Option<AnalyticsConfigToml>,
    /// TUI settings scoped to this profile.
    #[serde(default)]
    pub tui: Option<ProfileTui>,
    #[serde(default)]
    pub windows: Option<WindowsToml>,
    /// Optional feature toggles scoped to this profile.
    #[serde(default)]
    // Injects known feature keys into the schema and forbids unknown keys.
    #[schemars(schema_with = "crate::schema::features_schema")]
    pub features: Option<FeaturesToml>,
    pub oss_provider: Option<String>,
}

/// TUI settings supported inside a named profile.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(deny_unknown_fields)]
pub struct ProfileTui {
    /// Preferred layout for resume/fork session picker results.
    #[serde(default)]
    pub session_picker_view: Option<SessionPickerViewMode>,
}

#[cfg(test)]
mod tests {
    use super::ConfigProfile;
    use codex_protocol::openai_models::ContextWindowTier;
    use pretty_assertions::assert_eq;

    #[test]
    fn profile_model_context_tier_round_trips() {
        let profile: ConfigProfile =
            toml::from_str("model_context_tier = \"long_context\"").expect("profile should parse");
        assert_eq!(
            profile.model_context_tier,
            Some(ContextWindowTier::LongContext)
        );

        let serialized = toml::to_string(&profile).expect("profile should serialize");
        let reparsed: ConfigProfile =
            toml::from_str(&serialized).expect("serialized profile should parse");
        assert_eq!(reparsed, profile);
    }

    #[test]
    fn profile_rejects_unknown_context_tier_key() {
        let path = std::path::Path::new("/tmp/profile.toml");
        let error = crate::strict_config::config_error_from_ignored_toml_fields::<ConfigProfile>(
            path,
            "model_context_tier_typo = \"long_context\"",
        )
        .expect("unknown field should be rejected");
        let message = error.message;
        assert!(message.contains("unknown configuration field"));
        assert!(message.contains("model_context_tier_typo"));
    }
}
