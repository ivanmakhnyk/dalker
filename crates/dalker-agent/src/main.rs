//! `dalkerd` — the Dalker host agent.
//!
//! Threads:
//! * **capture** – owns the [`ScreenCapturer`], publishes the latest frame on a
//!   `watch` channel (only while someone is watching).
//! * **input** – owns the [`InputInjector`], applies events from every session
//!   and releases held keys when a session ends.
//! * **clipboard** – owns the host's [`Clipboard`], publishes its text while someone is
//!   watching and applies text a viewer copied.
//! * **tokio** – HTTP (login, signaling) and WebRTC; one encoder thread per viewer.
//! * **files** – one per viewer with file transfers open, acting as the signed-in user.

// A release build on Windows is a background app: no console window. Command line use still
// prints, because `attach_parent_console` reconnects to the terminal that started it, and
// the agent logs to a file when there is nowhere else to log.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod cli;
mod config_watch;
mod listeners;
mod settings;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, FromArgMatches};
use dalker_core::{
    CaptureError, Clipboard, ClipboardError, DisplayInfo, InputEvent, InputInjector, InputTracker,
    MockCapturer, MockClipboard, MockInjector, RawFrame, ScreenCapturer, MAX_CLIPBOARD_BYTES,
};
use dalker_mcp::tokens::{self, TokenRecord};
use dalker_mcp::{HostCommand, McpServer};
use dalker_web::{Authenticator, BoxFuture, Credentials, SignalingHandler};
use dalker_webrtc::{
    ClipboardState, ControlMessage, EndReason, SessionId, WebRtcConfig, WebRtcEngine,
};
use tokio::sync::{mpsc, oneshot, watch};

use cli::{CaptureMode, Cli, Command, McpTokenCommand, OnOff};

fn main() -> Result<()> {
    dalker_platform::attach_parent_console();
    // Config file first (before any threads): env vars override it, flags override both.
    let config_path = cli::config_path();
    // A config file this user can't read (root-only on Linux) only stops the agent itself:
    // `mcp-token list --config <file>`, `--version` and the like still work.
    let (loaded, config_error) = match cli::load_config_into_env(&config_path) {
        Ok(loaded) => (loaded, None),
        Err(e) => (None, Some(e)),
    };
    let matches = Cli::command().get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    if let Some(e) = config_error {
        if matches!(cli.command, None | Some(Command::Service)) {
            return Err(e);
        }
    }
    let service = matches!(cli.command, Some(Command::Service));
    if service {
        // The agents the service starts read the config file themselves, so they see a password
        // changed on the host instead of the value the service read at boot.
        for key in loaded.iter().flatten() {
            std::env::remove_var(key);
        }
    }
    if service && std::env::var_os(LOG_DIR_ENV).is_none() {
        // SYSTEM's own profile is a poor place for logs; the agent inherits this too.
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| r"C:\ProgramData".into());
        std::env::set_var(
            LOG_DIR_ENV,
            std::path::Path::new(&base).join("Dalker").join("logs"),
        );
    }
    init_logging(if service { "service.log" } else { "dalker.log" });
    install_panic_hook();

    if service {
        return run_service();
    }

    if let Some(Command::HashPassword { password }) = &cli.command {
        let password = match password {
            Some(p) => p.clone(),
            None => cli::read_password_from_stdin("hash-password")?,
        };
        println!("{}", dalker_web::hash_password(&password)?);
        return Ok(());
    }

    if let Some(Command::SetPassword { password, config }) = &cli.command {
        let password = match password {
            Some(p) => p.clone(),
            None => cli::read_password_from_stdin("set-password")?,
        };
        // The same rule as the settings window, whichever way the password came in.
        settings::check_new_password(&password, &password, settings::MIN_PASSWORD_LEN)
            .map_err(anyhow::Error::msg)?;
        let path = config.clone().unwrap_or_else(|| config_path.clone());
        cli::write_config_value(
            &path,
            "DALKER_PASSWORD_HASH",
            &dalker_web::hash_password(&password)?,
        )?;
        println!("password updated in {}", path.display());
        return Ok(());
    }

    if let Some(Command::Settings {
        config,
        elevated,
        no_window,
    }) = &cli.command
    {
        let path = config.clone().unwrap_or_else(|| config_path.clone());
        return settings::run(&path, *elevated, *no_window);
    }

    if let Some(Command::SettingsHelper {
        config,
        listen,
        owner,
    }) = &cli.command
    {
        let path = config.clone().unwrap_or_else(|| config_path.clone());
        #[cfg(unix)]
        if let (Some(socket), Some(owner)) = (listen, owner) {
            return settings::store::run_socket_helper(&path, socket, owner);
        }
        #[cfg(not(unix))]
        anyhow::ensure!(
            listen.is_none() && owner.is_none(),
            "--listen is for Linux only"
        );
        return settings::store::run_helper(&path);
    }

    if let Some(Command::McpToken { action }) = &cli.command {
        return run_mcp_token(action, &config_path);
    }

    if let Some(Command::SetConfig {
        key,
        value,
        config,
        if_missing,
    }) = &cli.command
    {
        anyhow::ensure!(key.starts_with("DALKER_"), "only DALKER_* keys can be set");
        let path = config.clone().unwrap_or_else(|| config_path.clone());
        if *if_missing {
            if !cli::write_config_value_if_missing(&path, key, value)? {
                println!("{key} already set in {}; left as it is", path.display());
                return Ok(());
            }
        } else {
            cli::write_config_value(&path, key, value)?;
        }
        println!("{key} set in {}", path.display());
        return Ok(());
    }

    if loaded.is_some() {
        tracing::info!(config = %config_path.display(), "loaded config");
    }
    // The listener settings the config file may change while the agent runs: not those given
    // on the command line or as environment variables of their own.
    let from_file = loaded.unwrap_or_default();
    let pinned = [
        ("web", listeners::KEY_WEB),
        ("bind", listeners::KEY_BIND),
        ("mcp", listeners::KEY_MCP),
        ("mcp_bind", listeners::KEY_MCP_BIND),
    ]
    .into_iter()
    .filter(|(id, key)| match matches.value_source(id) {
        Some(clap::parser::ValueSource::CommandLine) => true,
        Some(clap::parser::ValueSource::EnvVariable) => !from_file.iter().any(|k| k == key),
        _ => false,
    })
    .map(|(_, key)| key)
    .collect();
    let startup = listeners::Startup {
        web: cli.web == OnOff::On,
        bind: cli.bind,
        mcp: cli.mcp == OnOff::On,
        mcp_bind: cli.mcp_bind,
        pinned,
    };
    dalker_platform::init_process();
    // On macOS the main thread stays with AppKit (red border, menu bar icon).
    dalker_platform::run_app(move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(run(cli, startup))
    })
}

/// Overrides where log files go (set by the service for itself and the agent it starts).
const LOG_DIR_ENV: &str = "DALKER_LOG_DIR";

