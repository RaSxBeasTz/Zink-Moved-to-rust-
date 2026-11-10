//! Windows.Graphics.Capture (WGC) session over a raw D3D11 device.
//!
//! Produces `CapturedFrame`s (BGRA8 D3D11 textures) pushed through a channel.
//! Frames stay on the GPU; no CPU readback happens here. The `FrameArrived`
//! callback recreates the frame pool when the content size changes.

use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::{IInspectable, Interface, Result as WResult};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11Texture2D, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_VIDEO_SUPPORT, D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{MonitorFromWindow, HMONITOR, MONITOR_DEFAULTTOPRIMARY};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::{CaptureTarget, CapturedFrame, PipelineError, Result};

/// Number of buffers in the WGC frame pool. Two is the common minimum that
/// avoids stalling the compositor while we consume the previous frame.
// ponytail: fixed at 2; bump only if we observe capture stalls under load.
const FRAME_POOL_BUFFERS: i32 = 2;

/// A running capture session. Dropping it stops capture and releases the
/// D3D11/WGC resources.
pub struct CaptureSession {
    _item: GraphicsCaptureItem,
    frame_pool: Direct3D11CaptureFramePool,
    session: GraphicsCaptureSession,
    device: ID3D11Device,
    // Kept so the FrameArrived handler stays registered for the session's life.
    _frame_arrived_token: i64,
}

impl CaptureSession {
    /// The raw D3D11 device backing this capture, shared later with the encoder
    /// so frames need no cross-device copy.
    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        // Close is best-effort; if the session already tore down, ignore.
        let _ = self.session.Close();
        let _ = self.frame_pool.Close();
    }
}

/// Options for a capture session.
#[derive(Clone, Copy, Debug)]
pub struct CaptureConfig {
    pub target: CaptureTarget,
    pub capture_cursor: bool,
}

/// Start capturing `config.target`. Frames arrive on the returned `Receiver`.
///
/// The channel is bounded (`bound`); if the consumer falls behind, the oldest
/// slot backpressures the callback, which drops the frame rather than block the
/// compositor thread. Returns the session (keep it alive) and the receiver.
pub fn start(
    config: CaptureConfig,
    bound: usize,
) -> Result<(CaptureSession, Receiver<CapturedFrame>)> {
    start_with_format(config, bound, DirectXPixelFormat::B8G8R8A8UIntNormalized)
}

/// Preserve HDR desktop luminance in an FP16 scRGB surface.
pub fn start_scrgb(config: CaptureConfig, bound: usize) -> Result<(CaptureSession, Receiver<CapturedFrame>)> {
    start_with_format(config, bound, DirectXPixelFormat::R16G16B16A16Float)
}

fn start_with_format(config: CaptureConfig, bound: usize, format: DirectXPixelFormat) -> Result<(CaptureSession, Receiver<CapturedFrame>)> {
    if !GraphicsCaptureSession::IsSupported()? {
        return Err(PipelineError::CaptureUnsupported);
    }

    let device = create_d3d_device()?;
    let d3d_interop = create_winrt_device(&device)?;

    let item = create_capture_item(config.target)?;
    let size = item.Size()?;

    let frame_pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &d3d_interop,
        format,
        FRAME_POOL_BUFFERS,
        size,
    )?;

    let (tx, rx) = std::sync::mpsc::sync_channel::<CapturedFrame>(bound);

    // Shared state the callback needs: the sender, the frame pool (to recreate
    // on resize), the winrt device (for recreation), and the capture-start QPC
    // baseline (set on the first frame so timestamps are relative).
    let state = std::sync::Arc::new(Mutex::new(FrameState {
        tx,
        device: AgileDevice(d3d_interop.clone()),
        last_size: size,
        format,
        start_ticks: None,
    }));

    let handler_state = state.clone();
    let handler = TypedEventHandler::<Direct3D11CaptureFramePool, IInspectable>::new(
        move |pool, _| -> WResult<()> {
            let pool = pool.as_ref().expect("frame pool present in callback");
            on_frame_arrived(pool, &handler_state);
            Ok(())
        },
    );

    let token = frame_pool.FrameArrived(&handler)?;

    let session = frame_pool.CreateCaptureSession(&item)?;
    // Cursor toggle is available on the session; ignore if the OS build lacks it.
    let _ = session.SetIsCursorCaptureEnabled(config.capture_cursor);
    session.StartCapture()?;

    Ok((
        CaptureSession {
            _item: item,
            frame_pool,
            session,
            device,
            _frame_arrived_token: token,
        },
        rx,
    ))
}

struct FrameState {
    tx: SyncSender<CapturedFrame>,
    device: AgileDevice,
    last_size: SizeInt32,
    format: DirectXPixelFormat,
    start_ticks: Option<i64>,
}

/// The `windows` bindings mark COM interfaces `!Send` conservatively, but WGC
/// objects created via `CreateFreeThreaded` are agile (marshal-free across
/// threads). The free-threaded frame pool invokes `FrameArrived` on an
/// arbitrary pool thread, so we must move the device we use for `Recreate`
/// into that closure. This newtype asserts the agility WGC guarantees.
// ponytail: narrow unsafe Send only for the agile WGC device, not a blanket wrapper.
struct AgileDevice(IDirect3DDevice);
unsafe impl Send for AgileDevice {}

