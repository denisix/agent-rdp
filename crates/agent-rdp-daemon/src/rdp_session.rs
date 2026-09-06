//! RDP session wrapper using IronRDP.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use parking_lot::RwLock;
use thiserror::Error;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use agent_rdp_protocol::DriveMapping;
use ironrdp::connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationState,
};
use ironrdp::connector::{self, ClientConnector, ConnectorResult, Credentials, ServerName, Sequence as _};
use ironrdp::pdu::gcc::KeyboardType;
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp::pdu::rdp::client_info::PerformanceFlags;
use ironrdp::pdu::rdp::headers::ShareDataPdu;
use ironrdp::pdu::rdp::refresh_rectangle::RefreshRectanglePdu;
use ironrdp::session::image::DecodedImage;
use ironrdp::session::{ActiveStage, ActiveStageOutput};
use ironrdp_dvc::DrdynvcClient;
use ironrdp_rdpdr::Rdpdr;

use crate::automation::{AutomationDvcListener, SharedDvcState};
use crate::rdpdr::MultiDriveBackend;
use ironrdp_rdpsnd::client::{NoopRdpsndBackend, Rdpsnd};
use ironrdp_tokio::{FramedWrite, TokioFramed};
use tokio::net::TcpStream;

pub mod clipboard;

#[derive(Error, Debug)]
pub enum RdpError {
    #[error("Connection failed: {0}")]
    ConnectionFailed(String),

    #[error("Authentication failed")]
    AuthenticationFailed,

    #[error("TLS error: {0}")]
    TlsError(String),

    #[error("Protocol error: {0}")]
    ProtocolError(String),

    #[error("Not connected")]
    NotConnected,

    #[error("Session closed")]
    SessionClosed,

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Invalid input: {0}")]
    InvalidInput(String),

    /// The frame processor did not answer in time.
    ///
    /// Distinct from `SessionClosed`: the session still exists, but the task
    /// that services it is wedged - typically the RDP transport stalled, or
    /// remote-initiated drive I/O is blocking the processor. Without this the
    /// caller waited forever and the command only ended when the CLI's
    /// watchdog killed it, leaving the daemon-side task leaked and the user
    /// with no idea why.
    #[error("The RDP session is not responding ({0}). The transport may be wedged - reconnect with `agent-rdp connect ...`")]
    Unresponsive(String),
}

/// Configuration for an RDP connection.
pub struct RdpConfig {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub password: String,
    pub domain: Option<String>,
    pub width: u16,
    pub height: u16,
    /// Drives to map at connect time.
    pub drives: Vec<DriveMapping>,
    /// Shared DVC state for automation (enables DVC channel if provided).
    pub automation_dvc_state: Option<SharedDvcState>,
    /// Seconds between keep-alive PDUs; `None` disables them.
    pub keep_alive: Option<std::time::Duration>,
}

use crate::automation::DvcCommandReceiver;

/// Commands sent to the background frame processor.
enum SessionCommand {
    SendInput(Vec<FastPathInputEvent>),
    /// Set clipboard text and announce to remote.
    ClipboardSet {
        text: String,
        response_tx: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    /// Get clipboard text from remote.
    ClipboardGet {
        response_tx: tokio::sync::oneshot::Sender<Result<Option<String>, String>>,
    },
    Shutdown,
}

/// Shared session state accessible from the main thread.
struct SharedState {
    image: DecodedImage,
    host: String,
    width: u16,
    height: u16,
    /// Bumped whenever the server paints into `image`. Lets the streaming
    /// tick skip re-encoding a framebuffer that has not changed - without
    /// this, a completely static desktop is copied and JPEG-encoded at the
    /// full frame rate forever.
    frame_generation: u64,
    /// When the last PDU was successfully read from the server. Distinct
    /// from `frame_generation`, which only counts *content* changes - an
    /// idle-but-alive desktop and a dead connection look identical by that
    /// counter alone. This is the only signal a handler can consult
    /// synchronously to tell "genuinely alive, just quiet" apart from "the
    /// transport died and nothing has noticed yet" (detection otherwise only
    /// happens reactively, when `read_pdu()` itself errors).
    last_frame_at: std::time::Instant,
    /// Why the frame processor stopped, once it has. Set at every exit point
    /// before the drop is notified, so a caller holding the session can tell
    /// a live one from a corpse without waiting for the daemon's teardown to
    /// run - `connect` uses it to avoid reporting success for a session that
    /// died while the automation agent was starting.
    drop_reason: Option<String>,
    /// Drives that were mapped at connect time.
    drives: Vec<DriveMapping>,
    /// Clipboard state for CLIPRDR.
    clipboard: Arc<parking_lot::Mutex<clipboard::ClipboardState>>,
}

/// An active RDP session with background frame processing.
pub struct RdpSession {
    /// Shared state (image, connection info)
    shared: Arc<RwLock<SharedState>>,
    /// Channel to send commands to the background task
    command_tx: mpsc::Sender<SessionCommand>,
    /// Handle to the background frame-processor task. Joined (with a bound)
    /// by `disconnect`, aborted on drop - never just detached, see
    /// `disconnect_with_join`.
    task_handle: tokio::task::JoinHandle<()>,
    /// Shared with the RDPDR backend: when the remote last read a script.
    script_activity: Arc<std::sync::atomic::AtomicU64>,
    /// When this daemon last drove the session itself (input events or a
    /// clipboard write), as Unix milliseconds; 0 if never. The relaunch
    /// supervisor waits for this to go quiet before typing into the
    /// session on its own.
    input_activity: Arc<std::sync::atomic::AtomicU64>,
    /// The keep-alive interval this session was built with, for `session
    /// info` - what a reported frame age has to be read against.
    keep_alive: Option<std::time::Duration>,
}

/// A handle that outlives the session it watches.
///
/// `connect` has to re-check for a drop after work that gives up the session
/// lock, by which point the daemon's teardown may already have replaced the
/// slot with `None` - and "the session is gone" is exactly the case where the
/// reason matters most.
#[derive(Clone)]
pub struct DropProbe {
    shared: Arc<RwLock<SharedState>>,
}

impl DropProbe {
    /// Why the watched session's frame processor stopped, if it has.
    pub fn drop_reason(&self) -> Option<String> {
        self.shared.read().drop_reason.clone()
    }
}

/// Unix milliseconds now, for the activity stamps.
fn unix_millis_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// What the daemon learns when a session's frame processor stops without
/// being asked to.
#[derive(Debug, Clone)]
pub struct DisconnectEvent {
    /// Generation of the session that dropped (see `DisconnectNotify`).
    pub generation: u64,
    /// What the processor saw: a read failure, a server-initiated
    /// termination, a failed reactivation.
    pub reason: String,
}

/// How long `disconnect` waits for the frame processor to exit on its own
/// before aborting it.
pub const DISCONNECT_JOIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// How long data we sent may go unacknowledged before the connection is
/// declared dead. See `RdpSession::apply_tcp_unacked_timeout`.
pub const UNACKED_DATA_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Consecutive keep-alive refreshes the server may leave unanswered before
/// the transport is declared dead. Three, not one: a single missed repaint
/// could be a busy server or a frame lost to a momentary stall, and a false
/// positive here costs a reconnect.
pub const KEEP_ALIVE_MISSED_LIMIT: u32 = 3;

/// Counts keep-alive refreshes the server has left unanswered, in a row.
///
/// Strikes, not elapsed time, on purpose. The frame processor services RDPDR
/// I/O synchronously, so a wedged file operation can stall this loop for
/// minutes with the server perfectly healthy and its replies queued in the
/// socket buffer. Measured as "time since the last inbound PDU", that stall
/// looked like death the instant it ended - the overdue timer and the
/// buffered read become ready together, `select!` picks one at random, and
/// the timer arm saw a stale timestamp. Counted per send, a stall delivers at
/// most one tick (`MissedTickBehavior::Delay`) and so at most one strike;
/// the next send finds the drained replies and clears the count. Only a
/// server that stays silent across `KEEP_ALIVE_MISSED_LIMIT` consecutive
/// sends, each a full period apart, is declared dead.
#[derive(Debug, Default)]
pub struct KeepAliveWatch {
    unanswered: u32,
}

impl KeepAliveWatch {
    /// Record that a refresh was just sent. `answered_since_last_send` is
    /// whether any PDU arrived after the *previous* send. Returns `true`
    /// when the server has now been silent for `KEEP_ALIVE_MISSED_LIMIT`
    /// sends in a row.
    pub fn record_send(&mut self, answered_since_last_send: bool) -> bool {
        if answered_since_last_send {
            self.unanswered = 0;
        } else {
            self.unanswered = self.unanswered.saturating_add(1);
        }
        self.unanswered >= KEEP_ALIVE_MISSED_LIMIT
    }

