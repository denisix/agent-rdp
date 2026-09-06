//! DVC processor for automation communication.
//!
//! Implements the DvcProcessor trait for bidirectional communication with the
//! PowerShell automation agent via Dynamic Virtual Channel.

use std::collections::HashMap;
use std::sync::Arc;

use ironrdp_dvc::ironrdp_pdu::PduResult;
use ironrdp_dvc::{DvcMessage, DvcProcessor};
use ironrdp_svc::impl_as_any;
use parking_lot::Mutex;
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, error, info, trace, warn};

/// DVC channel name for automation.
pub const CHANNEL_NAME: &str = "AgentRdp::Automation";

/// Message types for DVC protocol.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DvcProtocolMessage {
    /// Handshake sent by PowerShell agent when channel opens.
    Handshake {
        version: String,
        agent_pid: u32,
        capabilities: Vec<String>,
        #[serde(default)]
        build_id: Option<String>,
    },
    /// Request sent from Rust to PowerShell.
    Request {
        id: String,
        command: String,
        params: serde_json::Value,
    },
    /// Response sent from PowerShell to Rust.
    Response {
        id: String,
        success: bool,
        data: Option<serde_json::Value>,
        error: Option<DvcError>,
    },
    /// Poll message from PowerShell to trigger sending queued requests.
    Poll,
}

/// Error in DVC response.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DvcError {
    pub code: String,
    pub message: String,
}

/// Handshake data from the PowerShell agent.
#[derive(Debug, Clone)]
pub struct DvcHandshake {
    pub version: String,
    pub agent_pid: u32,
    pub capabilities: Vec<String>,
    /// Hash of every embedded script the launching daemon deployed
    /// (`bootstrap::expected_build_id`), echoed back by the agent it
    /// launched. `$script:Version` alone names `agent.ps1`; this covers the
    /// library files too, so a daemon whose scripts changed without a
    /// version bump does not adopt a survivor still running the old ones -
    /// silently talking to an old script is exactly what the daemon/CLI
    /// version contract exists to prevent, one layer down. `None` from an
    /// agent that predates this field.
    pub build_id: Option<String>,
}

/// Response data for pending requests.
#[derive(Debug)]
pub struct DvcResponse {
    pub success: bool,
    pub data: Option<serde_json::Value>,
    pub error: Option<DvcError>,
}

/// Command to send DVC data through the RDP session.
#[derive(Debug)]
pub struct DvcSendCommand {
    pub channel_id: u32,
    pub data: Vec<u8>,
}

/// Sender for DVC commands to the RDP session.
pub type DvcCommandSender = mpsc::UnboundedSender<DvcSendCommand>;
/// Receiver for DVC commands in the RDP session.
pub type DvcCommandReceiver = mpsc::UnboundedReceiver<DvcSendCommand>;

/// Shared state for DVC communication, accessible from both the processor and IPC.
#[derive(Debug)]
pub struct DvcSharedState {
    /// Pending requests awaiting response (id -> sender).
    pub pending: HashMap<String, oneshot::Sender<DvcResponse>>,
    /// Handshake received from PowerShell.
    pub handshake: Option<DvcHandshake>,
    /// When `handshake` was set - the basis for the agent uptime reported by
    /// `automate status`.
    pub handshake_at: Option<std::time::Instant>,
    /// Channel ID (set when opened).
    pub channel_id: Option<u32>,
    /// Channels opened by agents other than the one `channel_id` belongs to.
    ///
    /// Two agents can legitimately race: one that survived a transport drop
    /// re-opens its channel while a fresh `connect` is typing Win+R for a new
    /// one. Whichever opens first is the agent this daemon talks to; the
    /// others are told to exit. Tracked by id so a late `close()` from a
    /// rejected agent cannot clear the live agent's handshake.
    pub extras: std::collections::HashSet<u32>,
    /// Sender to send DVC data through the RDP session.
    pub command_tx: Option<DvcCommandSender>,
    /// Fired from `close()` so the session's relaunch supervisor learns that
    /// the agent process ended while the RDP session may still be alive.
    /// Dropped with the state at cleanup, which ends the supervisor.
    pub closed_notify: Option<mpsc::UnboundedSender<()>>,
}

