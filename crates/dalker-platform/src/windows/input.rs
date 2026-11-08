use std::time::{Duration, Instant};

use dalker_core::keymap::{self, WIN_VK_PAUSE};
use dalker_core::{DisplayInfo, InputError, InputEvent, InputInjector, MouseButton, NormPoint};

use super::desktop;
use windows::Win32::Foundation::{CloseHandle, HANDLE, HWND, POINT};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TOKEN_MANDATORY_LABEL, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentProcess, GetCurrentThreadId, OpenProcess, OpenProcessToken,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, GetKeyboardLayout, MapVirtualKeyExW, SendInput, VkKeyScanExW, HKL, INPUT, INPUT_0,
    INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYBD_EVENT_FLAGS, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MAPVK_VK_TO_CHAR, MAPVK_VK_TO_VSC_EX,
    MOUSEEVENTF_ABSOLUTE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP,
    MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY, VK_CAPITAL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetAncestor, GetForegroundWindow, GetSystemMetrics, GetWindowThreadProcessId, WindowFromPoint,
    GA_ROOT, SM_CXSCREEN, SM_CYSCREEN,
};

const XBUTTON1: u32 = 0x0001;
const XBUTTON2: u32 = 0x0002;
/// Set-1 scancode of the left Ctrl key.
const CTRL_SCANCODE: u16 = 0x1D;
/// One wheel notch (`WHEEL_DELTA`), which is also the browser's normalized unit.
const WHEEL_DELTA: f32 = 120.0;
/// Unassigned virtual key used to prevent Windows from treating a standalone Win key release
/// as a tap that opens the Start menu.
const DUMMY_VK: u16 = 0xE8;

/// Injects input with `SendInput`: absolute mouse on the primary monitor and
/// keyboard by scancode (so the host's keyboard layout applies).
pub struct SendInputInjector {
    display: DisplayInfo,
    /// Partial scroll (Win32 sign convention) not yet sent as a whole notch.
    wheel_acc: (f32, f32),
    /// Integrity level RID of this process; `None` if it could not be read.
    own_integrity: Option<u32>,
    /// When the last typed character was sent (see [`TEXT_GAP`]).
    last_char: Instant,
    /// Whether a Windows/Meta key is currently held down.
    meta_down: bool,
    /// Whether a Ctrl key is currently held down.
    ctrl_down: bool,
    /// Whether an Alt key is currently held down.
    alt_down: bool,
    /// Whether a Shift key is currently held down.
    shift_down: bool,
    /// Set while completing the Win+L lock sequence, so trailing keyups are dropped.
    win_l_locked: bool,
}

impl SendInputInjector {
    pub fn new() -> Self {
        let display = unsafe {
            DisplayInfo {
                width: GetSystemMetrics(SM_CXSCREEN).max(1) as u32,
                height: GetSystemMetrics(SM_CYSCREEN).max(1) as u32,
            }
        };
        let own_integrity = unsafe { token_integrity_of(GetCurrentProcess()) };
        Self {
            display,
            wheel_acc: (0.0, 0.0),
            own_integrity,
            last_char: Instant::now(),
            meta_down: false,
            ctrl_down: false,
            alt_down: false,
            shift_down: false,
            win_l_locked: false,
        }
    }

    /// Converts normalized coordinates to pixels of the streamed display and
    /// then to the `0..=65535` absolute range `SendInput` expects for the
    /// primary monitor.
    fn absolute(&self, x: f32, y: f32) -> (i32, i32) {
        let (px, py) = NormPoint::new(x, y).to_pixels(self.display.width, self.display.height);
        let to_abs = |p: i32, size: u32| -> i32 {
            let max = size.saturating_sub(1).max(1) as i64;
            ((p as i64 * 65535) / max) as i32
        };
        (
            to_abs(px, self.display.width),
            to_abs(py, self.display.height),
        )
    }

