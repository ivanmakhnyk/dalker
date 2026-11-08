use std::io::{BufRead, IsTerminal};
use std::net::SocketAddr;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OnOff {
    On,
    Off,
}

impl OnOff {
    /// `on`/`off` as written in the config file (also accepts true/false, yes/no, 1/0).
    pub fn parse(s: &str) -> Option<bool> {
        match s.trim().to_ascii_lowercase().as_str() {
            "on" | "true" | "yes" | "1" => Some(true),
            "off" | "false" | "no" | "0" => Some(false),
            _ => None,
        }
    }

    pub fn word(on: bool) -> &'static str {
        if on {
            "on"
        } else {
            "off"
        }
    }
}

/// Where the web client is served unless the config says otherwise.
pub const DEFAULT_BIND: &str = "0.0.0.0:8742";
/// Where the MCP server listens unless the config says otherwise (this computer only; the
/// settings window's "Allow other computers" switch changes it to 0.0.0.0).
pub const DEFAULT_MCP_BIND: &str = "127.0.0.1:8743";

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CaptureMode {
    /// Real screen + input (DXGI/SendInput on Windows, X11/uinput on Linux).
    Native,
    /// Synthetic 30 FPS test pattern; input is drawn on it instead of injected.
    Mock,
}

/// Dalker host agent: remote desktop over WebRTC.
#[derive(Debug, Parser)]
#[command(name = "dalkerd", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Web client and signaling server: `off` stops it (AI agents can still use MCP).
    #[arg(long, value_enum, env = "DALKER_WEB", default_value = "on")]
    pub web: OnOff,

    /// HTTP listen address for the web client and signaling.
    #[arg(long, env = "DALKER_BIND", default_value = DEFAULT_BIND)]
    pub bind: SocketAddr,

    /// Argon2 PHC hash of the login password (preferred; see `hash-password`).
    #[arg(long, env = "DALKER_PASSWORD_HASH", hide_env_values = true)]
    pub password_hash: Option<String>,

    /// Plain login password (hashed at startup). Prefer --password-hash.
    #[arg(long, env = "DALKER_PASSWORD", hide_env_values = true)]
    pub password: Option<String>,

    /// `Secure` attribute of the login cookie: `auto` (only for requests a reverse proxy marks
    /// `X-Forwarded-Proto: https`), `on` or `off`. Set it in the config file for a TLS proxy.
    #[arg(long, env = "DALKER_COOKIE_SECURE", default_value = "auto", value_parser = parse_cookie_secure)]
    pub cookie_secure: dalker_web::CookieSecure,

    /// How long a login stays valid, in hours.
    #[arg(long, default_value_t = 12)]
    pub session_hours: u64,

    /// Capture backend.
    #[arg(long, value_enum, env = "DALKER_CAPTURE", default_value = "native")]
    pub capture: CaptureMode,

    #[arg(long, default_value_t = 1280)]
    pub mock_width: u32,
    #[arg(long, default_value_t = 720)]
    pub mock_height: u32,

    /// Maximum frames per second sent to viewers.
    #[arg(long, default_value_t = 30, value_parser = clap::value_parser!(u32).range(1..=120))]
    pub fps: u32,

    /// Target H.264 bitrate in kbit/s.
    #[arg(long, default_value_t = 6000)]
    pub bitrate_kbps: u32,

    /// Downscale screens wider than this (integer factor). 0 = always native.
    #[arg(long, default_value_t = 1920)]
    pub max_width: u32,

    /// Concurrent viewers allowed.
    #[arg(long, default_value_t = 2)]
    pub max_sessions: usize,

    /// Use a single UDP port for all WebRTC traffic (for firewalls / port forwards).
    #[arg(long, env = "DALKER_UDP_PORT")]
    pub udp_port: Option<u16>,

    /// Additional IP to advertise as a host ICE candidate (NAT 1:1). Repeatable.
    #[arg(long, env = "DALKER_PUBLIC_IP", value_delimiter = ',')]
    pub public_ip: Vec<String>,

    /// STUN/TURN server URL, e.g. stun:stun.l.google.com:19302. Repeatable.
    #[arg(long, value_delimiter = ',')]
    pub stun: Vec<String>,

    /// Do not show the notification-area (tray) icon (Windows).
    #[arg(long)]
    #[cfg_attr(not(windows), allow(dead_code))]
    pub no_tray: bool,

    /// MCP server for AI agents (Claude Desktop/Code...): `on` serves `/mcp` on --mcp-bind.
    /// Agents need a token (see `mcp-token create`).
    #[arg(long, value_enum, env = "DALKER_MCP", default_value = "off")]
    pub mcp: OnOff,

    /// Listen address of the MCP server. Loopback by default; 0.0.0.0:PORT lets other computers
    /// connect (the settings window's "Allow other computers" switch).
    #[arg(long, env = "DALKER_MCP_BIND", default_value = DEFAULT_MCP_BIND)]
    pub mcp_bind: SocketAddr,

    /// Allow a non-loopback --mcp-bind although bearer tokens then travel as plain HTTP (the
    /// default, so the settings switch works). `false` refuses to serve MCP on a network address.
    #[arg(long, env = "DALKER_MCP_ALLOW_PLAINTEXT", value_parser = clap::builder::BoolishValueParser::new(),
          num_args = 0..=1, default_missing_value = "true", default_value = "true")]
    pub mcp_allow_plaintext: bool,

    /// The address AI agents reach the MCP server at when a reverse proxy is in front of it,
    /// e.g. https://desk.example.com/mcp. Only shown in the settings window; it changes nothing
    /// else.
    #[arg(long, env = "DALKER_MCP_PUBLIC_URL")]
    pub mcp_public_url: Option<String>,

    /// AI agents (tokens) connected at once; they also count toward --max-sessions.
    #[arg(long, env = "DALKER_MCP_MAX_SESSIONS", default_value_t = 1)]
    pub mcp_max_sessions: usize,
}

