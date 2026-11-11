//! Wayland client pieces for desktops other than GNOME (KDE Plasma, sway, Hyprland...):
//!
//! * [`outputs`] reads the monitor layout (`wl_output` + `xdg-output`), so the absolute
//!   pointer, which spans every monitor, lands on the one that is streamed.
//! * [`LayerBorder`] draws the red "remote session" frame with `wlr-layer-shell`: four thin,
//!   click-through strips per monitor on the overlay layer, above every window and panel.
//!
//! This talks the Wayland protocol directly (no libwayland), so it needs `$WAYLAND_DISPLAY`
//! and `$XDG_RUNTIME_DIR` of the desktop session.

use std::io::Write;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use dalker_core::ViewerIndicator;
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_output, wl_region, wl_registry, wl_shm, wl_shm_pool, wl_surface,
};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle, WEnum};
use wayland_protocols::xdg::xdg_output::zv1::client::{zxdg_output_manager_v1, zxdg_output_v1};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};

use super::gnome::BORDER_INSET;

/// One monitor as the compositor lays it out.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutputInfo {
    pub name: Option<String>,
    /// Position and size in the compositor's (logical) coordinate space.
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    /// Size of the current mode in physical pixels, as the monitor shows it (rotated).
    pub pixels: (u32, u32),
}

/// What `wl_output` and `xdg_output` told us about one monitor.
#[derive(Debug, Default)]
struct RawOutput {
    name: Option<String>,
    position: (i32, i32),
    mode: Option<(i32, i32)>,
    transform: u32,
    scale: i32,
    logical_position: Option<(i32, i32)>,
    logical_size: Option<(i32, i32)>,
}

impl RawOutput {
    fn info(&self) -> Option<OutputInfo> {
        let (mw, mh) = self.mode?;
        // Odd transforms are 90° and 270°: the monitor shows the mode on its side.
        let (pw, ph) = if self.transform % 2 == 1 {
            (mh, mw)
        } else {
            (mw, mh)
        };
        let scale = self.scale.max(1);
        let (x, y) = self.logical_position.unwrap_or(self.position);
        let (w, h) = self
            .logical_size
            .filter(|(w, h)| *w > 0 && *h > 0)
            .unwrap_or((pw / scale, ph / scale));
        Some(OutputInfo {
            name: self.name.clone(),
            x,
            y,
            width: w.max(1) as u32,
            height: h.max(1) as u32,
            pixels: (pw.max(1) as u32, ph.max(1) as u32),
        })
    }
}

struct Output {
    global: u32,
    wl: wl_output::WlOutput,
    xdg: Option<zxdg_output_v1::ZxdgOutputV1>,
    raw: RawOutput,
}

/// One red strip along one edge of one monitor.
struct Strip {
    output: u32,
    surface: wl_surface::WlSurface,
    layer: zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
    /// The size asked for; 0 = as long as the monitor's edge.
    size: (u32, u32),
    buffer: Option<wl_buffer::WlBuffer>,
    /// Size of `buffer`.
    drawn: (u32, u32),
}

impl Strip {
    fn destroy(self) {
        self.layer.destroy();
        self.surface.destroy();
        if let Some(b) = self.buffer {
            b.destroy();
        }
    }
}

#[derive(Default)]
struct State {
    compositor: Option<wl_compositor::WlCompositor>,
    shm: Option<wl_shm::WlShm>,
    layer_shell: Option<zwlr_layer_shell_v1::ZwlrLayerShellV1>,
    xdg_manager: Option<zxdg_output_manager_v1::ZxdgOutputManagerV1>,
    outputs: Vec<Output>,
    strips: Vec<Strip>,
    /// Set when drawing a strip failed; the border is then reported unhealthy.
    failed: Option<String>,
}

impl State {
    fn connect() -> Result<(Connection, EventQueue<State>, State), String> {
        let conn = Connection::connect_to_env()
            .map_err(|e| format!("cannot connect to the Wayland compositor: {e}"))?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        // Globals, then the outputs' details (xdg-output may be announced after them).
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        state.add_xdg_outputs(&qh);
        queue.roundtrip(&mut state).map_err(|e| e.to_string())?;
        Ok((conn, queue, state))
    }

    fn add_xdg_outputs(&mut self, qh: &QueueHandle<State>) {
        let Some(manager) = &self.xdg_manager else {
            return;
        };
        for o in self.outputs.iter_mut().filter(|o| o.xdg.is_none()) {
            o.xdg = Some(manager.get_xdg_output(&o.wl, qh, o.global));
        }
    }

