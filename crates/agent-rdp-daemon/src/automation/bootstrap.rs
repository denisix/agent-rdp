//! Bootstrap automation agent on remote Windows machine.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_rdp_protocol::DriveMapping;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tracing::{debug, info, warn};

use super::{new_shared_dvc_state, AutomationState, DvcIpc, SharedDvcState};
use crate::rdp_session::RdpSession;

/// Launch attempts before `launch_and_wait` gives up.
pub const LAUNCH_ATTEMPTS: usize = 3;

/// A script read off the mapped drive this recently means PowerShell is
/// still loading the agent.
const SCRIPT_ACTIVITY_WINDOW: Duration = Duration::from_secs(30);

/// Worst-case duration of `launch_and_wait`: fixed launch waits plus every
/// attempt's handshake window, each extended once. Exposed so the CLI's
/// connect/restart budgets can be checked against it.
pub fn launch_and_wait_worst_case() -> Duration {
    let mut total = Duration::ZERO;
    for attempt in 1..=LAUNCH_ATTEMPTS {
        total += LAUNCH_FIXED_WAITS + AutomationBootstrap::handshake_window(attempt) * 2;
    }
    total
}

/// Sleeps inside `launch_agent` (desktop settle, Run dialog, paste).
const LAUNCH_FIXED_WAITS: Duration = Duration::from_millis(2000 + 300 + 2000 + 500);

/// Relaunch budget for the supervisor: a bounded number of relaunches per
/// window, so a remote host that kills the agent on every start does not
/// keep the Run dialog busy forever.
#[derive(Debug)]
pub struct RelaunchBudget {
    window: Duration,
    max_in_window: usize,
    launches: Vec<std::time::Instant>,
}

impl RelaunchBudget {
    pub fn new(window: Duration, max_in_window: usize) -> Self {
        Self { window, max_in_window, launches: Vec::new() }
    }

    /// Whether another relaunch is allowed at `now`; records it if so.
    pub fn try_take(&mut self, now: std::time::Instant) -> bool {
        self.launches.retain(|t| now.duration_since(*t) < self.window);
        if self.launches.len() >= self.max_in_window {
            return false;
        }
        self.launches.push(now);
        true
    }
}

/// Relaunch the agent on an already-initialized session: `automate restart`
/// and the supervisor both come here. Serialized via `relaunch_in_flight`
/// so two callers cannot drive the Run dialog at once.
pub async fn relaunch_agent(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &Arc<Mutex<AutomationState>>,
) -> Result<(), String> {
    {
        let mut state = automation_state.lock().await;
        if !state.enabled {
            return Err("automation is not enabled for this session".to_string());
        }
        if state.relaunch_in_flight {
            return Err("a relaunch of the automation agent is already in progress".to_string());
        }
        state.relaunch_in_flight = true;
        state.agent_ready = false;
        state.agent_pid = None;
    }

    let bootstrap = AutomationBootstrap::new(crate::get_session_dir(""));
    let result = bootstrap.launch_and_wait(rdp_session, automation_state).await;

    let mut state = automation_state.lock().await;
    state.relaunch_in_flight = false;
    result
}

/// Watch one session's DVC channel and relaunch the agent when the channel
/// closes while the RDP session is still alive.
///
/// Scoped to a session: `rx` ends when `cleanup()` drops the DVC state, and
/// a changed `session_generation` means a newer `connect` owns everything
/// now. IronRDP also fires the close callback on an ordinary disconnect, so
/// both checks matter - a supervisor that outlived its session would relaunch
/// the agent into the next one.
pub fn spawn_relaunch_supervisor(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    rdp_session: Arc<Mutex<Option<RdpSession>>>,
    automation_state: Arc<Mutex<AutomationState>>,
    session_generation: Arc<std::sync::atomic::AtomicU64>,
    generation: u64,
) {
    const SETTLE: Duration = Duration::from_secs(5);
    tokio::spawn(async move {
        let mut budget = RelaunchBudget::new(Duration::from_secs(600), 3);
        while rx.recv().await.is_some() {
            // Drain bursts: a close can be reported more than once.
            while rx.try_recv().is_ok() {}

            if session_generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
                info!("Relaunch supervisor: session replaced; exiting");
                return;
            }
            sleep(SETTLE).await;
            if rdp_session.lock().await.is_none() {
                info!("Relaunch supervisor: RDP session is gone; nothing to relaunch");
                continue;
            }
            {
                let state = automation_state.lock().await;
                if !state.enabled || state.relaunch_in_flight {
                    continue;
                }
                if state.dvc_state.as_ref().map(|s| s.lock().handshake.is_some()).unwrap_or(false) {
                    // Already back (a manual `automate restart` beat us).
                    continue;
                }
            }
            if !budget.try_take(std::time::Instant::now()) {
                warn!("Relaunch supervisor: relaunch budget exhausted (3 per 10 minutes); leaving the agent down");
                continue;
            }
            info!("Relaunch supervisor: DVC channel closed while the RDP session is alive; relaunching the agent");
            match relaunch_agent(&rdp_session, &automation_state).await {
                Ok(()) => {
                    let mut state = automation_state.lock().await;
                    state.relaunches += 1;
                    info!("Relaunch supervisor: agent is back (relaunch #{})", state.relaunches);
                }
                Err(e) => warn!("Relaunch supervisor: relaunch failed: {}", e),
            }
        }
        debug!("Relaunch supervisor: DVC state dropped; exiting");
    });
}

