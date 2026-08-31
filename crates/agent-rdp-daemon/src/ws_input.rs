//! WebSocket input translation to RDP FastPath events.
//!
//! Translates input messages from the WebSocket viewer (matching agent-browser protocol)
//! to RDP FastPathInputEvent format.

use ironrdp::pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp::pdu::input::mouse::{MousePdu, PointerFlags};
use serde::Deserialize;

/// Mouse input message from WebSocket client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename = "input_mouse")]
pub struct MouseInputMessage {
    #[serde(rename = "eventType")]
    pub event_type: String,
    pub x: u16,
    pub y: u16,
    #[serde(default)]
    pub button: Option<String>,
    #[serde(rename = "deltaX", default)]
    pub delta_x: Option<i32>,
    #[serde(rename = "deltaY", default)]
    pub delta_y: Option<i32>,
}

/// Keyboard input message from WebSocket client.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename = "input_keyboard")]
pub struct KeyboardInputMessage {
    #[serde(rename = "eventType")]
    pub event_type: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// Clipboard content payload.
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardContent {
    pub content_type: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// Clipboard get request payload.
#[derive(Debug, Deserialize)]
pub struct ClipboardGetPayload {
    #[serde(default)]
    pub formats: Vec<String>,
}

/// Clipboard set payload (client setting clipboard before paste).
#[derive(Debug, Deserialize)]
pub struct ClipboardSetPayload {
    pub text: String,
}

/// Generic WebSocket input message (for dispatching).
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum WsInputMessage {
    #[serde(rename = "input_mouse")]
    Mouse(MouseInputPayload),
    #[serde(rename = "input_keyboard")]
    Keyboard(KeyboardInputPayload),
    #[serde(rename = "clipboard_get")]
    ClipboardGet(ClipboardGetPayload),
    #[serde(rename = "clipboard_set")]
    ClipboardSet(ClipboardSetPayload),
}

/// Mouse input payload (fields only, without type tag).
#[derive(Debug, Deserialize)]
pub struct MouseInputPayload {
    #[serde(rename = "eventType")]
    pub event_type: String,
    pub x: u16,
    pub y: u16,
    #[serde(default)]
    pub button: Option<String>,
    #[serde(rename = "deltaX", default)]
    pub delta_x: Option<i32>,
    #[serde(rename = "deltaY", default)]
    pub delta_y: Option<i32>,
}

/// Keyboard input payload (fields only, without type tag).
#[derive(Debug, Deserialize)]
pub struct KeyboardInputPayload {
    #[serde(rename = "eventType")]
    pub event_type: String,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
}

/// Convert a mouse input message to FastPath events.
pub fn mouse_to_fastpath(msg: &MouseInputPayload) -> Vec<FastPathInputEvent> {
    match msg.event_type.as_str() {
        "mousePressed" => {
            let button_flags = button_str_to_flags(msg.button.as_deref());
            vec![create_mouse_event(
                msg.x,
                msg.y,
                button_flags | PointerFlags::DOWN,
            )]
        }
        "mouseReleased" => {
            let button_flags = button_str_to_flags(msg.button.as_deref());
            // Release is sent WITHOUT the DOWN flag
            vec![create_mouse_event(msg.x, msg.y, button_flags)]
        }
        "mouseMoved" => {
            vec![create_mouse_event(msg.x, msg.y, PointerFlags::MOVE)]
        }
        "mouseWheel" => {
            // Handle vertical scroll
            if let Some(delta_y) = msg.delta_y {
                // RDP wheel rotation: positive = scroll up, negative = scroll down
                // The delta is typically in pixels, we need to convert to wheel units
                // Standard wheel rotation is 120 units per notch
                let wheel_delta = if delta_y < 0 {
                    // Scroll down (towards user) - negative delta in browser
                    -((-delta_y).min(32767) as i16)
                } else {
                    // Scroll up (away from user) - positive delta in browser
                    delta_y.min(32767) as i16
                };

                vec![FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::VERTICAL_WHEEL,
                    number_of_wheel_rotation_units: wheel_delta,
                    x_position: msg.x,
                    y_position: msg.y,
                })]
            } else if let Some(delta_x) = msg.delta_x {
                // Horizontal scroll
                let wheel_delta = if delta_x < 0 {
                    -((-delta_x).min(32767) as i16)
                } else {
                    delta_x.min(32767) as i16
                };

                vec![FastPathInputEvent::MouseEvent(MousePdu {
                    flags: PointerFlags::HORIZONTAL_WHEEL,
                    number_of_wheel_rotation_units: wheel_delta,
                    x_position: msg.x,
                    y_position: msg.y,
                })]
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Convert a keyboard input message to FastPath events.
pub fn keyboard_to_fastpath(msg: &KeyboardInputPayload) -> Vec<FastPathInputEvent> {
    match msg.event_type.as_str() {
        "keyDown" => {
            // Try to get scancode from key or code
            if let Some((scancode, extended)) = get_scancode_from_message(msg) {
                vec![create_key_event(scancode, extended, false)]
            } else {
                vec![]
            }
        }
        "keyUp" => {
            if let Some((scancode, extended)) = get_scancode_from_message(msg) {
                vec![create_key_event(scancode, extended, true)]
            } else {
                vec![]
            }
        }
        "char" => {
            // Send unicode character
            if let Some(text) = &msg.text {
                text.chars()
                    .flat_map(|ch| {
                        let code = ch as u16;
                        vec![
                            FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::empty(), code),
                            FastPathInputEvent::UnicodeKeyboardEvent(KeyboardFlags::RELEASE, code),
                        ]
                    })
                    .collect()
            } else {
                vec![]
            }
        }
        _ => vec![],
    }
}

/// Try to get scancode from the message's key or code fields.
fn get_scancode_from_message(msg: &KeyboardInputPayload) -> Option<(u8, bool)> {
    // First try the code field (e.g., "KeyA", "ArrowUp")
    if let Some(code) = &msg.code {
        if let Some(result) = code_to_scancode(code) {
            return Some(result);
        }
    }

    // Fall back to key field (e.g., "a", "Enter")
    if let Some(key) = &msg.key {
        if let Some(result) = key_to_scancode(key) {
            return Some(result);
        }
    }

    None
}

/// Convert button string to PointerFlags.
fn button_str_to_flags(button: Option<&str>) -> PointerFlags {
    match button {
        Some("left") | None => PointerFlags::LEFT_BUTTON,
        Some("right") => PointerFlags::RIGHT_BUTTON,
        Some("middle") => PointerFlags::MIDDLE_BUTTON_OR_WHEEL,
        _ => PointerFlags::LEFT_BUTTON,
    }
}

/// Create a mouse event.
fn create_mouse_event(x: u16, y: u16, flags: PointerFlags) -> FastPathInputEvent {
    FastPathInputEvent::MouseEvent(MousePdu {
        flags,
        number_of_wheel_rotation_units: 0,
        x_position: x,
        y_position: y,
    })
}

/// Create a keyboard event.
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

/// Convert JavaScript KeyboardEvent.code to scancode.
///
/// Delegates to `crate::keymap`, the single scancode table shared with the
/// CLI keyboard handler - this file used to carry two of its own,
/// independently incomplete copies.
/// See: https://developer.mozilla.org/en-US/docs/Web/API/KeyboardEvent/code/code_values
fn code_to_scancode(code: &str) -> Option<(u8, bool)> {
    crate::keymap::code_to_scancode(code)
}

/// Convert a JavaScript KeyboardEvent.key value to scancode.
fn key_to_scancode(key: &str) -> Option<(u8, bool)> {
    crate::keymap::key_to_scancode(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mouse_pressed() {
        let msg = MouseInputPayload {
            event_type: "mousePressed".to_string(),
            x: 100,
            y: 200,
            button: Some("left".to_string()),
            delta_x: None,
            delta_y: None,
        };
        let events = mouse_to_fastpath(&msg);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_mouse_wheel() {
        let msg = MouseInputPayload {
            event_type: "mouseWheel".to_string(),
            x: 100,
            y: 200,
            button: None,
            delta_x: None,
            delta_y: Some(-120),
        };
        let events = mouse_to_fastpath(&msg);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_keyboard_key_down() {
        let msg = KeyboardInputPayload {
            event_type: "keyDown".to_string(),
            key: Some("a".to_string()),
            code: Some("KeyA".to_string()),
            text: None,
        };
        let events = keyboard_to_fastpath(&msg);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn test_keyboard_char() {
        let msg = KeyboardInputPayload {
            event_type: "char".to_string(),
            key: None,
            code: None,
            text: Some("abc".to_string()),
        };
        let events = keyboard_to_fastpath(&msg);
        // 3 chars × 2 events (down + up) = 6 events
        assert_eq!(events.len(), 6);
    }

    #[test]
    fn test_code_to_scancode() {
        assert_eq!(code_to_scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(code_to_scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(code_to_scancode("ControlRight"), Some((0x1D, true)));
    }
}
