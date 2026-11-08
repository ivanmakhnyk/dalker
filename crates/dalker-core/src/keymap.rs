//! Physical key mapping from DOM `KeyboardEvent.code` to OS key identifiers.
//!
//! * `evdev`: Linux `KEY_*` code from `linux/input-event-codes.h`.
//! * `win_scan`: Windows PC/AT set-1 scancode for `SendInput(KEYEVENTF_SCANCODE)`.
//! * `win_extended`: the scancode needs `KEYEVENTF_EXTENDEDKEY` (the `E0` prefix).
//!
//! Using physical codes (not characters) keeps the host keyboard layout in charge,
//! exactly as if a USB keyboard were plugged into the host.

/// OS identifiers for a single physical key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyCodes {
    pub evdev: u16,
    pub win_scan: u16,
    pub win_extended: bool,
}

const fn k(evdev: u16, win_scan: u16, win_extended: bool) -> KeyCodes {
    KeyCodes {
        evdev,
        win_scan,
        win_extended,
    }
}

/// Windows virtual-key for `Pause`, which has no usable single scancode
/// (it is the `E1 1D 45` sequence). Injectors special-case it.
pub const WIN_VK_PAUSE: u16 = 0x13;

/// Looks up the OS codes for a DOM `KeyboardEvent.code` value.
pub fn lookup(code: &str) -> Option<KeyCodes> {
    Some(match code {
        "Escape" => k(1, 0x01, false),
        "Digit1" => k(2, 0x02, false),
        "Digit2" => k(3, 0x03, false),
        "Digit3" => k(4, 0x04, false),
        "Digit4" => k(5, 0x05, false),
        "Digit5" => k(6, 0x06, false),
        "Digit6" => k(7, 0x07, false),
        "Digit7" => k(8, 0x08, false),
        "Digit8" => k(9, 0x09, false),
        "Digit9" => k(10, 0x0A, false),
        "Digit0" => k(11, 0x0B, false),
        "Minus" => k(12, 0x0C, false),
        "Equal" => k(13, 0x0D, false),
        "Backspace" => k(14, 0x0E, false),
        "Tab" => k(15, 0x0F, false),
        "KeyQ" => k(16, 0x10, false),
        "KeyW" => k(17, 0x11, false),
        "KeyE" => k(18, 0x12, false),
        "KeyR" => k(19, 0x13, false),
        "KeyT" => k(20, 0x14, false),
        "KeyY" => k(21, 0x15, false),
        "KeyU" => k(22, 0x16, false),
        "KeyI" => k(23, 0x17, false),
        "KeyO" => k(24, 0x18, false),
        "KeyP" => k(25, 0x19, false),
        "BracketLeft" => k(26, 0x1A, false),
        "BracketRight" => k(27, 0x1B, false),
        "Enter" => k(28, 0x1C, false),
        "ControlLeft" => k(29, 0x1D, false),
        "KeyA" => k(30, 0x1E, false),
        "KeyS" => k(31, 0x1F, false),
        "KeyD" => k(32, 0x20, false),
        "KeyF" => k(33, 0x21, false),
        "KeyG" => k(34, 0x22, false),
        "KeyH" => k(35, 0x23, false),
        "KeyJ" => k(36, 0x24, false),
        "KeyK" => k(37, 0x25, false),
        "KeyL" => k(38, 0x26, false),
        "Semicolon" => k(39, 0x27, false),
        "Quote" => k(40, 0x28, false),
        "Backquote" => k(41, 0x29, false),
        "ShiftLeft" => k(42, 0x2A, false),
        "Backslash" => k(43, 0x2B, false),
        "KeyZ" => k(44, 0x2C, false),
        "KeyX" => k(45, 0x2D, false),
        "KeyC" => k(46, 0x2E, false),
        "KeyV" => k(47, 0x2F, false),
        "KeyB" => k(48, 0x30, false),
        "KeyN" => k(49, 0x31, false),
        "KeyM" => k(50, 0x32, false),
        "Comma" => k(51, 0x33, false),
        "Period" => k(52, 0x34, false),
        "Slash" => k(53, 0x35, false),
        "ShiftRight" => k(54, 0x36, false),
        "NumpadMultiply" => k(55, 0x37, false),
        "AltLeft" => k(56, 0x38, false),
        "Space" => k(57, 0x39, false),
        "CapsLock" => k(58, 0x3A, false),
        "F1" => k(59, 0x3B, false),
        "F2" => k(60, 0x3C, false),
        "F3" => k(61, 0x3D, false),
        "F4" => k(62, 0x3E, false),
        "F5" => k(63, 0x3F, false),
        "F6" => k(64, 0x40, false),
        "F7" => k(65, 0x41, false),
        "F8" => k(66, 0x42, false),
        "F9" => k(67, 0x43, false),
        "F10" => k(68, 0x44, false),
        "NumLock" => k(69, 0x45, true),
        "ScrollLock" => k(70, 0x46, false),
        "Numpad7" => k(71, 0x47, false),
        "Numpad8" => k(72, 0x48, false),
        "Numpad9" => k(73, 0x49, false),
        "NumpadSubtract" => k(74, 0x4A, false),
        "Numpad4" => k(75, 0x4B, false),
        "Numpad5" => k(76, 0x4C, false),
        "Numpad6" => k(77, 0x4D, false),
        "NumpadAdd" => k(78, 0x4E, false),
        "Numpad1" => k(79, 0x4F, false),
        "Numpad2" => k(80, 0x50, false),
        "Numpad3" => k(81, 0x51, false),
        "Numpad0" => k(82, 0x52, false),
        "NumpadDecimal" => k(83, 0x53, false),
        "IntlBackslash" => k(86, 0x56, false),
        "F11" => k(87, 0x57, false),
        "F12" => k(88, 0x58, false),
        // Sent by the viewer's "Prevent host sleep": no app reacts to it, but it is activity.
        "F15" => k(185, 0x66, false),
        "NumpadEnter" => k(96, 0x1C, true),
        "ControlRight" => k(97, 0x1D, true),
        "NumpadDivide" => k(98, 0x35, true),
        "PrintScreen" => k(99, 0x37, true),
        "AltRight" => k(100, 0x38, true),
        "Home" => k(102, 0x47, true),
        "ArrowUp" => k(103, 0x48, true),
        "PageUp" => k(104, 0x49, true),
        "ArrowLeft" => k(105, 0x4B, true),
        "ArrowRight" => k(106, 0x4D, true),
        "End" => k(107, 0x4F, true),
        "ArrowDown" => k(108, 0x50, true),
        "PageDown" => k(109, 0x51, true),
        "Insert" => k(110, 0x52, true),
        "Delete" => k(111, 0x53, true),
        "NumpadEqual" => k(117, 0x59, false),
        // win_scan 0 => injector must use WIN_VK_PAUSE.
        "Pause" => k(119, 0, false),
        "MetaLeft" | "OSLeft" => k(125, 0x5B, true),
        "MetaRight" | "OSRight" => k(126, 0x5C, true),
        "ContextMenu" => k(127, 0x5D, true),
        _ => return None,
    })
}

