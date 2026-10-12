//! Windows backend: DXGI Desktop Duplication (Windows 8 and later).
//!
//! Each [`DxgiCapturer`] owns a D3D11 device on the adapter that drives its
//! output, an `IDXGIOutputDuplication`, and a CPU-readable staging texture.
//! A frame is acquired, copied to staging on the GPU, released, then mapped
//! and copied row by row into a tightly packed buffer.
//!
//! Recovery (06 §1): `DXGI_ERROR_ACCESS_LOST` (desktop switch, UAC, lock
//! screen, mode change) and device loss drop the duplication; the next call
//! re-enumerates the outputs and rebuilds the device and duplication. While
//! the desktop cannot be duplicated (for example the secure desktop without
//! SYSTEM rights), calls wait out their timeout and return `Ok(None)`.

use std::time::Duration;

use windows::Win32::Foundation::{E_ACCESSDENIED, HMODULE};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_FLAG, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
    D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device,
    ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_NOT_CURRENTLY_AVAILABLE, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_SESSION_DISCONNECTED,
    DXGI_ERROR_UNSUPPORTED, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1,
    IDXGIFactory1, IDXGIOutput, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::core::{HRESULT, Interface};

use crate::cursor::{
    CursorShape, CursorState, color_to_rgba, masked_color_to_rgba, monochrome_to_rgba,
};
use crate::{BYTES_PER_PIXEL, CaptureError, Capturer, DisplayInfo, Frame, clock, copy_rows};
use std::sync::Arc;
use windows::Win32::Graphics::Dxgi::{
    DXGI_OUTDUPL_POINTER_POSITION, DXGI_OUTDUPL_POINTER_SHAPE_INFO,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR, DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR,
    DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME,
};

/// What to do after a DXGI call fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Recovery {
    /// No new frame within the timeout.
    Timeout,
    /// The duplication or device is gone; rebuild it.
    Reopen,
    /// Duplication is temporarily impossible; wait and retry.
    Transient,
    /// Anything else: report to the caller.
    Fatal,
}

fn classify(code: HRESULT) -> Recovery {
    match code {
        c if c == DXGI_ERROR_WAIT_TIMEOUT => Recovery::Timeout,
        c if c == DXGI_ERROR_ACCESS_LOST
            || c == DXGI_ERROR_DEVICE_REMOVED
            || c == DXGI_ERROR_DEVICE_RESET =>
        {
            Recovery::Reopen
        }
        c if c == E_ACCESSDENIED
            || c == DXGI_ERROR_NOT_CURRENTLY_AVAILABLE
            || c == DXGI_ERROR_SESSION_DISCONNECTED =>
        {
            Recovery::Transient
        }
        _ => Recovery::Fatal,
    }
}

fn os_error(context: &'static str, err: windows::core::Error) -> CaptureError {
    CaptureError::Os {
        context,
        code: err.code().0 as u32,
        message: err.message(),
    }
}

/// An attached output and the adapter that drives it.
struct Output {
    info: DisplayInfo,
    adapter: IDXGIAdapter1,
    output: IDXGIOutput,
}

fn device_name(raw: &[u16]) -> String {
    let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

/// Lists outputs attached to the desktop, across all adapters.
fn enumerate() -> Result<Vec<Output>, CaptureError> {
    // SAFETY: plain COM factory creation.
    let factory: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| os_error("CreateDXGIFactory1", e))?;
    let mut outputs: Vec<Output> = Vec::new();
    for a in 0.. {
        // SAFETY: index enumeration; DXGI_ERROR_NOT_FOUND marks the end.
        let adapter = match unsafe { factory.EnumAdapters1(a) } {
            Ok(adapter) => adapter,
            Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
            Err(e) => return Err(os_error("EnumAdapters1", e)),
        };
        for o in 0.. {
            // SAFETY: as above.
            let output = match unsafe { adapter.EnumOutputs(o) } {
                Ok(output) => output,
                Err(e) if e.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(e) => return Err(os_error("EnumOutputs", e)),
            };
            // SAFETY: GetDesc fills a plain struct.
            let desc = unsafe { output.GetDesc() }.map_err(|e| os_error("GetDesc", e))?;
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let name = device_name(&desc.DeviceName);
            // Hybrid-GPU systems can list one monitor under two adapters; keep the first.
            if outputs.iter().any(|known| known.info.name == name) {
                continue;
            }
            let r = desc.DesktopCoordinates;
            outputs.push(Output {
                info: DisplayInfo {
                    id: outputs.len() as u32,
                    name,
                    x: r.left,
                    y: r.top,
                    width: (r.right - r.left).max(0) as u32,
                    height: (r.bottom - r.top).max(0) as u32,
                    // The primary monitor always has its top-left corner at the origin.
                    primary: r.left == 0 && r.top == 0,
                },
                adapter: adapter.clone(),
                output,
            });
        }
    }
    Ok(outputs)
}

