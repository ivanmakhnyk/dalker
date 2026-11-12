use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use dalker_core::ViewerIndicator;

use super::desktop;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CombineRgn, CreateRectRgn, CreateSolidBrush, SetWindowRgn, RGN_DIFF,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetSystemMetrics,
    PeekMessageW, RegisterClassExW, SetLayeredWindowAttributes, SetWindowDisplayAffinity,
    SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA, MSG, PM_REMOVE,
    SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Frame thickness in physical pixels.
const BORDER_PX: i32 = 5;
/// COLORREF is 0x00BBGGRR.
const RED: COLORREF = COLORREF(0x0000_00FF);
const TICK: Duration = Duration::from_millis(100);

/// Red frame around the primary monitor, shown while a viewer is connected.
///
/// A layered, topmost, click-through popup whose window region is cut into a
/// frame. It is excluded from screen capture (`WDA_EXCLUDEFROMCAPTURE`), so it is
/// seen by people at the host but not in the stream the viewer receives (on
/// Windows older than 10 2004 it appears in the stream as well).
///
/// The window follows the input desktop (user desktop, sign-in/lock screen, UAC
/// prompt): when it changes, the window is destroyed and recreated there. Capture
/// only publishes frames of the desktop the border is currently visible on.
pub struct BorderOverlay {
    active: Arc<AtomicBool>,
    /// The window is on screen on the input desktop (set a tick after ShowWindow).
    shown: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BorderOverlay {
    pub fn new() -> Result<Self, String> {
        let active = Arc::new(AtomicBool::new(false));
        let shown = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (a, v, s) = (Arc::clone(&active), Arc::clone(&shown), Arc::clone(&stop));
        let thread = std::thread::Builder::new()
            .name("dalker-border".into())
            .spawn(move || unsafe { run(a, v, s, ready_tx) })
            .map_err(|e| format!("spawn overlay thread: {e}"))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                active,
                shown,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("overlay thread did not start".into()),
        }
    }
}

impl ViewerIndicator for BorderOverlay {
    fn name(&self) -> &str {
        "win32-border"
    }

    fn set_active(&self, active: bool) {
        self.active.store(active, Ordering::SeqCst);
    }

    fn shown(&self) -> bool {
        self.shown.load(Ordering::SeqCst)
    }
}

impl Drop for BorderOverlay {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

fn screen_size() -> (i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_CXSCREEN).max(1),
            GetSystemMetrics(SM_CYSCREEN).max(1),
        )
    }
}

unsafe fn apply_frame_region(hwnd: HWND, w: i32, h: i32) {
    let outer = CreateRectRgn(0, 0, w, h);
    let inner = CreateRectRgn(BORDER_PX, BORDER_PX, w - BORDER_PX, h - BORDER_PX);
    CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
    // The window takes ownership of `outer`; `inner` is ours to free.
    let _ = SetWindowRgn(hwnd, Some(outer), true);
    let _ = windows::Win32::Graphics::Gdi::DeleteObject(inner.into());
}

/// Keeps the border on the host's screen but out of every screen capture, our own
/// stream included (Windows 10 2004+). On older Windows it shows up in the stream too.
unsafe fn exclude_from_capture(hwnd: HWND) {
    static WARNED: std::sync::Once = std::sync::Once::new();
    if let Err(e) = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) {
        WARNED.call_once(|| {
            tracing::warn!("border cannot be hidden from capture ({e}; needs Windows 10 2004 or later); viewers will see it too")
        });
    }
}

unsafe fn create_window() -> windows::core::Result<(HWND, i32, i32)> {
    let hinst = HINSTANCE(GetModuleHandleW(PCWSTR::null())?.0);
    let class = w!("DalkerBorder");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinst,
        hbrBackground: CreateSolidBrush(RED),
        lpszClassName: class,
        ..Default::default()
    };
    // 0 with ERROR_CLASS_ALREADY_EXISTS is fine (second overlay in one process).
    RegisterClassExW(&wc);

    let (w, h) = screen_size();
    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
        class,
        w!("Dalker remote session"),
        WS_POPUP,
        0,
        0,
        w,
        h,
        None,
        None,
        Some(hinst),
        None,
    )?;
    SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
    apply_frame_region(hwnd, w, h);
    exclude_from_capture(hwnd);
    Ok((hwnd, w, h))
}

unsafe fn run(
    active: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    // Without access to the input desktop (normal user process at the secure desktop)
    // the window simply stays on the thread's own desktop.
    let mut desk = desktop::attach_thread_to_input_desktop().ok();
    let (mut hwnd, mut cw, mut ch) = match create_window() {
        Ok(v) => {
            let _ = ready.send(Ok(()));
            v
        }
        Err(e) => {
            let _ = ready.send(Err(format!("create border window: {e}")));
            return;
        }
    };

    let mut shown = false;
    // The border counts as visible from the tick after ShowWindow, once the
    // compositor has had a chance to draw it.
    let mut announced = false;
    while !stop.load(Ordering::SeqCst) {
        let input = desktop::input_desktop_name();
        if input.is_some() && input != desk {
            // A thread cannot switch desktops while it owns windows.
            desktop::set_border_desktop(None);
            visible.store(false, Ordering::SeqCst);
            announced = false;
            shown = false;
            let _ = DestroyWindow(hwnd);
            drain_messages();
            match desktop::attach_thread_to_input_desktop() {
                Ok(name) => {
                    tracing::info!(desktop = %name, "border moved to the input desktop");
                    desk = Some(name);
                }
                Err(e) => {
                    tracing::warn!("border cannot follow the input desktop: {e}");
                    // Don't retry every tick; the gate below uses the real thread desktop.
                    desk = input.clone();
                }
            }
            match create_window() {
                Ok(v) => (hwnd, cw, ch) = v,
                Err(e) => {
                    tracing::error!("recreate border window: {e}");
                    std::thread::sleep(TICK);
                    match create_window() {
                        Ok(v) => (hwnd, cw, ch) = v,
                        Err(_) => return,
                    }
                }
            }
        }

        drain_messages();

        let (nw, nh) = screen_size();
        if (nw, nh) != (cw, ch) {
            let _ = SetWindowPos(hwnd, None, 0, 0, nw, nh, SWP_NOACTIVATE | SWP_NOZORDER);
            apply_frame_region(hwnd, nw, nh);
            (cw, ch) = (nw, nh);
        }

        let want = active.load(Ordering::SeqCst);
        if want != shown {
            if !want {
                desktop::set_border_desktop(None);
                visible.store(false, Ordering::SeqCst);
                announced = false;
            }
            let _ = ShowWindow(hwnd, if want { SW_SHOWNOACTIVATE } else { SW_HIDE });
            shown = want;
        } else if shown && !announced {
            desktop::set_border_desktop(desktop::thread_desktop_name());
            visible.store(true, Ordering::SeqCst);
            announced = true;
        }
        if shown {
            // Keep winning against other topmost windows.
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        std::thread::sleep(TICK);
    }
    desktop::set_border_desktop(None);
    visible.store(false, Ordering::SeqCst);
    let _ = DestroyWindow(hwnd);
}

unsafe fn drain_messages() {
    let mut msg = MSG::default();
    while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}
