// Dalker Settings page. One page for the host's settings window (Windows and Ubuntu: a
// native web view, authorized by a per-launch key in the URL fragment) and the web client's
// "AI agents" panel (signed in with the access password; MCP tokens only). The server says
// what the visitor may change (`access`) and answers every change with the new state.
"use strict";

const $ = (id) => document.getElementById(id);
const KEY = new URLSearchParams(location.hash.slice(1)).get("key");
const EMBEDDED = new URLSearchParams(location.search).has("embed");
if (EMBEDDED) document.body.classList.add("embedded");

let state = null;
let busy = 0;
// The open tab (host only), kept across reloads of the page.
let tab = (() => { try { return sessionStorage.getItem("dalker.settings.tab") || "web"; } catch { return "web"; } })();
const dirty = { "web-port": false, "mcp-port": false };

// ---------------------------------------------------------------- API

// Paths are relative to the page (see app.js): the settings also work under a proxy prefix.
async function api(method, path, body) {
  path = path.replace(/^\//, "");
  const headers = {};
  if (KEY) headers["X-Dalker-Key"] = KEY;
  if (body !== undefined) headers["Content-Type"] = "application/json";
  busy++;
  try {
    const r = await fetch(path, { method, headers, credentials: "same-origin",
      body: body === undefined ? undefined : JSON.stringify(body) });
    const data = await r.json().catch(() => ({}));
    if (!r.ok) {
      const err = new Error(data.error || `HTTP ${r.status}`);
      err.status = r.status;
      throw err;
    }
    return data;
  } finally {
    busy--;
  }
}

async function load() {
  try {
    render(await api("GET", "/api/settings"));
  } catch (e) {
    if (e.status === 401) {
      showBlocked(KEY ? "This settings page has expired. Close the window and open Dalker Settings again."
        : "Sign in to the web client first.");
    } else {
      setSync("bad", "NO CONNECTION");
    }
  }
}

function applyResult(data, okText) {
  if (data && data.state) render(data.state);
  if (okText) message(okText, true);
}

// ---------------------------------------------------------------- rendering

function render(s) {
  state = s;
  const host = s.access === "host";
  $("access-tag").hidden = !host;
  $("version").textContent = s.version ? `v${s.version}` : "";
  $("version").hidden = !s.version;
  $("mcp-version").textContent = $("version").textContent;
  $("mcp-version").hidden = !s.version || host;
  if (s.blocked) return showBlocked(s.blocked);
  $("blocked").hidden = true;
  $("tabs").hidden = !host;
  const shown = host ? tab : "mcp";
  $("card-web").hidden = shown !== "web";
  $("card-mcp").hidden = shown !== "mcp";
  for (const t of document.querySelectorAll(".tab")) t.setAttribute("aria-selected", String(t.dataset.tab === shown));
  $("tab-web-dot").className = "tab-dot dot " + statusClass(s.web.status);
  $("tab-mcp-dot").className = "tab-dot dot " + statusClass(s.mcp.status);

  // Web remote access (host only).
  if (host) {
    setSwitch("web-enabled", s.web.enabled);
    setPort("web-port", s.web.port, !s.web.enabled);
    const web = renderAddress("web", s.web.urls);
    $("web-url").textContent = web.url;
    $("web-dot").className = "dot " + statusClass(s.web.status);
    $("web-reach").textContent = !s.web.enabled
      ? "The web server is off."
      : s.web.loopback
        ? "Only browsers on this computer can open it."
        : isLocal(web.host)
          ? "This address works only on this computer. Choose a network address for other computers."
          : "Open it in a browser on another computer on the same network.";
    const pw = $("pw-badge");
    pw.className = "badge " + (s.web.password_set ? "ok" : "bad");
    pw.innerHTML = s.web.password_set
      ? `${ICON.shield}<span>Password is set &amp; active</span>`
      : `${ICON.alert}<span>No password: nobody can sign in</span>`;
    $("pw-hint").textContent =
      `At least ${s.min_password} characters. Changing it signs out everyone who is connected.`;
  }

  // AI agents.
  setSwitch("mcp-enabled", s.mcp.enabled);
  $("mcp-enabled").disabled = !host;
  $("mcp-switch-box").hidden = !host;
  $("mcp-remote-box").hidden = !host;
  setSwitch("mcp-remote", !s.mcp.loopback);
  $("mcp-switch-box").title = host ? "" : "The MCP server is turned on and off on the host, in Dalker Settings.";
  if (host) setPort("mcp-port", s.mcp.port, !s.mcp.enabled);
  else $("mcp-port").value = String(s.mcp.port);
  $("mcp-port").readOnly = !host;
  $("mcp-port-apply").hidden = !host;
  $("mcp-port-default").hidden = !host;
  if (host) renderAddress("mcp", s.mcp.urls);
  $("mcp-url").textContent = mcpUrl();
  $("mcp-dot").className = "dot " + statusClass(s.mcp.status);
  $("mcp-reach").textContent = !s.mcp.enabled
    ? "The MCP server is off."
    : s.mcp.loopback
      ? "Only programs on the host computer can reach it. Use an SSH tunnel or VPN to reach it from elsewhere."
      : "Other computers on the network can connect too (plain HTTP, protected by the token).";
  renderTokens(s.tokens);

  // Footer.
  pill("pill-web", "Web", s.web.status, s.web.port, host);
  pill("pill-mcp", "MCP", s.mcp.status, s.mcp.port, true);
  $("pill-web").hidden = !host;
  $("file").textContent = s.file || "";
  const statuses = host ? [s.web.status, s.mcp.status] : [s.mcp.status];
  if (statuses.includes("down")) setSync("bad", "NOT ANSWERING");
  else if (statuses.includes("stopping")) setSync("warn", "APPLYING");
  else if (statuses.includes("checking")) setSync("", "CHECKING");
  else setSync("on live", host ? "AGENT IN SYNC" : "CONNECTED");
}

for (const t of document.querySelectorAll(".tab")) {
  t.addEventListener("click", () => {
    tab = t.dataset.tab;
    try { sessionStorage.setItem("dalker.settings.tab", tab); } catch {}
    if (state) render(state);
  });
}
document.querySelector(".tabs").addEventListener("keydown", (e) => {
  if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
  const next = tab === "web" ? "mcp" : "web";
  $("tab-" + next).click();
  $("tab-" + next).focus();
});

function showBlocked(text) {
  $("tabs").hidden = true;
  $("blocked").hidden = false;
  $("blocked").textContent = text;
  $("card-web").hidden = true;
  $("card-mcp").hidden = true;
  setSync("bad", "UNAVAILABLE");
}

function setSync(cls, text) {
  $("sync-pill").querySelector(".dot").className = "dot " + cls;
  $("sync-text").textContent = text;
}

function statusClass(st) {
  return { running: "on live", stopped: "", down: "bad", stopping: "warn", checking: "" }[st] ?? "";
}

function pill(id, label, st, port, show) {
  const el = $(id);
  el.querySelector(".dot").className = "dot " + statusClass(st);
  const text = { running: `:${port}`, stopped: "off", down: `:${port} not answering`, stopping: "stopping…", checking: "…" }[st];
  el.querySelector("strong").textContent = text ?? "";
  el.title = {
    running: `${label} server running on port ${port}`,
    stopped: `${label} server is off`,
    down: `${label} server is on but not answering on port ${port}: Dalker may not be running, may still be applying the change, or another program uses the port`,
    stopping: `${label} server is being stopped`,
    checking: `Checking the ${label} server`,
  }[st] ?? "";
  el.hidden = !show;
}

function setSwitch(id, on) {
  const sw = $(id);
  if (!sw.dataset.pending) sw.checked = on;
}

// The MCP address. In the web client: the protocol and host the browser opened the web client
// with, and the MCP server's port (the host computer's own view of it is 127.0.0.1).
function mcpUrl() {
  // Set in the config file (DALKER_MCP_PUBLIC_URL) when a reverse proxy fronts the MCP server.
  if (state.mcp.public_url) return state.mcp.public_url;
  if (state.access === "host") return pickAddress(state.mcp.urls).url;
  return `${location.protocol}//${location.hostname}:${state.mcp.port}/mcp`;
}

// Which of the host's addresses the web and MCP endpoints show, when it has more than one (LAN,
// VPN, virtual switch). The server lists them best first; the choice is kept for both tabs.
let pickedHost = (() => { try { return localStorage.getItem("dalker.settings.host") || ""; } catch { return ""; } })();

function pickAddress(urls) {
  return urls.find((u) => u.host === pickedHost) || urls[0];
}

function isLocal(host) {
  return host === "127.0.0.1" || host === "::1";
}

function renderAddress(kind, urls) {
  const chosen = pickAddress(urls);
  const select = $(`${kind}-addr`);
  const options = urls.map((u) => `<option value="${esc(u.host)}">${esc(u.name ? `${u.name} · ${u.host}` : u.host)}</option>`).join("");
  if (select.dataset.options !== options) {
    select.innerHTML = options;
    select.dataset.options = options;
  }
  select.value = chosen.host;
  select.hidden = urls.length < 2;
  return chosen;
}

for (const kind of ["web", "mcp"]) {
  $(`${kind}-addr`).addEventListener("change", (e) => {
    pickedHost = e.target.value;
    try { localStorage.setItem("dalker.settings.host", pickedHost); } catch {}
    if (state) render(state);
  });
}

function setPort(id, port, disabled) {
  const input = $(id);
  input.disabled = disabled;
  if (!dirty[id] && document.activeElement !== input) input.value = String(port);
  validatePort(id);
}

// ---------------------------------------------------------------- tokens

const ICON = {
  mouse: '<svg viewBox="0 0 24 24"><rect x="6" y="3" width="12" height="18" rx="6"/><path d="M12 7v4"/></svg>',
  shot: '<svg viewBox="0 0 24 24"><path d="M3 7V5a2 2 0 0 1 2-2h2M17 3h2a2 2 0 0 1 2 2v2M21 17v2a2 2 0 0 1-2 2h-2M7 21H5a2 2 0 0 1-2-2v-2"/><circle cx="12" cy="12" r="3"/></svg>',
  clip: '<svg viewBox="0 0 24 24"><rect x="8" y="2" width="8" height="4" rx="1"/><path d="M16 4h2a2 2 0 0 1 2 2v14a2 2 0 0 1-2 2H6a2 2 0 0 1-2-2V6a2 2 0 0 1 2-2h2"/></svg>',
  off: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="m5.7 5.7 12.6 12.6"/></svg>',
  copy: '<svg viewBox="0 0 24 24"><rect x="9" y="9" width="12" height="12" rx="2"/><path d="M5 15V5a2 2 0 0 1 2-2h10"/></svg>',
  json: '<svg viewBox="0 0 24 24"><path d="M8 3H7a2 2 0 0 0-2 2v5a2 2 0 0 1-2 2 2 2 0 0 1 2 2v5a2 2 0 0 0 2 2h1M16 3h1a2 2 0 0 1 2 2v5a2 2 0 0 0 2 2 2 2 0 0 0-2 2v5a2 2 0 0 1-2 2h-1"/></svg>',
  trash: '<svg viewBox="0 0 24 24"><path d="M3 6h18M8 6V4a1 1 0 0 1 1-1h6a1 1 0 0 1 1 1v2M19 6l-1 14a2 2 0 0 1-2 2H8a2 2 0 0 1-2-2L5 6M10 11v6M14 11v6"/></svg>',
  shield: '<svg viewBox="0 0 24 24"><path d="M12 2 4 5v6c0 5 3.4 9.3 8 11 4.6-1.7 8-6 8-11V5z"/><path d="m9 12 2 2 4-4"/></svg>',
  alert: '<svg viewBox="0 0 24 24"><circle cx="12" cy="12" r="9"/><path d="M12 8v4M12 16h.01"/></svg>',
  check: '<svg viewBox="0 0 24 24"><path d="M20 6 9 17l-5-5"/></svg>',
};

const esc = (t) => String(t).replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));

