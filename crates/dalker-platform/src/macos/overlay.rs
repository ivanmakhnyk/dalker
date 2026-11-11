//! The red "someone is connected" frame: four thin, click-through windows along the edges
//! of the main screen, above everything (screen-saver level) and on every Space.
//!
//! AppKit windows live on the main thread, so they are kept in a main-thread local and
//! every change is sent there.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use dalker_core::ViewerIndicator;
use objc2::rc::Retained;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSBackingStoreType, NSColor, NSScreen, NSScreenSaverWindowLevel, NSWindow,
    NSWindowCollectionBehavior, NSWindowSharingType, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize};

use super::main_thread::{on_main, on_main_async};

/// Border width in points (twice that in pixels on a Retina screen).
const BORDER_PT: f64 = 4.0;

thread_local! {
    /// The four edge windows (bottom, top, left, right); main thread only.
    static EDGES: RefCell<Vec<Retained<NSWindow>>> = const { RefCell::new(Vec::new()) };
}

fn edge_frames(screen: NSRect) -> [NSRect; 4] {
    let (x, y, w, h, t) = (
        screen.origin.x,
        screen.origin.y,
        screen.size.width,
        screen.size.height,
        BORDER_PT,
    );
    let rect = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
    [
        rect(x, y, w, t),
        rect(x, y + h - t, w, t),
        rect(x, y, t, h),
        rect(x + w - t, y, t, h),
    ]
}

/// The screen with the menu bar (the first one), in AppKit coordinates.
fn main_screen_frame(mtm: MainThreadMarker) -> Option<NSRect> {
    NSScreen::screens(mtm).firstObject().map(|s| s.frame())
}

fn create_edges(mtm: MainThreadMarker) -> Result<(), String> {
    let screen = main_screen_frame(mtm).ok_or("no screen")?;
    let red = NSColor::colorWithSRGBRed_green_blue_alpha(0.93, 0.11, 0.14, 1.0);
    let windows: Vec<_> = edge_frames(screen)
        .into_iter()
        .map(|frame| {
            // SAFETY: a borderless window created on the main thread.
            let w = unsafe {
                NSWindow::initWithContentRect_styleMask_backing_defer(
                    mtm.alloc(),
                    frame,
                    NSWindowStyleMask::Borderless,
                    NSBackingStoreType::Buffered,
                    false,
                )
            };
            // SAFETY: we keep our own reference; AppKit must not free it on close.
            unsafe { w.setReleasedWhenClosed(false) };
            w.setBackgroundColor(Some(&red));
            w.setOpaque(true);
            w.setHasShadow(false);
            w.setIgnoresMouseEvents(true);
            w.setLevel(NSScreenSaverWindowLevel);
            w.setCollectionBehavior(
                NSWindowCollectionBehavior::CanJoinAllSpaces
                    | NSWindowCollectionBehavior::Stationary
                    | NSWindowCollectionBehavior::IgnoresCycle
                    | NSWindowCollectionBehavior::FullScreenAuxiliary,
            );
            // Keep the frame out of the picture viewers see.
            w.setSharingType(NSWindowSharingType::None);
            w
        })
        .collect();
    EDGES.with(|e| *e.borrow_mut() = windows);
    Ok(())
}

fn show_edges(mtm: MainThreadMarker, active: bool) -> bool {
    let screen = main_screen_frame(mtm);
    EDGES.with(|edges| {
        let edges = edges.borrow();
        for (i, w) in edges.iter().enumerate() {
            if active {
                // Follow resolution and arrangement changes.
                if let Some(frame) = screen.map(|s| edge_frames(s)[i]) {
                    w.setFrame_display(frame, true);
                }
                w.orderFrontRegardless();
            } else {
                w.orderOut(None);
            }
        }
        edges.len() == 4 && (!active || edges.iter().all(|w| w.isVisible()))
    })
}

/// Red frame around the main screen while viewers are connected.
pub struct BorderOverlay {
    healthy: Arc<AtomicBool>,
}

impl BorderOverlay {
    /// Creates the (hidden) windows. Needs the AppKit loop on the main thread
    /// ([`super::run_app`]); errors otherwise.
    pub fn new() -> Result<Self, String> {
        on_main(create_edges)??;
        Ok(Self {
            healthy: Arc::new(AtomicBool::new(true)),
        })
    }
}

impl ViewerIndicator for BorderOverlay {
    fn name(&self) -> &str {
        "appkit-border"
    }

    fn set_active(&self, active: bool) {
        let healthy = Arc::clone(&self.healthy);
        on_main_async(move |mtm| {
            let ok = show_edges(mtm, active);
            if !ok {
                tracing::warn!(active, "red border windows did not change as asked");
            }
            healthy.store(ok, Ordering::SeqCst);
        });
    }

    fn healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }
}

impl Drop for BorderOverlay {
    fn drop(&mut self) {
        on_main_async(|_| {
            EDGES.with(|edges| {
                for w in edges.borrow_mut().drain(..) {
                    w.orderOut(None);
                }
            })
        });
    }
}
