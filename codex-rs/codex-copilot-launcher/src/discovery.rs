use std::path::PathBuf;

/// Find the codex-core binary.
///
/// Search order:
/// 1. `CODEX_CORE_PATH` env var
/// 2. Same directory as the current executable
pub fn find_codex_core() -> anyhow::Result<PathBuf> {
    let bin_name = if cfg!(windows) {
        "codex-core.exe"
    } else {
        "codex-core"
    };

    // 1. Explicit env override
    if let Ok(p) = std::env::var("CODEX_CORE_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!("CODEX_CORE_PATH is set to {p} but the file does not exist");
    }

    // 2. Same directory as current exe
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(bin_name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    anyhow::bail!(
        "codex-core not found. Expected {bin_name} next to the codex-copilot-launcher binary, \
         or set CODEX_CORE_PATH env var."
    )
}

/// Find the codex-copilot-gateway binary.
///
/// Search order:
/// 1. `CODEX_COPILOT_GATEWAY_PATH` env var
/// 2. Same directory as the current executable
pub fn find_codex_copilot_gateway() -> anyhow::Result<PathBuf> {
    let bin_name = if cfg!(windows) {
        "codex-copilot-gateway.exe"
    } else {
        "codex-copilot-gateway"
    };

    if let Ok(p) = std::env::var("CODEX_COPILOT_GATEWAY_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!(
            "CODEX_COPILOT_GATEWAY_PATH is set to {p} but the file does not exist"
        );
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(bin_name);
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    anyhow::bail!(
        "codex-copilot-gateway not found. Expected {bin_name} next to the codex-copilot-launcher binary, \
         or set CODEX_COPILOT_GATEWAY_PATH env var."
    )
}