function renderTokens(tokens) {
  $("token-empty").hidden = tokens.length > 0;
  const old = "Made before Dalker kept tokens: only its hash is stored. Generate a new token to copy it.";
  $("token-rows").innerHTML = tokens.map((t) => {
    const dot = t.expired ? "bad" : t.copyable ? "on" : "warn";
    const remote = t.remote === "full"
      ? `<button class="pill on" data-act="remote" title="Screenshots, mouse and keyboard. Click to turn off.">${ICON.mouse}Screen + Input</button>`
      : t.remote === "view"
        ? `<button class="pill view" data-act="remote" title="Screenshots only (made on the command line). Click to allow mouse and keyboard too.">${ICON.shot}Screenshots only</button>`
        : `<button class="pill" data-act="remote" title="Click to allow screenshots, mouse and keyboard.">${ICON.off}Off</button>`;
    const clip = t.clipboard
      ? `<button class="pill on" data-act="clip" title="Reads and writes the clipboard. Click to turn off.">${ICON.clip}Active</button>`
      : `<button class="pill" data-act="clip" title="Click to allow clipboard access.">${ICON.off}Off</button>`;
    const sub = t.expired ? "expired" : t.expires ? `until ${t.expires}` : `created ${t.created}`;
    return `<tr data-id="${esc(t.id)}" data-name="${esc(t.name)}" class="${t.expired ? "expired" : ""}">
      <td><div class="tname"><span class="dot ${dot}"></span><strong title="${esc(t.name)}">${esc(t.name)}</strong><small>${esc(sub)}</small></div></td>
      <td>${t.expired ? '<span class="expired-tag">EXPIRED</span>' : `<span class="tkey">${esc(t.masked)}</span>`}</td>
      <td class="c">${t.expired ? '<span class="pill dead">—</span>' : remote}</td>
      <td class="c">${t.expired ? '<span class="pill dead">—</span>' : clip}</td>
      <td class="r"><div class="actions">
        <button class="icon-btn" data-act="copy" ${t.copyable ? 'title="Copy the token"' : `disabled title="${old}"`} aria-label="Copy the token">${ICON.copy}</button>
        <button class="icon-btn indigo" data-act="config" ${t.copyable ? 'title="MCP configuration for Claude, Cursor or LM Studio"' : `disabled title="${old}"`} aria-label="MCP configuration">${ICON.json}</button>
        <button class="icon-btn danger" data-act="revoke" title="Revoke the token" aria-label="Revoke the token">${ICON.trash}</button>
      </div></td>
    </tr>`;
  }).join("");
}

