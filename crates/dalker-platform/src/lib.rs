//! # dalker-platform
//!
//! OS backends for [`dalker_core::ScreenCapturer`] and [`dalker_core::InputInjector`]:
//!
//! | OS      | Capture                                 | Input                         |
//! |---------|-----------------------------------------|-------------------------------|
//! | Windows | DXGI Desktop Duplication (primary output)| `SendInput` (scancodes, absolute mouse) |
//! | Linux   | GNOME screen cast, xdg-desktop-portal screen cast (other Wayland desktops), or the X11 root window via MIT-SHM | `/dev/uinput` virtual devices |
//! | macOS   | ScreenCaptureKit (main display)          | `CGEventPost` (HID level)     |
//!
//! Use [`create_capturer`] / [`create_injector`] to get the backend for the
//! current OS without any `cfg` at the call site. On macOS the agent must also run inside
//! [`run_app`], which keeps the main thread for AppKit.

use dalker_core::{
    CaptureError, Clipboard, FileAccess, InputError, InputInjector, ScreenCapturer, ViewerIndicator,
};

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

/// Runs `f` (the whole agent) and returns its result. On macOS `f` runs on another thread
/// while the main thread runs the AppKit event loop that the red border and the menu bar
/// icon need; elsewhere it just calls `f`.
pub fn run_app<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    #[cfg(target_os = "macos")]
    {
        macos::run_app(f)
    }
    #[cfg(not(target_os = "macos"))]
    {
        f()
    }
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
            // Any other Wayland desktop (KDE Plasma, sway...): the screen-cast portal. XWayland
            // only shows X11 apps, so never pretend to capture the desktop with it.
            return match linux::portal::PortalCapturer::new() {
                Ok(c) => Ok(Box::new(c)),
                Err(CaptureError::Unsupported(portal_why)) => {
                    Err(CaptureError::Unsupported(format!(
                        "Wayland session without a usable screen cast (GNOME: {gnome_why}; \
                         xdg-desktop-portal: {portal_why})"
                    )))
                }
                Err(e) => Err(e),
            };
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
        return Ok(Box::new(macos::SckCapturer::new()?));
    }
    #[allow(unreachable_code)]
    Err(CaptureError::Unsupported(std::env::consts::OS.to_string()))
}

/// Names the capture backend when starting it only to see whether it works would bother
/// someone, so it should first be started when a viewer connects: the xdg-desktop-portal
/// screen cast may ask the person at the host to allow it. `None` = probing is harmless.
pub fn capture_backend_without_probe() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        if linux::is_wayland_session()
            && !linux::gnome::gnome_session_available()
            && linux::portal::available()
        {
            return Some("portal-screencast");
        }
    }
    None
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
        return Ok(Box::new(macos::CgInjector::new()?));
    }
    #[allow(unreachable_code)]
    Err(InputError::Unsupported(std::env::consts::OS.to_string()))
}

/// Opens the host's clipboard (text), shared with viewers. Call it on the thread that will
/// use it: on Windows it is tied to that thread.
///
/// Errors when there is none to share: Linux needs GNOME Shell with the Dalker extension, or
/// KDE Plasma's Klipper (Wayland lets nothing else reach the clipboard); other desktops are
/// not supported yet.
pub fn create_clipboard() -> Result<Box<dyn Clipboard>, String> {
    #[cfg(target_os = "windows")]
    {
        return Ok(Box::new(windows::WinClipboard::new()?));
    }
    #[cfg(target_os = "linux")]
    {
        if !linux::gnome::gnome_session_available() && linux::kde::klipper_available() {
            return Ok(Box::new(linux::kde::KlipperClipboard::new()?));
        }
        return Ok(Box::new(linux::gnome::GnomeClipboard::new()?));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(macos::MacClipboard::new()?));
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
    #[cfg(target_os = "macos")]
    {
        return macos::files::open_file_access();
    }
    #[allow(unreachable_code)]
    Err(format!(
        "no file transfer support for {}",
        std::env::consts::OS
    ))
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
        // Under GNOME the border must come from the shell extension, and on other Wayland
        // desktops from a layer-shell surface: an X11 overlay would "succeed" against
        // XWayland yet never be visible, so there is no fallback to it.
        if linux::gnome::gnome_session_available() {
            return Ok(Box::new(linux::gnome::GnomeBorder::new()?));
        }
        if linux::is_wayland_session() {
            return match linux::wayland::LayerBorder::new() {
                Ok(b) => Ok(Box::new(b)),
                // GNOME that is still starting has no layer-shell either: say what it needs.
                Err(layer) => match linux::gnome::GnomeBorder::new() {
                    Ok(b) => Ok(Box::new(b)),
                    Err(gnome) => Err(format!("{layer} (GNOME: {gnome})")),
                },
            };
        }
        return Ok(Box::new(linux::BorderOverlay::new()?));
    }
    #[cfg(target_os = "macos")]
    {
        return Ok(Box::new(macos::BorderOverlay::new()?));
    }
    #[allow(unreachable_code)]
    Err(format!(
        "no on-screen indicator for {}",
        std::env::consts::OS
    ))
}

/// True while the host's lock screen is up and its picture is withheld from viewers,
/// who then show a "locked" notice. Windows and GNOME stream the lock screen (Windows:
/// lock and sign-in screens; GNOME: the border extension stays on there), so there it is
/// always false. macOS doesn't let a user's agent capture or type into the lock screen.
pub fn screen_locked() -> bool {
    #[cfg(target_os = "macos")]
    {
        macos::screen_locked()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
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
/// scheduler has no console, and launchd points a macOS LaunchAgent's at `/dev/null`, so the
/// agent logs to a file instead. Always true on Linux (systemd keeps the journal).
pub fn has_std_error() -> bool {
    #[cfg(target_os = "windows")]
    {
        windows::has_std_error()
    }
    #[cfg(target_os = "macos")]
    {
        macos::has_std_error()
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        true
    }
}
