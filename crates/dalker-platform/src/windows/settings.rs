//! Helpers for the host's settings window (`dalkerd settings`, drawn by the agent).
//!
//! The config file is writable by administrators only, so the agent relaunches itself
//! elevated ([`relaunch_elevated`]) before showing the window.

use windows::core::{w, PCWSTR};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, MessageBoxW, ASFW_ANY, MB_ICONERROR, MB_OK, MB_SETFOREGROUND,
    SW_SHOWNORMAL,
};

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Shows an error message box (no parent window).
pub fn error_box(text: &str) {
    let text = wide(text);
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            w!("Dalker Settings"),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

/// Lets another process (the settings window that is already open) bring its window to the
/// front: Windows only allows that to the process the user just started.
pub fn allow_foreground() {
    unsafe {
        let _ = AllowSetForegroundWindow(ASFW_ANY);
    }
}

/// Starts this program again with administrator rights (UAC prompt) and the given arguments.
/// Returns an error if it could not be started, including when the user says no.
pub fn relaunch_elevated(args: &str) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("own path: {e}"))?;
    let exe = wide(&exe.display().to_string());
    let args = wide(args);
    let r = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe.as_ptr()),
            PCWSTR(args.as_ptr()),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // Values above 32 mean success.
    if r.0 as isize > 32 {
        Ok(())
    } else {
        Err(format!("ShellExecute runas failed ({})", r.0 as isize))
    }
}