/// Folder for the agent's own files (log): `DALKER_LOG_DIR`, else per user
/// (`%LOCALAPPDATA%\\Dalker` on Windows, `~/Library/Logs/Dalker` on macOS).
fn local_data_dir() -> Option<std::path::PathBuf> {
    if let Some(dir) = std::env::var_os(LOG_DIR_ENV) {
        return Some(dir.into());
    }
    if cfg!(target_os = "macos") {
        return std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Library/Logs/Dalker"));
    }
    std::env::var_os("LOCALAPPDATA").map(|d| std::path::PathBuf::from(d).join("Dalker"))
}

/// `dalkerd [options] service`: hand over to the service control manager and keep
/// `dalkerd [options] --no-tray` running in the console session.
#[cfg(windows)]
fn run_service() -> Result<()> {
    use dalker_platform::windows::service;
    let exe = std::env::current_exe().context("own path")?;
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Options come before the subcommand, so `service` is the last argument.
    if args.last().map(String::as_str) == Some("service") {
        args.pop();
    }
    // The tray menu of a SYSTEM process would open the browser as SYSTEM.
    if !args.iter().any(|a| a == "--no-tray") {
        args.push("--no-tray".into());
    }
    let command = std::iter::once(exe.display().to_string())
        .chain(args)
        .map(|a| service::quote_arg(&a))
        .collect::<Vec<_>>()
        .join(" ");
    service::run(command).map_err(anyhow::Error::msg)
}

#[cfg(not(windows))]
fn run_service() -> Result<()> {
    bail!("`service` is only available on Windows")
}

/// Threads the agent can't work without: if one of them panics, the process ends so the service
/// (systemd, the Windows service, launchd) starts a fresh one, instead of serving a black screen
/// or ignoring input.
const ESSENTIAL_THREADS: [&str; 3] = ["dalker-capture", "dalker-input", "dalker-clipboard"];

/// Logs every panic where the agent's log goes (stderr may go nowhere on Windows and macOS), and
/// aborts when an essential thread panicked.
fn install_panic_hook() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_owned();
        let message = panic_message(info.payload());
        let location = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!(thread = %name, %location, "panic: {message}");
        default(info);
        if ESSENTIAL_THREADS.contains(&name.as_str()) {
            tracing::error!(thread = %name, "an essential thread panicked; stopping so Dalker is restarted");
            std::process::abort();
        }
    }));
}

/// The text a panic was raised with (`panic!("...")` gives a `&str` or a `String`).
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "(no message)".to_owned())
}

fn init_logging(file_name: &str) {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        "info,rmcp=warn,webrtc=warn,webrtc_ice=warn,webrtc_mdns=warn,webrtc_dtls=warn,webrtc_sctp=warn,webrtc_srtp=warn".into()
    });
    if dalker_platform::has_std_error() {
        tracing_subscriber::fmt().with_env_filter(filter).init();
        return;
    }
    // No console (GUI build started by the service): log to a file instead.
    let file = local_data_dir().and_then(|dir| {
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join(file_name);
        if std::fs::metadata(&path)
            .map(|m| m.len() > 5 * 1024 * 1024)
            .unwrap_or(false)
        {
            let _ = std::fs::rename(&path, dir.join(format!("{file_name}.old")));
        }
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .ok()
    });
    if let Some(file) = file {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .init();
    }
}

/// The viewer's address on this host right now, if the web server runs.
#[cfg(any(windows, target_os = "macos"))]
fn web_url(status: &listeners::Status) -> Option<String> {
    let addr = (*status.web.lock().unwrap())?;
    let host = if addr.ip().is_unspecified() {
        "127.0.0.1".to_owned()
    } else {
        addr.ip().to_string()
    };
    let host = if addr.is_ipv6() && !addr.ip().is_unspecified() {
        format!("[{host}]")
    } else {
        host
    };
    Some(format!("http://{host}:{}/", addr.port()))
}

/// Opens the host's settings window, a process of its own (on Windows it asks for
/// administrator rights itself).
#[cfg(any(windows, target_os = "macos"))]
fn open_settings_window() {
    match std::env::current_exe() {
        Ok(exe) => {
            if let Err(e) = std::process::Command::new(exe).arg("settings").spawn() {
                tracing::warn!("could not open settings: {e}");
            }
        }
        Err(e) => tracing::warn!("could not open settings: {e}"),
    }
}

/// The menu bar icon (macOS): open the viewer, Dalker Settings, quit.
#[cfg(target_os = "macos")]
fn start_menu_bar(
    status: &Arc<listeners::Status>,
    exit: &Arc<tokio::sync::Notify>,
) -> Option<dalker_platform::macos::MenuBar> {
    let web = Arc::clone(status);
    let exit = Arc::clone(exit);
    let cfg = dalker_platform::macos::MenuBarConfig {
        open_url: Box::new(move || web_url(&web)),
        on_settings: Box::new(open_settings_window),
        on_exit: Box::new(move || exit.notify_one()),
    };
    match dalker_platform::macos::MenuBar::start(cfg) {
        Ok(m) => {
            tracing::info!("menu bar icon ready");
            Some(m)
        }
        Err(e) => {
            tracing::warn!("no menu bar icon: {e}");
            None
        }
    }
}

#[cfg(windows)]
fn start_tray(
    status: &Arc<listeners::Status>,
    viewers: &Arc<AtomicUsize>,
    mcp: &McpServer,
    exit: &Arc<tokio::sync::Notify>,
) -> Option<dalker_platform::windows::Tray> {
    let exit = Arc::clone(exit);
    // "Disconnect AI agents": ends their sessions and keeps them out until allowed again.
    let ai_agents = {
        let (a, b, c) = (mcp.clone(), mcp.clone(), mcp.clone());
        let running = Arc::clone(status);
        dalker_platform::windows::AiAgentsMenu {
            running: Box::new(move || running.mcp.lock().unwrap().is_some()),
            connected: Box::new(move || a.session_count()),
            blocked: Box::new(move || b.is_blocked()),
            set_blocked: Box::new(move |blocked| c.set_blocked(blocked)),
        }
    };
    let web = Arc::clone(status);
    let cfg = dalker_platform::windows::TrayConfig {
        // Wherever the web server is now (the settings window can move it or turn it off).
        open_url: Box::new(move || web_url(&web)),
        on_settings: Box::new(open_settings_window),
        log_dir: local_data_dir().map(|d| d.display().to_string()),
        viewers: Arc::clone(viewers),
        ai_agents: Some(ai_agents),
        on_exit: Box::new(move || exit.notify_one()),
    };
    match dalker_platform::windows::Tray::start(cfg) {
        Ok(t) => {
            tracing::info!("tray icon ready");
            Some(t)
        }
        Err(e) => {
            tracing::warn!("no tray icon: {e}");
            None
        }
    }
}

