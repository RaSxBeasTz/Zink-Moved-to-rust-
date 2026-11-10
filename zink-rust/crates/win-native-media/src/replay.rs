//! Native rolling replay buffer built on WGC, Media Foundation and WASAPI.
//! Encoded H.264/AAC packets are retained by time and muxed without re-encoding.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::audio::aac::AacEncoder;
use crate::audio::{capture as acap, EncodedAudio};
use crate::capture::{self, CaptureConfig};
use crate::encoder::mf_h264::MfH264Encoder;
use crate::encoder::{EncodedSample, ParameterSets};
use crate::mux::{AudioTrack, Mp4Recorder};
use crate::{AudioSource, CaptureTarget, PipelineError, Result, VideoConfig};

const WARMUP_FRAMES: usize = 60;

#[derive(Clone, Debug)]
pub struct ReplayConfig {
    pub capture_target: CaptureTarget,
    pub capture_cursor: bool,
    pub video: VideoConfig,
    pub duration: Duration,
    pub system_audio: bool,
    pub microphone: bool,
    pub system_audio_device_id: Option<String>,
    pub microphone_device_id: Option<String>,
    pub separate_audio_tracks: bool,
    pub audio_bitrate: u32,
}

#[derive(Default)]
struct ReplayState {
    video: VecDeque<EncodedSample>,
    audio: Vec<VecDeque<EncodedAudio>>,
    audio_tracks: Vec<AudioTrack>,
    parameters: ParameterSets,
    video_config: Option<VideoConfig>,
}

struct AudioWorker {
    _capture: acap::AudioCapture,
    receiver: std::sync::mpsc::Receiver<crate::AudioBuffer>,
    encoder: AacEncoder,
}

pub struct ReplayBuffer {
    running: Arc<AtomicBool>,
    state: Arc<Mutex<ReplayState>>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}

impl ReplayBuffer {
    pub fn start(config: ReplayConfig) -> Result<Self> {
        let running = Arc::new(AtomicBool::new(true));
        let state = Arc::new(Mutex::new(ReplayState::default()));
        let worker_running = Arc::clone(&running);
        let worker_state = Arc::clone(&state);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            run_replay_worker(config, worker_running, worker_state, ready_tx)
        });
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                running,
                state,
                worker: Some(worker),
            }),
            Ok(Err(error)) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(error)
            }
            Err(_) => {
                running.store(false, Ordering::SeqCst);
                let _ = worker.join();
                Err(PipelineError::Stopped)
            }
        }
    }

    #[must_use]
    pub fn buffered_duration(&self) -> Duration {
        self.state
            .lock()
            .ok()
            .and_then(|state| {
                Some(
                    state
                        .video
                        .back()?
                        .timestamp
                        .saturating_sub(state.video.front()?.timestamp),
                )
            })
            .unwrap_or_default()
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<Duration> {
        self.save_duration(path, None)
    }

    /// Saves only the requested tail of the retained replay when a duration is
    /// supplied. The start is advanced to the first keyframe at or after the
    /// requested boundary so the resulting MP4 remains independently decodable.
    pub fn save_duration(
        &self,
        path: impl AsRef<Path>,
        duration: Option<Duration>,
    ) -> Result<Duration> {
        let (mut video, audio, tracks, parameters, config) = {
            let state = self
                .state
                .lock()
                .map_err(|error| PipelineError::Audio(error.to_string()))?;
            (
                state.video.iter().cloned().collect::<Vec<_>>(),
                state
                    .audio
                    .iter()
                    .map(|queue| queue.iter().cloned().collect::<Vec<_>>())
                    .collect::<Vec<_>>(),
                state.audio_tracks.clone(),
                state.parameters.clone(),
                state.video_config.ok_or(PipelineError::Stopped)?,
            )
        };
        if video.is_empty() {
            return Err(PipelineError::Audio(
                "the replay buffer is still filling".into(),
            ));
        }
        if let Some(duration) = duration {
            trim_video_to_duration(&mut video, duration);
        }
        let first_keyframe = video
            .iter()
            .position(|sample| sample.is_keyframe)
            .unwrap_or(0);
        video.drain(..first_keyframe);
        let start = video
            .first()
            .map_or(Duration::ZERO, |sample| sample.timestamp);
        let end = video.last().map_or(start, |sample| sample.timestamp);
        for sample in &mut video {
            sample.timestamp = sample.timestamp.saturating_sub(start);
        }

        enum Packet {
            Video(EncodedSample),
            Audio(usize, EncodedAudio),
        }
        impl Packet {
            fn timestamp(&self) -> Duration {
                match self {
                    Self::Video(sample) => sample.timestamp,
                    Self::Audio(_, sample) => sample.timestamp,
                }
            }
        }

        let mut packets = video.into_iter().map(Packet::Video).collect::<Vec<_>>();
        for (track, samples) in audio.into_iter().enumerate() {
            packets.extend(samples.into_iter().filter_map(|mut sample| {
                if sample.timestamp < start || sample.timestamp > end {
                    return None;
                }
                sample.timestamp = sample.timestamp.saturating_sub(start);
                Some(Packet::Audio(track, sample))
            }));
        }
        packets.sort_by_key(Packet::timestamp);

        let mut recorder = Mp4Recorder::with_audio(path, &config, &parameters, &tracks)?;
        for packet in packets {
            match packet {
                Packet::Video(sample) => recorder.write(&sample)?,
                Packet::Audio(track, sample) => {
                    recorder.write_audio(track, &sample.data, sample.timestamp)?;
                }
            }
        }
        recorder.finalize()?;
        Ok(end.saturating_sub(start))
    }

    pub fn stop(&mut self) -> Result<()> {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| PipelineError::Audio("replay worker panicked".into()))??;
        }
        Ok(())
    }
}