/// GPU objects for one duplication session.
struct Session {
    context: ID3D11DeviceContext,
    device: ID3D11Device,
    duplication: IDXGIOutputDuplication,
    staging: Option<(ID3D11Texture2D, u32, u32)>,
}

impl Session {
    /// Creates a device on the output's adapter and starts duplication.
    fn start(out: &Output) -> windows::core::Result<Session> {
        let mut device = None;
        let mut context = None;
        // SAFETY: out pointers are valid locals; an explicit adapter requires DRIVER_TYPE_UNKNOWN.
        unsafe {
            D3D11CreateDevice(
                &out.adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_FLAG(0),
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
        }
        let device = device.expect("D3D11CreateDevice returned no device");
        let context = context.expect("D3D11CreateDevice returned no context");
        let output1: IDXGIOutput1 = out.output.cast()?;
        // SAFETY: device is a live D3D11 device on the output's adapter.
        let duplication = unsafe { output1.DuplicateOutput(&device) }?;
        Ok(Session {
            context,
            device,
            duplication,
            staging: None,
        })
    }

    /// Returns a staging texture matching `desc`, creating it if the size changed.
    fn staging_for(
        &mut self,
        src: &D3D11_TEXTURE2D_DESC,
    ) -> windows::core::Result<ID3D11Texture2D> {
        if let Some((tex, w, h)) = &self.staging
            && *w == src.Width
            && *h == src.Height
        {
            return Ok(tex.clone());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: src.Width,
            Height: src.Height,
            MipLevels: 1,
            ArraySize: 1,
            Format: src.Format,
            SampleDesc: src.SampleDesc,
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };
        let mut tex = None;
        // SAFETY: desc is fully initialised; out pointer is a valid local.
        unsafe { self.device.CreateTexture2D(&desc, None, Some(&mut tex)) }?;
        let tex = tex.expect("CreateTexture2D returned no texture");
        self.staging = Some((tex.clone(), src.Width, src.Height));
        Ok(tex)
    }
}

/// Outcome of one acquire attempt.
enum Acquired {
    /// The image changed; pixels are in the capturer buffer.
    Image { timestamp_us: u64 },
    /// Only the pointer changed.
    PointerOnly,
}

/// Reads the new pointer shape of the current frame (before it is released).
fn read_pointer_shape(
    duplication: &IDXGIOutputDuplication,
    size: u32,
    buffer: &mut Vec<u8>,
) -> windows::core::Result<Option<CursorShape>> {
    buffer.resize(size as usize, 0);
    let mut required = 0u32;
    let mut info = DXGI_OUTDUPL_POINTER_SHAPE_INFO::default();
    // SAFETY: the buffer has `size` writable bytes; out pointers are valid locals.
    unsafe {
        duplication.GetFramePointerShape(size, buffer.as_mut_ptr().cast(), &mut required, &mut info)
    }?;
    let data = &buffer[..required.min(size) as usize];
    let (width, pitch) = (info.Width, info.Pitch);
    let (height, rgba) = match info.Type as i32 {
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MONOCHROME.0 => {
            // The height covers both masks.
            let h = info.Height / 2;
            (h, monochrome_to_rgba(width, h, pitch, data))
        }
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_COLOR.0 => {
            (info.Height, color_to_rgba(width, info.Height, pitch, data))
        }
        t if t == DXGI_OUTDUPL_POINTER_SHAPE_TYPE_MASKED_COLOR.0 => (
            info.Height,
            masked_color_to_rgba(width, info.Height, pitch, data),
        ),
        _ => (0, None),
    };
    Ok(rgba.map(|rgba| {
        CursorShape::new(
            width,
            height,
            info.HotSpot.x.max(0) as u32,
            info.HotSpot.y.max(0) as u32,
            rgba,
        )
    }))
}

/// DXGI Desktop Duplication capturer for one output.
pub struct DxgiCapturer {
    display: DisplayInfo,
    session: Option<Session>,
    buffer: Vec<u8>,
    width: u32,
    height: u32,
    timestamp_us: u64,
    has_image: bool,
    cursor: CursorState,
    /// Top-left of the pointer image, as DXGI reports it.
    pointer_origin: (i32, i32),
    shape_buffer: Vec<u8>,
}

impl DxgiCapturer {
    fn find(name: &str) -> Result<Output, CaptureError> {
        enumerate()?
            .into_iter()
            .find(|o| o.info.name == name)
            .ok_or_else(|| CaptureError::DisplayNotFound(name.to_owned()))
    }

