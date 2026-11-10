use std::{
    cell::Cell,
    mem::ManuallyDrop,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, anyhow};
use windows::{
    Win32::{
        Media::MediaFoundation::{
            CODECAPI_AVLowLatencyMode, ICodecAPI, IMF2DBuffer2, IMFActivate, IMFTransform,
            MF_E_BUFFERTOOSMALL, MF_E_NO_MORE_TYPES, MF_E_NOTACCEPTING,
            MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE, MF_LOW_LATENCY,
            MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
            MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION, MF2DBuffer_LockFlags_Read,
            MFCreate2DMediaBuffer, MFCreateAlignedMemoryBuffer, MFCreateMediaType,
            MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_FULL, MFShutdown,
            MFStartup, MFT_CATEGORY_VIDEO_DECODER, MFT_ENUM_FLAG_SORTANDFILTER,
            MFT_ENUM_FLAG_SYNCMFT, MFT_INPUT_STREAM_INFO, MFT_MESSAGE_COMMAND_FLUSH,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM,
            MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE,
            MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_AV1, MFVideoFormat_H264_ES,
            MFVideoFormat_HEVC, MFVideoFormat_HEVC_ES, MFVideoFormat_NV12, MFVideoFormat_P010,
        },
        System::{
            Com::{
                CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
                CoTaskMemFree,
            },
            Variant::VARIANT,
        },
    },
    core::{GUID, Interface},
};

use crate::social_hdr_view::{HdrPresenter, HdrWindow};
use crate::{SocialCallEvent, push_social_call_event};

#[path = "social_playout.rs"]
mod playout;
use playout::PlayoutClock;
#[path = "social_av1.rs"]
mod av1_decoder;

const CMS_H264_DECODER_MFT: GUID = GUID::from_u128(0x62ce7e72_4c71_4d20_b15d_452831a87d9d);

struct Packet {
    frame: Vec<u8>,
    timestamp_ms: Option<i64>,
    received: Instant,
    reset: bool,
}

struct Presentation {
    deadline: Instant,
    timestamp_ms: Option<i64>,
    received: Instant,
    decode_time: Duration,
    rgba: Vec<u8>,
}

pub struct SocialVideoReceiver {
    frames: SyncSender<Packet>,
    needs_keyframe: Cell<bool>,
    stopped: Arc<AtomicBool>,
    hdr10: bool,
    av1: bool,
    _hdr_window: Option<HdrWindow>,
}

impl SocialVideoReceiver {
    pub fn start(width: u32, height: u32) -> anyhow::Result<Self> {
        Self::start_with_view(width, height, None, false)
    }

    pub fn start_av1(width: u32, height: u32) -> anyhow::Result<Self> {
        Self::start_with_view(width, height, None, true)
    }

    pub fn start_hdr10(width: u32, height: u32, owner: isize) -> anyhow::Result<Self> {
        Self::start_with_view(
            width,
            height,
            Some(HdrWindow::open(owner, width, height)?),
            false,
        )
    }

