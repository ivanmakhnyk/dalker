//! HTTPS without a reverse proxy: the agent's web and MCP ports take TLS and plain HTTP on the
//! same port (the first byte a client sends tells them apart), with a certificate the host
//! provides or a self-signed one made on first start.
//!
//! Plain HTTP keeps working unless [`Tls::redirect`] is on: then requests that come from
//! another computer over plain HTTP are sent to the same address over HTTPS. Requests from
//! this computer (loopback) stay on HTTP: they never cross the network, and local MCP clients
//! don't accept a self-signed certificate.
//!
//! When the redirect is wanted but the certificate can't be loaded ([`Tls::unavailable`]),
//! the port takes no TLS and refuses plain HTTP from other computers instead of serving it:
//! passwords and tokens must not cross the network in the clear because a file went missing.

use std::future::Future;
use std::io::{self, Read, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::connect_info::Connected;
use axum::extract::{ConnectInfo, Request};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::serve::{IncomingStream, Listener};
use axum::Router;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

/// First byte of a TLS handshake record; HTTP requests start with a method name.
const TLS_HANDSHAKE: u8 = 0x16;
/// How long a new connection may take to send its first byte and finish the TLS handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// File names of the self-signed certificate and its key in the agent's TLS folder.
pub const CERT_FILE: &str = "cert.pem";
pub const KEY_FILE: &str = "key.pem";

/// How a listener takes TLS: the certificate it presents and whether plain HTTP from other
/// computers is redirected to HTTPS.
#[derive(Clone)]
pub struct Tls {
    /// `None` when HTTPS is required but the certificate isn't available.
    config: Option<Arc<rustls::ServerConfig>>,
    pub redirect: bool,
}

impl Tls {
    pub fn new(identity: &Identity, redirect: bool) -> Self {
        Self {
            config: Some(Arc::clone(&identity.config)),
            redirect,
        }
    }

    /// HTTPS is required (the redirect is on) but there is no certificate to serve it with:
    /// plain HTTP from other computers is refused until there is one.
    pub fn unavailable() -> Self {
        Self {
            config: None,
            redirect: true,
        }
    }

    /// Whether this listener takes HTTPS (it has a certificate).
    pub fn has_certificate(&self) -> bool {
        self.config.is_some()
    }

    fn plain_from_network(&self) -> PlainHttp {
        match (self.redirect, self.has_certificate()) {
            (false, _) => PlainHttp::Served,
            (true, true) => PlainHttp::Redirected,
            (true, false) => PlainHttp::Refused,
        }
    }
}

/// What happens to a plain HTTP request from another computer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlainHttp {
    Served,
    Redirected,
    Refused,
}

/// A certificate (with its chain) and key, ready to serve.
#[derive(Clone)]
pub struct Identity {
    config: Arc<rustls::ServerConfig>,
    /// SHA-256 of the leaf certificate, as `AB:CD:...` (what browsers show).
    pub fingerprint: String,
    /// Where the certificate was read from.
    pub cert_path: PathBuf,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Identity")
            .field("fingerprint", &self.fingerprint)
            .field("cert_path", &self.cert_path)
            .finish_non_exhaustive()
    }
}

/// Where the certificate comes from.
#[derive(Debug, Clone)]
pub enum CertSource {
    /// The host's own certificate (PEM, may include the chain) and key (`DALKER_TLS_CERT`,
    /// `DALKER_TLS_KEY`).
    Files { cert: PathBuf, key: PathBuf },
    /// A self-signed certificate in this folder, made there when it isn't yet.
    SelfSigned { dir: PathBuf, names: Vec<String> },
}

impl Identity {
    /// Reads the certificate, making the self-signed one first if needed.
    pub fn load(source: &CertSource) -> anyhow::Result<Self> {
        match source {
            CertSource::Files { cert, key } => Self::from_files(cert, key),
            CertSource::SelfSigned { dir, names } => {
                let (cert, key) = (dir.join(CERT_FILE), dir.join(KEY_FILE));
                if !cert.exists() || !key.exists() {
                    write_self_signed(dir, names)?;
                    tracing::info!("made a self-signed TLS certificate in {}", dir.display());
                }
                Self::from_files(&cert, &key)
            }
        }
    }

