//! # dalker-web
//!
//! The agent's HTTP face:
//!
//! | Route              | Auth | Purpose                                   |
//! |--------------------|------|-------------------------------------------|
//! | `GET /`, assets    | no   | embedded web client (`web-ui/`)           |
//! | `POST /api/login`  | no   | `{password}` → session cookie             |
//! | `POST /api/logout` | yes  | drop the session                          |
//! | `GET /api/info`    | yes  | host/agent info for the UI                |
//! | `POST /api/offer`  | yes  | `{sdp}` offer → `{sdp}` answer (WebRTC)   |
//!
//! The login password can't be changed over HTTP: only on the host (its settings UI or
//! `dalkerd set-password`).
//!
//! WebRTC itself lives behind the [`SignalingHandler`] trait so this crate has
//! no media dependencies.

pub mod auth;

use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;

pub use auth::{hash_password, Authenticator, CookieSecure, Credentials};

pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send + 'static>>;

/// Implemented by the agent to plug WebRTC into the HTTP API.
pub trait SignalingHandler: Send + Sync + 'static {
    /// Accepts a complete SDP offer and returns a complete SDP answer.
    fn handle_offer(&self, offer_sdp: String) -> BoxFuture<Result<String, String>>;
    /// Arbitrary JSON shown by the UI (hostname, OS, capture backend...).
    fn info(&self) -> serde_json::Value;
}

#[derive(RustEmbed)]
#[folder = "../../web-ui/"]
struct Assets;

#[derive(Clone)]
struct AppState {
    auth: Arc<Authenticator>,
    signaling: Arc<dyn SignalingHandler>,
    /// Serialises login attempts to blunt online brute force.
    login_gate: Arc<Semaphore>,
}

#[derive(Deserialize)]
struct LoginRequest {
    /// The host's access password; there is no user name. Clients from before that still send
    /// one, which is ignored.
    password: String,
}

#[derive(Deserialize, Serialize)]
pub struct SdpMessage {
    pub sdp: String,
}

#[derive(Serialize)]
struct ErrorBody {
    error: String,
}

fn error(status: StatusCode, msg: impl Into<String>) -> Response {
    (status, Json(ErrorBody { error: msg.into() })).into_response()
}

/// Builds the router (exposed for tests and for embedding).
pub fn router(auth: Arc<Authenticator>, signaling: Arc<dyn SignalingHandler>) -> Router {
    router_with(auth, signaling, Router::new())
}

/// The router plus `extra` routes (the agent's settings API), which check sign-in themselves
/// with [`signed_in`].
pub fn router_with(
    auth: Arc<Authenticator>,
    signaling: Arc<dyn SignalingHandler>,
    extra: Router,
) -> Router {
    let state = AppState {
        auth,
        signaling,
        login_gate: Arc::new(Semaphore::new(1)),
    };
    let main = Router::new()
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/info", get(info))
        .route("/api/offer", post(offer))
        .route("/", get(index))
        .route("/{*path}", get(asset))
        .with_state(state);
    main.merge(extra).layer(DefaultBodyLimit::max(256 * 1024))
}

/// Whether the request carries a valid web client session (signed in with the password).
pub fn signed_in(auth: &Authenticator, headers: &HeaderMap) -> bool {
    session_token(headers).is_some_and(|t| auth.is_valid(&t))
}

/// A file of the embedded web client (`web-ui/`), e.g. `settings.html`.
pub fn static_asset(path: &str) -> Response {
    serve_asset(if path.is_empty() { "index.html" } else { path })
}

/// Serves the router until `shutdown` resolves.
pub async fn serve(
    addr: SocketAddr,
    auth: Arc<Authenticator>,
    signaling: Arc<dyn SignalingHandler>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, auth, signaling, shutdown).await
}

/// Serves the router on a listener that is already bound until `shutdown` resolves.
pub async fn serve_listener(
    listener: tokio::net::TcpListener,
    auth: Arc<Authenticator>,
    signaling: Arc<dyn SignalingHandler>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    serve_listener_with(listener, auth, signaling, Router::new(), shutdown).await
}

