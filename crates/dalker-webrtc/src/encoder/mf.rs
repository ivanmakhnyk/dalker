//! The Media Foundation hardware encoder (Windows): the H.264 encoder MFT that the graphics
//! driver installs, which is NVIDIA NVENC, Intel Quick Sync or AMD AMF.
//!
//! Hardware MFTs are asynchronous: they say when they want input and when they have output
//! through media events. A thread waits for those events and hands them to the encoder
//! thread, which feeds one picture and then waits (briefly) for its coded frame, so a frame
//! goes out as soon as the GPU has encoded it.

use std::mem::ManuallyDrop;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use windows::core::{Interface, GUID, PWSTR};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_MULTITHREADED};
use windows::Win32::System::Variant::VARIANT;

use super::{has_sps, write_nv12, Backend, Config, I420};

/// Longest wait for the coded frame after a picture went in. Hardware encoders take a few
/// milliseconds; one that holds frames back longer is not waited for again (see `lagging`).
const OUTPUT_WAIT: Duration = Duration::from_millis(100);

/// Longest wait for the encoder to ask for the next picture.
const INPUT_WAIT: Duration = Duration::from_secs(2);

/// Waits for output this many times in a row without getting any, then stops waiting.
const MAX_LATE: u32 = 3;

/// Most bits one frame may get, per pixel. Raw NV12 is 12 and H.264 rarely needs even 1, so
/// this costs no quality and keeps a frame's budget well below the raw picture.
const MAX_BITS_PER_PIXEL: u64 = 4;

/// The bitrate the encoder is given: `bitrate` held to [`MAX_BITS_PER_PIXEL`] per frame.
///
/// AMD's encoder pads every frame to its constant bitrate and crashes the process (an
/// access violation on its own thread) when that budget is bigger than the raw picture:
/// 320x180 at 5 fps and 6 Mbps is 150 KB a frame for an 86 KB picture.
fn usable_bitrate(bitrate: u32, width: u64, height: u64, fps: u64) -> u32 {
    let cap = MAX_BITS_PER_PIXEL * width * height * fps.max(1);
    bitrate.min(cap.min(u32::MAX as u64) as u32)
}

/// Media Foundation, started once per process (and never shut down).
fn startup() -> Result<()> {
    static STARTED: std::sync::OnceLock<Result<(), String>> = std::sync::OnceLock::new();
    com_init();
    STARTED
        .get_or_init(|| {
            // SAFETY: plain library initialisation.
            unsafe { MFStartup(MF_VERSION, MFSTARTUP_FULL) }.map_err(|e| e.to_string())
        })
        .clone()
        .map_err(|e| anyhow::anyhow!("MFStartup: {e}"))
}

/// COM for this thread (multithreaded; a thread that already has COM keeps its own).
fn com_init() {
    // SAFETY: plain COM initialisation; S_FALSE and RPC_E_CHANGED_MODE are fine.
    let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
}

/// The vendor's name for its encoder, from the MFT's PCI vendor ID ("VEN_10DE").
fn vendor_name(vendor_id: &str, friendly: &str) -> String {
    let id = vendor_id.to_ascii_uppercase();
    if id.contains("10DE") {
        "NVIDIA NVENC".into()
    } else if id.contains("8086") {
        "Intel Quick Sync".into()
    } else if id.contains("1002") || id.contains("1022") {
        "AMD AMF".into()
    } else if !friendly.is_empty() {
        friendly.into()
    } else {
        "Media Foundation hardware encoder".into()
    }
}

/// Which hardware encoder to try first: NVIDIA, then the rest in Media Foundation's order.
fn vendor_rank(vendor_id: &str) -> u8 {
    if vendor_id.to_ascii_uppercase().contains("10DE") {
        0
    } else {
        1
    }
}

fn string_attr(attrs: &IMFAttributes, key: &GUID) -> String {
    let mut value = PWSTR::null();
    let mut len = 0;
    // SAFETY: on success the string is allocated for us and freed below.
    unsafe {
        if attrs.GetAllocatedString(key, &mut value, &mut len).is_err() || value.is_null() {
            return String::new();
        }
        let s = value.to_string().unwrap_or_default();
        CoTaskMemFree(Some(value.0 as _));
        s
    }
}

