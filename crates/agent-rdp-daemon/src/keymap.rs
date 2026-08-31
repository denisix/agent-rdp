//! The single scancode table for named keys.
//!
//! Both the CLI keyboard handler and the WebSocket input path used to carry
//! their own divergent copies of this mapping; a key fixed in one silently
//! stayed broken in the other. Everything key-name related lives here now.

/// Convert a key name to `(scancode, extended_flag)`.
///
/// Names are matched case-insensitively. Covers modifiers, function keys
/// F1-F24, navigation, the number row, letters, punctuation, the numeric
/// keypad and browser-style aliases (`arrowup`, `meta`).
pub fn key_to_scancode(key: &str) -> Option<(u8, bool)> {
    let key_lower = key.to_lowercase();
    lookup(&key_lower)
}

/// Convert a key name to the scancode sequence needed to produce it,
/// expanding shifted symbols.
///
/// `!` is not a key - it is Shift plus the `1` key. Callers that press keys
/// (rather than type text) need the expansion, otherwise every shifted symbol
/// fails with "Unknown key" even though its base key is mapped.
///
/// Returns the keys to hold in order (modifier first).
pub fn key_to_scancode_seq(key: &str) -> Option<Vec<(u8, bool)>> {
    const SHIFT: (u8, bool) = (0x2A, false);

    if let Some(direct) = key_to_scancode(key) {
        return Some(vec![direct]);
    }

    // Shifted symbols on a US layout, mapped to shift + the base scancode.
    let base = match key {
        "!" => "1",
        "@" => "2",
        "#" => "3",
        "$" => "4",
        "%" => "5",
        "^" => "6",
        "&" => "7",
        "*" => "8",
        "(" => "9",
        ")" => "0",
        "_" => "-",
        "+" => "=",
        "{" => "[",
        "}" => "]",
        "|" => "\\",
        ":" => ";",
        "\"" => "'",
        "<" => ",",
        ">" => ".",
        "?" => "/",
        "~" => "`",
        _ => return None,
    };

    lookup(base).map(|sc| vec![SHIFT, sc])
}

/// Convert a browser `KeyboardEvent.code` (e.g. `KeyA`, `ArrowUp`) to
/// `(scancode, extended_flag)`. Used by the WebSocket viewer input path.
pub fn code_to_scancode(code: &str) -> Option<(u8, bool)> {
    let mapped = match code {
        "KeyA" => "a",
        "KeyB" => "b",
        "KeyC" => "c",
        "KeyD" => "d",
        "KeyE" => "e",
        "KeyF" => "f",
        "KeyG" => "g",
        "KeyH" => "h",
        "KeyI" => "i",
        "KeyJ" => "j",
        "KeyK" => "k",
        "KeyL" => "l",
        "KeyM" => "m",
        "KeyN" => "n",
        "KeyO" => "o",
        "KeyP" => "p",
        "KeyQ" => "q",
        "KeyR" => "r",
        "KeyS" => "s",
        "KeyT" => "t",
        "KeyU" => "u",
        "KeyV" => "v",
        "KeyW" => "w",
        "KeyX" => "x",
        "KeyY" => "y",
        "KeyZ" => "z",
        "Digit0" => "0",
        "Digit1" => "1",
        "Digit2" => "2",
        "Digit3" => "3",
        "Digit4" => "4",
        "Digit5" => "5",
        "Digit6" => "6",
        "Digit7" => "7",
        "Digit8" => "8",
        "Digit9" => "9",
        "F1" => "f1",
        "F2" => "f2",
        "F3" => "f3",
        "F4" => "f4",
        "F5" => "f5",
        "F6" => "f6",
        "F7" => "f7",
        "F8" => "f8",
        "F9" => "f9",
        "F10" => "f10",
        "F11" => "f11",
        "F12" => "f12",
        "ShiftLeft" => "lshift",
        "ShiftRight" => "rshift",
        "ControlLeft" => "lctrl",
        "ControlRight" => "rctrl",
        "AltLeft" => "lalt",
        "AltRight" => "ralt",
        "MetaLeft" => "lwin",
        "MetaRight" => "rwin",
        "Escape" => "esc",
        "Tab" => "tab",
        "Enter" => "enter",
        "Backspace" => "backspace",
        "Space" => "space",
        "CapsLock" => "capslock",
        "ArrowUp" => "up",
        "ArrowDown" => "down",
        "ArrowLeft" => "left",
        "ArrowRight" => "right",
        "Insert" => "insert",
        "Delete" => "delete",
        "Home" => "home",
        "End" => "end",
        "PageUp" => "pageup",
        "PageDown" => "pagedown",
        "Minus" => "-",
        "Equal" => "=",
        "BracketLeft" => "[",
        "BracketRight" => "]",
        "Backslash" => "\\",
        "Semicolon" => ";",
        "Quote" => "'",
        "Backquote" => "`",
        "Comma" => ",",
        "Period" => ".",
        "Slash" => "/",
        "Numpad0" => "numpad0",
        "Numpad1" => "numpad1",
        "Numpad2" => "numpad2",
        "Numpad3" => "numpad3",
        "Numpad4" => "numpad4",
        "Numpad5" => "numpad5",
        "Numpad6" => "numpad6",
        "Numpad7" => "numpad7",
        "Numpad8" => "numpad8",
        "Numpad9" => "numpad9",
        "NumpadEnter" => "numpadenter",
        "NumpadAdd" => "numpadplus",
        "NumpadSubtract" => "numpadminus",
        "NumpadMultiply" => "numpadmultiply",
        "NumpadDivide" => "numpaddivide",
        "NumLock" => "numlock",
        "ContextMenu" => "apps",
        "PrintScreen" => "printscreen",
        "ScrollLock" => "scrolllock",
        "Pause" => "pause",
        _ => return None,
    };
    lookup(mapped)
}