/// [`serve_listener`] with `extra` routes (see [`router_with`]).
pub async fn serve_listener_with(
    listener: tokio::net::TcpListener,
    auth: Arc<Authenticator>,
    signaling: Arc<dyn SignalingHandler>,
    extra: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> anyhow::Result<()> {
    tracing::info!("web UI on http://{}", listener.local_addr()?);
    axum::serve(listener, router_with(auth, signaling, extra))
        .with_graceful_shutdown(shutdown)
        .await?;
    Ok(())
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(auth::token_from_cookie_header)
        .map(str::to_owned)
}

fn require_auth(state: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    match session_token(headers) {
        Some(t) if state.auth.is_valid(&t) => Ok(t),
        _ => Err(error(StatusCode::UNAUTHORIZED, "not logged in")),
    }
}

async fn login(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<LoginRequest>,
) -> Response {
    if !state.auth.has_password() {
        return error(
            StatusCode::SERVICE_UNAVAILABLE,
            "No access password is set yet. Choose one on the host, in Dalker Settings.",
        );
    }
    let _permit = state.login_gate.acquire().await;
    let auth = Arc::clone(&state.auth);
    // Argon2 is CPU-heavy; keep it off the async workers.
    let result = tokio::task::spawn_blocking(move || auth.login(&req.password))
        .await
        .ok()
        .flatten();
    match result {
        Some(token) => {
            let cookie = format!(
                "{}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}{}",
                auth::COOKIE_NAME,
                state.auth.ttl().as_secs(),
                secure_attr(&state.auth, &headers)
            );
            let mut resp = Json(serde_json::json!({"ok": true})).into_response();
            resp.headers_mut()
                .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
            resp
        }
        None => {
            tracing::warn!("failed login attempt");
            tokio::time::sleep(Duration::from_millis(750)).await;
            error(StatusCode::UNAUTHORIZED, "wrong password")
        }
    }
}

/// `; Secure` when the cookie should carry it (see [`CookieSecure`]).
fn secure_attr(auth: &Authenticator, headers: &HeaderMap) -> &'static str {
    let https = match auth.cookie_secure() {
        CookieSecure::Always => true,
        CookieSecure::Never => false,
        CookieSecure::Auto => headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .is_some_and(|v| v.trim().eq_ignore_ascii_case("https")),
    };
    if https {
        "; Secure"
    } else {
        ""
    }
}

async fn logout(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(t) = session_token(&headers) {
        state.auth.logout(&t);
    }
    let cookie = format!(
        "{}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0{}",
        auth::COOKIE_NAME,
        secure_attr(&state.auth, &headers)
    );
    let mut resp = Json(serde_json::json!({"ok": true})).into_response();
    resp.headers_mut()
        .insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    resp
}

async fn info(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    Json(state.signaling.info()).into_response()
}

async fn offer(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<SdpMessage>,
) -> Response {
    if let Err(r) = require_auth(&state, &headers) {
        return r;
    }
    match state.signaling.handle_offer(req.sdp).await {
        Ok(sdp) => Json(SdpMessage { sdp }).into_response(),
        Err(e) => {
            tracing::warn!("offer rejected: {e}");
            error(StatusCode::BAD_REQUEST, e)
        }
    }
}

async fn index() -> Response {
    serve_asset("index.html")
}

async fn asset(Path(path): Path<String>) -> Response {
    serve_asset(&path)
}

