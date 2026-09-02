//! Session manager for daemon discovery and creation.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use agent_rdp_daemon::{cleanup_session, get_pid_path, get_session_dir, get_socket_path};
use agent_rdp_protocol::{Request, ResponseData};
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

/// How long a health-check ping may take before the daemon counts as
/// unresponsive. A daemon in the middle of synchronous frame-processor work
/// can legitimately go quiet for a few seconds; 5s was tight enough to be
/// tripped by that alone.
const PING_TIMEOUT_MS: u64 = 10_000;

/// Lines of daemon.log quoted in the unresponsive message.
const LOG_TAIL_LINES: usize = 15;

/// Message for a daemon that is alive but did not answer the health check.
///
/// The distinction from "not running" matters because the right reaction is
/// the opposite: reconnecting tears down a session that is most likely just
/// busy. The log tail is included so the next bug report carries evidence of
/// what the daemon was doing instead of only the symptom.
pub fn daemon_unresponsive_message(session: &str, pid: Option<u32>) -> String {
    let log_path = daemon_log_path(session);
    let pid_text = pid.map(|p| format!("pid {}", p)).unwrap_or_else(|| "pid unknown".to_string());
    let tail = read_log_tail(&log_path, LOG_TAIL_LINES);
    let tail_text = if tail.is_empty() {
        format!("({} is empty or unreadable)", log_path.display())
    } else {
        format!("Last lines of {}:\n{}", log_path.display(), tail)
    };
    format!(
        "The daemon for this session ({}) is running and accepted the connection, but did \
         not answer a health check within {}s. It is most likely busy with a long \
         operation (an `automate run --wait`, a file transfer, or a wedged remote drive \
         read). Wait a few seconds and retry this command - do NOT reconnect, that \
         discards a working session. If retries keep failing for more than a minute, \
         `agent-rdp connect ...` replaces the daemon (the stuck one is killed first). {}",
        pid_text,
        PING_TIMEOUT_MS / 1000,
        tail_text
    )
}

/// This CLI's version, compared against what the daemon reports in `Pong`.
pub const CLI_VERSION: &str = env!("CARGO_PKG_VERSION");

/// How long a version-mismatched daemon gets to shut down gracefully before
/// it is killed. The daemon joins its frame processor on shutdown (bounded
/// at 2s there), so this comfortably covers the graceful path.
const SHUTDOWN_TIMEOUT_MS: u64 = 10_000;

/// How long to wait for a shut-down daemon's pid to disappear.
const EXIT_WAIT: Duration = Duration::from_secs(5);

/// Message for a daemon started by a different agent-rdp version.
///
/// This is the mechanism behind "upgraded, but the old bug still reproduces":
/// the daemon outlives the upgrade, and every command - including `connect`,
/// which redeploys the automation scripts *the daemon* embeds - keeps running
/// the old code. Say so explicitly, and name the two ways out.
pub fn daemon_version_mismatch_message(session: &str, pid: u32, daemon_version: &str) -> String {
    let daemon_text = if daemon_version.is_empty() {
        "an older agent-rdp (it predates version reporting)".to_string()
    } else {
        format!("agent-rdp {}", daemon_version)
    };
    format!(
        "The daemon for session '{}' (pid {}) is {} but this CLI is {}: the daemon kept \
         running across an upgrade and is still serving the old code, including the \
         automation agent it embeds. Run `agent-rdp connect ...` again - it replaces the \
         daemon and redeploys the automation agent. `agent-rdp disconnect` stops the old \
         daemon without reconnecting.",
        session, pid, daemon_text, CLI_VERSION
    )
}

/// Last `lines` lines of a log file, or an empty string if it cannot be read.
fn read_log_tail(path: &std::path::Path, lines: usize) -> String {
    let Ok(content) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let all: Vec<&str> = content.lines().collect();
    let start = all.len().saturating_sub(lines);
    all[start..].join("\n")
}

