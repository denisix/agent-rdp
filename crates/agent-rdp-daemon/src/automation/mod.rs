//! Windows UI Automation module.
//!
//! This module provides DVC-based IPC communication with a PowerShell agent
//! running on the remote Windows machine for UI automation via the Windows
//! UI Automation API.

mod bootstrap;
pub mod dvc_channel;
pub mod dvc_encode;
mod dvc_ipc;

#[cfg(test)]
pub use bootstrap::lf;
pub use bootstrap::{
    adopt_only, connect_bootstrap_worst_case, expected_agent_version, expected_build_id,
    launch_and_wait_worst_case, note_agent, note_launch_typed, restart_worst_case,
    launch_guarded, relaunch_agent, spawn_relaunch_supervisor, AutomationBootstrap,
    RelaunchBudget, LAUNCH_ATTEMPTS, MAX_LAUNCH_FAILURES, RETRY_INPUT_QUIET, SURVIVOR_WAIT,
};
pub use dvc_channel::{
    new_shared_dvc_state, AutomationDvc, AutomationDvcListener, DvcCommandReceiver,
    DvcCommandSender, DvcHandshake, DvcSendCommand, SharedDvcState, CHANNEL_NAME,
};
pub use dvc_encode::encode_dvc_data;
pub use dvc_ipc::{DvcIndeterminate, DvcIpc};

use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use uuid::Uuid;

/// Which agent process is on the other end of the channel.
///
/// PID alone cannot answer that: Windows reuses them, and a survivor and its
/// replacement are both just "a powershell.exe". The agent therefore mints an
/// instance id once per process and reports it in its handshake; an agent old
/// enough not to send one falls back to the pid, which is still better than
/// nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentIdentity {
    pub pid: u32,
    pub instance: Option<String>,
}

impl AgentIdentity {
    /// Whether this is the same agent process as `other`. Instance ids
    /// decide it when both sides have one; otherwise the pid does.
    pub fn is_same(&self, other: &AgentIdentity) -> bool {
        match (&self.instance, &other.instance) {
            (Some(a), Some(b)) => a == b,
            _ => self.pid == other.pid,
        }
    }
}

/// Automation state that persists across requests.
#[derive(Debug)]
pub struct AutomationState {
    /// Whether automation is enabled for this session.
    pub enabled: bool,
    /// Unique ID for this automation session (different from RDP session ID).
    pub automation_id: String,
    /// Path to the automation directory on the host side (for RDPDR bootstrap).
    pub automation_dir: PathBuf,
    /// Drive name mapped via RDPDR (still needed for bootstrap).
    pub drive_name: String,
    /// DVC-based IPC client.
    pub dvc_ipc: Option<DvcIpc>,
    /// Shared DVC state (for processor access).
    pub dvc_state: Option<SharedDvcState>,
    /// Whether the agent has completed handshake.
    pub agent_ready: bool,
    /// Agent process ID (if known).
    pub agent_pid: Option<u32>,
    /// Receiver for "the DVC channel closed" notifications, created by
    /// `initialize()` and taken by `connect` to spawn the session's relaunch
    /// supervisor.
    pub closed_rx: Option<tokio::sync::mpsc::UnboundedReceiver<()>>,
    /// A relaunch (`automate restart` or the supervisor) is running. Two at
    /// once would launch two agents and fight over the Run dialog.
    pub relaunch_in_flight: bool,
    /// Successful relaunches (supervisor or `automate restart`) since this
    /// session connected.
    pub relaunches: u32,
    /// Why the agent is down: the last bootstrap/relaunch failure. Cleared
    /// by a successful launch. Reported by `automate status` even while the
    /// agent cannot be reached.
    pub last_error: Option<String>,
    /// When the supervisor may next try to relaunch on its own. Armed only
    /// by a recorded failure - never by an in-progress bootstrap - so a
    /// supervisor tick cannot launch a second agent under a `connect` that
    /// is still waiting for the first.
    pub next_retry_at: Option<std::time::Instant>,
    /// Consecutive failed launches; drives the retry backoff and the
    /// give-up threshold. Reset by success and by `automate restart`.
    pub launch_failures: u32,
    /// `AGENT_RDP_NO_AUTO_RELAUNCH` was set when this session initialized:
    /// no retry is ever scheduled, and status says so instead of promising
    /// one.
    pub auto_relaunch_disabled: bool,
    /// Every successful launch this daemon process has done against the
    /// current target, `connect`'s own bootstrap included. Deliberately
    /// **not** reset by `initialize()`/`cleanup()`, unlike `relaunches`:
    /// a counter that resets on every reconnect cannot answer "has the
    /// agent been up all day, or was this session rebuilt an hour ago?".
    /// Reset only when `connect` targets a different host:port, since one
    /// counter spanning two machines would be worse than none.
    pub total_launches: u32,
    /// The `host:port` the launches above were counted against.
    pub launch_target: Option<String>,
    /// The current agent was adopted rather than launched: it outlived a
    /// transport drop and re-opened its channel, so this reconnect typed
    /// nothing on the remote desktop. Set on adoption, cleared by any launch.
    pub adopted: bool,
    /// Which initialization of this state a launch belongs to. Bumped by
    /// `initialize()` and `cleanup()`; a launch captures it when it starts
    /// and touches no bookkeeping - and types nothing - once it no longer
    /// matches. This is what stops a bootstrap abandoned by a transport
    /// drop from driving the Run dialog of, and recording a failure
    /// against, the session that a later `connect` built on the same
    /// state object.
    pub epoch: u64,
    /// `connect --defer-agent`: the caller chose not to have Win+R typed.
    /// The relaunch supervisor honours it (a stale survivor being evicted
    /// closes the channel, which would otherwise arm an automatic launch
    /// five seconds later); `automate restart` clears it.
    pub launch_deferred: bool,
    /// The instance id of the agent currently on the channel, when it
    /// reports one.
    pub agent_instance: Option<String>,
    /// The last agent this daemon recorded on the channel. Compared against
    /// what the channel actually holds to notice that the process changed -
    /// several paths can swap the agent with nothing else observing it (an
    /// extra channel promoted to primary, a late handshake). Survives
    /// `cleanup()`/`initialize()` like `total_launches`, because the
    /// question it answers ("is this the same agent as before the drop?")
    /// spans reconnects by definition.
    pub last_agent_identity: Option<AgentIdentity>,
    /// The pid of the agent before the current one, when it was replaced.
    pub previous_agent_pid: Option<u32>,
    /// The current agent is not the one this daemon last recorded, and no
    /// launch of ours produced it. `adopted` alone said "we did not type
    /// Win+R", which a caller reasonably reads as "the same agent is still
    /// running" - and that was false whenever a *different* process had
    /// taken the channel.
    pub adopted_replacement: bool,
    /// How many times the agent process behind this channel has changed.
    /// Counted against the same target as `total_launches`: monitoring that
    /// asks "did the agent stay up all day?" needs an answer that survives
    /// a reconnect.
    pub agent_changes: u32,
}

