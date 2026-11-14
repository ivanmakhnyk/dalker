//! The user file transfers run as.
//!
//! Under GNOME on Wayland the agent already runs as the signed-in user. On an X11 display it
//! runs as root: the transfer thread then takes the desktop user's file system identity
//! (fsuid, fsgid and groups, which Linux keeps per thread), so files are read and created
//! with that user's permissions and owned by them. Root's file capabilities are dropped
//! while the fsuid is not 0.

use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};
use std::process::Command;

use dalker_core::{FileAccess, PlatformError};

struct Account {
    name: String,
    uid: libc::uid_t,
    gid: libc::gid_t,
    home: PathBuf,
}

fn account_by_uid(uid: libc::uid_t) -> Option<Account> {
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe { libc::getpwuid_r(uid, &mut pw, buf.as_mut_ptr(), buf.len(), &mut out) };
    (r == 0 && !out.is_null()).then(|| unsafe { account_from(&pw) })
}

fn account_by_name(name: &str) -> Option<Account> {
    let cname = CString::new(name).ok()?;
    let mut buf = vec![0 as libc::c_char; 16 * 1024];
    let mut pw: libc::passwd = unsafe { std::mem::zeroed() };
    let mut out: *mut libc::passwd = std::ptr::null_mut();
    let r = unsafe {
        libc::getpwnam_r(
            cname.as_ptr(),
            &mut pw,
            buf.as_mut_ptr(),
            buf.len(),
            &mut out,
        )
    };
    (r == 0 && !out.is_null()).then(|| unsafe { account_from(&pw) })
}

unsafe fn account_from(pw: &libc::passwd) -> Account {
    Account {
        name: CStr::from_ptr(pw.pw_name).to_string_lossy().into_owned(),
        uid: pw.pw_uid,
        gid: pw.pw_gid,
        home: PathBuf::from(CStr::from_ptr(pw.pw_dir).to_string_lossy().into_owned()),
    }
}

/// The user of the active graphical session on seat0 (not the login screen), from logind.
fn desktop_user() -> Option<String> {
    let out = Command::new("loginctl")
        .args(["list-sessions", "--no-legend"])
        .output()
        .ok()?;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        // SESSION UID USER SEAT ...
        if cols.len() < 4 || cols[3] != "seat0" {
            continue;
        }
        let show = Command::new("loginctl")
            .args([
                "show-session",
                cols[0],
                "-p",
                "Class",
                "-p",
                "Active",
                "-p",
                "Type",
                "-p",
                "Name",
            ])
            .output()
            .ok()?;
        let props = String::from_utf8_lossy(&show.stdout);
        let get = |k: &str| {
            props
                .lines()
                .find_map(|l| l.strip_prefix(k)?.strip_prefix('='))
                .unwrap_or("")
        };
        if get("Class") == "user"
            && get("Active") == "yes"
            && matches!(get("Type"), "x11" | "wayland")
        {
            let name = get("Name");
            return Some(if name.is_empty() {
                cols[2].to_string()
            } else {
                name.to_string()
            });
        }
    }
    None
}

/// The user's Downloads folder: `XDG_DOWNLOAD_DIR` from `~/.config/user-dirs.dirs`, else
/// `~/Downloads`. Read as the user.
fn downloads_dir(home: &Path) -> PathBuf {
    let dirs = std::fs::read_to_string(home.join(".config/user-dirs.dirs")).unwrap_or_default();
    for line in dirs.lines() {
        let Some(v) = line.trim().strip_prefix("XDG_DOWNLOAD_DIR=") else {
            continue;
        };
        let v = v.trim().trim_matches('"');
        let path = match v.strip_prefix("$HOME") {
            Some(rest) => home.join(rest.trim_start_matches('/')),
            None => PathBuf::from(v),
        };
        // `$HOME/` alone means the desktop has no Downloads folder; keep the default.
        if path.is_absolute() && path != home {
            return path;
        }
    }
    home.join("Downloads")
}

/// Restores root's file system identity on the thread when dropped.
struct FsIdentity {
    groups: Vec<libc::gid_t>,
}

