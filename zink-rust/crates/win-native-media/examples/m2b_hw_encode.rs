//! M2b: force the HARDWARE H.264 encoder (zero-copy NV12 texture path) and
//! report exactly what happens. Diagnostic for the hardware encode work.
//!
//! Run: `cargo run --example m2b_hw_encode`

use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::MfH264Encoder;
use win_native_media::encoder::EncodedSample;
use win_native_media::{CaptureTarget, VideoConfig};

fn main() {
    tracing_subscriber::fmt().with_env_filter("info").init();

    let cfg = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate: 8_000_000,
        keyframe_interval: 30,
    };

    let (session, rx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        4,
    )
    .expect("capture");

    // Force hardware; do NOT fall back, so we see the real error.
    let mut encoder = match MfH264Encoder::new_with(session.device(), cfg, true) {
        Ok(e) => {
            println!("hardware encoder created OK");
            e
        }
        Err(e) => {
            println!("hardware encoder creation FAILED: {e:?}");
            std::process::exit(1);
        }
    };

    let mut samples: Vec<EncodedSample> = Vec::new();
    let mut fed = 0;
    while fed < 60 {
        let frame = match rx.recv_timeout(Duration::from_secs(5)) {
            Ok(f) => f,
            Err(_) => break,
        };
        let mut out = Vec::new();
        match encoder.encode(&frame.texture, frame.timestamp, &mut out) {
            Ok(()) => {}
            Err(e) => {
                println!("encode FAILED at frame {fed}: {e:?}");
                std::process::exit(1);
            }
        }
        fed += 1;
        samples.append(&mut out);
    }
    let mut tail = Vec::new();
    let _ = encoder.drain(&mut tail);
    samples.append(&mut tail);
    drop(session);

    let bytes: usize = samples.iter().map(|s| s.data.len()).sum();
    let keyframes = samples.iter().filter(|s| s.is_keyframe).count();
    println!("--- M2b hardware encode result ---");
    println!("  frames fed:      {fed}");
    println!("  output samples:  {}", samples.len());
    println!("  total bytes:     {bytes}");
    println!("  keyframes:       {keyframes}");
    println!(
        "  RESULT: {}",
        if !samples.is_empty() && bytes > 0 {
            "OK (hardware zero-copy encode works)"
        } else {
            "FAIL"
        }
    );
}
