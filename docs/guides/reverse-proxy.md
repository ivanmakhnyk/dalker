# Behind a reverse proxy (advanced)

For people who run nginx, Caddy, Traefik or similar and want HTTPS, a domain name or a shared
port in front of Dalker. Everything here is done in the **config file**, not in the settings
window:

| OS | Config file |
|----|-------------|
| Windows | `C:\ProgramData\Dalker\dalker.conf` |
| Linux | `/etc/dalker/dalker.conf` |
| macOS | `~/Library/Application Support/Dalker/dalker.conf` |

One `KEY=VALUE` per line, `#` for comments. Edit it as an administrator (on macOS the file belongs to your user). The listener keys
(`DALKER_WEB`, `DALKER_BIND`, `DALKER_MCP`, `DALKER_MCP_BIND`) take effect within a few
seconds; the others when the Dalker service restarts.

## What can sit behind the proxy

| Part | Through the proxy? |
|------|--------------------|
| Web client and sign-in (HTTP) | Yes |
| MCP server for AI agents (HTTP, event stream) | Yes |
| The screen video, mouse, keyboard, clipboard and files (WebRTC, UDP) | **No.** They go straight from the browser to the host. |

The proxy therefore protects the page and the sign-in, but the viewer's browser must still reach
the host over UDP. Open one UDP port and pin WebRTC to it, and tell Dalker the address viewers
should use if the host is behind NAT:

```ini
DALKER_UDP_PORT=8744
DALKER_PUBLIC_IP=203.0.113.7
```

Instead of a fixed `DALKER_PUBLIC_IP`, Dalker can ask a public STUN server for the address each
time it starts, which suits a home connection whose IP changes:

```ini
DALKER_UDP_PORT=8744
DALKER_STUN=stun:stun.l.google.com:19302
```

Either way, forward that UDP port on the router to the host (`UDP 8744 → the host's LAN IP`).
Viewers on the same network then reach the host through the router's public address too, which
needs the router to support NAT loopback (most do). If your provider gives the router no public
IP (carrier-grade NAT), no port forward can work; use a VPN to your network instead.

## Settings

```ini
# Listen on this computer only, so the proxy is the only way in.
DALKER_BIND=127.0.0.1:8742
DALKER_MCP=on
DALKER_MCP_BIND=127.0.0.1:8743

# Secure flag of the login cookie: auto (default), on or off.
# auto adds it when the proxy sends "X-Forwarded-Proto: https".
DALKER_COOKIE_SECURE=on

# What the settings window shows as the "Address for AI clients".
DALKER_MCP_PUBLIC_URL=https://desk.example.com/mcp
```

If the proxy is on **another computer**, bind to an address it can reach (for example
`DALKER_MCP_BIND=0.0.0.0:8743`). The MCP server refuses to listen on a network address when
`DALKER_MCP_ALLOW_PLAINTEXT=false`; leave it at its default, and keep the link between the proxy
and Dalker on a trusted network.

## Examples

### Caddy

```caddyfile
desk.example.com {
    reverse_proxy 127.0.0.1:8742
    handle /mcp* {
        reverse_proxy 127.0.0.1:8743 {
            flush_interval -1
        }
    }
}
```

### nginx

```nginx
server {
    listen 443 ssl;
    server_name desk.example.com;
    # ssl_certificate ...

    location /mcp {
        proxy_pass http://127.0.0.1:8743;
        proxy_http_version 1.1;
        proxy_set_header Connection "";
        proxy_buffering off;          # Dalker also sends X-Accel-Buffering: no
        proxy_read_timeout 1h;        # the event stream stays open
    }

    location / {
        proxy_pass http://127.0.0.1:8742;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
}
```

The AI app then connects to `https://desk.example.com/mcp` with its token as before.

### Under a path prefix

The web client uses addresses relative to its own, so it works at `https://example.com/desk/` when
the proxy strips the prefix:

```nginx
location /desk/ {
    proxy_pass http://127.0.0.1:8742/;
    proxy_set_header X-Forwarded-Proto $scheme;
}
```

Open it **with the trailing slash**; redirect `/desk` to `/desk/` in the proxy if people type it
without. The MCP server always answers on `/mcp`; give it its own prefix the same way
(`proxy_pass http://127.0.0.1:8743/;` under `location /desk-mcp/`, then set `DALKER_MCP_PUBLIC_URL`
to `https://example.com/desk-mcp/mcp`).

## Notes

- Dalker does not need to know the proxy's address: it ignores `Host` and `X-Forwarded-*`
  headers except `X-Forwarded-Proto` for the cookie flag.
- The MCP server refuses any request with an `Origin` header (web pages); proxies must not add
  one. Normal AI apps do not send it.
- Login attempts are limited to one at a time with a delay on a wrong password. For more, rate-limit
  `/api/login` in the proxy.
