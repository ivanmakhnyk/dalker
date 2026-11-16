//! Windows backends: DXGI Desktop Duplication capture + `SendInput` injection.
//!
//! The installed agent is started by the `Dalker` service ([`service`]) as SYSTEM in
//! the console session, so capture, input and the border follow the input desktop
//! ([`desktop`]): the user's desktop as well as the sign-in screen, the lock screen and
//! UAC prompts (`Winlogon`), and input reaches elevated windows.
//!
//! Started by hand as a normal user (development), the agent only sees that user's
//! desktop: the secure desktop is not capturable (capture reports
//! [`dalker_core::CaptureError::Lost`] and retries until it is back), and UIPI keeps
//! input away from elevated windows.

mod capture;
mod clipboard;
pub mod desktop;
pub mod files;
mod input;
mod overlay;
mod pointer;
pub mod service;
pub mod settings;
mod tray;

pub use capture::DxgiCapturer;
pub use clipboard::WinClipboard;
pub use input::SendInputInjector;
pub use overlay::BorderOverlay;
pub use tray::{AiAgentsMenu, Tray, TrayConfig};

/// Session attached to the physical console, if one is active.
pub fn console_session_id() -> Option<u32> {
    use windows::Win32::System::RemoteDesktop::WTSGetActiveConsoleSessionId;

    let id = unsafe { WTSGetActiveConsoleSessionId() };
    (id != u32::MAX).then_some(id)
}

/// Returns the kernel version independently of the executable's compatibility manifest.
pub fn os_version() -> Option<(u32, u32, u32)> {
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    #[link(name = "ntdll")]
    extern "system" {
        fn RtlGetVersion(version: *mut OSVERSIONINFOW) -> i32;
    }

    let mut version = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    (unsafe { RtlGetVersion(&mut version) } == 0).then_some((
        version.dwMajorVersion,
        version.dwMinorVersion,
        version.dwBuildNumber,
    ))
}

use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};

/// Opts into per-monitor DPI awareness so desktop duplication sizes and
/// `SendInput` coordinates are both in physical pixels.
pub fn init_process() {
    // Fails harmlessly if awareness was already set (e.g. via manifest).
    if let Err(e) =
        unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) }
    {
        tracing::debug!("SetProcessDpiAwarenessContext: {e}");
    }
}

/// Makes `println!`/`eprintln!` work again for a GUI-subsystem build that was started from a
/// terminal: attaches to the parent console and points any missing standard handle at it.
/// Does nothing when there is no parent console (started by the task scheduler, Explorer...)
/// and leaves redirected handles (pipes, files) alone.
pub fn attach_parent_console() {
    use windows::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
    };
    use windows::Win32::System::Console::{
        AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE,
        STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS).is_err() {
            return;
        }
        let targets = [
            (
                STD_OUTPUT_HANDLE,
                windows::core::w!("CONOUT$"),
                GENERIC_WRITE.0,
            ),
            (
                STD_ERROR_HANDLE,
                windows::core::w!("CONOUT$"),
                GENERIC_WRITE.0,
            ),
            (
                STD_INPUT_HANDLE,
                windows::core::w!("CONIN$"),
                GENERIC_READ.0,
            ),
        ];
        for (std, name, access) in targets {
            let missing = GetStdHandle(std)
                .map(|h| h.is_invalid() || h.0.is_null())
                .unwrap_or(true);
            if missing {
                if let Ok(h) = CreateFileW(
                    name,
                    access,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    None,
                    OPEN_EXISTING,
                    FILE_ATTRIBUTE_NORMAL,
                    None,
                ) {
                    let _ = SetStdHandle(std, h);
                }
            }
        }
    }
}

/// Handles Ctrl+C / Ctrl+Break by ignoring them (a handler routine, unlike
/// `SetConsoleCtrlHandler(None, TRUE)`, is not inherited by child processes).
pub fn outlive_console_ctrl_c() {
    use windows::core::BOOL;
    use windows::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT};
    unsafe extern "system" fn handler(event: u32) -> BOOL {
        (event == CTRL_C_EVENT || event == CTRL_BREAK_EVENT).into()
    }
    unsafe {
        let _ = SetConsoleCtrlHandler(Some(handler), true);
    }
}

/// True when standard error points somewhere (a console, a pipe or a file).
pub fn has_std_error() -> bool {
    use windows::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE};
    unsafe {
        GetStdHandle(STD_ERROR_HANDLE)
            .map(|h| !h.is_invalid() && !h.0.is_null())
            .unwrap_or(false)
    }
}
