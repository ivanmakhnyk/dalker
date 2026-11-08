//! `dalkerd settings`: the host's settings window. Web access (on/off, port, password) and
//! the MCP server (on/off, port, tokens and what each may do), all stored in the config file.
//!
//! The config file belongs to the administrators (it holds the password and token hashes):
//! * Windows: the window runs elevated (it relaunches itself through UAC) and edits the file.
//! * Linux: the window runs as the desktop user and edits the file through
//!   `dalkerd settings-helper`, started once through pkexec ([`store`]).
//!
//! The running agent follows the file, so changes apply within seconds without a restart
//! (see `listeners.rs`).

pub mod api;
pub mod store;
mod window;

use std::collections::BTreeSet;
use std::net::SocketAddr;

use anyhow::Result;
use dalker_mcp::tokens::{self, Scope, TokenRecord};

use crate::cli::{self, OnOff};
use crate::listeners::{KEY_BIND, KEY_MCP, KEY_MCP_BIND, KEY_WEB};

/// Shortest password the settings window accepts.
pub const MIN_PASSWORD_LEN: usize = 8;
/// Lowest port the settings window accepts (below needs root on Linux and clashes easily).
pub const MIN_PORT: u16 = 1024;

const KEY_PASSWORD_HASH: &str = "DALKER_PASSWORD_HASH";
const KEY_PASSWORD: &str = "DALKER_PASSWORD";

/// Opens the settings window for the config file at `path`.
pub fn run(path: &std::path::Path, elevated: bool, no_window: bool) -> Result<()> {
    // One window at a time: a second launch (Start menu, tray, app grid) brings the open one to
    // the front instead, before asking for administrator rights again.
    if !no_window && window::focus_open_window() {
        return Ok(());
    }
    let writable = cli::is_writable_config(path)
        && (std::fs::OpenOptions::new().append(true).open(path).is_ok() || !path.exists());
    let store: Result<Box<dyn store::Store>, String> = if writable {
        Ok(Box::new(store::FileStore::new(path)))
    } else {
        #[cfg(windows)]
        {
            elevate(path, elevated);
            return Ok(());
        }
        #[cfg(not(windows))]
        {
            let _ = elevated;
            // Asks for an administrator password once, for as long as the window is open.
            store::HelperStore::start(path).map(|s| Box::new(s) as Box<dyn store::Store>)
        }
    };
    window::show(store, no_window);
    Ok(())
}

/// Windows: starts the window again with administrator rights (UAC prompt).
#[cfg(windows)]
fn elevate(path: &std::path::Path, elevated: bool) {
    use dalker_platform::windows::settings;
    if elevated {
        settings::error_box(&format!("Dalker can't write its settings file:\n{}\n\nIs Dalker installed?", path.display()));
        return;
    }
    let args = format!(
        "settings --elevated --config {}",
        dalker_platform::windows::service::quote_arg(&path.display().to_string())
    );
    if let Err(e) = settings::relaunch_elevated(&args) {
        // Includes the user saying no to the UAC prompt.
        tracing::info!("settings not opened: {e}");
    }
}

/// What the config file says (the parts the window shows).
#[derive(Debug, Clone, PartialEq)]
pub struct Saved {
    pub web: bool,
    pub bind: SocketAddr,
    pub mcp: bool,
    pub mcp_bind: SocketAddr,
    /// `DALKER_MCP_PUBLIC_URL`: where agents reach the MCP server through a reverse proxy.
    pub mcp_public_url: Option<String>,
    pub password_set: bool,
    pub tokens: Vec<TokenRecord>,
    /// The tokens themselves, by id, for those made since they are kept (copy again later).
    pub secrets: std::collections::BTreeMap<String, String>,
}

