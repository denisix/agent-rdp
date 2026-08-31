//! IPC client for communicating with the daemon.

use std::io;
use std::path::Path;
use std::time::Duration;

use agent_rdp_protocol::{Request, Response};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::time::timeout;

/// Default connect timeout in seconds.
const CONNECT_TIMEOUT_SECS: u64 = 15;

/// Largest accepted response line, in bytes. Mirrors the daemon-side cap:
/// `read_line` is otherwise unbounded, so a broken peer could grow the CLI's
/// memory without limit. 64MB covers the largest legitimate payload (a
/// full-desktop PNG as base64 in JSON) with a wide margin.
const MAX_IPC_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[cfg(unix)]
type ReadHalf = tokio::net::unix::OwnedReadHalf;
#[cfg(unix)]
type WriteHalf = tokio::net::unix::OwnedWriteHalf;

#[cfg(windows)]
type ReadHalf = tokio::net::tcp::OwnedReadHalf;
#[cfg(windows)]
type WriteHalf = tokio::net::tcp::OwnedWriteHalf;

/// IPC client for daemon communication.
///
/// The reader half stays wrapped in one persistent `BufReader` for the life of
/// the client. The previous design built a fresh `BufReader` per response,
/// which silently discarded any bytes buffered past the newline - a latent
/// desync whenever more than one line was in flight.
pub struct IpcClient {
    reader: BufReader<ReadHalf>,
    writer: WriteHalf,
}

impl IpcClient {
    /// Connect to the daemon for the given session with a timeout.
    #[cfg(unix)]
    pub async fn connect(socket_path: &Path) -> io::Result<Self> {
        let connect_future = tokio::net::UnixStream::connect(socket_path);
        let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect_future)
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Connection to daemon timed out after {}s", CONNECT_TIMEOUT_SECS),
                )
            })??;
        let (read, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, read),
            writer,
        })
    }

    #[cfg(windows)]
    pub async fn connect(socket_path: &Path) -> io::Result<Self> {
        // On Windows, derive port from session name
        let session = socket_path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("default");

        let port = agent_rdp_daemon::get_session_port(session);
        let addr = format!("127.0.0.1:{}", port);
        let connect_future = tokio::net::TcpStream::connect(&addr);
        let stream = timeout(Duration::from_secs(CONNECT_TIMEOUT_SECS), connect_future)
            .await
            .map_err(|_| {
                io::Error::new(
                    io::ErrorKind::TimedOut,
                    format!("Connection to daemon timed out after {}s", CONNECT_TIMEOUT_SECS),
                )
            })??;
        let (read, writer) = stream.into_split();
        Ok(Self {
            reader: BufReader::with_capacity(64 * 1024, read),
            writer,
        })
    }

    /// Send a request and receive a response.
    pub async fn send(&mut self, request: &Request, timeout_ms: u64) -> anyhow::Result<Response> {
        let mut json = serde_json::to_vec(request)?;
        json.push(b'\n');

        // Write request and flush to ensure it's sent immediately
        self.writer.write_all(&json).await?;
        self.writer.flush().await?;

        // Read response with timeout. If the timeout fires mid-response the
        // stream is desynchronized (part of a line was consumed), so this
        // client must not be reused after a timeout error - callers open a
        // fresh client per command, which satisfies that.
        let response = timeout(Duration::from_millis(timeout_ms), self.read_response())
            .await
            .map_err(|_| anyhow::anyhow!("Request timed out"))??;

        Ok(response)
    }

    /// Read a response from the stream.
    async fn read_response(&mut self) -> anyhow::Result<Response> {
        // Screenshot responses run to a few MB of base64; give the line a
        // real starting capacity instead of growing from empty.
        let mut line = String::with_capacity(64 * 1024);
        read_line_capped(&mut self.reader, &mut line, MAX_IPC_MESSAGE_BYTES).await?;

        let response: Response = serde_json::from_str(line.trim())?;
        Ok(response)
    }
}