$("token-rows").addEventListener("click", async (e) => {
  const btn = e.target.closest("button[data-act]");
  if (!btn || btn.disabled) return;
  const row = btn.closest("tr");
  const id = row.dataset.id;
  const name = row.dataset.name;
  const t = state.tokens.find((x) => x.id === id);
  try {
    switch (btn.dataset.act) {
      case "remote":
        btn.disabled = true;
        applyResult(await api("PATCH", `/api/settings/tokens/${id}`, { remote: t.remote !== "full" }),
          `Permissions of “${name}” changed. An AI agent using it reconnects with them.`);
        break;
      case "clip":
        btn.disabled = true;
        applyResult(await api("PATCH", `/api/settings/tokens/${id}`, { clipboard: !t.clipboard }),
          `Permissions of “${name}” changed. An AI agent using it reconnects with them.`);
        break;
      case "copy": {
        const { token } = await api("GET", `/api/settings/tokens/${id}/secret`);
        await copyText(token);
        toast(`The token “${name}” is on the clipboard.`);
        break;
      }
      case "config": {
        const { token } = await api("GET", `/api/settings/tokens/${id}/secret`);
        openConfig(name, token);
        break;
      }
      case "revoke":
        revokeTarget = { id, name };
        $("dlg-revoke-title").textContent = `Revoke “${name}”?`;
        openModal("dlg-revoke");
        break;
    }
  } catch (err) {
    message(err.message, false);
    load();
  }
});

