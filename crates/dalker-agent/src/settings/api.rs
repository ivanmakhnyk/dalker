//! The settings API behind `web-ui/settings.html`, one implementation for every place the
//! page is shown:
//!
//! * the host's settings window (`dalkerd settings`): everything, authorized by a per-launch
//!   key ([`Access::Host`]);
//! * the web client (the agent's web server): the MCP tokens only, for signed-in viewers
//!   ([`Access::Tokens`]). The password, the switches and the ports stay host-only.
//!
//! | Route | Body | |
//! |---|---|---|
//! | `GET /api/settings` | | the whole state ([`state_json`]) |
//! | `POST /api/settings/web` | `{enabled?, port?}` | host only |
//! | `POST /api/settings/password` | `{password, repeat}` | host only |
//! | `POST /api/settings/mcp` | `{enabled?, port?, remote?}` | host only |
//! | `POST /api/settings/stream` | `{fps?, bitrate_kbps?, max_width?}` | host only: stream limits |
//! | `POST /api/settings/tokens` | `{name, remote, clipboard}` | → `{token, state}` |
//! | `PATCH /api/settings/tokens/{id}` | `{remote, clipboard}` | |
//! | `DELETE /api/settings/tokens/{id}` | | |
//! | `GET /api/settings/tokens/{id}/secret` | | → `{token}` |
//! | `POST /api/settings/screen-sharing` | | host only: show the portal's "share your screen" dialog now → `{allowed, state}` |
//!
//! Every change answers with the new state, so the page redraws from one source.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use dalker_mcp::tokens::{self, Scope};
use serde_json::{json, Value};

use super::store::Store;
use super::{
    date, masked, parse_port, remote_of, scopes_for, with_permissions, Remote, Saved,
    MIN_PASSWORD_LEN,
};
use crate::cli::OnOff;
use crate::listeners::{KEY_BIND, KEY_MCP, KEY_MCP_BIND, KEY_WEB};
use crate::stream;

/// What a caller of the API may change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// The host's settings window: everything.
    Host,
    /// A signed-in web client: list, create, change, copy and revoke MCP tokens.
    Tokens,
}

/// Whether each server answers right now: `None` = not known yet.
#[derive(Debug, Default, Clone, Copy)]
pub struct Live {
    pub web: Option<bool>,
    pub mcp: Option<bool>,
}

type Check = Box<dyn Fn(&HeaderMap) -> bool + Send + Sync>;
type Observer = Box<dyn Fn(&Saved) + Send + Sync>;

pub struct Api {
    store: Mutex<Box<dyn Store>>,
    access: Access,
    /// Allows the request (per-launch key, or a web client session).
    authorized: Check,
    /// Where the servers are running, for the status pills.
    live: Box<dyn Fn() -> Live + Send + Sync>,
    /// Why tokens can't be changed here (shown instead of the controls), if they can't.
    blocked: Option<String>,
    /// Told what the file says after every read (the window probes those addresses).
    observe: Option<Observer>,
}

impl Api {
    pub fn new(
        store: Box<dyn Store>,
        access: Access,
        authorized: impl Fn(&HeaderMap) -> bool + Send + Sync + 'static,
        live: impl Fn() -> Live + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: Mutex::new(store),
            access,
            authorized: Box::new(authorized),
            live: Box::new(live),
            blocked: None,
            observe: None,
        }
    }

    /// Calls `f` with what the file says after every read.
    pub fn on_read(mut self, f: impl Fn(&Saved) + Send + Sync + 'static) -> Self {
        self.observe = Some(Box::new(f));
        self
    }

    /// An API that can only explain why the settings can't be changed from here.
    pub fn unavailable(
        why: String,
        authorized: impl Fn(&HeaderMap) -> bool + Send + Sync + 'static,
    ) -> Self {
        Self {
            store: Mutex::new(Box::new(NoStore)),
            access: Access::Tokens,
            authorized: Box::new(authorized),
            live: Box::new(Live::default),
            blocked: Some(why),
            observe: None,
        }
    }

    pub fn router(self) -> Router {
        Router::new()
            .route("/api/settings", get(get_state))
            .route("/api/settings/web", post(set_web))
            .route("/api/settings/password", post(set_password))
            .route("/api/settings/mcp", post(set_mcp))
            .route("/api/settings/stream", post(set_stream))
            .route("/api/settings/tokens", post(create_token))
            .route(
                "/api/settings/tokens/{id}",
                patch(change_token).delete(revoke_token),
            )
            .route("/api/settings/tokens/{id}/secret", get(token_secret))
            .route("/api/settings/screen-sharing", post(allow_screen_sharing))
            .with_state(Arc::new(self))
    }

    fn read(&self) -> Result<Saved, String> {
        let saved = self
            .store
            .lock()
            .unwrap()
            .read()
            .map(|t| Saved::parse(&t))?;
        if let Some(f) = &self.observe {
            f(&saved);
        }
        Ok(saved)
    }

    fn edit(&self, set: &[(String, String)], remove: &[String]) -> Result<Saved, String> {
        self.store.lock().unwrap().edit(set, remove)?;
        self.read()
    }

    fn path(&self) -> String {
        self.store.lock().unwrap().path().display().to_string()
    }
}

