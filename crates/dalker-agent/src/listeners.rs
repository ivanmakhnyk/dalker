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
    /// HTTPS on both ports (`DALKER_TLS`).
    pub tls: bool,
    /// Plain HTTP from other computers is sent to HTTPS (`DALKER_TLS_REDIRECT`).
    pub tls_redirect: bool,
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
    pub tls: bool,
    pub tls_redirect: bool,
    /// Config keys the file can't change (the stream limits' too, see `stream.rs`).
    pub pinned: Vec<&'static str>,
}

/// Config keys of the listener settings.
pub const KEY_WEB: &str = "DALKER_WEB";
pub const KEY_BIND: &str = "DALKER_BIND";
pub const KEY_MCP: &str = "DALKER_MCP";
pub const KEY_MCP_BIND: &str = "DALKER_MCP_BIND";
pub const KEY_TLS: &str = "DALKER_TLS";
pub const KEY_TLS_REDIRECT: &str = "DALKER_TLS_REDIRECT";

impl Startup {
    pub fn wanted(&self) -> Wanted {
        Wanted {
            web: self.web.then_some(self.bind),
            mcp: self.mcp.then_some(self.mcp_bind),
            tls: self.tls,
            tls_redirect: self.tls_redirect,
        }
    }

    /// What the config file asks for. Keys it doesn't have take their defaults; pinned keys and
    /// values that don't parse keep what the agent started with.
    pub fn wanted_from(&self, file: &[(String, String)]) -> Wanted {
        let get = |key: &str| {
            if self.pinned.contains(&key) {
                return Err(());
            }
            Ok(file
                .iter()
                .rev()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str()))
        };
        let switch = |key: &str, started: bool| match get(key) {
            Err(()) => started,
            Ok(None) => key == KEY_WEB || key == KEY_TLS,
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
            tls: switch(KEY_TLS, self.tls),
            tls_redirect: switch(KEY_TLS_REDIRECT, self.tls_redirect),
        }
    }
}

