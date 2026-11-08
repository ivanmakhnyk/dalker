use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventType, InputId, KeyCode,
    RelativeAxisCode, UinputAbsSetup,
};
use std::time::{Duration, Instant};

use dalker_core::keymap;
use dalker_core::{DisplayInfo, InputError, InputEvent, InputInjector, MouseButton, NormPoint};

use super::gnome::{self, MonitorLayout};

/// One wheel notch in hi-res units (and in the browser's normalized units).
const WHEEL_NOTCH: f32 = 120.0;

/// How often the GNOME monitor layout is re-read while the pointer is used.
const LAYOUT_RECHECK: Duration = Duration::from_secs(2);

/// Keys travel through the kernel and the compositor's input thread, text over D-Bus
/// straight to the shell: text sent right after a key waits this long so it can't overtake it.
const KEY_SETTLE: Duration = Duration::from_millis(15);

/// Injects input through two `/dev/uinput` virtual devices: an absolute
/// pointer (like a QEMU/VNC "tablet") and a full keyboard.
///
/// Works under X11 *and* Wayland compositors, on the console and on the login
/// screen, because the kernel sees real input devices.
pub struct UinputInjector {
    keyboard: VirtualDevice,
    pointer: Option<VirtualDevice>,
    display: DisplayInfo,
    /// GNOME only: the absolute device spans every monitor, while only the primary one
    /// is streamed. `None` (X11) = the device spans exactly the captured screen.
    layout: Option<MonitorLayout>,
    gnome: Option<zbus::blocking::Connection>,
    /// Started for a Wayland session: GNOME may not be on the bus yet (the agent starts
    /// with the session), so keep looking for its monitor layout.
    wayland: bool,
    layout_checked: Instant,
    wheel_acc: (f32, f32),
    last_key: Instant,
    /// Text is being typed with US-layout keys because the GNOME extension can't type it.
    text_fallback: bool,
}

fn uinput_err(ctx: &str, e: std::io::Error) -> InputError {
    let hint = if e.kind() == std::io::ErrorKind::PermissionDenied || e.kind() == std::io::ErrorKind::NotFound {
        " (need write access to /dev/uinput: load the `uinput` module and add the udev rule from the README)"
    } else {
        ""
    };
    InputError::Backend(format!("{ctx}: {e}{hint}"))
}

