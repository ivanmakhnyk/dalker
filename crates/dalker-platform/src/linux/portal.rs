//! Capture through the `xdg-desktop-portal` ScreenCast portal, for Wayland desktops other
//! than GNOME (KDE Plasma, sway, Hyprland...).
//!
//! The portal asks the person at the host which monitor to share the first time. The
//! session is started with `persist_mode = 2`, so the portal hands back a restore token
//! that lets later starts skip the dialog; a new token comes back on every start and is
//! saved each time. The stream is read with the same `gst-launch-1.0 pipewiresrc` child as
//! on GNOME, given the portal's PipeWire connection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

use dalker_core::{CaptureError, DisplayInfo, RawFrame, ScreenCapturer};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedObjectPath, OwnedValue, Value};

use super::gnome::{border_crop, layout_of, LogicalRect, MonitorLayout, BORDER_INSET};
use super::gst::{self, backend, GstStream};
use super::wayland::{self, OutputInfo};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const PORTAL_PATH: &str = "/org/freedesktop/portal/desktop";
const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
const REQUEST: &str = "org.freedesktop.portal.Request";
const SESSION: &str = "org.freedesktop.portal.Session";

/// `SelectSources` source type and cursor modes.
const SOURCE_MONITOR: u32 = 1;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
/// Keep the permission until it is revoked (needs version 4 of the portal).
const PERSIST_UNTIL_REVOKED: u32 = 2;

/// How long the person at the host has to answer the "share your screen" dialog.
const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
/// Requests that need no answer from anyone.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// After the dialog was declined (or left unanswered), don't ask again for this long.
const DECLINED_PAUSE: Duration = Duration::from_secs(60);

/// When the dialog was last declined or left unanswered.
static DECLINED: Mutex<Option<Instant>> = Mutex::new(None);

/// The monitor being streamed, in the compositor's coordinates: (x, y) when the portal
/// says, and the size. Kept after the capturer ends, for the pointer.
static STREAMED: Mutex<Option<StreamedMonitor>> = Mutex::new(None);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StreamedMonitor {
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

fn unsupported(msg: impl std::fmt::Display) -> CaptureError {
    CaptureError::Unsupported(msg.to_string())
}

/// True when the session bus has an `xdg-desktop-portal` with the ScreenCast portal.
pub fn available() -> bool {
    Connection::session()
        .ok()
        .and_then(|c| screencast_version(&c).ok())
        .is_some()
}

fn screencast_version(conn: &Connection) -> Result<u32, CaptureError> {
    let proxy = Proxy::new(conn, PORTAL, PORTAL_PATH, SCREENCAST)
        .map_err(|e| backend("ScreenCast portal proxy", e))?;
    proxy
        .get_property::<u32>("version")
        .map_err(|e| unsupported(format!("no xdg-desktop-portal ScreenCast portal: {e}")))
}

/// Where the restore token is kept: `$XDG_STATE_HOME/dalker/screencast-restore-token`.
fn token_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_STATE_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|v| !v.is_empty())
                .map(|h| PathBuf::from(h).join(".local/state"))
        })?;
    Some(base.join("dalker").join("screencast-restore-token"))
}

fn load_token() -> Option<String> {
    let text = std::fs::read_to_string(token_path()?).ok()?;
    let token = text.trim();
    (!token.is_empty()).then(|| token.to_string())
}

fn save_token(token: &str) {
    let Some(path) = token_path() else { return };
    let result = (|| {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        // Replace it in one step, so a crash never leaves half a token. Only its owner can
        // read it: with it, any program of theirs could cast the screen without a dialog.
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let tmp = path.with_extension("tmp");
        let _ = std::fs::remove_file(&tmp);
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        f.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        f.write_all(token.as_bytes())?;
        f.sync_all()?;
        drop(f);
        std::fs::rename(&tmp, &path)
    })();
    if let Err(e) = result {
        tracing::warn!(
            "cannot save the screen-sharing permission to {}: {e}; the host will be asked again next time",
            path.display()
        );
    }
}

/// Whether screen sharing was allowed ahead of time: a restore token is saved.
pub fn sharing_allowed() -> bool {
    load_token().is_some()
}