/// Every DOM code that [`lookup`] knows, for building uinput capability sets.
pub const ALL_CODES: &[&str] = &[
    "Escape",
    "Digit1",
    "Digit2",
    "Digit3",
    "Digit4",
    "Digit5",
    "Digit6",
    "Digit7",
    "Digit8",
    "Digit9",
    "Digit0",
    "Minus",
    "Equal",
    "Backspace",
    "Tab",
    "KeyQ",
    "KeyW",
    "KeyE",
    "KeyR",
    "KeyT",
    "KeyY",
    "KeyU",
    "KeyI",
    "KeyO",
    "KeyP",
    "BracketLeft",
    "BracketRight",
    "Enter",
    "ControlLeft",
    "KeyA",
    "KeyS",
    "KeyD",
    "KeyF",
    "KeyG",
    "KeyH",
    "KeyJ",
    "KeyK",
    "KeyL",
    "Semicolon",
    "Quote",
    "Backquote",
    "ShiftLeft",
    "Backslash",
    "KeyZ",
    "KeyX",
    "KeyC",
    "KeyV",
    "KeyB",
    "KeyN",
    "KeyM",
    "Comma",
    "Period",
    "Slash",
    "ShiftRight",
    "NumpadMultiply",
    "AltLeft",
    "Space",
    "CapsLock",
    "F1",
    "F2",
    "F3",
    "F4",
    "F5",
    "F6",
    "F7",
    "F8",
    "F9",
    "F10",
    "NumLock",
    "ScrollLock",
    "Numpad7",
    "Numpad8",
    "Numpad9",
    "NumpadSubtract",
    "Numpad4",
    "Numpad5",
    "Numpad6",
    "NumpadAdd",
    "Numpad1",
    "Numpad2",
    "Numpad3",
    "Numpad0",
    "NumpadDecimal",
    "IntlBackslash",
    "F11",
    "F12",
    "F15",
    "NumpadEnter",
    "ControlRight",
    "NumpadDivide",
    "PrintScreen",
    "AltRight",
    "Home",
    "ArrowUp",
    "PageUp",
    "ArrowLeft",
    "ArrowRight",
    "End",
    "ArrowDown",
    "PageDown",
    "Insert",
    "Delete",
    "NumpadEqual",
    "Pause",
    "MetaLeft",
    "MetaRight",
    "ContextMenu",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_codes_resolve() {
        for code in ALL_CODES {
            assert!(lookup(code).is_some(), "{code} missing from lookup()");
        }
    }

    #[test]
    fn evdev_codes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL_CODES {
            assert!(
                seen.insert(lookup(code).unwrap().evdev),
                "duplicate evdev code for {code}"
            );
        }
    }

    #[test]
    fn windows_scancodes_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL_CODES {
            let kc = lookup(code).unwrap();
            if kc.win_scan != 0 {
                assert!(
                    seen.insert((kc.win_scan, kc.win_extended)),
                    "duplicate scancode for {code}"
                );
            }
        }
    }

    #[test]
    fn spot_checks() {
        assert_eq!(lookup("KeyA"), Some(k(30, 0x1E, false)));
        assert_eq!(lookup("ArrowUp"), Some(k(103, 0x48, true)));
        assert_eq!(lookup("Nope"), None);
    }
}
