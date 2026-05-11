use std::path::PathBuf;

pub struct SandboxConfig {
    pub default_shell: Option<String>,
    pub auto_load_claude_md: Option<bool>,
}

/// Load configuration from `~/.codex-copilot/config.toml`.
/// Returns defaults if the file is missing or unparseable.
///
/// Recognized keys:
/// - `default_shell` (string): absolute path to the shell binary to pin
///   for tool-exec turns. Optional.
/// - `auto_load_claude_md` (bool): controls whether the launcher asks
///   codex-core to load `CLAUDE.md` where `AGENTS.md` is absent. Optional;
///   unset resolves to enabled by the launcher.
///
/// NOT read here:
/// - `model`: codex-core reads `~/.codex/config.toml::model` natively.
///   The launcher used to force `-c model=<launcher-default>` on every
///   run, which silently overrode the user's choice.
/// - `copilot_api_port`: leftover from the v5 loopback model-service era.
///   The launcher no longer binds a port; existing `copilot_api_port = ...`
///   keys in old config files are ignored.
pub fn load_config() -> SandboxConfig {
    let config_path = match config_path() {
        Some(p) => p,
        None => return parse_sandbox_config(""),
    };

    let content = match std::fs::read_to_string(&config_path) {
        Ok(c) => c,
        Err(_) => return parse_sandbox_config(""),
    };

    parse_sandbox_config(&content)
}

pub(crate) fn parse_sandbox_config(content: &str) -> SandboxConfig {
    let defaults = SandboxConfig {
        default_shell: None,
        auto_load_claude_md: None,
    };

    let table: toml::Table = match content.parse() {
        Ok(t) => t,
        Err(_) => return defaults,
    };

    SandboxConfig {
        default_shell: table
            .get("default_shell")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
        auto_load_claude_md: table.get("auto_load_claude_md").and_then(|v| v.as_bool()),
    }
}

