//! BGRA → I420 conversion and the OpenH264 wrapper.

use anyhow::{Context, Result};
use dalker_core::{PixelFormat, RawFrame};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

/// Planar 4:2:0 image with even dimensions.
pub struct I420 {
    pub width: usize,
    pub height: usize,
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// Picks the integer downscale factor so the output is at most `max_width`
/// wide (`0` = never downscale).
pub fn scale_factor(width: u32, max_width: u32) -> usize {
    if max_width == 0 || width <= max_width {
        1
    } else {
        width.div_ceil(max_width) as usize
    }
}

/// Never downscale a picture below this width on the viewer's request.
const MIN_DOWNSCALED_WIDTH: u32 = 320;

/// The host's `factor` times the viewer's extra `downscale`, but the extra part stops before
/// the picture gets narrower than [`MIN_DOWNSCALED_WIDTH`].
pub fn downscaled(factor: usize, downscale: u32, width: u32) -> usize {
    let mut f = factor * downscale.max(1) as usize;
    while f > factor && (width as usize / f) < MIN_DOWNSCALED_WIDTH as usize {
        f -= 1;
    }
    f
}

/// Converts a BGRA frame to I420 (BT.601 limited range), downscaling by the
/// integer `factor` (nearest sample) and cropping to even dimensions.
pub fn bgra_to_i420(frame: &RawFrame, factor: usize, out: &mut I420) {
    debug_assert_eq!(frame.format, PixelFormat::Bgra8);
    let f = factor.max(1);
    let w = (frame.width as usize / f) & !1;
    let h = (frame.height as usize / f) & !1;
    let (cw, ch) = (w / 2, h / 2);
    if out.width != w || out.height != h {
        *out = I420 { width: w, height: h, y: vec![0; w * h], u: vec![0; cw * ch], v: vec![0; cw * ch] };
    }
    let data = &frame.data[..];
    let px = |x: usize, y: usize| -> (i32, i32, i32) {
        let i = (y * f) * frame.stride + (x * f) * 4;
        (data[i + 2] as i32, data[i + 1] as i32, data[i] as i32)
    };

    for cy in 0..ch {
        for cx in 0..cw {
            let (mut rs, mut gs, mut bs) = (0, 0, 0);
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let (x, y) = (cx * 2 + dx, cy * 2 + dy);
                let (r, g, b) = px(x, y);
                out.y[y * w + x] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                rs += r;
                gs += g;
                bs += b;
            }
            let (r, g, b) = (rs / 4, gs / 4, bs / 4);
            out.u[cy * cw + cx] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
            out.v[cy * cw + cx] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
        }
    }
}

/// Encoder settings.
#[derive(Debug, Clone, Copy)]
pub struct EncoderSettings {
    pub bitrate_bps: u32,
    pub max_fps: f32,
    /// Downscale wider sources to at most this width (`0` = native).
    pub max_width: u32,
    /// Periodic IDR interval in frames (helps recovery from packet loss).
    pub keyframe_interval: u32,
    /// Extra integer downscale on top of `max_width` (`1` = none), chosen by the viewer.
    pub downscale: u32,
    /// Hold `bitrate_bps` by skipping frames when needed. Off, OpenH264 treats the bitrate
    /// as a loose target only (it can't enforce it without skipping); a viewer that asks
    /// for less than the host's bitrate gets it on.
    pub strict_bitrate: bool,
}

impl Default for EncoderSettings {
    fn default() -> Self {
        Self { bitrate_bps: 6_000_000, max_fps: 30.0, max_width: 1920, keyframe_interval: 300, downscale: 1, strict_bitrate: false }
    }
}

/// Encoder threads. OpenH264 only spreads a frame over threads when it is cut into slices,
/// and gains little past four (1080p full-screen change: 66 ms on one thread, 34 ms on four).
const MAX_THREADS: usize = 4;

/// Largest slice (NAL unit) in bytes: lets the encoder work on slices in parallel, and each
/// slice fits one RTP packet, so a lost packet costs one slice instead of the whole frame.
const MAX_SLICE_BYTES: u32 = 1200;

/// H.264 Constrained Baseline encoder tuned for low-latency screen content.
pub struct H264Encoder {
    settings: EncoderSettings,
    encoder: Option<Encoder>,
    dims: (usize, usize),
    yuv: I420,
    force_idr: bool,
}

