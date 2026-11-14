//! The OpenH264 backend (on the CPU): used when there is no hardware encoder.

use anyhow::{Context, Result};
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, UsageType,
};
use openh264::formats::YUVSlices;
use openh264::OpenH264API;

use super::{Backend, Config, I420};

/// Encoder threads. OpenH264 only spreads a frame over threads when it is cut into slices,
/// and gains little past four (1080p full-screen change: 66 ms on one thread, 34 ms on four).
const MAX_THREADS: usize = 4;

/// Largest slice (NAL unit) in bytes: lets the encoder work on slices in parallel, and each
/// slice fits one RTP packet, so a lost packet costs one slice instead of the whole frame.
const MAX_SLICE_BYTES: u32 = 1200;

pub(crate) struct OpenH264 {
    encoder: Encoder,
}

impl OpenH264 {
    pub fn open(config: &Config) -> Result<Self> {
        let encoder_config = EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .profile(Profile::Baseline)
            .rate_control_mode(RateControlMode::Bitrate)
            .bitrate(BitRate::from_bps(config.bitrate_bps))
            .max_frame_rate(FrameRate::from_hz(config.fps))
            .skip_frames(config.strict_bitrate)
            .intra_frame_period(IntraFramePeriod::from_num_frames(config.keyframe_interval))
            .num_threads(
                std::thread::available_parallelism().map_or(1, |n| n.get().min(MAX_THREADS)) as u16,
            )
            .max_slice_len(MAX_SLICE_BYTES);
        let encoder = Encoder::with_api_config(OpenH264API::from_source(), encoder_config)
            .context("create OpenH264 encoder")?;
        Ok(Self { encoder })
    }
}

impl Backend for OpenH264 {
    fn encode(&mut self, yuv: &I420, idr: bool) -> Result<Option<(Vec<u8>, bool)>> {
        if idr {
            self.encoder.force_intra_frame();
        }
        let dims = (yuv.width, yuv.height);
        let src = YUVSlices::new(
            (&yuv.y, &yuv.u, &yuv.v),
            dims,
            (dims.0, dims.0 / 2, dims.0 / 2),
        );
        let bitstream = self.encoder.encode(&src).context("H.264 encode")?;
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
