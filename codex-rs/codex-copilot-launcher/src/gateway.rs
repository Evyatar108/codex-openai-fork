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
    let dir = base.join("codex-copilot");
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

/// Health check: GET /healthz and verify we get a 200 response.
/// Returns Ok(()) on success, Err with details on failure.
fn health_check(port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = ([127, 0, 0, 1], port).into();
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(5))
        .map_err(|e| anyhow::anyhow!("connection failed: {e}"))?;

    use std::io::{Read, Write};
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    write!(
        stream,
        "GET /healthz HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;

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

/// Check gateway health after codex-core exits with an error.
/// If the gateway is down, prints the log and returns an error message.
/// Call this from main.rs on Windows after codex-core exits non-zero.
pub fn check_health_or_print_log(port: u16) {
    if is_port_listening(port) {
        if let Err(e) = health_check(port) {
            eprintln!("[codex-copilot] codex-copilot-gateway health check failed: {e}");
            if let Ok(dir) = state_dir() {
                eprintln!("[codex-copilot] codex-copilot-gateway log:");
                print_log_tail(&dir.join("codex-copilot-gateway.log"), 20);
            }
        }
    } else {
        eprintln!(
            "[codex-copilot] codex-copilot-gateway is not running (port {} closed)",
            port
        );
        if let Ok(dir) = state_dir() {
            eprintln!("[codex-copilot] codex-copilot-gateway log:");
            print_log_tail(&dir.join("codex-copilot-gateway.log"), 20);
        }
    }
}

/// Ensure codex-copilot-gateway is running on the given port.
/// If already listening, returns immediately. Otherwise starts it and waits.
pub fn ensure_running(port: u16) -> anyhow::Result<()> {
    if is_port_listening(port) {
        return Ok(());
    }

    let dir = state_dir()?;
    let log_path = dir.join("codex-copilot-gateway.log");

    let gateway = discovery::find_codex_copilot_gateway()?;
    let exe_path = gateway.to_string_lossy().into_owned();
    let exe_args = vec![
        "start".to_string(),
        "--port".to_string(),
        port.to_string(),
    ];

    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x00000200;

        let log_file = fs::File::create(&log_path)?;
        let log_file_err = log_file.try_clone()?;

        let child = Command::new(&exe_path)
            .args(&exe_args)
            .stdin(std::process::Stdio::null())
            .stdout(log_file)
            .stderr(log_file_err)
            .creation_flags(CREATE_NEW_PROCESS_GROUP)
            .spawn()
            .map_err(|e| {
                anyhow::anyhow!("failed to start codex-copilot-gateway: {e}\nCommand: {exe_path}")
            })?;

        let pid_file = dir.join("codex-copilot-gateway.pid");
        let _ = fs::write(&pid_file, child.id().to_string());
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
                "failed to start codex-copilot-gateway: {e}\nPath: {exe_path}"
            )
        })?;

        let pid_file = dir.join("codex-copilot-gateway.pid");
        let _ = fs::write(&pid_file, child.id().to_string());
        std::mem::forget(child);
    }

    if !wait_for_port(port, Duration::from_secs(10)) {
        eprintln!("[codex-copilot] codex-copilot-gateway failed to start. Log:");
        print_log_tail(&log_path, 10);
        anyhow::bail!(
            "codex-copilot-gateway did not start in time. Have you logged in?\n  \
             Run: codex-copilot-gateway login"
        );
    }

    if let Err(e) = health_check(port) {
        eprintln!("[codex-copilot] codex-copilot-gateway health check failed: {e}");
        eprintln!("[codex-copilot] codex-copilot-gateway log:");
        print_log_tail(&log_path, 20);
        anyhow::bail!(
            "codex-copilot-gateway started but is not healthy. Check the log above.\n  \
             Common causes: expired token, network issues.\n  \
             Re-login: codex-copilot-gateway login"
        );
    }

    if let Ok(content) = fs::read_to_string(&log_path) {
        let reader = BufReader::new(content.as_bytes());
        for line in reader.lines().map_while(Result::ok) {
            if line.contains("Logged in as") {
                eprintln!("[codex-copilot] {}", line.trim());
            }
        }
    }

    Ok(())
}
