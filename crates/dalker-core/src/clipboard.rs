//! Text clipboard of the host, shared with viewers.
//!
//! The agent owns one [`Clipboard`] on a dedicated thread: it polls for changes while
//! someone is viewing and applies text a viewer copied. Only plain text is synced.

use std::sync::{Arc, Mutex};

/// Largest clipboard text (in bytes of UTF-8) passed either way. Bigger copies are not synced.
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;

/// Why the host's clipboard could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardError {
    /// Only for now (another app has it open, nobody is viewing yet...): try again soon.
    Busy(String),
    /// Not usable until the clipboard is opened again ([`Clipboard`] is recreated).
    Failed(String),
}

impl std::fmt::Display for ClipboardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ClipboardError::Busy(why) | ClipboardError::Failed(why) => f.write_str(why),
        }
    }
}

/// The host's clipboard (plain text only).
///
/// Implementations are blocking and may be thread-affine (Windows needs a window on the
/// calling thread), so the agent creates and uses one on a single thread.
pub trait Clipboard: Send {
    fn name(&self) -> &str;

    /// A cheap value that changes whenever the clipboard may have changed (Windows: the
    /// clipboard sequence number). `None` = no such thing; [`Clipboard::get_text`] is then
    /// polled and compared.
    fn change_marker(&mut self) -> Result<Option<u64>, ClipboardError> {
        Ok(None)
    }

    /// Current clipboard text. `Ok(None)` when it holds no text (empty, an image, files...).
    fn get_text(&mut self) -> Result<Option<String>, ClipboardError>;

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError>;
}

/// In-memory clipboard for the mock capture mode and tests. Clones share their contents,
/// so a test can play "an app on the host copied something".
#[derive(Clone, Default)]
pub struct MockClipboard {
    inner: Arc<Mutex<(u64, Option<String>)>>,
}

impl MockClipboard {
    pub fn new() -> Self {
        Self::default()
    }
}

impl Clipboard for MockClipboard {
    fn name(&self) -> &str {
        "mock"
    }

    fn change_marker(&mut self) -> Result<Option<u64>, ClipboardError> {
        Ok(Some(self.inner.lock().unwrap().0))
    }

    fn get_text(&mut self) -> Result<Option<String>, ClipboardError> {
        Ok(self.inner.lock().unwrap().1.clone())
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let mut g = self.inner.lock().unwrap();
        g.0 += 1;
        g.1 = Some(text.to_owned());
        Ok(())
    }
}
