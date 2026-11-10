use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use win_native_media::{
    CaptureTarget, CapturedFrame, HdrToSdr, VideoConfig,
    capture::{self, CaptureConfig},
    encoder::mf_h264::MfH264Encoder,
};
use windows::Win32::{
    Graphics::Direct3D11::{ID3D11Device, ID3D11Texture2D},
    System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize},
};

use crate::{SocialCallCommand, SocialCallEvent, push_social_call_event};
use tokio::sync::mpsc::Sender as CallSender;

#[path = "social_desktop_capture.rs"]
pub(crate) mod desktop;

#[derive(Default)]
pub(crate) struct ShareRate {
    start: Option<Instant>,
    frames: u32,
    bytes: usize,
}

impl ShareRate {
    pub(crate) fn frame(&mut self, bytes: usize) -> Option<(f64, f64)> {
        let now = Instant::now();
        let elapsed = now.duration_since(*self.start.get_or_insert(now));
        self.frames += 1;
        self.bytes += bytes;
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        let seconds = elapsed.as_secs_f64();
        let rates = (
            f64::from(self.frames - 1) / seconds,
            self.bytes.saturating_sub(bytes) as f64 * 8.0 / seconds / 1_000_000.0,
        );
        *self = Self {
            start: Some(now),
            frames: 1,
            bytes,
        };
        Some(rates)
    }
}

pub struct SocialScreenShare {
    stop: SyncSender<()>,
    force_sdr: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ShareControl {
    pub bitrate: AtomicU32,
    pub profile_bitrate: u32,
    pub use_av1: AtomicBool,
    pub av1_failed: AtomicBool,
    pub keyframe_requested: AtomicBool,
}

impl SocialScreenShare {
    pub fn start(
        call_sender: CallSender<SocialCallCommand>,
        quality: &str,
        prefer_hdr10: bool,
    ) -> anyhow::Result<Self> {
        let video = video_config(quality);
        let (stop, stop_rx) = mpsc::sync_channel(1);
        let force_sdr = Arc::new(AtomicBool::new(false));
        let worker_force_sdr = Arc::clone(&force_sdr);
        thread::Builder::new()
            .name("zink-social-screen-share".to_owned())
            .spawn(move || {
                // Graphics Capture uses WinRT before the encoder initializes COM.
                let result = unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
                    .context("initialize Windows Runtime for screen capture")
                    .and_then(|()| {
                        let result = run_capture(
                            &call_sender,
                            &stop_rx,
                            video,
                            prefer_hdr10,
                            &worker_force_sdr,
                        );
                        // Balance the initialization on the same worker thread.
                        unsafe { RoUninitialize() };
                        result
                    });
                if let Err(error) = result {
                    let detail = format!("{error:#}");
                    tracing::warn!(%detail, "call screen capture failed");
                    push_social_call_event(SocialCallEvent::ScreenShareFailed(detail));
                }
                let _ = call_sender.try_send(SocialCallCommand::ScreenStopped);
            })
            .context("start the screen-sharing worker")?;
        Ok(Self { stop, force_sdr })
    }