impl Saved {
    pub fn parse(text: &str) -> Self {
        let pairs = cli::parse_config(text);
        let get = |key: &str| pairs.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
        let addr = |key: &str, default: &str| {
            get(key).and_then(|v| v.parse().ok()).unwrap_or_else(|| default.parse().expect("default address"))
        };
        let mut tokens: Vec<TokenRecord> = pairs
            .iter()
            .filter(|(k, _)| k.starts_with(tokens::CONFIG_KEY_PREFIX))
            .filter_map(|(k, v)| TokenRecord::from_config(k, v))
            .collect();
        tokens.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        Self {
            web: get(KEY_WEB).and_then(OnOff::parse).unwrap_or(true),
            bind: addr(KEY_BIND, cli::DEFAULT_BIND),
            mcp: get(KEY_MCP).and_then(OnOff::parse).unwrap_or(false),
            mcp_bind: addr(KEY_MCP_BIND, cli::DEFAULT_MCP_BIND),
            mcp_public_url: get("DALKER_MCP_PUBLIC_URL").map(str::trim).filter(|v| !v.is_empty()).map(str::to_owned),
            password_set: get(KEY_PASSWORD_HASH).is_some_and(|v| !v.is_empty())
                || get(KEY_PASSWORD).is_some_and(|v| !v.is_empty()),
            secrets: pairs
                .iter()
                .filter_map(|(k, v)| {
                    let id = k.strip_prefix(tokens::SECRET_KEY_PREFIX)?;
                    // Only a token that matches its record's id counts.
                    (tokens::token_id(v) == Some(id)).then(|| (id.to_owned(), v.clone()))
                })
                .collect(),
            tokens,
        }
    }
}

pub fn parse_port(s: &str) -> Result<u16, String> {
    let port: u32 = s.trim().parse().map_err(|_| "enter a number from 1024 to 65535.".to_owned())?;
    if !(MIN_PORT as u32..=65535).contains(&port) {
        return Err("choose a port from 1024 to 65535.".into());
    }
    Ok(port as u16)
}

/// Checks a new password the way the settings window does.
pub fn check_new_password(new: &str, repeat: &str, min_len: usize) -> Result<(), String> {
    if new.is_empty() {
        return Err("Enter the new password.".into());
    }
    if new.chars().count() < min_len {
        return Err(format!("The password must have at least {min_len} characters."));
    }
    if new != repeat {
        return Err("The two passwords do not match.".into());
    }
    Ok(())
}

/// The two permissions the window offers, as token scopes: "screen capture and remote control"
/// is `view` + `input`, "clipboard" is `clipboard`.
pub fn scopes_for(remote_control: bool, clipboard: bool) -> Result<BTreeSet<Scope>, String> {
    let mut scopes = BTreeSet::new();
    if remote_control {
        scopes.extend([Scope::View, Scope::Input]);
    }
    if clipboard {
        scopes.insert(Scope::Clipboard);
    }
    if scopes.is_empty() {
        return Err("A token needs at least one permission. Revoke it instead if it should not be used.".into());
    }
    Ok(scopes)
}

/// How much remote control a token has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    Full,
    /// Screenshots only (made with `mcp-token create --scopes view`).
    ViewOnly,
    None,
}

pub fn remote_of(record: &TokenRecord) -> Remote {
    match (record.scopes.contains(&Scope::View), record.scopes.contains(&Scope::Input)) {
        (true, true) => Remote::Full,
        (true, false) => Remote::ViewOnly,
        // Input without view is not something the window makes; count it as on.
        (false, true) => Remote::Full,
        (false, false) => Remote::None,
    }
}

/// The token with its remote control and clipboard permissions set as given (same secret).
pub fn with_permissions(record: &TokenRecord, remote_control: bool, clipboard: bool) -> Result<TokenRecord, String> {
    // A view-only token stays view-only while only its clipboard permission changes.
    let scopes = if !remote_control && remote_of(record) == Remote::ViewOnly {
        let mut s = BTreeSet::from([Scope::View]);
        if clipboard {
            s.insert(Scope::Clipboard);
        }
        s
    } else {
        scopes_for(remote_control, clipboard)?
    };
    Ok(TokenRecord { scopes, ..record.clone() })
}

