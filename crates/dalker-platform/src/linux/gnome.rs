//! Capture and border for GNOME on Wayland (Ubuntu 26.04 has no Xorg session).
//!
//! * **Capture** – `org.gnome.Mutter.ScreenCast` on the *user's* session bus creates a
//!   PipeWire stream of the monitor without any approval dialog; frames are read by a
//!   `gst-launch-1.0 pipewiresrc` child process (so no libpipewire headers are needed to
//!   build). The agent must therefore run as the logged-in user.
//! * **Border** – Wayland has no way to draw a click-through, always-on-top frame from
//!   outside the compositor, so a small GNOME Shell extension (`dalker-border@ivanmakhnyk.github.io`)
//!   owns `io.github.ivanmakhnyk.Dalker` on the session bus and exports the
//!   `io.github.ivanmakhnyk.Dalker.Border` interface. [`GnomeBorder`] keeps pinging it
//!   while a viewer is connected; the extension hides the frame if the pings stop.
//! * **Clipboard** – Wayland gives clients outside the compositor no clipboard access, so
//!   the same extension also exports the shell's clipboard text ([`GnomeClipboard`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use dalker_core::{
    CaptureError, Clipboard, ClipboardError, DisplayInfo, RawFrame, ScreenCapturer, ViewerIndicator,
};
use zbus::blocking::fdo::DBusProxy;
use zbus::blocking::{Connection, Proxy};
use zbus::names::BusName;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::gst::{self, backend, GstStream};

const SC: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SESSION_IFACE: &str = "org.gnome.Mutter.ScreenCast.Session";
const STREAM_IFACE: &str = "org.gnome.Mutter.ScreenCast.Stream";
/// The bus name the extension owns, and its interface.
const BUS_NAME: &str = "io.github.ivanmakhnyk.Dalker";
const BORDER: &str = "io.github.ivanmakhnyk.Dalker.Border";
const BORDER_PATH: &str = "/io/github/ivanmakhnyk/Dalker/Border";

pub(super) fn name_has_owner(conn: &Connection, name: &str) -> bool {
    let Ok(bus_name) = BusName::try_from(name) else {
        return false;
    };
    DBusProxy::new(conn)
        .ok()
        .and_then(|p| p.name_has_owner(bus_name).ok())
        .unwrap_or(false)
}

/// True when this process can see a GNOME Shell (Mutter) screen-cast service on its
/// session bus, i.e. the desktop is GNOME on Wayland (or X11 under Mutter).
pub fn gnome_session_available() -> bool {
    Connection::session()
        .map(|c| name_has_owner(&c, SC))
        .unwrap_or(false)
}

/// True while GNOME's lock screen is up (`org.gnome.ScreenSaver.GetActive`). False when
/// there is no GNOME session bus to ask. The connection is kept, since viewers poll this.
pub fn screen_locked() -> bool {
    static CONN: Mutex<Option<Connection>> = Mutex::new(None);
    let mut conn = CONN.lock().unwrap();
    if conn.is_none() {
        *conn = Connection::session().ok();
    }
    let Some(c) = conn.as_ref() else { return false };
    let reply = Proxy::new(
        c,
        "org.gnome.ScreenSaver",
        "/org/gnome/ScreenSaver",
        "org.gnome.ScreenSaver",
    )
    .and_then(|p| p.call::<_, _, bool>("GetActive", &()));
    match reply {
        Ok(active) => active,
        Err(_) => {
            // The bus may have gone away with the session; reconnect next time.
            *conn = None;
            false
        }
    }
}

// ---------------------------------------------------------------------------- layout

/// Where the captured (primary) monitor lies on the GNOME desktop, in Mutter's layout
/// coordinates. An absolute pointer device spans the whole desktop (every monitor), so
/// the injector needs this to land on the monitor that is streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonitorLayout {
    /// Offset of the primary monitor from the desktop's top-left corner.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Bounding box of all monitors.
    pub desktop_width: u32,
    pub desktop_height: u32,
}

/// Width of the extension's red frame in layout pixels (`BORDER_PX` in extension.js).
/// It is cut out of the stream, so viewers see the desktop inside it.
pub const BORDER_INSET: u32 = 5;

/// Stream pixels to cut from each edge for a monitor `stream_width` pixels wide that
/// takes `layout_width` layout pixels (more than [`BORDER_INSET`] when it is scaled).
pub(super) fn border_crop(stream_width: u32, layout_width: u32) -> u32 {
    if layout_width == 0 {
        return BORDER_INSET;
    }
    (BORDER_INSET as u64 * stream_width as u64).div_ceil(layout_width as u64) as u32
}

