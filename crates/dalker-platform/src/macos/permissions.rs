//! The two privacy permissions Dalker needs on macOS, and what to tell the user when one is
//! missing. Both are granted per app in System Settings › Privacy & Security and tied to the
//! app's code signature.

use objc2_core_foundation::{CFBoolean, CFDictionary, CFString};

#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrustedWithOptions(options: Option<&CFDictionary>) -> bool;
}

/// Whether this process may capture the screen. With `prompt`, asks macOS to show its
/// permission dialog (once per app) when it may not.
pub fn screen_recording(prompt: bool) -> bool {
    objc2_core_graphics::CGPreflightScreenCaptureAccess()
        || (prompt && objc2_core_graphics::CGRequestScreenCaptureAccess())
}

/// Whether this process may post keyboard and mouse events. With `prompt`, asks macOS to
/// show its dialog that leads to the Accessibility settings when it may not.
pub fn accessibility(prompt: bool) -> bool {
    let key = CFString::from_static_str("AXTrustedCheckOptionPrompt");
    let options =
        CFDictionary::<CFString, CFBoolean>::from_slices(&[&key], &[CFBoolean::new(prompt)]);
    // SAFETY: plain C call with a valid dictionary.
    unsafe { AXIsProcessTrustedWithOptions(Some(options.as_ref())) }
}

pub(crate) const SCREEN_RECORDING_MISSING: &str = "Screen Recording permission is missing: turn on Dalker in System Settings › Privacy & Security › Screen & System Audio Recording, then restart Dalker";
pub(crate) const ACCESSIBILITY_MISSING: &str = "Accessibility permission is missing: turn on Dalker in System Settings › Privacy & Security › Accessibility";
