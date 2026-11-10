//! GPU port of the original Zink scRGB-to-PQ screen-share conversion.

use windows::core::{s, PCSTR};
use windows::Win32::Graphics::{
    Direct3D::{Fxc::D3DCompile, ID3DBlob, D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST},
    Direct3D11::*,
    Dxgi::Common::*,
};

use crate::{PipelineError, Result};

const SHADER: &str = r#"
Texture2D<float4> Desktop : register(t0);
SamplerState LinearClamp : register(s0);
struct Vertex { float4 position : SV_Position; float2 uv : TEXCOORD0; };
Vertex VS(uint id : SV_VertexID) {
    Vertex output;
    output.uv = float2((id << 1) & 2, id & 2);
    output.position = float4(output.uv * float2(2, -2) + float2(-1, 1), 0, 1);
    return output;
}
float3 Encode(float2 uv) {
    float3 rgb = Desktop.SampleLevel(LinearClamp, uv, 0).rgb;
    float3 rec2020 = mul(float3x3(.627404,.329282,.043314,.069097,.919540,.011361,.016392,.088013,.895595), rgb);
    // scRGB units represent 80 nits, while ST 2084 is normalized to 10,000 nits.
    float3 p = pow(saturate(rec2020 * .008), 2610.0 / 16384.0);
    float3 pq = pow((3424.0 / 4096.0 + (2413.0 / 128.0) * p) / (1 + (2392.0 / 128.0) * p), 2523.0 / 32.0);
    float y = dot(pq, float3(.2627, .6780, .0593));
    float3 code = float3(64 + 876*y, 512 + 896*(pq.b-y)/1.8814, 512 + 896*(pq.r-y)/1.4746);
    // P010 stores each ten-bit component in the high bits of a uint16.
    return round(code) * (64.0 / 65535.0);
}
float Y(Vertex v) : SV_Target { return Encode(v.uv).x; }
float2 UV(Vertex v) : SV_Target { return Encode(v.uv).yz; }
float4 SDR(Vertex v) : SV_Target {
    // Match the original Zink exposure. 203 nits maps to about 80% sRGB.
    float3 rgb = max(Desktop.SampleLevel(LinearClamp, v.uv, 0).rgb, 0) * .24;
    float luminance = dot(rgb, float3(.2126, .7152, .0722));
    float mapped = luminance <= .7 ? luminance : .7 + .3 * (1 - exp(-(luminance - .7) / .3));
    rgb *= mapped / max(luminance, .00001);
    rgb /= max(1.0, max(rgb.r, max(rgb.g, rgb.b)));
    float3 srgb = lerp(12.92 * rgb, 1.055 * pow(rgb, 1.0 / 2.4) - .055, step(.0031308, rgb));
    return float4(saturate(srgb), 1);
}
"#;

pub(crate) struct HdrToP010 {
    renderer: FullscreenShader,
    luma: Plane,
    chroma: Plane,
}

/// Tone-map an FP16 Windows capture before encoding it as SDR.
pub struct HdrToSdr {
    renderer: FullscreenShader,
    output: ShaderPass,
    shader_input: Option<ID3D11Texture2D>,
}

struct FullscreenShader {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vertex: ID3D11VertexShader,
    sampler: ID3D11SamplerState,
}

struct Plane {
    pass: ShaderPass,
    readback: ID3D11Texture2D,
    row_bytes: usize,
}

struct ShaderPass {
    target: ID3D11Texture2D,
    view: ID3D11RenderTargetView,
    shader: ID3D11PixelShader,
    width: u32,
    height: u32,
}

impl FullscreenShader {
    fn new(device: &ID3D11Device) -> Result<Self> {
        unsafe {
            let mut vertex = None;
            let shader = compile(s!("VS"), s!("vs_5_0"))?;
            device.CreateVertexShader(blob_bytes(&shader), None, Some(&mut vertex))?;
            let mut sampler = None;
            device.CreateSamplerState(
                &D3D11_SAMPLER_DESC {
                    Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                    AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                    AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D11_COMPARISON_NEVER,
                    MaxLOD: f32::MAX,
                    ..Default::default()
                },
                Some(&mut sampler),
            )?;
            Ok(Self {
                device: device.clone(),
                context: device.GetImmediateContext()?,
                vertex: vertex.ok_or(PipelineError::CaptureUnsupported)?,
                sampler: sampler.ok_or(PipelineError::CaptureUnsupported)?,
            })
        }
    }

    fn render(&self, source: &ID3D11Texture2D, passes: &[&ShaderPass]) -> Result<()> {
        unsafe {
            let mut view = None;
            self.device
                .CreateShaderResourceView(source, None, Some(&mut view))?;
            self.context
                .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            self.context.VSSetShader(&self.vertex, None);
            self.context.PSSetShaderResources(0, Some(&[view]));
            self.context
                .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
            for pass in passes {
                self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
                    Width: pass.width as f32,
                    Height: pass.height as f32,
                    MaxDepth: 1.0,
                    ..Default::default()
                }]));
                self.context
                    .OMSetRenderTargets(Some(&[Some(pass.view.clone())]), None);
                self.context.PSSetShader(&pass.shader, None);
                self.context.Draw(3, 0);
            }
            self.context.OMSetRenderTargets(None, None);
            self.context.PSSetShaderResources(0, Some(&[None]));
        }
        Ok(())
    }
}

