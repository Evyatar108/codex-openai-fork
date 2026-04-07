use std::fs;
use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::discovery;

/// Return (and create) the per-session state directory.
fn state_dir() -> anyhow::Result<PathBuf> {
    let base = if cfg!(windows) {
        PathBuf::from(
            std::env::var("TEMP")
                .unwrap_or_else(|_| std::env::temp_dir().to_string_lossy().into_owned()),
        )
    } else {
        std::env::temp_dir()
    };
    let dir = base.join("codex-sandbox");
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Check if the given port is already listening by attempting a TCP connection.
fn is_port_listening(port: u16) -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

/// Wait for a TCP port to accept connections, with timeout.
fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    while Instant::now() < deadline {
        if TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    false
}

/// Print the last N lines of a log file to stderr.
fn print_log_tail(log_path: &Path, n: usize) {
    if let Ok(content) = fs::read_to_string(log_path) {
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(n);
        for line in &lines[start..] {
            eprintln!("  {line}");
        }
    }
}

/// Health check: GET /v1/models and verify we get a 200 response.
/// Returns Ok(()) on success, Err with details on failure.
fn health_check(port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("connection failed: {e}"))?;

    // Send raw HTTP/1.1 GET request
    use std::io::{Read, Write};
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(stream, "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")?;

    // Read response (just the status line is enough)
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).unwrap_or(0);
    let response = String::from_utf8_lossy(&buf[..n]);

    if response.starts_with("HTTP/1.1 200") || response.starts_with("HTTP/1.0 200") {
        Ok(())
    } else {
        let status_line = response.lines().next().unwrap_or("(empty response)");
        anyhow::bail!("unhealthy response: {status_line}")
    }
}

/// Check copilot-api health after codex-core exits with an error.
/// If copilot-api is down, prints the log and returns an error message.
/// Call this from main.rs on Windows after codex-core exits non-zero.
pub fn check_health_or_print_log(port: u16) {
    if is_port_listening(port) {
        if let Err(e) = health_check(port) {
            eprintln!("[sandbox] copilot-api health check failed: {e}");
            if let Ok(dir) = state_dir() {
                eprintln!("[sandbox] copilot-api log:");
                print_log_tail(&dir.join("copilot-api.log"), 20);
            }
        }
    } else {
        eprintln!("[sandbox] copilot-api is not running (port {} closed)", port);
        if let Ok(dir) = state_dir() {
            eprintln!("[sandbox] copilot-api log:");
            print_log_tail(&dir.join("copilot-api.log"), 20);
        }
    }
}

/// Ensure copilot-api is running on the given port.
/// If already listening, returns immediately. Otherwise starts it and waits.
pub fn ensure_running(port: u16) -> anyhow::Result<()> {
    // Quick check: if port is already listening, copilot-api is running
    if is_port_listening(port) {
        return Ok(());
    }

    let dir = state_dir()?;
    let log_path = dir.join("copilot-api.log");

    // Find copilot-api (source or binary)
    let location = discovery::find_copilot_api()?;

    // Build the executable path and args for copilot-api
    let (exe_path, exe_args): (String, Vec<String>) = match &location {
        discovery::CopilotApiLocation::Source(source_path) => {
            let bun = discovery::find_bun()?;
            (
                bun.to_string_lossy().into_owned(),
                vec![
                    "run".to_string(),
                    source_path.to_string_lossy().into_owned(),
                    "start".to_string(),
                    "--port".to_string(),
                    port.to_string(),
                ],
            )
        }
        discovery::CopilotApiLocation::Binary(bin_path) => (
            bin_path.to_string_lossy().into_owned(),
            vec!["start".to_string(), "--port".to_string(), port.to_string()],
        ),
    };

    // Spawn copilot-api as a fully detached background process.
    //
    // On Windows, Rust's Command::spawn() always sets bInheritHandles=TRUE
    // in CreateProcessW, which means the child inherits the parent's
    // stdout/stderr pipe handles. If the parent was launched by a tool that
    // captures output (e.g. Claude Code's Bash tool), the tool waits for
    // ALL holders of those pipes to close — including copilot-api. This
    // causes the wrapper to appear to hang even after codex-core exits.
    //
    // Fix: On Windows, use `cmd /c start /b` which spawns a truly independent
    // process that does not inherit the parent's handles. Output is redirected
    // to a log file via shell redirection.
    //
    // On Unix, use pre_exec with setsid() to create a new session.
    #[cfg(windows)]
    {
        let log_path_str = log_path.to_string_lossy();
        // Build: cmd /c start /b "" <exe> <args> > <log> 2>&1
        // The empty "" is the window title (required by start when exe is quoted)
        let mut shell_cmd = format!("\"{}\"", exe_path);
        for arg in &exe_args {
            shell_cmd.push_str(&format!(" \"{}\"", arg));
        }
        shell_cmd.push_str(&format!(" > \"{}\" 2>&1", log_path_str));

        let child = Command::new("cmd")
            .args(["/c", "start", "/b", "", "cmd", "/c", &shell_cmd])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to start copilot-api via cmd: {e}\nCommand: {exe_path}"
                )
            })?;
        // cmd /c exits immediately after launching; forget the handle
        std::mem::forget(child);
    }

    #[cfg(unix)]
    {
        let log_file = fs::File::create(&log_path)?;
        let log_file_err = log_file.try_clone()?;
        let mut cmd = Command::new(&exe_path);
        cmd.args(&exe_args);
        cmd.stdin(std::process::Stdio::null());
        cmd.stdout(log_file);
        cmd.stderr(log_file_err);

        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid() is async-signal-safe and has no preconditions
            // that could be violated in a pre_exec context.
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }

        let child = cmd.spawn().map_err(|e| {
            anyhow::anyhow!(
                "failed to start copilot-api: {e}\nLocation: {exe_path}"
            )
        })?;

        let pid_file = dir.join("copilot-api.pid");
        let _ = fs::write(&pid_file, child.id().to_string());
        std::mem::forget(child);
    }

    // Wait for port to be ready
    if !wait_for_port(port, Duration::from_secs(10)) {
        eprintln!("[sandbox] copilot-api failed to start. Log:");
        print_log_tail(&log_path, 10);

        // Provide helpful error message based on source type
        match &location {
            discovery::CopilotApiLocation::Source(p) => {
                anyhow::bail!(
                    "copilot-api did not start in time. Have you logged in?\n  \
                     Run: bun run {} start",
                    p.display()
                );
            }
            discovery::CopilotApiLocation::Binary(_) => {
                anyhow::bail!(
                    "copilot-api did not start in time. Check the log above for details."
                );
            }
        }
    }

    // Health check: verify copilot-api actually responds, not just port open
    if let Err(e) = health_check(port) {
        eprintln!("[sandbox] copilot-api health check failed: {e}");
        eprintln!("[sandbox] copilot-api log:");
        print_log_tail(&log_path, 20);
        anyhow::bail!(
            "copilot-api started but is not healthy. Check the log above.\n  \
             Common causes: expired token, network issues.\n  \
             Re-login: bun run <copilot-api-source>/src/main.ts start"
        );
    }

    // Show login info from log if available
    if let Ok(content) = fs::read_to_string(&log_path) {
        let reader = BufReader::new(content.as_bytes());
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("Logged in as") {
                eprintln!("[sandbox] {}", line.trim());
            }
        }
    }

    Ok(())
}
