//! Clipboard backend for CLIPRDR integration.
//!
//! This module provides a custom clipboard backend that stores clipboard data
//! and communicates with the frame processor via channels.

use std::sync::Arc;

use ironrdp_cliprdr::backend::{CliprdrBackend, ClipboardMessage, ClipboardMessageProxy};
use ironrdp_cliprdr::pdu::{
    ClipboardGeneralCapabilityFlags, FileContentsRequest, FileContentsResponse,
    FormatDataRequest, FormatDataResponse, LockDataId, OwnedFormatDataResponse,
};
use ironrdp_cliprdr::{Cliprdr, Client};
use ironrdp_svc::impl_as_any;
use parking_lot::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

// Re-export types needed by rdp_session
pub use ironrdp_cliprdr::pdu::{ClipboardFormat, ClipboardFormatId};
pub use ironrdp_cliprdr::CliprdrClient;

/// Standard clipboard format ID for Unicode text (CF_UNICODETEXT = 13).
pub fn cf_unicodetext() -> ClipboardFormatId {
    ClipboardFormatId::new(13)
}

/// Encode text as `CF_UNICODETEXT`: UTF-16LE, CRLF line endings, NUL-terminated.
///
/// Windows text is CRLF. Sending a bare `\n` is off-spec, and it is exactly
/// what made a multi-line script pasted via `clipboard set` come out of
/// `Get-Clipboard | Set-Content` as one line - `Get-Clipboard` splits on CRLF.
/// Normalizing here is idempotent: already-CRLF input is not turned into CRCRLF.
pub fn encode_cf_unicodetext(text: &str) -> Vec<u8> {
    let normalized = normalize_to_crlf(text);
    let utf16: Vec<u16> = normalized
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    utf16.iter().flat_map(|&c| c.to_le_bytes()).collect()
}

/// `\n` and lone `\r` become `\r\n`; existing `\r\n` is left alone.
pub fn normalize_to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 16);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\r\n");
            }
            '\n' => out.push_str("\r\n"),
            other => out.push(other),
        }
    }
    out
}

#[cfg(test)]
mod line_ending_tests {
    use super::*;

    fn decode(bytes: &[u8]) -> String {
        let utf16: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&utf16).unwrap()
    }

    #[test]
    fn lf_becomes_crlf() {
        assert_eq!(normalize_to_crlf("a\nb\n"), "a\r\nb\r\n");
    }

    #[test]
    fn crlf_is_unchanged() {
        assert_eq!(normalize_to_crlf("a\r\nb\r\n"), "a\r\nb\r\n");
    }

    #[test]
    fn normalization_is_idempotent() {
        let once = normalize_to_crlf("x\ny\r\nz\r");
        assert_eq!(normalize_to_crlf(&once), once);
        assert_eq!(once, "x\r\ny\r\nz\r\n");
    }

    #[test]
    fn encoded_text_is_nul_terminated_utf16le_with_crlf() {
        let bytes = encode_cf_unicodetext("Привет\nmir");
        assert_eq!(&bytes[bytes.len() - 2..], &[0, 0]);
        assert_eq!(decode(&bytes), "Привет\r\nmir\0");
    }

    #[test]
    fn empty_text_is_just_the_terminator() {
        assert_eq!(encode_cf_unicodetext(""), vec![0, 0]);
    }
}

/// Messages from backend to frame processor.
#[derive(Debug)]
pub enum BackendMessage {
    /// Backend wants to initiate copy (announce formats).
    InitiateCopy(Vec<ClipboardFormat>),
    /// Backend has format data ready to send.
    FormatData(OwnedFormatDataResponse),
    /// Backend wants to request data from remote.
    InitiatePaste(ClipboardFormatId),
}

/// Proxy that sends messages to the frame processor.
#[derive(Debug, Clone)]
pub struct ChannelProxy {
    tx: mpsc::UnboundedSender<BackendMessage>,
}

impl ChannelProxy {
    pub fn new(tx: mpsc::UnboundedSender<BackendMessage>) -> Self {
        Self { tx }
    }
}

impl ClipboardMessageProxy for ChannelProxy {
    fn send_clipboard_message(&self, message: ClipboardMessage) {
        let backend_msg = match message {
            ClipboardMessage::SendInitiateCopy(formats) => BackendMessage::InitiateCopy(formats),
            ClipboardMessage::SendFormatData(data) => BackendMessage::FormatData(data),
            ClipboardMessage::SendInitiatePaste(format_id) => BackendMessage::InitiatePaste(format_id),
            ClipboardMessage::Error(e) => {
                warn!("Clipboard backend error: {}", e);
                return;
            }
            // File-transfer clipboard (copying files between host and remote)
            // is not implemented - only text is. Listed explicitly rather than
            // caught by a wildcard so that new variants keep breaking the build
            // instead of being silently swallowed here.
            ClipboardMessage::SendInitiateFileCopy(_)
            | ClipboardMessage::SendFileContentsRequest(_)
            | ClipboardMessage::SendFileContentsResponse(_) => {
                debug!("Ignoring clipboard file-transfer message: not supported");
                return;
            }
        };
        let _ = self.tx.send(backend_msg);
    }
}