// ---------------------------------------------------------------- switches, ports, password

for (const [id, path, what] of [["web-enabled", "/api/settings/web", "Web server"], ["mcp-enabled", "/api/settings/mcp", "MCP server"]]) {
  $(id).addEventListener("change", async (e) => {
    const sw = e.target;
    const on = sw.checked;
    sw.dataset.pending = "1";
    sw.disabled = true;
    try {
      applyResult(await api("POST", path, { enabled: on }),
        `${what} turned ${on ? "on" : "off"}. Dalker applies it within a few seconds.`);
    } catch (err) {
      sw.checked = !on;
      message(err.message, false);
    } finally {
      delete sw.dataset.pending;
      sw.disabled = id === "mcp-enabled" && state?.access !== "host";
    }
  });
}

$("mcp-remote").addEventListener("change", async (e) => {
  const sw = e.target;
  const on = sw.checked;
  sw.dataset.pending = "1";
  sw.disabled = true;
  try {
    applyResult(await api("POST", "/api/settings/mcp", { remote: on }),
      on ? "Other computers can now connect to the MCP server." : "Only this computer can connect to the MCP server now.");
  } catch (err) {
    sw.checked = !on;
    message(err.message, false);
  } finally {
    delete sw.dataset.pending;
    sw.disabled = false;
  }
});

