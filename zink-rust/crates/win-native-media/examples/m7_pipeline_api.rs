//! M7 verification: drive the whole thing through the public `Pipeline` API,
//! exactly as the Tauri backend would. Records + streams for a few seconds via
//! start()/stop(), then checks the MP4 and the is_recording/is_streaming flags.
//!
//! Prereq for the stream branch: a local RTMP server (mediamtx). Run:
//!   cargo run --example m7_pipeline_api
//! Override URL/key: cargo run --example m7_pipeline_api -- rtmp://host/app key
//! Pass "norstream" as the first arg to test record-only.

use std::time::Duration;

use win_native_media::{
    CaptureTarget, Pipeline, PipelineConfig, RecordConfig, StreamConfig, VideoConfig,
};

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_default();
    let record_only = first == "norstream";
    let url = if record_only {
        String::new()
    } else if first.is_empty() {
        "rtmp://localhost:1935/live".into()
    } else {
        first
    };
    let key = args.next().unwrap_or_else(|| "test".into());

    let config = PipelineConfig {
        capture_target: CaptureTarget::Monitor(0),
        capture_cursor: true,
        video: VideoConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 4_000_000,
            keyframe_interval: 30,
        },
        record: Some(RecordConfig {
            output_path: "test_pipeline.mp4".into(),
        }),
        stream: if record_only {
            None
        } else {
            Some(StreamConfig {
                url: url.clone(),
                stream_key: key.clone(),
            })
        },
        audio: Some(win_native_media::AudioConfig::default()),
    };

    let mut pipeline = Pipeline::new(config).expect("new pipeline");

    println!("starting pipeline...");
    pipeline.start().await.expect("start");
    println!(
        "  is_recording={} is_streaming={}",
        pipeline.is_recording(),
        pipeline.is_streaming()
    );

    // Let it run.
    tokio::time::sleep(Duration::from_secs(8)).await;

    println!("stopping pipeline...");
    pipeline.stop().await.expect("stop");
    println!(
        "  is_recording={} is_streaming={}",
        pipeline.is_recording(),
        pipeline.is_streaming()
    );

    let mp4 = std::fs::metadata("test_pipeline.mp4")
        .map(|m| m.len())
        .unwrap_or(0);
    let data = std::fs::read("test_pipeline.mp4").unwrap_or_default();
    let has_moov = data.windows(4).any(|w| w == b"moov");

    println!("--- M7 result ---");
    println!("  test_pipeline.mp4: {mp4} bytes (moov: {has_moov})");
    if mp4 > 0 && has_moov {
        println!("  RESULT: OK (pipeline API produced a finalized MP4)");
    } else {
        println!("  RESULT: FAIL");
        std::process::exit(1);
    }
}
