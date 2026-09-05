//! Session management command implementation.

use agent_rdp_protocol::{Request, ResponseData, SessionSummary, ConnectionState};

use crate::cli::{SessionAction, SessionArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

pub async fn run(
    session: &str,
    args: SessionArgs,
    output: &Output,
    timeout_ms: u64,
) -> anyhow::Result<()> {
    match args.action {
        SessionAction::List => {
            list_sessions(output).await
        }
        SessionAction::Info => {
            session_info(session, output, timeout_ms).await
        }
        SessionAction::Daemon => {
            run_daemon(session).await
        }
    }
}

async fn list_sessions(output: &Output) -> anyhow::Result<()> {
    let sessions = SessionManager::list_sessions();

    let mut summaries = Vec::new();
    for session_name in sessions {
        let manager = SessionManager::new(session_name.clone());

        let state = if manager.is_daemon_alive() {
            // Try to get session info
            if let Ok(mut client) = crate::ipc_client::try_connect(
                &manager.socket_path(),
                1,
                100,
            ).await {
                if let Ok(response) = client.send(&Request::SessionInfo, 5000).await {
                    if let Some(ResponseData::SessionInfo(info)) = response.data {
                        summaries.push(SessionSummary {
                            name: session_name,
                            state: info.state,
                            host: info.host,
                        });
                        continue;
                    }
                }
            }
            ConnectionState::Disconnected
        } else {
            ConnectionState::Disconnected
        };

        summaries.push(SessionSummary {
            name: session_name,
            state,
            host: None,
        });
    }

    let response = agent_rdp_protocol::Response::success(ResponseData::SessionList {
        sessions: summaries,
    });

    output.print_response(&response);
    Ok(())
}

async fn session_info(session: &str, output: &Output, timeout_ms: u64) -> anyhow::Result<()> {
    let manager = SessionManager::new(session.to_string());

    let mut client = match manager.connect_existing().await {
        Ok(client) => client,
        Err(unavailable) => {
            output.print_error(unavailable.code(), unavailable.message());
            std::process::exit(1);
        }
    };
    let mut response = manager.send_with_retry(&mut client, &Request::SessionInfo, timeout_ms).await?;
    // The daemon reports its own version; this side of the contract is
    // ours to add, so one `session info` shows both.
    if let Some(ResponseData::SessionInfo(ref mut info)) = response.data {
        info.cli_version = Some(crate::session_manager::CLI_VERSION.to_string());
    }
    output.print_response(&response);

    Ok(())
}

/// Run as the background daemon (called by session manager).
async fn run_daemon(session: &str) -> anyhow::Result<()> {
    agent_rdp_daemon::run_server(session).await
}
