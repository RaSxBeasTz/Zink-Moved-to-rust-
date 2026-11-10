//! On-GPU BGRA -> NV12 color conversion via the D3D11 Video Processor.
//!
//! Hardware H.264 encoder MFTs require NV12 input, but WGC hands us BGRA8.
//! This converts on the GPU (no CPU readback) by VideoProcessorBlt-ing each
//! captured BGRA texture into a reused NV12 texture that the encoder consumes.

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;

use crate::Result;

pub struct Bgra2Nv12 {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    device: ID3D11Device,
    width: u32,
    height: u32,
    /// Reused NV12 output texture (bind flag RENDER_TARGET so it can be a
    /// processor output, plus SHADER_RESOURCE not required for encoder input).
    nv12: ID3D11Texture2D,
    output_view: ID3D11VideoProcessorOutputView,
    /// Staging texture for CPU readback (software encoder path). Created lazily.
    staging: Option<ID3D11Texture2D>,
}

impl Bgra2Nv12 {
    /// Match the call viewer's limited-range Rec.601 NV12 conversion.
    pub(crate) fn use_sdr_studio_range(&self) -> Result<()> {
        let context: ID3D11VideoContext1 = self.video_context.cast()?;
        unsafe {
            context.VideoProcessorSetStreamColorSpace1(&self.processor, 0, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709);
            context.VideoProcessorSetOutputColorSpace1(&self.processor, DXGI_COLOR_SPACE_YCBCR_STUDIO_G22_LEFT_P601);
        }
        Ok(())
    }

    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()?;
            let context = device.GetImmediateContext()?;
            let video_context: ID3D11VideoContext = context.cast()?;

            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                InputWidth: width,
                InputHeight: height,
                OutputFrameRate: DXGI_RATIONAL {
                    Numerator: 30,
                    Denominator: 1,
                },
                OutputWidth: width,
                OutputHeight: height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };

            let enumerator = video_device.CreateVideoProcessorEnumerator(&content_desc)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            // Create the reusable NV12 output texture.
            let nv12_desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_NV12,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                // RENDER_TARGET so the video processor can write it; VIDEO_ENCODER
                // so the hardware H.264 MFT will accept it as input (without this
                // flag ProcessInput rejects the texture with E_UNEXPECTED).
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut nv12: Option<ID3D11Texture2D> = None;
            device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12))?;
            let nv12 = nv12.unwrap();

            let ovd = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view: Option<ID3D11VideoProcessorOutputView> = None;
            video_device.CreateVideoProcessorOutputView(
                &nv12,
                &enumerator,
                &ovd,
                Some(&mut output_view),
            )?;
            let output_view = output_view.unwrap();

            Ok(Self {
                video_device,
                video_context,
                processor,
                enumerator,
                device: device.clone(),
                width,
                height,
                nv12,
                output_view,
                staging: None,
            })
        }
    }

    /// Convert BGRA and read the NV12 result back into a tightly-packed CPU
    /// buffer (Y plane then interleaved UV), for the software encoder path.
    /// Returns (buffer, width, height). NV12 buffer size = w*h + w*h/2.
    pub fn convert_to_cpu(&mut self, bgra: &ID3D11Texture2D) -> Result<Vec<u8>> {
        self.convert(bgra)?;
        unsafe {
            let context = self.device.GetImmediateContext()?;
            if self.staging.is_none() {
                let desc = D3D11_TEXTURE2D_DESC {
                    Width: self.width,
                    Height: self.height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_NV12,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };
                let mut s = None;
                self.device.CreateTexture2D(&desc, None, Some(&mut s))?;
                self.staging = s;
            }
            let staging = self.staging.as_ref().unwrap();
            context.CopyResource(staging, &self.nv12);

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let w = self.width as usize;
            let h = self.height as usize;
            let pitch = mapped.RowPitch as usize;
            let src = mapped.pData as *const u8;

            // NV12: Y plane (h rows) then UV plane (h/2 rows), each row `pitch`
            // wide in the mapped texture but `w` wide when packed.
            let mut out = Vec::with_capacity(w * h + w * h / 2);
            for row in 0..h {
                let line = std::slice::from_raw_parts(src.add(row * pitch), w);
                out.extend_from_slice(line);
            }
            // UV plane starts after h rows in the mapped resource.
            let uv_base = src.add(h * pitch);
            for row in 0..(h / 2) {
                let line = std::slice::from_raw_parts(uv_base.add(row * pitch), w);
                out.extend_from_slice(line);
            }
            context.Unmap(staging, 0);
            Ok(out)
        }
    }

    /// Convert a BGRA source texture into the internal NV12 texture and return
    /// a reference to it. The returned texture is reused across calls (single
    /// in-flight frame — fine for our serialized encode loop).
    pub fn convert(&mut self, bgra: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
        let output_view = self.output_view.clone();
        self.blit(bgra, &output_view)?;
        Ok(&self.nv12)
    }

    /// Convert a BGRA source directly into a caller-supplied NV12 texture (e.g.
    /// one owned by the hardware encoder's sample allocator). The destination
    /// must be NV12 and created with D3D11_BIND_RENDER_TARGET.
    pub fn convert_into(
        &mut self,
        bgra: &ID3D11Texture2D,
        dst: &ID3D11Texture2D,
        dst_array_slice: u32,
    ) -> Result<()> {
        unsafe {
            // Allocator textures may be a single 2D texture or a texture array.
            // Pick the matching output-view dimension; targeting a slice of a
            // non-array texture (or vice versa) yields E_UNEXPECTED.
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            dst.GetDesc(&mut desc);
            let ovd = if desc.ArraySize > 1 {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2DARRAY,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2DArray: D3D11_TEX2D_ARRAY_VPOV {
                            MipSlice: 0,
                            FirstArraySlice: dst_array_slice,
                            ArraySize: 1,
                        },
                    },
                }
            } else {
                D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                    ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                    Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                        Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                    },
                }
            };
            let mut view: Option<ID3D11VideoProcessorOutputView> = None;
            self.video_device.CreateVideoProcessorOutputView(
                dst,
                &self.enumerator,
                &ovd,
                Some(&mut view),
            )?;
            let view = view.unwrap();
            self.blit(bgra, &view)
        }
    }

    /// Core VideoProcessorBlt: BGRA input view -> the given NV12 output view.
    fn blit(
        &mut self,
        bgra: &ID3D11Texture2D,
        output_view: &ID3D11VideoProcessorOutputView,
    ) -> Result<()> {
        unsafe {
            let ivd = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV {
                        MipSlice: 0,
                        ArraySlice: 0,
                    },
                },
            };
            let mut input_view: Option<ID3D11VideoProcessorInputView> = None;
            self.video_device.CreateVideoProcessorInputView(
                bgra,
                &self.enumerator,
                &ivd,
                Some(&mut input_view),
            )?;
            let input_view = input_view.unwrap();

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };

            let mut streams = [stream];
            self.video_context
                .VideoProcessorBlt(&self.processor, output_view, 0, &streams)?;
            let _ = std::mem::ManuallyDrop::take(&mut streams[0].pInputSurface);
            let _ = std::mem::ManuallyDrop::take(&mut streams[0].pInputSurfaceRight);
            Ok(())
        }
    }
}