    pub fn fallback_to_sdr(&self) {
        self.force_sdr.store(true, Ordering::Relaxed);
    }
}

impl Drop for SocialScreenShare {
    fn drop(&mut self) {
        let _ = self.stop.try_send(());
    }
}

fn run_capture(
    call_sender: &CallSender<SocialCallCommand>,
    stop_rx: &mpsc::Receiver<()>,
    video: VideoConfig,
    prefer_hdr10: bool,
    force_sdr: &AtomicBool,
) -> anyhow::Result<()> {
    let monitor = capture::monitor_at_index(0).map_err(|error| anyhow!(error))?;
    let source_hdr = desktop::monitor_has_hdr(monitor);
    if prefer_hdr10 {
        let result = (|| {
            let mut desktop = desktop::DesktopCapture::start_hdr10(monitor)?;
            let device = desktop.device().clone();
            let started = Instant::now();
            encode_frames(
                call_sender,
                stop_rx,
                video,
                &device,
                ScreenColor::Hdr10,
                move |earliest, busy| {
                    anyhow::ensure!(
                        !force_sdr.load(Ordering::Relaxed),
                        "the viewer requested SDR"
                    );
                    next_desktop_frame(&mut desktop, started, earliest, busy)
                },
            )
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(error) => tracing::info!(%error, "HDR10 unavailable, falling back to SDR"),
        }
    }
    let (desktop, color) = if source_hdr {
        (
            desktop::DesktopCapture::start_hdr10(monitor),
            ScreenColor::ToneMappedSdr,
        )
    } else {
        (desktop::DesktopCapture::start(monitor), ScreenColor::Sdr)
    };
    if let Ok(mut desktop) = desktop
        .inspect_err(|error| tracing::info!(%error, "using Windows Graphics Capture fallback"))
    {
        tracing::info!("sharing the display with DXGI Desktop Duplication");
        let device = desktop.device().clone();
        let started = Instant::now();
        let result = encode_frames(
            call_sender,
            stop_rx,
            video,
            &device,
            color,
            move |earliest, busy| next_desktop_frame(&mut desktop, started, earliest, busy),
        );
        match result {
            Err(error) if source_hdr => {
                tracing::info!(%error, "using FP16 Windows Graphics Capture fallback")
            }
            result => return result,
        }
    }

    let config = CaptureConfig {
        target: CaptureTarget::Monitor(0),
        capture_cursor: true,
    };
    let (session, frames) = if source_hdr {
        capture::start_scrgb(config, 3)
    } else {
        capture::start(config, 3)
    }
    .map_err(|error| anyhow!(error))
    .context("start Windows Graphics Capture")?;
    encode_frames(
        call_sender,
        stop_rx,
        video,
        session.device(),
        color,
        |_, _| match frames.recv_timeout(Duration::from_millis(50)) {
            Ok(frame) => Ok(Some(ScreenFrame::GraphicsCapture(frame))),
            Err(mpsc::RecvTimeoutError::Timeout) => Ok(None),
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                Err(anyhow!("the Windows capture session ended"))
            }
        },
    )
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ScreenColor {
    Sdr,
    ToneMappedSdr,
    Hdr10,
}

enum ScreenFrame {
    Desktop(ID3D11Texture2D, Duration),
    GraphicsCapture(CapturedFrame),
}

fn next_desktop_frame(
    desktop: &mut desktop::DesktopCapture,
    started: Instant,
    earliest: Duration,
    busy: bool,
) -> anyhow::Result<Option<ScreenFrame>> {
    let wait = earliest.saturating_sub(started.elapsed());
    if busy || !wait.is_zero() {
        // Leave opportunities to drain an asynchronous encoder while
        // avoiding full-resolution desktop copies between capture ticks.
        thread::sleep(if busy {
            Duration::from_millis(2)
        } else {
            wait.min(Duration::from_millis(2))
        });
        return Ok(None);
    }
    Ok(desktop
        .next_frame()?
        .map(|texture| ScreenFrame::Desktop(texture, started.elapsed())))
}

fn encode_frames(
    call_sender: &CallSender<SocialCallCommand>,
    stop_rx: &mpsc::Receiver<()>,
    video: VideoConfig,
    device: &ID3D11Device,
    color: ScreenColor,
    mut next_frame: impl FnMut(Duration, bool) -> anyhow::Result<Option<ScreenFrame>>,
) -> anyhow::Result<()> {
    let hdr10 = color == ScreenColor::Hdr10;
    let mut tone_mapper = if color == ScreenColor::ToneMappedSdr {
        tracing::info!("tone-mapping FP16 screen capture to SDR before H.264 encoding");
        Some(
            HdrToSdr::new(device, video.width, video.height)
                .map_err(|error| anyhow!(error))
                .context("create the HDR-to-SDR tone mapper")?,
        )
    } else {
        None
    };
    let mut hardware_sdr = !hdr10;
    let mut encoder = if hdr10 {
        MfH264Encoder::new_hdr10(device, video)
    } else {
        MfH264Encoder::new_sdr_studio(device, video, true).or_else(|error| {
            tracing::info!(%error, "using the Windows software screen-share encoder");
            hardware_sdr = false;
            MfH264Encoder::new_sdr_studio(device, video, false)
        })
    }
    .map_err(|error| anyhow!(error))
    .context("configure the Media Foundation screen-share encoder")?;
    let mut announced = false;
    let control = Arc::new(ShareControl {
        profile_bitrate: video.bitrate,
        ..ShareControl::default()
    });
    let mut av1 = false;
    let mut sequence_header = Vec::new();
    let mut applied_bitrate = video.bitrate;
    let mut next_rate_change = Instant::now();
    let mut first_hardware_input = None;
    let deadline = Instant::now() + Duration::from_secs(8);

    let frame_interval = Duration::from_secs_f64(1.0 / f64::from(video.fps));
    let mut next_timestamp = Duration::ZERO;
    // Limit video independently of audio and signalling. Skip raw frames while
    // the connection is busy so we preserve encoded reference frames.
    let video_slots = Arc::new(tokio::sync::Semaphore::new(2));
    while matches!(stop_rx.try_recv(), Err(mpsc::TryRecvError::Empty)) {
        if hdr10 && !announced && Instant::now() >= deadline {
            return Err(anyhow!("HDR10 encoder did not produce a frame"));
        }
        let frame = next_frame(
            next_timestamp.saturating_sub(frame_interval / 8),
            video_slots.available_permits() == 0,
        )
        .context("read the shared display")?;
        let capture = frame
            .as_ref()
            .map(|frame| match frame {
                ScreenFrame::Desktop(texture, timestamp) => (texture, *timestamp),
                ScreenFrame::GraphicsCapture(frame) => (&frame.texture, frame.timestamp),
            })
            .filter(|(_, timestamp)| *timestamp + frame_interval / 8 >= next_timestamp);
        if capture.is_none() && !hardware_sdr {
            continue;
        }
        let Ok(slot) = Arc::clone(&video_slots).try_acquire_owned() else {
            continue;
        };
        // A frame skipped during backpressure does not spend a capture slot.
        // Admit the next available frame as soon as transport has room.
        if let Some((_, timestamp)) = capture {
            next_timestamp = if timestamp.saturating_sub(next_timestamp) >= frame_interval {
                timestamp + frame_interval
            } else {
                next_timestamp + frame_interval
            };
        }
        if !hdr10 && control.use_av1.load(Ordering::Relaxed) != av1 {
            let want_av1 = control.use_av1.load(Ordering::Relaxed);
            let next = if want_av1 {
                MfH264Encoder::new_av1_call(device, video)
            } else {
                MfH264Encoder::new_sdr_studio(device, video, true)
                    .or_else(|_| MfH264Encoder::new_sdr_studio(device, video, false))
            };
            match next {
                Ok(next) => {
                    encoder = next;
                    av1 = want_av1;
                    hardware_sdr = true;
                    first_hardware_input = None;
                    announced = false;
                    applied_bitrate = video.bitrate;
                    next_rate_change = Instant::now();
                    sequence_header.clear();
                }
                Err(error) if want_av1 => {
                    tracing::info!(%error, "using H.264 because hardware AV1 could not start");
                    control.av1_failed.store(true, Ordering::Relaxed);
                    control.use_av1.store(false, Ordering::Relaxed);
                }
                Err(error) => {
                    return Err(anyhow!(error)).context("restart the H.264 screen encoder");
                }
            }
        }
        let mut samples = Vec::new();
        if !hdr10 && Instant::now() >= next_rate_change {
            next_rate_change = Instant::now() + Duration::from_millis(350);
            let estimate = control.bitrate.load(Ordering::Relaxed);
            let wanted = if estimate == 0 {
                video.bitrate
            } else {
                (estimate * 85 / 100).clamp(350_000, video.bitrate)
            };
            if (estimate == 0 && applied_bitrate != wanted)
                || applied_bitrate.abs_diff(wanted) >= applied_bitrate / 8
            {
                match encoder.set_sdr_call_bitrate(wanted) {
                    Ok(()) => applied_bitrate = wanted,
                    Err(error) => {
                        tracing::debug!(%error, "screen encoder rejected a bitrate change")
                    }
                }
            }
        }
        let result = if let Some((texture, timestamp)) = capture {
            if !hdr10
                && control.keyframe_requested.swap(false, Ordering::AcqRel)
                && let Err(error) = encoder.request_keyframe()
            {
                tracing::debug!(%error, "screen encoder rejected a keyframe request");
            }
            let texture = if let Some(tone_mapper) = &mut tone_mapper {
                tone_mapper
                    .convert(texture)
                    .map_err(|error| anyhow!(error))
                    .context("tone-map the HDR screen capture")?
            } else {
                texture
            };
            encoder.encode(texture, timestamp, &mut samples)
        } else {
            encoder.poll_sdr_call_output(&mut samples)
        };
        let stalled = hardware_sdr
            && !announced
            && capture.is_some()
            && (samples.is_empty()
                || (av1
                    && !samples.iter().any(|sample| {
                        crate::social_screen_rtc::av1_sequence_header(&sample.data).is_some()
                    })))
            && first_hardware_input
                .get_or_insert_with(Instant::now)
                .elapsed()
                > Duration::from_secs(2);
        if hardware_sdr && (result.is_err() || stalled) {
            tracing::info!(error = ?result.err(), stalled, "switching screen sharing to the Windows software encoder");
            encoder = MfH264Encoder::new_sdr_studio(device, video, false)
                .map_err(|error| anyhow!(error))
                .context("start the software screen-share encoder")?;
            hardware_sdr = false;
            announced = false;
            if av1 {
                av1 = false;
                control.av1_failed.store(true, Ordering::Relaxed);
                control.use_av1.store(false, Ordering::Relaxed);
            }
            applied_bitrate = video.bitrate;
            continue;
        }
        result
            .map_err(|error| anyhow!(error))
            .context("encode the captured screen frame")?;
        let slot = Arc::new(slot);
        for mut sample in samples {
            if av1 {
                let Some((header, picture)) = crate::social_screen_rtc::inspect_av1(&sample.data)
                else {
                    continue;
                };
                if let Some(header) = header {
                    sequence_header = header.to_vec();
                }
                if !picture {
                    continue;
                }
                if header.is_some() {
                    sample.is_keyframe = true;
                } else if (sample.is_keyframe || !announced) && !sequence_header.is_empty() {
                    let mut data = sequence_header.clone();
                    data.extend_from_slice(&sample.data);
                    if crate::social_screen_rtc::av1_frame(&data).is_none() {
                        continue;
                    }
                    sample.data = data;
                    sample.is_keyframe = true;
                }
                if !announced && sequence_header.is_empty() {
                    continue;
                }
            } else if !hdr10 && !sample.is_keyframe {
                sample.is_keyframe = sample
                    .data
                    .windows(4)
                    .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5);
            }
            if !announced {
                tracing::info!(
                    width = video.width,
                    height = video.height,
                    fps = video.fps,
                    bitrate = applied_bitrate,
                    av1,
                    hdr10,
                    "screen encoder started"
                );
                call_sender
                    .try_send(SocialCallCommand::ScreenStarted {
                        width: video.width,
                        height: video.height,
                        hdr10,
                        av1,
                        control: Arc::clone(&control),
                    })
                    .context("queue screen-share start")?;
                push_social_call_event(SocialCallEvent::ScreenShareMode { hdr10, av1 });
                announced = true;
            }
            let timestamp_ms = i64::try_from(sample.timestamp.as_millis()).unwrap_or(i64::MAX);
            let mut command = SocialCallCommand::ScreenFrame {
                data: sample.data,
                width: video.width,
                height: video.height,
                timestamp_ms,
                key_frame: sample.is_keyframe,
                slot: Arc::clone(&slot),
            };
            // Backpressure drops raw captures before encoding. Dropping an
            // encoded reference frame would corrupt later frames in the GOP.
            loop {
                match call_sender.try_send(command) {
                    Ok(()) => break,
                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => return Ok(()),
                    Err(tokio::sync::mpsc::error::TrySendError::Full(pending)) => {
                        command = pending;
                        if !matches!(
                            stop_rx.recv_timeout(Duration::from_millis(5)),
                            Err(mpsc::RecvTimeoutError::Timeout)
                        ) {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

fn video_config(quality: &str) -> VideoConfig {
    match quality {
        "Gaming · 1080p 60 FPS" | "1080p 60 FPS" => VideoConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 16_000_000,
            keyframe_interval: 60,
        },
        "1440p 60 FPS" => VideoConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            bitrate: 18_000_000,
            keyframe_interval: 60,
        },
        "Desktop · 1440p 30 FPS" | "1440p 30 FPS" => VideoConfig {
            width: 2560,
            height: 1440,
            fps: 30,
            bitrate: 12_000_000,
            keyframe_interval: 30,
        },
        "1080p 30 FPS" => VideoConfig {
            width: 1920,
            height: 1080,
            fps: 30,
            bitrate: 10_000_000,
            keyframe_interval: 30,
        },
        _ => VideoConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 6_000_000,
            keyframe_interval: 30,
        },
    }
}

#[cfg(test)]
mod tests {
    use std::{
        thread,
        time::{Duration, Instant},
    };

    use super::{SocialScreenShare, video_config};
    use crate::social_video_decoder::SocialVideoReceiver;
    use crate::{SOCIAL_CALL_EVENTS, SocialCallCommand, SocialCallEvent};

    #[test]
    fn screen_share_profiles_map_to_native_encoder_settings() {
        let hd = video_config("720p 30 FPS");
        assert_eq!((hd.width, hd.height, hd.fps), (1280, 720, 30));
        let full_hd = video_config("1080p 30 FPS");
        assert_eq!(
            (full_hd.width, full_hd.height, full_hd.fps),
            (1920, 1080, 30)
        );
        let smooth = video_config("1080p 60 FPS");
        assert_eq!((smooth.width, smooth.height, smooth.fps), (1920, 1080, 60));
        let desktop = video_config("1440p 30 FPS");
        assert_eq!(
            (desktop.width, desktop.height, desktop.fps),
            (2560, 1440, 30)
        );
        let desktop_smooth = video_config("1440p 60 FPS");
        assert_eq!(
            (
                desktop_smooth.width,
                desktop_smooth.height,
                desktop_smooth.fps
            ),
            (2560, 1440, 60)
        );
    }

    #[test]
    #[ignore = "requires a live Windows display and Media Foundation"]
    fn windows_screen_share_captures_encodes_and_decodes_a_frame() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        check_screen_share("720p 30 FPS", (1280, 720), false);
        check_screen_share("1080p 30 FPS", (1920, 1080), false);
        check_screen_share("Gaming · 1080p 60 FPS", (1920, 1080), false);
        check_screen_share("Desktop · 1440p 30 FPS", (2560, 1440), false);
    }

    #[test]
    #[ignore = "requires a live Windows display, a hardware AV1 encoder, and an AV1 decoder"]
    fn windows_screen_share_switches_to_hardware_av1() {
        let _ = tracing_subscriber::fmt().with_test_writer().try_init();
        assert!(super::MfH264Encoder::av1_call_available());
        assert!(crate::social_video_decoder::av1_decoder_available());
        check_screen_share("720p 30 FPS", (1280, 720), true);
        check_screen_share("1080p 30 FPS", (1920, 1080), true);
        check_screen_share("Gaming · 1080p 60 FPS", (1920, 1080), true);
        check_screen_share("Desktop · 1440p 30 FPS", (2560, 1440), true);
        check_screen_share("1440p 60 FPS", (2560, 1440), true);
    }

    fn check_screen_share(quality: &str, dimensions: (u32, u32), want_av1: bool) {
        if let Ok(mut events) = SOCIAL_CALL_EVENTS
            .get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
            .lock()
        {
            events.clear();
        }

        let (commands, mut receiver) = tokio::sync::mpsc::channel(32);
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("start the test runtime");
        let share = SocialScreenShare::start(commands, quality, false)
            .expect("start native Windows screen sharing");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut decoder = None;
        let mut encoded_frames = 0;
        let mut decoded_frame = None;

        while Instant::now() < deadline && decoded_frame.is_none() {
            let stopped = match runtime.block_on(async {
                tokio::time::timeout(Duration::from_millis(250), receiver.recv()).await
            }) {
                Ok(Some(SocialCallCommand::ScreenStarted {
                    width,
                    height,
                    hdr10,
                    av1,
                    control,
                })) => {
                    assert!(!hdr10);
                    if want_av1 && !av1 {
                        assert!(
                            !control.av1_failed.load(super::Ordering::Relaxed),
                            "hardware AV1 fell back to H.264"
                        );
                        control.use_av1.store(true, super::Ordering::Relaxed);
                    } else {
                        decoder = Some(
                            if av1 {
                                SocialVideoReceiver::start_av1(width, height)
                            } else {
                                SocialVideoReceiver::start(width, height)
                            }
                            .expect("start the Windows Media Foundation screen decoder"),
                        );
                    }
                    false
                }
                Ok(Some(SocialCallCommand::ScreenFrame {
                    data,
                    width,
                    height,
                    timestamp_ms,
                    key_frame,
                    ..
                })) => {
                    assert!(
                        !data.is_empty(),
                        "the H.264 encoder returned an empty packet"
                    );
                    assert_eq!((width, height), dimensions);
                    assert!(timestamp_ms >= 0);
                    let _ = key_frame;
                    encoded_frames += 1;
                    if let Some(decoder) = &decoder {
                        decoder.submit(data, Some(timestamp_ms), Instant::now());
                    }
                    false
                }
                Ok(Some(SocialCallCommand::ScreenStopped)) | Ok(None) => true,
                Ok(Some(_)) | Err(_) => false,
            };

            thread::sleep(Duration::from_millis(20));
            if let Ok(mut events) = SOCIAL_CALL_EVENTS
                .get_or_init(|| std::sync::Mutex::new(std::collections::VecDeque::new()))
                .lock()
            {
                while let Some(event) = events.pop_front() {
                    match event {
                        SocialCallEvent::RemoteFrame {
                            rgba,
                            width,
                            height,
                        } => decoded_frame = Some((rgba, width, height)),
                        SocialCallEvent::ScreenShareFailed(detail) => {
                            panic!("native screen-share pipeline failed: {detail}");
                        }
                        SocialCallEvent::Av1ReceiveFailed(detail) => {
                            panic!("native AV1 decoding failed: {detail}");
                        }
                        SocialCallEvent::Status(status) => {
                            assert!(
                                !status.contains("stopped:"),
                                "native screen-share pipeline failed: {status}"
                            );
                        }
                        _ => {}
                    }
                }
            }
            assert!(
                !stopped,
                "screen sharing stopped before a frame was decoded"
            );
        }

        drop(share);
        drop(decoder);
        // Wait for the capture worker to release its desktop before the next profile.
        while let Ok(Some(_)) = runtime
            .block_on(async { tokio::time::timeout(Duration::from_secs(1), receiver.recv()).await })
        {
        }
        let (rgba, width, height) = decoded_frame
            .expect("Windows capture produced no frame that Media Foundation could decode");
        assert!(encoded_frames > 0);
        assert_eq!((width, height), dimensions);
        assert_eq!(rgba.len(), (width * height * 4) as usize);
        assert!(rgba.chunks_exact(4).any(|pixel| pixel[..3] != [0, 0, 0]));
    }
}