    /// Rebuilds the session after loss. Returns `Ok(false)` if duplication is
    /// temporarily impossible.
    fn reopen(&mut self) -> Result<bool, CaptureError> {
        self.session = None;
        let out = Self::find(&self.display.name)?;
        match Session::start(&out) {
            Ok(session) => {
                self.display = DisplayInfo {
                    id: self.display.id,
                    ..out.info
                };
                self.session = Some(session);
                Ok(true)
            }
            Err(e) => match classify(e.code()) {
                Recovery::Transient | Recovery::Reopen => Ok(false),
                _ if e.code() == DXGI_ERROR_UNSUPPORTED => Ok(false),
                _ => Err(os_error("DuplicateOutput", e)),
            },
        }
    }

    /// Acquires one frame from the live session. `Err` carries the raw error for classification.
    fn acquire(&mut self, timeout: Duration) -> windows::core::Result<Acquired> {
        let session = self.session.as_mut().expect("acquire without session");
        let timeout_ms = timeout.as_millis().min(u32::MAX as u128) as u32;
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        // SAFETY: out pointers are valid locals.
        unsafe {
            session
                .duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        }?;

        // Pointer updates ride along with frames; the shape must be read
        // before the frame is released. A failed shape read only loses the shape.
        let pointer = (info.LastMouseUpdateTime != 0).then_some(info.PointerPosition);
        let shape = if info.PointerShapeBufferSize > 0 {
            read_pointer_shape(
                &session.duplication,
                info.PointerShapeBufferSize,
                &mut self.shape_buffer,
            )
            .ok()
            .flatten()
        } else {
            None
        };

        // From here on the frame must be released, whatever happens.
        let result = (|| {
            if info.LastPresentTime == 0 {
                return Ok(Acquired::PointerOnly);
            }
            let texture: ID3D11Texture2D = resource
                .as_ref()
                .expect("AcquireNextFrame returned no resource")
                .cast()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            // SAFETY: valid out pointer.
            unsafe { texture.GetDesc(&mut desc) };
            if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
                return Err(windows::core::Error::new(
                    DXGI_ERROR_UNSUPPORTED,
                    "desktop texture is not BGRA8",
                ));
            }
            let staging = session.staging_for(&desc)?;
            // SAFETY: both textures have identical size and format.
            unsafe { session.context.CopyResource(&staging, &texture) };
            Ok(Acquired::Image {
                timestamp_us: clock::qpc_to_us(info.LastPresentTime),
            })
        })();
        drop(resource);
        // SAFETY: a frame was acquired above.
        let released = unsafe { session.duplication.ReleaseFrame() };
        self.update_cursor(pointer, shape);
        let acquired = result?;
        released?;

        if let Acquired::Image { .. } = acquired {
            self.read_staging()?;
        }
        Ok(acquired)
    }

