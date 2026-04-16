mod config;
mod copilot_api;
mod discovery;
mod setup;

use std::process::Command;

fn main() {
    if let Err(e) = run() {
        eprintln!("ERROR: {e:#}");
        std::process::exit(1);
    }
}

fn run() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();

    // Passthrough commands: skip copilot-api startup, exec codex-core immediately
    let is_passthrough = args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--version" | "-V" | "--help" | "-h" | "completion"
        )
    });

    let codex_core = discovery::find_codex_core()?;

    if is_passthrough {
        return exec_codex_core(&codex_core, &args, None);
    }

    // Normal launch: first-run bootstrap, config, copilot-api, then codex-core
    setup::first_run_bootstrap()?;

    let cfg = config::load_config();
    copilot_api::ensure_running(cfg.copilot_api_port)?;

    // Build final args: user args first, then provider -c flags last.
    // Provider flags MUST come last so they always win — later -c values
    // override earlier ones for the same key, ensuring the sandbox endpoint
    // can never be overridden by user-supplied flags (which would leak data
    // to the official OpenAI API).
    let provider_flags =
        config::provider_config_flags(cfg.copilot_api_port, &cfg.default_model, cfg.default_shell.as_deref());
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

    exec_codex_core(&codex_core, &final_args, Some(cfg.copilot_api_port))
}

fn exec_codex_core(
    codex_core: &std::path::Path,
    args: &[String],
    copilot_api_port: Option<u16>,
) -> anyhow::Result<()> {
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

        let code = status.code().unwrap_or(1);
        // On non-zero exit, check if copilot-api is still healthy
        if code != 0 {
            if let Some(port) = copilot_api_port {
                copilot_api::check_health_or_print_log(port);
            }
        }
        std::process::exit(code);
    }
}
