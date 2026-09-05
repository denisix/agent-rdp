//! Main daemon event loop.

use std::sync::Arc;
use std::time::{Duration, Instant};

use agent_rdp_protocol::{Request, Response, ResponseData, SessionInfo, ConnectionState, ErrorCode};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, error, info, warn};

use crate::automation::{new_shared_state, AutomationBootstrap, SharedAutomationState};
use crate::handlers;
use crate::ipc_server::IpcServer;
use crate::rdp_session::{DisconnectEvent, RdpSession};
use crate::ws_server::WsServerHandle;

/// Highest allowed streaming frame rate.
///
/// The cap matters beyond taste: the frame period is computed as
/// `1000 / fps` milliseconds, and fps above 1000 would yield a zero period,
/// which `tokio::time::interval` panics on.
pub const MAX_STREAM_FPS: u32 = 60;

/// Largest accepted IPC request line, in bytes.
///
/// `read_line` is otherwise unbounded, and on Windows the IPC endpoint is
/// loopback TCP that any local process can reach - an endless line must not
/// be able to grow the daemon's memory without limit. 64MB comfortably covers
/// the largest legitimate payload (a full-desktop PNG as base64 in JSON).
pub const MAX_IPC_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

/// Shared WebSocket server state that can be started/stopped dynamically.
pub type SharedWsHandle = Arc<Mutex<Option<WsServerHandle>>>;

/// Clipboard change notification receiver (from RDP clipboard backend to daemon).
pub type ClipboardChangedRx = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>>;

/// The main daemon that manages an RDP session.
pub struct Daemon {
    /// Session name.
    session_name: String,

    /// The RDP session (if connected).
    rdp_session: Arc<Mutex<Option<RdpSession>>>,

    /// Automation state for UI automation.
    automation_state: SharedAutomationState,

    /// IPC server for CLI communication.
    ipc_server: IpcServer,

    /// Time when daemon started.
    start_time: Instant,

    /// Shutdown signal sender.
    shutdown_tx: broadcast::Sender<()>,

    /// Channel to receive connection drop notifications from RDP session,
    /// carrying the generation of the session that dropped and why.
    disconnect_rx: tokio::sync::mpsc::Receiver<DisconnectEvent>,

    /// Sender for connection drop notifications (passed to RDP sessions).
    disconnect_tx: tokio::sync::mpsc::Sender<DisconnectEvent>,

    /// The most recent transport drop, kept until the next successful
    /// `connect`, so "not connected" can say *why* and *when* - the
    /// difference between "reconnect the session" and "the daemon is gone".
    last_disconnect: SharedLastDisconnect,

    /// Generation of the session currently stored in `rdp_session`.
    ///
    /// Bumped by every `connect`. A drop notification tagged with an older
    /// generation belongs to a session that has already been replaced and
    /// must be ignored - acting on it would tear down the live session that
    /// took its place.
    session_generation: Arc<std::sync::atomic::AtomicU64>,

    /// WebSocket server handle for streaming (shared so connect handler can start it).
    ws_handle: SharedWsHandle,

    /// WebSocket streaming frame rate (used for frame broadcasting).
    stream_fps: u32,

    /// Clipboard change notification receiver (set up when RDP connects with WS streaming).
    clipboard_changed_rx: ClipboardChangedRx,
}

