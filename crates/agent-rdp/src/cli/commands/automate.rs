//! Automate command implementation for Windows UI Automation.

use agent_rdp_protocol::{
    AccessibilityElement, AutomateRequest, AutomationScrollDirection, Request, ResponseData,
    WaitState, WindowAction,
};

use crate::cli::{AutomateAction, AutomateArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: AutomateArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };

    // `restart` isn't an `AutomateRequest` at all - it has to work even when
    // the DVC channel that carries every other automate command is dead,
    // which is exactly the case it exists to recover from - so it's a
    // separate top-level `Request` variant, dispatched before the mapping
    // below.
    if matches!(args.action, AutomateAction::Restart) {
        // Relaunching the agent drives the remote Run dialog and waits for a
        // DVC handshake, retrying up to three times - legitimately close to
        // 90s before it can honestly report failure.
        let restart_timeout_ms = timeout_ms.max(RESTART_MIN_TIMEOUT_MS);
        let response = client.send(&Request::AutomationRestart, restart_timeout_ms).await?;
        output.print_response(&response);
        if !response.success {
            std::process::exit(1);
        }
        return Ok(());
    }

    // `focused` is a snapshot under the hood, but its whole point is to be a
    // one-line answer to "which field am I typing into?", so it gets its own
    // rendering instead of the full tree dump.
    let focused_shorthand = matches!(args.action, AutomateAction::Focused);

    // `--follow` is a CLI-side loop over ordinary polls; remember the budget
    // before the action is consumed by the mapping.
    let follow_budget = match &args.action {
        AutomateAction::RunPoll { follow: true, follow_timeout, .. } => Some(
            std::time::Duration::from_millis(follow_timeout.unwrap_or(DEFAULT_FOLLOW_TIMEOUT_MS)),
        ),
        _ => None,
    };
    if let AutomateAction::Run { wait: true, stream: true, .. } = &args.action {
        eprintln!("Note: --stream is ignored when --wait is set; output is returned directly.");
    }

    let automate_request = match build_request(args.action) {
        Ok(request) => request,
        Err(message) => {
            output.print_error("invalid_request", &message);
            std::process::exit(1);
        }
    };

    let request = Request::Automate(automate_request);
    // The daemon can't answer until the remote command finishes, so the
    // socket read has to outlast the command's own budget - otherwise the
    // CLI gives up on a request the daemon and agent are still working on,
    // and the caller is left not knowing whether it applied.
    let mut response = manager
        .send_with_retry(&mut client, &request, automate_timeout_ms(&request, timeout_ms))
        .await?;

    // The daemon cannot know the CLI's version; filling it in here makes one
    // `automate status` answer "which three versions am I running".
    if let Some(ResponseData::AutomationStatus(status)) = response.data.as_mut() {
        status.cli_version = Some(crate::session_manager::CLI_VERSION.to_string());
    }

    if let Some(budget) = follow_budget {
        return follow_poll(&manager, &mut client, &request, response, budget, output, timeout_ms).await;
    }

    match (focused_shorthand, output.is_json(), response.success, &response.data) {
        (true, false, true, Some(ResponseData::Snapshot(snapshot))) => {
            println!("{}", describe_focused(&snapshot.root));
        }
        (true, true, true, Some(ResponseData::Snapshot(snapshot))) => {
            // Previously fell through to `print_response`, which dumped the
            // full snapshot tree - defeating the point of `focused`, which is
            // to be a *small* answer, for exactly the caller (a JSON-consuming
            // agent) that most needs it small.
            println!("{}", serde_json::to_string(&focused_json(&snapshot.root))?);
        }
        _ => output.print_response(&response),
    }

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}

/// Minimum IPC timeout for `automate restart`: three launch attempts with
/// handshake windows of 25/45/75s, each extendable once while the agent is
/// visibly starting, plus the fixed launch waits - see
/// `launch_and_wait_worst_case` in the daemon.
pub const RESTART_MIN_TIMEOUT_MS: u64 = 320_000;

