//! Where a viewer's file transfers go, and as whom they run, plus shared file operations
//! (listing, path containment, safe upload naming) used by both WebRTC and MCP.

use std::any::Any;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::PlatformError;

/// Most entries a folder listing returns.
pub const MAX_ENTRIES: usize = 5000;

/// Opens the user that file operations run as. Called on the worker thread, again after a
/// failure (nobody signed in yet).
pub type FileAccessFactory = Arc<dyn Fn() -> Result<FileAccess, PlatformError> + Send + Sync>;

/// The host user a viewer's file transfers run as: uploads land in `downloads`, downloads come
/// from below `home`.
///
/// It is opened on the thread that does the transfers and acts for that thread only: while it
/// is alive, files are read and created with the user's own permissions (never the agent's,
/// which may be SYSTEM or root). Drop it on the same thread.
pub struct FileAccess {
    /// Account name, for display.
    pub user: String,
    pub home: PathBuf,
    pub downloads: PathBuf,
    /// Keeps the thread acting as `user` until dropped.
    pub guard: Box<dyn Any>,
}

impl std::fmt::Debug for FileAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FileAccess")
            .field("user", &self.user)
            .field("home", &self.home)
            .field("downloads", &self.downloads)
            .finish_non_exhaustive()
    }
}

/// One entry in a folder listing below the user's home directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub name: String,
    pub dir: bool,
    pub size: u64,
    pub mtime: Option<u64>,
    pub hidden: bool,
}

/// A file name from a viewer/agent made safe for the host: no folders, no characters Windows
/// forbids, not a reserved device name, at most 200 bytes. `None` when nothing is left.
pub fn clean_name(name: &str) -> Option<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let mut s: String = base
        .chars()
        .map(|c| {
            if c.is_control() || r#"<>:"/\|?*"#.contains(c) {
                '_'
            } else {
                c
            }
        })
        .collect();
    s = s.trim().trim_end_matches(['.', ' ']).to_string();
    if s.is_empty() || s.chars().all(|c| c == '.') {
        return None;
    }
    let stem = s
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_ascii_uppercase();
    let reserved = matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (stem.len() == 4
            && (stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem.as_bytes()[3].is_ascii_digit());
    if reserved {
        s.insert(0, '_');
    }
    if s.len() > 200 {
        // Keep the extension when it is short.
        let ext = Path::new(&s)
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .filter(|e| e.len() <= 16);
        let keep = 200 - ext.as_ref().map_or(0, |e| e.len() + 1);
        let mut cut = keep;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        let mut short = s[..cut].trim_end_matches(['.', ' ']).to_string();
        if let Some(e) = ext {
            short.push('.');
            short.push_str(&e);
        }
        s = short;
    }
    Some(s)
}

/// `name (n).ext` for the n-th clash.
fn numbered(name: &str, n: u32) -> String {
    let p = Path::new(name);
    match (p.file_stem(), p.extension()) {
        (Some(stem), Some(ext)) if !stem.is_empty() => {
            format!("{} ({n}).{}", stem.to_string_lossy(), ext.to_string_lossy())
        }
        _ => format!("{name} ({n})"),
    }
}

/// Creates a new file for `name` in `dir` (created if missing) without replacing anything:
/// on a clash it becomes `name (1).ext`, `name (2).ext`…
pub fn create_unique(dir: &Path, name: &str) -> Result<(File, PathBuf, String), String> {
    let name = clean_name(name).ok_or("the file has no usable name")?;
    fs::create_dir_all(dir).map_err(|e| format!("can't open {}: {e}", dir.display()))?;
    for n in 0..1000u32 {
        let candidate = if n == 0 {
            name.clone()
        } else {
            numbered(&name, n)
        };
        let path = dir.join(&candidate);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((f, path, candidate)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("can't create {}: {e}", path.display())),
        }
    }
    Err(format!("too many files named like {name}"))
}

/// The real path of `rel` (folders separated by `/` or `\`) below `home`. Refuses anything
/// that leads outside it, through `..`, a drive or a link.
pub fn resolve(home: &Path, rel: &str) -> Result<PathBuf, String> {
    let mut p = home.to_path_buf();
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." {
            continue;
        }
        let mut comps = Path::new(part).components();
        match (comps.next(), comps.next()) {
            (Some(Component::Normal(_)), None) if !part.contains(':') => p.push(part),
            _ => return Err("not a folder in your home folder".into()),
        }
    }
    let home = home
        .canonicalize()
        .map_err(|e| format!("can't open the home folder: {e}"))?;
    let real = p.canonicalize().map_err(|e| e.to_string())?;
    if !real.starts_with(&home) {
        return Err("that is outside your home folder".into());
    }
    Ok(real)
}

/// Whether the file browser hides `name` while "Show hidden files" is off.
///
/// `own` is the entry itself (not followed) and `target` is what it points to. On
/// Windows both count: the legacy junctions in the home folder (`Application Data`,
/// `Cookies`, `NetHood`, ...) are hidden and system themselves while their targets
/// are not, so looking only at the target listed them.
fn is_hidden(name: &str, own: Option<&fs::Metadata>, target: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN_OR_SYSTEM: u32 = 0x2 | 0x4;
        let _ = name;
        own.into_iter()
            .chain([target])
            .any(|m| m.file_attributes() & HIDDEN_OR_SYSTEM != 0)
    }
    #[cfg(not(windows))]
    {
        let _ = (own, target);
        name.starts_with('.')
    }
}