async fn run(cli: Cli, startup: listeners::Startup) -> Result<()> {
    let password_hash = match (&cli.password_hash, &cli.password) {
        (Some(h), _) => h.clone(),
        (None, Some(p)) => {
            tracing::warn!("plain --password/DALKER_PASSWORD given; prefer DALKER_PASSWORD_HASH (see `dalkerd hash-password`)");
            dalker_web::hash_password(p)?
        }
        // Installers ask for no password: nobody can sign in until it is set on the host
        // (settings window, `dalkerd set-password`), which is picked up below.
        (None, None) => {
            tracing::warn!("no access password set yet: choose one in Dalker Settings (or `dalkerd set-password`)");
            String::new()
        }
    };
    // The password can only be changed on the host (settings window, `dalkerd set-password`),
    // which rewrites the config file; pick that up without a restart.
    let auth = Arc::new(
        Authenticator::new(
            Credentials { password_hash },
            Duration::from_secs(cli.session_hours * 3600),
        )?
        .with_cookie_secure(cli.cookie_secure),
    );
    // Shown to viewers; the probe below names the real backend (GNOME screen cast or X11).
    let mut capture_name = match cli.capture {
        CaptureMode::Mock => "mock",
        CaptureMode::Native if cfg!(windows) => "dxgi",
        // Under Wayland only a screen cast can serve (it may not be up yet at start); the
        // probe below names the one in use.
        CaptureMode::Native if std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") => {
            "gnome-screencast"
        }
        CaptureMode::Native => "x11",
    }
    .to_string();
    // Backends. Mock capture pairs with a mock injector so input is visible on screen.
    let (capture_factory, injector): (CaptureFactory, Option<Box<dyn InputInjector>>) = match cli
        .capture
    {
        CaptureMode::Mock => {
            let inj = MockInjector::new();
            let (w, h, fps) = (cli.mock_width, cli.mock_height, cli.fps);
            let paired = inj.clone();
            (
                Box::new(move || {
                    Ok(Box::new(MockCapturer::new(w, h, fps).with_injector(&paired)) as _)
                }),
                Some(Box::new(inj)),
            )
        }
        CaptureMode::Native => {
            // The service must be ready immediately, even before a display exists
            // (login screen not up yet, Wayland session...). Capture is retried in the
            // background, so a failed probe only logs.
            let probe = match dalker_platform::capture_backend_without_probe() {
                Some(name) => {
                    tracing::info!(
                        backend = name,
                        "capture starts when a viewer connects (the host may be asked to allow screen sharing)"
                    );
                    capture_name = name.to_string();
                    None
                }
                None => Some(dalker_platform::create_capturer()),
            };
            match probe {
                None => {}
                Some(Ok(probe)) => {
                    tracing::info!(backend = probe.name(), display = ?probe.display(), "capture backend ready");
                    capture_name = probe.name().to_string();
                }
                Some(Err(e)) => {
                    tracing::warn!("no screen to capture yet ({e}); serving anyway and retrying")
                }
            }
            let injector = match dalker_platform::create_injector() {
                Ok(i) => {
                    tracing::info!(backend = i.name(), "input backend ready");
                    Some(i)
                }
                Err(e) => {
                    tracing::error!("input injection disabled (view-only): {e}");
                    None
                }
            };
            (Box::new(dalker_platform::create_capturer), injector)
        }
    };
    let (frame_tx, _) = watch::channel::<Option<Arc<RawFrame>>>(None);
    let (display_tx, display_rx) = watch::channel::<Option<DisplayInfo>>(None);
    let (control_tx, control_rx) = mpsc::unbounded_channel::<(SessionId, ControlMessage)>();

    // Mock sources show no real screen, so they need no border; native capture is
    // gated on the border being up (see below).
    let border_ready = Arc::new(AtomicBool::new(cli.capture == CaptureMode::Mock));
    spawn_capture_thread(
        capture_factory,
        frame_tx.clone(),
        display_tx,
        Arc::clone(&border_ready),
    )?;
    let active = Arc::new(AtomicUsize::new(0));
    let clipboard_factory: ClipboardFactory = match cli.capture {
        CaptureMode::Mock => {
            let clip = MockClipboard::new();
            Box::new(move || Ok(Box::new(clip.clone()) as _))
        }
        CaptureMode::Native => Box::new(dalker_platform::create_clipboard),
    };
    let (clipboard_tx, clipboard_rx) = watch::channel(ClipboardState::default());
    let clipboard_requests =
        spawn_clipboard_thread(clipboard_factory, clipboard_tx, Arc::clone(&active))?;
    let input_available = injector.is_some();
    spawn_input_thread(injector, control_rx, display_rx, clipboard_requests.clone())?;

    let engine = Arc::new(
        WebRtcEngine::new(WebRtcConfig {
            ice_servers: cli.stun.clone(),
            udp_port: cli.udp_port,
            public_ips: cli.public_ip.clone(),
            encoder: dalker_webrtc::EncoderSettings {
                bitrate_bps: cli.bitrate_kbps * 1000,
                max_fps: cli.fps as f32,
                max_width: cli.max_width,
                keyframe_interval: cli.fps * 10,
                downscale: 1,
                strict_bitrate: false,
            },
            ..Default::default()
        })
        .await?
        // File transfers run as the signed-in user, never as SYSTEM or root (see FileAccess).
        .with_files(Arc::new(dalker_platform::open_file_access)),
    );

    let signaling = Arc::new(Agent {
        engine,
        frames: frame_tx,
        clipboard: clipboard_rx,
        control: control_tx,
        active,
        sign_out: watch::channel(None).0,
        streams: Arc::new(AtomicUsize::new(0)),
        max_sessions: cli.max_sessions,
        info: serde_json::json!({
            "hostname": hostname(),
            "os": std::env::consts::OS,
            "capture": capture_name,
            "version": env!("CARGO_PKG_VERSION"),
            "ice_servers": cli.stun,
            // Viewers can send files to the host and fetch files from it.
            "files": true,
            // The most a viewer can ask for in its stream settings.
            "stream": {
                "max_fps": cli.fps,
                "max_bitrate_kbps": cli.bitrate_kbps,
                "max_downscale": dalker_webrtc::settings::MAX_DOWNSCALE,
            },
        }),
        border_ready: Arc::clone(&border_ready),
        input_available,
        clipboard_requests,
        next_mcp_id: AtomicU64::new(1),
    });
    let mcp = mcp_server(&cli, &signaling);
    let status = Arc::new(listeners::Status::default());

    // No indicator, no remote access: the red border is what tells people at the
    // machine that it is being viewed. There is intentionally no flag to skip it
    // for a real desktop (the mock source shows no real screen). Until the border
    // can be shown, capture stays off, so no desktop frame is ever published
    // without it. Creation is retried, so the service copes with a display that
    // appears later (login screen, user session).
    if cli.capture == CaptureMode::Native {
        let active = Arc::clone(&signaling.active);
        let ready = Arc::clone(&border_ready);
        tokio::spawn(async move {
            let mut indicator: Option<Box<dyn dalker_core::ViewerIndicator>> = None;
            let mut last_err = String::new();
            let mut last_on = false;
            loop {
                if indicator.is_none() {
                    match tokio::task::spawn_blocking(dalker_platform::create_indicator).await {
                        Ok(Ok(i)) => {
                            tracing::info!(backend = i.name(), "remote-session border ready");
                            indicator = Some(i);
                            last_on = false;
                            last_err.clear();
                            ready.store(true, Ordering::SeqCst);
                        }
                        Ok(Err(e)) => {
                            if e != last_err {
                                tracing::warn!(
                                    "remote-session border unavailable ({e}); the desktop is not served until it can be shown"
                                );
                                last_err = e;
                            }
                        }
                        Err(e) => {
                            let e = format!("the border backend failed: {e}");
                            if e != last_err {
                                tracing::error!(
                                    "{e}; the desktop is not served until the border can be shown"
                                );
                                last_err = e;
                            }
                        }
                    }
                }
                if indicator.as_ref().is_some_and(|i| !i.healthy()) {
                    tracing::warn!("remote-session border went away; the desktop is not served until it is back");
                    ready.store(false, Ordering::SeqCst);
                    indicator = None;
                    last_err.clear();
                }
                if let Some(i) = &indicator {
                    let on = active.load(Ordering::SeqCst) > 0;
                    if on != last_on {
                        i.set_active(on);
                        last_on = on;
                    }
                }
                let pause = if indicator.is_some() { 100 } else { 1000 };
                tokio::time::sleep(Duration::from_millis(pause)).await;
            }
        });
    }

    // One reader for the config file: the password, the MCP tokens and the listeners follow it.
    let config = config_watch::spawn(cli::config_path());
    {
        let signaling = Arc::clone(&signaling);
        tokio::spawn(follow_password_changes(
            config.clone(),
            Arc::clone(&auth),
            move || signaling.end_streams(EndReason::PasswordChanged),
        ));
    }

    let exit = Arc::new(tokio::sync::Notify::new());
    #[cfg(windows)]
    let _tray = if cli.no_tray {
        None
    } else {
        start_tray(&status, &signaling.active, &mcp, &exit)
    };
    #[cfg(target_os = "macos")]
    let _menu_bar = if cli.no_tray {
        None
    } else {
        start_menu_bar(&status, &exit)
    };

    // Without a readable file at start, tokens and listeners come from the environment and
    // can't change while the agent runs.
    let follow_file = followed_config_file().is_some();
    if follow_file {
        tokio::spawn(follow_mcp_tokens(config.clone(), mcp.clone()));
    }
    let (wanted_tx, wanted_rx) = watch::channel(startup.wanted());
    if follow_file {
        tokio::spawn(listeners::follow_config(config, startup, wanted_tx));
    }
    let (stop_tx, stop_rx) = watch::channel(false);
    let web = listeners::WebParts {
        // The web client's "AI agents" panel: MCP tokens, for signed-in viewers.
        extra: web_settings_api(&auth, &status),
        auth,
        signaling: Arc::clone(&signaling) as Arc<dyn SignalingHandler>,
        sign_out: {
            let signaling = Arc::clone(&signaling);
            Box::new(move |reason| signaling.end_streams(reason))
        },
    };
    let mcp = listeners::McpParts {
        server: mcp,
        allow_plaintext: cli.mcp_allow_plaintext,
    };
    let supervisor = tokio::spawn(listeners::supervise(
        wanted_rx,
        web,
        mcp,
        stop_rx,
        Arc::clone(&status),
    ));

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = exit.notified() => {}
    }
    tracing::info!("shutting down");
    // Tell the viewers the host is going away, and give the message a moment to leave.
    signaling.end_streams(EndReason::HostShutdown);
    let deadline = Instant::now() + Duration::from_millis(1500);
    while signaling.streams.load(Ordering::SeqCst) > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let _ = stop_tx.send(true);
    let _ = supervisor.await;
    Ok(())
}

