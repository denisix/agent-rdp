//! File transfer between the local machine and the remote desktop.
//!
//! Moves bytes in chunks over the automation DVC channel, verified with a
//! SHA-256 both ends compute independently.
//!
//! The obvious-looking alternative does not work: the automation drive is
//! redirected and visible on the remote as `\\TSCLIENT\agent-automation`, but
//! reading it from inside the automation agent blocks forever - drive I/O is
//! serviced by the same frame-processor task that carries the agent's own DVC
//! traffic, so the agent ends up waiting on a reply that cannot be produced
//! until it stops waiting. Sending payloads through the clipboard has its own
//! ceiling and re-encodes the bytes. Chunked DVC transfer avoids both.

use std::sync::Arc;

use agent_rdp_protocol::{
    AutomateRequest, ErrorCode, FilePullRequest, FilePushRequest, FileTransferResult, Response,
    ResponseData,
};
use base64::Engine;
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::automation::{DvcIpc, SharedAutomationState};
use crate::rdp_session::RdpSession;

/// Raw bytes per chunk.
///
/// The agent reads a DVC message into a 1MB buffer, so the base64-expanded
/// payload (4/3 of this, plus JSON overhead) has to stay well inside that.
/// 192KB keeps a comfortable margin while still moving a megabyte in a
/// handful of round trips.
const CHUNK_BYTES: usize = 192 * 1024;

/// Largest file this will move.
///
/// Base64 in JSON over a virtual channel is not the right tool for disk
/// images; refusing early beats discovering it after twenty minutes.
const MAX_TRANSFER_BYTES: u64 = 128 * 1024 * 1024;

/// Per-chunk DVC deadline. Generous compared to an ordinary command: each
/// chunk includes a real disk write on the remote.
const CHUNK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// Deadline for the final chunk, which also hashes the whole file remotely.
const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Reject a transfer whose paths cannot mean what the caller thinks.
///
/// Both sides resolve relative paths against a working directory that is not
/// the caller's: the local one against the daemon's (inherited from whichever
/// shell first started it, possibly days ago), the remote one against the
/// agent's. Neither fails loudly - a stale same-named file in the daemon's
/// directory pushes the wrong bytes and verifies them happily, and a relative
/// remote path writes somewhere nobody looks while `file stat` on the
/// intended path keeps showing the old file. The CLI and SDK make paths
/// absolute before sending; this is the backstop for anything else.
fn check_paths(local: &str, remote: &str) -> Result<(), Response> {
    if !std::path::Path::new(local).is_absolute() {
        return Err(Response::error(
            ErrorCode::InvalidRequest,
            format!(
                "Local path '{}' must be absolute: it is resolved by the daemon, whose working \
                 directory is not the one you ran this from.",
                local
            ),
        ));
    }
    if !is_absolute_windows_path(remote) {
        return Err(Response::error(
            ErrorCode::InvalidRequest,
            format!(
                "Remote path '{}' must be absolute (like C:\\\\path\\\\file.txt or \
                 \\\\\\\\server\\\\share\\\\file.txt): a relative path is resolved against the \
                 automation agent's working directory, not any directory you chose.",
                remote
            ),
        ));
    }
    Ok(())
}

/// A drive-letter path (`C:\x`, `C:/x`) or a UNC path (`\\host\share`).
fn is_absolute_windows_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }
    path.starts_with("\\\\") || path.starts_with("//")
}

/// How many times a pull re-reads a file that changed underneath it.
const PULL_ATTEMPTS: u32 = 3;

/// First retry delay for that; doubles per attempt (150ms, 300ms).
const PULL_RETRY_BASE: std::time::Duration = std::time::Duration::from_millis(150);

/// Only files this small are re-read. A 128MB file retried three times
/// could outlast the CLI's own budget for a `file` command; the failure
/// this exists for is a small status file rewritten every few seconds.
const PULL_RETRY_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// And only while the attempts have been quick. Size alone does not bound
/// the time: a small file behind a stalled agent can still burn a full
/// `CHUNK_TIMEOUT` per attempt, and three of those would outlast the budget
/// the CLI allows the whole command.
const PULL_RETRY_MAX_ELAPSED: std::time::Duration = std::time::Duration::from_secs(30);

/// Whether a pull whose hash did not match should be attempted again.
///
/// Pure so the loop's actual decision is testable: the orchestration in
/// `handle_pull` is otherwise only reachable with a live agent.
fn retry_pull(attempt: u32, size: u64, elapsed: std::time::Duration) -> bool {
    attempt < PULL_ATTEMPTS && size <= PULL_RETRY_MAX_BYTES && elapsed < PULL_RETRY_MAX_ELAPSED
}