impl Daemon {
    /// Create a new daemon for the given session.
    pub async fn new(session_name: String) -> anyhow::Result<Self> {
        let socket_path = crate::get_socket_path(&session_name);

        // Clean up stale socket if it exists
        if socket_path.exists() {
            std::fs::remove_file(&socket_path)?;
        }

        let ipc_server = IpcServer::bind(&socket_path).await?;
        let (shutdown_tx, _) = broadcast::channel(1);
        // Room for a few notifications: with capacity 1 a stale notice from
        // an already-replaced session could sit buffered while the next
        // session's processor blocked trying to send its own.
        let (disconnect_tx, disconnect_rx) = tokio::sync::mpsc::channel(8);

        // Default frame rate (can be overridden by ConnectRequest)
        let stream_fps = crate::ws_server::get_stream_fps();

        let rdp_session = Arc::new(Mutex::new(None));

        // Initialize automation state
        let session_dir = crate::get_session_dir(&session_name);
        let automation_state = new_shared_state(session_dir);

        // WebSocket server is started dynamically when connect is called with stream_port > 0
        let ws_handle = Arc::new(Mutex::new(None));

        // Clipboard channels (receivers set up when RDP connects with WS streaming)
        let clipboard_changed_rx = Arc::new(Mutex::new(None));

        info!("Daemon started for session '{}' at {:?}", session_name, socket_path);

        Ok(Self {
            session_name,
            rdp_session,
            automation_state,
            ipc_server,
            start_time: Instant::now(),
            shutdown_tx,
            disconnect_rx,
            disconnect_tx,
            last_disconnect: Arc::new(std::sync::Mutex::new(None)),
            session_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            ws_handle,
            stream_fps,
            clipboard_changed_rx,
        })
    }

