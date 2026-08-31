//! Automation handler for Windows UI Automation.

use std::sync::Arc;

use agent_rdp_protocol::{
    AccessibilityElement, AccessibilitySnapshot, AutomateRequest, AutomationStatus, ClickResult,
    ElementBounds, ElementValue, ErrorCode, Response, ResponseData, RunPollResult, RunResult,
    WindowAction, WindowInfo,
};
use tokio::sync::Mutex;
use tracing::error;

use crate::automation::{AutomationBootstrap, SharedAutomationState};
use crate::rdp_session::RdpSession;

/// Relaunch the UI Automation agent without a full RDP reconnect.
///
/// Covers the case where the agent died mid-session or never came up after
/// `connect`, but the RDP transport itself is fine - previously the only
/// recovery was a full `disconnect` + `connect --enable-win-automation`,
/// which invalidates every outstanding element ref for no reason (the RDP
/// session, drive mapping, and DVC channel plumbing are all still intact).
pub async fn handle_restart(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
) -> Response {
    {
        let session = rdp_session.lock().await;
        if session.is_none() {
            return Response::error(ErrorCode::NotConnected, "Not connected to RDP server");
        }
    }

    {
        let state = automation_state.lock().await;
        if !state.enabled {
            return Response::error(
                ErrorCode::AutomationNotEnabled,
                "Automation was not initialized at connect - reconnect with \
                 --enable-win-automation, restart cannot add it retroactively",
            );
        }
    }

    {
        let mut state = automation_state.lock().await;
        state.agent_ready = false;
        state.agent_pid = None;
    }

    let session_dir = crate::get_session_dir("");
    let bootstrap = AutomationBootstrap::new(session_dir);

    match bootstrap.launch_and_wait(rdp_session, automation_state).await {
        Ok(()) => {
            let state = automation_state.lock().await;
            let dvc_ipc = state.dvc_ipc.as_ref();
            Response::success(ResponseData::AutomationStatus(AutomationStatus {
                agent_running: true,
                agent_pid: state.agent_pid,
                capabilities: dvc_ipc.map(|ipc| ipc.capabilities()).unwrap_or_default(),
                version: dvc_ipc.and_then(|ipc| ipc.agent_version()),
                uptime_secs: dvc_ipc.and_then(|ipc| ipc.agent_uptime_secs()),
                last_rtt_ms: None,
                consecutive_failures: 0,
            }))
        }
        Err(reason) => Response::error(
            ErrorCode::AutomationError,
            format!("Automation agent restart failed: {}", reason),
        ),
    }
}

/// Handle an automation request.
pub async fn handle(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    request: AutomateRequest,
) -> Response {
    // Check if connected
    {
        let session = rdp_session.lock().await;
        if session.is_none() {
            return Response::error(ErrorCode::NotConnected, "Not connected to RDP server");
        }
    }

    // Check if automation is enabled and agent is ready
    let state = automation_state.lock().await;
    if !state.enabled {
        return Response::error(
            ErrorCode::AutomationNotEnabled,
            "Automation not enabled. Use --enable-win-automation when connecting",
        );
    }

    // Check if DVC IPC is ready (handshake received)
    let dvc_ipc = match state.dvc_ipc.as_ref() {
        Some(ipc) => ipc,
        None => {
            return Response::error(
                ErrorCode::AutomationError,
                "Automation DVC IPC not initialized",
            );
        }
    };

    if !dvc_ipc.is_ready() {
        return Response::error(
            ErrorCode::AutomationError,
            "Automation agent not ready. Agent may still be starting or failed to launch via DVC",
        );
    }

    // Clone the IPC to release the lock before async operation
    let ipc = dvc_ipc.clone();
    drop(state);

    // Send request to PowerShell agent via DVC
    match ipc.send_request(&request).await {
        Ok(data) => convert_response(request, data, &ipc),
        Err(e) => {
            // A lost reply is not the same as a failed action - surface it under
            // its own code so callers can avoid retrying into a double-apply.
            if e.downcast_ref::<crate::automation::DvcIndeterminate>().is_some() {
                error!("Automation request outcome unknown: {}", e);
                let message = if is_read_only(&request) {
                    format!("{} This command is read-only - retrying is safe.", e)
                } else {
                    e.to_string()
                };
                return Response::error(ErrorCode::AutomationIndeterminate, message);
            }
            error!("Automation request failed: {}", e);
            Response::error(ErrorCode::AutomationError, stale_ref_hint(e.to_string()))
        }
    }
}