/// Why a command could not get a usable daemon connection.
#[derive(Debug)]
pub enum DaemonUnavailable {
    /// No daemon process: pid file missing, the process gone, or the socket
    /// refused. Reconnecting is the fix.
    NotRunning(String),
    /// The process is alive and accepted the socket, but a ping went
    /// unanswered. Waiting and retrying is the fix; reconnecting is not.
    Unresponsive(String),
    /// The process answered, but was built from a different agent-rdp
    /// version than this CLI. `connect` replaces it; everything else refuses.
    VersionMismatch(String),
}

impl DaemonUnavailable {
    /// Error code string for `Output::print_error`.
    pub fn code(&self) -> &'static str {
        match self {
            DaemonUnavailable::NotRunning(_) => "daemon_not_running",
            DaemonUnavailable::Unresponsive(_) => "daemon_unresponsive",
            DaemonUnavailable::VersionMismatch(_) => "daemon_version_mismatch",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            DaemonUnavailable::NotRunning(m)
            | DaemonUnavailable::Unresponsive(m)
            | DaemonUnavailable::VersionMismatch(m) => m,
        }
    }
}

/// Verdict of a health-check ping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DaemonHealth {
    /// Answered, and built from this CLI's version.
    Healthy,
    /// Answered, but from a different build. `daemon_version` is empty for a
    /// daemon that predates version reporting.
    VersionMismatch { daemon_version: String },
    /// No usable answer within the ping budget.
    Unresponsive,
}

/// Classify a daemon's reported version against this CLI's.
///
/// Exact string equality on purpose: there is no compatibility window to
/// reason about, because the daemon *is* the same binary as the CLI - any
/// difference means the user is not running what they just installed.
pub fn classify_daemon_version(daemon_version: &str, cli_version: &str) -> DaemonHealth {
    if daemon_version == cli_version {
        DaemonHealth::Healthy
    } else {
        DaemonHealth::VersionMismatch {
            daemon_version: daemon_version.to_string(),
        }
    }
}

