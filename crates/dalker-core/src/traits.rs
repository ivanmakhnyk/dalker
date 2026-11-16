use std::time::Duration;

use crate::{CaptureError, DisplayInfo, InputError, InputEvent, RawFrame};

/// A source of screen frames.
///
/// Implementations are **blocking** and are expected to live on a dedicated OS
/// thread (DXGI and X11 connections are thread-affine in practice). They are
/// `Send` so they can be constructed on one thread and moved to that worker.
pub trait ScreenCapturer: Send {
    /// Human readable backend name, e.g. `"dxgi"`, `"x11-shm"`, `"mock"`.
    fn name(&self) -> &str;

    /// Current geometry of the captured display.
    fn display(&self) -> DisplayInfo;

    /// Waits up to `timeout` for the next frame.
    ///
    /// * `Ok(Some(frame))` – a new frame.
    /// * `Ok(None)` – nothing changed within `timeout` (callers keep the previous frame).
    /// * `Err(CaptureError::Lost(_))` – recreate the capturer.
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError>;
}

/// A sink for remote input.
///
/// Coordinates arrive normalized (`0.0..=1.0`); the injector is the only place
/// where they are scaled to physical pixels of the captured display.
pub trait InputInjector: Send {
    fn name(&self) -> &str;

    /// Informs the injector about the geometry of the display being streamed,
    /// so normalized coordinates map onto the same pixels the viewer sees.
    fn set_display(&mut self, display: DisplayInfo);

    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError>;
}

impl<T: ScreenCapturer + ?Sized> ScreenCapturer for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn display(&self) -> DisplayInfo {
        (**self).display()
    }
    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        (**self).next_frame(timeout)
    }
}

impl<T: InputInjector + ?Sized> InputInjector for Box<T> {
    fn name(&self) -> &str {
        (**self).name()
    }
    fn set_display(&mut self, display: DisplayInfo) {
        (**self).set_display(display)
    }
    fn inject(&mut self, event: &InputEvent) -> Result<(), InputError> {
        (**self).inject(event)
    }
}

/// On-screen indicator that tells anyone at the host that the desktop is being
/// viewed/controlled remotely (a red border around the screen).
///
/// The agent switches it on as soon as a viewer starts connecting and off when
/// the last one leaves. It is deliberately not configurable off: the agent
/// refuses to serve a real desktop if no indicator can be shown.
pub trait ViewerIndicator: Send + Sync {
    fn name(&self) -> &str;
    fn set_active(&self, active: bool);
    /// True once the border asked for with `set_active(true)` is actually on the screen (drawn
    /// by the compositor, or acknowledged by the shell that draws it), false while it is off or
    /// still being shown. The agent serves frames only while this is true.
    fn shown(&self) -> bool;
    /// False if the indicator can no longer be shown (e.g. the compositor extension that
    /// draws it went away). The agent then stops serving frames until it is back.
    fn healthy(&self) -> bool {
        true
    }
    /// True once after the person at the host asked, through the indicator itself, to
    /// disconnect everyone (for example from the GNOME panel menu). Polled while it is on.
    fn take_disconnect_request(&self) -> bool {
        false
    }
    /// Optional badge label shown on the red border (for example `"AI agent"` while an MCP
    /// session is active). Default is a no-op.
    fn set_label(&self, label: Option<&str>) {
        let _ = label;
    }
}