impl Default for DvcSharedState {
    fn default() -> Self {
        Self {
            pending: HashMap::new(),
            handshake: None,
            handshake_at: None,
            channel_id: None,
            extras: std::collections::HashSet::new(),
            command_tx: None,
            closed_notify: None,
        }
    }
}

impl DvcSharedState {
    /// The agent has opened the channel but not completed its handshake -
    /// it is starting up. A relaunch now would produce two agents.
    pub fn is_launching(&self) -> bool {
        self.channel_id.is_some() && self.handshake.is_none()
    }
}

/// Shared state handle.
pub type SharedDvcState = Arc<Mutex<DvcSharedState>>;

/// Create a new shared DVC state.
pub fn new_shared_dvc_state() -> SharedDvcState {
    Arc::new(Mutex::new(DvcSharedState::default()))
}

/// DVC processor for automation channel.
pub struct AutomationDvc {
    /// Shared state for communication with IPC layer.
    state: SharedDvcState,
    /// Notify channel for handshake completion.
    handshake_tx: Option<mpsc::UnboundedSender<DvcHandshake>>,
}

impl AutomationDvc {
    /// Create a new automation DVC processor.
    pub fn new(state: SharedDvcState) -> Self {
        Self {
            state,
            handshake_tx: None,
        }
    }

    /// Create with a handshake notification channel.
    pub fn with_handshake_notify(
        state: SharedDvcState,
        handshake_tx: mpsc::UnboundedSender<DvcHandshake>,
    ) -> Self {
        Self {
            state,
            handshake_tx: Some(handshake_tx),
        }
    }

    /// Decode a JSON message from the buffer (DVC handles message framing).
    fn decode_message(payload: &[u8]) -> Result<DvcProtocolMessage, String> {
        if payload.is_empty() {
            return Err("Empty payload".to_string());
        }

        // Handle potential UTF-8 BOM
        let json_str = std::str::from_utf8(payload)
            .map_err(|e| format!("Invalid UTF-8: {}", e))?;
        let json_str = json_str.strip_prefix('\u{feff}').unwrap_or(json_str);

        serde_json::from_str(json_str)
            .map_err(|e| format!("JSON parse error: {}", e))
    }

    /// The one request ever sent to an agent this daemon is not adopting.
    ///
    /// Built here rather than through `DvcIpc` on purpose: there is no
    /// pending-request slot for it and no reply to wait for - the agent
    /// answers and exits.
    fn shutdown_request(channel_id: u32) -> Vec<u8> {
        serde_json::to_vec(&DvcProtocolMessage::Request {
            id: format!("shutdown-{}", channel_id),
            command: "shutdown".to_string(),
            params: serde_json::json!({}),
        })
        .expect("a fixed shutdown request always serializes")
    }

    /// Encode a message as JSON (used by tests).
    #[cfg(test)]
    fn encode_message(msg: &DvcProtocolMessage) -> Result<Vec<u8>, String> {
        serde_json::to_string(msg)
            .map(|s| s.into_bytes())
            .map_err(|e| format!("JSON encode error: {}", e))
    }
}

impl_as_any!(AutomationDvc);

impl DvcProcessor for AutomationDvc {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn start(&mut self, channel_id: u32) -> PduResult<Vec<DvcMessage>> {
        let mut state = self.state.lock();
        // A reused id that was rejected before is not still rejected: ids are
        // per-channel-instance, so this can only be a genuinely new open.
        state.extras.remove(&channel_id);
        match state.channel_id {
            None => {
                debug!("AutomationDvc channel started with ID {}", channel_id);
                state.channel_id = Some(channel_id);
            }
            Some(primary) => {
                // A second agent reached us while one is already connected.
                // Keep the first and stop the newcomer once it identifies
                // itself (see `process`), rather than switching mid-flight
                // and stranding the requests in `pending`.
                warn!(
                    "A second automation agent opened channel {} while {} is in use; \
                     it will be asked to exit",
                    channel_id, primary
                );
                state.extras.insert(channel_id);
            }
        }

