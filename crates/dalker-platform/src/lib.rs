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

/// The kind of desktop the agent serves, which decides every backend: capture, border,
/// clipboard. Detected by [`Desktop::detect`], in one place, so the backends can't disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Desktop {
    Windows,
    Mac,
    /// GNOME on Wayland or Xorg: Mutter's screen cast and the Dalker Shell extension.
    Gnome,
    /// Any other Wayland desktop (KDE Plasma, sway, Hyprland...): the xdg-desktop-portal
    /// screen cast and a wlr-layer-shell border.
    PortalWayland,
    /// An X server (`$DISPLAY`) without GNOME.
    X11,
    /// No desktop to serve yet (a Linux service started before anyone signed in).
    None,
}

impl Desktop {
    /// The desktop of this process's session. On Linux it asks the session bus for GNOME,
    /// which may come up after the agent started, so call it again when retrying.
    pub fn detect() -> Desktop {
        #[cfg(target_os = "windows")]
        {
            return Desktop::Windows;
        }
        #[cfg(target_os = "macos")]
        {
            return Desktop::Mac;
        }
        #[cfg(target_os = "linux")]
        {
            // GNOME first (Wayland, or any session whose bus has Mutter's ScreenCast): under
            // Wayland the X11 root window is just an empty XWayland surface.
            return Desktop::linux(
                linux::gnome::gnome_session_available(),
                linux::is_wayland_session(),
                std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty()),
            );
        }
        #[allow(unreachable_code)]
        Desktop::None
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    fn linux(gnome: bool, wayland: bool, x_display: bool) -> Desktop {
        if gnome {
            Desktop::Gnome
        } else if wayland {
            Desktop::PortalWayland
        } else if x_display {
            Desktop::X11
        } else {
            Desktop::None
        }
    }

    /// The capture backend this desktop uses, as its capturer names itself.
    pub fn capture_name(self) -> &'static str {
        match self {
            Desktop::Windows => "dxgi",
            Desktop::Mac => "screencapturekit",
            Desktop::Gnome => "gnome-screencast",
            Desktop::PortalWayland => "portal-screencast",
            Desktop::X11 => "x11-shm",
            Desktop::None => "none",
        }
    }
}

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

/// Creates the native screen capturer for `desktop` (see [`Desktop::detect`]).
pub fn create_capturer(desktop: Desktop) -> Result<Box<dyn ScreenCapturer>, CaptureError> {
    match desktop {
        #[cfg(target_os = "windows")]
        Desktop::Windows => Ok(Box::new(windows::DxgiCapturer::new()?)),
        #[cfg(target_os = "macos")]
        Desktop::Mac => Ok(Box::new(macos::SckCapturer::new()?)),
        #[cfg(target_os = "linux")]
        Desktop::Gnome => Ok(Box::new(linux::gnome::GnomeCapturer::new()?)),
        #[cfg(target_os = "linux")]
        Desktop::PortalWayland => {
            // XWayland only shows X11 apps, so never pretend to capture the desktop with it.
            match linux::portal::PortalCapturer::new() {
                Ok(c) => Ok(Box::new(c)),
                Err(CaptureError::Unsupported(why)) => Err(CaptureError::Unsupported(format!(
                    "Wayland session without a usable screen cast (no GNOME; xdg-desktop-portal: {why})"
                ))),
                Err(e) => Err(e),
            }
        }
        #[cfg(target_os = "linux")]
        Desktop::X11 => Ok(Box::new(linux::X11Capturer::new()?)),
        Desktop::None => Err(CaptureError::Unsupported(
            "no desktop session yet (nobody signed in)".into(),
        )),
        #[allow(unreachable_patterns)]
        other => Err(CaptureError::Unsupported(format!(
            "{other:?} is not a desktop of {}",
            std::env::consts::OS
        ))),
    }
}

/// Names the capture backend when starting it only to see whether it works would bother
/// someone, so it should first be started when a viewer connects: the xdg-desktop-portal
/// screen cast may ask the person at the host to allow it. `None` = probing is harmless.
pub fn capture_backend_without_probe(desktop: Desktop) -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        if desktop == Desktop::PortalWayland && linux::portal::available() {
            return Some(desktop.capture_name());
        }
    }
    let _ = desktop;
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
pub fn create_clipboard(desktop: Desktop) -> Result<Box<dyn Clipboard>, String> {
    match desktop {
        #[cfg(target_os = "windows")]
        Desktop::Windows => Ok(Box::new(windows::WinClipboard::new()?)),
        #[cfg(target_os = "macos")]
        Desktop::Mac => Ok(Box::new(macos::MacClipboard::new()?)),
        #[cfg(target_os = "linux")]
        Desktop::Gnome => Ok(Box::new(linux::gnome::GnomeClipboard::new()?)),
        #[cfg(target_os = "linux")]
        Desktop::PortalWayland | Desktop::X11 | Desktop::None => {
            if linux::kde::klipper_available() {
                return Ok(Box::new(linux::kde::KlipperClipboard::new()?));
            }
            // Says what is missing (the GNOME extension).
            Ok(Box::new(linux::gnome::GnomeClipboard::new()?))
        }
        #[allow(unreachable_patterns)]
        _ => Err(format!("no clipboard support for {}", std::env::consts::OS)),
    }
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
pub fn create_indicator(desktop: Desktop) -> Result<Box<dyn ViewerIndicator>, String> {
    match desktop {
        #[cfg(target_os = "windows")]
        Desktop::Windows => Ok(Box::new(windows::BorderOverlay::new()?)),
        #[cfg(target_os = "macos")]
        Desktop::Mac => Ok(Box::new(macos::BorderOverlay::new()?)),
        // Under GNOME the border must come from the shell extension, and on other Wayland
        // desktops from a layer-shell surface: an X11 overlay would "succeed" against
        // XWayland yet never be visible, so there is no fallback to it.
        #[cfg(target_os = "linux")]
        Desktop::Gnome => Ok(Box::new(linux::gnome::GnomeBorder::new()?)),
        #[cfg(target_os = "linux")]
        Desktop::PortalWayland => match linux::wayland::LayerBorder::new() {
            Ok(b) => Ok(Box::new(b)),
            // GNOME that is still starting has no layer-shell either: say what it needs.
            Err(layer) => match linux::gnome::GnomeBorder::new() {
                Ok(b) => Ok(Box::new(b)),
                Err(gnome) => Err(format!("{layer} (GNOME: {gnome})")),
            },
        },
        #[cfg(target_os = "linux")]
        Desktop::X11 => Ok(Box::new(linux::BorderOverlay::new()?)),
        Desktop::None => Err("no desktop session yet (nobody signed in)".into()),
        #[allow(unreachable_patterns)]
        _ => Err(format!(
            "no on-screen indicator for {}",
            std::env::consts::OS
        )),
    }
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

#[cfg(test)]
mod tests {
    use super::Desktop;

    #[test]
    fn gnome_wins_then_wayland_then_x11() {
        assert_eq!(Desktop::linux(true, true, true), Desktop::Gnome);
        assert_eq!(
            Desktop::linux(true, false, true),
            Desktop::Gnome,
            "GNOME on Xorg"
        );
        assert_eq!(
            Desktop::linux(false, true, true),
            Desktop::PortalWayland,
            "not XWayland"
        );
        assert_eq!(Desktop::linux(false, false, true), Desktop::X11);
        assert_eq!(Desktop::linux(false, false, false), Desktop::None);
        assert_eq!(Desktop::Mac.capture_name(), "screencapturekit");
    }
}