/// Shared state for clipboard data.
#[derive(Debug)]
pub struct ClipboardState {
    /// Text we want to send to remote (set by clipboard set command).
    pub local_text: Option<String>,
    /// Text received from remote.
    pub remote_text: Option<String>,
    /// Formats available on remote clipboard.
    pub remote_formats: Vec<ClipboardFormat>,
    /// Pending text get request response channel.
    pub pending_get: Option<tokio::sync::oneshot::Sender<Result<Option<String>, String>>>,
    /// Notify when remote clipboard changes (for WebSocket integration).
    pub clipboard_changed_tx: Option<mpsc::UnboundedSender<()>>,
}

impl Default for ClipboardState {
    fn default() -> Self {
        Self {
            local_text: None,
            remote_text: None,
            remote_formats: Vec::new(),
            pending_get: None,
            clipboard_changed_tx: None,
        }
    }
}

/// Custom clipboard backend that stores data in memory.
#[derive(Debug)]
pub struct AgentClipboardBackend {
    state: Arc<Mutex<ClipboardState>>,
    proxy: ChannelProxy,
}

impl_as_any!(AgentClipboardBackend);

impl AgentClipboardBackend {
    pub fn new(state: Arc<Mutex<ClipboardState>>, proxy: ChannelProxy) -> Self {
        Self { state, proxy }
    }
}

impl CliprdrBackend for AgentClipboardBackend {
    fn temporary_directory(&self) -> &str {
        ".cliprdr"
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::USE_LONG_FORMAT_NAMES
    }

    fn on_ready(&mut self) {
        info!("CLIPRDR clipboard ready");
    }

    fn on_request_format_list(&mut self) {
        debug!("Backend: on_request_format_list");
        // During initialization, send our available formats (if any).
        let state = self.state.lock();
        if state.local_text.is_some() {
            let formats = vec![ClipboardFormat::new(cf_unicodetext())];
            self.proxy.send_clipboard_message(ClipboardMessage::SendInitiateCopy(formats));
        } else {
            // Send empty format list to complete initialization.
            self.proxy.send_clipboard_message(ClipboardMessage::SendInitiateCopy(vec![]));
        }
    }

    fn on_process_negotiated_capabilities(&mut self, _capabilities: ClipboardGeneralCapabilityFlags) {
        debug!("Backend: negotiated capabilities");
    }

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        debug!("Backend: remote copied, formats: {:?}", available_formats);
        let mut state = self.state.lock();
        state.remote_formats = available_formats.to_vec();
        // Clear old remote data since new data is available.
        state.remote_text = None;

        // Notify WebSocket clients that clipboard changed (if channel is set up).
        if let Some(ref tx) = state.clipboard_changed_tx {
            let _ = tx.send(());
        }
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        debug!("Backend: format data request for {:?}", request.format);
        let state = self.state.lock();

        let response = if request.format == cf_unicodetext() {
            if let Some(ref text) = state.local_text {
                OwnedFormatDataResponse::new_data(encode_cf_unicodetext(text))
            } else {
                OwnedFormatDataResponse::new_error()
            }
        } else {
            OwnedFormatDataResponse::new_error()
        };

        self.proxy.send_clipboard_message(ClipboardMessage::SendFormatData(response));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        debug!("Backend: format data response, is_error={}", response.is_error());

        let mut state = self.state.lock();

        if response.is_error() {
            // Server returned error - clipboard is empty or doesn't have text format.
            // This is normal, not an error condition.
            if let Some(tx) = state.pending_get.take() {
                let _ = tx.send(Ok(None));
            }
            return;
        }

        // Decode UTF-16LE to String.
        let data = response.data();
        if data.len() >= 2 {
            let utf16: Vec<u16> = data
                .chunks_exact(2)
                .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
                .collect();
            // Remove null terminator if present.
            let text: String = String::from_utf16_lossy(&utf16)
                .trim_end_matches('\0')
                .to_string();

            debug!("Received clipboard text: {} chars", text.len());
            state.remote_text = Some(text.clone());

            if let Some(tx) = state.pending_get.take() {
                let _ = tx.send(Ok(Some(text)));
            }
        } else if let Some(tx) = state.pending_get.take() {
            let _ = tx.send(Ok(None));
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {
        debug!("Backend: file contents request (not supported)");
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {
        debug!("Backend: file contents response (not supported)");
    }

    fn on_lock(&mut self, _data_id: LockDataId) {
        debug!("Backend: lock");
    }

    fn on_unlock(&mut self, _data_id: LockDataId) {
        debug!("Backend: unlock");
    }
}

/// Create the cliprdr client with our custom backend.
/// Returns the cliprdr client and a receiver for backend messages.
pub fn create_cliprdr(
    state: Arc<Mutex<ClipboardState>>,
) -> (CliprdrClient, mpsc::UnboundedReceiver<BackendMessage>) {
    let (proxy_tx, proxy_rx) = mpsc::unbounded_channel();
    let proxy = ChannelProxy::new(proxy_tx);
    let backend = Box::new(AgentClipboardBackend::new(state, proxy));
    let cliprdr = Cliprdr::<Client>::new(backend);
    (cliprdr, proxy_rx)
}