    fn mouse(&self, dx: i32, dy: i32, data: u32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
        INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx,
                    dy,
                    mouseData: data,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        }
    }

    fn send(inputs: &[INPUT]) -> Result<(), InputError> {
        let send = || unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
        if let Some(input) = desktop::input_desktop_name() {
            if desktop::thread_desktop_name().as_deref() != Some(input.as_str()) {
                match desktop::attach_thread_to_input_desktop() {
                    Ok(now) => tracing::info!(desktop = %now, "input follows the input desktop"),
                    Err(e) => tracing::debug!("input cannot follow the input desktop: {e}"),
                }
            }
        }
        let mut sent = send();
        if sent as usize != inputs.len() {
            // Input only reaches the input desktop from a thread attached to it. Running
            // as SYSTEM (service mode), follow it to the sign-in/UAC desktop and back.
            let before = desktop::thread_desktop_name();
            if let Ok(now) = desktop::attach_thread_to_input_desktop() {
                if before.as_deref() != Some(now.as_str()) {
                    tracing::info!(desktop = %now, "input moved to the input desktop");
                    sent = send();
                }
            }
        }
        if sent as usize != inputs.len() {
            // UIPI never makes SendInput fail (it drops silently, see `uipi_check`); a
            // failure means our desktop is not the input desktop.
            return Err(InputError::Backend(format!(
                "SendInput injected {sent}/{} events (lock screen, UAC prompt or another secure desktop is active?): {}",
                inputs.len(),
                windows::core::Error::from_thread()
            )));
        }
        Ok(())
    }

    /// The lock screen's first view (clock and picture) ignores injected mouse clicks, so
    /// a click there would do nothing for the viewer. A lone Ctrl tap raises the sign-in
    /// prompt instead, and can never type anything into it.
    fn wake_lock_screen() {
        if desktop::thread_desktop_name().as_deref() != Some("Default")
            || !desktop::session_locked()
        {
            return;
        }
        let key = |up: bool| INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(0),
                    wScan: CTRL_SCANCODE,
                    dwFlags: if up {
                        KEYEVENTF_SCANCODE | KEYEVENTF_KEYUP
                    } else {
                        KEYEVENTF_SCANCODE
                    },
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        tracing::info!("click on the lock screen: sending Ctrl to show the sign-in prompt");
        if let Err(e) = Self::send(&[key(false), key(true)]) {
            tracing::debug!("lock screen wake-up: {e}");
        }
    }

    fn update_modifiers(&mut self, code: &str, down: bool) {
        match code {
            "ControlLeft" | "ControlRight" => self.ctrl_down = down,
            "AltLeft" | "AltRight" => self.alt_down = down,
            "ShiftLeft" | "ShiftRight" => self.shift_down = down,
            "MetaLeft" | "OSLeft" | "MetaRight" | "OSRight" => self.meta_down = down,
            _ => {}
        }
    }

    fn is_win_l_chord(&self, code: &str, down: bool) -> bool {
        down && code == "KeyL"
            && self.meta_down
            && !self.ctrl_down
            && !self.alt_down
            && !self.shift_down
    }

    fn release_meta() -> Result<(), InputError> {
        let mut inputs = Vec::with_capacity(4);
        // Inject a dummy unassigned key (VK 0xE8) while Win is still down so Windows
        // does not treat the Win down + Win up sequence as a lone Win tap and open the Start menu.
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(DUMMY_VK),
                    wScan: 0,
                    dwFlags: KEYBD_EVENT_FLAGS(0),
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        inputs.push(INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: VIRTUAL_KEY(DUMMY_VK),
                    wScan: 0,
                    dwFlags: KEYEVENTF_KEYUP,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        });
        for code in ["MetaLeft", "MetaRight"] {
            if let Some(kc) = keymap::lookup(code) {
                let mut flags = KEYBD_EVENT_FLAGS(KEYEVENTF_SCANCODE.0 | KEYEVENTF_KEYUP.0);
                if kc.win_extended {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                inputs.push(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0),
                            wScan: kc.win_scan,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                });
            }
        }
        Self::send(&inputs)
    }

    /// Windows silently discards injected input aimed at a window of a process with a
    /// higher integrity level than ours (UIPI): `SendInput` still reports success. Detect
    /// that case so it shows up in the log instead of input just "not working".
    fn uipi_check(&self, hwnd: HWND, what: &str) -> Result<(), InputError> {
        let Some(own) = self.own_integrity else {
            return Ok(());
        };
        if hwnd.is_invalid() {
            return Ok(());
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
        if pid == 0 {
            return Ok(());
        }
        let Some(target) = (unsafe { process_integrity(pid) }) else {
            return Ok(());
        };
        if uipi_blocks(own, target) {
            return Err(InputError::Backend(format!(
                "{what} dropped by Windows UIPI: the target window (pid {pid}, integrity {}) runs at a higher \
                 integrity level than Dalker ({}). Run Dalker elevated (the installed task does this \
                 for administrator accounts) to control elevated windows",
                integrity_name(target),
                integrity_name(own)
            )));
        }
        Ok(())
    }

    /// The top-level window under a point of the streamed (primary) display.
    fn window_at(&self, x: f32, y: f32) -> HWND {
        let (px, py) = NormPoint::new(x, y).to_pixels(self.display.width, self.display.height);
        unsafe {
            let hwnd = WindowFromPoint(POINT { x: px, y: py });
            if hwnd.is_invalid() {
                hwnd
            } else {
                GetAncestor(hwnd, GA_ROOT)
            }
        }
    }
}

