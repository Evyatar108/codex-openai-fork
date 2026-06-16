use codex_network_proxy::NetworkProxy;
use codex_utils_absolute_path::AbsolutePathBuf;
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use tokio::process::Child;
use tokio::process::Command;
use tracing::trace;

use codex_protocol::permissions::NetworkSandboxPolicy;

/// Experimental environment variable that will be set to some non-empty value
/// if both of the following are true:
///
/// 1. The process was spawned by Codex as part of a shell tool call.
/// 2. NetworkSandboxPolicy is restricted for the tool call.
///
/// We may try to have just one environment variable for all sandboxing
/// attributes, so this may change in the future.
pub const CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR: &str = "CODEX_SANDBOX_NETWORK_DISABLED";

/// Should be set when the process is spawned under a sandbox. Currently, the
/// value is "seatbelt" for macOS, but it may change in the future to
/// accommodate sandboxing configuration and other sandboxing mechanisms.
pub const CODEX_SANDBOX_ENV_VAR: &str = "CODEX_SANDBOX";

#[derive(Debug, Clone, Copy)]
pub enum StdioPolicy {
    RedirectForShellTool,
    Inherit,
}

/// Spawns the appropriate child process for the exec params and sandbox settings,
/// ensuring the args and environment variables used to create the `Command`
/// (and `Child`) honor the configuration.
///
/// For now, we take `NetworkSandboxPolicy` as a parameter to spawn_child()
/// because we need to determine whether to set the
/// `CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR` environment variable.
pub(crate) struct SpawnChildRequest<'a> {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub arg0: Option<&'a str>,
    pub cwd: AbsolutePathBuf,
    pub network_sandbox_policy: NetworkSandboxPolicy,
    pub network: Option<&'a NetworkProxy>,
    pub stdio_policy: StdioPolicy,
    pub env: HashMap<String, String>,
}

pub(crate) async fn spawn_child_async(request: SpawnChildRequest<'_>) -> std::io::Result<Child> {
    let SpawnChildRequest {
        program,
        args,
        arg0,
        cwd,
        network_sandbox_policy,
        network,
        stdio_policy,
        mut env,
    } = request;

    trace!(
        "spawn_child_async: {program:?} {args:?} {arg0:?} {cwd:?} {network_sandbox_policy:?} {stdio_policy:?} {env:?}"
    );

    let mut cmd = Command::new(&program);
    #[cfg(unix)]
    cmd.arg0(arg0.map_or_else(|| program.to_string_lossy().to_string(), String::from));
    cmd.args(args);
    cmd.current_dir(cwd);
    if let Some(network) = network {
        network.apply_to_env(&mut env);
    }
    cmd.env_clear();
    cmd.envs(env);

    if !network_sandbox_policy.is_enabled() {
        cmd.env(CODEX_SANDBOX_NETWORK_DISABLED_ENV_VAR, "1");
    }

    // If this Codex process dies (including being killed via SIGKILL), we want
    // any child processes that were spawned as part of a `"shell"` tool call
    // to also be terminated.

    #[cfg(unix)]
    unsafe {
        let detach_from_tty = matches!(stdio_policy, StdioPolicy::RedirectForShellTool);
        #[cfg(target_os = "linux")]
        let parent_pid = libc::getpid();
        cmd.pre_exec(move || {
            if detach_from_tty {
                codex_utils_pty::process_group::detach_from_tty()?;
            }

            // This relies on prctl(2), so it only works on Linux.
            #[cfg(target_os = "linux")]
            {
                // This prctl call effectively requests, "deliver SIGTERM when my
                // current parent dies."
                codex_utils_pty::process_group::set_parent_death_signal(parent_pid)?;
            }
            Ok(())
        });
    }

    match stdio_policy {
        StdioPolicy::RedirectForShellTool => {
            // Do not create a file descriptor for stdin because otherwise some
            // commands may hang forever waiting for input. For example, ripgrep has
            // a heuristic where it may try to read from stdin as explained here:
            // https://github.com/BurntSushi/ripgrep/blob/e2362d4d5185d02fa857bf381e7bd52e66fafc73/crates/core/flags/hiargs.rs#L1101-L1103
            cmd.stdin(Stdio::null());

            cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        }
        StdioPolicy::Inherit => {
            // Inherit stdin, stdout, and stderr from the parent process.
            cmd.stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        }
    }

    // On Windows, confine the child + its descendants in a Job Object so we
    // can reliably terminate the whole tree when the immediate child exits.
    // Without this, grandchildren (cargo/rustc/link.exe spawned by a shell)
    // outlive the child, keep the inherited stdout/stderr pipe handles open,
    // and leave `tokio::io::blocking` readers parked on `ReadFile` forever —
    // which in turn hangs `BlockingPool::Drop` during runtime shutdown.
    // This complements (not duplicates) the Unix `prctl(PR_SET_PDEATHSIG)`
    // mechanism above: prctl kills the immediate child when codex-core dies,
    // JobObject kills grandchildren when the immediate child dies.
    #[cfg(windows)]
    {
        // CREATE_SUSPENDED so grandchildren cannot spawn before we assign
        // the child to the Job Object.
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED);
    }

    let child = cmd.kill_on_drop(true).spawn()?;

    #[cfg(windows)]
    {
        if let Err(err) = attach_windows_job(&child) {
            // Non-fatal: fall back to the legacy behavior (no job + pipe
            // inheritance leak possible). Preserves functionality in
            // restricted environments (e.g. outer job without nested-job
            // support, some CI runners) at the cost of the grandchild-leak
            // shutdown hang this patch is designed to prevent. `kill_on_drop`
            // still terminates the immediate child.
            tracing::warn!("windows job-object attach failed; spawning without it: {err}");
            // The child was spawned CREATE_SUSPENDED; resume it now so tool
            // execution can proceed.
            if let Some(raw) = child
                .raw_handle()
                .map(|h| h as windows_sys::Win32::Foundation::HANDLE)
            {
                if let Err(resume_err) = crate::windows_job::resume_process(raw) {
                    tracing::error!(
                        "failed to resume suspended child after job fallback: {resume_err}"
                    );
                }
            }
        }
    }

    Ok(child)
}

