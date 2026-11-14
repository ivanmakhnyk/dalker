//! The menu bar icon: open the viewer in a browser, open Dalker Settings, disconnect everyone,
//! quit.
//!
//! While Screen Recording or Accessibility is not allowed, the icon turns into a warning sign
//! and the menu starts with what is missing: items that open the right System Settings pane,
//! and **Restart Dalker**, because Screen Recording only applies to a newly started process.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObjectProtocol, NSString};

use super::main_thread::{on_main, on_main_async};
use super::permissions;

/// How often the permissions are checked again while the menu bar item is up.
const PERMISSION_POLL: Duration = Duration::from_secs(2);
const SCREEN_RECORDING_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture";
const ACCESSIBILITY_PANE: &str =
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility";

/// Which of the two permissions this process has.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Permissions {
    screen_recording: bool,
    accessibility: bool,
}

impl Permissions {
    fn now() -> Self {
        Self {
            screen_recording: permissions::screen_recording(false),
            accessibility: permissions::accessibility(false),
        }
    }

    fn all(self) -> bool {
        self.screen_recording && self.accessibility
    }
}

fn open(target: &str) {
    if let Err(e) = std::process::Command::new("/usr/bin/open")
        .arg(target)
        .spawn()
    {
        tracing::warn!("could not open {target}: {e}");
    }
}

/// The `.app` bundle this process runs from, if any.
fn app_bundle() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.ancestors()
        .find(|p| p.extension().is_some_and(|e| e == "app"))
        .map(|p| p.to_path_buf())
}

/// Starts a small shell that waits for this process to end and then opens Dalker again.
fn relaunch_after_exit() -> std::io::Result<()> {
    let again = match app_bundle() {
        Some(bundle) => format!("/usr/bin/open {}", shell_quote(&bundle.to_string_lossy())),
        None => shell_quote(&std::env::current_exe()?.to_string_lossy()),
    };
    let script = format!(
        "while kill -0 {} 2>/dev/null; do sleep 0.2; done; {again}",
        std::process::id()
    );
    std::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map(drop)
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// What the menu items do. Called on the main thread; keep them short.
pub struct MenuBarConfig {
    /// The viewer's address right now, if the web server runs.
    pub open_url: Box<dyn Fn() -> Option<String> + Send>,
    pub on_settings: Box<dyn Fn() + Send>,
    /// Ends every viewer's and AI agent's session (no password needed).
    pub on_disconnect: Box<dyn Fn() + Send>,
    pub on_exit: Box<dyn Fn() + Send>,
}

define_class!(
    // SAFETY: NSObject has no subclassing requirements; MenuTarget has no Drop impl.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DalkerMenuTarget"]
    #[ivars = MenuBarConfig]
    struct MenuTarget;

    unsafe impl NSObjectProtocol for MenuTarget {}

    impl MenuTarget {
        #[unsafe(method(openViewer:))]
        fn open_viewer(&self, _sender: Option<&AnyObject>) {
            match (self.ivars().open_url)() {
                Some(url) => {
                    if let Err(e) = std::process::Command::new("/usr/bin/open").arg(url).spawn() {
                        tracing::warn!("could not open the browser: {e}");
                    }
                }
                None => tracing::info!("the web server is off; nothing to open"),
            }
        }

        #[unsafe(method(openSettings:))]
        fn open_settings(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_settings)();
        }

        #[unsafe(method(disconnectAll:))]
        fn disconnect_all(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_disconnect)();
        }

        #[unsafe(method(quit:))]
        fn quit(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_exit)();
        }

        #[unsafe(method(openScreenRecording:))]
        fn open_screen_recording(&self, _sender: Option<&AnyObject>) {
            // Adds Dalker to the list (turned off) the first time, so it can be switched on.
            permissions::screen_recording(true);
            open(SCREEN_RECORDING_PANE);
        }

        #[unsafe(method(openAccessibility:))]
        fn open_accessibility(&self, _sender: Option<&AnyObject>) {
            permissions::accessibility(true);
            open(ACCESSIBILITY_PANE);
        }

        #[unsafe(method(restart:))]
        fn restart(&self, _sender: Option<&AnyObject>) {
            match relaunch_after_exit() {
                Ok(()) => (self.ivars().on_exit)(),
                Err(e) => tracing::warn!("could not restart Dalker: {e}"),
            }
        }
    }
);

struct Installed {
    item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
    /// The items shown only while a permission is missing.
    setup: SetupItems,
}

struct SetupItems {
    heading: Retained<NSMenuItem>,
    screen_recording: Retained<NSMenuItem>,
    accessibility: Retained<NSMenuItem>,
    restart: Retained<NSMenuItem>,
    separator: Retained<NSMenuItem>,
    /// Screen Recording was off when Dalker started. Capture in this process won't work even
    /// once it is turned on, so Restart stays in the menu.
    restart_needed: bool,
}

