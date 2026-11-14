//! H.264 SPS and PPS for the VA-API encoder (ITU-T H.264 7.3.2.1 and 7.3.2.2): handed to the
//! driver as packed headers, or put in front of a keyframe when the driver wrote none.
//! Constrained Baseline, one reference frame, `pic_order_cnt_type` 2 (output order is
//! decoding order, so a decoder never holds a frame back).

/// The parameters the SPS, the PPS and the driver's slice headers all follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Params {
    pub width: u32,
    pub height: u32,
    pub level_idc: u8,
    pub log2_max_frame_num_minus4: u32,
    pub pic_init_qp: u8,
}

impl Params {
    pub fn width_in_mbs(&self) -> u32 {
        self.width.div_ceil(16)
    }

    pub fn height_in_mbs(&self) -> u32 {
        self.height.div_ceil(16)
    }

    /// Frame cropping (in 2-pixel units for 4:2:0) to the right and bottom of the picture.
    pub fn crop(&self) -> (u32, u32) {
        (
            (self.width_in_mbs() * 16 - self.width) / 2,
            (self.height_in_mbs() * 16 - self.height) / 2,
        )
    }
}

/// The lowest level (Table A-1) that allows this picture size at `fps`.
pub fn level_idc(width: u32, height: u32, fps: f32) -> u8 {
    let frame = width.div_ceil(16) * height.div_ceil(16);
    let rate = (frame as f32 * fps.max(1.0)) as u32;
    const LEVELS: [(u8, u32, u32); 9] = [
        (31, 3_600, 108_000),
        (32, 5_120, 216_000),
        (40, 8_192, 245_760),
        (42, 8_704, 522_240),
        (50, 22_080, 589_824),
        (51, 36_864, 983_040),
        (52, 36_864, 2_073_600),
        (60, 139_264, 4_177_920),
        (62, 139_264, 16_711_680),
    ];
    LEVELS
        .iter()
        .find(|(_, max_frame, max_rate)| frame <= *max_frame && rate <= *max_rate)
        .map_or(62, |l| l.0)
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    n: u32,
}

impl BitWriter {
    fn bit(&mut self, b: bool) {
        self.cur = (self.cur << 1) | b as u8;
        self.n += 1;
        if self.n == 8 {
            self.bytes.push(self.cur);
            (self.cur, self.n) = (0, 0);
        }
    }

    fn u(&mut self, bits: u32, v: u32) {
        for i in (0..bits).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self, v: u32) {
        let v = v as u64 + 1;
        let len = 64 - v.leading_zeros();
        self.u(len - 1, 0);
        for i in (0..len).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Signed Exp-Golomb.
    fn se(&mut self, v: i32) {
        self.ue(if v > 0 {
            2 * v as u32 - 1
        } else {
            2 * v.unsigned_abs()
        });
    }

    /// `rbsp_trailing_bits`, then the RBSP as a NAL unit with a start code and emulation
    /// prevention bytes.
    fn nal(mut self, header: u8) -> Vec<u8> {
        self.bit(true);
        while self.n != 0 {
            self.bit(false);
        }
        let mut out = vec![0, 0, 0, 1, header];
        let mut zeros = 0;
        for b in self.bytes {
            if zeros >= 2 && b <= 3 {
                out.push(3);
                zeros = 0;
            }
            zeros = if b == 0 { zeros + 1 } else { 0 };
            out.push(b);
        }
        out
    }
}

/// The sequence parameter set NAL unit (with its start code).
pub fn sps(p: &Params) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.u(8, 66); // profile_idc: Baseline
    w.u(8, 0b1100_0000); // constraint_set0 and 1: Constrained Baseline
    w.u(8, p.level_idc as u32);
    w.ue(0); // seq_parameter_set_id
    w.ue(p.log2_max_frame_num_minus4);
    w.ue(2); // pic_order_cnt_type
    w.ue(1); // max_num_ref_frames
    w.bit(false); // gaps_in_frame_num_value_allowed_flag
    w.ue(p.width_in_mbs() - 1);
    w.ue(p.height_in_mbs() - 1);
    w.bit(true); // frame_mbs_only_flag
    w.bit(true); // direct_8x8_inference_flag
    let (right, bottom) = p.crop();
    w.bit(right != 0 || bottom != 0);
    if right != 0 || bottom != 0 {
        w.ue(0);
        w.ue(right);
        w.ue(0);
        w.ue(bottom);
    }
    w.bit(false); // vui_parameters_present_flag
    w.nal(0x67) // nal_ref_idc 3, type 7
}

/// The picture parameter set NAL unit (with its start code).
pub fn pps(p: &Params) -> Vec<u8> {
    let mut w = BitWriter::default();
    w.ue(0); // pic_parameter_set_id
    w.ue(0); // seq_parameter_set_id
    w.bit(false); // entropy_coding_mode_flag: CAVLC
    w.bit(false); // bottom_field_pic_order_in_frame_present_flag
    w.ue(0); // num_slice_groups_minus1
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.bit(false); // weighted_pred_flag
    w.u(2, 0); // weighted_bipred_idc
    w.se(p.pic_init_qp as i32 - 26);
    w.se(0); // pic_init_qs_minus26
    w.se(0); // chroma_qp_index_offset
    w.bit(true); // deblocking_filter_control_present_flag
    w.bit(false); // constrained_intra_pred_flag
    w.bit(false); // redundant_pic_cnt_present_flag
    w.nal(0x68) // nal_ref_idc 3, type 8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb() {
        let mut w = BitWriter::default();
        for v in [0, 1, 2, 3, 7] {
            w.ue(v);
        }
        w.se(-1);
        w.se(1);
        // 1 010 011 00100 0001000 011 010, then the stop bit and zeros.
        assert_eq!(
            w.nal(0),
            vec![
                0,
                0,
                0,
                1,
                0,
                0b1010_0110,
                0b0100_0001,
                0b0000_1101,
                0b0100_0000
            ]
        );
    }

    #[test]
    fn emulation_prevention() {
        let mut w = BitWriter::default();
        w.u(24, 0x00_00_01);
        assert_eq!(w.nal(0x67)[5..], [0, 0, 3, 1, 0x80]);
    }

    #[test]
    fn sps_for_1080p() {
        let p = Params {
            width: 1920,
            height: 1080,
            level_idc: level_idc(1920, 1080, 30.0),
            log2_max_frame_num_minus4: 4,
            pic_init_qp: 26,
        };
        assert_eq!(p.level_idc, 40);
        assert_eq!(
            (p.width_in_mbs(), p.height_in_mbs(), p.crop()),
            (120, 68, (0, 4))
        );
        let sps = sps(&p);
        assert_eq!(sps[..8], [0, 0, 0, 1, 0x67, 66, 0xc0, 40]);
        // seq id 0, frame num 4, poc type 2, 1 ref, no gaps: 1 00101 011 010 0
        assert_eq!(sps[8], 0b1001_0101);
        assert_eq!(
            pps(&p),
            vec![0, 0, 0, 1, 0x68, 0b1100_1110, 0b0011_1100, 0b1000_0000]
        );
    }

    #[test]
    fn levels() {
        assert_eq!(level_idc(1280, 720, 30.0), 31);
        assert_eq!(level_idc(1920, 1080, 60.0), 42);
        assert_eq!(level_idc(3840, 2160, 30.0), 51);
        assert_eq!(level_idc(3840, 2160, 120.0), 60);
    }
}