/// How long to wait before attempt `attempt + 1`.
fn pull_backoff(attempt: u32) -> std::time::Duration {
    PULL_RETRY_BASE * 2u32.pow(attempt.saturating_sub(1).min(8))
}

/// One `file pull` attempt: stat, read, verify.
enum PullAttempt {
    Ok {
        data: Vec<u8>,
        chunks: u64,
        sha256: String,
        freshness: Option<Freshness>,
    },
    /// The bytes did not match the hash taken moments earlier - the file was
    /// rewritten mid-transfer. The only outcome worth retrying.
    Changed {
        remote: String,
        local: String,
        size: u64,
    },
    Failed(Response),
}

/// Stat the remote file, read it whole, and verify it against the hash from
/// that same stat. Every attempt re-stats: the freshness check and the hash
/// both have to describe the bytes this attempt actually delivered.
async fn pull_once(ipc: &crate::automation::DvcIpc, params: &FilePullRequest) -> PullAttempt {
    // Stat first: it tells us the size (so an oversized file is refused
    // before transferring any of it) and the remote hash to verify against.
    let stat = match ipc
        .send_request_with_timeout(
            &AutomateRequest::FileStat { path: params.remote_path.clone() },
            VERIFY_TIMEOUT,
        )
        .await
    {
        Ok(value) => value,
        Err(e) => {
            return PullAttempt::Failed(Response::error(
                ErrorCode::AutomationError,
                format!("Cannot stat '{}': {}", params.remote_path, e),
            ))
        }
    };

    if !stat["exists"].as_bool().unwrap_or(false) {
        return PullAttempt::Failed(Response::error(
            ErrorCode::InvalidRequest,
            format!("Remote file not found: {}", params.remote_path),
        ));
    }
    if stat["is_directory"].as_bool().unwrap_or(false) {
        return PullAttempt::Failed(Response::error(
            ErrorCode::InvalidRequest,
            format!("'{}' is a directory, not a file", params.remote_path),
        ));
    }

    let total_size = stat["size"].as_u64().unwrap_or(0);
    if total_size > MAX_TRANSFER_BYTES {
        return PullAttempt::Failed(Response::error(
            ErrorCode::InvalidRequest,
            format!(
                "'{}' is {} bytes; the transfer limit is {} bytes",
                params.remote_path, total_size, MAX_TRANSFER_BYTES
            ),
        ));
    }
    let remote_sha256 = stat["sha256"].as_str().unwrap_or_default().to_string();

    // Freshness, from the remote clock alone: both timestamps come from the
    // same machine, so the age is right even when the two hosts' clocks
    // disagree by hours. An agent predating the fields reports nothing.
    let freshness = file_freshness(&stat);
    if let (Some(max_age), Some(age)) = (params.max_age_secs, freshness.as_ref().map(|f| f.age_secs)) {
        if age > max_age {
            return PullAttempt::Failed(Response::error(
                ErrorCode::StaleFile,
                format!(
                    "'{}' was last written {}s ago (at {}), older than --max-age {}s - the \
                     command that was supposed to produce it did not write it. Nothing was \
                     transferred.",
                    params.remote_path,
                    age,
                    freshness.as_ref().map(|f| f.modified.clone()).unwrap_or_default(),
                    max_age
                ),
            ));
        }
    } else if params.max_age_secs.is_some() {
        return PullAttempt::Failed(Response::error(
            ErrorCode::AutomationError,
            "--max-age needs an automation agent that reports file times (1.5.0+); \
             reconnect to redeploy the agent"
                .to_string(),
        ));
    }

    info!("Pulling {} ({} bytes)", params.remote_path, total_size);

    let mut data: Vec<u8> = Vec::with_capacity(total_size as usize);
    let mut chunks: u64 = 0;

    while (data.len() as u64) < total_size {
        let request = AutomateRequest::FileReadChunk {
            path: params.remote_path.clone(),
            offset: data.len() as u64,
            length: CHUNK_BYTES as u64,
        };

        let value = match ipc.send_request_with_timeout(&request, CHUNK_TIMEOUT).await {
            Ok(value) => value,
            Err(e) => {
                return PullAttempt::Failed(Response::error(
                    ErrorCode::AutomationError,
                    format!(
                        "Transfer failed at offset {} of '{}': {}",
                        data.len(),
                        params.remote_path,
                        e
                    ),
                ))
            }
        };

        let encoded = value["data_b64"].as_str().unwrap_or_default();
        let decoded = match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(e) => {
                return PullAttempt::Failed(Response::error(
                    ErrorCode::InternalError,
                    format!("Agent returned an undecodable chunk: {}", e),
                ))
            }
        };

        chunks += 1;

        // Guard against a chunk that returns nothing while bytes remain:
        // without this the loop would spin forever on a file that shrank or
        // an agent that stopped making progress.
        if decoded.is_empty() {
            if value["eof"].as_bool().unwrap_or(false) {
                break;
            }
            return PullAttempt::Failed(Response::error(
                ErrorCode::InternalError,
                format!(
                    "Transfer stalled at offset {} of '{}' (agent returned no data)",
                    data.len(),
                    params.remote_path
                ),
            ));
        }

        data.extend_from_slice(&decoded);
    }

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let local_sha256 = hex_digest(hasher);

    if !remote_sha256.is_empty() && remote_sha256 != local_sha256 {
        return PullAttempt::Changed {
            remote: remote_sha256,
            local: local_sha256,
            size: total_size,
        };
    }

    PullAttempt::Ok { data, chunks, sha256: local_sha256, freshness }
}

