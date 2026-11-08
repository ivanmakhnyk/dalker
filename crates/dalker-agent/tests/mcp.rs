//! `dalkerd --capture mock` with the MCP server on: tokens made with `mcp-token`, a real MCP
//! client clicks and then sees the pointer the mock draws where it clicked.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use rmcp::model::{CallToolRequestParams, CallToolResult, ContentBlock};
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{RoleClient, ServiceExt};

const EXE: &str = env!("CARGO_BIN_EXE_dalkerd");

fn temp_config(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("dalker-mcp-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("dalker.conf")
}

fn dalkerd(config: &Path, args: &[&str]) -> std::process::Output {
    let out = Command::new(EXE).args(args).arg("--config").arg(config).env("DALKER_CONFIG", config).output().unwrap();
    assert!(out.status.success(), "dalkerd {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out
}

struct Agent(Child);

impl Drop for Agent {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Starts the agent and returns it with the MCP URL it logged.
fn start_agent(config: &Path) -> (Agent, String) {
    let mut child = Command::new(EXE)
        .args(["--capture", "mock", "--bind", "127.0.0.1:0", "--no-tray"])
        .env("DALKER_CONFIG", config)
        .env("DALKER_PASSWORD", "test-password")
        .env("DALKER_MCP", "on")
        .env("DALKER_MCP_BIND", "127.0.0.1:0")
        .env("RUST_LOG", "info")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(i) = line.find("MCP server on http://") {
                let url = line[i + "MCP server on ".len()..].split_whitespace().next().unwrap().to_owned();
                let _ = tx.send(url);
            }
        }
    });
    let agent = Agent(child);
    let url = rx.recv_timeout(Duration::from_secs(30)).expect("agent logged its MCP address");
    (agent, url)
}

async fn connect(url: &str, token: &str) -> RunningService<RoleClient, ()> {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_owned()).auth_header(token),
    );
    ().serve(transport).await.expect("MCP handshake")
}

async fn call(client: &RunningService<RoleClient, ()>, tool: &'static str, args: serde_json::Value) -> CallToolResult {
    let serde_json::Value::Object(map) = args else { panic!("args must be an object") };
    let r = client.call_tool(CallToolRequestParams::new(tool).with_arguments(map)).await.unwrap();
    assert_ne!(r.is_error, Some(true), "{tool} failed: {r:?}");
    r
}

fn image(r: &CallToolResult) -> image::RgbImage {
    let data = r
        .content
        .iter()
        .find_map(|c| match c {
            ContentBlock::Image(i) => Some(i.data.clone()),
            _ => None,
        })
        .expect("an image");
    let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
    image::load_from_memory(&bytes).unwrap().to_rgb8()
}

fn text(r: &CallToolResult) -> String {
    r.content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test(flavor = "multi_thread")]