impl H264Encoder {
    pub fn new(settings: EncoderSettings) -> Self {
        Self {
            settings,
            encoder: None,
            dims: (0, 0),
            yuv: I420 { width: 0, height: 0, y: vec![], u: vec![], v: vec![] },
            force_idr: true,
        }
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// Switches to new settings; the encoder is rebuilt (starting with a keyframe) when the
    /// bitrate or frame rate changed, and on the next frame size change anyway.
    pub fn set_settings(&mut self, settings: EncoderSettings) {
        let rc = |s: &EncoderSettings| (s.bitrate_bps, s.max_fps, s.strict_bitrate);
        if rc(&settings) != rc(&self.settings) {
            self.encoder = None;
        }
        self.settings = settings;
    }

    fn make_encoder(&self) -> Result<Encoder> {
        let config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(self.settings.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(self.settings.max_fps))
            .skip_frames(self.settings.strict_bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(self.settings.keyframe_interval))
            .num_threads(std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_THREADS)) as u16)
            .max_slice_len(MAX_SLICE_BYTES);
        Encoder::with_api_config(OpenH264API::from_source(), config).context("create OpenH264 encoder")
    }

    /// Encodes one frame into an Annex-B byte stream. Returns `(bytes, is_keyframe)`,
    /// or `None` if the encoder decided to skip the frame.
    pub fn encode(&mut self, frame: &RawFrame) -> Result<Option<(Vec<u8>, bool)>> {
        let factor = downscaled(scale_factor(frame.width, self.settings.max_width), self.settings.downscale, frame.width);
        bgra_to_i420(frame, factor, &mut self.yuv);
        let dims = (self.yuv.width, self.yuv.height);
        if dims.0 < 16 || dims.1 < 16 {
            anyhow::bail!("frame too small: {}x{}", dims.0, dims.1);
        }
        if self.encoder.is_none() || self.dims != dims {
            tracing::info!(width = dims.0, height = dims.1, factor, "configuring H.264 encoder");
            self.encoder = Some(self.make_encoder()?);
            self.dims = dims;
            self.force_idr = true;
        }
        let enc = self.encoder.as_mut().expect("encoder initialised above");
        if std::mem::take(&mut self.force_idr) {
            enc.force_intra_frame();
        }
        let src = YUVSlices::new(
            (&self.yuv.y, &self.yuv.u, &self.yuv.v),
            dims,
            (dims.0, dims.0 / 2, dims.0 / 2),
        );
        let bitstream = enc.encode(&src).context("H.264 encode")?;
        let kind = bitstream.frame_type();
        if matches!(kind, FrameType::Skip | FrameType::Invalid) {
            return Ok(None);
        }
        let bytes = bitstream.to_vec();
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some((bytes, matches!(kind, FrameType::IDR | FrameType::I))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use dalker_core::{MockCapturer, ScreenCapturer};
    use std::time::Duration;

    fn solid(w: u32, h: u32, bgra: [u8; 4]) -> RawFrame {
        RawFrame {
            width: w,
            height: h,
            stride: w as usize * 4,
            format: PixelFormat::Bgra8,
            data: Bytes::from(bgra.repeat((w * h) as usize)),
            timestamp: Duration::ZERO,
        }
    }

    #[test]
    fn colour_conversion_reference_values() {
        let mut out = I420 { width: 0, height: 0, y: vec![], u: vec![], v: vec![] };
        bgra_to_i420(&solid(4, 4, [255, 255, 255, 255]), 1, &mut out);
        assert_eq!((out.y[0], out.u[0], out.v[0]), (235, 128, 128));
        bgra_to_i420(&solid(4, 4, [0, 0, 0, 255]), 1, &mut out);
        assert_eq!((out.y[0], out.u[0], out.v[0]), (16, 128, 128));
        bgra_to_i420(&solid(4, 4, [0, 0, 255, 255]), 1, &mut out); // pure red
        assert_eq!((out.y[0], out.u[0], out.v[0]), (82, 90, 240));
    }

    #[test]
    fn downscale_and_even_crop() {
        assert_eq!(scale_factor(3840, 1920), 2);
        assert_eq!(scale_factor(2560, 1920), 2);
        assert_eq!(scale_factor(1920, 1920), 1);
        assert_eq!(scale_factor(5000, 0), 1);
        assert_eq!(downscaled(1, 2, 1920), 2);
        assert_eq!(downscaled(2, 2, 3840), 4);
        assert_eq!(downscaled(1, 3, 800), 2, "stops at 320 px wide");
        assert_eq!(downscaled(2, 3, 600), 2, "never below the host's own factor");
        let mut out = I420 { width: 0, height: 0, y: vec![], u: vec![], v: vec![] };
        bgra_to_i420(&solid(1367, 769, [10, 20, 30, 255]), 2, &mut out);
        assert_eq!((out.width, out.height), (682, 384));
        assert_eq!(out.u.len(), 341 * 192);
    }

    #[test]
    fn encodes_mock_frames_starting_with_idr() {
        let mut cap = MockCapturer::new(640, 360, 120);
        let mut enc = H264Encoder::new(EncoderSettings::default());
        let first = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        let (bytes, key) = enc.encode(&first).unwrap().expect("first frame encoded");
        assert!(key, "first frame must be a keyframe");
        assert_eq!(&bytes[..4], &[0, 0, 0, 1], "Annex-B start code");
        let mut saw_delta = false;
        for _ in 0..5 {
            let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            if let Some((_, key)) = enc.encode(&f).unwrap() {
                saw_delta |= !key;
            }
        }
        assert!(saw_delta, "expected P-frames after the IDR");
        enc.request_keyframe();
        let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        assert!(enc.encode(&f).unwrap().unwrap().1, "forced keyframe");

        // New settings rebuild the encoder: keyframe first, at the new size.
        enc.set_settings(EncoderSettings {
            bitrate_bps: 1_000_000,
            max_fps: 15.0,
            downscale: 2,
            strict_bitrate: true,
            ..Default::default()
        });
        let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
        assert!(enc.encode(&f).unwrap().unwrap().1, "keyframe after a settings change");
        assert_eq!((enc.yuv.width, enc.yuv.height), (320, 180));
    }
}