/// Resolve the automation IPC, or explain why file transfer is unavailable.
async fn ready_ipc(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
) -> Result<DvcIpc, Response> {
    {
        let session = rdp_session.lock().await;
        if session.is_none() {
            return Err(Response::error(
                ErrorCode::NotConnected,
                "Not connected to an RDP server",
            ));
        }
    }

    let state = automation_state.lock().await;
    if !state.enabled {
        return Err(Response::error(
            ErrorCode::AutomationNotEnabled,
            "File transfer uses the automation channel - reconnect with --enable-win-automation",
        ));
    }

    let Some(ipc) = state.dvc_ipc.as_ref() else {
        return Err(Response::error(
            ErrorCode::AutomationError,
            "Automation DVC IPC not initialized",
        ));
    };

    if !ipc.is_ready() {
        return Err(Response::error(
            ErrorCode::AutomationError,
            "Automation agent not ready - try `agent-rdp automate restart`",
        ));
    }

    Ok(ipc.clone())
}

fn hex_digest(hasher: Sha256) -> String {
    hasher.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// Push a local file to the remote machine.
pub async fn handle_push(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    params: FilePushRequest,
) -> Response {
    let ipc = match ready_ipc(rdp_session, automation_state).await {
        Ok(ipc) => ipc,
        Err(response) => return response,
    };

    if let Err(response) = check_paths(&params.local_path, &params.remote_path) {
        return response;
    }

    let data = match tokio::fs::read(&params.local_path).await {
        Ok(data) => data,
        Err(e) => {
            return Response::error(
                ErrorCode::InvalidRequest,
                format!("Cannot read '{}': {}", params.local_path, e),
            )
        }
    };

    if data.len() as u64 > MAX_TRANSFER_BYTES {
        return Response::error(
            ErrorCode::InvalidRequest,
            format!(
                "'{}' is {} bytes; the transfer limit is {} bytes",
                params.local_path,
                data.len(),
                MAX_TRANSFER_BYTES
            ),
        );
    }

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let sha256 = hex_digest(hasher);

    // An empty file still needs one (first+last) chunk, or nothing is
    // created on the remote at all.
    let chunks: Vec<&[u8]> = if data.is_empty() {
        vec![&[]]
    } else {
        data.chunks(CHUNK_BYTES).collect()
    };
    let total_chunks = chunks.len();

    info!(
        "Pushing {} ({} bytes) to {} in {} chunk(s)",
        params.local_path,
        data.len(),
        params.remote_path,
        total_chunks
    );

    // Names the sidecar the agent assembles into, so a failed or overlapping
    // transfer cannot leave a torn file where a good one used to be.
    let transfer_id = uuid::Uuid::new_v4().to_string();
    let mut final_reply = None;

    for (index, chunk) in chunks.iter().enumerate() {
        let last = index + 1 == total_chunks;
        let request = AutomateRequest::FileWriteChunk {
            path: params.remote_path.clone(),
            data_b64: base64::engine::general_purpose::STANDARD.encode(chunk),
            first: index == 0,
            last,
            // Sent only with the final chunk: the agent verifies the
            // assembled file and fails loudly rather than leaving a
            // silently-corrupt file behind.
            sha256: last.then(|| sha256.clone()),
            transfer_id: transfer_id.clone(),
        };

        let timeout = if last { VERIFY_TIMEOUT } else { CHUNK_TIMEOUT };
        match ipc.send_request_with_timeout(&request, timeout).await {
            Ok(reply) => {
                if last {
                    final_reply = Some(reply);
                }
            }
            Err(e) => {
            // A push is safe to repeat: chunk 0 carries `first: true`, which
            // truncates the remote file, so re-running starts from scratch
            // rather than appending. Say so instead of the generic
            // "retrying may apply it twice" the DVC layer attaches to every
            // indeterminate outcome - for this command that warning is wrong
            // and sent callers off to build gzip+clipboard workarounds.
                // Honest about what a retry can do. A lost reply leaves the
                // outcome unknown - the swap may already have happened - so
                // the caller should look before retrying. An error the agent
                // raised on the final step is deterministic for this input,
                // and callers rebuilt their workflows around a "re-run" hint
                // that could not help with it. Anything else is retryable.
                let indeterminate = e.downcast_ref::<crate::automation::DvcIndeterminate>().is_some();
                let advice = if indeterminate {
                    "The outcome is unknown (the reply was lost): the swap into place may or \
                     may not have happened. Check the destination with `file stat` before \
                     deciding whether to re-run."
                } else if last {
                    "The agent rejected the final step (verification or the swap into \
                     place) - that is deterministic for this input, so re-running will \
                     produce the same error; report it with the sidecar name above."
                } else {
                    "Re-running is safe: the transfer restarts from the first chunk."
                };
                return Response::error(
                    ErrorCode::AutomationError,
                    format!(
                        "Transfer failed on chunk {}/{} of '{}': {}. The destination was not \
                         replaced - the transfer is assembled in a sidecar \
                         ('{}.agent-rdp-{}.part') and only swapped in once it verifies. {}",
                        index + 1,
                        total_chunks,
                        params.remote_path,
                        e,
                        params.remote_path,
                        transfer_id,
                        advice
                    ),
                );
            }
        }
    }

    // What the agent says it ended up with, not what we sent. The two were
    // never compared: the daemon reported the local hash as if the remote had
    // confirmed it, so "SHA-verified on both ends" rested entirely on the
    // agent choosing to fail - and it skips its own check whenever the final
    // chunk arrives without a hash.
    if let Err(response) = verify_push(final_reply.as_ref(), &sha256, data.len() as u64, &params.remote_path) {
        return response;
    }
    // The agent resolves the path it actually wrote; report that rather than
    // the string we sent.
    let written_path = final_reply
        .as_ref()
        .and_then(|d| d.get("path"))
        .and_then(|p| p.as_str())
        .unwrap_or(&params.remote_path)
        .to_string();

    info!("Pushed {} bytes to {}", data.len(), written_path);

    Response::success(ResponseData::FileTransferResult(FileTransferResult {
        path: written_path,
        bytes: data.len() as u64,
        sha256,
        chunks: total_chunks as u64,
        modified: None,
        modified_unix: None,
        age_secs: None,
    }))
}

/// Check the agent's report of the finished file against what was sent.
///
/// A current agent verifies before it replaces the destination, so a mismatch
/// here should be unreachable against one - it would have refused instead of
/// reporting success. This is the backstop for the case that check exists to
/// close: a reply with no hash at all, which a current agent never sends and
/// an older one might (`transfer_id` is `#[serde(default)]`, so a pre-1.8.0
/// agent is still accepted for the earlier chunks; it wrote in place, so this
/// is where the daemon's only remaining leverage over its outcome is).
///
/// The daemon has no way to see the remote disk directly, so these messages
/// report the mismatch itself and let the caller `file stat` the path -
/// they do not claim to know whether the destination was touched.
fn verify_push(
    reply: Option<&serde_json::Value>,
    expected_sha: &str,
    expected_bytes: u64,
    remote_path: &str,
) -> Result<(), Response> {
    let remote_sha = reply
        .and_then(|d| d.get("sha256"))
        .and_then(|h| h.as_str())
        .map(|h| h.to_ascii_lowercase());

    let Some(remote_sha) = remote_sha else {
        return Err(Response::error(
            ErrorCode::TransferVerificationFailed,
            format!(
                "The agent did not report a hash for '{}', so the transfer could not be \
                 confirmed. Check with `file stat` before trusting the destination.",
                remote_path
            ),
        ));
    };

    if remote_sha != expected_sha.to_ascii_lowercase() {
        return Err(Response::error(
            ErrorCode::TransferVerificationFailed,
            format!(
                "'{}' hashed {} on the remote machine but {} was sent. A current agent refuses \
                 to replace the destination on a mismatch like this, so seeing it at all means \
                 something unexpected happened - check with `file stat`.",
                remote_path, remote_sha, expected_sha
            ),
        ));
    }

    if let Some(size) = reply.and_then(|d| d.get("total_size")).and_then(|s| s.as_u64()) {
        if size != expected_bytes {
            return Err(Response::error(
                ErrorCode::TransferVerificationFailed,
                format!(
                    "'{}' is {} bytes on the remote machine but {} were sent, despite a \
                     matching hash - check with `file stat`.",
                    remote_path, size, expected_bytes
                ),
            ));
        }
    }

    Ok(())
}

/// Pull a file from the remote machine.
pub async fn handle_pull(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    params: FilePullRequest,
) -> Response {
    let ipc = match ready_ipc(rdp_session, automation_state).await {
        Ok(ipc) => ipc,
        Err(response) => return response,
    };

    // Same trap as push, mirrored: the local path is where the daemon writes,
    // so a relative one lands in the daemon's directory rather than the
    // caller's.
    if let Err(response) = check_paths(&params.local_path, &params.remote_path) {
        return response;
    }

    // A file its producer rewrites (the common "poll a status file" case)
    // can change between the stat that hashes it and the reads that fetch
    // it - two independent remote opens. Retrying the pair usually lands
    // both inside one generation of the file.
    let mut attempt: u32 = 0;
    let started = std::time::Instant::now();
    let (data, chunks, local_sha256, freshness) = loop {
        attempt += 1;
        match pull_once(&ipc, &params).await {
            PullAttempt::Ok { data, chunks, sha256, freshness } => {
                break (data, chunks, sha256, freshness)
            }
            PullAttempt::Failed(response) => return response,
            PullAttempt::Changed { remote, local, size } => {
                // Re-reading a large file three times could outlast the CLI's
                // own budget for the command, so only small files retry.
                if !retry_pull(attempt, size, started.elapsed()) {
                    return Response::error(
                        ErrorCode::FileChangedDuringTransfer,
                        format!(
                            "'{}' changed while it was being transferred ({} attempt(s)): \
                             remote hash {}, received {}. It is being rewritten faster than \
                             it can be read - pull a snapshot copy instead.",
                            params.remote_path, attempt, remote, local
                        ),
                    );
                }
                let backoff = pull_backoff(attempt);
                warn!(
                    "'{}' changed during transfer (attempt {}); retrying in {}ms",
                    params.remote_path,
                    attempt,
                    backoff.as_millis()
                );
                tokio::time::sleep(backoff).await;
            }
        }
    };

    if let Some(parent) = std::path::Path::new(&params.local_path).parent() {
        if !parent.as_os_str().is_empty() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
    }
    if let Err(e) = tokio::fs::write(&params.local_path, &data).await {
        return Response::error(
            ErrorCode::InternalError,
            format!("Cannot write '{}': {}", params.local_path, e),
        );
    }

    info!("Pulled {} bytes to {}", data.len(), params.local_path);

    Response::success(ResponseData::FileTransferResult(FileTransferResult {
        path: params.local_path,
        bytes: data.len() as u64,
        sha256: local_sha256,
        chunks,
        modified: freshness.as_ref().map(|f| f.modified.clone()),
        modified_unix: freshness.as_ref().map(|f| f.modified_unix),
        age_secs: freshness.as_ref().map(|f| f.age_secs),
    }))
}

/// Inspect a remote path without transferring it.
pub async fn handle_stat(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    params: agent_rdp_protocol::FileStatRequest,
) -> Response {
    let ipc = match ready_ipc(rdp_session, automation_state).await {
        Ok(ipc) => ipc,
        Err(response) => return response,
    };

    let stat = match ipc
        .send_request_with_timeout(
            &AutomateRequest::FileStat { path: params.remote_path.clone() },
            VERIFY_TIMEOUT,
        )
        .await
    {
        Ok(value) => value,
        Err(e) => {
            return Response::error(
                ErrorCode::AutomationError,
                format!("Cannot stat '{}': {}", params.remote_path, e),
            )
        }
    };

    let exists = stat["exists"].as_bool().unwrap_or(false);
    let is_directory = stat["is_directory"].as_bool().unwrap_or(false);
    let freshness = file_freshness(&stat);
    Response::success(ResponseData::FileStat(agent_rdp_protocol::FileStatResult {
        path: params.remote_path,
        exists,
        is_directory,
        size: if exists && !is_directory { stat["size"].as_u64() } else { None },
        sha256: stat["sha256"].as_str().map(str::to_string),
        modified: freshness.as_ref().map(|f| f.modified.clone()),
        modified_unix: freshness.as_ref().map(|f| f.modified_unix),
        age_secs: freshness.as_ref().map(|f| f.age_secs),
    }))
}

/// What `file_stat` says about when the file was written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    pub modified: String,
    pub modified_unix: u64,
    pub age_secs: u64,
}

