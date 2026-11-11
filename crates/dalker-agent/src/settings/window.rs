//! The settings window: `web-ui/settings.html` in a native web view (WebView2 on Windows,
//! WebKitGTK on Linux). The page and its API are served by this process on a loopback port,
//! behind a key made at each launch, so the window works whether or not the agent (or its web
//! server) is running.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::http::HeaderMap;
use axum::routing::get;
use rand::TryRng;

use super::api::{Access, Api, Live};
use super::store::Store;

const TITLE: &str = "Dalker Settings";
/// Header that carries the per-launch key (the page reads it from its URL's fragment).
const KEY_HEADER: &str = "x-dalker-key";

/// Shows the window until it is closed. `store` is where the config file is read and written,
/// or why it can't be (shown on the page).
pub fn show(store: Result<Box<dyn Store>, String>, no_window: bool) {
    let mut raw = [0u8; 24];
    rand::rngs::SysRng
        .try_fill_bytes(&mut raw)
        .expect("the OS random number generator failed");
    let key: String = raw.iter().map(|b| format!("{b:02x}")).collect();

    let live = Arc::new(Mutex::new(Live::default()));
    let targets = Arc::new(Mutex::new(None::<(SocketAddr, SocketAddr)>));
    spawn_probe(Arc::clone(&live), Arc::clone(&targets));

    let check = {
        let key = key.clone();
        move |h: &HeaderMap| {
            h.get(KEY_HEADER)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| constant_eq(v, &key))
        }
    };
    let api = match store {
        Ok(store) => {
            let live = Arc::clone(&live);
            Api::new(store, Access::Host, check, move || *live.lock().unwrap())
                .on_read(move |saved| *targets.lock().unwrap() = Some((saved.bind, saved.mcp_bind)))
        }
        Err(why) => Api::unavailable(format!("The settings can't be opened: {why}"), check),
    };
    let app = api
        .router()
        .route(
            "/",
            get(|| async { dalker_web::static_asset("settings.html") }),
        )
        .route(
            "/{*path}",
            get(
                |axum::extract::Path(p): axum::extract::Path<String>| async move {
                    dalker_web::static_asset(&p)
                },
            ),
        );

    let (addr_tx, addr_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("settings-server".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => return drop(addr_tx.send(Err(e.to_string()))),
            };
            rt.block_on(async move {
                let listener = match tokio::net::TcpListener::bind(("127.0.0.1", 0)).await {
                    Ok(l) => l,
                    Err(e) => return drop(addr_tx.send(Err(e.to_string()))),
                };
                let _ = addr_tx.send(listener.local_addr().map_err(|e| e.to_string()));
                if let Err(e) = axum::serve(listener, app).await {
                    tracing::error!("settings server failed: {e}");
                }
            });
        })
        .expect("spawn settings server");
    let addr = match addr_rx.recv() {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return report(&format!("The settings could not start: {e}")),
        Err(_) => return report("The settings could not start."),
    };
    let url = format!("http://{addr}/#key={key}");
    if no_window {
        println!("{url}");
        loop {
            std::thread::park();
        }
    }
    if let Err(e) = open_window(&url) {
        report(&format!("The settings window could not be opened: {e}"));
    }
}

// ---------------------------------------------------------------- one window at a time
//
// The open window listens on a loopback port and writes `<port> <key>` to a file of the desktop
// user's; a second launch sends `focus <key> [token]` there and exits. The window process is
// the one that raises its window, so this works although the window runs elevated (Windows) or
// the config is edited as root (Linux). Raising another program's window is up to the desktop:
// on Windows the second process first allows it, on Linux it passes on the activation token its
// launcher gave it (Wayland's xdg-activation, X11's startup notification).

/// Where the open window says how to reach it. Per user: the window runs as the desktop user
/// (elevated on Windows, which keeps the same user and `%LOCALAPPDATA%`).
fn instance_file() -> Option<PathBuf> {
    #[cfg(windows)]
    let dir = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Dalker"));
    #[cfg(not(windows))]
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|d| d.is_dir());
    dir.map(|d| d.join("settings-window"))
}