fn serve_asset(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => {
            let mime = mime_guess::from_path(path).first_or_octet_stream();
            (
                [
                    (header::CONTENT_TYPE, mime.as_ref().to_owned()),
                    (header::CACHE_CONTROL, "no-cache".to_owned()),
                    (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_owned()),
                ],
                file.data.into_owned(),
            )
                .into_response()
        }
        None => error(StatusCode::NOT_FOUND, "not found"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    struct EchoSignaling;
    impl SignalingHandler for EchoSignaling {
        fn handle_offer(&self, offer_sdp: String) -> BoxFuture<Result<String, String>> {
            Box::pin(async move { Ok(format!("answer-to:{offer_sdp}")) })
        }
        fn info(&self) -> serde_json::Value {
            serde_json::json!({"host": "test"})
        }
    }

    fn app() -> Router {
        let auth = Authenticator::new(
            Credentials {
                password_hash: hash_password("pw").unwrap(),
            },
            Duration::from_secs(60),
        )
        .unwrap();
        router(Arc::new(auth), Arc::new(EchoSignaling))
    }

    fn json_req(method: &str, uri: &str, body: &str, cookie: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/json");
        if let Some(c) = cookie {
            b = b.header(header::COOKIE, c);
        }
        b.body(Body::from(body.to_owned())).unwrap()
    }

    #[tokio::test]
    async fn offer_requires_login_then_works() {
        let app = app();

        let r = app
            .clone()
            .oneshot(json_req("POST", "/api/offer", r#"{"sdp":"x"}"#, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let r = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/api/login",
                r#"{"password":"nope"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);

        let r = app
            .clone()
            .oneshot(json_req("POST", "/api/login", r#"{"password":"pw"}"#, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let set_cookie = r.headers()[header::SET_COOKIE].to_str().unwrap().to_owned();
        assert!(set_cookie.contains("HttpOnly") && set_cookie.contains("SameSite=Strict"));
        let cookie = set_cookie.split(';').next().unwrap().to_owned();

        let r = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/api/offer",
                r#"{"sdp":"v=0"}"#,
                Some(&cookie),
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let body = r.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(body.as_ref(), br#"{"sdp":"answer-to:v=0"}"#);

        let r = app
            .clone()
            .oneshot(json_req("POST", "/api/logout", "", Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app
            .oneshot(json_req("GET", "/api/info", "", Some(&cookie)))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn cookie_is_secure_behind_an_https_proxy() {
        async fn login(app: Router, proto: Option<&str>) -> String {
            let mut req = json_req("POST", "/api/login", r#"{"password":"pw"}"#, None);
            if let Some(p) = proto {
                req.headers_mut()
                    .insert("x-forwarded-proto", HeaderValue::from_str(p).unwrap());
            }
            let r = app.oneshot(req).await.unwrap();
            r.headers()[header::SET_COOKIE].to_str().unwrap().to_owned()
        }
        assert!(!login(app(), None).await.contains("Secure"));
        assert!(!login(app(), Some("http")).await.contains("Secure"));
        assert!(login(app(), Some("https")).await.contains("; Secure"));
        let hash = hash_password("pw").unwrap();
        let with = |mode| {
            let a = Authenticator::new(
                Credentials {
                    password_hash: hash.clone(),
                },
                Duration::from_secs(60),
            )
            .unwrap()
            .with_cookie_secure(mode);
            router(Arc::new(a), Arc::new(EchoSignaling))
        };
        assert!(login(with(CookieSecure::Always), None)
            .await
            .contains("; Secure"));
        assert!(!login(with(CookieSecure::Never), Some("https"))
            .await
            .contains("Secure"));
    }

    #[tokio::test]
    async fn old_clients_that_send_a_user_name_still_sign_in() {
        let r = app()
            .oneshot(json_req(
                "POST",
                "/api/login",
                r#"{"username":"anything","password":"pw"}"#,
                None,
            ))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn password_cannot_be_changed_over_http() {
        let app = app();
        let login = app
            .clone()
            .oneshot(json_req("POST", "/api/login", r#"{"password":"pw"}"#, None))
            .await
            .unwrap();
        let cookie = login.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let r = app
            .clone()
            .oneshot(json_req(
                "POST",
                "/api/password",
                r#"{"current":"pw","new":"whatever-123"}"#,
                Some(&cookie),
            ))
            .await
            .unwrap();
        assert!(r.status().is_client_error(), "got {}", r.status());
        // The old password still works.
        let r = app
            .oneshot(json_req("POST", "/api/login", r#"{"password":"pw"}"#, None))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn serves_embedded_ui() {
        let r = app()
            .oneshot(Request::get("/").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r.headers()[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        let r = app()
            .oneshot(Request::get("/app.js").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        let r = app()
            .oneshot(Request::get("/../Cargo.toml").body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::NOT_FOUND);
    }
}