/// The hardware H.264 encoders of every GPU, best first.
///
/// `MFTEnumEx` lists only the encoder of the GPU that drives the display: on a PC with an
/// integrated Radeon 780M and an RTX 4070 SUPER it found just AMD's. So each GPU is asked
/// for its own (`MFTEnum2` with the adapter's LUID), NVIDIA's first; `MFTEnumEx` is the
/// fallback where that finds nothing.
fn hardware_encoders() -> Result<Vec<Candidate>> {
    let mut found = Vec::new();
    for (vendor_id, luid, name) in adapters() {
        // SAFETY: a fresh attribute store holding the adapter's LUID.
        let listed = unsafe {
            let mut attrs = None;
            MFCreateAttributes(&mut attrs, 1)
                .ok()
                .and(attrs)
                .filter(|a| a.SetUINT64(&MFT_ENUM_ADAPTER_LUID, luid).is_ok())
                .map(|a| enum_encoders(Some(&a)))
        };
        match listed {
            Some(Ok(list)) => {
                tracing::debug!(
                    adapter = name,
                    vendor_id,
                    count = list.len(),
                    "hardware H.264 encoders"
                );
                found.extend(
                    list.into_iter()
                        .map(|a| Candidate::new(a, Some(name.clone()))),
                );
            }
            Some(Err(e)) => tracing::debug!(adapter = name, "MFTEnum2: {e:#}"),
            None => {}
        }
    }
    if found.is_empty() {
        found = enum_encoders(None)?
            .into_iter()
            .map(|a| Candidate::new(a, None))
            .collect();
    }
    found.sort_by_key(|c| vendor_rank(&c.vendor));
    Ok(found)
}

/// An encoder MFT to try.
pub(crate) struct Candidate {
    activate: IMFActivate,
    /// PCI vendor ID as Media Foundation gives it ("VEN_10DE").
    vendor: String,
    friendly: String,
    adapter: Option<String>,
}

impl Candidate {
    fn new(activate: IMFActivate, adapter: Option<String>) -> Self {
        Self {
            vendor: string_attr(&activate, &MFT_ENUM_HARDWARE_VENDOR_ID_Attribute),
            friendly: string_attr(&activate, &MFT_FRIENDLY_NAME_Attribute),
            activate,
            adapter,
        }
    }
}

/// The hardware GPUs: PCI vendor ID, LUID (high part above the low part) and name.
fn adapters() -> Vec<(u32, u64, String)> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE,
    };
    let mut out = Vec::new();
    // SAFETY: plain DXGI queries.
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return out;
        };
        for i in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(i) else {
                break;
            };
            let Ok(desc) = adapter.GetDesc1() else {
                continue;
            };
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                continue;
            }
            let luid =
                ((desc.AdapterLuid.HighPart as u32 as u64) << 32) | desc.AdapterLuid.LowPart as u64;
            let len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            out.push((
                desc.VendorId,
                luid,
                String::from_utf16_lossy(&desc.Description[..len]),
            ));
        }
    }
    out
}

/// The hardware NV12 → H.264 encoder MFTs: of the adapter `attrs` names (`MFTEnum2`), or of
/// the display's (`MFTEnumEx`).
fn enum_encoders(attrs: Option<&IMFAttributes>) -> Result<Vec<IMFActivate>> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let flags = MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0);
    let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut n = 0u32;
    // SAFETY: on success `list` holds `n` activation objects, owned by us; the array itself
    // is freed below.
    unsafe {
        match attrs {
            Some(a) => MFTEnum2(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                a,
                &mut list,
                &mut n,
            )
            .context("MFTEnum2")?,
            None => MFTEnumEx(
                MFT_CATEGORY_VIDEO_ENCODER,
                flags,
                Some(&input),
                Some(&output),
                &mut list,
                &mut n,
            )
            .context("MFTEnumEx")?,
        }
        if list.is_null() {
            return Ok(vec![]);
        }
        let found = std::slice::from_raw_parts_mut(list, n as usize)
            .iter_mut()
            .filter_map(Option::take)
            .collect();
        CoTaskMemFree(Some(list as _));
        Ok(found)
    }
}

