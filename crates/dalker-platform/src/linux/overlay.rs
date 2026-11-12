use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use dalker_core::ViewerIndicator;
use x11rb::connection::Connection;
use x11rb::protocol::shape::{self, ConnectionExt as _, SK, SO};
use x11rb::protocol::xproto::{
    ClipOrdering, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, Rectangle, StackMode,
    WindowClass,
};
use x11rb::rust_connection::RustConnection;

/// Frame thickness in pixels.
const BORDER_PX: u16 = 5;
const RED: u32 = 0x00FF_0000;
const TICK: Duration = Duration::from_millis(100);

/// Red frame around the X screen, shown while a viewer is connected.
///
/// An override-redirect window (no decorations, ignores the window manager)
/// whose bounding shape is a frame and whose input shape is empty, so mouse
/// events pass straight through to the windows below.
pub struct BorderOverlay {
    active: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BorderOverlay {
    pub fn new() -> Result<Self, String> {
        let active = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (a, s) = (Arc::clone(&active), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("dalker-border".into())
            .spawn(move || run(a, s, ready_tx))
            .map_err(|e| format!("spawn overlay thread: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                active,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("overlay thread did not start".into()),
        }
    }
}

impl ViewerIndicator for BorderOverlay {
    fn name(&self) -> &str {
        "x11-border"
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }
}

impl Drop for BorderOverlay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn frame_rects(w: u16, h: u16) -> [Rectangle; 4] {
    let t = BORDER_PX.min(w / 2).min(h / 2);
    let side_h = h.saturating_sub(2 * t);
    [
        Rectangle {
            x: 0,
            y: 0,
            width: w,
            height: t,
        },
        Rectangle {
            x: 0,
            y: (h - t) as i16,
            width: w,
            height: t,
        },
        Rectangle {
            x: 0,
            y: t as i16,
            width: t,
            height: side_h,
        },
        Rectangle {
            x: (w - t) as i16,
            y: t as i16,
            width: t,
            height: side_h,
        },
    ]
}

fn apply_shape(
    conn: &RustConnection,
    win: u32,
    w: u16,
    h: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    conn.shape_rectangles(
        SO::SET,
        SK::BOUNDING,
        ClipOrdering::UNSORTED,
        win,
        0,
        0,
        &frame_rects(w, h),
    )?;
    // Empty input region: the window never receives pointer events.
    conn.shape_rectangles(SO::SET, SK::INPUT, ClipOrdering::UNSORTED, win, 0, 0, &[])?;
    Ok(())
}

struct Setup {
    conn: RustConnection,
    win: u32,
    root: u32,
    size: (u16, u16),
}

fn setup() -> Result<Setup, String> {
    let e = |ctx: &'static str| move |err: Box<dyn std::error::Error>| format!("{ctx}: {err}");
    let (conn, screen_num) =
        x11rb::connect(None).map_err(|err| format!("connect to X server: {err}"))?;
    let (root, root_visual, root_depth) = {
        let s = &conn.setup().roots[screen_num];
        (s.root, s.root_visual, s.root_depth)
    };
    shape::query_version(&conn)
        .map_err(|err| format!("X SHAPE extension: {err}"))?
        .reply()
        .map_err(|err| format!("X SHAPE extension missing: {err}"))?;
    let geom = conn
        .get_geometry(root)
        .map_err(|err| format!("GetGeometry: {err}"))?
        .reply()
        .map_err(|err| format!("GetGeometry: {err}"))?;
    let (w, h) = (geom.width, geom.height);

    let win = conn
        .generate_id()
        .map_err(|err| format!("generate id: {err}"))?;
    conn.create_window(
        root_depth,
        win,
        root,
        0,
        0,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        root_visual,
        &CreateWindowAux::new()
            .override_redirect(1)
            .background_pixel(RED),
    )
    .map_err(|err| format!("CreateWindow: {err}"))?;
    apply_shape(&conn, win, w, h).map_err(e("shape"))?;
    conn.flush().map_err(|err| format!("flush: {err}"))?;
    Ok(Setup {
        conn,
        win,
        root,
        size: (w, h),
    })
}

fn run(active: Arc<AtomicBool>, stop: Arc<AtomicBool>, ready: mpsc::Sender<Result<(), String>>) {
    let Setup {
        conn,
        win,
        root,
        mut size,
    } = match setup() {
        Ok(s) => {
            let _ = ready.send(Ok(()));
            s
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };

    let mut shown = false;
    let mut tick = 0u32;
    while !stop.load(Ordering::SeqCst) {
        let want = active.load(Ordering::SeqCst);
        if want != shown {
            let _ = if want {
                conn.map_window(win)
            } else {
                conn.unmap_window(win)
            };
            shown = want;
        }
        if shown {
            // Stay above everything, including other override-redirect windows.
            let _ =
                conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
        }
        // Follow resolution changes about once a second.
        tick = tick.wrapping_add(1);
        if tick.is_multiple_of(10) {
            if let Ok(Ok(g)) = conn.get_geometry(root).map(|c| c.reply()) {
                if (g.width, g.height) != size && g.width > 0 && g.height > 0 {
                    size = (g.width, g.height);
                    let _ = conn.configure_window(
                        win,
                        &ConfigureWindowAux::new()
                            .width(size.0 as u32)
                            .height(size.1 as u32),
                    );
                    let _ = apply_shape(&conn, win, size.0, size.1);
                }
            }
        }
        let _ = conn.flush();
        std::thread::sleep(TICK);
    }
    let _ = conn.destroy_window(win);
    let _ = conn.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_covers_edges_not_centre() {
        let rects = frame_rects(100, 60);
        let covered = |x: i32, y: i32| {
            rects.iter().any(|r| {
                x >= r.x as i32
                    && x < r.x as i32 + r.width as i32
                    && y >= r.y as i32
                    && y < r.y as i32 + r.height as i32
            })
        };
        for (x, y) in [
            (0, 0),
            (99, 0),
            (0, 59),
            (99, 59),
            (50, 0),
            (50, 59),
            (0, 30),
            (99, 30),
        ] {
            assert!(covered(x, y), "({x},{y}) should be red");
        }
        assert!(!covered(50, 30));
        assert!(!covered(BORDER_PX as i32, BORDER_PX as i32));
    }
}
