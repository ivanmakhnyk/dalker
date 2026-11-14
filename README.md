<div align="center">

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="assets/branding/dalker-logo-dark.svg">
  <img src="assets/branding/dalker-logo-light.svg" alt="Dalker" width="420">
</picture>

# Dalker

**See and control your Windows, Linux or macOS computer from any web browser.**

Install Dalker on the computer you want to reach, open its address in a browser on another
device, and you are on its desktop. No account, no cloud service, no app on the viewing side.

[![Release](https://img.shields.io/github/v/release/ivanmakhnyk/dalker?include_prereleases)](https://github.com/ivanmakhnyk/dalker/releases/latest)
[![CI](https://github.com/ivanmakhnyk/dalker/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/ivanmakhnyk/dalker/actions/workflows/ci.yml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue)](#license)

[Platforms](#platform-support) ·
[Download](#download) ·
[Guides](#guides) ·
[Get started](#get-started) ·
[Using Dalker](#using-dalker) ·
[Settings](#settings) ·
[AI agents](#ai-agents-mcp) ·
[Questions](#questions)

<img src="docs/images/viewer-windows.jpg" alt="Dalker in a browser, showing a Windows desktop with the Dalker toolbar on top" width="820">

</div>

## Why Dalker

- **Just a browser.** Chrome, Edge, Firefox or Safari on a laptop, tablet or another PC. Nothing to
  install where you sit.
- **Fast and sharp.** The picture travels directly between the two computers, without passing
  through anyone's server: 30 frames per second out of the box, up to 120 when you raise the limit
  in Dalker Settings.
- **Your computer, your rules.** Dalker runs on your own network. There is no sign-up and no
  outside service in between; one password protects access.
- **You always know.** While anyone is connected, a red frame is drawn around the screen. It can't
  be turned off.
- **More than a picture.** Copy and paste text both ways, drag files across, send Ctrl+Alt+Del,
  and reach the Windows sign-in and lock screens.

<div align="center">
<img src="docs/images/host-red-frame.jpg" alt="The remote Windows computer while a viewer is connected, with a red frame around the screen" width="640">
<br><sub>The remote computer while someone is connected: the red frame shows it.</sub>
</div>

## Platform support

| Computer | Status |
|---|---|
| Windows 10 or 11, 64-bit | Supported. |
| Linux, 64-bit | Xorg desktops and GNOME on Wayland (Ubuntu's standard desktop) are tested. KDE Plasma, sway, Hyprland and other Wayland desktops, through xdg-desktop-portal, are experimental until they are tested in real sessions ([#27](https://github.com/ivanmakhnyk/dalker/issues/27)). |
| macOS 14 Sonoma or later | Preview, Apple silicon only. The installer is not notarized, so macOS asks you to click **Open Anyway** the first time. |

## Guides

- **[Getting started](docs/guides/getting-started.md):** install Dalker, choose the password,
  connect and work on the remote computer, step by step.
- **[AI agents (MCP)](docs/guides/ai-agents.md):** turn on the MCP server, create tokens and
  connect Claude, Cursor or LM Studio.

## Download

Get the installer for the computer you want to reach from
**[the latest release](https://github.com/ivanmakhnyk/dalker/releases/latest)**:

| Computer | Download |
|---|---|
| Windows 10 or 11 (64-bit) | `Dalker-<version>-x64.msi` |
| Ubuntu or Debian, 64-bit | `dalker_<version>_amd64.deb` |
| macOS 14 or later, Apple silicon (preview) | `Dalker-<version>.pkg` |

Only Ubuntu and Debian are packaged for Linux so far. The release's
`dalker-agent-linux-x86_64.tar.gz` holds just the `dalkerd` program, for running by hand or for
packaging; it has no service, launcher, GNOME extension or Dalker Settings app.

The computer you connect *from* needs only a web browser.

## Get started

### 1. Install Dalker

**Windows:** run the `.msi` and click **Install**. On the last page leave **Open Dalker Settings**
ticked and click **Finish**.

**Ubuntu:** open a terminal in the folder with the download and run

```bash
sudo apt install ./dalker_*_amd64.deb
```

then **log out and back in once** so the desktop loads Dalker's red frame. (Without the frame,
Dalker shows nothing; see [Questions](#questions).) On KDE Plasma, sway and other Wayland desktops
there is no extension to load; instead the desktop asks once to **Share** the screen, the first
time someone connects (see [Which Linux desktops work?](#questions)).

**macOS:** open the `.pkg`. The first time, macOS blocks it because it isn't notarized: open
**System Settings → Privacy & Security** and click **Open Anyway**. In the **Set up Dalker**
window, open each permission's settings and allow Dalker under **Screen & System Audio Recording**
and **Accessibility**. Choose **Restart Dalker**, then **Continue to Password**. The menu bar's
**Set Up Permissions…** item reopens the setup window.

Dalker now runs in the background and starts with the computer (on macOS, when you sign in).

### 2. Choose a password

Open **Dalker Settings** (Windows: the Start menu; Ubuntu: the app grid; macOS: the menu bar
icon). On Windows and Ubuntu it asks for administrator rights. Under **Web Remote Access**, type an access password and click **Apply**.

<img src="docs/images/settings-web.png" alt="The Web Remote Access tab of Dalker Settings" width="720">

Until a password is set, nobody can connect.

On KDE Plasma, sway, Hyprland and other Wayland desktops besides GNOME, also click **Allow screen
sharing now** on the same tab, pick the screen and click **Share** (keep "Allow restoring on
future sessions" ticked). Otherwise that dialog first opens when a viewer connects, and the viewer
waits until somebody at the computer answers it.

### 3. Connect

On the other device, open

```text
http://<the computer's IP address>:8742
```

and sign in with the access password. Dalker Settings shows the address to use under **Address for
browsers**. (From a terminal: `ipconfig` on Windows, `hostname -I` on Linux, or
`ipconfig getifaddr en0` on macOS.)

<img src="docs/images/sign-in.png" alt="The Dalker sign-in page in a browser" width="420">

## Using Dalker

Click the remote screen once and your mouse and keyboard work there. The toolbar at the top holds
everything else; drag it by its handle to move it, or fold it away with the arrow.

<img src="docs/images/toolbar.png" alt="The Dalker toolbar: connection status with the Dalker logo, clipboard, files, keys, AI agents, settings, fullscreen, fold away and disconnect" width="518">

| Button | What it does |
|---|---|
| **Status** | The Dalker logo shows the connection: green when connected, amber while connecting, red when it failed, grey when disconnected. Click for details: resolution, frame rate, delay. |
| **Clipboard** | Copy and paste text between the two computers (see below). |
| **Files** | Send files to the remote computer or fetch files from it. |
| **Keys** (keyboard icon) | Send Ctrl+Alt+Del, the Windows key, Alt+Tab and other combinations your own computer would catch. |
| **AI agents** | Manage access for AI assistants (shown when that's turned on). |
| **Settings** | Frame rate, bandwidth and resolution of the picture, fit-to-window or 1:1 view, keyboard mode, scrolling direction, and keeping the remote computer awake. |
| **Fullscreen** | Use the whole screen. Keyboard shortcuts then go to the remote computer too. Press Esc twice to leave. |
| **Disconnect** | End the session. The red frame on the remote computer goes away. |

### Connection details

Click the status pill to see the remote computer's name and system, the picture's resolution and
frame rate, the delay, packet loss and whether the connection is direct.

<img src="docs/images/widget-status.png" alt="Connection details: system, version, resolution, frame rate, round trip, packet loss, route" width="401">

### Copy and paste

Press Ctrl+V on the remote screen to paste text you copied on your computer. Text copied on the
remote computer arrives on your clipboard; the dot on the clipboard button turns green when both
sides match. The clipboard panel also lets you move text by hand.

<img src="docs/images/widget-clipboard.png" alt="The clipboard panel, synced" width="401">

### Files

Drag files onto the remote screen, or into the panel, to send them; they land in the **Downloads**
folder of the person signed in there. **Get files from host** lets you browse their home folder
and download files, with progress, pause and cancel.

<img src="docs/images/widget-files.png" alt="The File Transfers panel browsing the remote computer's home folder" width="401">

### Keys and keyboard mode

**Keys** sends combinations your own computer would otherwise catch: Ctrl+Alt+Del (which also
opens the Windows sign-in screen), the Windows key, Alt+Tab, Ctrl+Shift+Esc.

The keyboard mode decides how typing works. *Hardware* (the default) sends keys by position, so
the remote computer's layout decides the letters; that's best for shortcuts. If your layout
differs from the remote computer's, choose *Direct text* to type exactly what your keyboard shows.

<img src="docs/images/widget-keys.png" alt="The Keys menu with Ctrl+Alt+Del, Windows key, Alt+Tab, Ctrl+Shift+Esc and the keyboard mode" width="401">

### Your session's settings

The gear sets the frame rate, bandwidth and resolution for your session, fit-to-window or 1:1
view, natural scrolling, a keep-alive that stops the remote computer from sleeping while you
watch, and auto-hiding the toolbar (it comes back when the pointer reaches the top edge). The
stream choices go up to the limits set on the remote computer (30 fps and 6 Mbit/s by default; see
[Settings](#settings)).

<img src="docs/images/widget-settings.png" alt="The viewer's settings: stream, display, keyboard and mouse" width="401">

### Locked or signed out?

On Windows you can see and use the lock and sign-in screens: click the screen or press a key, or
use **Keys → Ctrl + Alt + Del**, then type the Windows password.

### Ubuntu too

<img src="docs/images/viewer-ubuntu.jpg" alt="Dalker showing an Ubuntu desktop" width="720">

## Settings

**Dalker Settings** on the remote computer controls who can connect and how:

- **Web Remote Access:** turn browser access on or off, change the port (8742 by default) and the
  access password. Changing the password signs everyone out.
- **Stream limits** (on the same tab, under **Set my own stream limits**): the most frame rate (30 fps by default, up to 120) and
  bitrate (6000 kbit/s by default) every viewer gets, and the width wider screens are scaled down
  to (1920 pixels by default; 0 sends them at full size). Viewers can choose less for their own
  session. Open streams switch to new limits too.
- **AI Agents (MCP):** let AI assistants use the computer, with their own access tokens; see below.

Changes apply within a few seconds; nothing needs a restart.

## AI agents (MCP)

Dalker can let an AI assistant such as Claude see the screen and use the mouse and keyboard, the
same way a person does in the browser. It is **off** until you turn it on.

1. In **Dalker Settings → AI Agents**, switch the MCP server on. It only accepts AI apps on the
   same computer until you also switch on **Allow other computers**.
2. Click **Generate New Token**, give it a name and choose what it may do: look at the screen,
   use mouse and keyboard, use the clipboard.
3. Click **MCP config…**, copy the configuration for your app and paste it into your AI app's settings (Claude Code, Claude
   Desktop, Cursor, LM Studio, Windsurf).

An assistant counts as a viewer: the red frame is on while it works, and revoking its token
disconnects it at once. The [AI agents guide](docs/guides/ai-agents.md) walks through every step.

> [!WARNING]
> Whatever is on the screen reaches the AI, including web pages or messages that might try to give
> it instructions. Grant only what's needed, use a separate computer or VM for unattended work,
> and watch the first runs.

## Questions

**Can I connect over the internet?**
Dalker is made for your home or office network. To reach it from outside, connect to that network
first with a VPN (for example WireGuard or Tailscale), then use the same address. If you run a
reverse proxy and can forward a UDP port on your router, see
[Behind a reverse proxy](docs/guides/reverse-proxy.md).

**Is the connection encrypted?**
The picture, your input, the clipboard and files travel encrypted (WebRTC). But the page itself,
the sign-in and the start of each connection use plain HTTP. Anyone who can read the traffic on
your network can see the access password and the sign-in cookie, and someone who can also change
that traffic can put themselves in the middle of the encrypted stream. The access password also
lets a signed-in viewer copy the AI agents' tokens. So the stream is only as safe as the network:
use Dalker on a network you trust, over a VPN (WireGuard, Tailscale), or behind an HTTPS reverse
proxy ([guide](docs/guides/reverse-proxy.md)) until Dalker serves HTTPS itself
([#15](https://github.com/ivanmakhnyk/dalker/issues/15)).

**The page opens but the picture stays black.**
Check that a firewall allows Dalker (the Windows installer adds the rule itself). On Linux the
picture uses UDP port 8443 next to the page's TCP port 8742, so a firewall needs both open:

```bash
sudo ufw allow 8742/tcp && sudo ufw allow 8443/udp                     # Ubuntu (ufw)
sudo firewall-cmd --permanent --add-port=8742/tcp --add-port=8443/udp \
  && sudo firewall-cmd --reload                                          # firewalld
```

On Ubuntu, also make sure you logged out and back in after installing; `journalctl -u dalker`
says what's missing.

**I forgot the access password.**
Open Dalker Settings on that computer and set a new one. From a terminal you can also run
`sudo dalkerd set-password` (Ubuntu), or on Windows, in a Command Prompt run as administrator,
`"C:\Program Files\Dalker\dalkerd" set-password` (without `.exe`, so the prompt waits for it).
On macOS, Dalker Settings opens from the menu bar icon without an administrator password, or run
`/Applications/Dalker.app/Contents/MacOS/dalkerd set-password` in Terminal.

**Which Linux desktops work?**
Ubuntu's standard GNOME desktop (Wayland or Xorg) and Xorg sessions are tested. KDE Plasma, sway,
Hyprland and other Wayland desktops that have xdg-desktop-portal screen sharing and the
layer-shell protocol are experimental for now ([#27](https://github.com/ivanmakhnyk/dalker/issues/27)). On
those, the desktop asks once to allow screen sharing: click **Allow screen sharing now** in Dalker
Settings, pick the screen and click **Share** (keep "Allow restoring on future sessions" ticked).
If nobody does, the dialog opens when the first viewer connects, and that viewer waits until
somebody at the computer answers it. Later connections start without asking. The Ubuntu login screen is not
supported yet. With several monitors, the main one is shown (on KDE and sway, the one picked in
that dialog).

**Where are the logs?**
Windows: `C:\ProgramData\Dalker\logs`. Ubuntu: `journalctl -u dalker`. macOS:
`~/Library/Logs/Dalker`.

**How do I uninstall?**
Windows: **Settings → Apps → Dalker → Uninstall**. Ubuntu: `sudo apt remove dalker`. macOS: in
Terminal run

```bash
launchctl bootout gui/$(id -u)/io.github.ivanmakhnyk.Dalker
sudo rm -rf /Applications/Dalker.app /Library/LaunchAgents/io.github.ivanmakhnyk.Dalker.plist
rm -rf ~/Library/Application\ Support/Dalker ~/Library/Logs/Dalker   # settings and logs
tccutil reset All io.github.ivanmakhnyk.Dalker                        # its permissions
```

## For developers

Configuration options, command-line flags, building from source, the architecture and the wire
protocol are in **[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md)**. Releases are described in
[RELEASING.md](RELEASING.md).

## License

Copyright (C) 2026 Ivan Makhnyk and Dalker contributors.

Dalker is free software: you can redistribute it and/or modify it under the terms of the
**GNU Affero General Public License, version 3** ([LICENSE](LICENSE), SPDX: `AGPL-3.0-only`).
It is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY; without even the
implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.

In short: you may use, study, change and share Dalker freely. If you distribute Dalker, or a
modified version of it, or let people use a modified version over a network (for example as a
hosted or managed service), you must offer them the complete corresponding source code under the
same license.

**Commercial license.** If you want to embed Dalker in a closed-source product or offer it as a
service without the AGPL obligations, a separate commercial license is available from the
copyright holder. See [COMMERCIAL.md](COMMERCIAL.md).

Contributions are accepted only with a [copyright assignment](CLA.md) to the maintainer; see
[CONTRIBUTING.md](CONTRIBUTING.md).