impl Default for SendInputInjector {
    fn default() -> Self {
        Self::new()
    }
}

impl InputInjector for SendInputInjector {
    fn name(&self) -> &str {
        "sendinput"
    }

    fn set_display(&mut self, display: DisplayInfo) {
        self.display = display;
    }

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        match event {
            InputEvent::MouseMove { x, y } => {
                let (ax, ay) = self.absolute(*x, *y);
                Self::send(&[self.mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE)])
            }
            InputEvent::MouseButton { button, down, x, y } => {
                let (ax, ay) = self.absolute(*x, *y);
                let (flags, data) = match (button, down) {
                    (MouseButton::Left, true) => (MOUSEEVENTF_LEFTDOWN, 0),
                    (MouseButton::Left, false) => (MOUSEEVENTF_LEFTUP, 0),
                    (MouseButton::Right, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
                    (MouseButton::Right, false) => (MOUSEEVENTF_RIGHTUP, 0),
                    (MouseButton::Middle, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
                    (MouseButton::Middle, false) => (MOUSEEVENTF_MIDDLEUP, 0),
                    (MouseButton::Back, true) => (MOUSEEVENTF_XDOWN, XBUTTON1),
                    (MouseButton::Back, false) => (MOUSEEVENTF_XUP, XBUTTON1),
                    (MouseButton::Forward, true) => (MOUSEEVENTF_XDOWN, XBUTTON2),
                    (MouseButton::Forward, false) => (MOUSEEVENTF_XUP, XBUTTON2),
                };
                Self::send(&[
                    self.mouse(ax, ay, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE),
                    self.mouse(0, 0, data, flags),
                ])?;
                if *down {
                    Self::wake_lock_screen();
                    self.uipi_check(self.window_at(*x, *y), "mouse click")?;
                }
                Ok(())
            }
            InputEvent::Wheel { dx, dy } => {
                let (v, h) = take_wheel_notches(&mut self.wheel_acc, *dx, *dy);
                let mut inputs = Vec::with_capacity(2);
                // mouseData is a DWORD carrying a signed delta.
                if v != 0 {
                    inputs.push(self.mouse(0, 0, v as u32, MOUSEEVENTF_WHEEL));
                }
                if h != 0 {
                    inputs.push(self.mouse(0, 0, h as u32, MOUSEEVENTF_HWHEEL));
                }
                if inputs.is_empty() {
                    return Ok(());
                }
                Self::send(&inputs)
            }
            InputEvent::Key { code, down } => {
                if self.win_l_locked {
                    if code == "KeyL" {
                        // While win_l_locked is set, drop every KeyL event (repeated keydowns
                        // and trailing keyup) so it never types into the lock screen.
                        return Ok(());
                    }
                    if matches!(
                        code.as_str(),
                        "MetaLeft" | "OSLeft" | "MetaRight" | "OSRight"
                    ) {
                        if !down {
                            // Win key was released: clear the locked state.
                            self.win_l_locked = false;
                            self.meta_down = false;
                        }
                        // Drop auto-repeated Win down and the trailing Win up (already released).
                        return Ok(());
                    }
                    // Any other key clears the locked state and is processed normally.
                    self.win_l_locked = false;
                }

                if self.is_win_l_chord(code, *down) {
                    tracing::info!("Win+L chord: locking workstation");
                    self.win_l_locked = true;
                    self.meta_down = false;
                    Self::release_meta()?;
                    return desktop::lock_workstation().map_err(InputError::Backend);
                }

                self.update_modifiers(code, *down);

                let kc =
                    keymap::lookup(code).ok_or_else(|| InputError::UnknownKey(code.clone()))?;
                // Right after typed text (Enter after a line, Shift+...), wait like the next
                // character would: otherwise Notepad loses the key and the last character's Shift.
                let since = self.last_char.elapsed();
                if since < TEXT_GAP {
                    std::thread::sleep(TEXT_GAP - since);
                }
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                let (vk, scan) = if kc.win_scan == 0 {
                    // Pause has no single scancode; use the virtual key.
                    (VIRTUAL_KEY(WIN_VK_PAUSE), 0)
                } else {
                    flags |= KEYEVENTF_SCANCODE;
                    if kc.win_extended {
                        flags |= KEYEVENTF_EXTENDEDKEY;
                    }
                    (VIRTUAL_KEY(0), kc.win_scan)
                };
                Self::send(&[INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: vk,
                            wScan: scan,
                            dwFlags: flags,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                }])?;
                if *down {
                    self.uipi_check(unsafe { GetForegroundWindow() }, "keyboard input")?;
                }
                Ok(())
            }
            InputEvent::Text { text } => {
                if text.is_empty() {
                    return Ok(());
                }
                let thread = unsafe { GetWindowThreadProcessId(GetForegroundWindow(), None) };
                let hkl = unsafe { GetKeyboardLayout(thread) };
                let caps = caps_lock_on(thread);
                for c in text.chars() {
                    let since = self.last_char.elapsed();
                    if since < TEXT_GAP {
                        std::thread::sleep(TEXT_GAP - since);
                    }
                    Self::send(
                        &layout_key_inputs(c, hkl, caps).unwrap_or_else(|| unicode_inputs(c)),
                    )?;
                    self.last_char = Instant::now();
                }
                self.uipi_check(unsafe { GetForegroundWindow() }, "keyboard input")
            }
            // The agent expands ReleaseAll via InputTracker; reset modifier state.
            InputEvent::ReleaseAll => {
                self.ctrl_down = false;
                self.alt_down = false;
                self.shift_down = false;
                self.meta_down = false;
                self.win_l_locked = false;
                Ok(())
            }
            // SendSAS only works from a service, so ask the Dalker service to do it.
            InputEvent::SecureAttention => {
                tracing::info!("Ctrl+Alt+Del requested by a viewer");
                super::service::request_secure_attention().map_err(InputError::Backend)
            }
        }
    }
}