        // No initial messages from client side - we wait for PowerShell to send handshake
        Ok(Vec::new())
    }

    fn process(&mut self, channel_id: u32, payload: &[u8]) -> PduResult<Vec<DvcMessage>> {
        trace!(
            "AutomationDvc received {} bytes on channel {}",
            payload.len(),
            channel_id
        );

        // Decode the incoming message
        // Anything arriving on a channel we already rejected is ignored, with
        // one exception below: its handshake is the first moment we can tell
        // it to exit.
        let is_extra = self.state.lock().extras.contains(&channel_id);

        let msg = match Self::decode_message(payload) {
            Ok(msg) => msg,
            Err(e) => {
                // Include what actually arrived: a bare serde error ("EOF
                // while parsing a value at line 1 column 0") says nothing
                // about whether the agent sent an empty frame, a BOM-only
                // frame, or binary garbage, and this is the only trace a
                // dropped message leaves.
                let preview_len = payload.len().min(32);
                let hex: String = payload[..preview_len]
                    .iter()
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ");
                error!(
                    "Failed to decode DVC message ({} bytes, first {} as hex: [{}]): {}",
                    payload.len(),
                    preview_len,
                    hex,
                    e
                );
                return Ok(Vec::new());
            }
        };

        match msg {
            DvcProtocolMessage::Handshake {
                version,
                agent_pid,
                capabilities,
                build_id,
            } => {
                // Decided fresh, under the lock, rather than from `is_extra`
                // above: the primary can have closed since `start()` ran (the
                // agent it belonged to exited), and an extra reaching its
                // handshake at that moment is the only agent left - reflexively
                // shutting it down would leave nothing connected at all.
                let promote = {
                    let state = self.state.lock();
                    state.channel_id.is_none() || state.channel_id == Some(channel_id)
                };

                if is_extra && !promote {
                    warn!(
                        "Second automation agent (pid {}, version {}) handshook on channel {}; \
                         asking it to exit and keeping the one already connected",
                        agent_pid, version, channel_id
                    );
                    return Ok(vec![Box::new(crate::automation::dvc_encode::RawDvcBytes(
                        Self::shutdown_request(channel_id),
                    ))]);
                }

                debug!(
                    "Received DVC handshake: version={}, pid={}, caps={:?}",
                    version, agent_pid, capabilities
                );

                let handshake = DvcHandshake {
                    version,
                    agent_pid,
                    capabilities,
                    build_id,
                };

                // Store handshake and notify
                {
                    let mut state = self.state.lock();
                    if is_extra {
                        info!(
                            "Promoting channel {} to primary: the previous primary is gone",
                            channel_id
                        );
                        state.extras.remove(&channel_id);
                        state.channel_id = Some(channel_id);
                    }
                    state.handshake = Some(handshake.clone());
                    state.handshake_at = Some(std::time::Instant::now());
                }

                if let Some(ref tx) = self.handshake_tx {
                    let _ = tx.send(handshake);
                }
            }

            DvcProtocolMessage::Response {
                id,
                success,
                data,
                error,
            } => {
                if is_extra {
                    // The only thing we ever sent it was `shutdown`, and it
                    // owns no pending request.
                    trace!("Ignoring response {} from the rejected agent on channel {}", id, channel_id);
                    return Ok(Vec::new());
                }

                debug!("Received DVC response for request {}: success={}", id, success);

                // Route to pending request
                let sender = {
                    let mut state = self.state.lock();
                    state.pending.remove(&id)
                };

                if let Some(sender) = sender {
                    let response = DvcResponse {
                        success,
                        data,
                        error,
                    };
                    let _ = sender.send(response);
                } else {
                    warn!("Received response for unknown request ID: {}", id);
                }
            }

            DvcProtocolMessage::Request { .. } => {
                // Unexpected - requests should only go from Rust to PowerShell
                warn!("Received unexpected request message from PowerShell");
            }

            DvcProtocolMessage::Poll => {
                // Poll message - no longer needed since we send proactively
                // Just acknowledge receipt
                trace!("Received poll from PowerShell (ignored - using proactive send)");
            }
        }

        // We now send data proactively through the command channel, so no queued messages
        Ok(Vec::new())
    }