function portValue(id) {
  const v = $(id).value.trim();
  return /^\d{1,5}$/.test(v) ? Number(v) : null;
}

function validatePort(id) {
  const input = $(id);
  const p = portValue(id);
  const errId = id + "-error";
  const saved = state ? (id === "web-port" ? state.web.port : state.mcp.port) : null;
  let err = "";
  if (input.value.trim() === "") err = "Enter a port from 1024 to 65535.";
  else if (p === null || p < 1024 || p > 65535) err = "Choose a port from 1024 to 65535.";
  else if (state && p === (id === "web-port" ? state.mcp.port : state.web.port)) err = `The other server already uses port ${p}.`;
  if (input.disabled) err = "";
  $(errId).textContent = err;
  input.classList.toggle("invalid", !!err);
  $(id + "-apply").disabled = input.disabled || !!err || p === saved;
  return !err;
}

for (const [id, path, what] of [["web-port", "/api/settings/web", "Web server"], ["mcp-port", "/api/settings/mcp", "MCP server"]]) {
  const input = $(id);
  input.addEventListener("input", () => {
    dirty[id] = true;
    validatePort(id);
  });
  input.addEventListener("keydown", (e) => { if (e.key === "Enter") $(id + "-apply").click(); });
  $(id + "-apply").addEventListener("click", async () => {
    if (!validatePort(id)) return;
    const port = portValue(id);
    $(id + "-apply").disabled = true;
    try {
      const data = await api("POST", path, { port });
      dirty[id] = false;
      applyResult(data, `${what} moves to port ${port} within a few seconds.`);
    } catch (err) {
      $(id + "-error").textContent = err.message;
      validatePort(id);
    }
  });
}

function validatePassword() {
  const a = $("pw-new").value;
  const b = $("pw-repeat").value;
  const min = state?.min_password ?? 8;
  let err = "";
  if (a && a.length < min) err = `The password must have at least ${min} characters.`;
  else if (a && b && a !== b) err = "The two passwords do not match.";
  $("pw-error").textContent = err;
  $("pw-apply").disabled = !a || !b || !!err;
}
$("pw-new").addEventListener("input", validatePassword);
$("pw-repeat").addEventListener("input", validatePassword);
$("pw-repeat").addEventListener("keydown", (e) => { if (e.key === "Enter") $("pw-apply").click(); });
$("pw-apply").addEventListener("click", async () => {
  validatePassword();
  if ($("pw-apply").disabled) return;
  $("pw-apply").disabled = true;
  try {
    const data = await api("POST", "/api/settings/password", { password: $("pw-new").value, repeat: $("pw-repeat").value });
    $("pw-new").value = "";
    $("pw-repeat").value = "";
    applyResult(data, "Password changed. Everyone who was connected is signed out.");
  } catch (err) {
    $("pw-error").textContent = err.message;
  } finally {
    validatePassword();
  }
});