/// The actual table. Kept as one match (not a HashMap) so lookups allocate
/// nothing and the compiler checks for duplicate keys.
fn lookup(key: &str) -> Option<(u8, bool)> {
    let entry = match key {
        // Modifier keys
        "ctrl" | "control" | "lctrl" => (0x1D, false),
        "rctrl" => (0x1D, true),
        "alt" | "lalt" => (0x38, false),
        "ralt" => (0x38, true),
        "shift" | "lshift" => (0x2A, false),
        "rshift" => (0x36, false),
        "win" | "windows" | "lwin" | "super" | "meta" => (0x5B, true),
        "rwin" => (0x5C, true),

        // Function keys
        "esc" | "escape" => (0x01, false),
        "f1" => (0x3B, false),
        "f2" => (0x3C, false),
        "f3" => (0x3D, false),
        "f4" => (0x3E, false),
        "f5" => (0x3F, false),
        "f6" => (0x40, false),
        "f7" => (0x41, false),
        "f8" => (0x42, false),
        "f9" => (0x43, false),
        "f10" => (0x44, false),
        "f11" => (0x57, false),
        "f12" => (0x58, false),
        // F13-F24 exist on extended keyboards and as app shortcuts.
        "f13" => (0x64, false),
        "f14" => (0x65, false),
        "f15" => (0x66, false),
        "f16" => (0x67, false),
        "f17" => (0x68, false),
        "f18" => (0x69, false),
        "f19" => (0x6A, false),
        "f20" => (0x6B, false),
        "f21" => (0x6C, false),
        "f22" => (0x6D, false),
        "f23" => (0x6E, false),
        "f24" => (0x76, false),

        // Navigation keys
        "tab" => (0x0F, false),
        "enter" | "return" => (0x1C, false),
        "backspace" => (0x0E, false),
        "space" | " " => (0x39, false),
        "capslock" | "caps" => (0x3A, false),

        // Arrow keys (extended), including browser-style aliases
        "up" | "arrowup" => (0x48, true),
        "down" | "arrowdown" => (0x50, true),
        "left" | "arrowleft" => (0x4B, true),
        "right" | "arrowright" => (0x4D, true),

        // Other navigation (extended)
        "insert" => (0x52, true),
        "delete" => (0x53, true),
        "home" => (0x47, true),
        "end" => (0x4F, true),
        "pageup" | "pgup" => (0x49, true),
        "pagedown" | "pgdn" => (0x51, true),

        // The context-menu key next to right Ctrl.
        "apps" | "menu" | "contextmenu" => (0x5D, true),

        // Printscreen/scroll/pause. E0 37 is the standard RDP encoding for
        // PrtSc; the E0 2A prefix real hardware sends is a compatibility
        // artefact servers do not require.
        "printscreen" | "prtsc" => (0x37, true),
        "scrolllock" => (0x46, false),
        "pause" | "break" => (0x45, false),

        // Number row
        "1" => (0x02, false),
        "2" => (0x03, false),
        "3" => (0x04, false),
        "4" => (0x05, false),
        "5" => (0x06, false),
        "6" => (0x07, false),
        "7" => (0x08, false),
        "8" => (0x09, false),
        "9" => (0x0A, false),
        "0" => (0x0B, false),

        // Letters
        "a" => (0x1E, false),
        "b" => (0x30, false),
        "c" => (0x2E, false),
        "d" => (0x20, false),
        "e" => (0x12, false),
        "f" => (0x21, false),
        "g" => (0x22, false),
        "h" => (0x23, false),
        "i" => (0x17, false),
        "j" => (0x24, false),
        "k" => (0x25, false),
        "l" => (0x26, false),
        "m" => (0x32, false),
        "n" => (0x31, false),
        "o" => (0x18, false),
        "p" => (0x19, false),
        "q" => (0x10, false),
        "r" => (0x13, false),
        "s" => (0x1F, false),
        "t" => (0x14, false),
        "u" => (0x16, false),
        "v" => (0x2F, false),
        "w" => (0x11, false),
        "x" => (0x2D, false),
        "y" => (0x15, false),
        "z" => (0x2C, false),

        // Punctuation (unshifted)
        "minus" | "-" => (0x0C, false),
        "equals" | "=" => (0x0D, false),
        "leftbracket" | "[" => (0x1A, false),
        "rightbracket" | "]" => (0x1B, false),
        "backslash" | "\\" => (0x2B, false),
        "semicolon" | ";" => (0x27, false),
        "quote" | "'" => (0x28, false),
        "grave" | "`" => (0x29, false),
        "comma" | "," => (0x33, false),
        "period" | "." => (0x34, false),
        "slash" | "/" => (0x35, false),

        // Numeric keypad. NumpadEnter and NumpadDivide are the extended
        // variants of their main-block keys.
        "numpad0" => (0x52, false),
        "numpad1" => (0x4F, false),
        "numpad2" => (0x50, false),
        "numpad3" => (0x51, false),
        "numpad4" => (0x4B, false),
        "numpad5" => (0x4C, false),
        "numpad6" => (0x4D, false),
        "numpad7" => (0x47, false),
        "numpad8" => (0x48, false),
        "numpad9" => (0x49, false),
        "numpadenter" => (0x1C, true),
        "numpadplus" | "numpadadd" => (0x4E, false),
        "numpadminus" | "numpadsubtract" => (0x4A, false),
        "numpadmultiply" => (0x37, false),
        "numpaddivide" => (0x35, true),
        "numlock" => (0x45, false),

        _ => return None,
    };
    Some(entry)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_names_positive() {
        assert_eq!(key_to_scancode("enter"), Some((0x1C, false)));
        assert_eq!(key_to_scancode("ENTER"), Some((0x1C, false)));
        assert_eq!(key_to_scancode("ctrl"), Some((0x1D, false)));
        assert_eq!(key_to_scancode("up"), Some((0x48, true)));
        assert_eq!(key_to_scancode("win"), Some((0x5B, true)));
    }

    #[test]
    fn test_new_keys_positive() {
        assert_eq!(key_to_scancode("numpad0"), Some((0x52, false)));
        assert_eq!(key_to_scancode("numpadenter"), Some((0x1C, true)));
        assert_eq!(key_to_scancode("numpaddivide"), Some((0x35, true)));
        assert_eq!(key_to_scancode("numlock"), Some((0x45, false)));
        assert_eq!(key_to_scancode("apps"), Some((0x5D, true)));
        assert_eq!(key_to_scancode("menu"), Some((0x5D, true)));
        assert_eq!(key_to_scancode("f13"), Some((0x64, false)));
        assert_eq!(key_to_scancode("f24"), Some((0x76, false)));
    }

    #[test]
    fn test_numpad_is_distinct_from_main_block() {
        // Same base scancodes as navigation keys, but WITHOUT the extended
        // flag - that flag is exactly what distinguishes Home from Numpad7.
        assert_eq!(key_to_scancode("home"), Some((0x47, true)));
        assert_eq!(key_to_scancode("numpad7"), Some((0x47, false)));
        assert_eq!(key_to_scancode("slash"), Some((0x35, false)));
        assert_eq!(key_to_scancode("numpaddivide"), Some((0x35, true)));
    }

    #[test]
    fn test_unknown_names_negative() {
        assert_eq!(key_to_scancode("f25"), None);
        assert_eq!(key_to_scancode("numpad10"), None);
        assert_eq!(key_to_scancode("hyper"), None);
        assert_eq!(key_to_scancode(""), None);
    }

    #[test]
    fn test_shifted_symbols_expand_to_shift_plus_base() {
        assert_eq!(key_to_scancode_seq("!"), Some(vec![(0x2A, false), (0x02, false)]));
        assert_eq!(key_to_scancode_seq("+"), Some(vec![(0x2A, false), (0x0D, false)]));
        assert_eq!(key_to_scancode_seq("?"), Some(vec![(0x2A, false), (0x35, false)]));
        assert_eq!(key_to_scancode_seq("\""), Some(vec![(0x2A, false), (0x28, false)]));
        assert_eq!(key_to_scancode_seq("~"), Some(vec![(0x2A, false), (0x29, false)]));
    }

    #[test]
    fn test_seq_passes_plain_keys_through() {
        assert_eq!(key_to_scancode_seq("enter"), Some(vec![(0x1C, false)]));
        assert_eq!(key_to_scancode_seq("nosuchkey"), None);
    }

    #[test]
    fn test_browser_codes() {
        assert_eq!(code_to_scancode("KeyA"), Some((0x1E, false)));
        assert_eq!(code_to_scancode("ArrowUp"), Some((0x48, true)));
        assert_eq!(code_to_scancode("ControlRight"), Some((0x1D, true)));
        assert_eq!(code_to_scancode("NumpadEnter"), Some((0x1C, true)));
        assert_eq!(code_to_scancode("ContextMenu"), Some((0x5D, true)));
        assert_eq!(code_to_scancode("NoSuchCode"), None);
        // Codes are case-sensitive by spec; a lowercased code is not a code.
        assert_eq!(code_to_scancode("keya"), None);
    }
}
