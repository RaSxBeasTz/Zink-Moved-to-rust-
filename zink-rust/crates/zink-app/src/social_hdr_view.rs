//! HDR10 presentation uses a Windows swap chain because Slint's image is RGBA8.

use std::{
    cell::Cell,
    time::{Duration, Instant},
};

use anyhow::{Context, ensure};
use windows::{
    Win32::{
        Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::{
            Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0},
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::{
                GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow,
                ScreenToClient,
            },
        },
        UI::{
            Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_F11},
            Shell::{DefSubclassProc, SetWindowSubclass},
            WindowsAndMessaging::*,
        },
    },
    core::{Interface, w},
};

pub struct HdrWindow {
    window: HWND,
    layout: Option<Box<HdrLayout>>,
}

struct HdrLayout {
    video: HWND,
    toggle: HWND,
    width: u32,
    height: u32,
    restore: Cell<Option<WINDOWPLACEMENT>>,
    last_cursor: Cell<Option<(i32, i32)>>,
    exit_until: Cell<Option<Instant>>,
    destroyed: Cell<bool>,
}

impl HdrWindow {
    pub fn open(owner: isize, width: u32, height: u32) -> anyhow::Result<Self> {
        let owner = HWND(owner as *mut _);
        let mut bounds = RECT::default();
        unsafe {
            GetWindowRect(owner, &mut bounds)?;
            let window = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Zink Call • HDR10 screen share"),
                WS_OVERLAPPEDWINDOW | WS_CLIPCHILDREN,
                bounds.left + 40,
                bounds.top + 64,
                960,
                580,
                Some(owner),
                None,
                None,
                None,
            )?;
            let mut viewer = Self {
                window,
                layout: None,
            };
            let video = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                1,
                1,
                Some(window),
                None,
                None,
                None,
            )?;
            let toggle = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("BUTTON"),
                w!("Fullscreen"),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                1,
                1,
                Some(window),
                None,
                None,
                None,
            )?;
            let layout = viewer.layout.insert(Box::new(HdrLayout {
                video,
                toggle,
                width,
                height,
                restore: Cell::new(None),
                last_cursor: Cell::new(None),
                exit_until: Cell::new(None),
                destroyed: Cell::new(false),
            }));
            let data = std::ptr::from_ref(layout.as_ref()) as usize;
            ensure!(
                SetWindowSubclass(window, Some(hdr_window_proc), 1, data).as_bool(),
                "initialize HDR fullscreen controls"
            );
            // Keep Escape available if the native button has keyboard focus.
            ensure!(
                SetWindowSubclass(toggle, Some(hdr_button_proc), 1, window.0 as usize).as_bool(),
                "initialize HDR fullscreen keyboard controls"
            );
            layout.position_children(window);
            let _ = ShowWindow(window, SW_SHOW);
            Ok(viewer)
        }
    }

    pub fn handle(&self) -> isize {
        self.layout
            .as_ref()
            .expect("initialized HDR viewer")
            .video
            .0 as isize
    }
}

impl Drop for HdrWindow {
    fn drop(&mut self) {
        if self
            .layout
            .as_ref()
            .is_none_or(|layout| !layout.destroyed.get())
        {
            unsafe {
                let _ = DestroyWindow(self.window);
            }
        }
    }
}

impl HdrLayout {
    fn position_children(&self, window: HWND) {
        let mut client = RECT::default();
        unsafe {
            if GetClientRect(window, &mut client).is_err()
                || client.right <= 0
                || client.bottom <= 0
            {
                return;
            }
            let scale = (f64::from(client.right) / f64::from(self.width))
                .min(f64::from(client.bottom) / f64::from(self.height));
            let width = (f64::from(self.width) * scale).round() as i32;
            let height = (f64::from(self.height) * scale).round() as i32;
            let _ = SetWindowPos(
                self.video,
                Some(HWND_BOTTOM),
                (client.right - width) / 2,
                (client.bottom - height) / 2,
                width,
                height,
                SWP_NOACTIVATE,
            );
            let _ = SetWindowPos(
                self.toggle,
                Some(HWND_TOP),
                (client.right - 162).max(0),
                12,
                150,
                40,
                SWP_NOACTIVATE,
            );
        }
    }