    /// Run the daemon event loop.
    pub async fn run(&mut self) -> anyhow::Result<()> {
        let mut shutdown_rx = self.shutdown_tx.subscribe();

        // Frame broadcast interval for WebSocket streaming. Starts at the
        // daemon-wide default and is re-tuned below once a stream is running,
        // since the rate can be chosen per-connection via ConnectRequest.
        // Clamp, don't just floor: fps >= 1001 makes the millisecond division
        // yield a zero period, and `tokio::time::interval` panics on that -
        // reachable straight from AGENT_RDP_STREAM_FPS. 60 is already beyond
        // what a JPEG-over-WebSocket stream can deliver.
        let mut current_fps = self.stream_fps.clamp(1, MAX_STREAM_FPS);
        let mut frame_timer =
            tokio::time::interval(Duration::from_millis(1000 / current_fps as u64));
        frame_timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Generation last sent to viewers. Skips re-copying and re-encoding
        // the framebuffer when nothing painted since the last tick, which
        // otherwise happens unconditionally at the full stream frame rate
        // even for a completely idle desktop.
        //
        // Known limitation: the generation counter resets to 0 on every
        // reconnect (it lives on the `RdpSession`, not the daemon), so in the
        // rare case where a fresh session's counter is still 0 when the next
        // tick fires, that tick is skipped. This self-corrects on the next
        // paint - reactivating the desktop itself counts as one - so the
        // worst case is one stale frame, not a stuck stream.
        let mut last_broadcast_generation: Option<u64> = None;

        loop {
            tokio::select! {
                // Accept new CLI connections
                result = self.ipc_server.accept() => {
                    match result {
                        Ok(stream) => {
                            let session = Arc::clone(&self.rdp_session);
                            let automation_state = Arc::clone(&self.automation_state);
                            let ws_handle = Arc::clone(&self.ws_handle);
                            let session_name = self.session_name.clone();
                            let start_time = self.start_time;
                            let shutdown_tx = self.shutdown_tx.clone();
                            let disconnect_tx = self.disconnect_tx.clone();
                            let last_disconnect = Arc::clone(&self.last_disconnect);
                            let clipboard_changed_rx = Arc::clone(&self.clipboard_changed_rx);
                            let session_generation = Arc::clone(&self.session_generation);

                            tokio::spawn(async move {
                                if let Err(e) = handle_client(stream, session, automation_state, ws_handle, session_name, start_time, shutdown_tx, disconnect_tx, clipboard_changed_rx, session_generation, last_disconnect).await {
                                    error!("Client handler error: {}", e);
                                }
                            });
                        }
                        Err(e) => {
                            error!("Failed to accept connection: {}", e);
                        }
                    }
                }

                // Handle connection drop from RDP session.
                //
                // Deliberately does NOT exit. The frame processor treats any
                // read error as terminal, so tearing the daemon down here meant
                // one transient RDP hiccup destroyed the whole session: every
                // in-flight command failed with `daemon_not_running`, and the
                // caller had to respawn a daemon rather than simply reconnect.
                // Drop the dead session instead and stay up, so commands report
                // "not connected" and `connect` can re-establish in place.
                Some(event) = self.disconnect_rx.recv() => {
                    let dropped_generation = event.generation;
                    // A notification from a session that has already been
                    // replaced must not touch the session that replaced it.
                    // Without this check, a drop notice still in flight when
                    // `connect` stored its new session tore that new session
                    // down - the caller saw a successful "Connected" and then
                    // `daemon_not_running`/"Not connected" on the very next
                    // command, and only a second reconnect appeared to help.
                    let current = self.session_generation.load(std::sync::atomic::Ordering::SeqCst);
                    if is_stale_disconnect(dropped_generation, current) {
                        info!(
                            "Ignoring stale disconnect from session generation {} (current is {})",
                            dropped_generation, current
                        );
                        continue;
                    }

                    warn!(
                        "RDP connection dropped ({}); session is now disconnected (daemon staying up)",
                        event.reason
                    );
                    *self.last_disconnect.lock().unwrap() = Some(DisconnectInfo {
                        at: std::time::SystemTime::now(),
                        reason: event.reason.clone(),
                    });
                    crate::transcript::append_event(
                        &self.session_name,
                        serde_json::json!({
                            "rdp_transport_dropped": { "reason": event.reason, "generation": dropped_generation }
                        }),
                    );

                    // Tear the dead session down on a separate task. This arm
                    // runs inside the accept loop's `select!`, so every lock it
                    // awaits here is time during which no new CLI connection
                    // is accepted - and `rdp_session` can be held for a long
                    // time by a handler mid-operation. The CLI's health check
                    // then timed out and reported the daemon as not running.
                    //
                    // Because the teardown is deferred, a `connect` can slip
                    // in before it runs. `connect` bumps the generation first
                    // thing, so re-checking it under each lock tells this task
                    // the state now belongs to a newer session and must be
                    // left alone.
                    let rdp_session = Arc::clone(&self.rdp_session);
                    let ws_handle = Arc::clone(&self.ws_handle);
                    let clipboard_changed_rx = Arc::clone(&self.clipboard_changed_rx);
                    let automation_state = Arc::clone(&self.automation_state);
                    let session_generation = Arc::clone(&self.session_generation);
                    tokio::spawn(async move {
                        let superseded = || {
                            session_generation.load(std::sync::atomic::Ordering::SeqCst)
                                != dropped_generation
                        };

                        {
                            let mut session = rdp_session.lock().await;
                            if superseded() {
                                info!("Dropped session already replaced by a newer connect; leaving it alone");
                                return;
                            }
                            *session = None;
                        }

                        // The stream belongs to the dead session - stop it too.
                        *ws_handle.lock().await = None;
                        *clipboard_changed_rx.lock().await = None;

                        // Automation belongs to the dead session too. Without
                        // this, a mid-session transport drop left
                        // `enabled=true` and `dvc_ipc=Some(<dead ipc>)` stale -
                        // `automate` calls would hang or fail against a channel
                        // that no longer has a remote end, and the state only
                        // got cleared by the next `connect`'s own pre-cleanup,
                        // not by the drop itself. `cleanup()` is a no-op when
                        // automation was never enabled, so this is safe to
                        // call unconditionally.
                        let mut auto_state = automation_state.lock().await;
                        if superseded() {
                            info!("Newer connect owns the automation state; skipping cleanup");
                            return;
                        }
                        let session_dir = crate::get_session_dir("");
                        let bootstrap = AutomationBootstrap::new(session_dir);
                        let _ = bootstrap.cleanup(&mut auto_state).await;
                        info!("Dropped session torn down");
                    });
                }

                // Handle shutdown signal from client
                _ = shutdown_rx.recv() => {
                    info!("Received shutdown request from client");
                    break;
                }

                // Handle Ctrl+C
                _ = tokio::signal::ctrl_c() => {
                    info!("Received Ctrl+C, cleaning up...");
                    break;
                }

                // Broadcast frames to WebSocket clients
                _ = frame_timer.tick() => {
                    let ws_handle = self.ws_handle.lock().await;
                    if let Some(ref handle) = *ws_handle {
                        // Adopt the rate the stream was started with, so
                        // ConnectRequest.stream_fps actually takes effect.
                        if handle.fps().clamp(1, MAX_STREAM_FPS) != current_fps {
                            current_fps = handle.fps().clamp(1, MAX_STREAM_FPS);
                            debug!("Stream frame rate set to {} fps", current_fps);
                            frame_timer = tokio::time::interval(
                                Duration::from_millis(1000 / current_fps as u64),
                            );
                            frame_timer
                                .set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                        }

                        if handle.has_clients() {
                            drop(ws_handle); // Release WS lock before acquiring RDP lock
                            let session = self.rdp_session.lock().await;
                            if let Some(ref rdp) = *session {
                                let generation = rdp.frame_generation();
                                if last_broadcast_generation != Some(generation) {
                                    let (width, height, data) = rdp.get_image_data();
                                    drop(session); // Release lock before broadcasting
                                    let ws_handle = self.ws_handle.lock().await;
                                    if let Some(ref handle) = *ws_handle {
                                        handle.broadcast_frame(width, height, &data);
                                    }
                                    last_broadcast_generation = Some(generation);
                                }
                            }
                        }
                    }
                }

                // Handle clipboard changed notifications from RDP backend
                result = async {
                    let mut rx_guard = self.clipboard_changed_rx.lock().await;
                    if let Some(ref mut rx) = *rx_guard {
                        rx.recv().await
                    } else {
                        std::future::pending().await
                    }
                } => {
                    if result.is_some() {
                        // Remote clipboard changed - notify WebSocket clients
                        let ws_handle = self.ws_handle.lock().await;
                        if let Some(ref handle) = *ws_handle {
                            handle.broadcast_clipboard_changed();
                        }
                    }
                }
            }
        }

        // Graceful shutdown
        self.shutdown().await
    }