fn parse_cookie_secure(s: &str) -> Result<dalker_web::CookieSecure, String> {
    dalker_web::CookieSecure::parse(s).ok_or_else(|| format!("`{s}`: expected auto, on or off"))
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Print an argon2 hash for DALKER_PASSWORD_HASH. Reads stdin if no argument.
    HashPassword { password: Option<String> },
    /// Store a new login password (as an argon2 hash) in the config file.
    /// The password is read from stdin, or from the argument if given.
    SetPassword {
        password: Option<String>,
        /// Config file to update (default: the system-wide location).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
    /// Open the host's settings window: web access, password, MCP server and its tokens.
    /// Asks for administrator rights (the config file is the administrators').
    Settings {
        /// Config file to update (default: the system-wide location).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Set when relaunched with administrator rights (avoids asking twice).
        #[arg(long, hide = true)]
        elevated: bool,
        /// Serve the settings page and print its address instead of opening the window
        /// (development and testing).
        #[arg(long, hide = true)]
        no_window: bool,
    },
    /// Reads and writes the config file for the settings window, which runs as the desktop
    /// user (Linux: started through pkexec). Speaks JSON lines on stdin/stdout.
    #[command(hide = true)]
    SettingsHelper {
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Linux launcher: serve the agent of a GNOME-on-Wayland session on this Unix socket
        /// (MCP tokens only) instead of stdin/stdout.
        #[arg(long, requires = "owner")]
        listen: Option<std::path::PathBuf>,
        /// The user the socket belongs to.
        #[arg(long)]
        owner: Option<String>,
    },
    /// Run as the `Dalker` Windows service (started by the service control manager).
    /// Keeps the agent running as SYSTEM in the console session, so the sign-in screen,
    /// lock screen and UAC prompts can be seen and controlled too. Options given before
    /// `service` are passed on to that agent.
    Service,
    /// Manage the bearer tokens AI agents use for the MCP server (DALKER_MCP=on).
    McpToken {
        #[command(subcommand)]
        action: McpTokenCommand,
    },
    /// Set one `DALKER_*` key in the config file (used by the installers).
    SetConfig {
        key: String,
        value: String,
        /// Config file to update (default: the system-wide location).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
        /// Leave the file alone if it already has the key (installer upgrades).
        #[arg(long)]
        if_missing: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum McpTokenCommand {
    /// Create a token; it is printed once, only its hash is stored in the config file.
    Create {
        /// Name shown in the log and `list`, e.g. claude-laptop.
        #[arg(long)]
        name: String,
        /// Comma-separated: view (screenshots), input (mouse, keyboard), clipboard.
        #[arg(long, default_value = "view")]
        scopes: String,
        /// Lifetime such as 30d, 12h or 45m (default: never expires).
        #[arg(long)]
        expires: Option<String>,
        /// Config file to update (default: the system-wide location).
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
    /// List the tokens in the config file.
    List {
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
    /// Revoke a token (by name or id); a running agent ends its sessions within seconds.
    Revoke {
        name_or_id: String,
        #[arg(long)]
        config: Option<std::path::PathBuf>,
    },
}

/// Reads a password for `command` (`set-password`, `hash-password`) from standard input.
pub fn read_password_from_stdin(command: &str) -> anyhow::Result<String> {
    // A release dalkerd.exe is a GUI program, so cmd and PowerShell don't wait for it: they
    // would read the same console, take the typed password as a command and leave a later
    // line for us. Only read the console when the console launcher (dalkerd.com) started us.
    if cfg!(all(windows, not(debug_assertions)))
        && std::io::stdin().is_terminal()
        && std::env::var_os(dalker_platform::CONSOLE_LAUNCHER_ENV).is_none()
    {
        anyhow::bail!(
            "dalkerd.exe can't prompt for the password: the terminal doesn't wait for it. \
             Run `dalkerd {command}` (without .exe) from the Dalker folder, \
             or pass the password: `dalkerd.exe {command} <password>`"
        );
    }
    if std::io::stdin().is_terminal() {
        eprintln!("Password (input is not hidden; pipe it in to avoid echo):");
    }
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    let pw = line.trim_end_matches(['\r', '\n']).to_owned();
    anyhow::ensure!(
        !pw.is_empty(),
        "no password given: type it, pipe it in, or pass it as an argument (`dalkerd {command} <password>`)"
    );
    Ok(pw)
}

/// System-wide config file used by the installed service.
pub fn default_config_path() -> std::path::PathBuf {
    if cfg!(windows) {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        std::path::PathBuf::from(base).join("Dalker").join("dalker.conf")
    } else {
        std::path::PathBuf::from("/etc/dalker/dalker.conf")
    }
}

/// The config file in use: `$DALKER_CONFIG` or the system-wide default.
pub fn config_path() -> std::path::PathBuf {
    std::env::var_os("DALKER_CONFIG").map(std::path::PathBuf::from).unwrap_or_else(default_config_path)
}

/// Loads `KEY=VALUE` lines (`#` comments allowed) into the environment so clap's
/// `env = "DALKER_*"` options pick them up. Real environment variables win.
/// Must run before any threads are started. Returns the keys it set (`None`: no file).
pub fn load_config_into_env(path: &std::path::Path) -> anyhow::Result<Option<Vec<String>>> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => anyhow::bail!("read {}: {e}", path.display()),
    };
    let mut set = Vec::new();
    for (k, v) in parse_config(&text) {
        if std::env::var_os(&k).is_none() {
            std::env::set_var(&k, v);
            set.push(k);
        }
    }
    Ok(Some(set))
}

/// The `DALKER_*` `KEY=VALUE` pairs of a config file, in order.
pub fn parse_config(text: &str) -> Vec<(String, String)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches(['"', '\'']).to_owned()))
        .filter(|(k, _)| k.starts_with("DALKER_"))
        .collect()
}