impl UinputInjector {
    pub fn new() -> Result<Self, InputError> {
        let mut keys = AttributeSet::<KeyCode>::new();
        for code in keymap::ALL_CODES {
            if let Some(kc) = keymap::lookup(code) {
                keys.insert(KeyCode::new(kc.evdev));
            }
        }
        let keyboard = VirtualDevice::builder()
            .map_err(|e| uinput_err("open /dev/uinput", e))?
            .name("Dalker Virtual Keyboard")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x4e44, 0x0001, 1))
            .with_keys(&keys)
            .map_err(|e| uinput_err("keyboard keys", e))?
            .build()
            .map_err(|e| uinput_err("create keyboard", e))?;

        let mut inj = Self {
            keyboard,
            pointer: None,
            // Replaced by set_display() as soon as the capturer reports its size.
            display: DisplayInfo { width: 1920, height: 1080 },
            layout: None,
            gnome: None,
            wayland: super::is_wayland_session(),
            layout_checked: Instant::now(),
            wheel_acc: (0.0, 0.0),
            last_key: Instant::now(),
            text_fallback: false,
        };
        if inj.wayland || gnome::gnome_session_available() {
            inj.gnome = zbus::blocking::Connection::session().ok();
            inj.layout = inj.gnome.as_ref().and_then(gnome::monitor_layout);
        }
        inj.pointer = Some(inj.build_pointer()?);
        Ok(inj)
    }

    /// The absolute axes span the area the compositor maps the device onto: the captured
    /// display in pixels (X11), or the whole GNOME desktop in layout units, so device units
    /// map 1:1 onto screen positions.
    fn build_pointer(&self) -> Result<VirtualDevice, InputError> {
        let mut buttons = AttributeSet::<KeyCode>::new();
        for b in [KeyCode::BTN_LEFT, KeyCode::BTN_RIGHT, KeyCode::BTN_MIDDLE, KeyCode::BTN_SIDE, KeyCode::BTN_EXTRA] {
            buttons.insert(b);
        }
        let mut rel = AttributeSet::<RelativeAxisCode>::new();
        for r in [
            RelativeAxisCode::REL_WHEEL,
            RelativeAxisCode::REL_HWHEEL,
            RelativeAxisCode::REL_WHEEL_HI_RES,
            RelativeAxisCode::REL_HWHEEL_HI_RES,
        ] {
            rel.insert(r);
        }
        let (w, h) = match self.layout {
            Some(l) => (l.desktop_width, l.desktop_height),
            None => (self.display.width, self.display.height),
        };
        let max_x = w.saturating_sub(1).max(1) as i32;
        let max_y = h.saturating_sub(1).max(1) as i32;
        let abs_x = UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, AbsInfo::new(0, 0, max_x, 0, 0, 1));
        let abs_y = UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, AbsInfo::new(0, 0, max_y, 0, 0, 1));

        VirtualDevice::builder()
            .map_err(|e| uinput_err("open /dev/uinput", e))?
            .name("Dalker Virtual Pointer")
            .input_id(InputId::new(BusType::BUS_VIRTUAL, 0x4e44, 0x0002, 1))
            .with_keys(&buttons)
            .map_err(|e| uinput_err("pointer buttons", e))?
            .with_relative_axes(&rel)
            .map_err(|e| uinput_err("pointer wheel", e))?
            .with_absolute_axis(&abs_x)
            .map_err(|e| uinput_err("pointer ABS_X", e))?
            .with_absolute_axis(&abs_y)
            .map_err(|e| uinput_err("pointer ABS_Y", e))?
            .build()
            .map_err(|e| uinput_err("create pointer", e))
    }

    fn pointer(&mut self) -> Result<&mut VirtualDevice, InputError> {
        self.pointer
            .as_mut()
            .ok_or_else(|| InputError::Backend("pointer device unavailable".into()))
    }

    /// Re-reads the GNOME monitor layout and recreates the pointer if it changed
    /// (monitors added, moved, rescaled...).
    fn refresh_layout(&mut self) {
        self.layout_checked = Instant::now();
        if self.gnome.is_none() && self.wayland {
            self.gnome = zbus::blocking::Connection::session().ok();
        }
        let Some(conn) = &self.gnome else { return };
        let layout = gnome::monitor_layout(conn);
        if layout != self.layout || self.pointer.is_none() {
            tracing::info!(?layout, "pointer mapped to the GNOME monitor layout");
            self.layout = layout;
            self.rebuild_pointer();
        }
    }

    fn rebuild_pointer(&mut self) {
        // The axis range is fixed at creation time, so recreate the pointer.
        self.pointer = None;
        match self.build_pointer() {
            Ok(p) => self.pointer = Some(p),
            Err(e) => tracing::error!("failed to recreate uinput pointer: {e}"),
        }
    }

    fn abs_events(&mut self, x: f32, y: f32) -> [evdev::InputEvent; 2] {
        if (self.gnome.is_some() || self.wayland) && self.layout_checked.elapsed() >= LAYOUT_RECHECK {
            self.refresh_layout();
        }
        let (px, py) = map_point(NormPoint::new(x, y), self.display, self.layout);
        [
            evdev::InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, px),
            evdev::InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, py),
        ]
    }

    fn wheel_events(&mut self, dx: f32, dy: f32) -> Vec<evdev::InputEvent> {
        let rel = |code: RelativeAxisCode, v: i32| evdev::InputEvent::new(EventType::RELATIVE.0, code.0, v);
        let mut out = Vec::with_capacity(4);
        // Browser: +dy = down, +dx = right. evdev: +REL_WHEEL = up, +REL_HWHEEL = right.
        let v = -dy;
        let h = dx;
        if v.round() as i32 != 0 {
            out.push(rel(RelativeAxisCode::REL_WHEEL_HI_RES, v.round() as i32));
        }
        if h.round() as i32 != 0 {
            out.push(rel(RelativeAxisCode::REL_HWHEEL_HI_RES, h.round() as i32));
        }
        // Legacy clients only understand whole notches; accumulate partial scrolls.
        self.wheel_acc.0 += v;
        self.wheel_acc.1 += h;
        let notches_v = (self.wheel_acc.0 / WHEEL_NOTCH).trunc();
        let notches_h = (self.wheel_acc.1 / WHEEL_NOTCH).trunc();
        if notches_v != 0.0 {
            self.wheel_acc.0 -= notches_v * WHEEL_NOTCH;
            out.push(rel(RelativeAxisCode::REL_WHEEL, notches_v as i32));
        }
        if notches_h != 0.0 {
            self.wheel_acc.1 -= notches_h * WHEEL_NOTCH;
            out.push(rel(RelativeAxisCode::REL_HWHEEL, notches_h as i32));
        }
        out
    }

    /// Types text in the viewer's layout. GNOME: through the Dalker extension, whatever
    /// the layout here is. Otherwise (X11, the GDM login screen, an extension that is not
    /// loaded yet) only ASCII can be typed, as US-layout keys.
    fn type_text(&mut self, text: &str) -> Result<(), InputError> {
        if text.is_empty() {
            return Ok(());
        }
        let since = self.last_key.elapsed();
        if since < KEY_SETTLE {
            std::thread::sleep(KEY_SETTLE - since);
        }
        if self.gnome.is_none() && self.wayland {
            self.gnome = zbus::blocking::Connection::session().ok();
        }
        if let Some(conn) = &self.gnome {
            match gnome::type_text(conn, text) {
                Ok(()) => {
                    self.text_fallback = false;
                    return Ok(());
                }
                Err(e) if !self.text_fallback => {
                    tracing::warn!("typing text as US-layout keys instead: {e}");
                    self.text_fallback = true;
                }
                Err(_) => {}
            }
        }
        self.type_ascii(text)
    }

    fn type_ascii(&mut self, text: &str) -> Result<(), InputError> {
        let io = |e: std::io::Error| InputError::Backend(format!("uinput write: {e}"));
        let key = |code: u16, down: bool| evdev::InputEvent::new(EventType::KEY.0, code, down as i32);
        let shift = KeyCode::KEY_LEFTSHIFT.0;
        let mut missing = String::new();
        for c in text.chars() {
            let Some((code, shifted)) = us_key(c).and_then(|(code, shifted)| Some((keymap::lookup(code)?.evdev, shifted)))
            else {
                missing.push(c);
                continue;
            };
            if shifted {
                self.keyboard.emit(&[key(shift, true)]).map_err(io)?;
            }
            self.keyboard.emit(&[key(code, true)]).map_err(io)?;
            self.keyboard.emit(&[key(code, false)]).map_err(io)?;
            if shifted {
                self.keyboard.emit(&[key(shift, false)]).map_err(io)?;
            }
        }
        self.last_key = Instant::now();
        if missing.is_empty() {
            Ok(())
        } else {
            Err(InputError::Backend(format!(
                "can't type {missing:?} without the Dalker GNOME Shell extension (only ASCII works without it)"
            )))
        }
    }
}