/// Default wall-clock budget for `run-poll --follow`.
pub const DEFAULT_FOLLOW_TIMEOUT_MS: u64 = 60_000;

/// Interval between polls in `run-poll --follow`.
const FOLLOW_INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);

/// Poll until the process exits or the budget runs out.
///
/// Human output streams chunks as they arrive; `--json` aggregates them into
/// one final `run_poll_result` so the consumer still gets a single document.
/// Any poll error aborts the loop (it propagates as `cli_error`): retrying
/// would race the wall-clock budget against the watchdog.
async fn follow_poll(
    manager: &SessionManager,
    client: &mut crate::ipc_client::IpcClient,
    request: &Request,
    first: agent_rdp_protocol::Response,
    budget: std::time::Duration,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    use std::io::Write;

    let started = std::time::Instant::now();
    let mut current = first;
    let mut all_stdout = String::new();
    let mut all_stderr = String::new();

    loop {
        if !current.success {
            output.print_response(&current);
            std::process::exit(1);
        }
        let Some(ResponseData::RunPollResult(result)) = &current.data else {
            output.print_response(&current);
            return Ok(());
        };

        if output.is_json() {
            all_stdout.push_str(&result.stdout_chunk);
            all_stderr.push_str(&result.stderr_chunk);
        } else {
            if !result.stdout_chunk.is_empty() {
                print!("{}", result.stdout_chunk);
                let _ = std::io::stdout().flush();
            }
            if !result.stderr_chunk.is_empty() {
                eprint!("{}", result.stderr_chunk);
                let _ = std::io::stderr().flush();
            }
        }

        if result.exited {
            if output.is_json() {
                let merged = agent_rdp_protocol::RunPollResult {
                    pid: result.pid,
                    stdout_chunk: std::mem::take(&mut all_stdout),
                    stderr_chunk: std::mem::take(&mut all_stderr),
                    exited: true,
                    exit_code: result.exit_code,
                    finished_unix: result.finished_unix,
                    pending: false,
                };
                output.print_response(&agent_rdp_protocol::Response::success(
                    ResponseData::RunPollResult(merged),
                ));
            } else {
                eprintln!(
                    "Process {} exited{}",
                    result.pid,
                    result.exit_code.map(|c| format!(" (code {})", c)).unwrap_or_default()
                );
            }
            return Ok(());
        }

        if started.elapsed() >= budget {
            if output.is_json() {
                let stdout_chunk = std::mem::take(&mut all_stdout);
                let stderr_chunk = std::mem::take(&mut all_stderr);
                // Same rule as a single poll, applied to the whole window: a
                // follow that ran its budget without ever seeing a byte has
                // to say so, or it hands back the very shape (`exited:false`
                // plus empty chunks) that `pending` exists to disambiguate.
                let pending = stdout_chunk.is_empty() && stderr_chunk.is_empty();
                let merged = agent_rdp_protocol::RunPollResult {
                    pid: result.pid,
                    stdout_chunk,
                    stderr_chunk,
                    exited: false,
                    exit_code: None,
                    finished_unix: None,
                    pending,
                };
                output.print_response(&agent_rdp_protocol::Response::success(
                    ResponseData::RunPollResult(merged),
                ));
            } else {
                eprintln!(
                    "Process {} still running after {}s; run `automate run-poll {} --follow` again to keep collecting",
                    result.pid,
                    budget.as_secs(),
                    result.pid
                );
            }
            return Ok(());
        }

        tokio::time::sleep(FOLLOW_INTERVAL).await;
        current = manager.send_with_retry(client, request, timeout_ms).await?;
    }
}

/// agent-rdp's own `run` options. If one of these shows up among the
/// arguments handed to the remote shell, the caller almost certainly put it
/// after the command (or after `--`), where clap stops parsing options: the
/// process would be launched detached with a bare "Process ID" line, and a
/// background launch is easy to mistake for a hang. Refuse instead.
const RUN_OPTIONS: &[&str] = &[
    "--wait",
    "--hidden",
    "--stream",
    "--process-timeout",
    "--shell",
    "--idempotency-key",
    "--json",
    "--timeout",
    "--session",
    "--stream-port",
];

