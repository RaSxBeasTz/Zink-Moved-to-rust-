//! Audio subsystem: WASAPI capture + Media Foundation AAC encoding.
//!
//! Loopback (system) and microphone are captured as independent PCM streams and
//! encoded to independent AAC tracks (per the two-separate-tracks design).

pub mod aac;
pub mod capture;
pub mod endpoints;
pub mod mix;
pub mod visualizer;

use std::time::Duration;

/// One encoded AAC access unit.
#[derive(Clone)]
pub struct EncodedAudio {
    /// Raw AAC (AudioSpecificConfig framing is carried separately in
    /// `AacEncoder::audio_specific_config`).
    pub data: Vec<u8>,
    pub timestamp: Duration,
}