/// The key (DOM code) and Shift state that type an ASCII character on a US layout.
fn us_key(c: char) -> Option<(&'static str, bool)> {
    const LETTERS: [&str; 26] = [
        "KeyA", "KeyB", "KeyC", "KeyD", "KeyE", "KeyF", "KeyG", "KeyH", "KeyI", "KeyJ", "KeyK", "KeyL", "KeyM",
        "KeyN", "KeyO", "KeyP", "KeyQ", "KeyR", "KeyS", "KeyT", "KeyU", "KeyV", "KeyW", "KeyX", "KeyY", "KeyZ",
    ];
    const DIGITS: [&str; 10] =
        ["Digit0", "Digit1", "Digit2", "Digit3", "Digit4", "Digit5", "Digit6", "Digit7", "Digit8", "Digit9"];
    Some(match c {
        'a'..='z' => (LETTERS[c as usize - 'a' as usize], false),
        'A'..='Z' => (LETTERS[c as usize - 'A' as usize], true),
        '0'..='9' => (DIGITS[c as usize - '0' as usize], false),
        ' ' => ("Space", false),
        ')' => ("Digit0", true),
        '!' => ("Digit1", true),
        '@' => ("Digit2", true),
        '#' => ("Digit3", true),
        '$' => ("Digit4", true),
        '%' => ("Digit5", true),
        '^' => ("Digit6", true),
        '&' => ("Digit7", true),
        '*' => ("Digit8", true),
        '(' => ("Digit9", true),
        '-' => ("Minus", false),
        '_' => ("Minus", true),
        '=' => ("Equal", false),
        '+' => ("Equal", true),
        '[' => ("BracketLeft", false),
        '{' => ("BracketLeft", true),
        ']' => ("BracketRight", false),
        '}' => ("BracketRight", true),
        '\\' => ("Backslash", false),
        '|' => ("Backslash", true),
        ';' => ("Semicolon", false),
        ':' => ("Semicolon", true),
        '\'' => ("Quote", false),
        '"' => ("Quote", true),
        '`' => ("Backquote", false),
        '~' => ("Backquote", true),
        ',' => ("Comma", false),
        '<' => ("Comma", true),
        '.' => ("Period", false),
        '>' => ("Period", true),
        '/' => ("Slash", false),
        '?' => ("Slash", true),
        _ => return None,
    })
}

