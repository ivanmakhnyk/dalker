//! Host clipboard text through `NSPasteboard` (the general pasteboard).

use dalker_core::{Clipboard, ClipboardError, PlatformError};
use objc2::rc::Retained;
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// The general pasteboard. `NSPasteboard` may be used from any thread.
pub struct MacClipboard {
    board: Retained<NSPasteboard>,
}

// SAFETY: NSPasteboard is documented as thread-safe, and the agent uses this from one
// thread at a time.
unsafe impl Send for MacClipboard {}

impl MacClipboard {
    pub fn new() -> Result<Self, PlatformError> {
        Ok(Self {
            board: NSPasteboard::generalPasteboard(),
        })
    }
}

impl Clipboard for MacClipboard {
    fn name(&self) -> &str {
        "nspasteboard"
    }

    fn change_marker(&mut self) -> Result<Option<u64>, ClipboardError> {
        Ok(Some(self.board.changeCount() as u64))
    }

    fn get_text(&mut self) -> Result<Option<String>, ClipboardError> {
        // SAFETY: NSPasteboardTypeString is an immutable framework constant.
        let kind = unsafe { NSPasteboardTypeString };
        Ok(self.board.stringForType(kind).map(|s| s.to_string()))
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        // SAFETY: as above.
        let kind = unsafe { NSPasteboardTypeString };
        self.board.clearContents();
        if self
            .board
            .setString_forType(&NSString::from_str(text), kind)
        {
            Ok(())
        } else {
            Err(ClipboardError::Busy(
                "the pasteboard refused the text".into(),
            ))
        }
    }
}
