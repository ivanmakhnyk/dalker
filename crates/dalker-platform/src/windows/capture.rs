use std::time::{Duration, Instant};

use bytes::Bytes;
use dalker_core::{CaptureError, DisplayInfo, PixelFormat, RawFrame, ScreenCapturer};
use windows::core::Interface;
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use super::desktop;
use super::pointer::{self, PointerShape, ShapeKind};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
    IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_INVALID_CALL, DXGI_ERROR_NOT_FOUND,
    DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
};

/// Captures the primary monitor with the DXGI Desktop Duplication API.
///
/// Duplicates the desktop the creating thread is attached to, which is the input
/// desktop at creation time (the sign-in/UAC desktop `Winlogon` too, when running
/// as SYSTEM). When the input desktop changes, `next_frame` reports
/// [`CaptureError::Lost`] so the owner recreates the capturer on the new desktop.
pub struct DxgiCapturer {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    width: u32,
    height: u32,
    start: Instant,
    frame_acquired: bool,
    /// Last desktop image (without the pointer), `height * width * 4` bytes.
    image: Option<Vec<u8>>,
    /// Pointer state: DXGI reports it separately and only when it changes.
    pointer_pos: (i32, i32),
    pointer_visible: bool,
    pointer_shape: Option<PointerShape>,
    /// An update was held back because the border wasn't up yet. The border is excluded
    /// from capture, so showing it causes no new DXGI frame: publish the cached image
    /// as soon as it is up instead.
    withheld: bool,
    /// Name of the duplicated desktop (`Default`, `Winlogon`), if known.
    desktop: Option<String>,
}

// SAFETY: D3D11 devices are free-threaded. The immediate context and the
// duplication object are not thread-safe, but `DxgiCapturer` owns them
// exclusively and every method takes `&mut self`, so they are only ever used
// from one thread at a time; moving the owner between threads is fine.
unsafe impl Send for DxgiCapturer {}

impl DxgiCapturer {
    pub fn new() -> Result<Self, CaptureError> {
        unsafe { Self::create() }.map_err(|e| match e.code() {
            c if c == DXGI_ERROR_ACCESS_LOST => CaptureError::Lost(e.to_string()),
            // E_ACCESSDENIED: secure desktop (UAC/lock screen) or no interactive session.
            c if c.0 as u32 == 0x8007_0005 => CaptureError::Lost(format!(
                "desktop duplication access denied (secure desktop or non-interactive session): {e}"
            )),
            _ => CaptureError::Backend(format!("DXGI init failed: {e}")),
        })
    }

