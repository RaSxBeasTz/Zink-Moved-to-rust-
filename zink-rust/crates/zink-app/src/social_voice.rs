use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus_rs::{Application, OpusDecoder, OpusEncoder};
use tokio::sync::mpsc::Sender;

use crate::SocialCallCommand;

const CALL_SAMPLE_RATE: u32 = 16_000;
const CALL_FRAME_SAMPLES: usize = 320;

#[path = "social_desktop_audio.rs"]
mod desktop_audio;

type VoiceFrame = [f32; CALL_FRAME_SAMPLES];
type DesktopQueue = Arc<Mutex<Option<Receiver<VoiceFrame>>>>;

struct VoiceFrames {
    input_rate: u32,
    pending: VoiceFrame,
    filled: usize,
    phase: u32,
}

impl VoiceFrames {
    fn new(input_rate: u32) -> Self {
        Self {
            input_rate,
            pending: [0.0; CALL_FRAME_SAMPLES],
            filled: 0,
            phase: 0,
        }
    }

    fn push(&mut self, sample: f32, sender: &SyncSender<VoiceFrame>) {
        self.phase += CALL_SAMPLE_RATE;
        while self.phase >= self.input_rate {
            self.phase -= self.input_rate;
            self.pending[self.filled] = sample.clamp(-1.0, 1.0);
            self.filled += 1;
            if self.filled == CALL_FRAME_SAMPLES {
                let _ = sender.try_send(self.pending);
                self.filled = 0;
            }
        }
    }
}

pub struct SocialVoiceEngine {
    _input: cpal::Stream,
    _output: cpal::Stream,
    decoder: OpusDecoder,
    output_queue: Arc<Mutex<VecDeque<f32>>>,
    output_rate: u32,
    output_channels: usize,
    muted: Arc<AtomicBool>,
    deafened: Arc<AtomicBool>,
    desktop_audio: Option<desktop_audio::DesktopAudio>,
    desktop_queue: DesktopQueue,
    stopped: Arc<AtomicBool>,
}

impl SocialVoiceEngine {
    #[allow(clippy::too_many_lines)]
    pub fn start(
        sender: Sender<SocialCallCommand>,
        input_name: Option<&str>,
        output_name: Option<&str>,
        muted: bool,
        deafened: bool,
    ) -> anyhow::Result<Self> {
        let host = cpal::default_host();
        let input_device = choose_device(&host, true, input_name)?;
        let output_device = choose_device(&host, false, output_name)?;
        let input_supported = input_device
            .default_input_config()
            .context("read the default microphone format")?;
        let output_supported = output_device
            .default_output_config()
            .context("read the default call output format")?;
        let input_rate = input_supported.sample_rate();
        let input_channels = usize::from(input_supported.channels());
        let output_rate = output_supported.sample_rate();
        let output_channels = usize::from(output_supported.channels());
        let muted = Arc::new(AtomicBool::new(muted || deafened));
        let deafened = Arc::new(AtomicBool::new(deafened));
        let (mic_frames, microphone) = mpsc::sync_channel(4);
        let desktop_queue = Arc::new(Mutex::new(None));
        let mut encoder =
            OpusEncoder::new(16_000, 1, Application::Audio).map_err(|error| anyhow!(error))?;
        encoder.bitrate_bps = 32_000;
        encoder.use_cbr = false;

        let input_config = input_supported.config();
        let input = match input_supported.sample_format() {
            cpal::SampleFormat::F32 => build_input_stream::<f32>(
                &input_device,
                &input_config,
                input_rate,
                input_channels,
                mic_frames,
                Arc::clone(&muted),
                |sample| sample,
            ),
            cpal::SampleFormat::I16 => build_input_stream::<i16>(
                &input_device,
                &input_config,
                input_rate,
                input_channels,
                mic_frames,
                Arc::clone(&muted),
                |sample| f32::from(sample) / 32_768.0,
            ),
            cpal::SampleFormat::U16 => build_input_stream::<u16>(
                &input_device,
                &input_config,
                input_rate,
                input_channels,
                mic_frames,
                Arc::clone(&muted),
                |sample| (f32::from(sample) - 32_768.0) / 32_768.0,
            ),
            format => return Err(anyhow!("unsupported microphone sample format: {format:?}")),
        }?;

        let output_queue = Arc::new(Mutex::new(VecDeque::with_capacity(
            usize::try_from(output_rate).unwrap_or(48_000),
        )));
        let output_config = output_supported.config();
        let output = match output_supported.sample_format() {
            cpal::SampleFormat::F32 => build_output_stream::<f32>(
                &output_device,
                &output_config,
                Arc::clone(&output_queue),
                Arc::clone(&deafened),
                |sample| sample,
            ),
            cpal::SampleFormat::I16 => build_output_stream::<i16>(
                &output_device,
                &output_config,
                Arc::clone(&output_queue),
                Arc::clone(&deafened),
                f32_to_i16,
            ),
            cpal::SampleFormat::U16 => build_output_stream::<u16>(
                &output_device,
                &output_config,
                Arc::clone(&output_queue),
                Arc::clone(&deafened),
                f32_to_u16,
            ),
            format => return Err(anyhow!("unsupported speaker sample format: {format:?}")),
        }?;
        if !muted.load(Ordering::Acquire) {
            input.play().context("start Windows microphone capture")?;
        }
        if !deafened.load(Ordering::Acquire) {
            output.play().context("start Windows call playback")?;
        }
        let desktop_for_worker = Arc::clone(&desktop_queue);
        let muted_for_worker = Arc::clone(&muted);
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stopped);
        let decoder = OpusDecoder::new(16_000, 1).map_err(|error| anyhow!(error))?;
        thread::Builder::new()
            .name("zink-call-audio-encode".into())
            .spawn(move || {
                encode_call_audio(
                    encoder,
                    microphone,
                    desktop_for_worker,
                    sender,
                    muted_for_worker,
                    worker_stop,
                )
            })
            .context("start call audio encoder")?;