    fn start_with_view(
        width: u32,
        height: u32,
        window: Option<HdrWindow>,
        av1: bool,
    ) -> anyhow::Result<Self> {
        let hwnd = window.as_ref().map(HdrWindow::handle);
        let hdr10 = hwnd.is_some();
        // Leave room for the jitter buffer and a short burst from the relay
        // without dropping reference frames and waiting for the next IDR.
        let (frames, receiver) = mpsc::sync_channel::<Packet>(12);
        let stopped = Arc::new(AtomicBool::new(false));
        let (decoded, presentation) = mpsc::sync_channel::<Presentation>(4);
        if !hdr10 {
            let stopped = Arc::clone(&stopped);
            thread::Builder::new()
                .name("zink-social-video-present".to_owned())
                .spawn(move || {
                    let mut first = None;
                    let mut previous: Option<(Instant, Instant, Option<i64>)> = None;
                    let mut next_log = Instant::now();
                    while let Ok(frame) = presentation.recv() {
                        wait_until(frame.deadline);
                        if stopped.load(Ordering::Acquire) {
                            break;
                        }
                        let now = Instant::now();
                        let started = *first.get_or_insert(now);
                        if let Some((last, last_received, last_timestamp)) =
                            previous.replace((now, frame.received, frame.timestamp_ms))
                            && now >= next_log
                            && now.duration_since(last) >= Duration::from_millis(80)
                        {
                            tracing::info!(
                                share_ms = now.duration_since(started).as_millis() as u64,
                                gap_ms = now.duration_since(last).as_millis() as u64,
                                source_gap_ms = ?frame.timestamp_ms.zip(last_timestamp).map(|(now, last)| now.saturating_sub(last)),
                                arrival_gap_ms = frame.received.saturating_duration_since(last_received).as_millis() as u64,
                                decode_ms = frame.decode_time.as_millis() as u64,
                                late_ms = now.saturating_duration_since(frame.deadline).as_millis() as u64,
                                "screen viewer frame gap"
                            );
                            next_log = now + Duration::from_secs(1);
                        }
                        push_social_call_event(SocialCallEvent::RemoteFrame {
                            rgba: frame.rgba,
                            width,
                            height,
                        });
                    }
                })
                .context("start the remote screen presenter")?;
        }
        let worker_stopped = Arc::clone(&stopped);
        thread::Builder::new()
            .name("zink-social-video-decode".to_owned())
            .spawn(move || {
                let result = (|| -> anyhow::Result<()> {
                    let mut decoder = ScreenDecoder::new(width, height, hwnd, av1)?;
                    let mut clock = PlayoutClock::new(av1 && (width, height) == (1280, 720));
                    let mut av1_errors = 0;
                    let mut waiting_for_keyframe = false;
                    let mut decoded_picture = false;
                    while let Ok(packet) = receiver.recv() {
                        if worker_stopped.load(Ordering::Acquire) {
                            break;
                        }
                        if packet.reset {
                            decoder.restart()?;
                            clock.reset();
                        }
                        if waiting_for_keyframe {
                            if crate::social_screen_rtc::av1_sequence_header(&packet.frame).is_none() {
                                continue;
                            }
                            waiting_for_keyframe = false;
                            clock.reset();
                        }
                        let (deadline, duration) =
                            clock.schedule(packet.timestamp_ms, packet.received, Instant::now());
                        if hdr10 {
                            // The native HDR presenter runs inside the decoder.
                            wait_until(deadline);
                            if worker_stopped.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        let decode_started = Instant::now();
                        match decoder.decode(&packet.frame, duration) {
                            Ok(Some(rgba)) => {
                                decoded_picture = true;
                                av1_errors = 0;
                                // Only a decoded display frame can be dropped here.
                                let _ = decoded.try_send(Presentation {
                                    deadline,
                                    timestamp_ms: packet.timestamp_ms,
                                    received: packet.received,
                                    decode_time: decode_started.elapsed(),
                                    rgba,
                                });
                            }
                            Ok(None) => {}
                            Err(error) if av1 && decoded_picture && av1_errors < 2 && (matches!(decoder, ScreenDecoder::RustAv1(_)) || error.chain().any(|cause|
                                cause.downcast_ref::<windows::core::Error>()
                                    .is_some_and(|error| error.code() == MF_E_BUFFERTOOSMALL)
                            )) => {
                                av1_errors += 1;
                                tracing::info!(width, height, %error, "recovering AV1 at the next keyframe");
                                decoder.restart()?;
                                waiting_for_keyframe = true;
                            }
                            Err(error) => return Err(error),
                        }
                    }
                    Ok(())
                })();
                if let Err(error) = result
                    && !worker_stopped.load(Ordering::Acquire)
                {
                    if hdr10 {
                        push_social_call_event(SocialCallEvent::HdrReceiveFailed(format!(
                            "{error:#}"
                        )));
                    } else if av1 {
                        push_social_call_event(SocialCallEvent::Av1ReceiveFailed(format!(
                            "{error:#}"
                        )));
                    } else {
                        push_social_call_event(SocialCallEvent::Status(format!(
                            "Remote screen decoder stopped: {error:#}"
                        )));
                    }
                }
            })
            .context("start the remote screen decoder")?;
        Ok(Self {
            frames,
            needs_keyframe: Cell::new(false),
            stopped,
            hdr10,
            av1,
            _hdr_window: window,
        })
    }

    pub fn submit(&self, frame: Vec<u8>, timestamp_ms: Option<i64>, received: Instant) {
        let reset = self.needs_keyframe.get();
        let can_resume = || {
            if self.av1 {
                crate::social_screen_rtc::av1_sequence_header(&frame).is_some()
            } else {
                frame.windows(4).any(|nal| {
                    nal[..3] == [0, 0, 1]
                        && if self.hdr10 {
                            matches!((nal[3] >> 1) & 0x3f, 16..=21)
                        } else {
                            nal[3] & 0x1f == 5
                        }
                })
            }
        };
        if reset && !can_resume() {
            return;
        }
        self.needs_keyframe.set(
            self.frames
                .try_send(Packet {
                    frame,
                    timestamp_ms,
                    received,
                    reset,
                })
                .is_err(),
        );
    }

    pub fn is_hdr10(&self) -> bool {
        self.hdr10
    }

    pub fn is_av1(&self) -> bool {
        self.av1
    }
}

fn wait_until(deadline: Instant) {
    let delay = deadline.saturating_duration_since(Instant::now());
    if !delay.is_zero() {
        thread::sleep(delay);
    }
}

enum ScreenDecoder {
    Windows(H264Decoder),
    RustAv1(av1_decoder::Av1Decoder),
}

impl ScreenDecoder {
    fn new(width: u32, height: u32, hwnd: Option<isize>, av1: bool) -> anyhow::Result<Self> {
        // Some Windows AV1 transforms fail on 720p while allocating their own output.
        if !av1 || (width, height) != (1280, 720) {
            match H264Decoder::new(width, height, hwnd, av1) {
                Ok(decoder) => return Ok(Self::Windows(decoder)),
                Err(error) if av1 => tracing::debug!(%error, "using the Rust AV1 decoder"),
                Err(error) => return Err(error),
            }
        }
        av1_decoder::Av1Decoder::new(width, height).map(Self::RustAv1)
    }

