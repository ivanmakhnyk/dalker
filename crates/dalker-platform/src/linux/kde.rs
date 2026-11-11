//! KDE Plasma pieces that only D-Bus can reach on Wayland.

use dalker_core::{Clipboard, ClipboardError};
use zbus::blocking::{Connection, Proxy};

use super::gnome::name_has_owner;

const KLIPPER: &str = "org.kde.klipper";
const KLIPPER_PATH: &str = "/klipper";
const KLIPPER_IFACE: &str = "org.kde.klipper.klipper";

/// The Plasma clipboard through Klipper's D-Bus interface (`getClipboardContents`,
/// `setClipboardContents`). Unavailable when Klipper (the clipboard applet) is turned off.
/// The connection is made again after an error, so the clipboard comes back when Plasma
/// restarts.
pub struct KlipperClipboard {
    conn: Option<Connection>,
}

/// True when Klipper is on the session bus.
pub fn klipper_available() -> bool {
    Connection::session()
        .map(|c| name_has_owner(&c, KLIPPER))
        .unwrap_or(false)
}

impl KlipperClipboard {
    pub fn new() -> Result<Self, String> {
        let mut c = Self { conn: None };
        c.proxy().map_err(|e| e.to_string())?;
        Ok(c)
    }

    fn proxy(&mut self) -> Result<Proxy<'static>, ClipboardError> {
        if self.conn.is_none() {
            let conn = Connection::session()
                .map_err(|e| ClipboardError::Failed(format!("no D-Bus session bus: {e}")))?;
            self.conn = Some(conn);
        }
        let conn = self.conn.clone().unwrap();
        if !name_has_owner(&conn, KLIPPER) {
            self.conn = None;
            return Err(ClipboardError::Failed(
                "Klipper (the Plasma clipboard) is not running".into(),
            ));
        }
        Proxy::new(&conn, KLIPPER, KLIPPER_PATH, KLIPPER_IFACE)
            .map_err(|e| ClipboardError::Failed(e.to_string()))
    }

    fn failed(&mut self, method: &str, e: zbus::Error) -> ClipboardError {
        self.conn = None;
        ClipboardError::Failed(format!("{method}: {e}"))
    }
}

impl Clipboard for KlipperClipboard {
    fn name(&self) -> &str {
        "klipper"
    }

    fn get_text(&mut self) -> Result<Option<String>, ClipboardError> {
        let proxy = self.proxy()?;
        match proxy.call::<_, _, String>("getClipboardContents", &()) {
            Ok(text) => Ok((!text.is_empty()).then_some(text)),
            Err(e) => Err(self.failed("getClipboardContents", e)),
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        let proxy = self.proxy()?;
        match proxy.call::<_, _, ()>("setClipboardContents", &(text,)) {
            Ok(()) => Ok(()),
            Err(e) => Err(self.failed("setClipboardContents", e)),
        }
    }
}