    /// Gracefully shut down the daemon.
    async fn shutdown(&mut self) -> anyhow::Result<()> {
        info!("Shutting down daemon...");

        // Disconnect RDP session if connected. A shorter join than the
        // default: the CLI that sent `Shutdown` is waiting on this with its
        // own budget (10s for a version-mismatch replacement), and a
        // processor that has not stopped in 2s is not going to.
        let mut session = self.rdp_session.lock().await;
        if let Some(rdp) = session.take() {
            if let Err(e) = rdp.disconnect_with_join(std::time::Duration::from_secs(2)).await {
                warn!("Error during RDP disconnect: {}", e);
            }
        }

        // Clean up socket and PID files
        let socket_path = crate::get_socket_path(&self.session_name);
        let pid_path = crate::get_pid_path(&self.session_name);

        let _ = std::fs::remove_file(&socket_path);
        let _ = std::fs::remove_file(&pid_path);

        info!("Daemon shutdown complete");
        Ok(())
    }
}

/// Handle a single client connection.
async fn handle_client(
    stream: crate::ipc_server::IpcStream,
    rdp_session: Arc<Mutex<Option<RdpSession>>>,
    automation_state: SharedAutomationState,
    ws_handle: SharedWsHandle,
    session_name: String,
    start_time: Instant,
    shutdown_tx: broadcast::Sender<()>,
    disconnect_tx: tokio::sync::mpsc::Sender<DisconnectEvent>,
    clipboard_changed_rx: ClipboardChangedRx,
    session_generation: Arc<std::sync::atomic::AtomicU64>,
    last_disconnect: SharedLastDisconnect,
) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (reader, mut writer) = tokio::io::split(stream);
    let mut reader = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        let n = reader.read_line(&mut line).await?;

        if n == 0 {
            // Client disconnected
            break;
        }

        let request: Request = match serde_json::from_str(line.trim()) {
            Ok(req) => req,
            Err(e) => {
                let resp = Response::error(ErrorCode::InvalidRequest, format!("Invalid request: {}", e));
                let mut json = serde_json::to_vec(&resp)?;
                json.push(b'\n');
                writer.write_all(&json).await?;
                writer.flush().await?;
                continue;
            }
        };

        let is_shutdown = matches!(request, Request::Shutdown);

        let started = Instant::now();
        let mut response = process_request(
            request.clone(),
            &rdp_session,
            &automation_state,
            &ws_handle,
            &session_name,
            start_time,
            &disconnect_tx,
            &clipboard_changed_rx,
            &session_generation,
        ).await;

        // "Not connected" after a transport drop is a different situation
        // from "never connected" and from "the daemon is gone", and the
        // caller cannot tell them apart from the bare code. Annotate here,
        // once, rather than at the two dozen sites that produce the code.
        annotate_not_connected(&mut response, &last_disconnect);
        if matches!(request, Request::Connect(_)) && response.success {
            *last_disconnect.lock().unwrap() = None;
        }
        if let Some(ResponseData::SessionInfo(ref mut info)) = response.data {
            info.last_disconnect = last_disconnect.lock().unwrap().as_ref().map(DisconnectInfo::to_protocol);
        }

        // Evidence for the next bug report: what was asked, what came back,
        // and - when the failure is the kind a screenshot explains - the
        // screen at that moment. Both return immediately (the work is
        // spawned); neither touches the main accept loop.
        crate::transcript::record(&session_name, &request, &response, started.elapsed());
        crate::diagnostics::maybe_capture(&session_name, &rdp_session, &request, &response);

        // `to_vec` + push instead of `to_string + "\n"`: appending to a String
        // that just reached exactly its capacity re-allocates and copies the
        // whole multi-MB screenshot payload one extra time.
        let mut json = serde_json::to_vec(&response)?;
        json.push(b'\n');
        writer.write_all(&json).await?;
        writer.flush().await?;

        // Trigger daemon shutdown if this was a shutdown request
        if is_shutdown {
            info!("Shutdown request received, signaling daemon to exit");
            let _ = shutdown_tx.send(());
            break;
        }
    }

    Ok(())
}