        Ok(Self {
            _input: input,
            _output: output,
            decoder,
            output_queue,
            output_rate,
            output_channels,
            muted,
            deafened,
            desktop_audio: None,
            desktop_queue,
            stopped,
        })
    }

    pub fn set_screen_audio(&mut self, enabled: bool) -> anyhow::Result<()> {
        if enabled && self.desktop_audio.is_none() {
            let (sender, frames) = mpsc::sync_channel(4);
            let capture = desktop_audio::DesktopAudio::start(sender)?;
            *self
                .desktop_queue
                .lock()
                .map_err(|_| anyhow!("desktop audio queue poisoned"))? = Some(frames);
            self.desktop_audio = Some(capture);
        } else if !enabled {
            self.desktop_audio = None;
            if let Ok(mut queue) = self.desktop_queue.lock() {
                *queue = None;
            }
        }
        Ok(())
    }

    pub fn set_muted(&self, muted: bool) {
        if self.muted.swap(muted, Ordering::AcqRel) != muted {
            let result = if muted {
                self._input.pause()
            } else {
                self._input.play()
            };
            if let Err(error) = result {
                tracing::warn!(%error, muted, "could not change the call microphone stream");
            }
        }
    }

    pub fn set_deafened(&self, deafened: bool) {
        let changed = self.deafened.swap(deafened, Ordering::AcqRel) != deafened;
        if deafened && let Ok(mut queue) = self.output_queue.lock() {
            queue.clear();
        }
        if changed {
            let result = if deafened {
                self._output.pause()
            } else {
                self._output.play()
            };
            if let Err(error) = result {
                tracing::warn!(%error, deafened, "could not change the call speaker stream");
            }
        }
    }

    pub fn play_packet(&mut self, packet: &[u8]) -> anyhow::Result<()> {
        if self.deafened.load(Ordering::Relaxed) {
            return Ok(());
        }
        let mut decoded = [0.0_f32; CALL_FRAME_SAMPLES];
        let count = self
            .decoder
            .decode(packet, CALL_FRAME_SAMPLES, &mut decoded)
            .map_err(|error| anyhow!(error))?;
        let output_rate = usize::try_from(self.output_rate).unwrap_or(48_000);
        let call_rate = usize::try_from(CALL_SAMPLE_RATE).unwrap_or(16_000);
        let mut queue = self
            .output_queue
            .lock()
            .map_err(|_| anyhow!("call audio queue poisoned"))?;
        let output_count = count.saturating_mul(output_rate).div_ceil(call_rate);
        for output_index in 0..output_count {
            let source_index = output_index
                .saturating_mul(call_rate)
                .checked_div(output_rate)
                .unwrap_or_default()
                .min(count.saturating_sub(1));
            let sample = decoded[source_index];
            for _ in 0..self.output_channels {
                queue.push_back(sample);
            }
        }
        let maximum = usize::try_from(self.output_rate).unwrap_or(48_000) * self.output_channels;
        while queue.len() > maximum {
            queue.pop_front();
        }
        Ok(())
    }
}

