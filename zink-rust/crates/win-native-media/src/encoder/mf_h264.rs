//! Media Foundation H.264 encoder MFT.
//!
//! Enumerates a hardware H.264 encoder (software fallback), binds the shared
//! capture D3D11 device via a DXGI device manager, negotiates types, and runs
//! the async-or-sync MFT drain loop. Captured D3D11 textures are wrapped as
//! DXGI-backed IMFSamples (no CPU readback). Output is emitted as Annex-B
//! `EncodedSample`s.
//!
//! Threading: MF requires MTA. `MfH264Encoder::new` calls `CoInitializeEx`
//! (MTA) + `MFStartup` on the constructing thread; keep the encoder on one
//! thread. The pipeline runs it on a dedicated encode thread.

use std::time::Duration;

use windows::core::{Interface, GUID, PCWSTR};
use windows::Win32::Graphics::Direct3D11::ID3D11Device;
use windows::Win32::Graphics::Direct3D11::ID3D11Texture2D;
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::{EncodedSample, ParameterSets};
use crate::convert::Bgra2Nv12;
use crate::hdr_convert::HdrToP010;
use crate::{PipelineError, Result, VideoConfig};

/// 100ns ticks per second (MF time base).
const HNS_PER_SEC: i64 = 10_000_000;

struct MediaFoundationSession;

impl Drop for MediaFoundationSession {
    fn drop(&mut self) {
        unsafe { let _ = MFShutdown(); }
    }
}

pub struct MfH264Encoder {
    transform: IMFTransform,
    input_stream_id: u32,
    output_stream_id: u32,
    device_manager: IMFDXGIDeviceManager,
    cfg: VideoConfig,
    hdr10: bool,
    av1: bool,
    params: ParameterSets,
    /// Whether the MFT is an async (hardware) MFT driven by events.
    is_async: bool,
    /// The call encoders consume only outputs announced by the async MFT.
    call_encoder: bool,
    event_gen: Option<IMFMediaEventGenerator>,
    started: bool,
    /// Outstanding METransformNeedInput requests the async MFT has issued that
    /// we have not yet satisfied with a ProcessInput. Persisted across encode()
    /// calls so we never drop a request (dropping one deadlocks the MFT).
    pending_input_requests: u32,
    /// BGRA->NV12 converter, present only when the MFT requires NV12 input.
    converter: Option<Bgra2Nv12>,
    hdr_converter: Option<HdrToP010>,
    /// Sample allocator for the hardware D3D11-aware MFT. When present, input
    /// samples come from here (MFT-owned NV12 textures) instead of textures we
    /// create — hardware MFTs reject foreign textures at ProcessInput.
    allocator: Option<IMFVideoSampleAllocatorEx>,
    _media_foundation: MediaFoundationSession,
}

impl MfH264Encoder {
    /// Create the encoder sharing `device`.
    ///
    /// v1 uses the software (sync) H.264 MFT, which works reliably end-to-end.
    /// The hardware (async) MFT for zero-copy NV12 texture input is available via
    /// `new_with(.., true)` but its ProcessInput still rejects our
    /// video-processor NV12 texture (E_UNEXPECTED) — the async D3D11-aware MFT
    /// wants input textures from its own IMFVideoSampleAllocatorEx, not ones we
    /// allocate. Finishing that (M2b) is required for 4K, where the software
    /// path's per-frame GPU readback + CPU encode can't keep up. See
    /// examples/m2b_hw_encode.rs.
    // ponytail: software until the HW allocator path lands; upgrade to
    // new_with(true) once ProcessInput accepts our textures.
    pub fn new(device: &ID3D11Device, cfg: VideoConfig) -> Result<Self> {
        Self::new_with(device, cfg, false)
    }

    /// Create the encoder, explicitly choosing hardware or software.
    pub fn new_with(
        device: &ID3D11Device,
        cfg: VideoConfig,
        prefer_hardware: bool,
    ) -> Result<Self> {
        Self::create(device, cfg, prefer_hardware, false, false, false)
    }

    /// Encode an FP16 scRGB desktop as HEVC Main10 with HDR10 color signaling.
    pub fn new_hdr10(device: &ID3D11Device, cfg: VideoConfig) -> Result<Self> {
        Self::create(device, cfg, true, true, false, false)
    }

