//! Keyboard command implementation.

use agent_rdp_protocol::{KeyboardRequest, Request};

use crate::cli::{KeyboardAction, KeyboardArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: KeyboardArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    if !manager.is_daemon_alive() {
        output.print_error(
            "daemon_not_running",
            &crate::session_manager::daemon_not_running_message(session),
        );
        std::process::exit(1);
    }

    let mut client = manager.ensure_daemon().await?;

    let keyboard_request = match args.action {
        KeyboardAction::Type { text, delay } => KeyboardRequest::Type { text, delay_ms: delay },
        KeyboardAction::Press { keys } => KeyboardRequest::Press { keys },
    };

    let request = Request::Keyboard(keyboard_request);
    let response = client.send(&request, timeout_ms).await?;
    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
