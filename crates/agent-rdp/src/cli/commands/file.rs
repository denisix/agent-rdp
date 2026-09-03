//! File transfer command implementation.

use agent_rdp_protocol::{FilePullRequest, FilePushRequest, FileStatRequest, Request};

use crate::cli::{FileAction, FileArgs};
use crate::output::Output;
use crate::session_manager::SessionManager;

/// Extra IPC budget for a transfer, on top of the base timeout.
///
/// A transfer is many round trips, each with a remote disk write, plus a
/// full-file hash on both ends. The daemon caps the file size, so this is a
/// ceiling on the worst legitimate case rather than a guess about any
/// particular file.
const TRANSFER_TIMEOUT_MS: u64 = 10 * 60 * 1000;

pub async fn run(
    session: &str,
    args: FileArgs,
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

    let request = match args.action {
        FileAction::Push { local, remote } => Request::FilePush(FilePushRequest {
            local_path: local,
            remote_path: remote,
        }),
        FileAction::Pull { remote, local, max_age } => Request::FilePull(FilePullRequest {
            remote_path: remote,
            local_path: local,
            max_age_secs: max_age,
        }),
        FileAction::Stat { remote } => Request::FileStat(FileStatRequest { remote_path: remote }),
    };

    // `stat` is one round trip (read-only, so retried once if the daemon
    // drops the connection); transfers get the long budget.
    let budget = if matches!(request, Request::FileStat(_)) {
        timeout_ms
    } else {
        timeout_ms.saturating_add(TRANSFER_TIMEOUT_MS)
    };
    let response = manager.send_with_retry(&mut client, &request, budget).await?;

    output.print_response(&response);

    if !response.success {
        std::process::exit(1);
    }

    Ok(())
}