    fn restart(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Windows(decoder) => decoder.restart(),
            Self::RustAv1(decoder) => {
                decoder.restart();
                Ok(())
            }
        }
    }

    fn decode(&mut self, data: &[u8], duration: Duration) -> anyhow::Result<Option<Vec<u8>>> {
        match self {
            Self::Windows(decoder) => decoder.decode(data, duration),
            Self::RustAv1(decoder) => decoder.decode(data),
        }
    }
}

impl Drop for SocialVideoReceiver {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
    }
}

struct H264Decoder {
    transform: IMFTransform,
    width: u32,
    height: u32,
    sample_time: i64,
    output_stride: usize,
    output_height: usize,
    coded_width: u32,
    av1: bool,
    output_capacity: u32,
    av1_output: Av1Output,
    hdr_window: Option<isize>,
    presenter: Option<HdrPresenter>,
    hdr_rate: crate::social_screen_share::ShareRate,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Av1Output {
    Default,
    Surface,
    Linear,
}

fn screen_buffer(
    size: u32,
    alignment: u32,
) -> windows::core::Result<windows::Win32::Media::MediaFoundation::IMFMediaBuffer> {
    unsafe {
        if alignment == 0 {
            MFCreateMemoryBuffer(size)
        } else {
            MFCreateAlignedMemoryBuffer(size, alignment.next_power_of_two() - 1)
        }
    }
}

impl H264Decoder {
    fn restart(&self) -> anyhow::Result<()> {
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        Ok(())
    }

