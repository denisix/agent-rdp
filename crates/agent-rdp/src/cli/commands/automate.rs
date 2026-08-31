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
        Err(message) => {
            output.print_error("daemon_not_running", &message);
            std::process::exit(1);
        }
    };

    // `restart` isn't an `AutomateRequest` at all - it has to work even when
    // the DVC channel that carries every other automate command is dead,
    // which is exactly the case it exists to recover from - so it's a
    // separate top-level `Request` variant, dispatched before the mapping
    // below.
    if matches!(args.action, AutomateAction::Restart) {
        let response = client.send(&Request::AutomationRestart, timeout_ms).await?;
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

    let automate_request = match build_request(args.action) {
        Ok(request) => request,
        Err(message) => {
            output.print_error("invalid_request", &message);
            std::process::exit(1);
        }
    };

    let request = Request::Automate(automate_request);
    let response = client.send(&request, timeout_ms).await?;

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

/// Map a CLI action onto the wire request.
///
/// Split out from `run` so the mapping can be checked without a daemon - the
/// `focused` shorthand in particular encodes decisions that are easy to regress.
/// `Err` carries a message for the user; nothing here talks to the daemon.
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
        } => AutomateRequest::Run {
            command,
            args: cmd_args,
            wait,
            hidden,
            timeout_ms: process_timeout.unwrap_or(10000),
            shell,
            stream,
        },

        AutomateAction::RunPoll { pid } => AutomateRequest::RunPoll { pid },

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
}
