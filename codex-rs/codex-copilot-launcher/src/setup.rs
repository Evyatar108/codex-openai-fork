use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use toml::{Table, Value};

use crate::discovery;

/// First-run bootstrap: TTY-gated shell prompt, sandbox config write, and
/// Copilot login trigger. Idempotent — returns `Ok(())` immediately when
/// both the sandbox config and the Copilot token already exist.
pub fn first_run_bootstrap() -> Result<()> {
    let config_path =
        sandbox_config_path().context("unable to resolve home directory for sandbox config")?;
    let token_path =
        copilot_token_path().context("unable to resolve home directory for copilot token")?;

    migrate_legacy_sandbox_dir(&config_path);

    let config_exists = config_path.exists();
    let token_exists = token_path.exists();

    if config_exists && token_exists {
        return Ok(());
    }

    let is_tty = io::stdin().is_terminal();

    if !is_tty {
        if !token_exists {
            eprintln!(
                "First-time setup requires an interactive terminal.\n  Run manually: codex login --provider copilot\n  Then re-run codex."
            );
            std::process::exit(1);
        }
        if !config_exists {
            write_sandbox_config(&config_path, None)?;
        }
        return Ok(());
    }

    if !config_exists {
        let chosen = prompt_for_shell()?;
        write_sandbox_config(&config_path, chosen.as_deref())?;
    }

    if !token_exists {
        run_login()?;
    }

    Ok(())
}

fn sandbox_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex-copilot").join("config.toml"))
}

/// Legacy config path (`~/.codex-sandbox/config.toml`) used before the
/// `codex-copilot` rename. Detected at first-run so we can migrate.
fn legacy_sandbox_config_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| h.join(".codex-sandbox").join("config.toml"))
}

/// One-shot migration from `~/.codex-sandbox/` to `~/.codex-copilot/`.
///
/// If the new config path already exists, or the legacy path does not, this
/// is a no-op. Otherwise it renames the legacy directory to the new name and
/// emits a stderr notice so the user knows the migration ran.
fn migrate_legacy_sandbox_dir(new_config_path: &Path) {
    if new_config_path.exists() {
        return;
    }
    let Some(legacy) = legacy_sandbox_config_path() else {
        return;
    };
    if !legacy.exists() {
        return;
    }
    let Some(new_dir) = new_config_path.parent() else {
        return;
    };
    let Some(legacy_dir) = legacy.parent() else {
        return;
    };
    match std::fs::rename(legacy_dir, new_dir) {
        Ok(()) => {
            eprintln!(
                "Migrated configuration directory: {} -> {}",
                legacy_dir.display(),
                new_dir.display(),
            );
        }
        Err(err) => {
            eprintln!(
                "Warning: failed to migrate {} to {}: {err}",
                legacy_dir.display(),
                new_dir.display(),
            );
        }
    }
}

fn copilot_token_path() -> Option<PathBuf> {
    dirs::home_dir().map(|h| {
        h.join(".local")
            .join("share")
            .join("copilot-api")
            .join("github_token")
    })
}

fn write_sandbox_config(path: &Path, default_shell: Option<&str>) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut table = Table::new();
    // SANDBOX PATCH: model selection is read from `~/.codex/config.toml` by
    // codex-core, not from this file. Do not write `default_model` here.
    // SANDBOX PATCH: `copilot_api_port` is a v5-era loopback artifact;
    // nothing binds a port in v6. Do not write it.
    if let Some(shell) = default_shell {
        table.insert("default_shell".into(), Value::String(shell.to_string()));
    }
    let content = toml::to_string(&table).context("failed to serialize sandbox config")?;
    std::fs::write(path, content).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(())
}

fn prompt_for_shell() -> Result<Option<String>> {
    let git_bash = detect_git_bash();

    println!("Select default shell:");
    println!("  [1] PowerShell (default)");
    let other_index: u8 = if let Some(path) = &git_bash {
        println!("  [2] Git Bash (detected at {})", path.display());
        println!("  [3] Other (enter absolute path)");
        3
    } else {
        println!("  [2] Other (enter absolute path)");
        2
    };
    print!("Choice [1]: ");
    io::stdout().flush().ok();

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let choice = line.trim();

    if choice.is_empty() || choice == "1" {
        return Ok(None);
    }

    if choice == "2" && git_bash.is_some() {
        return Ok(Some(git_bash.unwrap().to_string_lossy().into_owned()));
    }

    if choice == other_index.to_string() {
        return prompt_for_other_path();
    }

    bail!("invalid choice: {choice}");
}

