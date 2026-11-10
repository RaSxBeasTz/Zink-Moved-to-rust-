//! The public `Pipeline` — the single entry point the Tauri backend calls.
//!
//! It wires capture -> encode -> fork -> {MP4 recorder, RTMP publisher} on a
//! background worker thread, controllable via `start`/`stop`. No Media
//! Foundation / D3D / windows types appear in this module's public API, so a v2
//! encoder backend (NVENC/AMF) can replace the internals without changing it.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::audio::aac::AacEncoder;
use crate::audio::{capture as acap, mix};
use crate::capture::{self, CaptureConfig};
use crate::encoder::mf_h264::MfH264Encoder;
use crate::encoder::EncodedSample;
use crate::fork::Fork;
use crate::mux::{AudioTrack, Mp4Recorder};
use crate::stream::RtmpPublisher;
use crate::{AudioSource, CaptureTarget, PipelineError, Result, StreamConfig, VideoConfig};

/// Recording configuration.
#[derive(Clone, Debug)]
pub struct RecordConfig {
    /// Output MP4 file path.
    pub output_path: std::path::PathBuf,
}

/// Audio configuration for the pipeline.
///
/// When set, the recorder writes system-loopback and (if present) microphone as
/// two separate AAC tracks, and the streamer publishes a single AAC track mixed
/// from both sources. Each consumer opens its own WASAPI clients, so no audio
/// data crosses the video fork.
#[derive(Clone, Debug)]
pub struct AudioConfig {
    /// Capture system/desktop audio via loopback.
    pub loopback: bool,
    /// Capture the default microphone (silently skipped if none exists).
    pub microphone: bool,
    /// Optional render endpoint ID. `None` follows the Windows default output.
    pub loopback_device_id: Option<String>,
    /// Optional capture endpoint ID. `None` follows the Windows default input.
    pub microphone_device_id: Option<String>,
    /// Linear desktop/loopback gain. `1.0` preserves the captured level.
    pub loopback_gain: f32,
    /// Linear microphone gain. `1.0` preserves the captured level.
    pub microphone_gain: f32,
    /// AAC bitrate in bits/sec (e.g. 128_000).
    pub bitrate: u32,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            loopback: true,
            microphone: true,
            loopback_device_id: None,
            microphone_device_id: None,
            loopback_gain: 1.0,
            microphone_gain: 1.0,
            bitrate: 128_000,
        }
    }
}

/// Full pipeline configuration.
#[derive(Clone, Debug)]
pub struct PipelineConfig {
    pub capture_target: CaptureTarget,
    pub capture_cursor: bool,
    pub video: VideoConfig,
    /// Enable recording to an MP4 file.
    pub record: Option<RecordConfig>,
    /// Enable RTMP streaming.
    pub stream: Option<StreamConfig>,
    /// Enable audio capture/encode. `None` = video only.
    pub audio: Option<AudioConfig>,
}

/// How long to warm up the encoder waiting for SPS/PPS before giving up.
const WARMUP_FRAMES: usize = 60;
/// Fork channel depth (~4s at 30fps).
const FORK_BOUND: usize = 120;

/// Live measurements from the native capture/encode pipeline.
#[derive(Clone, Copy, Debug, Default)]
pub struct PipelineStats {
    pub encoded_frames: u64,
    pub encoded_bytes: u64,
    pub dropped_stream_frames: u64,
    pub elapsed: Duration,
}

impl PipelineStats {
    pub fn encoded_fps(self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            self.encoded_frames as f64 / seconds
        } else {
            0.0
        }
    }

    pub fn video_mbps(self) -> f64 {
        let seconds = self.elapsed.as_secs_f64();
        if seconds > 0.0 {
            self.encoded_bytes as f64 * 8.0 / seconds / 1_000_000.0
        } else {
            0.0
        }
    }
}

#[derive(Default)]
struct PipelineCounters {
    encoded_frames: AtomicU64,
    encoded_bytes: AtomicU64,
    dropped_stream_frames: AtomicU64,
}

