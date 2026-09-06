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

/// How long `connect` waits for an agent that outlived the previous
/// transport drop to re-open its channel, before typing Win+R for a new one.
///
/// The agent retries its channel open every 3s, so this covers two attempts.
/// Paid on every connect, including cold ones where no agent exists: 6s
/// against the ~30s a launch costs, in exchange for not taking foreground on
/// a desktop that may be shared. Deliberately not conditioned on this daemon
/// having launched an agent before - a daemon replaced by `connect --replace`
/// or by an upgrade starts with no memory at all, and that is exactly when a
/// survivor is waiting.
pub const SURVIVOR_WAIT: Duration = Duration::from_secs(6);

/// How often the survivor wait re-checks for a handshake.
const SURVIVOR_POLL: Duration = Duration::from_millis(250);

/// Worst case for `connect`'s automation bootstrap: the survivor wait plus a
/// full launch sequence. Distinct from `launch_and_wait_worst_case`, which
/// stays the bound for `automate restart` (it never waits for a survivor).
pub fn connect_bootstrap_worst_case() -> Duration {
    SURVIVOR_WAIT + launch_and_wait_worst_case()
}

/// The agent version this daemon ships, parsed out of the embedded script.
///
/// An adopted agent is a *previous* daemon's process, so it can predate an
/// upgrade. Comparing against the script we would deploy is what keeps a
/// stale agent from being adopted into a daemon whose protocol has moved on.
pub fn expected_agent_version() -> Option<String> {
    let marker = "$script:Version = \"";
    let start = AGENT_SCRIPT.find(marker)? + marker.len();
    let rest = &AGENT_SCRIPT[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Hash of every embedded PowerShell file this daemon deploys.
///
/// `$script:Version` names `agent.ps1` alone, so a change to a library file
/// with no version bump - this very release touched `actions.ps1` without
/// touching the version string - would let a survivor running the old
/// library be adopted as if nothing had changed. Passed to the launched
/// agent as `-BuildId` and echoed back in its handshake; `adopt_survivor`
/// compares it, not the version string, before adopting.
pub fn expected_build_id() -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for script in [AGENT_SCRIPT, LIB_TYPES, LIB_SNAPSHOT, LIB_SELECTORS, LIB_ACTIONS, LIB_DVC] {
        hasher.update(script.as_bytes());
    }
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

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

/// Outcome of the survivor wait at the head of `connect`'s bootstrap.
enum Adoption {
    /// An agent was already there and is now this daemon's agent.
    Adopted,
    /// Nothing usable was there; launch one.
    None,
}

/// Consecutive failed launches after which the supervisor stops retrying
/// on its own. `automate restart` resets the count.
pub const MAX_LAUNCH_FAILURES: u32 = 6;

/// The supervisor types into the session (Win+R, paste, Enter) to launch
/// the agent, and if the Run dialog does not come up that text lands in
/// whatever has focus. So it only does this once nobody has driven the
/// session for this long.
pub const RETRY_INPUT_QUIET: Duration = Duration::from_secs(120);

/// How often the supervisor checks whether a scheduled retry is due.
const SUPERVISOR_TICK: Duration = Duration::from_secs(30);

/// Environment variable that disables automatic relaunches entirely
/// (`automate restart` still works). Read once when the supervisor starts;
/// the daemon inherits the environment of the `connect` that spawned it.
pub const AUTO_RELAUNCH_KILL_SWITCH: &str = "AGENT_RDP_NO_AUTO_RELAUNCH";

/// Delay before the next automatic launch after `failures` consecutive
/// failed ones: 60s, 120s, 240s, then 300s.
pub fn retry_backoff(failures: u32) -> Duration {
    let doublings = failures.saturating_sub(1).min(4);
    Duration::from_secs((60u64 << doublings).min(300))
}

/// Whether automatic relaunches are disabled by the environment.
pub fn auto_relaunch_disabled() -> bool {
    std::env::var_os(AUTO_RELAUNCH_KILL_SWITCH)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

/// Record what a launch attempt did to the retry schedule. Success clears
/// everything; a failure schedules the next automatic attempt, or gives up
/// after `MAX_LAUNCH_FAILURES` with a message that says so.
pub fn record_launch_outcome(
    state: &mut AutomationState,
    result: &Result<(), String>,
    now: std::time::Instant,
) {
    match result {
        Ok(()) => {
            state.last_error = None;
            state.next_retry_at = None;
            state.launch_failures = 0;
            // Counts `connect`'s bootstrap too, which `relaunches` does not:
            // that launch types Win+R and pastes on the remote desktop, so it
            // needs to be visible somewhere. An adopted agent is the one case
            // that succeeded without typing anything, so it is not a launch.
            if !state.adopted {
                state.total_launches = state.total_launches.saturating_add(1);
            }
        }
        Err(reason) => {
            state.launch_failures = state.launch_failures.saturating_add(1);
            if state.auto_relaunch_disabled {
                state.next_retry_at = None;
                state.last_error = Some(format!(
                    "{} (automatic relaunch is disabled by {}; `automate restart` relaunches)",
                    reason, AUTO_RELAUNCH_KILL_SWITCH
                ));
            } else if state.launch_failures >= MAX_LAUNCH_FAILURES {
                state.next_retry_at = None;
                state.last_error = Some(format!(
                    "{} (gave up after {} consecutive failed launches; `automate restart` tries again)",
                    reason, state.launch_failures
                ));
            } else {
                state.next_retry_at = Some(now + retry_backoff(state.launch_failures));
                state.last_error = Some(reason.clone());
            }
        }
    }
}

/// The one launch path for an initialized session: `connect`'s bootstrap,
/// `automate restart` and the supervisor all come here. Serialized via
/// `relaunch_in_flight` so two callers cannot drive the Run dialog at once -
/// and so a supervisor tick sees `connect`'s own bootstrap as in flight.
pub async fn launch_guarded(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &Arc<Mutex<AutomationState>>,
    adopt_first: bool,
) -> Result<(), String> {
    {
        let mut state = automation_state.lock().await;
        if !state.enabled {
            return Err("automation is not enabled for this session".to_string());
        }
        if state.relaunch_in_flight {
            return Err("a launch of the automation agent is already in progress".to_string());
        }
        state.relaunch_in_flight = true;
        state.agent_ready = false;
        state.agent_pid = None;
        // Cleared up front: whatever comes out of this call, "adopted" must
        // describe the agent that ends up connected, not an earlier one.
        state.adopted = false;
    }

    let bootstrap = AutomationBootstrap::new(crate::get_session_dir(""));
    let result = bootstrap
        .launch_and_wait(rdp_session, automation_state, adopt_first)
        .await;

    let mut state = automation_state.lock().await;
    state.relaunch_in_flight = false;
    record_launch_outcome(&mut state, &result, std::time::Instant::now());
    result
}

/// Adopt an agent that survived the last transport drop, without ever
/// launching one. Returns whether an agent is now connected.
///
/// This is `connect --defer-agent`: the caller wants the session up without
/// anything appearing on the remote desktop, but a survivor costs nothing to
/// take. Guarded like a launch so it cannot race one.
pub async fn adopt_only(automation_state: &Arc<Mutex<AutomationState>>) -> bool {
    let dvc_state = {
        let mut state = automation_state.lock().await;
        if !state.enabled || state.relaunch_in_flight {
            return false;
        }
        state.relaunch_in_flight = true;
        state.adopted = false;
        state.dvc_state.clone()
    };

    let bootstrap = AutomationBootstrap::new(crate::get_session_dir(""));
    let adopted = match dvc_state {
        Some(dvc_state) => matches!(
            bootstrap.adopt_survivor(&dvc_state, automation_state).await,
            Adoption::Adopted
        ),
        None => false,
    };

    let mut state = automation_state.lock().await;
    state.relaunch_in_flight = false;
    if adopted {
        record_launch_outcome(&mut state, &Ok(()), std::time::Instant::now());
    }
    adopted
}

/// Relaunch the agent on an already-initialized session (`automate restart`
/// and the supervisor). Counts in `relaunches` on success.
///
/// Never adopts: a relaunch exists because the caller wants a *new* agent,
/// and the one still holding the channel is what they are replacing.
pub async fn relaunch_agent(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &Arc<Mutex<AutomationState>>,
) -> Result<(), String> {
    let result = launch_guarded(rdp_session, automation_state, false).await;
    if result.is_ok() {
        automation_state.lock().await.relaunches += 1;
    }
    result
}

/// Everything the supervisor's retry decision depends on, captured at one
/// instant so the decision itself is a pure function.
#[derive(Debug, Clone, Copy)]
pub struct RetrySnapshot {
    pub generation_matches: bool,
    pub session_alive: bool,
    pub enabled: bool,
    pub relaunch_in_flight: bool,
    pub handshake_done: bool,
    pub agent_starting: bool,
    pub next_retry_at: Option<std::time::Instant>,
    pub last_input_age: Option<Duration>,
    pub auto_relaunch_disabled: bool,
}

/// Whether the supervisor should launch the agent now; the reason not to,
/// otherwise. `next_retry_at` is armed only by a recorded failure (or a
/// close notification), never by an in-progress bootstrap, so a `connect`
/// still waiting for its first handshake can never be doubled.
pub fn should_retry(s: &RetrySnapshot, now: std::time::Instant) -> Result<(), &'static str> {
    if !s.generation_matches {
        return Err("session replaced");
    }
    if s.auto_relaunch_disabled {
        return Err("automatic relaunch disabled by AGENT_RDP_NO_AUTO_RELAUNCH");
    }
    if !s.session_alive {
        return Err("RDP session is gone");
    }
    if !s.enabled {
        return Err("automation not enabled");
    }
    if s.relaunch_in_flight {
        return Err("a launch is in progress");
    }
    if s.handshake_done {
        return Err("agent is up");
    }
    let Some(due) = s.next_retry_at else {
        return Err("no retry scheduled");
    };
    if now < due {
        return Err("retry not due yet");
    }
    if s.agent_starting {
        return Err("an agent is visibly still starting");
    }
    if let Some(age) = s.last_input_age {
        if age < RETRY_INPUT_QUIET {
            return Err("session input is not quiet yet");
        }
    }
    Ok(())
}

async fn retry_snapshot(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &Arc<Mutex<AutomationState>>,
    session_generation: &Arc<std::sync::atomic::AtomicU64>,
    generation: u64,
) -> RetrySnapshot {
    let generation_matches =
        session_generation.load(std::sync::atomic::Ordering::SeqCst) == generation;
    let (session_alive, last_input_age) = {
        let session = rdp_session.lock().await;
        match session.as_ref() {
            Some(rdp) => (true, rdp.last_input_age()),
            None => (false, None),
        }
    };
    let (enabled, relaunch_in_flight, handshake_done, next_retry_at, dvc_state, auto_relaunch_disabled) = {
        let state = automation_state.lock().await;
        (
            state.enabled,
            state.relaunch_in_flight,
            state
                .dvc_state
                .as_ref()
                .map(|s| s.lock().handshake.is_some())
                .unwrap_or(false),
            state.next_retry_at,
            state.dvc_state.clone(),
            state.auto_relaunch_disabled,
        )
    };
    let agent_starting = match dvc_state {
        Some(dvc_state) => AutomationBootstrap::agent_is_starting(&dvc_state, rdp_session).await,
        None => false,
    };
    RetrySnapshot {
        generation_matches,
        session_alive,
        enabled,
        relaunch_in_flight,
        handshake_done,
        agent_starting,
        next_retry_at,
        last_input_age,
        auto_relaunch_disabled,
    }
}

/// One supervised launch attempt. A budget refusal reschedules a minute
/// out without counting as a failure; a failed launch schedules its own
/// retry through `record_launch_outcome`.
async fn supervisor_attempt(
    budget: &mut RelaunchBudget,
    reason: &str,
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &Arc<Mutex<AutomationState>>,
) {
    let now = std::time::Instant::now();
    if !budget.try_take(now) {
        warn!("Relaunch supervisor: relaunch budget exhausted (3 per 10 minutes); trying again in a minute");
        automation_state.lock().await.next_retry_at = Some(now + Duration::from_secs(60));
        return;
    }
    info!("Relaunch supervisor: {}; relaunching the agent", reason);
    match relaunch_agent(rdp_session, automation_state).await {
        Ok(()) => {
            let relaunches = automation_state.lock().await.relaunches;
            info!("Relaunch supervisor: agent is back (relaunch #{})", relaunches);
        }
        Err(e) => warn!("Relaunch supervisor: relaunch failed: {}", e),
    }
}

/// Watch one session's automation agent: relaunch it when its DVC channel
/// closes while the RDP session is still alive, and keep retrying (with
/// backoff, once the session is idle) when a launch fails - including
/// `connect`'s own bootstrap, which otherwise left the agent down until
/// someone ran `automate restart`.
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
        let auto_disabled = automation_state.lock().await.auto_relaunch_disabled;
        if auto_disabled {
            info!("Relaunch supervisor: automatic relaunch disabled by {}", AUTO_RELAUNCH_KILL_SWITCH);
        }
        loop {
            let closed = match tokio::time::timeout(SUPERVISOR_TICK, rx.recv()).await {
                Ok(None) => break,
                Ok(Some(())) => {
                    // Drain bursts: a close can be reported more than once.
                    while rx.try_recv().is_ok() {}
                    true
                }
                Err(_) => false,
            };

            if session_generation.load(std::sync::atomic::Ordering::SeqCst) != generation {
                info!("Relaunch supervisor: session replaced; exiting");
                return;
            }

            if closed {
                sleep(SETTLE).await;
                // A close arms an immediate retry; the same gate as a
                // scheduled one decides whether it is safe to type now.
                let mut state = automation_state.lock().await;
                let agent_up = state
                    .dvc_state
                    .as_ref()
                    .map(|s| s.lock().handshake.is_some())
                    .unwrap_or(false);
                if state.enabled && !state.relaunch_in_flight && !agent_up {
                    if state.next_retry_at.is_none() {
                        state.next_retry_at = Some(std::time::Instant::now());
                    }
                    if state.last_error.is_none() {
                        state.last_error =
                            Some("the agent's DVC channel closed (the agent process ended)".to_string());
                    }
                    info!("Relaunch supervisor: DVC channel closed while the RDP session is alive");
                }
            }

            let snapshot =
                retry_snapshot(&rdp_session, &automation_state, &session_generation, generation).await;
            match should_retry(&snapshot, std::time::Instant::now()) {
                Ok(()) => {
                    let reason = if closed {
                        "DVC channel closed while the RDP session is alive"
                    } else {
                        "scheduled retry after a failed launch"
                    };
                    supervisor_attempt(&mut budget, reason, &rdp_session, &automation_state).await;
                }
                Err(why) => {
                    if snapshot.next_retry_at.is_some() {
                        debug!("Relaunch supervisor: not launching yet ({})", why);
                    }
                }
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
        state.last_error = None;
        state.next_retry_at = None;
        state.launch_failures = 0;
        state.auto_relaunch_disabled = auto_relaunch_disabled();

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
        // `-BuildId` is what a later connect's adoption check compares
        // against - it has to come from this daemon's own launch, not from
        // anything the agent could compute about itself.
        let ps_command = format!(
            "powershell -ExecutionPolicy Bypass -WindowStyle Hidden -File \"\\\\TSCLIENT\\{}\\scripts\\agent.ps1\" -BasePath \"\\\\TSCLIENT\\{}\" -BuildId \"{}\"",
            state.drive_name,
            state.drive_name,
            expected_build_id()
        );

        debug!("PowerShell command: {}", ps_command);

        // These keystrokes are ours, not an operator's: they must not count
        // against the supervisor's input-quiet gate, or a failed launch
        // would push its own retry out by RETRY_INPUT_QUIET every time.
        let input_mark = rdp.input_activity_mark();
        let result = self.type_launch_command(rdp, ps_command).await;
        rdp.restore_input_activity(input_mark);
        result
    }

    async fn type_launch_command(&self, rdp: &RdpSession, ps_command: String) -> anyhow::Result<()> {
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
    ///
    /// `baseline` is the handshake this launch is trying to *replace*, taken
    /// before the launch. Without it, a launch whose whole point is to get rid
    /// of a wedged agent returned immediately on that agent's own handshake:
    /// `automate restart` typed Win+R, reported success, and left the old
    /// agent in place while the new one was rejected as a second channel.
    pub async fn wait_for_handshake(
        &self,
        dvc_state: &SharedDvcState,
        window: Duration,
        rdp_session: &Arc<Mutex<Option<RdpSession>>>,
        baseline: Option<std::time::Instant>,
    ) -> anyhow::Result<()> {
        let started = std::time::Instant::now();
        let mut delay = Duration::from_millis(500);
        let max_delay = Duration::from_secs(5);
        let mut extended = false;

        loop {
            if Self::handshake_is_newer(dvc_state, baseline) {
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

        if baseline.is_some() && dvc_state.lock().handshake.is_some() {
            anyhow::bail!(
                "The previous automation agent is still holding the channel after {:?}; the \
                 new one could not take over",
                started.elapsed()
            );
        }
        anyhow::bail!(
            "Automation agent DVC handshake timed out after {:?}",
            started.elapsed()
        )
    }

    /// Whether the connected agent is one that handshook after `baseline`.
    ///
    /// With no baseline (a first launch, or an adoption) any handshake counts.
    fn handshake_is_newer(dvc_state: &SharedDvcState, baseline: Option<std::time::Instant>) -> bool {
        let state = dvc_state.lock();
        if state.handshake.is_none() {
            return false;
        }
        match (baseline, state.handshake_at) {
            (None, _) => true,
            (Some(baseline), Some(at)) => at > baseline,
            // A handshake with no timestamp cannot be shown to be the new one.
            (Some(_), None) => false,
        }
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

    /// Wait briefly for an agent that outlived the last transport drop.
    ///
    /// The agent keeps re-opening its channel for several minutes after the
    /// client goes away, so the common reconnect finds one already there.
    /// Adopting it is the difference between a reconnect nobody on the remote
    /// desktop notices and one that takes foreground to type Win+R.
    ///
    /// A survivor from before an upgrade is told to exit instead: it is a
    /// previous daemon's process, running a script this daemon no longer
    /// ships.
    async fn adopt_survivor(
        &self,
        dvc_state: &SharedDvcState,
        automation_state: &Arc<Mutex<AutomationState>>,
    ) -> Adoption {
        let deadline = std::time::Instant::now() + SURVIVOR_WAIT;
        loop {
            let handshake = dvc_state.lock().handshake.clone();
            if let Some(handshake) = handshake {
                // Compared by build id, not `$script:Version`: a library file
                // can change with no version bump (this very release touched
                // `actions.ps1`), and adopting a survivor still running the
                // old one would be the daemon/CLI version-mismatch problem
                // recreated one layer down, silently, with no version-mismatch
                // check to catch it. An agent with no build id predates the
                // field and is always treated as stale.
                let expected = expected_build_id();
                if handshake.build_id.as_deref() != Some(expected.as_str()) {
                    warn!(
                        "An agent running different scripts than this daemon ships is still \
                         running (agent version {}, build {:?}, expected build {}); asking it \
                         to exit and launching a current one",
                        handshake.version,
                        handshake.build_id,
                        expected
                    );
                    Self::shutdown_stale_agent(dvc_state, automation_state).await;
                    return Adoption::None;
                }

                let mut state = automation_state.lock().await;
                Self::record_ready(&mut state);
                state.adopted = true;
                info!(
                    "Adopted the automation agent that survived the last drop (pid {}, version {}) \
                     - no Win+R was typed on the remote desktop",
                    handshake.agent_pid, handshake.version
                );
                return Adoption::Adopted;
            }
            if std::time::Instant::now() >= deadline {
                debug!("No surviving automation agent within {:?}; launching", SURVIVOR_WAIT);
                return Adoption::None;
            }
            sleep(SURVIVOR_POLL).await;
        }
    }

    /// Ask an agent we are not adopting to exit, and wait briefly for it to
    /// go. Best-effort: if it will not leave, the launch proceeds anyway and
    /// the second agent is rejected when it opens its own channel.
    async fn shutdown_stale_agent(
        dvc_state: &SharedDvcState,
        automation_state: &Arc<Mutex<AutomationState>>,
    ) {
        let ipc = automation_state.lock().await.dvc_ipc.clone();
        if let Some(ipc) = ipc {
            let _ = ipc
                .send_request_with_timeout(
                    &agent_rdp_protocol::AutomateRequest::Shutdown,
                    Duration::from_secs(5),
                )
                .await;
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if dvc_state.lock().handshake.is_none() {
                return;
            }
            sleep(SURVIVOR_POLL).await;
        }
        warn!("The previous agent did not exit within 5s; launching anyway");
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
        adopt_first: bool,
    ) -> Result<(), String> {
        let mut last_reason = String::new();

        let dvc_state = match automation_state.lock().await.dvc_state.clone() {
            Some(state) => state,
            None => return Err("Automation DVC state not initialized".to_string()),
        };

        if adopt_first {
            match self.adopt_survivor(&dvc_state, automation_state).await {
                Adoption::Adopted => return Ok(()),
                Adoption::None => {}
            }
        }

        // Whatever handshake is on the channel right now is the one this
        // launch has to replace, not accept - `None` only when the channel
        // is genuinely empty (a cold connect, or eviction above succeeded).
        // A stale agent that `adopt_survivor`/`stop_running_agent` failed to
        // evict is exactly the case this guards: without it, the wait below
        // returned on that same stale handshake and reported success having
        // replaced nothing.
        let baseline = dvc_state.lock().handshake_at;

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
                match self.wait_for_handshake(&dvc_state, window, rdp_session, baseline).await {
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
        state.last_error = None;
        state.next_retry_at = None;
        state.launch_failures = 0;
        // The session that was adopted is over; a later `offline_status`
        // must not keep reporting it.
        state.adopted = false;

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
        assert!(LIB_ACTIONS.contains("function Get-ChildScriptShape"));
        assert!(LIB_ACTIONS.contains("$ast.ParamBlock"));
        assert!(LIB_ACTIONS.contains("if ($Stream -and -not $Wait)"));
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

    /// A `run` that does not parse as Windows PowerShell is refused before
    /// any child is launched, with the parser's positions; every run
    /// reply and every run error carries the text the agent executed.
    #[test]
    fn run_refuses_parse_errors_and_echoes_the_command_line() {
        assert!(LIB_ACTIONS.contains("function Test-DefaultShell"));
        assert!(LIB_ACTIONS.contains("\"parse_error: the command does not parse as Windows PowerShell:"));
        assert!(LIB_ACTIONS.contains("line $($_.Extent.StartLineNumber), column $($_.Extent.StartColumnNumber)"));
        // Only the default shell shares the agent's parser.
        assert!(LIB_ACTIONS.contains("-and (Test-DefaultShell -Shell $Shell)"));
        assert!(LIB_ACTIONS.contains("$result.command_line = $userScript"));
        assert!(LIB_ACTIONS.contains("`nCommand line as executed by the agent: $userScript"));
    }

    /// Waited runs and finished stream polls report when the process
    /// exited, by the remote clock: the freshness marker for their output.
    #[test]
    fn run_reports_finished_unix() {
        assert!(LIB_ACTIONS.contains("finished_unix = Get-UnixNow"));
        assert!(LIB_ACTIONS.contains("$state.ExitedUnix = Get-UnixNow"));
        assert!(LIB_ACTIONS.contains("$poll.finished_unix = $state.ExitedUnix"));
    }

    /// Keyed `run` results reach the disk journal so a retry after a
    /// reconnect or relaunch replays instead of re-executing.
    #[test]
    fn keyed_runs_are_journaled_on_disk() {
        // Location: per-account local profile, never TEMP (per-session on
        // RDS, wiped at logoff) and never the mapped drive.
        assert!(LIB_ACTIONS.contains("$root = $env:LOCALAPPDATA"));
        assert!(!LIB_ACTIONS.contains("TSCLIENT\\agent-rdp"));
        assert!(LIB_ACTIONS.contains("function Write-PersistedJournalEntry"));
        assert!(LIB_ACTIONS.contains("function Get-PersistedJournalEntry"));
        assert!(LIB_ACTIONS.contains("function Remove-ExpiredJournalEntries"));
        assert!(LIB_ACTIONS.contains("function ConvertTo-Hashtable"));
        // Write-once through File.Move; UTF-8 without BOM on both sides.
        assert!(LIB_ACTIONS.contains("[System.IO.File]::Move($tmp, $path)"));
        assert!(LIB_ACTIONS.contains("[System.IO.File]::WriteAllText($tmp, $json, [System.Text.UTF8Encoding]::new($false))"));
        assert!(LIB_ACTIONS.contains("[System.IO.File]::ReadAllText($path, [System.Text.UTF8Encoding]::new($false))"));
        assert!(LIB_ACTIONS.contains("journal_truncated"));
        // Only keyed runs persist; the dispatch loop decides.
        assert!(AGENT_SCRIPT.contains("$keyed = ($request.command -eq \"run\" -and [bool]$request.params.idempotency_key)"));
        assert!(AGENT_SCRIPT.contains("-IncludeDisk:$keyed"));
        assert!(AGENT_SCRIPT.contains("-Persist:$persist"));
        // Replays are marked with the original time and never mutate the
        // stored entry.
        assert!(AGENT_SCRIPT.contains("$replayData = $replayData.Clone()"));
        assert!(AGENT_SCRIPT.contains("$replayData[\"replayed_at_unix\"] = $replay.at_unix"));
        assert!(AGENT_SCRIPT.contains("replayed from the journal: this idempotency key already ran"));
        // The budget is not part of the key.
        assert!(LIB_ACTIONS.contains("$copy.Remove(\"timeout_ms\")"));
        assert!(LIB_ACTIONS.contains("$copy.PSObject.Properties.Remove(\"timeout_ms\")"));
        // Duplicate ids no longer inflate the FIFO.
        assert!(LIB_ACTIONS.contains("if (-not $script:ResultJournalOrder.Contains($Id))"));
        assert!(AGENT_SCRIPT.contains("\"persistent_journal\""));
        assert!(AGENT_SCRIPT.contains("$script:Version = \"1.8.0\""));
    }

    #[test]
    fn retry_backoff_grows_and_caps() {
        assert_eq!(retry_backoff(0), Duration::from_secs(60));
        assert_eq!(retry_backoff(1), Duration::from_secs(60));
        assert_eq!(retry_backoff(2), Duration::from_secs(120));
        assert_eq!(retry_backoff(3), Duration::from_secs(240));
        assert_eq!(retry_backoff(4), Duration::from_secs(300));
        assert_eq!(retry_backoff(50), Duration::from_secs(300), "no overflow, just the cap");
    }

    fn ready_snapshot(now: std::time::Instant) -> RetrySnapshot {
        RetrySnapshot {
            generation_matches: true,
            session_alive: true,
            enabled: true,
            relaunch_in_flight: false,
            handshake_done: false,
            agent_starting: false,
            next_retry_at: Some(now - Duration::from_secs(1)),
            last_input_age: Some(RETRY_INPUT_QUIET + Duration::from_secs(1)),
            auto_relaunch_disabled: false,
        }
    }

    /// The retry decision, case by case. The load-bearing ones: an
    /// in-progress bootstrap (no retry armed, or in flight) never launches
    /// a second agent, and recent operator input holds the launch back.
    #[test]
    fn should_retry_table() {
        let now = std::time::Instant::now();
        assert_eq!(should_retry(&ready_snapshot(now), now), Ok(()));

        let mut s = ready_snapshot(now);
        s.next_retry_at = None;
        assert_eq!(should_retry(&s, now), Err("no retry scheduled"), "connect's bootstrap in progress");

        let mut s = ready_snapshot(now);
        s.relaunch_in_flight = true;
        assert_eq!(should_retry(&s, now), Err("a launch is in progress"));

        let mut s = ready_snapshot(now);
        s.next_retry_at = Some(now + Duration::from_secs(30));
        assert_eq!(should_retry(&s, now), Err("retry not due yet"));

        let mut s = ready_snapshot(now);
        s.last_input_age = Some(Duration::from_secs(10));
        assert_eq!(should_retry(&s, now), Err("session input is not quiet yet"));

        let mut s = ready_snapshot(now);
        s.last_input_age = None;
        assert_eq!(should_retry(&s, now), Ok(()), "never driven is quiet");

        let mut s = ready_snapshot(now);
        s.handshake_done = true;
        assert_eq!(should_retry(&s, now), Err("agent is up"));

        let mut s = ready_snapshot(now);
        s.agent_starting = true;
        assert_eq!(should_retry(&s, now), Err("an agent is visibly still starting"));

        let mut s = ready_snapshot(now);
        s.generation_matches = false;
        assert_eq!(should_retry(&s, now), Err("session replaced"));

        let mut s = ready_snapshot(now);
        s.session_alive = false;
        assert_eq!(should_retry(&s, now), Err("RDP session is gone"));

        let mut s = ready_snapshot(now);
        s.auto_relaunch_disabled = true;
        assert!(should_retry(&s, now).is_err());
    }

    /// Failures schedule the next attempt with growing backoff and give up
    /// after the cap; one success wipes the slate.
    #[test]
    fn launch_outcome_bookkeeping() {
        let dir = TempDir::new().unwrap();
        let mut state = AutomationState::new(dir.path().to_path_buf());
        let t0 = std::time::Instant::now();

        record_launch_outcome(&mut state, &Err("handshake timed out".into()), t0);
        assert_eq!(state.launch_failures, 1);
        assert_eq!(state.next_retry_at, Some(t0 + Duration::from_secs(60)));
        assert_eq!(state.last_error.as_deref(), Some("handshake timed out"));
        assert_eq!(state.next_retry_secs(t0), Some(60));
        assert_eq!(state.next_retry_secs(t0 + Duration::from_secs(90)), Some(0), "due is 0, not negative");

        record_launch_outcome(&mut state, &Err("again".into()), t0);
        assert_eq!(state.next_retry_at, Some(t0 + Duration::from_secs(120)));

        for _ in 0..(MAX_LAUNCH_FAILURES - 2) {
            record_launch_outcome(&mut state, &Err("again".into()), t0);
        }
        assert_eq!(state.launch_failures, MAX_LAUNCH_FAILURES);
        assert_eq!(state.next_retry_at, None, "gave up");
        assert!(state.last_error.as_deref().unwrap().contains("gave up after 6"));
        assert!(state.last_error.as_deref().unwrap().contains("automate restart"));

        record_launch_outcome(&mut state, &Ok(()), t0);
        assert_eq!(state.launch_failures, 0);
        assert_eq!(state.next_retry_at, None);
        assert_eq!(state.last_error, None);
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

#[cfg(test)]
mod retry_edge_tests {
    use super::*;
    use tempfile::TempDir;

    fn state() -> AutomationState {
        AutomationState::new(TempDir::new().unwrap().path().to_path_buf())
    }

    /// With the kill switch on, a failure never promises a retry: no
    /// `next_retry_at`, `next_retry_secs` stays `None`, and the error says
    /// what to do instead.
    #[test]
    fn kill_switch_never_schedules_a_retry() {
        let mut s = state();
        s.auto_relaunch_disabled = true;
        let now = std::time::Instant::now();
        record_launch_outcome(&mut s, &Err("handshake timed out".into()), now);
        assert_eq!(s.next_retry_at, None);
        assert_eq!(s.next_retry_secs(now), None);
        assert!(s.last_error.as_deref().unwrap().contains(AUTO_RELAUNCH_KILL_SWITCH));
        assert!(s.last_error.as_deref().unwrap().contains("automate restart"));

        // Even an armed retry (a close event arms one) is hidden and refused.
        s.next_retry_at = Some(now);
        assert_eq!(s.next_retry_secs(now), None);
        let snap = RetrySnapshot {
            generation_matches: true,
            session_alive: true,
            enabled: true,
            relaunch_in_flight: false,
            handshake_done: false,
            agent_starting: false,
            next_retry_at: Some(now),
            last_input_age: None,
            auto_relaunch_disabled: true,
        };
        assert_eq!(
            should_retry(&snap, now),
            Err("automatic relaunch disabled by AGENT_RDP_NO_AUTO_RELAUNCH"),
            "the kill switch is reported before any 'not due' verdict"
        );
    }

    /// The schedule around the give-up threshold, and what a close event
    /// does afterwards: it re-arms once, and the next failure gives up
    /// again immediately.
    #[test]
    fn give_up_then_close_then_give_up_again() {
        let mut s = state();
        let t0 = std::time::Instant::now();
        for _ in 0..(MAX_LAUNCH_FAILURES - 1) {
            record_launch_outcome(&mut s, &Err("x".into()), t0);
        }
        assert_eq!(s.launch_failures, 5);
        assert_eq!(s.next_retry_at, Some(t0 + Duration::from_secs(300)), "capped backoff");

        record_launch_outcome(&mut s, &Err("x".into()), t0);
        assert_eq!(s.next_retry_at, None, "sixth failure gives up");

        // A close event (the supervisor's close arm) arms an immediate retry.
        s.next_retry_at = Some(t0);
        record_launch_outcome(&mut s, &Err("x".into()), t0);
        assert_eq!(s.next_retry_at, None, "still past the threshold");
        assert!(s.last_error.as_deref().unwrap().contains("gave up after 7"));
    }

    /// The bootstrap's own keystrokes are not operator input: the mark is
    /// restored, so the retry gate measures operator activity only.
    #[test]
    fn launch_keystrokes_do_not_count_as_operator_input() {
        // The restore call sits in `launch_agent`, around the typing.
        let src = include_str!("bootstrap.rs");
        let mark_at = src.find("let input_mark = rdp.input_activity_mark();").unwrap();
        let type_at = src.find("self.type_launch_command(rdp, ps_command).await").unwrap();
        let restore_at = src.find("rdp.restore_input_activity(input_mark);").unwrap();
        assert!(mark_at < type_at && type_at < restore_at);
    }

    /// PowerShell side: the disk tier never falls back to TEMP, streamed
    /// launches say so, and only launched keyed runs persist their failure.
    #[test]
    fn journal_persistence_edges_in_the_agent() {
        assert!(!LIB_ACTIONS.contains("$root = $env:TEMP"));
        assert!(LIB_ACTIONS.contains("LOCALAPPDATA is not set"));
        assert!(LIB_ACTIONS.contains("$launch.streamed = $true"));
        assert!(LIB_ACTIONS.contains("$script:LastRunLaunched = $true"));
        assert!(LIB_ACTIONS.contains("$script:LastRunLaunched = $false"));
        assert!(AGENT_SCRIPT.contains("$persist = $keyed -and ($success -or [bool]$script:LastRunLaunched)"));
        assert!(AGENT_SCRIPT.contains("-Persist:$persist"));
        // A corrupt entry is removed so the next execution can be recorded.
        assert!(LIB_ACTIONS.contains("Discarding unreadable journal entry"));
        assert!(LIB_ACTIONS.contains("Remove-Item -LiteralPath $path -Force -ErrorAction SilentlyContinue"));
        // An unparseable --shell path is treated as non-default, not as a crash.
        let shell_fn = LIB_ACTIONS.find("function Test-DefaultShell").unwrap();
        assert!(LIB_ACTIONS[shell_fn..shell_fn + 600].contains("} catch {"));
    }
}

#[cfg(test)]
mod keep_alive_and_launch_count_tests {
    use super::*;

    fn state() -> AutomationState {
        AutomationState::new(std::path::PathBuf::from("/tmp/agent-rdp-test"))
    }

    /// Every successful launch counts, `connect`'s bootstrap included -
    /// that one types Win+R on the remote desktop and was previously
    /// invisible, because only `relaunch_agent` touched `relaunches`.
    #[test]
    fn every_successful_launch_counts_including_the_bootstrap() {
        let mut st = state();
        let now = std::time::Instant::now();
        assert_eq!(st.total_launches, 0);

        record_launch_outcome(&mut st, &Ok(()), now);
        assert_eq!(st.total_launches, 1);
        assert_eq!(st.relaunches, 0, "the bootstrap is not a *re*launch");

        record_launch_outcome(&mut st, &Err("no handshake".into()), now);
        assert_eq!(st.total_launches, 1, "a failed launch does not count");

        record_launch_outcome(&mut st, &Ok(()), now);
        assert_eq!(st.total_launches, 2);
    }

    /// `relaunches` is zeroed by every `connect`; `total_launches` is not,
    /// which is the whole point - otherwise nothing distinguishes "up all
    /// day" from "the session was rebuilt an hour ago".
    #[test]
    fn a_reconnect_resets_relaunches_but_not_total_launches() {
        let source = include_str!("bootstrap.rs");
        let initialize = source
            .split("pub async fn initialize")
            .nth(1)
            .expect("initialize exists");
        let body = &initialize[..initialize.find("\n    }").unwrap_or(initialize.len())];
        assert!(body.contains("state.relaunches = 0"));
        assert!(
            !body.contains("total_launches"),
            "initialize() must not reset total_launches"
        );

        let cleanup = source.split("pub async fn cleanup").nth(1).expect("cleanup exists");
        let cleanup_body = &cleanup[..cleanup.find("\n    }").unwrap_or(cleanup.len())];
        assert!(
            !cleanup_body.contains("total_launches"),
            "cleanup() must not reset total_launches either"
        );
    }

    /// The keep-alive must stay a Refresh Rect PDU. A synchronize input
    /// event is the tempting "simpler" rewrite and would silently switch
    /// Num Lock off on the remote desktop on every tick.
    #[test]
    fn keep_alive_never_sends_an_input_event() {
        let source = include_str!("../rdp_session.rs");
        let arm = source
            .split("// Keep the connection alive while idle")
            .nth(1)
            .expect("keep-alive arm exists");
        let arm = &arm[..arm.find("// Handle incoming commands").unwrap_or(arm.len())];
        assert!(arm.contains("RefreshRectangle"), "keep-alive sends Refresh Rect");
        assert!(
            !arm.contains("SyncEvent") && !arm.contains("FastPathInputEvent"),
            "a keep-alive must never be an input event: it would rewrite the \
             remote lock-key state every tick"
        );
        // Sending on a keep-alive tick must not stamp input activity, or the
        // supervisor's idle gate would never open.
        assert!(!arm.contains("stamp_input_activity"));
    }
}

#[cfg(test)]
mod survivor_tests {
    use super::*;

    /// Adoption is version-gated, so the version has to be readable from the
    /// script this daemon actually ships.
    #[test]
    fn the_shipped_agent_version_is_readable() {
        let version = expected_agent_version().expect("the script declares a version");
        assert_eq!(version, "1.8.0");
        assert!(
            AGENT_SCRIPT.contains(&format!("$script:Version = \"{}\"", version)),
            "parsed out of the script, not hardcoded twice"
        );
    }

    /// The version string alone misses a library-only change - this cycle's
    /// own `actions.ps1` edits carried no version bump. `expected_build_id`
    /// is what a survivor is actually checked against.
    #[test]
    fn the_build_id_covers_every_embedded_script_not_just_the_version_string() {
        let id = expected_build_id();
        assert_eq!(id.len(), 64, "a hex sha256 digest");
        assert_eq!(id, expected_build_id(), "deterministic across calls");

        // Prove it actually reads the library files, not just agent.ps1: a
        // hash over agent.ps1 alone would not catch this.
        let agent_only = {
            use sha2::{Digest, Sha256};
            let mut h = Sha256::new();
            h.update(AGENT_SCRIPT.as_bytes());
            h.finalize().iter().map(|b| format!("{:02x}", b)).collect::<String>()
        };
        assert_ne!(id, agent_only);
    }

    /// The launch command is where the running agent learns the build id it
    /// will be asked to prove later - it has to come from the daemon that
    /// launched it, not from anything the agent computes about itself.
    #[test]
    fn the_launch_command_carries_the_build_id() {
        let src = include_str!("bootstrap.rs");
        assert!(src.contains("-BuildId \\\"{}\\\""));
        assert!(AGENT_SCRIPT.contains("[string]$BuildId"));
        assert!(AGENT_SCRIPT.contains("-BuildId $BuildId"));
        assert!(LIB_DVC.contains("build_id = $BuildId"));
    }

    /// Connect pays for the survivor wait; restart never does, and its budget
    /// has less headroom - conflating the two would push restart over.
    #[test]
    fn connect_costs_the_survivor_wait_and_restart_does_not() {
        let launch = launch_and_wait_worst_case();
        let connect = connect_bootstrap_worst_case();
        assert_eq!(connect, launch + SURVIVOR_WAIT);
        assert!(connect > launch);
        // Small enough that a cold connect, which pays it for nothing, does
        // not notice against a ~30s launch.
        assert!(SURVIVOR_WAIT <= Duration::from_secs(10));
    }

    /// An adopted agent was not launched: nothing was typed on the remote
    /// desktop, so counting it as a launch would make the counter that exists
    /// to answer "did this reconnect disturb the desktop?" say yes.
    #[test]
    fn adoption_is_not_counted_as_a_launch() {
        let now = std::time::Instant::now();
        let mut state = AutomationState::new(std::path::PathBuf::from("/tmp/x"));

        state.adopted = false;
        record_launch_outcome(&mut state, &Ok(()), now);
        assert_eq!(state.total_launches, 1);

        state.adopted = true;
        record_launch_outcome(&mut state, &Ok(()), now);
        assert_eq!(state.total_launches, 1, "an adoption typed nothing");

        // It is still a success in every other respect.
        assert!(state.last_error.is_none());
        assert_eq!(state.launch_failures, 0);
    }

    /// The agent has to outlive a drop long enough for a reconnect to find
    /// it, and must not spin while it waits.
    #[test]
    fn the_agent_waits_for_a_client_instead_of_exiting() {
        assert!(AGENT_SCRIPT.contains("$script:ReconnectWindowSec = 600"));
        assert!(AGENT_SCRIPT.contains("$script:ReconnectDelaySec = 3"));
        // The old three-attempt loop lost the race against every reconnect.
        assert!(!AGENT_SCRIPT.contains("$maxRetries = 3"));
        // The window is per outage, not per process.
        assert!(AGENT_SCRIPT.contains("$script:HandshakeSinceFailure"));
        // A channel whose client is gone can read 0 bytes forever.
        assert!(LIB_DVC.contains("$script:DvcMaxIdleReads"));
        assert!(LIB_DVC.contains("$script:DvcIdleReadSleepMs"));
    }

    /// `shutdown` is how both the daemon and the channel layer stop an agent
    /// they are not keeping; without the dispatch arm they wait for nothing.
    #[test]
    fn the_agent_can_be_asked_to_exit() {
        assert!(AGENT_SCRIPT.contains("\"shutdown\""));
        assert!(AGENT_SCRIPT.contains("$script:ShutdownRequested = $true"));
        assert!(
            AGENT_SCRIPT.contains("\"survives_reconnect\""),
            "advertised so a daemon can tell an adoptable agent from an old one"
        );
    }

    /// A streamed run used to open a visible console window on the remote
    /// desktop - the same foreground theft the survivor design exists to
    /// avoid, caused by us.
    #[test]
    fn a_streamed_run_does_not_open_a_window() {
        assert!(LIB_ACTIONS.contains("$startArgs.NoNewWindow = $true"));
    }
}
