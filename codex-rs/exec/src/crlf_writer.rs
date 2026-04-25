// CRLF-aware replacements for `eprintln!` / `println!` used inside the
// `codex-exec` crate.
//
// ## Why this exists
//
// On Windows, the console host (conhost / Windows Terminal) and most pipes
// treat a bare `\n` (LF) as "cursor down only — do not reset column". The
// upstream `codex exec` code base writes `\n`-terminated lines via
// `eprintln!` / `println!`, which in turn calls `Stderr::write` / `Stdout::write`
// in libstd. On Windows that path goes through either `WriteConsoleW`
// (when the handle is a console) or `WriteFile` (when it is a pipe / file).
// In neither case does Rust's stdio translate `\n` into `\r\n`. The
// `SetConsoleMode(ENABLE_VIRTUAL_TERMINAL_PROCESSING|ENABLE_PROCESSED_OUTPUT)`
// flags enable VT-escape parsing and handle a few control characters, but
// they do NOT translate bare LF into CRLF — see the Microsoft "Console
// Reference" docs for `ENABLE_PROCESSED_OUTPUT` and the related VT
// processing mode. The visible result is the cascading-indent output
// reported by users (`codex / pong / tokens used / 24` stair-stepping
// further right with each line).
//
// ## What this file does
//
// We define two `macro_rules!` macros, `eprintln` and `println`, that
// shadow the prelude versions when explicitly `use`'d via
// `use crate::crlf_writer::{eprintln, println};` at the top of a module.
// Each macro routes through a synchronous helper that:
//
// 1. Formats the arguments with `format_args!`.
// 2. On Windows, replaces every `\n` not already preceded by `\r` with
//    `\r\n` (idempotent: existing `\r\n` is preserved).
// 3. Locks the appropriate `io::Stderr`/`io::Stdout` handle and writes the
//    translated bytes (`write_all` followed by `flush`).
//
// The helper writes synchronously to the regular Rust stdio handle, so
// there is no pipe/thread/teardown ordering concern: bytes hit the OS
// handle in the same call that returned to the caller, and the libstd
// stdio cleanup at process exit still flushes any line-buffered tail.
//
// On non-Windows targets, the helper passes the bytes through unchanged
// (with a trailing `\n`), which makes the macros byte-identical to
// `eprintln!`/`println!` on those platforms.
//
// ## Why we did not pursue Option B-1 (CreatePipe + SetStdHandle redirect)
//
// Option B-1 (redirect `STD_ERROR_HANDLE` / `STD_OUTPUT_HANDLE` to a pipe
// whose reader thread does the `\n`->`\r\n` translation, then writes back
// to the saved original handle) works in principle and would catch every
// stray write — including writes from third-party crates. However it
// introduces a non-trivial ordering hazard at process exit: between
// libstd flushing line-buffered stdio into the pipe and the OS killing
// our reader thread, there is a window where tail bytes can be dropped.
// Cleanly closing the loop requires resetting the std handles, closing
// our duplicate pipe write end, and joining the reader thread before
// `run_main` returns. We chose B-2 (this file) because the call-site
// surface inside `exec/` is finite and well-bounded, and the synchronous
// wrapper avoids any tail-drop or shutdown-ordering risk.
//
// ## Idempotence and non-Windows behavior
//
// The translation only fires under `cfg(windows)`. Running on Linux/macOS
// produces byte-identical output to the prelude `eprintln!`/`println!`.
// The translation step itself is idempotent: input `\r\n` is preserved
// exactly, and only orphan `\n` becomes `\r\n`. Calling the macros from
// multiple call sites is safe because each call locks the underlying
// handle for the duration of the formatted line.

use std::fmt::Arguments;
use std::io::Write;

/// Translate a UTF-8 string slice so every `\n` that is not already
/// preceded by `\r` becomes `\r\n`. Existing `\r\n` is preserved.
///
/// Operates on bytes (UTF-8 is ASCII-compatible for `\r` and `\n`, so
/// this is safe for any UTF-8 input).
#[cfg(windows)]
fn translate_lf_to_crlf(input: &str) -> Vec<u8> {
    let bytes = input.as_bytes();
    // Worst case: every byte is a bare `\n`, doubling the size.
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 8 + 1);
    let mut prev: u8 = 0;
    for &b in bytes {
        if b == b'\n' && prev != b'\r' {
            out.push(b'\r');
        }
        out.push(b);
        prev = b;
    }
    out
}

