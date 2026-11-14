//! The VA-API hardware encoder (Linux): Intel Quick Sync (iHD and i965 drivers), AMD (Mesa),
//! and whatever else has a VA-API driver with H.264 encoding.
//!
//! libva is loaded at run time (see [`ffi`]). The encoder opens the first render node
//! (`/dev/dri/renderD*`) whose driver encodes H.264 Constrained Baseline, and encodes I and P
//! frames with one reference frame and one slice per picture. Rate control is CBR where the
//! driver has it, VBR otherwise.

mod ffi;
mod headers;

use std::ffi::{c_void, CStr};
use std::os::fd::AsRawFd;
use std::ptr;

use anyhow::{bail, Context, Result};

use self::ffi::*;
use super::{has_sps, write_nv12, Backend, Config, I420};

/// Recon (reference) surfaces: the picture being encoded and the one it refers to.
const RECON_SURFACES: usize = 2;

pub(crate) struct VaapiEncoder {
    va: &'static Va,
    /// Keeps the render node open while the display uses it.
    _node: std::fs::File,
    dpy: VADisplay,
    config_id: VAConfigID,
    context: VAContextID,
    input: VASurfaceID,
    recon: [VASurfaceID; RECON_SURFACES],
    coded: VABufferID,
    rate_control: u32,
    packed_headers: u32,
    params: headers::Params,
    settings: Config,
    /// Pictures since the last IDR (the next one's index).
    since_idr: u32,
    idr_pic_id: u16,
    /// Which recon surface the next picture is reconstructed into (the other one is its
    /// reference).
    current: usize,
}

