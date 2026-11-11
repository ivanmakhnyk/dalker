//! Keyboard and mouse input posted as Quartz events (`CGEventPost` at the HID level), the
//! way a real USB keyboard and mouse would arrive. Needs the Accessibility permission;
//! without it macOS silently drops the events.

use std::time::{Duration, Instant};

use dalker_core::{keymap, DisplayInfo, InputError, InputEvent, InputInjector, MouseButton};
use objc2_core_foundation::{CFRetained, CGPoint};
use objc2_core_graphics::{
    CGDisplayBounds, CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID,
    CGEventTapLocation, CGEventType, CGMainDisplayID, CGMouseButton, CGScrollEventUnit,
};

use super::permissions;

/// A second click on the same button within this time and distance counts as a double
/// click (macOS apps read the click count, not the timing).
const MULTI_CLICK_TIME: Duration = Duration::from_millis(500);
const MULTI_CLICK_DISTANCE: f64 = 4.0;
/// How often a missing Accessibility permission is checked again.
const PERMISSION_RECHECK: Duration = Duration::from_secs(2);
/// UTF-16 units per text event (`CGEventKeyboardSetUnicodeString` takes at most 20).
const TEXT_CHUNK: usize = 20;

struct LastClick {
    button: MouseButton,
    at: Instant,
    pos: CGPoint,
    count: i64,
}

/// Posts viewer input as CGEvents on the main display.
pub struct CgInjector {
    source: CFRetained<CGEventSource>,
    /// Modifier keys held now, put on every event so shortcuts and Shift-clicks work.
    flags: CGEventFlags,
    /// Buttons held now: a move while one is down is a drag.
    held: Vec<MouseButton>,
    pos: CGPoint,
    last_click: Option<LastClick>,
    trusted: bool,
    checked: Instant,
}

// SAFETY: CGEventSource and CGEvent are CF objects that may be used from any thread; the
// injector lives on the agent's input thread.
unsafe impl Send for CgInjector {}

impl CgInjector {
    pub fn new() -> Result<Self, InputError> {
        let source = CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok_or_else(|| InputError::Backend("no event source".into()))?;
        // Ask once, at startup; input simply starts working once it's granted.
        let trusted = permissions::accessibility(true);
        if !trusted {
            tracing::warn!("{}", permissions::ACCESSIBILITY_MISSING);
        }
        Ok(Self {
            source,
            flags: CGEventFlags::empty(),
            held: Vec::new(),
            pos: CGPoint::new(0.0, 0.0),
            last_click: None,
            trusted,
            checked: Instant::now(),
        })
    }

    fn check_permission(&mut self) -> Result<(), InputError> {
        if !self.trusted && self.checked.elapsed() >= PERMISSION_RECHECK {
            self.trusted = permissions::accessibility(false);
            self.checked = Instant::now();
        }
        if self.trusted {
            Ok(())
        } else {
            Err(InputError::Unsupported(
                permissions::ACCESSIBILITY_MISSING.into(),
            ))
        }
    }

    /// Normalized viewer coordinates to global display points on the main display.
    fn to_point(x: f32, y: f32) -> CGPoint {
        let p = dalker_core::NormPoint::new(x, y);
        let b = CGDisplayBounds(CGMainDisplayID());
        CGPoint::new(
            b.origin.x + p.x as f64 * (b.size.width - 1.0).max(0.0),
            b.origin.y + p.y as f64 * (b.size.height - 1.0).max(0.0),
        )
    }

    fn post(&self, event: Option<CFRetained<CGEvent>>) -> Result<(), InputError> {
        let event = event.ok_or_else(|| InputError::Backend("could not create event".into()))?;
        CGEvent::post(CGEventTapLocation::HIDEventTap, Some(&event));
        Ok(())
    }

    fn mouse(
        &self,
        kind: CGEventType,
        button: CGMouseButton,
        number: i64,
        clicks: i64,
    ) -> Result<(), InputError> {
        let event = CGEvent::new_mouse_event(Some(&self.source), kind, self.pos, button);
        if let Some(e) = &event {
            CGEvent::set_flags(Some(e), self.flags);
            CGEvent::set_integer_value_field(Some(e), CGEventField::MouseEventButtonNumber, number);
            if clicks > 0 {
                CGEvent::set_integer_value_field(
                    Some(e),
                    CGEventField::MouseEventClickState,
                    clicks,
                );
            }
        }
        self.post(event)
    }

    fn move_to(&mut self, x: f32, y: f32) -> Result<(), InputError> {
        self.pos = Self::to_point(x, y);
        let (kind, button) = match self.held.first() {
            Some(MouseButton::Left) => (CGEventType::LeftMouseDragged, CGMouseButton::Left),
            Some(MouseButton::Right) => (CGEventType::RightMouseDragged, CGMouseButton::Right),
            Some(_) => (CGEventType::OtherMouseDragged, CGMouseButton::Center),
            None => (CGEventType::MouseMoved, CGMouseButton::Left),
        };
        let number = self.held.first().map_or(0, |b| button_number(*b));
        self.mouse(kind, button, number, 0)
    }

