//! Automation types for Windows UI Automation via file-based IPC.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

impl AutomateRequest {
    /// Whether re-issuing this request cannot change remote state. A lost
    /// reply is ambiguous about whether the *action* happened, but the safe
    /// response to it is not: retrying a read is always safe, retrying a
    /// click or fill risks applying it twice. Used by the daemon's
    /// indeterminate-result text and by the CLI's automatic retry after a
    /// dropped IPC connection.
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            AutomateRequest::Snapshot { .. }
                | AutomateRequest::Get { .. }
                | AutomateRequest::Status
                | AutomateRequest::WaitFor { .. }
                | AutomateRequest::RunPoll { .. }
                | AutomateRequest::FileStat { .. }
                | AutomateRequest::FileReadChunk { .. }
                | AutomateRequest::QueryResult { .. }
                | AutomateRequest::Window { action: WindowAction::List, .. }
        )
    }
}

/// Automation request sent from CLI to daemon.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum AutomateRequest {
    /// Take a snapshot of the accessibility tree.
    Snapshot {
        /// Filter to interactive elements only (buttons, inputs, links).
        #[serde(default)]
        interactive_only: bool,
        /// Compact mode - remove empty structural elements.
        #[serde(default)]
        compact: bool,
        /// Maximum tree depth to traverse.
        #[serde(default = "default_max_depth")]
        max_depth: u32,
        /// Scope to a specific element (window, panel, etc.) via selector.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        selector: Option<String>,
        /// Start from the currently focused element.
        #[serde(default)]
        focused: bool,
    },

    /// Get element properties.
    Get {
        /// Element selector.
        selector: String,
        /// Property to retrieve (name, value, states, bounds, or all).
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        property: Option<String>,
    },

    /// Set focus to an element.
    Focus {
        /// Element selector.
        selector: String,
    },

    /// Click an element - for buttons, links, menu items.
    Click {
        /// Element selector.
        selector: String,
        /// Use double-click instead of single click.
        #[serde(default)]
        double_click: bool,
    },

    /// Select an element (SelectionItemPattern) - for list items, radio buttons.
    /// Can also select by item name within a container.
    Select {
        /// Element selector (container or item directly).
        selector: String,
        /// Item name to select within container (optional).
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        item: Option<String>,
    },

    /// Toggle an element (TogglePattern) - for checkboxes.
    Toggle {
        /// Element selector.
        selector: String,
        /// Target state: true=on, false=off, None=toggle.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        state: Option<bool>,
    },

    /// Expand an element (ExpandCollapsePattern) - for menus, tree items, combo boxes.
    Expand {
        /// Element selector.
        selector: String,
    },

    /// Collapse an element (ExpandCollapsePattern).
    Collapse {
        /// Element selector.
        selector: String,
    },

    /// Open context menu for an element (Focus + Shift+F10).
    ContextMenu {
        /// Element selector.
        selector: String,
    },

    /// Clear and fill text in an element.
    Fill {
        /// Element selector.
        selector: String,
        /// Text to fill.
        text: String,
    },

    /// Clear text from an element.
    Clear {
        /// Element selector.
        selector: String,
    },

    /// Scroll an element.
    Scroll {
        /// Element selector.
        selector: String,
        /// Scroll direction.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        direction: Option<AutomationScrollDirection>,
        /// Scroll amount.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        amount: Option<i32>,
        /// Child element to scroll into view.
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        to_child: Option<String>,
    },

    /// Window operations.
    Window {
        /// Window action to perform.
        action: WindowAction,
        /// Window selector (optional, uses foreground window if not specified).
        #[serde(skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        selector: Option<String>,
    },

    /// Run a PowerShell command.
    Run {
        /// Command to run.
        command: String,
        /// Command arguments.
        #[serde(default)]
        args: Vec<String>,
        /// Wait for command to complete.
        #[serde(default)]
        wait: bool,
        /// Run with hidden window.
        #[serde(default)]
        hidden: bool,
        /// Timeout in milliseconds when waiting.
        #[serde(default = "default_run_timeout")]
        #[ts(type = "number")]
        timeout_ms: u64,
        /// Shell executable to run the command through (default: powershell.exe).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        shell: Option<String>,
        /// Redirect stdout/stderr and keep the process alive for incremental
        /// retrieval via `RunPoll`, instead of waiting for exit or discarding
        /// output. Ignored if `wait` is also true.
        #[serde(default)]
        stream: bool,
        /// Caller-chosen request id. A retry that reuses the key gets the
        /// journaled result of the first execution back (`RunResult.replayed`)
        /// instead of running the command a second time - the difference
        /// between "retry after a lost reply" and "Add-Content applied
        /// twice". 1-64 chars of `[A-Za-z0-9._:-]`. Keyed results are
        /// journaled on the remote host (`%LOCALAPPDATA%\agent-rdp\journal`,
        /// 7 days / 256 keys, per Windows account), so a replay survives a
        /// reconnect and an agent relaunch. A different profile or host (a
        /// temporary profile, an RDS farm) starts empty - verify the side
        /// effect there. `timeout_ms` is not part of the key's fingerprint,
        /// so a retry with a longer `--process-timeout` still replays.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        idempotency_key: Option<String>,
    },

    /// Poll a process previously started with `Run { stream: true, .. }` for
    /// output produced since the last poll.
    RunPoll {
        /// Process ID returned by the initial `Run` call.
        pid: u32,
    },

    /// Wait for an element to reach a state.
    WaitFor {
        /// Element selector.
        selector: String,
        /// Timeout in milliseconds.
        #[serde(default = "default_wait_timeout")]
        #[ts(type = "number")]
        timeout_ms: u64,
        /// State to wait for.
        #[serde(default)]
        state: WaitState,
    },

    /// Get automation agent status.
    Status,

    /// Write one chunk of a file on the remote machine.
    FileWriteChunk {
        /// Destination path on the remote machine.
        path: String,
        /// Base64-encoded chunk bytes.
        data_b64: String,
        /// Truncate before writing (first chunk of a transfer).
        #[serde(default)]
        first: bool,
        /// Last chunk - triggers hash verification.
        #[serde(default)]
        last: bool,
        /// Expected SHA-256 of the complete file, checked when `last`.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        sha256: Option<String>,
    },

    /// Read one chunk of a file from the remote machine.
    FileReadChunk {
        /// Source path on the remote machine.
        path: String,
        /// Byte offset to read from.
        #[ts(type = "number")]
        offset: u64,
        /// Maximum bytes to read.
        #[ts(type = "number")]
        length: u64,
    },

    /// Size/hash/existence of a path on the remote machine.
    FileStat {
        /// Path on the remote machine.
        path: String,
    },

    /// Ask the agent what it did with an earlier request.
    ///
    /// Answers the question a lost DVC reply leaves open: whether a command
    /// ran. The agent keeps the last few results, so "unknown id" once it is
    /// responding again means the request never executed.
    QueryResult {
        /// Id of the request to look up.
        id: String,
    },
}