/// Read one `\n`-terminated line, erroring once `max_bytes` is exceeded.
///
/// The plain `read_line` accepts input without bound; this variant gives up as
/// soon as the accumulated line passes the cap, so a malformed or hostile peer
/// cannot OOM the process.
pub async fn read_line_capped<R>(
    reader: &mut R,
    line: &mut String,
    max_bytes: usize,
) -> io::Result<usize>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut buf: Vec<u8> = Vec::new();

    loop {
        let available = reader.fill_buf().await?;

        if available.is_empty() {
            break; // EOF
        }

        if let Some(pos) = available.iter().position(|&b| b == b'\n') {
            buf.extend_from_slice(&available[..=pos]);
            reader.consume(pos + 1);
            break;
        }

        let n = available.len();
        buf.extend_from_slice(available);
        reader.consume(n);

        if buf.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("IPC message exceeds {} byte limit", max_bytes),
            ));
        }
    }

    if buf.len() > max_bytes {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("IPC message exceeds {} byte limit", max_bytes),
        ));
    }

    let total = buf.len();
    let text = String::from_utf8(buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    line.push_str(&text);
    Ok(total)
}

/// Try to connect to an existing daemon, with optional retries.
pub async fn try_connect(socket_path: &Path, retries: u32, delay_ms: u64) -> io::Result<IpcClient> {
    // `retries` counts attempts; zero attempts makes no sense, so it means
    // "try once". The previous version returned a bogus "No connection
    // attempts made" error for 0 and slept pointlessly after the final
    // failure.
    let attempts = retries.max(1);
    let mut last_error = None;

    for attempt in 0..attempts {
        match IpcClient::connect(socket_path).await {
            Ok(client) => return Ok(client),
            Err(e) => {
                last_error = Some(e);
                if attempt + 1 < attempts {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
            }
        }
    }

    Err(last_error.expect("at least one attempt was made"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_read_line_capped_under_the_cap() {
        let data = b"hello world\nrest".to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        let n = read_line_capped(&mut reader, &mut line, 1024).await.unwrap();
        assert_eq!(line, "hello world\n");
        assert_eq!(n, 12);
    }

    #[tokio::test]
    async fn test_read_line_capped_over_the_cap_errors() {
        // No newline within the cap: must error, not accumulate forever.
        let data = vec![b'x'; 4096];
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        let err = read_line_capped(&mut reader, &mut line, 100).await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn test_read_line_capped_line_exactly_at_cap() {
        // 9 bytes of payload + newline == 10 == cap: allowed.
        let data = b"123456789\n".to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        read_line_capped(&mut reader, &mut line, 10).await.unwrap();
        assert_eq!(line, "123456789\n");
    }

    #[tokio::test]
    async fn test_read_line_capped_eof_without_newline() {
        let data = b"partial".to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        let n = read_line_capped(&mut reader, &mut line, 1024).await.unwrap();
        assert_eq!(line, "partial");
        assert_eq!(n, 7);
    }

    #[tokio::test]
    async fn test_read_line_capped_consecutive_lines_keep_stream_sync() {
        // The second read must pick up exactly where the first stopped -
        // this is the property the per-call BufReader broke.
        let data = b"first\nsecond\n".to_vec();
        let mut reader = BufReader::new(std::io::Cursor::new(data));

        let mut line = String::new();
        read_line_capped(&mut reader, &mut line, 1024).await.unwrap();
        assert_eq!(line, "first\n");

        let mut line = String::new();
        read_line_capped(&mut reader, &mut line, 1024).await.unwrap();
        assert_eq!(line, "second\n");
    }

    #[tokio::test]
    async fn test_read_line_capped_rejects_invalid_utf8() {
        let data = vec![0xFF, 0xFE, b'\n'];
        let mut reader = BufReader::new(std::io::Cursor::new(data));
        let mut line = String::new();
        assert!(read_line_capped(&mut reader, &mut line, 1024).await.is_err());
    }
}
