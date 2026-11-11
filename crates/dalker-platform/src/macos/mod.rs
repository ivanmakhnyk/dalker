//! macOS backends: ScreenCaptureKit capture, CGEvent input, an AppKit border overlay,
//! `NSPasteboard` clipboard and a menu bar icon. Needs macOS 14 or later.
//!
//! AppKit only works on the main thread while `NSApplication` runs there, so on macOS the
//! agent hands its main thread over with [`run_app`] and runs everything else elsewhere.
//! The pieces here that touch AppKit (border, menu bar) hop to the main thread themselves.
//!
//! Permissions the host user must grant (System Settings › Privacy & Security):
//! * **Screen Recording** for capture ([`permissions::screen_recording`]).
//! * **Accessibility** for input ([`permissions::accessibility`]).

mod capture;
mod clipboard;
pub mod files;
mod input;
mod main_thread;
mod menubar;
mod overlay;
pub mod permissions;

pub use capture::SckCapturer;
pub use clipboard::MacClipboard;
pub use input::CgInjector;
pub use main_thread::run_app;
pub use menubar::{MenuBar, MenuBarConfig};
pub use overlay::BorderOverlay;

/// True while the login window's lock screen is up (`CGSSessionScreenIsLocked` in the
/// current session's dictionary).
pub fn screen_locked() -> bool {
    use objc2_core_foundation::{CFBoolean, CFDictionary, CFRetained, CFString, CFType};
    let Some(dict) = objc2_core_graphics::CGSessionCopyCurrentDictionary() else {
        return false;
    };
    // SAFETY: the session dictionary maps CFString keys to CF objects.
    let dict: CFRetained<CFDictionary<CFString, CFType>> =
        unsafe { CFRetained::cast_unchecked(dict) };
    dict.get(&CFString::from_static_str("CGSSessionScreenIsLocked"))
        .and_then(|v| v.downcast::<CFBoolean>().ok())
        .is_some_and(|b| b.as_bool())
}

/// False when standard error is `/dev/null` (how launchd starts a LaunchAgent unless told
/// otherwise), so the agent writes its log to a file instead.
pub fn has_std_error() -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(null) = std::fs::metadata("/dev/null") else {
        return true;
    };
    // SAFETY: fstat on fd 2 with a zeroed stat buffer.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(2, &mut st) } != 0 {
        return false;
    }
    !(st.st_rdev as u64 == null.rdev() && st.st_ino as u64 == null.ino())
}
