//! SANDBOX PATCH behavioral test: an empty `CommandShell.program` runs the hook
//! command through the native Windows shell (`cmd.exe /C`).
//!
//! An empty program is exactly what core's `hook_shell_invocation` produces for a
//! Windows POSIX session shell (Bash/Zsh/Sh). Routing through `cmd.exe` is what
//! makes a substituted backslash `${CLAUDE_PLUGIN_ROOT}` path resolve instead of
//! being mangled by a POSIX shell. This proof uses a cmd-only command (`echo
//! %OS%`) so it depends on neither `node` nor a real `bash` install. See
//! `docs/implementation/patch-surface.md` §14.

use std::collections::HashMap;
use std::path::Path;

use codex_protocol::protocol::HookEventName;
use codex_protocol::protocol::HookSource;
use codex_utils_absolute_path::test_support::PathBufExt;
use codex_utils_absolute_path::test_support::test_path_buf;
use pretty_assertions::assert_eq;

use super::run_command;
use crate::engine::CommandShell;
use crate::engine::ConfiguredHandler;

fn cmd_only_handler() -> ConfiguredHandler {
    ConfiguredHandler {
        event_name: HookEventName::PreToolUse,
        matcher: None,
        command: "echo %OS%".to_string(),
        timeout_sec: 30,
        status_message: None,
        source_path: test_path_buf("/tmp/hooks.json").abs(),
        source: HookSource::User,
        display_order: 0,
        env: HashMap::new(),
    }
}

#[tokio::test]
async fn empty_program_runs_command_through_native_cmd() {
    let shell = CommandShell {
        program: String::new(),
        args: Vec::new(),
    };
    let result = run_command(&shell, &cmd_only_handler(), "{}", Path::new(".")).await;

    assert_eq!(result.error, None, "native cmd.exe run should not error");
    assert!(
        result.stdout.contains("Windows_NT"),
        "cmd.exe should expand `%OS%` to `Windows_NT`; stdout was {:?}",
        result.stdout,
    );
}
