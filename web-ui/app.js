// Dalker web client: login, WebRTC viewer, input capture, clipboard sync, settings,
// file transfer.
// Pointer coordinates are always sent normalized to 0..1 of the remote screen;
// the agent alone converts them to host pixels.
"use strict";

const $ = (id) => document.getElementById(id);
const video = $("screen");
const stage = $("stage");
const toolbar = $("toolbar");
const notch = $("notch");
const statusEl = $("status");

let pc = null;
let slotRetryTimer = null;
let slotRetries = 0;
let dc = null;
let statsTimer = null;
let lockTimer = null;
let info = {};
let state = "idle";
let statusText = "disconnected";
let lastRtt = null;
let hintTimer = null;
let hintFadeTimer = null;
// Why the host ended the session: the reason of its "session_end" message, until the
// channel closes.
let endReason = null;
let recoverTimer = null;
let recoverTries = 0;
let connectedAt = 0;
let quickDrops = 0;

// ---------------------------------------------------------------- UI helpers

function showHint(fullscreen = !!document.fullscreenElement) {
  clearTimeout(hintTimer);
  clearTimeout(hintFadeTimer);
  const el = $("hint");
  $("hint-esc").hidden = !fullscreen;
  el.hidden = false;
  el.classList.add("visible");
  hintTimer = setTimeout(() => {
    el.classList.remove("visible");
    hintFadeTimer = setTimeout(() => {
      el.hidden = true;
    }, 500);
  }, 4000);
}

function hideHint() {
  clearTimeout(hintTimer);
  clearTimeout(hintFadeTimer);
  const el = $("hint");
  el.classList.remove("visible");
  el.hidden = true;
  $("hint-esc").hidden = true;
}

function setStatus(text, cls) {
  const wasConnected = state === "connected";
  state = cls;
  statusText = text;
  // Connected, the pill shows the round trip once it is measured; otherwise the state.
  statusEl.textContent = cls === "connected" && lastRtt !== null ? `${lastRtt.toFixed(0)} ms` : text;
  $("btn-status").className = `pill ${cls}`;
  notch.className = cls;
  $("pop-status").querySelector(".status-mark").setAttribute("class", `status-mark ${cls}`);
  $("s-status").textContent = text;
  const live = cls === "connected" || cls === "connecting";
  $("btn-connect").hidden = live;
  $("btn-disconnect").hidden = !live;
  $("btn-fullscreen").hidden = cls !== "connected";
  updateSessionGroup();
  applyAutoHide();
  updateKeysCapabilities();
  if (cls !== "connected") { closePopover("pop-keys"); closePopover("pop-clip"); closePopover("pop-settings"); closePopover("pop-files"); }
  if (cls === "connected") {
    if (!wasConnected) showHint();
  } else {
    hideHint();
    showLocked(false);
  }
}

// Hosts that can't stream their lock screen (GNOME: the border is off there) report
// "locked"; say so instead of showing a frozen picture. Clicks and keys still go through.
// Windows streams its lock and sign-in screens and never reports "locked".
function showLocked(locked) {
  const was = !$("locked").hidden;
  $("locked").hidden = !locked;
  stage.classList.toggle("locked", locked);
  // Keys must reach the host now, without a click on a picture that isn't there.
  if (locked && !was) focusStageUnlessTyping();
}

// A click on a withheld lock screen also sends a lone Ctrl press, which wakes it without
// typing anything. (The Windows agent does this itself for clicks on its lock screen.)
function wakeLockScreen() {
  if (!stage.classList.contains("locked")) return;
  send({ type: "key", code: "ControlLeft", down: true });
  send({ type: "key", code: "ControlLeft", down: false });
}

// Gives the remote screen keyboard focus unless the user is busy elsewhere on the page
// (login dialog, text fields, the toolbar).
function focusStageUnlessTyping() {
  const a = document.activeElement;
  if (a && a !== document.body && a !== stage && (a.closest("dialog, #toolbar") || /^(INPUT|TEXTAREA|SELECT)$/.test(a.tagName))) return;
  stage.focus({ preventScroll: true });
}

async function pollLocked() {
  try {
    const now = await api("/api/info");
    showLocked(!!now.locked && !!dc && dc.readyState === "open");
  } catch (_) { /* transient; the connection state covers real failures */ }
}

// Paths are relative to the page, so the client also works behind a reverse proxy that serves
// it under a prefix (https://host/dalker/).
async function api(path, body) {
  path = path.replace(/^\//, "");
  const opts = body === undefined
    ? { credentials: "same-origin" }
    : { method: "POST", credentials: "same-origin", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) };
  const r = await fetch(path, opts);
  const data = await r.json().catch(() => ({}));
  if (!r.ok) {
    const err = new Error(data.error || `HTTP ${r.status}`);
    err.status = r.status;
    throw err;
  }
  return data;
}

// ---------------------------------------------------------------- toolbar

const POPOVERS = { "pop-status": "btn-status", "pop-keys": "btn-keys", "pop-clip": "btn-clip", "pop-settings": "btn-settings", "pop-files": "btn-files" };

function closePopover(id) {
  $(id).hidden = true;
  $(POPOVERS[id]).setAttribute("aria-expanded", "false");
}
function closePopovers() { Object.keys(POPOVERS).forEach(closePopover); }
function togglePopover(id) {
  const open = $(id).hidden;
  closePopovers();
  $(id).hidden = !open;
  $(POPOVERS[id]).setAttribute("aria-expanded", String(open));
}

$("btn-status").addEventListener("click", () => togglePopover("pop-status"));
$("btn-keys").addEventListener("click", () => togglePopover("pop-keys"));
$("btn-clip").addEventListener("click", () => { togglePopover("pop-clip"); renderClip(); });
$("btn-settings").addEventListener("click", () => { togglePopover("pop-settings"); renderSettings(); });

// The group of session buttons shows while connected. The AI agents button sits in it too but
// also shows while disconnected, when the host's MCP server is on: then it is the only one.
function updateSessionGroup() {
  const connected = state === "connected";
  const group = $("group-keys");
  group.classList.toggle("disconnected", !connected);
  group.hidden = !connected && $("btn-ai").hidden;
}

// AI agents: the host's settings page, MCP tokens only (the server decides what may change).
const aiPanel = $("ai-panel");
$("btn-ai").addEventListener("click", () => {
  closePopovers();
  $("ai-frame").src = "settings.html?embed=1";
  aiPanel.hidden = false;
});
// The AI agents button shows only while the host's MCP server is on (it is turned on and off
// on the host, in Dalker Settings).
let mcpTimer = 0;
async function watchMcp() {
  clearTimeout(mcpTimer);
  let on = false;
  try {
    const s = await api("/api/settings");
    on = !s.blocked && s.mcp && s.mcp.enabled;
  } catch (err) {
    if (err.status === 401) return;
  }
  $("btn-ai").hidden = !on;
  updateSessionGroup();
  if (!on && !aiPanel.hidden) closeAiPanel();
  mcpTimer = setTimeout(watchMcp, 10000);
}
function closeAiPanel() {
  aiPanel.hidden = true;
  $("ai-frame").src = "about:blank";
  stage.focus({ preventScroll: true });
}
$("ai-close").addEventListener("click", closeAiPanel);
aiPanel.addEventListener("pointerdown", (e) => { if (e.target === aiPanel) closeAiPanel(); });
aiPanel.addEventListener("keydown", (e) => { if (e.key === "Escape") closeAiPanel(); });
// A click anywhere else closes the popovers.
document.addEventListener("pointerdown", (e) => {
  if (!toolbar.contains(e.target)) closePopovers();
}, true);
toolbar.addEventListener("keydown", (e) => {
  if (e.key === "Escape") { closePopovers(); stage.focus({ preventScroll: true }); }
});

// Collapse into a tab at the top edge, and back.
function collapseToolbar() {
  closePopovers();
  pointerNear = false;
  toolbar.hidden = true;
  notch.hidden = false;
}
function showToolbar() {
  autoHidden = false;
  notch.hidden = true;
  toolbar.hidden = false;
  clampToolbar();
  scheduleAutoHide();
}
$("btn-collapse").addEventListener("click", () => {
  clearTimeout(autoHideTimer);
  autoHidden = false;
  collapseToolbar();
  stage.focus({ preventScroll: true });
});
notch.addEventListener("click", showToolbar);

// Drag the toolbar by its grip; double-click the grip to put it back at top center.
const grip = $("grip");
let drag = null;
grip.addEventListener("pointerdown", (e) => {
  if (e.button !== 0) return;
  e.preventDefault();
  const t = toolbar.getBoundingClientRect();
  drag = { dx: e.clientX - t.left, dy: e.clientY - t.top };
  grip.setPointerCapture(e.pointerId);
  toolbar.classList.add("dragging");
});
grip.addEventListener("pointermove", (e) => {
  if (!drag) return;
  const s = stage.getBoundingClientRect();
  placeToolbar(e.clientX - drag.dx - s.left, e.clientY - drag.dy - s.top);
});
const endDrag = () => { drag = null; toolbar.classList.remove("dragging"); };
grip.addEventListener("pointerup", endDrag);
grip.addEventListener("pointercancel", endDrag);
grip.addEventListener("dblclick", () => {
  toolbar.classList.remove("moved");
  toolbar.style.left = toolbar.style.top = "";
});

// Keeps the bar (not its popovers) inside the stage.
function placeToolbar(x, y) {
  const s = stage.getBoundingClientRect();
  const b = toolbar.querySelector(".bar").getBoundingClientRect();
  const tw = toolbar.getBoundingClientRect().width;
  const barLeft = (tw - b.width) / 2;
  x = Math.min(Math.max(x, -barLeft), s.width - barLeft - b.width);
  y = Math.min(Math.max(y, 0), s.height - b.height);
  toolbar.classList.add("moved");
  toolbar.style.left = `${x}px`;
  toolbar.style.top = `${y}px`;
}
function clampToolbar() {
  if (!toolbar.classList.contains("moved") || toolbar.hidden) return;
  placeToolbar(parseFloat(toolbar.style.left) || 0, parseFloat(toolbar.style.top) || 0);
}
window.addEventListener("resize", clampToolbar);
document.addEventListener("fullscreenchange", () => requestAnimationFrame(clampToolbar));

