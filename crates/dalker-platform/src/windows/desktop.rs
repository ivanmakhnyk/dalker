//! Following the *input desktop*: the one that currently receives keyboard and mouse
//! input and is shown on screen. It is `Default` for the signed-in user's desktop and
//! `Winlogon` for the sign-in screen, the lock screen and UAC prompts (secure desktop).
//!
//! Capture, injection and the border window all work on the calling thread's desktop,
//! so each of those threads attaches itself to the input desktop whenever it changes.
//! Only a process running as SYSTEM (the agent started by the Dalker service) may
//! open `Winlogon`; for a normal user process opening it fails and nothing changes.

use std::cell::RefCell;
use std::sync::Mutex;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::StationsAndDesktops::{
    CloseDesktop, GetUserObjectInformationW, OpenInputDesktop, SetThreadDesktop, DESKTOP_ACCESS_FLAGS,
    DESKTOP_CONTROL_FLAGS, HDESK, UOI_NAME,
};

/// `GENERIC_ALL`: create windows, hook, read the screen, send input.
const DESKTOP_ALL: DESKTOP_ACCESS_FLAGS = DESKTOP_ACCESS_FLAGS(0x1000_0000);

thread_local! {
    /// The desktop this thread was attached to by [`attach_thread_to_input_desktop`].
    /// Kept open: a desktop that a thread uses must not be closed.
    static ATTACHED: RefCell<Option<(HDESK, String)>> = const { RefCell::new(None) };
}

/// Desktop on which the red border is currently visible (see [`set_border_desktop`]).
static BORDER_DESKTOP: Mutex<Option<String>> = Mutex::new(None);

fn desktop_name(desk: HDESK) -> Option<String> {
    let mut buf = [0u16; 128];
    let mut needed = 0u32;
    unsafe {
        GetUserObjectInformationW(
            HANDLE(desk.0),
            UOI_NAME,
            Some(buf.as_mut_ptr().cast()),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .ok()?;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    Some(String::from_utf16_lossy(&buf[..len]))
}

/// Name of the current input desktop, or `None` if this process may not open it
/// (a non-SYSTEM process while the secure desktop is shown).
pub fn input_desktop_name() -> Option<String> {
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0)).ok()?;
        let name = desktop_name(desk);
        let _ = CloseDesktop(desk);
        name
    }
}

/// Name of the desktop the calling thread was last attached to by this module.
pub fn thread_desktop_name() -> Option<String> {
    ATTACHED.with(|a| a.borrow().as_ref().map(|(_, n)| n.clone()))
}

/// Attaches the calling thread to the current input desktop (no-op if it already is)
/// and returns that desktop's name.
///
/// Fails if the input desktop cannot be opened, or if the thread still owns windows or
/// hooks on its current desktop (destroy them first).
pub fn attach_thread_to_input_desktop() -> Result<String, String> {
    unsafe {
        let desk = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ALL)
            .map_err(|e| format!("open input desktop: {e}"))?;
        let Some(name) = desktop_name(desk) else {
            let _ = CloseDesktop(desk);
            return Err("input desktop has no name".into());
        };
        if thread_desktop_name().as_deref() == Some(name.as_str()) {
            let _ = CloseDesktop(desk);
            return Ok(name);
        }
        if let Err(e) = SetThreadDesktop(desk) {
            let _ = CloseDesktop(desk);
            return Err(format!("attach thread to desktop {name}: {e}"));
        }
        let previous = ATTACHED.with(|a| a.borrow_mut().replace((desk, name.clone())));
        if let Some((old, _)) = previous {
            let _ = CloseDesktop(old);
        }
        Ok(name)
    }
}

/// True while this process's session is locked.
///
/// The first lock-screen view (clock and picture, `LockApp`) is drawn on the *Default*
/// desktop, above all topmost windows (it covers the red border there; that one screen is
/// streamed anyway). It ignores injected mouse clicks, so the injector sends a Ctrl tap
/// with a click on it to bring up the sign-in prompt (`Winlogon` desktop).
pub fn session_locked() -> bool {
    use windows::core::PWSTR;
    use windows::Win32::System::RemoteDesktop::{
        WTSFreeMemory, WTSQuerySessionInformationW, WTSSessionInfoEx, WTSINFOEXW, WTS_CURRENT_SESSION,
        WTS_SESSIONSTATE_LOCK,
    };
    unsafe {
        let mut buf = PWSTR::null();
        let mut len = 0u32;
        if WTSQuerySessionInformationW(None, WTS_CURRENT_SESSION, WTSSessionInfoEx, &mut buf, &mut len).is_err()
            || buf.is_null()
        {
            return false;
        }
        let locked = if len as usize >= std::mem::size_of::<WTSINFOEXW>() {
            let info = &*(buf.0 as *const WTSINFOEXW);
            info.Level == 1 && info.Data.WTSInfoExLevel1.SessionFlags as u32 == WTS_SESSIONSTATE_LOCK
        } else {
            false
        };
        WTSFreeMemory(buf.0.cast());
        locked
    }
}

/// Locks the workstation for the current interactive session.
///
/// Running under the service (LocalSystem in the console session), `LockWorkStation()`
/// requires acting as the logged-on user (`WTSQueryUserToken`), because SYSTEM is not
/// an interactive user. Running by hand as a normal user, the process is already that user.
pub fn lock_workstation() -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
    use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
    use windows::Win32::System::Shutdown::LockWorkStation;
    use windows::Win32::System::Threading::GetCurrentProcessId;

    unsafe {
        let mut session = 0u32;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_ok() {
            let mut token = HANDLE::default();
            if WTSQueryUserToken(session, &mut token).is_ok() {
                let impersonated = ImpersonateLoggedOnUser(token).is_ok();
                let lock_res = LockWorkStation();
                if impersonated {
                    let _ = RevertToSelf();
                }
                let _ = CloseHandle(token);
                return lock_res.map_err(|e| format!("LockWorkStation failed: {e}"));
            }
        }

        // Running by hand (already the logged-in user) or WTSQueryUserToken not available:
        LockWorkStation().map_err(|e| format!("LockWorkStation failed: {e}"))
    }
}

/// Records on which desktop the border is visible (`None`: hidden). Capture only
/// publishes frames of the desktop that currently shows the border.
pub(crate) fn set_border_desktop(name: Option<String>) {
    *BORDER_DESKTOP.lock().unwrap_or_else(|e| e.into_inner()) = name;
}

pub(crate) fn border_shown_on(name: &str) -> bool {
    BORDER_DESKTOP.lock().unwrap_or_else(|e| e.into_inner()).as_deref() == Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn border_gate_matches_desktop_name() {
        set_border_desktop(Some("Winlogon".into()));
        assert!(border_shown_on("Winlogon"));
        assert!(!border_shown_on("Default"));
        set_border_desktop(None);
        assert!(!border_shown_on("Winlogon"));
    }

    #[test]
    fn test_session_is_not_locked() {
        // Tests run in an unlocked interactive session (or session 0, never locked).
        assert!(!session_locked());
    }

    #[test]
    fn interactive_test_process_sees_an_input_desktop() {
        // Tests run in an interactive session (CI agents included), where the input
        // desktop is readable; skip quietly when it is not (service session 0).
        if let Some(name) = input_desktop_name() {
            assert!(!name.is_empty());
            assert_eq!(attach_thread_to_input_desktop().as_deref(), Ok(name.as_str()));
            assert_eq!(thread_desktop_name(), Some(name));
        }
    }
}
