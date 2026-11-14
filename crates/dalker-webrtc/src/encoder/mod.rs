//! BGRA → I420 conversion and the H.264 encoder.
//!
//! [`H264Encoder`] uses the GPU's encoder when there is one and falls back to OpenH264 on the
//! CPU otherwise ([`EncoderBackend`] chooses):
//!
//! * Windows: the hardware H.264 encoder that the graphics driver installs for Media
//!   Foundation (NVIDIA NVENC, Intel Quick Sync, AMD AMF), see [`mf`].
//! * Linux: VA-API (Intel, AMD, and NVIDIA through a VA-API driver), see [`vaapi`].
//!
//! A hardware encoder that can't be opened, or that fails while it runs, is given up for the
//! rest of the session and OpenH264 takes over from the next keyframe, so the stream goes on.

use anyhow::Result;
use dalker_core::{PixelFormat, RawFrame};

#[cfg(windows)]
mod mf;
mod software;
#[cfg(target_os = "linux")]
mod vaapi;

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
        *out = I420 {
            width: w,
            height: h,
            y: vec![0; w * h],
            u: vec![0; cw * ch],
            v: vec![0; cw * ch],
        };
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
        Self {
            bitrate_bps: 6_000_000,
            max_fps: 30.0,
            max_width: 1920,
            keyframe_interval: 300,
            downscale: 1,
            strict_bitrate: false,
        }
    }
}

/// Which H.264 encoder the host uses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum EncoderBackend {
    /// The GPU's encoder when there is one that works, otherwise OpenH264.
    #[default]
    Auto,
    /// Only the GPU's encoder (no fallback): for trying a hardware encoder out.
    Hardware,
    /// Always OpenH264.
    Software,
}

/// The encoder a session is using, for the viewer's connection details.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveEncoder {
    /// E.g. "NVIDIA NVENC", "Intel Quick Sync (VA-API)", "OpenH264".
    pub name: String,
    pub hardware: bool,
}

impl ActiveEncoder {
    /// The `encoder` message the viewer is sent when the session's encoder starts or changes:
    ///
    /// ```text
    /// {"type":"encoder","name":"NVIDIA NVENC","hardware":true}     host → viewer
    /// ```
    pub fn message(&self) -> String {
        serde_json::json!({"type": "encoder", "name": self.name, "hardware": self.hardware})
            .to_string()
    }
}

/// What a backend is opened with: the picture size and the rate control.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Config {
    pub width: usize,
    pub height: usize,
    pub bitrate_bps: u32,
    pub fps: f32,
    pub keyframe_interval: u32,
    pub strict_bitrate: bool,
}

/// One H.264 encoder implementation. Every backend produces a Constrained Baseline Annex-B
/// stream (what the viewer's SDP offers) with SPS and PPS in front of every keyframe.
pub(crate) trait Backend {
    /// Encodes one picture, a keyframe when `idr` is set. Returns `(bytes, is_keyframe)`, or
    /// `None` when the encoder skipped the picture (or has no output for it yet).
    fn encode(&mut self, yuv: &I420, idr: bool) -> Result<Option<(Vec<u8>, bool)>>;
}

/// Opens the platform's hardware encoder.
#[allow(unused_variables)]
fn open_hardware(config: &Config) -> Result<(Box<dyn Backend>, String)> {
    #[cfg(windows)]
    {
        mf::MfEncoder::open(config).map(|(e, name)| (Box::new(e) as Box<dyn Backend>, name))
    }
    #[cfg(target_os = "linux")]
    {
        vaapi::VaapiEncoder::open(config).map(|(e, name)| (Box::new(e) as Box<dyn Backend>, name))
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        anyhow::bail!("no hardware H.264 encoder support on this platform")
    }
}

/// Copies an I420 picture into an NV12 one (the layout hardware encoders take): `y` is the
/// luma plane with rows `y_pitch` bytes apart, `uv` the interleaved chroma plane with rows
/// `uv_pitch` apart. Rows and columns past the picture (a surface padded to whole
/// macroblocks) repeat the last row and column, so the padding costs few bits.
pub(crate) fn write_nv12(
    yuv: &I420,
    y: &mut [u8],
    y_pitch: usize,
    y_rows: usize,
    uv: &mut [u8],
    uv_pitch: usize,
    uv_rows: usize,
) {
    let (w, h) = (yuv.width, yuv.height);
    let (cw, ch) = (w / 2, h / 2);
    for row in 0..y_rows.min(y.len() / y_pitch.max(1)) {
        let src = &yuv.y[row.min(h - 1) * w..][..w];
        let dst = &mut y[row * y_pitch..][..y_pitch];
        dst[..w].copy_from_slice(src);
        let last = src[w - 1];
        dst[w..].fill(last);
    }
    for row in 0..uv_rows.min(uv.len() / uv_pitch.max(1)) {
        let r = row.min(ch - 1) * cw;
        let (us, vs) = (&yuv.u[r..r + cw], &yuv.v[r..r + cw]);
        let dst = &mut uv[row * uv_pitch..][..uv_pitch];
        for (i, pair) in dst.chunks_exact_mut(2).enumerate() {
            let x = i.min(cw - 1);
            pair[0] = us[x];
            pair[1] = vs[x];
        }
    }
}