/// The line the window writes to [`instance_file`].
fn instance_line(port: u16, key: &str) -> String {
    format!("{port} {key}\n")
}

fn parse_instance(text: &str) -> Option<(u16, &str)> {
    let mut parts = text.split_whitespace();
    let port = parts.next()?.parse().ok()?;
    let key = parts.next().filter(|k| !k.is_empty())?;
    Some((port, key))
}

/// If a settings window is open, asks it to come to the front and returns true.
pub fn focus_open_window() -> bool {
    let Some(file) = instance_file() else {
        return false;
    };
    let Ok(text) = std::fs::read_to_string(&file) else {
        return false;
    };
    let Some((port, key)) = parse_instance(&text) else {
        return false;
    };
    #[cfg(windows)]
    dalker_platform::windows::settings::allow_foreground();
    let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
        .iter()
        .find_map(|v| std::env::var(v).ok())
        // GNOME's tokens hold the app's name, spaces included: the token is the rest of the line.
        .map(|t| t.trim().to_owned())
        .filter(|t| !t.is_empty() && !t.contains(['\r', '\n']));
    send_focus(port, key, token.as_deref())
}

fn send_focus(port: u16, key: &str, token: Option<&str>) -> bool {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let Ok(mut s) = TcpStream::connect_timeout(&addr, Duration::from_millis(500)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
    let sent = match token {
        Some(t) => writeln!(s, "focus {key} {t}"),
        None => writeln!(s, "focus {key}"),
    };
    if sent.is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = BufReader::new(s).read_line(&mut reply);
    reply.trim() == "ok"
}

/// Answers [`send_focus`] on `listener`, calling `focus` (with the activation token, if any)
/// for each request with the right key.
fn serve_focus(
    listener: TcpListener,
    key: String,
    focus: impl Fn(Option<String>) + Send + 'static,
) {
    std::thread::Builder::new()
        .name("settings-focus".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                if BufReader::new(&conn).read_line(&mut line).is_err() {
                    continue;
                }
                let mut parts = line.trim().splitn(3, ' ');
                let ok = parts.next() == Some("focus")
                    && parts.next().is_some_and(|k| constant_eq(k, &key));
                if ok {
                    focus(
                        parts
                            .next()
                            .map(str::trim)
                            .filter(|t| !t.is_empty())
                            .map(str::to_owned),
                    );
                }
                let _ = writeln!(conn, "{}", if ok { "ok" } else { "no" });
            }
        })
        .ok();
}

/// Starts answering second launches; the returned guard removes the instance file.
fn announce(focus: impl Fn(Option<String>) + Send + 'static) -> Option<InstanceFile> {
    let file = instance_file()?;
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).ok()?;
    let port = listener.local_addr().ok()?.port();
    let mut raw = [0u8; 16];
    rand::rngs::SysRng
        .try_fill_bytes(&mut raw)
        .expect("the OS random number generator failed");
    let key: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    serve_focus(listener, key.clone(), focus);
    let line = instance_line(port, &key);
    if let Some(dir) = file.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    std::fs::write(&file, &line).ok()?;
    Some(InstanceFile { file, line })
}

struct InstanceFile {
    file: PathBuf,
    line: String,
}

impl Drop for InstanceFile {
    fn drop(&mut self) {
        // Only ours: a window opened meanwhile (after a crash of this one) keeps its file.
        if std::fs::read_to_string(&self.file).is_ok_and(|t| t == self.line) {
            let _ = std::fs::remove_file(&self.file);
        }
    }
}

fn report(msg: &str) {
    tracing::error!("{msg}");
    #[cfg(windows)]
    dalker_platform::windows::settings::error_box(msg);
    #[cfg(not(windows))]
    eprintln!("dalkerd settings: {msg}");
}