/// Process a single request and return a response.
async fn process_request(
    request: Request,
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    ws_handle: &SharedWsHandle,
    session_name: &str,
    start_time: Instant,
    disconnect_tx: &tokio::sync::mpsc::Sender<DisconnectEvent>,
    clipboard_changed_rx: &ClipboardChangedRx,
    session_generation: &Arc<std::sync::atomic::AtomicU64>,
) -> Response {
    match request {
        Request::Ping => Response::success(ResponseData::Pong {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }),

        Request::SessionInfo => {
            let session = rdp_session.lock().await;
            let (state, host, width, height, last_frame_age_ms) = if let Some(ref rdp) = *session {
                (
                    ConnectionState::Connected,
                    Some(rdp.host().to_string()),
                    Some(rdp.width()),
                    Some(rdp.height()),
                    Some(rdp.last_frame_age().as_millis() as u64),
                )
            } else {
                (ConnectionState::Disconnected, None, None, None, None)
            };

            Response::success(ResponseData::SessionInfo(SessionInfo {
                name: session_name.to_string(),
                state,
                host,
                width,
                height,
                pid: std::process::id(),
                daemon_version: env!("CARGO_PKG_VERSION").to_string(),
                // Filled in by the CLI; the daemon does not know it.
                cli_version: None,
                uptime_secs: start_time.elapsed().as_secs(),
                last_frame_age_ms,
                // Filled in by `handle_client`, which owns the drop state.
                last_disconnect: None,
            }))
        }

        Request::Shutdown => {
            // Will trigger shutdown after response is sent
            Response::ok()
        }

        Request::Connect(params) => {
            handlers::connect::handle(rdp_session, automation_state, ws_handle, params, disconnect_tx.clone(), clipboard_changed_rx, session_generation).await
        }

        Request::Disconnect => {
            handlers::connect::handle_disconnect(rdp_session, automation_state, ws_handle).await
        }

        Request::Screenshot(params) => {
            handlers::screenshot::handle(rdp_session, params).await
        }

        Request::Mouse(action) => {
            handlers::mouse::handle(rdp_session, action).await
        }

        Request::Keyboard(action) => {
            handlers::keyboard::handle(rdp_session, action).await
        }

        Request::Scroll(params) => {
            handlers::scroll::handle(rdp_session, params).await
        }

        Request::Clipboard(action) => {
            handlers::clipboard::handle(rdp_session, action).await
        }

        Request::Drive(action) => {
            handlers::drive::handle(rdp_session, action).await
        }

        Request::Automate(action) => {
            handlers::automate::handle(rdp_session, automation_state, action).await
        }

        Request::Locate(params) => {
            handlers::locate::handle(rdp_session, params).await
        }

        Request::ClickAt(params) => {
            handlers::locate::handle_click_at(rdp_session, params).await
        }

        Request::AutomationRestart => {
            handlers::automate::handle_restart(rdp_session, automation_state).await
        }

        Request::FilePush(params) => {
            handlers::file_transfer::handle_push(rdp_session, automation_state, params).await
        }

        Request::FilePull(params) => {
            handlers::file_transfer::handle_pull(rdp_session, automation_state, params).await
        }

        Request::FileStat(params) => {
            handlers::file_transfer::handle_stat(rdp_session, automation_state, params).await
        }
    }
}

