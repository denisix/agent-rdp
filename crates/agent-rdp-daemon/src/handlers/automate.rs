//! Automation handler for Windows UI Automation.

use std::sync::Arc;

use agent_rdp_protocol::{
    AccessibilityElement, AccessibilitySnapshot, AutomateRequest, AutomationStatus, ClickResult,
    ElementBounds, ElementValue, ErrorCode, Response, ResponseData, RunPollResult, RunResult,
    WindowInfo,
};
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::automation::SharedAutomationState;
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

    // Shared with the relaunch supervisor; serialized by `relaunch_in_flight`
    // so a manual restart and an automatic one never drive the Run dialog
    // at the same time.
    match crate::automation::relaunch_agent(rdp_session, automation_state).await {
        Ok(()) => {
            let state = automation_state.lock().await;
            let dvc_ipc = state.dvc_ipc.as_ref();
            Response::success(ResponseData::AutomationStatus(AutomationStatus {
                agent_running: true,
                agent_pid: state.agent_pid,
                capabilities: dvc_ipc.map(|ipc| ipc.capabilities()).unwrap_or_default(),
                version: dvc_ipc.and_then(|ipc| ipc.agent_version()),
                log_path: None,
                relaunches: state.relaunches,
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

    // Send request to PowerShell agent via DVC, giving commands that carry
    // their own budget the time they asked for.
    let response_timeout = request_timeout(&request, ipc.default_timeout());
    match ipc.send_request_with_timeout(&request, response_timeout).await {
        Ok(data) => {
            let mut response = convert_response(request, data, &ipc);
            if let Some(ResponseData::AutomationStatus(ref mut status)) = response.data {
                status.relaunches = automation_state.lock().await.relaunches;
            }
            response
        }
        Err(e) => {
            // A lost reply is not the same as a failed action - surface it under
            // its own code so callers can avoid retrying into a double-apply.
            if let Some(indeterminate) =
                e.downcast_ref::<crate::automation::DvcIndeterminate>()
            {
                error!("Automation request outcome unknown: {}", e);
                return resolve_indeterminate(&ipc, &request, &indeterminate.request_id, &e).await;
            }
            error!("Automation request failed: {}", e);
            Response::error(ErrorCode::AutomationError, stale_ref_hint(e.to_string()))
        }
    }
}

/// Turn "we don't know what happened" into a definite answer where possible.
///
/// The agent keeps the results of the last few requests, so once it is
/// answering again it can say whether a given request ever ran. That matters
/// most for the case this whole error exists to protect: a mutating command
/// whose acknowledgement was lost, where retrying blindly risks applying it
/// twice and not retrying risks skipping it entirely.
async fn resolve_indeterminate(
    ipc: &crate::automation::DvcIpc,
    request: &AutomateRequest,
    request_id: &str,
    original: &anyhow::Error,
) -> Response {
    if !ipc.capabilities().iter().any(|c| c == "query_result") {
        // Older agent - nothing to ask.
        return Response::error(
            ErrorCode::AutomationIndeterminate,
            indeterminate_message(request, original),
        );
    }

    let query = AutomateRequest::QueryResult { id: request_id.to_string() };

    // The agent is single-threaded: if it is still executing the original
    // command, the query queues behind it and answers once it frees up.
    // Retrying with backoff is what turns "busy" into an answer rather than
    // a second unknown.
    for attempt in 1..=QUERY_RESULT_ATTEMPTS {
        match ipc.send_request_with_timeout(&query, QUERY_RESULT_TIMEOUT).await {
            Ok(value) => {
                if value["known"].as_bool().unwrap_or(false) {
                    info!(
                        "Recovered the outcome of request {} from the agent's journal",
                        request_id
                    );
                    if value["success"].as_bool().unwrap_or(false) {
                        let data = value.get("data").cloned().unwrap_or(serde_json::Value::Null);
                        return convert_response(request.clone(), data, ipc);
                    }
                    let message = value["error"]["message"]
                        .as_str()
                        .unwrap_or("the command failed on the agent")
                        .to_string();
                    return Response::error(ErrorCode::AutomationError, stale_ref_hint(message));
                }

                // The agent is responsive and has no record of it, so it
                // never ran - the one case where retrying is unambiguously
                // safe, and worth saying outright.
                return Response::error(
                    ErrorCode::AutomationError,
                    format!(
                        "The automation agent never received request {} - it did not run, so                          retrying is safe.",
                        request_id
                    ),
                );
            }
            Err(_) if attempt < QUERY_RESULT_ATTEMPTS => {
                tokio::time::sleep(QUERY_RESULT_BACKOFF * attempt).await;
            }
            Err(_) => break,
        }
    }

    // Still busy: the original command is most likely still running.
    Response::error(
        ErrorCode::AutomationIndeterminate,
        format!(
            "{} The agent is still busy; once it responds, the outcome of request {} can be              recovered rather than guessed.",
            indeterminate_message(request, original),
            request_id
        ),
    )
}

/// How many times to ask the agent about a lost request before giving up.
const QUERY_RESULT_ATTEMPTS: u32 = 3;

/// Deadline for a single journal lookup.
const QUERY_RESULT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Base backoff between lookups, multiplied by the attempt number.
const QUERY_RESULT_BACKOFF: std::time::Duration = std::time::Duration::from_secs(2);

fn indeterminate_message(request: &AutomateRequest, error: &anyhow::Error) -> String {
    if is_read_only(request) {
        format!("{} This command is read-only - retrying is safe.", error)
    } else {
        error.to_string()
    }
}

/// How long to wait for the agent's reply to a given request.
///
/// Most commands are fast and keep the default ceiling. `run --wait` and
/// `wait-for` carry an explicit budget for how long the *remote* work may
/// take; the DVC reply cannot arrive before that work finishes, so the
/// transport deadline has to cover it plus room for the round trip itself.
/// Without this every long command reported `indeterminate` at the default
/// while the agent was still dutifully running it.
fn request_timeout(request: &AutomateRequest, default: std::time::Duration) -> std::time::Duration {
    let command_budget_ms = match request {
        AutomateRequest::Run { wait: true, timeout_ms, .. } => Some(*timeout_ms),
        AutomateRequest::WaitFor { timeout_ms, .. } => Some(*timeout_ms),
        _ => None,
    };

    match command_budget_ms {
        Some(ms) => std::time::Duration::from_millis(ms).saturating_add(default),
        None => default,
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
    // The classification lives in the protocol crate so the CLI can use the
    // same answer for its dropped-connection retry.
    request.is_read_only()
}

#[cfg(test)]
mod is_read_only_tests {
    use super::*;
    use agent_rdp_protocol::WindowAction;

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

    fn run_request(wait: bool, timeout_ms: u64) -> AutomateRequest {
        AutomateRequest::Run {
            command: "build.cmd".into(),
            args: Vec::new(),
            wait,
            hidden: false,
            timeout_ms,
            shell: None,
            stream: false,
            idempotency_key: None,
        }
    }

    #[test]
    fn long_run_wait_gets_its_full_budget_plus_transport_slack() {
        let default = std::time::Duration::from_secs(10);
        // A 4-minute command must not be cut off at the 10s default.
        let got = request_timeout(&run_request(true, 240_000), default);
        assert_eq!(got, std::time::Duration::from_secs(250));
    }

    #[test]
    fn wait_for_gets_its_budget_too() {
        let default = std::time::Duration::from_secs(10);
        let request = AutomateRequest::WaitFor {
            selector: "@e1".into(),
            timeout_ms: 60_000,
            state: agent_rdp_protocol::WaitState::Visible,
        };
        assert_eq!(request_timeout(&request, default), std::time::Duration::from_secs(70));
    }

    #[test]
    fn fire_and_forget_run_keeps_the_default() {
        let default = std::time::Duration::from_secs(10);
        // Without --wait the agent replies immediately, so the long
        // process timeout is irrelevant to the transport deadline.
        assert_eq!(request_timeout(&run_request(false, 240_000), default), default);
    }

    #[test]
    fn ordinary_commands_keep_the_default() {
        let default = std::time::Duration::from_secs(10);
        let request = AutomateRequest::Click { selector: "@e1".into(), double_click: false };
        assert_eq!(request_timeout(&request, default), default);
        assert_eq!(request_timeout(&AutomateRequest::Status, default), default);
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
                    // `relaunches` is filled by `handle`, which owns the state.
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
    let stderr = data["stderr"].as_str().map(clean_clixml);
    let pid = data["pid"].as_u64().map(|v| v as u32);
    let replayed = data["replayed"].as_bool().unwrap_or(false);
    let early_exit = data["early_exit"].as_bool().unwrap_or(false);
    let started_unix = data["started_unix"].as_u64();

    Ok(RunResult {
        exit_code,
        stdout,
        stderr,
        pid,
        replayed,
        early_exit,
        started_unix,
    })
}

/// Parse run_poll response from PowerShell agent.
fn parse_run_poll_response(data: serde_json::Value) -> anyhow::Result<RunPollResult> {
    let pid = data["pid"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("run_poll response missing pid"))? as u32;
    let stdout_chunk = data["stdout_chunk"].as_str().unwrap_or("").to_string();
    let stderr_chunk = clean_clixml(data["stderr_chunk"].as_str().unwrap_or(""));
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

/// Turn PowerShell's CLIXML-serialized stderr back into plain text.
///
/// When `powershell.exe` runs with its stderr redirected to a pipe (as the
/// agent's `Invoke-Run` does) it serializes error, warning, progress and
/// verbose records as `#< CLIXML` followed by an `<Objs>` document instead of
/// writing text. A caller reading stderr for a reason then had to dig the
/// actual message out of `<S S="Error">...</S>` elements padded with
/// `_x000D__x000A_`. This keeps the text of error and warning records, drops
/// progress/verbose/debug records (module autoload chatter), and leaves any
/// non-CLIXML text untouched. Best effort: a `run-poll` chunk can split the
/// XML mid-element, and whatever cannot be parsed is passed through as-is.
pub fn clean_clixml(stderr: &str) -> String {
    const MARKER: &str = "#< CLIXML";

    let Some(marker_at) = stderr.find(MARKER) else {
        return stderr.to_string();
    };

    let mut out = String::with_capacity(stderr.len());
    out.push_str(&stderr[..marker_at]);

    let xml = &stderr[marker_at + MARKER.len()..];
    let mut rest = xml;
    let mut extracted_any = false;
    while let Some(start) = rest.find("<S ") {
        let element = &rest[start..];
        let Some(tag_end) = element.find('>') else { break };
        let tag = &element[..tag_end];
        let body_start = tag_end + 1;
        let Some(close) = element[body_start..].find("</S>") else { break };
        let body = &element[body_start..body_start + close];

        let stream = tag
            .split_once("S=\"")
            .and_then(|(_, after)| after.split_once('"'))
            .map(|(name, _)| name)
            .unwrap_or("");
        if matches!(stream, "Error" | "error" | "Warning" | "warning") {
            let text = decode_clixml_text(body);
            if !text.trim().is_empty() {
                out.push_str(&text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                extracted_any = true;
            }
        }
        rest = &element[body_start + close + 4..];
    }

    // Structured records (`<Obj>` with a `<ToString>` rendering) instead of
    // stream strings: keep their text too, so an error serialized that way
    // is not silently dropped. Only when no string records were found, or
    // the same error would appear twice.
    if !extracted_any {
        let mut rest = xml;
        while let Some(start) = rest.find("<ToString>") {
            let body_start = start + "<ToString>".len();
            let Some(close) = rest[body_start..].find("</ToString>") else { break };
            let text = decode_clixml_text(&rest[body_start..body_start + close]);
            if !text.trim().is_empty() {
                out.push_str(&text);
                if !text.ends_with('\n') {
                    out.push('\n');
                }
                extracted_any = true;
            }
            rest = &rest[body_start + close + "</ToString>".len()..];
        }
    }

    // A chunk that carried the marker but no complete element yet (a poll
    // split the XML) - hand the raw bytes back rather than swallowing them.
    if !extracted_any && !xml.contains("</Objs>") && !xml.contains("<S ") {
        out.push_str(MARKER);
        out.push_str(xml);
    }

    out
}

/// Decode CLIXML string escapes: `_x000D_`-style code points and XML entities.
fn decode_clixml_text(body: &str) -> String {
    let mut text = String::with_capacity(body.len());
    let mut rest = body;
    while let Some(at) = rest.find("_x") {
        text.push_str(&rest[..at]);
        let candidate = &rest[at..];
        // `_xHHHH_` is exactly 7 bytes.
        if candidate.len() >= 7 && candidate.as_bytes()[6] == b'_' {
            if let Ok(code) = u32::from_str_radix(&candidate[2..6], 16) {
                if let Some(c) = char::from_u32(code) {
                    text.push(c);
                    rest = &candidate[7..];
                    continue;
                }
            }
        }
        text.push_str("_x");
        rest = &candidate[2..];
    }
    text.push_str(rest);

    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod clixml_tests {
    /// A structured `<Obj>` error record (no `<S S="Error">` strings) must
    /// not be dropped: its `<ToString>` rendering is the message.
    #[test]
    fn structured_error_records_keep_their_text() {
        let input = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">\
<Obj S=\"Error\" RefId=\"0\"><TN RefId=\"0\"><T>System.Management.Automation.ErrorRecord</T></TN>\
<ToString>Exception calling &quot;Execute&quot;: &quot;{Модуль: (12)}: Поле не найдено&quot;_x000D__x000A_</ToString>\
<Props><S N=\"FullyQualifiedErrorId\">ComMethodTargetInvocation</S></Props></Obj></Objs>";
        let cleaned = super::clean_clixml(input);
        assert!(cleaned.contains("Exception calling \"Execute\": \"{Модуль: (12)}: Поле не найдено\""), "{cleaned}");
        assert!(!cleaned.contains("<Obj"));
        assert!(!cleaned.contains("CLIXML"));
    }

    use super::clean_clixml;

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(clean_clixml("just an error\n"), "just an error\n");
        assert_eq!(clean_clixml(""), "");
    }

    #[test]
    fn progress_records_are_dropped_and_errors_kept() {
        let input = "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">\
            <Obj S=\"progress\" RefId=\"0\"><TN RefId=\"0\"><T>System.Management.Automation.PSCustomObject</T>\
            <T>System.Object</T></TN><MS><I64 N=\"SourceId\">1</I64><PR N=\"Record\"><AV>Preparing modules for first use.</AV>\
            <AI>0</AI><Nil /><PI>-1</PI><PC>-1</PC><T>Completed</T><SR>-1</SR><SD> </SD></PR></MS></Obj>\
            <S S=\"Error\">Get-Item : Cannot find path 'C:\\nope'._x000D__x000A_</S>\
            <S S=\"Error\">At line:1 char:1_x000D__x000A_</S></Objs>";
        let cleaned = clean_clixml(input);
        assert_eq!(
            cleaned,
            "Get-Item : Cannot find path 'C:\\nope'.\r\nAt line:1 char:1\r\n"
        );
        assert!(!cleaned.contains("Preparing modules"));
        assert!(!cleaned.contains("<Objs"));
    }

    #[test]
    fn text_before_marker_is_preserved() {
        let input = "warning: plain\n#< CLIXML\n<Objs><S S=\"Error\">boom</S></Objs>";
        assert_eq!(clean_clixml(input), "warning: plain\nboom\n");
    }

    #[test]
    fn xml_entities_are_decoded() {
        let input = "#< CLIXML\n<Objs><S S=\"Error\">a &lt; b &amp;&amp; c &gt; d &quot;q&quot;</S></Objs>";
        assert_eq!(clean_clixml(input), "a < b && c > d \"q\"\n");
    }

    #[test]
    fn cyrillic_survives() {
        let input = "#< CLIXML\n<Objs><S S=\"Error\">Не удается найти путь_x000D__x000A_</S></Objs>";
        assert_eq!(clean_clixml(input), "Не удается найти путь\r\n");
    }

    #[test]
    fn progress_only_block_becomes_empty() {
        let input = "#< CLIXML\n<Objs><Obj S=\"progress\"><PR><AV>Preparing modules for first use.</AV></PR></Obj></Objs>";
        assert_eq!(clean_clixml(input), "");
    }

    #[test]
    fn split_chunk_without_elements_is_passed_through() {
        let input = "#< CLIXML\n<Objs Version=\"1.1.0.1\"";
        assert_eq!(clean_clixml(input), input);
    }
}

/// Parse status response from PowerShell agent.
fn parse_status_response(data: serde_json::Value) -> anyhow::Result<AutomationStatus> {
    let agent_running = data["agent_running"].as_bool().unwrap_or(false);
    let agent_pid = data["agent_pid"].as_u64().map(|v| v as u32);
    let version = data["version"].as_str().map(|s| s.to_string());
    let log_path = data["log_path"].as_str().map(|s| s.to_string());

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
        log_path,
        relaunches: 0,
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
