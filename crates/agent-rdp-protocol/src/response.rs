//! Response types for daemon to CLI communication.

use crate::automation::{
    AccessibilitySnapshot, AutomationStatus, ClickResult, ElementValue, RunPollResult, RunResult,
    WindowInfo,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use ts_rs::TS;

/// A response from the daemon to the CLI.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct Response {
    /// Whether the operation succeeded.
    pub success: bool,

    /// Response data on success.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub data: Option<ResponseData>,

    /// Error details on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub error: Option<ErrorInfo>,
}

impl Response {
    /// Create a successful response with data.
    pub fn success(data: ResponseData) -> Self {
        Self {
            success: true,
            data: Some(data),
            error: None,
        }
    }

    /// Create a simple success response with no data.
    pub fn ok() -> Self {
        Self {
            success: true,
            data: Some(ResponseData::Ok),
            error: None,
        }
    }

    /// Create an error response.
    pub fn error(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            success: false,
            data: None,
            error: Some(ErrorInfo {
                code,
                message: message.into(),
            }),
        }
    }
}

/// Response data variants.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseData {
    /// Simple acknowledgment.
    Ok,

    /// Connection established.
    Connected {
        /// Server hostname.
        host: String,
        /// Desktop width.
        width: u16,
        /// Desktop height.
        height: u16,
        /// Whether the UI Automation agent came up, when it was requested.
        ///
        /// `None` if automation wasn't requested. `Some(false)` means RDP is
        /// connected but `automate` commands will not work - previously this
        /// failure was only warned about in the daemon log, so callers saw a
        /// successful connect and then an unexplained "agent not ready".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        automation_ready: Option<bool>,
        /// Why the automation agent failed to come up, when `automation_ready`
        /// is `Some(false)`. Distinguishes "the automation directory
        /// couldn't be created" from "the agent launched but the handshake
        /// timed out" - both used to collapse into the same generic
        /// "automation not enabled" message on the next `automate` call.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        automation_error: Option<String>,
    },

    /// Screenshot data.
    Screenshot {
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
        /// Image format.
        format: String,
        /// Base64-encoded image data.
        base64: String,
        /// X offset of the image within the full desktop (region captures only).
        ///
        /// Add this to an x coordinate read off the image to get a coordinate
        /// that can be clicked.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        offset_x: Option<u32>,
        /// Y offset of the image within the full desktop (region captures only).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[ts(optional)]
        offset_y: Option<u32>,
        /// Milliseconds since the last PDU was successfully read from the
        /// RDP server. A stuck-but-undetected connection previously kept
        /// returning the same cached frame forever with no way to tell -
        /// a large, growing value here (especially combined with an
        /// unchanged image) is the signal that the transport may be dead
        /// rather than the desktop just being idle.
        #[ts(type = "number")]
        frame_age_ms: u64,
        /// Generation counter of the framebuffer at capture time. Two
        /// screenshots with the same `frame_seq` are guaranteed
        /// pixel-identical; a sequence that never advances across an action
        /// that must have changed the screen is the stale-frame signal.
        #[ts(type = "number")]
        frame_seq: u64,
        /// FNV-1a 64-bit hash (16 hex digits) of the captured pixels. Same
        /// role as `frame_seq` but survives daemon restarts / independent of
        /// process state - a byte-for-byte fingerprint of what was actually
        /// captured.
        frame_hash: String,
    },

    /// Clipboard text content.
    Clipboard {
        /// Text content.
        text: String,
    },

    /// Session information.
    SessionInfo(SessionInfo),

    /// List of mapped drives.
    DriveList {
        /// Mapped drives.
        drives: Vec<MappedDrive>,
    },

    /// List of active sessions.
    SessionList {
        /// Active sessions.
        sessions: Vec<SessionSummary>,
    },

    /// Pong response for ping.
    Pong {
        /// Version of the daemon binary (`CARGO_PKG_VERSION`).
        ///
        /// The daemon persists across CLI upgrades: the socket and pid paths
        /// are derived from the session name alone, so a freshly installed
        /// CLI happily reuses a daemon started by the previous version - and
        /// that daemon keeps serving old code, including the automation
        /// scripts it embeds, until someone kills it. The CLI compares this
        /// against its own version to detect that. Defaulted so a reply from
        /// a daemon predating the field still parses (as an empty string,
        /// which the CLI treats as "older").
        #[serde(default)]
        version: String,
    },

    /// Accessibility tree snapshot.
    Snapshot(AccessibilitySnapshot),

    /// Element value/properties.
    Element(ElementValue),

    /// Window list.
    WindowList {
        /// List of windows.
        windows: Vec<WindowInfo>,
    },

    /// Automation agent status.
    AutomationStatus(AutomationStatus),

    /// Command run result.
    RunResult(RunResult),

    /// Incremental output from a streamed run.
    RunPollResult(RunPollResult),

    /// Click action result.
    ClickResult(ClickResult),

    /// OCR locate result.
    LocateResult(LocateResult),

    /// Result of a `ClickAt` request.
    ClickAtResult(ClickAtResult),

    /// Result of a file push/pull.
    FileTransferResult(FileTransferResult),
}

