//! Lightweight real-time spectrum analysis fed by the Windows system-loopback
//! capture.  The analyser owns one bounded WASAPI stream and publishes only the
//! newest frame, so leaving the Visualizer open cannot grow application memory.

use crate::{audio::capture, AudioSource, Result};
use std::sync::{Arc, Mutex};

pub const BAND_COUNT: usize = 24;

#[derive(Clone, Copy, Debug)]
pub struct VisualizerFrame {
    pub peak: f32,
    pub waveform: [f32; BAND_COUNT],
    pub spectrum: [f32; BAND_COUNT],
}

impl Default for VisualizerFrame {
    fn default() -> Self {
        Self {
            peak: 0.0,
            waveform: [0.0; BAND_COUNT],
            spectrum: [0.0; BAND_COUNT],
        }
    }
}

pub struct VisualizerAnalyzer {
    _capture: capture::AudioCapture,
    latest: Arc<Mutex<VisualizerFrame>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl VisualizerAnalyzer {
    pub fn start() -> Result<Self> {
        let (capture, receiver) = capture::start(AudioSource::SystemLoopback, 2)?;
        let latest = Arc::new(Mutex::new(VisualizerFrame::default()));
        let latest_worker = Arc::clone(&latest);
        let channels = usize::from(capture.channels.max(1));
        let sample_rate = capture.sample_rate;
        let worker = std::thread::spawn(move || {
            while let Ok(buffer) = receiver.recv() {
                let frame = analyse_pcm16(&buffer.data, channels, sample_rate);
                if let Ok(mut destination) = latest_worker.lock() {
                    *destination = frame;
                }
            }
        });
        Ok(Self {
            _capture: capture,
            latest,
            worker: Some(worker),
        })
    }

    #[must_use]
    pub fn latest(&self) -> VisualizerFrame {
        self.latest
            .lock()
            .map_or_else(|_| VisualizerFrame::default(), |value| *value)
    }
}

impl Drop for VisualizerAnalyzer {
    fn drop(&mut self) {
        // AudioCapture is dropped after this method. Its channel naturally
        // disconnects once capture stops; detaching here avoids blocking the UI
        // thread during Windows endpoint shutdown.
        let _ = self.worker.take();
    }
}

fn analyse_pcm16(data: &[u8], channels: usize, sample_rate: u32) -> VisualizerFrame {
    let interleaved: Vec<i16> = data
        .chunks_exact(2)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]))
        .collect();
    let mono: Vec<f32> = interleaved
        .chunks(channels)
        .map(|frame| {
            frame
                .iter()
                .map(|sample| f32::from(*sample) / 32768.0)
                .sum::<f32>()
                / frame.len().max(1) as f32
        })
        .collect();
    if mono.is_empty() {
        return VisualizerFrame::default();
    }

    let peak = mono
        .iter()
        .fold(0.0_f32, |value, sample| value.max(sample.abs()));
    let mut waveform = [0.0; BAND_COUNT];
    for (index, destination) in waveform.iter_mut().enumerate() {
        let source = index.saturating_mul(mono.len().saturating_sub(1)) / (BAND_COUNT - 1);
        *destination = mono[source];
    }

    // Log-spaced musical buckets. A direct DFT over 24 bins is considerably
    // cheaper than a general FFT at this update rate and avoids another native
    // dependency while preserving the actual frequency shape.
    let window_len = mono.len().min(2048);
    let samples = &mono[mono.len() - window_len..];
    let mut spectrum = [0.0; BAND_COUNT];
    let nyquist = sample_rate as f32 / 2.0;
    for (index, destination) in spectrum.iter_mut().enumerate() {
        let ratio = index as f32 / (BAND_COUNT - 1) as f32;
        let frequency = (45.0 * (16_000.0_f32 / 45.0).powf(ratio)).min(nyquist * 0.95);
        let step = std::f32::consts::TAU * frequency / sample_rate.max(1) as f32;
        let mut real = 0.0;
        let mut imaginary = 0.0;
        for (sample_index, sample) in samples.iter().enumerate() {
            let hann = 0.5
                - 0.5
                    * (std::f32::consts::TAU * sample_index as f32
                        / window_len.saturating_sub(1).max(1) as f32)
                        .cos();
            let phase = step * sample_index as f32;
            real += sample * hann * phase.cos();
            imaginary -= sample * hann * phase.sin();
        }
        let magnitude =
            (real.mul_add(real, imaginary * imaginary)).sqrt() / window_len.max(1) as f32;
        *destination = (magnitude * 12.0).sqrt().clamp(0.0, 1.0);
    }
    VisualizerFrame {
        peak,
        waveform,
        spectrum,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn analyser_finds_tone_near_its_log_bucket() {
        let sample_rate = 48_000_u32;
        let samples = 2_048;
        let mut pcm = Vec::with_capacity(samples * 4);
        for index in 0..samples {
            let value = (std::f32::consts::TAU * 1_000.0 * index as f32 / sample_rate as f32).sin();
            let sample = (value * 20_000.0) as i16;
            pcm.extend_from_slice(&sample.to_le_bytes());
            pcm.extend_from_slice(&sample.to_le_bytes());
        }
        let frame = analyse_pcm16(&pcm, 2, sample_rate);
        let strongest = frame
            .spectrum
            .iter()
            .enumerate()
            .max_by(|left, right| left.1.total_cmp(right.1))
            .map(|(index, _)| index)
            .unwrap();
        assert!((12..=15).contains(&strongest));
        assert!(frame.peak > 0.5);
    }
}
