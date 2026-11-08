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
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
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
    /// Whether a viewer slot is free now, or why not (what [`Self::open_viewer`] would say).
    /// Checked when an agent connects, so it fails fast instead of on its first tool call.
    fn check_viewer_slot(&self) -> Result<(), String> {
        Ok(())
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
    /// A session ends this long after its agent has no request left open (no tool call
    /// running, no event stream): the agent may have gone away without saying so.
    pub detached_timeout: Duration,
    /// A session ends this long after its agent's event stream closed with nothing else open:
    /// the agent went away, unless it reconnects first.
    pub reconnect_grace: Duration,
    pub limits: RateLimits,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            max_sessions: 1,
            idle_timeout: Duration::from_secs(600),
            detached_timeout: Duration::from_secs(120),
            reconnect_grace: Duration::from_secs(10),
            limits: RateLimits::default(),
        }
    }
}

pub(crate) struct Shared {
    desktop: Arc<dyn DesktopHandle>,
    tokens: Arc<TokenStore>,
    config: McpConfig,
    /// By token id.
    sessions: Mutex<HashMap<String, Arc<Session>>>,
    /// Open HTTP requests, by token id.
    links: Mutex<HashMap<String, Link>>,
    /// Set from the host ("Disconnect AI agents"): no tool call is served until cleared.
    blocked: AtomicBool,
}

const BLOCKED: &str =
    "the person at the host has disconnected AI agents; ask them to allow AI agents again";

/// The HTTP requests one token has open: tool calls in flight and event streams the agent
/// keeps open. While there are none, the token's session is on a short clock.
struct Link {
    open: usize,
    /// Set once nothing is open: the session ends then unless a request comes first.
    release_at: Option<(Instant, &'static str)>,
}

/// How an HTTP request of an agent ended, which sets how long its session may wait for more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Closed {
    /// A POST was answered: the agent may be thinking about the next call.
    Request,
    /// A GET event stream ended: the agent went away or lost its connection.
    Stream,
    /// The agent closed its MCP session.
    Delete,
}

impl Shared {
    /// Whether `who` may connect now: refused while AI agents are blocked or there is no free
    /// slot (unless it has a session already).
    pub(crate) fn admit(&self, who: &Identity) -> Result<(), String> {
        if self.blocked.load(Ordering::SeqCst) {
            return Err(BLOCKED.into());
        }
        let sessions = self.sessions.lock().unwrap();
        if sessions.get(&who.token_id).is_some_and(|s| !s.is_closed()) {
            return Ok(());
        }
        if sessions.values().filter(|s| !s.is_closed()).count() >= self.config.max_sessions {
            return Err(self.too_many());
        }
        self.desktop.check_viewer_slot()
    }

    fn too_many(&self) -> String {
        format!(
            "too many AI agents connected (max {})",
            self.config.max_sessions
        )
    }

