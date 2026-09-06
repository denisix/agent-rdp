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
        keep_alive: (params.keep_alive_secs > 0)
            .then(|| std::time::Duration::from_secs(params.keep_alive_secs)),
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

    // The frame processor is already running by the time `connect` returns,
    // so the session can be dead before it is ever stored - a server that
    // closes the connection right after logon does exactly that. Reporting
    // "Connected" here is what left callers with a successful connect and a
    // disconnected session.
    if let Some(reason) = rdp.drop_reason() {
        return Response::error(
            ErrorCode::ConnectionFailed,
            format!("Connected to {} but the transport dropped immediately: {}", host, reason),
        );
    }
    let drop_probe = rdp.drop_probe();
    let connected_at = std::time::Instant::now();

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
    let mut automation_deferred = false;
    if enable_automation {
        // `total_launches` counts against one target, so pointing the same
        // daemon at another machine starts the count over. Done here rather
        // than before connecting: a `connect` that fails auth or TCP should
        // not discard the count for the host still being served.
        {
            let target = format!("{}:{}", params.host, params.port);
            let mut auto_state = automation_state.lock().await;
            if auto_state.launch_target.as_deref() != Some(target.as_str()) {
                auto_state.total_launches = 0;
                auto_state.launch_target = Some(target);
            }
        }

        if automation_error.is_some() {
            // `initialize()` never produced a `dvc_ipc` - checked first, and
            // ahead of `defer_agent`, because there is nothing for
            // `adopt_only` to attach to either. Reporting this as a deferred
            // launch (as the order used to) hid the real reason and told the
            // caller `automate restart` would fix it, when nothing short of
            // reconnecting can.
            automation_ready = Some(false);
        } else if params.defer_agent {
            // The drive and the DVC channel are up, so an agent that outlived
            // an earlier drop can still reattach on its own - only the Win+R
            // is withheld. Try to adopt one anyway: that costs the remote
            // desktop nothing, which is the whole point of the flag.
            match crate::automation::adopt_only(automation_state).await {
                true => automation_ready = Some(true),
                false => {
                    automation_deferred = true;
                    let mut state = automation_state.lock().await;
                    // Not a failure, so the supervisor must not arm a retry
                    // and start typing on its own later.
                    state.last_error = Some(
                        "agent launch deferred by `connect --defer-agent`; `automate restart` \
                         starts it (this types Win+R on the remote desktop)"
                            .to_string(),
                    );
                    state.next_retry_at = None;
                    state.launch_failures = 0;
                }
            }
        } else {
            info!("Bootstrapping Windows UI Automation...");

            // RDP itself is fine even if this fails, so don't fail the
            // connect - but do report it, otherwise the caller sees a clean
            // "Connected" and only discovers the problem later as an
            // unexplained "agent not ready". The guarded path marks the
            // bootstrap as in flight (so the supervisor cannot double it)
            // and, on failure, schedules the supervisor's retries.
            // Adopts an agent that outlived the previous drop when there is
            // one, which is the common case on a reconnect - that path types
            // nothing on the remote desktop.
            match crate::automation::launch_guarded(rdp_session, automation_state, true).await {
                Ok(()) => automation_ready = Some(true),
                Err(reason) => {
                    automation_ready = Some(false);
                    // The whole sentence lives here, and `output.rs` prints
                    // it verbatim. It used to be half here and half in the
                    // CLI, which rendered the same advice twice.
                    automation_error = Some(format!(
                        "{}. Do not reconnect for this - the RDP session is up and the daemon \
                         keeps retrying once the desktop has been idle for {}s (`automate \
                         status` shows last_error and next_retry_secs); `automate restart` \
                         forces a retry now; details in the session's daemon.log",
                        reason,
                        crate::automation::RETRY_INPUT_QUIET.as_secs()
                    ));
                }
            }
        }
    }

    // The bootstrap above can take minutes, all of it after the session was
    // stored and none of it holding the session lock. A server that closes
    // the connection in that window leaves the daemon tearing the session
    // down while this handler is still on its way to reporting success.
    if let Some(reason) = drop_probe.drop_reason() {
        return Response::error(
            ErrorCode::ConnectionFailed,
            format!(
                "Connected to {}, but the transport dropped again {}s later while the \
                 automation agent was starting: {}. The daemon is alive; run `connect` again. \
                 If this repeats, the server is ending the session right after logon - check \
                 its session policy and event log.",
                host,
                connected_at.elapsed().as_secs(),
                reason
            ),
        );
    }

    Response::success(ResponseData::Connected {
        host,
        width,
        height,
        automation_ready,
        automation_error,
        automation_deferred,
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
