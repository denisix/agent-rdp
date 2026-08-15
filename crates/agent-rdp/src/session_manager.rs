//! Session manager for daemon discovery and creation.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use agent_rdp_daemon::{cleanup_session, get_pid_path, get_session_dir, get_socket_path};
use agent_rdp_protocol::Request;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use crate::ipc_client::IpcClient;

/// Path to the daemon's log file for a session.
///
/// The daemon runs detached, so this is the only place its output - including
/// a panic backtrace - is recorded.
pub fn daemon_log_path(session: &str) -> PathBuf {
    get_session_dir(session).join("daemon.log")
}

/// Message for when no daemon is running.
///
/// A session can vanish mid-run if the daemon exits unexpectedly, so point at
/// both the fix (reconnect) and the evidence (the daemon log) rather than just
/// stating the fact.
pub fn daemon_not_running_message(session: &str) -> String {
    format!(
        "No daemon running for this session. Reconnect with `agent-rdp connect ...`. \
         If the session was previously connected, the daemon exited unexpectedly - \
         see {} for why.",
        daemon_log_path(session).display()
    )
}

/// Session manager handles daemon lifecycle.
pub struct SessionManager {
    session: String,
}

impl SessionManager {
    /// Create a new session manager.
    pub fn new(session: String) -> Self {
        Self { session }
    }

    /// Get the session directory path.
    #[allow(dead_code)]
    pub fn session_dir(&self) -> PathBuf {
        get_session_dir(&self.session)
    }

    /// Get the socket path for this session.
    pub fn socket_path(&self) -> PathBuf {
        get_socket_path(&self.session)
    }

    /// Get the PID file path for this session.
    pub fn pid_path(&self) -> PathBuf {
        get_pid_path(&self.session)
    }

