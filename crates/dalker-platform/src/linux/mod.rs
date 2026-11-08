//! Linux backends: X11 capture (MIT-SHM, `GetImage` fallback) and `/dev/uinput` injection.
//!
//! Requirements on the host:
//! * An X11 session reachable via `$DISPLAY` (+ `$XAUTHORITY` when running as a
//!   service). Wayland sessions need the PipeWire portal backend ([`pipewire`]),
//!   which is not implemented yet.
//! * Write access to `/dev/uinput` (see the README for the udev rule).

mod capture;
pub mod files;
pub mod gnome;
mod input;
mod overlay;
pub mod pipewire;

pub use capture::X11Capturer;
pub use input::UinputInjector;
pub use overlay::BorderOverlay;

/// True when this process was started inside a Wayland session (the launcher sets
/// `XDG_SESSION_TYPE=wayland` for the agent it starts as the session user).
pub fn is_wayland_session() -> bool {
    std::env::var("XDG_SESSION_TYPE").is_ok_and(|t| t == "wayland") || std::env::var_os("WAYLAND_DISPLAY").is_some()
}