/// What the event thread passes on: the event type and its status.
type Event = (MF_EVENT_TYPE, windows::core::HRESULT);

/// The event generator, moved to the event thread. Media Foundation objects are
/// free-threaded.
struct SendGenerator(IMFMediaEventGenerator);
// SAFETY: see above; the MFT is created in the multithreaded apartment.
unsafe impl Send for SendGenerator {}

pub(crate) struct MfEncoder {
    activate: IMFActivate,
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    /// Events of an asynchronous MFT; `None` for a synchronous one.
    events: Option<mpsc::Receiver<Event>>,
    event_thread: Option<std::thread::JoinHandle<()>>,
    /// Pictures the MFT asked for that it hasn't been given yet.
    need_input: u32,
    /// Coded frames not handed out yet.
    pending: Vec<(Vec<u8>, bool)>,
    /// SPS and PPS from the output type, put in front of a keyframe that comes without them.
    sequence_header: Vec<u8>,
    width: usize,
    height: usize,
    frame_duration: i64,
    start: Instant,
    last_time: i64,
    late: u32,
    warned_keyframe: bool,
}

impl MfEncoder {
    pub fn open(config: &Config) -> Result<(Self, String)> {
        startup()?;
        let encoders = hardware_encoders()?;
        if encoders.is_empty() {
            bail!("no hardware H.264 encoder for Media Foundation");
        }
        let mut errors = Vec::new();
        for c in encoders {
            let adapter = c.adapter.as_deref().unwrap_or("display GPU");
            match Self::open_one(c.activate.clone(), config) {
                Ok(enc) => {
                    tracing::info!(
                        vendor = c.vendor,
                        mft = c.friendly,
                        adapter,
                        "Media Foundation H.264 encoder"
                    );
                    return Ok((enc, vendor_name(&c.vendor, &c.friendly)));
                }
                Err(e) => {
                    // SAFETY: releases the MFT that failed to start.
                    let _ = unsafe { c.activate.ShutdownObject() };
                    tracing::info!(
                        mft = c.friendly,
                        adapter,
                        "hardware encoder didn't start: {e:#}"
                    );
                    errors.push(format!("{} ({adapter}): {e:#}", c.friendly));
                }
            }
        }
        bail!("{}", errors.join("; "))
    }

    fn open_one(activate: IMFActivate, config: &Config) -> Result<Self> {
        // SAFETY: COM calls on objects we own, on this (COM-initialised) thread.
        unsafe {
            let transform: IMFTransform = activate.ActivateObject().context("activate the MFT")?;
            let attrs = transform.GetAttributes().ok();
            let is_async = attrs
                .as_ref()
                .is_some_and(|a| a.GetUINT32(&MF_TRANSFORM_ASYNC).unwrap_or(0) != 0);
            if is_async {
                attrs
                    .as_ref()
                    .expect("async MFTs have attributes")
                    .SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)
                    .context("unlock the asynchronous MFT")?;
            }
            if let Some(a) = &attrs {
                let _ = a.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            let codec: Option<ICodecAPI> = transform.cast().ok();
            let mut enc = Self {
                activate,
                transform,
                codec,
                events: None,
                event_thread: None,
                need_input: 0,
                pending: Vec::new(),
                sequence_header: Vec::new(),
                width: config.width,
                height: config.height,
                frame_duration: (10_000_000.0 / config.fps.max(1.0) as f64) as i64,
                start: Instant::now(),
                last_time: -1,
                late: 0,
                warned_keyframe: false,
            };
            enc.configure(config)?;
            if is_async {
                let generator: IMFMediaEventGenerator =
                    enc.transform.cast().context("no event generator")?;
                let (tx, rx) = mpsc::channel();
                let generator = SendGenerator(generator);
                enc.event_thread = Some(
                    std::thread::Builder::new()
                        .name("dalker-mf-events".into())
                        .spawn(move || event_loop(generator, tx))
                        .context("spawn the encoder's event thread")?,
                );
                enc.events = Some(rx);
            }
            enc.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("begin streaming")?;
            enc.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start of stream")?;
            Ok(enc)
        }
    }