/// Embedded PowerShell agent script (main entry point).
const AGENT_SCRIPT: &str = include_str!("scripts/agent.ps1");

/// Embedded PowerShell library files.
const LIB_TYPES: &str = include_str!("scripts/lib/types.ps1");
const LIB_SNAPSHOT: &str = include_str!("scripts/lib/snapshot.ps1");
const LIB_SELECTORS: &str = include_str!("scripts/lib/selectors.ps1");
const LIB_ACTIONS: &str = include_str!("scripts/lib/actions.ps1");
const LIB_DVC: &str = include_str!("scripts/lib/dvc.ps1");

/// Automation bootstrap handler.
pub struct AutomationBootstrap {
    /// Session directory path (kept for potential future use).
    _session_dir: PathBuf,
}

impl AutomationBootstrap {
    /// Create a new automation bootstrap handler.
    pub fn new(session_dir: PathBuf) -> Self {
        Self { _session_dir: session_dir }
    }

    /// Initialize automation directory and write the agent script.
    ///
    /// This creates the directory structure and scripts needed for RDPDR-based
    /// bootstrap. The actual IPC will be over DVC once the agent starts.
    pub async fn initialize(&self, state: &mut AutomationState) -> anyhow::Result<()> {
        info!("Initializing automation for session");

        // Create automation directory structure
        let automation_dir = &state.automation_dir;
        tokio::fs::create_dir_all(automation_dir).await?;
        tokio::fs::create_dir_all(automation_dir.join("scripts")).await?;
        tokio::fs::create_dir_all(automation_dir.join("scripts/lib")).await?;

        // Write the PowerShell agent script (main entry point)
        let script_path = state.script_path();
        tokio::fs::write(&script_path, AGENT_SCRIPT).await?;
        debug!("Wrote automation agent script to {:?}", script_path);

        // Write the PowerShell library files
        let lib_dir = automation_dir.join("scripts/lib");
        tokio::fs::write(lib_dir.join("types.ps1"), LIB_TYPES).await?;
        tokio::fs::write(lib_dir.join("snapshot.ps1"), LIB_SNAPSHOT).await?;
        tokio::fs::write(lib_dir.join("selectors.ps1"), LIB_SELECTORS).await?;
        tokio::fs::write(lib_dir.join("actions.ps1"), LIB_ACTIONS).await?;
        tokio::fs::write(lib_dir.join("dvc.ps1"), LIB_DVC).await?;
        debug!("Wrote automation library files to {:?}", lib_dir);

        // Initialize DVC state and IPC. The close-notification channel is
        // created here so its sender lives and dies with this session's DVC
        // state: `cleanup()` drops the state, the sender goes with it, and
        // the relaunch supervisor (spawned by `connect` from `closed_rx`)
        // sees the channel end and exits.
        let dvc_state = new_shared_dvc_state();
        let (closed_tx, closed_rx) = tokio::sync::mpsc::unbounded_channel();
        dvc_state.lock().closed_notify = Some(closed_tx);
        let dvc_ipc = DvcIpc::new(dvc_state.clone());
        state.dvc_state = Some(dvc_state);
        state.dvc_ipc = Some(dvc_ipc);
        state.closed_rx = Some(closed_rx);
        state.relaunch_in_flight = false;
        state.relaunches = 0;

        state.enabled = true;
        info!(
            "Automation initialized with ID {} at {:?}",
            state.automation_id, automation_dir
        );

        Ok(())
    }