/// Device position for a normalized point on the streamed screen. On GNOME the stream
/// is the primary monitor minus the red border, so the border strip is skipped too.
fn map_point(p: NormPoint, display: DisplayInfo, layout: Option<MonitorLayout>) -> (i32, i32) {
    match layout {
        Some(l) => {
            let inset = gnome::BORDER_INSET.min(l.width / 2).min(l.height / 2);
            let (x, y) = p.to_pixels(l.width - 2 * inset, l.height - 2 * inset);
            ((l.x + inset) as i32 + x, (l.y + inset) as i32 + y)
        }
        None => p.to_pixels(display.width, display.height),
    }
}

fn button_code(b: MouseButton) -> KeyCode {
    match b {
        MouseButton::Left => KeyCode::BTN_LEFT,
        MouseButton::Right => KeyCode::BTN_RIGHT,
        MouseButton::Middle => KeyCode::BTN_MIDDLE,
        MouseButton::Back => KeyCode::BTN_SIDE,
        MouseButton::Forward => KeyCode::BTN_EXTRA,
    }
}

impl InputInjector for UinputInjector {
    fn name(&self) -> &str {
        "uinput"
    }

    fn set_display(&mut self, display: DisplayInfo) {
        let changed = display != self.display;
        if self.gnome.is_some() || self.wayland {
            self.display = display;
            self.refresh_layout();
            if self.layout.is_some() {
                // The device spans the desktop, not the display: only the layout matters.
                return;
            }
        }
        if !changed && self.pointer.is_some() {
            return;
        }
        self.display = display;
        self.rebuild_pointer();
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        let io = |e: std::io::Error| InputError::Backend(format!("uinput write: {e}"));
        match event {
            InputEvent::MouseMove { x, y } => {
                let evs = self.abs_events(*x, *y);
                self.pointer()?.emit(&evs).map_err(io)
            }
            InputEvent::MouseButton { button, down, x, y } => {
                let [ax, ay] = self.abs_events(*x, *y);
                let btn = evdev::InputEvent::new(EventType::KEY.0, button_code(*button).0, *down as i32);
                // Move first (own SYN_REPORT) so the click lands at the right spot.
                let p = self.pointer()?;
                p.emit(&[ax, ay]).map_err(io)?;
                p.emit(&[btn]).map_err(io)
            }
            InputEvent::Wheel { dx, dy } => {
                let evs = self.wheel_events(*dx, *dy);
                if evs.is_empty() {
                    return Ok(());
                }
                self.pointer()?.emit(&evs).map_err(io)
            }
            InputEvent::Key { code, down } => {
                let kc = keymap::lookup(code).ok_or_else(|| InputError::UnknownKey(code.clone()))?;
                let ev = evdev::InputEvent::new(EventType::KEY.0, kc.evdev, *down as i32);
                self.last_key = Instant::now();
                self.keyboard.emit(&[ev]).map_err(io)
            }
            InputEvent::Text { text } => self.type_text(text),
            // The agent expands ReleaseAll via InputTracker.
            InputEvent::ReleaseAll => Ok(()),
            // Linux has no secure attention sequence; the keys themselves are what a
            // desktop reacts to (GNOME: the log-out dialog). The browser can't send them.
            InputEvent::SecureAttention => {
                let key = |code: u16, down: bool| evdev::InputEvent::new(EventType::KEY.0, code, down as i32);
                let (ctrl, alt, del) = (KeyCode::KEY_LEFTCTRL.0, KeyCode::KEY_LEFTALT.0, KeyCode::KEY_DELETE.0);
                for (code, down) in [(ctrl, true), (alt, true), (del, true), (del, false), (alt, false), (ctrl, false)] {
                    self.keyboard.emit(&[key(code, down)]).map_err(io)?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn us_keys_cover_printable_ascii() {
        for c in ' '..='~' {
            let (code, _) = us_key(c).unwrap_or_else(|| panic!("no key for {c:?}"));
            assert!(keymap::lookup(code).is_some(), "{code} is not in the keymap");
        }
        assert_eq!(us_key('A'), Some(("KeyA", true)));
        assert_eq!(us_key('?'), Some(("Slash", true)));
        assert_eq!(us_key('é'), None);
    }

    #[test]
    fn points_map_onto_the_primary_monitor_of_the_desktop() {
        let display = DisplayInfo { width: 1280, height: 800 };
        // Two monitors side by side, the streamed (primary) one on the left.
        let left = MonitorLayout { x: 0, y: 0, width: 1280, height: 800, desktop_width: 2560, desktop_height: 800 };
        assert_eq!(map_point(NormPoint::new(0.5, 0.5), display, Some(left)), (640, 400));
        // The stream's edges are just inside the 5 px red border, which is not streamed.
        assert_eq!(map_point(NormPoint::new(0.0, 0.0), display, Some(left)), (5, 5));
        assert_eq!(map_point(NormPoint::new(1.0, 1.0), display, Some(left)), (1274, 794));
        // ...and on the right, lower than a taller neighbour.
        let right = MonitorLayout { x: 1920, y: 100, width: 1280, height: 800, desktop_width: 3200, desktop_height: 1200 };
        assert_eq!(map_point(NormPoint::new(0.0, 0.0), display, Some(right)), (1925, 105));
        assert_eq!(map_point(NormPoint::new(1.0, 1.0), display, Some(right)), (3194, 894));
        // Scaled monitor: the stream is in physical pixels, the layout in logical ones.
        let hidpi = MonitorLayout { x: 0, y: 0, width: 1280, height: 720, desktop_width: 1280, desktop_height: 720 };
        let big = DisplayInfo { width: 2560, height: 1440 };
        assert_eq!(map_point(NormPoint::new(0.5, 0.5), big, Some(hidpi)), (640, 360));
        // X11: the display itself.
        assert_eq!(map_point(NormPoint::new(1.0, 0.0), display, None), (1279, 0));
    }
}