    /// Consecutive unanswered sends so far.
    pub fn unanswered(&self) -> u32 {
        self.unanswered
    }
}

/// Environment variable that keeps the keep-alive traffic but disables the
/// "silent server is dead" verdict. The rule assumes a live server answers
/// a Refresh Rect on an idle desktop with some PDU; if a server turns out not
/// to, this keeps the NAT-idle protection without the false drops, which
/// `--keep-alive-secs 0` would throw away together.
pub const SILENCE_DROP_KILL_SWITCH: &str = "AGENT_RDP_NO_SILENCE_DROP";

/// Whether the silence verdict is disabled by the environment.
pub fn silence_drop_disabled() -> bool {
    std::env::var_os(SILENCE_DROP_KILL_SWITCH)
        .map(|v| !v.is_empty() && v != "0")
        .unwrap_or(false)
}

impl Drop for RdpSession {
    fn drop(&mut self) {
        // No-op for a task that already finished (the normal case after
        // `disconnect`). For a session dropped any other way - a replaced
        // `Option<RdpSession>`, an early return - this is what stops the old
        // processor from living on with its framebuffer, TLS socket and
        // channels. Tasks are `Send`, so an abort never lands while a
        // `parking_lot` guard is held.
        self.task_handle.abort();
    }
}

/// How long to wait for the frame processor to accept a queued command.
///
/// Only bounded by channel capacity, so this expires solely when the
/// processor is not draining its queue at all.
const COMMAND_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long to wait for the remote to acknowledge a clipboard write.
const CLIPBOARD_SET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// How long to wait for the remote to hand back clipboard contents.
///
/// Longer than the write path: this is a full round trip to the remote
/// clipboard owner, which may be a busy application.
const CLIPBOARD_GET_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Callback for connection drop notification, tagged with the generation of
/// the session it belongs to.
///
/// The generation is what makes the notification safe to act on. Previously
/// this was a bare `mpsc::Sender<()>`: a notification from a session that had
/// already been replaced was indistinguishable from one for the current
/// session, so a drop notice that arrived just after a reconnect stored its
/// new session would tear that new session down instead - reported as
/// "daemon_not_running" on the very next command, or as the automation agent
/// failing to launch with "Not connected" moments after `connect` succeeded.
pub type DisconnectNotify = (mpsc::Sender<DisconnectEvent>, u64);

impl RdpSession {
    /// Enable OS-level TCP keepalive on the RDP socket so a black-holed
    /// connection (no RST/FIN from the peer) is detected in seconds instead
    /// of the OS's default multi-minute retransmission timeout.
    fn apply_tcp_keepalive(stream: &TcpStream) -> std::io::Result<()> {
        let sock_ref = socket2::SockRef::from(stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(std::time::Duration::from_secs(10))
            .with_interval(std::time::Duration::from_secs(5));
        #[cfg(not(any(target_os = "windows", target_os = "openbsd")))]
        let keepalive = keepalive.with_retries(4);
        sock_ref.set_tcp_keepalive(&keepalive)
    }

    /// Bound how long unacknowledged data may sit before the OS gives up.
    ///
    /// Keepalive alone stopped being enough when the keep-alive PDU arrived:
    /// keepalive probes are only sent on an *idle* connection, and a session
    /// that writes a Refresh Rect every 45s is never idle. A black-holed path
    /// then falls back to the OS retransmission timeout - minutes (the
    /// reported `os error 60` after 4-5 of them) during which every
    /// screenshot keeps succeeding against a stale framebuffer.
    ///
    /// This is the option built for that case. It fires only when data *we*
    /// sent goes unacknowledged for the whole window, so a server that is
    /// merely quiet is unaffected; combined with the keep-alive the worst
    /// case is one interval plus this timeout.
    fn apply_tcp_unacked_timeout(
        stream: &TcpStream,
        timeout: std::time::Duration,
    ) -> std::io::Result<()> {
        #[cfg(any(target_os = "linux", target_os = "android"))]
        {
            socket2::SockRef::from(stream).set_tcp_user_timeout(Some(timeout))
        }

        #[cfg(target_os = "macos")]
        {
            use std::os::fd::AsRawFd;
            // Not in `libc`: seconds to keep retransmitting before dropping
            // the connection (`TCP_RXT_CONNDROPTIME`, netinet/tcp.h).
            const TCP_RXT_CONNDROPTIME: libc::c_int = 0x80;
            let secs = timeout.as_secs() as libc::c_int;
            // SAFETY: a live fd, a fixed option, and a correctly sized
            // `c_int` payload.
            let rc = unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::IPPROTO_TCP,
                    TCP_RXT_CONNDROPTIME,
                    std::ptr::addr_of!(secs).cast(),
                    std::mem::size_of::<libc::c_int>() as libc::socklen_t,
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        #[cfg(target_os = "windows")]
        {
            use std::os::windows::io::AsRawSocket;
            use windows_sys::Win32::Networking::WinSock::{setsockopt, IPPROTO_TCP, TCP_MAXRT};
            let secs = timeout.as_secs() as u32;
            let bytes = secs.to_ne_bytes();
            // SAFETY: a live socket, a fixed option, and a correctly sized
            // DWORD payload.
            let rc = unsafe {
                setsockopt(
                    stream.as_raw_socket() as _,
                    IPPROTO_TCP,
                    TCP_MAXRT as i32,
                    bytes.as_ptr(),
                    bytes.len() as i32,
                )
            };
            if rc != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        }

        #[cfg(not(any(
            target_os = "linux",
            target_os = "android",
            target_os = "macos",
            target_os = "windows"
        )))]
        {
            let _ = (stream, timeout);
            Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "no unacknowledged-data timeout on this platform",
            ))
        }
    }

    /// Establish a new RDP connection.
    ///
    /// If `disconnect_notify` is provided, it will be signaled when the connection drops.
    pub async fn connect(
        config: RdpConfig,
        disconnect_notify: Option<DisconnectNotify>,
    ) -> Result<Self, RdpError> {
        info!("Connecting to {}:{}", config.host, config.port);

        // Shared with the RDPDR backend (script reads) and kept on the
        // session for the bootstrap's "is the agent still starting?" check.
        let script_activity = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Build connector config
        let connector_config = connector::Config {
            credentials: Credentials::UsernamePassword {
                username: config.username.clone(),
                password: config.password.clone(),
            },
            domain: config.domain.clone(),
            enable_tls: true,
            enable_credssp: true,
            keyboard_type: KeyboardType::IbmEnhanced,
            keyboard_subtype: 0,
            keyboard_functional_keys_count: 12,
            keyboard_layout: 0x409, // US English
            ime_file_name: String::new(),
            dig_product_id: String::new(),
            desktop_size: connector::DesktopSize {
                width: config.width,
                height: config.height,
            },
            bitmap: None,
            client_build: 0,
            client_name: "agent-rdp".to_string(),
            client_dir: String::new(),
            #[cfg(windows)]
            platform: MajorPlatformType::WINDOWS,
            #[cfg(target_os = "macos")]
            platform: MajorPlatformType::MACINTOSH,
            #[cfg(all(not(windows), not(target_os = "macos")))]
            platform: MajorPlatformType::UNIX,
            pointer_software_rendering: true,
            performance_flags: PerformanceFlags::default(),
            enable_server_pointer: false,
            request_data: None,
            autologon: true,
            enable_audio_playback: false,
            desktop_scale_factor: 0,
            hardware_id: None,
            license_cache: None,
            timezone_info: Default::default(),
            // Added in ironrdp-connector 0.10. Defaults preserve the previous
            // behaviour: no alternate shell, no bulk compression, and no
            // multitransport (we don't implement the UDP transports).
            alternate_shell: String::new(),
            work_dir: String::new(),
            compression_type: None,
            multitransport_flags: None,
        };

        // Establish TCP connection
        let addr = format!("{}:{}", config.host, config.port);
        let tcp_stream = TcpStream::connect(&addr).await?;
        let client_addr: SocketAddr = tcp_stream.local_addr()?;
        debug!("TCP connection established from {:?}", client_addr);

        // Without this, a peer that goes dark without sending TCP RST/FIN
        // (cable pull, firewalled path, paused VM) is invisible to us until
        // the OS's own retransmission timeout gives up - commonly 15-30
        // minutes. During that whole window `read_pdu()` just blocks and
        // every `screenshot` keeps "succeeding" with a stale cached frame.
        // A short keepalive turns that into a detected disconnect in ~20-30s.
        if let Err(e) = Self::apply_tcp_keepalive(&tcp_stream) {
            warn!("Failed to configure TCP keepalive (continuing without it): {}", e);
        }
        if let Err(e) = Self::apply_tcp_unacked_timeout(&tcp_stream, UNACKED_DATA_TIMEOUT) {
            warn!(
                "Failed to bound the unacknowledged-data timeout (a dead transport may take \
                 the OS's own retransmission timeout to notice): {}",
                e
            );
        }

        // Create framed transport for initial connection
        let mut framed: TokioFramed<TcpStream> = TokioFramed::new(tcp_stream);

        // Create connector
        let mut connector = ClientConnector::new(connector_config, client_addr);

        // Create clipboard state (shared between backend and session)
        let clipboard_state = Arc::new(parking_lot::Mutex::new(clipboard::ClipboardState::default()));

        // RDPSND (audio) channel - required for RDPDR on Windows 2012+ and good to have
        let rdpsnd = Rdpsnd::new(Box::new(NoopRdpsndBackend));
        connector.attach_static_channel(rdpsnd);

        // Set up CLIPRDR (clipboard) with our custom backend
        let (cliprdr, clipboard_backend_rx) = clipboard::create_cliprdr(Arc::clone(&clipboard_state));
        connector.attach_static_channel(cliprdr);
        info!("Clipboard redirection enabled");

        // Set up RDPDR (drive redirection) if drives are configured
        if !config.drives.is_empty() {
            // Create multi-drive backend with all drive paths
            let mut backend = MultiDriveBackend::new();
            backend.script_activity = Some(Arc::clone(&script_activity));

            // Configure drives - device IDs start at 1
            let drive_list: Vec<(u32, String)> = config
                .drives
                .iter()
                .enumerate()
                .map(|(idx, d)| {
                    let device_id = (idx + 1) as u32;
                    // Register path for this device ID
                    backend.add_drive(device_id, std::path::PathBuf::from(&d.path));
                    (device_id, d.name.clone())
                })
                .collect();

            let rdpdr = Rdpdr::new(Box::new(backend), "agent-rdp".to_string());
            let rdpdr = rdpdr.with_drives(Some(drive_list.clone()));
            connector.attach_static_channel(rdpdr);

            for (device_id, name) in &drive_list {
                let path = &config.drives[(*device_id - 1) as usize].path;
                info!(
                    "Drive redirection enabled: {} -> \\\\TSCLIENT\\{} (device_id={})",
                    path, name, device_id
                );
            }
        }

        // Set up DRDYNVC (dynamic virtual channels) for automation if enabled
        let dvc_command_rx: Option<DvcCommandReceiver> = if let Some(dvc_state) = config.automation_dvc_state {
            // Create command channel for sending DVC data
            let (command_tx, command_rx) = tokio::sync::mpsc::unbounded_channel();

            // Store the sender in the shared state
            {
                let mut state = dvc_state.lock();
                state.command_tx = Some(command_tx);
            }

            // A listener, not a pre-registered channel: the agent may open
            // the channel more than once in a single RDP session (it is
            // relaunched after a crash, and it survives a transport drop to
            // be adopted by the next connect). `with_dynamic_channel` hands
            // the processor out once and refuses every later open.
            let drdynvc =
                DrdynvcClient::new().with_listener(AutomationDvcListener::new(dvc_state));
            connector.attach_static_channel(drdynvc);
            info!("Dynamic Virtual Channel enabled for automation");
            Some(command_rx)
        } else {
            None
        };

        // Begin connection (pre-TLS)
        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .map_err(|e| RdpError::ConnectionFailed(explain_connect_error(&e.to_string())))?;

        // Perform TLS upgrade
        let initial_stream: TcpStream = framed.into_inner_no_leftover();
        let (tls_stream, server_cert) = Self::tls_upgrade(initial_stream, &config.host)
            .await
            .map_err(|e| RdpError::TlsError(e.to_string()))?;
        debug!("TLS connection established");

        // Mark upgrade as done
        let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);

        // Create framed transport for upgraded connection
        let mut upgraded_framed: TokioFramed<tokio_rustls::client::TlsStream<TcpStream>> =
            TokioFramed::new(tls_stream);

        // Extract server public key from certificate
        let server_public_key = Self::extract_public_key(&server_cert)?;

        // Create network client for CredSSP
        let mut network_client = NoopNetworkClient;

        // Convert host to ServerName
        let server_name: ServerName = config.host.clone().into();

        // Finalize connection (post-TLS)
        let connection_result = ironrdp_tokio::connect_finalize(
            upgraded,
            connector,
            &mut upgraded_framed,
            &mut network_client,
            server_name,
            server_public_key,
            None, // No Kerberos
        )
        .await
        .map_err(|e| RdpError::ConnectionFailed(e.to_string()))?;

        info!("RDP connection established to {}", config.host);

        // The size the server granted, which is not necessarily the one we
        // requested. Bound before the builder below partially moves out of
        // `connection_result`.
        let (desktop_width, desktop_height) = (
            connection_result.desktop_size.width,
            connection_result.desktop_size.height,
        );

        if (desktop_width, desktop_height) != (config.width, config.height) {
            info!(
                "Server granted {}x{} desktop (requested {}x{})",
                desktop_width, desktop_height, config.width, config.height
            );
        }

        // Create decoded image for storing desktop state
        let image = DecodedImage::new(
            ironrdp_graphics::image_processing::PixelFormat::RgbA32,
            desktop_width,
            desktop_height,
        );

        // Kept for the Deactivation-Reactivation Sequence, which needs to build
        // a fresh activation sequence long after the connector is gone.
        let activation_factory = connection_result.activation_factory.clone();

        // Create active stage for ongoing communication.
        // ironrdp-session 0.11 replaced ActiveStage::new(ConnectionResult) with
        // a builder; the fields still come straight off the connection result.
        let active_stage = ironrdp::session::ActiveStageBuilder {
            static_channels: connection_result.static_channels,
            user_channel_id: connection_result.user_channel_id,
            io_channel_id: connection_result.io_channel_id,
            message_channel_id: connection_result.message_channel_id,
            share_id: connection_result.share_id,
            compression_type: connection_result.compression_type,
            enable_server_pointer: connection_result.enable_server_pointer,
            pointer_software_rendering: connection_result.pointer_software_rendering,
        }
        .build();

        // Create shared state.
        //
        // The size recorded here is the one the *server* granted, not the one
        // we asked for - servers routinely snap the resolution (rounding, or
        // forcing the console session's size). Screenshots and OCR already work
        // off `image`, so taking the requested size here would make
        // `width()`/`height()` disagree with the actual framebuffer and put the
        // reported desktop size, the viewer viewport and scroll's default
        // centre point in the wrong place.
        let shared = Arc::new(RwLock::new(SharedState {
            image,
            host: config.host.clone(),
            width: desktop_width,
            height: desktop_height,
            frame_generation: 0,
            last_frame_at: std::time::Instant::now(),
            drop_reason: None,
            drives: config.drives.clone(),
            clipboard: clipboard_state,
        }));

        // Create command channel
        let (command_tx, command_rx) = mpsc::channel(32);

        // Spawn background frame processor
        let shared_clone = Arc::clone(&shared);
        let keep_alive = config.keep_alive;
        let task_handle = tokio::spawn(async move {
            run_frame_processor(
                upgraded_framed,
                active_stage,
                shared_clone,
                command_rx,
                disconnect_notify,
                clipboard_backend_rx,
                dvc_command_rx,
                activation_factory,
                keep_alive,
            )
            .await;
        });

        Ok(Self {
            shared,
            command_tx,
            task_handle,
            script_activity,
            input_activity: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            keep_alive,
        })
    }

    /// Why this session's frame processor stopped, if it has. `None` while
    /// the session is live.
    pub fn drop_reason(&self) -> Option<String> {
        self.shared.read().drop_reason.clone()
    }

    /// A handle that keeps answering `drop_reason` after this session has
    /// been taken out of the daemon's slot.
    pub fn drop_probe(&self) -> DropProbe {
        DropProbe {
            shared: Arc::clone(&self.shared),
        }
    }

    /// The keep-alive interval this session was built with, if enabled.
    pub fn keep_alive(&self) -> Option<std::time::Duration> {
        self.keep_alive
    }

    /// How long ago the remote last opened a `.ps1` on a mapped drive, or
    /// `None` if it never did. See `MultiDriveBackend::script_activity`.
    pub fn last_script_open_age(&self) -> Option<std::time::Duration> {
        Self::age_of(self.script_activity.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// How long ago this daemon last sent input or set the clipboard, or
    /// `None` if it never did in this session.
    pub fn last_input_age(&self) -> Option<std::time::Duration> {
        Self::age_of(self.input_activity.load(std::sync::atomic::Ordering::Relaxed))
    }

    fn stamp_input_activity(&self) {
        self.input_activity
            .store(unix_millis_now(), std::sync::atomic::Ordering::Relaxed);
    }

    /// The current input-activity mark, to hand back to
    /// `restore_input_activity` after input that is *not* an operator's -
    /// the automation bootstrap types Win+R and pastes, and must not count
    /// as "someone is driving the session" against its own retry gate.
    pub fn input_activity_mark(&self) -> u64 {
        self.input_activity.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Undo the stamps made since `mark` was taken.
    pub fn restore_input_activity(&self, mark: u64) {
        self.input_activity.store(mark, std::sync::atomic::Ordering::Relaxed);
    }

    fn age_of(at_ms: u64) -> Option<std::time::Duration> {
        if at_ms == 0 {
            return None;
        }
        Some(std::time::Duration::from_millis(unix_millis_now().saturating_sub(at_ms)))
    }

    /// Perform TLS upgrade on the stream.
    async fn tls_upgrade(
        stream: TcpStream,
        server_name: &str,
    ) -> Result<(tokio_rustls::client::TlsStream<TcpStream>, Vec<u8>), std::io::Error> {
        use tokio_rustls::TlsConnector;

        let tls_config = Self::create_tls_config();
        let connector = TlsConnector::from(Arc::new(tls_config));

        // Try to parse as IP address first, then as DNS name
        let server_name = if let Ok(ip) = server_name.parse::<std::net::IpAddr>() {
            rustls::pki_types::ServerName::IpAddress(ip.into())
        } else {
            rustls::pki_types::ServerName::try_from(server_name.to_string())
                .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?
        };

        let tls_stream = connector.connect(server_name, stream).await.map_err(|e| {
            // rustls deliberately never implements TLS below 1.2, so a peer that
            // resets the connection during the handshake is commonly a legacy
            // server (e.g. Windows Server 2008 R2 / TLS 1.0-only) rejecting our
            // ClientHello, not a transient network glitch. Surface that distinction
            // instead of the generic "Connection reset by peer" message.
            if matches!(
                e.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::ConnectionAborted
            ) {
                std::io::Error::new(
                    e.kind(),
                    format!(
                        "TLS handshake was reset by the server ({e}). This usually means the \
                         target only supports TLS 1.0/1.1 or legacy cipher suites, which agent-rdp \
                         (via rustls) does not support. Legacy Windows targets (e.g. Server 2008 R2) \
                         are not currently supported; see agent-rdp issue #73 for status."
                    ),
                )
            } else {
                e
            }
        })?;

        // Get peer certificate
        let (_, server_conn) = tls_stream.get_ref();
        let certs = server_conn
            .peer_certificates()
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "No peer certificate"))?;

        let cert_der = certs
            .first()
            .ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::Other, "Empty certificate chain")
            })?
            .to_vec();

        Ok((tls_stream, cert_der))
    }

    /// Create TLS configuration that accepts self-signed certificates.
    fn create_tls_config() -> rustls::ClientConfig {
        // Install ring as the default crypto provider
        let _ = rustls::crypto::ring::default_provider().install_default();

        let mut root_store = rustls::RootCertStore::empty();
        root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

        // RDP servers often use self-signed certificates
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(NoVerifier))
            .with_no_client_auth()
    }

    /// Extract public key from DER-encoded certificate.
    fn extract_public_key(cert_der: &[u8]) -> Result<Vec<u8>, RdpError> {
        use x509_cert::der::Decode;

        let cert = x509_cert::Certificate::from_der(cert_der)
            .map_err(|e| RdpError::TlsError(format!("Failed to parse certificate: {}", e)))?;

        Ok(cert
            .tbs_certificate
            .subject_public_key_info
            .subject_public_key
            .as_bytes()
            .ok_or_else(|| RdpError::TlsError("No public key in certificate".into()))?
            .to_vec())
    }

    /// Get the connected host.
    pub fn host(&self) -> String {
        self.shared.read().host.clone()
    }

    /// Get the desktop width.
    pub fn width(&self) -> u16 {
        self.shared.read().width
    }

    /// Get the desktop height.
    pub fn height(&self) -> u16 {
        self.shared.read().height
    }

    /// Get the drives that were mapped at connect time.
    pub fn get_drives(&self) -> Vec<DriveMapping> {
        self.shared.read().drives.clone()
    }

    /// Get a copy of the current desktop image data.
    pub fn get_image_data(&self) -> (u16, u16, Vec<u8>) {
        let state = self.shared.read();
        let width = state.image.width();
        let height = state.image.height();
        let data = state.image.data().to_vec();
        (width, height, data)
    }

    /// Copy only a sub-rectangle of the desktop.
    ///
    /// A `--region` request for one table row needs tens of KB; copying the
    /// whole multi-MB framebuffer under the read lock just to crop it again
    /// blocks the frame decoder for no reason. The region must already be
    /// clamped to the framebuffer (`Region::clamp_to`); an out-of-bounds
    /// region returns `None` rather than panicking on a bad row slice.
    pub fn get_region_data(&self, region: agent_rdp_protocol::Region) -> Option<Vec<u8>> {
        let state = self.shared.read();
        let full_width = state.image.width() as u32;
        let full_height = state.image.height() as u32;
        copy_rgba_region(state.image.data(), full_width, full_height, region)
    }

    /// Generation counter of the framebuffer contents; see
    /// `SharedState::frame_generation`.
    pub fn frame_generation(&self) -> u64 {
        self.shared.read().frame_generation
    }

    /// How long it has been since the last PDU was successfully read from
    /// the server. A large value does not by itself mean the connection is
    /// dead (RDP servers send nothing when the desktop is idle) - but
    /// combined with the TCP keepalive now enabled on connect, a genuinely
    /// dead transport is detected and disconnects within ~20-30s, so this
    /// value should never grow unbounded on a still-alive session.
    pub fn last_frame_age(&self) -> std::time::Duration {
        self.shared.read().last_frame_at.elapsed()
    }

    /// Send input events to the remote desktop.
    pub async fn send_input(&self, events: Vec<FastPathInputEvent>) -> Result<(), RdpError> {
        debug!("Sending {} input events to frame processor", events.len());
        self.stamp_input_activity();
        self.send_command(SessionCommand::SendInput(events), "input").await
    }

    /// Send a key combination (e.g., "super+r", "ctrl+c").
    pub async fn send_key_press(&self, keys: &str) -> Result<(), RdpError> {
        use std::time::Duration;

        let key_infos = parse_key_combination(keys)
            .map_err(|e| RdpError::InvalidInput(e))?;

        // Press all keys down
        for info in &key_infos {
            let event = create_key_event(info.scancode, info.extended, false);
            self.send_input(vec![event]).await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Small delay before releasing
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release all keys in reverse order
        for info in key_infos.iter().rev() {
            let event = create_key_event(info.scancode, info.extended, true);
            self.send_input(vec![event]).await?;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        Ok(())
    }

    /// Send text input as Unicode characters.
    /// Type Unicode text.
    ///
    /// Characters are batched into as few input PDUs as possible rather than
    /// one round-trip (plus a sleep) per character, which is what made typing
    /// a short string take tens of seconds.
    ///
    /// `delay_ms` inserts a pause between batches for remote apps that drop
    /// input when it arrives too fast; it defaults to none.
    pub async fn send_text(&self, text: &str, delay_ms: Option<u64>) -> Result<(), RdpError> {
        use std::time::Duration;

        // Two events (press + release) per UTF-16 code unit. FastPath encodes
        // the event count in a single byte, so stay well under 255 per PDU.
        const UNITS_PER_BATCH: usize = 64;

        // The RDP Unicode keyboard event carries UTF-16 code units, so a
        // character outside the BMP (emoji, rare CJK) must be sent as its
        // surrogate pair. The previous `ch as u16` silently truncated those to
        // an unrelated BMP character - the agent saw success and wrong text.
        let units: Vec<u16> = text.encode_utf16().collect();
        let mut first = true;

        for chunk in units.chunks(UNITS_PER_BATCH) {
            if !first {
                if let Some(ms) = delay_ms {
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
            }
            first = false;

            self.send_input(unicode_key_events(chunk)).await?;
        }

        Ok(())
    }

    /// Set clipboard text (will be available when remote pastes).
    pub async fn clipboard_set(&self, text: String) -> Result<(), RdpError> {
        self.stamp_input_activity();
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.send_command(SessionCommand::ClipboardSet { text, response_tx }, "clipboard set")
            .await?;

        Self::await_processor(response_rx, CLIPBOARD_SET_TIMEOUT, "clipboard set")
            .await?
            .map_err(RdpError::ProtocolError)
    }

    /// Get clipboard text from remote.
    pub async fn clipboard_get(&self) -> Result<Option<String>, RdpError> {
        let (response_tx, response_rx) = tokio::sync::oneshot::channel();
        self.send_command(SessionCommand::ClipboardGet { response_tx }, "clipboard get")
            .await?;

        // This one can park indefinitely by design: the reply only arrives
        // when the *remote* answers the format-data request, and a remote
        // that never answers leaves nothing to complete the oneshot.
        Self::await_processor(response_rx, CLIPBOARD_GET_TIMEOUT, "clipboard get")
            .await?
            .map_err(RdpError::ProtocolError)
    }

    /// Queue a command for the frame processor, bounded so a wedged processor
    /// surfaces as an error rather than an indefinite hang.
    async fn send_command(&self, command: SessionCommand, what: &str) -> Result<(), RdpError> {
        match tokio::time::timeout(COMMAND_SEND_TIMEOUT, self.command_tx.send(command)).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(_)) => Err(RdpError::SessionClosed),
            Err(_) => Err(RdpError::Unresponsive(format!(
                "{} could not be queued within {}s",
                what,
                COMMAND_SEND_TIMEOUT.as_secs()
            ))),
        }
    }

    /// Await a frame-processor reply with a deadline.
    async fn await_processor<T>(
        response_rx: tokio::sync::oneshot::Receiver<T>,
        limit: std::time::Duration,
        what: &str,
    ) -> Result<T, RdpError> {
        match tokio::time::timeout(limit, response_rx).await {
            Ok(Ok(value)) => Ok(value),
            Ok(Err(_)) => Err(RdpError::SessionClosed),
            Err(_) => Err(RdpError::Unresponsive(format!(
                "{} got no reply within {}s",
                what,
                limit.as_secs()
            ))),
        }
    }

    /// Disconnect from the RDP server and wait (bounded) for the frame
    /// processor to stop.
    pub async fn disconnect(self) -> Result<(), RdpError> {
        self.disconnect_with_join(DISCONNECT_JOIN_TIMEOUT).await
    }

    /// Disconnect, waiting at most `join_timeout` for the frame processor to
    /// exit before aborting it.
    ///
    /// The previous version queued `Shutdown` and returned. That let
    /// `connect` build the replacement session while the old processor was
    /// still alive - and if it was stuck in a `write_all` to a half-dead
    /// peer, it stayed alive for up to the TCP keepalive window (~30s) with
    /// its multi-MB framebuffer, TLS socket and DVC/clipboard channels. Rapid
    /// reconnects stacked those up, which is what "the daemon gets worse the
    /// longer it runs" looked like from the outside.
    ///
    /// `try_send` rather than `send().await`: the command channel is bounded,
    /// and a processor that is not draining it has a full queue - awaiting
    /// would then hang the caller, holding the session mutex, indefinitely.
    /// A queue that full means the processor is not going to act on
    /// `Shutdown` anyway; the join timeout and abort below cover it.
    pub async fn disconnect_with_join(mut self, join_timeout: std::time::Duration) -> Result<(), RdpError> {
        info!("Disconnecting from RDP session");
        if let Err(e) = self.command_tx.try_send(SessionCommand::Shutdown) {
            warn!("Frame processor is not accepting commands ({}); will abort it", e);
        }

        match tokio::time::timeout(join_timeout, &mut self.task_handle).await {
            Ok(_) => debug!("Frame processor joined"),
            Err(_) => {
                warn!(
                    "Frame processor did not stop within {}s; aborting it",
                    join_timeout.as_secs()
                );
                self.task_handle.abort();
            }
        }
        Ok(())
    }

    /// Set up clipboard change notification channel (for WebSocket integration).
    /// When the remote clipboard changes, a message will be sent through this channel.
    pub fn set_clipboard_changed_notify(&self, tx: mpsc::UnboundedSender<()>) {
        let state = self.shared.read();
        let mut clipboard = state.clipboard.lock();
        clipboard.clipboard_changed_tx = Some(tx);
    }
}