for (const eye of document.querySelectorAll(".eye")) {
  eye.addEventListener("click", () => {
    const input = $(eye.dataset.for);
    const show = input.type === "password";
    for (const id of ["pw-new", "pw-repeat"]) $(id).type = show ? "text" : "password";
    for (const e of document.querySelectorAll(".eye")) {
      e.setAttribute("aria-pressed", String(show));
      e.title = show ? "Hide password" : "Show password";
    }
    input.focus();
  });
}

$("web-url-copy").addEventListener("click", async () => {
  await copyText($("web-url").textContent);
  toast("The address is on the clipboard.");
});

$("mcp-url-copy").addEventListener("click", async () => {
  await copyText(mcpUrl());
  toast("The address is on the clipboard.");
});

// ---------------------------------------------------------------- dialogs

let revokeTarget = null;
let openDialog = null;

function openModal(id) {
  $("modal-back").hidden = false;
  for (const m of document.querySelectorAll(".modal")) m.hidden = m.id !== id;
  openDialog = id;
  const first = $(id).querySelector("input, button.btn-primary");
  if (first) setTimeout(() => first.focus(), 0);
}

function closeModal() {
  $("modal-back").hidden = true;
  openDialog = null;
}

for (const b of document.querySelectorAll("[data-close]")) b.addEventListener("click", closeModal);
$("modal-back").addEventListener("mousedown", (e) => {
  // A new token must not vanish by a stray click: only "Done" closes it.
  if (e.target === $("modal-back") && openDialog !== "dlg-token") closeModal();
});
document.addEventListener("keydown", (e) => {
  if (e.key === "Escape" && openDialog && openDialog !== "dlg-token") closeModal();
});

$("token-new").addEventListener("click", () => {
  $("new-name").value = "";
  $("new-remote").checked = true;
  $("new-clip").checked = false;
  $("new-error").textContent = "";
  openModal("dlg-new");
});
$("new-name").addEventListener("keydown", (e) => { if (e.key === "Enter") $("new-go").click(); });
$("new-go").addEventListener("click", async () => {
  const name = $("new-name").value.trim();
  if (!name) return ($("new-error").textContent = "Enter a name for the token.");
  if (!$("new-remote").checked && !$("new-clip").checked) {
    return ($("new-error").textContent = "Choose at least one permission.");
  }
  $("new-go").disabled = true;
  try {
    const data = await api("POST", "/api/settings/tokens",
      { name, remote: $("new-remote").checked, clipboard: $("new-clip").checked });
    applyResult(data, `Token “${name}” created.`);
    showToken(name, data.token);
  } catch (err) {
    $("new-error").textContent = err.message;
  } finally {
    $("new-go").disabled = false;
  }
});

let shown = null;
function showToken(name, token) {
  shown = { name, token };
  $("dlg-token-title").textContent = `Token for “${name}”`;
  $("token-value").textContent = token;
  openModal("dlg-token");
}
$("token-copy").addEventListener("click", async () => {
  await copyText(shown.token);
  toast("The token is on the clipboard.");
});
$("token-config").addEventListener("click", () => openConfig(shown.name, shown.token));

$("revoke-go").addEventListener("click", async () => {
  const { id, name } = revokeTarget;
  closeModal();
  try {
    applyResult(await api("DELETE", `/api/settings/tokens/${id}`), `“${name}” was revoked.`);
  } catch (err) {
    message(err.message, false);
  }
});