/// Append a "re-snapshot" hint when the agent's error text suggests the
/// selector's ref is stale after a UI change - the ref is snapshot-scoped by
/// design, but the raw PS error ("Element is disabled", "no longer exists")
/// gives no clue that a fresh `automate snapshot` is the fix rather than a
/// retry or a different selector.
fn stale_ref_hint(message: String) -> String {
    let lower = message.to_lowercase();
    if lower.contains("disabled") || lower.contains("no longer exists") || lower.contains("not found") {
        format!(
            "{} (the ref may be stale after a UI change - re-run `automate snapshot` and retry \
             with a fresh ref)",
            message
        )
    } else {
        message
    }
}

/// Whether an automate command only observes state rather than changing it.
///
/// `AutomationIndeterminate` (a lost DVC reply - the request may or may not
/// have been applied) is the same error for every command today, but the
/// safe response to it is not: retrying a read is always safe, retrying a
/// click or fill risks applying it twice.
fn is_read_only(request: &AutomateRequest) -> bool {
    matches!(
        request,
        AutomateRequest::Snapshot { .. }
            | AutomateRequest::Get { .. }
            | AutomateRequest::Status
            | AutomateRequest::WaitFor { .. }
            | AutomateRequest::RunPoll { .. }
            | AutomateRequest::Window { action: WindowAction::List, .. }
    )
}

#[cfg(test)]
mod is_read_only_tests {
    use super::*;

    #[test]
    fn read_only_commands_are_marked_safe_to_retry() {
        assert!(is_read_only(&AutomateRequest::Status));
        assert!(is_read_only(&AutomateRequest::Get { selector: "e1".into(), property: None }));
        assert!(is_read_only(&AutomateRequest::Snapshot {
            interactive_only: false,
            compact: false,
            max_depth: 10,
            selector: None,
            focused: false,
        }));
        assert!(is_read_only(&AutomateRequest::Window {
            action: WindowAction::List,
            selector: None,
        }));
    }

    #[test]
    fn mutating_commands_are_not_marked_safe_to_retry() {
        assert!(!is_read_only(&AutomateRequest::Click { selector: "e1".into(), double_click: false }));
        assert!(!is_read_only(&AutomateRequest::Fill { selector: "e1".into(), text: "x".into() }));
        assert!(!is_read_only(&AutomateRequest::Window {
            action: WindowAction::Focus,
            selector: None,
        }));
    }

    #[test]
    fn stale_ref_errors_get_a_resnapshot_hint() {
        let hinted = stale_ref_hint("command_failed: Element is disabled".to_string());
        assert!(hinted.contains("re-run `automate snapshot`"));

        let hinted = stale_ref_hint("command_failed: element no longer exists".to_string());
        assert!(hinted.contains("re-run `automate snapshot`"));
    }

    #[test]
    fn unrelated_errors_are_left_alone() {
        let message = "command_failed: invalid parameter".to_string();
        assert_eq!(stale_ref_hint(message.clone()), message);
    }
}