impl FsIdentity {
    /// Switches this thread's file system identity to `acct`. The raw syscalls change the
    /// calling thread only; glibc's wrappers would change every thread of the agent.
    fn assume(acct: &Account) -> Result<Self, String> {
        let saved = unsafe {
            let n = libc::getgroups(0, std::ptr::null_mut());
            let mut g = vec![0; n.max(0) as usize];
            let n = libc::getgroups(g.len() as libc::c_int, g.as_mut_ptr());
            g.truncate(n.max(0) as usize);
            g
        };
        let groups = user_groups(acct);
        let me = Self { groups: saved };
        unsafe {
            if libc::syscall(libc::SYS_setgroups, groups.len(), groups.as_ptr()) != 0 {
                return Err(format!("setgroups: {}", std::io::Error::last_os_error()));
            }
            libc::setfsgid(acct.gid);
            libc::setfsuid(acct.uid);
            // Both return the previous id; asking with an invalid one reads the current.
            let uid_now = libc::setfsuid(u32::MAX) as libc::uid_t;
            let gid_now = libc::setfsgid(u32::MAX) as libc::gid_t;
            if uid_now != acct.uid || gid_now != acct.gid {
                return Err("could not switch to the user's identity".into());
            }
        }
        Ok(me)
    }
}

impl Drop for FsIdentity {
    fn drop(&mut self) {
        unsafe {
            libc::setfsuid(0);
            libc::setfsgid(0);
            libc::syscall(libc::SYS_setgroups, self.groups.len(), self.groups.as_ptr());
        }
    }
}

fn user_groups(acct: &Account) -> Vec<libc::gid_t> {
    let Ok(name) = CString::new(acct.name.as_str()) else {
        return vec![acct.gid];
    };
    let mut n: libc::c_int = 64;
    loop {
        let mut groups = vec![0 as libc::gid_t; n as usize];
        let mut count = n;
        let r =
            unsafe { libc::getgrouplist(name.as_ptr(), acct.gid, groups.as_mut_ptr(), &mut count) };
        if r >= 0 {
            groups.truncate(count as usize);
            return groups;
        }
        if count <= n {
            return vec![acct.gid];
        }
        n = count;
    }
}

pub fn open_file_access() -> Result<FileAccess, PlatformError> {
    let euid = unsafe { libc::geteuid() };
    if euid != 0 {
        // Already the user (GNOME on Wayland, or started by hand).
        let acct =
            account_by_uid(euid).ok_or_else(|| PlatformError::Fatal("unknown user".into()))?;
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or(acct.home);
        return Ok(FileAccess {
            user: acct.name,
            downloads: downloads_dir(&home),
            home,
            guard: Box::new(()),
        });
    }
    let name = desktop_user()
        .ok_or_else(|| PlatformError::Transient("Nobody is signed in on the host.".into()))?;
    let acct = account_by_name(&name)
        .ok_or_else(|| PlatformError::Fatal(format!("unknown user {name}")))?;
    if acct.uid == 0 {
        return Err(PlatformError::Unsupported(
            "The host's desktop runs as root; file transfer is off.".into(),
        ));
    }
    let guard = FsIdentity::assume(&acct).map_err(PlatformError::Fatal)?;
    let downloads = downloads_dir(&acct.home);
    Ok(FileAccess {
        user: acct.name,
        home: acct.home,
        downloads,
        guard: Box::new(guard),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xdg_downloads_folder() {
        let home = std::env::temp_dir().join(format!("dalker-xdg-{}", std::process::id()));
        std::fs::create_dir_all(home.join(".config")).unwrap();
        assert_eq!(downloads_dir(&home), home.join("Downloads"));
        std::fs::write(
            home.join(".config/user-dirs.dirs"),
            "# x\nXDG_DOWNLOAD_DIR=\"$HOME/Téléchargements\"\n",
        )
        .unwrap();
        assert_eq!(downloads_dir(&home), home.join("Téléchargements"));
        std::fs::write(
            home.join(".config/user-dirs.dirs"),
            "XDG_DOWNLOAD_DIR=\"$HOME/\"\n",
        )
        .unwrap();
        assert_eq!(downloads_dir(&home), home.join("Downloads"));
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn current_user_when_not_root() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let a = open_file_access().unwrap();
        assert!(!a.user.is_empty());
    }
}
