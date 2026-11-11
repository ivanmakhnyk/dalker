# Security policy

Dalker gives remote control of a computer, so security reports get priority.

## Supported versions

Only the latest release gets security fixes. Pre-release builds from `develop` (`-dev` versions)
are fixed by the next build.

| Version | Supported |
|---|---|
| Latest release | Yes |
| Older releases | No, please update |

## Reporting a vulnerability

**Do not open a public issue, pull request or discussion for a security problem.**

Report it privately through GitHub:
[**Report a vulnerability**](https://github.com/ivanmakhnyk/dalker/security/advisories/new)
(the **Security** tab of the repository → **Report a vulnerability**).

Please include:

- the Dalker version, host OS and session type (Windows; Linux with GNOME on Wayland, KDE Plasma,
  sway or another Wayland desktop, or Xorg; macOS);
- what an attacker can do, and what access they need first (same LAN, internet, a viewer
  password, an MCP token, a local account on the host);
- steps or a proof of concept to reproduce it;
- any logs, with passwords, tokens and cookies removed.

## What happens next

- You get a reply within 7 days.
- The report is confirmed or declined, and you are kept informed while a fix is prepared.
- The fix is released, then a GitHub security advisory is published. You are credited in it
  unless you prefer not to be.

Please give a reasonable amount of time to release a fix before you disclose the problem
publicly.

## Scope

In scope: the `dalkerd` agent, the web client in `web-ui/`, the MCP server, Dalker Settings, and
the installers and packages in `packaging/`.

Out of scope: problems that need an administrator or root account on the host (on macOS, where
Dalker's settings belong to the signed-in user, problems that need that user's account are in
scope if they get around macOS's own permissions), a host exposed to
the internet without a password or TLS against the documented setup, and denial of service by
flooding the network.
