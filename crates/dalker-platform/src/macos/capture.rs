//! Screen capture with ScreenCaptureKit: an `SCStream` on the main display delivers BGRA
//! frames on its own dispatch queue; [`SckCapturer::next_frame`] waits for the latest one.
//!
//! ScreenCaptureKit only sends a frame when the picture changed, which matches the
//! `Ok(None)` = "nothing new" contract of [`ScreenCapturer::next_frame`].

use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use block2::RcBlock;
use bytes::Bytes;
use dalker_core::{CaptureError, DisplayInfo, PixelFormat, RawFrame, ScreenCapturer};
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_graphics::{CGDisplayMode, CGMainDisplayID};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_core_video::{
    kCVPixelFormatType_32BGRA, CVPixelBufferGetBaseAddress, CVPixelBufferGetBytesPerRow,
    CVPixelBufferGetHeight, CVPixelBufferGetWidth, CVPixelBufferLockBaseAddress,
    CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamDelegate, SCStreamOutput, SCStreamOutputType,
};

use super::permissions;

/// How long to wait for ScreenCaptureKit to answer a request (list displays, start).
const SCK_TIMEOUT: Duration = Duration::from_secs(5);

/// The newest frame, or why the stream stopped; filled on the capture queue.
#[derive(Default)]
struct Slot {
    frame: Option<RawFrame>,
    stopped: Option<String>,
}

struct Shared {
    slot: Mutex<Slot>,
    changed: Condvar,
    start: Instant,
}

impl Shared {
    fn put(&self, update: impl FnOnce(&mut Slot)) {
        update(&mut self.slot.lock().unwrap());
        self.changed.notify_all();
    }
}

struct OutputIvars {
    shared: Arc<Shared>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements, and the class has no Drop impl.
    #[unsafe(super(NSObject))]
    #[name = "DalkerStreamOutput"]
    #[ivars = OutputIvars]
    struct StreamOutput;

    unsafe impl NSObjectProtocol for StreamOutput {}

    unsafe impl SCStreamOutput for StreamOutput {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn stream_did_output(
            &self,
            _stream: &SCStream,
            sample: &CMSampleBuffer,
            kind: SCStreamOutputType,
        ) {
            if kind != SCStreamOutputType::Screen {
                return;
            }
            let shared = &self.ivars().shared;
            // Idle frames (nothing changed) carry no image.
            if let Some(frame) = copy_frame(sample, shared.start) {
                shared.put(|s| s.frame = Some(frame));
            }
        }
    }

    unsafe impl SCStreamDelegate for StreamOutput {
        #[unsafe(method(stream:didStopWithError:))]
        fn stream_did_stop(&self, _stream: &SCStream, error: &NSError) {
            let why = error.localizedDescription().to_string();
            self.ivars().shared.put(|s| s.stopped = Some(why));
        }
    }
);

impl StreamOutput {
    fn new(shared: Arc<Shared>) -> Retained<Self> {
        let this = Self::alloc().set_ivars(OutputIvars { shared });
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    }
}

/// Copies the BGRA pixels out of a sample buffer, or `None` for a frame without an image.
fn copy_frame(sample: &CMSampleBuffer, start: Instant) -> Option<RawFrame> {
    // SAFETY: the sample buffer is valid for the duration of the callback.
    let image = unsafe { sample.image_buffer() }?;
    let lock = CVPixelBufferLockFlags::ReadOnly;
    // SAFETY: a valid pixel buffer; unlocked below with the same flags.
    if unsafe { CVPixelBufferLockBaseAddress(&image, lock) } != 0 {
        return None;
    }
    let width = CVPixelBufferGetWidth(&image);
    let height = CVPixelBufferGetHeight(&image);
    let stride = CVPixelBufferGetBytesPerRow(&image);
    let base = CVPixelBufferGetBaseAddress(&image);
    let data = (!base.is_null()).then(|| {
        // SAFETY: while locked, `base` points at `stride * height` readable bytes.
        Bytes::copy_from_slice(unsafe {
            std::slice::from_raw_parts(base as *const u8, stride * height)
        })
    });
    // SAFETY: matches the lock above.
    unsafe { CVPixelBufferUnlockBaseAddress(&image, lock) };
    Some(RawFrame {
        width: width as u32,
        height: height as u32,
        stride,
        format: PixelFormat::Bgra8,
        data: data?,
        timestamp: start.elapsed(),
    })
}

/// Moves an Objective-C object across threads; only for handing a result back from a
/// completion handler.
struct Handoff<T>(T);
// SAFETY: the value is only moved to the waiting thread, never shared.
unsafe impl<T> Send for Handoff<T> {}

/// The display with the menu bar, as ScreenCaptureKit knows it.
fn main_display() -> Result<Retained<SCDisplay>, CaptureError> {
    let (tx, rx) = mpsc::channel();
    let handler = RcBlock::new(
        move |content: *mut SCShareableContent, error: *mut NSError| {
            // SAFETY: ScreenCaptureKit passes either a valid content object or a valid error.
            let result = match unsafe { (content.as_ref(), error.as_ref()) } {
                (Some(content), _) => {
                    let main = CGMainDisplayID();
                    // SAFETY: plain property reads.
                    let displays = unsafe { content.displays() }.to_vec();
                    let display = displays
                        .iter()
                        .find(|d| unsafe { d.displayID() } == main)
                        .or(displays.first())
                        .cloned();
                    display
                        .map(Handoff)
                        .ok_or_else(|| "no display to capture".to_string())
                }
                (None, Some(error)) => Err(error.localizedDescription().to_string()),
                (None, None) => Err("ScreenCaptureKit returned nothing".to_string()),
            };
            let _ = tx.send(result);
        },
    );
    // SAFETY: the handler matches the documented signature.
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&handler) };
    match rx.recv_timeout(SCK_TIMEOUT) {
        Ok(Ok(Handoff(display))) => Ok(display),
        Ok(Err(why)) if !permissions::screen_recording(false) => Err(CaptureError::Unsupported(
            format!("{} ({why})", permissions::SCREEN_RECORDING_MISSING),
        )),
        Ok(Err(why)) => Err(CaptureError::Backend(why)),
        Err(_) => Err(CaptureError::Backend(
            "ScreenCaptureKit did not list the displays".into(),
        )),
    }
}