/// Whether an Annex-B stream holds an SPS NAL unit.
pub(crate) fn has_sps(annex_b: &[u8]) -> bool {
    nal_types(annex_b).any(|t| t == 7)
}

/// The NAL unit types in an Annex-B stream, in order.
pub(crate) fn nal_types(annex_b: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut i = 0;
    std::iter::from_fn(move || {
        while i + 3 < annex_b.len() {
            let start = annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1;
            i += 1;
            if start {
                i += 2;
                return Some(annex_b[i] & 0x1f);
            }
        }
        None
    })
}

/// H.264 Constrained Baseline encoder tuned for low-latency screen content: the hardware
/// encoder when there is one ([`EncoderBackend`]), OpenH264 otherwise.
pub struct H264Encoder {
    settings: EncoderSettings,
    choice: EncoderBackend,
    backend: Option<Box<dyn Backend>>,
    config: Option<Config>,
    active: Option<ActiveEncoder>,
    /// The hardware encoder failed in this session: OpenH264 from now on.
    hardware_failed: bool,
    yuv: I420,
    force_idr: bool,
}

impl H264Encoder {
    /// An OpenH264 encoder.
    pub fn new(settings: EncoderSettings) -> Self {
        Self::with_backend(settings, EncoderBackend::Software)
    }

    /// An encoder that uses `choice` (the hardware one is opened with the first frame).
    pub fn with_backend(settings: EncoderSettings, choice: EncoderBackend) -> Self {
        Self {
            settings,
            choice,
            backend: None,
            config: None,
            active: None,
            hardware_failed: false,
            yuv: I420 {
                width: 0,
                height: 0,
                y: vec![],
                u: vec![],
                v: vec![],
            },
            force_idr: true,
        }
    }

    pub fn request_keyframe(&mut self) {
        self.force_idr = true;
    }

    /// The encoder in use, once the first frame was encoded.
    pub fn active(&self) -> Option<&ActiveEncoder> {
        self.active.as_ref()
    }

    /// Switches to new settings; the encoder is rebuilt (starting with a keyframe) when the
    /// bitrate or frame rate changed, and on the next frame size change anyway.
    pub fn set_settings(&mut self, settings: EncoderSettings) {
        self.settings = settings;
    }

    fn wants_hardware(&self) -> bool {
        match self.choice {
            EncoderBackend::Auto => !self.hardware_failed,
            EncoderBackend::Hardware => true,
            EncoderBackend::Software => false,
        }
    }

    fn open(&mut self, config: Config) -> Result<()> {
        self.backend = None;
        let mut opened = None;
        if self.wants_hardware() {
            match open_hardware(&config) {
                Ok((backend, name)) => {
                    opened = Some((
                        backend,
                        ActiveEncoder {
                            name,
                            hardware: true,
                        },
                    ))
                }
                Err(e) if self.choice == EncoderBackend::Hardware => {
                    return Err(e.context("open the hardware H.264 encoder"))
                }
                Err(e) => {
                    tracing::info!("no hardware H.264 encoder, using OpenH264: {e:#}");
                    self.hardware_failed = true;
                }
            }
        }
        let (backend, active) = match opened {
            Some(o) => o,
            None => (
                Box::new(software::OpenH264::open(&config)?) as Box<dyn Backend>,
                ActiveEncoder {
                    name: "OpenH264".into(),
                    hardware: false,
                },
            ),
        };
        tracing::info!(
            width = config.width,
            height = config.height,
            encoder = active.name,
            hardware = active.hardware,
            "configuring H.264 encoder"
        );
        self.backend = Some(backend);
        self.config = Some(config);
        self.active = Some(active);
        self.force_idr = true;
        Ok(())
    }

