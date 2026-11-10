//! M4 verification: capture -> encode -> stream to a local RTMP server.
//!
//! Prereq: run a local RTMP server, e.g. `mediamtx`. Then:
//!   cargo run --example m4_stream_rtmp
//! and view with: ffplay rtmp://localhost/live/test  (or check mediamtx logs /
//! its API confirming a publisher connected and pushed frames).
//!
//! URL/key default to rtmp://localhost:1935/live + "test"; override via args:
//!   cargo run --example m4_stream_rtmp -- rtmp://host:1935/app streamkey

use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::MfH264Encoder;
use win_native_media::encoder::EncodedSample;
use win_native_media::stream::RtmpPublisher;
use win_native_media::{CaptureTarget, StreamConfig, VideoConfig};

const FRAMES: usize = 300; // ~10s of capture

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "rtmp://localhost:1935/live".into());
    let key = args.next().unwrap_or_else(|| "test".into());

    let cfg = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate: 4_000_000,
        keyframe_interval: 30, // 1s GOP: quick keyframes for late joiners
    };

    let (session, rx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        8,
    )
    .expect("start capture");

    let mut encoder = MfH264Encoder::new(session.device(), cfg).expect("encoder");

    // Warm up the encoder until we have SPS/PPS (needed before RTMP connect).
    let mut pending: Vec<EncodedSample> = Vec::new();
    let mut fed = 0usize;
    while encoder.parameter_sets().sps.is_empty() && fed < 60 {
        if let Ok(frame) = rx.recv_timeout(Duration::from_secs(5)) {
            let mut out = Vec::new();
            encoder
                .encode(&frame.texture, frame.timestamp, &mut out)
                .expect("encode");
            fed += 1;
            pending.append(&mut out);
        } else {
            break;
        }
    }

    let params = encoder.parameter_sets().clone();
    if params.sps.is_empty() {
        eprintln!("no SPS captured; cannot stream");
        std::process::exit(1);
    }

    let mut pubr = RtmpPublisher::connect(
        StreamConfig {
            url: url.clone(),
            stream_key: key.clone(),
        },
        cfg,
        params,
    )
    .await
    .expect("rtmp connect");
    tracing::info!("connected to {url}/{key}, publishing");

    // Send the frames we already encoded, then keep going.
    for s in &pending {
        if let Err(e) = pubr.send_video(s).await {
            eprintln!("send error: {e:?}");
            let _ = pubr.reconnect().await;
        }
    }

    let mut sent = pending.len();
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
        for s in &out {
            match pubr.send_video(s).await {
                Ok(()) => sent += 1,
                Err(e) => {
                    tracing::warn!("send failed: {e:?}; reconnecting");
                    if pubr.reconnect().await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    drop(session);
    tracing::info!(frames_sent = sent, "done streaming");
    println!("RESULT: streamed {sent} video messages to {url}/{key}");
}