/// Shortest time between two typed characters. Some apps (the Windows 11 Notepad) read
/// key state and `VK_PACKET` characters late: a burst of characters comes out with Shift
/// lost and packets repeating the last character. Typing is slower than this anyway.
const TEXT_GAP: Duration = Duration::from_millis(30);

fn key_input(vk: u16, scan: u16, mut flags: KEYBD_EVENT_FLAGS, up: bool) -> INPUT {
    if up {
        flags |= KEYEVENTF_KEYUP;
    }
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(vk),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Whether Caps Lock is on for the app in front (thread `fg`). Key state is kept per input
/// queue, so this thread borrows the foreground one's for the question.
fn caps_lock_on(fg: u32) -> bool {
    unsafe {
        let me = GetCurrentThreadId();
        let attached = fg != 0 && fg != me && AttachThreadInput(me, fg, true).as_bool();
        let on = GetKeyState(VK_CAPITAL.0 as i32) & 1 != 0;
        if attached {
            let _ = AttachThreadInput(me, fg, false);
        }
        on
    }
}

/// The key presses that type `c` on keyboard layout `hkl` (with Shift and AltGr as the
/// layout needs them, and Shift flipped for letters while Caps Lock is on), or `None` if
/// the layout has no plain key for it. Real keys reach every app the way typing does, so
/// they are preferred over `VK_PACKET`.
fn layout_key_inputs(c: char, hkl: HKL, caps: bool) -> Option<Vec<INPUT>> {
    let unit = u16::try_from(c as u32).ok()?;
    let scan = unsafe { VkKeyScanExW(unit, hkl) };
    if scan == -1 {
        return None;
    }
    let (vk, mut mods) = ((scan as u16) & 0xFF, ((scan as u16) >> 8) & 0xFF);
    // Shift (1), Ctrl (2) and Alt (4) only; Ctrl+Alt is AltGr. Skip Ctrl or Alt alone
    // (those would be shortcuts) and the Japanese Hankaku and other layout-specific states.
    if mods & !0x07 != 0 || mods & 0x06 == 0x02 || mods & 0x06 == 0x04 {
        return None;
    }
    // A dead key would wait for the next key instead of typing the character.
    if unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_CHAR, Some(hkl)) } & 0x8000_0000 != 0 {
        return None;
    }
    if caps && c.is_alphabetic() && c.to_lowercase().ne(c.to_uppercase()) {
        mods ^= 0x01;
    }
    let vsc = unsafe { MapVirtualKeyExW(vk as u32, MAPVK_VK_TO_VSC_EX, Some(hkl)) };
    let ext = if vsc & 0xFF00 == 0xE000 {
        KEYEVENTF_EXTENDEDKEY
    } else {
        KEYBD_EVENT_FLAGS(0)
    };
    let mut mod_keys: Vec<(u16, u16, KEYBD_EVENT_FLAGS)> = Vec::new();
    if mods & 0x01 != 0 {
        mod_keys.push((0xA0, 0x2A, KEYBD_EVENT_FLAGS(0))); // VK_LSHIFT
    }
    if mods & 0x06 == 0x06 {
        mod_keys.push((0xA2, 0x1D, KEYBD_EVENT_FLAGS(0))); // VK_LCONTROL
        mod_keys.push((0xA5, 0x38, KEYEVENTF_EXTENDEDKEY)); // VK_RMENU (AltGr)
    }
    let mut out: Vec<INPUT> = mod_keys
        .iter()
        .map(|&(v, s, f)| key_input(v, s, f, false))
        .collect();
    out.push(key_input(vk, (vsc & 0xFF) as u16, ext, false));
    out.push(key_input(vk, (vsc & 0xFF) as u16, ext, true));
    out.extend(
        mod_keys
            .iter()
            .rev()
            .map(|&(v, s, f)| key_input(v, s, f, true)),
    );
    Some(out)
}