impl VaapiEncoder {
    pub fn open(config: &Config) -> Result<(Self, String)> {
        let va = Va::get().map_err(|e| anyhow::anyhow!("VA-API: {e}"))?;
        let mut errors = Vec::new();
        let mut nodes: Vec<_> = std::fs::read_dir("/dev/dri")
            .context("no /dev/dri")?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("renderD"))
            })
            .collect();
        nodes.sort();
        for path in nodes {
            match Self::open_node(va, &path, config) {
                Ok(opened) => return Ok(opened),
                Err(e) => errors.push(format!("{}: {e:#}", path.display())),
            }
        }
        if errors.is_empty() {
            bail!("no GPU render node in /dev/dri");
        }
        bail!("{}", errors.join("; "))
    }

    fn open_node(
        va: &'static Va,
        path: &std::path::Path,
        config: &Config,
    ) -> Result<(Self, String)> {
        let node = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .context("open")?;
        // SAFETY: a valid DRM fd, kept open by `node` for as long as the display lives.
        let dpy = unsafe { (va.vaGetDisplayDRM)(node.as_raw_fd()) };
        if dpy.is_null() {
            bail!("no VA display");
        }
        let (mut major, mut minor) = (0, 0);
        // SAFETY: `dpy` is a fresh display.
        if let Err(e) = va.check(
            unsafe { (va.vaInitialize)(dpy, &mut major, &mut minor) },
            "vaInitialize",
        ) {
            // SAFETY: terminating a display that failed to initialise is allowed.
            unsafe { (va.vaTerminate)(dpy) };
            return Err(e);
        }
        // From here on `Drop` cleans up whatever was created.
        let mut enc = Self {
            va,
            _node: node,
            dpy,
            config_id: VA_INVALID_ID,
            context: VA_INVALID_ID,
            input: VA_INVALID_SURFACE,
            recon: [VA_INVALID_SURFACE; RECON_SURFACES],
            coded: VA_INVALID_ID,
            rate_control: 0,
            packed_headers: 0,
            params: headers::Params {
                width: config.width as u32,
                height: config.height as u32,
                level_idc: headers::level_idc(
                    config.width as u32,
                    config.height as u32,
                    config.fps,
                ),
                log2_max_frame_num_minus4: 4,
                pic_init_qp: 26,
            },
            settings: *config,
            since_idr: 0,
            idr_pic_id: 0,
            current: 0,
        };
        // SAFETY: an initialised display; the string is static in the driver.
        let vendor = unsafe { CStr::from_ptr((va.vaQueryVendorString)(dpy)) }
            .to_string_lossy()
            .into_owned();
        enc.setup().with_context(|| vendor.clone())?;
        tracing::info!(
            node = %path.display(),
            vendor,
            "VA-API {major}.{minor} H.264 encoder (rate control {:#x}, packed headers {:#x})",
            enc.rate_control,
            enc.packed_headers
        );
        Ok((enc, encoder_name(&vendor)))
    }

    fn setup(&mut self) -> Result<()> {
        let (va, dpy) = (self.va, self.dpy);
        let profile = VA_PROFILE_H264_CONSTRAINED_BASELINE;
        // SAFETY: plain queries on an initialised display, into buffers of the stated size.
        let entrypoint = unsafe {
            let mut list = vec![0; (va.vaMaxNumEntrypoints)(dpy).max(1) as usize];
            let mut n = 0;
            va.check(
                (va.vaQueryConfigEntrypoints)(dpy, profile, list.as_mut_ptr(), &mut n),
                "no H.264 Constrained Baseline support",
            )?;
            list.truncate(n.max(0) as usize);
            [VA_ENTRYPOINT_ENC_SLICE, VA_ENTRYPOINT_ENC_SLICE_LP]
                .into_iter()
                .find(|e| list.contains(e))
                .context("the driver doesn't encode H.264 Constrained Baseline")?
        };
        let mut attribs = [
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: 0,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: 0,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: 0,
            },
        ];
        // SAFETY: `attribs` holds 3 entries.
        va.check(
            unsafe {
                (va.vaGetConfigAttributes)(dpy, profile, entrypoint, attribs.as_mut_ptr(), 3)
            },
            "vaGetConfigAttributes",
        )?;
        let supported = |a: &VAConfigAttrib| {
            if a.value == VA_ATTRIB_NOT_SUPPORTED {
                0
            } else {
                a.value
            }
        };
        if supported(&attribs[0]) & VA_RT_FORMAT_YUV420 == 0 {
            bail!("the driver doesn't encode YUV 4:2:0");
        }
        let rc = supported(&attribs[1]);
        self.rate_control = if rc & VA_RC_CBR != 0 {
            VA_RC_CBR
        } else if rc & VA_RC_VBR != 0 {
            VA_RC_VBR
        } else {
            bail!("the driver has no bitrate control (rate control modes {rc:#x})");
        };
        self.packed_headers =
            supported(&attribs[2]) & (VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_PICTURE);
        let mut chosen = vec![
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RT_FORMAT,
                value: VA_RT_FORMAT_YUV420,
            },
            VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_RATE_CONTROL,
                value: self.rate_control,
            },
        ];
        if self.packed_headers != 0 {
            chosen.push(VAConfigAttrib {
                type_: VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS,
                value: self.packed_headers,
            });
        }
        // SAFETY: `chosen` holds `chosen.len()` entries; the IDs are written on success.
        unsafe {
            va.check(
                (va.vaCreateConfig)(
                    dpy,
                    profile,
                    entrypoint,
                    chosen.as_mut_ptr(),
                    chosen.len() as i32,
                    &mut self.config_id,
                ),
                "vaCreateConfig",
            )?;
            let (w, h) = (
                self.params.width_in_mbs() * 16,
                self.params.height_in_mbs() * 16,
            );
            va.check(
                (va.vaCreateSurfaces)(
                    dpy,
                    VA_RT_FORMAT_YUV420,
                    w,
                    h,
                    &mut self.input,
                    1,
                    ptr::null_mut(),
                    0,
                ),
                "create the input surface",
            )?;
            va.check(
                (va.vaCreateSurfaces)(
                    dpy,
                    VA_RT_FORMAT_YUV420,
                    w,
                    h,
                    self.recon.as_mut_ptr(),
                    RECON_SURFACES as u32,
                    ptr::null_mut(),
                    0,
                ),
                "create the reference surfaces",
            )?;
            va.check(
                (va.vaCreateContext)(
                    dpy,
                    self.config_id,
                    w as i32,
                    h as i32,
                    VA_PROGRESSIVE,
                    self.recon.as_mut_ptr(),
                    RECON_SURFACES as i32,
                    &mut self.context,
                ),
                "vaCreateContext",
            )?;
            // Room for an uncompressed picture and then some (a keyframe of noise).
            let coded_size = w * h * 3 / 2 + 64 * 1024;
            va.check(
                (va.vaCreateBuffer)(
                    dpy,
                    self.context,
                    VA_ENC_CODED_BUFFER_TYPE,
                    coded_size,
                    1,
                    ptr::null_mut(),
                    &mut self.coded,
                ),
                "create the coded buffer",
            )?;
        }
        Ok(())
    }

    /// Copies the picture into the input surface.
    fn upload(&mut self, yuv: &I420) -> Result<()> {
        let (va, dpy) = (self.va, self.dpy);
        let mut image = VAImage::default();
        // SAFETY: `input` is a live surface; a derived image maps the surface itself.
        let status = unsafe { (va.vaDeriveImage)(dpy, self.input, &mut image) };
        let derived = status == VA_STATUS_SUCCESS && image.format.fourcc == VA_FOURCC_NV12;
        if !derived {
            if status == VA_STATUS_SUCCESS {
                // SAFETY: the image vaDeriveImage made, in a format we can't write.
                unsafe { (va.vaDestroyImage)(dpy, image.image_id) };
            }
            let mut format = VAImageFormat {
                fourcc: VA_FOURCC_NV12,
                byte_order: 1, // VA_LSB_FIRST
                bits_per_pixel: 12,
                ..Default::default()
            };
            image = VAImage::default();
            // SAFETY: plain image creation.
            va.check(
                unsafe {
                    (va.vaCreateImage)(
                        dpy,
                        &mut format,
                        (self.params.width_in_mbs() * 16) as i32,
                        (self.params.height_in_mbs() * 16) as i32,
                        &mut image,
                    )
                },
                "vaCreateImage",
            )?;
        }
        let result = self.write_image(&image, yuv).and_then(|()| {
            if derived {
                return Ok(());
            }
            // SAFETY: `image` and `input` are live and the same size.
            va.check(
                unsafe {
                    (va.vaPutImage)(
                        dpy,
                        self.input,
                        image.image_id,
                        0,
                        0,
                        image.width as u32,
                        image.height as u32,
                        0,
                        0,
                        image.width as u32,
                        image.height as u32,
                    )
                },
                "vaPutImage",
            )
        });
        // SAFETY: the image made above.
        unsafe { (va.vaDestroyImage)(dpy, image.image_id) };
        result
    }

    fn write_image(&self, image: &VAImage, yuv: &I420) -> Result<()> {
        let (va, dpy) = (self.va, self.dpy);
        let mut data: *mut c_void = ptr::null_mut();
        // SAFETY: the image's buffer, mapped until the unmap below.
        va.check(
            unsafe { (va.vaMapBuffer)(dpy, image.buf, &mut data) },
            "map the input image",
        )?;
        let rows = image.height as usize;
        let (y_pitch, uv_pitch) = (image.pitches[0] as usize, image.pitches[1] as usize);
        let y_len = y_pitch * rows;
        let uv_len = uv_pitch * rows / 2;
        let fits = (image.offsets[0] as usize + y_len).max(image.offsets[1] as usize + uv_len)
            <= image.data_size as usize
            && y_pitch >= yuv.width
            && uv_pitch >= yuv.width
            && rows >= yuv.height;
        if fits {
            // SAFETY: both planes lie within the mapped `data_size` bytes (checked above), and
            // don't overlap (NV12 planes are separate).
            let (y, uv) = unsafe {
                let base = data as *mut u8;
                (
                    std::slice::from_raw_parts_mut(base.add(image.offsets[0] as usize), y_len),
                    std::slice::from_raw_parts_mut(base.add(image.offsets[1] as usize), uv_len),
                )
            };
            // Only the picture's own width: past it each row is padding of the pitch.
            write_nv12(yuv, y, y_pitch, rows, uv, uv_pitch, rows / 2);
        }
        // SAFETY: mapped above.
        unsafe { (va.vaUnmapBuffer)(dpy, image.buf) };
        if !fits {
            bail!("unexpected VA image layout");
        }
        Ok(())
    }

    /// Creates a parameter buffer holding `data`.
    fn buffer<T>(&self, kind: i32, data: &T) -> Result<VABufferID> {
        self.raw_buffer(kind, (data as *const T).cast(), std::mem::size_of::<T>())
    }

    fn raw_buffer(&self, kind: i32, data: *const u8, len: usize) -> Result<VABufferID> {
        let mut id = VA_INVALID_ID;
        // SAFETY: libva copies `len` bytes from `data` into the new buffer.
        self.va.check(
            unsafe {
                (self.va.vaCreateBuffer)(
                    self.dpy,
                    self.context,
                    kind,
                    len as u32,
                    1,
                    data as *mut c_void,
                    &mut id,
                )
            },
            "vaCreateBuffer",
        )?;
        Ok(id)
    }

    /// A misc parameter buffer: its type, then `data`.
    fn misc<T>(&self, kind: u32, data: &T) -> Result<VABufferID> {
        let mut bytes = kind.to_ne_bytes().to_vec();
        // SAFETY: `T` is a plain `repr(C)` struct of integers.
        bytes.extend_from_slice(unsafe {
            std::slice::from_raw_parts((data as *const T).cast::<u8>(), std::mem::size_of::<T>())
        });
        self.raw_buffer(
            VA_ENC_MISC_PARAMETER_BUFFER_TYPE,
            bytes.as_ptr(),
            bytes.len(),
        )
    }

    /// Packed header parameter and data buffers for one NAL unit.
    fn packed(&self, kind: u32, nal: &[u8], out: &mut Vec<VABufferID>) -> Result<()> {
        let param = VAEncPackedHeaderParameterBuffer {
            type_: kind,
            bit_length: (nal.len() * 8) as u32,
            has_emulation_bytes: 1,
            va_reserved: [0; 4],
        };
        out.push(self.buffer(VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE, &param)?);
        out.push(self.raw_buffer(
            VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE,
            nal.as_ptr(),
            nal.len(),
        )?);
        Ok(())
    }

    /// The parameter buffers for one picture.
    fn picture_buffers(&self, idr: bool, out: &mut Vec<VABufferID>) -> Result<()> {
        let p = &self.params;
        let s = &self.settings;
        let max_frame_num = 1u32 << (p.log2_max_frame_num_minus4 + 4);
        let frame_num = (self.since_idr % max_frame_num) as u16;
        let poc = (2 * self.since_idr) as i32;
        let recon = self.recon[self.current];
        let reference = self.recon[1 - self.current];
        let ref_pic = VAPictureH264 {
            picture_id: reference,
            frame_idx: (self.since_idr.wrapping_sub(1) % max_frame_num),
            flags: VA_PICTURE_H264_SHORT_TERM_REFERENCE,
            TopFieldOrderCnt: poc - 2,
            BottomFieldOrderCnt: poc - 2,
            va_reserved: [0; 4],
        };

        if idr {
            let (right, bottom) = p.crop();
            let seq = VAEncSequenceParameterBufferH264 {
                seq_parameter_set_id: 0,
                level_idc: p.level_idc,
                intra_period: s.keyframe_interval.max(1),
                intra_idr_period: s.keyframe_interval.max(1),
                ip_period: 1,
                bits_per_second: s.bitrate_bps,
                max_num_ref_frames: 1,
                picture_width_in_mbs: p.width_in_mbs() as u16,
                picture_height_in_mbs: p.height_in_mbs() as u16,
                seq_fields: bits(&[
                    (1, 2),                           // chroma_format_idc: 4:2:0
                    (1, 1),                           // frame_mbs_only_flag
                    (0, 1),                           // mb_adaptive_frame_field_flag
                    (0, 1),                           // seq_scaling_matrix_present_flag
                    (1, 1),                           // direct_8x8_inference_flag
                    (p.log2_max_frame_num_minus4, 4), // log2_max_frame_num_minus4
                    (2, 2),                           // pic_order_cnt_type
                    (0, 4),                           // log2_max_pic_order_cnt_lsb_minus4
                    (0, 1),                           // delta_pic_order_always_zero_flag
                ]),
                bit_depth_luma_minus8: 0,
                bit_depth_chroma_minus8: 0,
                num_ref_frames_in_pic_order_cnt_cycle: 0,
                offset_for_non_ref_pic: 0,
                offset_for_top_to_bottom_field: 0,
                offset_for_ref_frame: [0; 256],
                frame_cropping_flag: (right != 0 || bottom != 0) as u8,
                frame_crop_left_offset: 0,
                frame_crop_right_offset: right,
                frame_crop_top_offset: 0,
                frame_crop_bottom_offset: bottom,
                vui_parameters_present_flag: 0,
                vui_fields: 0,
                aspect_ratio_idc: 0,
                sar_width: 0,
                sar_height: 0,
                num_units_in_tick: 0,
                time_scale: 0,
                va_reserved: [0; 4],
            };
            out.push(self.buffer(VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE, &seq)?);
            let rc = VAEncMiscParameterRateControl {
                bits_per_second: s.bitrate_bps,
                target_percentage: if self.rate_control == VA_RC_CBR {
                    100
                } else {
                    90
                },
                window_size: 1000,
                initial_qp: p.pic_init_qp as u32,
                min_qp: 0,
                max_qp: 51,
                // disable_frame_skip unless the bitrate must be held.
                rc_flags: bits(&[(0, 1), (!s.strict_bitrate as u32, 1)]),
                ..Default::default()
            };
            out.push(self.misc(VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL, &rc)?);
            let fr = VAEncMiscParameterFrameRate {
                framerate: (s.fps.round().max(1.0) as u32) | (1 << 16),
                ..Default::default()
            };
            out.push(self.misc(VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE, &fr)?);
            let hrd = VAEncMiscParameterHRD {
                buffer_size: s.bitrate_bps,
                initial_buffer_fullness: s.bitrate_bps / 2,
                ..Default::default()
            };
            out.push(self.misc(VA_ENC_MISC_PARAMETER_TYPE_HRD, &hrd)?);
            if self.packed_headers & VA_ENC_PACKED_HEADER_SEQUENCE != 0 {
                self.packed(VA_ENC_PACKED_HEADER_TYPE_SEQUENCE, &headers::sps(p), out)?;
            }
            if self.packed_headers & VA_ENC_PACKED_HEADER_PICTURE != 0 {
                self.packed(VA_ENC_PACKED_HEADER_TYPE_PICTURE, &headers::pps(p), out)?;
            }
        }

        let mut references = [VAPictureH264::INVALID; 16];
        if !idr {
            references[0] = ref_pic;
        }
        let pic = VAEncPictureParameterBufferH264 {
            CurrPic: VAPictureH264 {
                picture_id: recon,
                frame_idx: frame_num as u32,
                flags: 0,
                TopFieldOrderCnt: poc,
                BottomFieldOrderCnt: poc,
                va_reserved: [0; 4],
            },
            ReferenceFrames: references,
            coded_buf: self.coded,
            pic_parameter_set_id: 0,
            seq_parameter_set_id: 0,
            last_picture: 0,
            frame_num,
            pic_init_qp: p.pic_init_qp,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            chroma_qp_index_offset: 0,
            second_chroma_qp_index_offset: 0,
            pic_fields: bits(&[
                (idr as u32, 1), // idr_pic_flag
                (1, 2),          // reference_pic_flag
                (0, 1),          // entropy_coding_mode_flag: CAVLC
                (0, 1),          // weighted_pred_flag
                (0, 2),          // weighted_bipred_idc
                (0, 1),          // constrained_intra_pred_flag
                (0, 1),          // transform_8x8_mode_flag
                (1, 1),          // deblocking_filter_control_present_flag
            ]),
            va_reserved: [0; 4],
        };
        out.push(self.buffer(VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE, &pic)?);

        let mut list0 = [VAPictureH264::INVALID; 32];
        if !idr {
            list0[0] = ref_pic;
        }
        let slice = VAEncSliceParameterBufferH264 {
            macroblock_address: 0,
            num_macroblocks: p.width_in_mbs() * p.height_in_mbs(),
            macroblock_info: VA_INVALID_ID,
            slice_type: if idr { 2 } else { 0 }, // I or P
            pic_parameter_set_id: 0,
            idr_pic_id: self.idr_pic_id,
            pic_order_cnt_lsb: 0,
            delta_pic_order_cnt_bottom: 0,
            delta_pic_order_cnt: [0; 2],
            direct_spatial_mv_pred_flag: 0,
            num_ref_idx_active_override_flag: 0,
            num_ref_idx_l0_active_minus1: 0,
            num_ref_idx_l1_active_minus1: 0,
            RefPicList0: list0,
            RefPicList1: [VAPictureH264::INVALID; 32],
            luma_log2_weight_denom: 0,
            chroma_log2_weight_denom: 0,
            luma_weight_l0_flag: 0,
            luma_weight_l0: [0; 32],
            luma_offset_l0: [0; 32],
            chroma_weight_l0_flag: 0,
            chroma_weight_l0: [[0; 2]; 32],
            chroma_offset_l0: [[0; 2]; 32],
            luma_weight_l1_flag: 0,
            luma_weight_l1: [0; 32],
            luma_offset_l1: [0; 32],
            chroma_weight_l1_flag: 0,
            chroma_weight_l1: [[0; 2]; 32],
            chroma_offset_l1: [[0; 2]; 32],
            cabac_init_idc: 0,
            slice_qp_delta: 0,
            disable_deblocking_filter_idc: 0,
            slice_alpha_c0_offset_div2: 0,
            slice_beta_offset_div2: 0,
            va_reserved: [0; 4],
        };
        out.push(self.buffer(VA_ENC_SLICE_PARAMETER_BUFFER_TYPE, &slice)?);
        Ok(())
    }

    fn render(&mut self, idr: bool) -> Result<()> {
        let (va, dpy, ctx) = (self.va, self.dpy, self.context);
        let mut buffers = Vec::with_capacity(12);
        let result = self.picture_buffers(idr, &mut buffers).and_then(|()| {
            // SAFETY: a live context and input surface; the buffers were made for it.
            unsafe {
                va.check((va.vaBeginPicture)(dpy, ctx, self.input), "vaBeginPicture")?;
                let rendered = va.check(
                    (va.vaRenderPicture)(dpy, ctx, buffers.as_mut_ptr(), buffers.len() as i32),
                    "vaRenderPicture",
                );
                let ended = va.check((va.vaEndPicture)(dpy, ctx), "vaEndPicture");
                rendered.and(ended)
            }
        });
        for id in buffers {
            // SAFETY: buffers made above; libva 2 leaves them to the caller after rendering.
            unsafe { (va.vaDestroyBuffer)(dpy, id) };
        }
        result
    }

    /// Waits for the picture and copies the coded bytes out.
    fn read_coded(&self) -> Result<Vec<u8>> {
        let (va, dpy) = (self.va, self.dpy);
        // SAFETY: a live surface with a picture in flight.
        va.check(
            unsafe { (va.vaSyncSurface)(dpy, self.input) },
            "vaSyncSurface",
        )?;
        let mut seg: *mut c_void = ptr::null_mut();
        // SAFETY: mapping the coded buffer yields a list of segments, valid until unmapped.
        va.check(
            unsafe { (va.vaMapBuffer)(dpy, self.coded, &mut seg) },
            "map the coded buffer",
        )?;
        let mut out = Vec::new();
        while !seg.is_null() {
            // SAFETY: `seg` points at a VACodedBufferSegment of the mapped list.
            let s = unsafe { &*(seg as *const VACodedBufferSegment) };
            if !s.buf.is_null() && s.size > 0 {
                // SAFETY: `size` bytes at `buf` belong to the segment.
                out.extend_from_slice(unsafe {
                    std::slice::from_raw_parts(s.buf as *const u8, s.size as usize)
                });
            }
            seg = s.next;
        }
        // SAFETY: mapped above.
        unsafe { (va.vaUnmapBuffer)(dpy, self.coded) };
        Ok(out)
    }
}