/// Build the list of `-c` flag values for configuring codex-core to use
/// the built-in Copilot provider.
///
/// `model` is intentionally NOT included here — codex-core reads
/// `~/.codex/config.toml::model` natively, and provider `-c` flags would
/// override it.
pub fn provider_config_flags(cfg: &SandboxConfig) -> Vec<String> {
    let mut flags = vec![
        "model_provider=copilot".to_string(),
        // Source-level network patching handles isolation; disable codex-core's
        // built-in sandbox so it doesn't retry on sandbox-related errors.
        "sandbox_mode=\"danger-full-access\"".to_string(),
        // SANDBOX PATCH: remote_control is ChatGPT-only (protocol allows only
        // chatgpt.com / chatgpt-staging.com / localhost as the enroll target,
        // and the transport attaches ChatGPT-specific auth). Belt-and-suspenders
        // to the source-level force-disable in `app-server/src/transport/remote_control/mod.rs`.
        "features.remote_control=false".to_string(),
        // Disable OpenAI Curated plugins in the sandboxed Copilot bundle.
        "plugins.github@openai-curated.enabled=false".to_string(),
        "plugins.notion@openai-curated.enabled=false".to_string(),
        "plugins.slack@openai-curated.enabled=false".to_string(),
        "plugins.gmail@openai-curated.enabled=false".to_string(),
        "plugins.google-calendar@openai-curated.enabled=false".to_string(),
        "plugins.google-drive@openai-curated.enabled=false".to_string(),
        "plugins.linear@openai-curated.enabled=false".to_string(),
        "plugins.figma@openai-curated.enabled=false".to_string(),
    ];
    if let Some(shell) = cfg.default_shell.as_deref() {
        flags.push(format!("default_shell={shell}"));
    }
    if cfg.auto_load_claude_md.unwrap_or(true) {
        flags.push("project_doc_fallback_filenames=[\"CLAUDE.md\"]".to_string());
    }
    flags
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex-copilot").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_flags_always_emit_sandbox_mode() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(false),
        };
        let flags = provider_config_flags(&cfg);
        assert!(
            flags
                .iter()
                .any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
    }

    #[test]
    fn provider_flags_emit_sandbox_mode_with_shell() {
        let cfg = SandboxConfig {
            default_shell: Some(r"C:\Program Files\Git\bin\bash.exe".to_string()),
            auto_load_claude_md: Some(false),
        };
        let flags = provider_config_flags(&cfg);
        assert!(
            flags
                .iter()
                .any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
        assert!(
            flags.iter().all(|f| f != "features.unified_exec=false"),
            "launcher must not force unified_exec=false for bash — pipe mode (tty=false default) is safe on Cygwin: {flags:?}"
        );
    }

    #[test]
    fn provider_flags_do_not_emit_gateway_era_overrides() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(false),
        };
        let flags = provider_config_flags(&cfg);

        for forbidden in [
            "model_providers.copilot.base_url",
            "model_providers.copilot.supports_websockets",
            "model_providers.copilot.wire_api",
            "model_providers.copilot.name",
        ] {
            assert!(
                flags.iter().all(|flag| !flag.contains(forbidden)),
                "unexpected gateway-era override {forbidden} in {flags:?}"
            );
        }
    }

    #[test]
    fn provider_flags_do_not_force_model() {
        // SANDBOX PATCH: model selection must be read from ~/.codex/config.toml
        // by codex-core, not forced by the launcher. A `-c model=...` flag here
        // would silently override the user's config.
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(false),
        };
        let flags = provider_config_flags(&cfg);
        assert!(
            flags.iter().all(|flag| !flag.starts_with("model=")),
            "launcher must not force model selection: {flags:?}"
        );
    }

    #[test]
    fn provider_flags_emit_fallback_when_auto_load_enabled() {
        let cfg = SandboxConfig {
            default_shell: Some(r"C:\Program Files\Git\bin\bash.exe".to_string()),
            auto_load_claude_md: Some(true),
        };

        let flags = provider_config_flags(&cfg);

        assert_eq!(
            flags.last().map(String::as_str),
            Some("project_doc_fallback_filenames=[\"CLAUDE.md\"]"),
            "CLAUDE.md fallback must be the final provider flag: {flags:?}"
        );
    }

    #[test]
    fn provider_flags_omit_fallback_when_auto_load_disabled() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(false),
        };

        let flags = provider_config_flags(&cfg);

        assert!(
            flags
                .iter()
                .all(|flag| !flag.starts_with("project_doc_fallback_filenames=")),
            "CLAUDE.md fallback should be omitted when auto_load_claude_md=false: {flags:?}"
        );
    }

    #[test]
    fn provider_flags_emit_fallback_when_auto_load_unset_default() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: None,
        };

        let flags = provider_config_flags(&cfg);

        assert_eq!(
            flags.last().map(String::as_str),
            Some("project_doc_fallback_filenames=[\"CLAUDE.md\"]"),
            "auto_load_claude_md absent should default to CLAUDE.md fallback enabled: {flags:?}"
        );
    }

    #[test]
    fn auto_load_claude_md_default_when_unset() {
        let empty = parse_sandbox_config("");
        assert_eq!(empty.auto_load_claude_md, None);
        assert!(empty.auto_load_claude_md.unwrap_or(true));

        let shell_only =
            parse_sandbox_config(r#"default_shell = "C:\\Program Files\\Git\\bin\\bash.exe""#);
        assert_eq!(shell_only.auto_load_claude_md, None);
        assert!(shell_only.auto_load_claude_md.unwrap_or(true));
    }

    #[test]
    fn auto_load_claude_md_parses_true() {
        let cfg = parse_sandbox_config("auto_load_claude_md = true");
        assert_eq!(cfg.auto_load_claude_md, Some(true));
    }

    #[test]
    fn auto_load_claude_md_parses_false() {
        let cfg = parse_sandbox_config("auto_load_claude_md = false");
        assert_eq!(cfg.auto_load_claude_md, Some(false));
    }
}
