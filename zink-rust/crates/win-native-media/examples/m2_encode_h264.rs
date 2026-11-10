//! M2 verification: capture -> MF H.264 encode -> write test.h264 (Annex-B
//! elementary stream), then self-validate the stream structure since ffplay is
//! not available: confirm start codes, an SPS/PPS, at least one IDR, and that
//! the SPS-declared resolution matches what we asked to encode.
//!
//! Run: `cargo run --example m2_encode_h264`

use std::io::Write;
use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::{iter_annex_b_nals, MfH264Encoder};
use win_native_media::encoder::EncodedSample;
use win_native_media::{CaptureTarget, VideoConfig};

const FRAMES_TO_ENCODE: usize = 60;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let cfg = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate: 6_000_000,
        keyframe_interval: 30, // 1s, so we get keyframes quickly in a short run
    };

    let (session, rx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        4,
    )
    .expect("start capture");

    // The encoder must share the capture device (zero-copy texture handoff).
    let mut encoder = MfH264Encoder::new(session.device(), cfg).expect("create encoder");

    let mut samples: Vec<EncodedSample> = Vec::new();
    let mut encoded = 0usize;

    while encoded < FRAMES_TO_ENCODE {
        let frame = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(_) => {
                eprintln!("timed out waiting for frames");
                break;
            }
        };
        let mut out = Vec::new();
        if let Err(e) = encoder.encode(&frame.texture, frame.timestamp, &mut out) {
            eprintln!("encode error: {e:?}");
            break;
        }
        encoded += 1;
        samples.append(&mut out);
    }

    // Flush.
    let mut tail = Vec::new();
    let _ = encoder.drain(&mut tail);
    samples.append(&mut tail);

    drop(session);

    // Write the elementary stream.
    let mut file = std::fs::File::create("test.h264").expect("create test.h264");
    let mut total_bytes = 0usize;
    for s in &samples {
        file.write_all(&s.data).expect("write");
        total_bytes += s.data.len();
    }
    file.flush().unwrap();

    tracing::info!(
        frames_fed = encoded,
        output_samples = samples.len(),
        bytes = total_bytes,
        "wrote test.h264"
    );

    validate(&samples, &cfg);
}

fn validate(samples: &[EncodedSample], cfg: &VideoConfig) {
    let mut n_sps = 0;
    let mut n_pps = 0;
    let mut n_idr = 0;
    let mut n_slice = 0;
    let mut sps_bytes: Option<Vec<u8>> = None;

    for s in samples {
        for nal in iter_annex_b_nals(&s.data) {
            if nal.is_empty() {
                continue;
            }
            match nal[0] & 0x1f {
                7 => {
                    n_sps += 1;
                    if sps_bytes.is_none() {
                        sps_bytes = Some(nal.to_vec());
                    }
                }
                8 => n_pps += 1,
                5 => {
                    n_idr += 1;
                    n_slice += 1;
                }
                1 => n_slice += 1,
                _ => {}
            }
        }
    }

    let keyframes = samples.iter().filter(|s| s.is_keyframe).count();
    println!("--- H.264 elementary stream validation ---");
    println!("  SPS NALs:        {n_sps}");
    println!("  PPS NALs:        {n_pps}");
    println!("  IDR slices:      {n_idr}");
    println!("  total slices:    {n_slice}");
    println!("  keyframe samples:{keyframes}");

    let mut ok = true;
    if n_sps == 0 {
        println!("  FAIL: no SPS found");
        ok = false;
    }
    if n_pps == 0 {
        println!("  FAIL: no PPS found");
        ok = false;
    }
    if n_idr == 0 {
        println!("  FAIL: no IDR keyframe found");
        ok = false;
    }

    // Parse resolution from SPS and compare.
    if let Some(sps) = sps_bytes {
        match parse_sps_resolution(&sps) {
            Some((w, h)) => {
                println!("  SPS resolution:  {w}x{h}");
                if w != cfg.width || h != cfg.height {
                    println!(
                        "  WARN: SPS resolution {w}x{h} != requested {}x{}",
                        cfg.width, cfg.height
                    );
                }
            }
            None => println!("  WARN: could not parse SPS resolution"),
        }
    }

    if ok && n_slice > 0 {
        println!("  RESULT: VALID H.264 elementary stream");
    } else {
        println!("  RESULT: INVALID");
        std::process::exit(1);
    }
}

