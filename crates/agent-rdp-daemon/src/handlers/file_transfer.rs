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
use tracing::info;

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
        };

        let timeout = if last { VERIFY_TIMEOUT } else { CHUNK_TIMEOUT };
        if let Err(e) = ipc.send_request_with_timeout(&request, timeout).await {
            return Response::error(
                ErrorCode::AutomationError,
                format!(
                    "Transfer failed on chunk {}/{} of '{}': {}",
                    index + 1,
                    total_chunks,
                    params.remote_path,
                    e
                ),
            );
        }
    }

    info!("Pushed {} bytes to {}", data.len(), params.remote_path);

    Response::success(ResponseData::FileTransferResult(FileTransferResult {
        path: params.remote_path,
        bytes: data.len() as u64,
        sha256,
        chunks: total_chunks as u64,
    }))
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
            return Response::error(
                ErrorCode::AutomationError,
                format!("Cannot stat '{}': {}", params.remote_path, e),
            )
        }
    };

    if !stat["exists"].as_bool().unwrap_or(false) {
        return Response::error(
            ErrorCode::InvalidRequest,
            format!("Remote file not found: {}", params.remote_path),
        );
    }
    if stat["is_directory"].as_bool().unwrap_or(false) {
        return Response::error(
            ErrorCode::InvalidRequest,
            format!("'{}' is a directory, not a file", params.remote_path),
        );
    }

    let total_size = stat["size"].as_u64().unwrap_or(0);
    if total_size > MAX_TRANSFER_BYTES {
        return Response::error(
            ErrorCode::InvalidRequest,
            format!(
                "'{}' is {} bytes; the transfer limit is {} bytes",
                params.remote_path, total_size, MAX_TRANSFER_BYTES
            ),
        );
    }
    let remote_sha256 = stat["sha256"].as_str().unwrap_or_default().to_string();

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
                return Response::error(
                    ErrorCode::AutomationError,
                    format!(
                        "Transfer failed at offset {} of '{}': {}",
                        data.len(),
                        params.remote_path,
                        e
                    ),
                )
            }
        };

        let encoded = value["data_b64"].as_str().unwrap_or_default();
        let decoded = match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Response::error(
                    ErrorCode::InternalError,
                    format!("Agent returned an undecodable chunk: {}", e),
                )
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
            return Response::error(
                ErrorCode::InternalError,
                format!(
                    "Transfer stalled at offset {} of '{}' (agent returned no data)",
                    data.len(),
                    params.remote_path
                ),
            );
        }

        data.extend_from_slice(&decoded);
    }

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let local_sha256 = hex_digest(hasher);

    if !remote_sha256.is_empty() && remote_sha256 != local_sha256 {
        return Response::error(
            ErrorCode::InternalError,
            format!(
                "Transfer verification failed for '{}': remote hash {}, received {}",
                params.remote_path, remote_sha256, local_sha256
            ),
        );
    }

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
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
