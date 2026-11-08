//! macOS placeholder. The traits are in place so ScreenCaptureKit / CGEvent
//! backends can be added without touching the rest of the agent.

use std::time::Duration;

use dalker_core::{
    CaptureError, DisplayInfo, InputError, InputEvent, InputInjector, RawFrame, ScreenCapturer,
};

const NOT_YET: &str = "macOS not implemented yet";

pub struct MacCapturer;

impl MacCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        Err(CaptureError::Unsupported(NOT_YET.into()))
    }
}

impl ScreenCapturer for MacCapturer {
    fn name(&self) -> &str {
        "macos-stub"
    }
    fn display(&self) -> DisplayInfo {
        DisplayInfo {
            width: 0,
            height: 0,
        }
    }
    fn next_frame(&mut self, _timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        Err(CaptureError::Unsupported(NOT_YET.into()))
    }
}

pub struct MacInjector;

impl MacInjector {
    pub fn new() -> Result<Self, InputError> {
        Err(InputError::Unsupported(NOT_YET.into()))
    }
}

impl InputInjector for MacInjector {
    fn name(&self) -> &str {
        "macos-stub"
    }
    fn set_display(&mut self, _display: DisplayInfo) {}
    fn inject(&mut self, _event: &InputEvent) -> Result<(), InputError> {
        Err(InputError::Unsupported(NOT_YET.into()))
    }
}