/// Background task that continuously processes RDP frames.
async fn run_frame_processor(
    mut framed: TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
    mut active_stage: ActiveStage,
    shared: Arc<RwLock<SharedState>>,
    mut command_rx: mpsc::Receiver<SessionCommand>,
    disconnect_notify: Option<DisconnectNotify>,
    mut clipboard_backend_rx: mpsc::UnboundedReceiver<clipboard::BackendMessage>,
    mut dvc_command_rx: Option<DvcCommandReceiver>,
    activation_factory: ConnectionActivationFactory,
    keep_alive: Option<std::time::Duration>,
) {
    info!("Frame processor started");
    let mut graceful_shutdown = false;
    let mut drop_reason = String::from("frame processor stopped");

    // Warn on the transition into failure only: the read arm owns drop
    // detection, so a keep-alive that cannot write is not itself fatal - but
    // it must not log once per tick for the life of a dead socket either.
    let mut keep_alive_failing = false;
    let mut keep_alive_watch = KeepAliveWatch::default();
    let mut last_keep_alive_sent_at: Option<std::time::Instant> = None;
    let mut keep_alive_timer = keep_alive.map(|period| {
        info!("Keep-alive enabled: every {}s", period.as_secs());
        let mut timer = tokio::time::interval(period);
        // A tick missed while the frame processor was busy (RDPDR I/O is
        // serviced synchronously) should not fire immediately afterwards.
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `interval`'s first tick completes immediately; `reset` pushes it out
        // a full period so a fresh session sends nothing before it is idle.
        timer.reset();
        timer
    });

    loop {
        tokio::select! {
            // Keep the connection alive while idle. An RDP session at rest
            // sends nothing in either direction, so a NAT or middlebox on the
            // path drops it after its own idle timeout (~285s in the field);
            // TCP keepalive alone did not prevent that.
            _ = async {
                match keep_alive_timer.as_mut() {
                    Some(timer) => { timer.tick().await; }
                    None => std::future::pending().await,
                }
            } => {
                // Sent without checking the server's `refresh_rect_support`
                // capability: ironrdp's `ConnectionResult` does not retain the
                // negotiated capability sets, so there is nothing to read it
                // from. MS-RDPBCGR says a client should not send this unless
                // the server advertised support; Windows RDS does, and a
                // server that ignores it still sees the traffic, which is the
                // point. `--keep-alive-secs 0` is the escape hatch if some
                // server ever objects.
                //
                // A Refresh Rect PDU asks the server to repaint a region. It
                // is the only cheap client-to-server PDU with no side effects
                // on the remote desktop. A Synchronize input event would also
                // work as traffic, but it makes the server adopt the lock-key
                // state it carries, so it would silently switch Num Lock off
                // on someone's desktop every tick.
                let pdu = ShareDataPdu::RefreshRectangle(RefreshRectanglePdu {
                    areas_to_refresh: vec![ironrdp::pdu::geometry::InclusiveRectangle {
                        left: 0,
                        top: 0,
                        right: 0,
                        bottom: 0,
                    }],
                });
                let mut buf = ironrdp::core::WriteBuf::new();
                match active_stage.encode_static(&mut buf, pdu) {
                    Ok(_) => match framed.write_all(buf.filled()).await {
                        Ok(()) => {
                            if keep_alive_failing {
                                info!("Keep-alive recovered");
                                keep_alive_failing = false;
                            }
                            debug!("Keep-alive sent");

                            // A refresh is a request, and a live server answers it
                            // with a repaint. A server whose TCP stack is still up
                            // but whose RDP service is gone (the field's
                            // ERROR_SEM_TIMEOUT case) keeps ACKing these writes, so
                            // neither TCP keepalive nor the unacked-data bound ever
                            // fires - the client sat blind for 18 minutes on a
                            // transport that had answered nothing. Consecutive
                            // unanswered refreshes are the one signal that separates
                            // "idle" from "dead" here; see `KeepAliveWatch` for why
                            // they are counted per send rather than timed.
                            let now = std::time::Instant::now();
                            let answered = match last_keep_alive_sent_at {
                                Some(sent) => shared.read().last_frame_at > sent,
                                None => true,
                            };
                            last_keep_alive_sent_at = Some(now);
                            let dead = keep_alive_watch.record_send(answered);
                            if dead && !silence_drop_disabled() {
                                let inbound_age = shared.read().last_frame_at.elapsed();
                                drop_reason = format!(
                                    "the server answered none of the last {} keep-alive \
                                     refresh requests (no data from it for {}s; its TCP stack \
                                     acknowledged the sends, its RDP service did not respond)",
                                    keep_alive_watch.unanswered(),
                                    inbound_age.as_secs()
                                );
                                error!("{}", drop_reason);
                                break;
                            } else if dead {
                                warn!(
                                    "{} keep-alive refreshes unanswered; not dropping because {} \
                                     is set",
                                    keep_alive_watch.unanswered(),
                                    SILENCE_DROP_KILL_SWITCH
                                );
                            }
                        }
                        Err(e) => {
                            if !keep_alive_failing {
                                warn!(
                                    "Keep-alive send failed: {} (the read arm reports the drop \
                                     if the transport is gone)",
                                    e
                                );
                                keep_alive_failing = true;
                            }
                        }
                    },
                    Err(e) => warn!("Failed to encode keep-alive: {}", e),
                }
            }
            // Handle incoming commands
            cmd = command_rx.recv() => {
                match cmd {
                    Some(SessionCommand::SendInput(events)) => {
                        debug!("Frame processor received {} input events", events.len());
                        // Process input and collect response frames
                        let frames_to_send: Vec<Vec<u8>> = {
                            let mut state = shared.write();
                            match active_stage.process_fastpath_input(&mut state.image, &events) {
                                Ok(outputs) => {
                                    debug!("Input processing generated {} outputs", outputs.len());
                                    outputs.into_iter()
                                        .filter_map(|o| {
                                            if let ActiveStageOutput::ResponseFrame(frame) = o {
                                                Some(frame)
                                            } else {
                                                None
                                            }
                                        })
                                        .collect()
                                }
                                Err(e) => {
                                    error!("Failed to process input: {}", e);
                                    Vec::new()
                                }
                            }
                        };
                        // Send frames after releasing lock
                        debug!("Sending {} input response frames", frames_to_send.len());
                        for frame in &frames_to_send {
                            debug!("Sending input frame of {} bytes", frame.len());
                            if let Err(e) = framed.write_all(frame).await {
                                error!("Failed to send input frame: {}", e);
                            }
                        }
                    }
                    Some(SessionCommand::ClipboardSet { text, response_tx }) => {
                        debug!("Clipboard set: {} chars", text.len());
                        // Store text in clipboard state
                        {
                            let state = shared.read();
                            let mut clipboard = state.clipboard.lock();
                            clipboard.local_text = Some(text);
                        }
                        // Trigger initiate_copy to announce we have data
                        if let Some(cliprdr) = active_stage.get_svc_processor_mut::<clipboard::CliprdrClient>() {
                            let formats = vec![clipboard::ClipboardFormat::new(clipboard::cf_unicodetext())];
                            match cliprdr.initiate_copy(&formats) {
                                Ok(messages) => {
                                    if let Ok(pdu_bytes) = active_stage.process_svc_processor_messages(messages) {
                                        let _ = framed.write_all(&pdu_bytes).await;
                                    }
                                    let _ = response_tx.send(Ok(()));
                                }
                                Err(e) => {
                                    let _ = response_tx.send(Err(format!("initiate_copy failed: {}", e)));
                                }
                            }
                        } else {
                            let _ = response_tx.send(Err("Clipboard not available".to_string()));
                        }
                    }
                    Some(SessionCommand::ClipboardGet { response_tx }) => {
                        debug!("Clipboard get requested");
                        // Check if we already have remote text cached
                        let cached = {
                            let state = shared.read();
                            let clipboard = state.clipboard.lock();
                            clipboard.remote_text.clone()
                        };
                        if let Some(text) = cached {
                            let _ = response_tx.send(Ok(Some(text)));
                        } else {
                            // Need to request from remote - store the response channel
                            {
                                let state = shared.read();
                                let mut clipboard = state.clipboard.lock();
                                clipboard.pending_get = Some(response_tx);
                            }
                            // Initiate paste to request data
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<clipboard::CliprdrClient>() {
                                match cliprdr.initiate_paste(clipboard::cf_unicodetext()) {
                                    Ok(messages) => {
                                        if let Ok(pdu_bytes) = active_stage.process_svc_processor_messages(messages) {
                                            let _ = framed.write_all(&pdu_bytes).await;
                                        }
                                    }
                                    Err(e) => {
                                        error!("initiate_paste failed: {}", e);
                                        // Return pending response with error
                                        let state = shared.read();
                                        let mut clipboard = state.clipboard.lock();
                                        if let Some(tx) = clipboard.pending_get.take() {
                                            let _ = tx.send(Err(format!("initiate_paste failed: {}", e)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                    Some(SessionCommand::Shutdown) => {
                        info!("Shutdown command received");
                        graceful_shutdown = true;
                        // Collect shutdown frames
                        let frames_to_send: Vec<Vec<u8>> = {
                            if let Ok(outputs) = active_stage.graceful_shutdown() {
                                outputs.into_iter()
                                    .filter_map(|o| {
                                        if let ActiveStageOutput::ResponseFrame(frame) = o {
                                            Some(frame)
                                        } else {
                                            None
                                        }
                                    })
                                    .collect()
                            } else {
                                Vec::new()
                            }
                        };
                        // Send frames
                        for frame in frames_to_send {
                            let _ = framed.write_all(&frame).await;
                        }
                        break;
                    }
                    None => {
                        // Every command sender is gone, which only happens
                        // once the owning `RdpSession` has been dropped -
                        // i.e. someone deliberately tore this session down.
                        // Treating that as a *drop* raced with `disconnect()`,
                        // which sends Shutdown and returns without waiting:
                        // if the channel closed before this loop dequeued the
                        // Shutdown, a perfectly ordinary disconnect emitted a
                        // spurious "connection dropped" notification.
                        info!("Session command channel closed; shutting down frame processor");
                        graceful_shutdown = true;
                        break;
                    }
                }
            }

            // Process incoming RDP frames
            result = framed.read_pdu() => {
                match result {
                    Ok((action, payload)) => {
                        // Process frame and collect responses
                        let (frames_to_send, should_terminate, should_reactivate) = {
                            let mut state = shared.write();
                            state.last_frame_at = std::time::Instant::now();
                            match active_stage.process(&mut state.image, action, &payload) {
                                Ok(outputs) => {
                                    let mut frames = Vec::new();
                                    let mut terminate = false;
                                    let mut reactivate = false;
                                    let mut dirty = false;
                                    for output in outputs {
                                        match output {
                                            ActiveStageOutput::ResponseFrame(frame) => {
                                                frames.push(frame);
                                            }
                                            ActiveStageOutput::Terminate(reason) => {
                                                warn!("Session terminated: {:?}", reason);
                                                terminate = true;
                                            }
                                            ActiveStageOutput::DeactivateAll => {
                                                // The server is renegotiating, typically because
                                                // the desktop resolution changed. Ignoring this
                                                // leaves the framebuffer at the old size while the
                                                // server sends updates for the new one, so
                                                // screenshots and every coordinate derived from
                                                // them silently stop matching the real screen.
                                                reactivate = true;
                                            }
                                            ActiveStageOutput::GraphicsUpdate(_) => {
                                                // The server painted into `state.image`. Marking
                                                // this lets the WebSocket broadcast tick skip
                                                // re-encoding a framebuffer that has not actually
                                                // changed, instead of doing it unconditionally at
                                                // the full stream frame rate forever.
                                                dirty = true;
                                            }
                                            _ => {}
                                        }
                                    }
                                    if dirty {
                                        state.frame_generation = state.frame_generation.wrapping_add(1);
                                    }
                                    (frames, terminate, reactivate)
                                }
                                Err(e) => {
                                    error!("Failed to process frame: {}", e);
                                    (Vec::new(), false, false)
                                }
                            }
                        };
                        // Send frames after releasing lock
                        for frame in frames_to_send {
                            if let Err(e) = framed.write_all(&frame).await {
                                error!("Failed to send response frame: {}", e);
                            }
                        }
                        if should_reactivate {
                            if let Err(e) =
                                reactivate(&mut framed, &mut active_stage, &shared, &activation_factory).await
                            {
                                error!("Deactivation-Reactivation Sequence failed: {}", e);
                                drop_reason = format!("deactivation-reactivation sequence failed: {}", e);
                                break;
                            }
                        }
                        if should_terminate {
                            // Server-initiated termination - notify daemon.
                            // Stamped on the shared state first: this arm
                            // returns rather than breaking, so it never
                            // reaches the bottom of the function, and a
                            // `connect` still inside its automation bootstrap
                            // has no other way to learn the session under it
                            // just ended.
                            shared.write().drop_reason =
                                Some("server-initiated termination (disconnect PDU)".to_string());
                            if let Some((notify, generation)) = disconnect_notify {
                                let _ = notify
                                    .send(DisconnectEvent {
                                        generation,
                                        reason: "server-initiated termination (disconnect PDU)".to_string(),
                                    })
                                    .await;
                            }
                            return;
                        }
                    }
                    Err(e) => {
                        error!("Failed to read PDU: {}", e);
                        drop_reason = format!("failed to read from the RDP transport: {}", e);
                        break;
                    }
                }
            }

            // Handle clipboard backend messages
            msg = clipboard_backend_rx.recv() => {
                if let Some(msg) = msg {
                    match msg {
                        clipboard::BackendMessage::InitiateCopy(formats) => {
                            debug!("Backend: InitiateCopy with {} formats", formats.len());
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<clipboard::CliprdrClient>() {
                                if let Ok(messages) = cliprdr.initiate_copy(&formats) {
                                    if let Ok(pdu_bytes) = active_stage.process_svc_processor_messages(messages) {
                                        let _ = framed.write_all(&pdu_bytes).await;
                                    }
                                }
                            }
                        }
                        clipboard::BackendMessage::FormatData(response) => {
                            debug!("Backend: FormatData");
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<clipboard::CliprdrClient>() {
                                if let Ok(messages) = cliprdr.submit_format_data(response) {
                                    if let Ok(pdu_bytes) = active_stage.process_svc_processor_messages(messages) {
                                        let _ = framed.write_all(&pdu_bytes).await;
                                    }
                                }
                            }
                        }
                        clipboard::BackendMessage::InitiatePaste(format_id) => {
                            debug!("Backend: InitiatePaste for {:?}", format_id);
                            if let Some(cliprdr) = active_stage.get_svc_processor_mut::<clipboard::CliprdrClient>() {
                                if let Ok(messages) = cliprdr.initiate_paste(format_id) {
                                    if let Ok(pdu_bytes) = active_stage.process_svc_processor_messages(messages) {
                                        let _ = framed.write_all(&pdu_bytes).await;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Handle DVC commands (for automation)
            dvc_cmd = async {
                match dvc_command_rx.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Some(cmd) = dvc_cmd {
                    debug!("Sending {} bytes on DVC channel {}", cmd.data.len(), cmd.channel_id);

                    // `encode_dvc_data` splits into DataFirst+Data PDUs at
                    // MAX_DATA_SIZE (1590 bytes) the same way `DrdynvcClient`
                    // does for its own traffic. A single hand-built `Data`
                    // PDU used to be sent regardless of size: anything over
                    // that boundary was still fragmented by the static
                    // channel layer beneath DVC (CHANNEL_CHUNK_LENGTH =
                    // 1600), but the agent's `Read-DvcMessage` did not
                    // reassemble those fragments, so every oversized request
                    // (any `file push` chunk above ~1.6KB of JSON) arrived as
                    // unparseable pieces and silently got no reply. See
                    // `automation::dvc_encode` for the encoder itself.
                    match crate::automation::encode_dvc_data(cmd.channel_id, cmd.data) {
                        Ok(svc_messages) => match active_stage.encode_dvc_messages(svc_messages) {
                            Ok(frame) => {
                                if let Err(e) = framed.write_all(&frame).await {
                                    error!("Failed to send DVC data: {}", e);
                                }
                            }
                            Err(e) => {
                                error!("Failed to encode DVC data: {:?}", e);
                            }
                        },
                        Err(e) => {
                            error!("Failed to split DVC data into PDUs: {:?}", e);
                        }
                    }
                }
            }
        }
    }

    info!("Frame processor stopped (graceful={})", graceful_shutdown);

    // Notify daemon of connection drop (unless this was a graceful shutdown).
    // The generation goes with it so the daemon can tell a drop for the
    // session it currently holds from one for a session already replaced.
    if !graceful_shutdown {
        // Before the notification, so that a caller which sees the daemon's
        // teardown has already seen the reason. A graceful shutdown leaves it
        // unset: that session was asked to stop, it did not die.
        shared.write().drop_reason = Some(drop_reason.clone());
        if let Some((notify, generation)) = disconnect_notify {
            info!(
                "Notifying daemon of connection drop (generation {}): {}",
                generation, drop_reason
            );
            let _ = notify.send(DisconnectEvent { generation, reason: drop_reason }).await;
        }
    }
}

/// Run the [Deactivation-Reactivation Sequence] after a Server Deactivate All PDU.
///
/// The server sends this when it renegotiates the session - most often because
/// the desktop resolution changed. Until the sequence is driven to completion
/// and the framebuffer reallocated, the decoded image keeps the old dimensions
/// while the server streams updates for the new ones, which quietly invalidates
/// every coordinate taken from a screenshot.
///
/// [Deactivation-Reactivation Sequence]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/dfc234ce-481a-4674-9a5d-2a7bafb14432
async fn reactivate(
    framed: &mut TokioFramed<tokio_rustls::client::TlsStream<TcpStream>>,
    active_stage: &mut ActiveStage,
    shared: &Arc<RwLock<SharedState>>,
    activation_factory: &ConnectionActivationFactory,
) -> Result<(), RdpError> {
    info!("Server sent Deactivate All; running reactivation sequence");

    let mut activation = activation_factory.create();
    let mut buf = ironrdp::core::WriteBuf::new();

    while !activation.state().is_terminal() {
        ironrdp_tokio::single_sequence_step(framed, &mut activation, &mut buf)
            .await
            .map_err(|e| RdpError::ProtocolError(e.to_string()))?;
    }

    let ConnectionActivationState::Finalized {
        desktop_size,
        share_id,
        enable_server_pointer,
        ..
    } = activation.connection_activation_state()
    else {
        return Err(RdpError::ProtocolError(
            "reactivation sequence ended in a non-finalized state".to_string(),
        ));
    };

    // The x224 processor picks up the new share id. The fast-path processor
    // holds one too, but rebuilding it needs a BulkCompressor and the PDU ->
    // bulk compression-type mapping is private to ironrdp-session, so it keeps
    // the share id from the original activation. That only affects frame-marker
    // responses, and servers reuse the share id across reactivation in
    // practice; the framebuffer size below is what actually matters here.
    active_stage.set_share_id(share_id);
    active_stage.set_enable_server_pointer(enable_server_pointer);

    let (old_width, old_height) = {
        let mut state = shared.write();
        let old = (state.width, state.height);

        // Reallocate rather than resize: the old contents describe a screen
        // layout that no longer exists, and keeping them would leave stale
        // pixels wherever the server has not yet sent an update.
        state.image = DecodedImage::new(
            ironrdp_graphics::image_processing::PixelFormat::RgbA32,
            desktop_size.width,
            desktop_size.height,
        );
        state.width = desktop_size.width;
        state.height = desktop_size.height;

        old
    };

    info!(
        "Reactivated: desktop {}x{} (was {}x{})",
        desktop_size.width, desktop_size.height, old_width, old_height
    );

    Ok(())
}

/// Custom certificate verifier that accepts all certificates.
/// This is necessary because RDP servers typically use self-signed certificates.
#[derive(Debug)]
struct NoVerifier;

impl rustls::client::danger::ServerCertVerifier for NoVerifier {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        vec![
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ECDSA_NISTP521_SHA512,
            rustls::SignatureScheme::ED25519,
        ]
    }
}

/// No-op network client for CredSSP.
/// This works for basic NTLM authentication but doesn't support Kerberos.
struct NoopNetworkClient;

impl ironrdp_tokio::NetworkClient for NoopNetworkClient {
    fn send(
        &mut self,
        _network_request: &ironrdp::connector::sspi::generator::NetworkRequest,
    ) -> impl Future<Output = ConnectorResult<Vec<u8>>> {
        async move {
            // Return empty response - NTLM auth doesn't need network calls
            Ok(Vec::new())
        }
    }
}

// ============ Key Input Helpers ============

/// Key information including scancode and extended flag.
struct KeyInfo {
    scancode: u8,
    extended: bool,
}

/// Parse a key combination like "ctrl+c" into key info for sending.
///
/// Delegates to `crate::keymap`, the single scancode table shared with the
/// CLI keyboard handler and the WebSocket input path. This function used to
/// carry its own smaller, divergent copy - fine for the fixed sequences
/// automation bootstrap sends (`super+r`, `ctrl+v`, `return`) today, but a key
/// fixed in one table silently stayed broken in the others.
fn parse_key_combination(keys: &str) -> Result<Vec<KeyInfo>, String> {
    let parts: Vec<String> = keys.split('+').map(|s| s.trim().to_lowercase()).collect();

    let mut key_infos = Vec::new();

    for key in &parts {
        let (scancode, extended) = crate::keymap::key_to_scancode(key)
            .ok_or_else(|| format!("Unknown key: {}", key))?;
        key_infos.push(KeyInfo { scancode, extended });
    }

    Ok(key_infos)
}

/// Create a keyboard event with proper flags.
fn create_key_event(scancode: u8, extended: bool, release: bool) -> FastPathInputEvent {
    let mut flags = KeyboardFlags::empty();
    if release {
        flags |= KeyboardFlags::RELEASE;
    }
    if extended {
        flags |= KeyboardFlags::EXTENDED;
    }
    FastPathInputEvent::KeyboardEvent(flags, scancode)
}

/// Copy only the rows/columns of `region` out of a full RGBA `data` buffer.
///
/// Pure function so it is testable without a live `RdpSession`, and so its
/// output can be checked byte-for-byte against
/// `handlers::imaging::crop_to_region` (crop-then-encode vs. copy-then-encode
/// must agree, or `screenshot --region` and `locate --region` would disagree
/// on what pixels a coordinate refers to).
///
/// `region` must already be within `full_width x full_height`; an
/// out-of-bounds region returns `None` rather than panicking on a bad slice.
fn copy_rgba_region(
    data: &[u8],
    full_width: u32,
    full_height: u32,
    region: agent_rdp_protocol::Region,
) -> Option<Vec<u8>> {
    let (full_width, full_height) = (full_width as usize, full_height as usize);
    let (x, y) = (region.x as usize, region.y as usize);
    let (w, h) = (region.width as usize, region.height as usize);
    if w == 0 || h == 0 || x + w > full_width || y + h > full_height {
        return None;
    }

    let mut out = Vec::with_capacity(w * h * 4);
    for row in y..y + h {
        let start = (row * full_width + x) * 4;
        out.extend_from_slice(&data[start..start + w * 4]);
    }
    Some(out)
}

/// Build press+release fast-path events for a batch of UTF-16 code units.
fn unicode_key_events(units: &[u16]) -> Vec<FastPathInputEvent> {
    let mut events = Vec::with_capacity(units.len() * 2);
    for &code in units {
        events.push(FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), code));
        events.push(FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, code));
    }
    events
}

/// Turn IronRDP's negotiation errors into something a user can act on.
///
/// The underlying strings describe the protocol outcome but not the fix, and
/// the most common one ("standard RDP security") is a host misconfiguration
/// that cannot be worked around client-side: IronRDP refuses that security
/// layer outright, so no flag will make the connection succeed.
fn explain_connect_error(raw: &str) -> String {
    let lower = raw.to_lowercase();

    if lower.contains("standard rdp security") {
        // The server refused TLS and offered only the legacy layer. Don't assert
        // *why*: this also fires with SecurityLayer=1 (Negotiate) on hosts that
        // demonstrably do TLS at SecurityLayer=2, so claiming "TLS is disabled"
        // would be wrong. Point at the setting that is known to work instead.
        return format!(
            "{raw}. The server declined TLS and offered only legacy Standard RDP \
             Security, which is not supported (RC4 with a well-known key derivation, \
             so credentials sent over it are recoverable). This happens when the host \
             has NLA disabled AND is not set to require TLS. Under \
             HKLM\\System\\CurrentControlSet\\Control\\Terminal Server\\WinStations\\\
             RDP-Tcp, either set UserAuthentication=1 (enables NLA, which forces \
             TLS and adds pre-authentication) or SecurityLayer=2 (forces TLS). Either \
             one is enough. New connections normally pick the change up immediately; \
             restart TermService only if they do not."
        );
    }

    if lower.contains("requires enhanced rdp security with credssp")
        || lower.contains("hybrid_required")
    {
        return format!("{raw}. The server requires NLA/CredSSP; check the username, password and domain.");
    }

    raw.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn codes(events: &[FastPathInputEvent]) -> Vec<u16> {
        events
            .iter()
            .filter_map(|e| match e {
                FastPathInputEvent::UnicodeKeyboardEvent(flags, code)
                    if !flags.contains(KeyboardFlags::RELEASE) =>
                {
                    Some(*code)
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn test_unicode_key_events_bmp_char_is_untruncated() {
        // A BMP character round-trips as itself.
        let units: Vec<u16> = "A".encode_utf16().collect();
        assert_eq!(units, vec![0x0041]);
        let events = unicode_key_events(&units);
        assert_eq!(events.len(), 2); // press + release
        assert_eq!(codes(&events), vec![0x0041]);
    }

    #[test]
    fn test_unicode_key_events_surrogate_pair_is_not_truncated() {
        // U+1F642 (slightly smiling face) is above the BMP and must be sent
        // as its two surrogate halves, not truncated to a BMP code point.
        // `ch as u16` truncation would have produced 0xF642 - a private-use
        // codepoint with no relation to the emoji.
        let units: Vec<u16> = "🙂".encode_utf16().collect();
        assert_eq!(units.len(), 2, "U+1F642 must encode as a surrogate pair");
        assert_eq!(units, vec![0xD83D, 0xDE42]);

        let events = unicode_key_events(&units);
        assert_eq!(events.len(), 4); // two code units x press+release
        assert_eq!(codes(&events), vec![0xD83D, 0xDE42]);

        // The old bug's output must not appear anywhere in the stream.
        assert!(!codes(&events).contains(&0xF642));
    }

    #[test]
    fn test_unicode_key_events_press_before_release_per_unit() {
        let units: Vec<u16> = "Z".encode_utf16().collect();
        let events = unicode_key_events(&units);
        match &events[..] {
            [FastPathInputEvent::UnicodeKeyboardEvent(f0, c0), FastPathInputEvent::UnicodeKeyboardEvent(f1, c1)] => {
                assert!(!f0.contains(KeyboardFlags::RELEASE));
                assert!(f1.contains(KeyboardFlags::RELEASE));
                assert_eq!((c0, c1), (&0x005A, &0x005A));
            }
            other => panic!("unexpected event shape: {:?}", other),
        }
    }

    #[test]
    fn test_unicode_key_events_empty_input() {
        assert!(unicode_key_events(&[]).is_empty());
    }

    fn coordinate_rgba(width: u32, height: u32) -> image::RgbaImage {
        image::RgbaImage::from_fn(width, height, |x, y| {
            image::Rgba([(x % 256) as u8, (y % 256) as u8, (x / 256) as u8, 255])
        })
    }

    #[test]
    fn test_copy_rgba_region_matches_crop_to_region() {
        // The region-only copy path (used by screenshot/locate) and the
        // crop-then-encode path (used by handlers::imaging) must produce
        // byte-identical pixels for the same region, or the two commands
        // would disagree on what a coordinate refers to.
        let source = coordinate_rgba(1280, 800);
        let region = agent_rdp_protocol::Region { x: 100, y: 380, width: 400, height: 30 };

        let copied = copy_rgba_region(source.as_raw(), 1280, 800, region).unwrap();
        let (cropped, used) = crate::handlers::imaging::crop_to_region(&source, region).unwrap();

        assert_eq!(used, region);
        assert_eq!(copied, cropped.into_raw());
    }

    #[test]
    fn test_copy_rgba_region_matches_crop_to_region_at_the_corner() {
        // Same check at the framebuffer's bottom-right, where an off-by-one
        // in either implementation would show up as a size mismatch.
        let source = coordinate_rgba(640, 480);
        let region = agent_rdp_protocol::Region { x: 639, y: 479, width: 1, height: 1 };

        let copied = copy_rgba_region(source.as_raw(), 640, 480, region).unwrap();
        let (cropped, _) = crate::handlers::imaging::crop_to_region(&source, region).unwrap();

        assert_eq!(copied, cropped.into_raw());
    }

    #[test]
    fn test_copy_rgba_region_rejects_out_of_bounds() {
        let source = coordinate_rgba(100, 100);
        assert_eq!(
            copy_rgba_region(source.as_raw(), 100, 100, agent_rdp_protocol::Region { x: 100, y: 0, width: 1, height: 1 }),
            None
        );
        assert_eq!(
            copy_rgba_region(source.as_raw(), 100, 100, agent_rdp_protocol::Region { x: 0, y: 0, width: 0, height: 10 }),
            None
        );
    }

    #[test]
    fn test_unicode_key_events_mixed_bmp_and_surrogate_text() {
        // Cyrillic (BMP) followed by an emoji (surrogate pair): every unit
        // must survive, in order, none silently dropped or corrupted.
        let units: Vec<u16> = "Привет🙂".encode_utf16().collect();
        let events = unicode_key_events(&units);
        assert_eq!(events.len(), units.len() * 2);
        assert_eq!(codes(&events), units);
    }
}

/// Liveness plumbing that needs no RDP server: the drop probe a `connect`
/// holds across its bootstrap, the socket option that bounds a dead path,
/// and the keep-alive verdict.
#[cfg(test)]
mod liveness_tests {
    use super::*;
    use std::time::Duration;

    fn shared_state() -> Arc<RwLock<SharedState>> {
        Arc::new(RwLock::new(SharedState {
            image: DecodedImage::new(ironrdp_graphics::image_processing::PixelFormat::RgbA32, 1, 1),
            host: "test".to_string(),
            width: 1,
            height: 1,
            frame_generation: 0,
            last_frame_at: std::time::Instant::now(),
            drop_reason: None,
            drives: Vec::new(),
            clipboard: Arc::new(parking_lot::Mutex::new(clipboard::ClipboardState::default())),
        }))
    }

    /// `connect` clones the probe before a bootstrap that can take minutes
    /// and consults it afterwards: it has to be a live view of the session
    /// state, not a snapshot taken when it was cloned.
    #[test]
    fn a_drop_probe_sees_a_drop_recorded_after_it_was_taken() {
        let shared = shared_state();
        let probe = DropProbe { shared: Arc::clone(&shared) };
        assert!(probe.drop_reason().is_none());

        shared.write().drop_reason = Some("failed to read from the RDP transport".to_string());

        assert_eq!(
            probe.drop_reason().as_deref(),
            Some("failed to read from the RDP transport")
        );
        // And a clone of the probe is the same view, not a copy.
        assert!(probe.clone().drop_reason().is_some());
    }

    /// Runs the real `setsockopt` on whatever OS executes the tests. CI's
    /// `test` job runs on Linux and Windows, so this is the only execution the
    /// Windows `TCP_MAXRT` arm gets before a deployment; there is no macOS
    /// runner in that job, so the macOS arm is verified by compilation only.
    #[tokio::test]
    async fn the_unacked_data_bound_applies_to_a_real_socket() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stream = TcpStream::connect(addr).await.unwrap();
        let _peer = listener.accept().await.unwrap();

        RdpSession::apply_tcp_unacked_timeout(&stream, Duration::from_secs(5))
            .expect("the platform's unacked-data option should apply to a live socket");
        // The keepalive path it sits next to must keep working as well.
        RdpSession::apply_tcp_keepalive(&stream).expect("keepalive applies to a live socket");
    }

    /// Three unanswered sends in a row, not one: a single missed repaint is
    /// a busy server, three consecutive on a socket that still ACKs is a
    /// dead service.
    #[test]
    fn the_server_gets_three_unanswered_sends_before_it_is_declared_dead() {
        let mut watch = KeepAliveWatch::default();
        assert!(!watch.record_send(true), "first send, nothing to compare against");
        assert!(!watch.record_send(false));
        assert!(!watch.record_send(false));
        assert_eq!(watch.unanswered(), 2);
        assert!(watch.record_send(false), "the third consecutive silence is death");
        assert_eq!(watch.unanswered(), 3);
    }

    /// One answer anywhere resets the count: an idle desktop that answers
    /// every refresh never accumulates strikes.
    #[test]
    fn any_answer_clears_the_strikes() {
        let mut watch = KeepAliveWatch::default();
        watch.record_send(false);
        watch.record_send(false);
        assert!(!watch.record_send(true));
        assert_eq!(watch.unanswered(), 0);
        assert!(!watch.record_send(false));
        assert!(!watch.record_send(false));
        assert!(watch.record_send(false));
    }

    /// The stall the counter exists for: the loop was blocked for longer than
    /// the whole limit while the server was fine. Only one tick fires when it
    /// resumes, so a single strike at most - and the drained replies clear it
    /// on the next send, whichever arm `select!` happened to run first.
    #[test]
    fn a_long_local_stall_is_at_most_one_strike() {
        let mut watch = KeepAliveWatch::default();
        watch.record_send(true);
        // Resume after a 10-minute stall: the timer fires once, and the
        // replies to the pre-stall send are still in the socket buffer.
        assert!(!watch.record_send(false), "one overdue tick is one strike, not death");
        assert_eq!(watch.unanswered(), 1);
        // The read arm drains the buffer; the next send sees an answer.
        assert!(!watch.record_send(true));
        assert_eq!(watch.unanswered(), 0);
    }

    #[test]
    fn the_silence_kill_switch_reads_like_the_relaunch_one() {
        // Same convention as AGENT_RDP_NO_AUTO_RELAUNCH: any non-empty value
        // other than "0" disables. Checked through the parser only, since the
        // process environment is shared with other tests.
        let parse = |v: Option<&str>| v.map(|v| !v.is_empty() && v != "0").unwrap_or(false);
        assert!(!parse(None));
        assert!(!parse(Some("")));
        assert!(!parse(Some("0")));
        assert!(parse(Some("1")));
        assert!(parse(Some("yes")));
    }
}