impl Drop for SocialVoiceEngine {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

fn encode_call_audio(
    mut encoder: OpusEncoder,
    microphone: Receiver<VoiceFrame>,
    desktop: DesktopQueue,
    sender: Sender<SocialCallCommand>,
    muted: Arc<AtomicBool>,
    stopped: Arc<AtomicBool>,
) {
    let period = Duration::from_millis(20);
    let mut deadline = Instant::now();
    loop {
        deadline += period;
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        if stopped.load(Ordering::Acquire) {
            break;
        }
        deadline = deadline.max(Instant::now() - period);
        let mic = microphone
            .try_recv()
            .ok()
            .filter(|_| !muted.load(Ordering::Relaxed));
        let game = desktop
            .lock()
            .ok()
            .and_then(|queue| queue.as_ref()?.try_recv().ok());
        let frame = match (mic, game) {
            (Some(mut mic), Some(game)) => {
                for (voice, game) in mic.iter_mut().zip(game) {
                    *voice = (*voice + game).clamp(-1.0, 1.0);
                }
                mic
            }
            (Some(frame), None) | (None, Some(frame)) => frame,
            (None, None) => continue,
        };
        let mut encoded = [0_u8; 512];
        if let Ok(length) = encoder.encode(&frame, CALL_FRAME_SAMPLES, &mut encoded) {
            let _ = sender.try_send(SocialCallCommand::Audio(encoded[..length].to_vec()));
        }
    }
}

pub fn input_devices() -> anyhow::Result<Vec<String>> {
    device_names(true)
}

pub fn output_devices() -> anyhow::Result<Vec<String>> {
    device_names(false)
}

fn device_names(input: bool) -> anyhow::Result<Vec<String>> {
    let host = cpal::default_host();
    let devices = if input {
        host.input_devices()
            .context("enumerate Windows microphones")?
    } else {
        host.output_devices()
            .context("enumerate Windows call outputs")?
    };
    let mut names = devices
        .filter_map(|device| {
            device
                .description()
                .ok()
                .map(|value| value.name().to_owned())
        })
        .collect::<Vec<_>>();
    names.sort_unstable();
    names.dedup();
    Ok(names)
}

fn choose_device(
    host: &cpal::Host,
    input: bool,
    selected_name: Option<&str>,
) -> anyhow::Result<cpal::Device> {
    if let Some(selected_name) = selected_name.filter(|name| !name.is_empty()) {
        let devices = if input {
            host.input_devices()
                .context("enumerate Windows microphones")?
        } else {
            host.output_devices()
                .context("enumerate Windows call outputs")?
        };
        if let Some(device) = devices
            .filter_map(|device| {
                device
                    .description()
                    .ok()
                    .map(|value| (device, value.name().to_owned()))
            })
            .find_map(|(device, name)| (name == selected_name).then_some(device))
        {
            return Ok(device);
        }
        anyhow::bail!("the selected call audio device is no longer available: {selected_name}");
    }
    if input {
        host.default_input_device()
            .context("no Windows microphone is available")
    } else {
        host.default_output_device()
            .context("no Windows call output device is available")
    }
}

#[allow(clippy::too_many_arguments)]
fn build_input_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    input_rate: u32,
    channels: usize,
    sender: SyncSender<VoiceFrame>,
    muted: Arc<AtomicBool>,
    convert: fn(T) -> f32,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + Copy + Send + 'static,
{
    let mut state = VoiceFrames::new(input_rate);
    device
        .build_input_stream(
            *config,
            move |data: &[T], _| {
                if muted.load(Ordering::Relaxed) {
                    state.filled = 0;
                    return;
                }
                let channel_divisor = u16::try_from(channels).map_or(1.0, f32::from);
                for frame in data.chunks_exact(channels) {
                    let mono = frame.iter().copied().map(convert).sum::<f32>() / channel_divisor;
                    state.push(mono, &sender);
                }
            },
            |error| {
                if error.kind() == cpal::ErrorKind::Xrun {
                    tracing::debug!(%error, "Windows microphone buffer discontinuity");
                } else {
                    tracing::error!(%error, "Windows call microphone stream failed");
                }
            },
            None,
        )
        .context("create Windows microphone stream")
}

fn build_output_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    queue: Arc<Mutex<VecDeque<f32>>>,
    deafened: Arc<AtomicBool>,
    convert: fn(f32) -> T,
) -> anyhow::Result<cpal::Stream>
where
    T: cpal::SizedSample + Copy + Send + 'static,
{
    device
        .build_output_stream(
            *config,
            move |data: &mut [T], _| {
                let Ok(mut queue) = queue.lock() else { return };
                for sample in data {
                    let value = if deafened.load(Ordering::Relaxed) {
                        0.0
                    } else {
                        queue.pop_front().unwrap_or(0.0)
                    };
                    *sample = convert(value);
                }
            },
            |error| tracing::error!(%error, "Windows call speaker stream failed"),
            None,
        )
        .context("create Windows call speaker stream")
}

