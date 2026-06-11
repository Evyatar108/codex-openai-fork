// SANDBOX PATCH: Live `/models` fetch for Copilot sessions.
//
// Copilot's `/models` endpoint returns a richer shape than upstream's OpenAI
// `/models` (it embeds a `data: [...]` array with `capabilities`, `policy`,
// `model_picker_enabled`, etc.), and authentication goes through the
// CopilotHeaderSource — not the standard `AuthProvider` path. This module
// implements `ModelsEndpointClient` against that shape so that
// `OpenAiModelsManager` can drive cache + refresh policy normally.
//
// Filtering: only models with `model_picker_enabled=true`, `capabilities.type
// == "chat"` (or absent), and `supported_endpoints` containing `/responses`
// are surfaced — codex's Copilot transport is hard-wired to the Responses
// API, so a `/chat/completions`-only entry would be unusable as a default.
//
// Translation: for slugs already present in the bundled `models.json`,
// preserve the bundled metadata (base_instructions, reasoning levels,
// truncation policy, ...) and only override `display_name` / `visibility` /
// `used_fallback_model_metadata`. For slugs that Copilot ships before our
// next rebase pulls them into the bundle, synthesize a minimal `ModelInfo`
// from `capabilities`.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use codex_copilot::CopilotAuth;
use codex_copilot::CopilotHeaderSource;
use codex_login::default_client::build_reqwest_client;
use codex_models_manager::bundled_models_response;
use codex_models_manager::manager::ModelsEndpointClient;
use codex_models_manager::model_info::BASE_INSTRUCTIONS;
use codex_protocol::config_types::ReasoningSummary;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CoreResult;
use codex_protocol::openai_models::ConfigShellToolType;
use codex_protocol::openai_models::ModelInfo;
use codex_protocol::openai_models::ModelVisibility;
use codex_protocol::openai_models::ModelWireRoute;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::openai_models::ReasoningEffortPreset;
use codex_protocol::openai_models::TruncationPolicyConfig;
use codex_protocol::openai_models::WebSearchToolType;
use codex_protocol::openai_models::default_input_modalities;
use http::HeaderMap;
use serde::Deserialize;
use tokio::sync::OnceCell;
use tokio::time::timeout;
use tracing::warn;

use crate::anthropic_gate::anthropic_models_resolved;

const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_PATH: &str = "/models";
const COPILOT_RESPONSES_ENDPOINT: &str = "/responses";
// SANDBOX PATCH: D-001 Claude-via-Copilot chat-completions transport.
const COPILOT_CHAT_ENDPOINT: &str = "/chat/completions";

#[derive(Debug, Deserialize)]
struct CopilotModelsResponse {
    #[serde(default)]
    data: Vec<CopilotModelEntry>,
}

#[derive(Debug, Deserialize)]
struct CopilotModelEntry {
    id: String,
    #[serde(default)]
    name: Option<String>,
    // SANDBOX PATCH: Knob B context-window tier. Vendor drives the per-family
    // curated default tier (OpenAI=400k, Anthropic=264k). See patch-surface §14.
    #[serde(default)]
    vendor: Option<String>,
    #[serde(default)]
    model_picker_enabled: bool,
    #[serde(default)]
    supported_endpoints: Vec<String>,
    #[serde(default)]
    capabilities: Option<CopilotCapabilities>,
}

#[derive(Debug, Deserialize)]
struct CopilotCapabilities {
    #[serde(default, rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    limits: Option<CopilotLimits>,
    #[serde(default)]
    supports: Option<CopilotSupports>,
}

#[derive(Debug, Deserialize)]
struct CopilotLimits {
    #[serde(default)]
    max_context_window_tokens: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct CopilotSupports {
    #[serde(default)]
    parallel_tool_calls: bool,
    #[serde(default)]
    reasoning_effort: Vec<String>,
    #[serde(default)]
    vision: bool,
}

#[derive(Debug)]
pub(crate) struct CopilotModelsEndpoint {
    base_url: String,
    auth: Arc<OnceCell<Arc<CopilotAuth>>>,
}

impl CopilotModelsEndpoint {
    pub(crate) fn new(base_url: String, auth: Arc<OnceCell<Arc<CopilotAuth>>>) -> Self {
        Self { base_url, auth }
    }