/// `KEYEVENTF_UNICODE` key presses typing `c` whatever the host's keyboard layout is
/// (apps receive a `VK_PACKET` that turns into `WM_CHAR`). A character outside the BMP is
/// a surrogate pair: both halves go down, then both come up, so the pair arrives together.
fn unicode_inputs(c: char) -> Vec<INPUT> {
    let key = |unit: u16, up: bool| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: unit,
                dwFlags: if up {
                    KEYEVENTF_UNICODE | KEYEVENTF_KEYUP
                } else {
                    KEYEVENTF_UNICODE
                },
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let mut buf = [0u16; 2];
    let units = c.encode_utf16(&mut buf);
    let mut out: Vec<INPUT> = units.iter().map(|&u| key(u, false)).collect();
    out.extend(units.iter().map(|&u| key(u, true)));
    out
}

/// Adds a browser scroll delta (`+dy` = down, `+dx` = right, 120 per notch) to the
/// accumulator and returns the `mouseData` to inject for `(WHEEL, HWHEEL)`.
///
/// Only whole multiples of `WHEEL_DELTA` are emitted: Chrome sends 100 per notch (and
/// touchpads far less), and many Win32 apps compute `delta / WHEEL_DELTA` with integer
/// division, so smaller values would never scroll them. The remainder is kept for the
/// next event and dropped when the direction reverses.
fn take_wheel_notches(acc: &mut (f32, f32), dx: f32, dy: f32) -> (i32, i32) {
    // Win32: +WHEEL = scroll up, +HWHEEL = scroll right.
    fn axis(acc: &mut f32, delta: f32) -> i32 {
        if delta == 0.0 || !delta.is_finite() {
            return 0;
        }
        if acc.signum() != delta.signum() {
            *acc = 0.0;
        }
        *acc += delta;
        let notches = (*acc / WHEEL_DELTA).trunc();
        *acc -= notches * WHEEL_DELTA;
        (notches * WHEEL_DELTA) as i32
    }
    (axis(&mut acc.0, -dy), axis(&mut acc.1, dx))
}