/// Stands in for a store where none can be opened.
struct NoStore;

impl Store for NoStore {
    fn read(&mut self) -> Result<String, String> {
        Ok(String::new())
    }
    fn edit(&mut self, _: &[(String, String)], _: &[String]) -> Result<(), String> {
        Err("the settings can't be changed here".into())
    }
    fn path(&self) -> &std::path::Path {
        std::path::Path::new("")
    }
}

type ApiState = State<Arc<Api>>;

fn fail(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(json!({ "error": msg.into() }))).into_response()
}

/// Runs `f` off the async workers (argon2, pipes to the pkexec helper) after the checks.
async fn guarded<F>(api: Arc<Api>, headers: HeaderMap, host_only: bool, f: F) -> Response
where
    F: FnOnce(&Api) -> Result<Value, (StatusCode, String)> + Send + 'static,
{
    if !(api.authorized)(&headers) {
        return fail(StatusCode::UNAUTHORIZED, "not signed in");
    }
    if host_only && api.access != Access::Host {
        return fail(
            StatusCode::FORBIDDEN,
            "this setting can only be changed on the host, in Dalker Settings",
        );
    }
    if let Some(why) = &api.blocked {
        return fail(StatusCode::SERVICE_UNAVAILABLE, why.clone());
    }
    match tokio::task::spawn_blocking(move || f(&api)).await {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err((status, msg))) => fail(status, msg),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

fn bad(msg: impl Into<String>) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, msg.into())
}

fn saved_or_500(r: Result<Saved, String>) -> Result<Saved, (StatusCode, String)> {
    r.map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("The settings could not be saved: {e}"),
        )
    })
}

/// The whole state the page draws.
pub fn state_json(api: &Api, saved: &Saved) -> Value {
    let live = (api.live)();
    let now = tokens::unix_now();
    let token_rows: Vec<Value> = saved
        .tokens
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "name": r.name,
                "masked": masked(r),
                "remote": match remote_of(r) { Remote::Full => "full", Remote::ViewOnly => "view", Remote::None => "none" },
                "clipboard": r.scopes.contains(&Scope::Clipboard),
                "created": date(r.created),
                "expires": r.expires.map(date),
                "expired": r.expired_at(now),
                "copyable": saved.secrets.contains_key(&r.id),
            })
        })
        .collect();
    let host = api.access == Access::Host;
    // The web client reaches the host by the name it opened it with (see `mcpUrl()` in
    // settings.js), so only the host's own window lists the host's network addresses.
    let addrs = if host { host_addresses() } else { Vec::new() };
    let mcp = reach(saved.mcp_bind, &addrs, "/mcp");
    json!({
        "access": if host { "host" } else { "tokens" },
        "version": env!("CARGO_PKG_VERSION"),
        "blocked": api.blocked,
        "file": if host { Some(api.path()) } else { None },
        "web": {
            "enabled": saved.web,
            "port": saved.bind.port(),
            "status": status(saved.web, live.web),
            "password_set": saved.password_set,
            "loopback": saved.bind.ip().is_loopback(),
            "urls": reach(saved.bind, &addrs, "/"),
        },
        "mcp": {
            "enabled": saved.mcp,
            "port": saved.mcp_bind.port(),
            "url": mcp[0]["url"],
            "urls": mcp,
            "public_url": saved.mcp_public_url,
            "loopback": saved.mcp_bind.ip().is_loopback(),
            "status": status(saved.mcp, live.mcp),
        },
        "stream": {
            "fps": saved.stream.fps,
            "bitrate_kbps": saved.stream.bitrate_kbps,
            "max_width": saved.stream.max_width,
            "default": {
                "fps": stream::DEFAULT_FPS,
                "bitrate_kbps": stream::DEFAULT_BITRATE_KBPS,
                "max_width": stream::DEFAULT_MAX_WIDTH,
            },
            "range": {
                "fps": [1, stream::MAX_FPS],
                "bitrate_kbps": [stream::MIN_BITRATE_KBPS, stream::MAX_BITRATE_KBPS],
                "max_width": [stream::MIN_MAX_WIDTH, stream::MAX_MAX_WIDTH],
            },
        },
        "tokens": token_rows,
        "min_password": MIN_PASSWORD_LEN,
        // `null` where nobody at the host is asked to allow screen sharing.
        "screen_sharing": (host && portal_desktop()).then(|| json!({
            "allowed": dalker_platform::screen_sharing_allowed(),
        })),
    })
}

