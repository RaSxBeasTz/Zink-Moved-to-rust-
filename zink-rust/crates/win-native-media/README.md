# win-native-media

A native Windows media capture, encode, record, and stream pipeline in Rust — no FFmpeg required.

Captures the screen via **Windows.Graphics.Capture**, encodes to **H.264** via Media Foundation, records to **MP4**, and streams via **RTMP**. Audio capture (WASAPI loopback + microphone) and **AAC** encoding are included. Frames stay on the GPU through the encode stage; no per-frame readbacks.

## Features

- **Screen capture**: Windows Graphics Capture (WGC) with cursor toggle and monitor/window selection.
- **Video encoding**: Hardware-accelerated H.264 (with software fallback) via Media Foundation MFT.
- **Audio capture**: WASAPI system loopback and microphone with AAC encoding.
- **Recording**: MP4 muxing with separate audio tracks (loopback + mic) via IMFSinkWriter.
- **Streaming**: RTMP publish client (hand-rolled, no external RTMP library) with FLV container.
- **Forking**: Single encoded stream to both record and stream consumers with asymmetric drop policy (record lossless, stream drops non-keyframes under backpressure).
- **Public API**: `Pipeline` type hides all Windows COM/Media Foundation details; configurable via `PipelineConfig`.

## Usage

Add to `Cargo.toml`:

```toml
[dependencies]
win-native-media = "0.1"
tokio = { version = "1", features = ["rt", "macros", "sync"] }
```

Minimal example:

```rust
use win_native_media::{Pipeline, PipelineConfig, VideoConfig, RecordConfig, CaptureTarget};

#[tokio::main]
async fn main() {
    let config = PipelineConfig {
        capture_target: CaptureTarget::Monitor(0),
        capture_cursor: true,
        video: VideoConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 6_000_000,
            keyframe_interval: 60,
        },
        record: Some(RecordConfig {
            output_path: "capture.mp4".into(),
        }),
        stream: None,
        audio: None,
    };

    let mut pipeline = Pipeline::new(config).expect("create pipeline");
    pipeline.start().await.expect("start");
    
    // ... running ...
    
    pipeline.stop().await.expect("stop");
}
```

See [examples/](examples/) for runnable demonstrations of each milestone (capture, encode, record, stream, fork, audio, full API).

## Configuration

All settings are `PipelineConfig` fields:

- **`capture_target`**: `Monitor(index)` or `Window(hwnd)`.
- **`capture_cursor`**: Include mouse cursor in frames.
- **Video**: `width`, `height`, `fps`, `bitrate` (bits/sec), `keyframe_interval` (frames).
- **Audio**: Optional `AudioConfig` with `loopback`, `microphone`, `bitrate` flags. Set to `None` for video-only.
- **`record`**: Optional `RecordConfig` with `output_path` for MP4. `None` to disable.
- **`stream`**: Optional `StreamConfig` with RTMP `url` and `stream_key`. `None` to disable.

At least one of `record` or `stream` must be enabled.

See the [API configuration docs](./Api%20Doc.html) for detailed field reference and presets.

## Architecture

```
Capture (WGC) → Encode (MF H.264) → Fork → Record (MP4 muxing)
                                          ↘ Stream (RTMP client)
```

- **Capture** (`capture.rs`): Direct3D11 device, frame pool with resize handling, QPC timestamps.
- **Encode** (`encoder/mf_h264.rs`): Media Foundation H.264 MFT (async event loop for hardware, sync for software); output via Annex-B NAL units.
- **Convert** (`convert.rs`): BGRA→NV12 via D3D11 Video Processor (GPU-resident).
- **Fork** (`fork.rs`): Broadcast encoded samples to record and stream branches; backpressure handling.
- **Mux** (`mux.rs`): IMFSinkWriter MP4 output with optional audio tracks.
- **Stream** (`stream/`): Hand-rolled RTMP client (handshake, AMF0, chunking, FLV tags).
- **Audio** (`audio/`): WASAPI capture + Media Foundation AAC encoder; optional loopback+mic mixing.
- **Pipeline** (`pipeline.rs`): Orchestrator; public API hides COM and MF types.

## Platform Requirements

- **Windows 10+** (Windows 11 recommended).
- **GPU with H.264 encoding support** (Intel Quick Sync, NVIDIA NVENC, AMD VCE) or software fallback.
- **Default audio device** (loopback and/or microphone).

## Known Limitations

- **4K@30 fps** on the software encoder may not sustain; the hardware async MFT zero-copy path (M2b) is in progress for efficiency at 4K.
- **RTMP only**: no RTMPS or other protocols yet.
- **MP4 only for recording**: no WebM, MKV, or other containers in v1.
- **Single RTMP stream**: no multi-bitrate or adaptive streaming yet.

## Testing

Run examples:

```bash
cargo run --example m1_capture_dump      # Dump 10 frames as BMP
cargo run --example m2_encode_h264       # Encode to .h264 file
cargo run --example m3_record_mp4        # Record 120 frames to MP4
cargo run --example m4_stream_rtmp       # Stream to RTMP (mediamtx required)
cargo run --example m5_record_and_stream # Both simultaneously
cargo run --example m6_audio_capture     # Audio capture + AAC encode
cargo run --example m7_pipeline_api      # Full pipeline through public API
```

Run unit tests:

```bash
cargo test --lib
```

## Crate Details

- **Edition**: 2021
- **Dependencies**: `windows` (0.62, with graphics/media features), `tokio` (async runtime), `thiserror` (error handling), `tracing` (instrumentation).
- **No FFmpeg, no external RTMP library**: All encoding, muxing, and streaming are native.

## License

(Add your license here.)

## References

- [Windows Graphics Capture](https://docs.microsoft.com/en-us/windows/win32/direct3ddxgi/desktop-dup-api)
- [Media Foundation](https://docs.microsoft.com/en-us/windows/win32/medfound/microsoft-media-foundation)
- [RTMP Specification](https://en.wikipedia.org/wiki/Real-Time_Messaging_Protocol)
- [FLV Specification](https://www.adobe.io/open/standards/RTMP.html)