/// UIPI blocks injected input into processes with a higher integrity level.
fn uipi_blocks(own: u32, target: u32) -> bool {
    target > own
}

fn integrity_name(rid: u32) -> String {
    match rid {
        0x0000 => "untrusted".into(),
        0x1000 => "low".into(),
        0x2000 => "medium".into(),
        0x2100 => "medium+".into(),
        0x3000 => "high/elevated".into(),
        0x4000 => "system".into(),
        _ => format!("0x{rid:04x}"),
    }
}

unsafe fn process_integrity(pid: u32) -> Option<u32> {
    let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
    let rid = token_integrity_of(process);
    let _ = CloseHandle(process);
    rid
}

/// Integrity level RID (`SECURITY_MANDATORY_*_RID`) of a process.
unsafe fn token_integrity_of(process: HANDLE) -> Option<u32> {
    let mut token = HANDLE::default();
    OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
    let rid = (|| {
        let mut len = 0u32;
        let _ = GetTokenInformation(token, TokenIntegrityLevel, None, 0, &mut len);
        if len == 0 {
            return None;
        }
        // u64 storage keeps the TOKEN_MANDATORY_LABEL pointer-aligned.
        let mut buf = vec![0u64; (len as usize).div_ceil(8)];
        GetTokenInformation(
            token,
            TokenIntegrityLevel,
            Some(buf.as_mut_ptr().cast()),
            len,
            &mut len,
        )
        .ok()?;
        let label = &*(buf.as_ptr() as *const TOKEN_MANDATORY_LABEL);
        let sid = label.Label.Sid;
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, count as u32 - 1))
    })();
    let _ = CloseHandle(token);
    rid
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::Input::KeyboardAndMouse::{LoadKeyboardLayoutW, KLF_NOTELLSHELL};

    #[test]
    fn unicode_text_presses_each_utf16_unit() {
        let keys = |c| -> Vec<(u16, bool)> {
            unicode_inputs(c)
                .iter()
                .map(|i| unsafe {
                    (
                        i.Anonymous.ki.wScan,
                        i.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP),
                    )
                })
                .collect()
        };
        assert_eq!(keys('é'), vec![(0xE9, false), (0xE9, true)]);
        assert_eq!(
            keys('😀'),
            vec![
                (0xD83D, false),
                (0xDE00, false),
                (0xD83D, true),
                (0xDE00, true)
            ]
        );
    }

    #[test]
    fn layout_keys_type_with_shift_on_us() {
        // 00000409 = US English, loaded by every Windows install.
        let hkl =
            unsafe { LoadKeyboardLayoutW(windows::core::w!("00000409"), KLF_NOTELLSHELL) }.unwrap();
        let vks = |c| -> Option<Vec<(u16, bool)>> {
            layout_key_inputs(c, hkl, false).map(|v| {
                v.iter()
                    .map(|i| unsafe {
                        (
                            i.Anonymous.ki.wVk.0,
                            i.Anonymous.ki.dwFlags.contains(KEYEVENTF_KEYUP),
                        )
                    })
                    .collect()
            })
        };
        assert_eq!(vks('a'), Some(vec![(0x41, false), (0x41, true)]));
        assert_eq!(
            vks('A'),
            Some(vec![
                (0xA0, false),
                (0x41, false),
                (0x41, true),
                (0xA0, true)
            ])
        );
        assert_eq!(vks('ж'), None);
        let caps_a = layout_key_inputs('a', hkl, true).unwrap();
        assert_eq!(unsafe { caps_a[0].Anonymous.ki.wVk.0 }, 0xA0);
        assert_eq!(vks('😀'), None);
    }

    #[test]
    fn absolute_mapping_hits_corners() {
        let inj = SendInputInjector {
            display: DisplayInfo {
                width: 1920,
                height: 1080,
            },
            ..SendInputInjector::new()
        };
        assert_eq!(inj.absolute(0.0, 0.0), (0, 0));
        assert_eq!(inj.absolute(1.0, 1.0), (65535, 65535));
        let (x, y) = inj.absolute(0.5, 0.5);
        assert!(
            (32700..32800).contains(&x) && (32700..32800).contains(&y),
            "{x},{y}"
        );
    }

    #[test]
    fn wheel_whole_notches_pass_through_with_win32_sign() {
        let mut acc = (0.0, 0.0);
        assert_eq!(
            take_wheel_notches(&mut acc, 0.0, 120.0),
            (-120, 0),
            "browser down = Win32 negative"
        );
        assert_eq!(take_wheel_notches(&mut acc, 0.0, -240.0), (240, 0));
        assert_eq!(
            take_wheel_notches(&mut acc, 120.0, 0.0),
            (0, 120),
            "right stays positive"
        );
        assert_eq!(acc, (0.0, 0.0));
    }

    #[test]
    fn wheel_chrome_pixel_deltas_accumulate_to_notches() {
        // Chrome reports 100 per notch: 6 notches = 600 = exactly 5 Win32 notches.
        let mut acc = (0.0, 0.0);
        let out: Vec<i32> = (0..6)
            .map(|_| take_wheel_notches(&mut acc, 0.0, 100.0).0)
            .collect();
        assert_eq!(out, vec![0, -120, -120, -120, -120, -120]);
        assert_eq!(acc.0, 0.0);
    }

    #[test]
    fn wheel_touchpad_deltas_and_horizontal_scroll() {
        let mut acc = (0.0, 0.0);
        let mut total = (0, 0);
        for _ in 0..30 {
            let (v, h) = take_wheel_notches(&mut acc, -8.0, -8.0);
            assert!(v % 120 == 0 && h % 120 == 0, "only whole notches: {v} {h}");
            total = (total.0 + v, total.1 + h);
        }
        // 240 up and 240 left in tiny steps.
        assert_eq!(total, (240, -240));
    }

    #[test]
    fn wheel_remainder_dropped_on_direction_change() {
        let mut acc = (0.0, 0.0);
        assert_eq!(take_wheel_notches(&mut acc, 0.0, 100.0), (0, 0));
        // Reversing must not first cancel the pending 100 of the old direction.
        assert_eq!(take_wheel_notches(&mut acc, 0.0, -120.0), (120, 0));
    }

    #[test]
    fn wheel_ignores_non_finite_and_zero() {
        let mut acc = (0.0, 0.0);
        assert_eq!(
            take_wheel_notches(&mut acc, f32::NAN, f32::INFINITY),
            (0, 0)
        );
        assert_eq!(take_wheel_notches(&mut acc, 0.0, 0.0), (0, 0));
        assert_eq!(acc, (0.0, 0.0));
    }

    #[test]
    fn uipi_only_blocks_higher_integrity() {
        assert!(
            uipi_blocks(0x2000, 0x3000),
            "medium agent -> elevated window"
        );
        assert!(!uipi_blocks(0x3000, 0x3000));
        assert!(!uipi_blocks(0x3000, 0x2000));
        assert!(!uipi_blocks(0x2000, 0x2000));
    }

    #[test]
    fn own_integrity_is_readable() {
        let rid = unsafe { token_integrity_of(GetCurrentProcess()) };
        assert!(matches!(rid, Some(r) if r >= 0x1000), "{rid:?}");
    }
    #[test]
    fn win_l_chord_detection() {
        let mut inj = SendInputInjector::new();

        // KeyL alone does not trigger Win+L chord
        assert!(!inj.is_win_l_chord("KeyL", true));

        // MetaLeft down triggers modifier tracking
        inj.update_modifiers("MetaLeft", true);
        assert!(inj.meta_down);
        assert!(inj.is_win_l_chord("KeyL", true));
        assert!(!inj.is_win_l_chord("KeyL", false));
        assert!(!inj.is_win_l_chord("KeyR", true));

        // When Ctrl is also down, Win+L is not triggered
        inj.update_modifiers("ControlLeft", true);
        assert!(!inj.is_win_l_chord("KeyL", true));
        inj.update_modifiers("ControlLeft", false);
        assert!(inj.is_win_l_chord("KeyL", true));

        // When Alt is also down, Win+L is not triggered
        inj.update_modifiers("AltLeft", true);
        assert!(!inj.is_win_l_chord("KeyL", true));
        inj.update_modifiers("AltLeft", false);
        assert!(inj.is_win_l_chord("KeyL", true));

        // When Shift is also down, Win+L is not triggered
        inj.update_modifiers("ShiftLeft", true);
        assert!(!inj.is_win_l_chord("KeyL", true));
        inj.update_modifiers("ShiftLeft", false);
        assert!(inj.is_win_l_chord("KeyL", true));

        // MetaRight also works
        inj.update_modifiers("MetaLeft", false);
        assert!(!inj.is_win_l_chord("KeyL", true));
        inj.update_modifiers("MetaRight", true);
        assert!(inj.is_win_l_chord("KeyL", true));

        // ReleaseAll clears modifiers
        inj.inject(&InputEvent::ReleaseAll).unwrap();
        assert!(!inj.meta_down);
        assert!(!inj.is_win_l_chord("KeyL", true));
    }

    #[test]
    fn win_l_auto_repeat_drops_win_and_l_until_meta_release() {
        let mut inj = SendInputInjector::new();
        inj.win_l_locked = true;

        // Auto-repeated KeyL down is dropped without error and without clearing locked state
        assert!(inj
            .inject(&InputEvent::Key {
                code: "KeyL".into(),
                down: true
            })
            .is_ok());
        assert!(inj.win_l_locked);

        // Auto-repeated Win down is dropped without setting meta_down
        assert!(inj
            .inject(&InputEvent::Key {
                code: "MetaLeft".into(),
                down: true
            })
            .is_ok());
        assert!(inj.win_l_locked);
        assert!(!inj.meta_down);

        // KeyL up is dropped without error
        assert!(inj
            .inject(&InputEvent::Key {
                code: "KeyL".into(),
                down: false
            })
            .is_ok());
        assert!(inj.win_l_locked);

        // Win up clears win_l_locked and meta_down
        assert!(inj
            .inject(&InputEvent::Key {
                code: "MetaLeft".into(),
                down: false
            })
            .is_ok());
        assert!(!inj.win_l_locked);
        assert!(!inj.meta_down);
    }
}