/// Whether this desktop shares the screen through xdg-desktop-portal (asked once: the page
/// redraws every two seconds).
fn portal_desktop() -> bool {
    static PORTAL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *PORTAL.get_or_init(dalker_platform::screen_sharing_needs_permission)
}

/// Shows the "share your screen" dialog while the person is at the computer and keeps the
/// answer, so a remote viewer later doesn't wait for somebody to click Share. The settings
/// window runs in the desktop user's session, like the agent that captures.
async fn allow_screen_sharing(State(api): ApiState, headers: HeaderMap) -> Response {
    guarded(api, headers, true, move |api| {
        let allowed = dalker_platform::allow_screen_sharing_now()
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;
        tracing::info!(allowed, "screen sharing asked ahead of time");
        let saved = api.read().map_err(bad)?;
        Ok(json!({ "allowed": allowed, "state": state_json(api, &saved) }))
    })
    .await
}

/// `running`, `stopped`, `down` (on but not answering), `stopping` or `checking`.
fn status(enabled: bool, up: Option<bool>) -> &'static str {
    match (enabled, up) {
        (_, None) => "checking",
        (true, Some(true)) => "running",
        (false, Some(false)) => "stopped",
        (true, Some(false)) => "down",
        (false, Some(true)) => "stopping",
    }
}

/// A network address of this computer, as the settings page offers it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct HostAddr {
    ip: IpAddr,
    /// The interface it is on, e.g. `Ethernet` or `vEthernet (Default Switch)`.
    name: String,
}

/// This computer's addresses other computers can use: no loopback or link-local ones, LAN
/// (private) IPv4 addresses first, then other IPv4 ones (VPNs, public), then IPv6.
fn host_addresses() -> Vec<HostAddr> {
    let found = match if_addrs::get_if_addrs() {
        Ok(found) => found,
        Err(e) => {
            tracing::debug!("network addresses: {e}");
            return Vec::new();
        }
    };
    let mut addrs: Vec<HostAddr> = found
        .into_iter()
        .filter(|i| {
            i.is_oper_up() && !i.is_loopback() && !i.is_link_local() && !i.ip().is_unspecified()
        })
        .map(|i| HostAddr {
            ip: i.ip(),
            name: i.name,
        })
        .collect();
    addrs.sort_by_key(|a| match a.ip {
        IpAddr::V4(v4) if v4.is_private() => 0,
        IpAddr::V4(_) => 1,
        IpAddr::V6(_) => 2,
    });
    addrs.dedup_by(|a, b| a.ip == b.ip);
    addrs
}

/// The addresses a server bound to `bind` answers on, best first, as `{host, name, url}`
/// (`path` is appended to each URL). One logic for the web server and the MCP server:
///
/// * bound to every address (`0.0.0.0` or `[::]`): each of `addrs` of that family, then this
///   computer's own `127.0.0.1` (or `[::1]`) for clients on the host;
/// * bound to loopback or to one address: that address alone.
///
/// Never empty.
fn reach(bind: SocketAddr, addrs: &[HostAddr], path: &str) -> Vec<Value> {
    let url = |ip: IpAddr| match ip {
        IpAddr::V6(v6) => format!("http://[{v6}]:{}{path}", bind.port()),
        IpAddr::V4(v4) => format!("http://{v4}:{}{path}", bind.port()),
    };
    let entry =
        |ip: IpAddr, name: &str| json!({ "host": ip.to_string(), "name": name, "url": url(ip) });
    match bind.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => addrs
            .iter()
            .filter(|a| a.ip.is_ipv4())
            .map(|a| entry(a.ip, &a.name))
            .chain([entry(IpAddr::V4(Ipv4Addr::LOCALHOST), "This computer only")])
            .collect(),
        // `[::]` also takes IPv4 connections on most systems.
        IpAddr::V6(v6) if v6.is_unspecified() => addrs
            .iter()
            .map(|a| entry(a.ip, &a.name))
            .chain([entry(IpAddr::V6(Ipv6Addr::LOCALHOST), "This computer only")])
            .collect(),
        ip if ip.is_loopback() => vec![entry(ip, "This computer only")],
        ip => vec![entry(ip, "")],
    }
}