    pub fn from_files(cert_path: &Path, key_path: &Path) -> anyhow::Result<Self> {
        use anyhow::Context;
        let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_file_iter(cert_path)
            .with_context(|| format!("can't read {}", cert_path.display()))?
            .collect::<Result<_, _>>()
            .with_context(|| format!("{} is not a PEM certificate", cert_path.display()))?;
        anyhow::ensure!(
            !chain.is_empty(),
            "{} has no certificate",
            cert_path.display()
        );
        let key = PrivateKeyDer::from_pem_file(key_path)
            .with_context(|| format!("can't read a PEM private key from {}", key_path.display()))?;
        let fingerprint = fingerprint(&chain[0]);
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .context("the TLS certificate and key don't go together")?;
        // The server speaks HTTP/1.1 only (axum is built without HTTP/2), so it must not offer
        // `h2`: a browser that picks it sends HTTP/2 frames the server can't read, and the
        // connection is dropped.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Self {
            config: Arc::new(config),
            fingerprint,
            cert_path: cert_path.to_owned(),
        })
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// SHA-256 of a DER certificate as upper-case hex pairs separated by colons.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der)
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

/// Makes a self-signed certificate for `names` (host names and IP addresses) and returns it
/// and its private key as PEM.
pub fn self_signed(names: &[String]) -> anyhow::Result<(String, String)> {
    let mut params = rcgen::CertificateParams::new(names.to_vec())?;
    params.distinguished_name.push(
        rcgen::DnType::CommonName,
        names.first().map_or("Dalker", |n| n.as_str()),
    );
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "Dalker");
    let year = current_year();
    params.not_before = rcgen::date_time_ymd(year - 1, 1, 1);
    params.not_after = rcgen::date_time_ymd(year + 10, 1, 1);
    let key = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key)?;
    Ok((cert.pem(), key.serialize_pem()))
}

fn current_year() -> i32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    1970 + (secs / 31_556_952) as i32
}

/// Writes a new self-signed certificate (`cert.pem`, readable by everyone) and its key
/// (`key.pem`, the owner's only) into `dir`, replacing what is there.
pub fn write_self_signed(dir: &Path, names: &[String]) -> anyhow::Result<()> {
    use anyhow::Context;
    let (cert, key) = self_signed(names)?;
    std::fs::create_dir_all(dir).with_context(|| format!("can't create {}", dir.display()))?;
    write_private(&dir.join(KEY_FILE), key.as_bytes())?;
    std::fs::write(dir.join(CERT_FILE), cert)
        .with_context(|| format!("can't write {}", dir.join(CERT_FILE).display()))?;
    Ok(())
}

fn write_private(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use anyhow::Context;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("can't write {}", path.display()))?;
    file.write_all(data)?;
    Ok(())
}

/// How a request reached the server (a request extension, and the connection info).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Transport {
    /// Came in over TLS.
    pub https: bool,
    pub peer: SocketAddr,
}

impl Connected<IncomingStream<'_, TlsListener>> for Transport {
    fn connect_info(stream: IncomingStream<'_, TlsListener>) -> Self {
        *stream.remote_addr()
    }
}

/// A connection that may be TLS.
pub enum MaybeTls {
    Plain(TcpStream),
    Tls(Box<tokio_rustls::server::TlsStream<TcpStream>>),
}