/// Current value of `key` in the config file (`None` if the file or the key is missing or
/// can't be read).
pub fn read_config_value(path: &std::path::Path, key: &str) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    parse_config(&text).into_iter().rev().find(|(k, _)| k == key).map(|(_, v)| v)
}

/// Whether `path` can hold settings: a regular file (or a new one), not e.g. `/dev/null`,
/// which accepts writes and forgets them.
pub fn is_writable_config(path: &std::path::Path) -> bool {
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && !m.permissions().readonly(),
        Err(e) => e.kind() == std::io::ErrorKind::NotFound,
    }
}

/// Removes `key` from the config file, keeping every other line. Returns whether it was there.
#[cfg_attr(not(test), allow(dead_code))]
pub fn remove_config_value(path: &std::path::Path, key: &str) -> anyhow::Result<bool> {
    let existing = std::fs::read_to_string(path)?;
    let had = parse_config(&existing).iter().any(|(k, _)| k == key);
    if had {
        edit_config(path, &[], &[key.to_owned()])?;
    }
    Ok(had)
}

/// Sets `key` to `value` in the config file, keeping every other line.
pub fn write_config_value(path: &std::path::Path, key: &str, value: &str) -> anyhow::Result<()> {
    edit_config(path, &[(key.to_owned(), value.to_owned())], &[])
}

/// Sets `key` to `value` unless the config file already has it. The check and the write happen
/// under one lock, so a concurrent writer can't slip in between. Returns whether it was set.
pub fn write_config_value_if_missing(path: &std::path::Path, key: &str, value: &str) -> anyhow::Result<bool> {
    let _lock = lock_config(path)?;
    if read_config_value(path, key).is_some() {
        return Ok(false);
    }
    edit_config_unlocked(path, &[(key.to_owned(), value.to_owned())], &[])?;
    Ok(true)
}

