//! The Windows clipboard (Unicode text), for sharing it with viewers.
//!
//! `SetClipboardData` needs a clipboard owner window, so [`WinClipboard`] creates a
//! message-only window on the thread that creates it and must stay on that thread. That
//! thread has to pump messages ([`WinClipboard::pump`]) so that other apps emptying the
//! clipboard we own are not kept waiting.

use std::thread::sleep;
use std::time::Duration;

use dalker_core::{Clipboard, ClipboardError};
use windows::core::w;
use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, GetClipboardSequenceNumber,
    IsClipboardFormatAvailable, OpenClipboard, SetClipboardData,
};
use windows::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, PeekMessageW, TranslateMessage, HWND_MESSAGE,
    MSG, PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE,
};

const CF_UNICODETEXT: u32 = 13;

pub struct WinClipboard {
    hwnd: HWND,
}

// The window handle is only used on the thread that created it (see the module docs);
// the agent creates the clipboard on its clipboard thread and never moves it.
unsafe impl Send for WinClipboard {}

impl WinClipboard {
    pub fn new() -> Result<Self, String> {
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Dalker clipboard"),
                WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                Some(HWND_MESSAGE),
                None,
                None,
                None,
            )
        }
        .map_err(|e| format!("clipboard window: {e}"))?;
        Ok(Self { hwnd })
    }

    /// Handles pending window messages (the system sends the owner `WM_DESTROYCLIPBOARD`).
    pub fn pump(&self) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(self.hwnd), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    /// Opens the clipboard, retrying briefly while another app holds it open.
    fn open(&self) -> Result<Opened, ClipboardError> {
        let mut last = String::new();
        for _ in 0..10 {
            match unsafe { OpenClipboard(Some(self.hwnd)) } {
                Ok(()) => return Ok(Opened),
                Err(e) => last = e.to_string(),
            }
            sleep(Duration::from_millis(15));
        }
        Err(ClipboardError::Busy(format!(
            "another app has the clipboard open: {last}"
        )))
    }
}

impl Drop for WinClipboard {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// Closes the clipboard when dropped.
struct Opened;

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

impl Clipboard for WinClipboard {
    fn name(&self) -> &str {
        "windows"
    }

    fn change_marker(&mut self) -> Result<Option<u64>, ClipboardError> {
        self.pump();
        Ok(Some(unsafe { GetClipboardSequenceNumber() } as u64))
    }

    fn get_text(&mut self) -> Result<Option<String>, ClipboardError> {
        if unsafe { IsClipboardFormatAvailable(CF_UNICODETEXT) }.is_err() {
            return Ok(None);
        }
        let _open = self.open()?;
        unsafe {
            let Ok(handle) = GetClipboardData(CF_UNICODETEXT) else {
                return Ok(None);
            };
            let mem = HGLOBAL(handle.0);
            let ptr = GlobalLock(mem) as *const u16;
            if ptr.is_null() {
                return Ok(None);
            }
            // The text is NUL-terminated; never read past the allocation if it isn't.
            let max = GlobalSize(mem) / 2;
            let units = std::slice::from_raw_parts(ptr, max);
            let len = units.iter().position(|&c| c == 0).unwrap_or(max);
            let text = String::from_utf16_lossy(&units[..len]);
            let _ = GlobalUnlock(mem);
            Ok(Some(text))
        }
    }

    fn set_text(&mut self, text: &str) -> Result<(), ClipboardError> {
        // Windows apps expect CRLF line ends on the clipboard.
        let text = text.replace("\r\n", "\n").replace('\n', "\r\n");
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let _open = self.open()?;
        unsafe {
            EmptyClipboard().map_err(|e| ClipboardError::Busy(format!("EmptyClipboard: {e}")))?;
            let mem = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2)
                .map_err(|e| ClipboardError::Busy(format!("GlobalAlloc: {e}")))?;
            let ptr = GlobalLock(mem) as *mut u16;
            if ptr.is_null() {
                let _ = GlobalFree(Some(mem));
                return Err(ClipboardError::Busy("GlobalLock failed".into()));
            }
            std::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
            let _ = GlobalUnlock(mem);
            if let Err(e) = SetClipboardData(CF_UNICODETEXT, Some(HANDLE(mem.0))) {
                // Still ours: the clipboard only takes ownership on success.
                let _ = GlobalFree(Some(mem));
                return Err(ClipboardError::Busy(format!("SetClipboardData: {e}")));
            }
        }
        Ok(())
    }
}