    /// Encodes one frame into an Annex-B byte stream. Returns `(bytes, is_keyframe)`,
    /// or `None` if the encoder decided to skip the frame.
    pub fn encode(&mut self, frame: &RawFrame) -> Result<Option<(Vec<u8>, bool)>> {
        let factor = downscaled(
            scale_factor(frame.width, self.settings.max_width),
            self.settings.downscale,
            frame.width,
        );
        bgra_to_i420(frame, factor, &mut self.yuv);
        let (width, height) = (self.yuv.width, self.yuv.height);
        if width < 16 || height < 16 {
            anyhow::bail!("frame too small: {width}x{height}");
        }
        let config = Config {
            width,
            height,
            bitrate_bps: self.settings.bitrate_bps,
            fps: self.settings.max_fps,
            keyframe_interval: self.settings.keyframe_interval,
            strict_bitrate: self.settings.strict_bitrate,
        };
        if self.backend.is_none() || self.config != Some(config) {
            self.open(config)?;
        }
        let idr = std::mem::take(&mut self.force_idr);
        let backend = self.backend.as_mut().expect("encoder opened above");
        match backend.encode(&self.yuv, idr) {
            Ok(out) => Ok(out),
            Err(e) if self.choice == EncoderBackend::Auto && self.active_is_hardware() => {
                tracing::warn!("hardware H.264 encoder failed, switching to OpenH264: {e:#}");
                self.hardware_failed = true;
                self.open(config)?;
                self.force_idr = false;
                let backend = self.backend.as_mut().expect("encoder opened above");
                backend.encode(&self.yuv, true)
            }
            Err(e) => Err(e),
        }
    }

    fn active_is_hardware(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.hardware)
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
        let mut out = I420 {
            width: 0,
            height: 0,
            y: vec![],
            u: vec![],
            v: vec![],
        };
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
        assert_eq!(
            downscaled(2, 3, 600),
            2,
            "never below the host's own factor"
        );
        let mut out = I420 {
            width: 0,
            height: 0,
            y: vec![],
            u: vec![],
            v: vec![],
        };
        bgra_to_i420(&solid(1367, 769, [10, 20, 30, 255]), 2, &mut out);
        assert_eq!((out.width, out.height), (682, 384));
        assert_eq!(out.u.len(), 341 * 192);
    }

    #[test]
    fn nv12_interleaves_chroma_and_repeats_the_edges() {
        let yuv = I420 {
            width: 4,
            height: 2,
            y: vec![1, 2, 3, 4, 5, 6, 7, 8],
            u: vec![10, 11],
            v: vec![20, 21],
        };
        // Padded to 6x4 (pitches 6), as a surface of whole macroblocks would be.
        let (mut y, mut uv) = (vec![0; 6 * 4], vec![0; 6 * 2]);
        write_nv12(&yuv, &mut y, 6, 4, &mut uv, 6, 2);
        assert_eq!(&y[..12], &[1, 2, 3, 4, 4, 4, 5, 6, 7, 8, 8, 8]);
        assert_eq!(&y[12..], &[5, 6, 7, 8, 8, 8, 5, 6, 7, 8, 8, 8]);
        assert_eq!(uv, vec![10, 20, 11, 21, 11, 21, 10, 20, 11, 21, 11, 21]);
    }

    #[test]
    fn nal_unit_types() {
        let stream = [
            0, 0, 0, 1, 0x67, 9, 9, 0, 0, 1, 0x68, 9, 0, 0, 0, 1, 0x65, 1, 2,
        ];
        assert_eq!(nal_types(&stream).collect::<Vec<_>>(), vec![7, 8, 5]);
        assert!(has_sps(&stream));
        assert!(!has_sps(&[0, 0, 1, 0x41, 0, 0]));
    }

    #[test]
    fn auto_streams_from_some_encoder() {
        // OpenH264 here unless this machine has a GPU encoder; either way frames come out.
        let mut cap = MockCapturer::new(640, 360, 120);
        let mut enc = H264Encoder::with_backend(EncoderSettings::default(), EncoderBackend::Auto);
        assert!(enc.active().is_none());
        let mut got = 0;
        for _ in 0..10 {
            let f = cap.next_frame(Duration::from_secs(1)).unwrap().unwrap();
            got += enc.encode(&f).unwrap().is_some() as u32;
        }
        assert!(got >= 5, "{got} frames");
        let active = enc.active().expect("an encoder").clone();
        assert!(!active.name.is_empty());
        let msg: serde_json::Value = serde_json::from_str(&active.message()).unwrap();
        assert_eq!(msg["type"], "encoder");
        assert_eq!(msg["hardware"], active.hardware);
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
        assert!(
            enc.encode(&f).unwrap().unwrap().1,
            "keyframe after a settings change"
        );
        assert_eq!((enc.yuv.width, enc.yuv.height), (320, 180));
    }
}