    /// Encode tone-mapped BGRA as the limited-range SDR expected by the call viewer.
    pub fn new_sdr_studio(device: &ID3D11Device, cfg: VideoConfig, prefer_hardware: bool) -> Result<Self> {
        Self::create(device, cfg, prefer_hardware, false, true, false)
    }

    pub fn new_av1_call(device: &ID3D11Device, cfg: VideoConfig) -> Result<Self> {
        Self::create(device, cfg, true, false, true, true)
    }

    pub fn av1_call_available() -> bool {
        unsafe {
            let initialized = CoInitializeEx(None, COINIT_MULTITHREADED).is_ok();
            let supported = if MFStartup(MF_VERSION, MFSTARTUP_FULL).is_ok() {
                let _session = MediaFoundationSession;
                enumerate_encoder(true, MFVideoFormat_AV1).is_ok()
            } else {
                false
            };
            if initialized {
                windows::Win32::System::Com::CoUninitialize();
            }
            supported
        }
    }

    fn create(device: &ID3D11Device, cfg: VideoConfig, prefer_hardware: bool, hdr10: bool, studio_sdr: bool, av1: bool) -> Result<Self> {
        unsafe {
            // MF needs MTA. Ignore RPC_E_CHANGED_MODE if already initialized.
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            MFStartup(MF_VERSION, MFSTARTUP_FULL)?;
            let media_foundation = MediaFoundationSession;

            let device_manager = create_device_manager(device)?;
            let (transform, activate) = enumerate_encoder(prefer_hardware, encoded_subtype(hdr10, av1))?;
            let (input_id, output_id) = stream_ids(&transform)?;

            // Detect async (hardware) MFT via attribute.
            let is_async = mft_is_async(&transform);

            // Hand the D3D device manager only to hardware MFTs (software MFTs
            // take CPU buffers and can choke on a D3D manager).
            // The live call path feeds system-memory NV12/P010. This avoids
            // requiring the encoder and the captured display to share a GPU.
            if is_async && !hdr10 && !studio_sdr {
                let _ = transform.ProcessMessage(
                    MFT_MESSAGE_SET_D3D_MANAGER,
                    std::mem::transmute::<IMFDXGIDeviceManager, usize>(device_manager.clone()),
                );
            }

            if let Ok(codec) = transform.cast::<ICodecAPI>() {
                let gop = windows::Win32::System::Variant::VARIANT::from(cfg.keyframe_interval.max(1));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &gop);
                if studio_sdr || hdr10 {
                    use windows::Win32::System::Variant::VARIANT;
                    let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                    let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &VARIANT::from(0_u32));
                    if studio_sdr && !av1 {
                        let _ = codec.SetValue(&CODECAPI_AVEncH264CABACEnable, &VARIANT::from(true));
                    }
                }
            }