    fn close(&mut self, channel_id: u32) {
        let mut state = self.state.lock();

        // A rejected agent exiting must not look like the live one leaving:
        // clearing the handshake here would strand every later request and
        // wake the relaunch supervisor for an agent that is fine.
        if state.extras.remove(&channel_id) {
            debug!("Rejected automation agent on channel {} exited", channel_id);
            return;
        }
        if state.channel_id.is_some_and(|primary| primary != channel_id) {
            debug!(
                "Ignoring close of channel {} - it is not the channel in use",
                channel_id
            );
            return;
        }

        debug!("AutomationDvc channel {} closed", channel_id);
        state.channel_id = None;
        state.handshake = None;
        state.handshake_at = None;

        // Synchronous, never blocks: this runs inside the frame processor
        // under its write lock. The supervisor decides whether the session
        // is still alive and a relaunch is warranted.
        if let Some(notify) = state.closed_notify.as_ref() {
            let _ = notify.send(());
        }

        // Notify all pending requests that the channel closed
        for (id, sender) in state.pending.drain() {
            warn!("Channel closed, failing pending request {}", id);
            let _ = sender.send(DvcResponse {
                success: false,
                data: None,
                error: Some(DvcError {
                    code: "channel_closed".to_string(),
                    message: "DVC channel was closed".to_string(),
                }),
            });
        }
    }
}

/// Accepts an automation channel every time the remote side opens one.
///
/// The alternative, `DrdynvcClient::with_dynamic_channel`, registers the
/// processor through a listener that hands it out exactly once per RDP
/// session (`OnceListener::create` is a `take`). Every later open in the same
/// session is answered with NO_LISTENER - so a relaunched agent could never
/// reattach, and both the self-heal supervisor and `automate restart` waited
/// out their handshake windows against a channel the client had already
/// refused. Only a full reconnect, which builds a new `DrdynvcClient`, used to
/// recover.
pub struct AutomationDvcListener {
    state: SharedDvcState,
}

impl AutomationDvcListener {
    /// Create a listener that binds every accepted channel to `state`.
    pub fn new(state: SharedDvcState) -> Self {
        Self { state }
    }
}

impl ironrdp_dvc::DvcChannelListener for AutomationDvcListener {
    fn channel_name(&self) -> &str {
        CHANNEL_NAME
    }

