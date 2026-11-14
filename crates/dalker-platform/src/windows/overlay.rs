use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use dalker_core::{PlatformError, ViewerIndicator};

use super::desktop;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateFontW, CreateRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EndPaint, InvalidateRect, SelectObject, SetBkMode, SetTextColor, SetWindowRgn,
    CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH, DT_CENTER, DT_SINGLELINE, DT_VCENTER,
    FF_SWISS, FW_BOLD, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PROOF_QUALITY, RGN_DIFF, RGN_OR,
    TRANSPARENT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect,
    GetSystemMetrics, PeekMessageW, RegisterClassExW, SetLayeredWindowAttributes,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, TranslateMessage, HWND_TOPMOST, LWA_ALPHA,
    MSG, PM_REMOVE, SM_CXSCREEN, SM_CYSCREEN, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SW_HIDE, SW_SHOWNOACTIVATE, WDA_EXCLUDEFROMCAPTURE, WM_PAINT, WNDCLASSEXW, WS_EX_LAYERED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

/// Frame thickness in physical pixels.
const BORDER_PX: i32 = 5;
const LABEL_W: i32 = 96;
const LABEL_H: i32 = 22;
/// COLORREF is 0x00BBGGRR.
const RED: COLORREF = COLORREF(0x0000_00FF);
const WHITE: COLORREF = COLORREF(0x00FF_FFFF);
const TICK: Duration = Duration::from_millis(100);

thread_local! {
    static CURRENT_LABEL: RefCell<Option<String>> = const { RefCell::new(None) };
}

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
    label: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl BorderOverlay {
    pub fn new() -> Result<Self, PlatformError> {
        let active = Arc::new(AtomicBool::new(false));
        let shown = Arc::new(AtomicBool::new(false));
        let label = Arc::new(Mutex::new(None));
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = mpsc::channel::<Result<(), String>>();
        let (a, v, l, s) = (
            Arc::clone(&active),
            Arc::clone(&shown),
            Arc::clone(&label),
            Arc::clone(&stop),
        );
        let thread = std::thread::Builder::new()
            .name("dalker-border".into())
            .spawn(move || unsafe { run(a, v, l, s, ready_tx) })
            .map_err(|e| PlatformError::Fatal(format!("spawn overlay thread: {e}")))?;
        match ready_rx.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self {
                active,
                shown,
                label,
                stop,
                thread: Some(thread),
            }),
            // Retried: the desktop may be switching (sign-in screen, UAC prompt).
            Ok(Err(e)) => Err(PlatformError::Transient(e)),
            Err(_) => Err(PlatformError::Fatal("overlay thread did not start".into())),
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

    fn set_label(&self, label: Option<&str>) {
        *self.label.lock().unwrap_or_else(|e| e.into_inner()) = label.map(str::to_owned);
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
    if msg == WM_PAINT {
        let mut ps = PAINTSTRUCT::default();
        let hdc = BeginPaint(hwnd, &mut ps);
        let text = CURRENT_LABEL.with(|c| c.borrow().clone());
        if let Some(text) = text {
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                let left = (rc.right - LABEL_W) / 2;
                let mut badge = RECT {
                    left,
                    top: BORDER_PX,
                    right: left + LABEL_W,
                    bottom: BORDER_PX + LABEL_H,
                };
                let font = CreateFontW(
                    15,
                    0,
                    0,
                    0,
                    FW_BOLD.0 as i32,
                    0,
                    0,
                    0,
                    DEFAULT_CHARSET,
                    OUT_DEFAULT_PRECIS,
                    CLIP_DEFAULT_PRECIS,
                    PROOF_QUALITY,
                    (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
                    w!("Segoe UI"),
                );
                let old_font = SelectObject(hdc, font.into());
                SetBkMode(hdc, TRANSPARENT);
                SetTextColor(hdc, WHITE);
                let mut wide: Vec<u16> = text.encode_utf16().collect();
                DrawTextW(
                    hdc,
                    &mut wide,
                    &mut badge,
                    DT_CENTER | DT_VCENTER | DT_SINGLELINE,
                );
                SelectObject(hdc, old_font);
                let _ = DeleteObject(font.into());
            }
        }
        let _ = EndPaint(hwnd, &ps);
        return LRESULT(0);
    }
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

unsafe fn apply_frame_region(hwnd: HWND, w: i32, h: i32, has_label: bool) {
    let outer = CreateRectRgn(0, 0, w, h);
    let inner = CreateRectRgn(BORDER_PX, BORDER_PX, w - BORDER_PX, h - BORDER_PX);
    CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
    let _ = DeleteObject(inner.into());
    if has_label {
        let left = (w - LABEL_W) / 2;
        let badge = CreateRectRgn(left, BORDER_PX, left + LABEL_W, BORDER_PX + LABEL_H);
        CombineRgn(Some(outer), Some(outer), Some(badge), RGN_OR);
        let _ = DeleteObject(badge.into());
    }
    // The window takes ownership of `outer`.
    let _ = SetWindowRgn(hwnd, Some(outer), true);
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

unsafe fn create_window(has_label: bool) -> windows::core::Result<(HWND, i32, i32)> {
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
    apply_frame_region(hwnd, w, h, has_label);
    exclude_from_capture(hwnd);
    Ok((hwnd, w, h))
}

unsafe fn run(
    active: Arc<AtomicBool>,
    visible: Arc<AtomicBool>,
    label: Arc<Mutex<Option<String>>>,
    stop: Arc<AtomicBool>,
    ready: mpsc::Sender<Result<(), String>>,
) {
    // Without access to the input desktop (normal user process at the secure desktop)
    // the window simply stays on the thread's own desktop.
    let mut desk = desktop::attach_thread_to_input_desktop().ok();
    let mut cur_label = label.lock().unwrap_or_else(|e| e.into_inner()).clone();
    CURRENT_LABEL.with(|c| *c.borrow_mut() = cur_label.clone());
    let (mut hwnd, mut cw, mut ch) = match create_window(cur_label.is_some()) {
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
            match create_window(cur_label.is_some()) {
                Ok(v) => (hwnd, cw, ch) = v,
                Err(e) => {
                    tracing::error!("recreate border window: {e}");
                    std::thread::sleep(TICK);
                    match create_window(cur_label.is_some()) {
                        Ok(v) => (hwnd, cw, ch) = v,
                        Err(_) => return,
                    }
                }
            }
        }

        drain_messages();

        let (nw, nh) = screen_size();
        let next_label = label.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let label_changed = next_label != cur_label;
        if label_changed {
            cur_label = next_label;
            CURRENT_LABEL.with(|c| *c.borrow_mut() = cur_label.clone());
        }
        if (nw, nh) != (cw, ch) {
            let _ = SetWindowPos(hwnd, None, 0, 0, nw, nh, SWP_NOACTIVATE | SWP_NOZORDER);
            apply_frame_region(hwnd, nw, nh, cur_label.is_some());
            (cw, ch) = (nw, nh);
            let _ = InvalidateRect(Some(hwnd), None, true);
        } else if label_changed {
            apply_frame_region(hwnd, nw, nh, cur_label.is_some());
            let _ = InvalidateRect(Some(hwnd), None, true);
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