    fn infos(&self) -> Vec<OutputInfo> {
        self.outputs.iter().filter_map(|o| o.raw.info()).collect()
    }

    fn output_mut(&mut self, global: u32) -> Option<&mut RawOutput> {
        self.outputs
            .iter_mut()
            .find(|o| o.global == global)
            .map(|o| &mut o.raw)
    }

    /// Adds the four strips of every monitor that has none yet.
    fn show(&mut self, qh: &QueueHandle<State>) {
        use zwlr_layer_surface_v1::Anchor;
        let (Some(compositor), Some(layer_shell)) = (&self.compositor, &self.layer_shell) else {
            return;
        };
        let t = BORDER_INSET;
        let edges = [
            (Anchor::Top | Anchor::Left | Anchor::Right, (0, t)),
            (Anchor::Bottom | Anchor::Left | Anchor::Right, (0, t)),
            (Anchor::Left | Anchor::Top | Anchor::Bottom, (t, 0)),
            (Anchor::Right | Anchor::Top | Anchor::Bottom, (t, 0)),
        ];
        for o in &self.outputs {
            if self.strips.iter().any(|s| s.output == o.global) {
                continue;
            }
            for (anchor, size) in edges {
                let surface = compositor.create_surface(qh, ());
                // Clicks go through to whatever is under the frame.
                let region = compositor.create_region(qh, ());
                surface.set_input_region(Some(&region));
                region.destroy();
                let layer = layer_shell.get_layer_surface(
                    &surface,
                    Some(&o.wl),
                    zwlr_layer_shell_v1::Layer::Overlay,
                    "dalker-border".into(),
                    qh,
                    (),
                );
                layer.set_anchor(anchor);
                layer.set_size(size.0, size.1);
                // -1: lie on the very edge, over panels, and push nothing aside.
                layer.set_exclusive_zone(-1);
                layer
                    .set_keyboard_interactivity(zwlr_layer_surface_v1::KeyboardInteractivity::None);
                surface.commit();
                self.strips.push(Strip {
                    output: o.global,
                    surface,
                    layer,
                    size,
                    buffer: None,
                    drawn: (0, 0),
                });
            }
        }
    }

    fn hide(&mut self) {
        for s in self.strips.drain(..) {
            s.destroy();
        }
    }

    /// Draws a configured strip `width`x`height` surface pixels big.
    fn draw(
        &mut self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        w: u32,
        h: u32,
        qh: &QueueHandle<State>,
    ) {
        let strip = self.strips.iter().position(|s| &s.layer == layer);
        let (Some(i), Some(shm)) = (strip, self.shm.clone()) else {
            return;
        };
        let strip = &mut self.strips[i];
        let w = if w == 0 { strip.size.0.max(1) } else { w };
        let h = if h == 0 { strip.size.1.max(1) } else { h };
        // Compositors re-send the same configure whenever the layout is rearranged; the
        // acknowledgement is enough then.
        if strip.buffer.is_some() && strip.drawn == (w, h) {
            return;
        }
        let buffer = match red_buffer(&shm, qh, w, h) {
            Ok(b) => b,
            Err(e) => {
                self.failed = Some(e);
                return;
            }
        };
        strip.surface.attach(Some(&buffer), 0, 0);
        strip.surface.damage(0, 0, w as i32, h as i32);
        strip.surface.commit();
        strip.drawn = (w, h);
        if let Some(old) = strip.buffer.replace(buffer) {
            old.destroy();
        }
    }
}