/// Arguments that PowerShell will re-parse differently from how the shell
/// delivered them. The agent quotes each argument as a single-quoted
/// literal, but a value like `1,2` then reaches a nested `powershell -File`
/// as one token that PowerShell may split, join or treat as an array - a
/// field report saw `-Param 1,2` arrive as `12`. Warn, and point at the
/// form that survives: the whole command as one string with its own quoting.
fn warn_argument_hazards(args: &[String]) {
    let hazardous: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| a.contains(',') || (a.starts_with('-') && a.contains(char::is_whitespace)))
        .collect();
    if hazardous.is_empty() {
        return;
    }
    eprintln!(
        "Note: argument(s) {} contain ',' or embedded whitespace and may be re-parsed by \
         PowerShell (arrays split, values joined). Prefer passing the whole command as one \
         quoted string with its own quoting, e.g. automate run \"powershell -File x.ps1 -Param '1,2'\".",
        hazardous.iter().map(|a| format!("'{}'", a)).collect::<Vec<_>>().join(", ")
    );
}

/// Signs that the caller's *local* shell rewrote the command before agent-rdp
/// saw it - the classic being `"... | ForEach-Object { $_.Line.Trim() }"` in
/// double quotes, where bash expands `$_` to its last argument (usually
/// nothing) and the agent receives `{ .Line.Trim() }`. The agent echoes what
/// it ran as `command_line`; this note explains the discrepancy up front.
/// A `Some` is the warning text.
fn local_shell_expansion_hint(command: &str) -> Option<String> {
    // A script block whose first token starts with `.` is what a vanished
    // `$_` leaves behind - unless it is a relative path or a dot-source
    // (`{ .\build.ps1 }`, `{ . .\lib.ps1 }`), which are legitimate.
    let orphaned_member = command.split('{').skip(1).any(|block| {
        let block = block.trim_start();
        block.starts_with('.')
            && !block.starts_with(".\\")
            && !block.starts_with("./")
            && !block.starts_with("..")
            && !block.starts_with(". ")
    });
    // Script-block pipelines that are nearly always written with `$_`, and
    // no `$` survived anywhere in the command. The simplified syntax
    // (`Where-Object Name -eq x`) has no block and is left alone.
    let dollarless_pipeline = !command.contains('$')
        && ["ForEach-Object {", "Where-Object {", "| % {", "| ? {", "|% {", "|? {"]
            .iter()
            .any(|marker| command.contains(marker));
    if !orphaned_member && !dollarless_pipeline {
        return None;
    }
    Some(
        "Note: this command looks like your local shell expanded `$_`/`$var` before agent-rdp \
         saw it (a script block starting with `.`, or a *-Object pipeline with no `$` left). \
         Quote the command with single quotes locally, or `file push` a .ps1 and run it with \
         -File. The reply's `command_line` shows exactly what the agent executed."
            .to_string(),
    )
}

fn reject_misplaced_run_options(args: &[String]) -> Result<(), String> {
    let misplaced: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| RUN_OPTIONS.contains(a))
        .collect();
    if misplaced.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} appeared after the command, so it would be passed to the remote shell as an \
         argument instead of being applied by agent-rdp. Put agent-rdp options before the \
         command (`automate run --wait --hidden \"<command>\"`); everything after the command \
         or after `--` goes to the remote shell verbatim. If the remote command genuinely \
         needs that literal token, embed it in the command string.",
        misplaced.join(", ")
    ))
}