/// Convert the JSON response from PowerShell agent to protocol response.
fn convert_response(
    request: AutomateRequest,
    data: serde_json::Value,
    ipc: &crate::automation::DvcIpc,
) -> Response {
    match request {
        AutomateRequest::Snapshot { .. } => {
            match parse_snapshot_response(data) {
                Ok(snapshot) => Response::success(ResponseData::Snapshot(snapshot)),
                Err(e) => {
                    error!("Failed to parse snapshot response: {}", e);
                    Response::error(ErrorCode::AutomationError, e.to_string())
                }
            }
        }

        AutomateRequest::Get { .. } => {
            match parse_element_response(data) {
                Ok(element) => Response::success(ResponseData::Element(element)),
                Err(e) => {
                    error!("Failed to parse element response: {}", e);
                    Response::error(ErrorCode::AutomationError, e.to_string())
                }
            }
        }

        AutomateRequest::Window { action, .. } => {
            if action == agent_rdp_protocol::WindowAction::List {
                match parse_window_list_response(data) {
                    Ok(windows) => Response::success(ResponseData::WindowList { windows }),
                    Err(e) => {
                        error!("Failed to parse window list response: {}", e);
                        Response::error(ErrorCode::AutomationError, e.to_string())
                    }
                }
            } else {
                Response::ok()
            }
        }

        AutomateRequest::Run { wait, .. } => {
            if wait {
                match parse_run_response(data) {
                    Ok(result) => Response::success(ResponseData::RunResult(result)),
                    Err(e) => {
                        error!("Failed to parse run response: {}", e);
                        Response::error(ErrorCode::AutomationError, e.to_string())
                    }
                }
            } else {
                match parse_run_response(data) {
                    Ok(result) => Response::success(ResponseData::RunResult(result)),
                    Err(_) => Response::ok(),
                }
            }
        }

        AutomateRequest::RunPoll { .. } => {
            match parse_run_poll_response(data) {
                Ok(result) => Response::success(ResponseData::RunPollResult(result)),
                Err(e) => {
                    error!("Failed to parse run_poll response: {}", e);
                    Response::error(ErrorCode::AutomationError, e.to_string())
                }
            }
        }

        AutomateRequest::Status => {
            match parse_status_response(data) {
                Ok(mut status) => {
                    // These come from the daemon's own DVC IPC bookkeeping,
                    // not the PS agent's JSON - the PS side has no visibility
                    // into DVC-layer timing or the daemon's failure counter.
                    status.uptime_secs = ipc.agent_uptime_secs();
                    status.last_rtt_ms = ipc.last_rtt_ms();
                    status.consecutive_failures = ipc.consecutive_failures();
                    Response::success(ResponseData::AutomationStatus(status))
                }
                Err(e) => {
                    error!("Failed to parse status response: {}", e);
                    Response::error(ErrorCode::AutomationError, e.to_string())
                }
            }
        }

        AutomateRequest::Click { .. } => {
            match parse_click_response(data) {
                Ok(result) => Response::success(ResponseData::ClickResult(result)),
                Err(e) => {
                    error!("Failed to parse click response: {}", e);
                    Response::error(ErrorCode::AutomationError, e.to_string())
                }
            }
        }

        // All other actions return simple Ok
        _ => Response::ok(),
    }
}

/// Parse snapshot response from PowerShell agent.
fn parse_snapshot_response(data: serde_json::Value) -> anyhow::Result<AccessibilitySnapshot> {
    let snapshot_id = data["snapshot_id"]
        .as_str()
        .unwrap_or("unknown")
        .to_string();
    let ref_count = data["ref_count"].as_u64().unwrap_or(0) as u32;
    let truncated = data["truncated"].as_bool().unwrap_or(false);
    let max_depth = data["max_depth"].as_u64().unwrap_or(10) as u32;
    let root_data = &data["root"];

    let root = parse_element(root_data)?;

    Ok(AccessibilitySnapshot {
        snapshot_id,
        ref_count,
        truncated,
        max_depth,
        root,
    })
}

/// Parse a single element from the accessibility tree.
fn parse_element(data: &serde_json::Value) -> anyhow::Result<AccessibilityElement> {
    let r#ref = data["ref"].as_u64().map(|v| v as u32);
    let role = data["role"].as_str().unwrap_or("unknown").to_string();
    let name = data["name"].as_str().map(|s| s.to_string());
    let automation_id = data["automation_id"].as_str().map(|s| s.to_string());
    let class_name = data["class_name"].as_str().map(|s| s.to_string());
    let value = data["value"].as_str().map(|s| s.to_string());

    let bounds = if let Some(bounds_data) = data.get("bounds") {
        Some(ElementBounds {
            x: bounds_data["x"].as_i64().unwrap_or(0) as i32,
            y: bounds_data["y"].as_i64().unwrap_or(0) as i32,
            width: bounds_data["width"].as_i64().unwrap_or(0) as i32,
            height: bounds_data["height"].as_i64().unwrap_or(0) as i32,
        })
    } else {
        None
    };

    let states = data["states"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let patterns = data["patterns"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let children = data["children"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| parse_element(v).ok())
                .collect()
        })
        .unwrap_or_default();

    Ok(AccessibilityElement {
        r#ref,
        role,
        name,
        automation_id,
        class_name,
        bounds,
        states,
        value,
        patterns,
        children,
    })
}

