//! The host's stream limits: the most frame rate and bitrate a viewer gets, and the width wider
//! screens are scaled down to. Viewers can ask for less in their own settings, never for more.
//!
//! They come from `--fps`, `--bitrate-kbps` and `--max-width`, or from `DALKER_FPS`,
//! `DALKER_BITRATE_KBPS` and `DALKER_MAX_WIDTH` in the config file (Dalker Settings writes
//! them). The running agent follows the file: running streams switch to new limits within
//! seconds, without a restart.

use dalker_webrtc::EncoderSettings;
use tokio::sync::watch;

use crate::config_watch;

pub const KEY_FPS: &str = "DALKER_FPS";
pub const KEY_BITRATE_KBPS: &str = "DALKER_BITRATE_KBPS";
pub const KEY_MAX_WIDTH: &str = "DALKER_MAX_WIDTH";

pub const DEFAULT_FPS: u32 = 30;
pub const DEFAULT_BITRATE_KBPS: u32 = 6000;
pub const DEFAULT_MAX_WIDTH: u32 = 1920;

/// Highest frame rate the host can send.
pub const MAX_FPS: u32 = 120;
/// Bitrate range in kbit/s (the low end is the least a viewer can ask for).
pub const MIN_BITRATE_KBPS: u32 = dalker_webrtc::settings::MIN_BITRATE_KBPS;
pub const MAX_BITRATE_KBPS: u32 = 100_000;
/// Narrowest `max_width` other than `0` (native), and the widest.
pub const MIN_MAX_WIDTH: u32 = 320;
pub const MAX_MAX_WIDTH: u32 = 16_384;

/// The host's limits for every viewer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// `0` = always native.
    pub max_width: u32,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            fps: DEFAULT_FPS,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
            max_width: DEFAULT_MAX_WIDTH,
        }
    }
}

impl Limits {
    /// The encoder settings a viewer starts with (before asking for less).
    pub fn encoder(&self) -> EncoderSettings {
        EncoderSettings {
            bitrate_bps: self.bitrate_kbps * 1000,
            max_fps: self.fps as f32,
            max_width: self.max_width,
            keyframe_interval: self.fps * 10,
            downscale: 1,
            strict_bitrate: false,
        }
    }

    /// What the config file asks for. Keys it doesn't have take their defaults; pinned keys
    /// (given on the command line or as their own environment variables) and values out of
    /// range keep `started`.
    pub fn from_config(file: &[(String, String)], started: Self, pinned: &[&str]) -> Self {
        let pick =
            |key: &str, started: u32, default: u32, parse: fn(&str) -> Result<u32, String>| {
                if pinned.contains(&key) {
                    return started;
                }
                match config_watch::value(file, key) {
                    None => default,
                    Some(v) => parse(v).unwrap_or_else(|e| {
                        tracing::warn!("ignoring {key}={v} in the config file: {e}");
                        started
                    }),
                }
            };
        Self {
            fps: pick(KEY_FPS, started.fps, DEFAULT_FPS, parse_fps),
            bitrate_kbps: pick(
                KEY_BITRATE_KBPS,
                started.bitrate_kbps,
                DEFAULT_BITRATE_KBPS,
                parse_bitrate_kbps,
            ),
            max_width: pick(
                KEY_MAX_WIDTH,
                started.max_width,
                DEFAULT_MAX_WIDTH,
                parse_max_width,
            ),
        }
    }
}

fn number(s: &str) -> Result<u32, String> {
    s.trim()
        .parse::<u32>()
        .map_err(|_| format!("`{}` is not a whole number", s.trim()))
}

/// A frame rate from 1 to [`MAX_FPS`].
pub fn parse_fps(s: &str) -> Result<u32, String> {
    let v = number(s)?;
    if !(1..=MAX_FPS).contains(&v) {
        return Err(format!("the frame rate must be from 1 to {MAX_FPS}"));
    }
    Ok(v)
}