    fn set_fullscreen(&self, window: HWND, fullscreen: bool) {
        unsafe {
            if fullscreen {
                let mut placement = WINDOWPLACEMENT {
                    length: size_of::<WINDOWPLACEMENT>() as u32,
                    ..Default::default()
                };
                let mut monitor = MONITORINFO {
                    cbSize: size_of::<MONITORINFO>() as u32,
                    ..Default::default()
                };
                if GetWindowPlacement(window, &mut placement).is_err()
                    || !GetMonitorInfoW(
                        MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST),
                        &mut monitor,
                    )
                    .as_bool()
                {
                    return;
                }
                self.restore.set(Some(placement));
                let _ = ShowWindow(window, SW_RESTORE);
                SetWindowLongW(
                    window,
                    GWL_STYLE,
                    (WS_POPUP | WS_VISIBLE | WS_CLIPCHILDREN).0 as i32,
                );
                let rect = monitor.rcMonitor;
                let _ = SetWindowPos(
                    window,
                    Some(HWND_TOPMOST),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_FRAMECHANGED,
                );
                let _ = SetWindowTextW(self.toggle, w!("Exit fullscreen"));
                self.last_cursor.set(None);
                self.exit_until.set(None);
                let _ = SetTimer(Some(window), 1, 100, None);
                self.update_exit_button(window);
            } else if let Some(placement) = self.restore.take() {
                let _ = KillTimer(Some(window), 1);
                SetWindowLongW(
                    window,
                    GWL_STYLE,
                    (WS_OVERLAPPEDWINDOW | WS_VISIBLE | WS_CLIPCHILDREN).0 as i32,
                );
                let _ = SetWindowPos(
                    window,
                    Some(HWND_NOTOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_FRAMECHANGED,
                );
                let _ = SetWindowPlacement(window, &placement);
                let _ = SetWindowTextW(self.toggle, w!("Fullscreen"));
                let _ = ShowWindow(self.toggle, SW_SHOWNA);
            }
            let _ = SetFocus(Some(window));
        }
    }

    fn update_exit_button(&self, window: HWND) {
        let mut cursor = POINT::default();
        let mut client = RECT::default();
        unsafe {
            let inside = GetCursorPos(&mut cursor).is_ok()
                && ScreenToClient(window, &mut cursor).as_bool()
                && GetClientRect(window, &mut client).is_ok()
                && (0..client.right).contains(&cursor.x)
                && (0..client.bottom).contains(&cursor.y);
            let now = Instant::now();
            let previous = self.last_cursor.replace(Some((cursor.x, cursor.y)));
            if inside && previous.is_some_and(|point| point != (cursor.x, cursor.y)) {
                self.exit_until.set(Some(now + Duration::from_millis(2500)));
            }
            let reveal = inside && self.exit_until.get().is_some_and(|deadline| now < deadline);
            if IsWindowVisible(self.toggle).as_bool() != reveal {
                let _ = ShowWindow(self.toggle, if reveal { SW_SHOWNA } else { SW_HIDE });
            }
        }
    }
}

