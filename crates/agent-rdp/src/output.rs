//! Output formatting for CLI responses.

use agent_rdp_protocol::Response;

/// Output formatter.
pub struct Output {
    json: bool,
}

impl Output {
    /// Create a new output formatter.
    pub fn new(json: bool) -> Self {
        Self { json }
    }

    /// Whether JSON output is enabled.
    pub fn is_json(&self) -> bool {
        self.json
    }

    /// Print a response.
    pub fn print_response(&self, response: &Response) {
        if self.json {
            // `Connected { automation_ready: Some(false), .. }` still has
            // `success: true` (RDP itself connected - only automation
            // failed), so a JSON caller checking only `success` or skimming
            // `data` for a familiar field sees a clean success. Splice in an
            // explicit `warning` key so the failure can't be missed without
            // specifically knowing to check `automation_ready`.
            if let Some(agent_rdp_protocol::ResponseData::Connected {
                automation_ready: Some(false),
                automation_error,
                ..
            }) = &response.data
            {
                let mut value = serde_json::to_value(response).unwrap();
                let warning = match automation_error {
                    Some(reason) => format!(
                        "The UI Automation agent did not start: {}. Reconnect to retry.",
                        reason
                    ),
                    None => "The UI Automation agent did not start. Reconnect to retry.".to_string(),
                };
                if let Some(data) = value.get_mut("data") {
                    data["warning"] = serde_json::Value::String(warning);
                }
                println!("{}", value);
                return;
            }
            println!("{}", serde_json::to_string(response).unwrap());
        } else if response.success {
            if let Some(ref data) = response.data {
                self.print_data(data);
            } else {
                println!("OK");
            }
        } else {
            // Error case - always print something
            if let Some(ref error) = response.error {
                eprintln!("Error [{}]: {}", error.code, error.message);
            } else {
                eprintln!("Error: Command failed (no details provided)");
            }
        }
    }