pub struct Pipeline {
    config: PipelineConfig,
    running: Arc<AtomicBool>,
    is_recording: Arc<AtomicBool>,
    is_streaming: Arc<AtomicBool>,
    counters: Arc<PipelineCounters>,
    started_at: Option<Instant>,
    worker: Option<std::thread::JoinHandle<Result<()>>>,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Result<Self> {
        if config.record.is_none() && config.stream.is_none() {
            return Err(PipelineError::Rtmp(
                "pipeline needs at least one of record or stream".into(),
            ));
        }
        Ok(Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            is_recording: Arc::new(AtomicBool::new(false)),
            is_streaming: Arc::new(AtomicBool::new(false)),
            counters: Arc::new(PipelineCounters::default()),
            started_at: None,
            worker: None,
        })
    }

    /// Start the pipeline. Spawns the capture/encode/fork worker and the
    /// enabled consumers. Non-blocking: returns once the worker is launched.
    ///
    /// Async only so it fits the Tauri command signature; the heavy work runs
    /// on a dedicated OS thread, not the async runtime.
    pub async fn start(&mut self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(()); // already running
        }

        let config = self.config.clone();
        let running = self.running.clone();
        let is_recording = self.is_recording.clone();
        let is_streaming = self.is_streaming.clone();
        let counters = self.counters.clone();
        counters.encoded_frames.store(0, Ordering::Relaxed);
        counters.encoded_bytes.store(0, Ordering::Relaxed);
        counters.dropped_stream_frames.store(0, Ordering::Relaxed);
        self.started_at = Some(Instant::now());

        // A oneshot to surface startup errors (device/encoder/connect) back to
        // the caller instead of losing them inside the thread.
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<()>>();

        let worker = std::thread::spawn(move || {
            run_worker(
                config,
                running,
                is_recording,
                is_streaming,
                counters,
                ready_tx,
            )
        });
        self.worker = Some(worker);

        // Wait for the worker to report it initialized (or failed).
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => {
                self.running.store(false, Ordering::SeqCst);
                if let Some(w) = self.worker.take() {
                    let _ = w.join();
                }
                Err(e)
            }
            Err(_) => {
                self.running.store(false, Ordering::SeqCst);
                Err(PipelineError::Rtmp("worker exited before ready".into()))
            }
        }
    }

    /// Stop the pipeline. Signals the worker to drain and join, finalizing the
    /// MP4 and closing the RTMP connection cleanly.
    pub async fn stop(&mut self) -> Result<()> {
        if !self.running.swap(false, Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| PipelineError::Rtmp("worker thread panicked".into()))??;
        }
        self.is_recording.store(false, Ordering::SeqCst);
        self.is_streaming.store(false, Ordering::SeqCst);
        Ok(())
    }

    pub fn is_recording(&self) -> bool {
        self.is_recording.load(Ordering::SeqCst)
    }

    pub fn is_streaming(&self) -> bool {
        self.is_streaming.load(Ordering::SeqCst)
    }

    pub fn stats(&self) -> PipelineStats {
        PipelineStats {
            encoded_frames: self.counters.encoded_frames.load(Ordering::Relaxed),
            encoded_bytes: self.counters.encoded_bytes.load(Ordering::Relaxed),
            dropped_stream_frames: self.counters.dropped_stream_frames.load(Ordering::Relaxed),
            elapsed: self
                .started_at
                .map_or(Duration::ZERO, |started| started.elapsed()),
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        // Ensure the worker is signalled and joined so files finalize.
        self.running.store(false, Ordering::SeqCst);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

/// The worker body: owns capture, encoder, and fork; drives them until
/// `running` clears. Consumers run on their own thread/runtime.
fn run_worker(
    config: PipelineConfig,
    running: Arc<AtomicBool>,
    is_recording: Arc<AtomicBool>,
    is_streaming: Arc<AtomicBool>,
    counters: Arc<PipelineCounters>,
    ready_tx: std::sync::mpsc::Sender<Result<()>>,
) -> Result<()> {
    // --- init: capture + encoder + warmup for SPS/PPS ---
    let init = (|| -> Result<_> {
        let (session, rx) = capture::start(
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
                tracing::warn!(%hardware_error, "hardware H.264 encoder unavailable; using Media Foundation software encoder");
                MfH264Encoder::new(session.device(), config.video)
            })?;

        let mut warmup: Vec<EncodedSample> = Vec::new();
        let mut fed = 0usize;
        while encoder.parameter_sets().sps.is_empty() && fed < WARMUP_FRAMES {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(frame) => {
                    let mut out = Vec::new();
                    if let Err(hardware_error) =
                        encoder.encode(&frame.texture, frame.timestamp, &mut out)
                    {
                        if !using_hardware {
                            return Err(hardware_error);
                        }
                        tracing::warn!(%hardware_error, "hardware H.264 encoder rejected input; switching to Media Foundation software encoder");
                        encoder = MfH264Encoder::new(session.device(), config.video)?;
                        using_hardware = false;
                        out.clear();
                        encoder.encode(&frame.texture, frame.timestamp, &mut out)?;
                    }
                    fed += 1;
                    warmup.extend(out);
                }
                Err(_) => break,
            }
        }
        if encoder.parameter_sets().sps.is_empty() {
            return Err(PipelineError::TypeNegotiation(
                "no SPS/PPS produced during warmup".into(),
            ));
        }
        Ok((session, rx, encoder, warmup))
    })();

    let (session, rx, mut encoder, warmup) = match init {
        Ok(v) => v,
        Err(e) => {
            let _ = ready_tx.send(Err(e));
            return Ok(());
        }
    };

    let params = encoder.parameter_sets().clone();
    let want_record = config.record.is_some();
    let want_stream = config.stream.is_some();
    let (mut fork, receivers) = Fork::new(want_record, want_stream, FORK_BOUND);

    // --- spawn record consumer ---
    let record_handle = if let Some(rec) = config.record.clone() {
        let record_rx = receivers.record.unwrap();
        let vcfg = config.video;
        let rparams = params.clone();
        let audio_cfg = config.audio.clone();
        is_recording.store(true, Ordering::SeqCst);
        Some(std::thread::spawn(move || -> Result<()> {
            record_consumer(rec, vcfg, rparams, audio_cfg, record_rx)
        }))
    } else {
        None
    };

    // --- spawn stream consumer (own current-thread tokio runtime) ---
    let stream_handle = if let Some(scfg) = config.stream.clone() {
        let stream_rx = receivers.stream.unwrap();
        let vcfg = config.video;
        let sparams = params.clone();
        let audio_cfg = config.audio.clone();
        is_streaming.store(true, Ordering::SeqCst);
        Some(std::thread::spawn(move || -> Result<()> {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(stream_consumer(scfg, vcfg, sparams, audio_cfg, stream_rx))
        }))
    } else {
        None
    };

    // The consumers are up (recorder created, RTMP connected inside their
    // threads). Report ready. If a consumer fails to init it will surface on
    // join at stop() time.
    let _ = ready_tx.send(Ok(()));

    // --- main loop: push warmup, then capture/encode until stopped ---
    for s in warmup {
        counters.encoded_frames.fetch_add(1, Ordering::Relaxed);
        counters
            .encoded_bytes
            .fetch_add(s.data.len() as u64, Ordering::Relaxed);
        fork.distribute(s);
        counters
            .dropped_stream_frames
            .store(fork.dropped_stream_frames(), Ordering::Relaxed);
    }
    while running.load(Ordering::SeqCst) {
        let frame = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(f) => f,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => continue,
            Err(_) => break,
        };
        let mut out = Vec::new();
        if let Err(e) = encoder.encode(&frame.texture, frame.timestamp, &mut out) {
            tracing::error!("encode error: {e}");
            break;
        }
        for s in out {
            counters.encoded_frames.fetch_add(1, Ordering::Relaxed);
            counters
                .encoded_bytes
                .fetch_add(s.data.len() as u64, Ordering::Relaxed);
            if !fork.distribute(s) {
                break; // all consumers gone
            }
            counters
                .dropped_stream_frames
                .store(fork.dropped_stream_frames(), Ordering::Relaxed);
        }
    }

    // --- shutdown: drain encoder, close fork, join consumers ---
    let mut tail = Vec::new();
    let _ = encoder.drain(&mut tail);
    for s in tail {
        counters.encoded_frames.fetch_add(1, Ordering::Relaxed);
        counters
            .encoded_bytes
            .fetch_add(s.data.len() as u64, Ordering::Relaxed);
        fork.distribute(s);
        counters
            .dropped_stream_frames
            .store(fork.dropped_stream_frames(), Ordering::Relaxed);
    }
    drop(fork); // closes channels -> consumers finish
    drop(session);

    if let Some(h) = record_handle {
        h.join()
            .map_err(|_| PipelineError::Rtmp("record thread panicked".into()))??;
    }
    if let Some(h) = stream_handle {
        h.join()
            .map_err(|_| PipelineError::Rtmp("stream thread panicked".into()))??;
    }
    Ok(())
}

