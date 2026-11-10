//! Borderless monitor capture for calls using DXGI Desktop Duplication.

use anyhow::{Context, anyhow, ensure};
use windows::{
    Win32::{
        Foundation::{HMODULE, POINT},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::{
                D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                D3D11_RESOURCE_MISC_GDI_COMPATIBLE, D3D11_SDK_VERSION, D3D11CreateDevice,
                ID3D11Device, ID3D11Texture2D,
            },
            Dxgi::{
                Common::{
                    DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_FORMAT,
                    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT,
                    DXGI_MODE_ROTATION_IDENTITY,
                },
                CreateDXGIFactory1, DXGI_ERROR_NOT_FOUND, DXGI_ERROR_WAIT_TIMEOUT,
                DXGI_OUTDUPL_FRAME_INFO, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput1, IDXGIOutput5,
                IDXGIOutput6, IDXGIOutputDuplication, IDXGISurface1,
            },
            Gdi::HMONITOR,
        },
        UI::WindowsAndMessaging::{
            CURSOR_SHOWING, CURSORINFO, DI_NORMAL, DrawIconEx, GetCursorInfo, HICON,
        },
    },
    core::Interface,
};

pub struct DesktopCapture {
    duplication: IDXGIOutputDuplication,
    device: ID3D11Device,
    snapshot: Option<ID3D11Texture2D>,
    pointer_position: Option<POINT>,
    format: DXGI_FORMAT,
}

impl DesktopCapture {
    pub fn start(monitor: HMONITOR) -> anyhow::Result<Self> {
        Self::create(monitor, false)
    }

    pub fn start_hdr10(monitor: HMONITOR) -> anyhow::Result<Self> {
        Self::create(monitor, true)
    }

    fn create(monitor: HMONITOR, hdr10: bool) -> anyhow::Result<Self> {
        let (adapter, output) = find_output(monitor)?;
        if hdr10 {
            ensure!(
                monitor_has_hdr(monitor),
                "Windows HDR is disabled on the shared display"
            );
        }
        let mut device = None;
        unsafe {
            D3D11CreateDevice(
                &adapter,
                D3D_DRIVER_TYPE_UNKNOWN,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
        }
        let device = device.context("DXGI did not provide a capture device")?;
        let (duplication, format) = unsafe {
            if hdr10 {
                let format = DXGI_FORMAT_R16G16B16A16_FLOAT;
                (
                    output
                        .cast::<IDXGIOutput5>()?
                        .DuplicateOutput1(&device, 0, &[format])?,
                    format,
                )
            } else {
                (output.DuplicateOutput(&device)?, DXGI_FORMAT_B8G8R8A8_UNORM)
            }
        };
        ensure!(
            unsafe { duplication.GetDesc() }.Rotation == DXGI_MODE_ROTATION_IDENTITY,
            "desktop duplication needs an unrotated display"
        );
        Ok(Self {
            duplication,
            device,
            snapshot: None,
            pointer_position: None,
            format,
        })
    }

    pub fn device(&self) -> &ID3D11Device {
        &self.device
    }

    pub fn next_frame(&mut self) -> anyhow::Result<Option<ID3D11Texture2D>> {
        // At the capture deadline, reuse the last image if the desktop has
        // not changed. Only the first acquisition needs to wait for an image.
        let timeout_ms = if self.snapshot.is_some() { 0 } else { 50 };
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        match unsafe {
            self.duplication
                .AcquireNextFrame(timeout_ms, &mut info, &mut resource)
        } {
            Ok(()) => {}
            // Repeat the last image on an idle desktop so the H.264 decoder
            // can start or recover without someone moving the mouse first.
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => {
                return Ok(self.snapshot.clone());
            }
            Err(error) => return Err(error.into()),
        }
        let frame = resource
            .context("DXGI returned an empty frame")
            .and_then(|resource| self.copy_frame(&resource.cast()?, &info));
        let release = unsafe { self.duplication.ReleaseFrame() };
        let frame = frame?;
        release?;
        Ok(Some(frame))
    }

    fn copy_frame(
        &mut self,
        source: &ID3D11Texture2D,
        info: &DXGI_OUTDUPL_FRAME_INFO,
    ) -> anyhow::Result<ID3D11Texture2D> {
        unsafe {
            if self.snapshot.is_none() {
                let mut desc = Default::default();
                source.GetDesc(&mut desc);
                ensure!(desc.Format == self.format, "unexpected desktop format");
                desc.BindFlags = if self.format == DXGI_FORMAT_B8G8R8A8_UNORM {
                    D3D11_BIND_RENDER_TARGET.0 as u32
                } else {
                    D3D11_BIND_SHADER_RESOURCE.0 as u32
                };
                desc.MiscFlags = if self.format == DXGI_FORMAT_B8G8R8A8_UNORM {
                    D3D11_RESOURCE_MISC_GDI_COMPATIBLE.0 as u32
                } else {
                    0
                };
                self.device
                    .CreateTexture2D(&desc, None, Some(&mut self.snapshot))?;
            }
            let snapshot = self
                .snapshot
                .as_ref()
                .context("create the desktop texture")?;
            self.device
                .GetImmediateContext()?
                .CopyResource(snapshot, source);
            // DXGI reports a visible pointer only when it is separate from the image.
            if info.LastMouseUpdateTime != 0 {
                self.pointer_position = info
                    .PointerPosition
                    .Visible
                    .as_bool()
                    .then_some(info.PointerPosition.Position);
            }
            if self.format == DXGI_FORMAT_B8G8R8A8_UNORM
                && let Some(position) = self.pointer_position
                && let Err(error) = draw_cursor(snapshot, position)
            {
                tracing::debug!(%error, "could not composite the desktop cursor");
            }
            Ok(snapshot.clone())
        }
    }
}

pub(crate) fn monitor_has_hdr(monitor: HMONITOR) -> bool {
    find_output(monitor)
        .and_then(|(_, output)| Ok(unsafe { output.cast::<IDXGIOutput6>()?.GetDesc1()? }))
        .is_ok_and(|desc| desc.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020)
}

fn find_output(monitor: HMONITOR) -> anyhow::Result<(IDXGIAdapter1, IDXGIOutput1)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        for index in 0.. {
            let adapter = match factory.EnumAdapters1(index) {
                Ok(adapter) => adapter,
                Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                Err(error) => return Err(error.into()),
            };
            for index in 0.. {
                let output = match adapter.EnumOutputs(index) {
                    Ok(output) => output,
                    Err(error) if error.code() == DXGI_ERROR_NOT_FOUND => break,
                    Err(error) => return Err(error.into()),
                };
                let desc = output.GetDesc()?;
                if desc.AttachedToDesktop.as_bool() && desc.Monitor == monitor {
                    return Ok((adapter, output.cast()?));
                }
            }
        }
    }
    Err(anyhow!("the shared display has no DXGI output"))
}

fn draw_cursor(texture: &ID3D11Texture2D, position: POINT) -> anyhow::Result<()> {
    let mut cursor = CURSORINFO {
        cbSize: size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    unsafe {
        GetCursorInfo(&mut cursor)?;
        if cursor.flags != CURSOR_SHOWING {
            return Ok(());
        }
        // Desktop Duplication gives the shape's top-left in output coordinates.
        let surface: IDXGISurface1 = texture.cast()?;
        let dc = surface.GetDC(false)?;
        let result = DrawIconEx(
            dc,
            position.x,
            position.y,
            HICON(cursor.hCursor.0),
            0,
            0,
            0,
            None,
            DI_NORMAL,
        );
        let release = surface.ReleaseDC(None);
        result?;
        release?;
    }
    Ok(())
}