    fn set_codec(&self, api: &GUID, value: VARIANT) -> bool {
        let Some(codec) = &self.codec else {
            return false;
        };
        // SAFETY: a plain property set.
        unsafe { codec.SetValue(api, &value).is_ok() }
    }

    /// Rate control, then the output (H.264) and input (NV12) types.
    unsafe fn configure(&mut self, config: &Config) -> Result<()> {
        let (w, h) = (config.width as u64, config.height as u64);
        let fps = config.fps.round().max(1.0) as u64;
        let mean_bitrate = usable_bitrate(config.bitrate_bps, w, h, fps);
        let gop = config.keyframe_interval.max(1);
        let low_latency = self.set_codec(&CODECAPI_AVLowLatencyMode, VARIANT::from(true));
        let cbr = self.set_codec(
            &CODECAPI_AVEncCommonRateControlMode,
            VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32),
        );
        self.set_codec(
            &CODECAPI_AVEncCommonMeanBitRate,
            VARIANT::from(mean_bitrate),
        );
        self.set_codec(&CODECAPI_AVEncMPVGOPSize, VARIANT::from(gop));
        self.set_codec(&CODECAPI_AVEncMPVDefaultBPictureCount, VARIANT::from(0u32));
        tracing::debug!(
            low_latency,
            cbr,
            bitrate = mean_bitrate,
            "Media Foundation encoder settings"
        );