unsafe extern "system" fn hdr_window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    data: usize,
) -> LRESULT {
    // The viewer owns this state until after the window is destroyed.
    let layout = unsafe { &*(data as *const HdrLayout) };
    match message {
        WM_SIZE => layout.position_children(window),
        WM_COMMAND if lparam.0 == layout.toggle.0 as isize && wparam.0 >> 16 == 0 => {
            layout.set_fullscreen(window, layout.restore.get().is_none());
            return LRESULT(0);
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_ESCAPE.0) && layout.restore.get().is_some() => {
            layout.set_fullscreen(window, false);
            return LRESULT(0);
        }
        WM_KEYDOWN if wparam.0 == usize::from(VK_F11.0) => {
            if lparam.0 & (1 << 30) == 0 {
                layout.set_fullscreen(window, layout.restore.get().is_none());
            }
            return LRESULT(0);
        }
        WM_TIMER if wparam.0 == 1 && layout.restore.get().is_some() => {
            layout.update_exit_button(window);
        }
        WM_ACTIVATE if layout.restore.get().is_some() => {
            let order = if wparam.0 & 0xffff == 0 {
                HWND_NOTOPMOST
            } else {
                HWND_TOPMOST
            };
            unsafe {
                let _ = SetWindowPos(
                    window,
                    Some(order),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
        WM_NCDESTROY => layout.destroyed.set(true),
        WM_ERASEBKGND => {
            use windows::Win32::Graphics::Gdi::{
                BLACK_BRUSH, FillRect, GetStockObject, HBRUSH, HDC,
            };
            let mut rect = RECT::default();
            unsafe {
                if GetClientRect(window, &mut rect).is_ok() {
                    FillRect(
                        HDC(wparam.0 as *mut _),
                        &rect,
                        HBRUSH(GetStockObject(BLACK_BRUSH).0),
                    );
                    return LRESULT(1);
                }
            }
        }
        _ => {}
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

unsafe extern "system" fn hdr_button_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _: usize,
    owner: usize,
) -> LRESULT {
    if message == WM_KEYDOWN
        && (wparam.0 == usize::from(VK_ESCAPE.0) || wparam.0 == usize::from(VK_F11.0))
    {
        return unsafe { SendMessageW(HWND(owner as *mut _), message, Some(wparam), Some(lparam)) };
    }
    unsafe { DefSubclassProc(window, message, wparam, lparam) }
}

pub fn supports_display(owner: isize) -> bool {
    let monitor = unsafe { MonitorFromWindow(HWND(owner as *mut _), MONITOR_DEFAULTTONEAREST) };
    super::social_screen_share::desktop::monitor_has_hdr(monitor)
}

pub struct HdrPresenter {
    window: HWND,
    device_context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext1,
    processor: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    swap_chain: IDXGISwapChain3,
}

impl HdrPresenter {
    pub fn new(
        handle: isize,
        width: u32,
        height: u32,
        coded_width: u32,
        coded_height: u32,
    ) -> anyhow::Result<Self> {
        let window = HWND(handle as *mut _);
        unsafe {
            let mut device = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
                Some(&[D3D_FEATURE_LEVEL_11_0]),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                None,
            )?;
            let device = device.context("create the HDR presentation device")?;
            let device_context = device.GetImmediateContext()?;
            let video_context: ID3D11VideoContext1 = device_context.cast()?;
            let video_device: ID3D11VideoDevice = device.cast()?;
            let factory: IDXGIFactory2 = device.cast::<IDXGIDevice>()?.GetAdapter()?.GetParent()?;
            let chain_desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_R10G10B10A2_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                AlphaMode: DXGI_ALPHA_MODE_IGNORE,
                ..Default::default()
            };
            let swap_chain: IDXGISwapChain3 = factory
                .CreateSwapChainForHwnd(&device, window, &chain_desc, None, None)?
                .cast()?;
            let pq = DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020;
            ensure!(
                swap_chain.CheckColorSpaceSupport(pq)?
                    & DXGI_SWAP_CHAIN_COLOR_SPACE_SUPPORT_FLAG_PRESENT.0 as u32
                    != 0,
                "the display cannot present an HDR10 swap chain"
            );
            swap_chain.SetColorSpace1(pq)?;

            let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputWidth: coded_width,
                InputHeight: coded_height,
                OutputWidth: width,
                OutputHeight: height,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;
            let input_space = DXGI_COLOR_SPACE_YCBCR_STUDIO_G2084_LEFT_P2020;
            video_context.VideoProcessorSetStreamColorSpace1(&processor, 0, input_space);
            video_context.VideoProcessorSetOutputColorSpace1(&processor, pq);
            let visible = RECT {
                left: 0,
                top: 0,
                right: width as i32,
                bottom: height as i32,
            };
            video_context.VideoProcessorSetStreamSourceRect(&processor, 0, true, Some(&visible));

            let input_desc = D3D11_TEXTURE2D_DESC {
                Width: coded_width,
                Height: coded_height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_P010,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                ..Default::default()
            };
            let mut input = None;
            device.CreateTexture2D(&input_desc, None, Some(&mut input))?;
            let input = input.context("create the P010 playback texture")?;
            let mut input_view = None;
            video_device.CreateVideoProcessorInputView(
                &input,
                &enumerator,
                &D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                    FourCC: 0,
                    ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPIV {
                            MipSlice: 0,
                            ArraySlice: 0,
                        },
                    },
                },
                Some(&mut input_view),
            )?;
            let back_buffer: ID3D11Texture2D = swap_chain.GetBuffer(0)?;
            let mut output_view = None;
            video_device.CreateVideoProcessorOutputView(
                &back_buffer,
                &enumerator,
                &D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                },
                Some(&mut output_view),
            )?;
            Ok(Self {
                window,
                device_context,
                video_context,
                processor,
                input,
                input_view: input_view.context("open the HDR input view")?,
                output_view: output_view.context("open the HDR output view")?,
                swap_chain,
            })
        }
    }

    pub fn present(&self, data: &[u8], stride: u32) -> anyhow::Result<()> {
        unsafe {
            ensure!(
                IsWindow(Some(self.window)).as_bool(),
                "the HDR viewer was closed"
            );
            self.device_context.UpdateSubresource(
                &self.input,
                0,
                None,
                data.as_ptr().cast(),
                stride,
                0,
            );
            let mut stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
                ..Default::default()
            };
            let result = self.video_context.VideoProcessorBlt(
                &self.processor,
                &self.output_view,
                0,
                std::slice::from_ref(&stream),
            );
            let _ = std::mem::ManuallyDrop::take(&mut stream.pInputSurface);
            result?;
            self.swap_chain.Present(0, DXGI_PRESENT(0)).ok()?;
        }
        Ok(())
    }
}