impl std::fmt::Display for DaemonUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for DaemonUnavailable {}

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
        self.alive_pid().is_some()
    }

    /// Read-only view of the pid file: `(pid, process exists)`. Unlike
    /// `alive_pid`, never cleans anything up - for `diagnose`, which must
    /// leave the session directory exactly as it found it.
    pub fn daemon_status(&self) -> Option<(u32, bool)> {
        let pid: u32 = std::fs::read_to_string(self.pid_path()).ok()?.trim().parse().ok()?;
        Some((pid, Self::process_exists(pid)))
    }

    /// The daemon's pid if its pid file is valid and the process exists.
    /// Cleans up the session files when either check fails.
    fn alive_pid(&self) -> Option<u32> {
        let pid_path = self.pid_path();

        if !pid_path.exists() {
            return None;
        }

        // Read PID from file
        let pid: u32 = match std::fs::read_to_string(&pid_path) {
            Ok(content) => match content.trim().parse() {
                Ok(p) => p,
                Err(_) => {
                    self.cleanup_stale_session();
                    return None;
                }
            },
            Err(_) => {
                self.cleanup_stale_session();
                return None;
            }
        };

        // Check if process exists
        if Self::process_exists(pid) {
            Some(pid)
        } else {
            self.cleanup_stale_session();
            None
        }
    }

    /// Forcibly terminate a daemon process. Used only when the daemon is
    /// alive but unresponsive and `connect` is about to replace it;
    /// otherwise the stuck process would linger, still holding its RDP
    /// session, next to the fresh one.
    #[cfg(unix)]
    fn kill_process(pid: u32) {
        unsafe {
            libc::kill(pid as i32, libc::SIGKILL);
        }
    }

    #[cfg(windows)]
    fn kill_process(pid: u32) {
        use std::ptr;
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};

        unsafe {
            let handle = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if handle != ptr::null_mut() {
                TerminateProcess(handle, 1);
                CloseHandle(handle);
            }
        }
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
        if let Some(pid) = self.alive_pid() {
            debug!("Daemon already running, connecting...");
            match self.connect_to_daemon().await {
                Ok(mut client) => match Self::check_daemon_health(&mut client).await {
                    DaemonHealth::Healthy => return Ok(client),
                    DaemonHealth::VersionMismatch { daemon_version } => {
                        // `connect` is about to build a new RDP session
                        // anyway, so this is the one place replacing the
                        // daemon costs nothing extra. Ask it to stop first:
                        // a graceful shutdown joins its frame processor and
                        // removes its own files.
                        warn!(
                            "Daemon (pid {}) is agent-rdp {}, this CLI is {} - replacing it",
                            pid,
                            if daemon_version.is_empty() { "<unversioned>" } else { &daemon_version },
                            CLI_VERSION
                        );
                        self.stop_daemon(client, pid).await;
                    }
                    DaemonHealth::Unresponsive => {
                        warn!(
                            "Daemon (pid {}) not responsive, killing it and starting a fresh one...",
                            pid
                        );
                        drop(client);
                        // Deleting its pid/socket files alone left the stuck
                        // process running - with its RDP session - beside the
                        // replacement, and the two fought over the remote desktop.
                        Self::kill_process(pid);
                        sleep(Duration::from_millis(200)).await;
                    }
                },
                Err(e) => {
                    warn!("Failed to connect to daemon: {}", e);
                }
            }
            // Daemon exists but is unusable, clean up
            self.cleanup_stale_session();
        }

        // Start the daemon
        info!("Starting daemon for session '{}'", self.session);
        self.start_daemon()?;

        // Wait for daemon to be ready
        self.wait_for_daemon().await
    }

    /// Connect to a daemon that must already exist - never spawn one.
    ///
    /// Every command except `connect` goes through here. The previous flow
    /// (an `is_daemon_alive` check followed by `ensure_daemon`) had a gap: a
    /// daemon dying between the two silently spawned a fresh, connectionless
    /// daemon, and the command then failed with a misleading `NotConnected`
    /// instead of the daemon_not_running message that points at daemon.log.
    ///
    /// Four verdicts, because they call for different reactions: no process
    /// or a refused socket is `NotRunning` (reconnect); a process that
    /// accepted the socket but did not answer a ping is `Unresponsive` (wait
    /// and retry); one that answered from a different build is
    /// `VersionMismatch` (run `connect`, which replaces it). Collapsing the
    /// second into the first - as this once did - sent callers into a
    /// reconnect loop every time the daemon was merely busy; not checking the
    /// third let an upgraded CLI keep driving the old daemon indefinitely.
    pub async fn connect_existing(&self) -> Result<IpcClient, DaemonUnavailable> {
        self.connect_existing_impl(true).await
    }

    /// Like `connect_existing`, but accepts a daemon from any version. Only
    /// for commands that stop the daemon (`disconnect`): the user must always
    /// be able to get rid of a mismatched daemon without host credentials.
    pub async fn connect_existing_any_version(&self) -> Result<IpcClient, DaemonUnavailable> {
        self.connect_existing_impl(false).await
    }

    async fn connect_existing_impl(&self, require_version: bool) -> Result<IpcClient, DaemonUnavailable> {
        let Some(pid) = self.alive_pid() else {
            return Err(DaemonUnavailable::NotRunning(daemon_not_running_message(&self.session)));
        };

        let mut client = self
            .connect_to_daemon()
            .await
            .map_err(|_| DaemonUnavailable::NotRunning(daemon_not_running_message(&self.session)))?;

        match Self::check_daemon_health(&mut client).await {
            DaemonHealth::Healthy => Ok(client),
            DaemonHealth::VersionMismatch { daemon_version } => {
                if require_version {
                    Err(DaemonUnavailable::VersionMismatch(daemon_version_mismatch_message(
                        &self.session,
                        pid,
                        &daemon_version,
                    )))
                } else {
                    Ok(client)
                }
            }
            DaemonHealth::Unresponsive => Err(DaemonUnavailable::Unresponsive(
                daemon_unresponsive_message(&self.session, Some(pid)),
            )),
        }
    }

    /// Ping the daemon over the connection that will actually be used - not
    /// a throwaway second connection, which both doubled the per-command
    /// connect cost and proved nothing about the client being handed back -
    /// and compare the version it reports with this CLI's.
    async fn check_daemon_health(client: &mut IpcClient) -> DaemonHealth {
        match client.send(&Request::Ping, PING_TIMEOUT_MS).await {
            Ok(response) if response.success => {
                let daemon_version = match response.data {
                    Some(ResponseData::Pong { version }) => version,
                    _ => String::new(),
                };
                classify_daemon_version(&daemon_version, CLI_VERSION)
            }
            _ => DaemonHealth::Unresponsive,
        }
    }

    /// Stop a daemon we can talk to, then make sure it is gone.
    ///
    /// Graceful first (`Shutdown` lets it join its frame processor and remove
    /// its own files), then wait for the pid to disappear, then kill. The
    /// kill fallback matters: a daemon wedged mid-shutdown would otherwise
    /// keep its RDP session open next to the replacement.
    async fn stop_daemon(&self, mut client: IpcClient, pid: u32) {
        let _ = client.send(&Request::Shutdown, SHUTDOWN_TIMEOUT_MS).await;
        drop(client);

        let deadline = std::time::Instant::now() + EXIT_WAIT;
        while std::time::Instant::now() < deadline {
            if !Self::process_exists(pid) {
                return;
            }
            sleep(Duration::from_millis(100)).await;
        }

        warn!("Daemon (pid {}) did not exit after Shutdown, killing it", pid);
        Self::kill_process(pid);
        sleep(Duration::from_millis(200)).await;
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

#[cfg(test)]
mod version_tests {
    use super::*;

    #[test]
    fn same_version_is_healthy() {
        assert_eq!(classify_daemon_version("0.8.0", "0.8.0"), DaemonHealth::Healthy);
    }

    #[test]
    fn older_daemon_is_a_mismatch() {
        assert_eq!(
            classify_daemon_version("0.7.6", "0.8.0"),
            DaemonHealth::VersionMismatch { daemon_version: "0.7.6".into() }
        );
    }

    #[test]
    fn newer_daemon_is_also_a_mismatch() {
        // A downgraded CLI is just as wrong about what code is running.
        assert!(matches!(
            classify_daemon_version("0.9.0", "0.8.0"),
            DaemonHealth::VersionMismatch { .. }
        ));
    }

    #[test]
    fn unversioned_pong_is_a_mismatch() {
        // A daemon predating the field replies `{"type":"pong"}`, which
        // deserializes to an empty version - and is by definition older.
        assert_eq!(
            classify_daemon_version("", "0.8.0"),
            DaemonHealth::VersionMismatch { daemon_version: String::new() }
        );
    }

    #[test]
    fn legacy_pong_without_version_deserializes() {
        let response: agent_rdp_protocol::Response =
            serde_json::from_str(r#"{"success":true,"data":{"type":"pong"}}"#).unwrap();
        assert!(matches!(
            response.data,
            Some(ResponseData::Pong { ref version }) if version.is_empty()
        ));
    }

    #[test]
    fn mismatch_message_names_both_versions_and_the_fix() {
        let msg = daemon_version_mismatch_message("default", 42, "0.7.6");
        assert!(msg.contains("0.7.6"));
        assert!(msg.contains(CLI_VERSION));
        assert!(msg.contains("pid 42"));
        assert!(msg.contains("agent-rdp connect"));
        assert!(msg.contains("agent-rdp disconnect"));

        let legacy = daemon_version_mismatch_message("default", 42, "");
        assert!(legacy.contains("predates version reporting"));
    }
}