        let video_type = |subtype: &GUID| -> Result<IMFMediaType> {
            let t = MFCreateMediaType()?;
            t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            t.SetGUID(&MF_MT_SUBTYPE, subtype)?;
            t.SetUINT64(&MF_MT_FRAME_SIZE, (w << 32) | h)?;
            t.SetUINT64(&MF_MT_FRAME_RATE, (fps << 32) | 1)?;
            t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1 << 32) | 1)?;
            t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            Ok(t)
        };
        let output = video_type(&MFVideoFormat_H264)?;
        output.SetUINT32(&MF_MT_AVG_BITRATE, mean_bitrate)?;
        // Constrained Baseline where the encoder has it, plain Baseline otherwise (the same
        // stream in practice: hardware encoders use none of the tools Constrained leaves out).
        output.SetUINT32(
            &MF_MT_MPEG2_PROFILE,
            eAVEncH264VProfile_ConstrainedBase.0 as u32,
        )?;
        if self.transform.SetOutputType(0, &output, 0).is_err() {
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base.0 as u32)?;
            self.transform
                .SetOutputType(0, &output, 0)
                .context("set the H.264 output type")?;
        }

        let input = video_type(&MFVideoFormat_NV12)?;
        input.SetUINT32(&MF_MT_DEFAULT_STRIDE, w as u32)?;
        if let Err(e) = self.transform.SetInputType(0, &input, 0) {
            // Start from one of the encoder's own NV12 types instead.
            let mut set = false;
            for i in 0.. {
                let Ok(t) = self.transform.GetInputAvailableType(0, i) else {
                    break;
                };
                if t.GetGUID(&MF_MT_SUBTYPE).ok() != Some(MFVideoFormat_NV12) {
                    continue;
                }
                t.SetUINT64(&MF_MT_FRAME_SIZE, (w << 32) | h)?;
                t.SetUINT64(&MF_MT_FRAME_RATE, (fps << 32) | 1)?;
                if self.transform.SetInputType(0, &t, 0).is_ok() {
                    set = true;
                    break;
                }
            }
            if !set {
                return Err(anyhow::Error::from(e).context("set the NV12 input type"));
            }
        }
        self.read_sequence_header();
        Ok(())
    }

    fn read_sequence_header(&mut self) {
        // SAFETY: plain attribute reads; the blob is allocated for us and freed below.
        unsafe {
            let Ok(t) = self.transform.GetOutputCurrentType(0) else {
                return;
            };
            let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
            if t.GetAllocatedBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut data, &mut len)
                .is_ok()
                && !data.is_null()
            {
                self.sequence_header = std::slice::from_raw_parts(data, len as usize).to_vec();
                CoTaskMemFree(Some(data as _));
            }
        }
    }

    /// An NV12 sample holding the picture, stamped with the time since the encoder started.
    unsafe fn sample(&mut self, yuv: &I420) -> Result<IMFSample> {
        let (w, h) = (self.width, self.height);
        let len = w * h * 3 / 2;
        let buffer = MFCreateMemoryBuffer(len as u32)?;
        let mut data = std::ptr::null_mut();
        buffer.Lock(&mut data, None, None)?;
        {
            let bytes = std::slice::from_raw_parts_mut(data, len);
            let (y, uv) = bytes.split_at_mut(w * h);
            write_nv12(yuv, y, w, h, uv, w, h / 2);
        }
        buffer.Unlock()?;
        buffer.SetCurrentLength(len as u32)?;
        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        let now = (self.start.elapsed().as_nanos() / 100) as i64;
        let time = now.max(self.last_time + 1);
        self.last_time = time;
        sample.SetSampleTime(time)?;
        sample.SetSampleDuration(self.frame_duration)?;
        Ok(sample)
    }

    /// Takes one coded frame from the MFT, if it has one.
    unsafe fn take_output(&mut self) -> Result<bool> {
        let info = self.transform.GetOutputStreamInfo(0)?;
        let provides = info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                as u32
            != 0;
        let own = if provides {
            None
        } else {
            let s = MFCreateSample()?;
            let size = info.cbSize.max((self.width * self.height * 3 / 2) as u32);
            s.AddBuffer(&MFCreateMemoryBuffer(size)?)?;
            Some(s)
        };
        let mut out = [MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(own),
            dwStatus: 0,
            pEvents: ManuallyDrop::new(None),
        }];
        let mut status = 0;
        let result = self.transform.ProcessOutput(0, &mut out, &mut status);
        let sample = ManuallyDrop::take(&mut out[0].pSample);
        drop(ManuallyDrop::take(&mut out[0].pEvents));
        match result {
            Ok(()) => {}
            Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
            Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                let t = self.transform.GetOutputAvailableType(0, 0)?;
                self.transform.SetOutputType(0, &t, 0)?;
                self.read_sequence_header();
                return Ok(false);
            }
            Err(e) => return Err(anyhow::Error::from(e).context("ProcessOutput")),
        }
        let Some(sample) = sample else {
            return Ok(false);
        };
        let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
        let buffer = sample.ConvertToContiguousBuffer()?;
        let (mut data, mut len) = (std::ptr::null_mut(), 0u32);
        buffer.Lock(&mut data, None, Some(&mut len))?;
        let mut bytes = std::slice::from_raw_parts(data, len as usize).to_vec();
        buffer.Unlock()?;
        if bytes.is_empty() {
            return Ok(false);
        }
        if key && !has_sps(&bytes) && !self.sequence_header.is_empty() {
            let mut with_header = self.sequence_header.clone();
            with_header.append(&mut bytes);
            bytes = with_header;
        }
        self.pending.push((bytes, key));
        Ok(true)
    }

    /// Handles the MFT's events until `done` holds or `deadline` passes. Returns whether
    /// `done` held.
    unsafe fn pump(&mut self, deadline: Instant, done: impl Fn(&Self) -> bool) -> Result<bool> {
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            let events = self.events.as_ref().expect("asynchronous MFT");
            let (kind, status) = match events.recv_timeout(left) {
                Ok(e) => e,
                Err(mpsc::RecvTimeoutError::Timeout) => return Ok(false),
                Err(mpsc::RecvTimeoutError::Disconnected) => bail!("the encoder's events stopped"),
            };
            status.ok().context("encoder error event")?;
            if kind == METransformNeedInput {
                self.need_input += 1;
            } else if kind == METransformHaveOutput {
                self.take_output()?;
            } else if kind == MEError {
                bail!("encoder error event");
            }
        }
        Ok(true)
    }

    fn force_keyframe(&mut self) {
        if !self.set_codec(&CODECAPI_AVEncVideoForceKeyFrame, VARIANT::from(1u32))
            && !std::mem::replace(&mut self.warned_keyframe, true)
        {
            tracing::warn!("the hardware encoder can't be asked for a keyframe");
        }
    }
}