    fn button(&mut self, button: MouseButton, down: bool) -> Result<(), InputError> {
        let (kind, cg) = match (button, down) {
            (MouseButton::Left, true) => (CGEventType::LeftMouseDown, CGMouseButton::Left),
            (MouseButton::Left, false) => (CGEventType::LeftMouseUp, CGMouseButton::Left),
            (MouseButton::Right, true) => (CGEventType::RightMouseDown, CGMouseButton::Right),
            (MouseButton::Right, false) => (CGEventType::RightMouseUp, CGMouseButton::Right),
            (_, true) => (CGEventType::OtherMouseDown, CGMouseButton::Center),
            (_, false) => (CGEventType::OtherMouseUp, CGMouseButton::Center),
        };
        let clicks = if down {
            let now = Instant::now();
            let count = match &self.last_click {
                Some(c)
                    if c.button == button
                        && now.duration_since(c.at) <= MULTI_CLICK_TIME
                        && (c.pos.x - self.pos.x).abs() <= MULTI_CLICK_DISTANCE
                        && (c.pos.y - self.pos.y).abs() <= MULTI_CLICK_DISTANCE =>
                {
                    c.count + 1
                }
                _ => 1,
            };
            self.last_click = Some(LastClick {
                button,
                at: now,
                pos: self.pos,
                count,
            });
            if !self.held.contains(&button) {
                self.held.push(button);
            }
            count
        } else {
            self.held.retain(|b| *b != button);
            match &self.last_click {
                Some(c) if c.button == button => c.count,
                _ => 1,
            }
        };
        self.mouse(kind, cg, button_number(button), clicks)
    }

    fn wheel(&self, dx: f32, dy: f32) -> Result<(), InputError> {
        // The browser's positive dy scrolls down; a positive CG wheel value scrolls up.
        let event = CGEvent::new_scroll_wheel_event2(
            Some(&self.source),
            CGScrollEventUnit::Pixel,
            2,
            (-dy).round() as i32,
            (-dx).round() as i32,
            0,
        );
        if let Some(e) = &event {
            CGEvent::set_flags(Some(e), self.flags);
        }
        self.post(event)
    }

    fn key(&mut self, code: &str, down: bool) -> Result<(), InputError> {
        let vk =
            keymap::mac_virtual_key(code).ok_or_else(|| InputError::UnknownKey(code.to_owned()))?;
        if let Some(flag) = modifier_flag(code) {
            self.flags.set(flag, down);
        }
        let event = CGEvent::new_keyboard_event(Some(&self.source), vk, down);
        if let Some(e) = &event {
            CGEvent::set_flags(Some(e), self.flags);
        }
        self.post(event)
    }

    fn text(&self, text: &str) -> Result<(), InputError> {
        let units: Vec<u16> = text.encode_utf16().collect();
        for chunk in units.chunks(TEXT_CHUNK) {
            for down in [true, false] {
                let event = CGEvent::new_keyboard_event(Some(&self.source), 0, down);
                if let Some(e) = &event {
                    // Typed as characters: no modifiers, or they would turn into shortcuts.
                    CGEvent::set_flags(Some(e), CGEventFlags::empty());
                    // SAFETY: `chunk` is a live slice of `chunk.len()` UTF-16 units.
                    unsafe {
                        CGEvent::keyboard_set_unicode_string(
                            Some(e),
                            chunk.len() as _,
                            chunk.as_ptr(),
                        )
                    };
                }
                self.post(event)?;
            }
        }
        Ok(())
    }
}

/// CG button number for a mouse button (0 left, 1 right, 2 middle, 3 back, 4 forward).
fn button_number(button: MouseButton) -> i64 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
        MouseButton::Back => 3,
        MouseButton::Forward => 4,
    }
}

/// The event flag a modifier key sets while it is held.
fn modifier_flag(code: &str) -> Option<CGEventFlags> {
    Some(match code {
        "ShiftLeft" | "ShiftRight" => CGEventFlags::MaskShift,
        "ControlLeft" | "ControlRight" => CGEventFlags::MaskControl,
        "AltLeft" | "AltRight" => CGEventFlags::MaskAlternate,
        "MetaLeft" | "MetaRight" | "OSLeft" | "OSRight" => CGEventFlags::MaskCommand,
        _ => return None,
    })
}

impl InputInjector for CgInjector {
    fn name(&self) -> &str {
        "cgevent"
    }

    // Points come from the display's bounds at each event, so a resolution change needs
    // nothing here.
    fn set_display(&mut self, _display: DisplayInfo) {}

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        self.check_permission()?;
        match event.clone().sanitized() {
            InputEvent::MouseMove { x, y } => self.move_to(x, y),
            InputEvent::MouseButton { button, down, x, y } => {
                self.move_to(x, y)?;
                self.button(button, down)
            }
            InputEvent::Wheel { dx, dy } => self.wheel(dx, dy),
            InputEvent::Key { code, down } => self.key(&code, down),
            InputEvent::Text { text } => self.text(&text),
            InputEvent::ReleaseAll => {
                // The agent releases what it saw pressed; also forget our own state.
                self.flags = CGEventFlags::empty();
                for button in std::mem::take(&mut self.held) {
                    self.button(button, false)?;
                }
                Ok(())
            }
            InputEvent::SecureAttention => Err(InputError::Unsupported(
                "Ctrl+Alt+Del is Windows only".into(),
            )),
        }
    }
}