/// Freshness from a `file_stat` reply, or `None` when the agent does not
/// report times (pre-1.5.0). The age saturates: a file "modified in the
/// future" (clock stepped back) is simply fresh, not an underflow.
pub fn file_freshness(stat: &serde_json::Value) -> Option<Freshness> {
    let modified_unix = stat["modified_unix"].as_u64()?;
    let now_unix = stat["now_unix"].as_u64()?;
    Some(Freshness {
        modified: crate::timefmt::utc_rfc3339(
            std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified_unix),
        ),
        modified_unix,
        age_secs: now_unix.saturating_sub(modified_unix),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_comes_from_the_remote_clock() {
        let stat = serde_json::json!({
            "exists": true, "size": 10, "sha256": "x",
            "modified_unix": 1_788_360_301u64, "now_unix": 1_788_360_401u64
        });
        let f = file_freshness(&stat).unwrap();
        assert_eq!(f.age_secs, 100);
        assert_eq!(f.modified, "2026-09-02T14:45:01Z");
        assert_eq!(f.modified_unix, 1_788_360_301);
    }

    #[test]
    fn future_mtime_is_fresh_not_underflow() {
        let stat = serde_json::json!({ "modified_unix": 200u64, "now_unix": 100u64 });
        assert_eq!(file_freshness(&stat).unwrap().age_secs, 0);
    }

    #[test]
    fn old_agents_report_no_freshness() {
        let stat = serde_json::json!({ "exists": true, "size": 10, "sha256": "x" });
        assert!(file_freshness(&stat).is_none());
    }

    #[test]
    fn chunking_covers_every_byte_exactly_once() {
        let data: Vec<u8> = (0..CHUNK_BYTES * 2 + 7).map(|i| (i % 251) as u8).collect();
        let chunks: Vec<&[u8]> = data.chunks(CHUNK_BYTES).collect();

        assert_eq!(chunks.len(), 3);
        let reassembled: Vec<u8> = chunks.concat();
        assert_eq!(reassembled, data);
    }

    #[test]
    fn a_file_smaller_than_one_chunk_is_a_single_chunk() {
        let data = vec![1u8, 2, 3];
        assert_eq!(data.chunks(CHUNK_BYTES).count(), 1);
    }

    #[test]
    fn hashing_matches_a_known_vector() {
        // SHA-256 of "abc".
        let mut hasher = Sha256::new();
        hasher.update(b"abc");
        assert_eq!(
            hex_digest(hasher),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn hashing_detects_a_one_byte_difference() {
        let mut a = Sha256::new();
        a.update(b"payload");
        let mut b = Sha256::new();
        b.update(b"payloae");
        assert_ne!(hex_digest(a), hex_digest(b));
    }

    #[test]
    fn base64_round_trips_arbitrary_bytes() {
        // Byte-exactness is the whole point: text-mode transfer is what
        // corrupted non-ASCII content before.
        let data: Vec<u8> = (0..=255u8).collect();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&data);
        let decoded = base64::engine::general_purpose::STANDARD.decode(&encoded).unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn base64_expansion_stays_within_the_agents_read_buffer() {
        // The agent reads into a 1MB buffer; a chunk plus JSON overhead has
        // to fit or the message is silently truncated.
        let encoded_len = (CHUNK_BYTES + 2) / 3 * 4;
        assert!(encoded_len < 900 * 1024, "encoded chunk is {} bytes", encoded_len);
    }
}

#[cfg(test)]
mod pull_retry_tests {
    use super::*;

    /// The retry exists for a file rewritten every few seconds; a 128MB file
    /// re-read three times could outlast the CLI's own budget for the command.
    #[test]
    fn only_small_files_are_re_read() {
        assert!(PULL_RETRY_MAX_BYTES < MAX_TRANSFER_BYTES);
        assert!(PULL_RETRY_MAX_BYTES > CHUNK_BYTES as u64);
    }

    /// Backoff doubles, and the whole ladder stays short enough that three
    /// attempts still feel like one command.
    #[test]
    fn backoff_doubles_and_stays_short() {
        assert_eq!(pull_backoff(1), PULL_RETRY_BASE);
        assert_eq!(pull_backoff(2), PULL_RETRY_BASE * 2);
        let total: std::time::Duration = (1..PULL_ATTEMPTS).map(pull_backoff).sum();
        assert!(total < std::time::Duration::from_secs(1), "{:?}", total);
        // Never panics however far the caller counts.
        assert!(pull_backoff(u32::MAX) > std::time::Duration::ZERO);
        assert!(pull_backoff(0) > std::time::Duration::ZERO);
    }

    /// The loop's actual decision: retry a small file until the attempts run
    /// out, never retry a big one.
    #[test]
    fn the_retry_loop_stops_where_it_should() {
        let small = 1024;
        let quick = std::time::Duration::ZERO;
        assert!(retry_pull(1, small, quick), "a first mismatch is retried");
        assert!(retry_pull(PULL_ATTEMPTS - 1, small, quick), "the last retry is allowed");
        assert!(
            !retry_pull(PULL_ATTEMPTS, small, quick),
            "after the final attempt it must report, not loop"
        );
        // Sequence a whole run: exactly PULL_ATTEMPTS attempts happen.
        let attempts = (1..).take_while(|a| retry_pull(*a, small, quick)).count() + 1;
        assert_eq!(attempts as u32, PULL_ATTEMPTS);

        assert!(
            !retry_pull(1, PULL_RETRY_MAX_BYTES + 1, quick),
            "a large file is never re-read: three passes could outlast the CLI budget"
        );
        assert!(
            retry_pull(1, PULL_RETRY_MAX_BYTES, quick),
            "the threshold itself still retries"
        );
    }

    /// Size is not a time bound: a small file behind a stalled agent can
    /// still spend a full chunk timeout per attempt.
    #[test]
    fn a_slow_attempt_is_not_retried_however_small_the_file() {
        assert!(!retry_pull(1, 1024, PULL_RETRY_MAX_ELAPSED));
        assert!(!retry_pull(1, 1024, CHUNK_TIMEOUT), "one stalled chunk already exceeds it");
        assert!(retry_pull(1, 1024, PULL_RETRY_MAX_ELAPSED - std::time::Duration::from_millis(1)));
        // The whole retry ladder must fit inside the bound it is gated on.
        let sleeps: std::time::Duration = (1..PULL_ATTEMPTS).map(pull_backoff).sum();
        assert!(sleeps < PULL_RETRY_MAX_ELAPSED);
    }

    /// A mid-transfer change must not read as a transfer bug: callers polling
    /// a file its producer rewrites need to tell the two apart.
    #[test]
    fn a_changed_file_is_not_an_internal_error() {
        let changed = PullAttempt::Changed {
            remote: "aaa".into(),
            local: "bbb".into(),
            size: 10,
        };
        assert!(matches!(changed, PullAttempt::Changed { .. }));
        assert_eq!(
            serde_json::to_value(ErrorCode::FileChangedDuringTransfer).unwrap(),
            serde_json::Value::String("file_changed_during_transfer".into())
        );
        assert_ne!(ErrorCode::FileChangedDuringTransfer, ErrorCode::InternalError);
    }

    /// Each attempt re-stats, so `--max-age` describes the bytes actually
    /// delivered rather than the first attempt's.
    #[test]
    fn freshness_is_evaluated_per_attempt() {
        let source = include_str!("file_transfer.rs");
        let attempt_fn = source
            .split("async fn pull_once")
            .nth(1)
            .expect("pull_once exists");
        assert!(attempt_fn.contains("FileStat"), "each attempt re-stats");
        assert!(attempt_fn.contains("max_age_secs"), "freshness lives inside the attempt");
    }
}

#[cfg(test)]
mod push_integrity_tests {
    use super::*;

    #[test]
    fn a_relative_local_path_is_refused() {
        // It would be read from the daemon's working directory, which is
        // whatever shell started the daemon - possibly days ago. A stale
        // same-named file there transfers and verifies perfectly.
        let err = check_paths("report.json", "C:\\out\\report.json").unwrap_err();
        assert_eq!(err.error.unwrap().code, ErrorCode::InvalidRequest);
        // The daemon runs on any of Linux/macOS/Windows, and "absolute" means
        // whatever `Path::is_absolute` says on that host - a Unix path is not
        // absolute by Windows's rules, so this has to use a path this test's
        // own platform actually treats as absolute.
        let absolute = std::env::temp_dir().join("report.json");
        assert!(absolute.is_absolute());
        assert!(check_paths(&absolute.to_string_lossy(), "C:\\out\\report.json").is_ok());
    }

    #[test]
    fn a_relative_remote_path_is_refused() {
        // A local path this platform calls absolute, so the remote check is
        // the one that fires (`/tmp/x` is relative by Windows's rules).
        let local = std::env::temp_dir().join("x");
        let err = check_paths(&local.to_string_lossy(), "out\\report.json").unwrap_err();
        let info = err.error.unwrap();
        assert_eq!(info.code, ErrorCode::InvalidRequest);
        assert!(info.message.contains("C:"), "the message shows what a good path looks like");
    }

    #[test]
    fn absolute_windows_paths_are_recognised() {
        for good in [
            "C:\\dir\\f.txt",
            "c:/dir/f.txt",
            "C:\\",
            "D:/mixed\\separators/f.txt",
            "\\\\srv\\share\\f.txt",
            "//srv/share/f",
        ] {
            assert!(is_absolute_windows_path(good), "{good} is absolute");
        }
        for bad in ["dir\\f.txt", "./f.txt", "/usr/local/f", "C:", "C:f.txt", "", " ", "1:\\x"] {
            assert!(!is_absolute_windows_path(bad), "{bad} is not an absolute Windows path");
        }
    }

    #[test]
    fn empty_paths_are_refused_on_both_sides() {
        assert!(check_paths("", "C:\\f").is_err());
        let absolute = std::env::temp_dir().join("f");
        assert!(check_paths(&absolute.to_string_lossy(), "").is_err());
    }

    /// The agent's reply is untrusted input; a reply that is not even an
    /// object must land in the "nothing to verify against" branch, not panic.
    #[test]
    fn a_non_object_reply_cannot_verify_a_push() {
        for reply in [serde_json::Value::Null, serde_json::json!("ok"), serde_json::json!([1, 2])] {
            let err = verify_push(Some(&reply), "abc", 4, "C:\\f").unwrap_err();
            assert_eq!(err.error.unwrap().code, ErrorCode::TransferVerificationFailed);
        }
    }

    /// The overwrite branch failed on every attempt in the field: PowerShell
    /// hands a .NET `string` parameter "" for `$null`, and File.Replace
    /// rejects "" as a backup path. Only [NullString]::Value is a real null.
    #[test]
    fn the_overwrite_passes_a_real_null_for_the_backup_path() {
        let script = include_str!("../automation/scripts/lib/actions.ps1");
        assert!(script.contains("[System.IO.File]::Replace($part, $path, [NullString]::Value)"));
        assert!(
            !script.contains("::Replace($part, $path, $null)"),
            "`$null` becomes \"\" here and every overwrite fails"
        );
        // The failure text names the sidecar, so a report can be acted on.
        assert!(script.contains("staged as '$part'"));
    }

    /// The daemon used to report the hash of the bytes it *sent* as though
    /// the remote had confirmed it. The agent's own check was the only
    /// verification, and it skipped itself whenever the final chunk arrived
    /// without a hash.
    #[test]
    fn a_push_is_checked_against_what_the_agent_reports() {
        let sha = "abc123";
        let good = serde_json::json!({ "sha256": "ABC123", "total_size": 4, "path": "C:\\f" });
        assert!(
            verify_push(Some(&good), sha, 4, "C:\\f").is_ok(),
            "case differences are not a mismatch"
        );

        let wrong_hash = serde_json::json!({ "sha256": "deadbeef", "total_size": 4 });
        let err = verify_push(Some(&wrong_hash), sha, 4, "C:\\f").unwrap_err();
        assert_eq!(err.error.unwrap().code, ErrorCode::TransferVerificationFailed);

        let wrong_size = serde_json::json!({ "sha256": sha, "total_size": 9 });
        assert!(verify_push(Some(&wrong_size), sha, 4, "C:\\f").is_err());
    }

    #[test]
    fn an_unverifiable_push_is_a_failure_not_a_success() {
        // No hash at all, and no reply at all: both used to pass silently.
        let no_hash = serde_json::json!({ "total_size": 4 });
        assert!(verify_push(Some(&no_hash), "abc", 4, "C:\\f").is_err());
        assert!(verify_push(None, "abc", 4, "C:\\f").is_err());
    }

    #[test]
    fn an_older_agent_without_a_size_still_verifies_by_hash() {
        // Agent 1.7.0 reports `sha256` but no `total_size` for the final
        // chunk; the hash alone is still a real check.
        let old = serde_json::json!({ "sha256": "abc" });
        assert!(verify_push(Some(&old), "abc", 4, "C:\\f").is_ok());
    }

    /// The remote write is staged and swapped in, so a transfer that fails
    /// part-way leaves the previous file intact.
    #[test]
    fn the_remote_write_is_staged_before_it_replaces_anything() {
        let script = include_str!("../automation/scripts/lib/actions.ps1");
        assert!(script.contains(".agent-rdp-$tid.part"));
        assert!(
            script.contains("[System.IO.File]::Replace($part, $path, [NullString]::Value)"),
            "Move-Item is delete-then-move, and moves into a directory rather than failing"
        );
        assert!(
            script.contains("Cannot write '$path': it is a directory"),
            "a directory destination must fail, not swallow the file"
        );
        assert!(
            script.contains("carried no expected hash"),
            "a missing hash is a failure, not a skipped check"
        );
    }
}
