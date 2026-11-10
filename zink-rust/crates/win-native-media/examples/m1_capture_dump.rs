//! M1 verification: capture the primary monitor, save a few frames as BMP,
//! and log inter-frame timing.
//!
//! Run: `cargo run --example m1_capture_dump`
//! Output: frame_000.bmp .. in the working directory. Inspect visually; the
//! logged deltas should look like ~monitor-refresh-spaced capture times.

use std::io::Write;
use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::{CaptureTarget, CapturedFrame};
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D, D3D11_CPU_ACCESS_READ,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};

const FRAMES_TO_DUMP: usize = 10;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let (session, rx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        4,
    )
    .expect("start capture");

    let device = session.device().clone();
    let context = unsafe { device.GetImmediateContext().expect("immediate context") };

    let mut saved = 0usize;
    let mut prev: Option<Duration> = None;
    // Skip the very first few frames while the pool warms up.
    while saved < FRAMES_TO_DUMP {
        let frame = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(_) => {
                eprintln!("timed out waiting for frames");
                break;
            }
        };

        let delta = prev.map(|p| frame.timestamp.saturating_sub(p));
        prev = Some(frame.timestamp);
        tracing::info!(
            frame = saved,
            ts_ms = frame.timestamp.as_millis(),
            delta_ms = delta.map(|d| d.as_millis()).unwrap_or(0),
            "{}x{}",
            frame.width,
            frame.height
        );

        let path = format!("frame_{saved:03}.bmp");
        if let Err(e) = save_frame_bmp(&device, &context, &frame, &path) {
            eprintln!("save {path} failed: {e:?}");
        }
        saved += 1;
    }

    drop(session);
    println!("dumped {saved} frames");
}

/// Copy the GPU texture to a staging texture, map it, write a 24-bit BMP.
/// This is the only CPU readback in the crate and exists purely for M1 eyeballing.
fn save_frame_bmp(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    frame: &CapturedFrame,
    path: &str,
) -> windows::core::Result<()> {
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        frame.texture.GetDesc(&mut desc);

        let mut staging_desc = desc;
        staging_desc.Usage = D3D11_USAGE_STAGING;
        staging_desc.BindFlags = 0;
        staging_desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        staging_desc.MiscFlags = 0;

        let mut staging: Option<ID3D11Texture2D> = None;
        device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();

        context.CopyResource(&staging, &frame.texture);

        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

        let w = desc.Width as usize;
        let h = desc.Height as usize;
        let src =
            std::slice::from_raw_parts(mapped.pData as *const u8, mapped.RowPitch as usize * h);

        let result = write_bmp(path, w, h, src, mapped.RowPitch as usize);
        context.Unmap(&staging, 0);
        result
            .map_err(|e| windows::core::Error::new(windows::core::HRESULT(-1), format!("{e}")))?;
    }
    Ok(())
}

/// Minimal bottom-up 24-bit BMP writer from BGRA source rows.
fn write_bmp(path: &str, w: usize, h: usize, bgra: &[u8], row_pitch: usize) -> std::io::Result<()> {
    let row_bytes = w * 3;
    let padding = (4 - (row_bytes % 4)) % 4;
    let padded_row = row_bytes + padding;
    let pixel_data = padded_row * h;
    let file_size = 54 + pixel_data;

    let mut f = std::io::BufWriter::new(std::fs::File::create(path)?);

    // BITMAPFILEHEADER
    f.write_all(b"BM")?;
    f.write_all(&(file_size as u32).to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&54u32.to_le_bytes())?;
    // BITMAPINFOHEADER
    f.write_all(&40u32.to_le_bytes())?;
    f.write_all(&(w as i32).to_le_bytes())?;
    f.write_all(&(h as i32).to_le_bytes())?;
    f.write_all(&1u16.to_le_bytes())?;
    f.write_all(&24u16.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?; // BI_RGB
    f.write_all(&(pixel_data as u32).to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&2835i32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;
    f.write_all(&0u32.to_le_bytes())?;

    let pad = [0u8; 3];
    // BMP is bottom-up.
    for y in (0..h).rev() {
        let row = &bgra[y * row_pitch..y * row_pitch + w * 4];
        for x in 0..w {
            let b = row[x * 4];
            let g = row[x * 4 + 1];
            let r = row[x * 4 + 2];
            f.write_all(&[b, g, r])?;
        }
        f.write_all(&pad[..padding])?;
    }
    f.flush()
}