/// Where the listeners actually are right now (the tray uses it).
#[derive(Debug, Default)]
pub struct Status {
    pub web: Mutex<Option<SocketAddr>>,
    pub mcp: Mutex<Option<SocketAddr>>,
    /// SHA-256 fingerprint of the certificate served, while HTTPS is on.
    pub fingerprint: Mutex<Option<String>>,
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
    /// The address asked for (may have port 0) and the TLS setup.
    wanted: Setup,
    stop: oneshot::Sender<()>,
    task: JoinHandle<()>,
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        let mut task = self.task;
        // Graceful first; long-lived requests (MCP event streams) must not keep the port.
        if tokio::time::timeout(Duration::from_secs(5), &mut task)
            .await
            .is_err()
        {
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
    certs: crate::tls::Certs,
) {
    let mut web_slot = Slot::default();
    let mut mcp_slot = Slot::default();
    let mut certificate = Certificate::default();
    loop {
        let want = *wanted.borrow_and_update();
        let tls = if want.tls && (want.web.is_some() || want.mcp.is_some()) {
            certificate.get(&certs).map(|id| Tls {
                tls: dalker_web::tls::Tls::new(id, want.tls_redirect),
                fingerprint: id.fingerprint.clone(),
            })
        } else {
            None
        };
        *status.fingerprint.lock().unwrap() = tls.as_ref().map(|t| t.fingerprint.clone());
        reconcile_web(&mut web_slot, want.web, tls.as_ref(), &web, &status).await;
        reconcile_mcp(&mut mcp_slot, want.mcp, tls.as_ref(), &mcp, &status).await;
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

/// The certificate, read once (and again after an error, every few seconds).
#[derive(Default)]
struct Certificate {
    identity: Option<dalker_web::tls::Identity>,
    last_error: Option<String>,
}

impl Certificate {
    fn get(&mut self, certs: &crate::tls::Certs) -> Option<&dalker_web::tls::Identity> {
        if self.identity.is_none() {
            match certs.load() {
                Ok(id) => {
                    tracing::info!(
                        fingerprint = %id.fingerprint,
                        "HTTPS certificate: {}",
                        id.cert_path.display()
                    );
                    self.identity = Some(id);
                    self.last_error = None;
                }
                Err(e) => {
                    let e = format!("{e:#}");
                    if self.last_error.as_ref() != Some(&e) {
                        tracing::error!("HTTPS not available: {e} (plain HTTP only; retrying)");
                        self.last_error = Some(e);
                    }
                }
            }
        }
        self.identity.as_ref()
    }
}

/// How the listeners take TLS right now.
struct Tls {
    tls: dalker_web::tls::Tls,
    fingerprint: String,
}

/// What a running listener is compared by: its address, and with TLS the certificate and
/// whether plain HTTP is redirected.
type Setup = (SocketAddr, Option<(String, bool)>);

fn setup(addr: SocketAddr, tls: Option<&Tls>) -> Setup {
    (addr, tls.map(|t| (t.fingerprint.clone(), t.tls.redirect)))
}

async fn reconcile_web(
    slot: &mut Slot,
    want: Option<SocketAddr>,
    tls: Option<&Tls>,
    parts: &WebParts,
    status: &Status,
) {
    if slot.running.as_ref().is_some_and(|r| r.task.is_finished()) {
        slot.running = None;
        *status.web.lock().unwrap() = None;
    }
    let want_setup = want.map(|a| setup(a, tls));
    if slot.running.as_ref().map(|r| r.wanted.clone()) == want_setup {
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
    let tls = tls.map(|t| t.tls.clone());
    let (auth, signaling, extra) = (
        Arc::clone(&parts.auth),
        Arc::clone(&parts.signaling),
        parts.extra.clone(),
    );
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop_rx.await;
        };
        if let Err(e) =
            dalker_web::serve_listener_tls(listener, tls, auth, signaling, extra, shutdown).await
        {
            tracing::error!("web server failed: {e:#}");
        }
    });
    slot.running = Some(Running {
        wanted: want_setup.expect("an address"),
        stop: stop_tx,
        task,
    });
    slot.last_error = None;
    *status.web.lock().unwrap() = Some(local);
}

async fn reconcile_mcp(
    slot: &mut Slot,
    want: Option<SocketAddr>,
    tls: Option<&Tls>,
    parts: &McpParts,
    status: &Status,
) {
    if slot.running.as_ref().is_some_and(|r| r.task.is_finished()) {
        slot.running = None;
        *status.mcp.lock().unwrap() = None;
    }
    let want_setup = want.map(|a| setup(a, tls));
    if slot.running.as_ref().map(|r| r.wanted.clone()) == want_setup {
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
    // With the redirect, other computers' requests are only served over HTTPS (one that
    // still uses http:// is answered with the redirect alone).
    let https_only = tls.is_some_and(|t| t.tls.redirect);
    if !loopback && !parts.allow_plaintext && !https_only {
        return slot.fail(
            "MCP server",
            format!(
                "{addr} is not a loopback address and DALKER_MCP_ALLOW_PLAINTEXT is off (bearer tokens would \
                 cross the network as plain HTTP). Set DALKER_MCP_BIND to 127.0.0.1 and use an SSH tunnel, VPN \
                 or TLS reverse proxy, turn on DALKER_TLS_REDIRECT, or allow plain HTTP on a network you trust"
            ),
        );
    }
    if !loopback {
        let how = match tls {
            Some(t) if t.tls.redirect => "HTTPS",
            Some(_) => "HTTPS and plain HTTP",
            None => "plain HTTP",
        };
        tracing::info!("MCP server reachable from the network on {addr} ({how})");
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
    let tls = tls.map(|t| t.tls.clone());
    match &tls {
        Some(_) => tracing::info!("MCP server on http://{local}/mcp and https://{local}/mcp"),
        None => tracing::info!("MCP server on http://{local}/mcp"),
    }
    let task = tokio::spawn(async move {
        let shutdown = async move {
            let _ = stop_rx.await;
        };
        let served = server.serve_with(shutdown, move |router, shutdown| {
            dalker_web::tls::serve(listener, tls, router, shutdown)
        });
        if let Err(e) = served.await {
            tracing::error!("MCP server failed: {e:#}");
        }
    });
    slot.running = Some(Running {
        wanted: want_setup.expect("an address"),
        stop: stop_tx,
        task,
    });
    slot.last_error = None;
    *status.mcp.lock().unwrap() = Some(local);
}

/// Follows the listener settings in the config file and publishes them on `wanted`.
pub async fn follow_config(
    mut config: watch::Receiver<crate::config_watch::Snapshot>,
    startup: Startup,
    wanted: watch::Sender<Wanted>,
) {
    while config.changed().await.is_ok() {
        let snapshot = config.borrow_and_update().clone();
        let now = startup.wanted_from(&snapshot);
        wanted.send_if_modified(|w| {
            if *w == now {
                return false;
            }
            tracing::info!(
                web = ?now.web,
                mcp = ?now.mcp,
                tls = now.tls,
                tls_redirect = now.tls_redirect,
                "listener settings changed on the host"
            );
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
            tls: true,
            tls_redirect: false,
            pinned,
        }
    }

    fn kv(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn file_settings_and_defaults() {
        let s = startup(vec![]);
        // An empty file means the defaults: web on its default port, MCP off.
        assert_eq!(
            s.wanted_from(&[]),
            Wanted {
                web: Some("0.0.0.0:8742".parse().unwrap()),
                mcp: None,
                tls: true,
                tls_redirect: false,
            }
        );
        let w = s.wanted_from(&kv(&[
            ("DALKER_WEB", "off"),
            ("DALKER_MCP", "on"),
            ("DALKER_MCP_BIND", "127.0.0.1:7000"),
            ("DALKER_TLS", "off"),
            ("DALKER_TLS_REDIRECT", "on"),
        ]));
        assert_eq!(
            w,
            Wanted {
                web: None,
                mcp: Some("127.0.0.1:7000".parse().unwrap()),
                tls: false,
                tls_redirect: true,
            }
        );
        // Values that don't parse keep what the agent started with; the last line wins.
        let w = s.wanted_from(&kv(&[
            ("DALKER_BIND", "0.0.0.0:1"),
            ("DALKER_BIND", "nonsense"),
            ("DALKER_MCP", "maybe"),
        ]));
        assert_eq!(
            w,
            Wanted {
                web: Some("0.0.0.0:9000".parse().unwrap()),
                mcp: None,
                tls: true,
                tls_redirect: false,
            }
        );
    }

    #[test]
    fn pinned_settings_ignore_the_file() {
        let s = startup(vec![KEY_BIND, KEY_MCP, KEY_TLS]);
        let w = s.wanted_from(&kv(&[
            ("DALKER_BIND", "0.0.0.0:1234"),
            ("DALKER_MCP", "on"),
            ("DALKER_WEB", "on"),
            ("DALKER_TLS", "off"),
        ]));
        assert_eq!(
            w,
            Wanted {
                web: Some("0.0.0.0:9000".parse().unwrap()),
                mcp: None,
                tls: true,
                tls_redirect: false,
            }
        );
    }
}