    /// Print response data in human-readable format.
    fn print_data(&self, data: &agent_rdp_protocol::ResponseData) {
        use agent_rdp_protocol::ResponseData;

        match data {
            ResponseData::Ok => {
                println!("OK");
            }
            ResponseData::Connected { host, width, height, automation_ready, automation_error } => {
                println!("Connected to {} ({}x{})", host, width, height);
                if *automation_ready == Some(false) {
                    match automation_error {
                        Some(reason) => eprintln!(
                            "Warning: the UI Automation agent did not start: {}. Reconnect to \
                             retry; see the session's daemon.log for details.",
                            reason
                        ),
                        None => eprintln!(
                            "Warning: the UI Automation agent did not start, so `automate` \
                             commands will not work. Reconnect to retry; see the session's \
                             daemon.log for why."
                        ),
                    }
                }
            }
            ResponseData::Screenshot { width, height, format, .. } => {
                println!("Screenshot: {}x{} ({})", width, height, format);
            }
            ResponseData::Clipboard { text } => {
                println!("{}", text);
            }
            ResponseData::SessionInfo(info) => {
                println!("Session: {}", info.name);
                println!("State: {:?}", info.state);
                if let Some(ref host) = info.host {
                    println!("Host: {}", host);
                }
                if let (Some(w), Some(h)) = (info.width, info.height) {
                    println!("Resolution: {}x{}", w, h);
                }
                println!("PID: {}", info.pid);
                println!("Uptime: {}s", info.uptime_secs);
                if let Some(age_ms) = info.last_frame_age_ms {
                    println!("Last frame from server: {}s ago", age_ms / 1000);
                }
            }
            ResponseData::DriveList { drives } => {
                if drives.is_empty() {
                    println!("No drives mapped");
                } else {
                    for drive in drives {
                        println!("{}: {}", drive.name, drive.path);
                    }
                }
            }
            ResponseData::SessionList { sessions } => {
                if sessions.is_empty() {
                    println!("No active sessions");
                } else {
                    for session in sessions {
                        let host = session.host.as_deref().unwrap_or("-");
                        println!("{}: {:?} ({})", session.name, session.state, host);
                    }
                }
            }
            ResponseData::Pong => {
                println!("Pong");
            }
            ResponseData::Snapshot(snapshot) => {
                // Print full accessibility tree like agent-browser
                println!("Snapshot ID: {}", snapshot.snapshot_id);
                println!("Elements: {}", snapshot.ref_count);
                if snapshot.truncated {
                    println!(
                        "[Truncated at depth {} - use -d to increase or -s to scope to a window]",
                        snapshot.max_depth
                    );
                }
                println!();
                self.print_element_tree(&snapshot.root, 0);
            }
            ResponseData::Element(element) => {
                if let Some(ref name) = element.name {
                    println!("Name: {}", name);
                }
                if let Some(ref value) = element.value {
                    println!("Value: {}", value);
                }
                if !element.states.is_empty() {
                    println!("States: {}", element.states.join(", "));
                }
                if let Some(ref bounds) = element.bounds {
                    println!("Bounds: {}x{} at ({}, {})",
                        bounds.width, bounds.height, bounds.x, bounds.y);
                }
            }
            ResponseData::WindowList { windows } => {
                if windows.is_empty() {
                    println!("No windows found");
                } else {
                    for window in windows {
                        let process = window.process_name.as_deref().unwrap_or("-");
                        println!("{}: {} ({})", window.title, process,
                            if window.minimized { "minimized" }
                            else if window.maximized { "maximized" }
                            else { "normal" });
                    }
                }
            }
            ResponseData::AutomationStatus(status) => {
                println!("Agent running: {}", status.agent_running);
                if let Some(pid) = status.agent_pid {
                    println!("Agent PID: {}", pid);
                }
                if let Some(ref version) = status.version {
                    println!("Version: {}", version);
                }
                if !status.capabilities.is_empty() {
                    println!("Capabilities: {}", status.capabilities.join(", "));
                }
                if let Some(uptime) = status.uptime_secs {
                    println!("Agent uptime: {}s", uptime);
                }
                match status.last_rtt_ms {
                    Some(rtt) => println!("Last request RTT: {}ms", rtt),
                    None => println!("Last request RTT: none yet"),
                }
                if status.consecutive_failures > 0 {
                    println!(
                        "Consecutive failures: {} (channel degraded, not necessarily dead - \
                         reconnecting invalidates all refs, so prefer probing status again first)",
                        status.consecutive_failures
                    );
                }
            }
            ResponseData::RunResult(result) => {
                // Status lines go to stderr so stdout carries only what the
                // remote command actually printed - otherwise every parser
                // has to strip an "Exit code: 0" line the program never
                // produced.
                if let Some(code) = result.exit_code {
                    eprintln!("Exit code: {}", code);
                }
                if let Some(ref stdout) = result.stdout {
                    if !stdout.is_empty() {
                        println!("{}", stdout);
                    }
                }
                if let Some(ref stderr) = result.stderr {
                    if !stderr.is_empty() {
                        eprintln!("{}", stderr);
                    }
                }
                if let Some(pid) = result.pid {
                    eprintln!("Process ID: {}", pid);
                }
            }
            ResponseData::RunPollResult(result) => {
                if !result.stdout_chunk.is_empty() {
                    println!("{}", result.stdout_chunk);
                }
                if !result.stderr_chunk.is_empty() {
                    eprintln!("{}", result.stderr_chunk);
                }
                if result.exited {
                    eprintln!(
                        "Process {} exited{}",
                        result.pid,
                        result
                            .exit_code
                            .map(|c| format!(" (code {})", c))
                            .unwrap_or_default()
                    );
                }
            }
            ResponseData::LocateResult(result) => {
                if result.matches.is_empty() {
                    println!("No matches found ({} words detected)", result.total_words);
                } else {
                    println!("Found {} match(es):", result.matches.len());
                    for m in &result.matches {
                        println!("  '{}' at ({}, {}) size {}x{} - center: ({}, {})",
                            m.text, m.x, m.y, m.width, m.height, m.center_x, m.center_y);
                    }
                }
            }
            ResponseData::ClickAtResult(result) => {
                // The click-at command renders this itself with more context;
                // this arm only fires for callers reaching it another way.
                if result.clicked {
                    println!("Clicked at ({}, {})", result.x, result.y);
                } else {
                    println!(
                        "Click refused: {} text regions near ({}, {})",
                        result.nearby.len(),
                        result.x,
                        result.y
                    );
                }
            }
            ResponseData::FileTransferResult(result) => {
                println!(
                    "Transferred {} bytes to {} in {} chunk(s)",
                    result.bytes, result.path, result.chunks
                );
                println!("SHA-256: {}", result.sha256);
            }
            ResponseData::ClickResult(result) => {
                if result.method == "double_click" {
                    println!("Double-clicked at ({}, {})", result.x.unwrap_or(0), result.y.unwrap_or(0));
                } else {
                    println!("Clicked at ({}, {})", result.x.unwrap_or(0), result.y.unwrap_or(0));
                }
            }
        }
    }

