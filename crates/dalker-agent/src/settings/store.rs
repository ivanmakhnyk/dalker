//! Where the settings window reads and writes the config file.
//!
//! * [`FileStore`]: this process may write the file (Windows, elevated; or a test config).
//! * [`HelperStore`] (Linux): the window runs as the desktop user, and a root helper started
//!   once through pkexec does the reading and writing for it: `dalkerd settings-helper`,
//!   which speaks one JSON object per line on stdin/stdout:
//!
//! ```text
//! -> {"op":"read"}                                   <- {"ok":true,"text":"<the file>"}
//! -> {"op":"edit","set":[["K","V"]],"remove":["K"]}  <- {"ok":true}
//!                                                    <- {"error":"why"}
//! ```
//!
//! Only `DALKER_*` keys can be written. Secrets never reach the helper: the window sends the
//! argon2 hashes of a new password or token, not the password or token itself.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::cli;

pub trait Store: Send {
    /// The whole config file (empty if there is none yet).
    fn read(&mut self) -> Result<String, String>;
    /// Sets and removes keys in one write.
    fn edit(&mut self, set: &[(String, String)], remove: &[String]) -> Result<(), String>;
    /// The file, for the window's footer.
    fn path(&self) -> &Path;
}

pub struct FileStore {
    path: PathBuf,
}

impl FileStore {
    pub fn new(path: &Path) -> Self {
        Self { path: path.to_owned() }
    }
}

impl Store for FileStore {
    fn read(&mut self) -> Result<String, String> {
        read_file(&self.path)
    }

    fn edit(&mut self, set: &[(String, String)], remove: &[String]) -> Result<(), String> {
        cli::edit_config(&self.path, set, remove).map_err(|e| format!("{e:#}"))
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn read_file(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(format!("can't read {}: {e}", path.display())),
    }
}

/// What the helper lets its caller change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Allowed {
    /// Any `DALKER_*` key (the settings window, after an administrator password).
    All,
    /// MCP token records only (the agent of a GNOME-on-Wayland session, for its web client).
    #[cfg_attr(not(unix), allow(dead_code))]
    Tokens,
}

impl Allowed {
    fn permits(self, key: &str) -> bool {
        match self {
            Allowed::All => key.starts_with("DALKER_"),
            Allowed::Tokens => {
                key.starts_with(dalker_mcp::tokens::CONFIG_KEY_PREFIX)
                    || key.starts_with(dalker_mcp::tokens::SECRET_KEY_PREFIX)
            }
        }
    }
}

/// `dalkerd settings-helper`: serves [`Store`] requests for `path` until stdin closes.
pub fn run_helper(path: &Path) -> anyhow::Result<()> {
    let stdin = std::io::stdin();
    serve_lines(path, Allowed::All, stdin.lock(), std::io::stdout().lock())
}

/// Answers one request per line of `input` until it ends.
fn serve_lines(path: &Path, allowed: Allowed, input: impl BufRead, mut out: impl Write) -> anyhow::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Ok(req) => handle(path, allowed, &req),
            Err(e) => json!({ "error": format!("bad request: {e}") }),
        };
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

/// `dalkerd settings-helper --listen <socket> --owner <user>` (Linux, run as root by the
/// launcher): lets the agent of a GNOME-on-Wayland session, which runs as the desktop user and
/// can't write the root-only config, change MCP tokens for its web client. The socket belongs to
/// that user alone (mode 0600). Runs until killed.
#[cfg(unix)]
pub fn run_socket_helper(path: &Path, socket: &Path, owner: &str) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let (uid, gid) = user_ids(owner).ok_or_else(|| anyhow::anyhow!("no such user: {owner}"))?;
    let _ = std::fs::remove_file(socket);
    let listener = std::os::unix::net::UnixListener::bind(socket)?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o600))?;
    std::os::unix::fs::chown(socket, Some(uid), Some(gid))?;
    for conn in listener.incoming() {
        let Ok(conn) = conn else { continue };
        let path = path.to_owned();
        std::thread::spawn(move || {
            let Ok(reader) = conn.try_clone() else { return };
            let _ = serve_lines(&path, Allowed::Tokens, std::io::BufReader::new(reader), conn);
        });
    }
    Ok(())
}