    async fn copilot_auth(&self) -> CoreResult<Arc<CopilotAuth>> {
        self.auth
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
impl ModelsEndpointClient for CopilotModelsEndpoint {
    fn has_command_auth(&self) -> bool {
        // Copilot sessions provide their own per-request auth via
        // CopilotHeaderSource. Returning true tells `OpenAiModelsManager`
        // to refresh on demand without consulting `AuthManager`.
        true
    }

    async fn uses_codex_backend(&self) -> bool {
        false
    }

    async fn list_models(
        &self,
        _client_version: &str,
    ) -> CoreResult<(Vec<ModelInfo>, Option<String>)> {
        let url = format!("{}{}", self.base_url.trim_end_matches('/'), MODELS_PATH);

        let auth = self.copilot_auth().await?;
        let header_source = CopilotHeaderSource::new(auth)
            .await
            .map_err(|err| CodexErr::Fatal(format!("copilot header source: {err}")))?;
        let mut headers = HeaderMap::new();
        header_source.inject(&mut headers);
        // The base CopilotHeaderSource caches request-id-style headers from
        // construction time. /models is a one-shot GET, so reusing the cached
        // values is fine; the per-request mutation hook (used for streaming
        // Responses turns) is not relevant here.

        let client = build_reqwest_client();
        let request = client.get(&url).headers(headers);

        let response = timeout(MODELS_REFRESH_TIMEOUT, request.send())
            .await
            .map_err(|_| CodexErr::Timeout)?
            .map_err(|err| CodexErr::Fatal(format!("copilot /models request: {err}")))?;

        let status = response.status();
        let etag = response
            .headers()
            .get(http::header::ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);

        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(CodexErr::Fatal(format!(
                "copilot /models returned {status}: {}",
                truncate(&body, 256)
            )));
        }

        let body: CopilotModelsResponse = response
            .json()
            .await
            .map_err(|err| CodexErr::Fatal(format!("copilot /models decode: {err}")))?;

        let bundled = bundled_models_response()
            .map(|resp| resp.models)
            .unwrap_or_default();

        let anthropic_enabled = anthropic_models_resolved();
        let models = body
            .data
            .into_iter()
            .filter(|entry| is_chat_responses_picker_entry(entry, anthropic_enabled))
            .map(|entry| translate_entry(entry, &bundled, anthropic_enabled))
            .collect::<Vec<_>>();

        if models.is_empty() {
            warn!("copilot /models returned no /responses-capable, picker-enabled chat models");
        }
        Ok((models, etag))
    }
}

// SANDBOX PATCH: D-001/D-002. Surface chat-only Claude rows only when the
// Anthropic transport opt-in is enabled. GPT `/responses` rows stay unaffected.
fn is_chat_responses_picker_entry(entry: &CopilotModelEntry, anthropic_enabled: bool) -> bool {
    if !entry.model_picker_enabled {
        return false;
    }
    let kind_ok = entry
        .capabilities
        .as_ref()
        .and_then(|c| c.kind.as_deref())
        .is_none_or(|t| t == "chat");
    if !kind_ok {
        return false;
    }
    let has_responses = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_RESPONSES_ENDPOINT);
    let has_chat = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_CHAT_ENDPOINT);
    has_responses || (anthropic_enabled && has_chat)
}

// SANDBOX PATCH: D-001/D-002. A row advertising `/chat/completions` but NOT
// `/responses` routes over the chat transport only when Anthropic is opted in;
// everything else keeps the provider default.
fn wire_route_for(entry: &CopilotModelEntry, anthropic_enabled: bool) -> ModelWireRoute {
    let has_responses = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_RESPONSES_ENDPOINT);
    let has_chat = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_CHAT_ENDPOINT);
    if anthropic_enabled && has_chat && !has_responses {
        ModelWireRoute::ChatCompletions
    } else {
        ModelWireRoute::ProviderDefault
    }
}