    /// Get the drive mapping for the automation directory.
    pub fn get_drive_mapping(&self, state: &AutomationState) -> DriveMapping {
        DriveMapping {
            path: state.automation_dir.to_string_lossy().to_string(),
            name: state.drive_name.clone(),
        }
    }

    /// Launch the automation agent on the remote Windows machine via Win+R.
    pub async fn launch_agent(
        &self,
        rdp: &RdpSession,
        state: &AutomationState,
    ) -> anyhow::Result<()> {
        info!("Launching automation agent on remote Windows machine");

        // Wait for desktop to stabilize after RDP connection
        debug!("Waiting for remote desktop to stabilize...");
        sleep(Duration::from_secs(2)).await;

        // The command to run via Win+R
        // Uses the mapped drive path: \\TSCLIENT\<drive_name>\scripts\agent.ps1
        let ps_command = format!(
            "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File \"\\\\TSCLIENT\\{}\\scripts\\agent.ps1\" -BasePath \"\\\\TSCLIENT\\{}\"",
            state.drive_name,
            state.drive_name
        );

        debug!("PowerShell command: {}", ps_command);

        // Set the command in clipboard first (paste is more reliable than typing
        // long commands character-by-character into the Run dialog)
        rdp.clipboard_set(ps_command).await
            .map_err(|e| anyhow::anyhow!("Failed to set clipboard: {}", e))?;
        sleep(Duration::from_millis(300)).await;

        // Press Win+R to open Run dialog
        rdp.send_key_press("super+r").await?;

        // Wait long enough for the Run dialog to appear and grab focus.
        // 500ms is insufficient when foreground apps (Steam, browsers, etc.)
        // are active — the dialog can take 1-2s to steal focus.
        sleep(Duration::from_millis(2000)).await;

        // Paste the command from clipboard (avoids character-by-character timing issues)
        rdp.send_key_press("ctrl+v").await?;
        sleep(Duration::from_millis(500)).await;

        // Press Enter to execute
        rdp.send_key_press("return").await?;

        info!("Automation agent launch command sent");
        Ok(())
    }

    /// Wait for the agent to complete its DVC handshake, polling the shared
    /// DVC state directly. Takes the state `Arc`, not the `AutomationState`
    /// lock: the previous version held `automation_state` for the whole
    /// window, so every `connect`/`disconnect`/`automate` queued behind a
    /// bootstrap for its full duration.
    ///
    /// The window is per attempt: PowerShell start-up on a CPU-starved host
    /// can take well over the old 25s, and three short windows burned the
    /// whole budget without ever giving one launch enough time.
    pub async fn wait_for_handshake(
        &self,
        dvc_state: &SharedDvcState,
        window: Duration,
        rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut delay = Duration::from_millis(500);
        let max_delay = Duration::from_secs(5);
        let mut extended = false;

        loop {
            if dvc_state.lock().handshake.is_some() {
                return Ok(());
            }

            let elapsed = started.elapsed();
            if elapsed >= window {
                // Still starting? A channel open without a handshake, or a
                // script read off the mapped drive in the last 30s, means
                // PowerShell is loading - launching a second copy now would
                // only give us two agents. Extend once.
                if !extended && Self::agent_is_starting(dvc_state, rdp_session).await {
                    info!(
                        "Agent is still starting after {:?}; extending the handshake window",
                        window
                    );
                    extended = true;
                    continue;
                }
                if elapsed >= window * 2 {
                    break;
                }
                if !extended {
                    break;
                }
            }

            sleep(delay).await;
            delay = (delay * 3 / 2).min(max_delay);
        }

        anyhow::bail!(
            "Automation agent DVC handshake timed out after {:?}",
            started.elapsed()
        )
    }

    /// Signals that an agent process is starting but has not handshaken.
    async fn agent_is_starting(
        dvc_state: &SharedDvcState,
        rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    ) -> bool {
        if dvc_state.lock().is_launching() {
            return true;
        }
        let session = rdp_session.lock().await;
        match session.as_ref().and_then(|rdp| rdp.last_script_open_age()) {
            Some(age) => age < SCRIPT_ACTIVITY_WINDOW,
            None => false,
        }
    }