/// Asks the person at the computer to allow screen sharing now ("share your screen"
/// dialog), saves the restore token, and stops at once. Later connections then start
/// without the dialog. `Ok(false)` when they declined or didn't answer.
pub fn allow_sharing_now() -> Result<bool, String> {
    let conn = Connection::session().map_err(|e| format!("no D-Bus session bus: {e}"))?;
    let result = (|| {
        let version = screencast_version(&conn).map_err(|e| e.to_string())?;
        if version < 4 {
            return Err(format!(
                "this xdg-desktop-portal (ScreenCast version {version}) can't remember the                  permission; the host will be asked on every connection"
            ));
        }
        let sc = Proxy::new(&conn, PORTAL, PORTAL_PATH, SCREENCAST)
            .map_err(|e| format!("ScreenCast portal proxy: {e}"))?;
        let Some(session) = create_session(&conn, &sc).map_err(|e| e.to_string())? else {
            return Ok(false);
        };
        let granted = grant(&conn, &sc, version, &session);
        close_session(&conn, &session);
        match granted.map_err(|e| e.to_string())? {
            Some(_) => Ok(sharing_allowed()),
            None => Ok(false),
        }
    })();
    let _ = conn.close();
    if matches!(result, Ok(true)) {
        // Asked and answered: don't hold a viewer back for an earlier "no".
        *DECLINED.lock().unwrap() = None;
    }
    result
}

/// Builds the `Request` object path the portal will answer on, for `token`.
fn request_path(conn: &Connection, token: &str) -> Result<OwnedObjectPath, CaptureError> {
    let sender = conn
        .unique_name()
        .ok_or_else(|| CaptureError::Backend("no unique name on the session bus".into()))?;
    let sender = sender.trim_start_matches(':').replace('.', "_");
    OwnedObjectPath::try_from(format!("{PORTAL_PATH}/request/{sender}/{token}"))
        .map_err(|e| backend("request path", e))
}