thread_local! {
    /// The installed menu bar item; main thread only.
    static INSTALLED: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

fn install(mtm: MainThreadMarker, cfg: MenuBarConfig) {
    let target: Retained<MenuTarget> = {
        let this = MenuTarget::alloc(mtm).set_ivars(cfg);
        // SAFETY: NSObject's `init` on a freshly allocated object.
        unsafe { msg_send![super(this), init] }
    };
    let menu = NSMenu::new(mtm);
    let entry = |title: &str, action| {
        // SAFETY: the selector names a method of `target`, which outlives the item.
        let item = unsafe {
            NSMenuItem::initWithTitle_action_keyEquivalent(
                mtm.alloc(),
                &NSString::from_str(title),
                action,
                &NSString::from_str(""),
            )
        };
        if action.is_some() {
            // SAFETY: as above.
            unsafe { item.setTarget(Some(&target)) };
        }
        menu.addItem(&item);
        item
    };
    // Without an action the heading is shown greyed out, as a label.
    let setup = SetupItems {
        heading: entry("Dalker needs your permission:", None),
        screen_recording: entry("Allow Screen Recording…", Some(sel!(openScreenRecording:))),
        accessibility: entry("Allow Accessibility…", Some(sel!(openAccessibility:))),
        restart: entry("Restart Dalker", Some(sel!(restart:))),
        separator: {
            let item = NSMenuItem::separatorItem(mtm);
            menu.addItem(&item);
            item
        },
        restart_needed: !permissions::screen_recording(false),
    };
    entry("Open Dalker in Browser", Some(sel!(openViewer:)));
    entry("Dalker Settings…", Some(sel!(openSettings:)));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    entry("Disconnect Everyone", Some(sel!(disconnectAll:)));
    entry("Quit Dalker", Some(sel!(quit:)));

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    item.setMenu(Some(&menu));
    let installed = Installed {
        item,
        _menu: menu,
        _target: target,
        setup,
    };
    show(mtm, &installed, Permissions::now());
    INSTALLED.with(|i| *i.borrow_mut() = Some(installed));
}

/// Shows the icon and the setup items for these permissions.
fn show(mtm: MainThreadMarker, installed: &Installed, now: Permissions) {
    let s = &installed.setup;
    let restart = s.restart_needed || !now.screen_recording;
    let setup = !now.all() || restart;
    s.heading.setHidden(!setup);
    s.separator.setHidden(!setup);
    s.screen_recording.setHidden(now.screen_recording);
    s.restart.setHidden(!restart);
    s.accessibility.setHidden(now.accessibility);

    let Some(button) = installed.item.button(mtm) else {
        return;
    };
    let (symbol, tip) = if !setup {
        ("display", "Dalker")
    } else {
        (
            "exclamationmark.triangle",
            "Dalker needs a permission, or a restart to use it",
        )
    };
    let image = NSImage::imageWithSystemSymbolName_accessibilityDescription(
        &NSString::from_str(symbol),
        Some(&NSString::from_str(tip)),
    );
    match image {
        Some(image) => {
            image.setTemplate(true);
            button.setImage(Some(&image));
        }
        None => button.setTitle(&NSString::from_str(if setup {
            "Dalker ⚠"
        } else {
            "Dalker"
        })),
    }
    button.setToolTip(Some(&NSString::from_str(tip)));
}

/// Checks the permissions every [`PERMISSION_POLL`] and updates the menu when they change,
/// until `stop` is set.
fn follow_permissions(stop: Arc<AtomicBool>) {
    let mut last = None;
    while !stop.load(Ordering::Relaxed) {
        let now = Permissions::now();
        if last != Some(now) {
            if last.is_some() {
                tracing::info!(
                    screen_recording = now.screen_recording,
                    accessibility = now.accessibility,
                    "macOS permissions changed"
                );
            }
            last = Some(now);
            on_main_async(move |mtm| {
                INSTALLED.with(|i| {
                    if let Some(installed) = i.borrow().as_ref() {
                        show(mtm, installed, now);
                    }
                })
            });
        }
        std::thread::sleep(PERMISSION_POLL);
    }
}

/// The menu bar item; removed when dropped.
pub struct MenuBar {
    stop: Arc<AtomicBool>,
}

impl MenuBar {
    /// Puts the item in the menu bar. Needs the AppKit loop on the main thread
    /// ([`super::run_app`]).
    pub fn start(cfg: MenuBarConfig) -> Result<Self, String> {
        on_main(move |mtm| install(mtm, cfg))?;
        let stop = Arc::new(AtomicBool::new(false));
        let watcher = Arc::clone(&stop);
        if let Err(e) = std::thread::Builder::new()
            .name("dalker-permissions".into())
            .spawn(move || follow_permissions(watcher))
        {
            tracing::warn!("could not watch the macOS permissions: {e}");
        }
        Ok(Self { stop })
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        on_main_async(|_| {
            if let Some(installed) = INSTALLED.with(|i| i.borrow_mut().take()) {
                NSStatusBar::systemStatusBar().removeStatusItem(&installed.item);
            }
        });
    }
}