impl AsyncRead for MaybeTls {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            MaybeTls::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for MaybeTls {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<io::Result<usize>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            MaybeTls::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => std::pin::Pin::new(s).poll_flush(cx),
            MaybeTls::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }
    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<io::Result<()>> {
        match self.get_mut() {
            MaybeTls::Plain(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            MaybeTls::Tls(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

/// Accepts TCP connections and hands them on once it knows whether they speak TLS (after the
/// handshake for those that do). Handshakes run side by side, so a slow client holds up no one.
pub struct TlsListener {
    local: SocketAddr,
    ready: mpsc::Receiver<(MaybeTls, Transport)>,
    accept_task: tokio::task::JoinHandle<()>,
}

impl Drop for TlsListener {
    fn drop(&mut self) {
        // The task owns the socket: stopping it frees the port.
        self.accept_task.abort();
    }
}

impl TlsListener {
    pub fn new(listener: TcpListener, tls: Option<Tls>) -> io::Result<Self> {
        let local = listener.local_addr()?;
        let (tx, ready) = mpsc::channel(64);
        let acceptor = tls
            .and_then(|t| t.config)
            .map(tokio_rustls::TlsAcceptor::from);
        let accept_task = tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(c) => c,
                    Err(e) => {
                        // Out of file descriptors and the like: don't spin.
                        tracing::debug!("accept failed: {e}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    }
                };
                let (tx, acceptor) = (tx.clone(), acceptor.clone());
                tokio::spawn(async move {
                    let conn = tokio::time::timeout(HANDSHAKE_TIMEOUT, open(stream, acceptor));
                    match conn.await {
                        Ok(Ok((io, https))) => {
                            let _ = tx.send((io, Transport { https, peer })).await;
                        }
                        Ok(Err(e)) => tracing::debug!("connection from {peer} dropped: {e}"),
                        Err(_) => tracing::debug!("connection from {peer} timed out"),
                    }
                });
            }
        });
        Ok(Self {
            local,
            ready,
            accept_task,
        })
    }
}

async fn open(
    stream: TcpStream,
    acceptor: Option<tokio_rustls::TlsAcceptor>,
) -> io::Result<(MaybeTls, bool)> {
    let Some(acceptor) = acceptor else {
        return Ok((MaybeTls::Plain(stream), false));
    };
    let mut first = [0u8; 1];
    let n = stream.peek(&mut first).await?;
    if n == 0 || first[0] != TLS_HANDSHAKE {
        return Ok((MaybeTls::Plain(stream), false));
    }
    let tls = acceptor.accept(stream).await?;
    Ok((MaybeTls::Tls(Box::new(tls)), true))
}

impl Listener for TlsListener {
    type Io = MaybeTls;
    type Addr = Transport;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        match self.ready.recv().await {
            Some(c) => c,
            // The accept task ended (it only does when aborted): no more connections.
            None => std::future::pending().await,
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(Transport {
            https: false,
            peer: self.local,
        })
    }
}

/// Serves `router` on `listener`, with TLS when `tls` is given, until `shutdown` resolves.
/// Handlers find the [`Transport`] in the request extensions.
pub async fn serve(
    listener: TcpListener,
    tls: Option<Tls>,
    router: Router,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> io::Result<()> {
    let plain = tls
        .as_ref()
        .map_or(PlainHttp::Served, Tls::plain_from_network);
    let listener = TlsListener::new(listener, tls)?;
    let router = router.layer(axum::middleware::from_fn(move |req, next| {
        mark_transport(plain, req, next)
    }));
    axum::serve(
        listener,
        router.into_make_service_with_connect_info::<Transport>(),
    )
    .with_graceful_shutdown(shutdown)
    .await
}

async fn mark_transport(plain: PlainHttp, mut req: Request, next: Next) -> Response {
    let Some(ConnectInfo(transport)) = req.extensions().get::<ConnectInfo<Transport>>().copied()
    else {
        // Every connection the listener hands on has its transport: without one, don't guess
        // that it is safe.
        if plain == PlainHttp::Served {
            return next.run(req).await;
        }
        return https_unavailable();
    };
    if !transport.https && !transport.peer.ip().is_loopback() {
        match plain {
            PlainHttp::Served => {}
            PlainHttp::Redirected => return redirect_to_https(&req),
            PlainHttp::Refused => return https_unavailable(),
        }
    }
    req.extensions_mut().insert(transport);
    next.run(req).await
}

/// `503` for plain HTTP from the network while HTTPS is required but has no certificate.
fn https_unavailable() -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [(header::RETRY_AFTER, HeaderValue::from_static("30"))],
        "HTTPS is required but not available on the host (its TLS certificate can't be read)",
    )
        .into_response()
}

/// `308` to the same address and path over HTTPS (same port: it takes both).
fn redirect_to_https(req: &Request) -> Response {
    let Some(host) = req
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .filter(|h| !h.is_empty() && !h.contains(['/', '\\', '@', ' ']))
    else {
        return (StatusCode::BAD_REQUEST, "use https://").into_response();
    };
    let path = req.uri().path_and_query().map_or("/", |p| p.as_str());
    match HeaderValue::from_str(&format!("https://{host}{path}")) {
        Ok(location) => (
            StatusCode::PERMANENT_REDIRECT,
            [(header::LOCATION, location)],
        )
            .into_response(),
        Err(_) => (StatusCode::BAD_REQUEST, "use https://").into_response(),
    }
}

/// Whether the request came in over TLS (directly, not through a proxy).
pub fn is_https(extensions: &axum::http::Extensions) -> bool {
    extensions.get::<Transport>().is_some_and(|t| t.https)
}

/// The SHA-256 fingerprint of the certificate a server presents on `addr`, if it takes TLS
/// there. Blocking; for the host's settings window, which shows it so people can compare it
/// with what their browser shows. The certificate is not checked: it is only looked at.
pub fn fetch_fingerprint(addr: SocketAddr, timeout: Duration) -> Option<String> {
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .ok()?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AnyCert(provider())))
        .with_no_client_auth();
    let name = ServerName::IpAddress(addr.ip().into());
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name).ok()?;
    let mut sock = std::net::TcpStream::connect_timeout(&addr, timeout).ok()?;
    sock.set_read_timeout(Some(timeout)).ok()?;
    sock.set_write_timeout(Some(timeout)).ok()?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock).ok()?;
    }
    let fp = conn.peer_certificates()?.first().map(|c| fingerprint(c))?;
    conn.send_close_notify();
    let _ = conn.complete_io(&mut sock);
    let _ = sock.read(&mut [0u8; 1]);
    Some(fp)
}

