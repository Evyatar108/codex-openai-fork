//! SANDBOX PATCH tests: Windows native-shell routing for plugin hook execution.
//!
//! Pins `hook_shell_should_use_native` / `hook_shell_invocation` in the parent
//! module. See `docs/implementation/patch-surface.md` §14 and
//! `docs/implementation/regression-history.md`. The truth-table assertions are
//! platform-gated (`cfg!(windows)`) so they stay green on every CI target.

use std::path::PathBuf;

use pretty_assertions::assert_eq;

use super::hook_shell_invocation;
use super::hook_shell_should_use_native;
use crate::shell::Shell;
use crate::shell::ShellType;

fn shell_of(shell_type: ShellType) -> Shell {
    Shell {
        shell_type,
        shell_path: PathBuf::from("test-shell"),
    }
}

#[test]
fn posix_shells_route_to_native_only_on_windows() {
    for shell_type in [ShellType::Bash, ShellType::Zsh, ShellType::Sh] {
        assert_eq!(
            hook_shell_should_use_native(&shell_of(shell_type)),
            cfg!(windows),
            "POSIX shell {shell_type:?} should route hooks to the native shell only on Windows",
        );
    }
}

#[test]
fn native_shells_keep_the_session_shell() {
    for shell_type in [ShellType::PowerShell, ShellType::Cmd] {
        assert!(
            !hook_shell_should_use_native(&shell_of(shell_type)),
            "native shell {shell_type:?} should always keep the session shell for hooks",
        );
    }
}

#[test]
fn invocation_skips_posix_session_shell_on_windows() {
    let bash = shell_of(ShellType::Bash);
    // On Windows the POSIX session shell is dropped (empty program -> native
    // cmd.exe fallback); elsewhere the session shell is used unchanged.
    let expected = if cfg!(windows) {
        (None, Vec::new())
    } else {
        (Some("test-shell".to_string()), vec!["-c".to_string()])
    };
    assert_eq!(hook_shell_invocation(Some(&bash)), expected);
}

#[test]
fn invocation_preserves_native_session_shell() {
    let powershell = shell_of(ShellType::PowerShell);
    assert_eq!(
        hook_shell_invocation(Some(&powershell)),
        (
            Some("test-shell".to_string()),
            vec!["-NoProfile".to_string(), "-Command".to_string()],
        ),
    );
}

#[test]
fn invocation_without_session_shell_is_empty() {
    assert_eq!(hook_shell_invocation(None), (None, Vec::new()));
}