            if is_async {
                // Unlock async MFT so we may use event-driven processing, and
                // request low-latency mode (Intel Quick Sync MFT wants this for
                // real-time D3D input).
                if let Ok(attrs) = transform.GetAttributes() {
                    let _ = attrs.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1);
                    let _ = attrs.SetUINT32(&MF_LOW_LATENCY, 1);
                }
            }

            // Output type MUST be set before input type for encoders.
            set_output_type(&transform, output_id, &cfg, hdr10, av1)?;
            // Hardware (async) MFTs take D3D textures only as NV12 — they may
            // *accept* ARGB32 during negotiation but reject a D3D ARGB texture at
            // ProcessInput. So force NV12 for the async path; software MFTs can
            // take ARGB32 directly from a CPU buffer.
            let input_format = set_input_type(&transform, input_id, &cfg, is_async || studio_sdr, hdr10)?;
            if let (true, Ok(codec)) = (studio_sdr, transform.cast::<ICodecAPI>()) {
                use windows::Win32::System::Variant::VARIANT;
                let mode = VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32);
                let mean = VARIANT::from(cfg.bitrate);
                // Quarter-second buffer allows key frames to be larger without
                // letting motion fill the upload queue with seconds of data.
                let buffer = VARIANT::from((cfg.bitrate / 32).max(1));
                for (property, value) in [
                    (CODECAPI_AVEncCommonRateControlMode, &mode),
                    (CODECAPI_AVEncCommonMeanBitRate, &mean),
                    (CODECAPI_AVEncCommonBufferSize, &buffer),
                ] {
                    if let Err(error) = codec.SetValue(&property, value) {
                        tracing::debug!(?property, %error, "screen share encoder rejected rate setting");
                    }
                }
            }
            // Hardware MFTs require NV12; build the on-GPU converter for that path.
            let hdr_converter = if hdr10 {
                Some(HdrToP010::new(device, cfg.width, cfg.height)?)
            } else {
                None
            };
            let converter = if input_format == InputFormat::Nv12 {
                let converter = Bgra2Nv12::new(device, cfg.width, cfg.height)?;
                if studio_sdr {
                    converter.use_sdr_studio_range()?;
                }
                Some(converter)
            } else {
                None
            };

            // Hardware D3D11-aware MFTs demand input samples from their own
            // allocator. Build one bound to the shared device manager, producing
            // NV12 textures with RENDER_TARGET (video-processor writable) +
            // VIDEO_ENCODER bind flags.
            let allocator = if is_async && !studio_sdr && input_format == InputFormat::Nv12 {
                create_input_allocator(&transform, input_id, &device_manager, &cfg).ok()
            } else {
                None
            };

            let event_gen = if is_async {
                Some(transform.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };

            drop(activate); // MFT is created; activate no longer needed.

            Ok(Self {
                transform,
                input_stream_id: input_id,
                output_stream_id: output_id,
                device_manager,
                cfg,
                hdr10,
                av1,
                params: ParameterSets::default(),
                is_async,
                call_encoder: studio_sdr,
                event_gen,
                started: false,
                pending_input_requests: 0,
                converter,
                hdr_converter,
                allocator,
                _media_foundation: media_foundation,
            })
        }
    }

    /// SPS/PPS captured after the first output. Empty until then.
    pub fn parameter_sets(&self) -> &ParameterSets {
        &self.params
    }

    pub fn set_sdr_call_bitrate(&self, bitrate: u32) -> Result<()> {
        let codec = self.transform.cast::<ICodecAPI>()?;
        let value = windows::Win32::System::Variant::VARIANT::from(bitrate);
        unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &value)?; }
        Ok(())
    }

    pub fn request_keyframe(&self) -> Result<()> {
        let codec = self.transform.cast::<ICodecAPI>()?;
        let value = windows::Win32::System::Variant::VARIANT::from(1_u32);
        unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &value)? };
        Ok(())
    }

    fn ensure_started(&mut self) -> Result<()> {
        if self.started {
            return Ok(());
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            self.transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
        }
        self.started = true;
        Ok(())
    }

    /// Encode one captured texture at `timestamp`. Pushes any produced encoded
    /// samples to `out`. May produce zero, one, or more samples per input.
    pub fn encode(
        &mut self,
        texture: &ID3D11Texture2D,
        timestamp: Duration,
        out: &mut Vec<EncodedSample>,
    ) -> Result<()> {
        self.ensure_started()?;
        // Build the input sample. Software (sync) MFT path: convert BGRA->NV12
        // on-GPU, read back to a CPU buffer, wrap in an MF memory buffer.
        // Hardware (async) path: wrap the GPU NV12 texture directly (zero-copy).
        let sample = if let Some(converter) = self.hdr_converter.as_ref() {
            let p010 = converter.convert(texture)?;
            self.wrap_cpu_nv12(&p010, timestamp)?
        } else { match (self.converter.as_mut(), self.is_async && !self.call_encoder) {
            (Some(conv), false) => {
                let nv12_cpu = conv.convert_to_cpu(texture)?;
                self.wrap_cpu_nv12(&nv12_cpu, timestamp)?
            }
            (Some(_), true) => {
                // Hardware path: get an MFT-owned NV12 sample from the allocator,
                // blit the captured BGRA into its texture, feed that sample.
                self.build_allocated_sample(texture, timestamp)?
            }
            (None, _) => self.wrap_texture(texture, timestamp)?,
        }};

        if self.is_async {
            self.encode_async(sample, out)
        } else {
            self.encode_sync(sample, out)
        }
    }

    /// Flush at end of stream; drains remaining output.
    pub fn drain(&mut self, out: &mut Vec<EncodedSample>) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }

        if self.is_async {
            // Async MFT signals completion by emitting METransformDrainComplete
            // after all outputs; pull on each HaveOutput until then.
            let gen = self
                .event_gen
                .clone()
                .expect("async MFT has event generator");
            loop {
                let event = unsafe { gen.GetEvent(MF_EVENT_FLAG_NONE)? };
                let met = unsafe { event.GetType()? } as i32;
                if met == METransformHaveOutput.0 {
                    self.pull_output(out)?;
                } else if met == METransformDrainComplete.0 {
                    break;
                } else if met == METransformNeedInput.0 {
                    // Ignore during drain.
                }
            }
        } else {
            // Sync MFT: pull until it needs more input.
            loop {
                match self.pull_output(out) {
                    Ok(true) => continue,
                    Ok(false) => break,
                    Err(e) => return Err(e),
                }
            }
        }
        Ok(())
    }

    fn wrap_texture(&self, texture: &ID3D11Texture2D, timestamp: Duration) -> Result<IMFSample> {
        unsafe {
            let buffer = MFCreateDXGISurfaceBuffer(&ID3D11Texture2D::IID, texture, 0, false)?;
            // The hardware encoder needs a correct current length. A DXGI
            // surface buffer's GetMaxLength is unreliable for NV12; the real
            // packed size comes from IMF2DBuffer::GetContiguousLength. Set that
            // as the current length or the MFT rejects the sample (E_UNEXPECTED).
            if let Ok(two_d) = buffer.cast::<IMF2DBuffer>() {
                if let Ok(len) = two_d.GetContiguousLength() {
                    let _ = buffer.SetCurrentLength(len);
                }
            } else if let Ok(len) = buffer.GetMaxLength() {
                let _ = buffer.SetCurrentLength(len);
            }
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let hns = (timestamp.as_nanos() as i64) / 100;
            sample.SetSampleTime(hns)?;
            let frame_dur = HNS_PER_SEC / self.cfg.fps.max(1) as i64;
            sample.SetSampleDuration(frame_dur)?;
            Ok(sample)
        }
    }

    /// Hardware path: allocate an MFT-owned NV12 sample, blit BGRA into its
    /// texture via the video processor, and set its time/duration.
    fn build_allocated_sample(
        &mut self,
        bgra: &ID3D11Texture2D,
        timestamp: Duration,
    ) -> Result<IMFSample> {
        let allocator = self
            .allocator
            .as_ref()
            .expect("allocator present on hardware path")
            .clone();
        let conv = self.converter.as_mut().expect("converter on hardware path");
        unsafe {
            let sample = allocator.AllocateSample()?;
            // Extract the D3D11 texture backing the sample's buffer.
            let buffer = sample.GetBufferByIndex(0)?;
            let dxgi_buf = buffer.cast::<IMFDXGIBuffer>()?;
            let mut tex_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
            dxgi_buf.GetResource(&ID3D11Texture2D::IID, &mut tex_ptr)?;
            let dst = ID3D11Texture2D::from_raw(tex_ptr);
            let slice = dxgi_buf.GetSubresourceIndex().unwrap_or(0);

            // Blit the captured BGRA into the MFT-owned NV12 texture's slice.
            conv.convert_into(bgra, &dst, slice)?;

            let hns = (timestamp.as_nanos() as i64) / 100;
            sample.SetSampleTime(hns)?;
            sample.SetSampleDuration(HNS_PER_SEC / self.cfg.fps.max(1) as i64)?;
            Ok(sample)
        }
    }

    /// Wrap a CPU NV12 byte buffer as an IMFSample for the software MFT.
    fn wrap_cpu_nv12(&self, nv12: &[u8], timestamp: Duration) -> Result<IMFSample> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut max_len = 0u32;
            buffer.Lock(&mut ptr, Some(&mut max_len), None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), ptr, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            let hns = (timestamp.as_nanos() as i64) / 100;
            sample.SetSampleTime(hns)?;
            let frame_dur = HNS_PER_SEC / self.cfg.fps.max(1) as i64;
            sample.SetSampleDuration(frame_dur)?;
            Ok(sample)
        }
    }

    fn encode_sync(&mut self, sample: IMFSample, out: &mut Vec<EncodedSample>) -> Result<()> {
        unsafe {
            self.transform
                .ProcessInput(self.input_stream_id, &sample, 0)?;
        }
        loop {
            match self.pull_output(out) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn encode_async(&mut self, sample: IMFSample, out: &mut Vec<EncodedSample>) -> Result<()> {
        let gen = self
            .event_gen
            .clone()
            .expect("async MFT has event generator");

        // Async MFTs decouple input and output: they emit METransformNeedInput
        // (a request for a frame) and METransformHaveOutput (a produced sample)
        // independently, and may queue several NeedInput requests ahead. We must
        // never drop a NeedInput or the MFT deadlocks. So: pump events until we
        // have satisfied exactly one input request with our sample; a request
        // already in hand (pending_input_requests) is used immediately without
        // blocking, and any surplus request seen while draining is remembered.
        loop {
            if self.pending_input_requests > 0 {
                unsafe {
                    self.transform
                        .ProcessInput(self.input_stream_id, &sample, 0)?;
                }
                self.pending_input_requests -= 1;
                break;
            }
            let event = if self.hdr10 || self.call_encoder {
                match unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                    Ok(event) => event,
                    Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                    Err(error) => return Err(error.into()),
                }
            } else {
                unsafe { gen.GetEvent(MF_EVENT_FLAG_NONE)? }
            };
            unsafe { event.GetStatus()?.ok()?; }
            let met = unsafe { event.GetType()? } as i32;
            if met == METransformNeedInput.0 {
                self.pending_input_requests += 1;
            } else if met == METransformHaveOutput.0 {
                self.pull_output(out)?;
            }
        }

        if self.hdr10 {
            return Ok(());
        }
        if self.call_encoder {
            return self.poll_sdr_call_output(out);
        }

        // Opportunistically drain any output already queued, without blocking:
        // use MFTransformHasOutput-style non-blocking peek via ProcessOutput.
        loop {
            match self.pull_output(out) {
                Ok(true) => continue,
                Ok(false) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// Collect finished call frames between scheduled captures. GPU drivers
    /// require a HaveOutput event for each ProcessOutput call.
    pub fn poll_sdr_call_output(&mut self, out: &mut Vec<EncodedSample>) -> Result<()> {
        if !self.call_encoder || !self.started {
            return Ok(());
        }
        let Some(gen) = self.event_gen.clone() else {
            return Ok(());
        };
        loop {
            let event = match unsafe { gen.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error.into()),
            };
            unsafe { event.GetStatus()?.ok()?; }
            let kind = unsafe { event.GetType()? } as i32;
            if kind == METransformHaveOutput.0 {
                self.pull_output(out)?;
            } else if kind == METransformNeedInput.0 {
                self.pending_input_requests += 1;
            }
        }
    }

    /// Pull one output sample if available. Returns Ok(true) if a sample was
    /// produced, Ok(false) if the MFT needs more input, Err on real failure.
    fn pull_output(&mut self, out: &mut Vec<EncodedSample>) -> Result<bool> {
        unsafe {
            let stream_info = self.transform.GetOutputStreamInfo(self.output_stream_id)?;

            // For encoders the MFT usually allocates output samples itself
            // (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES). If not, we must allocate.
            let provides_samples = (stream_info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0)
                    as u32)
                != 0;

            let mut output = MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: self.output_stream_id,
                ..Default::default()
            };
            if !provides_samples {
                let sample = MFCreateSample()?;
                let buf = MFCreateMemoryBuffer(stream_info.cbSize.max(1))?;
                sample.AddBuffer(&buf)?;
                output.pSample = std::mem::ManuallyDrop::new(Some(sample));
            }

            let mut status: u32 = 0;
            let mut buffers = [output];
            let hr = self.transform.ProcessOutput(0, &mut buffers, &mut status);

            match hr {
                Ok(()) => {
                    let produced = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    if let Some(sample) = produced {
                        self.emit_sample(&sample, out)?;
                    }
                    Ok(true)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                    // Reclaim the sample we allocated (if any) — dropped here.
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    Ok(false)
                }
                Err(e) if e.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // Output type changed; re-set and retry once.
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    set_output_type(&self.transform, self.output_stream_id, &self.cfg, self.hdr10, self.av1)?;
                    Ok(false)
                }
                Err(e) => {
                    let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                    Err(PipelineError::Windows(e))
                }
            }
        }
    }

    fn emit_sample(&mut self, sample: &IMFSample, out: &mut Vec<EncodedSample>) -> Result<()> {
        unsafe {
            let is_keyframe = sample
                .GetUINT32(&MFSampleExtension_CleanPoint)
                .map(|v| v != 0)
                .unwrap_or(false);
            let time_hns = sample.GetSampleTime().unwrap_or(0);
            let timestamp = Duration::from_nanos((time_hns.max(0) as u64) * 100);

            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            let mut len: u32 = 0;
            buffer.Lock(&mut ptr, None, Some(&mut len))?;
            let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
            let _ = buffer.Unlock();

            // Capture SPS/PPS from the first keyframe's parameter sets.
            if !self.hdr10 && !self.av1 && self.params.sps.is_empty() && is_keyframe {
                self.params = extract_parameter_sets(&data);
            }

            out.push(EncodedSample {
                data,
                timestamp,
                is_keyframe,
            });
        }
        Ok(())
    }
}