// ---------------------------------------------------------------- auth

function showLogin(message = "") {
  $("login-error").textContent = message;
  closePopovers();
  if (!$("login").open) $("login").showModal();
  $("login-password").focus();
}

// The host's access password is the only credential: there is no user name.
$("login-form").addEventListener("submit", async (e) => {
  e.preventDefault();
  const password = $("login-password").value;
  const go = e.target.querySelector(".login-go");
  $("login-error").textContent = "";
  $("login-form").querySelector(".pw-field").classList.remove("bad");
  go.disabled = true;
  try {
    await api("/api/login", { password });
    $("login-password").value = "";
    $("login").close();
    await start();
  } catch (err) {
    $("login-error").textContent = err.status === 401 ? "Wrong password" : err.message;
    $("login-form").querySelector(".pw-field").classList.add("bad");
    $("login-password").select();
  } finally {
    go.disabled = false;
  }
});

$("login-show").addEventListener("click", () => {
  const field = $("login-password");
  const show = field.type === "password";
  field.type = show ? "text" : "password";
  $("login-show").setAttribute("aria-pressed", String(show));
  $("login-show").title = $("login-show").ariaLabel = show ? "Hide password" : "Show password";
  field.focus();
});
// Login is mandatory: don't let Esc dismiss the dialog.
$("login").addEventListener("cancel", (e) => e.preventDefault());

// ---------------------------------------------------------------- WebRTC

function waitIceComplete(peer, timeoutMs = 3000) {
  if (peer.iceGatheringState === "complete") return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => { clearTimeout(t); peer.removeEventListener("icegatheringstatechange", check); resolve(); };
    const check = () => { if (peer.iceGatheringState === "complete") done(); };
    const t = setTimeout(done, timeoutMs);
    peer.addEventListener("icegatheringstatechange", check);
  });
}

async function connect() {
  clearTimeout(slotRetryTimer);
  disconnect();
  connectedAt = 0;
  setStatus("connecting…", "connecting");
  pc = new RTCPeerConnection({ iceServers: (info.ice_servers || []).map((u) => ({ urls: u })) });
  const transceiver = pc.addTransceiver("video", { direction: "recvonly" });
  // Ask for minimal buffering: this is an interactive stream.
  try { transceiver.receiver.jitterBufferTarget = 0; } catch (_) {}
  try { transceiver.receiver.playoutDelayHint = 0; } catch (_) {}

  dc = pc.createDataChannel("control", { ordered: true });
  dc.onopen = () => { connectedAt = Date.now(); setStatus("connected", "connected"); sendStreamSettings(); };
  dc.onmessage = (ev) => onHostMessage(ev.data);
  // The host ended the session (or the channel died): close everything, see above, and say why.
  dc.onclose = () => { const reason = endReason; disconnect(); sessionEnded(reason); };
  openFilesChannel();

  pc.ontrack = (ev) => {
    video.srcObject = ev.streams[0] || new MediaStream([ev.track]);
    video.play().catch(() => {});
  };
  pc.onconnectionstatechange = () => {
    const s = pc && pc.connectionState;
    // Close a dead connection for good: left open, the browser keeps sending to the host's
    // media port, which breaks the next viewer behind a single-peer forward (QEMU user net).
    if (s === "failed") {
      const reason = endReason;
      const wasConnected = connectedAt > 0;
      disconnect();
      if (wasConnected) sessionEnded(reason);
      else setStatus("connection failed", "error");
    }
    // The network dropped out; WebRTC recovers by itself if it comes back soon.
    else if (s === "disconnected") setStatus("network lost: reconnecting…", "connecting");
    else if (s === "connected" && dc && dc.readyState === "open") setStatus("connected", "connected");
    else if (s === "closed") setStatus("disconnected", "idle");
  };

  // A second connect() replaces `pc` while this one awaits; never apply this offer's
  // answer to that newer connection, which would fail with a confusing SDP error.
  const peer = pc;
  try {
    await peer.setLocalDescription(await peer.createOffer());
    await waitIceComplete(peer);
    const answer = await api("/api/offer", { sdp: peer.localDescription.sdp });
    if (pc !== peer) { peer.close(); return; }
    await peer.setRemoteDescription({ type: "answer", sdp: answer.sdp });
    slotRetries = 0;
    statsTimer = setInterval(updateStats, 1000);
    lockTimer = setInterval(pollLocked, 1000);
  } catch (err) {
    if (pc !== peer) return;
    if (err.status === 401) { disconnect(); showLogin("Your session expired. Sign in again."); return; }
    // Viewer slots can still be held by a connection that is closing; they free up within
    // about 20 s, so wait and try again instead of showing a dead end.
    if (err.status === 400 && /too many viewers/i.test(err.message) && slotRetries < 6) {
      slotRetries++;
      setStatus(`waiting for a free viewer slot… (${slotRetries}/6)`, "connecting");
      if (pc) { pc.close(); pc = null; }
      slotRetryTimer = setTimeout(connect, 5000);
      return;
    }
    setStatus(`error: ${err.message}`, "error");
  }
}

function disconnect() {
  clearTimeout(recoverTimer);
  recoverTimer = null;
  endReason = null;
  releaseAll();
  clearInterval(statsTimer);
  statsTimer = null;
  clearInterval(lockTimer);
  lockTimer = null;
  showLocked(false);
  if (dc) { dc.onclose = null; dc.onmessage = null; dc.close(); dc = null; }
  applied = null;
  resetClip();
  closeFilesChannel();
  if (pc) { pc.onconnectionstatechange = null; pc.close(); pc = null; }
  video.srcObject = null;
  resetStats();
  setStatus("disconnected", "idle");
}

// ---------------------------------------------------------------- why the session ended

// After the session ended (`reason` is what the host said, if anything): tell the viewer why,
// and reconnect by itself when the host or the network comes back.
function sessionEnded(reason) {
  if (reason === "password_changed") {
    setStatus("signed out: the password was changed", "idle");
    showLogin("The password was changed on the host. Sign in again.");
    return;
  }
  if (reason === "host_disconnected") {
    setStatus("the person at the host disconnected everyone", "error");
    return;
  }
  if (reason === "signed_out" || reason === "session_expired") {
    setStatus("signed out", "idle");
    showLogin(reason === "signed_out" ? "You signed out of this host. Sign in again." : "Your sign-in expired. Sign in again.");
    return;
  }
  if (reason === "web_off") {
    setStatus("remote access was turned off on the host", "error");
    return;
  }
  // Without a reason the stream or the network broke. Don't keep reconnecting to a host that
  // drops every new stream at once.
  quickDrops = Date.now() - connectedAt < 30000 ? quickDrops + 1 : 0;
  connectedAt = 0;
  if (quickDrops >= 3) {
    quickDrops = 0;
    setStatus("the connection to the host keeps dropping", "error");
    return;
  }
  recoverTries = 0;
  recover(reason === "host_shutdown" ? "the host is restarting: reconnecting…" : "connection lost: reconnecting…", reason);
}

// Waits for the host to answer again (about two minutes at most), then connects. A host that
// answers but no longer knows this browser (it restarted, or the login expired) asks to sign in.
const RECOVER_TRIES = 40;
function recover(text, reason) {
  clearTimeout(recoverTimer);
  setStatus(text, "connecting");
  recoverTimer = setTimeout(async () => {
    recoverTimer = null;
    try {
      info = await api("/api/info");
    } catch (err) {
      if (state !== "connecting") return; // the viewer pressed Disconnect meanwhile
      if (err.status === 401) {
        setStatus("signed out", "idle");
        showLogin(reason === "host_shutdown" ? "The host restarted. Sign in again." : "Your session ended on the host. Sign in again.");
        return;
      }
      if (++recoverTries < RECOVER_TRIES) { recover(text, reason); return; }
      setStatus("the host can't be reached", "error");
      return;
    }
    if (state === "connecting") connect();
  }, recoverTries ? 3000 : 500);
}

// ---------------------------------------------------------------- stats

const ROUTES = { host: "Direct (LAN)", srflx: "Direct (NAT)", prflx: "Direct (NAT)", relay: "Relayed (TURN)" };
let last = { bytes: 0, ts: 0, lost: 0, recv: 0 };

function resetStats() {
  last = { bytes: 0, ts: 0, lost: 0, recv: 0 };
  lastRtt = null;
  for (const id of ["s-res", "s-fps", "s-bitrate", "s-rtt", "s-loss", "s-codec", "s-route"]) {
    $(id).textContent = "–";
    $(id).className = "";
  }
  $("quality").hidden = true;
}

