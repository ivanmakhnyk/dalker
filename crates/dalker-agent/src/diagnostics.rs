//! Shareable host diagnostics. Only typed configuration and fixed log messages leave here.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::net::SocketAddr;
use std::path::Path;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::process::Command;

use dalker_platform::Desktop;

use crate::cli::{self, CaptureMode, OnOff};

const LOG_LINES: usize = 40;
const LOG_BYTES: u64 = 64 * 1024;

/// `journal` may come from the local Settings helper when the desktop user cannot read it.
pub fn collect(
    config: Option<&str>,
    capture: Option<CaptureMode>,
    journal: Option<&str>,
) -> String {
    let config_values: BTreeMap<_, _> = config
        .map(cli::parse_config)
        .unwrap_or_default()
        .into_iter()
        .collect();
    let capture =
        capture.unwrap_or_else(
            || match config_values.get("DALKER_CAPTURE").map(String::as_str) {
                Some("mock") => CaptureMode::Mock,
                _ => CaptureMode::Native,
            },
        );
    let desktop = Desktop::detect();
    let (capture, input) = match capture {
        CaptureMode::Mock => ("mock", "mock"),
        CaptureMode::Native => (
            desktop.capture_name(),
            match desktop {
                Desktop::Windows => "sendinput",
                Desktop::Mac => "cgevent",
                Desktop::Gnome | Desktop::PortalWayland | Desktop::X11 => "uinput",
                Desktop::None => "none",
            },
        ),
    };
    let session = match desktop {
        Desktop::Windows => "Windows console",
        Desktop::Mac => "macOS desktop",
        Desktop::Gnome if is_wayland() => "GNOME (Wayland)",
        Desktop::Gnome => "GNOME (X11)",
        Desktop::PortalWayland => "Wayland",
        Desktop::X11 => "X11",
        Desktop::None => "No desktop detected",
    };
    let mut report = format!(
        "Dalker diagnostics\nVersion: {}\nOS: {} ({})\nSession: {session}\nCapture backend (expected): {capture}\nInput backend (expected): {input}\nEncoder: OpenH264\n",
        env!("CARGO_PKG_VERSION"), os_build(), std::env::consts::ARCH,
    );
    #[cfg(windows)]
    if let Some(id) = dalker_platform::windows::console_session_id() {
        let _ = writeln!(report, "Console session ID: {id}");
    }
    report.push_str("\nSaved configuration (shareable settings)\n");
    let settings = safe_config(&config_values);
    report.push_str(if config.is_none() {
        "Configuration unavailable.\n"
    } else if settings.is_empty() {
        "No shareable overrides found.\n"
    } else {
        &settings
    });

    report.push_str("\nRecent log events (free-form details omitted)\n");
    let mut dirs: Vec<_> = crate::local_data_dir().into_iter().collect();
    if cfg!(windows) {
        let service_dir = crate::service_log_dir();
        if !dirs.contains(&service_dir) {
            dirs.push(service_dir);
        }
    }
    let start = report.len();
    for dir in dirs {
        for name in ["dalker.log", "service.log"] {
            let events = log_events(&file_tail(&dir.join(name)).unwrap_or_default());
            if !events.is_empty() {
                let _ = write!(report, "{name}\n{events}");
            }
        }
    }
    #[cfg(target_os = "linux")]
    {
        let events = journal.map(str::to_owned).unwrap_or_else(journal_events);
        if !events.is_empty() {
            let _ = write!(report, "dalker.service journal\n{events}");
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = journal;
    if report.len() == start {
        report.push_str("No recognizable events available in readable Dalker logs.\n");
    }
    report
}

fn is_wayland() -> bool {
    #[cfg(target_os = "linux")]
    return dalker_platform::linux::is_wayland_session();
    #[cfg(not(target_os = "linux"))]
    false
}

fn safe_config(values: &BTreeMap<String, String>) -> String {
    let mut text = String::new();
    for (key, value) in values {
        let safe = match key.as_str() {
            "DALKER_WEB" | "DALKER_MCP" | "DALKER_MCP_ALLOW_PLAINTEXT" => {
                OnOff::parse(value).map(|on| OnOff::word(on).to_owned())
            }
            // A port and bind scope are enough for diagnosis. Omit the host IP.
            "DALKER_BIND" | "DALKER_MCP_BIND" => value.parse::<SocketAddr>().ok().map(|addr| {
                let scope = if addr.ip().is_loopback() {
                    "loopback"
                } else if addr.ip().is_unspecified() {
                    "all interfaces"
                } else {
                    "specific interface"
                };
                format!("{scope}, port {}", addr.port())
            }),
            "DALKER_UDP_PORT" => value.parse::<u16>().ok().map(|port| port.to_string()),
            "DALKER_MCP_MAX_SESSIONS"
            | "DALKER_FPS"
            | "DALKER_BITRATE_KBPS"
            | "DALKER_MAX_WIDTH" => value.parse::<u32>().ok().map(|number| number.to_string()),
            "DALKER_CAPTURE" if matches!(value.as_str(), "mock" | "native") => Some(value.clone()),
            "DALKER_COOKIE_SECURE" if matches!(value.as_str(), "auto" | "on" | "off") => {
                Some(value.clone())
            }
            _ => None,
        };
        if let Some(safe) = safe {
            let _ = writeln!(text, "{key}={safe}");
        }
    }
    text
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn command_output(command: &mut Command) -> Option<String> {
    let output = command.output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn os_build() -> String {
    #[cfg(windows)]
    {
        return dalker_platform::windows::os_version()
            .map(|(major, minor, build)| format!("Windows {major}.{minor}, build {build}"))
            .unwrap_or_else(|| "Windows".into());
    }
    #[cfg(target_os = "macos")]
    {
        let version = command_output(Command::new("/usr/bin/sw_vers").arg("-productVersion"));
        let build = command_output(Command::new("/usr/bin/sw_vers").arg("-buildVersion"));
        return match (version, build) {
            (Some(version), Some(build)) => format!("macOS {version}, build {build}"),
            (Some(version), None) => format!("macOS {version}"),
            _ => "macOS".into(),
        };
    }
    #[cfg(target_os = "linux")]
    {
        let release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        let name = release
            .lines()
            .find_map(|line| line.strip_prefix("PRETTY_NAME="))
            .map(|name| name.trim_matches('"'))
            .unwrap_or("Linux");
        return match command_output(Command::new("/usr/bin/uname").arg("-r")) {
            Some(kernel) => format!("{name}, kernel {kernel}"),
            None => name.to_owned(),
        };
    }
    #[allow(unreachable_code)]
    std::env::consts::OS.to_owned()
}

fn file_tail(path: &Path) -> std::io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let start = file.metadata()?.len().saturating_sub(LOG_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(LOG_BYTES).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    Ok(if start == 0 {
        text.into_owned()
    } else {
        text.split_once('\n')
            .map_or("", |(_, rest)| rest)
            .to_owned()
    })
}

#[cfg(target_os = "linux")]
pub fn journal_events() -> String {
    let mut command = Command::new("/usr/bin/journalctl");
    command.args([
        "--unit=dalker.service",
        "--lines=40",
        "--output=cat",
        "--no-pager",
    ]);
    command_output(&mut command).map_or_else(String::new, |text| log_events(&text))
}

// Tracing messages can interpolate credentials, file names, or remote text. Project recent
// events onto fixed descriptions, with no raw message fragments or fields copied into the report.
fn log_events(text: &str) -> String {
    const EVENTS: &[&str] = &[
        "loaded config",
        "desktop detected",
        "capture backend ready",
        "input backend ready",
        "capture started",
        "capture lost",
        "capture error",
        "no viewers; capture paused",
        "remote-session border ready",
        "the border backend failed",
        "remote-session border went away",
        "input injection failed",
        "host clipboard ready",
        "host clipboard not shared",
        "service running",
        "agent started",
        "agent exited",
        "cannot start agent",
        "web server failed",
        "MCP server failed",
        "listener settings changed on the host",
        "peer connection state",
        "viewer never connected",
        "session negotiated",
        "connection came back",
        "encode failed",
        "encoder stopped",
        "shutting down",
        "panic",
    ];
    let mut events = String::new();
    for line in text
        .lines()
        .rev()
        .take(LOG_LINES)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        // The service journal can contain colored tracing output.
        let line: String = line
            .split('\x1b')
            .enumerate()
            .map(|(index, part)| {
                if index == 0 {
                    part
                } else {
                    part.split_once('m').map_or("", |(_, rest)| rest)
                }
            })
            .collect();
        let Some((prefix, message)) = line.split_once(": ") else {
            continue;
        };
        let mut fields = prefix.split_ascii_whitespace();
        let (Some(_timestamp), Some(level @ ("INFO" | "WARN" | "ERROR")), Some(target)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        if !matches!(
            target.split("::").next(),
            Some("dalkerd" | "dalker_webrtc" | "dalker_platform")
        ) {
            continue;
        }
        if let Some(event) = EVENTS.iter().find(|event| message.starts_with(**event)) {
            let _ = writeln!(events, "{level} {event}");
        }
    }
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_only_exports_validated_settings() {
        let config = cli::parse_config(
            "DALKER_WEB=on\nDALKER_BIND=192.0.2.5:8742\nDALKER_UDP_PORT=secret\n\
             DALKER_PASSWORD=test-password\nDALKER_PASSWORD_HASH=test-hash\n\
             DALKER_MCP_TOKEN_example=test-token\nDALKER_STUN=turn:user:secret@example.org\n\
             DALKER_MCP_PUBLIC_URL=https://user:test@example.org/mcp\nDALKER_UNKNOWN=test-secret",
        );
        assert_eq!(
            safe_config(&config.into_iter().collect()),
            "DALKER_BIND=specific interface, port 8742\nDALKER_WEB=on\n"
        );
    }

    #[test]
    fn log_events_keep_only_fixed_messages_from_the_tail() {
        let lines = format!(
            "2026-11-14T00:00:00Z INFO dalkerd: loaded config path=test-path\n{}\
             2026-11-14T00:00:00Z INFO dalker_platform::windows::service: service running command=--password secret\n\
             \x1b[2m2026-11-14T00:00:00Z\x1b[0m \x1b[33m WARN\x1b[0m \x1b[2mdalkerd\x1b[0m\x1b[2m:\x1b[0m capture lost (secret)\n\
             2026-11-14T00:00:00Z ERROR dalkerd: raw secret message",
            "ignored debug line\n".repeat(LOG_LINES),
        );
        assert_eq!(
            log_events(&lines),
            "INFO service running\nWARN capture lost\n"
        );
    }
}