impl Drop for MfH264Encoder {
    fn drop(&mut self) {
        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
            // Release the D3D manager reference held by the MFT.
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_SET_D3D_MANAGER, 0);
            let _ = &self.device_manager;
        }
    }
}

// --- free functions -------------------------------------------------------

/// Create and initialize a video sample allocator that produces NV12 textures
/// the hardware MFT will accept as input. Bound to the shared DXGI device
/// manager so the textures live on the encoder's device.
unsafe fn create_input_allocator(
    transform: &IMFTransform,
    input_id: u32,
    device_manager: &IMFDXGIDeviceManager,
    cfg: &VideoConfig,
) -> Result<IMFVideoSampleAllocatorEx> {
    // Only meaningful if the MFT reports D3D11 awareness on its input stream.
    if let Ok(attrs) = transform.GetInputStreamAttributes(input_id) {
        let aware = attrs.GetUINT32(&MF_SA_D3D11_AWARE).unwrap_or(0);
        if aware == 0 {
            return Err(PipelineError::Audio("MFT not D3D11-aware".into()));
        }
    }

    let mut alloc: *mut core::ffi::c_void = std::ptr::null_mut();
    MFCreateVideoSampleAllocatorEx(&IMFVideoSampleAllocatorEx::IID, &mut alloc)?;
    let allocator = IMFVideoSampleAllocatorEx::from_raw(alloc);

    allocator.SetDirectXManager(device_manager)?;

    // The NV12 media type the allocator produces.
    let nv12_type = MFCreateMediaType()?;
    nv12_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    nv12_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
    set_frame_size(&nv12_type, cfg.width, cfg.height)?;
    set_ratio(&nv12_type, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
    set_ratio(&nv12_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    nv12_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

    // Attributes controlling the allocated textures' bind flags: RENDER_TARGET
    // (video processor writes) + VIDEO_ENCODER (hardware encoder input).
    let attrs = create_mf_attributes(1)?;
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_VIDEO_ENCODER,
    };
    attrs.SetUINT32(
        &MF_SA_D3D11_BINDFLAGS,
        (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_VIDEO_ENCODER.0) as u32,
    )?;

    // A small pool: a few in-flight frames.
    allocator.InitializeSampleAllocatorEx(2, 6, &attrs, &nv12_type)?;
    Ok(allocator)
}

unsafe fn create_mf_attributes(count: u32) -> Result<IMFAttributes> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, count)?;
    Ok(attrs.unwrap())
}