async fn get_state(State(api): ApiState, headers: HeaderMap) -> Response {
    if !(api.authorized)(&headers) {
        return fail(StatusCode::UNAUTHORIZED, "not signed in");
    }
    let result = tokio::task::spawn_blocking(move || {
        let saved = if api.blocked.is_some() {
            Ok(Saved::parse(""))
        } else {
            api.read()
        };
        saved.map(|s| state_json(&api, &s))
    })
    .await;
    match result {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => fail(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("The settings can't be read: {e}"),
        ),
        Err(_) => fail(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

/// `port` checked against the other server's port.
fn check_port(
    port: u64,
    other: SocketAddr,
    own_ip: std::net::IpAddr,
    what: &str,
) -> Result<u16, (StatusCode, String)> {
    let port = parse_port(&port.to_string()).map_err(|e| bad(format!("{what} port: {e}")))?;
    let clash = own_ip == other.ip() || own_ip.is_unspecified() || other.ip().is_unspecified();
    if port == other.port() && clash {
        return Err(bad(format!(
            "The web server and the MCP server can't both use port {port}."
        )));
    }
    Ok(port)
}

async fn set_web(State(api): ApiState, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    guarded(api, headers, true, move |api| {
        let saved = api.read().map_err(bad)?;
        let mut set = Vec::new();
        if let Some(on) = body["enabled"].as_bool() {
            set.push((KEY_WEB.to_owned(), OnOff::word(on).to_owned()));
        }
        if let Some(port) = body["port"].as_u64() {
            let port = check_port(port, saved.mcp_bind, saved.bind.ip(), "Web server")?;
            set.push((KEY_BIND.to_owned(), SocketAddr::new(saved.bind.ip(), port).to_string()));
        } else if !body["port"].is_null() {
            return Err(bad("Web server port: enter a number from 1024 to 65535."));
        }
        let saved = saved_or_500(api.edit(&set, &[]))?;
        tracing::info!(keys = ?set.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), "settings saved");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

async fn set_mcp(State(api): ApiState, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    guarded(api, headers, true, move |api| {
        let saved = api.read().map_err(bad)?;
        let mut set = Vec::new();
        if let Some(on) = body["enabled"].as_bool() {
            set.push((KEY_MCP.to_owned(), OnOff::word(on).to_owned()));
        }
        // "remote": other computers may connect (0.0.0.0) or only this one (127.0.0.1).
        let ip = match body["remote"].as_bool() {
            Some(true) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Some(false) => IpAddr::V4(Ipv4Addr::LOCALHOST),
            None => saved.mcp_bind.ip(),
        };
        let port = if let Some(port) = body["port"].as_u64() {
            check_port(port, saved.bind, ip, "MCP server")?
        } else if !body["port"].is_null() {
            return Err(bad("MCP server port: enter a number from 1024 to 65535."));
        } else {
            saved.mcp_bind.port()
        };
        let bind = SocketAddr::new(ip, port);
        if bind != saved.mcp_bind {
            set.push((KEY_MCP_BIND.to_owned(), bind.to_string()));
        }
        let saved = saved_or_500(api.edit(&set, &[]))?;
        tracing::info!(keys = ?set.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), "settings saved");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

/// The stream limits every viewer gets: the most frame rate and bitrate, and the width wider
/// screens are scaled down to. The agent applies them to running streams too.
async fn set_stream(State(api): ApiState, headers: HeaderMap, Json(body): Json<Value>) -> Response {
    guarded(api, headers, true, move |api| {
        type Parse = fn(&str) -> Result<u32, String>;
        let fields: [(&str, &str, &str, Parse); 3] = [
            ("fps", stream::KEY_FPS, "Frame rate", stream::parse_fps),
            ("bitrate_kbps", stream::KEY_BITRATE_KBPS, "Bitrate", stream::parse_bitrate_kbps),
            ("max_width", stream::KEY_MAX_WIDTH, "Width", stream::parse_max_width),
        ];
        let mut set = Vec::new();
        for (field, key, what, parse) in fields {
            let value = &body[field];
            if value.is_null() {
                continue;
            }
            let v = value
                .as_u64()
                .ok_or_else(|| bad(format!("{what}: enter a whole number.")))?;
            let v = parse(&v.to_string()).map_err(|e| bad(format!("{}.", capitalize(&e))))?;
            set.push((key.to_owned(), v.to_string()));
        }
        let saved = saved_or_500(api.edit(&set, &[]))?;
        tracing::info!(keys = ?set.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>(), "settings saved");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

async fn set_password(
    State(api): ApiState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    guarded(api, headers, true, move |api| {
        let new = body["password"].as_str().unwrap_or_default();
        let repeat = body["repeat"].as_str().unwrap_or_default();
        super::check_new_password(new, repeat, MIN_PASSWORD_LEN).map_err(bad)?;
        let hash = dalker_web::hash_password(new).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("The password could not be hashed: {e:#}"),
            )
        })?;
        // A plain-text password line has no business in the file once there is a hash.
        let saved = saved_or_500(api.edit(
            &[("DALKER_PASSWORD_HASH".to_owned(), hash)],
            &["DALKER_PASSWORD".to_owned()],
        ))?;
        tracing::info!("access password changed in the settings");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

async fn create_token(
    State(api): ApiState,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    guarded(api, headers, false, move |api| {
        let name = body["name"].as_str().unwrap_or_default().trim().to_owned();
        tokens::validate_name(&name).map_err(|e| bad(capitalize(&e)))?;
        let saved = api.read().map_err(bad)?;
        if saved.tokens.iter().any(|t| t.name == name) {
            return Err(bad(format!("There is already a token named “{name}”.")));
        }
        let scopes = scopes_for(body["remote"].as_bool().unwrap_or(false), body["clipboard"].as_bool().unwrap_or(false))
            .map_err(bad)?;
        let (token, record) = tokens::generate(&name, scopes, None)
            .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("{e:#}")))?;
        // The token itself is kept too (admin-only file), so it can be copied again later.
        let saved = saved_or_500(api.edit(
            &[(record.config_key(), record.config_value()), (record.secret_key(), token.clone())],
            &[],
        ))?;
        tracing::info!(token = %record.name, id = %record.id, scopes = %tokens::format_scopes(&record.scopes), "MCP token created in the settings");
        Ok(json!({ "token": token, "state": state_json(api, &saved) }))
    })
    .await
}

async fn change_token(
    State(api): ApiState,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    guarded(api, headers, false, move |api| {
        let saved = api.read().map_err(bad)?;
        let record = saved.tokens.iter().find(|t| t.id == id).ok_or_else(|| bad("No such token."))?;
        let remote = body["remote"].as_bool().unwrap_or(remote_of(record) == Remote::Full);
        let clipboard = body["clipboard"].as_bool().unwrap_or(record.scopes.contains(&Scope::Clipboard));
        let changed = with_permissions(record, remote, clipboard).map_err(bad)?;
        let saved = saved_or_500(api.edit(&[(changed.config_key(), changed.config_value())], &[]))?;
        tracing::info!(token = %changed.name, scopes = %tokens::format_scopes(&changed.scopes), "MCP token permissions changed in the settings");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

async fn revoke_token(
    State(api): ApiState,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    guarded(api, headers, false, move |api| {
        let saved = api.read().map_err(bad)?;
        let record = saved
            .tokens
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| bad("No such token."))?;
        let saved = saved_or_500(api.edit(&[], &[record.config_key(), record.secret_key()]))?;
        tracing::info!(token = %record.name, id = %record.id, "MCP token revoked in the settings");
        Ok(json!({ "state": state_json(api, &saved) }))
    })
    .await
}

async fn token_secret(
    State(api): ApiState,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    guarded(api, headers, false, move |api| {
        let saved = api.read().map_err(bad)?;
        let token = saved.secrets.get(&id).ok_or_else(|| {
            bad("This token was made before Dalker kept tokens: only its hash is stored. Generate a new one.")
        })?;
        Ok(json!({ "token": token }))
    })
    .await
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => {
            f.to_uppercase().chain(c).collect::<String>() + if s.ends_with('.') { "" } else { "." }
        }
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    fn temp(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("dalker-api-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("dalker.conf")
    }

    fn app(path: &std::path::Path, access: Access) -> Router {
        let store = Box::new(super::super::store::FileStore::new(path));
        Api::new(
            store,
            access,
            |h: &HeaderMap| h.get("x-key").is_some_and(|v| v == "k"),
            || Live {
                web: Some(true),
                mcp: Some(false),
            },
        )
        .router()
    }

    async fn call(app: &Router, method: &str, uri: &str, body: Value) -> (StatusCode, Value) {
        let req = Request::builder()
            .method(method)
            .uri(uri)
            .header("x-key", "k")
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let bytes = r.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    #[tokio::test]
    async fn host_changes_everything() {
        let path = temp("host");
        let app = app(&path, Access::Host);
        let (s, v) = call(&app, "GET", "/api/settings", Value::Null).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["web"]["port"], 8742);
        assert_eq!(v["web"]["status"], "running");
        assert_eq!(v["mcp"]["status"], "stopped");
        let web = v["web"]["urls"].as_array().unwrap();
        assert_eq!(
            web.last().unwrap()["url"],
            "http://127.0.0.1:8742/",
            "{web:?}"
        );
        assert_eq!(
            v["mcp"]["url"], "http://127.0.0.1:8743/mcp",
            "this computer only by default"
        );

        let (s, v) = call(&app, "POST", "/api/settings/web", json!({"port": 9000})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(v["state"]["web"]["port"], 9000);
        let (s, v) = call(&app, "POST", "/api/settings/mcp", json!({"port": 9000})).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
        assert!(v["error"].as_str().unwrap().contains("both use port 9000"));
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/mcp",
            json!({"enabled": true, "port": 9001}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            v["state"]["mcp"]["loopback"], true,
            "this computer only by default"
        );
        let (s, v) = call(&app, "POST", "/api/settings/mcp", json!({"remote": true})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(
            (
                v["state"]["mcp"]["loopback"].as_bool(),
                v["state"]["mcp"]["port"].as_u64()
            ),
            (Some(false), Some(9001))
        );
        assert!(std::fs::read_to_string(&path)
            .unwrap()
            .contains("DALKER_MCP_BIND=0.0.0.0:9001"));
        let (_, v) = call(&app, "POST", "/api/settings/mcp", json!({"remote": false})).await;
        assert_eq!(v["state"]["mcp"]["loopback"], true);
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/password",
            json!({"password": "short", "repeat": "short"}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/password",
            json!({"password": "long-enough", "repeat": "long-enough"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["state"]["web"]["password_set"], true);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("DALKER_BIND=0.0.0.0:9000") && text.contains("DALKER_MCP=on"),
            "{text}"
        );

        // Stream limits.
        let (_, v) = call(&app, "GET", "/api/settings", Value::Null).await;
        assert_eq!(
            v["stream"],
            json!({"fps": 30, "bitrate_kbps": 6000, "max_width": 1920,
                   "default": {"fps": 30, "bitrate_kbps": 6000, "max_width": 1920},
                   "range": {"fps": [1, 120], "bitrate_kbps": [250, 100000], "max_width": [320, 16384]}})
        );
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/stream",
            json!({"fps": 60, "bitrate_kbps": 12000, "max_width": 0}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!(
            (
                &v["state"]["stream"]["fps"],
                &v["state"]["stream"]["bitrate_kbps"],
                &v["state"]["stream"]["max_width"]
            ),
            (&json!(60), &json!(12000), &json!(0))
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("DALKER_FPS=60")
                && text.contains("DALKER_BITRATE_KBPS=12000")
                && text.contains("DALKER_MAX_WIDTH=0"),
            "{text}"
        );
        for bad in [
            json!({"fps": 121}),
            json!({"fps": "fast"}),
            json!({"bitrate_kbps": 100}),
            json!({"max_width": 100}),
        ] {
            let (s, v) = call(&app, "POST", "/api/settings/stream", bad.clone()).await;
            assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}: {v}");
        }
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("DALKER_FPS=60"),
            "unchanged"
        );

        // Tokens: create, copy again, change, revoke.
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/tokens",
            json!({"name": "LM Studio", "remote": true}),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let token = v["token"].as_str().unwrap().to_owned();
        let row = &v["state"]["tokens"][0];
        assert_eq!(row["remote"], "full");
        assert_eq!(row["copyable"], true);
        let id = row["id"].as_str().unwrap().to_owned();
        let (_, v) = call(
            &app,
            "GET",
            &format!("/api/settings/tokens/{id}/secret"),
            Value::Null,
        )
        .await;
        assert_eq!(v["token"], token);
        let (s, v) = call(
            &app,
            "PATCH",
            &format!("/api/settings/tokens/{id}"),
            json!({"clipboard": true}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["state"]["tokens"][0]["clipboard"], true);
        let (s, v) = call(
            &app,
            "PATCH",
            &format!("/api/settings/tokens/{id}"),
            json!({"remote": false, "clipboard": false}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "no permission left: {v}");
        let (s, v) = call(
            &app,
            "POST",
            "/api/settings/tokens",
            json!({"name": "LM Studio", "remote": true}),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "duplicate: {v}");
        let (s, v) = call(
            &app,
            "DELETE",
            &format!("/api/settings/tokens/{id}"),
            Value::Null,
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["state"]["tokens"].as_array().unwrap().len(), 0);
        assert!(
            !std::fs::read_to_string(&path)
                .unwrap()
                .contains("DALKER_MCP_SECRET_"),
            "secret removed too"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn addresses_for_each_bind() {
        let addrs = [
            HostAddr {
                ip: "192.168.1.20".parse().unwrap(),
                name: "Ethernet".into(),
            },
            HostAddr {
                ip: "100.101.102.103".parse().unwrap(),
                name: "Tailscale".into(),
            },
            HostAddr {
                ip: "2001:db8::5".parse().unwrap(),
                name: "Ethernet".into(),
            },
        ];
        let urls = |bind: &str, path| {
            reach(bind.parse().unwrap(), &addrs, path)
                .iter()
                .map(|v| v["url"].as_str().unwrap().to_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            urls("0.0.0.0:8742", "/"),
            [
                "http://192.168.1.20:8742/",
                "http://100.101.102.103:8742/",
                "http://127.0.0.1:8742/"
            ]
        );
        assert_eq!(
            urls("127.0.0.1:8743", "/mcp"),
            ["http://127.0.0.1:8743/mcp"]
        );
        assert_eq!(urls("10.0.0.7:8743", "/mcp"), ["http://10.0.0.7:8743/mcp"]);
        assert_eq!(urls("[::]:8743", "/mcp").len(), 4);
        assert_eq!(
            urls("[::]:8743", "/mcp")[2],
            "http://[2001:db8::5]:8743/mcp"
        );
        assert_eq!(
            reach("0.0.0.0:8742".parse().unwrap(), &[], "/")[0]["url"],
            "http://127.0.0.1:8742/",
            "no network"
        );
        assert_eq!(
            reach("0.0.0.0:8742".parse().unwrap(), &addrs, "/")[0]["name"],
            "Ethernet"
        );
    }

    #[tokio::test]
    async fn web_clients_manage_tokens_only() {
        let path = temp("tokens");
        let app = app(&path, Access::Tokens);
        let (s, v) = call(&app, "GET", "/api/settings", Value::Null).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(v["access"], "tokens");
        assert!(
            v["file"].is_null(),
            "the host's file path is not shown to viewers"
        );
        assert!(v["screen_sharing"].is_null(), "a host-only setting");
        for (uri, body) in [
            ("/api/settings/web", json!({"enabled": false})),
            ("/api/settings/stream", json!({"fps": 60})),
            ("/api/settings/mcp", json!({"enabled": true})),
            (
                "/api/settings/password",
                json!({"password": "long-enough", "repeat": "long-enough"}),
            ),
            ("/api/settings/screen-sharing", Value::Null),
        ] {
            let (s, _) = call(&app, "POST", uri, body).await;
            assert_eq!(s, StatusCode::FORBIDDEN, "{uri}");
        }
        let (s, _) = call(
            &app,
            "POST",
            "/api/settings/tokens",
            json!({"name": "web", "remote": true}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // Without the key (or session) nothing is answered.
        let req = Request::builder()
            .uri("/api/settings")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(req).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