    fn new(width: u32, height: u32, hdr_window: Option<isize>, av1: bool) -> anyhow::Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("start Media Foundation")?;
            let hdr10 = hdr_window.is_some();
            let transform = if hdr10 || av1 {
                create_extended_decoder(av1)?
            } else {
                CoCreateInstance(&CMS_H264_DECODER_MFT, None, CLSCTX_INPROC_SERVER)
                    .context("create the Windows H.264 decoder")?
            };
            if let Ok(attributes) = transform.GetAttributes() {
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
            }
            if let Ok(codec) = transform.cast::<ICodecAPI>() {
                let enable = if hdr10 {
                    VARIANT::from(true)
                } else {
                    VARIANT::from(1_u32)
                };
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &enable);
            }
            configure_decoder(&transform, width, height, hdr10, av1)?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("begin H.264 decoding")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start the H.264 stream")?;
            let mut decoder = Self {
                transform,
                width,
                height,
                sample_time: 0,
                output_stride: width as usize,
                output_height: height as usize,
                coded_width: width,
                av1,
                output_capacity: 0,
                av1_output: Av1Output::Default,
                hdr_window,
                presenter: None,
                hdr_rate: Default::default(),
            };
            decoder.update_output_layout()?;
            Ok(decoder)
        }
    }

    fn update_output_layout(&mut self) -> anyhow::Result<()> {
        let format = unsafe { self.transform.GetOutputCurrentType(0)? };
        let size = unsafe { format.GetUINT64(&MF_MT_FRAME_SIZE)? };
        self.coded_width = ((size >> 32) as u32).max(self.width);
        self.output_height = (size as u32).max(self.height) as usize;
        anyhow::ensure!(
            self.coded_width <= 8192 && self.output_height <= 4320,
            "invalid coded screen-share dimensions"
        );
        let row_bytes = self.coded_width * if self.hdr_window.is_some() { 2 } else { 1 };
        self.output_stride = unsafe { format.GetUINT32(&MF_MT_DEFAULT_STRIDE) }
            .unwrap_or(row_bytes)
            .max(row_bytes) as usize;
        self.presenter = None;
        Ok(())
    }

    fn decode(&mut self, frame: &[u8], duration: Duration) -> anyhow::Result<Option<Vec<u8>>> {
        if frame.is_empty() {
            return Ok(None);
        }
        unsafe {
            // RTP removes temporal delimiters. Restore the AV1 temporal unit
            // boundary expected by the Windows elementary stream decoder.
            let delimiter: &[u8] = if self.av1 && frame[0] & 0x78 != 0x10 {
                &[0x12, 0]
            } else {
                &[]
            };
            let length = u32::try_from(frame.len() + delimiter.len())
                .context("screen video packet is too large")?;
            let mut input = MFT_INPUT_STREAM_INFO::default();
            self.transform.GetInputStreamInfo(0, &mut input)?;
            let buffer = screen_buffer(length.max(input.cbSize), input.cbAlignment)?;
            let mut destination = std::ptr::null_mut();
            buffer.Lock(&raw mut destination, None, None)?;
            std::ptr::copy_nonoverlapping(delimiter.as_ptr(), destination, delimiter.len());
            std::ptr::copy_nonoverlapping(
                frame.as_ptr(),
                destination.add(delimiter.len()),
                frame.len(),
            );
            buffer.Unlock()?;
            buffer.SetCurrentLength(length)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.sample_time)?;
            let ticks = (duration.as_nanos() / 100) as i64;
            sample.SetSampleDuration(ticks)?;
            self.sample_time += ticks;

            let mut pending = None;
            for _ in 0..8 {
                match self.transform.ProcessInput(0, &sample, 0) {
                    Ok(()) => return Ok(self.pull_output()?.or(pending)),
                    Err(error) if error.code() == MF_E_NOTACCEPTING => {
                        pending = self.pull_output()?.or(pending);
                    }
                    Err(error) => {
                        return Err(anyhow!(error)).context("submit compressed screen frame");
                    }
                }
            }
            Err(anyhow!(
                "the H.264 decoder would not accept input after draining output"
            ))
        }
    }

    unsafe fn pull_output(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        for _ in 0..4 {
            match unsafe { self.read_output() } {
                Err(error)
                    if error
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|error| error.code() == MF_E_TRANSFORM_STREAM_CHANGE) =>
                {
                    select_decoder_output(&self.transform, self.hdr_window.is_some())?;
                    self.update_output_layout()?;
                    self.output_capacity = 0;
                    self.av1_output = Av1Output::Default;
                }
                Err(error)
                    if error
                        .downcast_ref::<windows::core::Error>()
                        .is_some_and(|error| error.code() == MF_E_BUFFERTOOSMALL) =>
                {
                    let flags = unsafe { self.transform.GetOutputStreamInfo(0)? }.dwFlags;
                    if self.av1
                        && self.av1_output == Av1Output::Default
                        && flags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 == 0
                    {
                        self.av1_output = Av1Output::Surface;
                        continue;
                    }
                    if self.av1_output == Av1Output::Surface {
                        self.av1_output = Av1Output::Linear;
                        continue;
                    }
                    self.output_capacity = self.output_capacity.saturating_mul(2);
                    if self.output_capacity == 0 || self.output_capacity > 64 * 1024 * 1024 {
                        return Err(error).with_context(|| {
                            format!(
                                "Windows screen decoder rejected output ({}×{}, flags={flags:#x}, buffer={} bytes)",
                                self.width, self.height, self.output_capacity
                            )
                        });
                    }
                }
                result => return result,
            }
        }
        Err(anyhow!(
            "the H.264 decoder repeatedly changed its output layout"
        ))
    }

    unsafe fn read_output(&mut self) -> anyhow::Result<Option<Vec<u8>>> {
        let info = unsafe { self.transform.GetOutputStreamInfo(0)? };
        let layout_size = self.output_stride.saturating_mul(
            self.output_height
                .saturating_add(self.output_height.div_ceil(2)),
        );
        let provides_samples = info.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
        let can_provide_samples =
            info.dwFlags & MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32 != 0;
        let sample = if provides_samples
            || (self.av1 && can_provide_samples && self.av1_output == Av1Output::Default)
        {
            None
        } else if self.av1_output == Av1Output::Surface {
            let sample = unsafe { MFCreateSample()? };
            let buffer = unsafe {
                MFCreate2DMediaBuffer(
                    self.coded_width,
                    self.output_height as u32,
                    MFVideoFormat_NV12.data1,
                    false,
                )?
            };
            unsafe { sample.AddBuffer(&buffer)? };
            Some(sample)
        } else {
            // AV1 transforms may write padded superblock rows even when
            // GetOutputStreamInfo initially reports only the visible size.
            let padded = if self.av1 {
                let stride = self.output_stride.div_ceil(256) * 256;
                let rows = self.output_height.div_ceil(128) * 128;
                stride * (rows + rows.div_ceil(2))
            } else {
                layout_size
            };
            let required = u32::try_from(layout_size.max(padded))
                .context("screen decoder output is too large")?
                .max(info.cbSize)
                .max(self.output_capacity)
                .max(1);
            self.output_capacity = required;
            let sample = unsafe { MFCreateSample()? };
            let buffer = screen_buffer(required, info.cbAlignment)?;
            unsafe { sample.AddBuffer(&buffer)? };
            Some(sample)
        };
        let output = MFT_OUTPUT_DATA_BUFFER {
            dwStreamID: 0,
            pSample: ManuallyDrop::new(sample),
            ..Default::default()
        };
        let mut outputs = [output];
        let mut status = 0;
        let result = unsafe {
            self.transform
                .ProcessOutput(0, &mut outputs, &raw mut status)
        };
        let produced = unsafe { ManuallyDrop::take(&mut outputs[0].pSample) };
        let _events = unsafe { ManuallyDrop::take(&mut outputs[0].pEvents) };
        match result {
            Ok(()) => {
                if outputs[0].dwStatus & 0x300 == MFT_OUTPUT_DATA_BUFFER_NO_SAMPLE.0 as u32 {
                    return Ok(None);
                }
                let Some(sample) = produced else {
                    return Ok(None);
                };
                let contiguous = unsafe { sample.ConvertToContiguousBuffer()? };
                if self.av1
                    && let Ok(surface) = contiguous.cast::<IMF2DBuffer2>()
                {
                    return self.read_av1_surface(&surface);
                }
                let mut source = std::ptr::null_mut();
                let mut length = 0;
                unsafe {
                    contiguous.Lock(&raw mut source, None, Some(&raw mut length))?;
                }
                let bytes = unsafe {
                    std::slice::from_raw_parts(source, usize::try_from(length).unwrap_or_default())
                };
                let frame = if let Some(window) = self.hdr_window {
                    self.present_hdr(window, bytes).map(|()| None)
                } else {
                    let rgba = nv12_to_rgba(
                        bytes,
                        self.width,
                        self.height,
                        self.output_stride,
                        self.output_height,
                    );
                    Ok((!rgba.is_empty()).then_some(rgba))
                };
                unsafe { contiguous.Unlock()? };
                frame
            }
            Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(None),
            Err(error) => Err(anyhow!(error)),
        }
    }

    fn read_av1_surface(&self, surface: &IMF2DBuffer2) -> anyhow::Result<Option<Vec<u8>>> {
        let (mut scanline, mut base) = (std::ptr::null_mut(), std::ptr::null_mut());
        let (mut pitch, mut length) = (0, 0);
        unsafe {
            surface.Lock2DSize(
                MF2DBuffer_LockFlags_Read,
                &mut scanline,
                &mut pitch,
                &mut base,
                &mut length,
            )?;
        }
        let offset = (scanline as usize).checked_sub(base as usize);
        let rgba = match offset {
            Some(offset) if !scanline.is_null() && pitch > 0 && offset < length as usize => {
                let bytes =
                    unsafe { std::slice::from_raw_parts(scanline, length as usize - offset) };
                nv12_to_rgba(
                    bytes,
                    self.width,
                    self.height,
                    pitch as usize,
                    self.output_height,
                )
            }
            _ => Vec::new(),
        };
        unsafe { surface.Unlock2D()? };
        Ok((!rgba.is_empty()).then_some(rgba))
    }

    fn present_hdr(&mut self, window: isize, bytes: &[u8]) -> anyhow::Result<()> {
        let required = self.output_stride * (self.output_height + self.output_height.div_ceil(2));
        anyhow::ensure!(bytes.len() >= required, "the P010 frame was incomplete");
        let first_frame = self.presenter.is_none();
        if first_frame {
            self.presenter = Some(HdrPresenter::new(
                window,
                self.width,
                self.height,
                self.coded_width,
                self.output_height as u32,
            )?);
        }
        self.presenter
            .as_ref()
            .context("create the HDR viewer")?
            .present(bytes, self.output_stride as u32)?;
        if first_frame {
            push_social_call_event(SocialCallEvent::HdrPresented);
        }
        if let Some((fps, _)) = self.hdr_rate.frame(0) {
            push_social_call_event(SocialCallEvent::HdrShareRate(fps));
        }
        Ok(())
    }
}