fn event_loop(generator: SendGenerator, tx: mpsc::Sender<Event>) {
    com_init();
    loop {
        // SAFETY: blocks until the MFT has an event; fails once it is shut down.
        let event = unsafe {
            generator
                .0
                .GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(0))
        };
        let Ok(event) = event else { return };
        // SAFETY: plain reads of the event.
        let (kind, status) = unsafe {
            (
                MF_EVENT_TYPE(event.GetType().unwrap_or(0) as i32),
                event.GetStatus().unwrap_or_default(),
            )
        };
        if tx.send((kind, status)).is_err() {
            return;
        }
    }
}

impl Backend for MfEncoder {
    fn encode(&mut self, yuv: &I420, idr: bool) -> Result<Option<(Vec<u8>, bool)>> {
        // The first picture is a keyframe anyway.
        if idr && self.last_time >= 0 {
            self.force_keyframe();
        }
        // SAFETY: COM calls on the MFT this encoder owns, from its encoder thread.
        unsafe {
            let sample = self.sample(yuv)?;
            if self.events.is_some() {
                if !self.pump(Instant::now() + INPUT_WAIT, |e| e.need_input > 0)? {
                    bail!("the hardware encoder stopped asking for input");
                }
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("ProcessInput")?;
                self.need_input -= 1;
                if self.late < MAX_LATE {
                    let got = self.pump(Instant::now() + OUTPUT_WAIT, |e| !e.pending.is_empty())?;
                    self.late = if got { 0 } else { self.late + 1 };
                    if self.late == MAX_LATE {
                        tracing::info!(
                            "the hardware encoder holds frames back; not waiting for them"
                        );
                    }
                } else {
                    self.pump(Instant::now(), |_| false)?;
                }
            } else {
                self.transform
                    .ProcessInput(0, &sample, 0)
                    .context("ProcessInput")?;
                while self.take_output()? {}
            }
        }
        if self.pending.is_empty() {
            return Ok(None);
        }
        let mut frames = std::mem::take(&mut self.pending).into_iter();
        let (mut bytes, mut key) = frames.next().expect("not empty");
        for (more, k) in frames {
            bytes.extend_from_slice(&more);
            key |= k;
        }
        Ok(Some((bytes, key)))
    }
}