impl AutomationState {
    /// Create a new automation state.
    pub fn new(session_dir: PathBuf) -> Self {
        let automation_id = Uuid::new_v4().to_string()[..8].to_string();
        let automation_dir = session_dir.join(format!("automation-{}", automation_id));

        Self {
            enabled: false,
            automation_id,
            automation_dir,
            drive_name: "agent-automation".to_string(),
            dvc_ipc: None,
            dvc_state: None,
            agent_ready: false,
            agent_pid: None,
            closed_rx: None,
            relaunch_in_flight: false,
            relaunches: 0,
            last_error: None,
            next_retry_at: None,
            launch_failures: 0,
            auto_relaunch_disabled: false,
            total_launches: 0,
            launch_target: None,
            adopted: false,
            epoch: 0,
            launch_deferred: false,
            agent_instance: None,
            last_agent_identity: None,
            previous_agent_pid: None,
            adopted_replacement: false,
            agent_changes: 0,
        }
    }

    /// Forget everything counted against one target machine.
    ///
    /// `total_launches` and the agent-identity history deliberately outlive
    /// a reconnect, so the only thing that may reset them is pointing this
    /// daemon at a different host - one counter spanning two machines would
    /// be worse than none. Kept together in one place because a partial
    /// reset is how these two drift into disagreeing.
    pub fn reset_target_counters(&mut self) {
        self.total_launches = 0;
        self.last_agent_identity = None;
        self.previous_agent_pid = None;
        self.adopted_replacement = false;
        self.agent_changes = 0;
    }

    /// Seconds until the next automatic relaunch attempt, if one is
    /// scheduled and not yet due (0 when due). Never `Some` when automatic
    /// relaunches are disabled.
    pub fn next_retry_secs(&self, now: std::time::Instant) -> Option<u64> {
        if self.auto_relaunch_disabled {
            return None;
        }
        self.next_retry_at
            .map(|at| at.saturating_duration_since(now).as_secs())
    }

    /// Get the path where the PowerShell script should be written.
    pub fn script_path(&self) -> PathBuf {
        self.automation_dir.join("scripts").join("agent.ps1")
    }

    /// Check if DVC IPC is ready.
    pub fn is_dvc_ready(&self) -> bool {
        self.dvc_ipc.as_ref().map(|ipc| ipc.is_ready()).unwrap_or(false)
    }
}

/// Thread-safe automation state handle.
pub type SharedAutomationState = Arc<Mutex<AutomationState>>;

/// Create a new shared automation state.
pub fn new_shared_state(session_dir: PathBuf) -> SharedAutomationState {
    Arc::new(Mutex::new(AutomationState::new(session_dir)))
}
