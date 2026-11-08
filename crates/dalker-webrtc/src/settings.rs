//! Per-viewer stream settings on the `"control"` DataChannel.
//!
//! The viewer asks for a frame rate, a bitrate and a downscale for its own stream; the host
//! clamps them to its own limits (the agent's `--fps` and `--bitrate-kbps` are maximums) and
//! answers with what it applied. A missing field means the host's default, so every request
//! is complete and `{"type":"settings"}` alone resets the stream.
//! ```json
//! {"type":"settings","fps":15,"bitrate_kbps":3000,"downscale":2}             viewer → host
//! {"type":"settings_applied","fps":15,"bitrate_kbps":3000,"downscale":2,
//!  "max_fps":30,"max_bitrate_kbps":6000,"max_downscale":3}                    host → viewer
//! ```

use serde::Deserialize;

use crate::encoder::EncoderSettings;

/// Lowest bitrate a viewer can ask for, in kbit/s.
pub const MIN_BITRATE_KBPS: u32 = 250;
/// Largest extra downscale a viewer can ask for (on top of the host's `max_width`).
pub const MAX_DOWNSCALE: u32 = 3;

#[derive(Debug, Default, Deserialize, PartialEq)]
pub(crate) struct Request {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    fps: Option<f64>,
    #[serde(default)]
    bitrate_kbps: Option<f64>,
    #[serde(default)]
    downscale: Option<f64>,
}

/// A settings request from the viewer, or `None` for anything else.
pub(crate) fn parse(data: &[u8]) -> Option<Request> {
    if !data.windows(10).any(|w| w == b"\"settings\"") {
        return None;
    }
    serde_json::from_slice::<Request>(data)
        .ok()
        .filter(|r| r.kind == "settings")
}

/// The host's `base` settings with the request applied, never above the host's limits.
pub(crate) fn apply(base: &EncoderSettings, req: &Request) -> EncoderSettings {
    let clamp = |v: Option<f64>, lo: u32, hi: u32| -> Option<u32> {
        v.filter(|v| v.is_finite())
            .map(|v| (v.round().max(0.0) as u32).clamp(lo, hi.max(lo)))
    };
    let max_fps = base.max_fps.max(1.0).round() as u32;
    let max_kbps = (base.bitrate_bps / 1000).max(MIN_BITRATE_KBPS);
    let mut s = *base;
    if let Some(fps) = clamp(req.fps, 1, max_fps) {
        s.max_fps = fps as f32;
    }
    if let Some(kbps) = clamp(req.bitrate_kbps, MIN_BITRATE_KBPS, max_kbps) {
        s.bitrate_bps = kbps * 1000;
    }
    s.downscale = clamp(req.downscale, 1, MAX_DOWNSCALE).unwrap_or(base.downscale);
    // Below the host's bitrate the viewer wants a real cap (e.g. a slow link), so enforce it.
    s.strict_bitrate = base.strict_bitrate || s.bitrate_bps < base.bitrate_bps;
    s
}

/// The answer to a settings request.
pub(crate) fn applied_message(base: &EncoderSettings, s: &EncoderSettings) -> String {
    serde_json::json!({
        "type": "settings_applied",
        "fps": s.max_fps.round() as u32,
        "bitrate_kbps": s.bitrate_bps / 1000,
        "downscale": s.downscale,
        "max_fps": base.max_fps.round() as u32,
        "max_bitrate_kbps": base.bitrate_bps / 1000,
        "max_downscale": MAX_DOWNSCALE,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> EncoderSettings {
        EncoderSettings {
            bitrate_bps: 6_000_000,
            max_fps: 30.0,
            max_width: 1920,
            keyframe_interval: 300,
            downscale: 1,
            strict_bitrate: false,
        }
    }

    #[test]
    fn only_settings_messages_parse() {
        assert!(parse(br#"{"type":"key","code":"KeyA","down":true}"#).is_none());
        assert!(parse(br#"{"type":"clipboard","text":"\"settings\""}"#).is_none());
        assert!(parse(br#"{"type":"settings_applied","fps":5}"#).is_none());
        assert_eq!(
            parse(br#"{"type":"settings"}"#),
            Some(Request {
                kind: "settings".into(),
                ..Default::default()
            })
        );
    }

    #[test]
    fn requests_are_clamped_to_the_host_limits() {
        let req =
            parse(br#"{"type":"settings","fps":144,"bitrate_kbps":50000,"downscale":9}"#).unwrap();
        let s = apply(&base(), &req);
        assert_eq!(
            (s.max_fps, s.bitrate_bps, s.downscale),
            (30.0, 6_000_000, MAX_DOWNSCALE)
        );
        let req = parse(br#"{"type":"settings","fps":0,"bitrate_kbps":1,"downscale":0}"#).unwrap();
        let s = apply(&base(), &req);
        assert_eq!(
            (s.max_fps, s.bitrate_bps, s.downscale),
            (1.0, MIN_BITRATE_KBPS * 1000, 1)
        );
        let req =
            parse(br#"{"type":"settings","fps":15,"bitrate_kbps":1500,"downscale":2}"#).unwrap();
        let s = apply(&base(), &req);
        assert_eq!(
            (s.max_fps, s.bitrate_bps, s.downscale, s.max_width),
            (15.0, 1_500_000, 2, 1920)
        );
        assert!(s.strict_bitrate, "a lower bitrate is enforced");
        assert!(
            !apply(
                &base(),
                &parse(br#"{"type":"settings","bitrate_kbps":6000}"#).unwrap()
            )
            .strict_bitrate
        );
    }

    #[test]
    fn missing_fields_mean_the_host_default() {
        let s = apply(&base(), &parse(br#"{"type":"settings","fps":10}"#).unwrap());
        assert_eq!(
            (s.max_fps, s.bitrate_bps, s.downscale),
            (10.0, 6_000_000, 1)
        );
        let msg: serde_json::Value = serde_json::from_str(&applied_message(&base(), &s)).unwrap();
        assert_eq!(
            msg,
            serde_json::json!({"type": "settings_applied", "fps": 10, "bitrate_kbps": 6000, "downscale": 1,
                               "max_fps": 30, "max_bitrate_kbps": 6000, "max_downscale": MAX_DOWNSCALE})
        );
    }
}