// MCP client configurations, in the field order each client's documentation uses. `text` gives
// what to copy; by default the client's JSON with the token written out.
//
// Claude Code reads `.mcp.json` from the project, which usually ends up in Git: its default is
// a command that keeps the token in the user's own config instead, and its project option reads
// the token from an environment variable.
const CLIENTS = [
  { id: "code", label: "Claude Code", where: "Run this in a terminal. It adds Dalker for your user only (in ~/.claude.json), so the token stays out of your projects.",
    text: (url, auth) => `claude mcp add --scope user --transport http dalker ${url} --header "Authorization: ${auth}"` },
  { id: "code-project", label: "Claude Code (project)", where: "Paste it into .mcp.json in your project. The token stays out of the file: set DALKER_MCP_TOKEN to the token (its copy button in the token list) where Claude Code starts, for example in your shell profile.",
    server: (url) => ({ type: "http", url, headers: { Authorization: "Bearer ${DALKER_MCP_TOKEN}" } }) },
  { id: "cursor", label: "Cursor, LM Studio, Windsurf", where: "Paste it into the client's mcp.json (LM Studio: Program › Edit mcp.json). It contains the token: keep that file out of Git.",
    server: (url, auth) => ({ url, headers: { Authorization: auth } }) },
  { id: "desktop", label: "Claude Desktop", where: "Paste it into claude_desktop_config.json (Settings › Developer › Edit Config). Needs Node.js: the mcp-remote bridge runs through npx.",
    // mcp-remote refuses plain http to anything but localhost unless told to allow it.
    server: (url, auth) => ({ command: "npx", args: ["mcp-remote", url, "--header", "Authorization:${AUTH}", ...(url.startsWith("http:") ? ["--allow-http"] : [])], env: { AUTH: auth } }) },
];
let config = null;

function configText() {
  const c = CLIENTS.find((x) => x.id === config.client);
  const auth = `Bearer ${config.token}`;
  if (c.text) return c.text(mcpUrl(), auth);
  return JSON.stringify({ mcpServers: { dalker: c.server(mcpUrl(), auth) } }, null, 2);
}

function renderConfig() {
  $("config-clients").innerHTML = CLIENTS.map((c) =>
    `<button role="radio" aria-checked="${c.id === config.client}" data-client="${c.id}">${esc(c.label)}</button>`).join("");
  $("config-where").textContent = CLIENTS.find((x) => x.id === config.client).where;
  $("config-json").textContent = configText();
  $("config-off").hidden = state.mcp.enabled;
}

function openConfig(name, token) {
  config = { name, token, client: config?.client ?? "code" };
  $("dlg-config-title").textContent = `MCP configuration for “${name}”`;
  renderConfig();
  openModal("dlg-config");
}
$("config-clients").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-client]");
  if (!b) return;
  config.client = b.dataset.client;
  renderConfig();
});
$("config-copy").addEventListener("click", async () => {
  await copyText(configText());
  toast("The configuration is on the clipboard.");
});

// ---------------------------------------------------------------- feedback

let msgTimer = 0;
function message(text, ok) {
  const el = $("message");
  el.className = "msg" + (ok ? "" : " err");
  el.innerHTML = `${ok ? ICON.check : ICON.alert}<span>${esc(text)}</span>`;
  clearTimeout(msgTimer);
  msgTimer = setTimeout(() => { el.innerHTML = ""; }, 12000);
}

let toastTimer = 0;
function toast(text, bad = false) {
  const t = $("toast");
  t.textContent = text;
  t.className = "toast" + (bad ? " err" : "");
  t.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => { t.hidden = true; }, 2600);
}

// The async clipboard needs a secure page (the settings window's 127.0.0.1 is one; the web
// client over plain HTTP on the LAN is not): fall back to a selected text area.
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return;
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.cssText = "position:fixed;opacity:0;top:0;left:0";
    document.body.appendChild(ta);
    ta.select();
    document.execCommand("copy");
    ta.remove();
  }
}

// ---------------------------------------------------------------- start

load();
setInterval(() => { if (!busy && !openDialog) load(); }, 2000);