fn trim_video_to_duration(video: &mut Vec<EncodedSample>, duration: Duration) {
    let Some(end) = video.last().map(|sample| sample.timestamp) else {
        return;
    };
    let boundary = end.saturating_sub(duration);
    if let Some(start) = video
        .iter()
        .position(|sample| sample.timestamp >= boundary && sample.is_keyframe)
    {
        video.drain(..start);
    }
}

impl Drop for ReplayBuffer {
    fn drop(&mut self) {
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run_replay_worker(
    config: ReplayConfig,
    running: Arc<AtomicBool>,
    state: Arc<Mutex<ReplayState>>,
    ready: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    let initialized = (|| -> Result<_> {
        let (session, receiver) = capture::start(
            CaptureConfig {
                target: config.capture_target,
                capture_cursor: config.capture_cursor,
            },
            8,
        )?;
        let mut using_hardware = true;
        let mut encoder = MfH264Encoder::new_with(session.device(), config.video, true)
            .or_else(|hardware_error| {
                using_hardware = false;
                tracing::warn!(%hardware_error, "hardware replay encoder unavailable; using Media Foundation software encoder");
                MfH264Encoder::new(session.device(), config.video)
            })?;
        let mut warmup = Vec::new();
        for _ in 0..WARMUP_FRAMES {
            if !encoder.parameter_sets().sps.is_empty() {
                break;
            }
            let frame = receiver
                .recv_timeout(Duration::from_secs(5))
                .map_err(|_| PipelineError::Stopped)?;
            if let Err(hardware_error) =
                encoder.encode(&frame.texture, frame.timestamp, &mut warmup)
            {
                if !using_hardware {
                    return Err(hardware_error);
                }
                tracing::warn!(%hardware_error, "hardware replay encoder rejected input; switching to Media Foundation software encoder");
                encoder = MfH264Encoder::new(session.device(), config.video)?;
                using_hardware = false;
                warmup.clear();
                encoder.encode(&frame.texture, frame.timestamp, &mut warmup)?;
            }
        }
        if encoder.parameter_sets().sps.is_empty() {
            return Err(PipelineError::TypeNegotiation(
                "no SPS/PPS produced while starting replay".into(),
            ));
        }
        Ok((session, receiver, encoder, warmup))
    })();
    let (session, receiver, mut encoder, warmup) = match initialized {
        Ok(value) => value,
        Err(error) => {
            let _ = ready.send(Err(error));
            return Ok(());
        }
    };

    let mut audio_workers = Vec::new();
    let system_source = config.system_audio_device_id.clone().map_or(
        AudioSource::SystemLoopback,
        AudioSource::SystemLoopbackDevice,
    );
    let microphone_source = config
        .microphone_device_id
        .clone()
        .map_or(AudioSource::Microphone, AudioSource::MicrophoneDevice);
    for (enabled, source) in [
        (config.system_audio, system_source),
        (config.microphone, microphone_source),
    ] {
        if !enabled {
            continue;
        }
        if let Ok((capture, receiver)) = acap::start(source, 64) {
            let encoder =
                AacEncoder::new(capture.sample_rate, capture.channels, config.audio_bitrate)?;
            audio_workers.push(AudioWorker {
                _capture: capture,
                receiver,
                encoder,
            });
        }
    }
    if let Ok(mut state) = state.lock() {
        state.parameters = encoder.parameter_sets().clone();
        state.video_config = Some(config.video);
        let mut audio_tracks = audio_workers
            .iter()
            .map(|worker| AudioTrack {
                sample_rate: worker._capture.sample_rate,
                channels: worker._capture.channels,
                asc: worker.encoder.audio_specific_config().to_vec(),
                bitrate: config.audio_bitrate,
            })
            .collect::<Vec<_>>();
        if !config.separate_audio_tracks {
            audio_tracks.truncate(1);
        }
        state.audio_tracks = audio_tracks;
        state.audio = (0..state.audio_tracks.len())
            .map(|_| VecDeque::new())
            .collect();
        state.video.extend(warmup);
    }
    let _ = ready.send(Ok(()));

    while running.load(Ordering::SeqCst) {
        match receiver.recv_timeout(Duration::from_millis(5)) {
            Ok(frame) => {
                let mut encoded = Vec::new();
                encoder.encode(&frame.texture, frame.timestamp, &mut encoded)?;
                if let Ok(mut state) = state.lock() {
                    state.video.extend(encoded);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        if let Ok(mut state) = state.lock() {
            if config.separate_audio_tracks {
                for (index, worker) in audio_workers.iter_mut().enumerate() {
                    while let Ok(buffer) = worker.receiver.try_recv() {
                        let mut encoded = Vec::new();
                        worker
                            .encoder
                            .encode(&buffer.data, buffer.timestamp, &mut encoded)?;
                        state.audio[index].extend(encoded);
                    }
                }
            } else if let Some((primary, rest)) = audio_workers.split_first_mut() {
                while let Ok(buffer) = primary.receiver.try_recv() {
                    let mixed = rest
                        .first()
                        .and_then(|secondary| secondary.receiver.try_recv().ok())
                        .map_or_else(
                            || buffer.data.clone(),
                            |secondary| crate::audio::mix::mix_pcm16(&buffer.data, &secondary.data),
                        );
                    let mut encoded = Vec::new();
                    primary
                        .encoder
                        .encode(&mixed, buffer.timestamp, &mut encoded)?;
                    state.audio[0].extend(encoded);
                }
            }
            prune_state(&mut state, config.duration);
        }
    }
    drop(session);
    Ok(())
}

fn prune_state(state: &mut ReplayState, duration: Duration) {
    let Some(latest) = state.video.back().map(|sample| sample.timestamp) else {
        return;
    };
    let cutoff = latest.saturating_sub(duration);
    while state.video.len() > 1
        && state
            .video
            .get(1)
            .is_some_and(|sample| sample.timestamp < cutoff)
    {
        state.video.pop_front();
    }
    for queue in &mut state.audio {
        while queue
            .front()
            .is_some_and(|sample| sample.timestamp < cutoff)
        {
            queue.pop_front();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replay_pruning_keeps_only_the_requested_window() {
        let mut state = ReplayState::default();
        for second in 0..=10 {
            state.video.push_back(EncodedSample {
                data: vec![0],
                timestamp: Duration::from_secs(second),
                is_keyframe: second % 2 == 0,
            });
        }
        prune_state(&mut state, Duration::from_secs(4));
        assert_eq!(
            state.video.front().unwrap().timestamp,
            Duration::from_secs(5)
        );
        assert_eq!(
            state.video.back().unwrap().timestamp,
            Duration::from_secs(10)
        );
    }

    #[test]
    fn duration_export_starts_on_the_first_safe_keyframe() {
        let mut video = (0..=12)
            .map(|second| EncodedSample {
                data: vec![0],
                timestamp: Duration::from_secs(second),
                is_keyframe: second % 3 == 0,
            })
            .collect::<Vec<_>>();
        trim_video_to_duration(&mut video, Duration::from_secs(5));
        assert_eq!(video.first().unwrap().timestamp, Duration::from_secs(9));
        assert!(video.first().unwrap().is_keyframe);
    }
}
