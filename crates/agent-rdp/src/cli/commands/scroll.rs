//! Scroll command implementation.

use agent_rdp_protocol::{Request, ScrollDirection as ProtoScrollDirection, ScrollRequest};

use crate::cli::{ScrollArgs, ScrollDirection};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: ScrollArgs,
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

    let (direction, amount, at) = match args.direction {
        ScrollDirection::Up { amount, at } => (ProtoScrollDirection::Up, amount, at),
        ScrollDirection::Down { amount, at } => (ProtoScrollDirection::Down, amount, at),
        ScrollDirection::Left { amount, at } => (ProtoScrollDirection::Left, amount, at),
        ScrollDirection::Right { amount, at } => (ProtoScrollDirection::Right, amount, at),
    };

    let (x, y) = match at {
        Some(coords) if coords.len() == 2 => (Some(coords[0]), Some(coords[1])),
        _ => (None, None),
    };

    let request = Request::Scroll(ScrollRequest {
        direction,
        amount,
        x,
        y,
    });

    let response = client.send(&request, timeout_ms).await?;
    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
