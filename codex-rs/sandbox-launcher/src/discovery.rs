use std::path::PathBuf;

/// Where copilot-api was found.
pub enum CopilotApiLocation {
    /// TypeScript source to be run with bun.
    Source(PathBuf),
    /// Pre-compiled binary.
    Binary(PathBuf),
}

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
        "codex-core not found. Expected {bin_name} next to the sandbox-launcher binary, \
         or set CODEX_CORE_PATH env var."
    )
}

/// Find the bun executable.
///
/// Search order:
/// 1. `BUN_PATH` env var
/// 2. `~/.bun/bin/bun` (Unix)
/// 3. Search PATH
pub fn find_bun() -> anyhow::Result<PathBuf> {
    // 1. Explicit env override
    if let Ok(p) = std::env::var("BUN_PATH") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(path);
        }
        anyhow::bail!("BUN_PATH is set to {p} but the file does not exist");
    }

    // 2. ~/.bun/bin/bun (Unix convention)
    #[cfg(unix)]
    {
        if let Some(home) = dirs::home_dir() {
            let candidate = home.join(".bun").join("bin").join("bun");
            if candidate.exists() {
                return Ok(candidate);
            }
        }
    }

    // 3. Search PATH
    let names: &[&str] = if cfg!(windows) {
        &["bun.exe", "bun.cmd"]
    } else {
        &["bun"]
    };

    if let Some(path) = search_path(names) {
        return Ok(path);
    }

    anyhow::bail!(
        "bun not found. Install with: curl -fsSL https://bun.sh/install | bash\n  \
         Or set BUN_PATH env var."
    )
}

/// Find copilot-api (source or binary).
///
/// Source search order:
/// 1. `COPILOT_API_SOURCE` env var
/// 2. Relative to exe: `../../external/repos/copilot-api/src/main.ts` (dev layout)
/// 3. Relative to exe: `../copilot-api/src/main.ts` (installed alongside)
///
/// Binary fallback:
/// 4. `COPILOT_API_BIN` env var
/// 5. Same directory as exe: `copilot-api.exe` / `copilot-api`
pub fn find_copilot_api() -> anyhow::Result<CopilotApiLocation> {
    let bin_name = if cfg!(windows) {
        "copilot-api.exe"
    } else {
        "copilot-api"
    };

    // 1. COPILOT_API_SOURCE env var
    if let Ok(p) = std::env::var("COPILOT_API_SOURCE") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(CopilotApiLocation::Source(path));
        }
        anyhow::bail!("COPILOT_API_SOURCE is set to {p} but the file does not exist");
    }

    // 2-3. Relative to exe (source)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            // Dev layout: sandbox-launcher is in codex-rs/target/release/
            // copilot-api source is at codex-rs/../../external/repos/copilot-api/src/main.ts
            let dev_source = dir
                .join("..")
                .join("..")
                .join("external")
                .join("repos")
                .join("copilot-api")
                .join("src")
                .join("main.ts");
            if dev_source.exists() {
                return Ok(CopilotApiLocation::Source(dev_source));
            }

            // Installed alongside: ../copilot-api/src/main.ts
            let installed_source = dir
                .join("..")
                .join("copilot-api")
                .join("src")
                .join("main.ts");
            if installed_source.exists() {
                return Ok(CopilotApiLocation::Source(installed_source));
            }
        }
    }

    // 4. COPILOT_API_BIN env var
    if let Ok(p) = std::env::var("COPILOT_API_BIN") {
        let path = PathBuf::from(&p);
        if path.exists() {
            return Ok(CopilotApiLocation::Binary(path));
        }
        anyhow::bail!("COPILOT_API_BIN is set to {p} but the file does not exist");
    }

    // 5. Same directory as exe (binary)
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let candidate = dir.join(bin_name);
            if candidate.exists() {
                return Ok(CopilotApiLocation::Binary(candidate));
            }
        }
    }

    anyhow::bail!(
        "copilot-api not found. Expected source at external/repos/copilot-api/src/main.ts \
         or binary {bin_name} next to the launcher.\n  \
         Set COPILOT_API_SOURCE or COPILOT_API_BIN env var to override."
    )
}

/// Check if a native @openai/codex is installed in PATH and would conflict.
pub fn check_native_codex_conflict() -> anyhow::Result<()> {
    let names: &[&str] = if cfg!(windows) {
        &["codex.cmd", "codex"]
    } else {
        &["codex"]
    };

    if let Some(path) = search_path(names) {
        let path_str = path.to_string_lossy();
        // Check if this is the npm-installed @openai/codex
        if path_str.contains("node_modules/@openai")
            || path_str.contains("node_modules\\@openai")
        {
            anyhow::bail!(
                "Native @openai/codex detected in PATH at: {path_str}\n  \
                 This will conflict with the sandbox launcher.\n  \
                 Uninstall it: npm uninstall -g @openai/codex"
            );
        }
    }
    Ok(())
}

/// Search PATH for any of the given binary names. Returns the first match.
fn search_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    let dirs = std::env::split_paths(&path_var);
    for dir in dirs {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}
