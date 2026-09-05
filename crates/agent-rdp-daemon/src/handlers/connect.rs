//! Connection handler.

use std::sync::Arc;

use agent_rdp_protocol::{ConnectRequest, ErrorCode, Response, ResponseData};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::automation::{AutomationBootstrap, SharedAutomationState};
use crate::daemon::{ClipboardChangedRx, SharedWsHandle};
use crate::rdp_session::{RdpConfig, RdpSession};
use crate::ws_server::{WsServer, WsServerConfig};

/// Handle a connect request.
pub async fn handle(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    ws_handle: &SharedWsHandle,
    params: ConnectRequest,
    disconnect_tx: tokio::sync::mpsc::Sender<crate::rdp_session::DisconnectEvent>,
    clipboard_changed_rx: &ClipboardChangedRx,
    session_generation: &Arc<std::sync::atomic::AtomicU64>,
) -> Response {
    let enable_automation = params.enable_win_automation;
    let stream_port = params.stream_port;
    let stream_bind = params.stream_bind.clone();
    let stream_fps = params.stream_fps;
    let stream_quality = params.stream_quality;
    let serve_viewer = params.serve_viewer;

    // Claim a fresh generation for the session about to be created, before
    // touching any shared state. Anything the *previous* session's frame
    // processor reports from here on is stale by definition, and the daemon
    // discards it rather than tearing down what this connect is building.
    // This has to happen first: the drop-teardown task in `daemon.rs`
    // re-checks the generation under each lock it takes, and only an early
    // bump guarantees it cannot wipe the automation state initialized below.
    let generation = session_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;

    // Auto-disconnect if already connected (handles stale/dropped connections)
    {
        let mut session = rdp_session.lock().await;
        if let Some(old_session) = session.take() {
            info!("Disconnecting existing session before new connection");
            if let Err(e) = old_session.disconnect().await {
                // Log but don't fail - the old connection might already be dead
                info!("Previous disconnect returned error (may be expected): {}", e);
            }
        }
    }

    // Clean up any previous automation state
    {
        let mut auto_state = automation_state.lock().await;
        if auto_state.enabled {
            let session_dir = crate::get_session_dir("");
            let bootstrap = AutomationBootstrap::new(session_dir);
            let _ = bootstrap.cleanup(&mut auto_state).await;
        }
    }

    // Build drive list, adding automation drive if enabled
    // IMPORTANT: Create the automation directory BEFORE registering the drive,
    // otherwise Windows will get "invalid address" errors trying to access it
    let mut drives = params.drives.clone();
    // Tracks whether `initialize()` actually succeeded, independent of
    // `enable_automation` - the launch loop further down must not run
    // against a drive/DVC state that was never created. Without this gate it
    // used to burn 3 blind Win+R/paste/Enter attempts on the remote desktop
    // referencing a `\\TSCLIENT\<drive>` path that was never mapped, then
    // fail with the same generic "automation not enabled" message a session
    // that never requested automation at all would get.
    let mut automation_init_error: Option<String> = None;
    if enable_automation {
        let session_dir = crate::get_session_dir("");
        let bootstrap = AutomationBootstrap::new(session_dir);

        // Initialize automation directory structure first. Retry once on
        // failure - a `cleanup()` from the previous session's `remove_dir_all`
        // (bootstrap.rs) not having fully settled is exactly the kind of
        // transient local-fs condition a short retry absorbs, instead of
        // forcing the caller through an entire extra reconnect to recover.
        let mut init_result = {
            let mut auto_state = automation_state.lock().await;
            bootstrap.initialize(&mut auto_state).await
        };
        if let Err(ref e) = init_result {
            warn!("Failed to initialize automation directory (attempt 1/2): {}", e);
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            init_result = {
                let mut auto_state = automation_state.lock().await;
                bootstrap.initialize(&mut auto_state).await
            };
        }

        match init_result {
            Ok(()) => {
                // Only add the drive if the directory was actually created.
                let auto_state = automation_state.lock().await;
                drives.push(bootstrap.get_drive_mapping(&auto_state));
            }
            Err(e) => {
                warn!("Failed to initialize automation directory (attempt 2/2): {}", e);
                automation_init_error = Some(format!("Failed to initialize automation: {}", e));
            }
        }
    }

    // Log the drives being configured
    for (idx, drive) in drives.iter().enumerate() {
        info!(
            "Drive {}: name={}, path={}",
            idx + 1,
            drive.name,
            drive.path
        );
    }

    // Get DVC state for automation (if enabled)
    let automation_dvc_state = if enable_automation {
        let auto_state = automation_state.lock().await;
        auto_state.dvc_state.clone()
    } else {
        None
    };

    // Build configuration
    let config = RdpConfig {
        host: params.host.clone(),
        port: params.port,
        username: params.username,
        password: params.password,
        domain: params.domain,
        width: params.width,
        height: params.height,
        drives,
        automation_dvc_state,
    };

    // Attempt connection
    let rdp = match RdpSession::connect(config, Some((disconnect_tx, generation))).await {
        Ok(rdp) => rdp,
        Err(e) => {
            let code = match &e {
                crate::rdp_session::RdpError::AuthenticationFailed => ErrorCode::AuthenticationFailed,
                _ => ErrorCode::ConnectionFailed,
            };
            return Response::error(code, e.to_string());
        }
    };

    let host = rdp.host();
    let width = rdp.width();
    let height = rdp.height();

    // Store the session
    {
        let mut session = rdp_session.lock().await;
        *session = Some(rdp);
    }

    info!("Connected to {} ({}x{})", host, width, height);

    // The agent's keeper for this session: relaunches it if its DVC channel
    // closes while the RDP session is still up. Bound to this generation and
    // to this session's DVC state, so it cannot act on a later session.
    if enable_automation {
        let closed_rx = automation_state.lock().await.closed_rx.take();
        if let Some(closed_rx) = closed_rx {
            crate::automation::spawn_relaunch_supervisor(
                closed_rx,
                Arc::clone(rdp_session),
                Arc::clone(automation_state),
                Arc::clone(session_generation),
                generation,
            );
        }
    }

    // Load the OCR models now rather than on the first `locate` call. Model
    // loading is the slow part (disk I/O plus building the rten graphs), so
    // paying for it here means the first `locate` an agent issues is as fast
    // as every later one. Fire-and-forget: a failure just leaves the lazy
    // path in `handlers::locate` to retry and report it when `locate` is
    // actually used.
    tokio::spawn(async {
        crate::handlers::locate::get_or_init_ocr_service().await;
    });

    // Start WebSocket streaming server if requested
    if stream_port > 0 {
        let mut ws = ws_handle.lock().await;
        if ws.is_none() {
            let config = WsServerConfig {
                port: stream_port,
                bind: stream_bind.clone(),
                fps: stream_fps,
                jpeg_quality: stream_quality,
                serve_viewer,
            };
            let ws_server = WsServer::new(config);
            match ws_server.start(Arc::clone(rdp_session)).await {
                Ok(handle) => {
                    info!(
                        "WebSocket streaming enabled on {}:{}",
                        stream_bind, stream_port
                    );
                    *ws = Some(handle);

                    // Set up clipboard change notification channel
                    let session = rdp_session.lock().await;
                    if let Some(ref rdp) = *session {
                        let (changed_tx, changed_rx) = tokio::sync::mpsc::unbounded_channel();
                        rdp.set_clipboard_changed_notify(changed_tx);
                        *clipboard_changed_rx.lock().await = Some(changed_rx);
                        info!("Clipboard WebSocket integration enabled");
                    }
                }
                Err(e) => {
                    warn!("Failed to start WebSocket server: {}", e);
                }
            }
        } else {
            info!("WebSocket server already running");
        }
    }

    // Bootstrap automation if enabled (directory was already created before connection)
    let mut automation_ready = None;
    let mut automation_error = automation_init_error;
    if enable_automation {
        if automation_error.is_some() {
            // `initialize()` never produced a `dvc_ipc`, so there is nothing
            // for `launch_agent`/`wait_for_agent` to do but fail - launching
            // anyway would still open the remote Run dialog and paste a
            // command referencing a drive that was never mapped, three times,
            // for no benefit. Report the real reason immediately instead.
            automation_ready = Some(false);
        } else {
            info!("Bootstrapping Windows UI Automation...");

            // RDP itself is fine even if this fails, so don't fail the
            // connect - but do report it, otherwise the caller sees a clean
            // "Connected" and only discovers the problem later as an
            // unexplained "agent not ready". The guarded path marks the
            // bootstrap as in flight (so the supervisor cannot double it)
            // and, on failure, schedules the supervisor's retries.
            match crate::automation::launch_guarded(rdp_session, automation_state).await {
                Ok(()) => automation_ready = Some(true),
                Err(reason) => {
                    automation_ready = Some(false);
                    automation_error = Some(format!(
                        "{}. The daemon keeps retrying in the background once the session has \
                         been idle for {}s (`automate status` shows last_error and \
                         next_retry_secs); `automate restart` forces a retry now",
                        reason,
                        crate::automation::RETRY_INPUT_QUIET.as_secs()
                    ));
                }
            }
        }
    }

    Response::success(ResponseData::Connected {
        host,
        width,
        height,
        automation_ready,
        automation_error,
    })
}

/// Handle a disconnect request.
pub async fn handle_disconnect(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    automation_state: &SharedAutomationState,
    ws_handle: &SharedWsHandle,
) -> Response {
    // Stop WebSocket server if running
    {
        let mut ws = ws_handle.lock().await;
        if ws.is_some() {
            info!("Stopping WebSocket streaming server");
            *ws = None; // Drop the handle to stop the server
        }
    }

    // Clean up automation state
    {
        let mut auto_state = automation_state.lock().await;
        if auto_state.enabled {
            let session_dir = crate::get_session_dir("");
            let bootstrap = AutomationBootstrap::new(session_dir);
            if let Err(e) = bootstrap.cleanup(&mut auto_state).await {
                warn!("Error cleaning up automation: {}", e);
            }
        }
    }

    let mut session = rdp_session.lock().await;

    match session.take() {
        Some(rdp) => {
            if let Err(e) = rdp.disconnect().await {
                return Response::error(ErrorCode::InternalError, format!("Disconnect error: {}", e));
            }
            Response::ok()
        }
        None => {
            Response::error(ErrorCode::NotConnected, "Not connected to an RDP server")
        }
    }
}