    /// Applies a pointer update; bumps the serial if anything changed.
    fn update_cursor(
        &mut self,
        pointer: Option<DXGI_OUTDUPL_POINTER_POSITION>,
        shape: Option<CursorShape>,
    ) {
        let before = (
            self.cursor.visible,
            self.cursor.x,
            self.cursor.y,
            self.cursor.shape.as_ref().map(|s| s.id),
        );
        if let Some(shape) = shape
            && self.cursor.shape.as_ref().map(|s| s.id) != Some(shape.id)
        {
            self.cursor.shape = Some(Arc::new(shape));
        }
        if let Some(p) = pointer {
            self.pointer_origin = (p.Position.x, p.Position.y);
            self.cursor.visible = p.Visible.as_bool();
        }
        let hot = self
            .cursor
            .shape
            .as_ref()
            .map_or((0, 0), |s| (s.hot_x as i32, s.hot_y as i32));
        self.cursor.x = self.pointer_origin.0 + hot.0;
        self.cursor.y = self.pointer_origin.1 + hot.1;
        let after = (
            self.cursor.visible,
            self.cursor.x,
            self.cursor.y,
            self.cursor.shape.as_ref().map(|s| s.id),
        );
        if after != before {
            self.cursor.serial += 1;
        }
    }

    /// Maps the staging texture and copies it into the tightly packed buffer.
    fn read_staging(&mut self) -> windows::core::Result<()> {
        let session = self.session.as_ref().expect("read without session");
        let (staging, width, height) = session.staging.as_ref().expect("no staging texture");
        let (width, height) = (*width, *height);
        let row_bytes = width as usize * BYTES_PER_PIXEL;
        self.buffer.resize(row_bytes * height as usize, 0);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        // SAFETY: staging is a CPU-readable texture on this context's device.
        unsafe {
            session
                .context
                .Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?
        };
        let pitch = mapped.RowPitch as usize;
        let len = pitch * (height as usize).saturating_sub(1) + row_bytes;
        // SAFETY: Map succeeded, so pData points to at least `len` readable bytes until Unmap.
        let src = unsafe { std::slice::from_raw_parts(mapped.pData as *const u8, len) };
        copy_rows(src, pitch, &mut self.buffer, row_bytes, height as usize);
        // SAFETY: matches the Map above.
        unsafe { session.context.Unmap(staging, 0) };

        self.width = width;
        self.height = height;
        Ok(())
    }

    fn frame(&self, dirty: bool) -> Frame<'_> {
        Frame {
            width: self.width,
            height: self.height,
            stride: self.width as usize * BYTES_PER_PIXEL,
            data: &self.buffer,
            timestamp_us: self.timestamp_us,
            dirty,
        }
    }
}

impl Capturer for DxgiCapturer {
    fn displays() -> Result<Vec<DisplayInfo>, CaptureError> {
        Ok(enumerate()?.into_iter().map(|o| o.info).collect())
    }

    fn open(display: &DisplayInfo) -> Result<Self, CaptureError> {
        let out = Self::find(&display.name)?;
        let session = Session::start(&out).map_err(|e| {
            if e.code() == DXGI_ERROR_UNSUPPORTED || classify(e.code()) == Recovery::Transient {
                CaptureError::Unsupported(e.message())
            } else {
                os_error("DuplicateOutput", e)
            }
        })?;
        Ok(DxgiCapturer {
            display: DisplayInfo {
                id: display.id,
                ..out.info
            },
            session: Some(session),
            buffer: Vec::new(),
            width: 0,
            height: 0,
            timestamp_us: 0,
            has_image: false,
            cursor: CursorState::default(),
            pointer_origin: (0, 0),
            shape_buffer: Vec::new(),
        })
    }

    fn display(&self) -> &DisplayInfo {
        &self.display
    }