fn default_max_depth() -> u32 {
    10
}

fn default_wait_timeout() -> u64 {
    30000
}

fn default_run_timeout() -> u64 {
    10000
}

/// Scroll direction for automation.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum AutomationScrollDirection {
    Up,
    Down,
    Left,
    Right,
}

/// Window action for automation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum WindowAction {
    /// List all windows.
    List,
    /// Focus a window.
    Focus,
    /// Maximize a window.
    Maximize,
    /// Minimize a window.
    Minimize,
    /// Restore a window.
    Restore,
    /// Close a window.
    Close,
}

/// State to wait for in WaitFor command.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum WaitState {
    /// Element is visible.
    #[default]
    Visible,
    /// Element is enabled.
    Enabled,
    /// Element is gone (no longer exists).
    Gone,
}

/// Accessibility tree snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct AccessibilitySnapshot {
    /// Unique snapshot ID.
    pub snapshot_id: String,
    /// Total number of elements with refs.
    pub ref_count: u32,
    /// Whether the tree was truncated due to depth limit.
    #[serde(default)]
    pub truncated: bool,
    /// Maximum depth used for this snapshot.
    #[serde(default)]
    pub max_depth: u32,
    /// Root element of the tree.
    pub root: AccessibilityElement,
}

/// An element in the accessibility tree.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct AccessibilityElement {
    /// Reference number (for @ref selectors).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub r#ref: Option<u32>,
    /// Element role (control type).
    pub role: String,
    /// Element name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    /// Automation ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub automation_id: Option<String>,
    /// Win32 class name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub class_name: Option<String>,
    /// Bounding rectangle.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bounds: Option<ElementBounds>,
    /// Element states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<String>,
    /// Current value (for editable elements).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub value: Option<String>,
    /// Supported UI Automation patterns.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub patterns: Vec<String>,
    /// Child elements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<AccessibilityElement>,
}

