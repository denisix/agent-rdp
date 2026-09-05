//! Output formatting for CLI responses.

use agent_rdp_protocol::Response;

/// Output formatter.
pub struct Output {
    json: bool,
    /// Session and command, when known: every CLI-side error is then also
    /// appended to the session's transcript, so a run of
    /// `daemon_not_running`/`watchdog_timeout` verdicts shows which command
    /// hit them and when - the daemon never saw those.
    context: Option<(String, String)>,
}

impl Output {
    /// Create a new output formatter.
    pub fn new(json: bool) -> Self {
        Self { json, context: None }
    }

    /// A formatter that also records CLI-side errors to the session's
    /// transcript, labelled with the command.
    pub fn with_context(json: bool, session: &str, command: &str) -> Self {
        Self {
            json,
            context: Some((session.to_string(), command.to_string())),
        }
    }

    /// Append a CLI-side error to the transcript without printing it.
    pub fn record_error(&self, code: &str, message: &str) {
        let Some((session, command)) = &self.context else {
            return;
        };
        // The unresponsive verdict quotes a log tail; keep the line short.
        let brief: String = message.chars().take(512).collect();
        agent_rdp_daemon::transcript::append_event(
            session,
            serde_json::json!({
                "cli_error": { "command": command, "code": code, "message": brief }
            }),
        );
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
                        "The UI Automation agent did not start: {}. The daemon keeps retrying \
                         (`automate status` shows last_error/next_retry_secs); `automate \
                         restart` forces a retry now.",
                        reason
                    ),
                    None => "The UI Automation agent did not start. The daemon keeps retrying \
                             (`automate status` shows progress); `automate restart` forces a \
                             retry now."
                        .to_string(),
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
                            "Warning: the UI Automation agent did not start: {}. Do not \
                             reconnect for this - the daemon keeps retrying (`automate status` \
                             shows last_error/next_retry_secs) and `automate restart` forces \
                             a retry now; see the session's daemon.log for details.",
                            reason
                        ),
                        None => eprintln!(
                            "Warning: the UI Automation agent did not start, so `automate` \
                             commands will not work yet. The daemon keeps retrying (`automate \
                             status` shows progress); `automate restart` forces a retry now; \
                             see the session's daemon.log for why."
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
                let cli_version = crate::session_manager::CLI_VERSION;
                println!("CLI version: {}", info.cli_version.as_deref().unwrap_or(cli_version));
                if info.daemon_version.is_empty() {
                    println!(
                        "Daemon version: unknown (predates {}; run `agent-rdp connect` to replace it)",
                        cli_version
                    );
                } else if info.daemon_version != cli_version {
                    println!(
                        "Daemon version: {} (CLI is {} - run `agent-rdp connect` to replace it)",
                        info.daemon_version, cli_version
                    );
                } else {
                    println!("Daemon version: {}", info.daemon_version);
                }
                println!("Uptime: {}s", info.uptime_secs);
                if let Some(age_ms) = info.last_frame_age_ms {
                    println!("Last frame from server: {}s ago", age_ms / 1000);
                }
                if let Some(drop) = &info.last_disconnect {
                    println!(
                        "Last transport drop: {} ({}s ago): {} - the daemon stayed up; `connect` re-establishes the session",
                        drop.at, drop.seconds_ago, drop.reason
                    );
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
            ResponseData::Pong { version } => {
                println!("Pong (daemon {})", if version.is_empty() { "unversioned" } else { version });
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
                    println!("Agent version: {}", version);
                }
                if let Some(ref version) = status.daemon_version {
                    println!("Daemon version: {}", version);
                }
                if let Some(ref version) = status.cli_version {
                    println!("CLI version: {}", version);
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
                if status.relaunches > 0 {
                    println!("Relaunches since connect: {}", status.relaunches);
                }
                if status.total_launches > 0 {
                    // `relaunches` resets on every connect, so on its own it
                    // cannot tell "up all day" from "rebuilt an hour ago".
                    println!(
                        "Agent launches against this host: {} (includes each connect's bootstrap)",
                        status.total_launches
                    );
                }
                if let Some(ref err) = status.last_error {
                    println!("Last launch error: {}", err);
                }
                match status.next_retry_secs {
                    Some(0) => println!(
                        "Next automatic relaunch: as soon as the session has been idle for 2 minutes"
                    ),
                    Some(secs) => println!("Next automatic relaunch: in {}s (once the session is idle)", secs),
                    None if !status.agent_running => println!(
                        "Next automatic relaunch: none scheduled - `automate restart` relaunches now"
                    ),
                    None => {}
                }
            }
            ResponseData::RunResult(result) => {
                // Status lines go to stderr so stdout carries only what the
                // remote command actually printed - otherwise every parser
                // has to strip an "Exit code: 0" line the program never
                // produced.
                let remote_time = |unix: u64| {
                    agent_rdp_daemon::timefmt::utc_rfc3339(
                        std::time::UNIX_EPOCH + std::time::Duration::from_secs(unix),
                    )
                };
                if result.replayed {
                    match result.replayed_at_unix {
                        Some(at) => eprintln!(
                            "(replayed from the journal - the command was not run again; it originally ran at {} by the remote clock)",
                            remote_time(at)
                        ),
                        None => eprintln!("(replayed from the journal - the command was not run again)"),
                    }
                }
                if let Some(started) = result.started_unix {
                    eprintln!("Started: {} (remote clock)", remote_time(started));
                }
                if let Some(finished) = result.finished_unix {
                    eprintln!("Finished: {} (remote clock)", remote_time(finished));
                }
                if let Some(code) = result.exit_code {
                    eprintln!("Exit code: {}", code);
                }
                // A non-zero exit is where "what did it actually run" matters;
                // the JSON output always carries `command_line`.
                if matches!(result.exit_code, Some(code) if code != 0) {
                    if let Some(ref line) = result.command_line {
                        eprintln!("Command line as executed by the agent: {}", line);
                    }
                }
                if let Some(ref stdout) = result.stdout {
                    if !stdout.is_empty() {
                        println!("{}", stdout);
                    }
                }
                if let Some(note) = run_capture_note(result) {
                    eprintln!("{}", note);
                }
                if let Some(ref stderr) = result.stderr {
                    if !stderr.is_empty() {
                        eprintln!("{}", stderr);
                    }
                }
                if let Some(pid) = result.pid {
                    eprintln!("Process ID: {}", pid);
                }
                if result.early_exit {
                    eprintln!(
                        "Process exited immediately{} - it did not start properly; run it with \
                         --wait to see its stderr",
                        result
                            .exit_code
                            .map(|c| format!(" with code {}", c))
                            .unwrap_or_default()
                    );
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
                } else if result.pending {
                    // Printing nothing here is indistinguishable from a poll
                    // that never reached the agent.
                    eprintln!(
                        "Process {} is running; no output since the last poll",
                        result.pid
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
                if let (Some(modified), Some(age)) = (&result.modified, result.age_secs) {
                    println!("Modified: {} ({}s ago by the remote clock)", modified, age);
                }
            }
            ResponseData::FileStat(stat) => {
                if !stat.exists {
                    println!("{}: does not exist", stat.path);
                } else if stat.is_directory {
                    println!("{}: directory", stat.path);
                } else {
                    println!("{}: file", stat.path);
                    if let Some(size) = stat.size {
                        println!("Size: {} bytes", size);
                    }
                    if let Some(sha) = &stat.sha256 {
                        println!("SHA-256: {}", sha);
                    }
                    if let (Some(modified), Some(age)) = (&stat.modified, stat.age_secs) {
                        println!("Modified: {} ({}s ago by the remote clock)", modified, age);
                    }
                }
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
        self.record_error(code, message);
        if self.json {
            let response = agent_rdp_protocol::Response {
                success: false,
                data: None,
                error: Some(agent_rdp_protocol::ErrorInfo {
                    code: error_code_for(code),
                    message: message.to_string(),
                }),
            };
            println!("{}", serde_json::to_string(&response).unwrap());
        } else {
            eprintln!("Error [{}]: {}", code, message);
        }
    }
}

/// Map a CLI-side error code string to the protocol enum for `--json` output.
///
/// This used to hard-code `InternalError`, so a JSON consumer could not tell
/// `daemon_not_running` (reconnect) from `daemon_unresponsive` (wait) from
/// `watchdog_timeout` (the daemon may be fine) - the human-readable path
/// printed the right code all along, the machine-readable one lost it.
pub fn error_code_for(code: &str) -> agent_rdp_protocol::ErrorCode {
    use agent_rdp_protocol::ErrorCode;
    match code {
        "daemon_not_running" => ErrorCode::DaemonNotRunning,
        "daemon_unresponsive" => ErrorCode::DaemonUnresponsive,
        "daemon_version_mismatch" => ErrorCode::DaemonVersionMismatch,
        "watchdog_timeout" | "timeout" => ErrorCode::Timeout,
        "not_connected" => ErrorCode::NotConnected,
        "invalid_request" => ErrorCode::InvalidRequest,
        "ipc_error" | "cli_error" => ErrorCode::IpcError,
        _ => ErrorCode::InternalError,
    }
}

/// The one-line note that says what happened to a run's stdout when there
/// is none to print: a waited run that printed nothing, a streamed launch
/// whose output is waiting in `run-poll`, or a detached launch where nothing
/// was captured at all. `None` when stdout was printed or nothing applies.
pub fn run_capture_note(result: &agent_rdp_protocol::RunResult) -> Option<String> {
    match result.stdout {
        Some(ref stdout) if !stdout.is_empty() => None,
        Some(_) => Some("(no stdout captured: the command printed nothing)".to_string()),
        // A replayed streamed launch belongs to an earlier agent process;
        // its spool may be gone, so do not promise a run-poll.
        None if result.streamed && result.replayed => Some(
            "(streaming launch replayed from the journal: its output was captured by an \
             earlier agent process and may no longer be pollable)"
                .to_string(),
        ),
        None if result.streamed => result
            .pid
            .map(|pid| format!("(streaming: collect output with `automate run-poll {}`)", pid)),
        None if result.exit_code.is_none() && result.pid.is_some() => {
            Some("(detached: output is not captured - use --wait or --stream)".to_string())
        }
        None => None,
    }
}

#[cfg(test)]
mod run_capture_note_tests {
    use super::run_capture_note;
    use agent_rdp_protocol::RunResult;

    fn result() -> RunResult {
        RunResult {
            exit_code: None,
            stdout: None,
            stderr: None,
            pid: None,
            replayed: false,
            early_exit: false,
            started_unix: None,
            finished_unix: None,
            command_line: None,
            replayed_at_unix: None,
            streamed: false,
        }
    }

    #[test]
    fn waited_runs_distinguish_empty_from_uncaptured() {
        let printed = RunResult { exit_code: Some(0), stdout: Some("hi".into()), ..result() };
        assert_eq!(run_capture_note(&printed), None);
        let empty = RunResult { exit_code: Some(0), stdout: Some(String::new()), ..result() };
        assert!(run_capture_note(&empty).unwrap().contains("printed nothing"));
        // A replayed waited result with empty stdout is still "printed nothing".
        let replayed = RunResult { replayed: true, ..empty };
        assert!(run_capture_note(&replayed).unwrap().contains("printed nothing"));
    }

    #[test]
    fn streamed_and_detached_launches_get_their_own_notes() {
        let streamed = RunResult { pid: Some(7), streamed: true, ..result() };
        assert_eq!(
            run_capture_note(&streamed).as_deref(),
            Some("(streaming: collect output with `automate run-poll 7`)")
        );
        let replayed_stream = RunResult { pid: Some(7), streamed: true, replayed: true, ..result() };
        assert!(run_capture_note(&replayed_stream).unwrap().contains("may no longer be pollable"));
        let detached = RunResult { pid: Some(7), ..result() };
        assert!(run_capture_note(&detached).unwrap().starts_with("(detached:"));
        // An early-exited detached launch has an exit code and no stdout.
        let early = RunResult { pid: Some(7), exit_code: Some(1), early_exit: true, ..result() };
        assert_eq!(run_capture_note(&early), None);
    }
}