/// One logical monitor (a monitor, or mirrored monitors) in layout coordinates.
/// `primary` marks the one that is streamed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LogicalRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub primary: bool,
}

/// Size a monitor takes up in the layout: rotated by `transform` (odd values are
/// 90°/270°), and divided by `scale` when the layout uses logical pixels.
fn layout_size(mode: (i32, i32), scale: f64, transform: u32, logical_pixels: bool) -> (u32, u32) {
    let (w, h) = if transform % 2 == 1 {
        (mode.1, mode.0)
    } else {
        mode
    };
    let s = if logical_pixels && scale > 0.0 {
        scale
    } else {
        1.0
    };
    (
        (w as f64 / s).round().max(1.0) as u32,
        (h as f64 / s).round().max(1.0) as u32,
    )
}

pub(super) fn layout_of(rects: &[LogicalRect]) -> Option<MonitorLayout> {
    let primary = rects.iter().find(|r| r.primary).or_else(|| rects.first())?;
    let min_x = rects.iter().map(|r| r.x).min()?;
    let min_y = rects.iter().map(|r| r.y).min()?;
    let max_x = rects.iter().map(|r| r.x + r.width as i32).max()?;
    let max_y = rects.iter().map(|r| r.y + r.height as i32).max()?;
    Some(MonitorLayout {
        x: (primary.x - min_x) as u32,
        y: (primary.y - min_y) as u32,
        width: primary.width,
        height: primary.height,
        desktop_width: (max_x - min_x) as u32,
        desktop_height: (max_y - min_y) as u32,
    })
}

type MonitorSpec = (String, String, String, String);
type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Monitor = (MonitorSpec, Vec<Mode>, HashMap<String, OwnedValue>);
type Logical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<MonitorSpec>,
    HashMap<String, OwnedValue>,
);
type CurrentState = (u32, Vec<Monitor>, Vec<Logical>, HashMap<String, OwnedValue>);

/// Reads the monitor layout from `org.gnome.Mutter.DisplayConfig` on the session bus.
/// `None` when that is not available (not GNOME, or no session bus).
pub fn monitor_layout(conn: &Connection) -> Option<MonitorLayout> {
    let proxy = Proxy::new(
        conn,
        "org.gnome.Mutter.DisplayConfig",
        "/org/gnome/Mutter/DisplayConfig",
        "org.gnome.Mutter.DisplayConfig",
    )
    .ok()?;
    let (_serial, monitors, logical, props): CurrentState =
        proxy.call("GetCurrentState", &()).ok()?;
    // layout-mode 1 = logical pixels (the default), 2 = physical pixels.
    let logical_pixels = props
        .get("layout-mode")
        .and_then(|v| u32::try_from(v.clone()).ok())
        .is_none_or(|m| m == 1);
    let current_mode = |spec: &MonitorSpec| -> Option<(i32, i32)> {
        let (_, modes, _) = monitors.iter().find(|(s, _, _)| s == spec)?;
        modes
            .iter()
            .find(|m| {
                m.6.get("is-current")
                    .and_then(|v| bool::try_from(v.clone()).ok())
                    .unwrap_or(false)
            })
            .map(|m| (m.1, m.2))
    };
    let rects: Vec<LogicalRect> = logical
        .iter()
        .filter_map(|(x, y, scale, transform, primary, specs, _)| {
            let mode = specs.iter().find_map(current_mode)?;
            let (width, height) = layout_size(mode, *scale, *transform, logical_pixels);
            Some(LogicalRect {
                x: *x,
                y: *y,
                width,
                height,
                primary: *primary,
            })
        })
        .collect();
    layout_of(&rects)
}

// ---------------------------------------------------------------------------- capture

pub struct GnomeCapturer {
    conn: Connection,
    session_path: OwnedObjectPath,
    stream: GstStream,
}

fn unsupported(msg: impl std::fmt::Display) -> CaptureError {
    CaptureError::Unsupported(msg.to_string())
}