    unsafe fn create() -> windows::core::Result<Self> {
        // DuplicateOutput only works for the desktop the thread is attached to.
        let desktop = match desktop::attach_thread_to_input_desktop() {
            Ok(name) => Some(name),
            Err(e) => {
                tracing::debug!("capture stays on its desktop: {e}");
                desktop::thread_desktop_name()
            }
        };
        let (adapter, output) = find_primary_output()?;

        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))?;
        let context = context.ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))?;

        let duplication = output.DuplicateOutput(&device)?;
        let desc = duplication.GetDesc();
        let (width, height) = (desc.ModeDesc.Width, desc.ModeDesc.Height);

        let tex_desc = D3D11_TEXTURE2D_DESC {
            Width: width,
            Height: height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&tex_desc, None, Some(&mut staging))?;
        let staging = staging.ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))?;

        tracing::info!(width, height, desktop = desktop.as_deref().unwrap_or("?"), "DXGI desktop duplication started");
        Ok(Self {
            _device: device,
            context,
            duplication,
            staging,
            width,
            height,
            start: Instant::now(),
            frame_acquired: false,
            image: None,
            pointer_pos: (0, 0),
            pointer_visible: false,
            pointer_shape: None,
            withheld: false,
            desktop,
        })
    }

    unsafe fn acquire(&mut self, timeout: Duration) -> windows::core::Result<Option<RawFrame>> {
        if self.frame_acquired {
            // Release the previous frame as late as possible (recommended by MS docs).
            let _ = self.duplication.ReleaseFrame();
            self.frame_acquired = false;
        }

        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        let ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        if let Err(e) = self.duplication.AcquireNextFrame(ms, &mut info, &mut resource) {
            if e.code() == DXGI_ERROR_WAIT_TIMEOUT {
                return Ok(if self.withheld { self.compose() } else { None });
            }
            return Err(e);
        }
        self.frame_acquired = true;

        // The pointer is not part of the duplicated image; DXGI reports its position and
        // shape separately, and only when they change.
        let mut pointer_changed = false;
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            let pos = (p.Position.x, p.Position.y);
            let visible = p.Visible.as_bool();
            pointer_changed = pos != self.pointer_pos || visible != self.pointer_visible;
            self.pointer_pos = pos;
            self.pointer_visible = visible;
        }
        if info.PointerShapeBufferSize > 0 {
            match self.read_pointer_shape(info.PointerShapeBufferSize) {
                Ok(shape) => {
                    self.pointer_shape = shape;
                    pointer_changed = true;
                }
                Err(e) => tracing::debug!("pointer shape: {e}"),
            }
        }

        let image_changed = info.LastPresentTime != 0 && resource.is_some();
        if image_changed {
            let Some(resource) = resource else { return Ok(None) };
            let texture: ID3D11Texture2D = resource.cast()?;
            self.context.CopyResource(&self.staging, &texture);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
            let row_bytes = self.width as usize * 4;
            let image = self.image.get_or_insert_with(Vec::new);
            image.resize(row_bytes * self.height as usize, 0);
            let src = mapped.pData as *const u8;
            for y in 0..self.height as usize {
                std::ptr::copy_nonoverlapping(
                    src.add(y * mapped.RowPitch as usize),
                    image.as_mut_ptr().add(y * row_bytes),
                    row_bytes,
                );
            }
            self.context.Unmap(&self.staging, 0);
        }
        if !image_changed && !pointer_changed && !self.withheld {
            return Ok(None);
        }
        self.withheld = true;
        Ok(self.compose())
    }

    /// The cached image with the pointer drawn on it, if it may be published now.
    fn compose(&mut self) -> Option<RawFrame> {
        // Never publish a desktop unless the red border is up on the host's screen, on
        // this desktop. (It is excluded from capture, so this can't be judged from the
        // frame itself; the overlay reports where its window is shown.)
        if !self.desktop.as_deref().is_some_and(desktop::border_shown_on) {
            return None;
        }
        // A pointer-only update before the first image has nothing to draw on.
        let image = self.image.as_ref()?;
        self.withheld = false;
        let mut data = image.clone();
        let row_bytes = self.width as usize * 4;
        if self.pointer_visible {
            if let Some(shape) = &self.pointer_shape {
                let (x, y) = self.pointer_pos;
                pointer::draw(&mut data, self.width, self.height, row_bytes, shape, x, y);
            }
        }

        Some(RawFrame {
            width: self.width,
            height: self.height,
            stride: row_bytes,
            format: PixelFormat::Bgra8,
            data: Bytes::from(data),
            timestamp: self.start.elapsed(),
        })
    }

    /// Reads the new pointer shape of the frame just acquired.
    unsafe fn read_pointer_shape(&self, size: u32) -> windows::core::Result<Option<PointerShape>> {
        let mut data = vec![0u8; size as usize];
        let mut required = 0u32;
        let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
        self.duplication.GetFramePointerShape(size, data.as_mut_ptr().cast(), &mut required, &mut info)?;
        data.truncate(required.min(size) as usize);
        Ok(ShapeKind::from_dxgi(info.Type).map(|kind| PointerShape {
            kind,
            width: info.Width,
            height: info.Height,
            pitch: info.Pitch,
            data,
        }))
    }
}

impl Drop for DxgiCapturer {
    fn drop(&mut self) {
        if self.frame_acquired {
            unsafe {
                let _ = self.duplication.ReleaseFrame();
            }
        }
    }
}

impl ScreenCapturer for DxgiCapturer {
    fn name(&self) -> &str {
        "dxgi"
    }

    fn display(&self) -> DisplayInfo {
        DisplayInfo { width: self.width, height: self.height }
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<RawFrame>, CaptureError> {
        // DXGI notices most desktop switches itself, but not reliably all of them.
        if let (Some(ours), Some(input)) = (&self.desktop, desktop::input_desktop_name()) {
            if *ours != input {
                return Err(CaptureError::Lost(format!("input desktop changed: {ours} -> {input}")));
            }
        }
        unsafe { self.acquire(timeout) }.map_err(|e| {
            let code = e.code();
            if code == DXGI_ERROR_ACCESS_LOST || code == DXGI_ERROR_INVALID_CALL {
                // Mode change, desktop switch (UAC, lock screen), fullscreen app...
                CaptureError::Lost(e.to_string())
            } else {
                CaptureError::Backend(e.to_string())
            }
        })
    }
}

/// Returns the adapter and output that host the primary monitor
/// (the one whose desktop rectangle starts at `(0, 0)`).
unsafe fn find_primary_output() -> windows::core::Result<(IDXGIAdapter1, IDXGIOutput1)> {
    let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
    let mut fallback = None;
    let mut a = 0;
    while let Ok(adapter) = factory.EnumAdapters1(a) {
        let mut o = 0;
        while let Ok(output) = adapter.EnumOutputs(o) {
            let desc = output.GetDesc()?;
            if desc.AttachedToDesktop.as_bool() {
                let output1: IDXGIOutput1 = output.cast()?;
                let r = desc.DesktopCoordinates;
                if r.left == 0 && r.top == 0 {
                    return Ok((adapter, output1));
                }
                if fallback.is_none() {
                    fallback = Some((adapter.clone(), output1));
                }
            }
            o += 1;
        }
        a += 1;
    }
    fallback.ok_or_else(|| windows::core::Error::from(DXGI_ERROR_NOT_FOUND))
}
