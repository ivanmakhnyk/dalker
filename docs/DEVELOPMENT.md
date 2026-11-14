# Dalker for developers

Configuration reference, building from source, architecture and protocol notes. For installing
and using Dalker, see the [README](../README.md).

## Configuration

Every setting is a command-line flag (`dalkerd --help`). The installed service also reads a
config file of `KEY=VALUE` lines (`#` comments allowed, real environment variables win):

| OS | Default path |
|---|---|
| Windows | `C:\ProgramData\Dalker\dalker.conf` |
| Linux | `/etc/dalker/dalker.conf` (mode `0600`) |
| macOS | `~/Library/Application Support/Dalker/dalker.conf` (per user; the agent runs as the signed-in user) |

Set `DALKER_CONFIG` to use a different file.

```ini
# dalker.conf

# Web client and signaling: on/off, and its address (default port 8742)
DALKER_WEB=on
DALKER_BIND=0.0.0.0:8742

# Login. Store only a hash: `dalkerd set-password` writes it for you.
DALKER_PASSWORD_HASH=$argon2id$v=19$m=19456,t=2,p=1$...

# Pin all WebRTC traffic to ONE UDP port (open/forward just this port in your firewall/NAT).
# New Linux installs set 8443; Windows and macOS use random ports unless this is set.
DALKER_UDP_PORT=8443

# Extra address(es) to advertise as ICE host candidates (NAT 1:1, port forwards), comma-separated
DALKER_PUBLIC_IP=203.0.113.10

# Capture backend: native (Windows: DXGI; Linux: GNOME screen cast, the xdg-desktop-portal screen
# cast on other Wayland desktops, or X11; macOS: ScreenCaptureKit) or mock (test pattern)
DALKER_CAPTURE=native
```

The easiest way to change the main settings is the host's **Dalker Settings** window (below). A
running agent follows the config file: within a few seconds it picks up a new password (and signs
every viewer out), turns the web and MCP servers on or off, moves them to a new port, and applies
new, changed or revoked MCP tokens, without a restart. Settings given on the command line or as
environment variables can't be changed that way; other keys need a service restart.

### Dalker Settings (host)

`dalkerd settings` opens the settings window: **Dalker Settings** in the Windows Start menu (and
the tray menu when `dalkerd` runs by hand; the service has no tray), in the Linux app grid, or in the
macOS menu bar icon. On Windows and Linux it asks for administrator rights (UAC, or a polkit
password prompt), because the config file belongs to the administrators; on macOS the config is the
user's own and no password is asked.

The window is one web page, `web-ui/settings.html`, shown in a native web view (WebView2 on Windows,
WebKitGTK on Ubuntu) and served by the window's own process on a loopback port behind a key made at
each launch, so it works even while the web server is off. The same page, limited to the MCP tokens,
is the web client's **AI agents** panel (robot button in the toolbar).

- **Web remote access:** turn the web server on or off, its HTTP port (default 8742), and the access
  password (shown or hidden while typing; only an Argon2 hash is stored).
- **AI agents (MCP):** turn the MCP server on or off, its port (default 8743), and its access tokens:
  generate one, copy it again at any time, copy a ready **MCP config** (JSON for Claude Code,
  Cursor/LM Studio/Windsurf or Claude Desktop), switch its remote control (screenshots, mouse,
  keyboard) and clipboard permissions on and off without a new token, or revoke it.
- Switches and token changes take effect at once; ports and the password have their own **Apply**
  button. The footer shows whether each server is running.
- In the web client's AI agents panel, viewers signed in with the access password can create, copy,
  change and revoke MCP tokens. The password, the switches and the ports can only be changed on the
  host.

The stream and session options are flags only:

| Flag | Default | Purpose |
|------|---------|---------|
| `--capture native\|mock` | `native` | `mock` = synthetic test pattern, input drawn on it |
| `--fps` | `30` | max frame rate (1–120) |
| `--bitrate-kbps` | `6000` | H.264 target bitrate |
| `--max-width` | `1920` | downscale wider screens; `0` = always native |
| `--udp-port` | ephemeral | single UDP port for all WebRTC traffic |
| `--public-ip` | – | extra host ICE candidate (repeatable); with `--udp-port` and no `--public-ip`, a `stun:` server is asked for it at startup, every 10 minutes and after a viewer fails to connect |
| `--stun` | – | STUN/TURN server URL for non-LAN use (repeatable) |
| `--max-sessions` | `2` | concurrent viewers |
| `--session-hours` | `12` | how long a login stays valid |
| `--no-tray` | – | hide the notification-area icon (Windows) or menu bar icon (macOS) |