pub fn hdr_decoder_available() -> bool {
    extended_decoder_available(false)
}

pub fn av1_decoder_available() -> bool {
    // The Rust decoder can receive AV1 even when the optional Windows
    // AV1 extension is absent. Higher resolutions prefer Media Foundation.
    true
}

fn extended_decoder_available(av1: bool) -> bool {
    unsafe {
        if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_err() {
            return false;
        }
        let supported = create_extended_decoder(av1)
            .and_then(|decoder| configure_decoder(&decoder, 1280, 720, !av1, av1))
            .is_ok();
        let _ = MFShutdown();
        supported
    }
}

fn create_extended_decoder(av1: bool) -> anyhow::Result<IMFTransform> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: if av1 {
                MFVideoFormat_AV1
            } else {
                MFVideoFormat_HEVC
            },
        };
        let output = MFT_REGISTER_TYPE_INFO {
            guidMajorType: MFMediaType_Video,
            guidSubtype: if av1 {
                MFVideoFormat_NV12
            } else {
                MFVideoFormat_P010
            },
        };
        let mut activations: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut activations,
            &mut count,
        )?;
        let mut decoder = None;
        if !activations.is_null() {
            for activation in std::slice::from_raw_parts_mut(activations, count as usize) {
                if let Some(activation) = activation.take()
                    && decoder.is_none()
                {
                    decoder = activation.ActivateObject::<IMFTransform>().ok();
                }
            }
            CoTaskMemFree(Some(activations.cast()));
        }
        decoder.context(if av1 {
            "Windows has no AV1 decoder with NV12 output"
        } else {
            "Windows has no HEVC Main10 decoder with P010 output"
        })
    }
}