/// Wraps the Windows-specific work of attaching a freshly-spawned
/// (CREATE_SUSPENDED) child to a kill-on-close Job Object, resuming it,
/// and installing a watcher task that closes the job once the immediate
/// child exits.
#[cfg(windows)]
fn attach_windows_job(child: &Child) -> std::io::Result<()> {
    let raw_handle = child.raw_handle().ok_or_else(|| {
        std::io::Error::other("tokio::process::Child has no raw_handle on Windows")
    })? as windows_sys::Win32::Foundation::HANDLE;
    let job = crate::windows_job::create_kill_on_close_job()?;
    crate::windows_job::assign_process_to_job(&job, raw_handle)?;
    crate::windows_job::resume_process(raw_handle)?;
    // `close_job_on_child_exit` duplicates `raw_handle` internally, so the
    // watcher is independent of `tokio::process::Child`'s handle lifetime.
    // This prevents PID recycling races when `kill_on_drop` closes the
    // Child's handle before the watcher runs.
    crate::windows_job::close_job_on_child_exit(raw_handle, job)?;
    // SANDBOX PATCH: emit watcher-installed breadcrumb. Gated by CODEX_SHUTDOWN_TRACE=1; the
    // corresponding `close` breadcrumb fires from `windows_job::close_job_on_child_exit`. See
    // docs/implementation/patch-surface.md §14 invariant 26.
    if let Some(pid) = child.id() {
        codex_stream_diagnostics::trace_job_object_watcher_installed(pid);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    // STRUCTURAL-ONLY INVARIANT TEST.
    // This test asserts that specific source-text patterns exist in the production code
    // via include_str!() + .find(). It does NOT exercise behavior — a regression that
    // changes BEHAVIOR while preserving the source text WILL NOT be caught. Behavioral
    // coverage of the Windows job-object spawn ordering is intentionally deferred
    // (Plan Risk #4). If you are investigating a real bug here, add a separate
    // behavioral test that drives the Windows spawn path through the runtime.
    #[cfg(windows)]
    #[test]
    fn windows_spawn_child_async_attaches_job_wrapper() {
        let source = include_str!("spawn.rs");
        let create_suspended = source
            .find("cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_SUSPENDED)")
            .expect("Windows spawn should create the child suspended");
        let spawn_child = source
            .find("let child = cmd.kill_on_drop(true).spawn()?;")
            .expect("spawn_child_async should spawn exactly once through tokio Command");
        let attach_job = source
            .find("attach_windows_job(&child)")
            .expect("Windows spawn should attach the job wrapper after spawning");

        assert!(
            create_suspended < spawn_child && spawn_child < attach_job,
            "Windows tool-exec spawn must create a suspended child, then attach the Job Object wrapper",
        );
    }
}