impl GnomeCapturer {
    /// Starts a screen cast of the primary monitor.
    ///
    /// Returns [`CaptureError::Unsupported`] when there is no GNOME screen-cast service
    /// on the session bus, so callers can fall back to X11.
    pub fn new() -> Result<Self, CaptureError> {
        let conn =
            Connection::session().map_err(|e| unsupported(format!("no D-Bus session bus: {e}")))?;
        if !name_has_owner(&conn, SC) {
            return Err(unsupported(
                "org.gnome.Mutter.ScreenCast is not on the session bus",
            ));
        }
        gst::check_installed()?;

        let sc = Proxy::new(&conn, SC, SC_PATH, SC).map_err(|e| backend("ScreenCast proxy", e))?;
        let session_path: OwnedObjectPath = sc
            .call("CreateSession", &(HashMap::<&str, Value>::new(),))
            .map_err(|e| backend("CreateSession", e))?;
        let session = Proxy::new(&conn, SC, session_path.clone(), SESSION_IFACE)
            .map_err(|e| backend("Session proxy", e))?;
        // GNOME stops every screen cast when the screen locks (and the border extension
        // allows a new one while someone is connected). The PipeWire stream then just goes
        // quiet, so watch for the session's end to recreate the capturer.
        let mut session_closed = session
            .receive_signal("Closed")
            .map_err(|e| backend("subscribe Closed", e))?;

        // Empty connector = primary monitor; cursor-mode 1 = cursor embedded in the frames.
        let mut props = HashMap::<&str, Value>::new();
        props.insert("cursor-mode", Value::U32(1));
        let stream_path: OwnedObjectPath = session
            .call("RecordMonitor", &("", props))
            .map_err(|e| backend("RecordMonitor", e))?;
        let stream = Proxy::new(&conn, SC, stream_path, STREAM_IFACE)
            .map_err(|e| backend("Stream proxy", e))?;

        // Subscribe *before* Start, then wait for the PipeWire node id on a helper thread
        // (the blocking signal iterator has no timeout of its own).
        let (node_tx, node_rx) = mpsc::channel::<u32>();
        let mut signals = stream
            .receive_signal("PipeWireStreamAdded")
            .map_err(|e| backend("subscribe", e))?;
        std::thread::spawn(move || {
            if let Some(msg) = signals.next() {
                if let Ok(node) = msg.body().deserialize::<u32>() {
                    let _ = node_tx.send(node);
                }
            }
        });
        session
            .call_method("Start", &())
            .map_err(|e| backend("Start", e))?;
        let node = node_rx.recv_timeout(Duration::from_secs(8)).map_err(|_| {
            CaptureError::Backend("timed out waiting for the PipeWire stream".into())
        })?;

        let params: HashMap<String, OwnedValue> = stream
            .get_property("Parameters")
            .map_err(|e| backend("stream Parameters", e))?;
        let (w, h) = params
            .get("size")
            .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
            .filter(|(w, h)| *w > 0 && *h > 0)
            .ok_or_else(|| CaptureError::Backend("stream has no size".into()))?;
        // The red border belongs on the host's screen, not in the viewer: cut it off every
        // frame. It is BORDER_INSET layout pixels wide, which is more stream pixels on a
        // scaled monitor. The injector skips the same strip (see `map_point`).
        let crop = monitor_layout(&conn)
            .map(|l| border_crop(w as u32, l.width))
            .unwrap_or(BORDER_INSET);
        let (w, h) = (
            (w as u32).saturating_sub(2 * crop) & !1,
            (h as u32).saturating_sub(2 * crop) & !1,
        );
        if w == 0 || h == 0 {
            return Err(CaptureError::Backend(
                "monitor smaller than its border".into(),
            ));
        }

        let stream = GstStream::spawn(node, None, crop, w, h)?;
        let closer = stream.closer();
        std::thread::Builder::new()
            .name("dalker-sc-closed".into())
            .spawn(move || {
                // Ends on the signal, or when the connection goes away with the capturer.
                if session_closed.next().is_some() {
                    closer.close("GNOME ended the screen cast (screen locked?)");
                }
            })
            .map_err(|e| backend("spawn session watcher", e))?;

        tracing::info!(width = w, height = h, node, "GNOME screen cast started");
        Ok(Self {
            conn,
            session_path,
            stream,
        })
    }
}

impl Drop for GnomeCapturer {
    fn drop(&mut self) {
        self.stream.stop();
        if let Ok(session) = Proxy::new(&self.conn, SC, self.session_path.clone(), SESSION_IFACE) {
            let _ = session.call_method("Stop", &());
        }
    }
}

impl ScreenCapturer for GnomeCapturer {
    fn name(&self) -> &str {
        "gnome-screencast"
    }

