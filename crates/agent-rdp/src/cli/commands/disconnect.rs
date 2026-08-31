//! Disconnect command implementation.

use agent_rdp_protocol::Request;

use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
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
    // Send Shutdown to disconnect RDP and close the session daemon
    let response = client.send(&Request::Shutdown, timeout_ms).await?;
    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
