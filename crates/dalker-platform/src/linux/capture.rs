use std::time::{Duration, Instant};

use bytes::Bytes;
use dalker_core::{CaptureError, DisplayInfo, PixelFormat, RawFrame, ScreenCapturer};
use x11rb::connection::Connection;
use x11rb::protocol::shm::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat, ImageOrder, Window};
use x11rb::rust_connection::RustConnection;

const DEFAULT_MAX_FPS: u32 = 30;
/// How often to poll the root window size for xrandr changes.
const GEOMETRY_CHECK: Duration = Duration::from_secs(1);

/// Captures the whole X11 root window (all monitors of the X screen).
///
/// X11 has no cheap "something changed" signal without the DAMAGE extension,
/// so this backend polls at a fixed rate (default 30 FPS).
pub struct X11Capturer {
    conn: RustConnection,
    root: Window,
    width: u16,
    height: u16,
    shm: Option<ShmSegment>,
    start: Instant,
    frame_interval: Duration,
    next_deadline: Instant,
    last_geometry_check: Instant,
}

/// A SysV shared-memory segment attached to both us and the X server.
struct ShmSegment {
    seg: shm::Seg,
    addr: *mut u8,
    size: usize,
}

// SAFETY: the mapping is process-wide memory owned exclusively by the capturer.
unsafe impl Send for ShmSegment {}

fn backend<E: std::fmt::Display>(ctx: &'static str) -> impl FnOnce(E) -> CaptureError {
    move |e| CaptureError::Backend(format!("{ctx}: {e}"))
}

impl X11Capturer {
    /// Connects to `$DISPLAY` and prepares capture of the default screen.
    pub fn new() -> Result<Self, CaptureError> {
        if super::pipewire::PipeWireCapturer::session_is_wayland() {
            tracing::warn!(
                "Wayland session detected: X11 capture will only see XWayland windows. \
                 Log in with an Xorg session for full-desktop capture."
            );
        }
        let (conn, screen_num) = x11rb::connect(None).map_err(|e| {
            CaptureError::Unsupported(format!("cannot connect to X server (is $DISPLAY set?): {e}"))
        })?;
        let setup = conn.setup();
        let screen = &setup.roots[screen_num];
        let root = screen.root;
        let depth = screen.root_depth;

        if setup.image_byte_order != ImageOrder::LSB_FIRST {
            return Err(CaptureError::Unsupported("big-endian X server image order".into()));
        }
        let bpp = setup
            .pixmap_formats
            .iter()
            .find(|f| f.depth == depth)
            .map(|f| f.bits_per_pixel)
            .unwrap_or(0);
        if !(depth == 24 || depth == 32) || bpp != 32 {
            return Err(CaptureError::Unsupported(format!(
                "unsupported root visual: depth {depth}, {bpp} bits per pixel (need 24/32 @ 32 bpp)"
            )));
        }

        let geom = conn
            .get_geometry(root)
            .map_err(backend("GetGeometry"))?
            .reply()
            .map_err(backend("GetGeometry"))?;

        let mut cap = Self {
            conn,
            root,
            width: geom.width,
            height: geom.height,
            shm: None,
            start: Instant::now(),
            frame_interval: Duration::from_secs(1) / DEFAULT_MAX_FPS,
            next_deadline: Instant::now(),
            last_geometry_check: Instant::now(),
        };
        match cap.attach_shm() {
            Ok(seg) => cap.shm = Some(seg),
            Err(e) => tracing::warn!("MIT-SHM unavailable, falling back to GetImage (slower): {e}"),
        }
        tracing::info!(
            width = cap.width,
            height = cap.height,
            shm = cap.shm.is_some(),
            "X11 capture started"
        );
        Ok(cap)
    }

    /// Caps the polling rate.
    pub fn with_max_fps(mut self, fps: u32) -> Self {
        self.frame_interval = Duration::from_secs(1) / fps.clamp(1, 240);
        self
    }