fn configure_decoder(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    hdr10: bool,
    av1: bool,
) -> anyhow::Result<()> {
    unsafe {
        let input = MFCreateMediaType()?;
        input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        // Zink's native encoder sends Annex-B NAL units (start-code delimited),
        // so advertise an elementary stream rather than AVCC/length-prefixed H.264.
        input.SetGUID(
            &MF_MT_SUBTYPE,
            &if av1 {
                MFVideoFormat_AV1
            } else if hdr10 {
                MFVideoFormat_HEVC_ES
            } else {
                MFVideoFormat_H264_ES
            },
        )?;
        input.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
        input.SetUINT64(&MF_MT_FRAME_RATE, pack_size(30, 1))?;
        input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_size(1, 1))?;
        transform.SetInputType(0, &input, 0)?;
        configure_output(transform, width, height, hdr10)
    }
}

fn configure_output(
    transform: &IMFTransform,
    width: u32,
    height: u32,
    hdr10: bool,
) -> anyhow::Result<()> {
    unsafe {
        let output = MFCreateMediaType()?;
        output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output.SetGUID(
            &MF_MT_SUBTYPE,
            &if hdr10 {
                MFVideoFormat_P010
            } else {
                MFVideoFormat_NV12
            },
        )?;
        output.SetUINT64(&MF_MT_FRAME_SIZE, pack_size(width, height))?;
        output.SetUINT64(&MF_MT_FRAME_RATE, pack_size(30, 1))?;
        output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack_size(1, 1))?;
        transform
            .SetOutputType(0, &output, 0)
            .context("select NV12 output from the Windows H.264 decoder")?;
    }
    Ok(())
}

