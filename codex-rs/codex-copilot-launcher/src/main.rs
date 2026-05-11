mod config;
mod discovery;
mod setup;

use std::process::Command;

const PROJECT_DOC_FALLBACK_KEY: &str = "project_doc_fallback_filenames";
const AUTO_LOAD_WARN: &str = "WARN: auto_load_claude_md is overriding ~/.codex/config.toml project_doc_fallback_filenames with [\"CLAUDE.md\"]; set auto_load_claude_md = false in ~/.codex-copilot/config.toml to keep your configured project_doc_fallback_filenames.";

fn main() {
    if let Err(e) = run() {
        eprintln!("ERROR: {e:#}");
        std::process::exit(1);
    }
}

fn is_passthrough(args: &[String]) -> bool {
    matches!(
        args.first().map(String::as_str),
        Some("login" | "completion" | "debug" | "features" | "mcp" | "marketplace")
    ) || args
        .iter()
        .any(|arg| matches!(arg.as_str(), "--version" | "-V" | "--help" | "-h"))
}

fn auto_load_claude_md_resolved(cfg: &config::SandboxConfig) -> bool {
    cfg.auto_load_claude_md.unwrap_or(true)
}

fn codex_config_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex").join("config.toml"))
}

fn args_set_project_doc_fallback(args: &[String]) -> bool {
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if arg == "-c" || arg == "--config" {
            if iter
                .next()
                .is_some_and(|value| override_sets_project_doc_fallback(value))
            {
                return true;
            }
            continue;
        }

        if let Some(value) = arg.strip_prefix("--config=") {
            if override_sets_project_doc_fallback(value) {
                return true;
            }
        }
    }

    false
}

fn override_sets_project_doc_fallback(value: &str) -> bool {
    value
        .split_once('=')
        .is_some_and(|(key, _)| key.trim() == PROJECT_DOC_FALLBACK_KEY)
}

fn config_sets_project_doc_fallback(path: &std::path::Path) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(table) = content.parse::<toml::Table>() else {
        return false;
    };

    table.contains_key(PROJECT_DOC_FALLBACK_KEY)
}

fn warn_helper_should_emit(
    args: &[String],
    cfg: &config::SandboxConfig,
    codex_config: Option<&std::path::Path>,
) -> bool {
    auto_load_claude_md_resolved(cfg)
        && (args_set_project_doc_fallback(args)
            || codex_config.is_some_and(config_sets_project_doc_fallback))
}

fn warn_if_auto_load_overrides_project_doc_fallbacks(args: &[String], cfg: &config::SandboxConfig) {
    if warn_helper_should_emit(args, cfg, codex_config_path().as_deref()) {
        eprintln!("{AUTO_LOAD_WARN}");
    }
}

pub(crate) fn configure_launcher_env() {
    // SAFETY: single-threaded launcher, all mutations happen before codex-core is exec'd.
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-sandbox-copilot-api-handles-auth");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("http_proxy");
        std::env::remove_var("https_proxy");
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    let codex_core = discovery::find_codex_core()?;

    // Passthrough commands skip bootstrap and exec codex-core immediately.
    if is_passthrough(&args) {
        return exec_codex_core(&codex_core, &args);
    }

    // Normal launch: first-run bootstrap, config, then codex-core.
    setup::first_run_bootstrap()?;

    let cfg = config::load_config();
    warn_if_auto_load_overrides_project_doc_fallbacks(&args, &cfg);

    // Build final args: user args first, then provider -c flags last.
    // Provider flags MUST come last so they always win — later -c values
    // override earlier ones for the same key, ensuring the built-in Copilot
    // provider selection and sandbox defaults win over user-supplied flags.
    //
    // SANDBOX PATCH: model is intentionally NOT among the forced flags.
    // codex-core resolves `model` from `~/.codex/config.toml` natively.
    let provider_flags = config::provider_config_flags(&cfg);
    let mut final_args: Vec<String> = args;
    for flag in &provider_flags {
        final_args.push("-c".to_string());
        final_args.push(flag.clone());
    }

    // Set env — unsafe in Rust 2024 edition (single-threaded at this point, safe in practice)
    configure_launcher_env();

    // Clean ~/.codex/.tmp/ (curated plugin cache)
    if let Some(home) = dirs::home_dir() {
        let tmp_dir = home.join(".codex").join(".tmp");
        if tmp_dir.exists() {
            let _ = std::fs::remove_dir_all(&tmp_dir);
        }
    }

    exec_codex_core(&codex_core, &final_args)
}

