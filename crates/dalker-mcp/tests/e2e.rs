//! End to end: a real MCP client over Streamable HTTP against a mock host (the agent's
//! `--capture mock` pairing: injected input is drawn on the test pattern).

use std::collections::{BTreeSet, HashMap};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use dalker_core::{
    Clipboard, InputEvent, InputInjector, InputTracker, MockCapturer, MockClipboard, MockInjector, MouseButton,
    RawFrame, ScreenCapturer,
};
use dalker_mcp::tokens::{self, parse_scopes};
use dalker_mcp::{BoxFuture, DesktopHandle, FrameWatch, HostCommand, McpConfig, McpServer, TokenStore};
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::watch;

const W: u32 = 640;
const H: u32 = 360;

/// What the agent does, in miniature: capture while subscribed, apply input with a tracker
/// per session, count viewers.
struct MockDesktop {
    frames: watch::Sender<Option<Arc<RawFrame>>>,
    injector: Mutex<MockInjector>,
    trackers: Mutex<HashMap<u64, InputTracker>>,
    viewers: AtomicUsize,
    next_id: AtomicU64,
    clipboard: Mutex<MockClipboard>,
    commands: Mutex<Vec<(u64, HostCommand)>>,
}

impl MockDesktop {
    fn start() -> (Arc<Self>, MockInjector) {
        let injector = MockInjector::new();
        let (frames, _) = watch::channel(None);
        let me = Arc::new(Self {
            frames,
            injector: Mutex::new(injector.clone()),
            trackers: Mutex::default(),
            viewers: AtomicUsize::new(0),
            next_id: AtomicU64::new(1),
            clipboard: Mutex::new(MockClipboard::new()),
            commands: Mutex::default(),
        });
        let weak = Arc::downgrade(&me);
        let paired = injector.clone();
        std::thread::spawn(move || {
            let mut cap = MockCapturer::new(W, H, 30).with_injector(&paired);
            while let Some(me) = weak.upgrade() {
                if me.frames.receiver_count() == 0 {
                    drop(me);
                    std::thread::sleep(Duration::from_millis(20));
                    continue;
                }
                if let Ok(Some(f)) = cap.next_frame(Duration::from_millis(50)) {
                    me.frames.send_replace(Some(Arc::new(f)));
                }
            }
        });
        (me, injector)
    }

    fn apply(&self, ev: &InputEvent) {
        let _ = self.injector.lock().unwrap().inject(ev);
    }
}

impl DesktopHandle for MockDesktop {
    fn info(&self) -> serde_json::Value {
        serde_json::json!({"hostname": "mock-host", "os": "test", "capture": "mock", "locked": false})
    }
    fn frames(&self) -> FrameWatch {
        self.frames.subscribe()
    }
    fn open_viewer(&self) -> Result<u64, String> {
        self.viewers.fetch_add(1, Ordering::SeqCst);
        Ok(self.next_id.fetch_add(1, Ordering::SeqCst))
    }
    fn close_viewer(&self, _id: u64) {
        self.viewers.fetch_sub(1, Ordering::SeqCst);
    }
    fn send(&self, id: u64, cmd: HostCommand) {
        self.commands.lock().unwrap().push((id, cmd.clone()));
        let mut trackers = self.trackers.lock().unwrap();
        let tracker = trackers.entry(id).or_default();
        match cmd {
            HostCommand::Input(InputEvent::ReleaseAll) | HostCommand::EndSession => {
                for ev in tracker.drain_release_events() {
                    self.apply(&ev);
                }
                if cmd == HostCommand::EndSession {
                    trackers.remove(&id);
                }
            }
            HostCommand::Input(ev) => {
                tracker.observe(&ev);
                self.apply(&ev);
            }
            HostCommand::SetClipboard(text) => {
                let _ = self.clipboard.lock().unwrap().set_text(&text);
            }
        }
    }
    fn read_clipboard(&self) -> BoxFuture<Result<Option<String>, String>> {
        let text = self.clipboard.lock().unwrap().get_text().map_err(|e| e.to_string());
        Box::pin(async move { text })
    }
}