/// A bitrate from [`MIN_BITRATE_KBPS`] to [`MAX_BITRATE_KBPS`] kbit/s.
pub fn parse_bitrate_kbps(s: &str) -> Result<u32, String> {
    let v = number(s)?;
    if !(MIN_BITRATE_KBPS..=MAX_BITRATE_KBPS).contains(&v) {
        return Err(format!(
            "the bitrate must be from {MIN_BITRATE_KBPS} to {MAX_BITRATE_KBPS} kbit/s"
        ));
    }
    Ok(v)
}

/// `0` (always native) or a width from [`MIN_MAX_WIDTH`] to [`MAX_MAX_WIDTH`] pixels.
pub fn parse_max_width(s: &str) -> Result<u32, String> {
    let v = number(s)?;
    if v != 0 && !(MIN_MAX_WIDTH..=MAX_MAX_WIDTH).contains(&v) {
        return Err(format!(
            "the width must be 0 (native) or from {MIN_MAX_WIDTH} to {MAX_MAX_WIDTH} pixels"
        ));
    }
    Ok(v)
}

/// Applies the limits the config file asks for to every viewer, running streams included.
pub async fn follow_config(
    mut config: watch::Receiver<config_watch::Snapshot>,
    started: Limits,
    pinned: Vec<&'static str>,
    apply: impl Fn(Limits),
) {
    let mut seen = Limits::from_config(&config.borrow_and_update(), started, &pinned);
    if seen != started {
        apply(seen);
    }
    while config.changed().await.is_ok() {
        let now = Limits::from_config(&config.borrow_and_update(), started, &pinned);
        if now != seen {
            tracing::info!(
                fps = now.fps,
                bitrate_kbps = now.bitrate_kbps,
                max_width = now.max_width,
                "stream limits changed on the host"
            );
            apply(now);
            seen = now;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn values_are_checked() {
        assert_eq!(parse_fps("60"), Ok(60));
        assert_eq!(parse_fps(" 120 "), Ok(120));
        assert!(parse_fps("0").is_err());
        assert!(parse_fps("121").is_err());
        assert!(parse_fps("fast").is_err());
        assert_eq!(parse_bitrate_kbps("250"), Ok(250));
        assert!(parse_bitrate_kbps("249").is_err());
        assert!(parse_bitrate_kbps("100001").is_err());
        assert_eq!(parse_max_width("0"), Ok(0));
        assert_eq!(parse_max_width("2560"), Ok(2560));
        assert!(parse_max_width("100").is_err());
        assert!(parse_max_width("-1").is_err());
    }

    #[test]
    fn follows_the_file() {
        let started = Limits {
            fps: 60,
            bitrate_kbps: 8000,
            max_width: 0,
        };
        assert_eq!(
            Limits::from_config(&[], started, &[]),
            Limits::default(),
            "missing keys take their defaults"
        );
        let f = file(&[
            (KEY_FPS, "90"),
            (KEY_BITRATE_KBPS, "12000"),
            (KEY_MAX_WIDTH, "2560"),
        ]);
        assert_eq!(
            Limits::from_config(&f, started, &[]),
            Limits {
                fps: 90,
                bitrate_kbps: 12000,
                max_width: 2560
            }
        );
        assert_eq!(
            Limits::from_config(&f, started, &[KEY_FPS]),
            Limits {
                fps: 60,
                bitrate_kbps: 12000,
                max_width: 2560
            },
            "a pinned key keeps its startup value"
        );
        let bad = file(&[(KEY_FPS, "500"), (KEY_MAX_WIDTH, "wide")]);
        let got = Limits::from_config(&bad, started, &[]);
        assert_eq!((got.fps, got.max_width), (60, 0), "bad values keep startup");
    }

    #[test]
    fn encoder_settings() {
        let s = Limits {
            fps: 60,
            bitrate_kbps: 10_000,
            max_width: 2560,
        }
        .encoder();
        assert_eq!(
            (s.max_fps, s.bitrate_bps, s.max_width, s.keyframe_interval),
            (60.0, 10_000_000, 2560, 600)
        );
    }
}
