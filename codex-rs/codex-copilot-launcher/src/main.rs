mod config;
mod discovery;
mod setup;

use std::process::Command;

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

    // Build final args: user args first, then provider -c flags last.
    // Provider flags MUST come last so they always win — later -c values
    // override earlier ones for the same key, ensuring the built-in Copilot
    // provider selection and sandbox defaults win over user-supplied flags.
    let provider_flags =
        config::provider_config_flags(&cfg.default_model, cfg.default_shell.as_deref());
    let mut final_args: Vec<String> = args;
    for flag in &provider_flags {
        final_args.push("-c".to_string());
        final_args.push(flag.clone());
    }

    // Set env — unsafe in Rust 2024 edition (single-threaded at this point, safe in practice)
    unsafe {
        std::env::set_var("OPENAI_API_KEY", "sk-sandbox-copilot-api-handles-auth");
        std::env::remove_var("HTTP_PROXY");
        std::env::remove_var("HTTPS_PROXY");
        std::env::remove_var("http_proxy");
        std::env::remove_var("https_proxy");
    }

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
    use super::is_passthrough;

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
}
