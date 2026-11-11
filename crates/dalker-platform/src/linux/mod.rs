//! Linux backends: screen capture, `/dev/uinput` injection and the red border.
//!
//! * X11: the root window (MIT-SHM, `GetImage` fallback) and an X11 overlay window, reached
//!   through `$DISPLAY` (+ `$XAUTHORITY` when running as a service).
//! * GNOME (Wayland or Xorg): Mutter's screen cast and the Dalker Shell extension ([`gnome`]).
//! * Other Wayland desktops (KDE Plasma, sway, Hyprland...): the xdg-desktop-portal screen
//!   cast ([`portal`]) and a wlr-layer-shell border ([`wayland`]).
//! * Input everywhere: write access to `/dev/uinput` (see the README for the udev rule).

mod capture;
pub mod files;
pub mod gnome;
mod gst;
mod input;
pub mod kde;
mod overlay;
pub mod portal;
pub mod wayland;

pub use capture::X11Capturer;
pub use input::UinputInjector;
pub use overlay::BorderOverlay;

/// True when this process was started inside a Wayland session (the launcher sets
/// `XDG_SESSION_TYPE=wayland` for the agent it starts as the session user).
pub fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland")
        || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