/// Takes any certificate: [`fetch_fingerprint`] only reads it.
#[derive(Debug)]
struct AnyCert(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCert {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::routing::get;

    fn temp(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "dalker-tls-{tag}-{}-{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn self_signed_is_made_once_and_kept() {
        let dir = temp("keep");
        let source = CertSource::SelfSigned {
            dir: dir.clone(),
            names: vec!["myhost".into(), "localhost".into(), "127.0.0.1".into()],
        };
        let first = Identity::load(&source).unwrap();
        let again = Identity::load(&source).unwrap();
        assert_eq!(first.fingerprint, again.fingerprint);
        assert_eq!(first.fingerprint.len(), 32 * 3 - 1);
        assert!(dir.join(CERT_FILE).exists() && dir.join(KEY_FILE).exists());
        // A new one replaces it.
        write_self_signed(&dir, &["myhost".into()]).unwrap();
        assert_ne!(
            Identity::load(&source).unwrap().fingerprint,
            first.fingerprint
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mismatched_files_are_refused() {
        let (a, b) = (temp("a"), temp("b"));
        write_self_signed(&a, &["a".into()]).unwrap();
        write_self_signed(&b, &["b".into()]).unwrap();
        let err = Identity::from_files(&a.join(CERT_FILE), &b.join(KEY_FILE)).unwrap_err();
        assert!(format!("{err:#}").contains("don't go together"), "{err:#}");
        let err = Identity::from_files(&a.join("missing.pem"), &a.join(KEY_FILE)).unwrap_err();
        assert!(format!("{err:#}").contains("missing.pem"), "{err:#}");
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    async fn start(redirect: bool) -> (SocketAddr, Identity, PathBuf) {
        let dir = temp("serve");
        let identity = Identity::load(&CertSource::SelfSigned {
            dir: dir.clone(),
            names: vec!["localhost".into(), "127.0.0.1".into()],
        })
        .unwrap();
        let router = Router::new().route(
            "/",
            get(|req: Request| async move {
                if is_https(req.extensions()) {
                    "https"
                } else {
                    "http"
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = Tls::new(&identity, redirect);
        tokio::spawn(serve(listener, Some(tls), router, std::future::pending()));
        (addr, identity, dir)
    }

    fn plain_get(addr: SocketAddr) -> String {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        write!(
            s,
            "GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = String::new();
        let _ = s.read_to_string(&mut out);
        out
    }

    fn tls_get(addr: SocketAddr) -> String {
        tls_get_offering(addr, &[]).0
    }

    /// A GET over TLS offering `alpn` (as browsers do), with the protocol the server picked.
    fn tls_get_offering(addr: SocketAddr, alpn: &[&[u8]]) -> (String, Option<Vec<u8>>) {
        let mut config = rustls::ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .unwrap()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCert(provider())))
            .with_no_client_auth();
        config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        let conn = rustls::ClientConnection::new(
            Arc::new(config),
            ServerName::IpAddress(addr.ip().into()),
        )
        .unwrap();
        let sock = std::net::TcpStream::connect(addr).unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut s = rustls::StreamOwned::new(conn, sock);
        write!(
            s,
            "GET / HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut out = Vec::new();
        let _ = s.read_to_end(&mut out);
        let alpn = s.conn.alpn_protocol().map(<[u8]>::to_vec);
        (String::from_utf8_lossy(&out).into_owned(), alpn)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn browsers_offering_http2_get_http1() {
        let (addr, _identity, dir) = start(false).await;
        let (tls, alpn) =
            tokio::task::spawn_blocking(move || tls_get_offering(addr, &[b"h2", b"http/1.1"]))
                .await
                .unwrap();
        assert_eq!(alpn.as_deref(), Some(&b"http/1.1"[..]));
        assert!(
            tls.starts_with("HTTP/1.1 200") && tls.ends_with("https"),
            "{tls}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_port_takes_https_and_http() {
        let (addr, identity, dir) = start(true).await;
        let (plain, tls, fp) = tokio::task::spawn_blocking(move || {
            (
                plain_get(addr),
                tls_get(addr),
                fetch_fingerprint(addr, Duration::from_secs(5)),
            )
        })
        .await
        .unwrap();
        // Loopback stays on HTTP even with the redirect on.
        assert!(
            plain.starts_with("HTTP/1.1 200") && plain.ends_with("http"),
            "{plain}"
        );
        assert!(
            tls.starts_with("HTTP/1.1 200") && tls.ends_with("https"),
            "{tls}"
        );
        assert_eq!(fp.as_deref(), Some(identity.fingerprint.as_str()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn plain_http_from_the_network_is_redirected() {
        let peer: SocketAddr = "192.168.1.5:50000".parse().unwrap();
        let req = |host: Option<&str>| {
            let mut b = Request::builder().uri("/api/info?x=1");
            if let Some(h) = host {
                b = b.header(header::HOST, h);
            }
            let mut r = b.body(axum::body::Body::empty()).unwrap();
            r.extensions_mut()
                .insert(ConnectInfo(Transport { https: false, peer }));
            r
        };
        let app = Router::new()
            .route("/api/info", get(|| async { "ok" }))
            .layer(axum::middleware::from_fn(|req, next| {
                mark_transport(PlainHttp::Redirected, req, next)
            }));
        use tower::ServiceExt;
        let r = app.clone().oneshot(req(Some("desk:8742"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::PERMANENT_REDIRECT);
        assert_eq!(
            r.headers()[header::LOCATION],
            "https://desk:8742/api/info?x=1"
        );
        let r = app.clone().oneshot(req(Some("evil.com/x"))).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
        let r = app.oneshot(req(None)).await.unwrap();
        assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn plain_http_from_the_network_is_refused_without_a_certificate() {
        use axum::routing::post;
        use tower::ServiceExt;
        let req = |method: &str, path: &str, peer: &str| {
            let mut r = Request::builder()
                .method(method)
                .uri(path)
                .header(header::HOST, "desk:8742")
                .header(header::CONTENT_TYPE, "application/json")
                .body(axum::body::Body::from(r#"{"password":"secret"}"#))
                .unwrap();
            r.extensions_mut().insert(ConnectInfo(Transport {
                https: false,
                peer: peer.parse().unwrap(),
            }));
            r
        };
        let tls = Tls::unavailable();
        assert!(tls.redirect && !tls.has_certificate());
        let plain = tls.plain_from_network();
        assert_eq!(plain, PlainHttp::Refused);
        let app = Router::new()
            .route("/", get(|| async { "page" }))
            .route("/api/login", post(|| async { "logged in" }))
            .layer(axum::middleware::from_fn(move |req, next| {
                mark_transport(plain, req, next)
            }));
        // Other computers: neither the page nor the login is served in the clear, and they
        // aren't sent to an HTTPS that isn't there.
        for (method, path) in [("GET", "/"), ("POST", "/api/login")] {
            let r = app
                .clone()
                .oneshot(req(method, path, "192.168.1.5:50000"))
                .await
                .unwrap();
            assert_eq!(
                r.status(),
                StatusCode::SERVICE_UNAVAILABLE,
                "{method} {path}"
            );
            assert!(r.headers().get(header::LOCATION).is_none());
        }
        // This computer still gets plain HTTP, as with the redirect.
        let r = app
            .clone()
            .oneshot(req("POST", "/api/login", "127.0.0.1:50000"))
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        // A request with no transport recorded is not trusted either.
        let mut r = req("GET", "/", "192.168.1.5:50000");
        r.extensions_mut().remove::<ConnectInfo<Transport>>();
        let r = app.oneshot(r).await.unwrap();
        assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_port_without_a_certificate_serves_loopback_and_takes_no_tls() {
        let router = Router::new().route("/", get(|| async { "http" }));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(serve(
            listener,
            Some(Tls::unavailable()),
            router,
            std::future::pending(),
        ));
        let (plain, fp) = tokio::task::spawn_blocking(move || {
            (
                plain_get(addr),
                fetch_fingerprint(addr, Duration::from_secs(2)),
            )
        })
        .await
        .unwrap();
        assert!(
            plain.starts_with("HTTP/1.1 200") && plain.ends_with("http"),
            "{plain}"
        );
        assert_eq!(fp, None);
    }

    #[test]
    fn fingerprint_format() {
        let fp = fingerprint(b"");
        assert!(fp.starts_with("E3:B0:C4:42"), "{fp}");
    }
}