fn constant_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(any(windows, target_os = "linux", target_os = "macos"))]
fn open_window(url: &str) -> Result<(), String> {
    use tao::dpi::LogicalSize;
    use tao::event::{Event, WindowEvent};
    use tao::event_loop::{ControlFlow, EventLoopBuilder};
    use tao::window::{Icon, Theme, WindowBuilder};
    use wry::{WebContext, WebViewBuilder};

    /// Sent by a second launch (see [`focus_open_window`]), with its activation token.
    struct Focus(Option<String>);
    let event_loop = EventLoopBuilder::<Focus>::with_user_event().build();
    // Big enough for the taskbar and Alt+Tab; the title bar scales it down.
    const ICON: usize = 64;
    let icon = Icon::from_rgba(
        dalker_platform::icon::rgba_sized(dalker_platform::icon::Look::Normal, ICON),
        ICON as u32,
        ICON as u32,
    )
    .ok();
    // As tall as the smallest screen allows (the window may open on any of them; small VM and
    // laptop screens), leaving room for the title bar and panels; at most 820 px.
    let height = event_loop
        .available_monitors()
        .map(|m| m.size().to_logical::<f64>(m.scale_factor()).height - 110.0)
        .reduce(f64::min)
        .unwrap_or(680.0)
        .clamp(560.0, 820.0);
    let window = WindowBuilder::new()
        .with_title(TITLE)
        .with_inner_size(LogicalSize::new(1000.0, height))
        .with_min_inner_size(LogicalSize::new(760.0, 560.0))
        .with_window_icon(icon)
        // The page is dark; so is its title bar.
        .with_theme(Some(Theme::Dark))
        .build(&event_loop)
        .map_err(|e| e.to_string())?;
    if let Some(m) = window.current_monitor() {
        let (screen, size) = (m.size(), window.outer_size());
        let x = m.position().x + (screen.width.saturating_sub(size.width) / 2) as i32;
        let y = m.position().y + (screen.height.saturating_sub(size.height) / 3) as i32;
        window.set_outer_position(tao::dpi::PhysicalPosition::new(x, y));
    }
    // The browser engine keeps its profile here, not next to the program (Program Files is
    // read-only).
    let data = std::env::temp_dir().join("Dalker-settings-webview");
    let mut context = WebContext::new(Some(data));
    let builder = WebViewBuilder::new_with_web_context(&mut context)
        .with_url(url)
        .with_background_color((6, 9, 14, 255))
        .with_devtools(cfg!(debug_assertions));
    #[cfg(any(windows, target_os = "macos"))]
    let webview = builder.build(&window).map_err(|e| e.to_string())?;
    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        let vbox = window.default_vbox().ok_or("no GTK container")?;
        builder.build_gtk(vbox).map_err(|e| e.to_string())?
    };
    let proxy = event_loop.create_proxy();
    let mut instance = announce(move |token| {
        let _ = proxy.send_event(Focus(token));
    });
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        let _keep = (&webview, &window);
        match event {
            Event::UserEvent(Focus(token)) => raise(&window, token),
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                // The event loop never returns: remove the instance file now.
                instance.take();
                *control_flow = ControlFlow::Exit;
            }
            _ => {}
        }
    });
}

/// Brings the window to the front, restored if it was minimized.
#[cfg(any(windows, target_os = "macos"))]
fn raise(window: &tao::window::Window, _token: Option<String>) {
    window.set_minimized(false);
    window.set_visible(true);
    window.set_focus();
}

/// Brings the window to the front, restored if it was minimized. Straight through GTK: tao skips
/// focusing a window it still thinks minimized, and only the launcher's token lets the desktop
/// raise a window the user didn't click.
#[cfg(target_os = "linux")]
fn raise(window: &tao::window::Window, token: Option<String>) {
    use gtk::prelude::*;
    use tao::platform::unix::WindowExtUnix;
    let gtk_window = window.gtk_window();
    if let Some(token) = token {
        gtk_window.set_startup_id(&token);
    }
    gtk_window.deiconify();
    gtk_window.present();
}

#[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
fn open_window(_url: &str) -> Result<(), String> {
    Err("no settings window on this OS".into())
}