fn new_token() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(0);
    format!(
        "dalker_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

type Results = HashMap<String, OwnedValue>;

/// How a portal request ended.
enum Answer {
    Granted(Results),
    Declined,
}

/// Calls a portal method that answers with a `Request`, and waits up to `timeout` for its
/// `Response` signal. `call` gets the `handle_token` to put in the options.
fn request(
    conn: &Connection,
    timeout: Duration,
    call: impl FnOnce(&str) -> zbus::Result<OwnedObjectPath>,
) -> Result<Answer, CaptureError> {
    let token = new_token();
    let path = request_path(conn, &token)?;
    // Subscribe before calling, so a quick answer can't be missed.
    let req =
        Proxy::new(conn, PORTAL, path.clone(), REQUEST).map_err(|e| backend("Request proxy", e))?;
    let mut responses = req
        .receive_signal("Response")
        .map_err(|e| backend("subscribe Response", e))?;
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("dalker-portal-request".into())
        .spawn(move || {
            // Ends with the answer, or when the connection is closed.
            if let Some(msg) = responses.next() {
                let _ = tx.send(msg.body().deserialize::<(u32, Results)>());
            }
        })
        .map_err(|e| backend("spawn request watcher", e))?;
    let handle = call(&token).map_err(|e| backend("portal call", e))?;
    if handle != path {
        // Very old portals choose their own path; we only listen on the expected one.
        return Err(CaptureError::Backend(format!(
            "the portal answers on {handle}, not {path} (xdg-desktop-portal too old)"
        )));
    }
    match rx.recv_timeout(timeout) {
        Ok(Ok((0, results))) => Ok(Answer::Granted(results)),
        Ok(Ok((1, _))) => Ok(Answer::Declined),
        Ok(Ok((code, _))) => Err(CaptureError::Backend(format!(
            "the screen-cast portal failed (response {code})"
        ))),
        Ok(Err(e)) => Err(backend("portal response", e)),
        Err(_) => {
            let _ = req.call_method("Close", &());
            Ok(Answer::Declined)
        }
    }
}

/// One stream that `Start` returned: its PipeWire node and properties.
struct PortalStream {
    node: u32,
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
}

fn parse_streams(results: &Results) -> Result<Vec<PortalStream>, CaptureError> {
    let streams = results
        .get("streams")
        .ok_or_else(|| CaptureError::Backend("the portal returned no streams".into()))?;
    let streams = <Vec<(u32, HashMap<String, OwnedValue>)>>::try_from(streams.clone())
        .map_err(|e| backend("portal streams", e))?;
    Ok(streams
        .into_iter()
        .map(|(node, props)| {
            let pair = |key: &str| {
                props
                    .get(key)
                    .and_then(|v| <(i32, i32)>::try_from(v.clone()).ok())
            };
            PortalStream {
                node,
                position: pair("position"),
                size: pair("size").filter(|(w, h)| *w > 0 && *h > 0),
            }
        })
        .collect())
}

/// The output a stream shows: the one at its position, else the only one, else the only
/// one of its size.
fn streamed_output(
    outputs: &[OutputInfo],
    position: Option<(i32, i32)>,
    size: Option<(i32, i32)>,
) -> Option<&OutputInfo> {
    if let Some((x, y)) = position {
        if let Some(o) = outputs.iter().find(|o| o.x == x && o.y == y) {
            return Some(o);
        }
    }
    if outputs.len() == 1 {
        return outputs.first();
    }
    let (w, h) = size?;
    let mut same = outputs
        .iter()
        .filter(|o| o.width as i32 == w && o.height as i32 == h);
    match (same.next(), same.next()) {
        (Some(o), None) => Some(o),
        _ => None,
    }
}

/// The monitor layout for the pointer: every monitor of the Wayland desktop, with the
/// streamed one as `primary`. `None` before anything was streamed, or when the streamed
/// monitor can't be told apart from the others.
pub fn monitor_layout() -> Option<MonitorLayout> {
    let streamed = (*STREAMED.lock().unwrap())?;
    let outputs = wayland::outputs().ok()?;
    let target = streamed_output(&outputs, streamed.position, streamed.size)?;
    let rects: Vec<LogicalRect> = outputs
        .iter()
        .map(|o| LogicalRect {
            x: o.x,
            y: o.y,
            width: o.width,
            height: o.height,
            primary: o == target,
        })
        .collect();
    layout_of(&rects)
}

/// A screen cast of one monitor through the ScreenCast portal.
pub struct PortalCapturer {
    conn: Option<Connection>,
    session: OwnedObjectPath,
    stream: GstStream,
}

impl PortalCapturer {
    /// Starts a screen cast, asking the person at the host the first time (or whenever the
    /// saved permission no longer applies).
    ///
    /// Returns [`CaptureError::Unsupported`] when there is no ScreenCast portal.
    pub fn new() -> Result<Self, CaptureError> {
        if let Some(at) = *DECLINED.lock().unwrap() {
            if at.elapsed() < DECLINED_PAUSE {
                return Err(CaptureError::Backend(
                    "screen sharing was not allowed at the host; asking again in a minute".into(),
                ));
            }
        }
        let conn =
            Connection::session().map_err(|e| unsupported(format!("no D-Bus session bus: {e}")))?;
        let version = screencast_version(&conn)?;
        gst::check_installed()?;
        // Closing the connection ends the threads still waiting for portal signals.
        match Self::start(&conn, version) {
            Ok((session, stream)) => Ok(Self {
                conn: Some(conn),
                session,
                stream,
            }),
            Err(e) => {
                let _ = conn.close();
                Err(e)
            }
        }
    }

    fn start(
        conn: &Connection,
        version: u32,
    ) -> Result<(OwnedObjectPath, GstStream), CaptureError> {
        let sc = Proxy::new(conn, PORTAL, PORTAL_PATH, SCREENCAST)
            .map_err(|e| backend("ScreenCast portal proxy", e))?;
        let declined = || {
            *DECLINED.lock().unwrap() = Some(Instant::now());
            CaptureError::Backend(
                "screen sharing was not allowed at the host (the \"share your screen\" dialog was \
                 declined or not answered)"
                    .into(),
            )
        };

        let Some(session) = create_session(conn, &sc)? else {
            return Err(declined());
        };
        let result = Self::start_session(conn, &sc, version, &session);
        if result.is_err() {
            close_session(conn, &session);
        }
        match result {
            Ok(Some(stream)) => Ok((session, stream)),
            Ok(None) => Err(declined()),
            Err(e) => Err(e),
        }
    }

    /// Selects a monitor, starts the cast and the reader. `None` when the person at the
    /// host declined.
    fn start_session(
        conn: &Connection,
        sc: &Proxy<'_>,
        version: u32,
        session: &OwnedObjectPath,
    ) -> Result<Option<GstStream>, CaptureError> {
        let Some((results, saved)) = grant(conn, sc, version, session)? else {
            return Ok(None);
        };
        let session_path = ObjectPath::from(session);
        let streams = parse_streams(&results)?;
        let stream = streams
            .first()
            .ok_or_else(|| CaptureError::Backend("the portal returned no streams".into()))?;
        *STREAMED.lock().unwrap() = Some(StreamedMonitor {
            position: stream.position,
            size: stream.size,
        });

        let fd: zbus::zvariant::OwnedFd = sc
            .call(
                "OpenPipeWireRemote",
                &(&session_path, HashMap::<&str, Value>::new()),
            )
            .map_err(|e| backend("OpenPipeWireRemote", e))?;

        // Stream in the monitor's own pixels when we can tell them (HiDPI); the portal's
        // size is in the compositor's logical units.
        let outputs = wayland::outputs().unwrap_or_default();
        let output = streamed_output(&outputs, stream.position, stream.size);
        let logical_width = output.map(|o| o.width).or(stream.size.map(|s| s.0 as u32));
        let (w, h) = output
            .map(|o| o.pixels)
            .or(stream.size.map(|(w, h)| (w as u32, h as u32)))
            .ok_or_else(|| CaptureError::Backend("the portal stream has no size".into()))?;
        // The red border (drawn by `LayerBorder`) shows up in the cast: cut it off, as on GNOME.
        let crop = logical_width.map_or(BORDER_INSET, |lw| border_crop(w, lw));
        let (w, h) = (
            w.saturating_sub(2 * crop) & !1,
            h.saturating_sub(2 * crop) & !1,
        );
        if w == 0 || h == 0 {
            return Err(CaptureError::Backend(
                "monitor smaller than its border".into(),
            ));
        }

        let session_proxy = Proxy::new(conn, PORTAL, session.clone(), SESSION)
            .map_err(|e| backend("Session proxy", e))?;
        // The compositor ends the cast on its own when the monitor goes away, the person
        // stops sharing, etc. Then recreate the capturer.
        let mut closed = session_proxy
            .receive_signal("Closed")
            .map_err(|e| backend("subscribe Closed", e))?;
        let gst = GstStream::spawn(stream.node, Some(fd.into()), crop, w, h)?;
        let closer = gst.closer();
        std::thread::Builder::new()
            .name("dalker-portal-closed".into())
            .spawn(move || {
                // Ends on the signal, or when the capturer closes the connection.
                if closed.next().is_some() {
                    closer.close("the desktop ended the screen cast");
                }
            })
            .map_err(|e| backend("spawn session watcher", e))?;
        tracing::info!(
            width = w,
            height = h,
            node = stream.node,
            position = ?stream.position,
            remembered = saved.is_some(),
            "portal screen cast started"
        );
        Ok(Some(gst))
    }
}

/// Opens a ScreenCast session. `None` when the portal declined.
fn create_session(
    conn: &Connection,
    sc: &Proxy<'_>,
) -> Result<Option<OwnedObjectPath>, CaptureError> {
    let session_token = new_token();
    let answer = request(conn, REQUEST_TIMEOUT, |token| {
        let mut opts = HashMap::<&str, Value>::new();
        opts.insert("handle_token", Value::from(token));
        opts.insert("session_handle_token", Value::from(session_token.as_str()));
        sc.call("CreateSession", &(opts,))
    })?;
    let Answer::Granted(results) = answer else {
        return Ok(None);
    };
    results
        .get("session_handle")
        .and_then(|v| {
            String::try_from(v.clone())
                .ok()
                .and_then(|s| OwnedObjectPath::try_from(s).ok())
                .or_else(|| OwnedObjectPath::try_from(v.clone()).ok())
        })
        .map(Some)
        .ok_or_else(|| CaptureError::Backend("the portal returned no session".into()))
}

/// Selects a monitor and starts the cast (the portal asks the person at the host unless a
/// saved restore token still applies), and saves the new restore token. Returns `Start`'s
/// results and the token that was used, or `None` when the person declined.
fn grant(
    conn: &Connection,
    sc: &Proxy<'_>,
    version: u32,
    session: &OwnedObjectPath,
) -> Result<Option<(Results, Option<String>)>, CaptureError> {
    let session_path = ObjectPath::from(session);
    let cursor_modes = sc.get_property::<u32>("AvailableCursorModes").unwrap_or(0);
    let cursor = if cursor_modes & CURSOR_EMBEDDED != 0 {
        CURSOR_EMBEDDED
    } else {
        CURSOR_HIDDEN
    };
    let saved = (version >= 4).then(load_token).flatten();
    let answer = request(conn, REQUEST_TIMEOUT, |token| {
        let mut opts = HashMap::<&str, Value>::new();
        opts.insert("handle_token", Value::from(token));
        opts.insert("types", Value::U32(SOURCE_MONITOR));
        opts.insert("multiple", Value::Bool(false));
        if version >= 2 && cursor_modes != 0 {
            opts.insert("cursor_mode", Value::U32(cursor));
        }
        if version >= 4 {
            opts.insert("persist_mode", Value::U32(PERSIST_UNTIL_REVOKED));
            if let Some(t) = &saved {
                opts.insert("restore_token", Value::from(t.as_str()));
            }
        }
        sc.call("SelectSources", &(&session_path, opts))
    })?;
    if matches!(answer, Answer::Declined) {
        return Ok(None);
    }

    if saved.is_none() {
        tracing::info!(
            "asking the person at the host to allow screen sharing (a dialog on the host's screen)"
        );
    }
    let answer = request(conn, APPROVAL_TIMEOUT, |token| {
        let mut opts = HashMap::<&str, Value>::new();
        opts.insert("handle_token", Value::from(token));
        sc.call("Start", &(&session_path, "", opts))
    })?;
    let Answer::Granted(results) = answer else {
        return Ok(None);
    };
    // A new token comes back on every start (the old one is used up): save it each time.
    if let Some(token) = results
        .get("restore_token")
        .and_then(|v| String::try_from(v.clone()).ok())
    {
        save_token(&token);
    }
    Ok(Some((results, saved)))
}

fn close_session(conn: &Connection, session: &OwnedObjectPath) {
    if let Ok(s) = Proxy::new(conn, PORTAL, session.clone(), SESSION) {
        let _ = s.call_method("Close", &());
    }
}

impl Drop for PortalCapturer {
    fn drop(&mut self) {
        self.stream.stop();
        if let Some(conn) = self.conn.take() {
            close_session(&conn, &self.session);
            let _ = conn.close();
        }
    }
}

impl ScreenCapturer for PortalCapturer {
    fn name(&self) -> &str {
        "portal-screencast"
    }

    fn display(&self) -> DisplayInfo {
        let (width, height) = self.stream.size();
        DisplayInfo { width, height }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        self.stream.next_frame(timeout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_restore_token_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("dalker-portal-token-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // No other test in this crate reads XDG_STATE_HOME.
        std::env::set_var("XDG_STATE_HOME", &dir);
        assert!(!sharing_allowed());
        save_token("first");
        save_token("second");
        assert_eq!(load_token().as_deref(), Some("second"));
        assert!(sharing_allowed());
        let mode = std::fs::metadata(token_path().unwrap())
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn output(x: i32, y: i32, width: u32, height: u32) -> OutputInfo {
        OutputInfo {
            name: None,
            x,
            y,
            width,
            height,
            pixels: (width, height),
        }
    }

    #[test]
    fn the_streamed_output_is_found_by_position_then_size() {
        let outputs = [output(0, 0, 1920, 1080), output(1920, 0, 1280, 800)];
        let found = |p, s| streamed_output(&outputs, p, s).map(|o| o.x);
        assert_eq!(found(Some((1920, 0)), Some((1280, 800))), Some(1920));
        assert_eq!(found(None, Some((1280, 800))), Some(1920));
        assert_eq!(found(None, None), None);
        let twins = [output(0, 0, 1920, 1080), output(1920, 0, 1920, 1080)];
        assert!(streamed_output(&twins, None, Some((1920, 1080))).is_none());
        let single = [output(0, 0, 1920, 1080)];
        assert!(streamed_output(&single, None, None).is_some());
    }

    #[test]
    fn streams_are_read_from_the_start_results() {
        let mut props = HashMap::new();
        props.insert(
            "position".to_string(),
            OwnedValue::try_from(Value::from((1920i32, 0i32))).unwrap(),
        );
        props.insert(
            "size".to_string(),
            OwnedValue::try_from(Value::from((1280i32, 800i32))).unwrap(),
        );
        let streams = vec![(42u32, props)];
        let mut results = Results::new();
        results.insert(
            "streams".to_string(),
            OwnedValue::try_from(Value::from(streams)).unwrap(),
        );
        let parsed = parse_streams(&results).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].node, 42);
        assert_eq!(parsed[0].position, Some((1920, 0)));
        assert_eq!(parsed[0].size, Some((1280, 800)));
        assert!(parse_streams(&Results::new()).is_err());
    }
}