    /// Print an element tree in compact Playwright-like aria format.
    /// Format: - role "name" [ref=eN, id=..., ...]
    fn print_element_tree(&self, element: &agent_rdp_protocol::AccessibilityElement, depth: usize) {
        let indent = "  ".repeat(depth);

        // Build the main line: - role "name"
        let mut line = format!("{}- {}", indent, element.role);

        // Add name if present
        if let Some(ref name) = element.name {
            if !name.is_empty() {
                line.push_str(&format!(" \"{}\"", name));
            }
        }

        // Build attributes in brackets
        let mut attrs = Vec::new();

        // Ref is always first attribute (with "e" prefix)
        if let Some(r) = element.r#ref {
            attrs.push(format!("ref=e{}", r));
        }

        if let Some(ref auto_id) = element.automation_id {
            if !auto_id.is_empty() {
                attrs.push(format!("id={}", auto_id));
            }
        }

        if let Some(ref class) = element.class_name {
            if !class.is_empty() {
                attrs.push(format!("class={}", class));
            }
        }

        if let Some(ref value) = element.value {
            if !value.is_empty() {
                attrs.push(format!("value=\"{}\"", value));
            }
        }

        // Add disabled tag if element is interactive but not enabled
        // Interactive = focusable OR has interactive patterns
        let interactive_patterns = [
            "invoke",
            "value",
            "toggle",
            "selectionitem",
            "expandcollapse",
            "rangevalue",
            "scroll",
        ];
        let is_focusable = element.states.contains(&"focusable".to_string());
        let has_interactive_pattern = element
            .patterns
            .iter()
            .any(|p| interactive_patterns.contains(&p.as_str()));
        let is_interactive = is_focusable || has_interactive_pattern;

        if is_interactive && !element.states.contains(&"enabled".to_string()) {
            attrs.push("disabled".to_string());
        }

        if !attrs.is_empty() {
            line.push_str(&format!(" [{}]", attrs.join(", ")));
        }

        println!("{}", line);

        // Recurse into children
        for child in &element.children {
            self.print_element_tree(child, depth + 1);
        }
    }

    /// Print an error message.
    pub fn print_error(&self, code: &str, message: &str) {
        if self.json {
            let response = agent_rdp_protocol::Response {
                success: false,
                data: None,
                error: Some(agent_rdp_protocol::ErrorInfo {
                    code: agent_rdp_protocol::ErrorCode::InternalError,
                    message: message.to_string(),
                }),
            };
            println!("{}", serde_json::to_string(&response).unwrap());
        } else {
            eprintln!("Error [{}]: {}", code, message);
        }
    }
}