/// Physical pixel size of a display (points × backing scale on Retina screens).
fn pixel_size(display: &SCDisplay) -> (usize, usize) {
    // SAFETY: plain property reads.
    let (id, points_w, points_h) =
        unsafe { (display.displayID(), display.width(), display.height()) };
    let mode = objc2_core_graphics::CGDisplayCopyDisplayMode(id);
    let pixels = mode.as_deref().map(|m| {
        (
            CGDisplayMode::pixel_width(Some(m)),
            CGDisplayMode::pixel_height(Some(m)),
        )
    });
    match pixels {
        Some((w, h)) if w > 0 && h > 0 => (w, h),
        _ => (points_w.max(1) as usize, points_h.max(1) as usize),
    }
}

/// The main display through an `SCStream`, at physical resolution, cursor included.
pub struct SckCapturer {
    stream: Retained<SCStream>,
    // Kept alive while the stream may call them.
    _output: Retained<StreamOutput>,
    _queue: DispatchRetained<DispatchQueue>,
    shared: Arc<Shared>,
    display: DisplayInfo,
}

// SAFETY: SCStream may be started and stopped from any thread; the output object is only
// touched by ScreenCaptureKit on its queue, and the rest is Send.
unsafe impl Send for SckCapturer {}

impl SckCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        if !permissions::screen_recording(true) {
            return Err(CaptureError::Unsupported(
                permissions::SCREEN_RECORDING_MISSING.into(),
            ));
        }
        let display = main_display()?;
        let (width, height) = pixel_size(&display);

        // SAFETY: configuring a fresh configuration object.
        let config = unsafe { SCStreamConfiguration::new() };
        unsafe {
            config.setWidth(width);
            config.setHeight(height);
            config.setPixelFormat(kCVPixelFormatType_32BGRA);
            config.setShowsCursor(true);
            config.setQueueDepth(4);
            // At most 120 frames per second; the encoder paces itself below that.
            config.setMinimumFrameInterval(CMTime::new(1, 120));
        }
        // SAFETY: a fresh filter for a display ScreenCaptureKit just listed.
        let filter = unsafe {
            SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            )
        };

        let shared = Arc::new(Shared {
            slot: Mutex::new(Slot::default()),
            changed: Condvar::new(),
            start: Instant::now(),
        });
        let output = StreamOutput::new(Arc::clone(&shared));
        let delegate = ProtocolObject::<dyn SCStreamDelegate>::from_ref(&*output);
        // SAFETY: valid filter, configuration and delegate (kept alive in `_output`).
        let stream = unsafe {
            SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(delegate),
            )
        };
        let queue = DispatchQueue::new("io.github.ivanmakhnyk.dalker.capture", None);
        let sink = ProtocolObject::<dyn SCStreamOutput>::from_ref(&*output);
        // SAFETY: the output and queue outlive the stream (both are kept in `Self`).
        unsafe {
            stream.addStreamOutput_type_sampleHandlerQueue_error(
                sink,
                SCStreamOutputType::Screen,
                Some(&queue),
            )
        }
        .map_err(|e| CaptureError::Backend(e.localizedDescription().to_string()))?;

        let (tx, rx) = mpsc::channel();
        let started = RcBlock::new(move |error: *mut NSError| {
            // SAFETY: null or a valid error.
            let result = unsafe { error.as_ref() }.map(|e| e.localizedDescription().to_string());
            let _ = tx.send(result);
        });
        // SAFETY: the handler matches the documented signature.
        unsafe { stream.startCaptureWithCompletionHandler(Some(&started)) };
        match rx.recv_timeout(SCK_TIMEOUT) {
            Ok(None) => {}
            Ok(Some(why)) if !permissions::screen_recording(false) => {
                return Err(CaptureError::Unsupported(format!(
                    "{} ({why})",
                    permissions::SCREEN_RECORDING_MISSING
                )));
            }
            Ok(Some(why)) => return Err(CaptureError::Backend(why)),
            Err(_) => {
                return Err(CaptureError::Backend(
                    "ScreenCaptureKit did not start the stream".into(),
                ))
            }
        }
        tracing::info!(width, height, "ScreenCaptureKit stream started");
        Ok(Self {
            stream,
            _output: output,
            _queue: queue,
            shared,
            display: DisplayInfo {
                width: width as u32,
                height: height as u32,
            },
        })
    }
}

impl Drop for SckCapturer {
    fn drop(&mut self) {
        // SAFETY: stopping a started stream; no completion handler needed.
        unsafe { self.stream.stopCaptureWithCompletionHandler(None) };
    }
}

impl ScreenCapturer for SckCapturer {
    fn name(&self) -> &str {
        "screencapturekit"
    }

    fn display(&self) -> DisplayInfo {
        self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        let deadline = Instant::now() + timeout;
        let mut slot = self.shared.slot.lock().unwrap();
        loop {
            if let Some(why) = slot.stopped.take() {
                return Err(CaptureError::Lost(why));
            }
            if let Some(frame) = slot.frame.take() {
                self.display = frame.display_info();
                return Ok(Some(frame));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(None);
            }
            slot = self.shared.changed.wait_timeout(slot, left).unwrap().0;
        }
    }
}