async function updateStats() {
  if (!pc) return;
  const report = await pc.getStats();
  let inbound = null, pair = null, selectedPairId = null;
  report.forEach((s) => {
    if (s.type === "inbound-rtp" && s.kind === "video") inbound = s;
    if (s.type === "transport" && s.selectedCandidatePairId) selectedPairId = s.selectedCandidatePairId;
  });
  if (selectedPairId) pair = report.get(selectedPairId);
  if (!pair) report.forEach((s) => { if (s.type === "candidate-pair" && s.state === "succeeded" && s.nominated) pair = s; });

  const rtt = pair && pair.currentRoundTripTime !== undefined ? pair.currentRoundTripTime * 1000 : null;
  lastRtt = rtt;
  let lossPct = null;
  if (inbound) {
    const { framesPerSecond: fps = 0, bytesReceived: bytes = 0, timestamp: ts, frameWidth: w = 0, frameHeight: h = 0 } = inbound;
    const lost = inbound.packetsLost || 0, recv = inbound.packetsReceived || 0;
    const kbps = last.ts && ts > last.ts ? ((bytes - last.bytes) * 8) / (ts - last.ts) : 0;
    const dLost = Math.max(0, lost - last.lost), dRecv = Math.max(0, recv - last.recv);
    if (last.ts) lossPct = dLost + dRecv > 0 ? (dLost * 100) / (dLost + dRecv) : 0;
    last = { bytes, ts, lost, recv };

    $("s-res").textContent = w && h ? `${w}×${h}` : "–";
    $("s-fps").textContent = `${fps.toFixed(0)} fps`;
    $("s-bitrate").textContent = `${(kbps / 1000).toFixed(1)} Mbit/s`;
    if (lossPct !== null) {
      $("s-loss").innerHTML = `${lossPct.toFixed(1)}% <small>(${lost} total)</small>`;
      $("s-loss").className = lossPct < 1 ? "good" : "";
    }
    const codec = inbound.codecId && report.get(inbound.codecId);
    if (codec && codec.mimeType) $("s-codec").textContent = codec.mimeType.replace(/^video\//i, "");
  }
  $("s-rtt").textContent = rtt !== null ? `${rtt.toFixed(0)} ms` : "–";
  $("s-rtt").className = rtt !== null && rtt < 50 ? "good" : "";
  const remote = pair && pair.remoteCandidateId && report.get(pair.remoteCandidateId);
  const local = pair && pair.localCandidateId && report.get(pair.localCandidateId);
  const types = [remote, local].filter(Boolean).map((c) => c.candidateType);
  const route = types.includes("relay") ? "relay" : types.find((t) => t !== "host") || types[0];
  $("s-route").textContent = ROUTES[route] || "–";
  $("s-route").className = "route";

  // A rough verdict from the measured round trip and packet loss.
  if (rtt !== null) {
    const loss = lossPct || 0;
    const q = rtt < 50 && loss < 1 ? "good" : rtt < 150 && loss < 5 ? "fair" : "poor";
    $("quality").textContent = { good: "Good", fair: "Fair", poor: "Poor" }[q];
    $("quality").className = `badge ${q}`;
    $("quality").hidden = false;
  }
  if (state === "connected") setStatus(statusText, state);
}

// ---------------------------------------------------------------- session controls

$("btn-connect").addEventListener("click", () => { slotRetries = 0; connect(); });
$("btn-disconnect").addEventListener("click", () => { clearTimeout(slotRetryTimer); slotRetries = 0; quickDrops = 0; disconnect(); });
const KEY_FOLDERS = [
  {
    os: "windows",
    label: "Windows",
    items: [
      { label: "Ctrl + Alt + Del", kbd: "Secure", action: "secure_attention", title: "Send Ctrl+Alt+Del to the host (Windows sign-in and lock screen)", pinned: true },
      { label: "Lock", kbd: "Win + L", combo: "MetaLeft KeyL", title: "Lock the host (Win + L)", pinned: true },
      { label: "Windows key", kbd: "Win", combo: "MetaLeft", title: "Press the Windows key", pinned: true },
      { label: "Alt + Tab", kbd: "Switch", combo: "AltLeft Tab", title: "Switch windows (Alt + Tab)", pinned: true },
      { label: "Task Manager", kbd: "Ctrl+Shift+Esc", combo: "ControlLeft ShiftLeft Escape", title: "Open Task Manager (Ctrl + Shift + Esc)", pinned: true },
      { label: "Task View", kbd: "Win + Tab", combo: "MetaLeft Tab", title: "Open Task View (Win + Tab)" },
      { label: "Run", kbd: "Win + R", combo: "MetaLeft KeyR", title: "Open Run dialog (Win + R)" },
      { label: "Show Desktop", kbd: "Win + D", combo: "MetaLeft KeyD", title: "Toggle desktop (Win + D)" },
    ],
  },
  {
    os: "macos",
    label: "macOS",
    items: [
      { label: "Lock Screen", kbd: "⌃ + ⌘ + Q", combo: "ControlLeft MetaLeft KeyQ", title: "Lock screen (Control + Command + Q)", pinned: true },
      { label: "Command key", kbd: "⌘", combo: "MetaLeft", title: "Press the Command key", pinned: true },
      { label: "App Switcher", kbd: "⌘ + Tab", combo: "MetaLeft Tab", title: "Switch applications (Command + Tab)", pinned: true },
      { label: "Spotlight", kbd: "⌘ + Space", combo: "MetaLeft Space", title: "Open Spotlight Search (Command + Space)", pinned: true },
      { label: "Force Quit", kbd: "⌘ + ⌥ + Esc", combo: "MetaLeft AltLeft Escape", title: "Force Quit Applications (Command + Option + Esc)", pinned: true },
      { label: "Option key", kbd: "⌥", combo: "AltLeft", title: "Press the Option key" },
      { label: "Control key", kbd: "⌃", combo: "ControlLeft", title: "Press the Control key" },
      { label: "Mission Control", kbd: "⌃ + ↑", combo: "ControlLeft ArrowUp", title: "Open Mission Control (Control + Up)" },
      { label: "Quit App", kbd: "⌘ + Q", combo: "MetaLeft KeyQ", title: "Quit active application (Command + Q)" },
      { label: "Close Window", kbd: "⌘ + W", combo: "MetaLeft KeyW", title: "Close active window (Command + W)" },
      { label: "Screenshot", kbd: "⌘ + ⇧ + 4", combo: "MetaLeft ShiftLeft Digit4", title: "Capture screenshot selection (Command + Shift + 4)" },
    ],
  },
  {
    os: "linux",
    label: "Linux",
    items: [
      { label: "Ctrl + Alt + Del", kbd: "System", action: "secure_attention", title: "Send Ctrl+Alt+Del to the host (GNOME: log out / power off dialog)", pinned: true },
      { label: "Lock", kbd: "Super + L", combo: "MetaLeft KeyL", title: "Lock the host (Super + L)", pinned: true },
      { label: "Super key", kbd: "Super", combo: "MetaLeft", title: "Press the Super key", pinned: true },
      { label: "Alt + Tab", kbd: "Switch", combo: "AltLeft Tab", title: "Switch windows (Alt + Tab)", pinned: true },
      { label: "Terminal", kbd: "Ctrl+Alt+T", combo: "ControlLeft AltLeft KeyT", title: "Open terminal (Ctrl + Alt + T)", pinned: true },
      { label: "Run Command", kbd: "Alt + F2", combo: "AltLeft F2", title: "Open Run Command dialog (Alt + F2)" },
      { label: "Workspace Left", kbd: "Ctrl+Alt+←", combo: "ControlLeft AltLeft ArrowLeft", title: "Switch to previous workspace (Ctrl + Alt + Left)" },
      { label: "Workspace Right", kbd: "Ctrl+Alt+→", combo: "ControlLeft AltLeft ArrowRight", title: "Switch to next workspace (Ctrl + Alt + Right)" },
    ],
  },
];

function createKeyButton(item) {
  const b = document.createElement("button");
  if (item.action) b.dataset.action = item.action;
  if (item.combo) b.dataset.combo = item.combo;
  if (item.title) b.title = item.title;
  const label = document.createElement("span");
  label.textContent = item.label;
  const kbd = document.createElement("kbd");
  kbd.textContent = item.kbd;
  b.append(label, kbd);
  return b;
}

function updateKeysCapabilities() {
  const cadSupported = !!info && ["windows", "linux"].includes(info.os);
  document.querySelectorAll('#pop-keys [data-action="secure_attention"]').forEach((b) => {
    b.disabled = !cadSupported;
    if (!cadSupported && info) b.title = "Ctrl+Alt+Del is only supported on Windows and Linux hosts";
  });
}

function renderKeysMenu() {
  const list = $("keys-list");
  list.replaceChildren();
  const hostOs = info?.os;
  const activeFolder = KEY_FOLDERS.find((f) => f.os === hostOs) || KEY_FOLDERS[0];

  for (const item of activeFolder.items.filter((i) => i.pinned)) {
    list.append(createKeyButton(item));
  }
  list.append(createKeyButton({ label: "Esc", kbd: "Cancel", combo: "Escape", title: "Send Escape" }));

  const sep = document.createElement("div");
  sep.className = "key-sep";
  list.append(sep);

  for (const folder of KEY_FOLDERS) {
    const details = document.createElement("details");
    details.className = "key-folder";
    const summary = document.createElement("summary");
    const title = document.createElement("span");
    title.className = "key-folder-label";
    title.textContent = folder.label;
    summary.append(title);
    if (folder.os === hostOs) {
      const badge = document.createElement("span");
      badge.className = "key-badge";
      badge.textContent = "Current";
      summary.append(badge);
    }
    const body = document.createElement("div");
    body.className = "key-folder-body";
    for (const item of folder.items) body.append(createKeyButton(item));
    details.append(summary, body);
    list.append(details);
  }
  updateKeysCapabilities();
}

renderKeysMenu();

// Key combinations the local OS or browser would catch before reaching the host.
$("keys-list").addEventListener("click", (e) => {
  const b = e.target.closest("button");
  if (!b || b.disabled) return;
  closePopovers();
  if (b.dataset.action === "secure_attention") {
    if (!dc || dc.readyState !== "open") {
      setStatus("not connected: Ctrl+Alt+Del was not sent", "error");
      return;
    }
    send({ type: "secure_attention" });
    stage.focus({ preventScroll: true });
    return;
  }
  if (b.dataset.combo) {
    const codes = b.dataset.combo.split(" ");
    codes.forEach((code) => send({ type: "key", code, down: true }));
    codes.slice().reverse().forEach((code) => send({ type: "key", code, down: false }));
    stage.focus({ preventScroll: true });
  }
});
$("btn-fullscreen").addEventListener("click", async () => {
  if (document.fullscreenElement) { await document.exitFullscreen().catch(() => {}); return; }
  await stage.requestFullscreen().catch(() => {});
  // Lets Esc, Alt+Tab-like combos etc. reach the host where supported.
  if (navigator.keyboard && navigator.keyboard.lock) navigator.keyboard.lock().catch(() => {});
  stage.focus();
});
document.addEventListener("fullscreenchange", () => {
  const full = !!document.fullscreenElement;
  $("btn-fullscreen").title = full ? "Exit fullscreen" : "Fullscreen (keyboard goes to the remote host)";
  if (!full && navigator.keyboard && navigator.keyboard.unlock) navigator.keyboard.unlock();
  if (full && state === "connected") showHint(true);
});

// ---------------------------------------------------------------- input

let lastInputAt = Date.now(); // for "Prevent host sleep"
const INPUT_TYPES = new Set(["mouse_move", "mouse_button", "wheel", "key", "text"]);
function send(msg) {
  if (!dc || dc.readyState !== "open") return;
  dc.send(JSON.stringify(msg));
  if (INPUT_TYPES.has(msg.type)) lastInputAt = Date.now();
}

function releaseAll() {
  heldKeys.clear();
  heldMods.clear();
  textKeys.clear();
  send({ type: "release_all" });
}

// Maps a client position to 0..1 of the *video content*, accounting for the
// letterboxing introduced by object-fit: contain.
function normalize(clientX, clientY) {
  const r = video.getBoundingClientRect();
  const vw = video.videoWidth || r.width;
  const vh = video.videoHeight || r.height;
  const scale = Math.min(r.width / vw, r.height / vh);
  const cw = vw * scale, ch = vh * scale;
  const ox = r.left + (r.width - cw) / 2;
  const oy = r.top + (r.height - ch) / 2;
  const clamp = (v) => Math.min(1, Math.max(0, v));
  return { x: clamp((clientX - ox) / cw), y: clamp((clientY - oy) / ch) };
}

const BUTTONS = ["left", "middle", "right", "back", "forward"];

// Coalesce pointer moves to one message per animation frame.
let pendingMove = null;
function flushMove() {
  if (pendingMove) send({ type: "mouse_move", ...pendingMove });
  pendingMove = null;
}
video.addEventListener("pointermove", (e) => {
  if (!pendingMove) requestAnimationFrame(flushMove);
  pendingMove = normalize(e.clientX, e.clientY);
});
video.addEventListener("pointerdown", (e) => {
  e.preventDefault();
  stage.focus();
  wakeLockScreen();
  video.setPointerCapture(e.pointerId);
  const button = BUTTONS[e.button];
  if (!button) return;
  pendingMove = null;
  send({ type: "mouse_button", button, down: true, ...normalize(e.clientX, e.clientY) });
});
video.addEventListener("pointerup", (e) => {
  e.preventDefault();
  const button = BUTTONS[e.button];
  if (!button) return;
  flushMove();
  send({ type: "mouse_button", button, down: false, ...normalize(e.clientX, e.clientY) });
});
video.addEventListener("contextmenu", (e) => e.preventDefault());
// Clicks next to the picture (letterbox, the locked notice) still mean "control the host";
// clicks on the toolbar don't.
stage.addEventListener("pointerdown", (e) => {
  if (aiPanel.contains(e.target)) return;
  if (!toolbar.contains(e.target) && !notch.contains(e.target)) copyPendingHostText();
  if (e.target === video || toolbar.contains(e.target) || notch.contains(e.target)) return;
  stage.focus({ preventScroll: true });
  wakeLockScreen();
});
video.addEventListener("wheel", (e) => {
  e.preventDefault();
  // Normalize to "120 per notch" units: lines ≈ 40px, pages ≈ 800px.
  const k = e.deltaMode === 1 ? 40 : e.deltaMode === 2 ? 800 : 1;
  const dir = prefs.invertWheel ? -1 : 1;
  send({ type: "wheel", dx: e.deltaX * k * dir, dy: e.deltaY * k * dir });
}, { passive: false });

// Keyboard goes to the host only while the remote screen itself has focus, not the toolbar.
// Only keys pressed here are released here: a toolbar button that hands focus back on
// Enter must not leave the host a stray key-up.
const forHost = (e) => e.target === stage;
const heldKeys = new Set();
stage.addEventListener("keydown", (e) => {
  if (!forHost(e) || e.isComposing) return;
  copyPendingHostText();
  if (prefs.keyboard === "text" && textKeyDown(e)) return;
  if (!e.code) { typeKeyWithoutCode(e); return; }
  if (e.repeat && e.code.startsWith("Shift")) { e.preventDefault(); return; }
  heldKeys.add(e.code);
  // A paste shortcut: let the browser fire its paste event (that is how a page gets the
  // clipboard without asking), send that text to the host, then the key. See onPaste.
  if (!e.repeat && isPasteKey(e) && syncsClipboard()) { holdPasteKey(e.code); return; }
  e.preventDefault();
  send({ type: "key", code: e.code, down: true });
});
// Some keyboards (IME, dead keys, some mobile keyboards) send no physical code. There is no
// key to press on the host, so type the character itself when there is one.
function typeKeyWithoutCode(e) {
  if ([...e.key].length !== 1 || e.ctrlKey || e.metaKey) return;
  e.preventDefault();
  send({ type: "text", text: e.key });
}
stage.addEventListener("keyup", (e) => {
  if (!forHost(e)) return;
  if (textKeyUp(e)) { e.preventDefault(); return; }
  if (!heldKeys.delete(e.code)) return;
  e.preventDefault();
  if (pasteHold && pasteHold.code === e.code) { pasteHold.released = true; return; }
  send({ type: "key", code: e.code, down: false });
});
// "Direct text" keyboard mode (prefs.keyboard === "text"): a key that types a character in
// this computer's layout sends that character ({"type":"text"}), which the host types as
// Unicode whatever its own layout is. Everything else (Enter, arrows, F-keys, Space, the
// numpad, and any key with Ctrl, Alt or Win/Cmd held) goes as a physical key, as in
// Hardware mode. Modifiers wait until a physical key needs them, so Shift or AltGr
// held for a character never reaches the host; one pressed and released alone is sent then.
// Caps Lock stays here. The host types each character with a key of its own layout where it
// has one, and as a Unicode character where it hasn't.
const MODIFIERS = new Set(["ShiftLeft", "ShiftRight", "ControlLeft", "ControlRight", "AltLeft", "AltRight", "MetaLeft", "MetaRight"]);
const isMac = /Mac|iPhone|iPad/.test(navigator.platform || "");
const heldMods = new Map(); // code → { sent, used }
const textKeys = new Set(); // keys whose press was sent as text: their key-up is dropped

function typesText(e) {
  if ([...e.key].length !== 1 || e.code === "Space" || e.code.startsWith("Numpad")) return false;
  // AltGr (Ctrl+Alt on Windows) and the Mac's Option are how layouts reach more characters.
  const altGr = e.getModifierState("AltGraph");
  if (e.metaKey || (e.ctrlKey && !altGr)) return false;
  return !e.altKey || altGr || isMac;
}

function flushMods() {
  for (const [code, m] of heldMods) {
    if (!m.sent) { m.sent = true; send({ type: "key", code, down: true }); }
  }
}

// True when the key was handled here.
function textKeyDown(e) {
  if (MODIFIERS.has(e.code)) {
    e.preventDefault();
    if (!heldMods.has(e.code)) heldMods.set(e.code, { sent: false, used: false });
    return true;
  }
  // The first half of a dead-key accent: the next key brings the accented character.
  // Caps Lock already shows in the characters this computer types; on the host it would
  // flip them again.
  if (e.key === "Dead" || e.code === "CapsLock") { e.preventDefault(); textKeys.add(e.code); return true; }
  if (typesText(e)) {
    e.preventDefault();
    heldMods.forEach((m) => { m.used = true; });
    textKeys.add(e.code);
    send({ type: "text", text: e.key });
    return true;
  }
  flushMods();
  heldMods.forEach((m) => { m.used = true; });
  return false;
}

function textKeyUp(e) {
  if (textKeys.delete(e.code)) return true;
  const m = heldMods.get(e.code);
  if (!m) return false;
  heldMods.delete(e.code);
  if (m.sent) send({ type: "key", code: e.code, down: false });
  // Pressed and released alone (the Windows key, Alt for a menu): send it now.
  else if (!m.used) { send({ type: "key", code: e.code, down: true }); send({ type: "key", code: e.code, down: false }); }
  return true;
}

// Never leave keys stuck on the host when the browser loses focus.
stage.addEventListener("blur", releaseAll);
window.addEventListener("blur", releaseAll);
document.addEventListener("visibilitychange", () => { if (document.hidden) releaseAll(); });


// ---------------------------------------------------------------- clipboard
//
// Text only, both ways, over the control channel ({"type":"clipboard","text":..,"more":..},
// split into parts like the host does).
// * Host → here: the host sends its text whenever it changes. It goes onto this computer's
//   clipboard with the async Clipboard API (HTTPS or localhost only) or, failing that, a
//   copy command, which browsers allow only right after a click or key press: the text then
//   waits (amber dot) and is copied at the next click or key on the remote screen.
// * Here → host: Ctrl+V (Cmd+V, Shift+Insert) on the remote screen lets the browser's own
//   paste event deliver the local clipboard, which works on plain HTTP too and asks
//   nothing; that text is put on the host's clipboard before the key is sent. Where the
//   browser already allows reading the clipboard, it is also sent when this page gets focus,
//   so the host's own paste menus have it as well.

const CHUNK = 8 * 1024; // characters per message; matches the agent
const MAX_CLIP = 1024 * 1024; // bytes, the agent ignores bigger texts
const AUTO_KEY = "dalker.clipboard.auto";
const secureClipboard = window.isSecureContext && !!(navigator.clipboard && navigator.clipboard.writeText);
const clip = {
  available: null,   // null until the host says
  reason: "",
  auto: true,
  hostText: null,    // last text the host sent
  synced: null,      // text both sides are known to have
  pending: null,     // host text not on this computer's clipboard yet
  incoming: "",
  lastRead: null,
};
try { clip.auto = localStorage.getItem(AUTO_KEY) !== "off"; } catch (_) {}
$("clip-auto").checked = clip.auto;
$("clip-read").hidden = !(window.isSecureContext && navigator.clipboard && navigator.clipboard.readText);

const normalizeEol = (t) => t.replace(/\r\n?/g, "\n");
const syncsClipboard = () => clip.auto && clip.available === true && !!dc && dc.readyState === "open";

function onHostMessage(data) {
  let msg;
  try { msg = JSON.parse(data); } catch (_) { return; }
  if (msg.type === "clipboard_status") {
    clip.available = !!msg.available;
    clip.reason = msg.reason || "";
    renderClip();
  } else if (msg.type === "settings_applied") {
    applied = msg;
    // The host's limits can change while connected (Dalker Settings on the host). The choices
    // here are shares of them, so ask again for the same shares of the new limits.
    const h = hostStream();
    if (h && msg.max_fps && (h.max_fps !== msg.max_fps || h.max_bitrate_kbps !== msg.max_bitrate_kbps)) {
      Object.assign(h, { max_fps: msg.max_fps, max_bitrate_kbps: msg.max_bitrate_kbps, max_downscale: msg.max_downscale });
      sendStreamSettings();
    }
    renderSettings();
  } else if (msg.type === "session_end") {
    // The host is about to close the session; the channel closes next (see sessionEnded).
    endReason = msg.reason || "";
  } else if (msg.type === "clipboard") {
    clip.incoming += msg.text || "";
    if (msg.more) return;
    const text = normalizeEol(clip.incoming);
    clip.incoming = "";
    onHostText(text);
  }
}

function onHostText(text) {
  clip.hostText = text;
  if (document.activeElement !== $("clip-text")) $("clip-text").value = text;
  if (text === clip.synced || !clip.auto) { renderClip(); return; }
  clip.pending = text;
  renderClip();
  copyPendingHostText();
}

// Puts pending host text on this computer's clipboard, if the browser lets us right now.
function copyPendingHostText() {
  const text = clip.pending;
  if (text === null || !clip.auto) return;
  if (copyWithCommand(text)) { copied(text); return; }
  if (secureClipboard && document.hasFocus()) {
    navigator.clipboard.writeText(text).then(() => { if (clip.pending === text) copied(text); }, () => {});
  }
}

function copied(text) {
  clip.pending = null;
  clip.synced = text;
  renderClip(true);
}

// document.execCommand("copy") works on plain HTTP, but only shortly after a user gesture.
let copyText = null;
document.addEventListener("copy", (e) => {
  if (copyText === null) return;
  e.clipboardData.setData("text/plain", copyText);
  e.preventDefault();
});
function copyWithCommand(text) {
  if (navigator.userActivation && !navigator.userActivation.isActive) return false;
  copyText = text;
  let ok = false;
  try { ok = document.execCommand("copy"); } catch (_) {}
  copyText = null;
  return ok;
}

// Sends this computer's text to the host's clipboard. Returns false when it wasn't sent.
function sendClipboard(text) {
  text = normalizeEol(text);
  if (!dc || dc.readyState !== "open" || clip.available !== true) return false;
  if (new Blob([text]).size > MAX_CLIP) {
    flashNote("That text is too large to send (the limit is 1 MB).");
    return false;
  }
  const parts = Array.from(text);
  if (parts.length === 0) {
    send({ type: "clipboard", text: "", more: false });
  } else {
    for (let i = 0; i < parts.length; i += CHUNK) {
      send({ type: "clipboard", text: parts.slice(i, i + CHUNK).join(""), more: i + CHUNK < parts.length });
    }
  }
  clip.synced = text;
  clip.pending = null;
  renderClip(true);
  return true;
}

const isPasteKey = (e) =>
  (e.code === "KeyV" && (e.ctrlKey || e.metaKey) && !e.altKey) || (e.code === "Insert" && e.shiftKey);

// The paste key waits for the browser's paste event (it follows the keydown at once), so
// the host gets the text before the key. Browsers that fire none still get the key.
let pasteHold = null;
function holdPasteKey(code) {
  releasePasteKey();
  pasteHold = { code, released: false, timer: setTimeout(releasePasteKey, 250) };
}
function releasePasteKey() {
  if (!pasteHold) return;
  const { code, released, timer } = pasteHold;
  clearTimeout(timer);
  pasteHold = null;
  send({ type: "key", code, down: true });
  if (released) send({ type: "key", code, down: false });
}
document.addEventListener("paste", (e) => {
  if (!pasteHold) return;
  e.preventDefault();
  const text = e.clipboardData && e.clipboardData.getData("text/plain");
  if (text) { clip.lastRead = normalizeEol(text); sendClipboard(text); }
  releasePasteKey();
}, true);

// Coming back to the page (after copying something elsewhere): send the new text if the
// browser already allows reading the clipboard. Never asks for that permission itself.
async function readOnFocus() {
  copyPendingHostText();
  // Text the host copied and this computer hasn't got yet is newer: keep it.
  if (clip.pending !== null) return;
  if (!syncsClipboard() || !window.isSecureContext || !navigator.clipboard || !navigator.clipboard.readText) return;
  try {
    const perm = await navigator.permissions.query({ name: "clipboard-read" });
    if (perm.state !== "granted") return;
    const text = normalizeEol(await navigator.clipboard.readText());
    // Only what changed here since the last look: don't undo a copy made on the host.
    if (text && text !== clip.lastRead && text !== clip.synced) sendClipboard(text);
    clip.lastRead = text;
  } catch (_) { /* not allowed or not supported (Firefox): Ctrl+V still works */ }
}
window.addEventListener("focus", readOnFocus);

function setClipAuto(on) {
  clip.auto = on;
  $("clip-auto").checked = $("set-clip").checked = on;
  try { localStorage.setItem(AUTO_KEY, clip.auto ? "on" : "off"); } catch (_) {}
  if (!clip.auto) clip.pending = null;
  renderClip();
}
$("clip-auto").addEventListener("change", (e) => setClipAuto(e.target.checked));
$("clip-send").addEventListener("click", () => {
  if (sendClipboard($("clip-text").value)) flashNote("Sent. Paste on the host as usual.");
});
$("clip-copy").addEventListener("click", async () => {
  const text = $("clip-text").value;
  let ok = copyWithCommand(text);
  if (!ok && secureClipboard) ok = await navigator.clipboard.writeText(text).then(() => true, () => false);
  if (ok) { if (text === clip.pending) clip.pending = null; clip.synced = text; renderClip(true); flashNote("Copied to this computer."); }
  else { $("clip-text").select(); flashNote("The browser didn't allow copying. Press Ctrl+C to copy the selected text."); }
});
$("clip-read").addEventListener("click", async () => {
  try {
    const text = await navigator.clipboard.readText();
    $("clip-text").value = text;
    clip.lastRead = normalizeEol(text);
    if (sendClipboard(text)) flashNote("Sent your clipboard to the host.");
  } catch (_) {
    flashNote("The browser didn't allow reading the clipboard. Paste into the box with Ctrl+V instead.");
  }
});

let noteTimer = null;
function flashNote(text) {
  clearTimeout(noteTimer);
  $("clip-note").textContent = text;
  noteTimer = setTimeout(() => { noteTimer = null; renderClip(); }, 4000);
}

function renderClip(flash = false) {
  const btn = $("btn-clip");
  let cls, badge, title, note;
  if (clip.available === false) {
    cls = "unavailable"; badge = "Unavailable";
    title = "Clipboard: not available on this host";
    note = `The host's clipboard can't be shared${clip.reason ? `: ${clip.reason}` : "."}`;
  } else if (clip.available === null) {
    cls = "off"; badge = "…"; title = "Clipboard"; note = "Waiting for the host…";
  } else if (!clip.auto) {
    cls = "off"; badge = "Off"; title = "Clipboard: sync is off";
    note = "Automatic sync is off. Move text by hand with the box below.";
  } else if (clip.pending !== null) {
    cls = "pending"; badge = "Waiting"; title = "Clipboard: the host copied text; click the screen to copy it here";
    note = "The host copied text. Click the remote screen or press a key there to put it on this computer's clipboard, or use Copy here.";
  } else {
    cls = "synced"; badge = "Synced"; title = "Clipboard: synced with the host";
    note = "Copy on either side and paste on the other. Ctrl+V on the remote screen sends your text to the host first.";
    if (!window.isSecureContext) {
      note += " This page isn't HTTPS, so text the host copies lands here after your next click or key on the remote screen.";
    }
  }
  btn.className = `icon clip session-only ${cls}`;
  btn.title = title;
  $("clip-badge").textContent = badge;
  $("clip-badge").className = `badge ${{ synced: "good", pending: "fair", unavailable: "poor", off: "off" }[cls]}`;
  if (!noteTimer) $("clip-note").textContent = note;
  const live = clip.available === true;
  $("clip-send").disabled = !live;
  $("clip-read").disabled = !live;
  if (flash) {
    btn.classList.remove("flash");
    void btn.offsetWidth;
    btn.classList.add("flash");
  }
}

function resetClip() {
  if (pasteHold) { clearTimeout(pasteHold.timer); pasteHold = null; }
  Object.assign(clip, { available: null, reason: "", hostText: null, synced: null, pending: null, incoming: "" });
  renderClip();
}
renderClip();

// ---------------------------------------------------------------- settings
//
// Stream settings go to the host for this viewer only ({"type":"settings",...}); the host
// caps them at its own limits (info.stream) and answers with what it applied. They are
// relative to those limits (a share of the host's bitrate, a divisor of its resolution), so
// one saved choice suits every host. The view, keyboard mode and clipboard sync only change
// this browser.

const SETTINGS_KEY = "dalker.settings";
const DEFAULTS = { fps: 0, bw: 1, scale: 1, view: "fit", keyboard: "hardware", invertWheel: false, keepAlive: false, keepAliveSec: 60, autoHide: false }; // fps 0 = the host's maximum
const AWAKE_CHOICES = [30, 60, 120, 300];
let prefs = { ...DEFAULTS };
let applied = null; // the host's last settings_applied
try { Object.assign(prefs, JSON.parse(localStorage.getItem(SETTINGS_KEY) || "{}")); } catch (_) {}

function savePrefs() {
  try { localStorage.setItem(SETTINGS_KEY, JSON.stringify(prefs)); } catch (_) {}
}

const hostStream = () => (info && info.stream) || null;
const mbit = (kbps) => `${+(kbps / 1000).toFixed(kbps < 1000 ? 2 : 1)}`;

function fpsChoices(max) {
  const list = [10, 15, 30, 60, 120].filter((v) => v < max);
  return [...list.slice(-3), max];
}

function streamRequest() {
  const h = hostStream();
  if (!h) return null;
  const req = { type: "settings" };
  if (prefs.fps) req.fps = Math.min(prefs.fps, h.max_fps);
  if (prefs.bw > 1) req.bitrate_kbps = Math.round(h.max_bitrate_kbps / prefs.bw);
  if (prefs.scale > 1) req.downscale = Math.min(prefs.scale, h.max_downscale || 1);
  return req;
}

function sendStreamSettings() {
  const req = streamRequest();
  if (req) send(req);
}

// A segmented control: options are [value, label, title?].
function renderSeg(id, options, current, onPick) {
  $(id).replaceChildren(...options.map(([value, label, title]) => {
    const b = document.createElement("button");
    b.type = "button";
    b.setAttribute("role", "radio");
    b.setAttribute("aria-checked", String(value === current));
    b.textContent = label;
    if (title) b.title = title;
    b.addEventListener("click", () => { onPick(value); savePrefs(); renderSettings(); });
    return b;
  }));
}

function renderSettings() {
  const h = hostStream();
  $("set-stream").hidden = !h;
  $("set-old").hidden = !!h;
  if (h) {
    const max = h.max_fps;
    const fps = prefs.fps && prefs.fps < max ? prefs.fps : max;
    $("set-fps-max").textContent = `host max ${max}`;
    renderSeg("set-fps", fpsChoices(max).map((v) => [v, `${v}`, `${v} frames per second`]), fps,
      (v) => { prefs.fps = v >= max ? 0 : v; sendStreamSettings(); });
    renderSeg("set-bw", [1, 2, 4, 8].map((d) => [d, mbit(h.max_bitrate_kbps / d), `Up to ${mbit(h.max_bitrate_kbps / d)} Mbit/s`]), prefs.bw,
      (v) => { prefs.bw = v; sendStreamSettings(); });
    const scales = [1, 2, 3].filter((d) => d <= (h.max_downscale || 1));
    renderSeg("set-scale", scales.map((d) => [d, `${Math.round(100 / d)}%`, d === 1 ? "As the host sends it" : `1/${d} of the width and height`]),
      Math.min(prefs.scale, scales[scales.length - 1]), (v) => { prefs.scale = v; sendStreamSettings(); });
  }
  renderSeg("set-view", [["fit", "Fit window"], ["actual", "1:1 pixels", "One stream pixel per screen pixel; scroll with the scrollbars"]], prefs.view,
    (v) => { prefs.view = v; applyView(); });
  renderKeyboardMode("set-kbd");
  $("set-clip").checked = clip.auto;
  $("set-wheel").checked = prefs.invertWheel;
  $("set-awake").checked = prefs.keepAlive;
  $("set-autohide").checked = prefs.autoHide;
  $("set-awake-row").hidden = !prefs.keepAlive;
  renderSeg("set-awake-sec", AWAKE_CHOICES.map((v) => [v, v < 60 ? `${v}s` : `${v / 60} min`, `Press F15 on the host after ${v} seconds without input`]),
    prefs.keepAliveSec, (v) => { prefs.keepAliveSec = v; });

  const live = !!dc && dc.readyState === "open";
  const badge = $("set-badge");
  if (live && applied) {
    $("set-applied").textContent = `The host sends up to ${applied.fps} fps at up to ${mbit(applied.bitrate_kbps)} Mbit/s` +
      (applied.downscale > 1 ? `, at 1/${applied.downscale} resolution.` : ".");
    badge.textContent = "Applied"; badge.className = "badge good"; badge.hidden = false;
  } else {
    $("set-applied").textContent = live || !h ? "" : "Stream settings apply when connected.";
    badge.hidden = true;
  }
}

// "1:1 pixels": one stream pixel per device pixel, scrolling when larger than the window.
function applyView() {
  const actual = prefs.view === "actual" && video.videoWidth > 0;
  stage.classList.toggle("actual", actual);
  const dpr = window.devicePixelRatio || 1;
  video.style.width = actual ? `${video.videoWidth / dpr}px` : "";
  video.style.height = actual ? `${video.videoHeight / dpr}px` : "";
}
video.addEventListener("resize", applyView);
window.addEventListener("resize", applyView);

$("set-clip").addEventListener("change", (e) => setClipAuto(e.target.checked));
$("set-wheel").addEventListener("change", (e) => { prefs.invertWheel = e.target.checked; savePrefs(); });
$("set-awake").addEventListener("change", (e) => { prefs.keepAlive = e.target.checked; savePrefs(); renderSettings(); });
$("set-autohide").addEventListener("change", (e) => { prefs.autoHide = e.target.checked; savePrefs(); applyAutoHide(); });

// The keyboard mode is in Settings and in the keys menu.
function renderKeyboardMode(id) {
  renderSeg(id, [
    ["hardware", "Hardware", "Keys by position: the host's keyboard layout decides the characters. Best for shortcuts and games"],
    ["text", "Direct text", "Types the characters of this computer's keyboard layout, whatever the host's layout is. Shortcuts still go by key position"],
  ], prefs.keyboard, (v) => { prefs.keyboard = v; releaseAll(); renderKeyboardMode("keys-kbd"); });
}
renderKeyboardMode("keys-kbd");

// "Prevent host sleep": after keepAliveSec without input from here, press F15 on the host,
// a key apps leave alone but that counts as activity, so the host doesn't lock or sleep.
setInterval(() => {
  if (!prefs.keepAlive || !dc || dc.readyState !== "open") return;
  if (Date.now() - lastInputAt < prefs.keepAliveSec * 1000 - 500) return;
  send({ type: "key", code: "F15", down: true });
  send({ type: "key", code: "F15", down: false });
}, 5000);
$("set-reset").addEventListener("click", () => {
  prefs = { ...DEFAULTS };
  savePrefs();
  applyView();
  sendStreamSettings();
  setClipAuto(true);
  applyAutoHide();
  renderSettings();
  renderKeyboardMode("keys-kbd");
});
$("set-done").addEventListener("click", () => { closePopover("pop-settings"); stage.focus({ preventScroll: true }); });

// "Auto-hide the toolbar": while connected, the toolbar folds into the notch after a few
// seconds without the pointer near it, and comes back when the pointer reaches the top edge of
// the screen (or leaves the page through it) or on a tap of the notch. It stays while a menu is
// open, while it has keyboard focus and while it is being dragged. Folding it away by hand
// keeps it folded until the notch is clicked.

const AUTO_HIDE_MS = 3000;
const NEAR_PX = 48; // how close counts as "near" the bar
const EDGE_PX = 6; // the strip along the top edge that brings it back
let autoHidden = false;
let autoHideTimer = 0;
let pointerNear = false;

function toolbarBusy() {
  return pointerNear || !!drag || toolbar.matches(":hover") || toolbar.contains(document.activeElement) ||
    Object.keys(POPOVERS).some((id) => !$(id).hidden);
}

function scheduleAutoHide() {
  clearTimeout(autoHideTimer);
  if (!prefs.autoHide || state !== "connected" || toolbar.hidden) return;
  autoHideTimer = setTimeout(() => {
    if (toolbarBusy()) { scheduleAutoHide(); return; }
    autoHidden = true;
    collapseToolbar();
  }, AUTO_HIDE_MS);
}

// Called when the setting or the connection changes.
function applyAutoHide() {
  if (prefs.autoHide && state === "connected") { scheduleAutoHide(); return; }
  clearTimeout(autoHideTimer);
  if (autoHidden) showToolbar();
}

function nearBar(x, y) {
  const r = toolbar.querySelector(".bar").getBoundingClientRect();
  const dx = Math.max(r.left - x, 0, x - r.right);
  const dy = Math.max(r.top - y, 0, y - r.bottom);
  return Math.hypot(dx, dy) <= NEAR_PX;
}

document.addEventListener("pointermove", (e) => {
  if (!prefs.autoHide) return;
  // A finger on the screen is controlling the host, not looking for the toolbar.
  if (e.pointerType === "touch") { pointerNear = false; return; }
  if (autoHidden) {
    if (e.clientY - stage.getBoundingClientRect().top <= EDGE_PX) showToolbar();
    return;
  }
  if (toolbar.hidden) return;
  pointerNear = nearBar(e.clientX, e.clientY);
  if (pointerNear) scheduleAutoHide();
}, true);
// A quick flick out of the window can skip the top strip; leaving through the top counts too.
document.documentElement.addEventListener("pointerleave", (e) => {
  pointerNear = false;
  if (prefs.autoHide && autoHidden && e.pointerType !== "touch" && e.clientY <= 0) showToolbar();
});
// Using the toolbar restarts the countdown.
toolbar.addEventListener("pointerdown", scheduleAutoHide);
toolbar.addEventListener("focusout", scheduleAutoHide);

// ---------------------------------------------------------------- file transfer
//
// A second DataChannel, "files" (protocol: the agent's files.rs). Uploads (drop files on
// the screen or in the panel, or pick them) go to the signed-in user's Downloads folder on
// the host under a free name, one file at a time. Downloads come from that user's home
// folder, browsed in the panel, and are saved through the browser when complete.

const FCHUNK = 60 * 1024;            // file bytes per message; matches the agent
const UP_WINDOW = 8 * 1024 * 1024;   // upload bytes sent but not yet written on the host
const BUF_HIGH = 1024 * 1024;        // channel buffer above which uploads wait
const READ_BLOCK = 16 * FCHUNK;      // bytes read from the local file at a time
let fdc = null;
const fileHost = { status: null, reason: "", user: "", home: "", downloads: "", cwd: "", listing: null, browsing: false, hidden: false };
let xfers = [];                      // newest first
let nextXferId = 1;
let xferTimer = null;
let wakers = [];

const fsend = (msg) => { if (fdc && fdc.readyState === "open") fdc.send(JSON.stringify(msg)); };
const filesReady = () => fileHost.status === "ready" && !!fdc && fdc.readyState === "open";
const activeXfers = () => xfers.filter((t) => t.state === "running" || t.state === "starting" || t.state === "queued");
const sep = () => (/^[A-Za-z]:\\/.test(fileHost.home) ? "\\" : "/");

function fmtBytes(n) {
  if (!(n >= 0)) return "–";
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1000 && i < u.length - 1) { n /= 1000; i++; }
  return `${n >= 100 || i === 0 ? n.toFixed(0) : n.toFixed(1)} ${u[i]}`;
}
function fmtTime(s) {
  if (!isFinite(s) || s < 0) return "";
  s = Math.ceil(s);
  if (s < 60) return `~${s}s left`;
  const m = Math.floor(s / 60);
  if (m < 60) return `~${m}m ${s % 60}s left`;
  return `~${Math.floor(m / 60)}h ${m % 60}m left`;
}