/// How a token is shown: its id and a mask in place of the secret.
pub fn masked(record: &TokenRecord) -> String {
    format!("dkmcp_{}_••••••••", record.id)
}

/// `YYYY-MM-DD` (UTC) of a unix time.
pub fn date(unix: u64) -> String {
    crate::date(unix)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "$argon2id$v=19$m=19456,t=2,p=1$c2FsdHNhbHRzYWx0$0123456789abcdef0123456789abcdef0123456789a";

    #[test]
    fn reads_the_config_file() {
        let s = Saved::parse("");
        assert!(s.web && !s.mcp && !s.password_set);
        assert_eq!(s.bind.to_string(), "0.0.0.0:8742");
        assert_eq!(s.mcp_bind.to_string(), "127.0.0.1:8743");
        assert_eq!(s.mcp_public_url, None);
        let s = Saved::parse("DALKER_MCP_PUBLIC_URL=https://desk.example.com/mcp
");
        assert_eq!(s.mcp_public_url.as_deref(), Some("https://desk.example.com/mcp"));
        let s = Saved::parse(&format!(
            "DALKER_BIND=0.0.0.0:8080\nDALKER_WEB=off\nDALKER_MCP=on\nDALKER_PASSWORD_HASH={HASH}\n"
        ));
        assert!(!s.web && s.mcp && s.password_set);
        assert_eq!(s.bind.port(), 8080);
    }

    #[test]
    fn permissions_map_to_scopes() {
        assert_eq!(tokens::format_scopes(&scopes_for(true, false).unwrap()), "view,input");
        assert_eq!(tokens::format_scopes(&scopes_for(true, true).unwrap()), "view,input,clipboard");
        assert_eq!(tokens::format_scopes(&scopes_for(false, true).unwrap()), "clipboard");
        assert!(scopes_for(false, false).is_err());

        let (_, rec) = tokens::generate("old", tokens::parse_scopes("view").unwrap(), None).unwrap();
        assert_eq!(remote_of(&rec), Remote::ViewOnly);
        let with_clip = with_permissions(&rec, false, true).unwrap();
        assert_eq!(tokens::format_scopes(&with_clip.scopes), "view,clipboard", "stays view-only");
        assert_eq!(with_clip.hash, rec.hash, "same token, other permissions");
        let back = with_permissions(&with_clip, false, false).unwrap();
        assert_eq!(tokens::format_scopes(&back.scopes), "view", "clipboard off again: still view-only");
        let full = with_permissions(&rec, true, false).unwrap();
        assert_eq!(remote_of(&full), Remote::Full);
        assert!(with_permissions(&full, false, false).is_err(), "no permission left");
        assert!(masked(&rec).starts_with(&format!("dkmcp_{}_", rec.id)));
    }

    #[test]
    fn keeps_tokens() {
        let (token, rec) = tokens::generate("lm", tokens::parse_scopes("view").unwrap(), None).unwrap();
        let text = format!(
            "{}={}\n{}={token}\nDALKER_MCP_SECRET_00000000={token}\n",
            rec.config_key(),
            rec.config_value(),
            rec.secret_key()
        );
        let saved = Saved::parse(&text);
        assert_eq!(saved.secrets.get(&rec.id), Some(&token));
        assert_eq!(saved.secrets.len(), 1, "a secret under another id is ignored");
    }

    #[test]
    fn passwords() {
        assert!(check_new_password("", "", 8).is_err());
        assert!(check_new_password("short", "short", 8).unwrap_err().contains("at least 8"));
        assert!(check_new_password("long-enough", "long-enougH", 8).unwrap_err().contains("match"));
        assert!(check_new_password("long-enough", "long-enough", 8).is_ok());
    }
}
