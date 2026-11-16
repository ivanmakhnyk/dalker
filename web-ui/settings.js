// Dalker Settings page. One page for the host's settings window (Windows and Ubuntu: a
// native web view, authorized by a per-launch key in the URL fragment) and the web client's
// "AI agents" panel (signed in with the access password; MCP tokens only). The server says
// what the visitor may change (`access`) and answers every change with the new state.
"use strict";

const $ = (id) => document.getElementById(id);
// Strings in the page's language (i18n.js, loaded first).
const tr = I18N.t;
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
      showBlocked(tr(KEY ? "settings.expired" : "settings.signInFirst"));
    } else {
      setSync("bad", "settings.sync.noConnection");
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
  $("copy-diagnostics").hidden = !host;
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
    $("web-reach").textContent = tr(!s.web.enabled
      ? "settings.web.off"
      : s.web.loopback
        ? "settings.web.loopback"
        : isLocal(web.host)
          ? "settings.web.localOnly"
          : "settings.web.network");
    // The certificate the agent serves over HTTPS, to compare with what the browser shows.
    const tls = s.tls || {};
    $("web-tls").hidden = !(s.web.enabled && tls.enabled && tls.fingerprint);
    $("web-fingerprint").textContent = tls.fingerprint || "";
    // A certificate of the host's own is trusted by browsers: no warning to compare with.
    $("web-fingerprint-hint").hidden = !!tls.own;
    const pw = $("pw-badge");
    pw.className = "badge " + (s.web.password_set ? "ok" : "bad");
    pw.innerHTML = s.web.password_set
      ? `${ICON.shield}<span>${esc(tr("settings.pw.set"))}</span>`
      : `${ICON.alert}<span>${esc(tr("settings.pw.none"))}</span>`;
    $("pw-hint").textContent = tr("settings.pw.hint", { n: s.min_password });
    $("web-port-default").textContent = tr("settings.default", { port: 8742 });
    renderStream(s.stream);
    renderSharing(s.screen_sharing);
  }

  // AI agents.
  setSwitch("mcp-enabled", s.mcp.enabled);
  $("mcp-enabled").disabled = !host;
  $("mcp-switch-box").hidden = !host;
  $("mcp-remote-box").hidden = !host;
  setSwitch("mcp-remote", !s.mcp.loopback);
  $("mcp-switch-box").title = host ? "" : tr("settings.mcp.hostOnly");
  $("mcp-port-default").textContent = tr("settings.default", { port: 8743 });
  if (host) setPort("mcp-port", s.mcp.port, !s.mcp.enabled);
  else $("mcp-port").value = String(s.mcp.port);
  $("mcp-port").readOnly = !host;
  $("mcp-port-apply").hidden = !host;
  $("mcp-port-default").hidden = !host;
  if (host) renderAddress("mcp", s.mcp.urls);
  $("mcp-url").textContent = mcpUrl();
  $("mcp-dot").className = "dot " + statusClass(s.mcp.status);
  $("mcp-reach").textContent = tr(!s.mcp.enabled
    ? "settings.mcp.off"
    : s.mcp.loopback
      ? "settings.mcp.loopback"
      : "settings.mcp.network");
  renderTokens(s.tokens);

  // Footer.
  pill("pill-web", "web", s.web.status, s.web.port, host);
  pill("pill-mcp", "mcp", s.mcp.status, s.mcp.port, true);
  $("pill-web").hidden = !host;
  $("file").textContent = s.file || "";
  const statuses = host ? [s.web.status, s.mcp.status] : [s.mcp.status];
  if (statuses.includes("down")) setSync("bad", "settings.sync.notAnswering");
  else if (statuses.includes("stopping")) setSync("warn", "settings.sync.applying");
  else if (statuses.includes("checking")) setSync("", "settings.sync.checking");
  else setSync("on live", host ? "settings.sync.inSync" : "settings.sync.connected");
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
  $("copy-diagnostics").hidden = true;
  $("tabs").hidden = true;
  $("blocked").hidden = false;
  $("blocked").textContent = text;
  $("card-web").hidden = true;
  $("card-mcp").hidden = true;
  setSync("bad", "settings.sync.unavailable");
}

// `key`: the string to show.
function setSync(cls, key) {
  $("sync-pill").querySelector(".dot").className = "dot " + cls;
  $("sync-text").textContent = tr(key);
}

