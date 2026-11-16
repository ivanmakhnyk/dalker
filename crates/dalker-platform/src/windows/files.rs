//! The user file transfers run as.
//!
//! The installed agent runs as SYSTEM in the console session. The transfer thread
//! impersonates the user signed in to that session (`WTSQueryUserToken`), so files are read
//! and created with that user's permissions and owned by them; for an administrator that is
//! the normal, non-elevated token. Started by hand, the agent already is that user.

use std::path::PathBuf;

use dalker_core::{FileAccess, PlatformError};
use windows::core::GUID;
use windows::Win32::Foundation::{CloseHandle, ERROR_NO_TOKEN, ERROR_PRIVILEGE_NOT_HELD, HANDLE};
use windows::Win32::Security::{ImpersonateLoggedOnUser, RevertToSelf};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSQueryUserToken};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Shell::{
    FOLDERID_Downloads, FOLDERID_Profile, SHGetKnownFolderPath, KF_FLAG_DEFAULT,
};

/// Stops impersonating and closes the user's token when dropped.
struct Impersonation(HANDLE);

impl Drop for Impersonation {
    fn drop(&mut self) {
        unsafe {
            let _ = RevertToSelf();
            let _ = CloseHandle(self.0);
        }
    }
}

fn known_folder(id: &GUID, token: Option<HANDLE>) -> Result<PathBuf, String> {
    unsafe {
        let p = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, token)
            .map_err(|e| format!("known folder: {e}"))?;
        let path = p.to_string().map(PathBuf::from).map_err(|e| e.to_string());
        CoTaskMemFree(Some(p.0 as *const _));
        path
    }
}

fn account_name(home: &std::path::Path) -> String {
    home.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

pub fn open_file_access() -> Result<FileAccess, PlatformError> {
    unsafe {
        let mut session = 0u32;
        ProcessIdToSessionId(GetCurrentProcessId(), &mut session)
            .map_err(|e| PlatformError::Fatal(format!("session: {e}")))?;
        let mut token = HANDLE::default();
        match WTSQueryUserToken(session, &mut token) {
            Ok(()) => {
                let guard = Impersonation(token);
                let home =
                    known_folder(&FOLDERID_Profile, Some(token)).map_err(PlatformError::Fatal)?;
                let downloads = known_folder(&FOLDERID_Downloads, Some(token))
                    .unwrap_or_else(|_| home.join("Downloads"));
                ImpersonateLoggedOnUser(token).map_err(|e| {
                    PlatformError::Fatal(format!("can't act as the signed-in user: {e}"))
                })?;
                Ok(FileAccess {
                    user: account_name(&home),
                    home,
                    downloads,
                    guard: Box::new(guard),
                })
            }
            // Not SYSTEM: started by hand, so already the user.
            Err(e) if e.code() == ERROR_PRIVILEGE_NOT_HELD.to_hresult() => {
                let home = known_folder(&FOLDERID_Profile, None).map_err(PlatformError::Fatal)?;
                let downloads = known_folder(&FOLDERID_Downloads, None)
                    .unwrap_or_else(|_| home.join("Downloads"));
                Ok(FileAccess {
                    user: account_name(&home),
                    home,
                    downloads,
                    guard: Box::new(()),
                })
            }
            Err(e) if e.code() == ERROR_NO_TOKEN.to_hresult() => Err(PlatformError::Transient(
                "Nobody is signed in on the host.".into(),
            )),
            Err(e) => Err(PlatformError::Fatal(format!(
                "can't find the signed-in user: {e}"
            ))),
        }
    }
}