function openFilesChannel() {
  if (!info.files) return;
  fdc = pc.createDataChannel("files", { ordered: true });
  fdc.binaryType = "arraybuffer";
  fdc.bufferedAmountLowThreshold = BUF_HIGH / 2;
  fdc.onmessage = (ev) => (typeof ev.data === "string" ? onFilesText(ev.data) : onFilesData(ev.data));
  fdc.onbufferedamountlow = wakeUploads;
  fdc.onclose = () => { fdc = null; fileHost.status = null; failActive("The connection closed."); renderFiles(); };
}

function closeFilesChannel() {
  if (fdc) { fdc.onclose = null; fdc.onmessage = null; fdc.close(); fdc = null; }
  fileHost.status = null;
  fileHost.listing = null;
  failActive("Disconnected.");
  renderFiles();
}

function onFilesText(data) {
  let m;
  try { m = JSON.parse(data); } catch (_) { return; }
  const t = m.id !== undefined ? xfers.find((x) => x.id === m.id) : null;
  switch (m.op) {
    case "ready":
      Object.assign(fileHost, { status: "ready", reason: "", user: m.user || "", home: m.home || "", downloads: m.downloads || "" });
      if (fileHost.browsing) listDir(fileHost.cwd);
      startNextUpload();
      break;
    case "unavailable":
      fileHost.status = "unavailable";
      fileHost.reason = m.reason || "File transfer isn't available on the host.";
      break;
    case "list":
      if (m.path !== fileHost.cwd) return;
      fileHost.listing = m.error ? { error: m.error } : { entries: m.entries || [] };
      renderBrowser();
      return;
    case "accepted":
      if (!t || t.state !== "starting") return;
      t.state = "running";
      t.hostName = m.name;
      t.mark = { at: performance.now(), done: 0 };
      if (t.dir === "down") { t.size = m.size; t.parts = []; }
      else pumpUpload(t);
      break;
    case "progress":
      if (t && t.dir === "up") { t.done = Math.max(t.done, m.bytes); wakeUploads(); }
      break;
    case "done":
      if (!t || t.state !== "running") return;
      if (t.dir === "up") { t.done = t.size; t.hostName = m.name || t.hostName; }
      else saveDownload(t);
      finish(t, "done");
      break;
    case "error":
      if (t && (t.state === "running" || t.state === "starting")) finish(t, "error", m.error);
      break;
  }
  renderFiles();
}

