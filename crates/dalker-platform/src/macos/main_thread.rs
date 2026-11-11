//! Hands the main thread to AppKit and runs the agent on another thread.

use std::sync::mpsc;
use std::time::Duration;

use dispatch2::DispatchQueue;
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSEvent, NSEventModifierFlags, NSEventType,
};
use objc2_foundation::NSPoint;

/// Runs `f` on a thread of its own while the calling (main) thread runs the AppKit event
/// loop, which the border overlay and the menu bar icon need. Returns what `f` returned,
/// once it has; a panic in `f` is passed on.
///
/// Called on any other thread than the main one, it just runs `f` there.
pub fn run_app<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    let Some(mtm) = MainThreadMarker::new() else {
        return f();
    };
    let app = NSApplication::sharedApplication(mtm);
    // A background agent: no Dock icon and no menu, only the menu bar item.
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    let worker = std::thread::Builder::new()
        .name("dalker-agent".into())
        .spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
            DispatchQueue::main().exec_async(stop_app);
            result
        })
        .expect("spawn agent thread");
    app.run();
    match worker.join().expect("agent thread") {
        Ok(r) => r,
        Err(panic) => std::panic::resume_unwind(panic),
    }
}

/// Ends `NSApplication::run`. `stop` only takes effect after the next event, so post one.
fn stop_app() {
    let mtm = MainThreadMarker::new().expect("main thread");
    let app = NSApplication::sharedApplication(mtm);
    app.stop(None);
    if let Some(event) =
        NSEvent::otherEventWithType_location_modifierFlags_timestamp_windowNumber_context_subtype_data1_data2(
            NSEventType::ApplicationDefined,
            NSPoint::new(0.0, 0.0),
            NSEventModifierFlags::empty(),
            0.0,
            0,
            None,
            0,
            0,
            0,
        )
    {
        app.postEvent_atStart(&event, true);
    }
}

/// Runs `f` on the main thread and waits for its result. Errors when the main thread does
/// not answer in time (no AppKit loop running there: [`run_app`] was not used).
pub(crate) fn on_main<R: Send + 'static>(
    f: impl FnOnce(MainThreadMarker) -> R + Send + 'static,
) -> Result<R, String> {
    if let Some(mtm) = MainThreadMarker::new() {
        return Ok(f(mtm));
    }
    let (tx, rx) = mpsc::channel();
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("main queue runs on the main thread");
        let _ = tx.send(f(mtm));
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "the main thread is not running an event loop".to_string())
}

/// Runs `f` on the main thread later, without waiting.
pub(crate) fn on_main_async(f: impl FnOnce(MainThreadMarker) + Send + 'static) {
    DispatchQueue::main().exec_async(move || {
        let mtm = MainThreadMarker::new().expect("main queue runs on the main thread");
        f(mtm);
    });
}