/// IPC timeout for an automate request: the base timeout, extended by the
/// command's own budget when it carries one.
///
/// Mirrors what the daemon does for its DVC deadline (`request_timeout` in
/// the daemon's automate handler) and what `locate --wait` already does for
/// its poll budget. All three have to agree, or the shortest one silently
/// decides the real limit.
fn automate_timeout_ms(request: &Request, base_timeout_ms: u64) -> u64 {
    let Request::Automate(automate) = request else {
        return base_timeout_ms;
    };

    let command_budget_ms = match automate {
        AutomateRequest::Run { wait: true, timeout_ms, .. } => *timeout_ms,
        AutomateRequest::WaitFor { timeout_ms, .. } => *timeout_ms,
        _ => 0,
    };

    base_timeout_ms.saturating_add(command_budget_ms)
}

/// Map a CLI action onto the wire request.
///
/// Split out from `run` so the mapping can be checked without a daemon - the
/// `focused` shorthand in particular encodes decisions that are easy to regress.
/// `Err` carries a message for the user; nothing here talks to the daemon.
/// The key becomes the DVC request id verbatim, so it has to be short and
/// plain: it travels inside JSON, lands in log lines and file names, and a
/// stray quote or newline would break framing before it ever reached the
/// agent.
fn validate_idempotency_key(key: &str) -> Result<(), String> {
    if key.is_empty() || key.len() > 64 {
        return Err(format!(
            "--idempotency-key must be 1-64 characters (got {})",
            key.len()
        ));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '-'))
    {
        return Err(
            "--idempotency-key may only contain ASCII letters, digits, '.', '_', ':' and '-'"
                .to_string(),
        );
    }
    Ok(())
}

fn build_request(action: AutomateAction) -> Result<AutomateRequest, String> {
    Ok(match action {
        AutomateAction::Snapshot {
            interactive,
            compact,
            depth,
            selector,
            focused,
        } => AutomateRequest::Snapshot {
            interactive_only: interactive,
            compact,
            max_depth: depth,
            selector,
            focused,
        },

        // Just enough of the tree to identify the focused control, and no more.
        //
        // Deliberately not compact, and not interactive-only: both filters can
        // prune the root itself, and the element most likely to be pruned - an
        // unnamed Custom/Pane with an empty value - is exactly the half-edited
        // grid cell this command exists to identify.
        AutomateAction::Focused => AutomateRequest::Snapshot {
            interactive_only: false,
            compact: false,
            max_depth: 1,
            selector: None,
            focused: true,
        },

        AutomateAction::Get { selector, property } => AutomateRequest::Get { selector, property },

        AutomateAction::Focus { selector } => AutomateRequest::Focus { selector },

        AutomateAction::Click { selector, double_click } => AutomateRequest::Click { selector, double_click },

        AutomateAction::Select { selector, item } => AutomateRequest::Select { selector, item },

        AutomateAction::Toggle { selector, state } => {
            let state = state.map(|s| matches!(s.as_str(), "on" | "true" | "1"));
            AutomateRequest::Toggle { selector, state }
        }

        AutomateAction::Expand { selector } => AutomateRequest::Expand { selector },

        AutomateAction::Collapse { selector } => AutomateRequest::Collapse { selector },

        AutomateAction::ContextMenu { selector } => AutomateRequest::ContextMenu { selector },

        AutomateAction::Fill { selector, text } => AutomateRequest::Fill { selector, text },

        AutomateAction::Clear { selector } => AutomateRequest::Clear { selector },

        AutomateAction::Scroll {
            selector,
            direction,
            amount,
            to_child,
        } => {
            let direction = direction.map(|d| match d.as_str() {
                "up" => AutomationScrollDirection::Up,
                "down" => AutomationScrollDirection::Down,
                "left" => AutomationScrollDirection::Left,
                "right" => AutomationScrollDirection::Right,
                _ => AutomationScrollDirection::Down,
            });
            AutomateRequest::Scroll {
                selector,
                direction,
                amount,
                to_child,
            }
        }

        AutomateAction::Window { action, selector } => {
            let action = match action.as_str() {
                "list" => WindowAction::List,
                "focus" => WindowAction::Focus,
                "maximize" => WindowAction::Maximize,
                "minimize" => WindowAction::Minimize,
                "restore" => WindowAction::Restore,
                "close" => WindowAction::Close,
                other => return Err(format!("Unknown window action: {}", other)),
            };
            AutomateRequest::Window { action, selector }
        }

        AutomateAction::Run {
            command,
            args: cmd_args,
            wait,
            hidden,
            process_timeout,
            shell,
            stream,
            idempotency_key,
        } => {
            if let Some(ref key) = idempotency_key {
                validate_idempotency_key(key)?;
            }
            reject_misplaced_run_options(&cmd_args)?;
            warn_argument_hazards(&cmd_args);
            if let Some(hint) = local_shell_expansion_hint(&command) {
                eprintln!("{}", hint);
            }
            AutomateRequest::Run {
                command,
                args: cmd_args,
                wait,
                hidden,
                timeout_ms: process_timeout.unwrap_or(10000),
                shell,
                stream,
                idempotency_key,
            }
        }

        AutomateAction::RunPoll { pid, .. } => AutomateRequest::RunPoll { pid },

        AutomateAction::WaitFor {
            selector,
            timeout,
            state,
        } => {
            let state = match state.as_deref() {
                Some("enabled") => WaitState::Enabled,
                Some("gone") => WaitState::Gone,
                _ => WaitState::Visible,
            };
            AutomateRequest::WaitFor {
                selector,
                timeout_ms: timeout.unwrap_or(30000),
                state,
            }
        }

        AutomateAction::Status => AutomateRequest::Status,

        // Handled directly in `run` before this function is called - it maps
        // to a top-level `Request` variant, not an `AutomateRequest`.
        AutomateAction::Restart => {
            return Err("restart is not an AutomateRequest - this is a bug".to_string())
        }
    })
}

