//! Command handlers for RDP operations.

pub mod automate;
pub mod clipboard;
pub mod connect;
pub mod drive;
pub mod file_transfer;
pub mod imaging;
pub mod keyboard;
pub mod locate;
pub mod mouse;
pub mod screenshot;
pub mod scroll;

/// The refusal for a session whose frame processor has already stopped.
///
/// Between the drop and the daemon's deferred teardown the session is still
/// in the slot; `session info` reports it as Disconnected, and the commands
/// that read the framebuffer (`screenshot`, `locate`) must say the same
/// rather than succeed against the last frame the dead transport painted.
pub fn refuse_if_dropped(rdp: &crate::rdp_session::RdpSession) -> Option<agent_rdp_protocol::Response> {
    let reason = rdp.drop_reason()?;
    Some(agent_rdp_protocol::Response::error(
        agent_rdp_protocol::ErrorCode::NotConnected,
        format!(
            "The RDP transport dropped ({}); the daemon is tearing the session down. Run \
             `connect` again.",
            reason
        ),
    ))
}
