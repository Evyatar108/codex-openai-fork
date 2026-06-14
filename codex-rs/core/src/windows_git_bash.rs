use crate::shell;
use crate::shell::Shell;
use crate::shell::ShellType;
use std::env;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;

// SANDBOX PATCH: Git-for-Windows fixed install paths checked before PATH probes.
const SYSTEM_GIT_BASH_PATHS: &[&str] = &[
    r"C:\Program Files\Git\bin\bash.exe",
    r"C:\Program Files (x86)\Git\bin\bash.exe",
];

pub(crate) fn detect() -> Option<Shell> {
    // SANDBOX PATCH: this detector only resolves a Shell. Git Bash commands
    // still run through the existing session-shell exec path and Windows Job
    // Object cleanup.
    let local_app_data = env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let candidates = ordered_candidates(
        local_app_data.as_deref(),
        &where_paths("git"),
        &where_paths("bash"),
    );
    first_existing_candidate(candidates, Path::is_file).and_then(shell_from_path)
}

fn shell_from_path(path: PathBuf) -> Option<Shell> {
    shell::get_shell(ShellType::Bash, Some(&path))
}

fn ordered_candidates(
    local_app_data: Option<&Path>,
    where_git_paths: &[PathBuf],
    where_bash_paths: &[PathBuf],
) -> Vec<PathBuf> {
    // SANDBOX PATCH: preserve the launcher-era ordering: fixed Git installs,
    // `where git`-derived Git Bash candidates, then last-resort `where bash`.
    let mut candidates = fixed_install_candidates(local_app_data);
    candidates.extend(
        where_git_paths
            .iter()
            .flat_map(|git_path| bash_candidates_from_git_path(git_path)),
    );
    candidates.extend(where_bash_paths.iter().cloned());
    candidates
}

fn fixed_install_candidates(local_app_data: Option<&Path>) -> Vec<PathBuf> {
    let mut candidates = SYSTEM_GIT_BASH_PATHS
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if let Some(local_app_data) = local_app_data {
        candidates.push(
            local_app_data
                .join("Programs")
                .join("Git")
                .join("bin")
                .join("bash.exe"),
        );
    }
    candidates
}

fn bash_candidates_from_git_path(git_path: &Path) -> Vec<PathBuf> {
    let Some(git_dir) = git_path.parent() else {
        return Vec::new();
    };

    let mut candidates = Vec::new();
    // SANDBOX PATCH: keep candidate order as ..\bin, .\bash, then ..\usr\bin.
    if let Some(git_root) = git_dir.parent() {
        candidates.push(git_root.join("bin").join("bash.exe"));
    }
    candidates.push(git_dir.join("bash.exe"));
    if let Some(git_root) = git_dir.parent() {
        candidates.push(git_root.join("usr").join("bin").join("bash.exe"));
    }
    candidates
}

fn first_existing_candidate(
    candidates: Vec<PathBuf>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    candidates.into_iter().find(|path| exists(path.as_path()))
}

fn where_paths(binary: &str) -> Vec<PathBuf> {
    let Ok(output) = Command::new("where").arg(binary).output() else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }

    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(PathBuf::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    #[test]
    fn where_git_candidates_preserve_launcher_order_before_where_bash() {
        // SANDBOX PATCH: detector ordering regression coverage.
        let git_path = PathBuf::from(r"C:\Program Files\Git\cmd\git.exe");
        let wsl_bash = PathBuf::from(r"C:\Windows\System32\bash.exe");

        let candidates = ordered_candidates(
            Some(Path::new(r"C:\Users\me\AppData\Local")),
            &[git_path],
            std::slice::from_ref(&wsl_bash),
        );

        assert_eq!(
            candidates,
            vec![
                PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
                PathBuf::from(r"C:\Program Files (x86)\Git\bin\bash.exe"),
                PathBuf::from(r"C:\Users\me\AppData\Local\Programs\Git\bin\bash.exe"),
                PathBuf::from(r"C:\Program Files\Git\bin\bash.exe"),
                PathBuf::from(r"C:\Program Files\Git\cmd\bash.exe"),
                PathBuf::from(r"C:\Program Files\Git\usr\bin\bash.exe"),
                wsl_bash,
            ]
        );
    }

    #[test]
    fn first_existing_candidate_uses_detector_order() {
        let first = PathBuf::from(r"C:\first\bash.exe");
        let second = PathBuf::from(r"C:\second\bash.exe");
        let third = PathBuf::from(r"C:\third\bash.exe");

        assert_eq!(
            first_existing_candidate(vec![first, second.clone(), third], |path| path == second),
            Some(second)
        );
    }

    #[test]
    fn detected_shell_routes_through_existing_bash_exec_args() -> anyhow::Result<()> {
        // SANDBOX PATCH: Git Bash must remain a normal Bash Shell consumed by
        // derive_exec_args, not a parallel spawn path that bypasses Job Objects.
        let temp_dir = tempfile::tempdir()?;
        let bash_path = temp_dir.path().join("bash.exe");
        std::fs::write(&bash_path, "")?;

        let shell = shell_from_path(bash_path.clone()).expect("temp bash should resolve");

        assert_eq!(shell.shell_type, ShellType::Bash);
        assert_eq!(
            shell.derive_exec_args("echo hello", /*use_login_shell*/ false),
            vec![
                bash_path.to_string_lossy().to_string(),
                "-c".to_string(),
                "echo hello".to_string(),
            ]
        );
        assert_eq!(
            shell.derive_exec_args("echo hello", /*use_login_shell*/ true),
            vec![
                bash_path.to_string_lossy().to_string(),
                "-lc".to_string(),
                "echo hello".to_string(),
            ]
        );
        Ok(())
    }
}