/// Bounding rectangle for an element.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ElementBounds {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Element value response.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ElementValue {
    /// Element name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub name: Option<String>,
    /// Element value.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub value: Option<String>,
    /// Element states.
    #[serde(default)]
    pub states: Vec<String>,
    /// Element bounds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bounds: Option<ElementBounds>,
}

/// Window information.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct WindowInfo {
    /// Window title.
    pub title: String,
    /// Process name.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub process_name: Option<String>,
    /// Process ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub process_id: Option<u32>,
    /// Window bounds.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub bounds: Option<ElementBounds>,
    /// Whether the window is minimized.
    #[serde(default)]
    pub minimized: bool,
    /// Whether the window is maximized.
    #[serde(default)]
    pub maximized: bool,
}

/// Automation agent status.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct AutomationStatus {
    /// Whether the automation agent is running.
    pub agent_running: bool,
    /// Agent process ID.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub agent_pid: Option<u32>,
    /// Supported capabilities.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Agent version.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub version: Option<String>,
    /// Path of the agent's own log file on the remote machine, when the
    /// agent reports one. `agent-rdp diagnose` pulls it into the bundle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub log_path: Option<String>,
    /// How many times the daemon relaunched the agent on its own since this
    /// session connected (the DVC channel closed while the RDP session was
    /// alive). Non-zero means the agent process is dying under you - look at
    /// the remote log via `agent-rdp diagnose`.
    #[serde(default)]
    pub relaunches: u32,
    /// Seconds since the current agent's DVC handshake completed. Distinct
    /// from `agent_running`: an agent can be "running" per the PS-reported
    /// fields yet the daemon-side DVC channel could have gone stale without
    /// a full status round-trip being able to tell - uptime combined with
    /// `last_rtt_ms` is what actually answers "is this still responsive".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub uptime_secs: Option<u64>,
    /// Round-trip time of the most recent successful DVC request, in
    /// milliseconds. `None` if no request has succeeded yet this session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub last_rtt_ms: Option<u64>,
    /// Count of consecutive requests that got no reply. Non-zero without the
    /// channel being reported dead means it is degraded, not yet unresponsive
    /// - a signal for deciding whether reconnecting is warranted (refs
    /// invalidate on reconnect, so this should inform that decision rather
    /// than triggering an automatic one).
    #[serde(default)]
    pub consecutive_failures: u32,
    /// Why the agent is down, when it is: the last bootstrap or relaunch
    /// failure, as recorded by the daemon. Cleared when a launch succeeds.
    /// Present even while the agent is unreachable, because `status` is
    /// answered from the daemon's own state in that case.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub last_error: Option<String>,
    /// Seconds until the daemon's supervisor next tries to relaunch the
    /// agent on its own (only while the agent is down and a retry is
    /// scheduled; retries wait for the session to be idle first).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub next_retry_secs: Option<u64>,
}