    /// Handshake window for launch attempt `attempt` (1-based).
    pub fn handshake_window(attempt: usize) -> Duration {
        match attempt {
            1 => Duration::from_secs(25),
            2 => Duration::from_secs(45),
            _ => Duration::from_secs(75),
        }
    }

    /// Record a completed handshake in the automation state.
    fn record_ready(state: &mut AutomationState) {
        if let Some(ipc) = state.dvc_ipc.as_ref() {
            let version = ipc.agent_version().unwrap_or_default();
            let pid = ipc.agent_pid().unwrap_or(0);
            let caps = ipc.capabilities();
            state.agent_ready = true;
            state.agent_pid = Some(pid);
            info!(
                "Automation agent ready via DVC: PID={}, version={}, capabilities={:?}",
                pid, version, caps
            );
        }
    }

    /// Launch the agent and wait for its handshake, retrying the launch
    /// itself if it doesn't take.
    ///
    /// Shared by `connect` (after `initialize()` has already mapped the
    /// drive and written the scripts), `automate restart`, and the relaunch
    /// supervisor. The launch drives the remote desktop's Run dialog, so it
    /// silently does nothing if the desktop isn't ready to accept input yet
    /// - the symptom is a launch that reports success followed by a
    /// handshake timeout, hence the retry rather than one long wait. Locks
    /// are held only around the launch keystrokes (~5s), never across the
    /// handshake wait.
    pub async fn launch_and_wait(
        &self,
        rdp_session: &Arc<Mutex<Option<RdpSession>>>,
        automation_state: &Arc<Mutex<AutomationState>>,
    ) -> Result<(), String> {
        let mut last_reason = String::new();

        let dvc_state = match automation_state.lock().await.dvc_state.clone() {
            Some(state) => state,
            None => return Err("Automation DVC state not initialized".to_string()),
        };

        for attempt in 1..=LAUNCH_ATTEMPTS {
            // Attempt 2+: if the previous launch is visibly still starting,
            // wait for it instead of firing Win+R again.
            let launched = if attempt > 1 && Self::agent_is_starting(&dvc_state, rdp_session).await {
                info!("Previous agent launch is still starting; waiting instead of relaunching");
                true
            } else {
                let session = rdp_session.lock().await;
                match session.as_ref() {
                    Some(rdp) => {
                        let auto_state = automation_state.lock().await;
                        match self.launch_agent(rdp, &auto_state).await {
                            Ok(()) => true,
                            Err(e) => {
                                warn!("Failed to launch automation agent: {}", e);
                                last_reason = format!("Failed to launch automation agent: {}", e);
                                false
                            }
                        }
                    }
                    None => {
                        last_reason = "Not connected to an RDP server".to_string();
                        false
                    }
                }
            };

            if launched {
                let window = Self::handshake_window(attempt);
                match self.wait_for_handshake(&dvc_state, window, rdp_session).await {
                    Ok(()) => {
                        let mut auto_state = automation_state.lock().await;
                        Self::record_ready(&mut auto_state);
                        return Ok(());
                    }
                    Err(e) => {
                        warn!(
                            "Automation agent handshake failed (attempt {}/{}): {}",
                            attempt, LAUNCH_ATTEMPTS, e
                        );
                        last_reason = format!("Automation agent handshake failed: {}", e);
                    }
                }
            }

            if attempt < LAUNCH_ATTEMPTS {
                info!("Retrying automation agent launch...");
            }
        }

        warn!("Automation agent did not come up after {} attempts", LAUNCH_ATTEMPTS);
        Err(last_reason)
    }

    /// Clean up automation resources.
    pub async fn cleanup(&self, state: &mut AutomationState) -> anyhow::Result<()> {
        if !state.enabled {
            return Ok(());
        }

        info!("Cleaning up automation resources");

        // Remove automation directory
        let automation_dir = &state.automation_dir;
        if automation_dir.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(automation_dir).await {
                warn!("Failed to remove automation directory: {}", e);
            } else {
                debug!("Removed automation directory: {:?}", automation_dir);
            }
        }

