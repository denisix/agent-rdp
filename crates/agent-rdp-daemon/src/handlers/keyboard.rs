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

    // One lock for the whole combination. Releasing it between the down and
    // the up let another handler - a paste, the agent bootstrap's Win+R, the
    // viewer - interleave its own input, which leaves a modifier held and
    // turns every following key into an unhandled chord the application
    // silently discards.
    if let KeyboardRequest::Press { ref keys } = action {
        info!("Pressing key combination: {}", keys);
        let key_infos = match parse_key_combination(keys) {
            Ok(infos) => infos,
            Err(e) => {
                return Response::error(ErrorCode::InvalidRequest, e);
            }
        };

        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
            }
        };
        return match press_combination(rdp, &key_infos).await {
            Ok(()) => Response::ok(),
            Err(e) => Response::error(ErrorCode::InternalError, e.to_string()),
        };
    }

    // A whole sequence under one lock, for the same reason, and because the
    // alternative - one CLI process per key - puts half a second between
    // presses, which is useless for anything that moves while you wait.
    if let KeyboardRequest::PressSeq { ref keys, interval_ms } = action {
        if let Err(e) = agent_rdp_protocol::validate_keyboard_request(&action) {
            return Response::error(ErrorCode::InvalidRequest, e);
        }
        // Parse every key before sending any: a typo in the seventh key
        // must not leave the first six typed.
        let mut parsed = Vec::with_capacity(keys.len());
        for keys_text in keys {
            match parse_key_combination(keys_text) {
                Ok(infos) => parsed.push(infos),
                Err(e) => {
                    return Response::error(
                        ErrorCode::InvalidRequest,
                        format!("{} (nothing was sent)", e),
                    );
                }
            }
        }
        let interval = Duration::from_millis(
            interval_ms.unwrap_or(agent_rdp_protocol::DEFAULT_PRESS_SEQ_INTERVAL_MS),
        );
        info!("Pressing a sequence of {} key combinations", parsed.len());

        let session = rdp_session.lock().await;
        let rdp = match session.as_ref() {
            Some(rdp) => rdp,
            None => {
                return Response::error(ErrorCode::NotConnected, "Not connected to an RDP server");
            }
        };
        for (i, key_infos) in parsed.iter().enumerate() {
            if i > 0 {
                sleep(interval).await;
            }
            if let Err(e) = press_combination(rdp, key_infos).await {
                // Say how far it got: the caller has to know which keys
                // landed before deciding what to do next.
                return Response::error(
                    ErrorCode::InternalError,
                    format!("{} (sent {} of {} keys)", e, i, parsed.len()),
                );
            }
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
        KeyboardRequest::Type { .. }
        | KeyboardRequest::Press { .. }
        | KeyboardRequest::PressSeq { .. }
        | KeyboardRequest::Paste { .. } => {
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
/// Press and release one combination on an already-held session.
///
/// Down in order, up in reverse, with the same small gaps as before. The
/// caller holds the session lock for the whole call, so nothing can
/// interleave between the down and the up.
/// Whole-loop budget for releasing a partially pressed combination.
const RELEASE_BUDGET: Duration = Duration::from_secs(3);

async fn press_combination(
    rdp: &RdpSession,
    key_infos: &[KeyInfo],
) -> Result<(), crate::rdp_session::RdpError> {
    // How many key-downs actually reached the transport. A modifier that
    // went down and never came up stays logically held on the remote
    // desktop, turning every later keystroke from any caller into a chord -
    // so a failure part-way through releases what it pressed before it
    // reports. Now that input is acknowledged, this failure is visible for
    // the first time, which is also what makes leaving it unhandled a real
    // defect rather than a theoretical one.
    let mut down = 0usize;
    let mut first_error = None;
    for info in key_infos {
        if let Err(e) = rdp
            .send_input(vec![create_key_event_ext(info.scancode, info.extended, false)])
            .await
        {
            first_error = Some(e);
            break;
        }
        down += 1;
        sleep(Duration::from_millis(10)).await;
    }
    if first_error.is_none() {
        sleep(Duration::from_millis(50)).await;
    }
    // Best effort, in reverse order, for exactly the keys that went down.
    // A release that fails too is not worth reporting over the first error:
    // the transport is gone either way, and so is the remote key state.
    for info in key_infos[..down].iter().rev() {
        if let Err(e) = rdp
            .send_input(vec![create_key_event_ext(info.scancode, info.extended, true)])
            .await
        {
            if first_error.is_none() {
                first_error = Some(e);
            }
        }
        sleep(Duration::from_millis(10)).await;
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

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
    /// A key-down that lands and a key-up that does not leaves the modifier
    /// held on the remote desktop, turning every later keystroke from any
    /// caller into a chord. Acknowledging input is what made this failure
    /// visible; handling it is what makes the acknowledgement safe.
    #[test]
    fn a_failed_combination_releases_what_it_pressed() {
        let source = crate::automation::lf(include_str!("keyboard.rs"));
        let at = source.find("async fn press_combination(").expect("press_combination");
        let body = &source[at..];
        let end = body.find("\nfn create_key_event_ext(").expect("the next item");
        let body = &body[..end];

        assert!(
            !body.contains("false)])\n            .await?;"),
            "a key-down failure must not skip the releases"
        );
        assert!(
            body.contains("key_infos[..down].iter().rev()"),
            "release exactly the keys that went down, in reverse"
        );
        assert!(
            body.contains("first_error"),
            "and still report the failure that started it"
        );
    }

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