/// Command run result.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct RunResult {
    /// Exit code (if waited).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub exit_code: Option<i32>,
    /// Standard output (if waited).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stdout: Option<String>,
    /// Standard error (if waited).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub stderr: Option<String>,
    /// Process ID (if not waited).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub pid: Option<u32>,
    /// True when this result was replayed from the agent's journal because
    /// the request reused an `idempotency_key` - the command did not run
    /// again.
    #[serde(default)]
    pub replayed: bool,
    /// Detached launch only (`wait: false`): the process had already exited
    /// when the agent checked ~250ms after starting it, and `exit_code`
    /// carries its status. A script that fails before its first real
    /// statement is otherwise indistinguishable from one that is running.
    #[serde(default)]
    pub early_exit: bool,
    /// When the process was started, in Unix seconds by the *remote* clock -
    /// the same clock `file pull`/`file stat` report modification times in,
    /// so a run can be tied to the files it produced.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub started_unix: Option<u64>,
    /// When the process exited, in Unix seconds by the remote clock (waited
    /// runs only). The freshness marker for `stdout`: a result whose
    /// `finished_unix` is older than the caller's own step started is a
    /// replay or a stale read, not this run's output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub finished_unix: Option<u64>,
    /// The exact PowerShell source the agent handed to the child shell:
    /// the command text followed by each argument as a single-quoted
    /// literal. This is what a parse error refers to - compare it with what
    /// you typed to see which layer (your local shell, the argument
    /// quoting) changed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub command_line: Option<String>,
    /// When `replayed`: the remote-clock time the original execution was
    /// recorded, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub replayed_at_unix: Option<u64>,
    /// The process was started with `stream: true`: its output is being
    /// captured for `RunPoll`, as opposed to a plain detached launch where
    /// nothing is captured.
    #[serde(default)]
    pub streamed: bool,
}

/// Incremental output from a process started with `Run { stream: true, .. }`.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct RunPollResult {
    /// Process ID that was polled.
    pub pid: u32,
    /// Standard output produced since the last poll.
    #[serde(default)]
    pub stdout_chunk: String,
    /// Standard error produced since the last poll.
    #[serde(default)]
    pub stderr_chunk: String,
    /// Whether the process has exited.
    pub exited: bool,
    /// Exit code, present once `exited` is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub exit_code: Option<i32>,
    /// When the process exited, in Unix seconds by the remote clock
    /// (present once `exited` is true). See `RunResult::finished_unix`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub finished_unix: Option<u64>,
}

/// Click action result.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ClickResult {
    /// Whether the click was performed.
    pub clicked: bool,
    /// Method used (click or double_click).
    pub method: String,
    /// X coordinate of click.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub x: Option<i32>,
    /// Y coordinate of click.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub y: Option<i32>,
}

/// Handshake data from PowerShell agent.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct AutomationHandshake {
    /// Agent version.
    pub version: String,
    /// Agent process ID.
    pub agent_pid: u32,
    /// Start timestamp (optional for backwards compatibility).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub started_at: Option<String>,
    /// Supported capabilities.
    pub capabilities: Vec<String>,
    /// Whether the agent is ready.
    pub ready: bool,
}

/// Request sent to PowerShell agent via file IPC.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FileIpcRequest {
    /// Unique request ID.
    pub id: String,
    /// Command to execute.
    pub command: String,
    /// Command parameters.
    #[ts(type = "unknown")]
    pub params: serde_json::Value,
}

/// Response from PowerShell agent via file IPC.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FileIpcResponse {
    /// Request ID this responds to.
    pub id: String,
    /// Response timestamp.
    pub timestamp: String,
    /// Whether the command succeeded.
    pub success: bool,
    /// Response data on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "unknown")]
    pub data: Option<serde_json::Value>,
    /// Error details on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<FileIpcError>,
}