/// Sets and removes several keys in one go, keeping comments and every other line. The file is
/// replaced at once (written next to it, then renamed), so the running agent, which follows
/// the file, never reads half of it. The new file keeps the old one's permissions.
///
/// The read-modify-write runs under an exclusive lock on a file next to the config (see
/// [`lock_config`]), so writers in other processes (e.g. several `dalkerd set-config` that a
/// shell started at once) wait for each other instead of dropping each other's changes.
pub fn edit_config(path: &std::path::Path, set: &[(String, String)], remove: &[String]) -> anyhow::Result<()> {
    let _lock = lock_config(path)?;
    edit_config_unlocked(path, set, remove)
}

/// Where the config file's lock lives: `.<name>.lock` in the same folder. It is a file of its
/// own because the config file itself is replaced on every write, and a lock on the replaced
/// file would not stop the next writer. It is left in place: removing it would let two
/// writers lock two different files.
fn config_lock_path(path: &std::path::Path) -> std::path::PathBuf {
    let (dir, name) = config_dir_and_name(path);
    dir.join(format!(".{name}.lock"))
}

fn config_dir_and_name(path: &std::path::Path) -> (std::path::PathBuf, String) {
    let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(std::path::Path::new("."));
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "dalker.conf".into());
    (dir.to_owned(), name)
}

/// Takes the exclusive lock that every writer of the config file holds while it reads, edits
/// and replaces it; blocks until it is free. Released when the returned file is dropped (and
/// by the OS if the process dies).
fn lock_config(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    let lock_path = config_lock_path(path);
    if let Some(dir) = lock_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&lock_path)
        // A lock file made by another user may only be readable to us; a read handle can
        // still be locked.
        .or_else(|_| std::fs::File::open(&lock_path))
        .map_err(|e| anyhow::anyhow!("open {}: {e}", lock_path.display()))?;
    file.lock().map_err(|e| anyhow::anyhow!("lock {}: {e}", lock_path.display()))?;
    Ok(file)
}