    fn create(&mut self, channel_id: u32) -> Option<Box<dyn DvcProcessor>> {
        debug!("Accepting automation channel {}", channel_id);
        Some(Box::new(AutomationDvc::new(Arc::clone(&self.state))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_handshake() {
        let msg = DvcProtocolMessage::Handshake {
            version: "1.0.0".to_string(),
            agent_pid: 1234,
            capabilities: vec!["snapshot".to_string(), "click".to_string()],
            build_id: Some("deadbeef".to_string()),
        };

        let encoded = AutomationDvc::encode_message(&msg).unwrap();
        let decoded = AutomationDvc::decode_message(&encoded).unwrap();

        match decoded {
            DvcProtocolMessage::Handshake {
                version,
                agent_pid,
                capabilities,
                build_id,
            } => {
                assert_eq!(version, "1.0.0");
                assert_eq!(agent_pid, 1234);
                assert_eq!(capabilities.len(), 2);
                assert_eq!(build_id.as_deref(), Some("deadbeef"));
            }
            _ => panic!("Expected handshake"),
        }
    }

    /// An agent from before this field existed omits it entirely; the
    /// `#[serde(default)]` is what keeps that message parseable.
    #[test]
    fn a_handshake_without_a_build_id_still_decodes() {
        let json = br#"{"type":"handshake","version":"1.7.0","agent_pid":1,"capabilities":[]}"#;
        let decoded = AutomationDvc::decode_message(json).unwrap();
        match decoded {
            DvcProtocolMessage::Handshake { build_id, .. } => assert!(build_id.is_none()),
            _ => panic!("Expected handshake"),
        }
    }

    #[test]
    fn test_encode_decode_request() {
        let msg = DvcProtocolMessage::Request {
            id: "abc123".to_string(),
            command: "snapshot".to_string(),
            params: serde_json::json!({"interactive_only": true}),
        };

        let encoded = AutomationDvc::encode_message(&msg).unwrap();
        let decoded = AutomationDvc::decode_message(&encoded).unwrap();

        match decoded {
            DvcProtocolMessage::Request { id, command, params } => {
                assert_eq!(id, "abc123");
                assert_eq!(command, "snapshot");
                assert_eq!(params["interactive_only"], true);
            }
            _ => panic!("Expected request"),
        }
    }

    #[test]
    fn test_encode_decode_response() {
        let msg = DvcProtocolMessage::Response {
            id: "abc123".to_string(),
            success: true,
            data: Some(serde_json::json!({"result": "ok"})),
            error: None,
        };

        let encoded = AutomationDvc::encode_message(&msg).unwrap();
        let decoded = AutomationDvc::decode_message(&encoded).unwrap();

        match decoded {
            DvcProtocolMessage::Response {
                id,
                success,
                data,
                error,
            } => {
                assert_eq!(id, "abc123");
                assert!(success);
                assert!(data.is_some());
                assert!(error.is_none());
            }
            _ => panic!("Expected response"),
        }
    }
}

/// Two agents can be alive at once now that one survives a transport drop:
/// the survivor and a replacement launched before it reattached. Exactly one
/// must win, and the loser must not take the winner's state with it.
#[cfg(test)]
mod two_agent_tests {
    use super::*;
    use ironrdp_dvc::DvcChannelListener;

    fn handshake_bytes(pid: u32, version: &str) -> Vec<u8> {
        AutomationDvc::encode_message(&DvcProtocolMessage::Handshake {
            version: version.to_string(),
            agent_pid: pid,
            capabilities: vec!["run".to_string()],
            build_id: None,
        })
        .unwrap()
    }

    /// The whole reason for a listener: `with_dynamic_channel` hands its
    /// processor out once per session, so a relaunched agent was refused and
    /// both self-heal and `automate restart` could never reattach.
    #[test]
    fn a_channel_can_be_opened_more_than_once_per_session() {
        let mut listener = AutomationDvcListener::new(new_shared_dvc_state());
        assert!(listener.create(1).is_some());
        assert!(listener.create(2).is_some(), "a relaunched agent must be able to reattach");
        assert_eq!(listener.channel_name(), CHANNEL_NAME);
    }

    #[test]
    fn the_first_agent_to_open_is_the_one_that_is_kept() {
        let state = new_shared_dvc_state();
        let mut first = AutomationDvc::new(Arc::clone(&state));
        let mut second = AutomationDvc::new(Arc::clone(&state));

        first.start(1).unwrap();
        second.start(2).unwrap();
        assert_eq!(state.lock().channel_id, Some(1));
        assert!(state.lock().extras.contains(&2));

        // The kept agent's handshake is recorded...
        first.process(1, &handshake_bytes(100, "1.8.0")).unwrap();
        assert_eq!(state.lock().handshake.as_ref().unwrap().agent_pid, 100);

        // ...and the other one is told to leave, without disturbing it.
        let reply = second.process(2, &handshake_bytes(200, "1.8.0")).unwrap();
        assert_eq!(reply.len(), 1, "the rejected agent gets exactly one message");
        assert_eq!(
            state.lock().handshake.as_ref().unwrap().agent_pid,
            100,
            "the second agent must not take over the first agent's slot"
        );
    }

    /// The message has to be a `shutdown` request the agent's dispatcher
    /// recognises, or the rejected agent stays forever.
    #[test]
    fn the_rejected_agent_is_sent_a_shutdown_request() {
        let raw = AutomationDvc::shutdown_request(7);
        let msg: DvcProtocolMessage = serde_json::from_slice(&raw).unwrap();
        match msg {
            DvcProtocolMessage::Request { command, id, .. } => {
                assert_eq!(command, "shutdown");
                assert_eq!(id, "shutdown-7");
            }
            other => panic!("expected a request, got {other:?}"),
        }
    }

    #[test]
    fn a_rejected_agent_exiting_leaves_the_live_one_alone() {
        let state = new_shared_dvc_state();
        let (tx, mut rx) = mpsc::unbounded_channel();
        state.lock().closed_notify = Some(tx);

        let mut first = AutomationDvc::new(Arc::clone(&state));
        let mut second = AutomationDvc::new(Arc::clone(&state));
        first.start(1).unwrap();
        second.start(2).unwrap();
        first.process(1, &handshake_bytes(100, "1.8.0")).unwrap();

        second.close(2);
        assert!(
            state.lock().handshake.is_some(),
            "closing the rejected channel must not clear the live agent's handshake"
        );
        assert_eq!(state.lock().channel_id, Some(1));
        assert!(rx.try_recv().is_err(), "no relaunch should be triggered");

        // The live agent leaving is still reported.
        first.close(1);
        assert!(state.lock().handshake.is_none());
        assert!(state.lock().channel_id.is_none());
        assert!(rx.try_recv().is_ok(), "the supervisor must hear about the real agent");
    }

    /// Order is by open, not by handshake: a survivor that opened first keeps
    /// the channel even if a freshly launched agent handshakes sooner.
    #[test]
    fn a_late_handshake_does_not_displace_the_earlier_channel() {
        let state = new_shared_dvc_state();
        let mut survivor = AutomationDvc::new(Arc::clone(&state));
        let mut launched = AutomationDvc::new(Arc::clone(&state));

        survivor.start(1).unwrap();
        launched.start(2).unwrap();

        // The launched agent gets there first.
        let reply = launched.process(2, &handshake_bytes(200, "1.8.0")).unwrap();
        assert_eq!(reply.len(), 1, "it opened second, so it is the one asked to leave");
        assert!(state.lock().handshake.is_none());

        survivor.process(1, &handshake_bytes(100, "1.8.0")).unwrap();
        assert_eq!(state.lock().handshake.as_ref().unwrap().agent_pid, 100);
    }

    /// A channel rejected while a primary existed becomes the primary if it
    /// is the only one left by the time it identifies itself - otherwise a
    /// reflexive shutdown would leave nothing connected at all.
    #[test]
    fn an_extra_is_promoted_once_the_primary_is_gone() {
        let state = new_shared_dvc_state();
        let (tx, mut rx) = mpsc::unbounded_channel();
        state.lock().closed_notify = Some(tx);

        let mut first = AutomationDvc::new(Arc::clone(&state));
        let mut second = AutomationDvc::new(Arc::clone(&state));
        first.start(1).unwrap();
        second.start(2).unwrap();
        first.process(1, &handshake_bytes(100, "1.8.0")).unwrap();
        assert!(state.lock().extras.contains(&2));

        // The primary leaves before the second agent has said who it is.
        first.close(1);
        assert!(state.lock().channel_id.is_none());
        assert!(rx.try_recv().is_ok(), "the real agent leaving is reported");

        let reply = second.process(2, &handshake_bytes(200, "1.8.0")).unwrap();
        assert!(reply.is_empty(), "no shutdown for the only agent left");
        let s = state.lock();
        assert_eq!(s.channel_id, Some(2), "promoted");
        assert!(!s.extras.contains(&2));
        assert_eq!(s.handshake.as_ref().unwrap().agent_pid, 200);
    }

    /// A reply on a channel that never went through `start()` - nothing to
    /// route it to, and nothing to panic over.
    #[test]
    fn a_response_on_an_unknown_channel_is_ignored() {
        let state = new_shared_dvc_state();
        let mut dvc = AutomationDvc::new(Arc::clone(&state));
        let (tx, _rx) = oneshot::channel();
        state.lock().pending.insert("real".to_string(), tx);

        let stray = AutomationDvc::encode_message(&DvcProtocolMessage::Response {
            id: "ghost".to_string(),
            success: true,
            data: None,
            error: None,
        })
        .unwrap();
        let out = dvc.process(42, &stray).unwrap();
        assert!(out.is_empty());
        assert!(state.lock().pending.contains_key("real"), "an unrelated pending request is untouched");
        assert!(state.lock().channel_id.is_none(), "a stray reply does not open a channel");
    }
}