async fn mock_agent_click_then_screenshot() {
    let config = temp_config("e2e");
    let out = dalkerd(&config, &["mcp-token", "create", "--name", "e2e", "--scopes", "view,input,clipboard"]);
    let token = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    assert!(token.starts_with("dkmcp_"), "{token}");
    let stored = std::fs::read_to_string(&config).unwrap();
    // The record holds the hash; the token itself is only on its own line, for the settings window.
    let with_token: Vec<&str> = stored.lines().filter(|l| l.contains(&token)).collect();
    assert!(stored.contains("DALKER_MCP_TOKEN_"), "{stored}");
    assert!(
        with_token.len() == 1 && with_token[0].starts_with("DALKER_MCP_SECRET_"),
        "the token is kept on its secret line only: {with_token:?}"
    );
    let list = String::from_utf8(dalkerd(&config, &["mcp-token", "list"]).stdout).unwrap();
    assert!(list.contains("e2e") && list.contains("view,input,clipboard") && list.contains("active"), "{list}");

    let (_agent, url) = start_agent(&config);
    let client = connect(&url, &token).await;

    let info: serde_json::Value =
        serde_json::from_str(&text(&call(&client, "get_host_info", serde_json::json!({})).await)).unwrap();
    assert_eq!(info["capture"], "mock");
    assert_eq!(info["screen_available"], true);

    // The mock screen is 1280x720, so screenshot pixels are screen pixels.
    let shot = call(&client, "screenshot", serde_json::json!({})).await;
    assert_eq!(image(&shot).dimensions(), (1280, 720));

    call(&client, "click", serde_json::json!({"x": 200, "y": 100})).await;
    // The mock draws the pointer as a magenta cross where the click landed.
    let r = call(
        &client,
        "screenshot",
        serde_json::json!({"region": {"x": 190, "y": 90, "w": 20, "h": 20}, "max_width": 1280}),
    )
    .await;
    let zoom = image(&r);
    assert_eq!(zoom.dimensions(), (20, 20));
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut zoom = zoom;
    while zoom.get_pixel(10, 10).0 != [255, 0, 255] && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(250)).await;
        zoom = image(
            &call(&client, "screenshot", serde_json::json!({"region": {"x": 190, "y": 90, "w": 20, "h": 20}})).await,
        );
    }
    assert_eq!(zoom.get_pixel(10, 10).0, [255, 0, 255], "pointer drawn at the click");

    // Clipboard through the agent's clipboard thread (the on-demand read).
    call(&client, "clipboard_set", serde_json::json!({"text": "from the agent"})).await;
    let r = call(&client, "clipboard_get", serde_json::json!({})).await;
    let v: serde_json::Value = serde_json::from_str(&text(&r)).unwrap();
    assert_eq!(v["text"], "from the agent");

    // Revoked on the host: the agent notices the config change and refuses the token.
    let out = dalkerd(&config, &["mcp-token", "revoke", "e2e"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("revoked"));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let params = CallToolRequestParams::new("get_host_info").with_arguments(Default::default());
        if client.call_tool(params).await.is_err() {
            break;
        }
        assert!(Instant::now() < deadline, "revoked token still accepted");
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let _ = client.cancel().await;
    let _ = std::fs::remove_dir_all(config.parent().unwrap());
}

#[test]
fn refuses_a_plaintext_lan_bind_and_bad_token_requests() {
    let config = temp_config("cli");
    let out = Command::new(EXE)
        .args(["mcp-token", "create", "--name", "x", "--scopes", "view,files", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!out.status.success(), "unknown scope rejected");
    let out = Command::new(EXE)
        .args(["mcp-token", "create", "--name", "bad;name", "--config"])
        .arg(&config)
        .output()
        .unwrap();
    assert!(!out.status.success(), "bad name rejected");
    dalkerd(&config, &["mcp-token", "create", "--name", "dup", "--expires", "1d"]);
    let out =
        Command::new(EXE).args(["mcp-token", "create", "--name", "dup", "--config"]).arg(&config).output().unwrap();
    assert!(!out.status.success(), "duplicate names rejected");

    // A LAN address with DALKER_MCP_ALLOW_PLAINTEXT=0: the MCP server stays off.
    let mut child = Command::new(EXE)
        .args(["--capture", "mock", "--bind", "127.0.0.1:0", "--no-tray"])
        .env("DALKER_CONFIG", &config)
        .env("DALKER_PASSWORD", "test-password")
        .env("DALKER_MCP", "on")
        .env("DALKER_MCP_BIND", "0.0.0.0:0")
        .env("DALKER_MCP_ALLOW_PLAINTEXT", "0")
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(30);
    let (mut refused, mut web) = (false, false);
    while let Ok(line) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        assert!(!line.contains("MCP server on"), "must not serve MCP: {line}");
        refused |= line.contains("MCP server not started");
        web |= line.contains("web UI on");
        if web && refused {
            break;
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    assert!(refused, "logged why the MCP server is off");
    let _ = std::fs::remove_dir_all(config.parent().unwrap());
}
