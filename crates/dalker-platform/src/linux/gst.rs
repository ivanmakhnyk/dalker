//! Reads a PipeWire screen-cast stream through a `gst-launch-1.0 pipewiresrc` child process
//! (so no libpipewire headers are needed to build). Shared by the GNOME screen cast and the
//! xdg-desktop-portal one.

use std::io::Read;
use std::os::fd::{AsRawFd, OwnedFd};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use dalker_core::{CaptureError, PixelFormat, RawFrame};

/// The fd number the portal's PipeWire connection gets in the `gst-launch-1.0` child.
const CHILD_PW_FD: i32 = 3;

pub(super) fn backend(ctx: &str, e: impl std::fmt::Display) -> CaptureError {
    CaptureError::Backend(format!("{ctx}: {e}"))
}

/// Fails with [`CaptureError::Unsupported`] when `gst-launch-1.0` is not installed.
pub(super) fn check_installed() -> Result<(), CaptureError> {
    match Command::new("gst-launch-1.0")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
    {
        Ok(_) => Ok(()),
        Err(_) => Err(CaptureError::Unsupported(
            "gst-launch-1.0 not found (install gstreamer1.0-tools and gstreamer1.0-pipewire)"
                .into(),
        )),
    }
}

struct Shared {
    /// (sequence, latest frame bytes). The reader overwrites, so slow consumers see fresh frames.
    latest: Mutex<(u64, Option<Bytes>)>,
    cond: Condvar,
    closed: AtomicBool,
    /// Set when we stop the stream ourselves, so its end is not reported as the desktop's.
    stopping: AtomicBool,
}

/// Lets another thread (a D-Bus signal watcher) end the stream.
#[derive(Clone)]
pub(super) struct StreamCloser(Arc<Shared>);

impl StreamCloser {
    /// Marks the stream as ended: the next [`GstStream::next_frame`] returns
    /// [`CaptureError::Lost`]. Logs `why` unless the stream is being stopped on purpose.
    pub fn close(&self, why: &str) {
        if !self.0.stopping.load(Ordering::SeqCst) {
            tracing::info!("{why}");
        }
        self.0.closed.store(true, Ordering::SeqCst);
        let _g = self.0.latest.lock().unwrap();
        self.0.cond.notify_all();
    }
}

/// A running `pipewiresrc ! videoconvert ! videocrop ! videoscale` pipeline that writes
/// fixed-size BGRx frames to a pipe.
pub(super) struct GstStream {
    child: Child,
    reader: Option<JoinHandle<()>>,
    shared: Arc<Shared>,
    size: (u32, u32),
    seen: u64,
    start: Instant,
}

impl GstStream {
    /// Starts reading PipeWire node `node`, from the PipeWire connection `fd` when given (the
    /// portal's) or from the user's default PipeWire daemon. `crop` stream pixels are cut from
    /// every edge, and frames are scaled to `width`x`height` (even numbers), so the output size
    /// never changes mid-stream: the stride is always `width * 4`.
    pub fn spawn(
        node: u32,
        fd: Option<OwnedFd>,
        crop: u32,
        width: u32,
        height: u32,
    ) -> Result<Self, CaptureError> {
        let mut cmd = Command::new("gst-launch-1.0");
        cmd.args(["-q", "pipewiresrc"]);
        if fd.is_some() {
            cmd.arg(format!("fd={CHILD_PW_FD}"));
        }
        cmd.arg(format!("path={node}"))
            .args(["do-timestamp=true", "!", "videoconvert", "!", "videocrop"])
            .args([
                format!("top={crop}"),
                format!("bottom={crop}"),
                format!("left={crop}"),
                format!("right={crop}"),
            ])
            .args(["!", "videoscale", "!"])
            .arg(format!(
                "video/x-raw,format=BGRx,width={width},height={height}"
            ))
            .args(["!", "fdsink", "fd=1"])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let raw_fd = fd.as_ref().map(|f| f.as_raw_fd());
        // If the agent dies without dropping the capturer (killed, crashed), take the
        // pipeline with it: an orphaned reader keeps the screen cast busy.
        // SAFETY: prctl, dup2 and fcntl are async-signal-safe and touch no memory of the parent.
        unsafe {
            std::os::unix::process::CommandExt::pre_exec(&mut cmd, move || {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
                if let Some(src) = raw_fd {
                    // dup2 clears close-on-exec on the copy, but does nothing when the
                    // numbers are equal: then clear it by hand.
                    if src == CHILD_PW_FD {
                        let flags = libc::fcntl(src, libc::F_GETFD);
                        if flags < 0
                            || libc::fcntl(src, libc::F_SETFD, flags & !libc::FD_CLOEXEC) < 0
                        {
                            return Err(std::io::Error::last_os_error());
                        }
                    } else if libc::dup2(src, CHILD_PW_FD) < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                }
                Ok(())
            });
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| backend("spawn gst-launch-1.0", e))?;
        // The child has its own copy now.
        drop(fd);
        let mut stdout = child.stdout.take().expect("piped stdout");

        let shared = Arc::new(Shared {
            latest: Mutex::new((0, None)),
            cond: Condvar::new(),
            closed: AtomicBool::new(false),
            stopping: AtomicBool::new(false),
        });
        let frame_len = width as usize * height as usize * 4;
        let sh = Arc::clone(&shared);
        let reader = std::thread::Builder::new()
            .name("dalker-gst-reader".into())
            .spawn(move || {
                let mut buf = vec![0u8; frame_len];
                while stdout.read_exact(&mut buf).is_ok() {
                    let mut g = sh.latest.lock().unwrap();
                    g.0 += 1;
                    g.1 = Some(Bytes::copy_from_slice(&buf));
                    sh.cond.notify_all();
                }
                sh.closed.store(true, Ordering::SeqCst);
                sh.cond.notify_all();
            });
        let reader = match reader {
            Ok(r) => r,
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(backend("spawn reader", e));
            }
        };
        Ok(Self {
            child,
            reader: Some(reader),
            shared,
            size: (width, height),
            seen: 0,
            start: Instant::now(),
        })
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn closer(&self) -> StreamCloser {
        StreamCloser(Arc::clone(&self.shared))
    }

    /// Stops the pipeline. Call before ending the screen-cast session, so the session's end
    /// is not reported as the desktop's.
    pub fn stop(&mut self) {
        self.shared.stopping.store(true, Ordering::SeqCst);
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
    }

    pub fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        let guard = self.shared.latest.lock().unwrap();
        let (guard, _) = self
            .shared
            .cond
            .wait_timeout_while(guard, timeout, |g| {
                g.0 == self.seen && !self.shared.closed.load(Ordering::SeqCst)
            })
            .unwrap();
        if guard.0 == self.seen {
            return if self.shared.closed.load(Ordering::SeqCst) {
                Err(CaptureError::Lost("the screen-cast stream ended".into()))
            } else {
                Ok(None)
            };
        }
        self.seen = guard.0;
        let data = guard.1.clone().expect("sequence advanced");
        drop(guard);
        Ok(Some(RawFrame {
            width: self.size.0,
            height: self.size.1,
            stride: self.size.0 as usize * 4,
            format: PixelFormat::Bgra8,
            data,
            timestamp: self.start.elapsed(),
        }))
    }
}

impl Drop for GstStream {
    fn drop(&mut self) {
        self.stop();
    }
}
