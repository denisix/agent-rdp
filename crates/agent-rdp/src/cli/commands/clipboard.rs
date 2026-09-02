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
    // Resolve the payload before touching the daemon, so an unreadable
    // --file fails on its own terms instead of surfacing as a daemon error.
    let clipboard_request = match &args.action {
        ClipboardAction::Get => ClipboardRequest::Get,
        ClipboardAction::Set { text, file } => {
            let text = match (text, file) {
                (_, Some(path)) => read_text_source(path)?,
                (Some(text), None) => text.clone(),
                (None, None) => unreachable!("clap requires text or --file"),
            };
            ClipboardRequest::Set { text }
        }
    };

    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };

    let request = Request::Clipboard(clipboard_request);
    let response = client.send(&request, timeout_ms).await?;

    output.print_response(&response);
    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}

/// Read clipboard content from a path, or from stdin for `-`.
fn read_text_source(path: &str) -> anyhow::Result<String> {
    use std::io::Read;

    if path == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|e| anyhow::anyhow!("Failed to read clipboard text from stdin: {}", e))?;
        return Ok(text);
    }

    std::fs::read_to_string(path)
        .map_err(|e| anyhow::anyhow!("Failed to read clipboard text from '{}': {}", path, e))
}
