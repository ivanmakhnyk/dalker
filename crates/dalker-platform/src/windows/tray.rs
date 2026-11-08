//! Notification-area (tray) icon for the Windows agent.
//!
//! * left click / double click: open the Dalker web client in the default browser
//! * right click: menu with "Open Dalker", "Settings" (the host's settings window: web access,
//!   password, MCP server and tokens), the number of connected viewers, "Disconnect AI agents"
//!   (while the MCP server is on), "Open log folder" and "Exit Dalker"
//! * the icon turns red and the tooltip says so while somebody is connected
//!
//! Everything runs on one dedicated thread that owns a hidden window and its message loop.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{CreateBitmap, DeleteObject, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD,
    NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyIcon,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, KillTimer,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassExW, RegisterWindowMessageW,
    SetForegroundWindow, SetTimer, TrackPopupMenu, TranslateMessage, HICON, ICONINFO,
    MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MF_DISABLED, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, MSG, SW_SHOWNORMAL, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CLOSE, WM_CONTEXTMENU, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP, WM_TIMER,
    WNDCLASSEXW, WS_POPUP,
};

const WM_TRAY: u32 = WM_APP + 1;
const TRAY_ID: u32 = 1;
const TIMER_ID: usize = 1;

const CMD_OPEN: usize = 1;
const CMD_SETTINGS: usize = 2;
const CMD_LOGS: usize = 3;
const CMD_EXIT: usize = 4;
const CMD_AI_AGENTS: usize = 5;

/// What the tray needs from the agent.
pub struct TrayConfig {
    /// Opened by a left click and by "Open Dalker", e.g. `http://127.0.0.1:8742/`; `None`
    /// while the web server is off.
    pub open_url: Box<dyn Fn() -> Option<String> + Send + Sync>,
    /// Called by "Settings": opens the host's settings window.
    pub on_settings: Box<dyn Fn() + Send + Sync>,
    /// Folder shown by "Open log folder" (skipped when `None`).
    pub log_dir: Option<String>,
    /// Number of connected viewers.
    pub viewers: Arc<AtomicUsize>,
    /// The "Disconnect AI agents" item; `None` hides it.
    pub ai_agents: Option<AiAgentsMenu>,
    /// Called once when the user confirms "Exit Dalker".
    pub on_exit: Box<dyn Fn() + Send + Sync>,
}

/// Lets the person at the host stop AI agents (MCP sessions) and keep them out.
pub struct AiAgentsMenu {
    /// Whether the MCP server is on (the item is hidden while it is off).
    pub running: Box<dyn Fn() -> bool + Send + Sync>,
    /// AI agents connected now.
    pub connected: Box<dyn Fn() -> usize + Send + Sync>,
    /// Whether AI agents are currently kept out.
    pub blocked: Box<dyn Fn() -> bool + Send + Sync>,
    /// `true`: disconnect them all and keep them out; `false`: allow them again.
    pub set_blocked: Box<dyn Fn(bool) + Send + Sync>,
}

/// Menu text of the AI agents item.
fn ai_agents_label(connected: usize, blocked: bool) -> String {
    match (blocked, connected) {
        (true, _) => "Allow AI agents again".to_owned(),
        (false, 0) => "Disconnect AI agents (none connected)".to_owned(),
        (false, 1) => "Disconnect AI agents (1 connected)".to_owned(),
        (false, n) => format!("Disconnect AI agents ({n} connected)"),
    }
}

struct State {
    cfg: TrayConfig,
    icon_idle: HICON,
    icon_busy: HICON,
    shown_viewers: usize,
    added: bool,
    taskbar_created: u32,
}

// SAFETY: HICON handles are process-wide GDI objects, only ever used on the tray thread.
unsafe impl Send for State {}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

/// Handle to the running tray thread. Dropping it removes the icon.
pub struct Tray {
    hwnd: isize,
    thread: Option<JoinHandle<()>>,
}