/// An opaque red ARGB8888 buffer. The pool and its memory are released as soon as the
/// buffer has been created; the buffer keeps the mapping alive in the compositor.
fn red_buffer(
    shm: &wl_shm::WlShm,
    qh: &QueueHandle<State>,
    w: u32,
    h: u32,
) -> Result<wl_buffer::WlBuffer, String> {
    let stride = w as usize * 4;
    let len = stride * h as usize;
    // SAFETY: memfd_create takes a NUL-terminated name and returns a new fd or -1.
    let fd = unsafe { libc::memfd_create(c"dalker-border".as_ptr(), libc::MFD_CLOEXEC) };
    if fd < 0 {
        return Err(format!("memfd_create: {}", std::io::Error::last_os_error()));
    }
    // SAFETY: fd is a fresh descriptor that nothing else owns.
    let fd = unsafe { OwnedFd::from_raw_fd(fd) };
    // Little-endian ARGB8888 = B, G, R, A in memory.
    let pixels: Vec<u8> = [0x00, 0x00, 0xFF, 0xFF].repeat(w as usize * h as usize);
    let mut file = std::fs::File::from(fd);
    file.write_all(&pixels)
        .map_err(|e| format!("write border buffer: {e}"))?;
    let pool = shm.create_pool(file.as_fd(), len as i32, qh, ());
    let buffer = pool.create_buffer(
        0,
        w as i32,
        h as i32,
        stride as i32,
        wl_shm::Format::Argb8888,
        qh,
        (),
    );
    pool.destroy();
    Ok(buffer)
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if interface == wl_compositor::WlCompositor::interface().name {
                    state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
                } else if interface == wl_shm::WlShm::interface().name {
                    state.shm = Some(registry.bind(name, 1, qh, ()));
                } else if interface == zwlr_layer_shell_v1::ZwlrLayerShellV1::interface().name {
                    state.layer_shell = Some(registry.bind(name, version.min(4), qh, ()));
                } else if interface == zxdg_output_manager_v1::ZxdgOutputManagerV1::interface().name
                {
                    state.xdg_manager = Some(registry.bind(name, version.min(3), qh, ()));
                    state.add_xdg_outputs(qh);
                } else if interface == wl_output::WlOutput::interface().name {
                    let wl = registry.bind(name, version.min(4), qh, name);
                    state.outputs.push(Output {
                        global: name,
                        wl,
                        xdg: None,
                        raw: RawOutput {
                            scale: 1,
                            ..Default::default()
                        },
                    });
                    state.add_xdg_outputs(qh);
                }
            }
            wl_registry::Event::GlobalRemove { name } => {
                if let Some(i) = state.outputs.iter().position(|o| o.global == name) {
                    let o = state.outputs.remove(i);
                    if let Some(x) = o.xdg {
                        x.destroy();
                    }
                    if o.wl.version() >= 3 {
                        o.wl.release();
                    }
                    let (gone, keep) = state.strips.drain(..).partition(|s| s.output == name);
                    state.strips = keep;
                    for s in gone {
                        Strip::destroy(s);
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_output::WlOutput, u32> for State {
    fn event(
        state: &mut Self,
        _: &wl_output::WlOutput,
        event: wl_output::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else {
            return;
        };
        match event {
            wl_output::Event::Geometry {
                x, y, transform, ..
            } => {
                o.position = (x, y);
                o.transform = match transform {
                    WEnum::Value(t) => t as u32,
                    WEnum::Unknown(t) => t,
                };
            }
            wl_output::Event::Mode {
                flags: WEnum::Value(flags),
                width,
                height,
                ..
            } if flags.contains(wl_output::Mode::Current) => o.mode = Some((width, height)),
            wl_output::Event::Scale { factor } => o.scale = factor,
            wl_output::Event::Name { name } => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<zxdg_output_v1::ZxdgOutputV1, u32> for State {
    fn event(
        state: &mut Self,
        _: &zxdg_output_v1::ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        global: &u32,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Some(o) = state.output_mut(*global) else {
            return;
        };
        match event {
            zxdg_output_v1::Event::LogicalPosition { x, y } => o.logical_position = Some((x, y)),
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                o.logical_size = Some((width, height))
            }
            zxdg_output_v1::Event::Name { name } if o.name.is_none() => o.name = Some(name),
            _ => {}
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer.ack_configure(serial);
                state.draw(layer, width, height, qh);
            }
            // The monitor went away (or the compositor dropped the surface): forget the
            // strip. A monitor that is still there gets new strips on the next pass.
            zwlr_layer_surface_v1::Event::Closed => {
                if let Some(i) = state.strips.iter().position(|s| &s.layer == layer) {
                    state.strips.remove(i).destroy();
                }
            }
            _ => {}
        }
    }
}

/// Objects without events we care about.
macro_rules! ignore_events {
    ($($t:ty),*) => {$(
        impl Dispatch<$t, ()> for State {
            fn event(
                _: &mut Self,
                _: &$t,
                _: <$t as Proxy>::Event,
                _: &(),
                _: &Connection,
                _: &QueueHandle<Self>,
            ) {
            }
        }
    )*};
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm::WlShm,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_region::WlRegion,
    wl_surface::WlSurface,
    zwlr_layer_shell_v1::ZwlrLayerShellV1,
    zxdg_output_manager_v1::ZxdgOutputManagerV1
);

/// The monitors of the Wayland desktop this process runs in.
pub fn outputs() -> Result<Vec<OutputInfo>, String> {
    let (_conn, _queue, state) = State::connect()?;
    Ok(state.infos())
}

// ---------------------------------------------------------------------------- border

/// Draws the red frame around every monitor while a viewer is connected, through
/// `wlr-layer-shell` (KDE Plasma, sway, Hyprland, and other wlroots compositors; not GNOME).
///
/// A thread owns the Wayland connection. If the agent dies, the compositor drops the
/// connection and the frame with it, so it can never be left behind.
pub struct LayerBorder {
    active: Arc<AtomicBool>,
    alive: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl LayerBorder {
    pub fn new() -> Result<Self, String> {
        let active = Arc::new(AtomicBool::new(false));
        let alive = Arc::new(AtomicBool::new(true));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (a, l, s) = (Arc::clone(&active), Arc::clone(&alive), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("dalker-border".into())
            .spawn(move || {
                let (conn, mut queue, mut state) = match State::connect() {
                    Ok(c) => c,
                    Err(e) => {
                        l.store(false, Ordering::SeqCst);
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if state.layer_shell.is_none() || state.compositor.is_none() || state.shm.is_none()
                {
                    l.store(false, Ordering::SeqCst);
                    let _ = ready_tx.send(Err(
                        "the Wayland compositor has no wlr-layer-shell, so the red border can't be drawn"
                            .into(),
                    ));
                    return;
                }
                let _ = ready_tx.send(Ok(()));
                if let Err(e) = run_border(&conn, &mut queue, &mut state, &a, &s) {
                    tracing::warn!("red border: {e}");
                }
                l.store(false, Ordering::SeqCst);
            })
            .map_err(|e| format!("spawn border thread: {e}"))?;
        let mut border = Self {
            active,
            alive,
            stop,
            thread: Some(thread),
        };
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(border),
            Ok(Err(e)) => Err(e),
            Err(_) => {
                border.stop.store(true, Ordering::SeqCst);
                border.thread.take();
                Err("the Wayland compositor did not answer".into())
            }
        }
    }
}

fn run_border(
    conn: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    active: &AtomicBool,
    stop: &AtomicBool,
) -> Result<(), String> {
    let qh = queue.handle();
    loop {
        if stop.load(Ordering::SeqCst) {
            state.hide();
            let _ = conn.flush();
            return Ok(());
        }
        if let Some(e) = state.failed.take() {
            state.hide();
            let _ = conn.flush();
            return Err(e);
        }
        if active.load(Ordering::SeqCst) {
            state.show(&qh);
        } else if !state.strips.is_empty() {
            state.hide();
        }
        queue
            .flush()
            .map_err(|e| format!("Wayland connection lost: {e}"))?;
        if let Some(guard) = queue.prepare_read() {
            let mut pfd = libc::pollfd {
                fd: guard.connection_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd for the duration of the call.
            let n = unsafe { libc::poll(&mut pfd, 1, 100) };
            if n > 0 {
                guard
                    .read()
                    .map_err(|e| format!("Wayland connection lost: {e}"))?;
            }
        }
        queue
            .dispatch_pending(state)
            .map_err(|e| format!("Wayland connection lost: {e}"))?;
    }
}

impl ViewerIndicator for LayerBorder {
    fn name(&self) -> &str {
        "wayland-layer-shell-border"
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    fn healthy(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

impl Drop for LayerBorder {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_info_prefers_xdg_output_and_rotates_the_mode() {
        let raw = RawOutput {
            name: Some("DP-1".into()),
            position: (0, 0),
            mode: Some((2560, 1440)),
            transform: 0,
            scale: 2,
            logical_position: Some((1920, 0)),
            logical_size: Some((1280, 720)),
        };
        let info = raw.info().unwrap();
        assert_eq!(
            (info.x, info.y, info.width, info.height),
            (1920, 0, 1280, 720)
        );
        assert_eq!(info.pixels, (2560, 1440));

        // Without xdg-output: wl_output's position, and the mode divided by the scale.
        let rotated = RawOutput {
            mode: Some((1920, 1080)),
            transform: 1,
            scale: 1,
            position: (-1080, 0),
            ..Default::default()
        };
        let info = rotated.info().unwrap();
        assert_eq!((info.x, info.width, info.height), (-1080, 1080, 1920));
        assert_eq!(info.pixels, (1080, 1920));

        assert!(RawOutput::default().info().is_none(), "no mode yet");
    }
}