// SANDBOX PATCH: Knob B context-window tier. The Copilot `/models` response does
// NOT expose a per-tier context field (confirmed by a live capture — see the
// spike findings). The "default" tier is a client-curated cap; the "full" tier
// is the live `max_context_window_tokens`. These per-family defaults match the
// VS Code Copilot CLI's curated defaults. See docs/implementation/patch-surface.md §14.
const GPT_DEFAULT_CONTEXT_TIER_TOKENS: i64 = 400_000;
const ANTHROPIC_DEFAULT_CONTEXT_TIER_TOKENS: i64 = 264_000;

/// The per-family curated default-tier context window, or `None` for families
/// without a curated default (those keep their single live window / bundled
/// default). Vendor is authoritative; the slug prefix is a fallback for entries
/// that omit `vendor`.
fn curated_default_context_tier(vendor: Option<&str>, slug: &str) -> Option<i64> {
    let is_gpt = vendor.is_some_and(|v| v.eq_ignore_ascii_case("openai")) || slug.starts_with("gpt-");
    let is_anthropic =
        vendor.is_some_and(|v| v.eq_ignore_ascii_case("anthropic")) || slug.starts_with("claude");
    if is_gpt {
        Some(GPT_DEFAULT_CONTEXT_TIER_TOKENS)
    } else if is_anthropic {
        Some(ANTHROPIC_DEFAULT_CONTEXT_TIER_TOKENS)
    } else {
        None
    }
}

/// Resolve the `(context_window, max_context_window)` pair for a model from its
/// full window and a chosen default-tier cap. The default is clamped to the full
/// window so a single-tier model (`full <= default`) collapses to one window and
/// never offers a `default | long_context` toggle.
fn knob_b_tier_windows(full: Option<i64>, chosen_default: Option<i64>) -> (Option<i64>, Option<i64>) {
    match full {
        Some(full) => {
            let default = chosen_default.map_or(full, |candidate| candidate.min(full));
            (Some(default), Some(full))
        }
        None => (None, None),
    }
}

fn translate_entry(
    entry: CopilotModelEntry,
    bundled: &[ModelInfo],
    anthropic_enabled: bool,
) -> ModelInfo {
    // SANDBOX PATCH: D-001 compute the wire-route before `entry` is consumed below.
    let wire_route = wire_route_for(&entry, anthropic_enabled);
    // SANDBOX PATCH: Knob B — capture the live full window + vendor before `entry`
    // fields are moved, so we can overlay the tier windows below.
    let live_max_context_window = entry
        .capabilities
        .as_ref()
        .and_then(|c| c.limits.as_ref())
        .and_then(|l| l.max_context_window_tokens);
    let vendor = entry.vendor.clone();
    if let Some(known) = bundled.iter().find(|info| info.slug == entry.id) {
        let mut info = known.clone();
        if let Some(name) = entry.name {
            info.display_name = name;
        }
        info.visibility = ModelVisibility::List;
        info.used_fallback_model_metadata = false;
        // SANDBOX PATCH: D-001 tag the bundled-clone row to its wire route.
        info.wire_route = wire_route;
        // SANDBOX PATCH: Knob B — overlay the live full window onto the (often
        // stale) bundled ceiling and apply the per-family curated default tier.
        // This is what recovers a model's long-context tier when the bundle was
        // cut before the full window shipped (e.g. gpt-5.5 bundle 272k vs live
        // 1.05M). For families without a curated default we preserve the bundled
        // default window, clamped to the full ceiling. See patch-surface §14.
        let full = live_max_context_window.or(info.max_context_window);
        let chosen_default =
            curated_default_context_tier(vendor.as_deref(), &info.slug).or(info.context_window);
        let (context_window, max_context_window) = knob_b_tier_windows(full, chosen_default);
        info.context_window = context_window;
        info.max_context_window = max_context_window;
        return info;
    }
    synthesize_from_capabilities(entry, anthropic_enabled)
}