        state.enabled = false;
        state.agent_ready = false;
        state.agent_pid = None;
        state.dvc_ipc = None;
        state.dvc_state = None;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// The `run` prelude is PowerShell source for the *child* process and
    /// must reach it verbatim. Written in a double-quoted string it was
    /// expanded inside the agent instead, and every `automate run` began
    /// with `Continue='SilentlyContinue';` - a CommandNotFoundException.
    /// PSScriptAnalyzer cannot catch this (both spellings are valid
    /// PowerShell), so pin the literal here.
    #[test]
    fn run_prelude_is_a_single_quoted_literal() {
        // Single-quoted here-strings: nothing may be expanded in the agent.
        assert!(LIB_ACTIONS.contains("$script:ChildPrelude = @'"));
        assert!(LIB_ACTIONS.contains("$script:ChildWrapperTail = @'"));
        assert!(
            !LIB_ACTIONS.contains("\"$ProgressPreference="),
            "the child prelude must not interpolate $ProgressPreference"
        );
        // Encoding first and guarded, then the preferences: with `Stop`
        // already set, a console-less child died on the encoding setter.
        let prelude_at = LIB_ACTIONS.find("[Text.UTF8Encoding]::new($false)").unwrap();
        let stop_at = LIB_ACTIONS.find("$ErrorActionPreference = 'Stop'").unwrap();
        assert!(prelude_at < stop_at, "console encoding must be set before Stop");
        assert!(LIB_ACTIONS.contains("$ProgressPreference = 'SilentlyContinue'"));
    }

    /// The child script wrapper: exception chain as plain text on stderr,
    /// native exit codes preserved, agent pid exposed, and `--wait` winning
    /// over `--stream`.
    #[test]
    fn child_script_wrapper_reports_errors_and_exit_codes() {
        assert!(LIB_ACTIONS.contains("if ($LASTEXITCODE) { exit $LASTEXITCODE }"));
        assert!(LIB_ACTIONS.contains("'  caused by ' + $agentRdpInner.GetType().FullName"));
        assert!(LIB_ACTIONS.contains("$agentRdpErr.ScriptStackTrace"));
        assert!(LIB_ACTIONS.contains("[Console]::Error.WriteLine"));
        assert!(LIB_ACTIONS.contains("$env:AGENT_RDP_AGENT_PID = '''"));
        assert!(LIB_ACTIONS.contains("function Test-ChildScriptWrappable"));
        assert!(LIB_ACTIONS.contains("$ast.ParamBlock"));
        assert!(LIB_ACTIONS.contains("if ($stream -and -not $wait)"));
        assert!(LIB_ACTIONS.contains("early_exit = $true"));

        // The wrapper travels inside -EncodedCommand toward the ~32KB
        // command-line limit; keep it small.
        let tail_start = LIB_ACTIONS.find("$script:ChildWrapperTail = @'").unwrap();
        let tail_end = LIB_ACTIONS[tail_start..].find("\n'@").unwrap();
        assert!(tail_end < 4096, "child wrapper tail is {} bytes", tail_end);
    }

    /// The agent must ride out transient DVC read errors and exit only on
    /// an explicitly fatal one - it used to exit on any non-109 error,
    /// which a CPU-starved host produces routinely.
    #[test]
    fn agent_survives_transient_dvc_errors() {
        assert!(LIB_DVC.contains("$script:DvcFatalWin32Errors = @("));
        for code in ["6,", "109,", "232,", "233,", "1167"] {
            assert!(LIB_DVC.contains(code), "fatal list must contain {code}");
        }
        assert!(LIB_DVC.contains("$script:DvcFatalPrefix = \"DVC_FATAL:\""));
        assert!(LIB_DVC.contains("function Skip-DvcTransientFailure"));
        assert!(LIB_DVC.contains("$script:ChannelFlagFirst = 0x00000001"));
        // Write failures are fatal too, with the same prefix.
        assert!(LIB_DVC.contains("$($script:DvcFatalPrefix) WriteFile failed"));
        // The main loop keys on the prefix, not on substrings.
        assert!(AGENT_SCRIPT.contains("$errorMsg.StartsWith($script:DvcFatalPrefix)"));
        assert!(!AGENT_SCRIPT.contains("$errorMsg -match \"Win32 error\""));
    }