unsafe fn create_device_manager(device: &ID3D11Device) -> Result<IMFDXGIDeviceManager> {
    let mut reset_token: u32 = 0;
    let mut manager: Option<IMFDXGIDeviceManager> = None;
    MFCreateDXGIDeviceManager(&mut reset_token, &mut manager)?;
    let manager = manager.expect("device manager created");
    manager.ResetDevice(device, reset_token)?;
    Ok(manager)
}

/// Enumerate H.264 video encoders, preferring hardware. Returns the created
/// transform and its activation object.
unsafe fn enumerate_encoder(prefer_hardware: bool, subtype: GUID) -> Result<(IMFTransform, IMFActivate)> {
    let output_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: subtype,
    };

    let flags = if prefer_hardware {
        MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_TRANSCODE_ONLY | MFT_ENUM_FLAG_SORTANDFILTER
    } else {
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_TRANSCODE_ONLY | MFT_ENUM_FLAG_SORTANDFILTER
    };

    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count: u32 = 0;
    MFTEnumEx(
        MFT_CATEGORY_VIDEO_ENCODER,
        flags,
        None,
        Some(&output_info),
        &mut activates,
        &mut count,
    )?;

    if count == 0 || activates.is_null() {
        return Err(PipelineError::NoEncoderFound);
    }

    let slice = std::slice::from_raw_parts_mut(activates, count as usize);
    // Take the first (sorted best-first by SORTANDFILTER).
    let result = (|| {
        for entry in slice.iter_mut() {
            if let Some(act) = entry.take() {
                if let Ok(transform) = act.ActivateObject::<IMFTransform>() {
                    return Some((transform, act));
                }
            }
        }
        None
    })();

    for entry in slice {
        drop(entry.take());
    }
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));

    result.ok_or(PipelineError::NoEncoderFound)
}

