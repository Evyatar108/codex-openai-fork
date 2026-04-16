use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use toml::{Table, Value};

use crate::discovery;

/// First-run bootstrap: TTY-gated shell prompt, sandbox config write, and
/// copilot-api login trigger. Idempotent — returns `Ok(())` immediately when
/// both the sandbox config and the copilot-api token already exist.
pub fn first_run_bootstrap() -> Result<()> {
    let config_path =
        sandbox_config_path().context("unable to resolve home directory for sandbox config")?;
    let token_path =
        copilot_token_path().context("unable to resolve home directory for copilot token")?;

    let config_exists = config_path.exists();
    let token_exists = token_path.exists();

    if config_exists && token_exists {
        return Ok(());
    }

    let is_tty = io::stdin().is_terminal();

    if !is_tty {
        if !token_exists {
            eprintln!(
                "First-time setup requires an interactive terminal.\n  Run manually: codex-copilot-gateway login\n  Then re-run codex."
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
    dirs::home_dir().map(|h| h.join(".codex-sandbox").join("config.toml"))
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
    table.insert("copilot_api_port".into(), Value::Integer(4141));
    table.insert("default_model".into(), Value::String("gpt-5.4".into()));
    if let Some(shell) = default_shell {
        table.insert("default_shell".into(), Value::String(shell.to_string()));
    }
    let content = toml::to_string(&table).context("failed to serialize sandbox config")?;
    std::fs::write(path, content)
        .with_context(|| format!("failed to write {}", path.display()))?;
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
        return Ok(Some(
            git_bash.unwrap().to_string_lossy().into_owned(),
        ));
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

/// Detect Git Bash. Checks (in order): `C:\Program Files\Git\bin\bash.exe`,
/// `C:\Program Files (x86)\Git\bin\bash.exe`,
/// `%LOCALAPPDATA%\Programs\Git\bin\bash.exe`, then `where bash`.
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
    where_bash()
}

fn first_existing(paths: &[PathBuf]) -> Option<PathBuf> {
    paths.iter().find(|p| p.exists()).cloned()
}

fn where_bash() -> Option<PathBuf> {
    let program = if cfg!(windows) { "where" } else { "which" };
    let output = Command::new(program).arg("bash").output().ok()?;
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
    let gateway = discovery::find_codex_copilot_gateway()?;
    let status = Command::new(&gateway)
        .arg("login")
        .stdin(std::process::Stdio::inherit())
        .stdout(std::process::Stdio::inherit())
        .stderr(std::process::Stdio::inherit())
        .status()
        .map_err(|e| anyhow::anyhow!("failed to run codex-copilot-gateway login: {e}"))?;
    if !status.success() {
        bail!("Login failed. Retry with: codex-copilot-gateway login");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(content.contains("copilot_api_port = 4141"));
        assert!(content.contains("default_model = \"gpt-5.4\""));
        assert!(content.contains("default_shell ="));
        assert!(content.contains("bash.exe"));
    }

    #[test]
    fn write_sandbox_config_omits_shell_when_none() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("config.toml");
        write_sandbox_config(&path, None).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("copilot_api_port = 4141"));
        assert!(content.contains("default_model = \"gpt-5.4\""));
        assert!(!content.contains("default_shell"));
    }
}