/// The settings API the web client uses (its "AI agents" panel): MCP tokens only, for viewers
/// signed in with the access password. The password, the switches and the ports stay
/// host-only (Dalker Settings).
fn web_settings_api(auth: &Arc<Authenticator>, status: &Arc<listeners::Status>) -> axum::Router {
    use settings::api::{Access, Api, Live};
    let signed_in = {
        let auth = Arc::clone(auth);
        move |h: &axum::http::HeaderMap| dalker_web::signed_in(&auth, h)
    };
    let live = {
        let status = Arc::clone(status);
        move || Live {
            web: Some(status.web.lock().unwrap().is_some()),
            mcp: Some(status.mcp.lock().unwrap().is_some()),
        }
    };
    let path = cli::config_path();
    // GNOME on Wayland: the agent runs as the desktop user, and the launcher's root helper
    // changes the tokens for it.
    #[cfg(unix)]
    if let Some(socket) = std::env::var_os("DALKER_SETTINGS_SOCKET") {
        let store = settings::store::SocketStore::new(std::path::Path::new(&socket), &path);
        return Api::new(Box::new(store), Access::Tokens, signed_in, live).router();
    }
    let writable = std::fs::OpenOptions::new().append(true).open(&path).is_ok()
        && cli::is_writable_config(&path);
    if writable {
        let store = settings::store::FileStore::new(&path);
        Api::new(Box::new(store), Access::Tokens, signed_in, live).router()
    } else {
        Api::unavailable(
            "MCP tokens can't be changed from the web client here: the agent can't write its settings file. Use Dalker \
             Settings on the host."
                .to_owned(),
            signed_in,
        )
        .router()
    }
}

/// Applies a new `DALKER_PASSWORD_HASH` written to the config file on the host and calls
/// `sign_out` to end the running streams. Only changes to the file count, so a password given
/// on the command line stays until the file changes.
async fn follow_password_changes(
    mut config: watch::Receiver<config_watch::Snapshot>,
    auth: Arc<Authenticator>,
    sign_out: impl Fn(),
) {
    const KEY: &str = "DALKER_PASSWORD_HASH";
    let mut seen = config_watch::value(&config.borrow_and_update(), KEY).map(str::to_owned);
    while config.changed().await.is_ok() {
        let now = config_watch::value(&config.borrow_and_update(), KEY).map(str::to_owned);
        if now == seen {
            continue;
        }
        if let Some(hash) = &now {
            match auth.set_password_hash(hash) {
                Ok(()) => {
                    tracing::info!("login password changed on the host; all viewers signed out");
                    sign_out();
                }
                Err(e) => tracing::warn!("ignoring the new password in the config file: {e:#}"),
            }
        }
        seen = now;
    }
}

// ------------------------------------------------------------------ MCP