function onFilesData(buf) {
  if (buf.byteLength < 4) return;
  const id = new DataView(buf).getUint32(0);
  const t = xfers.find((x) => x.id === id);
  if (!t || t.dir !== "down" || t.state !== "running" || !t.parts) return;
  t.parts.push(new Uint8Array(buf, 4));
  t.done += buf.byteLength - 4;
}

function saveDownload(t) {
  const url = URL.createObjectURL(new Blob(t.parts));
  t.parts = null;
  t.done = t.size;
  const a = document.createElement("a");
  a.href = url;
  a.download = t.hostName || t.name;
  document.body.appendChild(a);
  a.click();
  a.remove();
  setTimeout(() => URL.revokeObjectURL(url), 60000);
}

function finish(t, state, why = "") {
  t.state = state;
  t.error = why;
  t.paused = false;
  t.parts = null;
  wakeUploads();
  if (t.dir === "up") startNextUpload();
}

function failActive(why) {
  for (const t of xfers) {
    if (t.state === "running" || t.state === "starting" || t.state === "queued") finish(t, "error", why);
  }
}

// ---- uploads

function waitWake() { return new Promise((r) => { wakers.push(r); setTimeout(r, 250); }); }
function wakeUploads() { const w = wakers; wakers = []; w.forEach((f) => f()); }