struct Host {
    url: String,
    server: McpServer,
    desktop: Arc<MockDesktop>,
    injector: MockInjector,
}

async fn start(tokens: Vec<tokens::TokenRecord>) -> Host {
    let (desktop, injector) = MockDesktop::start();
    let server = McpServer::new(desktop.clone(), Arc::new(TokenStore::new(tokens)), McpConfig::default());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    let s = server.clone();
    tokio::spawn(async move { s.serve(listener, std::future::pending()).await.unwrap() });
    Host { url, server, desktop, injector }
}

fn token(name: &str, scopes: &str) -> (String, tokens::TokenRecord) {
    tokens::generate(name, parse_scopes(scopes).unwrap(), None).unwrap()
}

async fn connect(url: &str, token: &str) -> RunningService<RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_owned()).auth_header(token),
    );
    ().serve(transport).await.expect("MCP handshake")
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &'static str, args: serde_json::Value) -> CallToolResult {
    let mut params = CallToolRequestParams::new(tool);
    if let serde_json::Value::Object(map) = args {
        params = params.with_arguments(map);
    }
    client.call_tool(params).await.unwrap_or_else(|e| panic!("{tool}: {e}"))
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn image(r: &CallToolResult) -> (String, image::RgbImage) {
    let img = r
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image(i) => Some(i),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no image in {r:?}"));
    let bytes = base64::engine::general_purpose::STANDARD.decode(&img.data).unwrap();
    (img.mime_type.clone(), image::load_from_memory(&bytes).unwrap().to_rgb8())
}

fn ok(r: &CallToolResult) -> bool {
    r.is_error != Some(true)
}