fn exec_codex_core(codex_core: &std::path::Path, args: &[String]) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let err = Command::new(codex_core).args(args).exec();
        // exec() only returns on error
        anyhow::bail!("failed to exec codex-core: {err}");
    }

    #[cfg(not(unix))]
    {
        let status = Command::new(codex_core)
            .args(args)
            .stdin(std::process::Stdio::inherit())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run codex-core: {e}"))?;

        std::process::exit(status.code().unwrap_or(1));
    }
}

#[cfg(test)]
mod tests {
    use super::config::SandboxConfig;
    use super::is_passthrough;
    use super::warn_helper_should_emit;

    #[test]
    fn login_is_passthrough_arg() {
        let args = vec![
            "login".to_string(),
            "--provider".to_string(),
            "copilot".to_string(),
        ];

        assert!(is_passthrough(&args));
    }

    #[test]
    fn exec_is_not_passthrough() {
        let args = vec!["exec".to_string(), "say hi".to_string()];

        assert!(!is_passthrough(&args));
    }

    #[test]
    fn debug_is_passthrough_arg() {
        let args = vec!["debug".to_string(), "clear-memories".to_string()];

        assert!(is_passthrough(&args));
    }

    #[test]
    fn features_is_passthrough_arg() {
        let args = vec!["features".to_string(), "list".to_string()];

        assert!(is_passthrough(&args));
    }

    #[test]
    fn mcp_is_passthrough_arg() {
        let args = vec!["mcp".to_string(), "list".to_string()];

        assert!(is_passthrough(&args));
    }

    #[test]
    fn marketplace_is_passthrough_arg() {
        let args = vec![
            "marketplace".to_string(),
            "add".to_string(),
            "/tmp/source".to_string(),
        ];

        assert!(is_passthrough(&args));
    }

    #[test]
    fn warn_helper_warns_for_cli_project_doc_fallback_when_auto_load_enabled() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(true),
        };
        let args = vec![
            "-c".to_string(),
            "project_doc_fallback_filenames=[\"FOO.md\"]".to_string(),
        ];

        assert!(warn_helper_should_emit(&args, &cfg, None));
    }

    #[test]
    fn warn_helper_warns_for_user_config_project_doc_fallback_when_auto_load_enabled() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(true),
        };
        let fixture = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixtures")
            .join("warn-trigger-config.toml");

        assert!(warn_helper_should_emit(&[], &cfg, Some(&fixture)));
    }

    #[test]
    fn warn_helper_silent_when_auto_load_disabled() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(false),
        };
        let args = vec![
            "--config".to_string(),
            "project_doc_fallback_filenames=[\"FOO.md\"]".to_string(),
        ];

        assert!(!warn_helper_should_emit(&args, &cfg, None));
    }

    #[test]
    fn warn_helper_silent_when_user_config_missing_or_unparseable() {
        let cfg = SandboxConfig {
            default_shell: None,
            auto_load_claude_md: Some(true),
        };
        let tmp = tempfile::tempdir().expect("tempdir");
        let missing = tmp.path().join("missing.toml");
        let invalid = tmp.path().join("invalid.toml");
        std::fs::write(&invalid, "project_doc_fallback_filenames = [").expect("write fixture");

        assert!(!warn_helper_should_emit(&[], &cfg, Some(&missing)));
        assert!(!warn_helper_should_emit(&[], &cfg, Some(&invalid)));
    }
}