    /// The session of `who`, starting one if needed.
    pub(crate) fn session_for(&self, who: &Identity) -> Result<Arc<Session>, String> {
        if self.blocked.load(Ordering::SeqCst) {
            return Err(BLOCKED.into());
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
            return Err(self.too_many());
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
            let ids: Vec<String> = sessions
                .iter()
                .filter(|(_, s)| pred(s))
                .map(|(k, _)| k.clone())
                .collect();
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

    fn request_opened(&self, token_id: &str) {
        let mut links = self.links.lock().unwrap();
        let link = links.entry(token_id.to_owned()).or_insert(Link {
            open: 0,
            release_at: None,
        });
        link.open += 1;
        link.release_at = None;
    }

    fn request_closed(&self, token_id: &str, how: Closed) {
        {
            let mut links = self.links.lock().unwrap();
            let Some(link) = links.get_mut(token_id) else {
                return;
            };
            link.open = link.open.saturating_sub(1);
            if link.open > 0 {
                return;
            }
            let (wait, why) = match how {
                Closed::Request => (self.config.detached_timeout, "no request for a while"),
                Closed::Stream => (self.config.reconnect_grace, "connection closed"),
                Closed::Delete => (Duration::ZERO, "session closed by the agent"),
            };
            link.release_at = Some((Instant::now() + wait, why));
        }
        if how == Closed::Delete {
            self.release_detached(Instant::now());
        }
    }

    /// Ends the sessions of tokens that have had nothing open for long enough.
    fn release_detached(&self, now: Instant) {
        let mut due: Vec<(String, &'static str)> = Vec::new();
        self.links
            .lock()
            .unwrap()
            .retain(|token, link| match link.release_at {
                Some((at, why)) if link.open == 0 && at <= now => {
                    due.push((token.clone(), why));
                    false
                }
                _ => true,
            });
        for (token, why) in due {
            self.end_where(why, |s| s.token_id == token);
        }
    }

    fn reap(&self) {
        let now = Instant::now();
        self.release_detached(now);
        let unix = tokens::unix_now();
        let idle = self.config.idle_timeout;
        self.end_where("idle", |s| s.idle_for(now) >= idle);
        self.end_where("token revoked or expired", |s| {
            !self.tokens.is_live(&s.token_id, unix)
        });
    }
}

/// The MCP server. Cheap to clone.
#[derive(Clone)]
pub struct McpServer {
    shared: Arc<Shared>,
}

impl McpServer {
    pub fn new(
        desktop: Arc<dyn DesktopHandle>,
        tokens: Arc<TokenStore>,
        config: McpConfig,
    ) -> Self {
        Self {
            shared: Arc::new(Shared {
                desktop,
                tokens,
                config,
                sessions: Mutex::new(HashMap::new()),
                links: Mutex::new(HashMap::new()),
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
            self.shared
                .end_where("token revoked or changed", |s| gone.contains(&s.token_id));
        }
    }

    /// Connected AI agents.
    pub fn session_count(&self) -> usize {
        self.shared
            .sessions
            .lock()
            .unwrap()
            .values()
            .filter(|s| !s.is_closed())
            .count()
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
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&self.shared),
                authenticate,
            ))
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
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(shared) = shared.upgrade() else {
                    return;
                };
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
    resp.headers_mut()
        .insert("x-accel-buffering", HeaderValue::from_static("no"));
    resp
}

fn reject(status: StatusCode, msg: &str) -> Response {
    let mut r = (status, axum::Json(serde_json::json!({ "error": msg }))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        r.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            HeaderValue::from_static("Bearer realm=\"dalker\""),
        );
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
        .and_then(|v| {
            v.strip_prefix("Bearer ")
                .or_else(|| v.strip_prefix("bearer "))
        })
        .map(|t| t.trim().to_owned());
    let Some(token) = token else {
        return reject(StatusCode::UNAUTHORIZED, "missing bearer token");
    };
    let tokens = Arc::clone(&shared.tokens);
    // Argon2 the first time a token is seen: keep it off the async workers.
    let result =
        tokio::task::spawn_blocking(move || tokens.verify(&token, tokens::unix_now())).await;
    match result {
        Ok(Ok(who)) => {
            let how = match *req.method() {
                Method::GET => Closed::Stream,
                Method::DELETE => Closed::Delete,
                _ => Closed::Request,
            };
            // Counts as open until the response body is done or dropped (the client went away).
            let open = OpenRequest::new(Arc::clone(&shared), who.token_id.clone(), how);
            req.extensions_mut().insert(who);
            next.run(req)
                .await
                .map(|body| Body::new(TrackedBody { body, _open: open }))
        }
        Ok(Err(tokens::AuthError::Expired)) => reject(StatusCode::UNAUTHORIZED, "token expired"),
        _ => {
            tracing::warn!("MCP request with an invalid token refused");
            tokio::time::sleep(Duration::from_millis(500)).await;
            reject(StatusCode::UNAUTHORIZED, "invalid token")
        }
    }
}

/// One open HTTP request of an agent, until dropped.
struct OpenRequest {
    shared: Arc<Shared>,
    token_id: String,
    how: Closed,
}

impl OpenRequest {
    fn new(shared: Arc<Shared>, token_id: String, how: Closed) -> Self {
        shared.request_opened(&token_id);
        Self {
            shared,
            token_id,
            how,
        }
    }
}

impl Drop for OpenRequest {
    fn drop(&mut self) {
        self.shared.request_closed(&self.token_id, self.how);
    }
}

/// A response body that keeps its request open while it is being sent: an event stream ends
/// when the client disconnects, which hyper notices on the next keep-alive at the latest.
struct TrackedBody {
    body: Body,
    _open: OpenRequest,
}

impl http_body::Body for TrackedBody {
    type Data = Bytes;
    type Error = axum::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<http_body::Frame<Bytes>, axum::Error>>> {
        Pin::new(&mut self.body).poll_frame(cx)
    }

    fn is_end_stream(&self) -> bool {
        self.body.is_end_stream()
    }

    fn size_hint(&self) -> http_body::SizeHint {
        self.body.size_hint()
    }
}