/// The entries of the folder `rel` below `home`: folders first, then by name.
pub fn list(home: &Path, rel: &str) -> Result<Vec<FileEntry>, String> {
    let dir = resolve(home, rel)?;
    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir).map_err(|e| e.to_string())? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        // Follows links, so a link to a folder lists as one.
        let own = entry.metadata().ok();
        let Ok(meta) = fs::metadata(entry.path()).or_else(|e| own.clone().ok_or(e)) else {
            continue;
        };
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs());
        entries.push((
            meta.is_dir(),
            name.to_lowercase(),
            FileEntry {
                name,
                dir: meta.is_dir(),
                size: if meta.is_dir() { 0 } else { meta.len() },
                mtime,
                hidden: is_hidden(&entry.file_name().to_string_lossy(), own.as_ref(), &meta),
            },
        ));
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    Ok(entries.into_iter().map(|(_, _, e)| e).collect())
}

/// Opens a regular file at `rel` below `home` for reading, returning `(file, filename, size)`.
pub fn open_file(home: &Path, rel: &str) -> Result<(File, String, u64), String> {
    let path = resolve(home, rel)?;
    let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("only files can be downloaded".into());
    }
    let file = File::open(&path).map_err(|e| e.to_string())?;
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".into());
    Ok((file, name, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = std::env::temp_dir().join(format!(
            "dalker-core-files-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(clean_name("report.pdf").as_deref(), Some("report.pdf"));
        assert_eq!(clean_name("../../etc/passwd").as_deref(), Some("passwd"));
        assert_eq!(
            clean_name(r"C:\Windows\evil.exe").as_deref(),
            Some("evil.exe")
        );
        assert_eq!(clean_name("a:b*c?.txt").as_deref(), Some("a_b_c_.txt"));
        assert_eq!(clean_name("CON.txt").as_deref(), Some("_CON.txt"));
        assert_eq!(clean_name("com1").as_deref(), Some("_com1"));
        assert_eq!(clean_name("trailing. . ").as_deref(), Some("trailing"));
        assert_eq!(clean_name(".."), None);
        assert_eq!(clean_name("dir/"), None);
        let long = format!("{}.zip", "é".repeat(150));
        let short = clean_name(&long).unwrap();
        assert!(short.len() <= 200 && short.ends_with(".zip"), "{short}");
    }

    #[test]
    fn clashes_get_numbered_and_nothing_is_overwritten() {
        let dir = temp_dir("unique");
        fs::write(dir.join("a.txt"), b"keep").unwrap();
        let (_, p1, n1) = create_unique(&dir, "a.txt").unwrap();
        let (_, _, n2) = create_unique(&dir, "a.txt").unwrap();
        let (_, _, n3) = create_unique(&dir, "noext").unwrap();
        let (_, _, n4) = create_unique(&dir, "noext").unwrap();
        assert_eq!(
            (n1.as_str(), n2.as_str(), n3.as_str(), n4.as_str()),
            ("a (1).txt", "a (2).txt", "noext", "noext (1)")
        );
        assert_eq!(p1, dir.join("a (1).txt"));
        assert_eq!(fs::read(dir.join("a.txt")).unwrap(), b"keep");
        // The Downloads folder is created when missing.
        let (_, p, _) = create_unique(&dir.join("Downloads"), "x").unwrap();
        assert!(p.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn paths_stay_inside_home() {
        let root = temp_dir("resolve");
        let home = root.join("home");
        fs::create_dir_all(home.join("Documents")).unwrap();
        fs::write(root.join("secret"), b"x").unwrap();
        fs::write(home.join("Documents/a.txt"), b"x").unwrap();
        assert!(resolve(&home, "").is_ok());
        assert!(resolve(&home, "Documents/a.txt").is_ok());
        assert!(resolve(&home, r"Documents\a.txt").is_ok());
        assert!(resolve(&home, "../secret").is_err());
        assert!(resolve(&home, "Documents/../../secret").is_err());
        assert!(
            resolve(&home, "/etc/passwd").is_err(),
            "absolute paths are taken below home"
        );
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("secret"), home.join("link")).unwrap();
            assert!(
                resolve(&home, "link").is_err(),
                "a link out of home is refused"
            );
        }
        fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_hidden_junction_lists_as_hidden() {
        use std::process::Command;
        let home = temp_dir("junction");
        fs::create_dir_all(home.join("Target")).unwrap();
        let ok = Command::new("cmd")
            .args(["/C", "mklink", "/J", "Link", "Target"])
            .current_dir(&home)
            .status()
            .unwrap()
            .success();
        assert!(ok, "mklink /J failed");
        let ok = Command::new("attrib")
            .args(["+H", "+S", "/L", "Link"])
            .current_dir(&home)
            .status()
            .unwrap()
            .success();
        assert!(ok, "attrib failed");

        let entries = list(&home, "").unwrap();
        let hidden = |n: &str| {
            entries
                .iter()
                .find(|e| e.name == n)
                .unwrap_or_else(|| panic!("{n} not listed"))
                .hidden
        };
        assert!(hidden("Link"));
        assert!(!hidden("Target"));
        fs::remove_dir(home.join("Link")).unwrap();
        fs::remove_dir_all(&home).unwrap();
    }
}