/// `dalkerd mcp-token create|list|revoke`: tokens live in the config file, so (like the
/// password) they can only be managed on the host.
fn run_mcp_token(action: &McpTokenCommand, default_path: &std::path::Path) -> Result<()> {
    let path_of = |config: &Option<std::path::PathBuf>| {
        config.clone().unwrap_or_else(|| default_path.to_owned())
    };
    match action {
        McpTokenCommand::Create {
            name,
            scopes,
            expires,
            config,
        } => {
            let path = path_of(config);
            let scopes = tokens::parse_scopes(scopes).map_err(anyhow::Error::msg)?;
            let lifetime = match expires {
                Some(e) => tokens::parse_lifetime(e).map_err(anyhow::Error::msg)?,
                None => None,
            };
            if mcp_tokens_in(&path).iter().any(|r| r.name == *name) {
                bail!(
                    "a token named `{name}` already exists in {} (revoke it first)",
                    path.display()
                );
            }
            let (token, record) = tokens::generate(name, scopes, lifetime)?;
            // The token itself is kept too, so Dalker Settings can copy it again later.
            cli::edit_config(
                &path,
                &[
                    (record.config_key(), record.config_value()),
                    (record.secret_key(), token.clone()),
                ],
                &[],
            )?;
            eprintln!(
                "Token `{name}` (id {}, scopes {}, expires {}) stored in {}.",
                record.id,
                tokens::format_scopes(&record.scopes),
                record.expires.map_or("never".to_owned(), date),
                path.display()
            );
            eprintln!("Give it to the AI client as a bearer token (Dalker Settings can copy it again later):");
            println!("{token}");
            Ok(())
        }
        McpTokenCommand::List { config } => {
            let path = path_of(config);
            let records = mcp_tokens_in(&path);
            if records.is_empty() {
                println!("no MCP tokens in {}", path.display());
                return Ok(());
            }
            let now = tokens::unix_now();
            println!(
                "{:<10} {:<24} {:<22} {:<11} {:<11} STATUS",
                "ID", "NAME", "SCOPES", "CREATED", "EXPIRES"
            );
            for r in records {
                println!(
                    "{:<10} {:<24} {:<22} {:<11} {:<11} {}",
                    r.id,
                    r.name,
                    tokens::format_scopes(&r.scopes),
                    date(r.created),
                    r.expires.map_or("never".to_owned(), date),
                    if r.expired_at(now) {
                        "expired"
                    } else {
                        "active"
                    }
                );
            }
            Ok(())
        }
        McpTokenCommand::Revoke { name_or_id, config } => {
            let path = path_of(config);
            let records = mcp_tokens_in(&path);
            let matches: Vec<&TokenRecord> = records
                .iter()
                .filter(|r| r.name == *name_or_id || r.id == *name_or_id)
                .collect();
            match matches.as_slice() {
                [] => bail!("no MCP token named `{name_or_id}` in {}", path.display()),
                [r] => {
                    cli::edit_config(&path, &[], &[r.config_key(), r.secret_key()])?;
                    println!(
                        "token `{}` ({}) revoked; a running agent ends its sessions within seconds",
                        r.name, r.id
                    );
                    Ok(())
                }
                _ => bail!("several tokens match `{name_or_id}`; revoke by id"),
            }
        }
    }
}

/// The MCP token records in a config file (malformed lines are skipped with a warning).
fn mcp_tokens_in(path: &std::path::Path) -> Vec<TokenRecord> {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    mcp_tokens_of(&cli::parse_config(&text), path)
}

/// The MCP token records among a config file's settings (`path` is only for the warnings).
fn mcp_tokens_of(settings: &[(String, String)], path: &std::path::Path) -> Vec<TokenRecord> {
    settings
        .iter()
        .filter(|(k, _)| k.starts_with(tokens::CONFIG_KEY_PREFIX))
        .filter_map(|(k, v)| {
            let r = TokenRecord::from_config(k, v);
            if r.is_none() {
                tracing::warn!(
                    "ignoring malformed MCP token line {k} in {}",
                    path.display()
                );
            }
            r
        })
        .collect()
}

/// The config file the agent follows while it runs (password, MCP tokens, listeners), if this
/// process can read it. On Linux the launcher gives a GNOME-on-Wayland agent (which runs as
/// the desktop user) its own copy of the root-only config.
fn followed_config_file() -> Option<std::path::PathBuf> {
    let path = cli::config_path();
    let regular = std::fs::metadata(&path).is_ok_and(|m| m.is_file());
    (regular && std::fs::File::open(&path).is_ok()).then_some(path)
}

/// MCP token records passed as `DALKER_MCP_TOKEN_*` environment variables.
fn mcp_tokens_from_env() -> Vec<TokenRecord> {
    let mut records: Vec<TokenRecord> = std::env::vars()
        .filter(|(k, _)| k.starts_with(tokens::CONFIG_KEY_PREFIX))
        .filter_map(|(k, v)| {
            let r = TokenRecord::from_config(&k, &v);
            if r.is_none() {
                tracing::warn!("ignoring malformed MCP token variable {k}");
            }
            r
        })
        .collect();
    records.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
    records
}

/// `YYYY-MM-DD` (UTC) of a unix time.
fn date(unix: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (unix / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The MCP server, whether or not it listens yet (the host can turn it on while the agent runs).
fn mcp_server(cli: &Cli, agent: &Arc<Agent>) -> McpServer {
    let records = match followed_config_file() {
        Some(path) => mcp_tokens_in(&path),
        None => {
            tracing::info!(
                "MCP tokens taken from the environment (the config file isn't readable here); restart the service \
                 after creating or revoking tokens"
            );
            mcp_tokens_from_env()
        }
    };
    let config = dalker_mcp::McpConfig {
        max_sessions: cli.mcp_max_sessions,
        ..Default::default()
    };
    let agent: Arc<dyn dalker_mcp::DesktopHandle> = Arc::clone(agent) as _;
    McpServer::new(
        agent,
        Arc::new(dalker_mcp::TokenStore::new(records)),
        config,
    )
}

/// Picks up tokens created or revoked on the host (`mcp-token`) without a restart; a revoked
/// token's sessions end at once.
async fn follow_mcp_tokens(mut config: watch::Receiver<config_watch::Snapshot>, server: McpServer) {
    let mut seen = server.tokens().records();
    while config.changed().await.is_ok() {
        let snapshot = config.borrow_and_update().clone();
        let mut now = mcp_tokens_of(&snapshot, &cli::config_path());
        now.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.id.cmp(&b.id)));
        if now != seen {
            tracing::info!(tokens = now.len(), "MCP tokens changed on the host");
            server.set_tokens(now.clone());
            seen = now;
        }
    }
}

/// Input-session ids of AI agents start here, far from the WebRTC engine's own counter.
const MCP_SESSION_BASE: SessionId = 1 << 62;

impl dalker_mcp::DesktopHandle for Agent {
    fn info(&self) -> serde_json::Value {
        let mut info = serde_json::json!({});
        for key in ["hostname", "os", "capture", "version"] {
            info[key] = self.info[key].clone();
        }
        info["locked"] = serde_json::Value::Bool(dalker_platform::screen_locked());
        info
    }