unsafe fn stream_ids(transform: &IMFTransform) -> Result<(u32, u32)> {
    // Most encoder MFTs use stream id 0 for both. GetStreamIDs may return
    // E_NOTIMPL meaning "use 0-based indices".
    let mut input_ids = [0u32; 1];
    let mut output_ids = [0u32; 1];
    let _ = transform.GetStreamIDs(&mut input_ids, &mut output_ids);
    // If GetStreamIDs is not implemented, ids stay 0 which is correct.
    Ok((input_ids[0], output_ids[0]))
}

unsafe fn mft_is_async(transform: &IMFTransform) -> bool {
    if let Ok(attrs) = transform.GetAttributes() {
        if let Ok(v) = attrs.GetUINT32(&MF_TRANSFORM_ASYNC) {
            return v != 0;
        }
    }
    false
}

unsafe fn set_output_type(
    transform: &IMFTransform,
    output_id: u32,
    cfg: &VideoConfig,
    hdr10: bool,
    av1: bool,
) -> Result<()> {
    let media_type = MFCreateMediaType()?;
    media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    let subtype = encoded_subtype(hdr10, av1);
    media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
    media_type.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
    set_frame_size(&media_type, cfg.width, cfg.height)?;
    set_ratio(&media_type, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
    set_ratio(&media_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
    media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
    let profile = if av1 { eAVEncAV1VProfile_Main_420_8.0 } else if hdr10 { eAVEncH265VProfile_Main_420_10.0 } else { eAVEncH264VProfile_Main.0 };
    media_type.SetUINT32(&MF_MT_MPEG2_PROFILE, profile as u32)?;
    if hdr10 {
        set_hdr10_attributes(&media_type)?;
    }
    transform
        .SetOutputType(output_id, &media_type, 0)
        .map_err(|e| PipelineError::TypeNegotiation(format!("output: {e}")))?;
    Ok(())
}

fn encoded_subtype(hdr10: bool, av1: bool) -> GUID {
    if av1 { MFVideoFormat_AV1 } else if hdr10 { MFVideoFormat_HEVC } else { MFVideoFormat_H264 }
}

#[derive(PartialEq, Eq, Clone, Copy)]
enum InputFormat {
    Argb32,
    Nv12,
    P010,
}

unsafe fn set_input_type(
    transform: &IMFTransform,
    input_id: u32,
    cfg: &VideoConfig,
    force_nv12: bool,
    hdr10: bool,
) -> Result<InputFormat> {
    // For hardware/async MFTs fed D3D textures, NV12 is the only reliable input.
    // For software MFTs, ARGB32 (BGRA) from a CPU buffer works and skips the
    // color-convert step.
    let candidates: &[(windows::core::GUID, InputFormat)] = if hdr10 {
        &[(MFVideoFormat_P010, InputFormat::P010)]
    } else if force_nv12 {
        &[(MFVideoFormat_NV12, InputFormat::Nv12)]
    } else {
        &[
            (MFVideoFormat_ARGB32, InputFormat::Argb32),
            (MFVideoFormat_NV12, InputFormat::Nv12),
        ]
    };
    let mut last_err = None;
    for &(subtype, format) in candidates {
        let media_type = MFCreateMediaType()?;
        media_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        media_type.SetGUID(&MF_MT_SUBTYPE, &subtype)?;
        set_frame_size(&media_type, cfg.width, cfg.height)?;
        set_ratio(&media_type, &MF_MT_FRAME_RATE, cfg.fps, 1)?;
        set_ratio(&media_type, &MF_MT_PIXEL_ASPECT_RATIO, 1, 1)?;
        let _ = media_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32);
        if hdr10 {
            media_type.SetUINT32(&MF_MT_FIXED_SIZE_SAMPLES, 1)?;
            media_type.SetUINT32(&MF_MT_ALL_SAMPLES_INDEPENDENT, 1)?;
            media_type.SetUINT32(&MF_MT_SAMPLE_SIZE, cfg.width * cfg.height * 3)?;
            media_type.SetUINT32(&MF_MT_DEFAULT_STRIDE, cfg.width * 2)?;
            set_hdr10_attributes(&media_type)?;
        }
        match transform.SetInputType(input_id, &media_type, 0) {
            Ok(()) => return Ok(format),
            Err(e) => last_err = Some(e),
        }
    }
    Err(PipelineError::TypeNegotiation(format!(
        "input format: {:?}",
        last_err
    )))
}

unsafe fn set_hdr10_attributes(media_type: &IMFMediaType) -> Result<()> {
    media_type.SetUINT32(&MF_MT_VIDEO_PRIMARIES, MFVideoPrimaries_BT2020.0 as u32)?;
    media_type.SetUINT32(&MF_MT_TRANSFER_FUNCTION, MFVideoTransFunc_2084.0 as u32)?;
    media_type.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT2020_10.0 as u32)?;
    media_type.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)?;
    media_type.SetUINT32(&MF_MT_VIDEO_CHROMA_SITING, MFVideoChromaSubsampling_MPEG2.0 as u32)?;
    Ok(())
}

