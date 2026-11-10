//! M5 verification: capture -> encode -> FORK to both MP4 recording and RTMP
//! streaming simultaneously, from one encoder (no re-encode on either branch).
//!
//! Prereq: a local RTMP server (mediamtx). Then:
//!   cargo run --example m5_record_and_stream
//! Verify: test_fork.mp4 plays AND mediamtx shows a live H264 publisher, both
//! fed from the same encoder output with no desync.

use std::time::Duration;

use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::MfH264Encoder;
use win_native_media::encoder::EncodedSample;
use win_native_media::fork::Fork;
use win_native_media::mux::Mp4Recorder;
use win_native_media::stream::RtmpPublisher;
use win_native_media::{CaptureTarget, StreamConfig, VideoConfig};

const FRAMES: usize = 300;

fn main() {
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
        keyframe_interval: 30,
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

    // Warm up until SPS/PPS available (both consumers need it to initialize).
    let mut warmup: Vec<EncodedSample> = Vec::new();
    let mut fed = 0usize;
    while encoder.parameter_sets().sps.is_empty() && fed < 60 {
        if let Ok(frame) = rx.recv_timeout(Duration::from_secs(5)) {
            let mut out = Vec::new();
            encoder
                .encode(&frame.texture, frame.timestamp, &mut out)
                .expect("encode");
            fed += 1;
            warmup.append(&mut out);
        } else {
            break;
        }
    }
    let params = encoder.parameter_sets().clone();
    if params.sps.is_empty() {
        eprintln!("no SPS; abort");
        std::process::exit(1);
    }

    // Build the fork: both branches on, bounded at 120 samples (~4s).
    let (mut fork, receivers) = Fork::new(true, true, 120);

    // --- Record consumer: dedicated std thread, lossless. ---
    let record_rx = receivers.record.unwrap();
    let rec_cfg = cfg;
    let rec_params = params.clone();
    let record_thread = std::thread::spawn(move || {
        let mut recorder =
            Mp4Recorder::new("test_fork.mp4", &rec_cfg, &rec_params).expect("recorder");
        let mut n = 0u64;
        for sample in record_rx.iter() {
            recorder.write(&sample).expect("write");
            n += 1;
        }
        recorder.finalize().expect("finalize");
        n
    });

    // --- Stream consumer: its own current-thread tokio runtime on a thread. ---
    let stream_rx = receivers.stream.unwrap();
    let str_cfg = cfg;
    let str_params = params.clone();
    let stream_thread = std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("rt");
        rt.block_on(async move {
            let mut pubr = RtmpPublisher::connect(
                StreamConfig {
                    url,
                    stream_key: key,
                },
                str_cfg,
                str_params,
            )
            .await
            .expect("rtmp connect");
            let mut n = 0u64;
            for sample in stream_rx.iter() {
                if let Err(e) = pubr.send_video(&sample).await {
                    tracing::warn!("send failed: {e:?}; reconnecting");
                    if pubr.reconnect().await.is_err() {
                        break;
                    }
                } else {
                    n += 1;
                }
            }
            n
        })
    });

    // Push warmup samples into the fork, then continue capturing/encoding.
    for s in warmup {
        fork.distribute(s);
    }
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
        for s in out {
            if !fork.distribute(s) {
                eprintln!("all consumers gone");
                break;
            }
        }
    }

    // Drain the encoder, then drop the fork so both consumers see EOF.
    let mut tail = Vec::new();
    let _ = encoder.drain(&mut tail);
    for s in tail {
        fork.distribute(s);
    }
    let dropped = fork.dropped_stream_frames();
    drop(fork); // closes both channels -> consumers finish their iter()
    drop(session);

    let recorded = record_thread.join().expect("record join");
    let streamed = stream_thread.join().expect("stream join");

    let mp4_len = std::fs::metadata("test_fork.mp4")
        .map(|m| m.len())
        .unwrap_or(0);

    println!("--- M5 fork result ---");
    println!("  frames encoded:     {fed}");
    println!("  recorded (MP4):     {recorded}");
    println!("  streamed (RTMP):    {streamed}");
    println!("  stream drops:       {dropped}");
    println!("  test_fork.mp4:      {mp4_len} bytes");

    // Recording must be lossless: it should receive every produced sample.
    // Streaming may be <= recorded if frames were dropped under backpressure.
    if recorded > 0 && streamed > 0 && mp4_len > 0 && streamed <= recorded {
        println!("  RESULT: OK (both branches fed from one encoder)");
    } else {
        println!("  RESULT: FAIL");
        std::process::exit(1);
    }
}
