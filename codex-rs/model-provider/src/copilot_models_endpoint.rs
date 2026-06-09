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

const MODELS_REFRESH_TIMEOUT: Duration = Duration::from_secs(5);
const MODELS_PATH: &str = "/models";
const COPILOT_RESPONSES_ENDPOINT: &str = "/responses";
// SANDBOX PATCH: D-001 Claude-via-Copilot chat-completions transport.
const COPILOT_CHAT_ENDPOINT: &str = "/chat/completions";
// SANDBOX PATCH: D-001 hidden-until-transport guardrail. While `false`, a
// chat-only Claude row stays hidden from the picker even though the /models
// filter recognizes it, preventing a partial-rollout false affordance (a
// Claude row selectable before `core/src/client.rs` can route it). Flipped to
// `true` once the chat-completions transport is wired in core (US-005); the
// `core::chat_transport` static assert keeps the two in lockstep.
// See `docs/implementation/patch-surface.md` §14.
pub(crate) const CHAT_TRANSPORT_AVAILABLE: bool = true;

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

        let models = body
            .data
            .into_iter()
            .filter(is_chat_responses_picker_entry)
            .map(|entry| translate_entry(entry, &bundled))
            .collect::<Vec<_>>();

        if models.is_empty() {
            warn!("copilot /models returned no /responses-capable, picker-enabled chat models");
        }
        Ok((models, etag))
    }
}

// SANDBOX PATCH: D-001. Relaxed to ALSO surface chat-only Claude rows (advertising
// `/chat/completions` but not `/responses`) once the chat transport is available,
// tagging them to the chat wire via `wire_route`. A GPT `/responses` row is
// unaffected. The `chat_transport_available` arg drives the hidden-until-transport
// guardrail (see `CHAT_TRANSPORT_AVAILABLE`); the production caller passes the const.
fn is_chat_responses_picker_entry(entry: &CopilotModelEntry) -> bool {
    is_picker_entry_with_transport(entry, CHAT_TRANSPORT_AVAILABLE)
}

fn is_picker_entry_with_transport(
    entry: &CopilotModelEntry,
    chat_transport_available: bool,
) -> bool {
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
    has_responses || (chat_transport_available && has_chat)
}

// SANDBOX PATCH: D-001. A row advertising `/chat/completions` but NOT `/responses`
// routes over the chat transport; everything else keeps the provider default.
fn wire_route_for(entry: &CopilotModelEntry) -> ModelWireRoute {
    let has_responses = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_RESPONSES_ENDPOINT);
    let has_chat = entry
        .supported_endpoints
        .iter()
        .any(|e| e == COPILOT_CHAT_ENDPOINT);
    if has_chat && !has_responses {
        ModelWireRoute::ChatCompletions
    } else {
        ModelWireRoute::ProviderDefault
    }
}

fn translate_entry(entry: CopilotModelEntry, bundled: &[ModelInfo]) -> ModelInfo {
    // SANDBOX PATCH: D-001 compute the wire-route before `entry` is consumed below.
    let wire_route = wire_route_for(&entry);
    if let Some(known) = bundled.iter().find(|info| info.slug == entry.id) {
        let mut info = known.clone();
        if let Some(name) = entry.name {
            info.display_name = name;
        }
        info.visibility = ModelVisibility::List;
        info.used_fallback_model_metadata = false;
        // SANDBOX PATCH: D-001 tag the bundled-clone row to its wire route.
        info.wire_route = wire_route;
        return info;
    }
    synthesize_from_capabilities(entry)
}

fn synthesize_from_capabilities(entry: CopilotModelEntry) -> ModelInfo {
    // SANDBOX PATCH: D-001 derive the wire-route before `entry` fields are moved below.
    let wire_route = wire_route_for(&entry);
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

    let context_window = limits.and_then(|l| l.max_context_window_tokens);
    let display_name = entry.name.unwrap_or_else(|| entry.id.clone());

    ModelInfo {
        slug: entry.id,
        display_name,
        description: None,
        default_reasoning_level: supported_reasoning_levels.first().map(|p| p.effort),
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
        max_context_window: context_window,
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
    fn chat_only_row_hidden_until_transport_available() {
        let chat_only = entry(json!({
            "id": "claude-sonnet-4.6",
            "model_picker_enabled": true,
            "supported_endpoints": ["/chat/completions", "/v1/messages"],
            "capabilities": { "type": "chat" }
        }));
        // Hidden when the transport is not yet available; surfaced once it is.
        assert!(!is_picker_entry_with_transport(
            &chat_only, /*chat_transport_available*/ false
        ));
        assert!(is_picker_entry_with_transport(
            &chat_only, /*chat_transport_available*/ true
        ));
        assert_eq!(wire_route_for(&chat_only), ModelWireRoute::ChatCompletions);

        // A /responses GPT row is surfaced regardless and keeps the provider default.
        let responses_row = entry(json!({
            "id": "gpt-5.5",
            "model_picker_enabled": true,
            "supported_endpoints": ["/responses"],
            "capabilities": { "type": "chat" }
        }));
        assert!(is_picker_entry_with_transport(
            &responses_row,
            /*chat_transport_available*/ false
        ));
        assert_eq!(
            wire_route_for(&responses_row),
            ModelWireRoute::ProviderDefault
        );
    }

    #[test]
    fn wire_route_survives_models_cache_round_trip() {
        // A synthesized chat-only Claude row carries the ChatCompletions hint.
        let info = synthesize_from_capabilities(entry(json!({
            "id": "claude-sonnet-4.6",
            "model_picker_enabled": true,
            "supported_endpoints": ["/chat/completions", "/v1/messages"],
            "capabilities": { "type": "chat" }
        })));
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
}
