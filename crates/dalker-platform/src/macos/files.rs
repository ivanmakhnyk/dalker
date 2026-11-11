//! The host user that viewers' file transfers run as. On macOS the agent runs inside the
//! user's own session (a LaunchAgent), so that is simply the agent's own account.

use std::path::PathBuf;

use dalker_core::FileAccess;

/// The account this process runs as, with its home and Downloads folders.
pub fn open_file_access() -> Result<FileAccess, String> {
    // SAFETY: getuid never fails; getpwuid returns null or a pointer valid until the next
    // call on this thread, and is read right away.
    let (user, home) = unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if pw.is_null() {
            return Err("unknown user".into());
        }
        let name = std::ffi::CStr::from_ptr((*pw).pw_name)
            .to_string_lossy()
            .into_owned();
        let dir = std::ffi::CStr::from_ptr((*pw).pw_dir)
            .to_string_lossy()
            .into_owned();
        (name, PathBuf::from(dir))
    };
    if user == "root" {
        return Err("file transfers need a signed-in user, not root".into());
    }
    let downloads = home.join("Downloads");
    Ok(FileAccess {
        user,
        downloads,
        home,
        guard: Box::new(()),
    })
}