/// Checks every 1.5 s whether Dalker answers on the saved web and MCP addresses.
fn spawn_probe(live: Arc<Mutex<Live>>, targets: Arc<Mutex<Option<(SocketAddr, SocketAddr)>>>) {
    std::thread::Builder::new()
        .name("settings-probe".into())
        .spawn(move || loop {
            let now = *targets.lock().unwrap();
            if let Some((web, mcp)) = now {
                let web_up = probe_http(web, "/", "<title>Dalker</title>");
                let mcp_up = probe_http(mcp, "/mcp", "realm=\"dalker\"");
                if *targets.lock().unwrap() == now {
                    *live.lock().unwrap() = Live {
                        web: Some(web_up),
                        mcp: Some(mcp_up),
                    };
                } else {
                    *live.lock().unwrap() = Live::default();
                }
            }
            std::thread::sleep(Duration::from_millis(1500));
        })
        .ok();
}

/// Whether a Dalker server answers `GET path` on `addr` with something containing `marker`
/// (the web client's title; the MCP server's 401 with its bearer realm).
fn probe_http(addr: SocketAddr, path: &str, marker: &str) -> bool {
    let ip = match addr.ip() {
        IpAddr::V4(v4) if v4.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
        IpAddr::V6(v6) if v6.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
        ip => ip,
    };
    let target = SocketAddr::new(ip, addr.port());
    let Ok(mut s) = TcpStream::connect_timeout(&target, Duration::from_millis(600)) else {
        return false;
    };
    let _ = s.set_read_timeout(Some(Duration::from_secs(1)));
    let _ = s.set_write_timeout(Some(Duration::from_secs(1)));
    let request = format!("GET {path} HTTP/1.1\r\nHost: {target}\r\nConnection: close\r\n\r\n");
    if s.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut got = Vec::new();
    let mut buf = [0u8; 4096];
    while got.len() < 64 * 1024 {
        match s.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                got.extend_from_slice(&buf[..n]);
                if String::from_utf8_lossy(&got).contains(marker) {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_needs_the_marker() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (i, conn) in listener.incoming().take(2).enumerate() {
                let mut c = conn.unwrap();
                // Read the whole request: closing with unread data resets the connection.
                let mut req = Vec::new();
                let mut buf = [0u8; 256];
                while !req.ends_with(b"\r\n\r\n") {
                    match c.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let body = if i == 0 {
                    "<title>Dalker</title>"
                } else {
                    "<title>Jenkins</title>"
                };
                let _ = write!(
                    c,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        assert!(probe_http(addr, "/", "<title>Dalker</title>"));
        assert!(
            !probe_http(addr, "/", "<title>Dalker</title>"),
            "something else on the port"
        );
        let free = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(!probe_http(free, "/", "x"));
        assert!(
            constant_eq("abc", "abc") && !constant_eq("abc", "abd") && !constant_eq("ab", "abc")
        );
    }

    #[test]
    fn instance_line_round_trips() {
        let line = instance_line(51234, "00ff");
        assert_eq!(parse_instance(&line), Some((51234, "00ff")));
        assert_eq!(parse_instance(""), None);
        assert_eq!(parse_instance("x 00ff"), None);
        assert_eq!(parse_instance("51234"), None);
    }

    #[test]
    fn second_launch_focuses_the_open_window() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let focused = Arc::new(AtomicUsize::new(0));
        let tokens = Arc::new(Mutex::new(Vec::new()));
        let (count, seen) = (Arc::clone(&focused), Arc::clone(&tokens));
        serve_focus(listener, "secret".into(), move |token| {
            count.fetch_add(1, Ordering::SeqCst);
            seen.lock().unwrap().push(token);
        });
        assert!(!send_focus(port, "wrong", None), "a wrong key is refused");
        assert!(send_focus(port, "secret", None));
        assert!(send_focus(
            port,
            "secret",
            Some("gnome-shell/Dalker Settings/1745-3_TIME42")
        ));
        assert_eq!(focused.load(Ordering::SeqCst), 2);
        assert_eq!(
            *tokens.lock().unwrap(),
            [
                None,
                Some("gnome-shell/Dalker Settings/1745-3_TIME42".to_owned())
            ]
        );
        let closed = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        assert!(
            !send_focus(closed, "secret", None),
            "no window open: a new one opens"
        );
    }
}