/// Error from PowerShell agent.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FileIpcError {
    /// Error code.
    pub code: String,
    /// Error message.
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_automate_request_serialization() {
        let req = AutomateRequest::Snapshot {
            interactive_only: true,
            compact: false,
            max_depth: 10,
            selector: None,
            focused: false,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"snapshot\""));
        assert!(json.contains("\"interactive_only\":true"));

        let parsed: AutomateRequest = serde_json::from_str(&json).unwrap();
        match parsed {
            AutomateRequest::Snapshot { interactive_only, .. } => {
                assert!(interactive_only);
            }
            _ => panic!("unexpected request type"),
        }
    }

    #[test]
    fn test_click_request_serialization() {
        let req = AutomateRequest::Click {
            selector: "@5".to_string(),
            double_click: false,
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"click\""));
        assert!(json.contains("\"selector\":\"@5\""));
    }

    #[test]
    fn test_toggle_request_serialization() {
        let req = AutomateRequest::Toggle {
            selector: "@5".to_string(),
            state: Some(true),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"toggle\""));
        assert!(json.contains("\"state\":true"));
    }

    #[test]
    fn test_window_action_serialization() {
        let req = AutomateRequest::Window {
            action: WindowAction::Maximize,
            selector: Some("#Notepad".to_string()),
        };

        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("\"op\":\"window\""));
        assert!(json.contains("\"maximize\""));
    }

    #[test]
    fn test_accessibility_element_serialization() {
        let elem = AccessibilityElement {
            r#ref: Some(1),
            role: "button".to_string(),
            name: Some("OK".to_string()),
            automation_id: Some("btnOK".to_string()),
            class_name: Some("Button".to_string()),
            bounds: Some(ElementBounds {
                x: 100,
                y: 200,
                width: 80,
                height: 30,
            }),
            states: vec!["focusable".to_string(), "enabled".to_string()],
            value: None,
            patterns: vec!["invoke".to_string()],
            children: vec![],
        };

        let json = serde_json::to_string(&elem).unwrap();
        assert!(json.contains("\"role\":\"button\""));
        assert!(json.contains("\"ref\":1"));
    }

    /// New optional status fields default when absent and round-trip when present.
    #[test]
    fn automation_status_retry_fields_are_optional() {
        let old: AutomationStatus = serde_json::from_str(r#"{"agent_running":false}"#).unwrap();
        assert_eq!(old.last_error, None);
        assert_eq!(old.next_retry_secs, None);
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("last_error"), "absent fields are not emitted: {}", json);

        let full = AutomationStatus {
            last_error: Some("handshake timed out".into()),
            next_retry_secs: Some(42),
            ..old
        };
        let back: AutomationStatus = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back.last_error.as_deref(), Some("handshake timed out"));
        assert_eq!(back.next_retry_secs, Some(42));
    }

    /// `RunResult` from an older agent (no finished_unix/command_line) still
    /// parses; the new fields round-trip.
    #[test]
    fn run_result_new_fields_are_optional() {
        let old: RunResult = serde_json::from_str(r#"{"exit_code":0,"stdout":""}"#).unwrap();
        assert_eq!(old.finished_unix, None);
        assert_eq!(old.command_line, None);
        assert_eq!(old.replayed_at_unix, None);
        let json = serde_json::to_string(&old).unwrap();
        assert!(!json.contains("command_line"));
        let full = RunResult { finished_unix: Some(5), command_line: Some("x".into()), replayed_at_unix: Some(3), ..old };
        let back: RunResult = serde_json::from_str(&serde_json::to_string(&full).unwrap()).unwrap();
        assert_eq!(back.finished_unix, Some(5));
        assert_eq!(back.command_line.as_deref(), Some("x"));
        assert_eq!(back.replayed_at_unix, Some(3));
        let poll: RunPollResult = serde_json::from_str(r#"{"pid":1,"exited":true,"exit_code":0,"finished_unix":9}"#).unwrap();
        assert_eq!(poll.finished_unix, Some(9));
    }
}