/// The uid and gid of a user name, from /etc/passwd.
#[cfg(unix)]
fn user_ids(name: &str) -> Option<(u32, u32)> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|l| {
        let f: Vec<&str> = l.split(':').collect();
        (f.len() > 3 && f[0] == name).then(|| Some((f[2].parse().ok()?, f[3].parse().ok()?))).flatten()
    })
}

fn handle(path: &Path, allowed: Allowed, req: &Value) -> Value {
    let result = match req["op"].as_str() {
        Some("read") => read_file(path).map(|text| json!({ "ok": true, "text": text })),
        Some("edit") => parse_edit(req, allowed).and_then(|(set, remove)| {
            // No logging here: the log goes to stdout, which carries the answers.
            cli::edit_config(path, &set, &remove).map(|()| json!({ "ok": true })).map_err(|e| format!("{e:#}"))
        }),
        _ => Err("unknown request".into()),
    };
    result.unwrap_or_else(|e| json!({ "error": e }))
}

type Edit = (Vec<(String, String)>, Vec<String>);

fn parse_edit(req: &Value, allowed: Allowed) -> Result<Edit, String> {
    let bad = || "bad edit request".to_owned();
    let set = req["set"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|pair| match pair.as_array().map(|p| p.as_slice()) {
            Some([Value::String(k), Value::String(v)]) => Ok((k.clone(), v.clone())),
            _ => Err(bad()),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let remove = req["remove"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .map(|k| k.as_str().map(str::to_owned).ok_or_else(bad))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(k) = set.iter().map(|(k, _)| k).chain(&remove).find(|k| !allowed.permits(k)) {
        return Err(match allowed {
            Allowed::All => format!("only DALKER_* keys can be changed, not `{k}`"),
            Allowed::Tokens => format!("only MCP tokens can be changed here, not `{k}`"),
        });
    }
    Ok((set, remove))
}

/// Talks to `dalkerd settings-helper` running as root (started through pkexec).
#[cfg(not(windows))]
pub struct HelperStore {
    path: PathBuf,
    child: std::process::Child,
    stdin: std::process::ChildStdin,
    stdout: std::io::BufReader<std::process::ChildStdout>,
}

/// Installed by the .deb; pkexec's policy for it names Dalker in the password prompt.
#[cfg(not(windows))]
const HELPER: &str = "/usr/lib/dalker/dalker-settings-helper";

#[cfg(not(windows))]
impl HelperStore {
    /// Starts the helper (pkexec asks for an administrator password) and reads the file once,
    /// which also tells whether the password was accepted.
    pub fn start(path: &Path) -> Result<Self, String> {
        let mut cmd = std::process::Command::new("pkexec");
        if Path::new(HELPER).exists() {
            cmd.arg(HELPER);
        } else {
            // Not installed from the package (development): any polkit agent still asks.
            let exe = std::env::current_exe().map_err(|e| format!("own path: {e}"))?;
            cmd.arg(exe).arg("settings-helper");
        }
        cmd.arg("--config").arg(path);
        let mut child = cmd
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("can't start pkexec ({e}). Is polkit installed?"))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = std::io::BufReader::new(child.stdout.take().expect("piped stdout"));
        let mut store = Self { path: path.to_owned(), child, stdin, stdout };
        store.read()?;
        Ok(store)
    }

    fn call(&mut self, req: Value) -> Result<Value, String> {
        let gone = |child: &mut std::process::Child| match child.wait().map(|s| s.code()) {
            // pkexec: 126 = authentication dismissed or refused, 127 = not authorized.
            Ok(Some(126 | 127)) => "Administrator authentication was cancelled or refused.".to_owned(),
            Ok(code) => format!("The settings helper stopped (exit code {code:?})."),
            Err(e) => format!("The settings helper stopped: {e}"),
        };
        if writeln!(self.stdin, "{req}").and_then(|()| self.stdin.flush()).is_err() {
            return Err(gone(&mut self.child));
        }
        let mut line = String::new();
        match self.stdout.read_line(&mut line) {
            Ok(n) if n > 0 => {}
            _ => return Err(gone(&mut self.child)),
        }
        let reply: Value = serde_json::from_str(&line).map_err(|e| format!("bad answer from the settings helper: {e}"))?;
        match reply["error"].as_str() {
            Some(e) => Err(e.to_owned()),
            None => Ok(reply),
        }
    }
}

#[cfg(not(windows))]
impl Store for HelperStore {
    fn read(&mut self) -> Result<String, String> {
        let reply = self.call(json!({ "op": "read" }))?;
        reply["text"].as_str().map(str::to_owned).ok_or_else(|| "bad answer from the settings helper".into())
    }

    fn edit(&mut self, set: &[(String, String)], remove: &[String]) -> Result<(), String> {
        self.call(json!({ "op": "edit", "set": set, "remove": remove })).map(|_| ())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(not(windows))]
impl Drop for HelperStore {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Talks to the launcher's root helper over its socket (the agent of a GNOME-on-Wayland
/// session; MCP tokens only). One connection per request.
#[cfg(unix)]
pub struct SocketStore {
    socket: PathBuf,
    path: PathBuf,
}

#[cfg(unix)]
impl SocketStore {
    pub fn new(socket: &Path, path: &Path) -> Self {
        Self { socket: socket.to_owned(), path: path.to_owned() }
    }

    fn call(&self, req: Value) -> Result<Value, String> {
        let mut conn = std::os::unix::net::UnixStream::connect(&self.socket)
            .map_err(|e| format!("the Dalker service's settings helper is not reachable ({e})"))?;
        let _ = conn.set_read_timeout(Some(std::time::Duration::from_secs(10)));
        writeln!(conn, "{req}").map_err(|e| e.to_string())?;
        let mut line = String::new();
        std::io::BufReader::new(&conn).read_line(&mut line).map_err(|e| e.to_string())?;
        let reply: Value = serde_json::from_str(&line).map_err(|e| format!("bad answer from the settings helper: {e}"))?;
        match reply["error"].as_str() {
            Some(e) => Err(e.to_owned()),
            None => Ok(reply),
        }
    }
}

#[cfg(unix)]
impl Store for SocketStore {
    fn read(&mut self) -> Result<String, String> {
        let reply = self.call(json!({ "op": "read" }))?;
        reply["text"].as_str().map(str::to_owned).ok_or_else(|| "bad answer from the settings helper".into())
    }

    fn edit(&mut self, set: &[(String, String)], remove: &[String]) -> Result<(), String> {
        self.call(json!({ "op": "edit", "set": set, "remove": remove })).map(|_| ())
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn helper_requests() {
        let dir = std::env::temp_dir().join(format!("dalker-helper-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        assert_eq!(handle(&file, Allowed::All, &json!({"op": "read"})), json!({"ok": true, "text": ""}));
        let r = handle(&file, Allowed::All, &json!({"op": "edit", "set": [["DALKER_WEB", "off"], ["DALKER_X", "1"]], "remove": []}));
        assert_eq!(r, json!({"ok": true}));
        let r = handle(&file, Allowed::All, &json!({"op": "edit", "set": [], "remove": ["DALKER_X"]}));
        assert_eq!(r, json!({"ok": true}));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "DALKER_WEB=off\n");
        let r = handle(&file, Allowed::All, &json!({"op": "edit", "set": [["PATH", "/tmp"]]}));
        assert!(r["error"].as_str().unwrap().contains("DALKER_"));
        let r = handle(&file, Allowed::All, &json!({"op": "edit", "set": [["DALKER_A", 1]]}));
        assert!(r["error"].is_string());
        assert!(handle(&file, Allowed::All, &json!({"op": "rm -rf"}))["error"].is_string());
        // The Wayland agent's helper: MCP tokens only.
        let r = handle(&file, Allowed::Tokens, &json!({"op": "edit", "set": [["DALKER_WEB", "on"]]}));
        assert!(r["error"].as_str().unwrap().contains("only MCP tokens"));
        let r = handle(&file, Allowed::Tokens, &json!({"op": "edit", "set": [["DALKER_MCP_SECRET_00000000", "x"]]}));
        assert_eq!(r, json!({"ok": true}));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