/// Windows: format, translate, write to stderr (locked), flush.
#[cfg(windows)]
pub(crate) fn write_stderr_line(args: Arguments<'_>) {
    let mut s = std::fmt::format(args);
    s.push('\n');
    let bytes = translate_lf_to_crlf(&s);
    let stderr = std::io::stderr();
    let mut guard = stderr.lock();
    let _ = guard.write_all(&bytes);
    let _ = guard.flush();
}

/// Windows: format, translate, write to stdout (locked), flush.
#[cfg(windows)]
pub(crate) fn write_stdout_line(args: Arguments<'_>) {
    let mut s = std::fmt::format(args);
    s.push('\n');
    let bytes = translate_lf_to_crlf(&s);
    let stdout = std::io::stdout();
    let mut guard = stdout.lock();
    let _ = guard.write_all(&bytes);
    let _ = guard.flush();
}

/// Non-Windows: byte-identical to `eprintln!`.
#[cfg(not(windows))]
pub(crate) fn write_stderr_line(args: Arguments<'_>) {
    let stderr = std::io::stderr();
    let mut guard = stderr.lock();
    let _ = guard.write_fmt(args);
    let _ = guard.write_all(b"\n");
}

/// Non-Windows: byte-identical to `println!`.
#[cfg(not(windows))]
pub(crate) fn write_stdout_line(args: Arguments<'_>) {
    let stdout = std::io::stdout();
    let mut guard = stdout.lock();
    let _ = guard.write_fmt(args);
    let _ = guard.write_all(b"\n");
}

// `eprintln!` shadow. Routes to the locked stderr writer that translates
// `\n` to `\r\n` on Windows. The `pub(crate) use` re-export below makes
// this importable as `crate::crlf_writer::eprintln`, where it shadows the
// prelude `eprintln!` for any module that `use`s it.
macro_rules! eprintln_crlf {
    () => {
        $crate::crlf_writer::write_stderr_line(::core::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::crlf_writer::write_stderr_line(::core::format_args!($($arg)*))
    };
}

// `println!` shadow. Routes to the locked stdout writer that translates
// `\n` to `\r\n` on Windows.
macro_rules! println_crlf {
    () => {
        $crate::crlf_writer::write_stdout_line(::core::format_args!(""))
    };
    ($($arg:tt)*) => {
        $crate::crlf_writer::write_stdout_line(::core::format_args!($($arg)*))
    };
}

// Re-export the macros under the `eprintln` / `println` names so callers
// can `use crate::crlf_writer::{eprintln, println};` and have the shadows
// take precedence over the prelude.
pub(crate) use eprintln_crlf as eprintln;
pub(crate) use println_crlf as println;

#[cfg(all(test, windows))]
mod tests {
    use super::translate_lf_to_crlf;

    #[test]
    fn translates_bare_lf() {
        assert_eq!(translate_lf_to_crlf("a\nb"), b"a\r\nb");
    }

    #[test]
    fn preserves_existing_crlf() {
        assert_eq!(translate_lf_to_crlf("a\r\nb"), b"a\r\nb");
    }

    #[test]
    fn handles_multiple_lf() {
        assert_eq!(translate_lf_to_crlf("a\nb\nc"), b"a\r\nb\r\nc");
    }

    #[test]
    fn handles_lone_cr() {
        // A bare `\r` (not followed by `\n`) is left alone; only orphan
        // `\n` is translated.
        assert_eq!(translate_lf_to_crlf("a\rb"), b"a\rb");
    }

    #[test]
    fn handles_empty_input() {
        assert_eq!(translate_lf_to_crlf(""), b"");
    }

    #[test]
    fn idempotent_when_run_twice() {
        let once = translate_lf_to_crlf("a\nb\r\nc\nd");
        let once_str = std::str::from_utf8(&once).unwrap();
        let twice = translate_lf_to_crlf(once_str);
        assert_eq!(once, twice);
    }
}