    /// Check if the daemon is running.
    pub fn is_daemon_alive(&self) -> bool {
        let pid_path = self.pid_path();

        if !pid_path.exists() {
            return false;
        }

        // Read PID from file
        let pid: u32 = match std::fs::read_to_string(&pid_path) {
            Ok(content) => match content.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    self.cleanup_stale_session();
                    return false;
                }
            },
            Err(_) => {
                self.cleanup_stale_session();
                return false;
            }
        };

        // Check if process exists
        let alive = Self::process_exists(pid);

        if !alive {
            self.cleanup_stale_session();
        }

        alive
    }

    /// Check if a process exists.
    #[cfg(unix)]
    fn process_exists(pid: u32) -> bool {
        // kill(pid, 0) checks if process exists without sending a signal
        unsafe { libc::kill(pid as i32, 0) == 0 }
    }

    #[cfg(windows)]
    fn process_exists(pid: u32) -> bool {
        use std::ptr;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle != ptr::null_mut() {
                CloseHandle(handle);
                true
            } else {
                false
            }
        }
    }

    /// Clean up stale session directory.
    fn cleanup_stale_session(&self) {
        cleanup_session(&self.session);
    }

    /// Ensure the daemon is running, starting it if necessary.
    pub async fn ensure_daemon(&self) -> anyhow::Result<IpcClient> {
        // Check if already running
        if self.is_daemon_alive() {
            debug!("Daemon already running, connecting...");
            match self.connect_to_daemon().await {
                Ok(client) => {
                    // Verify daemon is responsive with a ping
                    if self.verify_daemon_health(&client).await {
                        return Ok(client);
                    }
                    warn!("Daemon not responsive, cleaning up and restarting...");
                    drop(client);
                }
                Err(e) => {
                    warn!("Failed to connect to daemon: {}", e);
                }
            }
            // Daemon exists but not responsive, clean up
            self.cleanup_stale_session();
        }

        // Start the daemon
        info!("Starting daemon for session '{}'", self.session);
        self.start_daemon()?;

        // Wait for daemon to be ready
        self.wait_for_daemon().await
    }

    /// Verify daemon is responsive by sending a ping.
    async fn verify_daemon_health(&self, _client: &IpcClient) -> bool {
        // Create a temporary mutable client for the ping
        let socket_path = self.socket_path();
        match IpcClient::connect(&socket_path).await {
            Ok(mut ping_client) => {
                match ping_client.send(&Request::Ping, 5000).await {
                    Ok(response) => response.success,
                    Err(_) => false,
                }
            }
            Err(_) => false,
        }
    }

    /// Start the daemon process.
    fn start_daemon(&self) -> anyhow::Result<()> {
        // Get path to current executable (the daemon is the same binary with a subcommand)
        let exe = std::env::current_exe()?;

        // Capture the daemon's output. It runs detached, so discarding stderr
        // means a panic leaves no trace at all and an unexpected exit is
        // undiagnosable after the fact. Appending keeps history across restarts.
        let log_path = crate::session_manager::daemon_log_path(&self.session);
        if let Some(parent) = log_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        // Rotate rather than append, so the log can't grow without bound while
        // still keeping the previous session's output across one reconnect -
        // which is precisely the case worth reading, since a reconnect is what
        // you do right after a session dies unexpectedly.
        if log_path.exists() {
            let _ = std::fs::rename(&log_path, log_path.with_extension("log.prev"));
        }
        let open_log = || {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)
        };

        // Fork daemon process
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;

            // Create a detached daemon process
            let mut cmd = Command::new(&exe);
            cmd.arg("--session")
                .arg(&self.session)
                .arg("session")
                .arg("daemon") // Internal command to run as daemon
                .stdin(Stdio::null())
                .stdout(open_log().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
                .stderr(open_log().map(Stdio::from).unwrap_or_else(|_| Stdio::null()));

            // Detach from parent process group
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }

            cmd.spawn()?;
        }

        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;

            const DETACHED_PROCESS: u32 = 0x00000008;
            const CREATE_NO_WINDOW: u32 = 0x08000000;

            Command::new(&exe)
                .arg("--session")
                .arg(&self.session)
                .arg("session")
                .arg("daemon")
                .stdin(Stdio::null())
                .stdout(open_log().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
                .stderr(open_log().map(Stdio::from).unwrap_or_else(|_| Stdio::null()))
                .creation_flags(DETACHED_PROCESS | CREATE_NO_WINDOW)
                .spawn()?;
        }

        Ok(())
    }

    /// Wait for the daemon to become ready.
    async fn wait_for_daemon(&self) -> anyhow::Result<IpcClient> {
        let socket_path = self.socket_path();
        let max_retries = 600; // 60 seconds total
        let retry_delay = Duration::from_millis(100);

        for _ in 0..max_retries {
            // On Windows, we use TCP ports so socket_path.exists() doesn't apply.
            // On Unix, we check if the socket file exists before trying to connect.
            #[cfg(unix)]
            let should_try = socket_path.exists();
            #[cfg(windows)]
            let should_try = true;

            if should_try {
                match IpcClient::connect(&socket_path).await {
                    Ok(client) => {
                        debug!("Connected to daemon");
                        return Ok(client);
                    }
                    Err(_) => {
                        // Connection failed, retry
                    }
                }
            }
            sleep(retry_delay).await;
        }

        anyhow::bail!("Daemon failed to start within timeout")
    }

    /// Connect to an existing daemon.
    async fn connect_to_daemon(&self) -> anyhow::Result<IpcClient> {
        let socket_path = self.socket_path();
        IpcClient::connect(&socket_path)
            .await
            .map_err(|e| anyhow::anyhow!("Failed to connect to daemon: {}", e))
    }

    /// List all active sessions.
    pub fn list_sessions() -> Vec<String> {
        let base_dir = agent_rdp_daemon::get_base_dir();
        let mut sessions = Vec::new();

        if let Ok(entries) = std::fs::read_dir(&base_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    if let Some(name) = entry.file_name().to_str() {
                        // Verify this session has a PID file (works on both Unix and Windows)
                        let pid_path = entry.path().join("pid");
                        if pid_path.exists() {
                            sessions.push(name.to_string());
                        }
                    }
                }
            }
        }

        sessions
    }
}