// --- consumers -----------------------------------------------------------

/// Recorder thread body: writes video + (optionally) two separate AAC audio
/// tracks (loopback = track 0, mic = track 1). Each audio source is captured
/// and encoded here so no audio crosses the video fork.
fn record_consumer(
    rec: RecordConfig,
    vcfg: VideoConfig,
    params: crate::encoder::ParameterSets,
    audio_cfg: Option<AudioConfig>,
    record_rx: std::sync::mpsc::Receiver<EncodedSample>,
) -> Result<()> {
    // Set up audio captures + encoders + MP4 track descriptors. The
    // AudioCapture handles (`_loop_h`/`_mic_h`) must stay alive for the whole
    // function — dropping one stops its capture thread.
    let mut loop_cap = None;
    let mut loop_enc = None;
    let mut mic_cap = None;
    let mut mic_enc = None;
    let mut _loop_h = None;
    let mut _mic_h = None;
    let mut tracks: Vec<AudioTrack> = Vec::new();

    if let Some(acfg) = &audio_cfg {
        if acfg.loopback {
            let source = acfg.loopback_device_id.clone().map_or(
                AudioSource::SystemLoopback,
                AudioSource::SystemLoopbackDevice,
            );
            if let Ok((cap, rx)) = acap::start(source, 64) {
                let enc = AacEncoder::new(cap.sample_rate, cap.channels, acfg.bitrate)?;
                tracks.push(AudioTrack {
                    sample_rate: cap.sample_rate,
                    channels: cap.channels,
                    asc: enc.audio_specific_config().to_vec(),
                    bitrate: acfg.bitrate,
                });
                _loop_h = Some(cap);
                loop_cap = Some(rx);
                loop_enc = Some(enc);
            }
        }
        if acfg.microphone {
            let source = acfg
                .microphone_device_id
                .clone()
                .map_or(AudioSource::Microphone, AudioSource::MicrophoneDevice);
            if let Ok((cap, rx)) = acap::start(source, 64) {
                let enc = AacEncoder::new(cap.sample_rate, cap.channels, acfg.bitrate)?;
                tracks.push(AudioTrack {
                    sample_rate: cap.sample_rate,
                    channels: cap.channels,
                    asc: enc.audio_specific_config().to_vec(),
                    bitrate: acfg.bitrate,
                });
                _mic_h = Some(cap);
                mic_cap = Some(rx);
                mic_enc = Some(enc);
            }
        }
    }

    // Track indices: loopback first (0) if present, then mic.
    let loop_track = if loop_enc.is_some() {
        Some(0usize)
    } else {
        None
    };
    let mic_track = if mic_enc.is_some() {
        Some(if loop_track.is_some() { 1 } else { 0 })
    } else {
        None
    };

    let mut recorder = Mp4Recorder::with_audio(&rec.output_path, &vcfg, &params, &tracks)?;

    // Poll video (blocking with timeout) + audio (non-blocking) until the video
    // channel closes (pipeline stop drops the fork sender).
    loop {
        match record_rx.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(sample) => recorder.write(&sample)?,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
        let loop_gain = audio_cfg.as_ref().map_or(1.0, |value| value.loopback_gain);
        let mic_gain = audio_cfg
            .as_ref()
            .map_or(1.0, |value| value.microphone_gain);
        drain_audio(
            &mut recorder,
            loop_track,
            &loop_cap,
            loop_enc.as_mut(),
            loop_gain,
        )?;
        drain_audio(
            &mut recorder,
            mic_track,
            &mic_cap,
            mic_enc.as_mut(),
            mic_gain,
        )?;
    }
    // Final audio drain.
    let loop_gain = audio_cfg.as_ref().map_or(1.0, |value| value.loopback_gain);
    let mic_gain = audio_cfg
        .as_ref()
        .map_or(1.0, |value| value.microphone_gain);
    drain_audio(
        &mut recorder,
        loop_track,
        &loop_cap,
        loop_enc.as_mut(),
        loop_gain,
    )?;
    drain_audio(
        &mut recorder,
        mic_track,
        &mic_cap,
        mic_enc.as_mut(),
        mic_gain,
    )?;

    recorder.finalize()?;
    Ok(())
}