impl Tray {
    pub fn start(cfg: TrayConfig) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel::<Result<isize, String>>();
        let thread = std::thread::Builder::new()
            .name("dalker-tray".into())
            .spawn(move || unsafe { run(cfg, tx) })
            .map_err(|e| format!("spawn tray thread: {e}"))?;
        match rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok(hwnd)) => Ok(Self {
                hwnd,
                thread: Some(thread),
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("tray thread did not start".into()),
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(self.hwnd as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------- icon art

/// 32x32 premultiplied-BGRA icon: the outlined Dalker mark, coloured as `look` says.
fn icon_pixels(look: crate::icon::Look) -> Vec<u8> {
    let mut px = crate::icon::rgba(look);
    for p in px.chunks_exact_mut(4) {
        let a = p[3] as u32;
        let (r, g, b) = (p[0] as u32, p[1] as u32, p[2] as u32);
        p[0] = (b * a / 255) as u8;
        p[1] = (g * a / 255) as u8;
        p[2] = (r * a / 255) as u8;
    }
    px
}

unsafe fn make_icon(look: crate::icon::Look) -> Result<HICON, String> {
    let px = icon_pixels(look);
    let color = CreateBitmap(32, 32, 1, 32, Some(px.as_ptr() as *const _));
    let mask_bits = vec![0u8; 32 * 4]; // 1 bpp, 32 rows of 4 bytes; alpha channel does the work
    let mask = CreateBitmap(32, 32, 1, 1, Some(mask_bits.as_ptr() as *const _));
    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: color,
    };
    let icon = CreateIconIndirect(&info).map_err(|e| format!("CreateIconIndirect: {e}"));
    let _ = DeleteObject(HGDIOBJ(color.0));
    let _ = DeleteObject(HGDIOBJ(mask.0));
    icon
}

// ---------------------------------------------------------------------------- shell glue

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn tip_for(viewers: usize) -> String {
    match viewers {
        0 => "Dalker - ready (nobody connected)".to_owned(),
        1 => "Dalker - 1 viewer connected (red border on)".to_owned(),
        n => format!("Dalker - {n} viewers connected (red border on)"),
    }
}

unsafe fn notify(
    hwnd: HWND,
    message: windows::Win32::UI::Shell::NOTIFY_ICON_MESSAGE,
    st: &State,
) -> bool {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_TRAY,
        hIcon: if st.shown_viewers > 0 {
            st.icon_busy
        } else {
            st.icon_idle
        },
        ..Default::default()
    };
    for (i, c) in tip_for(st.shown_viewers)
        .encode_utf16()
        .take(127)
        .enumerate()
    {
        data.szTip[i] = c;
    }
    Shell_NotifyIconW(message, &data).as_bool()
}

unsafe fn open(url: &str) {
    let url = wide(url);
    ShellExecuteW(
        None,
        w!("open"),
        PCWSTR(url.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn show_menu(hwnd: HWND) {
    let (viewers, has_logs, ai_agents, web_on) = {
        let st = STATE.get().unwrap().lock().unwrap();
        let ai = st
            .cfg
            .ai_agents
            .as_ref()
            .filter(|a| (a.running)())
            .map(|a| ai_agents_label((a.connected)(), (a.blocked)()));
        (
            st.cfg.viewers.load(Ordering::SeqCst),
            st.cfg.log_dir.is_some(),
            ai,
            (st.cfg.open_url)().is_some(),
        )
    };
    let Ok(menu) = CreatePopupMenu() else { return };
    let item = |id: usize, text: &str| {
        let t = wide(text);
        let _ = AppendMenuW(menu, MF_STRING, id, PCWSTR(t.as_ptr()));
    };
    if web_on {
        item(CMD_OPEN, "Open Dalker in the browser");
    } else {
        let t = wide("Open Dalker in the browser (web access is off)");
        let _ = AppendMenuW(
            menu,
            MF_STRING | MF_GRAYED | MF_DISABLED,
            0,
            PCWSTR(t.as_ptr()),
        );
    }
    item(CMD_SETTINGS, "Settings...");
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    let status = wide(&match viewers {
        0 => "Nobody is connected".to_owned(),
        1 => "1 viewer connected".to_owned(),
        n => format!("{n} viewers connected"),
    });
    let _ = AppendMenuW(
        menu,
        MF_STRING | MF_GRAYED | MF_DISABLED,
        0,
        PCWSTR(status.as_ptr()),
    );
    if let Some(label) = &ai_agents {
        item(CMD_AI_AGENTS, label);
    }
    if has_logs {
        item(CMD_LOGS, "Open log folder");
    }
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
    item(CMD_EXIT, "Exit Dalker");

    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // Required so the menu closes when the user clicks elsewhere.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(
        menu,
        TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN,
        pt.x,
        pt.y,
        None,
        hwnd,
        None,
    );
    let _ = DestroyMenu(menu);
    handle_command(hwnd, cmd.0 as usize);
}

unsafe fn handle_command(hwnd: HWND, cmd: usize) {
    let (open_url, log_dir) = {
        let st = STATE.get().unwrap().lock().unwrap();
        ((st.cfg.open_url)(), st.cfg.log_dir.clone())
    };
    match cmd {
        CMD_OPEN => {
            if let Some(url) = open_url {
                open(&url);
            }
        }
        CMD_SETTINGS => (STATE.get().unwrap().lock().unwrap().cfg.on_settings)(),
        CMD_AI_AGENTS => {
            let st = STATE.get().unwrap().lock().unwrap();
            if let Some(a) = &st.cfg.ai_agents {
                (a.set_blocked)(!(a.blocked)());
            }
        }
        CMD_LOGS => {
            if let Some(dir) = log_dir {
                open(&dir);
            }
        }
        CMD_EXIT => {
            let answer = MessageBoxW(
                Some(hwnd),
                w!("Stop Dalker?\n\nNobody will be able to connect to this PC until you sign in to Windows again."),
                w!("Dalker"),
                MB_YESNO | MB_ICONQUESTION | MB_SETFOREGROUND | MB_TOPMOST,
            );
            if answer == windows::Win32::UI::WindowsAndMessaging::IDYES {
                (STATE.get().unwrap().lock().unwrap().cfg.on_exit)();
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            }
        }
        _ => {}
    }
}

unsafe fn refresh(hwnd: HWND) {
    let mut st = STATE.get().unwrap().lock().unwrap();
    let viewers = st.cfg.viewers.load(Ordering::SeqCst);
    if !st.added {
        // The taskbar may not exist yet right after logon: keep trying.
        st.shown_viewers = viewers;
        st.added = notify(hwnd, NIM_ADD, &st);
    } else if viewers != st.shown_viewers {
        st.shown_viewers = viewers;
        notify(hwnd, NIM_MODIFY, &st);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_TRAY => {
            match (lp.0 as u32) & 0xFFFF {
                WM_RBUTTONUP | WM_CONTEXTMENU => show_menu(hwnd),
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => handle_command(hwnd, CMD_OPEN),
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER => {
            refresh(hwnd);
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let _ = KillTimer(Some(hwnd), TIMER_ID);
            if let Some(m) = STATE.get() {
                let mut st = m.lock().unwrap();
                if st.added {
                    notify(hwnd, NIM_DELETE, &st);
                    st.added = false;
                }
                let _ = DestroyIcon(st.icon_idle);
                let _ = DestroyIcon(st.icon_busy);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        other => {
            // Explorer restarted: its notification area is empty again.
            if let Some(m) = STATE.get() {
                let mut st = m.lock().unwrap();
                if other == st.taskbar_created && other != 0 {
                    st.added = false;
                    drop(st);
                    refresh(hwnd);
                    return LRESULT(0);
                }
            }
            DefWindowProcW(hwnd, msg, wp, lp)
        }
    }
}

unsafe fn run(cfg: TrayConfig, ready: mpsc::Sender<Result<isize, String>>) {
    let setup = || -> Result<HWND, String> {
        let icon_idle = make_icon(crate::icon::Look::Normal)?;
        let icon_busy = make_icon(crate::icon::Look::Connected)?;
        let taskbar_created = RegisterWindowMessageW(w!("TaskbarCreated"));
        STATE
            .set(Mutex::new(State {
                cfg,
                icon_idle,
                icon_busy,
                shown_viewers: 0,
                added: false,
                taskbar_created,
            }))
            .map_err(|_| "tray already running".to_owned())?;

        let hinst = HINSTANCE(
            GetModuleHandleW(PCWSTR::null())
                .map_err(|e| e.to_string())?
                .0,
        );
        let class = w!("DalkerTray");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            Default::default(),
            class,
            w!("Dalker tray"),
            WS_POPUP,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        )
        .map_err(|e| format!("create tray window: {e}"))?;
        refresh(hwnd);
        SetTimer(Some(hwnd), TIMER_ID, 500, None);
        Ok(hwnd)
    };
    let hwnd = match setup() {
        Ok(h) => {
            let _ = ready.send(Ok(h.0 as isize));
            h
        }
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    let _ = hwnd;
    let mut msg = MSG::default();
    while GetMessageW(&mut msg, None, 0, 0).as_bool() {
        let _ = TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icon_art_has_alpha_and_both_colours() {
        for look in [crate::icon::Look::Normal, crate::icon::Look::Connected] {
            let px = icon_pixels(look);
            assert_eq!(px.len(), 32 * 32 * 4);
            // Corner is transparent, the D's counter is the opaque dark outline colour.
            assert_eq!(px[3], 0, "corner pixel must be transparent");
            let dark = crate::icon::DARK;
            let counter = (10 * 32 + 11) * 4;
            assert_eq!(&px[counter..counter + 4], &[dark.2, dark.1, dark.0, 255]);
            // A pixel on the stem of the D (premultiplied BGRA, opaque): orange when idle,
            // red while connected.
            let stem = (14 * 32 + 7) * 4;
            let (b, g, r, a) = (px[stem], px[stem + 1], px[stem + 2], px[stem + 3]);
            match look {
                crate::icon::Look::Normal => assert!(
                    r > 200 && g > 70 && g < 150 && b < 80 && a == 255,
                    "{:?}",
                    (b, g, r, a)
                ),
                crate::icon::Look::Connected => {
                    let red = crate::icon::RED;
                    assert_eq!((b, g, r, a), (red.2, red.1, red.0, 255))
                }
            }
        }
    }

    #[test]
    fn tooltips() {
        assert!(tip_for(0).contains("ready"));
        assert!(tip_for(1).contains("1 viewer"));
        assert!(tip_for(3).contains("3 viewers"));
        assert_eq!(
            ai_agents_label(0, false),
            "Disconnect AI agents (none connected)"
        );
        assert_eq!(
            ai_agents_label(2, false),
            "Disconnect AI agents (2 connected)"
        );
        assert_eq!(ai_agents_label(1, true), "Allow AI agents again");
        assert!(tip_for(1).encode_utf16().count() < 127);
    }
}