function queueUploads(fileList) {
  for (const f of fileList) {
    xfers.unshift({ id: nextXferId++, dir: "up", name: f.name, file: f, size: f.size, done: 0, sent: 0, state: "queued", paused: false, speed: 0 });
  }
  startNextUpload();
  renderFiles();
}

// One upload at a time, oldest first.
function startNextUpload() {
  if (!filesReady() || xfers.some((t) => t.dir === "up" && (t.state === "running" || t.state === "starting"))) return;
  const next = xfers.filter((t) => t.dir === "up" && t.state === "queued").pop();
  if (!next) return;
  next.state = "starting";
  fsend({ op: "put", id: next.id, name: next.name, size: next.size });
}

async function pumpUpload(t) {
  if (t.pumping) return;
  t.pumping = true;
  let block = null; // { start, bytes }
  try {
    while (t.state === "running" && t.sent < t.size) {
      if (!fdc || fdc.readyState !== "open") return;
      if (t.paused || fdc.bufferedAmount > BUF_HIGH || t.sent - t.done > UP_WINDOW) { await waitWake(); continue; }
      if (!block || t.sent >= block.start + block.bytes.length) {
        const end = Math.min(t.sent + READ_BLOCK, t.size);
        try {
          block = { start: t.sent, bytes: new Uint8Array(await t.file.slice(t.sent, end).arrayBuffer()) };
        } catch (err) {
          fsend({ op: "cancel", id: t.id });
          finish(t, "error", `Can't read the file here: ${err.message || err}`);
          renderFiles();
          return;
        }
        continue;
      }
      const off = t.sent - block.start;
      const data = block.bytes.subarray(off, Math.min(off + FCHUNK, block.bytes.length));
      const msg = new Uint8Array(4 + data.length);
      new DataView(msg.buffer).setUint32(0, t.id);
      msg.set(data, 4);
      fdc.send(msg);
      t.sent += data.length;
    }
    if (t.state === "running" && t.sent >= t.size && !t.ended) {
      t.ended = true;
      fsend({ op: "end", id: t.id });
    }
  } finally {
    t.pumping = false;
  }
}