    fn next_frame(&mut self, timeout: Duration) -> Result<Option<Frame<'_>>, CaptureError> {
        if self.session.is_none() && !self.reopen()? {
            std::thread::sleep(timeout);
            return Ok(None);
        }
        match self.acquire(timeout) {
            Ok(Acquired::Image { timestamp_us }) => {
                self.timestamp_us = timestamp_us;
                self.has_image = true;
                Ok(Some(self.frame(true)))
            }
            Ok(Acquired::PointerOnly) if self.has_image => Ok(Some(self.frame(false))),
            Ok(Acquired::PointerOnly) => Ok(None),
            Err(e) => match classify(e.code()) {
                Recovery::Timeout => Ok(None),
                Recovery::Reopen | Recovery::Transient => {
                    // Drop the session; the next call rebuilds it.
                    self.session = None;
                    Ok(None)
                }
                Recovery::Fatal => Err(os_error("AcquireNextFrame", e)),
            },
        }
    }

    fn cursor(&self) -> Option<&CursorState> {
        Some(&self.cursor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::primary_display;
    use windows::Win32::Foundation::E_FAIL;

    #[test]
    fn classify_maps_dxgi_errors() {
        assert_eq!(classify(DXGI_ERROR_WAIT_TIMEOUT), Recovery::Timeout);
        assert_eq!(classify(DXGI_ERROR_ACCESS_LOST), Recovery::Reopen);
        assert_eq!(classify(DXGI_ERROR_DEVICE_REMOVED), Recovery::Reopen);
        assert_eq!(classify(E_ACCESSDENIED), Recovery::Transient);
        assert_eq!(
            classify(DXGI_ERROR_NOT_CURRENTLY_AVAILABLE),
            Recovery::Transient
        );
        assert_eq!(classify(E_FAIL), Recovery::Fatal);
    }

    #[test]
    fn device_name_stops_at_nul() {
        let mut raw = [0u16; 32];
        for (dst, src) in raw.iter_mut().zip(r"\\.\DISPLAY1".encode_utf16()) {
            *dst = src;
        }
        assert_eq!(device_name(&raw), r"\\.\DISPLAY1");
    }

    // The tests below need a real interactive desktop, which CI runners do not
    // have. Run them locally with `cargo test -p kanodesk-capture -- --ignored`.

    #[test]
    #[ignore = "needs an interactive desktop"]
    fn enumerates_a_primary_display() {
        let displays = DxgiCapturer::displays().unwrap();
        assert!(!displays.is_empty());
        assert!(displays.iter().any(|d| d.primary));
    }

    fn nudge_pointer() {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let mut p = POINT::default();
        // SAFETY: valid out pointer; same DPI context for both calls.
        unsafe {
            GetCursorPos(&mut p).unwrap();
            SetCursorPos(p.x + 1, p.y).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            SetCursorPos(p.x, p.y).unwrap();
        }
    }

    #[test]
    #[ignore = "needs an interactive desktop; nudges the pointer by 1 px"]
    fn the_pointer_is_reported_on_exactly_one_display() {
        let mut seen = Vec::new();
        for display in DxgiCapturer::displays().unwrap() {
            let mut capturer = DxgiCapturer::open(&display).unwrap();
            let _ = capturer.next_frame(Duration::from_millis(100)).unwrap();
            // DXGI reports the pointer when it changes: nudge it 1 px and back.
            nudge_pointer();
            for _ in 0..10 {
                let _ = capturer.next_frame(Duration::from_millis(50)).unwrap();
            }
            let cursor = capturer.cursor().unwrap().clone();
            if cursor.visible {
                let shape = cursor.shape.clone().expect("visible pointer has a shape");
                assert!(shape.width > 0 && shape.height > 0);
                assert_eq!(shape.rgba.len(), (shape.width * shape.height * 4) as usize);
                assert!(shape.hot_x < shape.width && shape.hot_y < shape.height);
                assert!(
                    shape.rgba.chunks(4).any(|p| p[3] > 0),
                    "not fully transparent"
                );
                assert!((0..display.width as i32).contains(&cursor.x), "{cursor:?}");
                assert!((0..display.height as i32).contains(&cursor.y), "{cursor:?}");
                assert!(cursor.serial > 0);
                seen.push(display.id);
            }
        }
        assert_eq!(seen.len(), 1, "visible on displays {seen:?}");
    }

    #[test]
    #[ignore = "needs an interactive desktop"]
    fn first_frame_is_dirty_and_full_size() {
        let displays = DxgiCapturer::displays().unwrap();
        let display = primary_display(&displays).unwrap();
        let mut capturer = DxgiCapturer::open(display).unwrap();
        // The first acquire after DuplicateOutput returns the whole desktop.
        let frame = (0..20)
            .find_map(|_| {
                capturer
                    .next_frame(Duration::from_millis(100))
                    .unwrap()
                    .map(|f| (f.width, f.height, f.stride, f.data.len(), f.dirty))
            })
            .expect("no frame within 2 s");
        let (w, h, stride, len, dirty) = frame;
        assert!(dirty);
        assert!(w > 0 && h > 0);
        assert_eq!(stride, w as usize * 4);
        assert_eq!(len, stride * h as usize);
    }
}
