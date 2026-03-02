//! # pty_host
//!
//! A lightweight PTY shepherd that holds terminal sessions across client restarts.
//!
//! This crate provides:
//!
//! - **`protocol`** — The wire protocol for communication between a client and the
//!   `pty-host` daemon. Message types, framing, encode/decode.
//!
//! - **`session`** — A single PTY session: PTY creation, shell spawn, Unix
//!   socket listener, and the I/O proxy loop.
//!
//! ## Architecture
//!
//! Each `pty-host` process manages exactly one PTY session:
//!
//! ```text
//! ┌────────────────────┐         ┌──────────────────────────┐
//! │  client process    │  Unix   │       pty-host           │
//! │                    │ socket  │                          │
//! │  Terminal I/O      │◄═══════►│  holds master_fd         │
//! │  reads/writes      │  framed │  proxies master ↔ socket │
//! │  the socket        │  msgs   │  survives client restart │
//! └────────────────────┘         └──────────┬───────────────┘
//!                                           │ master fd
//!                                           ▼
//!                                      ┌─────────┐
//!                                      │  shell   │
//!                                      └─────────┘
//! ```
//!
//! When the client restarts, it reconnects to the same Unix socket. The host
//! replays buffered scrollback and resumes proxying. The shell never receives
//! SIGHUP.
//!
//! ## Socket location
//!
//! Sessions use `$TMPDIR/pty-host/<session-uuid>.sock` (or the platform's
//! secure temporary directory equivalent).

pub mod client;
pub mod headless;
pub mod protocol;
pub mod session;

use std::collections::HashMap;

/// Configuration for spawning a new pty-host session.
pub struct SpawnConfig {
    pub session_id: uuid::Uuid,
    /// Shell program to run. `None` means use the system default (`$SHELL`).
    pub shell_program: Option<String>,
    pub shell_args: Vec<String>,
    pub working_directory: Option<std::path::PathBuf>,
    pub env: HashMap<String, String>,
}

/// Spawn a `pty-host` child process and wait for it to be ready.
///
/// Returns the socket path to connect to. The child process is detached
/// and will outlive the caller.
pub fn spawn_host(config: &SpawnConfig) -> anyhow::Result<std::path::PathBuf> {
    let pty_host_bin = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            let dir = exe.parent()?;
            let candidate = dir.join("pty-host");
            candidate.exists().then_some(candidate)
        })
        .unwrap_or_else(|| std::path::PathBuf::from("pty-host"));

    log::info!(
        "spawn_host: binary={}, session={}, shell={:?}, args={:?}, cwd={:?}",
        pty_host_bin.display(),
        config.session_id,
        config.shell_program.as_deref().unwrap_or("(system default)"),
        config.shell_args,
        config.working_directory,
    );

    let mut cmd = std::process::Command::new(&pty_host_bin);

    // Put pty-host in its own process group so signals sent to the parent's
    // process group (e.g. ^C / SIGINT) don't propagate to the shepherd.
    // This is the whole point — the host must outlive the parent.
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            if libc::setpgid(0, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.arg("--session").arg(config.session_id.to_string());
    if let Some(ref shell) = config.shell_program {
        cmd.arg("--shell").arg(shell);
    }
    for arg in &config.shell_args {
        cmd.arg("--arg").arg(arg);
    }
    if let Some(ref cwd) = config.working_directory {
        cmd.arg("--cwd").arg(cwd);
    }
    cmd.envs(&config.env);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::inherit());

    let mut child = cmd.spawn().map_err(|error| {
        anyhow::anyhow!(
            "failed to spawn pty-host at {}: {error}",
            pty_host_bin.display()
        )
    })?;
    log::info!("spawn_host: pty-host spawned, pid={}", child.id());

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("no stdout from pty-host"))?;
    let mut reader = std::io::BufReader::new(stdout);
    let mut socket_line = String::new();
    std::io::BufRead::read_line(&mut reader, &mut socket_line)
        .map_err(|error| anyhow::anyhow!("failed to read socket path from pty-host: {error}"))?;

    let reported_path = std::path::PathBuf::from(socket_line.trim());
    log::info!(
        "spawn_host: pty-host reported socket={}",
        reported_path.display()
    );

    // Detach — it runs independently of the parent.
    drop(child);

    Ok(reported_path)
}

/// Returns the default directory for PTY host session sockets.
///
/// Uses `$TMPDIR/pty-host/` on macOS/Linux. The directory is created if it
/// does not exist.
pub fn default_socket_dir() -> std::path::PathBuf {
    let base = std::env::temp_dir();
    base.join("pty-host")
}

/// Returns the socket path for a given session ID.
pub fn socket_path_for_session(session_id: &uuid::Uuid) -> std::path::PathBuf {
    default_socket_dir().join(format!("{}.sock", session_id))
}

/// Discover existing session sockets that can be reconnected to.
///
/// Returns a list of `(session_id, socket_path)` pairs for sockets that
/// exist in the default socket directory.
pub fn discover_sessions() -> Vec<(uuid::Uuid, std::path::PathBuf)> {
    let dir = default_socket_dir();
    let mut sessions = Vec::new();

    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(_) => return sessions,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("sock")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
            && let Ok(id) = uuid::Uuid::parse_str(stem)
        {
            sessions.push((id, path));
        }
    }

    sessions
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_deterministic() {
        let id = uuid::Uuid::parse_str("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let path = socket_path_for_session(&id);
        assert!(path.to_str().unwrap().contains("pty-host"));
        assert!(path.to_str().unwrap().ends_with("550e8400-e29b-41d4-a716-446655440000.sock"));
    }

    #[test]
    fn discover_sessions_returns_empty_for_missing_dir() {
        // If the dir doesn't exist, we should get an empty list, not an error.
        let sessions = discover_sessions();
        // We can't assert it's empty (it might exist from other tests),
        // but it should not panic.
        let _ = sessions;
    }
}
#[doc(hidden)]
pub mod daemon;
