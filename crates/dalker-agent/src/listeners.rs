//! The web and MCP listeners. They follow the config file while the agent runs: the host's
//! settings window turns them on and off and moves them to other ports by editing the file,
//! and the agent starts, stops or moves the listener within seconds, without a restart.
//!
//! Moving the web server keeps the viewers' streams (they don't use the HTTP connection);
//! turning it off ends them. Turning the MCP server off ends the AI agents' sessions.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use dalker_mcp::McpServer;
use dalker_web::{Authenticator, SignalingHandler};
use tokio::sync::{oneshot, watch};
use tokio::task::JoinHandle;

use crate::cli::{self, OnOff};

/// The listeners the agent should have; `None` = off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Wanted {
    pub web: Option<SocketAddr>,
    pub mcp: Option<SocketAddr>,
}

/// The listener settings the agent started with, and which of them the config file can't
/// change: given on the command line, or as an environment variable that didn't come from
/// the config file.
#[derive(Debug, Clone)]
pub struct Startup {
    pub web: bool,
    pub bind: SocketAddr,
    pub mcp: bool,
    pub mcp_bind: SocketAddr,
    pub pinned: Vec<&'static str>,
}

/// Config keys of the listener settings.
pub const KEY_WEB: &str = "DALKER_WEB";
pub const KEY_BIND: &str = "DALKER_BIND";
pub const KEY_MCP: &str = "DALKER_MCP";
pub const KEY_MCP_BIND: &str = "DALKER_MCP_BIND";

impl Startup {
    pub fn wanted(&self) -> Wanted {
        Wanted { web: self.web.then_some(self.bind), mcp: self.mcp.then_some(self.mcp_bind) }
    }

    /// What the config file asks for. Keys it doesn't have take their defaults; pinned keys and
    /// values that don't parse keep what the agent started with.
    pub fn wanted_from(&self, file: &[(String, String)]) -> Wanted {
        let get = |key: &str| {
            if self.pinned.contains(&key) {
                return Err(());
            }
            Ok(file.iter().rev().find(|(k, _)| k == key).map(|(_, v)| v.as_str()))
        };
        let switch = |key: &str, started: bool| match get(key) {
            Err(()) => started,
            Ok(None) => key == KEY_WEB,
            Ok(Some(v)) => OnOff::parse(v).unwrap_or(started),
        };
        let addr = |key: &str, started: SocketAddr, default: &str| match get(key) {
            Err(()) => started,
            Ok(None) => default.parse().expect("default address"),
            Ok(Some(v)) => v.parse().unwrap_or(started),
        };
        let web = switch(KEY_WEB, self.web);
        let mcp = switch(KEY_MCP, self.mcp);
        Wanted {
            web: web.then(|| addr(KEY_BIND, self.bind, cli::DEFAULT_BIND)),
            mcp: mcp.then(|| addr(KEY_MCP_BIND, self.mcp_bind, cli::DEFAULT_MCP_BIND)),
        }
    }
}

/// Where the listeners actually are right now (the tray uses it).
#[derive(Debug, Default)]
pub struct Status {
    pub web: Mutex<Option<SocketAddr>>,
    pub mcp: Mutex<Option<SocketAddr>>,
}

/// What the web listener serves.
pub struct WebParts {
    /// Routes served beside the web client (its settings API).
    pub extra: axum::Router,
    pub auth: Arc<Authenticator>,
    pub signaling: Arc<dyn SignalingHandler>,
    /// Called when the web server is turned off: ends the viewers' streams.
    pub sign_out: Box<dyn Fn(dalker_webrtc::EndReason) + Send + Sync>,
}

/// What the MCP listener serves.
pub struct McpParts {
    pub server: McpServer,
    pub allow_plaintext: bool,
}

struct Running {
    /// The address asked for (may have port 0).
    wanted: SocketAddr,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        let mut task = self.task;
        // Graceful first; long-lived requests (MCP event streams) must not keep the port.
        if tokio::time::timeout(Duration::from_secs(5), &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

/// One listener: what runs, and the last error (logged once).
#[derive(Default)]
struct Slot {
    running: Option<Running>,
    last_error: Option<String>,
}

impl Slot {
    fn fail(&mut self, what: &str, e: String) {
        if self.last_error.as_deref() != Some(e.as_str()) {
            tracing::error!("{what} not started: {e} (retrying)");
            self.last_error = Some(e);
        }
    }
}

/// Keeps the listeners as `wanted` says until `stop` turns true, then stops them.
pub async fn supervise(
    mut wanted: watch::Receiver<Wanted>,
    web: WebParts,
    mcp: McpParts,
    mut stop: watch::Receiver<bool>,
    status: Arc<Status>,
) {
    let mut web_slot = Slot::default();
    let mut mcp_slot = Slot::default();
    loop {
        let want = *wanted.borrow_and_update();
        reconcile_web(&mut web_slot, want.web, &web, &status).await;
        reconcile_mcp(&mut mcp_slot, want.mcp, &mcp, &status).await;
        tokio::select! {
            _ = wanted.changed() => {}
            _ = stop.wait_for(|s| *s) => break,
            // Retry what failed (port in use...) and notice a listener that died.
            _ = tokio::time::sleep(Duration::from_secs(5)) => {}
        }
    }
    if let Some(r) = web_slot.running.take() {
        r.stop().await;
    }
    if let Some(r) = mcp_slot.running.take() {
        r.stop().await;
    }
}

async fn reconcile_web(slot: &mut Slot, want: Option<SocketAddr>, parts: &WebParts, status: &Status) {
    if slot.running.as_ref().is_some_and(|r| r.task.is_finished()) {
        slot.running = None;
        *status.web.lock().unwrap() = None;
    }
    if slot.running.as_ref().map(|r| r.wanted) == want {
        return;
    }
    let was_running = slot.running.is_some();
    if let Some(r) = slot.running.take() {
        r.stop().await;
        *status.web.lock().unwrap() = None;
    }
    let Some(addr) = want else {
        if was_running {
            tracing::info!("web server turned off on the host; viewers disconnected");
            (parts.sign_out)(dalker_webrtc::EndReason::WebOff);
        }
        slot.last_error = None;
        return;
    };
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return slot.fail("web server", format!("can't listen on {addr}: {e}")),
    };
    let local = listener.local_addr().unwrap_or(addr);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let (auth, signaling, extra) = (Arc::clone(&parts.auth), Arc::clone(&parts.signaling), parts.extra.clone());
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop_rx.await;
        };
        if let Err(e) = dalker_web::serve_listener_with(listener, auth, signaling, extra, shutdown).await {
            tracing::error!("web server failed: {e:#}");
        }
    });
    slot.running = Some(Running { wanted: addr, stop: stop_tx, task });
    slot.last_error = None;
    *status.web.lock().unwrap() = Some(local);
}