function statusClass(st) {
  return { running: "on live", stopped: "", down: "bad", stopping: "warn", checking: "" }[st] ?? "";
}

// `kind`: "web" or "mcp".
function pill(id, kind, st, port, show) {
  const el = $(id);
  el.querySelector(".dot").className = "dot " + statusClass(st);
  const text = { running: `:${port}`, stopped: tr("settings.pill.off"), down: tr("settings.pill.notAnswering", { port }), stopping: tr("settings.pill.stopping"), checking: "…" }[st];
  el.querySelector("strong").textContent = text ?? "";
  const title = { running: "Running", stopped: "Stopped", down: "Down", stopping: "Stopping", checking: "Checking" }[st];
  el.title = title ? tr(`settings.pill.${kind}${title}`, { port }) : "";
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

// ---------------------------------------------------------------- stream limits

// The most frame rate and bitrate every viewer gets, and the width wider screens are scaled
// down to (0 = full size). The running agent applies them to open streams too.
const STREAM = [["stream-fps", "fps"], ["stream-bitrate", "bitrate_kbps"], ["stream-width", "max_width"]];
let streamDirty = false;
// Opened with the switch but not applied yet: the file still has no limits of its own.
let streamOpen = false;

function renderStream(st) {
  if (!st) return;
  const d = st.default;
  $("stream-default").textContent = tr("settings.stream.default", { fps: d.fps, kbps: d.bitrate_kbps, px: d.max_width });
  // Collapsed while the defaults apply; expanded once the host sets its own limits.
  const open = st.custom || streamOpen;
  setSwitch("stream-enabled", open);
  $("stream-fields").hidden = !open;
  $("stream-off-hint").hidden = open;
  $("stream-off-hint").textContent =
    tr("settings.stream.offHint", { fps: d.fps, kbps: d.bitrate_kbps, px: d.max_width });
  for (const [id, key] of STREAM) {
    if (!streamDirty && !STREAM.some(([i]) => document.activeElement === $(i))) $(id).value = String(st[key]);
  }
  validateStream();
}

function streamValues() {
  const out = {};
  for (const [id, key] of STREAM) {
    const v = $(id).value.trim();
    out[key] = /^\d{1,6}$/.test(v) ? Number(v) : null;
  }
  return out;
}

function validateStream() {
  const st = state && state.stream;
  if (!st) return false;
  const v = streamValues();
  const r = st.range;
  const within = (x, [lo, hi]) => x !== null && x >= lo && x <= hi;
  let err = "", bad = "";
  if (!within(v.fps, r.fps)) [bad, err] = ["stream-fps", tr("settings.stream.badFps", { min: r.fps[0], max: r.fps[1] })];
  else if (!within(v.bitrate_kbps, r.bitrate_kbps)) [bad, err] = ["stream-bitrate", tr("settings.stream.badBitrate", { min: r.bitrate_kbps[0], max: r.bitrate_kbps[1] })];
  else if (v.max_width !== 0 && !within(v.max_width, r.max_width)) {
    [bad, err] = ["stream-width", tr("settings.stream.badWidth", { min: r.max_width[0], max: r.max_width[1] })];
  }
  $("stream-error").textContent = err;
  for (const [id] of STREAM) $(id).classList.toggle("invalid", id === bad);
  const same = STREAM.every(([, key]) => v[key] === st[key]);
  $("stream-apply").disabled = !!err || same;
  return !err;
}

$("stream-enabled").addEventListener("change", async (e) => {
  const sw = e.target;
  if (sw.checked) {
    // Nothing changes until Apply: the fields start at the limits in force.
    streamOpen = true;
    render(state);
    $("stream-fps").focus();
    return;
  }
  streamOpen = false;
  streamDirty = false;
  if (!state.stream.custom) return render(state);
  sw.dataset.pending = "1";
  sw.disabled = true;
  try {
    const d = state.stream.default;
    applyResult(await api("POST", "/api/settings/stream", { custom: false }),
      tr("settings.stream.reset", { fps: d.fps, kbps: d.bitrate_kbps, px: d.max_width }));
  } catch (err) {
    sw.checked = true;
    message(err.message, false);
  } finally {
    delete sw.dataset.pending;
    sw.disabled = false;
  }
});

for (const [id] of STREAM) {
  $(id).addEventListener("input", () => { streamDirty = true; validateStream(); });
  $(id).addEventListener("keydown", (e) => { if (e.key === "Enter") $("stream-apply").click(); });
}
$("stream-apply").addEventListener("click", async () => {
  if (!validateStream()) return;
  const v = streamValues();
  $("stream-apply").disabled = true;
  try {
    const data = await api("POST", "/api/settings/stream", v);
    streamDirty = false;
    streamOpen = false;
    applyResult(data, tr("settings.stream.applied", { fps: v.fps, kbps: v.bitrate_kbps }));
  } catch (err) {
    $("stream-error").textContent = err.message;
    validateStream();
  }
});

// KDE Plasma, sway and other portal desktops ask the person at the computer to allow
// screen sharing. Doing it here, ahead of time, spares a remote viewer a black screen.
function renderSharing(share) {
  $("share-box").hidden = !share;
  if (!share) return;
  const badge = $("share-badge");
  badge.className = "badge " + (share.allowed ? "ok" : "bad");
  badge.innerHTML = share.allowed
    ? `${ICON.shield}<span>${esc(tr("settings.share.allowed"))}</span>`
    : `${ICON.alert}<span>${esc(tr("settings.share.willAsk"))}</span>`;
  $("share-hint").textContent = tr(share.allowed ? "settings.share.allowedHint" : "settings.share.askHint");
}

$("share-allow").addEventListener("click", async () => {
  const b = $("share-allow");
  b.disabled = true;
  try {
    const r = await api("POST", "/api/settings/screen-sharing");
    applyResult(r);
    toast(tr(r.allowed ? "settings.share.done" : "settings.share.refused"), !r.allowed);
  } catch (e) {
    toast(e.message, true);
  } finally {
    b.disabled = false;
  }
});

function renderAddress(kind, urls) {
  const chosen = pickAddress(urls);
  const select = $(`${kind}-addr`);
  // The server names loopback addresses in English; this page names them itself.
  const name = (u) => (isLocal(u.host) ? tr("settings.thisComputer") : u.name);
  const options = urls.map((u) => `<option value="${esc(u.host)}">${esc(name(u) ? `${name(u)} · ${u.host}` : u.host)}</option>`).join("");
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
  const old = esc(tr("settings.tokens.old"));
  const attr = (key) => esc(tr(key));
  $("token-rows").innerHTML = tokens.map((t) => {
    const dot = t.expired ? "bad" : t.copyable ? "on" : "warn";
    const remote = t.remote === "full"
      ? `<button class="pill on" data-act="remote" title="${attr("settings.tokens.fullTitle")}">${ICON.mouse}${attr("settings.tokens.full")}</button>`
      : t.remote === "view"
        ? `<button class="pill view" data-act="remote" title="${attr("settings.tokens.viewTitle")}">${ICON.shot}${attr("settings.tokens.view")}</button>`
        : `<button class="pill" data-act="remote" title="${attr("settings.tokens.remoteOffTitle")}">${ICON.off}${attr("settings.tokens.off")}</button>`;
    const clip = t.clipboard
      ? `<button class="pill on" data-act="clip" title="${attr("settings.tokens.clipOnTitle")}">${ICON.clip}${attr("settings.tokens.clipOn")}</button>`
      : `<button class="pill" data-act="clip" title="${attr("settings.tokens.clipOffTitle")}">${ICON.off}${attr("settings.tokens.off")}</button>`;
    const sub = t.expired ? tr("settings.tokens.expired")
      : t.expires ? tr("settings.tokens.until", { date: t.expires }) : tr("settings.tokens.created", { date: t.created });
    return `<tr data-id="${esc(t.id)}" data-name="${esc(t.name)}" class="${t.expired ? "expired" : ""}">
      <td><div class="tname"><span class="dot ${dot}"></span><strong title="${esc(t.name)}">${esc(t.name)}</strong><small>${esc(sub)}</small></div></td>
      <td>${t.expired ? `<span class="expired-tag">${attr("settings.tokens.expiredTag")}</span>` : `<span class="tkey">${esc(t.masked)}</span>`}</td>
      <td class="c">${t.expired ? '<span class="pill dead">—</span>' : remote}</td>
      <td class="c">${t.expired ? '<span class="pill dead">—</span>' : clip}</td>
      <td class="r"><div class="actions">
        <button class="icon-btn" data-act="copy" ${t.copyable ? `title="${attr("settings.tokens.copy")}"` : `disabled title="${old}"`} aria-label="${attr("settings.tokens.copy")}">${ICON.copy}</button>
        <button class="icon-btn indigo" data-act="config" ${t.copyable ? `title="${attr("settings.tokens.configTitle")}"` : `disabled title="${old}"`} aria-label="${attr("settings.tokens.config")}">${ICON.json}</button>
        <button class="icon-btn danger" data-act="revoke" title="${attr("settings.tokens.revoke")}" aria-label="${attr("settings.tokens.revoke")}">${ICON.trash}</button>
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
          tr("settings.tokens.permissionsChanged", { name }));
        break;
      case "clip":
        btn.disabled = true;
        applyResult(await api("PATCH", `/api/settings/tokens/${id}`, { clipboard: !t.clipboard }),
          tr("settings.tokens.permissionsChanged", { name }));
        break;
      case "copy": {
        const { token } = await api("GET", `/api/settings/tokens/${id}/secret`);
        await copyText(token);
        toast(tr("settings.tokens.copiedNamed", { name }));
        break;
      }
      case "config": {
        const { token } = await api("GET", `/api/settings/tokens/${id}/secret`);
        openConfig(name, token);
        break;
      }
      case "revoke":
        revokeTarget = { id, name };
        $("dlg-revoke-title").textContent = tr("settings.tokens.revokeNamed", { name });
        openModal("dlg-revoke");
        break;
    }
  } catch (err) {
    message(err.message, false);
    load();
  }
});

// ---------------------------------------------------------------- switches, ports, password

for (const [id, path, kind] of [["web-enabled", "/api/settings/web", "web"], ["mcp-enabled", "/api/settings/mcp", "mcp"]]) {
  $(id).addEventListener("change", async (e) => {
    const sw = e.target;
    const on = sw.checked;
    sw.dataset.pending = "1";
    sw.disabled = true;
    try {
      applyResult(await api("POST", path, { enabled: on }),
        tr(`settings.${kind}.${on ? "turnedOn" : "turnedOff"}`));
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
      tr(on ? "settings.mcp.remoteOn" : "settings.mcp.remoteOff"));
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
  if (input.value.trim() === "") err = tr("settings.port.enter");
  else if (p === null || p < 1024 || p > 65535) err = tr("settings.port.range");
  else if (state && p === (id === "web-port" ? state.mcp.port : state.web.port)) err = tr("settings.port.taken", { port: p });
  if (input.disabled) err = "";
  $(errId).textContent = err;
  input.classList.toggle("invalid", !!err);
  $(id + "-apply").disabled = input.disabled || !!err || p === saved;
  return !err;
}

for (const [id, path, kind] of [["web-port", "/api/settings/web", "web"], ["mcp-port", "/api/settings/mcp", "mcp"]]) {
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
      applyResult(data, tr(`settings.${kind}.moves`, { port }));
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
  if (a && [...a].length < min) err = tr("settings.pw.tooShort", { n: min });
  else if (a && b && a !== b) err = tr("settings.pw.mismatch");
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
    applyResult(data, tr("settings.pw.changed"));
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
      e.title = e.ariaLabel = tr(show ? "settings.pw.hide" : "settings.pw.show");
    }
    input.focus();
  });
}

$("web-url-copy").addEventListener("click", async () => {
  await copyText($("web-url").textContent);
  toast(tr("settings.addressCopied"));
});

$("mcp-url-copy").addEventListener("click", async () => {
  await copyText(mcpUrl());
  toast(tr("settings.addressCopied"));
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
  if (!name) return ($("new-error").textContent = tr("settings.tokens.enterName"));
  // The same rules as the server's (dalker-mcp's tokens.rs), so the reason shows in this
  // page's language.
  if (name.length > 64 || !/^[A-Za-z0-9\-_.@ ]+$/.test(name)) {
    return ($("new-error").textContent = tr("settings.tokens.badName"));
  }
  if (state && state.tokens.some((x) => x.name === name)) {
    return ($("new-error").textContent = tr("settings.tokens.duplicate", { name }));
  }
  if (!$("new-remote").checked && !$("new-clip").checked) {
    return ($("new-error").textContent = tr("settings.tokens.choosePermission"));
  }
  $("new-go").disabled = true;
  try {
    const data = await api("POST", "/api/settings/tokens",
      { name, remote: $("new-remote").checked, clipboard: $("new-clip").checked });
    applyResult(data, tr("settings.tokens.createdNamed", { name }));
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
  $("dlg-token-title").textContent = tr("settings.token.titleNamed", { name });
  $("token-value").textContent = token;
  openModal("dlg-token");
}
$("token-copy").addEventListener("click", async () => {
  await copyText(shown.token);
  toast(tr("settings.tokens.copied"));
});
$("token-config").addEventListener("click", () => openConfig(shown.name, shown.token));

$("revoke-go").addEventListener("click", async () => {
  const { id, name } = revokeTarget;
  closeModal();
  try {
    applyResult(await api("DELETE", `/api/settings/tokens/${id}`), tr("settings.tokens.revoked", { name }));
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
  { id: "code", label: "Claude Code", where: "settings.config.codeWhere",
    text: (url, auth) => `claude mcp add --scope user --transport http dalker ${url} --header "Authorization: ${auth}"` },
  { id: "code-project", label: "settings.config.codeProject", where: "settings.config.codeProjectWhere",
    server: (url) => ({ type: "http", url, headers: { Authorization: "Bearer ${DALKER_MCP_TOKEN}" } }) },
  { id: "cursor", label: "Cursor, LM Studio, Windsurf", where: "settings.config.cursorWhere",
    server: (url, auth) => ({ url, headers: { Authorization: auth } }) },
  { id: "desktop", label: "Claude Desktop", where: "settings.config.desktopWhere",
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
    `<button role="radio" aria-checked="${c.id === config.client}" data-client="${c.id}">${esc(tr(c.label))}</button>`).join("");
  $("config-where").textContent = tr(CLIENTS.find((x) => x.id === config.client).where);
  $("config-json").textContent = configText();
  $("config-off").hidden = state.mcp.enabled;
}

function openConfig(name, token) {
  config = { name, token, client: config?.client ?? "code" };
  $("dlg-config-title").textContent = tr("settings.config.titleNamed", { name });
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
  toast(tr("settings.config.copied"));
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
    return true;
  } catch {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.style.cssText = "position:fixed;opacity:0;top:0;left:0";
    document.body.appendChild(ta);
    ta.select();
    try {
      return document.execCommand("copy");
    } finally {
      ta.remove();
    }
  }
}

$("copy-diagnostics").addEventListener("click", async (event) => {
  const button = event.currentTarget;
  button.disabled = true;
  try {
    const { report } = await api("GET", "/api/settings/diagnostics");
    if (!await copyText(report)) throw new Error(tr("settings.diag.clipFailed"));
    toast(tr("settings.diag.copied"));
  } catch (e) {
    toast(tr("settings.diag.failed", { error: e.message }), true);
  } finally {
    button.disabled = false;
  }
});

// ---------------------------------------------------------------- language
//
// On the host the choice is saved in the config file; "auto" follows the system languages. In
// the web client's "AI agents" panel the viewer's choice applies and the picker is hidden.

function renderLanguagePicker() {
  const select = $("lang");
  select.hidden = EMBEDDED;
  const auto = new Option(tr(KEY ? "language.autoSystem" : "language.autoBrowser"), "auto");
  select.replaceChildren(auto, ...I18N.languages().map((l) => new Option(l.name, l.code)));
  select.value = I18N.choice();
}
$("lang").addEventListener("change", async (e) => {
  try {
    await I18N.setChoice(e.target.value);
  } catch (err) {
    message(tr("language.saveFailed", { error: err.message }), false);
    renderLanguagePicker();
  }
});
I18N.onChange(() => {
  renderLanguagePicker();
  if (state) render(state);
  if (openDialog === "dlg-config") renderConfig();
  if (openDialog === "dlg-token" && shown) $("dlg-token-title").textContent = tr("settings.token.titleNamed", { name: shown.name });
  if (openDialog === "dlg-revoke" && revokeTarget) $("dlg-revoke-title").textContent = tr("settings.tokens.revokeNamed", { name: revokeTarget.name });
  validatePassword();
  validatePort("web-port");
  validatePort("mcp-port");
});
renderLanguagePicker();

// ---------------------------------------------------------------- start

load();
setInterval(() => { if (!busy && !openDialog) load(); }, 2000);