/// Compact JSON rendering of the focused element, for `--json` callers.
///
/// Mirrors the fields `describe_focused` shows in its one-line human form -
/// the two must stay in sync, or a JSON caller and a human reading the same
/// terminal would end up looking at different information.
fn focused_json(element: &AccessibilityElement) -> serde_json::Value {
    serde_json::json!({
        "success": true,
        "data": {
            "type": "focused",
            "role": element.role,
            "name": element.name,
            "value": element.value.as_deref().unwrap_or(""),
            "bounds": element.bounds,
            "states": element.states,
        }
    })
}

/// One-line description of the focused element.
fn describe_focused(element: &AccessibilityElement) -> String {
    let mut line = element.role.clone();

    if let Some(name) = element.name.as_ref().filter(|n| !n.is_empty()) {
        line.push_str(&format!(" '{}'", name));
    }

    // The value is the part that answers "did my keystrokes land here?", so it
    // is always shown - an empty one included.
    line.push_str(&format!(" = {:?}", element.value.as_deref().unwrap_or("")));

    if let Some(b) = &element.bounds {
        line.push_str(&format!(" at ({}, {}) {}x{}", b.x, b.y, b.width, b.height));
    }

    if !element.states.is_empty() {
        line.push_str(&format!(" [{}]", element.states.join(", ")));
    }

    line
}

#[cfg(test)]
mod misplaced_option_tests {
    use super::reject_misplaced_run_options;

    fn args(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn options_after_the_command_are_refused() {
        let err = reject_misplaced_run_options(&args(&[
            "-Command", "Get-Date", "--wait", "--hidden",
        ]))
        .unwrap_err();
        assert!(err.contains("--wait, --hidden"), "{err}");
        assert!(err.contains("before the command"));
    }

    #[test]
    fn ordinary_child_arguments_pass() {
        assert!(reject_misplaced_run_options(&args(&["-NoProfile", "-File", "x.ps1", "--verbose"])).is_ok());
        assert!(reject_misplaced_run_options(&[]).is_ok());
    }
}

#[cfg(test)]
mod idempotency_key_tests {
    use super::validate_idempotency_key;

    #[test]
    fn accepts_plain_keys() {
        for key in ["k1", "chunk-07", "job:42.retry_1", "a".repeat(64).as_str()] {
            assert!(validate_idempotency_key(key).is_ok(), "{key}");
        }
    }