impl Backend for VaapiEncoder {
    fn encode(&mut self, yuv: &I420, idr: bool) -> Result<Option<(Vec<u8>, bool)>> {
        let idr = idr || self.since_idr >= self.settings.keyframe_interval.max(1);
        if idr {
            self.since_idr = 0;
            self.idr_pic_id = self.idr_pic_id.wrapping_add(1);
        }
        self.upload(yuv)?;
        self.render(idr)?;
        let mut bytes = self.read_coded()?;
        self.since_idr += 1;
        self.current = 1 - self.current;
        if bytes.is_empty() {
            return Ok(None);
        }
        if idr && !has_sps(&bytes) {
            // A driver that writes no SPS/PPS of its own and takes no packed ones.
            let mut with_headers = headers::sps(&self.params);
            with_headers.extend_from_slice(&headers::pps(&self.params));
            with_headers.append(&mut bytes);
            bytes = with_headers;
        }
        Ok(Some((bytes, idr)))
    }
}

impl Drop for VaapiEncoder {
    fn drop(&mut self) {
        let (va, dpy) = (self.va, self.dpy);
        // SAFETY: each object is destroyed once, only if it was created, before the display
        // is terminated.
        unsafe {
            if self.coded != VA_INVALID_ID {
                (va.vaDestroyBuffer)(dpy, self.coded);
            }
            if self.context != VA_INVALID_ID {
                (va.vaDestroyContext)(dpy, self.context);
            }
            if self.recon[0] != VA_INVALID_SURFACE {
                (va.vaDestroySurfaces)(dpy, self.recon.as_mut_ptr(), RECON_SURFACES as i32);
            }
            if self.input != VA_INVALID_SURFACE {
                (va.vaDestroySurfaces)(dpy, &mut self.input, 1);
            }
            if self.config_id != VA_INVALID_ID {
                (va.vaDestroyConfig)(dpy, self.config_id);
            }
            (va.vaTerminate)(dpy);
        }
    }
}

