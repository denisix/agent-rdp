//! Clipboard command implementation.

use agent_rdp_protocol::{ClipboardRequest, Request};

use crate::cli::{ClipboardAction, ClipboardArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: ClipboardArgs,
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

    let clipboard_request = match &args.action {
        ClipboardAction::Get => ClipboardRequest::Get,
        ClipboardAction::Set { text } => ClipboardRequest::Set { text: text.clone() },
    };

    let request = Request::Clipboard(clipboard_request);
    let response = client.send(&request, timeout_ms).await?;

    output.print_response(&response);
    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
