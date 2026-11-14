//! # dalker-core
//!
//! OS-agnostic building blocks shared by every Dalker crate:
//!
//! * [`RawFrame`] — an uncompressed captured frame (BGRA, top-down).
//! * [`InputEvent`] — remote input with **normalized** `0.0..=1.0` coordinates.
//! * [`ScreenCapturer`] / [`InputInjector`] — the traits platform backends implement.
//! * [`MockCapturer`] — a synthetic 30 FPS frame generator for transport/UI testing.
//! * [`Clipboard`] — the host's text clipboard, shared with viewers.
//! * [`FileAccess`] — the host user that file transfers run as.
//! * [`keymap`] — DOM `KeyboardEvent.code` → Linux evdev / Windows scancode tables.
//!
//! This crate deliberately has zero OS bindings.

pub mod clipboard;
pub mod error;
pub mod files;
pub mod frame;
pub mod input;
pub mod keymap;
pub mod mock;
pub mod traits;

pub use clipboard::{Clipboard, ClipboardError, MockClipboard, MAX_CLIPBOARD_BYTES};
pub use error::{CaptureError, InputError, PlatformError};
pub use files::{FileAccess, FileAccessFactory, FileEntry};
pub use frame::{DisplayInfo, PixelFormat, RawFrame};
pub use input::{InputEvent, InputTracker, MouseButton, NormPoint};
pub use mock::{MockCapturer, MockInjector};
pub use traits::{InputInjector, ScreenCapturer, ViewerIndicator};