/// [`edit_config`] for a caller that already holds the lock.
fn edit_config_unlocked(path: &std::path::Path, set: &[(String, String)], remove: &[String]) -> anyhow::Result<()> {
    for (k, v) in set {
        anyhow::ensure!(k.starts_with("DALKER_") && !k.contains(['=', '\n', '\r', ' ']), "bad config key `{k}`");
        anyhow::ensure!(!v.contains(['\n', '\r']), "the value of {k} must be one line");
    }
    let existing = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => anyhow::bail!("read {}: {e}", path.display()),
    };
    let key_of = |line: &str| {
        let line = line.trim_start();
        (!line.starts_with('#')).then(|| line.split_once('=').map(|(k, _)| k.trim().to_owned())).flatten()
    };
    let mut done = vec![false; set.len()];
    let mut out = Vec::new();
    for line in existing.lines() {
        match key_of(line) {
            Some(k) if remove.contains(&k) => {}
            Some(k) => match set.iter().position(|(sk, _)| *sk == k) {
                // A key that appears twice keeps only its first line.
                Some(i) if done[i] => {}
                Some(i) => {
                    out.push(format!("{k}={}", set[i].1));
                    done[i] = true;
                }
                None => out.push(line.to_owned()),
            },
            None => out.push(line.to_owned()),
        }
    }
    for (i, (k, v)) in set.iter().enumerate() {
        if !done[i] {
            out.push(format!("{k}={v}"));
        }
    }
    let (dir, name) = config_dir_and_name(path);
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let write = || -> std::io::Result<()> {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp)?;
        // Restricted before anything is in it: the file holds password and token hashes.
        if let Ok(m) = std::fs::metadata(path) {
            f.set_permissions(m.permissions())?;
        } else {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            }
        }
        f.write_all((out.join("\n") + "\n").as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        anyhow::anyhow!("write {}: {e}", path.display())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writable_config_is_a_regular_file() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        assert!(is_writable_config(&file), "a file that doesn't exist yet can be created");
        write_config_value(&file, "DALKER_PASSWORD_HASH", "x").unwrap();
        assert!(is_writable_config(&file));
        assert!(!is_writable_config(&dir), "a directory is not a config file");
        #[cfg(unix)]
        assert!(!is_writable_config(std::path::Path::new("/dev/null")), "writes to /dev/null are lost");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn reads_back_written_values() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-read-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        assert_eq!(read_config_value(&file, "DALKER_PASSWORD_HASH"), None);
        std::fs::write(&file, "# comment\nDALKER_BIND=0.0.0.0:8080\n").unwrap();
        assert_eq!(read_config_value(&file, "DALKER_PASSWORD_HASH"), None);
        write_config_value(&file, "DALKER_PASSWORD_HASH", "$argon2id$v=19$abc").unwrap();
        assert_eq!(read_config_value(&file, "DALKER_PASSWORD_HASH").as_deref(), Some("$argon2id$v=19$abc"));
        assert_eq!(read_config_value(&file, "DALKER_BIND").as_deref(), Some("0.0.0.0:8080"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn removes_values() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-remove-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        std::fs::write(&file, "# keep me\nDALKER_A=1\nDALKER_AB=2\nDALKER_B=3\n").unwrap();
        assert!(remove_config_value(&file, "DALKER_A").unwrap());
        assert!(!remove_config_value(&file, "DALKER_A").unwrap());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# keep me\nDALKER_AB=2\nDALKER_B=3\n");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn concurrent_writers_keep_every_key() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-concurrent-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        std::fs::write(&file, "# keep me\nDALKER_UDP_PORT=9000\n").unwrap();
        let keys: Vec<String> = (0..16).map(|i| format!("DALKER_TEST_{i}")).collect();
        std::thread::scope(|s| {
            for (i, key) in keys.iter().enumerate() {
                let file = &file;
                s.spawn(move || {
                    for round in 0..5 {
                        write_config_value(file, key, &format!("{i}-{round}")).unwrap();
                    }
                });
            }
        });
        let set = parse_config(&std::fs::read_to_string(&file).unwrap());
        assert_eq!(read_config_value(&file, "DALKER_UDP_PORT").as_deref(), Some("9000"));
        for (i, key) in keys.iter().enumerate() {
            assert_eq!(read_config_value(&file, key), Some(format!("{i}-4")), "{key} lost: {set:?}");
        }
        assert_eq!(set.len(), keys.len() + 1);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn writes_if_missing_only_once() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-if-missing-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        assert!(write_config_value_if_missing(&file, "DALKER_A", "1").unwrap());
        assert!(!write_config_value_if_missing(&file, "DALKER_A", "2").unwrap());
        assert_eq!(read_config_value(&file, "DALKER_A").as_deref(), Some("1"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn edits_several_keys_at_once() {
        let dir = std::env::temp_dir().join(format!("dalker-cli-edit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("dalker.conf");
        std::fs::write(&file, "# DALKER_A=comment\nDALKER_A=1\nDALKER_B=2\nDALKER_A=old\n").unwrap();
        let set = [("DALKER_A".to_owned(), "x".to_owned()), ("DALKER_C".to_owned(), "3".to_owned())];
        edit_config(&file, &set, &["DALKER_B".to_owned()]).unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "# DALKER_A=comment\nDALKER_A=x\nDALKER_C=3\n");
        assert!(edit_config(&file, &[("DALKER_X".into(), "a\nDALKER_Y=b".into())], &[]).is_err());
        assert!(edit_config(&file, &[("PATH".into(), "x".into())], &[]).is_err());
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != ".dalker.conf.lock")
            .collect();
        assert_eq!(left, ["dalker.conf"], "no temporary file left behind");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();
            write_config_value(&file, "DALKER_D", "4").unwrap();
            assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600);
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mcp_options() {
        let cli = Cli::try_parse_from(["dalkerd"]).unwrap();
        assert_eq!(cli.mcp, OnOff::Off, "off by default");
        assert_eq!(cli.mcp_bind, "127.0.0.1:8743".parse::<SocketAddr>().unwrap());
        assert_eq!(cli.bind, "0.0.0.0:8742".parse::<SocketAddr>().unwrap());
        assert_eq!(cli.web, OnOff::On);
        assert!(cli.mcp_allow_plaintext);
        assert_eq!(cli.mcp_max_sessions, 1);
        assert_eq!(cli.cookie_secure, dalker_web::CookieSecure::Auto);
        assert_eq!(cli.mcp_public_url, None);
        let cli = Cli::try_parse_from(["dalkerd", "--cookie-secure", "on"]).unwrap();
        assert_eq!(cli.cookie_secure, dalker_web::CookieSecure::Always);
        assert!(Cli::try_parse_from(["dalkerd", "--cookie-secure", "maybe"]).is_err());
        let cli = Cli::try_parse_from(["dalkerd", "--mcp", "on", "--mcp-allow-plaintext", "false"]).unwrap();
        assert_eq!(cli.mcp, OnOff::On);
        assert!(!cli.mcp_allow_plaintext);
        let cli = Cli::try_parse_from(["dalkerd", "mcp-token", "create", "--name", "x", "--scopes", "view,input"]).unwrap();
        assert!(matches!(cli.command, Some(Command::McpToken { action: McpTokenCommand::Create { .. } })));
    }
}
