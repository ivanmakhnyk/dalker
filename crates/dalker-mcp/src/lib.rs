//! # dalker-mcp
//!
//! A [Model Context Protocol](https://modelcontextprotocol.io) server that lets an AI agent see
//! and operate the host the way a viewer does: screenshots, mouse, keyboard and clipboard.
//! It is served as Streamable HTTP on `/mcp` (its own listener, loopback by default).
//!
//! An agent counts as a viewer, so it gets everything a human viewer has: the red border is
//! on while it is connected, screenshots only come from the border-gated capture thread, and
//! keys or buttons it holds are released when it goes away.
//!
//! * Requests need `Authorization: Bearer <token>` ([`tokens`]); a token carries scopes that
//!   are checked per tool. Requests from a browser (any `Origin` header) are refused.
//! * The agent talks to the host through [`DesktopHandle`], so this crate has no WebRTC or
//!   OS dependencies.

pub mod keys;
pub mod screen;
mod session;
pub mod tokens;
mod tools;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Router;
use dalker_core::{InputEvent, RawFrame};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use tokio::sync::watch;

pub use session::RateLimits;
pub use tokens::{Identity, Scope, TokenRecord, TokenStore};

use session::Session;

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Latest captured frame, as the agent's capture thread publishes it. Holding a receiver keeps
/// the capture running (once the border is up).
pub type FrameWatch = watch::Receiver<Option<Arc<RawFrame>>>;

/// What an MCP session sends to the host's input thread.
#[derive(Debug, Clone, PartialEq)]
pub enum HostCommand {
    Input(InputEvent),
    /// Put this text on the host's clipboard, in order with the input.
    SetClipboard(String),
    /// The session is over: release what it holds.
    EndSession,
}

/// The host as the MCP server sees it. Implemented by the agent.
pub trait DesktopHandle: Send + Sync + 'static {
    /// Host facts for `get_host_info` (hostname, OS, capture backend, version, locked...).
    fn info(&self) -> serde_json::Value;
    /// Subscribes to captured frames.
    fn frames(&self) -> FrameWatch;
    /// False while frames must not be served (the border can't be shown).
    fn screen_ready(&self) -> bool {
        true
    }
    /// False when the host has no input backend (view-only).
    fn input_available(&self) -> bool {
        true
    }
    /// Takes a viewer slot (the border goes on) and returns a new input-session id, or why not.
    fn open_viewer(&self) -> Result<u64, String>;
    /// Gives the slot back.
    fn close_viewer(&self, id: u64);
    fn send(&self, id: u64, cmd: HostCommand);
    /// Resolves once everything sent for `id` so far has been applied by the host.
    fn flush(&self, id: u64) -> BoxFuture<()> {
        let _ = id;
        Box::pin(std::future::ready(()))
    }
    /// Reads the host's clipboard text now (`None` when it holds no text).
    fn read_clipboard(&self) -> BoxFuture<Result<Option<String>, String>>;
}

#[derive(Debug, Clone)]
pub struct McpConfig {
    /// AI agents (tokens) connected at once. They also count toward the agent's viewer limit.
    pub max_sessions: usize,
    /// A session without tool calls for this long ends.
    pub idle_timeout: Duration,
    pub limits: RateLimits,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self { max_sessions: 1, idle_timeout: Duration::from_secs(600), limits: RateLimits::default() }
    }
}

pub(crate) struct Shared {
    desktop: Arc<dyn DesktopHandle>,
    tokens: Arc<TokenStore>,
    config: McpConfig,
    /// By token id.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Set from the host ("Disconnect AI agents"): no tool call is served until cleared.
    blocked: AtomicBool,
}

impl Shared {
    /// The session of `who`, starting one if needed.
    pub(crate) fn session_for(&self, who: &Identity) -> Result<Arc<Session>, String> {
        if self.blocked.load(Ordering::SeqCst) {
            return Err("the person at the host has disconnected AI agents; ask them to allow AI agents again".into());
        }
        let mut sessions = self.sessions.lock().unwrap();
        if let Some(s) = sessions.get(&who.token_id) {
            if !s.is_closed() {
                s.touch();
                return Ok(Arc::clone(s));
            }
        }
        sessions.retain(|_, s| !s.is_closed());
        if sessions.len() >= self.config.max_sessions {
            return Err(format!("too many AI agents connected (max {})", self.config.max_sessions));
        }
        let id = self.desktop.open_viewer()?;
        let s = Arc::new(Session::new(
            id,
            who.token_id.clone(),
            who.name.clone(),
            Arc::clone(&self.desktop),
            self.config.limits,
        ));
        sessions.insert(who.token_id.clone(), Arc::clone(&s));
        tracing::info!(token = %who.name, session = id, "AI agent connected");
        Ok(s)
    }

    fn end_where(&self, why: &str, mut pred: impl FnMut(&Session) -> bool) -> usize {
        let ended: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().unwrap();
            let ids: Vec<String> = sessions.iter().filter(|(_, s)| pred(s)).map(|(k, _)| k.clone()).collect();
            ids.iter().filter_map(|k| sessions.remove(k)).collect()
        };
        let mut n = 0;
        for s in ended {
            if s.close() {
                tracing::info!(token = %s.token_name, session = s.id, "AI agent disconnected: {why}");
                n += 1;
            }
        }
        n
    }

    fn reap(&self) {
        let now = Instant::now();
        let unix = tokens::unix_now();
        let idle = self.config.idle_timeout;
        self.end_where("idle", |s| s.idle_for(now) >= idle);
        self.end_where("token revoked or expired", |s| !self.tokens.is_live(&s.token_id, unix));
    }
}