    fn frame_size(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    fn attach_shm(&self) -> Result<ShmSegment, String> {
        let version = self
            .conn
            .shm_query_version()
            .map_err(|e| e.to_string())?
            .reply()
            .map_err(|e| e.to_string())?;
        tracing::debug!("MIT-SHM {}.{}", version.major_version, version.minor_version);

        let size = self.frame_size();
        unsafe {
            let shmid = libc::shmget(libc::IPC_PRIVATE, size, libc::IPC_CREAT | 0o600);
            if shmid < 0 {
                return Err(format!("shmget: {}", std::io::Error::last_os_error()));
            }
            let addr = libc::shmat(shmid, std::ptr::null(), 0);
            if addr as isize == -1 {
                let err = std::io::Error::last_os_error();
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                return Err(format!("shmat: {err}"));
            }
            let cleanup = |msg: String| {
                libc::shmdt(addr);
                libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
                msg
            };
            let seg = self.conn.generate_id().map_err(|e| cleanup(e.to_string()))?;
            self.conn
                .shm_attach(seg, shmid as u32, false)
                .map_err(|e| cleanup(e.to_string()))?
                .check()
                .map_err(|e| cleanup(format!("ShmAttach (remote X server?): {e}")))?;
            // Both sides are attached; mark for deletion so it is freed on detach/exit.
            libc::shmctl(shmid, libc::IPC_RMID, std::ptr::null_mut());
            Ok(ShmSegment { seg, addr: addr as *mut u8, size })
        }
    }

    fn detach_shm(&mut self) {
        if let Some(s) = self.shm.take() {
            let _ = self.conn.shm_detach(s.seg);
            let _ = self.conn.flush();
            unsafe {
                libc::shmdt(s.addr as *const libc::c_void);
            }
        }
    }

    fn check_geometry(&mut self) -> Result<(), CaptureError> {
        if self.last_geometry_check.elapsed() < GEOMETRY_CHECK {
            return Ok(());
        }
        self.last_geometry_check = Instant::now();
        let geom = self
            .conn
            .get_geometry(self.root)
            .map_err(backend("GetGeometry"))?
            .reply()
            .map_err(backend("GetGeometry"))?;
        if geom.width != self.width || geom.height != self.height {
            return Err(CaptureError::Lost(format!(
                "screen resized {}x{} -> {}x{}",
                self.width, self.height, geom.width, geom.height
            )));
        }
        Ok(())
    }

    fn grab(&self) -> Result<Vec<u8>, CaptureError> {
        let (w, h) = (self.width, self.height);
        if let Some(s) = &self.shm {
            self.conn
                .shm_get_image(self.root, 0, 0, w, h, !0, ImageFormat::Z_PIXMAP.into(), s.seg, 0)
                .map_err(backend("ShmGetImage"))?
                .reply()
                .map_err(backend("ShmGetImage"))?;
            // SAFETY: the server finished writing before replying; size matches geometry.
            let slice = unsafe { std::slice::from_raw_parts(s.addr, s.size) };
            Ok(slice.to_vec())
        } else {
            let reply = self
                .conn
                .get_image(ImageFormat::Z_PIXMAP, self.root, 0, 0, w, h, !0)
                .map_err(backend("GetImage"))?
                .reply()
                .map_err(backend("GetImage"))?;
            Ok(reply.data)
        }
    }
}

impl Drop for X11Capturer {
    fn drop(&mut self) {
        self.detach_shm();
    }
}

impl ScreenCapturer for X11Capturer {
    fn name(&self) -> &str {
        if self.shm.is_some() {
            "x11-shm"
        } else {
            "x11-getimage"
        }
    }

    fn display(&self) -> DisplayInfo {
        DisplayInfo { width: self.width as u32, height: self.height as u32 }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        let now = Instant::now();
        if self.next_deadline > now {
            let wait = self.next_deadline - now;
            if wait > timeout {
                std::thread::sleep(timeout);
                return Ok(None);
            }
            std::thread::sleep(wait);
        }
        self.next_deadline = (self.next_deadline + self.frame_interval).max(Instant::now());

        self.check_geometry()?;
        let data = self.grab()?;
        let stride = self.width as usize * 4;
        if data.len() < stride * self.height as usize {
            return Err(CaptureError::Lost(format!(
                "short image: {} bytes for {}x{}",
                data.len(),
                self.width,
                self.height
            )));
        }
        Ok(Some(RawFrame {
            width: self.width as u32,
            height: self.height as u32,
            stride,
            format: PixelFormat::Bgra8,
            data: Bytes::from(data),
            timestamp: self.start.elapsed(),
        }))
    }
}