/// Outcome of a file transfer in either direction.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct FileTransferResult {
    /// Path written on the destination machine.
    pub path: String,
    /// Bytes transferred.
    #[ts(type = "number")]
    pub bytes: u64,
    /// SHA-256 of the transferred file, verified on both ends.
    pub sha256: String,
    /// Number of chunks the transfer was split into.
    #[ts(type = "number")]
    pub chunks: u64,
    /// Pull only: the remote file's last-write time (RFC 3339, UTC).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub modified: Option<String>,
    /// Pull only: the same as Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub modified_unix: Option<u64>,
    /// Pull only: seconds between the remote file's last write and the
    /// remote machine's clock at stat time - the freshness signal that tells
    /// a just-written result from yesterday's, without depending on the two
    /// machines' clocks agreeing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub age_secs: Option<u64>,
}

/// Session information.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct SessionInfo {
    /// Session name.
    pub name: String,

    /// Connection state.
    pub state: ConnectionState,

    /// Connected server host (if connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,

    /// Desktop width (if connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub width: Option<u16>,

    /// Desktop height (if connected).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub height: Option<u16>,

    /// Daemon process ID.
    pub pid: u32,

    /// Version of the daemon binary (`CARGO_PKG_VERSION`). Empty when the
    /// daemon predates this field. See `ResponseData::Pong`.
    #[serde(default)]
    pub daemon_version: String,

    /// Time since daemon started (seconds).
    #[ts(type = "number")]
    pub uptime_secs: u64,

    /// Milliseconds since the last PDU was successfully read from the RDP
    /// server (only present while connected). See `Screenshot::frame_age_ms`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional, type = "number")]
    pub last_frame_age_ms: Option<u64>,
}

/// Connection state.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    /// Not connected to any RDP server.
    Disconnected,
    /// Currently connecting.
    Connecting,
    /// Connected and active.
    Connected,
    /// Connection failed.
    Failed,
}

/// Summary of a session for listing.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct SessionSummary {
    /// Session name.
    pub name: String,
    /// Connection state.
    pub state: ConnectionState,
    /// Connected host (if any).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub host: Option<String>,
}

/// Mapped drive information.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct MappedDrive {
    /// Drive name.
    pub name: String,
    /// Local path.
    pub path: String,
}

/// OCR locate result.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct LocateResult {
    /// Matching text regions found.
    pub matches: Vec<OcrMatch>,
    /// Total words detected on screen.
    pub total_words: u32,
}

/// Result of a `ClickAt` request.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ClickAtResult {
    /// Whether the click was actually sent.
    pub clicked: bool,
    pub x: u16,
    pub y: u16,
    /// Text OCR recognized in the region containing the point, if any.
    /// Best-effort - may be inaccurate or absent for scripts OCR recognition
    /// struggles with; the safety check itself only relies on detection
    /// (bounding boxes), not on this being correct.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub matched_text: Option<String>,
    /// Other detected regions within the configured gap of the point.
    /// Populated (with `clicked: false`) when the click was refused as
    /// ambiguous.
    #[serde(default)]
    pub nearby: Vec<OcrMatch>,
    /// Chebyshev distance between `(x, y)` and the confirm point, when a
    /// confirm point was supplied. Populated (with `clicked: false`) when
    /// the two measurements diverged past `max_divergence`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[ts(optional)]
    pub divergence: Option<u32>,
}

