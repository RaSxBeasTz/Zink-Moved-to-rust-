//! M6 (part 1) verification: capture system loopback + mic, encode each to AAC.
//!
//! Run: `cargo run --example m6_audio_capture`
//! Verifies both WASAPI sources deliver PCM and the AAC MFT produces frames with
//! a valid AudioSpecificConfig. Play some audio during the run so loopback isn't
//! silent (silent still works, just produces near-empty AAC).

use std::time::Duration;

use win_native_media::audio::aac::AacEncoder;
use win_native_media::audio::capture;
use win_native_media::audio::EncodedAudio;
use win_native_media::AudioSource;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    for source in [AudioSource::SystemLoopback, AudioSource::Microphone] {
        println!("=== {source:?} ===");
        match run_source(source.clone()) {
            Ok((pcm_buffers, aac_frames, asc, sr, ch)) => {
                println!("  device format:   {sr} Hz, {ch} ch");
                println!("  PCM buffers:     {pcm_buffers}");
                println!("  AAC frames:      {aac_frames}");
                println!("  ASC bytes:       {:02x?}", asc);
                let ok = pcm_buffers > 0 && aac_frames > 0 && asc.len() >= 2;
                println!("  RESULT: {}", if ok { "OK" } else { "FAIL" });
                if !ok {
                    std::process::exit(1);
                }
            }
            Err(e) => {
                println!("  ERROR: {e:?}");
                // Mic may be absent on some machines; only hard-fail loopback.
                if source == AudioSource::SystemLoopback {
                    std::process::exit(1);
                }
            }
        }
    }
}

fn run_source(source: AudioSource) -> win_native_media::Result<(usize, usize, Vec<u8>, u32, u16)> {
    let (cap, rx) = capture::start(source, 32)?;
    let sr = cap.sample_rate;
    let ch = cap.channels;

    let mut encoder = AacEncoder::new(sr, ch, 128_000)?;
    let asc = encoder.audio_specific_config().to_vec();

    let mut pcm_buffers = 0usize;
    let mut out: Vec<EncodedAudio> = Vec::new();

    // Capture ~2 seconds.
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(buf) => {
                pcm_buffers += 1;
                encoder.encode(&buf.data, buf.timestamp, &mut out)?;
            }
            Err(_) => break,
        }
    }
    encoder.drain(&mut out)?;
    let aac_frames = out.len();

    drop(cap);
    Ok((pcm_buffers, aac_frames, asc, sr, ch))
}