unsafe fn set_frame_size(mt: &IMFMediaType, w: u32, h: u32) -> Result<()> {
    let packed = ((w as u64) << 32) | (h as u64);
    mt.SetUINT64(&MF_MT_FRAME_SIZE, packed)?;
    Ok(())
}

unsafe fn set_ratio(mt: &IMFMediaType, key: *const GUID, num: u32, den: u32) -> Result<()> {
    let packed = ((num as u64) << 32) | (den as u64);
    mt.SetUINT64(&*key, packed)?;
    Ok(())
}

/// Split an Annex-B access unit into SPS (type 7) and PPS (type 8) NAL bodies
/// (without start codes).
fn extract_parameter_sets(annex_b: &[u8]) -> ParameterSets {
    let mut params = ParameterSets::default();
    for nal in iter_annex_b_nals(annex_b) {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1f;
        match nal_type {
            7 => params.sps = nal.to_vec(),
            8 => params.pps = nal.to_vec(),
            _ => {}
        }
    }
    params
}

/// Iterate NAL unit bodies (between 3- or 4-byte start codes), excluding the
/// start code itself.
pub fn iter_annex_b_nals(data: &[u8]) -> impl Iterator<Item = &[u8]> {
    let starts = find_nal_starts(data);
    let mut ranges = Vec::with_capacity(starts.len());
    for i in 0..starts.len() {
        let (body_start, _sc_len) = starts[i];
        let end = if i + 1 < starts.len() {
            starts[i + 1].0 - starts[i + 1].1
        } else {
            data.len()
        };
        ranges.push((body_start, end));
    }
    ranges.into_iter().map(move |(s, e)| &data[s..e])
}

