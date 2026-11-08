//! Placeholder for Wayland capture through `xdg-desktop-portal` ScreenCast + PipeWire.
//!
//! The intended flow: request a ScreenCast session over D-Bus, receive the
//! PipeWire node id + fd, consume `SPA_VIDEO_FORMAT_BGRx` buffers and expose
//! them through [`dalker_core::ScreenCapturer`].

use std::time::Duration;

use dalker_core::{CaptureError, DisplayInfo, RawFrame, ScreenCapturer};

pub struct PipeWireCapturer;

impl PipeWireCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unsupported(
            "PipeWire/Wayland capture not implemented yet".into(),
        ))
    }

    /// True when the current session looks like Wayland (where X11 capture
    /// would only see XWayland windows).
    pub fn session_is_wayland() -> bool {
        super::is_wayland_session()
    }
}

impl ScreenCapturer for PipeWireCapturer {
    fn name(&self) -> &str {
        "pipewire-stub"
    }
    fn display(&self) -> DisplayInfo {
        DisplayInfo {
            width: 0,
            height: 0,
        }
    }
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        Err(CaptureError::Unsupported(
            "PipeWire/Wayland capture not implemented yet".into(),
        ))
    }
}
