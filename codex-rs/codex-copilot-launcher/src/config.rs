use std::path::PathBuf;

pub struct SandboxConfig {
    pub default_shell: Option<String>,
}

/// Load configuration from `~/.codex-copilot/config.toml`.
/// Returns defaults if the file is missing or unparseable.
///
/// Recognized keys:
/// - `default_shell` (string): absolute path to the shell binary to pin
///   for tool-exec turns. Optional.
///
/// NOT read here:
/// - `model`: codex-core reads `~/.codex/config.toml::model` natively.
///   The launcher used to force `-c model=<launcher-default>` on every
///   run, which silently overrode the user's choice.
/// - `copilot_api_port`: leftover from the v5 loopback model-service era.
///   The launcher no longer binds a port; existing `copilot_api_port = ...`
///   keys in old config files are ignored.
pub fn load_config() -> SandboxConfig {
    let defaults = SandboxConfig {
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
        default_shell: table
            .get("default_shell")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    }
}

/// Build the list of `-c` flag values for configuring codex-core to use
/// the built-in Copilot provider.
///
/// `model` is intentionally NOT included here — codex-core reads
/// `~/.codex/config.toml::model` natively, and provider `-c` flags would
/// override it.
pub fn provider_config_flags(default_shell: Option<&str>) -> Vec<String> {
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
        let flags = provider_config_flags(None);
        assert!(
            flags
                .iter()
                .any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
    }

    #[test]
    fn provider_flags_emit_sandbox_mode_with_shell() {
        let flags = provider_config_flags(Some(r"C:\Program Files\Git\bin\bash.exe"));
        assert!(
            flags
                .iter()
                .any(|f| f == "sandbox_mode=\"danger-full-access\""),
            "expected sandbox_mode flag in {flags:?}"
        );
        assert!(
            flags.iter().any(|f| f == "features.unified_exec=false"),
            "expected unified_exec=false for bash in {flags:?}"
        );
    }

    #[test]
    fn provider_flags_do_not_emit_gateway_era_overrides() {
        let flags = provider_config_flags(None);

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
        let flags = provider_config_flags(None);
        assert!(
            flags.iter().all(|flag| !flag.starts_with("model=")),
            "launcher must not force model selection: {flags:?}"
        );
    }

    #[test]
    fn is_cygwin_shell_detects_bash() {
        assert!(is_cygwin_shell(r"C:\Program Files\Git\bin\bash.exe"));
        assert!(is_cygwin_shell("/usr/bin/bash"));
        assert!(is_cygwin_shell("/usr/bin/zsh"));
        assert!(!is_cygwin_shell(
            r"C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe"
        ));
    }
}