/// Returns (body_start_index, start_code_len) for each NAL. body_start is the
/// index just after the start code.
fn find_nal_starts(data: &[u8]) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            out.push((i + 3, 3));
            i += 3;
        } else if i + 4 <= data.len()
            && data[i] == 0
            && data[i + 1] == 0
            && data[i + 2] == 0
            && data[i + 3] == 1
        {
            out.push((i + 4, 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    out
}

// Suppress unused import warning for PCWSTR if not used in some builds.
const _: Option<PCWSTR> = None;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sps_pps_from_annex_b() {
        // start(4) SPS(type7) start(3) PPS(type8) start(4) IDR(type5)
        let stream = [
            0, 0, 0, 1, 0x67, 0xAA, 0xBB, // SPS
            0, 0, 1, 0x68, 0xCC, // PPS
            0, 0, 0, 1, 0x65, 0x11, 0x22, // IDR slice
        ];
        let p = extract_parameter_sets(&stream);
        assert_eq!(p.sps, vec![0x67, 0xAA, 0xBB]);
        assert_eq!(p.pps, vec![0x68, 0xCC]);
    }

    #[test]
    fn iterates_all_nals() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3];
        let nals: Vec<&[u8]> = iter_annex_b_nals(&stream).collect();
        assert_eq!(nals.len(), 2);
        assert_eq!(nals[0], &[0x67, 1, 2]);
        assert_eq!(nals[1], &[0x68, 3]);
    }
}
