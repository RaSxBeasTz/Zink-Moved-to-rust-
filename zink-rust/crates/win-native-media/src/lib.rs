//! Native Windows capture/encode/record/stream pipeline.
//!
//! The [`Pipeline`] type is the entry point: configure it with a
//! [`PipelineConfig`] and call `start`/`stop`. It captures the screen (WGC),
//! encodes H.264 (Media Foundation), and forks the encoded stream to an MP4
//! recorder and/or an RTMP publisher without re-encoding. No FFmpeg.

pub mod audio;
pub mod capture;
pub mod convert;
pub mod encoder;
pub mod fork;
mod hdr_convert;
pub mod mux;
pub mod pipeline;
pub mod replay;
pub mod stream;

pub use audio::endpoints::{capture_endpoints, render_endpoints, AudioEndpoint};
pub use hdr_convert::HdrToSdr;
pub use audio::visualizer::{
    VisualizerAnalyzer, VisualizerFrame, BAND_COUNT as VISUALIZER_BAND_COUNT,
};
pub use pipeline::{AudioConfig, Pipeline, PipelineConfig, PipelineStats, RecordConfig};
pub use replay::{ReplayBuffer, ReplayConfig};

use std::time::Duration;

/// A captured GPU frame handed out by the capture module.
///
/// Holds the D3D11 texture (kept on GPU) plus timing. `timestamp` is relative
/// to capture start, derived from the frame's QPC system-relative time.
pub struct CapturedFrame {
    // Keep the frame pool slot leased until the texture has been consumed.
    pub(crate) _capture_frame: windows::Graphics::Capture::Direct3D11CaptureFrame,
    /// The BGRA8 D3D11 texture for this frame.
    pub texture: windows::Win32::Graphics::Direct3D11::ID3D11Texture2D,
    /// Time since capture started.
    pub timestamp: Duration,
    /// Frame content width in pixels.
    pub width: u32,
    /// Frame content height in pixels.
    pub height: u32,
}

// The capture device is created multithread-protected (D3D11 immediate context
// serialized internally), so a texture handed off to another thread is safe to
// use there. The `windows` bindings mark ID3D11Texture2D `!Send` conservatively;
// we move frames from the WGC pool thread to the consumer, so assert Send.
// ponytail: relies on the multithread-protected device set in capture::create_d3d_device.
unsafe impl Send for CapturedFrame {}

/// What to capture.
#[derive(Clone, Copy, Debug)]
pub enum CaptureTarget {
    /// Monitor by index into the system's monitor enumeration.
    Monitor(usize),
    /// A specific top-level window by HWND (as isize).
    Window(isize),
}

#[derive(thiserror::Error, Debug)]
pub enum PipelineError {
    #[error("windows API error: {0}")]
    Windows(#[from] windows::core::Error),
    #[error("no monitor at index {0}")]
    MonitorNotFound(usize),
    #[error("capture is not supported on this system")]
    CaptureUnsupported,
    #[error("capture channel closed")]
    Stopped,
    #[error("no hardware or software H.264 encoder MFT found")]
    NoEncoderFound,
    #[error("encoder input/output type negotiation failed: {0}")]
    TypeNegotiation(String),
    #[error("rtmp io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("rtmp protocol error: {0}")]
    Rtmp(String),
    #[error("audio device error: {0}")]
    Audio(String),
}

/// Which audio source to capture.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AudioSource {
    /// System/desktop audio via WASAPI loopback on the default render device.
    SystemLoopback,
    /// Default capture device (microphone).
    Microphone,
    /// System/desktop loopback from a specific render endpoint ID.
    SystemLoopbackDevice(String),
    /// Capture from a specific microphone endpoint ID.
    MicrophoneDevice(String),
}

/// A buffer of captured PCM audio with a timestamp.
///
/// `data` is interleaved 16-bit signed PCM at `sample_rate`/`channels`, ready
/// for the AAC encoder. `timestamp` is relative to capture start.
pub struct AudioBuffer {
    pub data: Vec<u8>,
    pub timestamp: Duration,
    pub sample_rate: u32,
    pub channels: u16,
}

/// Config for RTMP streaming.
#[derive(Clone, Debug)]
pub struct StreamConfig {
    /// Full RTMP URL, e.g. rtmp://host:1935/app  (the part before the stream key).
    pub url: String,
    /// Stream key / name to publish (the path after the app).
    pub stream_key: String,
}

/// Video encoding parameters.
#[derive(Clone, Copy, Debug)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Average target bitrate in bits/sec.
    pub bitrate: u32,
    /// Keyframe (GOP) interval in frames. Keep short for RTMP (e.g. 2*fps).
    pub keyframe_interval: u32,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 6_000_000,
            keyframe_interval: 60, // 2s at 30fps
        }
    }
}

pub type Result<T> = std::result::Result<T, PipelineError>;