fn select_decoder_output(transform: &IMFTransform, hdr10: bool) -> anyhow::Result<()> {
    // After parsing the SPS, the decoder may require a coded size (for example
    // 1920×1088) different from the visible image. Accept its proposed layout.
    for index in 0.. {
        let output = match unsafe { transform.GetOutputAvailableType(0, index) } {
            Ok(output) => output,
            Err(error) if error.code() == MF_E_NO_MORE_TYPES => break,
            Err(error) => return Err(error.into()),
        };
        if unsafe { output.GetGUID(&MF_MT_SUBTYPE)? }
            == if hdr10 {
                MFVideoFormat_P010
            } else {
                MFVideoFormat_NV12
            }
        {
            return unsafe { transform.SetOutputType(0, &output, 0) }
                .context("accept the new NV12 layout from the H.264 decoder");
        }
    }
    Err(anyhow!("the H.264 decoder did not offer NV12 output"))
}

fn pack_size(width: u32, height: u32) -> u64 {
    (u64::from(width) << 32) | u64::from(height)
}

fn nv12_to_rgba(
    input: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    storage_height: usize,
) -> Vec<u8> {
    let width = usize::try_from(width).unwrap_or_default();
    let height = usize::try_from(height).unwrap_or_default();
    let y_plane_len = width.saturating_mul(height);
    let uv_start = stride.saturating_mul(storage_height);
    let required = uv_start.saturating_add(stride.saturating_mul(height.div_ceil(2)));
    if width == 0
        || height == 0
        || width % 2 != 0
        || stride < width
        || storage_height < height
        || input.len() < required
    {
        return Vec::new();
    }

    let mut output = vec![0; y_plane_len.saturating_mul(4)];
    for (rows, (luma, chroma)) in output.chunks_mut(width * 8).zip(
        input[..uv_start]
            .chunks(stride * 2)
            .zip(input[uv_start..].chunks(stride)),
    ) {
        let (top, bottom) = rows.split_at_mut(width * 4);
        for (pair, (uv, ys)) in top
            .chunks_exact_mut(8)
            .zip(chroma.chunks_exact(2).zip(luma.chunks_exact(2)))
        {
            write_nv12_pair(pair, ys, nv12_chroma(uv));
        }
        if !bottom.is_empty() {
            for (pair, (uv, ys)) in bottom
                .chunks_exact_mut(8)
                .zip(chroma.chunks_exact(2).zip(luma[stride..].chunks_exact(2)))
            {
                write_nv12_pair(pair, ys, nv12_chroma(uv));
            }
        }
    }
    output
}

