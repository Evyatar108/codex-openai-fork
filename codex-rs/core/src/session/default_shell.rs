use crate::shell;
use std::path::PathBuf;

// SANDBOX PATCH: startup warning when the opt-in Windows Git Bash shell feature
// cannot find Git Bash and falls back to the existing Windows default shell.
const WINDOWS_GIT_BASH_NOT_FOUND_WARNING: &str = "`windows_git_bash_shell` is enabled, but Codex could not detect Git Bash. Falling back to the default Windows shell.";

pub(super) struct DefaultShellSelection {
    pub(super) shell: shell::Shell,
    pub(super) startup_warning: Option<String>,
}

// SANDBOX PATCH: keep explicit default_shell and zsh-fork precedence ahead of
// the default-off Windows Git Bash detector.
pub(super) fn select_default_shell(
    user_shell_override: Option<shell::Shell>,
    use_zsh_fork_shell: bool,
    zsh_path: Option<&PathBuf>,
    use_windows_git_bash_shell: bool,
    detected_windows_git_bash_shell: Option<shell::Shell>,
    default_shell: impl FnOnce() -> shell::Shell,
) -> anyhow::Result<DefaultShellSelection> {
    if let Some(user_shell_override) = user_shell_override {
        return Ok(DefaultShellSelection {
            shell: user_shell_override,
            startup_warning: None,
        });
    }

    if use_zsh_fork_shell {
        let zsh_path = zsh_path.ok_or_else(|| {
            anyhow::anyhow!(
                "zsh fork feature enabled, but no packaged zsh fork is available for this install"
            )
        })?;
        let zsh_path = zsh_path.to_path_buf();
        let shell = shell::get_shell(shell::ShellType::Zsh, Some(&zsh_path)).ok_or_else(|| {
            anyhow::anyhow!(
                "zsh fork feature enabled, but packaged zsh fork `{}` is not usable",
                zsh_path.display()
            )
        })?;
        return Ok(DefaultShellSelection {
            shell,
            startup_warning: None,
        });
    }

    if use_windows_git_bash_shell {
        if let Some(shell) = detected_windows_git_bash_shell {
            return Ok(DefaultShellSelection {
                shell,
                startup_warning: None,
            });
        }

        return Ok(DefaultShellSelection {
            shell: default_shell(),
            startup_warning: Some(WINDOWS_GIT_BASH_NOT_FOUND_WARNING.to_string()),
        });
    }

    Ok(DefaultShellSelection {
        shell: default_shell(),
        startup_warning: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn test_shell(shell_type: shell::ShellType, path: &str) -> shell::Shell {
        shell::Shell {
            shell_type,
            shell_path: PathBuf::from(path),
            shell_snapshot: shell::empty_shell_snapshot_receiver(),
        }
    }

    #[test]
    fn explicit_override_wins_over_git_bash_feature() -> anyhow::Result<()> {
        let override_shell = test_shell(shell::ShellType::PowerShell, "pwsh.exe");
        let git_bash_shell =
            test_shell(shell::ShellType::Bash, r"C:\Program Files\Git\bin\bash.exe");

        let selected = select_default_shell(
            Some(override_shell.clone()),
            /*use_zsh_fork_shell*/ false,
            /*zsh_path*/ None,
            /*use_windows_git_bash_shell*/ true,
            Some(git_bash_shell),
            || test_shell(shell::ShellType::Cmd, "cmd.exe"),
        )?;

        assert_eq!(selected.shell, override_shell);
        assert_eq!(selected.startup_warning, None);
        Ok(())
    }

    #[test]
    fn zsh_fork_wins_over_git_bash_feature() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let zsh_path = temp_dir.path().join("zsh.exe");
        std::fs::write(&zsh_path, "")?;
        let git_bash_shell =
            test_shell(shell::ShellType::Bash, r"C:\Program Files\Git\bin\bash.exe");

        let selected = select_default_shell(
            /*user_shell_override*/ None,
            /*use_zsh_fork_shell*/ true,
            Some(&zsh_path),
            /*use_windows_git_bash_shell*/ true,
            Some(git_bash_shell),
            || test_shell(shell::ShellType::Cmd, "cmd.exe"),
        )?;

        assert_eq!(selected.shell.shell_type, shell::ShellType::Zsh);
        assert_eq!(selected.shell.shell_path, zsh_path);
        assert_eq!(selected.startup_warning, None);
        Ok(())
    }

    #[test]
    fn git_bash_feature_uses_detected_bash_shell() -> anyhow::Result<()> {
        let git_bash_shell =
            test_shell(shell::ShellType::Bash, r"C:\Program Files\Git\bin\bash.exe");

        let selected = select_default_shell(
            /*user_shell_override*/ None,
            /*use_zsh_fork_shell*/ false,
            /*zsh_path*/ None,
            /*use_windows_git_bash_shell*/ true,
            Some(git_bash_shell.clone()),
            || test_shell(shell::ShellType::PowerShell, "pwsh.exe"),
        )?;

        assert_eq!(selected.shell, git_bash_shell);
        assert_eq!(selected.startup_warning, None);
        Ok(())
    }

    #[test]
    fn missing_git_bash_warns_and_falls_back() -> anyhow::Result<()> {
        let fallback = test_shell(shell::ShellType::PowerShell, "pwsh.exe");

        let selected = select_default_shell(
            /*user_shell_override*/ None,
            /*use_zsh_fork_shell*/ false,
            /*zsh_path*/ None,
            /*use_windows_git_bash_shell*/ true,
            /*detected_windows_git_bash_shell*/ None,
            || fallback.clone(),
        )?;

        assert_eq!(selected.shell, fallback);
        assert_eq!(
            selected.startup_warning.as_deref(),
            Some(WINDOWS_GIT_BASH_NOT_FOUND_WARNING)
        );
        Ok(())
    }

    #[test]
    fn feature_off_uses_default_without_warning() -> anyhow::Result<()> {
        let fallback = test_shell(shell::ShellType::PowerShell, "pwsh.exe");
        let git_bash_shell =
            test_shell(shell::ShellType::Bash, r"C:\Program Files\Git\bin\bash.exe");

        let selected = select_default_shell(
            /*user_shell_override*/ None,
            /*use_zsh_fork_shell*/ false,
            /*zsh_path*/ None,
            /*use_windows_git_bash_shell*/ false,
            Some(git_bash_shell),
            || fallback.clone(),
        )?;

        assert_eq!(selected.shell, fallback);
        assert_eq!(selected.startup_warning, None);
        Ok(())
    }
}