async fn reconcile_mcp(slot: &mut Slot, want: Option<SocketAddr>, parts: &McpParts, status: &Status) {
    if slot.running.as_ref().is_some_and(|r| r.task.is_finished()) {
        slot.running = None;
        *status.mcp.lock().unwrap() = None;
    }
    if slot.running.as_ref().map(|r| r.wanted) == want {
        return;
    }
    let was_running = slot.running.is_some();
    if let Some(r) = slot.running.take() {
        // Its graceful shutdown ends the AI agents' sessions.
        r.stop().await;
        *status.mcp.lock().unwrap() = None;
    }
    let Some(addr) = want else {
        if was_running {
            tracing::info!("MCP server turned off on the host");
        }
        slot.last_error = None;
        return;
    };
    let loopback = addr.ip().is_loopback();
    if !loopback && !parts.allow_plaintext {
        return slot.fail(
            "MCP server",
            format!(
                "{addr} is not a loopback address and DALKER_MCP_ALLOW_PLAINTEXT is off (bearer tokens would \
                 cross the network as plain HTTP). Set DALKER_MCP_BIND to 127.0.0.1 and use an SSH tunnel, VPN \
                 or TLS reverse proxy, or allow plain HTTP on a network you trust"
            ),
        );
    }
    if !loopback {
        tracing::info!("MCP server reachable from the network on {addr} (plain HTTP)");
    }
    if parts.server.tokens().is_empty() {
        tracing::warn!("no MCP tokens yet: create one in the settings window or with `dalkerd mcp-token create`");
    }
    let listener = match tokio::net::TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => return slot.fail("MCP server", format!("can't listen on {addr}: {e}")),
    };
    let local = listener.local_addr().unwrap_or(addr);
    let (stop_tx, stop_rx) = oneshot::channel::<()>();
    let server = parts.server.clone();
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop_rx.await;
        };
        if let Err(e) = server.serve(listener, shutdown).await {
            tracing::error!("MCP server failed: {e:#}");
        }
    });
    slot.running = Some(Running { wanted: addr, stop: stop_tx, task });
    slot.last_error = None;
    *status.mcp.lock().unwrap() = Some(local);
}

/// Follows the listener settings in the config file and publishes them on `wanted`.
pub async fn follow_config(path: std::path::PathBuf, startup: Startup, wanted: watch::Sender<Wanted>) {
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        // A file that can't be read right now changes nothing.
        let Ok(text) = std::fs::read_to_string(&path) else { continue };
        let now = startup.wanted_from(&cli::parse_config(&text));
        wanted.send_if_modified(|w| {
            if *w == now {
                return false;
            }
            tracing::info!(web = ?now.web, mcp = ?now.mcp, "listener settings changed on the host");
            *w = now;
            true
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn startup(pinned: Vec<&'static str>) -> Startup {
        Startup {
            web: true,
            bind: "0.0.0.0:9000".parse().unwrap(),
            mcp: false,
            mcp_bind: "127.0.0.1:9001".parse().unwrap(),
            pinned,
        }
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn file_settings_and_defaults() {
        let s = startup(vec![]);
        // An empty file means the defaults: web on its default port, MCP off.
        assert_eq!(s.wanted_from(&[]), Wanted { web: Some("0.0.0.0:8742".parse().unwrap()), mcp: None });
        let w = s.wanted_from(&kv(&[
            ("DALKER_WEB", "off"),
            ("DALKER_MCP", "on"),
            ("DALKER_MCP_BIND", "127.0.0.1:7000"),
        ]));
        assert_eq!(w, Wanted { web: None, mcp: Some("127.0.0.1:7000".parse().unwrap()) });
        // Values that don't parse keep what the agent started with; the last line wins.
        let w = s.wanted_from(&kv(&[("DALKER_BIND", "0.0.0.0:1"), ("DALKER_BIND", "nonsense"), ("DALKER_MCP", "maybe")]));
        assert_eq!(w, Wanted { web: Some("0.0.0.0:9000".parse().unwrap()), mcp: None });
    }

    #[test]
    fn pinned_settings_ignore_the_file() {
        let s = startup(vec![KEY_BIND, KEY_MCP]);
        let w = s.wanted_from(&kv(&[("DALKER_BIND", "0.0.0.0:1234"), ("DALKER_MCP", "on"), ("DALKER_WEB", "on")]));
        assert_eq!(w, Wanted { web: Some("0.0.0.0:9000".parse().unwrap()), mcp: None });
    }
}