/// The MCP server. Cheap to clone.
#[derive(Clone)]
pub struct McpServer {
    shared: Arc<Shared>,
}

impl McpServer {
    pub fn new(desktop: Arc<dyn DesktopHandle>, tokens: Arc<TokenStore>, config: McpConfig) -> Self {
        Self {
            shared: Arc::new(Shared {
                desktop,
                tokens,
                config,
                sessions: Mutex::new(HashMap::new()),
                blocked: AtomicBool::new(false),
            }),
        }
    }

    pub fn tokens(&self) -> &Arc<TokenStore> {
        &self.shared.tokens
    }

    /// Installs the host's current token records; sessions of tokens that were removed or
    /// changed end at once.
    pub fn set_tokens(&self, records: Vec<TokenRecord>) {
        let gone = self.shared.tokens.replace(records);
        if !gone.is_empty() {
            self.shared.end_where("token revoked or changed", |s| gone.contains(&s.token_id));
        }
    }

    /// Connected AI agents.
    pub fn session_count(&self) -> usize {
        self.shared.sessions.lock().unwrap().values().filter(|s| !s.is_closed()).count()
    }

    /// Ends every session (held input is released). Returns how many there were.
    pub fn disconnect_all(&self, why: &str) -> usize {
        self.shared.end_where(why, |_| true)
    }

    /// While blocked, sessions are ended and no tool call is served.
    pub fn set_blocked(&self, blocked: bool) {
        self.shared.blocked.store(blocked, Ordering::SeqCst);
        if blocked {
            let n = self.disconnect_all("disconnected by the person at the host");
            tracing::info!(ended = n, "AI agents blocked on the host");
        } else {
            tracing::info!("AI agents allowed again on the host");
        }
    }

    pub fn is_blocked(&self) -> bool {
        self.shared.blocked.load(Ordering::SeqCst)
    }

    /// The `/mcp` endpoint with auth in front of it.
    pub fn router(&self) -> Router {
        let shared = Arc::clone(&self.shared);
        let config = StreamableHttpServerConfig::default()
            // Every request carries a bearer token a browser page can't know, and requests
            // with an Origin header are refused below, so Host checks would only get in the way
            // of tunnels and reverse proxies.
            .disable_allowed_hosts()
            .enforce_origin_validation()
            .with_max_request_body_bytes(4 * 1024 * 1024);
        let mcp = StreamableHttpService::new(
            move || Ok(tools::DalkerTools::new(Arc::clone(&shared))),
            LocalSessionManager::default().into(),
            config,
        );
        Router::new()
            .nest_service("/mcp", mcp)
            .layer(axum::middleware::from_fn_with_state(Arc::clone(&self.shared), authenticate))
            .layer(axum::middleware::from_fn(no_proxy_buffering))
    }

    /// Serves `/mcp` on `listener` until `shutdown` resolves; also ends idle and revoked
    /// sessions in the background.
    pub async fn serve(
        &self,
        listener: tokio::net::TcpListener,
        shutdown: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let shared = Arc::downgrade(&self.shared);
        // Stopped with the listener (even when this future is dropped): the host can turn the
        // server off and on again.
        struct AbortOnDrop(tokio::task::JoinHandle<()>);
        impl Drop for AbortOnDrop {
            fn drop(&mut self) {
                self.0.abort();
            }
        }
        let _reaper = AbortOnDrop(tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(5)).await;
                let Some(shared) = shared.upgrade() else { return };
                shared.reap();
            }
        }));
        tracing::info!("MCP server on http://{}/mcp", listener.local_addr()?);
        let me = self.clone();
        axum::serve(listener, self.router())
            .with_graceful_shutdown(async move {
                shutdown.await;
                me.disconnect_all("server shutting down");
            })
            .await?;
        Ok(())
    }
}

/// Event streams must reach the agent as they happen: nginx buffers responses unless told not
/// to (other proxies follow `Cache-Control: no-cache` / `text/event-stream`).
async fn no_proxy_buffering(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    resp.headers_mut().insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

fn reject(status: StatusCode, msg: &str) -> Response {
    let mut r = (status, axum::Json(serde_json::json!({ "error": msg }))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Bearer realm=\"dalker\""));
    }
    r
}

/// Bearer auth for every request; the identity travels to the tools in the request extensions.
async fn authenticate(State(shared): State<Arc<Shared>>, mut req: Request, next: Next) -> Response {
    if req.headers().contains_key(header::ORIGIN) {
        // A web page (or a DNS-rebinding attempt). MCP clients don't send Origin.
        tracing::warn!("MCP request with an Origin header refused");
        return reject(StatusCode::FORBIDDEN, "browser requests are not allowed");
    }
    let token = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|t| t.trim().to_owned());
    let Some(token) = token else {
        return reject(StatusCode::UNAUTHORIZED, "missing bearer token");
    };
    let tokens = Arc::clone(&shared.tokens);
    // Argon2 the first time a token is seen: keep it off the async workers.
    let result = tokio::task::spawn_blocking(move || tokens.verify(&token, tokens::unix_now())).await;
    match result {
        Ok(Ok(who)) => {
            req.extensions_mut().insert(who);
            next.run(req).await
        }
        Ok(Err(tokens::AuthError::Expired)) => reject(StatusCode::UNAUTHORIZED, "token expired"),
        _ => {
            tracing::warn!("MCP request with an invalid token refused");
            tokio::time::sleep(Duration::from_millis(500)).await;
            reject(StatusCode::UNAUTHORIZED, "invalid token")
        }
    }
}
