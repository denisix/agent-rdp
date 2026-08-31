//! Daemon process for agent-rdp RDP session management.
//!
//! This crate implements the background daemon that maintains RDP connections
//! and handles commands from CLI instances via IPC.

pub mod automation;
pub mod daemon;
pub mod handlers;
pub mod ipc_server;
pub mod keymap;
pub mod ocr;
pub mod rdp_session;
pub mod rdpdr;
pub mod ws_input;
pub mod ws_server;

pub use daemon::{Daemon, SharedWsHandle};
pub use ipc_server::IpcServer;
pub use rdp_session::RdpSession;

/// Get the base directory for all agent-rdp sessions.
pub fn get_base_dir() -> std::path::PathBuf {
    #[cfg(unix)]
    {
        std::path::PathBuf::from("/tmp/agent-rdp")
    }
    #[cfg(windows)]
    {
        let temp = std::env::var("TEMP")
            .or_else(|_| std::env::var("TMP"))
            .unwrap_or_else(|_| "C:\\Windows\\Temp".to_string());
        std::path::PathBuf::from(format!("{}\\agent-rdp", temp))
    }
}

/// Get the session directory path.
pub fn get_session_dir(session: &str) -> std::path::PathBuf {
    get_base_dir().join(session)
}

/// Get the socket path for a session.
pub fn get_socket_path(session: &str) -> std::path::PathBuf {
    #[cfg(unix)]
    {
        get_session_dir(session).join("socket")
    }
    #[cfg(windows)]
    {
        // On Windows, we use a named pipe path
        std::path::PathBuf::from(format!("\\\\.\\pipe\\agent-rdp-{}", session))
    }
}

/// Get the PID file path for a session.
pub fn get_pid_path(session: &str) -> std::path::PathBuf {
    get_session_dir(session).join("pid")
}

/// Get the TCP port for a session (Windows fallback).
/// Uses a deterministic hash of the session name to derive a port in the range 49152-65535.
pub fn get_session_port(session: &str) -> u16 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    session.hash(&mut hasher);
    let hash = hasher.finish();
    // Map to ephemeral port range: 49152-65535 (16384 ports)
    49152 + (hash % 16384) as u16
}

/// Clean up a session's transient state.
///
/// Deliberately removes individual entries rather than the whole directory:
/// `daemon.log` lives here and is the only record of why a session ended. This
/// runs from `is_daemon_alive()` whenever a dead PID is found, i.e. on the exact
/// path that then tells the user to go read that log - wiping the directory
/// destroyed the evidence before it could be read.
pub fn cleanup_session(session: &str) {
    let dir = get_session_dir(session);

    for entry in ["socket", "pid"] {
        let _ = std::fs::remove_file(dir.join(entry));
    }

    // Anything else transient (automation scratch dirs and the like) can go, but
    // keep the logs.
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name == "daemon.log" || name == "daemon.log.prev" {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                let _ = std::fs::remove_dir_all(&path);
            } else {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

/// Run the daemon server for the given session.
/// This is the main entry point called by `agent-rdp session daemon`.
pub async fn run_server(session: &str) -> anyhow::Result<()> {
    use std::io::Write;

    // Create session directory. It lives under a world-traversable /tmp and
    // holds the IPC socket, which grants full control of the session, so it is
    // restricted to the owner rather than left to the umask.
    let session_dir = get_session_dir(session);
    std::fs::create_dir_all(&session_dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))?;
    }

    // Write PID file
    let pid_path = get_pid_path(session);
    let mut pid_file = std::fs::File::create(&pid_path)?;
    writeln!(pid_file, "{}", std::process::id())?;
    drop(pid_file);

    // Create and run daemon
    let mut daemon = Daemon::new(session.to_string()).await?;
    let result = daemon.run().await;

    // Cleanup on exit
    cleanup_session(session);

    result
}