impl Drop for MfEncoder {
    fn drop(&mut self) {
        // SAFETY: COM calls on objects this encoder owns. Shutting the MFT down ends the
        // event thread's GetEvent.
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            let shut = self
                .transform
                .cast::<IMFShutdown>()
                .is_ok_and(|s| s.Shutdown().is_ok());
            let _ = self.activate.ShutdownObject();
            if let Some(thread) = self.event_thread.take() {
                // An MFT that can't be shut down may never send another event: leave its
                // thread rather than hang here.
                if shut {
                    let _ = thread.join();
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vendor_names() {
        assert_eq!(
            vendor_name("VEN_10DE", "NVIDIA H.264 Encoder MFT"),
            "NVIDIA NVENC"
        );
        assert_eq!(vendor_name("VEN_8086", ""), "Intel Quick Sync");
        assert_eq!(vendor_name("VEN_1002", "AMDh264Encoder"), "AMD AMF");
        assert_eq!(vendor_name("", "Some Encoder"), "Some Encoder");
        let mut ids = ["VEN_1002", "VEN_10DE", "VEN_8086"];
        ids.sort_by_key(|v| vendor_rank(v));
        assert_eq!(ids, ["VEN_10DE", "VEN_1002", "VEN_8086"]);
    }

    #[test]
    fn bitrate_is_held_to_what_the_picture_can_use() {
        // A small, slow picture: 4 bits a pixel, 1.152 Mbps instead of 6.
        assert_eq!(usable_bitrate(6_000_000, 320, 180, 5), 1_152_000);
        // Usual sizes and rates are left alone.
        assert_eq!(usable_bitrate(6_000_000, 1920, 1080, 30), 6_000_000);
        assert_eq!(usable_bitrate(6_000_000, 1280, 720, 5), 6_000_000);
    }

    /// A small picture at a low frame rate and a high bitrate, which crashed AMD's encoder.
    /// Skipped without a hardware encoder.
    #[test]
    fn small_slow_high_bitrate_stream_encodes_on_the_gpu() {
        let config = Config {
            width: 320,
            height: 180,
            bitrate_bps: 6_000_000,
            fps: 5.0,
            keyframe_interval: 300,
            strict_bitrate: false,
        };
        let mut enc = match MfEncoder::open(&config) {
            Ok((enc, _)) => enc,
            Err(e) => {
                eprintln!("skipped, no hardware encoder: {e:#}");
                return;
            }
        };
        let yuv = I420 {
            width: 320,
            height: 180,
            y: vec![16; 320 * 180],
            u: vec![128; 160 * 90],
            v: vec![128; 160 * 90],
        };
        let mut frames = 0;
        for i in 0..10 {
            frames += usize::from(enc.encode(&yuv, i == 0).unwrap().is_some());
        }
        assert!(frames >= 8, "only {frames} frames came out");
    }

    /// Encodes on this machine's GPU when it has a hardware encoder (`DALKER_TEST_MF=1` makes
    /// a missing one a failure) and checks the stream's shape.
    #[test]
    fn encodes_on_the_gpu_when_there_is_one() {
        let config = Config {
            width: 1280,
            height: 720,
            bitrate_bps: 4_000_000,
            fps: 30.0,
            keyframe_interval: 300,
            strict_bitrate: false,
        };
        startup().unwrap();
        for (vendor, _, name) in adapters() {
            eprintln!("GPU: {name} (vendor {vendor:#06x})");
        }
        for c in hardware_encoders().unwrap() {
            let result = MfEncoder::open_one(c.activate.clone(), &config).and_then(|mut e| {
                let yuv = I420 {
                    width: 1280,
                    height: 720,
                    y: vec![16; 1280 * 720],
                    u: vec![128; 640 * 360],
                    v: vec![128; 640 * 360],
                };
                e.encode(&yuv, true).map(|f| f.map(|f| f.0.len()))
            });
            // SAFETY: releases the MFT tried above.
            let _ = unsafe { c.activate.ShutdownObject() };
            eprintln!(
                "candidate: {} {} on {:?}: {:?}",
                c.vendor,
                c.friendly,
                c.adapter,
                result.map_err(|e| format!("{e:#}"))
            );
        }
        let mut enc = match MfEncoder::open(&config) {
            Ok((enc, name)) => {
                eprintln!("Media Foundation encoder: {name}");
                enc
            }
            Err(e) if std::env::var_os("DALKER_TEST_MF").is_none() => {
                eprintln!("skipped, no hardware encoder: {e:#}");
                return;
            }
            Err(e) => panic!("{e:#}"),
        };
        let mut yuv = I420 {
            width: 1280,
            height: 720,
            y: vec![16; 1280 * 720],
            u: vec![128; 640 * 360],
            v: vec![128; 640 * 360],
        };
        let mut frames = Vec::new();
        let started = Instant::now();
        for i in 0..30 {
            yuv.y[i * 4000..i * 4000 + 2000].fill(235);
            if let Some(f) = enc.encode(&yuv, i == 20).unwrap() {
                frames.push(f);
            }
        }
        eprintln!(
            "{} frames out of 30 in {:?}, keyframes at {:?}",
            frames.len(),
            started.elapsed(),
            frames
                .iter()
                .enumerate()
                .filter(|f| f.1 .1)
                .map(|f| f.0)
                .collect::<Vec<_>>()
        );
        let (first, key) = &frames[0];
        assert!(key, "the first frame is a keyframe");
        let types: Vec<_> = super::super::nal_types(first).collect();
        assert!(
            types.contains(&7) && types.contains(&8) && types.contains(&5),
            "{types:?}"
        );
        assert!(frames.len() >= 25, "only {} frames came out", frames.len());
        assert!(
            frames[1..].iter().any(|f| f.1),
            "the requested keyframe came"
        );
    }
}