#[inline]
fn nv12_chroma(uv: &[u8]) -> [i32; 3] {
    let u = i32::from(uv[0]) - 128;
    let v = i32::from(uv[1]) - 128;
    [409 * v + 128, -100 * u - 208 * v + 128, 516 * u + 128]
}

#[inline]
fn write_nv12_pair(pair: &mut [u8], ys: &[u8], [r, g, b]: [i32; 3]) {
    for (rgba, &y) in pair.chunks_exact_mut(4).zip(ys) {
        let luma = 298 * (i32::from(y) - 16).max(0);
        rgba.copy_from_slice(&[
            ((luma + r) >> 8).clamp(0, 255) as u8,
            ((luma + g) >> 8).clamp(0, 255) as u8,
            ((luma + b) >> 8).clamp(0, 255) as u8,
            255,
        ]);
    }
}

#[cfg(test)]
mod tests {
    use super::{nv12_to_rgba, pack_size};

    #[test]
    fn frame_size_packs_dimensions_for_media_foundation() {
        assert_eq!(pack_size(1_280, 720), (1_280_u64 << 32) | 720);
    }

    #[test]
    fn nv12_conversion_produces_opaque_rgba_pixels() {
        // A 2x2 neutral-chroma NV12 frame with four increasing luma values.
        let nv12 = [16, 82, 145, 235, 128, 128];
        let rgba = nv12_to_rgba(&nv12, 2, 2, 2, 2);
        assert_eq!(
            rgba,
            vec![
                0, 0, 0, 255, 77, 77, 77, 255, 150, 150, 150, 255, 255, 255, 255, 255,
            ]
        );
    }

    #[test]
    fn nv12_conversion_skips_row_and_plane_padding() {
        let padded = [
            16, 82, 0, 0, 145, 235, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 128, 128, 0, 0,
        ];
        let packed = [16, 82, 145, 235, 128, 128];
        assert_eq!(
            nv12_to_rgba(&padded, 2, 2, 4, 4),
            nv12_to_rgba(&packed, 2, 2, 2, 2)
        );
    }
}
