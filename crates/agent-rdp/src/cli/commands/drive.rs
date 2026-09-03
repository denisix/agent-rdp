//! Drive mapping command implementation.

use agent_rdp_protocol::{DriveRequest, Request};

use crate::cli::{DriveAction, DriveArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: DriveArgs,
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

    let drive_request = match args.action {
        DriveAction::List => DriveRequest::List,
    };

    let request = Request::Drive(drive_request);
    let response = manager.send_with_retry(&mut client, &request, timeout_ms).await?;
    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