impl HdrToSdr {
    pub fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            renderer: FullscreenShader::new(device)?,
            output: ShaderPass::new(device, width, height, DXGI_FORMAT_B8G8R8A8_UNORM, s!("SDR"))?,
            shader_input: None,
        })
    }

    pub fn convert(&mut self, source: &ID3D11Texture2D) -> Result<&ID3D11Texture2D> {
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            source.GetDesc(&mut desc);
            if desc.Format != DXGI_FORMAT_R16G16B16A16_FLOAT {
                return Err(PipelineError::TypeNegotiation(
                    "expected FP16 scRGB capture for tone mapping".to_owned(),
                ));
            }
            // Some Windows Graphics Capture surfaces cannot be sampled directly.
            let input = if desc.BindFlags & D3D11_BIND_SHADER_RESOURCE.0 as u32 == 0 {
                let needs_texture = self.shader_input.as_ref().is_none_or(|texture| {
                    let mut existing = D3D11_TEXTURE2D_DESC::default();
                    texture.GetDesc(&mut existing);
                    (existing.Width, existing.Height) != (desc.Width, desc.Height)
                });
                if needs_texture {
                    desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
                    desc.MiscFlags = 0;
                    self.renderer.device.CreateTexture2D(
                        &desc,
                        None,
                        Some(&mut self.shader_input),
                    )?;
                }
                let input = self
                    .shader_input
                    .as_ref()
                    .ok_or(PipelineError::CaptureUnsupported)?;
                self.renderer.context.CopyResource(input, source);
                input
            } else {
                source
            };
            self.renderer.render(input, &[&self.output])?;
        }
        Ok(&self.output.target)
    }
}

impl HdrToP010 {
    pub(crate) fn new(device: &ID3D11Device, width: u32, height: u32) -> Result<Self> {
        Ok(Self {
            renderer: FullscreenShader::new(device)?,
            luma: Plane::new(
                device,
                width,
                height,
                DXGI_FORMAT_R16_UNORM,
                s!("Y"),
                width as usize * 2,
            )?,
            chroma: Plane::new(
                device,
                width / 2,
                height / 2,
                DXGI_FORMAT_R16G16_UNORM,
                s!("UV"),
                width as usize * 2,
            )?,
        })
    }

    pub(crate) fn convert(&self, source: &ID3D11Texture2D) -> Result<Vec<u8>> {
        self.renderer
            .render(source, &[&self.luma.pass, &self.chroma.pass])?;
        unsafe {
            let context = &self.renderer.context;
            let mut p010 = Vec::with_capacity(
                self.luma.row_bytes * (self.luma.pass.height + self.chroma.pass.height) as usize,
            );
            for plane in [&self.luma, &self.chroma] {
                context.CopyResource(&plane.readback, &plane.pass.target);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(&plane.readback, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                for row in 0..plane.pass.height as usize {
                    let address = mapped
                        .pData
                        .cast::<u8>()
                        .add(row * mapped.RowPitch as usize);
                    p010.extend_from_slice(std::slice::from_raw_parts(address, plane.row_bytes));
                }
                context.Unmap(&plane.readback, 0);
            }
            Ok(p010)
        }
    }
}

impl Plane {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        entry: PCSTR,
        row_bytes: usize,
    ) -> Result<Self> {
        let pass = ShaderPass::new(device, width, height, format, entry)?;
        unsafe {
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            pass.target.GetDesc(&mut desc);
            desc.Usage = D3D11_USAGE_STAGING;
            desc.BindFlags = 0;
            desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
            let mut readback = None;
            device.CreateTexture2D(&desc, None, Some(&mut readback))?;
            Ok(Self {
                pass,
                row_bytes,
                readback: readback.ok_or(PipelineError::CaptureUnsupported)?,
            })
        }
    }
}

impl ShaderPass {
    fn new(
        device: &ID3D11Device,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        entry: PCSTR,
    ) -> Result<Self> {
        unsafe {
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut target = None;
            device.CreateTexture2D(&desc, None, Some(&mut target))?;
            let target = target.ok_or(PipelineError::CaptureUnsupported)?;
            let mut view = None;
            device.CreateRenderTargetView(&target, None, Some(&mut view))?;
            let blob = compile(entry, s!("ps_5_0"))?;
            let mut shader = None;
            device.CreatePixelShader(blob_bytes(&blob), None, Some(&mut shader))?;
            Ok(Self {
                target,
                width,
                height,
                view: view.ok_or(PipelineError::CaptureUnsupported)?,
                shader: shader.ok_or(PipelineError::CaptureUnsupported)?,
            })
        }
    }
}

fn compile(entry: PCSTR, target: PCSTR) -> Result<ID3DBlob> {
    let mut shader = None;
    let mut messages = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr().cast(),
            SHADER.len(),
            s!("Zink HDR10"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut shader,
            Some(&mut messages),
        )
    };
    if let Err(error) = result {
        let detail = messages
            .as_ref()
            .map(|blob| String::from_utf8_lossy(blob_bytes(blob)).into_owned());
        return Err(PipelineError::TypeNegotiation(
            detail.unwrap_or_else(|| error.to_string()),
        ));
    }
    shader.ok_or(PipelineError::CaptureUnsupported)
}

fn blob_bytes(blob: &ID3DBlob) -> &[u8] {
    unsafe { std::slice::from_raw_parts(blob.GetBufferPointer().cast(), blob.GetBufferSize()) }
}
