use thiserror::Error;

/// Errors produced by a [`crate::ScreenCapturer`].
#[derive(Debug, Error)]
pub enum CaptureError {
    /// The backend is not available on this OS / build.
    #[error("screen capture not supported: {0}")]
    Unsupported(String),
    /// The capture session was lost (resolution change, desktop switch, UAC prompt...).
    /// Callers should drop the capturer and create a new one.
    #[error("capture session lost: {0}")]
    Lost(String),
    /// Any other backend failure.
    #[error("capture backend error: {0}")]
    Backend(String),
}

/// Errors produced by an [`crate::InputInjector`].
#[derive(Debug, Error)]
pub enum InputError {
    #[error("input injection not supported: {0}")]
    Unsupported(String),
    #[error("unknown key code: {0}")]
    UnknownKey(String),
    #[error("input backend error: {0}")]
    Backend(String),
}
