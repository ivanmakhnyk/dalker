//! The part of libva (`va.h`, `va_drm.h`, `va_enc_h264.h`, libva 2.x, MIT) that the VA-API
//! encoder uses. libva is loaded at run time (`dlopen`), so Dalker runs on hosts without it
//! and doesn't need its headers to build. Bit fields are plain `u32`s here: GCC and Clang
//! fill them from the lowest bit up, which [`bits`] follows.

#![allow(non_snake_case, non_camel_case_types, dead_code)]

use std::ffi::{c_char, c_int, c_uint, c_void, CStr};
use std::sync::OnceLock;

pub type VADisplay = *mut c_void;
pub type VAStatus = c_int;
pub type VAGenericID = c_uint;
pub type VAConfigID = VAGenericID;
pub type VAContextID = VAGenericID;
pub type VASurfaceID = VAGenericID;
pub type VABufferID = VAGenericID;
pub type VAImageID = VAGenericID;

pub const VA_STATUS_SUCCESS: VAStatus = 0;
pub const VA_INVALID_ID: VAGenericID = 0xffff_ffff;
pub const VA_INVALID_SURFACE: VASurfaceID = VA_INVALID_ID;

pub const VA_PROFILE_H264_CONSTRAINED_BASELINE: c_int = 13;
pub const VA_ENTRYPOINT_ENC_SLICE: c_int = 6;
pub const VA_ENTRYPOINT_ENC_SLICE_LP: c_int = 8;

pub const VA_CONFIG_ATTRIB_RT_FORMAT: c_int = 0;
pub const VA_CONFIG_ATTRIB_RATE_CONTROL: c_int = 5;
pub const VA_CONFIG_ATTRIB_ENC_PACKED_HEADERS: c_int = 10;
pub const VA_ATTRIB_NOT_SUPPORTED: u32 = 0x8000_0000;

pub const VA_RT_FORMAT_YUV420: u32 = 0x0000_0001;
pub const VA_RC_CBR: u32 = 0x0000_0002;
pub const VA_RC_VBR: u32 = 0x0000_0004;
pub const VA_ENC_PACKED_HEADER_SEQUENCE: u32 = 0x0000_0001;
pub const VA_ENC_PACKED_HEADER_PICTURE: u32 = 0x0000_0002;

pub const VA_PROGRESSIVE: c_int = 0x1;
pub const VA_FOURCC_NV12: u32 = 0x3231_564E;

pub const VA_PICTURE_H264_INVALID: u32 = 0x0000_0001;
pub const VA_PICTURE_H264_SHORT_TERM_REFERENCE: u32 = 0x0000_0008;

// VABufferType
pub const VA_ENC_CODED_BUFFER_TYPE: c_int = 21;
pub const VA_ENC_SEQUENCE_PARAMETER_BUFFER_TYPE: c_int = 22;
pub const VA_ENC_PICTURE_PARAMETER_BUFFER_TYPE: c_int = 23;
pub const VA_ENC_SLICE_PARAMETER_BUFFER_TYPE: c_int = 24;
pub const VA_ENC_PACKED_HEADER_PARAMETER_BUFFER_TYPE: c_int = 25;
pub const VA_ENC_PACKED_HEADER_DATA_BUFFER_TYPE: c_int = 26;
pub const VA_ENC_MISC_PARAMETER_BUFFER_TYPE: c_int = 27;

// VAEncMiscParameterType
pub const VA_ENC_MISC_PARAMETER_TYPE_FRAME_RATE: u32 = 0;
pub const VA_ENC_MISC_PARAMETER_TYPE_RATE_CONTROL: u32 = 1;
pub const VA_ENC_MISC_PARAMETER_TYPE_HRD: u32 = 5;

// VAEncPackedHeaderType
pub const VA_ENC_PACKED_HEADER_TYPE_SEQUENCE: u32 = 1;
pub const VA_ENC_PACKED_HEADER_TYPE_PICTURE: u32 = 2;

