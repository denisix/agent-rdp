//! Keyboard command implementation.

use agent_rdp_protocol::{KeyboardRequest, Request};

use crate::cli::{KeyboardAction, KeyboardArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

/// How long this keyboard command may legitimately take, beyond a single
/// round trip.
///
/// A sequence is its own gaps plus a send per key; a delayed `type` is one
/// delay per 64-code-unit batch. Both used to budget zero and survive only
/// on the watchdog's grace period.
pub fn budget_ms(action: &KeyboardAction) -> u64 {
    match action {
        KeyboardAction::Send { keys, interval_ms } => {
            let count = keys.split_whitespace().count() as u64;
            let interval = interval_ms.unwrap_or(agent_rdp_protocol::DEFAULT_PRESS_SEQ_INTERVAL_MS);
            // Gaps, plus the per-key down/up sleeps the handler applies.
            count.saturating_sub(1).saturating_mul(interval) + count.saturating_mul(120)
        }
        KeyboardAction::Type { text, delay } => {
            let batches = (text.encode_utf16().count() as u64).div_ceil(64);
            batches.saturating_sub(1).saturating_mul(delay.unwrap_or(0))
        }
        _ => 0,
    }
}

pub async fn run(
    session: &str,
    args: KeyboardArgs,
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

    // Cloned before the mapping consumes it.
    let action_for_budget = args.action.clone();
    let keyboard_request = match args.action {
        KeyboardAction::Type { text, delay } => KeyboardRequest::Type { text, delay_ms: delay },
        KeyboardAction::Press { keys } => KeyboardRequest::Press { keys },
        KeyboardAction::Send { keys, interval_ms } => KeyboardRequest::PressSeq {
            keys: keys.split_whitespace().map(str::to_string).collect(),
            interval_ms,
        },
        KeyboardAction::Down { key } => KeyboardRequest::KeyDown { key },
        KeyboardAction::Up { key } => KeyboardRequest::KeyUp { key },
        KeyboardAction::Paste { text } => KeyboardRequest::Paste { text },
    };

    // Refused here rather than after a round trip: the daemon applies the
    // same rule, and a usage error should not need a daemon to discover.
    if let Err(e) = agent_rdp_protocol::validate_keyboard_request(&keyboard_request) {
        output.print_error("invalid_request", &e);
        std::process::exit(1);
    }

    let request = Request::Keyboard(keyboard_request);
    // The socket has to outlast the command, or the CLI gives up on input
    // the daemon is still delivering.
    let response = client
        .send(&request, timeout_ms.saturating_add(budget_ms(&action_for_budget)))
        .await?;
    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