/// Encode any pending PCM from `cap` and write the AAC frames to `track`.
fn drain_audio(
    recorder: &mut Mp4Recorder,
    track: Option<usize>,
    cap: &Option<std::sync::mpsc::Receiver<crate::AudioBuffer>>,
    enc: Option<&mut AacEncoder>,
    gain: f32,
) -> Result<()> {
    let (Some(track), Some(rx), Some(enc)) = (track, cap.as_ref(), enc) else {
        return Ok(());
    };
    while let Ok(buf) = rx.try_recv() {
        let mut out = Vec::new();
        let pcm = mix::apply_gain_pcm16(&buf.data, gain);
        enc.encode(&pcm, buf.timestamp, &mut out)?;
        for f in &out {
            recorder.write_audio(track, &f.data, f.timestamp)?;
        }
    }
    Ok(())
}

/// Streamer task body: publishes video + a single AAC track mixed from loopback
/// and mic (loopback alone if mic is absent).
async fn stream_consumer(
    scfg: StreamConfig,
    vcfg: VideoConfig,
    params: crate::encoder::ParameterSets,
    audio_cfg: Option<AudioConfig>,
    stream_rx: std::sync::mpsc::Receiver<EncodedSample>,
) -> Result<()> {
    let mut publisher = RtmpPublisher::connect(scfg, vcfg, params).await?;

    // Audio: capture loopback (+ mic), mix, encode to one AAC track. Keep the
    // AudioCapture handles alive (dropping one stops its capture thread).
    let mut loop_rx = None;
    let mut mic_rx = None;
    let mut mix_enc: Option<AacEncoder> = None;
    let mut _loop_h = None;
    let mut _mic_h = None;

    if let Some(acfg) = &audio_cfg {
        if acfg.loopback {
            let source = acfg.loopback_device_id.clone().map_or(
                AudioSource::SystemLoopback,
                AudioSource::SystemLoopbackDevice,
            );
            if let Ok((cap, rx)) = acap::start(source, 64) {
                let enc = AacEncoder::new(cap.sample_rate, cap.channels, acfg.bitrate)?;
                publisher
                    .send_audio_sequence_header(enc.audio_specific_config())
                    .await?;
                mix_enc = Some(enc);
                _loop_h = Some(cap);
                loop_rx = Some(rx);
            }
        }
        if acfg.microphone {
            let source = acfg
                .microphone_device_id
                .clone()
                .map_or(AudioSource::Microphone, AudioSource::MicrophoneDevice);
            if let Ok((cap, rx)) = acap::start(source, 64) {
                if mix_enc.is_none() {
                    let enc = AacEncoder::new(cap.sample_rate, cap.channels, acfg.bitrate)?;
                    publisher
                        .send_audio_sequence_header(enc.audio_specific_config())
                        .await?;
                    mix_enc = Some(enc);
                }
                _mic_h = Some(cap);
                mic_rx = Some(rx);
            }
        }
    }

    loop {
        match stream_rx.recv_timeout(std::time::Duration::from_millis(5)) {
            Ok(sample) => {
                if publisher.send_video(&sample).await.is_err()
                    && publisher.reconnect().await.is_err()
                {
                    break;
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }

        // Mix + send any pending audio.
        if let (Some(lrx), Some(enc)) = (&loop_rx, mix_enc.as_mut()) {
            while let Ok(buf) = lrx.try_recv() {
                let desktop = mix::apply_gain_pcm16(
                    &buf.data,
                    audio_cfg.as_ref().map_or(1.0, |value| value.loopback_gain),
                );
                let mixed = match mic_rx.as_ref().and_then(|r| r.try_recv().ok()) {
                    Some(m) => {
                        let microphone = mix::apply_gain_pcm16(
                            &m.data,
                            audio_cfg
                                .as_ref()
                                .map_or(1.0, |value| value.microphone_gain),
                        );
                        mix::mix_pcm16(&desktop, &microphone)
                    }
                    None => desktop,
                };
                let mut out = Vec::new();
                enc.encode(&mixed, buf.timestamp, &mut out)?;
                for f in &out {
                    let _ = publisher.send_audio(&f.data, f.timestamp).await;
                }
            }
        } else if let (Some(mrx), Some(enc)) = (&mic_rx, mix_enc.as_mut()) {
            while let Ok(buf) = mrx.try_recv() {
                let microphone = mix::apply_gain_pcm16(
                    &buf.data,
                    audio_cfg
                        .as_ref()
                        .map_or(1.0, |value| value.microphone_gain),
                );
                let mut out = Vec::new();
                enc.encode(&microphone, buf.timestamp, &mut out)?;
                for frame in &out {
                    let _ = publisher.send_audio(&frame.data, frame.timestamp).await;
                }
            }
        }
    }
    Ok(())
}