/// A text region found by OCR.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct OcrMatch {
    /// Recognized text.
    pub text: String,
    /// Left edge X coordinate.
    pub x: i32,
    /// Top edge Y coordinate.
    pub y: i32,
    /// Width of bounding box.
    pub width: i32,
    /// Height of bounding box.
    pub height: i32,
    /// Center X coordinate (for clicking).
    pub center_x: i32,
    /// Center Y coordinate (for clicking).
    pub center_y: i32,
}

/// Error information.
#[derive(Debug, Clone, Serialize, Deserialize, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
pub struct ErrorInfo {
    /// Error code.
    pub code: ErrorCode,
    /// Human-readable error message.
    pub message: String,
}

/// Error codes for structured error handling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Error, TS)]
#[ts(export, export_to = "../../../packages/agent-rdp/src/generated/")]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// Not connected to an RDP server.
    #[error("not connected")]
    NotConnected,

    /// Already connected to an RDP server.
    #[error("already connected")]
    AlreadyConnected,

    /// Failed to establish RDP connection.
    #[error("connection failed")]
    ConnectionFailed,

    /// Authentication failed.
    #[error("authentication failed")]
    AuthenticationFailed,

    /// Connection timed out.
    #[error("timeout")]
    Timeout,

    /// Invalid request parameters.
    #[error("invalid request")]
    InvalidRequest,

    /// Requested feature is not supported.
    #[error("not supported")]
    NotSupported,

    /// Internal daemon error.
    #[error("internal error")]
    InternalError,

    /// Session not found.
    #[error("session not found")]
    SessionNotFound,

    /// IPC communication error.
    #[error("ipc error")]
    IpcError,

    /// Daemon not running.
    #[error("daemon not running")]
    DaemonNotRunning,

    /// The daemon process is alive and accepted the connection, but did not
    /// answer a ping in time. Distinct from `DaemonNotRunning` because the
    /// right reaction is the opposite: wait and retry, do not reconnect - a
    /// daemon busy with a long operation is not a dead one, and tearing it
    /// down costs a full reconnect.
    #[error("daemon unresponsive")]
    DaemonUnresponsive,

    /// The running daemon was started by a different version of agent-rdp
    /// than the CLI issuing the command - typically an upgrade while the
    /// daemon kept running. Only `connect` replaces it; every other command
    /// refuses, because silently talking to the old daemon is exactly how
    /// "the fix didn't change anything" reports happen.
    #[error("daemon version mismatch")]
    DaemonVersionMismatch,

    /// `file pull --max-age`: the remote file is older than allowed - the
    /// command that was supposed to produce it did not write it.
    #[error("stale file")]
    StaleFile,

    /// Clipboard operation failed.
    #[error("clipboard error")]
    ClipboardError,

    /// Drive mapping error.
    #[error("drive error")]
    DriveError,

    /// Automation agent not running.
    #[error("automation not enabled")]
    AutomationNotEnabled,

    /// Automation agent error.
    #[error("automation error")]
    AutomationError,

    /// The request reached the automation agent but no reply came back, so
    /// whether it took effect is unknown. Distinct from `AutomationError`
    /// because retrying is unsafe: the action may already have been applied.
    #[error("automation indeterminate")]
    AutomationIndeterminate,

    /// Element not found.
    #[error("element not found")]
    ElementNotFound,

    /// Stale element reference.
    #[error("stale reference")]
    StaleRef,

    /// Automation command failed.
    #[error("command failed")]
    CommandFailed,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_success_response() {
        let resp = Response::success(ResponseData::Connected {
            automation_ready: None,
            automation_error: None,
            host: "192.168.1.100".to_string(),
            width: 1920,
            height: 1080,
        });

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"success\":true"));
        assert!(json.contains("\"type\":\"connected\""));
    }

    #[test]
    fn test_error_response() {
        let resp = Response::error(ErrorCode::ConnectionFailed, "Connection refused");

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"success\":false"));
        assert!(json.contains("\"code\":\"connection_failed\""));
    }

    #[test]
    fn test_screenshot_response() {
        let resp = Response::success(ResponseData::Screenshot {
            width: 1920,
            height: 1080,
            format: "png".to_string(),
            base64: "iVBORw0KGgo...".to_string(),
            offset_x: None,
            offset_y: None,
            frame_age_ms: 0,
            frame_seq: 0,
            frame_hash: "0000000000000000".to_string(),
        });

        let json = serde_json::to_string(&resp).unwrap();
        assert!(json.contains("\"type\":\"screenshot\""));
    }
}