/// A name for the viewer from the driver's vendor string ("Intel iHD driver for Intel(R)
/// Gen Graphics - 24.1.0 ()", "Mesa Gallium driver 24.0.5 for AMD Radeon RX 6600 ...").
fn encoder_name(vendor: &str) -> String {
    let v = vendor.to_ascii_lowercase();
    if v.contains("intel") {
        "Intel Quick Sync (VA-API)".into()
    } else if v.contains("amd") || v.contains("radeon") {
        "AMD (VA-API)".into()
    } else if v.contains("nvidia") || v.contains("nvenc") {
        "NVIDIA (VA-API)".into()
    } else {
        "VA-API".into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_from_vendor_strings() {
        assert_eq!(
            encoder_name("Intel iHD driver for Intel(R) Gen Graphics - 24.1.0 ()"),
            "Intel Quick Sync (VA-API)"
        );
        assert_eq!(
            encoder_name("Mesa Gallium driver 24.0.5 for AMD Radeon RX 6600 (radeonsi, navi23)"),
            "AMD (VA-API)"
        );
        assert_eq!(
            encoder_name("VA-API NVDEC driver [direct backend]"),
            "VA-API"
        );
    }

    /// Opens the GPU's encoder when the machine has one (`DALKER_TEST_VAAPI=1` makes a
    /// missing one a failure) and checks the stream's shape.
    #[test]
    fn encodes_on_the_gpu_when_there_is_one() {
        let config = Config {
            width: 640,
            height: 360,
            bitrate_bps: 2_000_000,
            fps: 30.0,
            keyframe_interval: 300,
            strict_bitrate: false,
        };
        let mut enc = match VaapiEncoder::open(&config) {
            Ok((enc, name)) => {
                eprintln!("VA-API encoder: {name}");
                enc
            }
            Err(e) if std::env::var_os("DALKER_TEST_VAAPI").is_none() => {
                eprintln!("skipped, no VA-API encoder: {e:#}");
                return;
            }
            Err(e) => panic!("{e:#}"),
        };
        let mut yuv = I420 {
            width: 640,
            height: 360,
            y: vec![16; 640 * 360],
            u: vec![128; 320 * 180],
            v: vec![128; 320 * 180],
        };
        let (bytes, key) = enc.encode(&yuv, true).unwrap().expect("a keyframe");
        assert!(key);
        let types: Vec<_> = super::super::nal_types(&bytes).collect();
        assert!(
            types.contains(&7) && types.contains(&8) && types.contains(&5),
            "{types:?}"
        );
        for i in 0..5 {
            yuv.y[i * 1000..i * 1000 + 500].fill(235);
            let (bytes, key) = enc.encode(&yuv, false).unwrap().expect("a P frame");
            assert!(!key);
            assert!(super::super::nal_types(&bytes).any(|t| t == 1));
        }
    }
}
