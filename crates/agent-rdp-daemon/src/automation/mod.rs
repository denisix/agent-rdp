//! Windows UI Automation module.
//!
//! This module provides DVC-based IPC communication with a PowerShell agent
//! running on the remote Windows machine for UI automation via the Windows
//! UI Automation API.

mod bootstrap;
pub mod dvc_channel;
pub mod dvc_encode;
mod dvc_ipc;

pub use bootstrap::{
    adopt_only, connect_bootstrap_worst_case, expected_agent_version, expected_build_id,
    launch_and_wait_worst_case,
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
        }
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
