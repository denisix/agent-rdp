//! Keyboard input handler.

use std::sync::Arc;

use agent_rdp_protocol::{ErrorCode, KeyboardRequest, Response};
use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tracing::info;

use crate::keymap::key_to_scancode_seq;
use crate::rdp_session::RdpSession;

/// Handle a keyboard request.
pub async fn handle(
    rdp_session: &Arc<Mutex<Option<RdpSession>>>,
    action: KeyboardRequest,
) -> Response {
    // Typing is batched into as few PDUs as possible; sending (and sleeping)
    // per character made even short strings take tens of seconds.
    if let KeyboardRequest::Type {
        ref text,
        delay_ms,
    } = action
    {
        info!("Typing {} characters", text.chars().count());

        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
            }
        };

        return match rdp.send_text(text, delay_ms).await {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(ErrorCode::InternalError, e.to_string()),
        };
    }

    if let KeyboardRequest::Paste { text } = action {
        info!("Pasting {} characters via clipboard", text.chars().count());
        return handle_paste(rdp_session, text).await;
    }

    // For key combinations, release lock between each key event
    if let KeyboardRequest::Press { ref keys } = action {
        info!("Pressing key combination: {}", keys);
        let key_infos = match parse_key_combination(keys) {
            Ok(infos) => infos,
            Err(e) => {
                return Response::error(ErrorCode::InvalidRequest, e);
            }
        };

        // Press all keys down
        for info in &key_infos {
            let event = create_key_event_ext(info.scancode, info.extended, false);
            {
                let session = rdp_session.lock().await;
                let rdp = match session.as_ref() {
                    Some(rdp) => rdp,
                    None => {
                        return Response::error(
                            ErrorCode::NotConnected,
                            "Not connected to an RDP server",
                        );
                    }
                };
                if let Err(e) = rdp.send_input(vec![event]).await {
                    return Response::error(ErrorCode::InternalError, e.to_string());
                }
            }
            sleep(Duration::from_millis(10)).await;
        }

        // Small delay before releasing
        sleep(Duration::from_millis(50)).await;

        // Release all keys in reverse order
        for info in key_infos.iter().rev() {
            let event = create_key_event_ext(info.scancode, info.extended, true);
            {
                let session = rdp_session.lock().await;
                let rdp = match session.as_ref() {
                    Some(rdp) => rdp,
                    None => {
                        return Response::error(
                            ErrorCode::NotConnected,
                            "Not connected to an RDP server",
                        );
                    }
                };
                if let Err(e) = rdp.send_input(vec![event]).await {
                    return Response::error(ErrorCode::InternalError, e.to_string());
                }
            }
            sleep(Duration::from_millis(10)).await;
        }

        return Response::ok();
    }

    // For single key operations (KeyDown/KeyUp), use a scoped lock
    let session = rdp_session.lock().await;
    let rdp = match session.as_ref() {
        Some(rdp) => rdp,
        None => {
            return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
        }
    };

    let events = match action {
        KeyboardRequest::Type { .. } | KeyboardRequest::Press { .. } | KeyboardRequest::Paste { .. } => {
            // Handled above
            unreachable!()
        }

        KeyboardRequest::KeyDown { key } => match key_to_scancode_seq(&key) {
            Some(seq) => seq
                .into_iter()
                .map(|(sc, ext)| create_key_event_ext(sc, ext, false))
                .collect(),
            None => {
                return Response::error(
                    ErrorCode::InvalidRequest,
                    format!("Unknown key: {}", key),
                );
            }
        },

        KeyboardRequest::KeyUp { key } => match key_to_scancode_seq(&key) {
            // Release in reverse order of press, so a shifted symbol releases
            // the base key before the modifier.
            Some(mut seq) => {
                seq.reverse();
                seq.into_iter()
                    .map(|(sc, ext)| create_key_event_ext(sc, ext, true))
                    .collect()
            }
            None => {
                return Response::error(
                    ErrorCode::InvalidRequest,
                    format!("Unknown key: {}", key),
                );
            }
        },
    };

    match rdp.send_input(events).await {
        Ok(()) => Response::ok(),
        Err(e) => Response::error(ErrorCode::InternalError, e.to_string()),
    }
}

