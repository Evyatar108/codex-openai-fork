//! One-shot console initialization for `codex exec` on Windows.
//!
//! Without this, the legacy Windows console renders Rust's bare-LF newlines
//! (`\n`) as a vertical-only cursor move, leaving the cursor in the previous
//! line's column. Each subsequent `eprintln!`/`println!` then begins indented
//! by the prior line's length — the classic cascading-indent symptom.
//!
//! Enabling `ENABLE_VIRTUAL_TERMINAL_PROCESSING` (and keeping
//! `ENABLE_PROCESSED_OUTPUT`) on the stdout/stderr console handles makes the
//! console treat a bare LF as a full newline (CR+LF behavior), which matches
//! what Rust's `println!`/`eprintln!` macros actually emit.
//!
//! See:
//! - <https://learn.microsoft.com/windows/console/setconsolemode>
//! - <https://learn.microsoft.com/windows/console/console-virtual-terminal-sequences>
//!
//! On non-Windows targets this is a no-op.

#[cfg(target_os = "windows")]
pub(crate) fn init_stdio_for_exec() {
    use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;
    use windows_sys::Win32::System::Console::ENABLE_PROCESSED_OUTPUT;
    use windows_sys::Win32::System::Console::ENABLE_VIRTUAL_TERMINAL_PROCESSING;
    use windows_sys::Win32::System::Console::GetConsoleMode;
    use windows_sys::Win32::System::Console::GetStdHandle;
    use windows_sys::Win32::System::Console::STD_ERROR_HANDLE;
    use windows_sys::Win32::System::Console::STD_OUTPUT_HANDLE;
    use windows_sys::Win32::System::Console::SetConsoleMode;

    // Apply to both stdout and stderr. If a handle is redirected to a file or
    // pipe (not a console), `GetConsoleMode` fails and we silently skip — no
    // VT processing is needed for non-console destinations.
    for std_handle_id in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: `GetStdHandle` / `GetConsoleMode` / `SetConsoleMode` are
        // documented thread-safe Win32 calls that take primitive arguments.
        // We validate the handle and the `GetConsoleMode` return value before
        // using them.
        unsafe {
            let handle = GetStdHandle(std_handle_id);
            // In windows-sys 0.52, `HANDLE` is `isize`. Treat 0 (null) and
            // `INVALID_HANDLE_VALUE` as failure.
            if handle == 0 || handle == INVALID_HANDLE_VALUE {
                continue;
            }
            let mut mode: u32 = 0;
            if GetConsoleMode(handle, &mut mode) == 0 {
                // Not a console (e.g. redirected to a pipe) — nothing to do.
                continue;
            }
            let new_mode = mode | ENABLE_PROCESSED_OUTPUT | ENABLE_VIRTUAL_TERMINAL_PROCESSING;
            if new_mode != mode {
                // Best-effort: ignore failures. If SetConsoleMode is unavailable
                // (e.g. legacy console host without VT support), the user will
                // still see the cascading-indent symptom but nothing else
                // breaks.
                let _ = SetConsoleMode(handle, new_mode);
            }
        }
    }
}

#[cfg(not(target_os = "windows"))]
pub(crate) fn init_stdio_for_exec() {}
