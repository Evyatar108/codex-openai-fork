use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_MODEL: &str = "gpt-5.4";

pub struct SandboxConfig {
    pub copilot_api_port: u16,
    pub default_model: String,
}

/// Load configuration from `~/.codex-sandbox/config.toml`.
/// Returns defaults if the file is missing or unparseable.
pub fn load_config() -> SandboxConfig {
    let defaults = SandboxConfig {
        copilot_api_port: DEFAULT_PORT,
        default_model: DEFAULT_MODEL.to_string(),
    };

    let config_path = match config_path() {
        Some(p) => p,
        None => return defaults,
    };

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return defaults,
    };

    let table: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(_) => return defaults,
    };

    SandboxConfig {
        copilot_api_port: table
            .get("copilot_api_port")
            .and_then(|v| v.as_integer())
            .and_then(|v| u16::try_from(v).ok())
            .unwrap_or(DEFAULT_PORT),
        default_model: table
            .get("default_model")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| DEFAULT_MODEL.to_string()),
    }
}

/// Build the list of `-c` flag values for configuring codex-core to use
/// copilot-api as its provider.
pub fn provider_config_flags(port: u16, model: &str) -> Vec<String> {
    vec![
        format!("model={model}"),
        "model_provider=copilot-sandbox".to_string(),
        "model_providers.copilot-sandbox.name=Copilot Sandbox".to_string(),
        format!("model_providers.copilot-sandbox.base_url=http://127.0.0.1:{port}/v1"),
        "model_providers.copilot-sandbox.wire_api=responses".to_string(),
        "model_providers.copilot-sandbox.supports_websockets=true".to_string(),
        // Disable OpenAI Curated plugins -- not available through copilot-api
        "plugins.github@openai-curated.enabled=false".to_string(),
        "plugins.notion@openai-curated.enabled=false".to_string(),
        "plugins.slack@openai-curated.enabled=false".to_string(),
        "plugins.gmail@openai-curated.enabled=false".to_string(),
        "plugins.google-calendar@openai-curated.enabled=false".to_string(),
        "plugins.google-drive@openai-curated.enabled=false".to_string(),
        "plugins.linear@openai-curated.enabled=false".to_string(),
        "plugins.figma@openai-curated.enabled=false".to_string(),
    ]
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex-sandbox").join("config.toml"))
}
