//! Video encoder abstraction.
//!
//! The public surface here deliberately hides Media Foundation types so a v2
//! NVENC/AMF/QSV backend can replace `mf_h264` without touching capture, mux,
//! or stream. Everything crosses this boundary as an `EncodedSample`.

pub mod mf_h264;

use std::time::Duration;

/// One encoded output unit from the video encoder.
///
/// `data` is an H.264 access unit in Annex-B byte-stream format (start-code
/// prefixed NAL units). Recording and streaming both consume this same buffer;
/// neither re-encodes. The stream path converts Annex-B to AVCC length-prefix
/// framing at the FLV boundary.
#[derive(Clone)]
pub struct EncodedSample {
    pub data: Vec<u8>,
    /// Presentation timestamp relative to encode start.
    pub timestamp: Duration,
    pub is_keyframe: bool,
}

/// SPS/PPS parameter sets captured from the encoder at start, in Annex-B form.
/// Needed by the MP4 muxer and the RTMP AVC sequence header.
#[derive(Clone, Default)]
pub struct ParameterSets {
    /// Sequence parameter set NAL (without start code).
    pub sps: Vec<u8>,
    /// Picture parameter set NAL (without start code).
    pub pps: Vec<u8>,
}