    #[test]
    fn relaunch_budget_is_bounded_per_window() {
        use std::time::{Duration, Instant};
        let mut budget = RelaunchBudget::new(Duration::from_secs(600), 3);
        let t0 = Instant::now();
        assert!(budget.try_take(t0));
        assert!(budget.try_take(t0 + Duration::from_secs(10)));
        assert!(budget.try_take(t0 + Duration::from_secs(20)));
        assert!(!budget.try_take(t0 + Duration::from_secs(30)), "fourth within the window is refused");
        assert!(budget.try_take(t0 + Duration::from_secs(601)), "the window slides");
    }

    #[test]
    fn handshake_windows_grow_and_the_worst_case_is_bounded() {
        assert!(AutomationBootstrap::handshake_window(1) < AutomationBootstrap::handshake_window(2));
        assert!(AutomationBootstrap::handshake_window(2) < AutomationBootstrap::handshake_window(3));
        let worst = launch_and_wait_worst_case();
        assert!(worst.as_secs() > 250 && worst.as_secs() < 330, "{:?}", worst);
    }

    /// `run` reports when the child started, by the remote clock.
    #[test]
    fn run_reports_started_unix() {
        assert!(LIB_ACTIONS.contains("function Get-UnixNow"));
        assert!(LIB_ACTIONS.contains("started_unix = $startedUnix"));
    }

    /// `file_stat` reports both timestamps from the remote clock, from an
    /// explicit UTC epoch (a parsed '1970-01-01Z' is Local-kind).
    #[test]
    fn file_stat_reports_remote_times() {
        assert!(LIB_ACTIONS.contains("modified_unix = [int64]($item.LastWriteTimeUtc - $epoch)"));
        assert!(LIB_ACTIONS.contains("now_unix = [int64]([datetime]::UtcNow - $epoch)"));
        assert!(LIB_ACTIONS.contains("[System.DateTimeKind]::Utc"));
    }

    /// A retried request id must be answered from the journal, not
    /// executed again - and only when it is the same command.
    #[test]
    fn dispatch_replays_journaled_results_by_id() {
        assert!(LIB_ACTIONS.contains("function Get-JournalEntry"));
        assert!(LIB_ACTIONS.contains("function Get-RequestFingerprint"));
        assert!(LIB_ACTIONS.contains("fingerprint = $Fingerprint"));
        assert!(AGENT_SCRIPT.contains("Replaying journaled result for request"));
        assert!(AGENT_SCRIPT.contains("idempotency_key_reused"));
        assert!(
            AGENT_SCRIPT.contains("-Fingerprint $fingerprint"),
            "every journaled result must carry the request fingerprint"
        );
    }

    /// `Read-DvcMessage` has to reassemble CHANNEL_PDU_HEADER fragments; the
    /// single-read version silently dropped every request over ~1.6KB.
    #[test]
    fn dvc_reader_reassembles_fragments() {
        assert!(LIB_DVC.contains("ChannelFlagLast"));
        assert!(LIB_DVC.contains("MemoryStream"));
    }

    #[tokio::test]
    async fn test_initialize_creates_structure() {
        let temp_dir = TempDir::new().unwrap();
        let bootstrap = AutomationBootstrap::new(temp_dir.path().to_path_buf());
        let mut state = AutomationState::new(temp_dir.path().to_path_buf());

        bootstrap.initialize(&mut state).await.unwrap();

        assert!(state.enabled);
        assert!(state.automation_dir.exists());
        assert!(state.script_path().exists());

        // Verify library files are created
        let lib_dir = state.automation_dir.join("scripts/lib");
        assert!(lib_dir.join("types.ps1").exists());
        assert!(lib_dir.join("snapshot.ps1").exists());
        assert!(lib_dir.join("selectors.ps1").exists());
        assert!(lib_dir.join("actions.ps1").exists());
        assert!(lib_dir.join("dvc.ps1").exists());

        // Verify DVC IPC is initialized
        assert!(state.dvc_ipc.is_some());
        assert!(state.dvc_state.is_some());
    }

    #[test]
    fn test_get_drive_mapping() {
        let temp_dir = TempDir::new().unwrap();
        let bootstrap = AutomationBootstrap::new(temp_dir.path().to_path_buf());
        let state = AutomationState::new(temp_dir.path().to_path_buf());

        let mapping = bootstrap.get_drive_mapping(&state);

        assert_eq!(mapping.name, "agent-automation");
        assert!(mapping.path.contains("automation-"));
    }
}
