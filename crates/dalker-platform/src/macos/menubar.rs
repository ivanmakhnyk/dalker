//! The menu bar icon: open the viewer in a browser, open Dalker Settings, disconnect everyone,
//! quit.

use std::cell::RefCell;

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSVariableStatusItemLength,
};
use objc2_foundation::{NSObjectProtocol, NSString};

use super::main_thread::{on_main, on_main_async};

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
    }
);

struct Installed {
    item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<MenuTarget>,
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
                Some(action),
                &NSString::from_str(""),
            )
        };
        // SAFETY: as above.
        unsafe { item.setTarget(Some(&target)) };
        menu.addItem(&item);
    };
    entry("Open Dalker in Browser", sel!(openViewer:));
    entry("Dalker Settings…", sel!(openSettings:));
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    entry("Disconnect Everyone", sel!(disconnectAll:));
    entry("Quit Dalker", sel!(quit:));

    let item = NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = item.button(mtm) {
        let symbol = NSImage::imageWithSystemSymbolName_accessibilityDescription(
            &NSString::from_str("display"),
            Some(&NSString::from_str("Dalker")),
        );
        match symbol {
            Some(image) => {
                image.setTemplate(true);
                button.setImage(Some(&image));
            }
            None => button.setTitle(&NSString::from_str("Dalker")),
        }
        button.setToolTip(Some(&NSString::from_str("Dalker")));
    }
    item.setMenu(Some(&menu));
    INSTALLED.with(|i| {
        *i.borrow_mut() = Some(Installed {
            item,
            _menu: menu,
            _target: target,
        })
    });
}

/// The menu bar item; removed when dropped.
pub struct MenuBar(());

impl MenuBar {
    /// Puts the item in the menu bar. Needs the AppKit loop on the main thread
    /// ([`super::run_app`]).
    pub fn start(cfg: MenuBarConfig) -> Result<Self, String> {
        on_main(move |mtm| install(mtm, cfg))?;
        Ok(Self(()))
    }
}

impl Drop for MenuBar {
    fn drop(&mut self) {
        on_main_async(|_| {
            if let Some(installed) = INSTALLED.with(|i| i.borrow_mut().take()) {
                NSStatusBar::systemStatusBar().removeStatusItem(&installed.item);
            }
        });
    }
}
