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

    // Build the command based on source vs binary
    let mut cmd = match &location {
        discovery::CopilotApiLocation::Source(source_path) => {
            let bun = discovery::find_bun()?;
            let mut c = Command::new(&bun);
            c.args([
                "run",
                &source_path.to_string_lossy(),
                "start",
                "--port",
                &port.to_string(),
            ]);
            c
        }
        discovery::CopilotApiLocation::Binary(bin_path) => {
            let mut c = Command::new(bin_path);
            c.args(["start", "--port", &port.to_string()]);
            c
        }
    };

    // Redirect stdout/stderr to log file
    let log_file = fs::File::create(&log_path)?;
    let log_file_err = log_file.try_clone()?;
    cmd.stdout(log_file);
    cmd.stderr(log_file_err);

    // Platform-specific detached process flags
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        const DETACHED_PROCESS: u32 = 0x00000008;
        cmd.creation_flags(CREATE_NO_WINDOW | DETACHED_PROCESS);
    }

    #[cfg(unix)]
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
            "failed to start copilot-api: {e}\nLocation: {}",
            match &location {
                discovery::CopilotApiLocation::Source(p) => p.display().to_string(),
                discovery::CopilotApiLocation::Binary(p) => p.display().to_string(),
            }
        )
    })?;

    // Write PID file
    let pid_file = dir.join("copilot-api.pid");
    let _ = fs::write(&pid_file, child.id().to_string());

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