#[allow(clippy::cast_possible_truncation)]
fn f32_to_i16(sample: f32) -> i16 {
    (sample.clamp(-1.0, 1.0) * 32_767.0).round() as i16
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn f32_to_u16(sample: f32) -> u16 {
    ((sample.clamp(-1.0, 1.0) + 1.0) * 32_767.5).round() as u16
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::SocialVoiceEngine;
    use crate::SocialCallCommand;

    #[test]
    fn call_sample_conversion_clamps_to_device_ranges() {
        assert_eq!(super::f32_to_i16(-2.0), i16::MIN + 1);
        assert_eq!(super::f32_to_i16(2.0), i16::MAX);
        assert_eq!(super::f32_to_u16(-2.0), u16::MIN);
        assert_eq!(super::f32_to_u16(2.0), u16::MAX);
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires live Windows microphone and playback devices"]
    fn windows_voice_engine_captures_and_plays_an_opus_packet()
    -> Result<(), Box<dyn std::error::Error>> {
        let (sender, mut receiver) = tokio::sync::mpsc::channel(16);
        let mut engine = SocialVoiceEngine::start(sender, None, None, true, true)?;
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()?;
        assert!(
            runtime
                .block_on(async {
                    tokio::time::timeout(Duration::from_millis(180), receiver.recv()).await
                })
                .is_err(),
            "a call started muted must not send microphone packets"
        );
        engine.set_deafened(false);
        engine.set_muted(false);
        let packet = runtime
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(5), async {
                    while let Some(command) = receiver.recv().await {
                        if let SocialCallCommand::Audio(packet) = command {
                            return Some(packet);
                        }
                    }
                    None
                })
                .await
            })?
            .ok_or("voice capture channel disconnected")?;
        assert!(!packet.is_empty());
        engine.play_packet(&packet)?;
        engine.set_muted(true);
        engine.set_deafened(true);
        engine.play_packet(&packet)?;
        assert!(engine.output_queue.lock().unwrap().is_empty());
        drop(engine);
        Ok(())
    }
}
