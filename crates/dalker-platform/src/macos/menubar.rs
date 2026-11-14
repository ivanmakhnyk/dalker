//! The menu bar icon: open the viewer in a browser, open Dalker Settings, disconnect everyone,
//! quit.
//!
//! While Screen Recording or Accessibility is not allowed, the icon turns into a warning sign
//! and the menu opens a setup window with shortcuts to the right System Settings panes.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSBackingStoreType, NSButton, NSImage, NSMenu, NSMenuItem, NSStatusBar,
    NSStatusItem, NSTextField, NSVariableStatusItemLength, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{NSObjectProtocol, NSPoint, NSRect, NSSize, NSString};

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
    /// Opens the setup window on first launch of the installed app until a password is set.
    pub needs_password: bool,
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

        #[unsafe(method(openSetup:))]
        fn open_setup(&self, _sender: Option<&AnyObject>) {
            INSTALLED.with(|i| {
                if let Some(installed) = i.borrow().as_ref() {
                    installed.present_setup(self.mtm());
                }
            });
        }

        #[unsafe(method(continueSetup:))]
        fn continue_setup(&self, _sender: Option<&AnyObject>) {
            INSTALLED.with(|i| {
                if let Some(installed) = i.borrow().as_ref() {
                    installed.setup.window.orderOut(None);
                }
            });
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
    setup: SetupWindow,
}

struct SetupWindow {
    window: Retained<NSWindow>,
    screen_status: Retained<NSTextField>,
    accessibility_status: Retained<NSTextField>,
    screen_settings: Retained<NSButton>,
    accessibility_settings: Retained<NSButton>,
    restart: Retained<NSButton>,
    continue_button: Retained<NSButton>,
    /// Screen Recording was off when Dalker started. Capture in this process won't work even
    /// once it is turned on, so Restart stays in the menu.
    restart_needed: bool,
}

impl SetupWindow {
    fn new(mtm: MainThreadMarker, target: &MenuTarget, restart_needed: bool) -> Self {
        let rect = |x, y, w, h| NSRect::new(NSPoint::new(x, y), NSSize::new(w, h));
        // SAFETY: the window is built and retained on the main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect(0.0, 0.0, 560.0, 208.0),
                NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // SAFETY: SetupWindow retains the window and may show it again after closing.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str("Set up Dalker"));
        let content = window.contentView().expect("setup window content view");
        let label = |title, y, width| {
            let field = NSTextField::labelWithString(&NSString::from_str(title), mtm);
            field.setFrame(rect(20.0, y, width, 22.0));
            content.addSubview(&field);
            field
        };
        let button = |title, action, x, y, width| {
            // SAFETY: all selectors belong to MenuTarget, retained by Installed.
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str(title),
                    Some(target),
                    Some(action),
                    mtm,
                )
            };
            button.setFrame(rect(x, y, width, 32.0));
            content.addSubview(&button);
            button
        };
        label(
            "Allow Dalker to show your screen and use your mouse and keyboard.",
            169.0,
            520.0,
        );
        let screen_status = label("", 127.0, 380.0);
        let accessibility_status = label("", 83.0, 380.0);
        let screen_settings = button(
            "Open Settings…",
            sel!(openScreenRecording:),
            400.0,
            126.0,
            140.0,
        );
        let accessibility_settings = button(
            "Open Settings…",
            sel!(openAccessibility:),
            400.0,
            82.0,
            140.0,
        );
        if restart_needed {
            label(
                "After allowing Screen Recording in System Settings, restart Dalker.",
                55.0,
                520.0,
            );
        }
        let restart = button("Restart Dalker", sel!(restart:), 246.0, 18.0, 140.0);
        let continue_button = button(
            "Continue to Password",
            sel!(continueSetup:),
            386.0,
            18.0,
            154.0,
        );
        window.center();
        Self {
            window,
            screen_status,
            accessibility_status,
            screen_settings,
            accessibility_settings,
            restart,
            continue_button,
            restart_needed,
        }
    }

    fn update(&self, now: Permissions) {
        let screen_status = match (now.screen_recording, self.restart_needed) {
            (true, false) => "Screen & System Audio Recording: Allowed",
            (true, true) => "Screen & System Audio Recording: Restart required",
            (false, _) => "Screen & System Audio Recording: Permission needed",
        };
        self.screen_status
            .setStringValue(&NSString::from_str(screen_status));
        self.accessibility_status
            .setStringValue(&NSString::from_str(if now.accessibility {
                "Accessibility: Allowed"
            } else {
                "Accessibility: Permission needed"
            }));
        self.screen_settings.setEnabled(!now.screen_recording);
        self.accessibility_settings.setEnabled(!now.accessibility);
        // macOS can keep reporting denial to the current process after the toggle is on.
        self.restart
            .setEnabled(self.restart_needed || !now.screen_recording);
        self.continue_button
            .setEnabled(now.all() && !self.restart_needed);
    }
}

impl Installed {
    fn present_setup(&self, mtm: MainThreadMarker) {
        self.setup.update(Permissions::now());
        self.setup.window.makeKeyAndOrderFront(None);
        NSApplication::sharedApplication(mtm).activate();
    }
}

thread_local! {
    /// The installed menu bar item; main thread only.
    static INSTALLED: RefCell<Option<Installed>> = const { RefCell::new(None) };
}

fn install(mtm: MainThreadMarker, cfg: MenuBarConfig) {
    let first_run = cfg.needs_password && app_bundle().is_some();
    let now = Permissions::now();
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
    entry("Set Up Permissions…", Some(sel!(openSetup:)));
    let setup = SetupWindow::new(mtm, &target, !now.screen_recording);
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
    show(mtm, &installed, now);
    if first_run {
        installed.present_setup(mtm);
    }
    INSTALLED.with(|i| *i.borrow_mut() = Some(installed));
}

/// Shows the icon and the setup items for these permissions.
fn show(mtm: MainThreadMarker, installed: &Installed, now: Permissions) {
    let setup = !now.all() || installed.setup.restart_needed;
    installed.setup.update(now);

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