    fn frames(&self) -> dalker_mcp::FrameWatch {
        self.frames.subscribe()
    }

    fn screen_ready(&self) -> bool {
        self.border_ready.load(Ordering::SeqCst)
    }

    fn input_available(&self) -> bool {
        self.input_available
    }

    fn check_viewer_slot(&self) -> Result<(), String> {
        if self.active.load(Ordering::SeqCst) >= self.max_sessions {
            return Err(format!(
                "too many viewers connected (max {})",
                self.max_sessions
            ));
        }
        Ok(())
    }

    fn open_viewer(&self) -> Result<u64, String> {
        // Shares the viewer limit (and the border) with WebRTC viewers.
        if !reserve_slot(&self.active, self.max_sessions) {
            return Err(format!(
                "too many viewers connected (max {})",
                self.max_sessions
            ));
        }
        Ok(MCP_SESSION_BASE + self.next_mcp_id.fetch_add(1, Ordering::SeqCst))
    }

    fn close_viewer(&self, id: u64) {
        let n = self.active.fetch_sub(1, Ordering::SeqCst) - 1;
        tracing::info!(session = id, remaining = n, "AI agent left");
    }

    fn send(&self, id: u64, cmd: HostCommand) {
        let msg = match cmd {
            HostCommand::Input(ev) => ControlMessage::Input(ev),
            HostCommand::SetClipboard(text) => ControlMessage::Clipboard(text),
            HostCommand::EndSession => ControlMessage::Closed,
        };
        let _ = self.control.send((id, msg));
    }

    fn flush(&self, id: u64) -> dalker_mcp::BoxFuture<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        let sent = self.control.send((id, ControlMessage::Flush(tx))).is_ok();
        Box::pin(async move {
            if sent {
                // Typing on Windows takes ~30 ms a character; the caller bounds the wait.
                let _ =
                    tokio::task::spawn_blocking(move || rx.recv_timeout(Duration::from_secs(600)))
                        .await;
            }
        })
    }

    fn read_clipboard(&self) -> dalker_mcp::BoxFuture<Result<Option<String>, String>> {
        let (tx, rx) = oneshot::channel();
        let sent = self
            .clipboard_requests
            .send(ClipboardRequest::Get(tx))
            .is_ok();
        Box::pin(async move {
            if !sent {
                return Err("the host's clipboard is not available".into());
            }
            match tokio::time::timeout(Duration::from_secs(2), rx).await {
                Ok(Ok(result)) => result,
                _ => Err("the host's clipboard did not answer".into()),
            }
        })
    }
}

// ------------------------------------------------------------------ signaling

struct Agent {
    engine: Arc<WebRtcEngine>,
    frames: watch::Sender<Option<Arc<RawFrame>>>,
    clipboard: watch::Receiver<ClipboardState>,
    control: mpsc::UnboundedSender<(SessionId, ControlMessage)>,
    active: Arc<AtomicUsize>,
    /// Set when the host ends every running stream (password changed, web access turned off,
    /// shutdown): each stream tells its viewer the reason and closes.
    sign_out: watch::Sender<Option<EndReason>>,
    /// WebRTC streams still running (unlike `active`, without MCP sessions).
    streams: Arc<AtomicUsize>,
    max_sessions: usize,
    info: serde_json::Value,
    /// False while the border can't be shown (capture is off then).
    border_ready: Arc<AtomicBool>,
    input_available: bool,
    clipboard_requests: std::sync::mpsc::Sender<ClipboardRequest>,
    next_mcp_id: AtomicU64,
}

impl Agent {
    /// Ends every running WebRTC stream, telling each viewer why.
    fn end_streams(&self, reason: EndReason) {
        self.sign_out.send_replace(Some(reason));
    }
}

/// Takes one of `max` viewer slots, or returns false when they are all in use.
fn reserve_slot(active: &AtomicUsize, max: usize) -> bool {
    let mut n = active.load(Ordering::SeqCst);
    while n < max {
        match active.compare_exchange_weak(n, n + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return true,
            Err(now) => n = now,
        }
    }
    false
}

impl SignalingHandler for Agent {
    fn handle_offer(&self, offer_sdp: String) -> BoxFuture<Result<String, String>> {
        // Reserve a slot up front so concurrent offers can't exceed the limit.
        let active = Arc::clone(&self.active);
        if !reserve_slot(&active, self.max_sessions) {
            let max = self.max_sessions;
            return Box::pin(async move { Err(format!("too many viewers (max {max})")) });
        }
        let frames = self.frames.subscribe();
        let clipboard = self.clipboard.clone();
        let mut sign_out = self.sign_out.subscribe();
        let streams = Arc::clone(&self.streams);
        let control = self.control.clone();
        let engine = Arc::clone(&self.engine);
        Box::pin(async move {
            match engine
                .accept_offer(offer_sdp, frames, clipboard, control)
                .await
            {
                Ok((answer, mut session)) => {
                    streams.fetch_add(1, Ordering::SeqCst);
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = session.wait_closed() => {}
                            Ok(()) = sign_out.changed() => {
                                let reason = sign_out.borrow_and_update().unwrap_or(EndReason::HostShutdown);
                                tracing::info!(session = session.id, reason = reason.as_str(), "the host ends the stream");
                                session.end(reason).await;
                            }
                        }
                        streams.fetch_sub(1, Ordering::SeqCst);
                        let n = active.fetch_sub(1, Ordering::SeqCst) - 1;
                        tracing::info!(session = session.id, remaining = n, "viewer left");
                    });
                    Ok(answer)
                }
                Err(e) => {
                    active.fetch_sub(1, Ordering::SeqCst);
                    Err(format!("{e:#}"))
                }
            }
        })
    }

    fn info(&self) -> serde_json::Value {
        let mut info = self.info.clone();
        info["locked"] = serde_json::Value::Bool(dalker_platform::screen_locked());
        info
    }
}

// ------------------------------------------------------------------ capture

type CaptureFactory = Box<dyn Fn() -> Result<Box<dyn ScreenCapturer>, CaptureError> + Send>;