/// Parse element value response from PowerShell agent.
fn parse_element_response(data: serde_json::Value) -> anyhow::Result<ElementValue> {
    let name = data["name"].as_str().map(|s| s.to_string());
    let value = data["value"].as_str().map(|s| s.to_string());

    let states = data["states"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let bounds = if let Some(bounds_data) = data.get("bounds") {
        Some(ElementBounds {
            x: bounds_data["x"].as_i64().unwrap_or(0) as i32,
            y: bounds_data["y"].as_i64().unwrap_or(0) as i32,
            width: bounds_data["width"].as_i64().unwrap_or(0) as i32,
            height: bounds_data["height"].as_i64().unwrap_or(0) as i32,
        })
    } else {
        None
    };

    Ok(ElementValue {
        name,
        value,
        states,
        bounds,
    })
}

/// Parse window list response from PowerShell agent.
fn parse_window_list_response(data: serde_json::Value) -> anyhow::Result<Vec<WindowInfo>> {
    let windows_data = data["windows"]
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("Missing windows array"))?;

    let windows = windows_data
        .iter()
        .map(|w| {
            let title = w["title"].as_str().unwrap_or("").to_string();
            let process_name = w["process_name"].as_str().map(|s| s.to_string());
            let process_id = w["process_id"].as_u64().map(|v| v as u32);

            let bounds = if let Some(bounds_data) = w.get("bounds") {
                Some(ElementBounds {
                    x: bounds_data["x"].as_i64().unwrap_or(0) as i32,
                    y: bounds_data["y"].as_i64().unwrap_or(0) as i32,
                    width: bounds_data["width"].as_i64().unwrap_or(0) as i32,
                    height: bounds_data["height"].as_i64().unwrap_or(0) as i32,
                })
            } else {
                None
            };

            let minimized = w["minimized"].as_bool().unwrap_or(false);
            let maximized = w["maximized"].as_bool().unwrap_or(false);

            WindowInfo {
                title,
                process_name,
                process_id,
                bounds,
                minimized,
                maximized,
            }
        })
        .collect();

    Ok(windows)
}

/// Parse run command response from PowerShell agent.
fn parse_run_response(data: serde_json::Value) -> anyhow::Result<RunResult> {
    let exit_code = data["exit_code"].as_i64().map(|v| v as i32);
    let stdout = data["stdout"].as_str().map(|s| s.to_string());
    let stderr = data["stderr"].as_str().map(|s| s.to_string());
    let pid = data["pid"].as_u64().map(|v| v as u32);

    Ok(RunResult {
        exit_code,
        stdout,
        stderr,
        pid,
    })
}

/// Parse run_poll response from PowerShell agent.
fn parse_run_poll_response(data: serde_json::Value) -> anyhow::Result<RunPollResult> {
    let pid = data["pid"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("run_poll response missing pid"))? as u32;
    let stdout_chunk = data["stdout_chunk"].as_str().unwrap_or("").to_string();
    let stderr_chunk = data["stderr_chunk"].as_str().unwrap_or("").to_string();
    let exited = data["exited"].as_bool().unwrap_or(false);
    let exit_code = data["exit_code"].as_i64().map(|v| v as i32);

    Ok(RunPollResult {
        pid,
        stdout_chunk,
        stderr_chunk,
        exited,
        exit_code,
    })
}

/// Parse status response from PowerShell agent.
fn parse_status_response(data: serde_json::Value) -> anyhow::Result<AutomationStatus> {
    let agent_running = data["agent_running"].as_bool().unwrap_or(false);
    let agent_pid = data["agent_pid"].as_u64().map(|v| v as u32);
    let version = data["version"].as_str().map(|s| s.to_string());

    let capabilities = data["capabilities"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    Ok(AutomationStatus {
        agent_running,
        agent_pid,
        capabilities,
        version,
        uptime_secs: None,
        last_rtt_ms: None,
        consecutive_failures: 0,
    })
}

/// Parse click response from PowerShell agent.
fn parse_click_response(data: serde_json::Value) -> anyhow::Result<ClickResult> {
    tracing::debug!("Click response data: {}", data);
    let clicked = data["clicked"].as_bool().unwrap_or(false);
    let method = data["method"].as_str().unwrap_or("unknown").to_string();
    // Handle both int and float (PowerShell may serialize as either)
    let x = data["x"]
        .as_i64()
        .map(|v| v as i32)
        .or_else(|| data["x"].as_f64().map(|v| v as i32));
    let y = data["y"]
        .as_i64()
        .map(|v| v as i32)
        .or_else(|| data["y"].as_f64().map(|v| v as i32));
    tracing::debug!("Parsed click: clicked={}, method={}, x={:?}, y={:?}", clicked, method, x, y);

    Ok(ClickResult {
        clicked,
        method,
        x,
        y,
    })
}
