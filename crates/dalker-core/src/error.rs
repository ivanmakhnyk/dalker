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

/// Why a platform backend (capture, input, border, clipboard, file access) could not be
/// opened. Every factory in `dalker-platform` returns it, so callers decide how to retry and
/// how loudly to log from the kind, never from the message. The message is shown as is
/// (to viewers too), so it carries no prefix of its own.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PlatformError {
    /// This desktop, OS or build has no such backend, and waiting won't bring one (no
    /// screen cast service, no clipboard to reach, a desktop running as root). Only another
    /// desktop session can change that.
    #[error("{0}")]
    Unsupported(String),
    /// Not available yet, and expected to come by itself or once the person at the host
    /// acts: nobody signed in, the shell or its extension still starting, the screen locked,
    /// a permission not granted yet, the display just switched.
    #[error("{0}")]
    Transient(String),
    /// The backend failed in a way nobody expects (an OS call error, a thread that did not
    /// start). Worth reporting as an error; retrying may still help.
    #[error("{0}")]
    Fatal(String),
}

impl PlatformError {
    /// The message, without the kind.
    pub fn message(&self) -> &str {
        match self {
            Self::Unsupported(m) | Self::Transient(m) | Self::Fatal(m) => m,
        }
    }

    /// Whether both are the same kind of failure, whatever their messages say.
    pub fn same_kind(&self, other: &PlatformError) -> bool {
        std::mem::discriminant(self) == std::mem::discriminant(other)
    }

    /// Rewrites the message, keeping the kind.
    pub fn map_message(self, f: impl FnOnce(String) -> String) -> Self {
        match self {
            Self::Unsupported(m) => Self::Unsupported(f(m)),
            Self::Transient(m) => Self::Transient(f(m)),
            Self::Fatal(m) => Self::Fatal(f(m)),
        }
    }
}

/// A lost capture session is recreated; any other backend failure is unexpected.
impl From<CaptureError> for PlatformError {
    fn from(e: CaptureError) -> Self {
        match e {
            CaptureError::Unsupported(m) => Self::Unsupported(m),
            CaptureError::Lost(m) => Self::Transient(m),
            CaptureError::Backend(m) => Self::Fatal(m),
        }
    }
}

impl From<InputError> for PlatformError {
    fn from(e: InputError) -> Self {
        match e {
            InputError::Unsupported(m) => Self::Unsupported(m),
            InputError::UnknownKey(m) | InputError::Backend(m) => Self::Fatal(m),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_not_messages() {
        let a = PlatformError::Transient("GNOME is starting".into());
        let b = PlatformError::Transient("the screen is locked".into());
        assert!(a.same_kind(&b));
        assert!(!a.same_kind(&PlatformError::Fatal("GNOME is starting".into())));
        assert_eq!(a.to_string(), "GNOME is starting");
        assert_eq!(
            PlatformError::from(CaptureError::Lost("desktop switch".into())),
            PlatformError::Transient("desktop switch".into())
        );
        assert_eq!(
            PlatformError::Unsupported("x".into()).map_message(|m| format!("border: {m}")),
            PlatformError::Unsupported("border: x".into())
        );
    }
}
