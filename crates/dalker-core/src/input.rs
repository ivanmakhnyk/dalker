use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A point in normalized display space: `(0,0)` is top-left, `(1,1)` bottom-right.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct NormPoint {
    pub x: f32,
    pub y: f32,
}

impl NormPoint {
    /// Creates a point, clamping each axis into `0.0..=1.0`. NaN becomes `0.0`.
    pub fn new(x: f32, y: f32) -> Self {
        Self { x: clamp_unit(x), y: clamp_unit(y) }
    }

    /// Returns a copy with both axes clamped into `0.0..=1.0`.
    pub fn clamped(self) -> Self {
        Self::new(self.x, self.y)
    }

    /// Scales to integer pixel coordinates on a `width`×`height` display.
    /// `1.0` maps to the last pixel (`width - 1`), never past the edge.
    pub fn to_pixels(self, width: u32, height: u32) -> (i32, i32) {
        let p = self.clamped();
        let sx = (p.x * width.saturating_sub(1) as f32).round() as i32;
        let sy = (p.y * height.saturating_sub(1) as f32).round() as i32;
        (sx, sy)
    }
}

fn clamp_unit(v: f32) -> f32 {
    if v.is_nan() {
        0.0
    } else {
        v.clamp(0.0, 1.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

/// A remote input event as sent by the viewer over the `"control"` DataChannel.
///
/// Wire format (JSON), e.g.:
/// ```json
/// {"type":"mouse_move","x":0.25,"y":0.5}
/// {"type":"mouse_button","button":"left","down":true,"x":0.25,"y":0.5}
/// {"type":"wheel","dx":0,"dy":-120}
/// {"type":"key","code":"KeyA","down":true}
/// {"type":"text","text":"é"}
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputEvent {
    /// Absolute pointer move, normalized coordinates.
    MouseMove { x: f32, y: f32 },
    /// Button press/release. The position is applied before the button change.
    MouseButton { button: MouseButton, down: bool, x: f32, y: f32 },
    /// Scroll. Units follow the browser `WheelEvent` in `DOM_DELTA_PIXEL` mode;
    /// 120 ≈ one notch. Positive `dy` scrolls down, positive `dx` scrolls right.
    Wheel { dx: f32, dy: f32 },
    /// Keyboard key by physical position, using DOM `KeyboardEvent.code` names
    /// (`"KeyA"`, `"ShiftLeft"`, `"ArrowUp"`...). Layout-independent.
    Key { code: String, down: bool },
    /// Characters typed in the viewer's own keyboard layout ("Direct text" mode), injected
    /// as Unicode so the host's layout does not change them. Keys that are not characters
    /// (Enter, arrows, shortcuts...) still arrive as [`InputEvent::Key`].
    Text { text: String },
    /// Release every key and button that is currently held (sent on blur/disconnect).
    ReleaseAll,
    /// Ctrl+Alt+Del (the secure attention sequence), which cannot be typed as keys.
    /// Opens the sign-in prompt on the Windows lock screen. Windows only.
    SecureAttention,
}

impl InputEvent {
    /// Returns the event with its coordinates clamped to `0.0..=1.0`.
    pub fn sanitized(self) -> Self {
        match self {
            InputEvent::MouseMove { x, y } => {
                let p = NormPoint::new(x, y);
                InputEvent::MouseMove { x: p.x, y: p.y }
            }
            InputEvent::MouseButton { button, down, x, y } => {
                let p = NormPoint::new(x, y);
                InputEvent::MouseButton { button, down, x: p.x, y: p.y }
            }
            InputEvent::Wheel { dx, dy } => InputEvent::Wheel {
                dx: if dx.is_finite() { dx.clamp(-10_000.0, 10_000.0) } else { 0.0 },
                dy: if dy.is_finite() { dy.clamp(-10_000.0, 10_000.0) } else { 0.0 },
            },
            InputEvent::Text { text } => InputEvent::Text { text: sanitize_text(&text) },
            other => other,
        }
    }
}

/// Longest text one [`InputEvent::Text`] may inject. A viewer sends a character or an IME
/// commit at a time; this only bounds what a misbehaving one can type.
pub const MAX_TEXT_CHARS: usize = 256;

/// Keeps printable characters only: control characters would act as keys (Enter, Esc...),
/// and those must come as [`InputEvent::Key`].
fn sanitize_text(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).take(MAX_TEXT_CHARS).collect()
}

/// Tracks held keys/buttons so a session can release everything when the
/// viewer disconnects (otherwise a dropped connection could leave e.g. `Ctrl`
/// stuck down on the host).
#[derive(Debug, Default)]
pub struct InputTracker {
    keys: BTreeSet<String>,
    buttons: BTreeSet<MouseButton>,
    last_pos: Option<NormPoint>,
}

impl InputTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records the effect of `event`.
    pub fn observe(&mut self, event: &InputEvent) {
        match event {
            InputEvent::MouseMove { x, y } => self.last_pos = Some(NormPoint::new(*x, *y)),
            InputEvent::MouseButton { button, down, x, y } => {
                self.last_pos = Some(NormPoint::new(*x, *y));
                if *down {
                    self.buttons.insert(*button);
                } else {
                    self.buttons.remove(button);
                }
            }
            InputEvent::Key { code, down } => {
                if *down {
                    self.keys.insert(code.clone());
                } else {
                    self.keys.remove(code);
                }
            }
            InputEvent::ReleaseAll => {
                self.keys.clear();
                self.buttons.clear();
            }
            InputEvent::Wheel { .. } | InputEvent::Text { .. } | InputEvent::SecureAttention => {}
        }
    }

    /// Returns the "up" events needed to release everything held, and clears state.
    pub fn drain_release_events(&mut self) -> Vec<InputEvent> {
        let pos = self.last_pos.unwrap_or(NormPoint { x: 0.5, y: 0.5 });
        let mut out: Vec<InputEvent> = std::mem::take(&mut self.buttons)
            .into_iter()
            .map(|button| InputEvent::MouseButton { button, down: false, x: pos.x, y: pos.y })
            .collect();
        out.extend(
            std::mem::take(&mut self.keys)
                .into_iter()
                .map(|code| InputEvent::Key { code, down: false }),
        );
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_roundtrip() {
        let ev: InputEvent =
            serde_json::from_str(r#"{"type":"mouse_button","button":"left","down":true,"x":0.5,"y":1.0}"#)
                .unwrap();
        assert_eq!(
            ev,
            InputEvent::MouseButton { button: MouseButton::Left, down: true, x: 0.5, y: 1.0 }
        );
        let key: InputEvent = serde_json::from_str(r#"{"type":"key","code":"KeyA","down":false}"#).unwrap();
        assert_eq!(key, InputEvent::Key { code: "KeyA".into(), down: false });
        let sas: InputEvent = serde_json::from_str(r#"{"type":"secure_attention"}"#).unwrap();
        assert_eq!(sas, InputEvent::SecureAttention);
        let text: InputEvent = serde_json::from_str(r#"{"type":"text","text":"é"}"#).unwrap();
        assert_eq!(text, InputEvent::Text { text: "é".into() });
        let s = serde_json::to_string(&InputEvent::ReleaseAll).unwrap();
        assert_eq!(s, r#"{"type":"release_all"}"#);
    }

    #[test]
    fn sanitize_clamps_and_kills_nan() {
        let ev = InputEvent::MouseMove { x: -3.0, y: f32::NAN }.sanitized();
        assert_eq!(ev, InputEvent::MouseMove { x: 0.0, y: 0.0 });
        let ev = InputEvent::MouseMove { x: 7.0, y: 0.25 }.sanitized();
        assert_eq!(ev, InputEvent::MouseMove { x: 1.0, y: 0.25 });
    }

    #[test]
    fn sanitize_text_drops_controls_and_caps_length() {
        let ev = InputEvent::Text { text: "a\nб\u{1b}😀".into() }.sanitized();
        assert_eq!(ev, InputEvent::Text { text: "aб😀".into() });
        let long = "x".repeat(MAX_TEXT_CHARS + 10);
        let InputEvent::Text { text } = (InputEvent::Text { text: long }).sanitized() else { unreachable!() };
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
    }

    #[test]
    fn to_pixels_edges() {
        assert_eq!(NormPoint::new(0.0, 0.0).to_pixels(1920, 1080), (0, 0));
        assert_eq!(NormPoint::new(1.0, 1.0).to_pixels(1920, 1080), (1919, 1079));
        assert_eq!(NormPoint::new(0.5, 0.5).to_pixels(1921, 1081), (960, 540));
    }

    #[test]
    fn tracker_releases_held_inputs() {
        let mut t = InputTracker::new();
        t.observe(&InputEvent::Key { code: "ControlLeft".into(), down: true });
        t.observe(&InputEvent::Key { code: "KeyC".into(), down: true });
        t.observe(&InputEvent::Key { code: "KeyC".into(), down: false });
        t.observe(&InputEvent::MouseButton { button: MouseButton::Left, down: true, x: 0.1, y: 0.2 });
        let rel = t.drain_release_events();
        assert_eq!(
            rel,
            vec![
                InputEvent::MouseButton { button: MouseButton::Left, down: false, x: 0.1, y: 0.2 },
                InputEvent::Key { code: "ControlLeft".into(), down: false },
            ]
        );
        assert!(t.drain_release_events().is_empty());
    }
}
