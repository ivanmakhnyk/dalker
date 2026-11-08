//! Friendly key names (`"ctrl+shift+t"`, `"enter"`, `"pgdn"`) to the DOM `KeyboardEvent.code`
//! names that [`dalker_core::keymap`] maps to scancodes and evdev codes.
//!
//! Keys are physical positions, as on a US keyboard: `"ctrl+z"` presses the key labelled Z on
//! a US layout, whatever the host's layout is. Typing text belongs in `type_text`.

use dalker_core::keymap;

/// One chord: modifiers held (in order) while `key` is pressed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord {
    pub modifiers: Vec<&'static str>,
    pub key: String,
}

impl Chord {
    /// The key events of one press: modifiers down, key down, key up, modifiers up (reversed).
    pub fn events(&self) -> Vec<(String, bool)> {
        let mut out: Vec<(String, bool)> = self.modifiers.iter().map(|m| (m.to_string(), true)).collect();
        out.push((self.key.clone(), true));
        out.push((self.key.clone(), false));
        out.extend(self.modifiers.iter().rev().map(|m| (m.to_string(), false)));
        out
    }
}

fn modifier(name: &str) -> Option<&'static str> {
    Some(match name {
        "ctrl" | "control" | "ctl" | "lctrl" => "ControlLeft",
        "rctrl" => "ControlRight",
        "shift" | "lshift" => "ShiftLeft",
        "rshift" => "ShiftRight",
        "alt" | "option" | "opt" | "lalt" => "AltLeft",
        "ralt" | "altgr" => "AltRight",
        "meta" | "win" | "windows" | "super" | "cmd" | "command" | "os" | "lmeta" => "MetaLeft",
        "rmeta" => "MetaRight",
        _ => return None,
    })
}

fn named_key(name: &str) -> Option<String> {
    let code = match name {
        "enter" | "return" => "Enter",
        "esc" | "escape" => "Escape",
        "tab" => "Tab",
        "space" | "spacebar" => "Space",
        "backspace" | "bksp" => "Backspace",
        "delete" | "del" => "Delete",
        "insert" | "ins" => "Insert",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" => "PageDown",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "capslock" | "caps" => "CapsLock",
        "numlock" => "NumLock",
        "scrolllock" => "ScrollLock",
        "printscreen" | "prtsc" | "prtscr" | "print" => "PrintScreen",
        "pause" | "break" => "Pause",
        "menu" | "contextmenu" | "apps" => "ContextMenu",
        "minus" | "-" => "Minus",
        "equal" | "equals" | "=" => "Equal",
        "plus" | "+" => "NumpadAdd",
        "bracketleft" | "[" => "BracketLeft",
        "bracketright" | "]" => "BracketRight",
        "backslash" | "\\" => "Backslash",
        "semicolon" | ";" => "Semicolon",
        "quote" | "apostrophe" | "'" => "Quote",
        "backquote" | "backtick" | "grave" | "`" => "Backquote",
        "comma" | "," => "Comma",
        "period" | "dot" | "." => "Period",
        "slash" | "/" => "Slash",
        "numpadenter" => "NumpadEnter",
        "numpadadd" => "NumpadAdd",
        "numpadsubtract" => "NumpadSubtract",
        "numpadmultiply" => "NumpadMultiply",
        "numpaddivide" => "NumpadDivide",
        "numpaddecimal" => "NumpadDecimal",
        _ => {
            let mut chars = name.chars();
            return match (chars.next(), chars.next()) {
                (Some(c @ 'a'..='z'), None) => Some(format!("Key{}", c.to_ascii_uppercase())),
                (Some(c @ '0'..='9'), None) => Some(format!("Digit{c}")),
                _ => {
                    if let Some(n) =
                        name.strip_prefix("numpad").filter(|n| n.len() == 1 && n.as_bytes()[0].is_ascii_digit())
                    {
                        Some(format!("Numpad{n}"))
                    } else {
                        name.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()).map(|n| format!("F{n}"))
                    }
                }
            };
        }
    };
    Some(code.to_owned())
}

/// Resolves one key name: a friendly name, a modifier, or a DOM code itself (`"KeyA"`).
fn resolve(part: &str) -> Result<String, String> {
    let lower = part.to_ascii_lowercase();
    let code = modifier(&lower)
        .map(str::to_owned)
        .or_else(|| named_key(&lower))
        .or_else(|| keymap::lookup(part).map(|_| part.to_owned()))
        .ok_or_else(|| format!("unknown key `{part}`"))?;
    if keymap::lookup(&code).is_none() {
        return Err(format!("key `{part}` is not supported"));
    }
    Ok(code)
}

/// Parses one chord such as `"ctrl+shift+t"`, `"alt+f4"`, `"enter"` or `"ctrl++"`.
pub fn parse_chord(text: &str) -> Result<Chord, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("empty key".into());
    }
    // `+` separates keys; a trailing `+` after a separator is the key itself ("ctrl++").
    let mut parts: Vec<&str> = text.split('+').collect();
    if text.ends_with("++") || text == "+" {
        parts.retain(|p| !p.is_empty());
        parts.push("+");
    }
    if parts.iter().any(|p| p.trim().is_empty()) {
        return Err(format!("malformed key chord `{text}`"));
    }
    let (last, mods) = parts.split_last().expect("not empty");
    let mut modifiers = Vec::new();
    for m in mods {
        let lower = m.trim().to_ascii_lowercase();
        let code = modifier(&lower)
            .ok_or_else(|| format!("`{}` is not a modifier (ctrl, shift, alt, meta) in `{text}`", m.trim()))?;
        if !modifiers.contains(&code) {
            modifiers.push(code);
        }
    }
    let key = resolve(last.trim())?;
    // "shift+ctrl" style: the last part may itself be a modifier; press it as the key.
    modifiers.retain(|m| *m != key);
    Ok(Chord { modifiers, key })
}