// ---- downloads

function startDownload(path, name, size) {
  if (!filesReady()) return;
  const t = { id: nextXferId++, dir: "down", name, path, size, done: 0, state: "starting", paused: false, speed: 0 };
  xfers.unshift(t);
  fsend({ op: "get", id: t.id, path });
  renderFiles();
}

// ---- controls

function pauseXfer(t) {
  if (t.state !== "running") return;
  t.paused = !t.paused;
  if (t.dir === "down") fsend({ op: t.paused ? "pause" : "resume", id: t.id });
  t.mark = { at: performance.now(), done: t.done };
  t.speed = 0;
  wakeUploads();
  if (t.dir === "up" && !t.paused) pumpUpload(t);
  renderFiles();
}

function cancelXfer(t) {
  if (t.state === "running" || t.state === "starting") fsend({ op: "cancel", id: t.id });
  if (t.state === "running" || t.state === "starting" || t.state === "queued") finish(t, "cancelled");
  renderFiles();
}

// ---- rendering

const ICONS = {
  pause: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><rect x="6" y="4" width="4" height="16"/><rect x="14" y="4" width="4" height="16"/></svg>',
  resume: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><polygon points="6 4 20 12 6 20 6 4"/></svg>',
  cancel: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>',
  folder: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2z"/></svg>',
  file: '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2"><path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><polyline points="14 2 14 8 20 8"/></svg>',
};

function xferMeta(t) {
  const where = t.dir === "up" ? "Uploading to host" : "Downloading here";
  switch (t.state) {
    case "queued": return [where.replace("Uploading", "Waiting to upload"), "queued"];
    case "starting": return [t.dir === "up" ? "Starting upload" : "Starting download", ""];
    case "done": return t.dir === "up"
      ? ["Saved on host", `Downloads${sep()}${t.hostName || t.name}`]
      : ["Downloaded", "saved by your browser"];
    case "cancelled": return ["Cancelled", ""];
    case "error": return ["Failed", t.error || ""];
  }
  if (t.paused) return ["Paused", ""];
  const rest = t.speed > 0 ? fmtTime((t.size - t.done) / t.speed) : "";
  return [where, [t.speed > 0 ? `${fmtBytes(t.speed)}/s` : "", rest].filter(Boolean).join(" • ")];
}

function renderXfers() {
  const list = $("xfer-list");
  const seen = new Set();
  for (const t of xfers) {
    seen.add(String(t.id));
    let el = list.querySelector(`[data-id="${t.id}"]`);
    if (!el) {
      el = document.createElement("div");
      el.dataset.id = t.id;
      el.innerHTML = `<div class="xfer-top"><span class="xdot"></span><span class="xname"></span><span class="xbtns"></span></div>
        <div class="xmeta"><span class="xdir"></span><span class="xinfo"></span></div>
        <div class="xprog"><span class="xbytes"></span><span class="pct"></span></div>
        <div class="xbar"><i></i></div>`;
      el.querySelector(".xname").textContent = t.name;
      el.querySelector(".xname").title = t.name;
    }
    const active = t.state === "running" || t.state === "starting" || t.state === "queued";
    el.className = `xfer ${t.dir} ${t.state}${t.paused ? " paused" : ""}`;
    const [dir, more] = xferMeta(t);
    el.querySelector(".xdir").textContent = dir;
    el.querySelector(".xinfo").textContent = more ? ` • ${more}` : "";
    el.querySelector(".xmeta").title = more || "";
    const pct = t.size > 0 ? Math.min(100, (100 * t.done) / t.size) : t.state === "done" ? 100 : 0;
    el.querySelector(".xbytes").textContent = `${fmtBytes(t.done)} / ${fmtBytes(t.size)}`;
    el.querySelector(".pct").textContent = `${Math.floor(pct)}%`;
    el.querySelector(".xbar i").style.width = `${pct}%`;
    // Rebuild the buttons only when they change, so a click in progress isn't lost.
    const want = active ? (t.state === "running" ? (t.paused ? "resume" : "pause") + " cancel" : "cancel") : "remove";
    const btns = el.querySelector(".xbtns");
    if (btns.dataset.want !== want) {
      btns.dataset.want = want;
      btns.innerHTML = want.split(" ").map((a) => {
        const title = { pause: "Pause", resume: "Resume", cancel: "Cancel", remove: "Remove from the list" }[a];
        return `<button type="button" class="${a === "remove" ? "cancel" : a}" data-act="${a}" title="${title}" aria-label="${title}">${ICONS[a === "remove" ? "cancel" : a]}</button>`;
      }).join("");
    }
    list.appendChild(el); // keeps the order (newest first)
  }
  for (const el of [...list.children]) if (!seen.has(el.dataset.id)) el.remove();
}