The MCP server for AI agents has its own settings; see [AI Agents (MCP)](../README.md#ai-agents-mcp).

Viewers can lower the frame rate, bandwidth share and resolution for their own session in the web
client's settings, within these host limits.


## Building from source

### Prerequisites

- **Rust** stable, 1.89 or newer (`cargo`, `rustc`; the workspace uses the 2021 edition).
- **A C/C++ compiler**: OpenH264 is built from source.
- **[NASM](https://www.nasm.us)** on `PATH` (`winget install NASM.NASM`, `apt install nasm`) for
  OpenH264's assembly. Without it the build still succeeds (with a warning) but the encoder is
  several times slower: a 1080p stream drops to about 14 fps. If you install it after a build,
  run `cargo clean -p openh264-sys2` so OpenH264 is rebuilt with it.
  - **Windows:** MSVC C++ Build Tools with the Windows SDK. [WiX Toolset](https://wixtoolset.org)
    if you want to build the MSI.
  - **Linux:** `build-essential`, plus `libwebkit2gtk-4.1-dev` and `libgtk-3-dev` for the settings
    window's web view. The X11 and uinput code is pure Rust, so no `libx11-dev` or
    `libxtst-dev` is needed. At runtime the agent needs read/write access to `/dev/uinput`, and
    Wayland capture (GNOME, KDE, sway...) needs `gstreamer1.0-tools` and
    `gstreamer1.0-pipewire`; other Wayland desktops than GNOME also need `xdg-desktop-portal` with
    their desktop's backend (`xdg-desktop-portal-kde`, `xdg-desktop-portal-wlr`...). The Wayland
    client code is pure Rust too, so no `libwayland-dev` is needed.
  - **macOS** (14 or later, Apple silicon): Xcode Command Line Tools (`xcode-select --install`)
    and `brew install nasm`. No Apple Developer account is needed.

### Build

```bash
git clone https://github.com/ivanmakhnyk/dalker.git
cd dalker

cargo build -p dalker-agent                 # debug   -> target/debug/dalkerd(.exe)
cargo build --release -p dalker-agent       # release -> target/release/dalkerd(.exe)
cargo test --workspace
```

Release builds use thin LTO, a single codegen unit and stripped symbols.

### Run locally

```bash
cargo run -p dalker-agent -- --capture mock          # safe test pattern
DALKER_PASSWORD=dev cargo run --release -p dalker-agent
```

### Packages

```bash
sh packaging/linux/build-deb.sh target/release/dalkerd dist   # -> dist/dalker_<version>_amd64.deb
pwsh packaging/windows/build-msi.ps1                            # -> dist/Dalker-<version>-x64.msi
sh packaging/macos/build-pkg.sh target/release/dalkerd dist     # -> dist/Dalker-<version>.pkg
```

### CI and releases

Every push to `main` and `develop` and every pull request is checked for formatting (rustfmt), built,
linted (clippy) and tested on Windows and Ubuntu by [`ci.yml`](../.github/workflows/ci.yml) (macOS:
see below), and
[`pr-rules.yml`](../.github/workflows/pr-rules.yml) checks each pull request against the rules in
[CONTRIBUTING.md](../CONTRIBUTING.md). The version is set once, in `[workspace.package]`
of `Cargo.toml`; the crates, the `.deb`, the MSI and the web client (connection details → Version) all
take it from there. Pushing a `vX.Y.Z` tag builds the MSI and the `.deb` and publishes a GitHub
Release ([`release.yml`](../.github/workflows/release.yml)); the macOS `.pkg` follows from its own
workflow. See [RELEASING.md](../RELEASING.md).

macOS has its own two workflows, so a Mac failure never blocks the Windows and Linux ones:
[`macos.yml`](../.github/workflows/macos.yml) builds, lints and tests on Apple silicon whenever
shared code or the macOS parts change (check name `macOS`), and
[`release-macos.yml`](../.github/workflows/release-macos.yml) builds `Dalker-<version>.pkg` for the
same tag and adds it to the release that `release.yml` creates. If it fails, fix it and run it again
for that tag from the Actions tab. It signs the app with the project's self-signed certificate from
the `MACOS_CERT_P12` and `MACOS_CERT_PASSWORD` secrets (ad hoc when they're missing).

## How the host runs

### Windows

Install the MSI (its last page offers to open Dalker Settings, where you choose the access
password; silent installs can pass `DALKER_PASSWORD=...`). It registers the **Dalker** service (LocalSystem, automatic start). The service
starts `dalkerd` as SYSTEM in the active console session and restarts it on crashes, session
switches and logoffs, so the sign-in screen, the lock screen and UAC prompts can be seen and
controlled as well as the desktop, including elevated windows.

- The red border is excluded from screen capture, so viewers don't see it in the stream (Windows 10
  2004 or later; older versions show it in the stream too). The one exception is the first
  lock-screen view (clock and picture), which Windows draws above every other window.
- To sign back in remotely, click the screen or press a key to bring up the sign-in prompt, then
  type the password. Clicks on the lock screen are sent with a Ctrl tap, because Windows ignores
  injected clicks there. The **Ctrl+Alt+Del** button also opens the sign-in prompt (the installer
  sets the `SoftwareSASGeneration` policy so services can send it).
- Change web access, the password and the MCP server with **Dalker Settings** from the Start
  menu or the tray (asks for administrator rights), or `dalkerd set-password` as administrator.
  In a terminal, `dalkerd` runs `dalkerd.com`, a console front end that starts `dalkerd.exe` and
  waits for it. A release `dalkerd.exe` is a GUI program the terminal doesn't wait for, so it
  refuses to prompt for a password itself (pass the password as an argument or use `dalkerd`).
- Installs from before the settings window kept the web client on port 8080; upgrades keep it there
  (`DALKER_BIND` in the config file). New installs use 8742.
- Logs: `C:\ProgramData\Dalker\logs`. In service mode there is no tray icon.

For development, `dalkerd.exe` can also run by hand in the logged-in user's session. It then only
sees that user's desktop (no secure desktop) and can't control elevated windows unless it runs
elevated itself.

### Linux (Ubuntu and Debian)

Install the package, choose the access password in the **Dalker Settings** app (only an Argon2
hash is stored, in `/etc/dalker/dalker.conf`), then **log out and back in once** so GNOME loads
the border extension:

```bash
sudo apt install ./dalker_*_amd64.deb
```

It runs as the systemd service `dalker`, starts at boot and restarts on failure. The launcher
(`/usr/lib/dalker/dalker-launch`) follows the screen:

| Session | Capture | Border | Runs as |
|---------|---------|--------|---------|
| **GNOME on Wayland** | `org.gnome.Mutter.ScreenCast` + PipeWire (via `gst-launch-1.0 pipewiresrc`), no prompt | GNOME Shell extension `dalker-border@ivanmakhnyk.github.io` | the logged-in user |
| **Other Wayland desktops** (KDE Plasma, sway, Hyprland…) | `org.freedesktop.portal.ScreenCast` + PipeWire (same `gst-launch-1.0` reader), asks once | `wlr-layer-shell` surfaces on the overlay layer | the logged-in user |
| Xorg (login screen or session) | X11 root window via MIT-SHM | override-redirect X11 window | root |
| none yet | the agent still answers, but serves no desktop | – | root |

- The GNOME lock screen is streamed and can be unlocked from the viewer: the border extension lifts
  GNOME's "no screen cast while locked" rule only while a Dalker viewer is connected.
- The red border is cropped out of the stream, so the outer 5 pixels of the screen are not visible
  in the viewer. The pointer still reaches the screen's edges: the outermost 5 pixels of the
  picture stretch across the border.
- **No border, no desktop.** If the extension isn't loaded the agent serves nothing and says why in
  `journalctl -u dalker`.
- Change web access, the password and the MCP server with **Dalker Settings** in the app grid
  (polkit asks for an administrator password), or `sudo dalkerd set-password`. Under GNOME on
  Wayland the agent gets its own copy of the config (`/run/dalker/agent.conf`, readable by the
  signed-in user only), which the launcher refreshes when the config changes (MCP tokens themselves left out). Its web
  client changes MCP tokens through a root helper the launcher starts on
  `/run/dalker/settings.sock` (owned by that user; MCP token lines only).
- Configs from before the settings window keep their `DALKER_BIND` (port 8080); new installs use
  8742.
- **Other Wayland desktops** go through xdg-desktop-portal. The first screen cast opens the
  desktop's "share your screen" dialog on the host, so someone there has to accept it once. The
  session asks to be remembered (`persist_mode` 2): the portal returns a restore token, saved in
  `~/.local/state/dalker/screencast-restore-token` and replaced on every start, so later casts
  start silently. A declined (or unanswered, after 3 minutes) dialog is not shown again for a
  minute. The agent doesn't start a cast at startup to probe it, only when a viewer connects.
  The pointer is mapped onto the streamed monitor using the portal's stream position and the
  compositor's `xdg-output` layout. The clipboard comes from Klipper on KDE (`org.kde.klipper`);
  other compositors have none yet. Text that isn't plain ASCII can't be typed there (US-layout
  keys only).
- Limits: the Wayland login screen (GDM, SDDM) is not captured; with several monitors only one is
  captured (GNOME: the primary one; elsewhere: the one picked in the portal dialog).

Manual run (X11 only): set `DISPLAY`/`XAUTHORITY`, give yourself `/dev/uinput` access, then run
`dalkerd`.

### macOS

macOS 14 Sonoma or later. The `.pkg` installs `Dalker.app` in `/Applications` and a LaunchAgent
(`/Library/LaunchAgents/io.github.ivanmakhnyk.Dalker.plist`) that starts it in each user's
session at login; the installer starts it for whoever is signed in right away.

| Piece | How |
|-------|-----|
| Capture | ScreenCaptureKit (`SCStream`) on the main display, at physical (Retina) resolution |
| Input | `CGEventPost` at the HID level; Insert = Help, PrintScreen = F13, ScrollLock = F14 |
| Border | four click-through AppKit windows above everything, on every Space, kept out of the stream |
| Clipboard | `NSPasteboard` text |
| Runs as | the signed-in user; the main thread runs the AppKit loop (`dalker_platform::run_app`) |

- **Permissions.** macOS asks the user to allow Dalker under System Settings › Privacy & Security
  › **Screen & System Audio Recording** and › **Accessibility**. Until both are on, the agent
  serves no picture or ignores input, and says why in its log. Meanwhile the menu bar icon is a
  warning sign, and its menu starts with **Allow Screen Recording…** and **Allow Accessibility…**
  (each opens its System Settings pane) for whichever is missing. Screen Recording only applies
  to a newly started process, so the menu also offers **Restart Dalker** until it is on.
- **Signing without an Apple Developer account.** Releases are signed with a self-signed
  certificate, not notarized. macOS ties both permissions to that signature, so they stay
  granted across updates. The first install is blocked by Gatekeeper: open System Settings ›
  Privacy & Security and click **Open Anyway**. On macOS 15 and later, macOS also asks every
  so often whether Dalker may keep recording the screen.
- **Config and logs** are per user: `~/Library/Application Support/Dalker/dalker.conf` and
  `~/Library/Logs/Dalker`. Dalker Settings opens from the menu bar icon and needs no
  administrator password.
- **Limits:** the login window and FileVault unlock screen can't be reached (the agent only runs
  inside a signed-in session; viewers see a "locked" notice while the screen is locked), Ctrl+Alt+Del
  does nothing, and with several monitors only the main one is captured.
- **Uninstall:** `launchctl bootout gui/$(id -u)/io.github.ivanmakhnyk.Dalker`, then
  `sudo rm -rf /Applications/Dalker.app /Library/LaunchAgents/io.github.ivanmakhnyk.Dalker.plist`.

Manual run: `cargo run -p dalker-agent` from Terminal, after allowing Terminal (not Dalker) under
Screen Recording and Accessibility: macOS gives a program started from Terminal Terminal's
permissions.

### What rights the agent runs with

One `dalkerd` process does everything: the web server (axum), WebRTC (SDP, ICE, DTLS, SCTP,
SRTP in webrtc-rs), DataChannel messages, the OpenH264 encoder, the MCP server, capture, input
and the border. All of it runs with the rights of that one process:

| Host | Runs as |
|------|---------|
| Windows (installed service) | **SYSTEM**, in the console session |
| Linux, Xorg (login screen or session) | **root** |
| Linux, no desktop yet | **root** (serves no desktop) |
| Linux, Wayland (GNOME, KDE Plasma, sway…) | the signed-in user |
| macOS | the signed-in user |

So on Windows and on Xorg, a memory-safety bug in the network-facing code (some of it is C or
`unsafe`) would give an attacker full control of the computer, not only of the desktop. File
transfers on Windows already impersonate the signed-in user (`WTSQueryUserToken`), and the
Wayland agent changes MCP tokens through a small root helper instead of running as root, but the
rest has no separation yet.

The plan (#93) is to split the agent into a small privileged broker that only captures, injects
input, follows the secure desktop and draws the border, and an unprivileged process that runs
HTTP, WebRTC, the encoder and MCP, talking over a local pipe or socket. Until then, keep the host
off networks you don't trust, or put it behind a VPN (see the README's
[Is the connection encrypted?](../README.md#questions)).

## AI agents (MCP) reference

`dalkerd` can serve a [Model Context Protocol](https://modelcontextprotocol.io) endpoint, so an
AI agent can operate the host the way a viewer does. It is **off by default** and can only be turned
on on the host. An agent counts as a viewer: the red border is on while it is connected, it takes a
`--max-sessions` slot, and every key or button it holds is released when it disconnects.

Tools: `get_host_info`, `screenshot` (JPEG, longest side 1280 by default; `region` zooms in as
PNG), `wait`, `mouse_move`, `click`, `drag`, `scroll`, `type_text`, `press_keys` (`"ctrl+c"`,
`"alt+tab"`), `release_all`, `clipboard_get`, `clipboard_set`. Mouse coordinates are pixels of the
last screenshot. Input tools can return a fresh screenshot (`screenshot_after`).

**1. Turn it on** in **Dalker Settings** (AI Agents section), or in the config file
(`C:\ProgramData\Dalker\dalker.conf` or `/etc/dalker/dalker.conf` as administrator/root, or
`~/Library/Application Support/Dalker/dalker.conf` on macOS).
The running agent starts it within seconds:

```ini
DALKER_MCP=on
# Default: this computer only. 0.0.0.0:8743 lets AI clients on other computers connect
# ("Allow other computers" in Dalker Settings).
DALKER_MCP_BIND=127.0.0.1:8743
# AI agents connected at once (default 1); they also count toward --max-sessions.
# Changing this one needs a service restart.
DALKER_MCP_MAX_SESSIONS=1
```

With other computers allowed, tokens cross the network as plain HTTP, like the web client's
password. (Viewers signed in with that password can also copy every token from the web client's AI
agents panel, so the web password protects the tokens too.) On a network you don't trust, leave the switch off and use an SSH tunnel, VPN or TLS
reverse proxy; `DALKER_MCP_ALLOW_PLAINTEXT=0` refuses any network address outright.

**2. Create a token** on the host, in **Dalker Settings** ("Generate new token") or on the
command line. The agent checks requests against an Argon2 hash; the token itself is also kept in
the (administrator-only) config file as `DALKER_MCP_SECRET_<id>`, so the settings window can copy
it and its client configuration again later. On Ubuntu the desktop user's copy of the config
leaves those lines out. Tokens made by older versions have only the hash. Give an agent only
the scopes it needs: `view` (host info, screenshots), `input` (mouse, keyboard), `clipboard`. The
settings window's "remote control" permission is `view` + `input`.

```bash
sudo dalkerd mcp-token create --name claude-laptop --scopes view,input,clipboard --expires 30d
dalkerd mcp-token list
sudo dalkerd mcp-token revoke claude-laptop
```

(On Windows run `dalkerd.exe mcp-token ...` from an administrator terminal. On macOS no `sudo` is
needed, and the program is `/Applications/Dalker.app/Contents/MacOS/dalkerd`, which isn't on
`PATH`.) The agent picks up
new, changed and revoked tokens within seconds; revoking a token or changing its permissions ends
its session at once. Requests carry
`Authorization: Bearer <token>`; requests from web pages (any `Origin` header) are refused.

**3. Connect a client.** From another machine, tunnel the port first:
`ssh -L 8743:127.0.0.1:8743 user@host`.

- *Claude Code:*
  ```bash
  claude mcp add --scope user --transport http dalker http://127.0.0.1:8743/mcp --header "Authorization: Bearer dkmcp_..."
  ```
  or in a project's `.mcp.json`, with the token in an environment variable (never the token itself,
  since that file is usually committed):
  ```json
  {
    "mcpServers": {
      "dalker": {
        "type": "http",
        "url": "http://127.0.0.1:8743/mcp",
        "headers": { "Authorization": "Bearer ${DALKER_MCP_TOKEN}" }
      }
    }
  }
  ```
- *Claude Desktop:* its config file starts local (stdio) servers, so bridge with the
  [`mcp-remote`](https://www.npmjs.com/package/mcp-remote) package in `claude_desktop_config.json`:
  ```json
  {
    "mcpServers": {
      "dalker": {
        "command": "npx",
        "args": ["mcp-remote", "http://127.0.0.1:8743/mcp", "--header", "Authorization:${AUTH}"],
        "env": { "AUTH": "Bearer dkmcp_..." }
      }
    }
  }
  ```

**Stopping it at the host.** Turn the MCP server off in Dalker Settings, or revoke the token. When
`dalkerd` runs by hand on Windows (not as the service, which has no tray), its tray menu also has
**Disconnect AI agents**: it ends their sessions and keeps them out until you choose **Allow AI
agents again**. On macOS, **Quit Dalker** in the menu bar ends every session. Idle agents are disconnected after 10
minutes; an agent that goes away without closing its session gives its slot back within seconds of
its connection closing, or after 2 minutes without any request open. When every slot is taken, a
new agent's `initialize` is refused. Each agent is rate limited. Every tool call is logged with the token name and a short
summary (coordinates, key chord, text length), never the typed text or clipboard contents.

> [!WARNING]
> **Prompt injection.** Everything on the screen reaches the model, and a web page, document or
> message can contain text that tries to give the agent instructions. Use the narrowest scopes that
> work (`view` alone is useful), prefer a dedicated machine or VM for unattended use, keep tokens
> short-lived, and watch the first runs.

## Clipboard and file transfer details

Plain text (up to 1 MB) syncs both ways; images and files don't (files have
their own panel, below). The clipboard button's dot shows the state: green = in sync,
amber = the host copied text that isn't on this computer's clipboard yet, red = the host can't share
its clipboard (the popover says why), grey = sync off. The popover also has a text box to move text
by hand.

- **Viewer → host:** press Ctrl+V (Cmd+V, Shift+Insert) on the remote screen. The viewer puts the
  pasted text on the host's clipboard, then sends the key. This needs no permission and works on
  plain HTTP. Where the browser allows clipboard reads (Chrome/Edge on HTTPS or localhost), new local
  text is also sent whenever the tab gets focus.
- **Host → viewer:** the host clipboard is checked four times a second while someone is viewing. On
  HTTPS or localhost it is written to the local clipboard at once; on plain HTTP at the next click or
  key on the remote screen (Ctrl+C in the viewer counts).
- **Hosts:** Windows uses the session's clipboard (also on the sign-in and lock screens). Under GNOME
  (Wayland or Xorg) the border extension hands the agent the shell's clipboard, and under KDE
  Plasma on Wayland Klipper does (`org.kde.klipper`, so not when the clipboard applet is turned
  off). Other Linux desktops have no clipboard sync yet.

The upload button opens the File Transfers panel. Transfers share the picture's WebRTC connection
(a second DataChannel, `"files"`), so no extra port is opened, and show progress, speed and time
left, with pause and cancel.

- **To the host:** drop files on the remote screen or the panel. They land in the signed-in user's
  Downloads folder; a taken name becomes `name (1).ext`, nothing is overwritten, and cancelled
  uploads are deleted. Folders can't be sent (zip them first).
- **From the host:** "Get files from host" browses the signed-in user's home folder (nothing above
  it; links out of it are refused).
- **Permissions:** files are always read and written as the user signed in to the host's desktop,
  never as SYSTEM or root. With nobody signed in, file transfer is off.

## Architecture

```text
dalker/
├── crates/
│   ├── dalker-core       RawFrame, InputEvent (normalized 0..1 coords), ScreenCapturer /
│   │                       InputInjector traits, MockCapturer, keymap, clipboard types
│   ├── dalker-platform   Windows: DXGI capture, SendInput, service, tray, border overlay
│   │                       Linux:   Mutter ScreenCast or xdg-desktop-portal ScreenCast via
│   │                                PipeWire, X11 MIT-SHM capture, uinput, GNOME extension
│   │                                bridge (D-Bus), wlr-layer-shell and X11 border overlays,
│   │                                Klipper clipboard (KDE)
│   │                       macOS:   ScreenCaptureKit, CGEventPost, AppKit border and menu
│   │                                bar, NSPasteboard
│   ├── dalker-webrtc     webrtc-rs pipeline: BGRA→I420 + OpenH264 encoder, H.264 track,
│   │                       "control" and "files" DataChannels, clipboard sync
│   ├── dalker-web        axum server: Argon2 login, session cookies, /api/offer signaling,
│   │                       embedded web client (rust-embed)
│   ├── dalker-mcp        MCP server for AI agents (rmcp, Streamable HTTP /mcp): tokens,
│   │                       scopes, screenshot, input and clipboard tools
│   └── dalker-agent      the `dalkerd` binary: CLI, config file, orchestration
├── web-ui/                 plain HTML/CSS/JS browser client (no build step)
└── packaging/
    ├── windows/            WiX MSI
    ├── linux/              .deb, systemd unit, launcher, GNOME border extension
    └── macos/              .pkg, Dalker.app, LaunchAgent
```

```text
 Browser                                   Host (dalkerd)
 ───────                                   ────────────────
 POST /api/offer  ── HTTP (axum) ───────▶  login check, SDP answer
 <video>  ◀────── WebRTC H.264 track ────  capture → I420 → OpenH264
 "control" DC  ── mouse / keys / text ──▶  SendInput / uinput
 "control" DC  ◀────── clipboard ───────▶  session clipboard
 "files"   DC  ◀───── file transfer ────▶  user's Downloads / home folder
```

## Hosts behind QEMU user networking (`-netdev user`)

The guest's UDP candidates (10.0.2.x) aren't reachable from outside, so pin WebRTC to one port,
forward it, and advertise the forwarded address. Bind the **UDP** forward to the host's LAN IP, not
`127.0.0.1`: slirp sends the guest's replies from the forward socket, and a loopback-bound socket
can't reach the viewer's LAN address, so ICE never completes.

```text
# host LAN IP = 192.168.50.23 in this example
qemu ... -netdev user,id=net0,hostfwd=tcp::8742-:8742,hostfwd=udp:192.168.50.23:8443-:8443
dalkerd --udp-port 8443 --public-ip 192.168.50.23     # in the guest
```

Windows guests without virtio drivers need `-device e1000e,netdev=net0` instead of `virtio-net-pci`.

## Wire protocol

Signaling: `POST /api/offer {"sdp": "<complete offer>"}` → `{"sdp": "<complete answer>"}`
(non-trickle ICE). Control channel messages (JSON):

```json
{"type":"mouse_move","x":0.25,"y":0.5}
{"type":"mouse_button","button":"left","down":true,"x":0.25,"y":0.5}
{"type":"wheel","dx":0,"dy":120}
{"type":"key","code":"KeyA","down":true}
{"type":"text","text":"é"}
{"type":"release_all"}
{"type":"clipboard","text":"copied text","more":false}
```

Coordinates are normalized to the streamed image; keys are DOM `KeyboardEvent.code` values.
Clipboard text is split into parts of 8192 characters (`"more": true` on every part but the last).
The host also sends `{"type":"clipboard_status","available":true,"backend":"windows"}` when the
channel opens and whenever that changes.

When the host ends a session it first sends `{"type":"session_end","reason":"..."}`, so the viewer
can say why: `password_changed` (the access password was changed on the host; sign in again),
`signed_out` (the sign-in that started the stream was signed out, from another tab for
example), `session_expired` (that sign-in reached its lifetime), `web_off` (web remote access
was turned off) or `host_shutdown` (the agent is stopping or restarting; the viewer reconnects
when it is back). A session that closes without one lost its
network; the viewer reconnects by itself while the host can be reached.

File transfers use the `"files"` channel: JSON requests and replies (`hello`, `list`, `put`, `end`,
`get`, `pause`, `resume`, `cancel`) plus binary messages, each a 4-byte big-endian transfer id
followed by up to 60 KiB of data. See `crates/dalker-webrtc/src/files.rs`.

## Contributing

Branches, commit messages, pull requests, the checks they must pass and the
[Contributor Agreement](../CLA.md) are described in [CONTRIBUTING.md](../CONTRIBUTING.md).