fn spawn_capture_thread(
    factory: CaptureFactory,
    frames: watch::Sender<Option<Arc<RawFrame>>>,
    display: watch::Sender<Option<DisplayInfo>>,
    border_ready: Arc<AtomicBool>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("dalker-capture".into())
        .spawn(move || {
            let mut capturer: Option<Box<dyn ScreenCapturer>> = None;
            loop {
                // Idle: release the capture backend entirely. Recreating it on
                // demand also guarantees DXGI hands us a fresh full frame.
                if frames.receiver_count() == 0 || !border_ready.load(Ordering::SeqCst) {
                    if capturer.take().is_some() {
                        tracing::info!("no viewers; capture paused");
                    }
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
                let cap = match &mut capturer {
                    Some(c) => c,
                    None => match factory() {
                        Ok(c) => {
                            tracing::info!(backend = c.name(), display = ?c.display(), "capture started");
                            display.send_replace(Some(c.display()));
                            capturer.insert(c)
                        }
                        Err(e) => {
                            tracing::warn!("capture init failed, retrying: {e}");
                            std::thread::sleep(Duration::from_secs(1));
                            continue;
                        }
                    },
                };
                match cap.next_frame(Duration::from_millis(100)) {
                    Ok(Some(frame)) => {
                        let info = frame.display_info();
                        if *display.borrow() != Some(info) {
                            display.send_replace(Some(info));
                        }
                        frames.send_replace(Some(Arc::new(frame)));
                    }
                    Ok(None) => {}
                    Err(CaptureError::Lost(why)) => {
                        tracing::info!("capture lost ({why}); recreating");
                        capturer = None;
                        std::thread::sleep(Duration::from_millis(250));
                    }
                    Err(e) => {
                        tracing::warn!("capture error: {e}");
                        capturer = None;
                        std::thread::sleep(Duration::from_secs(1));
                    }
                }
            }
        })
        .context("spawn capture thread")?;
    Ok(())
}

// ------------------------------------------------------------------ input

fn spawn_input_thread(
    mut injector: Option<Box<dyn InputInjector>>,
    mut control: mpsc::UnboundedReceiver<(SessionId, ControlMessage)>,
    mut display: watch::Receiver<Option<DisplayInfo>>,
    clipboard: std::sync::mpsc::Sender<ClipboardRequest>,
) -> Result<()> {
    std::thread::Builder::new()
        .name("dalker-input".into())
        .spawn(move || {
            let mut trackers: HashMap<SessionId, InputTracker> = HashMap::new();
            let mut errors = InjectErrorLog::default();
            let mut apply = |inj: &mut Option<Box<dyn InputInjector>>, ev: &InputEvent| {
                if let Some(inj) = inj {
                    if let Err(e) = inj.inject(ev) {
                        errors.failed(&e.to_string(), ev, Instant::now());
                    }
                }
            };
            while let Some((id, msg)) = control.blocking_recv() {
                if display.has_changed().unwrap_or(false) {
                    if let (Some(inj), Some(d)) = (&mut injector, *display.borrow_and_update()) {
                        tracing::debug!(?d, "input mapped to display");
                        inj.set_display(d);
                    }
                }
                let tracker = trackers.entry(id).or_default();
                match msg {
                    ControlMessage::Input(InputEvent::ReleaseAll) => {
                        for ev in tracker.drain_release_events() {
                            apply(&mut injector, &ev);
                        }
                    }
                    // Some keyboards (IME, dead keys, some mobile keyboards) send key events
                    // with no physical code: nothing to press, so drop them quietly.
                    ControlMessage::Input(InputEvent::Key { code, .. }) if code.is_empty() => {
                        tracing::debug!("ignoring key event without a code");
                    }
                    ControlMessage::Input(ev) => {
                        tracker.observe(&ev);
                        apply(&mut injector, &ev);
                    }
                    ControlMessage::Clipboard(text) => {
                        // Wait until it is on the clipboard, so a paste shortcut sent right
                        // after it pastes this text and not the previous one.
                        let (done_tx, done_rx) = std::sync::mpsc::channel();
                        if clipboard.send(ClipboardRequest::Set(text, done_tx)).is_ok() {
                            let _ = done_rx.recv_timeout(Duration::from_secs(1));
                        }
                    }
                    ControlMessage::Flush(done) => {
                        let _ = done.send(());
                    }
                    ControlMessage::Closed => {
                        if let Some(mut t) = trackers.remove(&id) {
                            for ev in t.drain_release_events() {
                                apply(&mut injector, &ev);
                            }
                        }
                    }
                }
            }
        })
        .context("spawn input thread")?;
    Ok(())
}

// ------------------------------------------------------------------ clipboard

type ClipboardFactory = Box<dyn Fn() -> Result<Box<dyn Clipboard>, String> + Send>;

/// What the clipboard thread is asked to do.
enum ClipboardRequest {
    /// Text a viewer copied, and who to tell once it is on the host's clipboard.
    Set(String, std::sync::mpsc::Sender<()>),
    /// Read the host's text now (an AI agent's `clipboard_get`), whether or not it changed.
    Get(oneshot::Sender<Result<Option<String>, String>>),
}

/// How often the host's clipboard is checked for changes while someone is viewing.
const CLIPBOARD_POLL: Duration = Duration::from_millis(250);
/// How long to wait before opening the clipboard again after it failed.
const CLIPBOARD_RETRY: Duration = Duration::from_secs(5);

/// Calls `op` again (up to four more times, 50 ms apart) while another program holds the
/// clipboard.
fn retry_busy<T>(mut op: impl FnMut() -> Result<T, ClipboardError>) -> Result<T, ClipboardError> {
    let mut result = op();
    for _ in 0..4 {
        let Err(ClipboardError::Busy(_)) = &result else {
            break;
        };
        std::thread::sleep(Duration::from_millis(50));
        result = op();
    }
    result
}

/// Line ends as the browser uses them; Windows apps put CRLF on the clipboard.
fn browser_line_ends(text: String) -> String {
    if text.contains('\r') {
        text.replace("\r\n", "\n")
    } else {
        text
    }
}

/// Runs the host's clipboard on its own thread (Windows ties it to one) and returns where to
/// send requests (text a viewer copied, a read). While viewers are connected, every change of
/// the host's text is published on `state`; what was copied before they connected is not.
fn spawn_clipboard_thread(
    factory: ClipboardFactory,
    state: watch::Sender<ClipboardState>,
    viewers: Arc<AtomicUsize>,
) -> Result<std::sync::mpsc::Sender<ClipboardRequest>> {
    let (set_tx, set_rx) = std::sync::mpsc::channel::<ClipboardRequest>();
    std::thread::Builder::new()
        .name("dalker-clipboard".into())
        .spawn(move || {
            let mut clip: Option<Box<dyn Clipboard>> = None;
            let mut next_open = Instant::now();
            let mut next_poll = Instant::now();
            // What the viewers last saw: (marker, text). `None` = take a fresh baseline.
            let mut seen: Option<(Option<u64>, Option<String>)> = None;
            let fail =
                |clip: &mut Option<Box<dyn Clipboard>>, next_open: &mut Instant, why: String| {
                    *clip = None;
                    *next_open = Instant::now() + CLIPBOARD_RETRY;
                    state.send_if_modified(|s| {
                        let changed = s.available || s.reason.as_deref() != Some(why.as_str());
                        if changed {
                            tracing::warn!("host clipboard not shared: {why}");
                            s.available = false;
                            s.reason = Some(why);
                        }
                        changed
                    });
                };
            loop {
                // Short waits keep the window messages flowing (Windows); see WinClipboard.
                let request = match set_rx.recv_timeout(Duration::from_millis(25)) {
                    Ok(r) => Some(r),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
                };
                if clip.is_none() && Instant::now() >= next_open {
                    match factory() {
                        Ok(c) => {
                            tracing::info!(backend = c.name(), "host clipboard ready");
                            state.send_modify(|s| {
                                s.available = true;
                                s.backend = c.name().to_string();
                                s.reason = None;
                            });
                            clip = Some(c);
                        }
                        Err(why) => fail(&mut clip, &mut next_open, why),
                    }
                }
                if let Some(ClipboardRequest::Get(reply)) = request {
                    let result = match clip.as_mut() {
                        None => Err(state
                            .borrow()
                            .reason
                            .clone()
                            .unwrap_or_else(|| "the host's clipboard is not available".into())),
                        Some(c) => match retry_busy(|| c.get_text()) {
                            Ok(t) => Ok(t.map(browser_line_ends)),
                            Err(ClipboardError::Busy(why)) => Err(why),
                            Err(ClipboardError::Failed(why)) => {
                                fail(&mut clip, &mut next_open, why.clone());
                                Err(why)
                            }
                        },
                    };
                    let _ = reply.send(result);
                    continue;
                }
                if let Some(ClipboardRequest::Set(text, done)) = request {
                    if let Some(c) = clip.as_mut() {
                        match retry_busy(|| c.set_text(&text)) {
                            Ok(()) => {}
                            Err(ClipboardError::Busy(why)) => {
                                tracing::info!("viewer's clipboard text not applied: {why}")
                            }
                            Err(ClipboardError::Failed(why)) => {
                                fail(&mut clip, &mut next_open, why)
                            }
                        }
                    }
                    let _ = done.send(());
                    // Look at once: other viewers get it without waiting for the next poll.
                    next_poll = Instant::now();
                }
                if viewers.load(Ordering::SeqCst) == 0 {
                    seen = None;
                    continue;
                }
                let Some(c) = clip.as_mut() else { continue };
                if Instant::now() < next_poll {
                    continue;
                }
                next_poll = Instant::now() + CLIPBOARD_POLL;
                let marker = match c.change_marker() {
                    Ok(m) => m,
                    Err(ClipboardError::Busy(_)) => continue,
                    Err(ClipboardError::Failed(why)) => {
                        fail(&mut clip, &mut next_open, why);
                        continue;
                    }
                };
                if marker.is_some() && seen.as_ref().is_some_and(|(m, _)| *m == marker) {
                    continue;
                }
                let text = match c.get_text() {
                    Ok(t) => t.map(browser_line_ends),
                    Err(ClipboardError::Busy(_)) => continue,
                    Err(ClipboardError::Failed(why)) => {
                        fail(&mut clip, &mut next_open, why);
                        continue;
                    }
                };
                let first = seen.is_none();
                let changed = seen.as_ref().is_none_or(|(_, t)| *t != text);
                seen = Some((marker, text.clone()));
                if first || !changed {
                    continue;
                }
                if let Some(t) = text {
                    if t.len() > MAX_CLIPBOARD_BYTES {
                        tracing::info!(bytes = t.len(), "host clipboard text too large to share");
                        continue;
                    }
                    tracing::debug!(chars = t.chars().count(), "host clipboard changed");
                    state.send_modify(|s| {
                        s.serial += 1;
                        s.text = Some(t.into());
                    });
                }
            }
        })
        .context("spawn clipboard thread")?;
    Ok(set_tx)
}

/// Logs injection failures where people look (warn), without one line per mouse move:
/// a new or changed error is logged at once, a repeating one at most every
/// [`InjectErrorLog::REPEAT`] with a count of what was suppressed in between.
#[derive(Default)]
struct InjectErrorLog {
    last: Option<(String, Instant)>,
    suppressed: u32,
}

impl InjectErrorLog {
    const REPEAT: Duration = Duration::from_secs(30);

    /// Returns true when the failure was logged at warn level.
    fn failed(&mut self, msg: &str, ev: &InputEvent, now: Instant) -> bool {
        let loud = match &self.last {
            Some((m, at)) => m != msg || now.duration_since(*at) >= Self::REPEAT,
            None => true,
        };
        if loud {
            if self.suppressed > 0 {
                tracing::warn!(
                    "input injection failed: {msg} ({} similar failures suppressed)",
                    self.suppressed
                );
            } else {
                tracing::warn!("input injection failed: {msg}");
            }
            self.last = Some((msg.to_string(), now));
            self.suppressed = 0;
        } else {
            tracing::debug!("inject {ev:?}: {msg}");
            self.suppressed += 1;
        }
        loud
    }
}

fn hostname() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .or_else(|| {
            std::fs::read_to_string("/etc/hostname")
                .ok()
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clipboard_helpers() {
        assert_eq!(browser_line_ends("a\r\nb\nc".into()), "a\nb\nc");
        let mut calls = 0;
        let r = retry_busy(|| {
            calls += 1;
            if calls < 3 {
                Err(ClipboardError::Busy("held".into()))
            } else {
                Ok(calls)
            }
        });
        assert_eq!(r.ok(), Some(3));
        let mut calls = 0;
        let r: Result<(), _> = retry_busy(|| {
            calls += 1;
            Err(ClipboardError::Busy("held".into()))
        });
        assert!(r.is_err());
        assert_eq!(calls, 5, "one try and four retries");
    }

    #[test]
    fn panic_messages_are_read_from_both_payload_kinds() {
        let fixed = std::panic::catch_unwind(|| panic!("fixed text")).unwrap_err();
        assert_eq!(panic_message(fixed.as_ref()), "fixed text");
        let n = 3;
        let formatted = std::panic::catch_unwind(|| panic!("formatted {n}")).unwrap_err();
        assert_eq!(panic_message(formatted.as_ref()), "formatted 3");
        let other = std::panic::catch_unwind(|| std::panic::panic_any(42u8)).unwrap_err();
        assert_eq!(panic_message(other.as_ref()), "(no message)");
    }

    #[test]
    fn inject_errors_are_rate_limited_per_message() {
        let mut log = InjectErrorLog::default();
        let ev = InputEvent::MouseMove { x: 0.5, y: 0.5 };
        let t0 = Instant::now();
        assert!(
            log.failed("secure desktop", &ev, t0),
            "first failure is logged"
        );
        assert!(
            !log.failed("secure desktop", &ev, t0 + Duration::from_secs(1)),
            "repeat is suppressed"
        );
        assert!(
            log.failed("uipi", &ev, t0 + Duration::from_secs(2)),
            "a different error is logged"
        );
        assert!(!log.failed("uipi", &ev, t0 + Duration::from_secs(3)));
        assert!(
            log.failed("uipi", &ev, t0 + Duration::from_secs(33)),
            "repeats again after REPEAT"
        );
        assert_eq!(log.suppressed, 0);
    }
}