fn prompt_for_other_path() -> Result<Option<String>> {
    print!("Absolute path to shell executable: ");
    io::stdout().flush().ok();
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    let path = line.trim();
    if path.is_empty() {
        bail!("shell path cannot be empty");
    }
    let pb = PathBuf::from(path);
    if !pb.exists() {
        bail!("shell path does not exist: {}", pb.display());
    }
    Ok(Some(pb.to_string_lossy().into_owned()))
}

/// Detect Git Bash. Checks (in order):
/// 1. `C:\Program Files\Git\bin\bash.exe`
/// 2. `C:\Program Files (x86)\Git\bin\bash.exe`
/// 3. `%LOCALAPPDATA%\Programs\Git\bin\bash.exe`
/// 4. `where git` → resolve `<git-root>\bin\bash.exe` (typical Git for
///    Windows install layout: `git.exe` is in `Git\cmd\`, bash in `Git\bin\`).
/// 5. `where bash` (last-resort PATH lookup; covers MSYS2-standalone setups).
pub(crate) fn detect_git_bash() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = vec![
        PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
        PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
    ];
    if let Some(la) = std::env::var_os("LOCALAPPDATA") {
        candidates.push(
            PathBuf::from(la)
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    if let Some(p) = first_existing(&candidates) {
        return Some(p);
    }
    if let Some(p) = where_git_to_bash() {
        return Some(p);
    }
    where_bash()
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

/// Locate `git.exe` on PATH and resolve the sibling `bash.exe` via the
/// Git for Windows directory layout. `git.exe` lives in `<root>\cmd\` on
/// most installs (and in `<root>\bin\` on a few); bash is at
/// `<root>\bin\bash.exe`. Also probes `<root>\usr\bin\bash.exe` (the
/// MSYS2-style location used by some portable installs).
fn where_git_to_bash() -> Option<PathBuf> {
    let git = where_program("git")?;
    let parent = git.parent()?;
    let candidates = [
        // git.exe in cmd/ → ../bin/bash.exe
        parent.parent().map(|p| p.join("bin").join("bash.exe")),
        // git.exe in bin/ → ./bash.exe
        Some(parent.join("bash.exe")),
        // MSYS2-style portable git → ../usr/bin/bash.exe
        parent
            .parent()
            .map(|p| p.join("usr").join("bin").join("bash.exe")),
    ];
    candidates.into_iter().flatten().find(|p| p.exists())
}

fn where_bash() -> Option<PathBuf> {
    where_program("bash")
}

/// Resolve `<program>` via `where` (Windows) / `which` (Unix). Returns the
/// first existing absolute path, or `None`.
fn where_program(program: &str) -> Option<PathBuf> {
    let resolver = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(resolver).arg(program).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let p = PathBuf::from(trimmed);
        if p.exists() {
            return Some(p);
        }
    }
    None
}

fn run_login() -> Result<()> {
    let codex_core = discovery::find_codex_core()?;
    run_login_with(&codex_core, false)
}

fn run_login_with(codex_core: &Path, force: bool) -> Result<()> {
    let mut cmd = Command::new(codex_core);
    cmd.arg("login").arg("--provider").arg("copilot");
    if force {
        cmd.arg("--force");
    }

    let status = cmd
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run codex-core login --provider copilot: {e}"))?;
    if !status.success() {
        bail!("Login failed. Retry with: codex login --provider copilot");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_sandbox_config;
    use tempfile::tempdir;

    #[test]
    fn first_existing_returns_first_present() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        let c = dir.path().join("c");
        std::fs::write(&b, "").unwrap();
        std::fs::write(&c, "").unwrap();
        assert_eq!(first_existing(&[a, b.clone(), c]), Some(b));
    }

    #[test]
    fn first_existing_returns_none_when_absent() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        assert_eq!(first_existing(&[a, b]), None);
    }

    #[test]
    fn where_git_to_bash_resolves_cmd_layout() {
        // Simulate `where git` returning <root>\cmd\git.exe by hand-constructing
        // the layout in a tempdir and asserting the candidate-search picks
        // <root>\bin\bash.exe. This tests the path-resolution logic directly,
        // independent of the host's PATH.
        let dir = tempdir().unwrap();
        let root = dir.path();
        let cmd_dir = root.join("cmd");
        let bin_dir = root.join("bin");
        std::fs::create_dir_all(&cmd_dir).unwrap();
        std::fs::create_dir_all(&bin_dir).unwrap();
        let bash = bin_dir.join("bash.exe");
        std::fs::write(&bash, "").unwrap();
        let git = cmd_dir.join("git.exe");
        std::fs::write(&git, "").unwrap();

        // Manually exercise the candidate list (`where_git_to_bash` shells out
        // to `where`, which we cannot stub portably from a unit test).
        let parent = git.parent().unwrap();
        let candidates: Vec<PathBuf> = [
            parent.parent().map(|p| p.join("bin").join("bash.exe")),
            Some(parent.join("bash.exe")),
            parent
                .parent()
                .map(|p| p.join("usr").join("bin").join("bash.exe")),
        ]
        .into_iter()
        .flatten()
        .filter(|p| p.exists())
        .collect();

        assert_eq!(candidates.first(), Some(&bash));
    }

    #[test]
    fn detect_git_bash_uses_localappdata() {
        let dir = tempdir().unwrap();
        let fake_bash = dir
            .path()
            .join("Programs")
            .join("Git")
            .join("bin")
            .join("bash.exe");
        std::fs::create_dir_all(fake_bash.parent().unwrap()).unwrap();
        std::fs::write(&fake_bash, "").unwrap();

        let prev = std::env::var_os("LOCALAPPDATA");
        // SAFETY: setting env vars within a single test; concurrent tests in
        // this module do not rely on LOCALAPPDATA.
        unsafe {
            std::env::set_var("LOCALAPPDATA", dir.path());
        }
        let found = detect_git_bash();
        unsafe {
            match prev {
                Some(v) => std::env::set_var("LOCALAPPDATA", v),
                None => std::env::remove_var("LOCALAPPDATA"),
            }
        }

        // First-existing semantics: earlier hard-coded paths win if the dev
        // machine actually has Git installed there. Otherwise, the tempdir
        // (resolved via LOCALAPPDATA) wins.
        let pf = PathBuf::from(r"C:\Program Files\Git\bin\bash.exe");
        let pf86 = PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe");
        if pf.exists() {
            assert_eq!(found, Some(pf));
        } else if pf86.exists() {
            assert_eq!(found, Some(pf86));
        } else {
            assert_eq!(found, Some(fake_bash));
        }
    }

    #[test]
    fn write_sandbox_config_includes_shell_when_provided() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nested").join("config.toml");
        write_sandbox_config(&path, Some(r"C:\Program Files\Git\bin\bash.exe")).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // SANDBOX PATCH: launcher must NOT write `default_model` — that key is
        // read from `~/.codex/config.toml` by codex-core natively.
        assert!(!content.contains("default_model"));
        // SANDBOX PATCH: `copilot_api_port` is a v5-era loopback artifact and
        // must not be re-introduced.
        assert!(!content.contains("copilot_api_port"));
        assert!(content.contains("default_shell ="));
        assert!(content.contains("bash.exe"));
    }

    #[test]
    fn write_sandbox_config_omits_shell_when_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_sandbox_config(&path, None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        // SANDBOX PATCH: launcher must NOT write `default_model` or
        // `copilot_api_port`.
        assert!(!content.contains("default_model"));
        assert!(!content.contains("copilot_api_port"));
        assert!(!content.contains("default_shell"));
    }

    #[test]
    fn write_sandbox_config_omits_auto_load_claude_md() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");

        write_sandbox_config(&path, None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("auto_load_claude_md"));
        let cfg = parse_sandbox_config(&content);
        assert!(cfg.auto_load_claude_md.unwrap_or(true));

        write_sandbox_config(&path, Some(r"C:\Program Files\Git\bin\bash.exe")).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("auto_load_claude_md"));
        let cfg = parse_sandbox_config(&content);
        assert!(cfg.auto_load_claude_md.unwrap_or(true));
    }

    #[test]
    fn run_login_invokes_codex_core_not_launcher() {
        let dir = tempdir().unwrap();
        let marker_path = dir.path().join("marker.txt");
        let args_path = dir.path().join("args.txt");

        #[cfg(windows)]
        let codex_core = {
            let script = dir.path().join("codex-core.cmd");
            std::fs::write(
                &script,
                format!(
                    "@echo off\r\necho invoked>\"{}\"\r\necho %* > \"{}\"\r\n",
                    marker_path.display(),
                    args_path.display()
                ),
            )
            .unwrap();
            script
        };

        #[cfg(unix)]
        let codex_core = {
            use std::os::unix::fs::PermissionsExt;

            let script = dir.path().join("codex-core");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nprintf 'invoked\n' > '{}'\nprintf '%s\n' \"$*\" > '{}'\n",
                    marker_path.display(),
                    args_path.display()
                ),
            )
            .unwrap();
            let mut perms = std::fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&script, perms).unwrap();
            script
        };

        run_login_with(&codex_core, false).unwrap();

        assert!(marker_path.exists(), "expected {}", marker_path.display());
        let args = std::fs::read_to_string(&args_path).unwrap();
        assert!(
            args.contains("login --provider copilot"),
            "expected forwarded login args in {args:?}"
        );
    }
}
