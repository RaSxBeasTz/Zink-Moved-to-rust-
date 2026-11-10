//! M3 verification: capture -> encode -> record to test.mp4.
//!
//! Run: `cargo run --example m3_record_mp4`
//! Verify: play test.mp4 in Windows Media Player / VLC. The example also prints
//! the file size and, if the muxer wrote a moov atom, a basic structural check.

use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::MfH264Encoder;
use win_native_media::encoder::EncodedSample;
use win_native_media::mux::Mp4Recorder;
use win_native_media::{CaptureTarget, VideoConfig};

const FRAMES: usize = 120; // ~4s at 30fps

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
        keyframe_interval: 30,
    };

    let (session, rx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        4,
    )
    .expect("start capture");

    let mut encoder = MfH264Encoder::new(session.device(), cfg).expect("encoder");

    // Encode all frames first, buffering samples, so we have SPS/PPS (captured
    // at the first keyframe) before building the recorder's media type.
    let mut samples: Vec<EncodedSample> = Vec::new();
    let mut fed = 0usize;
    while fed < FRAMES {
        let frame = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(_) => break,
        };
        let mut out = Vec::new();
        if let Err(e) = encoder.encode(&frame.texture, frame.timestamp, &mut out) {
            eprintln!("encode error: {e:?}");
            break;
        }
        fed += 1;
        samples.append(&mut out);
    }
    let mut tail = Vec::new();
    let _ = encoder.drain(&mut tail);
    samples.append(&mut tail);

    let params = encoder.parameter_sets().clone();
    drop(session);

    if params.sps.is_empty() {
        eprintln!("no SPS captured; cannot mux");
        std::process::exit(1);
    }

    let mut recorder = Mp4Recorder::new("test.mp4", &cfg, &params).expect("recorder");
    for s in &samples {
        recorder.write(s).expect("write sample");
    }
    recorder.finalize().expect("finalize");

    let meta = std::fs::metadata("test.mp4").expect("stat");
    tracing::info!(
        samples = samples.len(),
        bytes = meta.len(),
        "wrote test.mp4"
    );

    validate_mp4("test.mp4");
}

/// Structural sanity check without ffmpeg: confirm the file has the MP4 box
/// signature (ftyp) and a moov atom (required for a playable, finalized file).
fn validate_mp4(path: &str) {
    let data = std::fs::read(path).expect("read mp4");
    println!("--- MP4 structural validation ---");
    println!("  size: {} bytes", data.len());

    let has_ftyp = find_box(&data, b"ftyp").is_some();
    let has_moov = find_box(&data, b"moov").is_some();
    let has_mdat = find_box(&data, b"mdat").is_some();
    println!("  ftyp box: {has_ftyp}");
    println!("  moov box: {has_moov}");
    println!("  mdat box: {has_mdat}");

    if has_ftyp && has_moov && has_mdat {
        println!("  RESULT: VALID finalized MP4");
    } else {
        println!("  RESULT: INVALID (missing required boxes)");
        std::process::exit(1);
    }
}

/// Scan top-level boxes for a 4CC. MP4 boxes are [u32 size][4CC type][...].
fn find_box(data: &[u8], fourcc: &[u8; 4]) -> Option<usize> {
    let mut pos = 0;
    while pos + 8 <= data.len() {
        let size =
            u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        let typ = &data[pos + 4..pos + 8];
        if typ == fourcc {
            return Some(pos);
        }
        if size < 8 {
            // 0 = extends to EOF, 1 = 64-bit size; stop scanning conservatively.
            break;
        }
        pos += size;
    }
    None
}