/// Packs bit fields, lowest first: `bits(&[(value, width), ...])`.
pub fn bits(fields: &[(u32, u32)]) -> u32 {
    let mut out = 0;
    let mut shift = 0;
    for &(value, width) in fields {
        out |= (value & ((1u64 << width) - 1) as u32) << shift;
        shift += width;
    }
    out
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAConfigAttrib {
    pub type_: c_int,
    pub value: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAImageFormat {
    pub fourcc: u32,
    pub byte_order: u32,
    pub bits_per_pixel: u32,
    pub depth: u32,
    pub red_mask: u32,
    pub green_mask: u32,
    pub blue_mask: u32,
    pub alpha_mask: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAImage {
    pub image_id: VAImageID,
    pub format: VAImageFormat,
    pub buf: VABufferID,
    pub width: u16,
    pub height: u16,
    pub data_size: u32,
    pub num_planes: u32,
    pub pitches: [u32; 3],
    pub offsets: [u32; 3],
    pub num_palette_entries: i32,
    pub entry_bytes: i32,
    pub component_order: [i8; 4],
    pub va_reserved: [u32; 4],
}

#[repr(C)]
pub struct VACodedBufferSegment {
    pub size: u32,
    pub bit_offset: u32,
    pub status: u32,
    pub reserved: u32,
    pub buf: *mut c_void,
    pub next: *mut c_void,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAPictureH264 {
    pub picture_id: VASurfaceID,
    pub frame_idx: u32,
    pub flags: u32,
    pub TopFieldOrderCnt: i32,
    pub BottomFieldOrderCnt: i32,
    pub va_reserved: [u32; 4],
}

impl VAPictureH264 {
    pub const INVALID: Self = Self {
        picture_id: VA_INVALID_SURFACE,
        frame_idx: 0,
        flags: VA_PICTURE_H264_INVALID,
        TopFieldOrderCnt: 0,
        BottomFieldOrderCnt: 0,
        va_reserved: [0; 4],
    };
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSequenceParameterBufferH264 {
    pub seq_parameter_set_id: u8,
    pub level_idc: u8,
    pub intra_period: u32,
    pub intra_idr_period: u32,
    pub ip_period: u32,
    pub bits_per_second: u32,
    pub max_num_ref_frames: u32,
    pub picture_width_in_mbs: u16,
    pub picture_height_in_mbs: u16,
    /// chroma_format_idc:2, frame_mbs_only_flag:1, mb_adaptive_frame_field_flag:1,
    /// seq_scaling_matrix_present_flag:1, direct_8x8_inference_flag:1,
    /// log2_max_frame_num_minus4:4, pic_order_cnt_type:2,
    /// log2_max_pic_order_cnt_lsb_minus4:4, delta_pic_order_always_zero_flag:1
    pub seq_fields: u32,
    pub bit_depth_luma_minus8: u8,
    pub bit_depth_chroma_minus8: u8,
    pub num_ref_frames_in_pic_order_cnt_cycle: u8,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: [i32; 256],
    pub frame_cropping_flag: u8,
    pub frame_crop_left_offset: u32,
    pub frame_crop_right_offset: u32,
    pub frame_crop_top_offset: u32,
    pub frame_crop_bottom_offset: u32,
    pub vui_parameters_present_flag: u8,
    pub vui_fields: u32,
    pub aspect_ratio_idc: u8,
    pub sar_width: u32,
    pub sar_height: u32,
    pub num_units_in_tick: u32,
    pub time_scale: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncPictureParameterBufferH264 {
    pub CurrPic: VAPictureH264,
    pub ReferenceFrames: [VAPictureH264; 16],
    pub coded_buf: VABufferID,
    pub pic_parameter_set_id: u8,
    pub seq_parameter_set_id: u8,
    pub last_picture: u8,
    pub frame_num: u16,
    pub pic_init_qp: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub chroma_qp_index_offset: i8,
    pub second_chroma_qp_index_offset: i8,
    /// idr_pic_flag:1, reference_pic_flag:2, entropy_coding_mode_flag:1,
    /// weighted_pred_flag:1, weighted_bipred_idc:2, constrained_intra_pred_flag:1,
    /// transform_8x8_mode_flag:1, deblocking_filter_control_present_flag:1,
    /// redundant_pic_cnt_present_flag:1, pic_order_present_flag:1,
    /// pic_scaling_matrix_present_flag:1
    pub pic_fields: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct VAEncSliceParameterBufferH264 {
    pub macroblock_address: u32,
    pub num_macroblocks: u32,
    pub macroblock_info: VABufferID,
    pub slice_type: u8,
    pub pic_parameter_set_id: u8,
    pub idr_pic_id: u16,
    pub pic_order_cnt_lsb: u16,
    pub delta_pic_order_cnt_bottom: i32,
    pub delta_pic_order_cnt: [i32; 2],
    pub direct_spatial_mv_pred_flag: u8,
    pub num_ref_idx_active_override_flag: u8,
    pub num_ref_idx_l0_active_minus1: u8,
    pub num_ref_idx_l1_active_minus1: u8,
    pub RefPicList0: [VAPictureH264; 32],
    pub RefPicList1: [VAPictureH264; 32],
    pub luma_log2_weight_denom: u8,
    pub chroma_log2_weight_denom: u8,
    pub luma_weight_l0_flag: u8,
    pub luma_weight_l0: [i16; 32],
    pub luma_offset_l0: [i16; 32],
    pub chroma_weight_l0_flag: u8,
    pub chroma_weight_l0: [[i16; 2]; 32],
    pub chroma_offset_l0: [[i16; 2]; 32],
    pub luma_weight_l1_flag: u8,
    pub luma_weight_l1: [i16; 32],
    pub luma_offset_l1: [i16; 32],
    pub chroma_weight_l1_flag: u8,
    pub chroma_weight_l1: [[i16; 2]; 32],
    pub chroma_offset_l1: [[i16; 2]; 32],
    pub cabac_init_idc: u8,
    pub slice_qp_delta: i8,
    pub disable_deblocking_filter_idc: u8,
    pub slice_alpha_c0_offset_div2: i8,
    pub slice_beta_offset_div2: i8,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterRateControl {
    pub bits_per_second: u32,
    pub target_percentage: u32,
    pub window_size: u32,
    pub initial_qp: u32,
    pub min_qp: u32,
    pub basic_unit_size: u32,
    /// reset:1, disable_frame_skip:1, disable_bit_stuffing:1, mb_rate_control:4,
    /// temporal_id:8, cfs_I_frames:1, enable_parallel_brc:1, enable_dynamic_scaling:1,
    /// frame_tolerance_mode:2, reserved:12
    pub rc_flags: u32,
    pub ICQ_quality_factor: u32,
    pub max_qp: u32,
    pub quality_factor: u32,
    pub target_frame_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterFrameRate {
    /// Numerator in the low 16 bits, denominator in the high 16 (`0` = 1).
    pub framerate: u32,
    pub framerate_flags: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncMiscParameterHRD {
    pub initial_buffer_fullness: u32,
    pub buffer_size: u32,
    pub va_reserved: [u32; 4],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct VAEncPackedHeaderParameterBuffer {
    pub type_: u32,
    pub bit_length: u32,
    pub has_emulation_bytes: u8,
    pub va_reserved: [u32; 4],
}

/// The libva functions, from `libva.so.2` and `libva-drm.so.2`.
pub struct Va {
    pub vaGetDisplayDRM: unsafe extern "C" fn(c_int) -> VADisplay,
    pub vaInitialize: unsafe extern "C" fn(VADisplay, *mut c_int, *mut c_int) -> VAStatus,
    pub vaTerminate: unsafe extern "C" fn(VADisplay) -> VAStatus,
    pub vaErrorStr: unsafe extern "C" fn(VAStatus) -> *const c_char,
    pub vaQueryVendorString: unsafe extern "C" fn(VADisplay) -> *const c_char,
    pub vaMaxNumEntrypoints: unsafe extern "C" fn(VADisplay) -> c_int,
    pub vaQueryConfigEntrypoints:
        unsafe extern "C" fn(VADisplay, c_int, *mut c_int, *mut c_int) -> VAStatus,
    pub vaGetConfigAttributes:
        unsafe extern "C" fn(VADisplay, c_int, c_int, *mut VAConfigAttrib, c_int) -> VAStatus,
    pub vaCreateConfig: unsafe extern "C" fn(
        VADisplay,
        c_int,
        c_int,
        *mut VAConfigAttrib,
        c_int,
        *mut VAConfigID,
    ) -> VAStatus,
    pub vaDestroyConfig: unsafe extern "C" fn(VADisplay, VAConfigID) -> VAStatus,
    pub vaCreateSurfaces: unsafe extern "C" fn(
        VADisplay,
        c_uint,
        c_uint,
        c_uint,
        *mut VASurfaceID,
        c_uint,
        *mut c_void,
        c_uint,
    ) -> VAStatus,
    pub vaDestroySurfaces: unsafe extern "C" fn(VADisplay, *mut VASurfaceID, c_int) -> VAStatus,
    pub vaCreateContext: unsafe extern "C" fn(
        VADisplay,
        VAConfigID,
        c_int,
        c_int,
        c_int,
        *mut VASurfaceID,
        c_int,
        *mut VAContextID,
    ) -> VAStatus,
    pub vaDestroyContext: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub vaCreateBuffer: unsafe extern "C" fn(
        VADisplay,
        VAContextID,
        c_int,
        c_uint,
        c_uint,
        *mut c_void,
        *mut VABufferID,
    ) -> VAStatus,
    pub vaDestroyBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    pub vaMapBuffer: unsafe extern "C" fn(VADisplay, VABufferID, *mut *mut c_void) -> VAStatus,
    pub vaUnmapBuffer: unsafe extern "C" fn(VADisplay, VABufferID) -> VAStatus,
    pub vaDeriveImage: unsafe extern "C" fn(VADisplay, VASurfaceID, *mut VAImage) -> VAStatus,
    pub vaCreateImage:
        unsafe extern "C" fn(VADisplay, *mut VAImageFormat, c_int, c_int, *mut VAImage) -> VAStatus,
    pub vaDestroyImage: unsafe extern "C" fn(VADisplay, VAImageID) -> VAStatus,
    pub vaPutImage: unsafe extern "C" fn(
        VADisplay,
        VASurfaceID,
        VAImageID,
        c_int,
        c_int,
        c_uint,
        c_uint,
        c_int,
        c_int,
        c_uint,
        c_uint,
    ) -> VAStatus,
    pub vaBeginPicture: unsafe extern "C" fn(VADisplay, VAContextID, VASurfaceID) -> VAStatus,
    pub vaRenderPicture:
        unsafe extern "C" fn(VADisplay, VAContextID, *mut VABufferID, c_int) -> VAStatus,
    pub vaEndPicture: unsafe extern "C" fn(VADisplay, VAContextID) -> VAStatus,
    pub vaSyncSurface: unsafe extern "C" fn(VADisplay, VASurfaceID) -> VAStatus,
}

fn open_lib(name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: a library name; the handle is never closed (the functions stay loaded).
    let handle = unsafe { libc::dlopen(name.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
    if handle.is_null() {
        return Err(format!("{} not found", name.to_string_lossy()));
    }
    Ok(handle)
}

fn symbol(handle: *mut c_void, name: &CStr) -> Result<*mut c_void, String> {
    // SAFETY: `handle` is an open library.
    let sym = unsafe { libc::dlsym(handle, name.as_ptr()) };
    if sym.is_null() {
        return Err(format!("libva has no {}", name.to_string_lossy()));
    }
    Ok(sym)
}

/// A symbol as the function pointer type `F`.
///
/// # Safety
/// `F` must be an `extern "C" fn` type matching the symbol's C signature.
unsafe fn function<F: Copy>(sym: *mut c_void) -> F {
    assert_eq!(std::mem::size_of::<F>(), std::mem::size_of::<*mut c_void>());
    // SAFETY: a non-null function address, as a function pointer of the caller's type.
    unsafe { std::mem::transmute_copy(&sym) }
}

impl Va {
    /// libva, loaded once per process.
    pub fn get() -> Result<&'static Va, String> {
        static VA: OnceLock<Result<Va, String>> = OnceLock::new();
        VA.get_or_init(Self::load).as_ref().map_err(Clone::clone)
    }

    fn load() -> Result<Va, String> {
        let va = open_lib(c"libva.so.2")?;
        let drm = open_lib(c"libva-drm.so.2")?;
        macro_rules! sym {
            ($lib:expr, $name:ident) => {
                // SAFETY: the symbol is the libva 2.x function of this name, whose C signature
                // the field's type matches.
                unsafe {
                    function(symbol(
                        $lib,
                        &CStr::from_bytes_with_nul(concat!(stringify!($name), "\0").as_bytes())
                            .expect("symbol name"),
                    )?)
                }
            };
        }
        Ok(Va {
            vaGetDisplayDRM: sym!(drm, vaGetDisplayDRM),
            vaInitialize: sym!(va, vaInitialize),
            vaTerminate: sym!(va, vaTerminate),
            vaErrorStr: sym!(va, vaErrorStr),
            vaQueryVendorString: sym!(va, vaQueryVendorString),
            vaMaxNumEntrypoints: sym!(va, vaMaxNumEntrypoints),
            vaQueryConfigEntrypoints: sym!(va, vaQueryConfigEntrypoints),
            vaGetConfigAttributes: sym!(va, vaGetConfigAttributes),
            vaCreateConfig: sym!(va, vaCreateConfig),
            vaDestroyConfig: sym!(va, vaDestroyConfig),
            vaCreateSurfaces: sym!(va, vaCreateSurfaces),
            vaDestroySurfaces: sym!(va, vaDestroySurfaces),
            vaCreateContext: sym!(va, vaCreateContext),
            vaDestroyContext: sym!(va, vaDestroyContext),
            vaCreateBuffer: sym!(va, vaCreateBuffer),
            vaDestroyBuffer: sym!(va, vaDestroyBuffer),
            vaMapBuffer: sym!(va, vaMapBuffer),
            vaUnmapBuffer: sym!(va, vaUnmapBuffer),
            vaDeriveImage: sym!(va, vaDeriveImage),
            vaCreateImage: sym!(va, vaCreateImage),
            vaDestroyImage: sym!(va, vaDestroyImage),
            vaPutImage: sym!(va, vaPutImage),
            vaBeginPicture: sym!(va, vaBeginPicture),
            vaRenderPicture: sym!(va, vaRenderPicture),
            vaEndPicture: sym!(va, vaEndPicture),
            vaSyncSurface: sym!(va, vaSyncSurface),
        })
    }

    /// `Ok` for `VA_STATUS_SUCCESS`, otherwise libva's message for the status.
    pub fn check(&self, status: VAStatus, what: &str) -> anyhow::Result<()> {
        if status == VA_STATUS_SUCCESS {
            return Ok(());
        }
        // SAFETY: vaErrorStr returns a static string for any status.
        let msg = unsafe { CStr::from_ptr((self.vaErrorStr)(status)) };
        anyhow::bail!("{what}: {} ({status:#x})", msg.to_string_lossy())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{offset_of, size_of};

    /// Sizes and offsets as GCC lays out libva 2.20's structs on x86-64 and aarch64.
    #[test]
    fn layouts_match_libva() {
        assert_eq!(size_of::<VAPictureH264>(), 36);
        assert_eq!(size_of::<VAImageFormat>(), 48);
        assert_eq!(size_of::<VAImage>(), 120);
        assert_eq!(offset_of!(VAImage, pitches), 68);
        assert_eq!(offset_of!(VAImage, offsets), 80);
        assert_eq!(size_of::<VACodedBufferSegment>(), 48);
        assert_eq!(offset_of!(VACodedBufferSegment, next), 24);
        assert_eq!(size_of::<VAEncSequenceParameterBufferH264>(), 1132);
        assert_eq!(offset_of!(VAEncSequenceParameterBufferH264, seq_fields), 28);
        assert_eq!(
            offset_of!(VAEncSequenceParameterBufferH264, frame_cropping_flag),
            1068
        );
        assert_eq!(
            offset_of!(VAEncSequenceParameterBufferH264, time_scale),
            1112
        );
        assert_eq!(size_of::<VAEncPictureParameterBufferH264>(), 648);
        assert_eq!(offset_of!(VAEncPictureParameterBufferH264, coded_buf), 612);
        assert_eq!(offset_of!(VAEncPictureParameterBufferH264, frame_num), 620);
        assert_eq!(offset_of!(VAEncPictureParameterBufferH264, pic_fields), 628);
        assert_eq!(size_of::<VAEncSliceParameterBufferH264>(), 3140);
        assert_eq!(offset_of!(VAEncSliceParameterBufferH264, RefPicList0), 36);
        assert_eq!(
            offset_of!(VAEncSliceParameterBufferH264, cabac_init_idc),
            3118
        );
        assert_eq!(size_of::<VAEncMiscParameterRateControl>(), 60);
        assert_eq!(size_of::<VAEncMiscParameterFrameRate>(), 24);
        assert_eq!(size_of::<VAEncMiscParameterHRD>(), 24);
        assert_eq!(size_of::<VAEncPackedHeaderParameterBuffer>(), 28);
    }

    #[test]
    fn bit_fields_fill_from_the_lowest_bit() {
        // chroma_format_idc=1, frame_mbs_only_flag=1, direct_8x8_inference_flag=1.
        assert_eq!(bits(&[(1, 2), (1, 1), (0, 1), (0, 1), (1, 1)]), 0b10_0101);
        assert_eq!(bits(&[(0, 1), (3, 2)]), 0b110);
    }
}
