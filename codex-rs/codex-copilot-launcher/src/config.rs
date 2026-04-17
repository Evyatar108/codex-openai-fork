use std::path::PathBuf;

pub const DEFAULT_PORT: u16 = 4141;
pub const DEFAULT_MODEL: &str = "gpt-5.4";

pub struct SandboxConfig {
    pub copilot_api_port: u16,
    pub default_model: String,
    pub default_shell: Option<String>,
}

/// Load configuration from `~/.codex-copilot/config.toml`.
/// Returns defaults if the file is missing or unparseable.
pub fn load_config() -> SandboxConfig {
    let defaults = SandboxConfig {
        copilot_api_port: DEFAULT_PORT,
        default_model: DEFAULT_MODEL.to_string(),
        default_shell: None,
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
        default_shell: table
            .get("default_shell")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

/// Build the list of `-c` flag values for configuring codex-core to use
/// codex-copilot-gateway as its provider.
pub fn provider_config_flags(port: u16, model: &str, default_shell: Option<&str>) -> Vec<String> {
    let mut flags = vec![
        format!("model={model}"),
        "model_provider=copilot".to_string(),
        "model_providers.copilot.name=Copilot".to_string(),
        format!("model_providers.copilot.base_url=http://127.0.0.1:{port}/v1"),
        "model_providers.copilot.wire_api=responses".to_string(),
        "model_providers.copilot.supports_websockets=true".to_string(),
        // Source-level network patching handles isolation; disable codex-core's
        // built-in sandbox so it doesn't retry on sandbox-related errors.
        "sandbox_mode=\"danger-full-access\"".to_string(),
        // Disable OpenAI Curated plugins -- not available through codex-copilot-gateway
        "plugins.github@openai-curated.enabled=false".to_string(),
        "plugins.notion@openai-curated.enabled=false".to_string(),
        "plugins.slack@openai-curated.enabled=false".to_string(),
        "plugins.gmail@openai-curated.enabled=false".to_string(),
        "plugins.google-calendar@openai-curated.enabled=false".to_string(),
        "plugins.google-drive@openai-curated.enabled=false".to_string(),
        "plugins.linear@openai-curated.enabled=false".to_string(),
        "plugins.figma@openai-curated.enabled=false".to_string(),
    ];
    if let Some(shell) = default_shell {
        flags.push(format!("default_shell={shell}"));
        // Cygwin/MSYS2-based shells (Git Bash, MSYS2 bash) crash under ConPTY
        // with "CreateFileMapping ... Win32 error 5" during shared memory init.
        // Disable the ConPTY-based UnifiedExec path and fall back to the classic
        // ShellCommand path which uses tokio::process::Command (pipe-based I/O).
        if is_cygwin_shell(shell) {
            flags.push("features.unified_exec=false".to_string());
        }
    }
    flags
}

/// Returns true if the shell path points to a Cygwin/MSYS2-based executable.
/// On Windows, bash/sh/zsh are always Cygwin-based (Git for Windows, MSYS2).
fn is_cygwin_shell(shell_path: &str) -> bool {
    let lower = shell_path.to_ascii_lowercase();
    let name = std::path::Path::new(&lower)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    matches!(name, "bash" | "sh" | "zsh")
}

fn config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex-copilot").join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_flags_always_emit_sandbox_mode() {
        let flags = provider_config_flags(4141, "gpt-5.4", None);
        assert!(
            flags.iter().any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
    }

    #[test]
    fn provider_flags_emit_sandbox_mode_with_shell() {
        let flags = provider_config_flags(
            4141,
            "gpt-5.4",
            Some(r"C:\Program Files\Git\bin\bash.exe"),
        );
        assert!(
            flags.iter().any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
        assert!(
            flags.iter().any(|f| f == "features.unified_exec=false"),
            "expected unified_exec=false for bash in {flags:?}"
        );
    }

    #[test]
    fn is_cygwin_shell_detects_bash() {
        assert!(is_cygwin_shell(r"C:\Program Files\Git\bin\bash.exe"));
        assert!(is_cygwin_shell("/usr/bin/bash"));
        assert!(is_cygwin_shell("/usr/bin/zsh"));
        assert!(!is_cygwin_shell(r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"));
    }
}