/// Parses a space-separated sequence of chords: `"ctrl+a backspace"`.
pub fn parse_sequence(text: &str) -> Result<Vec<Chord>, String> {
    let chords = text.split_whitespace().map(parse_chord).collect::<Result<Vec<_>, _>>()?;
    if chords.is_empty() {
        return Err("no keys given".into());
    }
    Ok(chords)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(mods: &[&'static str], key: &str) -> Chord {
        Chord { modifiers: mods.to_vec(), key: key.into() }
    }

    #[test]
    fn single_keys() {
        assert_eq!(parse_chord("enter").unwrap(), chord(&[], "Enter"));
        assert_eq!(parse_chord("Return").unwrap(), chord(&[], "Enter"));
        assert_eq!(parse_chord("a").unwrap(), chord(&[], "KeyA"));
        assert_eq!(parse_chord("Z").unwrap(), chord(&[], "KeyZ"));
        assert_eq!(parse_chord("7").unwrap(), chord(&[], "Digit7"));
        assert_eq!(parse_chord("f5").unwrap(), chord(&[], "F5"));
        assert_eq!(parse_chord("F12").unwrap(), chord(&[], "F12"));
        assert_eq!(parse_chord("pgdn").unwrap(), chord(&[], "PageDown"));
        assert_eq!(parse_chord("esc").unwrap(), chord(&[], "Escape"));
        assert_eq!(parse_chord("numpad3").unwrap(), chord(&[], "Numpad3"));
        assert_eq!(parse_chord("/").unwrap(), chord(&[], "Slash"));
        assert_eq!(parse_chord("ArrowUp").unwrap(), chord(&[], "ArrowUp"), "DOM codes work too");
        assert_eq!(parse_chord("KeyQ").unwrap(), chord(&[], "KeyQ"));
        assert_eq!(parse_chord("ctrl").unwrap(), chord(&[], "ControlLeft"), "a lone modifier is a key");
    }

    #[test]
    fn chords() {
        assert_eq!(parse_chord("ctrl+c").unwrap(), chord(&["ControlLeft"], "KeyC"));
        assert_eq!(parse_chord("Ctrl+Shift+T").unwrap(), chord(&["ControlLeft", "ShiftLeft"], "KeyT"));
        assert_eq!(parse_chord("alt+tab").unwrap(), chord(&["AltLeft"], "Tab"));
        assert_eq!(parse_chord("alt+f4").unwrap(), chord(&["AltLeft"], "F4"));
        assert_eq!(parse_chord("cmd+space").unwrap(), chord(&["MetaLeft"], "Space"));
        assert_eq!(parse_chord("win+r").unwrap(), chord(&["MetaLeft"], "KeyR"));
        assert_eq!(parse_chord("ctrl+alt+delete").unwrap(), chord(&["ControlLeft", "AltLeft"], "Delete"));
        assert_eq!(parse_chord(" ctrl + c ").unwrap(), chord(&["ControlLeft"], "KeyC"));
        assert_eq!(parse_chord("ctrl+ctrl+c").unwrap(), chord(&["ControlLeft"], "KeyC"), "duplicates collapse");
        assert_eq!(parse_chord("ctrl+shift").unwrap(), chord(&["ControlLeft"], "ShiftLeft"));
        assert_eq!(parse_chord("ctrl++").unwrap(), chord(&["ControlLeft"], "NumpadAdd"));
        assert_eq!(parse_chord("ctrl+-").unwrap(), chord(&["ControlLeft"], "Minus"));
    }

    #[test]
    fn rejects_garbage() {
        for bad in ["", "ctrl+", "+c", "ctrl++c", "c+ctrl", "hyper+x", "f99", "ü", "ctrl+banana", "numpad10"] {
            assert!(parse_chord(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn every_resolved_code_is_in_the_keymap() {
        for name in [
            "enter", "tab", "space", "home", "end", "insert", "menu", "pause", "prtsc", "`", "'", "[", "]", "\\", ";",
            ",", ".", "=",
        ] {
            let c = parse_chord(name).unwrap();
            assert!(keymap::lookup(&c.key).is_some(), "{name} -> {}", c.key);
        }
    }

    #[test]
    fn sequences_and_events() {
        let seq = parse_sequence("ctrl+a  backspace").unwrap();
        assert_eq!(seq, vec![chord(&["ControlLeft"], "KeyA"), chord(&[], "Backspace")]);
        assert!(parse_sequence("   ").is_err());
        assert!(parse_sequence("ctrl+a nope").is_err());
        let ev = parse_chord("ctrl+shift+t").unwrap().events();
        let expect: Vec<(String, bool)> = [
            ("ControlLeft", true),
            ("ShiftLeft", true),
            ("KeyT", true),
            ("KeyT", false),
            ("ShiftLeft", false),
            ("ControlLeft", false),
        ]
        .iter()
        .map(|(c, d)| (c.to_string(), *d))
        .collect();
        assert_eq!(ev, expect);
    }
}