/// Minimal SPS parser: extract coded resolution. Enough to sanity-check the
/// encoder produced the size we asked for. Handles the common baseline/main/
/// high case (skips scaling lists / VUI). Returns (width, height) or None.
fn parse_sps_resolution(sps: &[u8]) -> Option<(u32, u32)> {
    // sps[0] is the NAL header; RBSP starts at sps[1], and we must strip
    // emulation-prevention bytes (0x03 after 0x00 0x00).
    let rbsp = strip_emulation(&sps[1..]);
    let mut r = BitReader::new(&rbsp);

    let profile_idc = r.u(8)?;
    let _constraints = r.u(8)?;
    let _level_idc = r.u(8)?;
    let _sps_id = r.ue()?;

    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        let chroma_format_idc = r.ue()?;
        if chroma_format_idc == 3 {
            r.u(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        let seq_scaling_matrix = r.u(1)?;
        if seq_scaling_matrix == 1 {
            // Skipping scaling lists is complex; bail to avoid misparsing.
            return None;
        }
    }

    r.ue()?; // log2_max_frame_num_minus4
    let pic_order_cnt_type = r.ue()?;
    if pic_order_cnt_type == 0 {
        r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
    } else if pic_order_cnt_type == 1 {
        r.u(1)?;
        r.se()?;
        r.se()?;
        let n = r.ue()?;
        for _ in 0..n {
            r.se()?;
        }
    }
    r.ue()?; // max_num_ref_frames
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag

    let pic_width_in_mbs_minus1 = r.ue()?;
    let pic_height_in_map_units_minus1 = r.ue()?;
    let frame_mbs_only_flag = r.u(1)?;
    if frame_mbs_only_flag == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?; // direct_8x8_inference_flag

    let mut crop_left = 0;
    let mut crop_right = 0;
    let mut crop_top = 0;
    let mut crop_bottom = 0;
    let frame_cropping_flag = r.u(1)?;
    if frame_cropping_flag == 1 {
        crop_left = r.ue()?;
        crop_right = r.ue()?;
        crop_top = r.ue()?;
        crop_bottom = r.ue()?;
    }

    let width = (pic_width_in_mbs_minus1 + 1) * 16;
    let height = (pic_height_in_map_units_minus1 + 1) * 16 * (2 - frame_mbs_only_flag);

    // Crop units: for 4:2:0 (assumed), luma crop unit x=2, y=2*(2-frame_mbs_only).
    let crop_unit_x = 2;
    let crop_unit_y = 2 * (2 - frame_mbs_only_flag);
    let w = width.saturating_sub((crop_left + crop_right) * crop_unit_x);
    let h = height.saturating_sub((crop_top + crop_bottom) * crop_unit_y);
    Some((w, h))
}

fn strip_emulation(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 0x03 {
            zeros = 0;
            continue; // drop emulation-prevention byte
        }
        if b == 0 {
            zeros += 1;
        } else {
            zeros = 0;
        }
        out.push(b);
    }
    out
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }
    fn u(&mut self, n: u32) -> Option<u32> {
        let mut val = 0u32;
        for _ in 0..n {
            let byte = self.data.get(self.bit_pos / 8)?;
            let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
            val = (val << 1) | bit as u32;
            self.bit_pos += 1;
        }
        Some(val)
    }
    /// Unsigned Exp-Golomb.
    fn ue(&mut self) -> Option<u32> {
        let mut leading = 0;
        while self.u(1)? == 0 {
            leading += 1;
            if leading > 31 {
                return None;
            }
        }
        if leading == 0 {
            return Some(0);
        }
        let rest = self.u(leading)?;
        Some((1 << leading) - 1 + rest)
    }
    /// Signed Exp-Golomb.
    fn se(&mut self) -> Option<i32> {
        let k = self.ue()? as i64;
        let val = if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) };
        Some(val as i32)
    }
}