function renderFiles() {
  const active = activeXfers();
  $("btn-files").classList.toggle("active", active.length > 0);
  $("btn-files").title = active.length ? `File transfers (${active.length} active)` : "File transfers";
  const badge = $("files-badge");
  badge.hidden = active.length === 0;
  badge.textContent = `${active.length} active`;
  const note = $("files-note");
  const usable = filesReady();
  if (fileHost.status === "unavailable") { note.hidden = false; note.className = "note warn"; note.textContent = fileHost.reason; }
  else if (!usable && dc && dc.readyState === "open" && fileHost.status === null && info.files) { note.hidden = false; note.className = "note"; note.textContent = "Connecting to the host's files…"; }
  else note.hidden = true;
  $("dropzone").classList.toggle("disabled", !usable);
  $("drop-dest").textContent = fileHost.downloads ? `To ${fileHost.downloads} on the host` : "They go to the Downloads folder on the host";
  $("drop-overlay-dest").textContent = fileHost.downloads ? `Files go to ${fileHost.downloads}` : "Files go to the Downloads folder";
  $("fb-toggle").disabled = !usable;
  $("fb-toggle-label").textContent = fileHost.browsing ? "Hide host files" : "Get files from host";
  $("fb").hidden = !fileHost.browsing || !usable;
  $("xfer-clear").hidden = !xfers.some((t) => !["running", "starting", "queued"].includes(t.state));
  renderXfers();
  if (active.length && !xferTimer) xferTimer = setInterval(tickXfers, 500);
  if (!active.length && xferTimer) { clearInterval(xferTimer); xferTimer = null; }
}

// Speeds and time left, smoothed.
function tickXfers() {
  const now = performance.now();
  for (const t of xfers) {
    if (t.state !== "running" || t.paused || !t.mark) continue;
    const dt = (now - t.mark.at) / 1000;
    if (dt < 0.4) continue;
    const inst = Math.max(0, (t.done - t.mark.done) / dt);
    t.speed = t.speed ? 0.7 * t.speed + 0.3 * inst : inst;
    t.mark = { at: now, done: t.done };
  }
  renderFiles();
}

// ---- host file browser

function listDir(path) {
  fileHost.cwd = path;
  fileHost.listing = null;
  renderBrowser();
  fsend({ op: "list", path });
}

function renderBrowser() {
  const parts = fileHost.cwd ? fileHost.cwd.split("/") : [];
  const crumbs = $("fb-crumbs");
  crumbs.textContent = "";
  const crumb = (label, path) => {
    const b = document.createElement("button");
    b.type = "button";
    b.textContent = label;
    b.title = label;
    b.addEventListener("click", () => listDir(path));
    crumbs.appendChild(b);
  };
  crumb(fileHost.user ? `${fileHost.user} (home)` : "Home", "");
  parts.forEach((p, i) => {
    crumbs.appendChild(document.createTextNode("/"));
    crumb(p, parts.slice(0, i + 1).join("/"));
  });
  $("fb-up").disabled = parts.length === 0;
  const list = $("fb-list");
  list.textContent = "";
  const l = fileHost.listing;
  const msg = (text, cls = "empty") => { const d = document.createElement("div"); d.className = cls; d.textContent = text; list.appendChild(d); };
  if (!l) return msg("Loading…");
  if (l.error) return msg(l.error, "empty err");
  const shown = l.entries.filter((e) => fileHost.hidden || !e.hidden);
  if (!shown.length) return msg("This folder is empty.");
  for (const e of shown) {
    const b = document.createElement("button");
    b.type = "button";
    b.className = e.dir ? "dir" : "file";
    b.setAttribute("role", "listitem");
    b.innerHTML = `${e.dir ? ICONS.folder : ICONS.file}<span class="fname"></span><span class="fsize"></span>`;
    b.querySelector(".fname").textContent = e.name;
    b.querySelector(".fsize").textContent = e.dir ? "" : fmtBytes(e.size);
    const path = fileHost.cwd ? `${fileHost.cwd}/${e.name}` : e.name;
    b.title = e.dir ? `Open ${e.name}` : `Download ${e.name} (${fmtBytes(e.size)})`;
    b.addEventListener("click", () => (e.dir ? listDir(path) : startDownload(path, e.name, e.size)));
    list.appendChild(b);
  }
}

// ---- wiring

$("btn-files").addEventListener("click", () => {
  togglePopover("pop-files");
  // Someone may have signed in on the host since it said "unavailable".
  if (fileHost.status !== "ready") fsend({ op: "hello" });
  renderFiles();
});
$("xfer-list").addEventListener("click", (e) => {
  const b = e.target.closest("button[data-act]");
  if (!b) return;
  const t = xfers.find((x) => String(x.id) === b.closest("[data-id]").dataset.id);
  if (!t) return;
  if (b.dataset.act === "cancel") cancelXfer(t);
  else if (b.dataset.act === "remove") { xfers = xfers.filter((x) => x !== t); renderFiles(); }
  else pauseXfer(t);
});
$("xfer-clear").addEventListener("click", () => {
  xfers = xfers.filter((t) => ["running", "starting", "queued"].includes(t.state));
  renderFiles();
});
$("fb-toggle").addEventListener("click", () => {
  fileHost.browsing = !fileHost.browsing;
  if (fileHost.browsing) listDir(fileHost.cwd);
  renderFiles();
});
$("fb-up").addEventListener("click", () => listDir(fileHost.cwd.split("/").slice(0, -1).join("/")));
$("fb-hidden").addEventListener("change", (e) => { fileHost.hidden = e.target.checked; renderBrowser(); });
$("file-input").addEventListener("change", (e) => {
  if (e.target.files.length) queueUploads([...e.target.files]);
  e.target.value = "";
});
$("dropzone").addEventListener("keydown", (e) => {
  if (e.key === "Enter" || e.key === " ") { e.preventDefault(); $("file-input").click(); }
});

// Files dropped on the panel or anywhere on the remote screen. Folders can't be sent.
const hasFiles = (e) => !!e.dataTransfer && [...e.dataTransfer.types].includes("Files");
function droppedFiles(e) {
  const out = [];
  const items = [...(e.dataTransfer.items || [])];
  [...e.dataTransfer.files].forEach((f, i) => {
    const entry = items[i] && items[i].webkitGetAsEntry ? items[i].webkitGetAsEntry() : null;
    if (entry && entry.isDirectory) {
      xfers.unshift({ id: nextXferId++, dir: "up", name: f.name, size: 0, done: 0, state: "error", error: "Folders can't be sent. Zip it first." });
    } else out.push(f);
  });
  return out;
}
let dragDepth = 0;
const overlay = $("drop-overlay");
// Without this the browser would open a file dropped anywhere on the page.
document.addEventListener("dragover", (e) => { if (hasFiles(e)) e.preventDefault(); });
document.addEventListener("drop", (e) => { if (hasFiles(e)) e.preventDefault(); });
stage.addEventListener("dragenter", (e) => {
  if (!hasFiles(e) || !filesReady()) return;
  dragDepth++;
  if (!$("pop-files").contains(e.target)) overlay.hidden = false;
});
stage.addEventListener("dragover", (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  e.dataTransfer.dropEffect = filesReady() ? "copy" : "none";
  const inPanel = $("pop-files").contains(e.target);
  $("dropzone").classList.toggle("over", inPanel);
  overlay.hidden = inPanel || !filesReady();
});
stage.addEventListener("dragleave", () => {
  dragDepth = Math.max(0, dragDepth - 1);
  if (!dragDepth) { overlay.hidden = true; $("dropzone").classList.remove("over"); }
});
stage.addEventListener("drop", (e) => {
  if (!hasFiles(e)) return;
  e.preventDefault();
  dragDepth = 0;
  overlay.hidden = true;
  $("dropzone").classList.remove("over");
  if (!filesReady()) return;
  const list = droppedFiles(e);
  if ($("pop-files").hidden) togglePopover("pop-files");
  if (list.length) queueUploads(list);
  else renderFiles();
});

// Leaving the page would drop transfers in progress.
window.addEventListener("beforeunload", (e) => {
  if (activeXfers().length) { e.preventDefault(); e.returnValue = ""; }
});

// ---------------------------------------------------------------- boot

async function start() {
  try {
    info = await api("/api/info");
  } catch (err) {
    if (err.status === 401) { showLogin(); return; }
    setStatus(`error: ${err.message}`, "error");
    return;
  }
  $("pop-host").textContent = info.hostname || "Dalker";
  $("s-system").textContent = [info.os, info.capture].filter(Boolean).join(" · ") || "–";
  $("s-version").textContent = info.version ? `Dalker ${info.version}` : "–";
  renderKeysMenu();
  $("set-version").textContent = info.version ? `v${info.version}` : "";
  $("btn-files").hidden = !info.files;
  renderSettings();
  watchMcp();
  document.title = info.hostname ? `${info.hostname} — Dalker` : "Dalker";
  await connect();
}

setStatus("disconnected", "idle");
start();