fn on_frame_arrived(pool: &Direct3D11CaptureFramePool, state: &std::sync::Arc<Mutex<FrameState>>) {
    let mut guard = match state.lock() {
        Ok(g) => g,
        Err(_) => return,
    };

    let frame = match pool.TryGetNextFrame() {
        Ok(f) => f,
        Err(_) => return,
    };

    // Content size can change (window resize / display mode). WGC requires the
    // frame pool be recreated at the new size; do it, then this frame is the
    // last at the old size and we skip pushing it.
    let content_size = match frame.ContentSize() {
        Ok(s) => s,
        Err(_) => return,
    };
    if content_size.Width != guard.last_size.Width || content_size.Height != guard.last_size.Height
    {
        drop(frame);
        if pool.Recreate(
            &guard.device.0,
            guard.format,
            FRAME_POOL_BUFFERS,
            content_size,
        ).is_ok() {
            guard.last_size = content_size;
        }
        return;
    }

    // QPC system-relative time -> Duration since first frame.
    let ticks = match frame.SystemRelativeTime() {
        Ok(t) => t.Duration, // 100ns units
        Err(_) => return,
    };
    let start = *guard.start_ticks.get_or_insert(ticks);
    let rel_100ns = (ticks - start).max(0) as u64;
    let timestamp = Duration::from_nanos(rel_100ns * 100);

    // Pull the D3D11 texture out of the WinRT surface (stays on GPU).
    let surface = match frame.Surface() {
        Ok(s) => s,
        Err(_) => return,
    };
    let texture: ID3D11Texture2D = match surface.cast::<IDirect3DDxgiInterfaceAccess>() {
        Ok(access) => match unsafe { access.GetInterface::<ID3D11Texture2D>() } {
            Ok(t) => t,
            Err(_) => return,
        },
        Err(_) => return,
    };

    let width = content_size.Width.max(0) as u32;
    let height = content_size.Height.max(0) as u32;

    // Bounded, non-blocking: if the consumer is behind, drop this frame rather
    // than stall the compositor.
    let _ = state_send_nonblocking(
        &guard.tx,
        CapturedFrame {
            _capture_frame: frame,
            texture,
            timestamp,
            width,
            height,
        },
    );
}

fn state_send_nonblocking(tx: &SyncSender<CapturedFrame>, f: CapturedFrame) -> Result<()> {
    match tx.try_send(f) {
        Ok(()) => Ok(()),
        Err(std::sync::mpsc::TrySendError::Full(_)) => Ok(()), // drop, don't stall
        Err(std::sync::mpsc::TrySendError::Disconnected(_)) => Err(PipelineError::Stopped),
    }
}

/// Create a hardware D3D11 device with BGRA + video support and multithread
/// protection (needed later so the encoder's DXGI device manager can share it).
fn create_d3d_device() -> Result<ID3D11Device> {
    use windows::Win32::Foundation::HMODULE;
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let mut device: Option<ID3D11Device> = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            flags,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
    }
    let device = device.expect("D3D11CreateDevice succeeded but returned null device");

    // Enable multithread protection: WGC callbacks and later the encoder touch
    // the immediate context from different threads.
    use windows::Win32::Graphics::Direct3D11::ID3D11Multithread;
    if let Ok(mt) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = mt.SetMultithreadProtected(true);
        };
    }

    Ok(device)
}

/// Wrap the raw D3D11 device as a WinRT `IDirect3DDevice` for WGC.
fn create_winrt_device(device: &ID3D11Device) -> Result<IDirect3DDevice> {
    let dxgi: IDXGIDevice = device.cast()?;
    let inspectable = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi)? };
    let winrt: IDirect3DDevice = inspectable.cast()?;
    Ok(winrt)
}

/// Build a `GraphicsCaptureItem` for a monitor or window via the interop factory.
fn create_capture_item(target: CaptureTarget) -> Result<GraphicsCaptureItem> {
    let interop: IGraphicsCaptureItemInterop =
        windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;

    match target {
        CaptureTarget::Monitor(index) => {
            let hmon = monitor_at_index(index)?;
            let item: GraphicsCaptureItem = unsafe { interop.CreateForMonitor(hmon)? };
            Ok(item)
        }
        CaptureTarget::Window(hwnd) => {
            let item: GraphicsCaptureItem =
                unsafe { interop.CreateForWindow(HWND(hwnd as *mut _))? };
            Ok(item)
        }
    }
}

/// Resolve a monitor index into an `HMONITOR` by enumerating display monitors.
pub fn monitor_at_index(index: usize) -> Result<HMONITOR> {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{LPARAM, RECT};
    use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC};

    struct Collector {
        monitors: Vec<HMONITOR>,
    }

    unsafe extern "system" fn cb(
        hmon: HMONITOR,
        _hdc: HDC,
        _rect: *mut RECT,
        data: LPARAM,
    ) -> BOOL {
        let collector = &mut *(data.0 as *mut Collector);
        collector.monitors.push(hmon);
        BOOL(1) // continue
    }

    let mut collector = Collector {
        monitors: Vec::new(),
    };
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(cb),
            LPARAM(&mut collector as *mut _ as isize),
        );
    }

    collector
        .monitors
        .get(index)
        .copied()
        .ok_or(PipelineError::MonitorNotFound(index))
}

/// Convenience: the primary monitor's HMONITOR (used by examples).
pub fn primary_monitor() -> HMONITOR {
    unsafe { MonitorFromWindow(HWND(std::ptr::null_mut()), MONITOR_DEFAULTTOPRIMARY) }
}