    fn display(&self) -> DisplayInfo {
        let (width, height) = self.stream.size();
        DisplayInfo { width, height }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        self.stream.next_frame(timeout)
    }
}

// ---------------------------------------------------------------------------- border

/// Drives the `dalker-border@ivanmakhnyk.github.io` GNOME Shell extension.
pub struct GnomeBorder {
    conn: Connection,
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    heartbeat: Option<JoinHandle<()>>,
}

impl GnomeBorder {
    pub fn new() -> Result<Self, String> {
        let conn = Connection::session().map_err(|e| format!("no D-Bus session bus: {e}"))?;
        if !name_has_owner(&conn, BUS_NAME) {
            if screen_locked() {
                return Err(
                    "the screen is locked (GNOME turns extensions off on the lock screen; \
                            the desktop is served again once it is unlocked)"
                        .into(),
                );
            }
            return Err(
                "GNOME Shell extension dalker-border@ivanmakhnyk.github.io is not running \
                 (it is loaded at login: log out and back in after installing)"
                    .to_string(),
            );
        }
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (c, a, s) = (conn.clone(), Arc::clone(&active), Arc::clone(&stop));
        // Ping every second while active; the extension hides the frame ~3 s after the
        // last ping, so a crashed agent can never leave a stale "recording" border (or,
        // more importantly, stream without one).
        let heartbeat = std::thread::Builder::new()
            .name("dalker-border-ping".into())
            .spawn(move || {
                let proxy = Proxy::new(&c, BUS_NAME, BORDER_PATH, BORDER).ok();
                let mut last = false;
                while !s.load(Ordering::SeqCst) {
                    let now = a.load(Ordering::SeqCst);
                    if let Some(p) = &proxy {
                        if now || last {
                            let _ = p.call_method("SetActive", &(now,));
                        }
                    }
                    last = now;
                    std::thread::sleep(Duration::from_millis(if now { 1000 } else { 200 }));
                }
                if let Some(p) = &proxy {
                    let _ = p.call_method("SetActive", &(false,));
                }
            })
            .map_err(|e| format!("spawn border thread: {e}"))?;
        Ok(Self {
            conn,
            active,
            stop,
            heartbeat: Some(heartbeat),
        })
    }
}

impl ViewerIndicator for GnomeBorder {
    fn name(&self) -> &str {
        "gnome-shell-border"
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    fn healthy(&self) -> bool {
        name_has_owner(&self.conn, BUS_NAME)
    }
}

impl Drop for GnomeBorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.heartbeat.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------- clipboard

/// D-Bus error the extension answers clipboard calls with while no viewer is connected.
const INACTIVE_ERROR: &str = "io.github.ivanmakhnyk.Dalker.Border.Error.Inactive";

/// The GNOME Shell clipboard, through the Dalker extension (`io.github.ivanmakhnyk.Dalker`):
/// `ClipboardSerial() -> u` (bumped when the clipboard changes), `GetClipboardText() -> s`
/// and `SetClipboardText(s)`, which only work while the border is shown. The connection is made again after an error, so the clipboard
/// comes back when the shell restarts.
pub struct GnomeClipboard {
    conn: Option<Connection>,
}

impl GnomeClipboard {
    pub fn new() -> Result<Self, String> {
        let mut c = Self { conn: None };
        c.proxy().map_err(|e| e.to_string())?;
        Ok(c)
    }

    fn proxy(&mut self) -> Result<Proxy<'static>, ClipboardError> {
        if self.conn.is_none() {
            let conn = Connection::session()
                .map_err(|e| ClipboardError::Failed(format!("no D-Bus session bus: {e}")))?;
            self.conn = Some(conn);
        }
        let conn = self.conn.clone().unwrap();
        if !name_has_owner(&conn, BUS_NAME) {
            self.conn = None;
            return Err(ClipboardError::Failed(
                "the Dalker GNOME Shell extension is not running".into(),
            ));
        }
        Proxy::new(&conn, BUS_NAME, BORDER_PATH, BORDER)
            .map_err(|e| ClipboardError::Failed(e.to_string()))
    }

    fn failed(&mut self, method: &str, e: zbus::Error) -> ClipboardError {
        if let zbus::Error::MethodError(name, _, _) = &e {
            match name.as_str() {
                // The shell keeps running the extension it loaded at login.
                "org.freedesktop.DBus.Error.UnknownMethod" => {
                    return ClipboardError::Failed(
                        "the Dalker GNOME Shell extension running now has no clipboard support \
                         (it was updated: log out and back in to load the new one)"
                            .into(),
                    )
                }
                // The extension only hands out the clipboard while its border is shown.
                INACTIVE_ERROR => {
                    return ClipboardError::Busy(
                        "the remote-session border is not shown yet".into(),
                    )
                }
                _ => {}
            }
        }
        self.conn = None;
        ClipboardError::Failed(format!("{method}: {e}"))
    }
}

