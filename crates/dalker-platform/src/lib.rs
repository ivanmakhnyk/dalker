//! # dalker-platform
//!
//! OS backends for [`dalker_core::ScreenCapturer`] and [`dalker_core::InputInjector`]:
//!
//! | OS      | Capture                                 | Input                         |
//! |---------|-----------------------------------------|-------------------------------|
//! | Windows | DXGI Desktop Duplication (primary output)| `SendInput` (scancodes, absolute mouse) |
//! | Linux   | X11 root window via MIT-SHM (falls back to `GetImage`) | `/dev/uinput` virtual devices |
//! | macOS   | not implemented yet                     | not implemented yet           |
//!
//! Use [`create_capturer`] / [`create_injector`] to get the backend for the
//! current OS without any `cfg` at the call site.

use dalker_core::{CaptureError, Clipboard, FileAccess, InputError, InputInjector, ScreenCapturer, ViewerIndicator};

pub mod icon;
#[cfg(target_os = "linux")]
pub mod linux;
#[cfg(target_os = "macos")]
pub mod macos;
#[cfg(target_os = "windows")]
pub mod windows;

/// Prepares the process for capture/injection. Call once at startup, before
/// creating any window or capturer (on Windows this enables per-monitor DPI
/// awareness so DXGI and `SendInput` agree on physical pixels).
pub fn init_process() {
    #[cfg(target_os = "windows")]
    windows::init_process();
}

/// Creates the native screen capturer for this OS.
pub fn create_capturer() -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(windows::DxgiCapturer::new()?));
    }
    #[cfg(target_os = "linux")]
    {
        // GNOME (Wayland, or any session whose bus has Mutter's ScreenCast) first: under
        // Wayland the X11 root window is just an empty XWayland surface.
        let gnome_why = match linux::gnome::GnomeCapturer::new() {
            Ok(c) => return Ok(Box::new(c)),
            Err(CaptureError::Unsupported(why)) => why,
            Err(e) => return Err(e),
        };
        if linux::is_wayland_session() {
            // XWayland only shows X11 apps; never pretend to capture the desktop with it.
            return Err(CaptureError::Unsupported(format!(
                "Wayland session without a usable GNOME screen cast ({gnome_why}); only GNOME on Wayland is supported"
            )));
        }
        return match linux::X11Capturer::new() {
            Ok(c) => Ok(Box::new(c)),
            Err(CaptureError::Unsupported(x11)) => Err(CaptureError::Unsupported(format!(
                "GNOME screen cast: {gnome_why}; X11: {x11}"
            ))),
            Err(e) => Err(e),
        };
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(macos::MacCapturer::new()?));
    }
    #[allow(unreachable_code)]
    Err(CaptureError::Unsupported(std::env::consts::OS.to_string()))
}

/// Creates the native input injector for this OS.
pub fn create_injector() -> Result<Box<dyn InputInjector>, InputError> {
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(windows::SendInputInjector::new()));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(linux::UinputInjector::new()?));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(macos::MacInjector::new()?));
    }
    #[allow(unreachable_code)]
    Err(InputError::Unsupported(std::env::consts::OS.to_string()))
}

/// Opens the host's clipboard (text), shared with viewers. Call it on the thread that will
/// use it: on Windows it is tied to that thread.
///
/// Errors when there is none to share: Linux needs GNOME Shell with the Dalker extension
/// (Wayland lets nothing else reach the clipboard); other X11 desktops are not supported yet.
pub fn create_clipboard() -> Result<Box<dyn Clipboard>, String> {
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(windows::WinClipboard::new()?));
    }
    #[cfg(target_os = "linux")]
    {
        return Ok(Box::new(linux::gnome::GnomeClipboard::new()?));
    }
    #[allow(unreachable_code)]
    Err(format!("no clipboard support for {}", std::env::consts::OS))
}

/// Opens the host user that viewers' file transfers run as, for the calling thread only (see
/// [`FileAccess`]): the signed-in user, never SYSTEM or root. When the agent runs as that
/// user already, that is the agent's own account.
///
/// Errors when nobody is signed in (the sign-in screen is up).
pub fn open_file_access() -> Result<FileAccess, String> {
    #[cfg(target_os = "windows")]
    {
        return windows::files::open_file_access();
    }
    #[cfg(target_os = "linux")]
    {
        return linux::files::open_file_access();
    }
    #[allow(unreachable_code)]
    Err(format!("no file transfer support for {}", std::env::consts::OS))
}

/// Creates the on-screen "remote session active" indicator (red screen border).
///
/// Errors if it cannot be drawn; callers serving a real desktop must treat that
/// as fatal rather than run without a visible indicator.
pub fn create_indicator() -> Result<Box<dyn ViewerIndicator>, String> {
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(windows::BorderOverlay::new()?));
    }
    #[cfg(target_os = "linux")]
    {
        // Under GNOME the border must come from the shell extension: an X11 overlay
        // would "succeed" against XWayland yet never be visible, so there is no fallback.
        if linux::gnome::gnome_session_available() || linux::is_wayland_session() {
            return Ok(Box::new(linux::gnome::GnomeBorder::new()?));
        }
        return Ok(Box::new(linux::BorderOverlay::new()?));
    }
    #[allow(unreachable_code)]
    Err(format!("no on-screen indicator for {}", std::env::consts::OS))
}

/// True while the host's lock screen is up and its picture is withheld from viewers,
/// who then show a "locked" notice. Every backend streams the lock screen now (Windows:
/// lock and sign-in screens; GNOME: the border extension stays on there), so this is
/// always false.
pub fn screen_locked() -> bool {
    false
}

/// For GUI-subsystem Windows builds: reconnect to the launching terminal (if any) so command
/// line output is visible. No-op elsewhere.
pub fn attach_parent_console() {
    #[cfg(target_os = "windows")]
    windows::attach_parent_console();
}

/// Set by the Windows console launcher (`dalkerd.com`) for the `dalkerd.exe` it starts and
/// waits for: the terminal that ran it is waiting too, so the console is safe to read.
pub const CONSOLE_LAUNCHER_ENV: &str = "DALKER_CONSOLE_LAUNCHER";

/// For the Windows console launcher (`dalkerd.com`): survive Ctrl+C and Ctrl+Break so the
/// launcher keeps waiting for `dalkerd.exe`, which shares the console, gets the same signal and
/// decides for itself. Not inherited by child processes. No-op elsewhere.
pub fn outlive_console_ctrl_c() {
    #[cfg(target_os = "windows")]
    windows::outlive_console_ctrl_c();
}

/// Whether standard error goes anywhere. A GUI-subsystem Windows build started by the task
/// scheduler has no console, so the agent logs to a file instead. Always true elsewhere.
pub fn has_std_error() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::has_std_error()
    }
    #[cfg(not(target_os = "windows"))]
    {
        true
    }
}