#[tokio::test(flavor = "multi_thread")]
async fn click_then_screenshot_shows_the_pointer() {
    let (secret, rec) = token("e2e", "view,input,clipboard");
    let host = start(vec![rec]).await;
    let client = connect(&host.url, &secret).await;

    let tools: BTreeSet<String> =
        client.list_all_tools().await.unwrap().into_iter().map(|t| t.name.to_string()).collect();
    let expected = [
        "get_host_info",
        "screenshot",
        "wait",
        "mouse_move",
        "click",
        "drag",
        "scroll",
        "type_text",
        "press_keys",
        "release_all",
        "clipboard_get",
        "clipboard_set",
    ];
    assert_eq!(tools, expected.iter().map(|s| s.to_string()).collect(), "the 12 v1 tools");
    assert_eq!(host.desktop.viewers.load(Ordering::SeqCst), 0, "no viewer until a tool is used");

    let info = call(&client, "get_host_info", serde_json::json!({})).await;
    assert!(ok(&info), "{info:?}");
    let info: serde_json::Value = serde_json::from_str(&text(&info)).unwrap();
    assert_eq!(info["hostname"], "mock-host");
    assert_eq!(info["scopes"], serde_json::json!(["view", "input", "clipboard"]));
    assert_eq!(host.desktop.viewers.load(Ordering::SeqCst), 1, "an agent counts as a viewer");

    // A full screenshot: the screen is smaller than 1280, so it is native size (JPEG).
    let shot = call(&client, "screenshot", serde_json::json!({})).await;
    let (mime, img) = image(&shot);
    assert_eq!(mime, "image/jpeg");
    assert_eq!(img.dimensions(), (W, H));
    // Downscaled: coordinates now refer to a 320x180 image.
    let shot = call(&client, "screenshot", serde_json::json!({"max_width": 320})).await;
    assert_eq!(image(&shot).1.dimensions(), (320, 180), "{}", text(&shot));

    // Click at (100, 50) of the 320x180 screenshot = native (200, 100), take a screenshot after.
    let r = call(&client, "click", serde_json::json!({"x": 100, "y": 50, "screenshot_after": true, "settle_ms": 150}))
        .await;
    assert!(ok(&r), "{r:?}");
    assert_eq!(image(&r).1.dimensions(), (320, 180), "screenshot_after uses the same size");
    let events = host.injector.events();
    let (nx, ny) = (100.0 / 319.0, 50.0 / 179.0);
    let near = |x: f32, y: f32| (x - nx).abs() < 1e-4 && (y - ny).abs() < 1e-4;
    assert!(
        events.iter().any(
            |e| matches!(e, InputEvent::MouseButton { button: MouseButton::Left, down: true, x, y } if near(*x, *y))
        ),
        "{events:?}"
    );
    assert!(events.iter().any(|e| matches!(e, InputEvent::MouseButton { down: false, .. })));

    // The mock draws the pointer as a magenta cross: zoom in on it at native resolution.
    let r = call(&client, "screenshot", serde_json::json!({"region": {"x": 95, "y": 45, "w": 10, "h": 10}})).await;
    let (mime, img) = image(&r);
    assert_eq!(mime, "image/png", "regions default to lossless PNG");
    assert_eq!(img.dimensions(), (20, 20), "10x10 of a half-size screenshot = 20x20 native");
    let (nxp, nyp) = dalker_core::NormPoint::new(nx, ny).to_pixels(W, H);
    let (cx, cy) = (nxp as u32 - 190, nyp as u32 - 90);
    assert_eq!(img.get_pixel(cx, cy).0, [255, 0, 255], "pointer drawn where we clicked; {}", text(&r));

    // Keyboard: chords become key events, text becomes Text + Enter.
    let r = call(&client, "press_keys", serde_json::json!({"keys": "ctrl+shift+t"})).await;
    assert!(ok(&r), "{r:?}");
    let r = call(&client, "type_text", serde_json::json!({"text": "héllo\nworld"})).await;
    assert!(ok(&r), "{r:?}");
    let events = host.injector.events();
    let keys: Vec<(String, bool)> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::Key { code, down } => Some((code.clone(), *down)),
            _ => None,
        })
        .collect();
    let k = |c: &str, d: bool| (c.to_string(), d);
    assert_eq!(
        keys,
        vec![
            k("ControlLeft", true),
            k("ShiftLeft", true),
            k("KeyT", true),
            k("KeyT", false),
            k("ShiftLeft", false),
            k("ControlLeft", false),
            k("Enter", true),
            k("Enter", false),
        ]
    );
    let texts: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            InputEvent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, ["héllo", "world"]);

    // Scroll: 2 notches down = two wheel events of +120.
    let r = call(&client, "scroll", serde_json::json!({"direction": "down", "amount": 2, "x": 10, "y": 10})).await;
    assert!(ok(&r), "{r:?}");
    let wheels =
        host.injector.events().into_iter().filter(|e| matches!(e, InputEvent::Wheel { dx: 0.0, dy: 120.0 })).count();
    assert_eq!(wheels, 2);

    // Clipboard both ways.
    let r = call(&client, "clipboard_set", serde_json::json!({"text": "secret stuff"})).await;
    assert!(ok(&r), "{r:?}");
    let r = call(&client, "clipboard_get", serde_json::json!({})).await;
    let v: serde_json::Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(v, serde_json::json!({"has_text": true, "text": "secret stuff"}));

    // Bad input is reported to the model, not a protocol error.
    let r = call(&client, "click", serde_json::json!({"x": 5000, "y": 10})).await;
    assert!(!ok(&r) && text(&r).contains("outside the screenshot"), "{r:?}");
    let r = call(&client, "press_keys", serde_json::json!({"keys": "ctrl+banana"})).await;
    assert!(!ok(&r) && text(&r).contains("unknown key"), "{r:?}");

    // A held button is released when the host ends the session.
    let session_id = host.desktop.commands.lock().unwrap()[0].0;
    host.desktop.send(session_id, HostCommand::Input(InputEvent::Key { code: "AltLeft".into(), down: true }));
    assert_eq!(host.server.disconnect_all("test"), 1);
    assert_eq!(host.desktop.viewers.load(Ordering::SeqCst), 0, "slot given back");
    assert!(
        matches!(host.injector.events().last(), Some(InputEvent::Key { code, down: false }) if code == "AltLeft"),
        "held key released on disconnect"
    );
    assert_eq!(host.server.session_count(), 0);

    // The next call starts a fresh session.
    let r = call(&client, "get_host_info", serde_json::json!({})).await;
    assert!(ok(&r));
    assert_eq!(host.server.session_count(), 1);
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scopes_are_enforced() {
    let (secret, rec) = token("viewer", "view");
    let host = start(vec![rec]).await;
    let client = connect(&host.url, &secret).await;
    let r = call(&client, "screenshot", serde_json::json!({})).await;
    assert!(ok(&r), "{r:?}");
    for (tool, args) in [
        ("click", serde_json::json!({"x": 1, "y": 1})),
        ("type_text", serde_json::json!({"text": "x"})),
        ("release_all", serde_json::json!({})),
        ("clipboard_get", serde_json::json!({})),
    ] {
        let r = call(&client, tool, args).await;
        assert!(!ok(&r) && text(&r).contains("scope"), "{tool}: {r:?}");
    }
    assert!(host.injector.events().is_empty(), "nothing reached the host");
    let _ = client.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_a_token_ends_its_session() {
    let (secret, rec) = token("temp", "view,input");
    let (other_secret, other) = token("other", "view");
    let host = start(vec![rec.clone(), other.clone()]).await;
    let client = connect(&host.url, &secret).await;
    assert!(ok(&call(&client, "mouse_move", serde_json::json!({"x": 1, "y": 1})).await));
    assert_eq!(host.server.session_count(), 1);

    host.server.set_tokens(vec![other]);
    assert_eq!(host.server.session_count(), 0, "revoked at once");
    assert_eq!(host.desktop.viewers.load(Ordering::SeqCst), 0);
    // The old token no longer gets in (the client sees an HTTP 401).
    let mut params = CallToolRequestParams::new("get_host_info");
    params = params.with_arguments(Default::default());
    assert!(client.call_tool(params).await.is_err());

    // Only one AI agent at a time by default.
    let (third_secret, third) = token("third", "view");
    host.server.set_tokens(vec![host.server.tokens().records()[0].clone(), third]);
    let a = connect(&host.url, &other_secret).await;
    assert!(ok(&call(&a, "get_host_info", serde_json::json!({})).await));
    let b = connect(&host.url, &third_secret).await;
    let r = call(&b, "get_host_info", serde_json::json!({})).await;
    assert!(!ok(&r) && text(&r).contains("too many"), "{r:?}");

    // Blocked from the host: sessions end and calls are refused until allowed again.
    host.server.set_blocked(true);
    assert_eq!(host.server.session_count(), 0);
    let r = call(&a, "get_host_info", serde_json::json!({})).await;
    assert!(!ok(&r) && text(&r).contains("disconnected AI agents"), "{r:?}");
    host.server.set_blocked(false);
    assert!(ok(&call(&a, "get_host_info", serde_json::json!({})).await));
    let _ = a.cancel().await;
    let _ = b.cancel().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_requests_need_a_token_and_no_origin() {
    let (secret, rec) = token("raw", "view");
    let host = start(vec![rec]).await;
    let body = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"1"}}}"#;
    let post = |auth: Option<String>, origin: Option<&'static str>| {
        let url = host.url.clone();
        async move {
            let client = reqwest::Client::new();
            let mut req = client
                .post(url)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .body(body);
            if let Some(a) = auth {
                req = req.header("authorization", a);
            }
            if let Some(o) = origin {
                req = req.header("origin", o);
            }
            req.send().await.unwrap().status().as_u16()
        }
    };
    assert_eq!(post(None, None).await, 401);
    assert_eq!(post(Some("Bearer dkmcp_deadbeef_".to_owned() + &"0".repeat(64)), None).await, 401);
    assert_eq!(post(Some(format!("Basic {secret}")), None).await, 401);
    assert_eq!(post(Some(format!("Bearer {secret}")), Some("http://evil.example")).await, 403, "browsers are refused");
    assert_eq!(post(Some(format!("Bearer {secret}")), Some("http://127.0.0.1:8081")).await, 403);
    assert_eq!(post(Some(format!("Bearer {secret}")), None).await, 200);
}