impl Clipboard for GnomeClipboard {
    fn name(&self) -> &str {
        "gnome-shell"
    }

    fn change_marker(&mut self) -> Result<Option<u64>, ClipboardError> {
        let proxy = self.proxy()?;
        match proxy.call::<_, _, u32>("ClipboardSerial", &()) {
            Ok(serial) => Ok(Some(serial as u64)),
            Err(e) => Err(self.failed("ClipboardSerial", e)),
        }
    }

    fn get_text(&mut self) -> Result<Option<String>, ClipboardError> {
        let proxy = self.proxy()?;
        match proxy.call::<_, _, String>("GetClipboardText", &()) {
            Ok(text) => Ok((!text.is_empty()).then_some(text)),
            Err(e) => Err(self.failed("GetClipboardText", e)),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let proxy = self.proxy()?;
        match proxy.call::<_, _, ()>("SetClipboardText", &(text,)) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.failed("SetClipboardText", e)),
        }
    }
}

// ---------------------------------------------------------------------------- text

/// Types `text` through the Dalker extension (`TypeText(s)`): committed to the focused
/// text field through the shell's input method, so the host's keyboard layout plays no
/// part. Only works while the border is shown.
pub fn type_text(conn: &Connection, text: &str) -> Result<(), String> {
    let proxy = Proxy::new(conn, BUS_NAME, BORDER_PATH, BORDER).map_err(|e| e.to_string())?;
    proxy.call::<_, _, ()>("TypeText", &(text,)).map_err(|e| match &e {
        zbus::Error::MethodError(name, _, _) if name.as_str() == "org.freedesktop.DBus.Error.UnknownMethod" => {
            "the Dalker GNOME Shell extension running now can't type text              (it was updated: log out and back in to load the new one)"
                .to_string()
        }
        _ => format!("TypeText: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(x: i32, y: i32, width: u32, height: u32, primary: bool) -> LogicalRect {
        LogicalRect {
            x,
            y,
            width,
            height,
            primary,
        }
    }

    #[test]
    fn single_monitor_is_the_whole_desktop() {
        let l = layout_of(&[rect(0, 0, 1280, 800, true)]).unwrap();
        assert_eq!(
            l,
            MonitorLayout {
                x: 0,
                y: 0,
                width: 1280,
                height: 800,
                desktop_width: 1280,
                desktop_height: 800
            }
        );
    }

    #[test]
    fn primary_on_the_right_of_a_taller_monitor() {
        let l = layout_of(&[
            rect(0, 0, 1920, 1200, false),
            rect(1920, 100, 1280, 800, true),
        ])
        .unwrap();
        assert_eq!(
            l,
            MonitorLayout {
                x: 1920,
                y: 100,
                width: 1280,
                height: 800,
                desktop_width: 3200,
                desktop_height: 1200
            }
        );
    }

    #[test]
    fn negative_origins_are_normalized() {
        let l = layout_of(&[
            rect(-1280, 0, 1280, 800, false),
            rect(0, 0, 1280, 800, true),
        ])
        .unwrap();
        assert_eq!(
            (l.x, l.y, l.desktop_width, l.desktop_height),
            (1280, 0, 2560, 800)
        );
        assert!(layout_of(&[]).is_none());
    }

    #[test]
    fn border_crop_follows_the_monitor_scale() {
        assert_eq!(border_crop(1280, 1280), 5);
        assert_eq!(border_crop(2560, 1280), 10);
        assert_eq!(
            border_crop(1920, 1536),
            7,
            "1.25x rounds up so no red is left"
        );
        assert_eq!(border_crop(1280, 0), BORDER_INSET);
    }

    #[test]
    fn layout_size_scales_and_rotates() {
        assert_eq!(layout_size((2560, 1440), 2.0, 0, true), (1280, 720));
        assert_eq!(layout_size((2560, 1440), 2.0, 0, false), (2560, 1440));
        assert_eq!(layout_size((1920, 1080), 1.0, 1, true), (1080, 1920));
        assert_eq!(layout_size((1920, 1080), 1.25, 0, true), (1536, 864));
    }
}