/// Set the clipboard to `text` and paste it with Ctrl+V, as one command.
///
/// Two separate CLI calls (`clipboard set` + `keyboard press ctrl+v`) leave a
/// window where focus can move between them; doing both under one lock
/// acquisition removes that race. This is also the reliable path for long or
/// non-Latin text: it cannot lose individual keystrokes the way `keyboard
/// type` can under a slow or busy remote app.
async fn handle_paste(rdp_session: &Arc<Mutex<Option<RdpSession>>>, text: String) -> Response {
    let session = rdp_session.lock().await;
    let rdp = match session.as_ref() {
        Some(rdp) => rdp,
        None => {
            return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
        }
    };

    if let Err(e) = rdp.clipboard_set(text).await {
        return Response::error(ErrorCode::ClipboardError, format!("Failed to set clipboard: {}", e));
    }

    let ctrl = create_key_event_ext(0x1D, false, false);
    let v_down = create_key_event_ext(0x2F, false, false);
    let v_up = create_key_event_ext(0x2F, false, true);
    let ctrl_up = create_key_event_ext(0x1D, false, true);

    if let Err(e) = rdp.send_input(vec![ctrl, v_down]).await {
        return Response::error(ErrorCode::InternalError, e.to_string());
    }
    sleep(Duration::from_millis(10)).await;
    if let Err(e) = rdp.send_input(vec![v_up, ctrl_up]).await {
        return Response::error(ErrorCode::InternalError, e.to_string());
    }

    Response::ok()
}

/// Parse a key combination like "ctrl+c" into key info for sending.
fn parse_key_combination(keys: &str) -> Result<Vec<KeyInfo>, String> {
    let parts: Vec<String> = keys.split('+').map(|s| s.trim().to_lowercase()).collect();

    let mut key_infos = Vec::new();

    for key in &parts {
        let seq = key_to_scancode_seq(key).ok_or_else(|| format!("Unknown key: {}", key))?;
        for (scancode, extended) in seq {
            key_infos.push(KeyInfo { scancode, extended });
        }
    }

    Ok(key_infos)
}

/// Key information including scancode and extended flag.
struct KeyInfo {
    scancode: u8,
    extended: bool,
}

/// Create a keyboard event with proper flags.
fn create_key_event_ext(scancode: u8, extended: bool, release: bool) -> FastPathInputEvent {
    let mut flags = KeyboardFlags::empty();
    if release {
        flags |= KeyboardFlags::RELEASE;
    }
    if extended {
        flags |= KeyboardFlags::EXTENDED;
    }
    FastPathInputEvent::KeyboardEvent(flags, scancode)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_key_combination_basic() {
        let key_infos = parse_key_combination("ctrl+c").unwrap();
        assert_eq!(key_infos.len(), 2);
        assert_eq!(key_infos[0].scancode, 0x1D); // ctrl
        assert_eq!(key_infos[1].scancode, 0x2E); // c
    }

    #[test]
    fn test_parse_key_combination_unknown_key() {
        assert!(parse_key_combination("ctrl+nosuchkey").is_err());
    }

    #[test]
    fn test_parse_key_combination_expands_shifted_symbol() {
        // "ctrl+!" must expand to ctrl, shift, 1 - not fail as "unknown key !".
        let key_infos = parse_key_combination("ctrl+!").unwrap();
        assert_eq!(key_infos.len(), 3);
        assert_eq!(key_infos[0].scancode, 0x1D); // ctrl
        assert_eq!(key_infos[1].scancode, 0x2A); // shift
        assert_eq!(key_infos[2].scancode, 0x02); // 1
    }

    #[test]
    fn test_parse_key_combination_single_key() {
        let key_infos = parse_key_combination("enter").unwrap();
        assert_eq!(key_infos.len(), 1);
        assert_eq!(key_infos[0].scancode, 0x1C);
    }
}