/// The most recent transport drop while the daemon stayed up.
#[derive(Debug, Clone)]
pub struct DisconnectInfo {
    pub at: std::time::SystemTime,
    pub reason: String,
}

impl DisconnectInfo {
    fn seconds_ago(&self) -> u64 {
        self.at.elapsed().map(|d| d.as_secs()).unwrap_or(0)
    }

    fn to_protocol(&self) -> agent_rdp_protocol::LastDisconnect {
        agent_rdp_protocol::LastDisconnect {
            at: crate::timefmt::utc_rfc3339(self.at),
            seconds_ago: self.seconds_ago(),
            reason: self.reason.clone(),
        }
    }
}

/// Shared handle to the last drop, written by the drop arm and read by
/// every client handler. A std mutex: the critical sections are a clone.
pub type SharedLastDisconnect = Arc<std::sync::Mutex<Option<DisconnectInfo>>>;

/// Extend a `not_connected` error with what happened to the session.
fn annotate_not_connected(response: &mut Response, last_disconnect: &SharedLastDisconnect) {
    let Some(error) = response.error.as_mut() else { return };
    if error.code != ErrorCode::NotConnected {
        return;
    }
    let Some(info) = last_disconnect.lock().unwrap().clone() else {
        return;
    };
    error.message = format!(
        "{}. The RDP transport dropped {}s ago ({}); the daemon itself is alive - \
         re-establish the session with `agent-rdp connect ...` (automation is relaunched \
         automatically).",
        error.message.trim_end_matches('.'),
        info.seconds_ago(),
        info.reason
    );
}

/// Whether a drop notification belongs to a session that has already been
/// replaced, and so must not be acted on.
///
/// Split out as a pure function so the rule can be tested without standing up
/// a daemon: getting it wrong in either direction is costly. Ignoring a live
/// session's drop leaves the daemon believing a dead session is usable; acting
/// on a stale one tears down the session that replaced it, which is exactly
/// the failure this generation tag exists to prevent.
fn is_stale_disconnect(dropped_generation: u64, current_generation: u64) -> bool {
    dropped_generation != current_generation
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_generation_drop_is_acted_on() {
        assert!(!is_stale_disconnect(7, 7));
    }

    #[test]
    fn older_generation_drop_is_ignored() {
        // The classic race: session 3 died, the notification was still in
        // flight while `connect` established session 4.
        assert!(is_stale_disconnect(3, 4));
    }

    #[test]
    fn a_drop_before_any_connect_is_ignored() {
        // Generation starts at 0 and no session can carry it, so anything
        // arriving against it is stale.
        assert!(is_stale_disconnect(1, 0));
    }

    #[test]
    fn several_reconnects_only_the_latest_counts() {
        let current = 10;
        for stale in 1..current {
            assert!(is_stale_disconnect(stale, current), "generation {} should be stale", stale);
        }
        assert!(!is_stale_disconnect(current, current));
    }
}