    #[test]
    fn rejects_empty_long_and_unsafe_keys() {
        assert!(validate_idempotency_key("").is_err());
        assert!(validate_idempotency_key(&"a".repeat(65)).is_err());
        for key in ["bad key", "quote\"", "new\nline", "slash/", "ünïcode"] {
            assert!(validate_idempotency_key(key).is_err(), "{key:?}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_rdp_protocol::ElementBounds;

    fn element(role: &str, name: Option<&str>, value: Option<&str>) -> AccessibilityElement {
        AccessibilityElement {
            r#ref: Some(1),
            role: role.to_string(),
            name: name.map(str::to_string),
            automation_id: None,
            class_name: None,
            bounds: Some(ElementBounds { x: 110, y: 391, width: 80, height: 18 }),
            states: vec!["focusable".to_string()],
            value: value.map(str::to_string),
            patterns: vec![],
            children: vec![],
        }
    }

    #[test]
    fn test_describe_focused_full() {
        let line = describe_focused(&element("edit", Some("Количество"), Some("5,000")));
        assert_eq!(line, "edit 'Количество' = \"5,000\" at (110, 391) 80x18 [focusable]");
    }

    #[test]
    fn test_describe_focused_shows_empty_value() {
        // An empty value is the interesting case: it means the keystrokes went
        // somewhere else. It must not be hidden.
        let line = describe_focused(&element("edit", Some("Цена"), None));
        assert!(line.contains("= \"\""), "empty value must be visible: {}", line);
    }

    #[test]
    fn test_describe_focused_without_name_or_bounds() {
        let mut e = element("pane", None, Some("x"));
        e.bounds = None;
        e.states.clear();
        assert_eq!(describe_focused(&e), "pane = \"x\"");
    }

    #[test]
    fn test_describe_focused_treats_an_empty_name_as_absent() {
        // UIA hands back "" for unnamed controls; printing `'' ` would be noise.
        let line = describe_focused(&element("edit", Some(""), Some("5")));
        assert!(!line.contains("''"), "empty name should be omitted: {}", line);
        assert!(line.starts_with("edit ="), "unexpected shape: {}", line);
    }

    #[test]
    fn test_describe_focused_quotes_values_with_spaces() {
        // A trailing space or an empty-looking value is exactly what a
        // half-committed 1C cell edit looks like, so quoting has to make it
        // visible rather than blend into the line.
        let line = describe_focused(&element("edit", Some("Цена"), Some("123 ")));
        assert!(line.contains("\"123 \""), "value should be quoted verbatim: {}", line);
    }

    #[test]
    fn test_describe_focused_lists_all_states() {
        let mut e = element("edit", Some("Количество"), Some("5"));
        e.states = vec!["focusable".to_string(), "readonly".to_string()];
        let line = describe_focused(&e);
        assert!(line.ends_with("[focusable, readonly]"), "unexpected shape: {}", line);
    }

    #[test]
    fn test_focused_shorthand_does_not_filter_the_root_away() {
        // Regression: with compact or interactive_only set, the PowerShell
        // agent prunes an unnamed Custom/Pane with no value - which is what a
        // half-edited 1C grid cell looks like - and the response then has no
        // root at all. The shorthand must ask for the element unfiltered.
        match build_request(AutomateAction::Focused).unwrap() {
            AutomateRequest::Snapshot {
                interactive_only,
                compact,
                focused,
                selector,
                max_depth,
            } => {
                assert!(focused, "must start from the focused element");
                assert!(!compact, "compact can prune the focused element itself");
                assert!(!interactive_only, "interactive_only can prune it too");
                assert_eq!(selector, None);
                assert!(max_depth >= 1, "the root must be reachable");
            }
            other => panic!("focused should map to a snapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_snapshot_flags_are_passed_through_unchanged() {
        // The explicit snapshot command must keep honouring its own flags -
        // the focused shorthand's opinions must not leak into it.
        match build_request(AutomateAction::Snapshot {
            interactive: true,
            compact: true,
            depth: 3,
            selector: Some("#Notepad".to_string()),
            focused: false,
        })
        .unwrap()
        {
            AutomateRequest::Snapshot {
                interactive_only,
                compact,
                max_depth,
                selector,
                focused,
            } => {
                assert!(interactive_only && compact && !focused);
                assert_eq!(max_depth, 3);
                assert_eq!(selector.as_deref(), Some("#Notepad"));
            }
            other => panic!("expected a snapshot, got {:?}", other),
        }
    }

    #[test]
    fn test_window_action_parsing_positive_and_negative() {
        for (input, expected) in [
            ("list", WindowAction::List),
            ("focus", WindowAction::Focus),
            ("maximize", WindowAction::Maximize),
            ("minimize", WindowAction::Minimize),
            ("restore", WindowAction::Restore),
            ("close", WindowAction::Close),
        ] {
            let request = build_request(AutomateAction::Window {
                action: input.to_string(),
                selector: None,
            })
            .unwrap();
            match request {
                AutomateRequest::Window { action, .. } => assert_eq!(action, expected),
                other => panic!("expected a window request, got {:?}", other),
            }
        }

        // An unknown action is rejected rather than silently defaulting to one
        // of the destructive ones like close.
        let err = build_request(AutomateAction::Window {
            action: "destroy".to_string(),
            selector: None,
        })
        .unwrap_err();
        assert!(err.contains("destroy"), "should name the bad action: {}", err);

        // Case matters - "Close" is not "close", and must not be guessed at.
        assert!(build_request(AutomateAction::Window {
            action: "Close".to_string(),
            selector: None,
        })
        .is_err());
    }

    #[test]
    fn test_focused_json_shape() {
        let e = element("edit", Some("Количество"), Some("5,000"));
        let v = focused_json(&e);
        assert_eq!(v["success"], serde_json::json!(true));
        assert_eq!(v["data"]["type"], "focused");
        assert_eq!(v["data"]["role"], "edit");
        assert_eq!(v["data"]["name"], "Количество");
        assert_eq!(v["data"]["value"], "5,000");
        assert_eq!(v["data"]["bounds"]["x"], 110);
        assert_eq!(v["data"]["states"], serde_json::json!(["focusable"]));
    }

    #[test]
    fn test_focused_json_shows_empty_value_not_null() {
        // The JSON reader needs the same signal the human one gets: an empty
        // value means the keystrokes did not land here, and must not be
        // conflated with a genuinely absent field.
        let e = element("edit", Some("Цена"), None);
        let v = focused_json(&e);
        assert_eq!(v["data"]["value"], "");
    }

    #[test]
    fn test_focused_json_without_bounds_is_null_not_missing() {
        let mut e = element("pane", None, Some("x"));
        e.bounds = None;
        let v = focused_json(&e);
        assert!(v["data"]["bounds"].is_null());
    }

    #[test]
    fn test_describe_focused_handles_negative_bounds() {
        // A control on a secondary monitor left of the primary has negative x;
        // it must render, not panic or get dropped.
        let mut e = element("edit", Some("Поиск"), Some(""));
        e.bounds = Some(ElementBounds { x: -1920, y: 100, width: 200, height: 24 });
        assert!(describe_focused(&e).contains("at (-1920, 100)"));
    }

    #[test]
    fn run_wait_extends_the_ipc_timeout_by_its_process_budget() {
        let request = Request::Automate(AutomateRequest::Run {
            command: "build.cmd".into(),
            args: Vec::new(),
            wait: true,
            hidden: false,
            timeout_ms: 240_000,
            shell: None,
            stream: false,
            idempotency_key: None,
        });
        // Otherwise the CLI would abandon a 4-minute command at 30s.
        assert_eq!(automate_timeout_ms(&request, 30_000), 270_000);
    }

    #[test]
    fn wait_for_extends_the_ipc_timeout_too() {
        let request = Request::Automate(AutomateRequest::WaitFor {
            selector: "@e1".into(),
            timeout_ms: 60_000,
            state: WaitState::Visible,
        });
        assert_eq!(automate_timeout_ms(&request, 30_000), 90_000);
    }

    #[test]
    fn short_commands_keep_the_base_timeout() {
        let request = Request::Automate(AutomateRequest::Status);
        assert_eq!(automate_timeout_ms(&request, 30_000), 30_000);

        // Without --wait the agent answers immediately.
        let detached = Request::Automate(AutomateRequest::Run {
            command: "long.exe".into(),
            args: Vec::new(),
            wait: false,
            hidden: false,
            timeout_ms: 240_000,
            shell: None,
            stream: true,
            idempotency_key: None,
        });
        assert_eq!(automate_timeout_ms(&detached, 30_000), 30_000);
    }

    #[test]
    fn non_automate_requests_are_unchanged() {
        assert_eq!(automate_timeout_ms(&Request::Ping, 30_000), 30_000);
    }
}

#[cfg(test)]
mod shell_expansion_hint_tests {
    use super::local_shell_expansion_hint;

    #[test]
    fn a_vanished_dollar_underscore_is_recognised() {
        // bash: "... { $_.Line.Trim() }" -> "... { .Line.Trim() }"
        assert!(local_shell_expansion_hint("Get-Content x | ForEach-Object { .Line.Trim() }").is_some());
        assert!(local_shell_expansion_hint("gci | ? {.Length -gt 1}").is_some());
        // *-Object with no `$` anywhere.
        assert!(local_shell_expansion_hint("Get-Process | Where-Object { .CPU -gt 1 }").is_some());
        assert!(local_shell_expansion_hint("ls | ForEach-Object { Write-Host }").is_some());
    }

    #[test]
    fn intact_commands_are_left_alone() {
        assert!(local_shell_expansion_hint("Get-Content x | ForEach-Object { $_.Line.Trim() }").is_none());
        assert!(local_shell_expansion_hint("Get-Process").is_none());
        assert!(local_shell_expansion_hint("powershell -File x.ps1 -Param '1,2'").is_none());
        // A hashtable literal is not an orphaned member access.
        assert!(local_shell_expansion_hint("@{ a = 1 }").is_none());
        assert!(local_shell_expansion_hint("if ($x) { Write-Host 1 }").is_none());
        // Simplified syntax has no script block and no `$_`.
        assert!(local_shell_expansion_hint("Get-Process | Where-Object Name -eq notepad").is_none());
        assert!(local_shell_expansion_hint("gci | ForEach-Object Name").is_none());
        // Relative paths and dot-sourcing inside a block.
        assert!(local_shell_expansion_hint("if (Test-Path x) { .\\build.ps1 }").is_none());
        assert!(local_shell_expansion_hint("& { . .\\lib.ps1; Run }").is_none());
        assert!(local_shell_expansion_hint("& { ./run.sh }").is_none());
        assert!(local_shell_expansion_hint("& { ..\\up.ps1 }").is_none());
    }
}

#[cfg(test)]
mod follow_aggregate_tests {
    /// `follow_poll`'s budget-expiry branch builds its JSON from the text
    /// collected across the whole window, so `pending` has to be derived the
    /// same way a single poll derives it. Hardcoding `false` here handed back
    /// exactly the shape (`exited: false` + empty chunks) that `pending`
    /// exists to disambiguate.
    #[test]
    fn a_follow_that_saw_nothing_still_reports_pending() {
        let source = include_str!("automate.rs");
        let expiry = source
            .split("if started.elapsed() >= budget")
            .nth(1)
            .expect("the budget-expiry branch exists");
        let branch = &expiry[..expiry.find("} else {").unwrap_or(expiry.len())];
        assert!(
            branch.contains("stdout_chunk.is_empty() && stderr_chunk.is_empty()"),
            "pending must be computed from the aggregate, not hardcoded"
        );
        assert!(
            !branch.contains("pending: false"),
            "a silent follow window is pending, not 'not pending'"
        );
    }
}