fn synthesize_from_capabilities(entry: CopilotModelEntry, anthropic_enabled: bool) -> ModelInfo {
    // SANDBOX PATCH: D-001 derive the wire-route before `entry` fields are moved below.
    let wire_route = wire_route_for(&entry, anthropic_enabled);
    let supports = entry
        .capabilities
        .as_ref()
        .and_then(|c| c.supports.as_ref());
    let limits = entry.capabilities.as_ref().and_then(|c| c.limits.as_ref());

    let supported_reasoning_levels = supports
        .map(|s| {
            s.reasoning_effort
                .iter()
                .filter_map(|effort| {
                    serde_json::from_value::<ReasoningEffort>(serde_json::Value::String(
                        effort.clone(),
                    ))
                    .ok()
                })
                .map(|effort| ReasoningEffortPreset {
                    effort,
                    description: String::new(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    let full_context_window = limits.and_then(|l| l.max_context_window_tokens);
    // SANDBOX PATCH: Knob B — apply the per-family curated default tier. Full
    // tier = the live `max_context_window_tokens`; default tier = curated cap
    // clamped to full (so single-tier families collapse to one window). See
    // docs/implementation/patch-surface.md §14.
    let curated_default = curated_default_context_tier(entry.vendor.as_deref(), &entry.id);
    let (context_window, max_context_window) =
        knob_b_tier_windows(full_context_window, curated_default);
    let display_name = entry.name.unwrap_or_else(|| entry.id.clone());

    // SANDBOX PATCH: D-001 Knob A default. Copilot's `/models` lists reasoning
    // levels low-first, so `.first()` would default an UNSELECTED turn to
    // "low" — a regression for Claude, which previously ran at "medium" (the
    // chat-completions dispatch arm in core falls back to this
    // `default_reasoning_level` when no effort is explicitly selected). For the
    // Claude/Anthropic chat-completions route, prefer Medium when the model
    // advertises it; otherwise keep the lowest advertised level. GPT
    // `/responses` rows are `ProviderDefault` and keep their existing
    // low-first default untouched.
    let default_reasoning_level = if wire_route == ModelWireRoute::ChatCompletions
        && supported_reasoning_levels.iter().any(|p| p.effort == ReasoningEffort::Medium)
    {
        Some(ReasoningEffort::Medium)
    } else {
        supported_reasoning_levels.first().map(|p| p.effort)
    };

    ModelInfo {
        slug: entry.id,
        display_name,
        description: None,
        default_reasoning_level,
        supported_reasoning_levels,
        shell_type: ConfigShellToolType::ShellCommand,
        visibility: ModelVisibility::List,
        supported_in_api: true,
        priority: 50,
        additional_speed_tiers: Vec::new(),
        // SANDBOX PATCH: new field added upstream in v0.130; empty default
        // since Copilot doesn't enumerate service tiers in the /models wire shape.
        service_tiers: Vec::new(),
        default_service_tier: None,
        availability_nux: None,
        upgrade: None,
        base_instructions: BASE_INSTRUCTIONS.to_string(),
        model_messages: None,
        supports_reasoning_summaries: false,
        default_reasoning_summary: ReasoningSummary::Auto,
        support_verbosity: false,
        default_verbosity: None,
        apply_patch_tool_type: None,
        web_search_tool_type: WebSearchToolType::Text,
        truncation_policy: TruncationPolicyConfig::bytes(/*limit*/ 10_000),
        supports_parallel_tool_calls: supports.is_some_and(|s| s.parallel_tool_calls),
        supports_image_detail_original: supports.is_some_and(|s| s.vision),
        context_window,
        max_context_window,
        auto_compact_token_limit: None,
        effective_context_window_percent: 95,
        experimental_supported_tools: Vec::new(),
        input_modalities: default_input_modalities(),
        used_fallback_model_metadata: false,
        supports_search_tool: false,
        // SANDBOX PATCH: D-001 per-model chat-completions wire-route hint.
        wire_route,
    }
}

fn truncate(input: &str, max: usize) -> &str {
    if input.len() <= max {
        input
    } else {
        let mut end = max;
        while !input.is_char_boundary(end) {
            end -= 1;
        }
        &input[..end]
    }
}

// SANDBOX PATCH: D-001 chat-completions transport tests (filter guardrail + cache).
#[cfg(test)]
mod chat_transport_tests {
    use super::*;
    use serde_json::json;

    fn entry(value: serde_json::Value) -> CopilotModelEntry {
        serde_json::from_value(value).expect("valid entry")
    }

    #[test]
    fn chat_only_row_hidden_until_anthropic_enabled() {
        let chat_only = entry(json!({
            "id": "claude-sonnet-4.6",
            "model_picker_enabled": true,
            "supported_endpoints": ["/chat/completions", "/v1/messages"],
            "capabilities": { "type": "chat" }
        }));
        assert!(!is_chat_responses_picker_entry(
            &chat_only, /*anthropic_enabled*/ false
        ));
        assert!(is_chat_responses_picker_entry(
            &chat_only, /*anthropic_enabled*/ true
        ));
        assert_eq!(
            wire_route_for(&chat_only, /*anthropic_enabled*/ false),
            ModelWireRoute::ProviderDefault
        );
        assert_eq!(
            wire_route_for(&chat_only, /*anthropic_enabled*/ true),
            ModelWireRoute::ChatCompletions
        );

        // A /responses GPT row is surfaced regardless and keeps the provider default.
        let responses_row = entry(json!({
            "id": "gpt-5.5",
            "model_picker_enabled": true,
            "supported_endpoints": ["/responses"],
            "capabilities": { "type": "chat" }
        }));
        assert!(is_chat_responses_picker_entry(
            &responses_row,
            /*anthropic_enabled*/ false
        ));
        assert_eq!(
            wire_route_for(&responses_row, /*anthropic_enabled*/ false),
            ModelWireRoute::ProviderDefault
        );
    }

    #[test]
    fn wire_route_survives_models_cache_round_trip() {
        // A synthesized chat-only Claude row carries the ChatCompletions hint.
        let info = synthesize_from_capabilities(
            entry(json!({
                "id": "claude-sonnet-4.6",
                "model_picker_enabled": true,
                "supported_endpoints": ["/chat/completions", "/v1/messages"],
                "capabilities": { "type": "chat" }
            })),
            /*anthropic_enabled*/ true,
        );
        assert_eq!(info.wire_route, ModelWireRoute::ChatCompletions);

        // Serialize -> re-read (the models_cache.json round trip) preserves the hint.
        let serialized = serde_json::to_string(&info).expect("serialize");
        let reloaded: ModelInfo = serde_json::from_str(&serialized).expect("reload");
        assert_eq!(reloaded.wire_route, ModelWireRoute::ChatCompletions);

        // A pre-hint cache entry (field absent) deserializes to ProviderDefault.
        let mut without_hint: serde_json::Value = serde_json::from_str(&serialized).unwrap();
        without_hint.as_object_mut().unwrap().remove("wire_route");
        let legacy: ModelInfo = serde_json::from_value(without_hint).expect("legacy reload");
        assert_eq!(legacy.wire_route, ModelWireRoute::ProviderDefault);
    }

    #[test]
    fn gpt_responses_row_unaffected_by_gate_in_both_states() {
        // US-006 / AC #7: the opt-in gate must never touch a GPT `/responses` row. It
        // is admitted identically and keeps `ProviderDefault` whether Anthropic is
        // opted in or not — the gate branch is only reachable for chat-only rows.
        let responses_row = entry(json!({
            "id": "gpt-5.5",
            "model_picker_enabled": true,
            "supported_endpoints": ["/responses"],
            "capabilities": { "type": "chat" }
        }));
        for anthropic_enabled in [false, true] {
            assert!(is_chat_responses_picker_entry(
                &responses_row,
                anthropic_enabled
            ));
            assert_eq!(
                wire_route_for(&responses_row, anthropic_enabled),
                ModelWireRoute::ProviderDefault
            );
        }
    }

    #[test]
    fn synthesized_claude_defaults_reasoning_to_medium() {
        // Knob A default: an UNSELECTED Claude/Copilot turn must default to
        // "medium" — not the low-first `.first()` value — since the
        // chat-completions dispatch arm falls back to `default_reasoning_level`
        // when no effort is explicitly selected.
        let info = synthesize_from_capabilities(
            entry(json!({
                "id": "claude-opus-4.8",
                "model_picker_enabled": true,
                "supported_endpoints": ["/chat/completions", "/v1/messages"],
                "capabilities": {
                    "type": "chat",
                    "supports": { "reasoning_effort": ["low", "medium", "high", "xhigh"] }
                }
            })),
            /*anthropic_enabled*/ true,
        );
        assert_eq!(info.wire_route, ModelWireRoute::ChatCompletions);
        assert_eq!(info.default_reasoning_level, Some(ReasoningEffort::Medium));
        // Wire serialization (what the dispatch arm sends when unselected).
        assert_eq!(
            info.default_reasoning_level.map(|level| level.to_string()),
            Some("medium".to_string())
        );
    }

    #[test]
    fn synthesized_gpt_keeps_low_first_default() {
        // A synthesized GPT `/responses` row (ProviderDefault) must keep its
        // existing low-first default; the Medium preference is scoped to the
        // Claude/Anthropic chat-completions route only.
        let info = synthesize_from_capabilities(
            entry(json!({
                "id": "gpt-5.5",
                "model_picker_enabled": true,
                "supported_endpoints": ["/responses"],
                "capabilities": {
                    "type": "chat",
                    "supports": { "reasoning_effort": ["low", "medium", "high", "xhigh"] }
                }
            })),
            /*anthropic_enabled*/ true,
        );
        assert_eq!(info.wire_route, ModelWireRoute::ProviderDefault);
        assert_eq!(info.default_reasoning_level, Some(ReasoningEffort::Low));
    }

    #[test]
    fn synthesized_claude_without_medium_keeps_first() {
        // Guard the "else keep the model default" branch: a Claude chat row that
        // does not advertise Medium falls back to the lowest advertised level.
        let info = synthesize_from_capabilities(
            entry(json!({
                "id": "claude-haiku-mini",
                "model_picker_enabled": true,
                "supported_endpoints": ["/chat/completions", "/v1/messages"],
                "capabilities": {
                    "type": "chat",
                    "supports": { "reasoning_effort": ["low", "high"] }
                }
            })),
            /*anthropic_enabled*/ true,
        );
        assert_eq!(info.wire_route, ModelWireRoute::ChatCompletions);
        assert_eq!(info.default_reasoning_level, Some(ReasoningEffort::Low));
    }
}

// SANDBOX PATCH: Knob B context-window tier parser tests.
#[cfg(test)]
mod knob_b_tier_tests {
    use super::*;
    use serde_json::json;

    fn entry(value: serde_json::Value) -> CopilotModelEntry {
        serde_json::from_value(value).expect("valid entry")
    }

    fn synth(id: &str, vendor: &str, max_tokens: i64) -> ModelInfo {
        synthesize_from_capabilities(
            entry(json!({
                "id": id,
                "vendor": vendor,
                "model_picker_enabled": true,
                "supported_endpoints": ["/responses"],
                "capabilities": { "type": "chat", "limits": { "max_context_window_tokens": max_tokens } }
            })),
            /*anthropic_enabled*/ false,
        )
    }

    #[test]
    fn curated_default_context_tier_by_family() {
        assert_eq!(
            curated_default_context_tier(Some("OpenAI"), "gpt-5.5"),
            Some(GPT_DEFAULT_CONTEXT_TIER_TOKENS)
        );
        assert_eq!(
            curated_default_context_tier(Some("Anthropic"), "claude-opus-4.8"),
            Some(ANTHROPIC_DEFAULT_CONTEXT_TIER_TOKENS)
        );
        // Vendor absent -> slug-prefix fallback.
        assert_eq!(
            curated_default_context_tier(None, "gpt-5.4"),
            Some(GPT_DEFAULT_CONTEXT_TIER_TOKENS)
        );
        assert_eq!(
            curated_default_context_tier(None, "claude-sonnet-4.6"),
            Some(ANTHROPIC_DEFAULT_CONTEXT_TIER_TOKENS)
        );
        // Other families have no curated default.
        assert_eq!(curated_default_context_tier(Some("Google"), "gemini-3.1-pro-preview"), None);
    }

    #[test]
    fn knob_b_tier_windows_clamps_default_to_full() {
        assert_eq!(
            knob_b_tier_windows(Some(1_050_000), Some(400_000)),
            (Some(400_000), Some(1_050_000))
        );
        // default above full collapses to single-tier (default == full).
        assert_eq!(
            knob_b_tier_windows(Some(200_000), Some(264_000)),
            (Some(200_000), Some(200_000))
        );
        // no curated default -> single window.
        assert_eq!(knob_b_tier_windows(Some(400_000), None), (Some(400_000), Some(400_000)));
        assert_eq!(knob_b_tier_windows(None, Some(400_000)), (None, None));
    }

    #[test]
    fn synthesized_gpt_is_two_tier() {
        let info = synth("gpt-5.5", "OpenAI", 1_050_000);
        assert_eq!(info.context_window, Some(400_000));
        assert_eq!(info.max_context_window, Some(1_050_000));
        assert!(info.supports_context_window_tier_selection());
    }

    #[test]
    fn synthesized_gpt_single_tier_when_full_not_above_default() {
        // gpt-5.3-codex / gpt-5.4-mini live at 400k == default -> single-tier.
        let codex = synth("gpt-5.3-codex", "OpenAI", 400_000);
        assert_eq!(codex.context_window, Some(400_000));
        assert_eq!(codex.max_context_window, Some(400_000));
        assert!(!codex.supports_context_window_tier_selection());
        // gpt-5-mini live at 264k < 400k default -> clamp to 264k, single-tier.
        let mini = synth("gpt-5-mini", "OpenAI", 264_000);
        assert_eq!(mini.context_window, Some(264_000));
        assert_eq!(mini.max_context_window, Some(264_000));
        assert!(!mini.supports_context_window_tier_selection());
    }

    #[test]
    fn synthesized_anthropic_two_tier_and_single_tier() {
        // 1M Claude -> two-tier (264k default / 1M full).
        let opus = synth("claude-opus-4.8", "Anthropic", 1_000_000);
        assert_eq!(opus.context_window, Some(264_000));
        assert_eq!(opus.max_context_window, Some(1_000_000));
        assert!(opus.supports_context_window_tier_selection());
        // 200k Claude -> full below the 264k default -> single-tier at 200k; a
        // stale long_context cannot widen it.
        let sonnet45 = synth("claude-sonnet-4.5", "Anthropic", 200_000);
        assert_eq!(sonnet45.context_window, Some(200_000));
        assert_eq!(sonnet45.max_context_window, Some(200_000));
        assert!(!sonnet45.supports_context_window_tier_selection());
    }

    #[test]
    fn synthesized_absent_limits_yields_no_window() {
        let info = synthesize_from_capabilities(
            entry(json!({
                "id": "gpt-x",
                "vendor": "OpenAI",
                "model_picker_enabled": true,
                "supported_endpoints": ["/responses"],
                "capabilities": { "type": "chat" }
            })),
            /*anthropic_enabled*/ false,
        );
        assert_eq!(info.context_window, None);
        assert_eq!(info.max_context_window, None);
        assert!(!info.supports_context_window_tier_selection());
    }

    #[test]
    fn bundled_overlay_recovers_full_tier_from_stale_bundle() {
        // Simulate a stale bundled gpt-5.5 (both windows at 272k) and a live
        // /models row reporting the real 1.05M full window. The overlay must
        // recover the long-context tier (default 400k / full 1.05M).
        let mut stale = synth("gpt-5.5", "OpenAI", 272_000);
        stale.context_window = Some(272_000);
        stale.max_context_window = Some(272_000);
        let bundled = vec![stale];

        let info = translate_entry(
            entry(json!({
                "id": "gpt-5.5",
                "vendor": "OpenAI",
                "model_picker_enabled": true,
                "supported_endpoints": ["/responses"],
                "capabilities": { "type": "chat", "limits": { "max_context_window_tokens": 1_050_000 } }
            })),
            &bundled,
            /*anthropic_enabled*/ false,
        );
        assert_eq!(info.context_window, Some(400_000));
        assert_eq!(info.max_context_window, Some(1_050_000));
        assert!(info.supports_context_window_tier_selection());
    }
}
