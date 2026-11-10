//! Media Foundation AAC audio encoder MFT.
//!
//! Input: interleaved 16-bit PCM (from WASAPI capture, converted). Output: raw
//! AAC frames. The AAC MFT is synchronous, so the drive loop is a simple
//! ProcessInput/ProcessOutput cycle. We also derive the AudioSpecificConfig
//! (needed for the RTMP AAC sequence header) from the encoder's output type.

use std::time::Duration;

use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use super::EncodedAudio;
use crate::{PipelineError, Result};

const HNS_PER_SEC: i64 = 10_000_000;

pub struct AacEncoder {
    transform: IMFTransform,
    sample_rate: u32,
    channels: u16,
    /// AudioSpecificConfig bytes (for the RTMP AAC sequence header / MP4 esds).
    asc: Vec<u8>,
    started: bool,
}

impl AacEncoder {
    /// Create an AAC encoder for the given PCM format. `bitrate` in bits/sec
    /// (typical: 128000). AAC-LC only.
    pub fn new(sample_rate: u32, channels: u16, bitrate: u32) -> Result<Self> {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);

            let transform = create_aac_mft()?;

            // Output type (AAC) MUST be set considering input; MF AAC MFT wants
            // input set first for audio, then output.
            set_input_pcm(&transform, sample_rate, channels)?;
            let out_type = set_output_aac(&transform, sample_rate, channels, bitrate)?;

            // Extract AudioSpecificConfig from the output type's user data.
            let asc = extract_asc(&out_type).unwrap_or_else(|| {
                // Fall back to a computed 2-byte ASC for AAC-LC.
                default_asc(sample_rate, channels)
            });

            Ok(Self {
                transform,
                sample_rate,
                channels,
                asc,
                started: false,
            })
        }
    }

    pub fn audio_specific_config(&self) -> &[u8] {
        &self.asc
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

    /// Encode one PCM16 buffer, pushing any produced AAC frames to `out`.
    pub fn encode(
        &mut self,
        pcm16: &[u8],
        timestamp: Duration,
        out: &mut Vec<EncodedAudio>,
    ) -> Result<()> {
        self.ensure_started()?;
        unsafe {
            let sample = self.make_input_sample(pcm16, timestamp)?;
            self.transform.ProcessInput(0, &sample, 0)?;
        }
        self.pull(out)
    }

    /// Flush remaining audio at end of stream.
    pub fn drain(&mut self, out: &mut Vec<EncodedAudio>) -> Result<()> {
        if !self.started {
            return Ok(());
        }
        unsafe {
            self.transform
                .ProcessMessage(MFT_MESSAGE_COMMAND_DRAIN, 0)?;
        }
        self.pull(out)
    }

    unsafe fn make_input_sample(&self, pcm16: &[u8], timestamp: Duration) -> Result<IMFSample> {
        let buffer = MFCreateMemoryBuffer(pcm16.len() as u32)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(pcm16.as_ptr(), ptr, pcm16.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(pcm16.len() as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        sample.SetSampleTime((timestamp.as_nanos() as i64) / 100)?;
        // Duration from sample count.
        let frame_bytes = 2 * self.channels as i64;
        let frames = pcm16.len() as i64 / frame_bytes.max(1);
        let dur = frames * HNS_PER_SEC / self.sample_rate.max(1) as i64;
        sample.SetSampleDuration(dur)?;
        Ok(sample)
    }

    fn pull(&mut self, out: &mut Vec<EncodedAudio>) -> Result<()> {
        unsafe {
            loop {
                let stream_info = self.transform.GetOutputStreamInfo(0)?;
                let mut output = MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    ..Default::default()
                };
                let provides = (stream_info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0) as u32)
                    != 0;
                if !provides {
                    let sample = MFCreateSample()?;
                    let buf = MFCreateMemoryBuffer(stream_info.cbSize.max(1))?;
                    sample.AddBuffer(&buf)?;
                    output.pSample = std::mem::ManuallyDrop::new(Some(sample));
                }

                let mut status = 0u32;
                let mut buffers = [output];
                match self.transform.ProcessOutput(0, &mut buffers, &mut status) {
                    Ok(()) => {
                        if let Some(sample) = std::mem::ManuallyDrop::take(&mut buffers[0].pSample)
                        {
                            self.emit(&sample, out)?;
                        }
                    }
                    Err(e) if e.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => {
                        let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        break;
                    }
                    Err(e) => {
                        let _ = std::mem::ManuallyDrop::take(&mut buffers[0].pSample);
                        return Err(PipelineError::Windows(e));
                    }
                }
            }
        }
        Ok(())
    }

    unsafe fn emit(&self, sample: &IMFSample, out: &mut Vec<EncodedAudio>) -> Result<()> {
        let time_hns = sample.GetSampleTime().unwrap_or(0);
        let timestamp = Duration::from_nanos((time_hns.max(0) as u64) * 100);
        let buffer = sample.ConvertToContiguousBuffer()?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        let mut len = 0u32;
        buffer.Lock(&mut ptr, None, Some(&mut len))?;
        let data = std::slice::from_raw_parts(ptr, len as usize).to_vec();
        let _ = buffer.Unlock();
        out.push(EncodedAudio { data, timestamp });
        Ok(())
    }
}

// --- free functions -------------------------------------------------------

unsafe fn create_aac_mft() -> Result<IMFTransform> {
    let out_info = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Audio,
        guidSubtype: MFAudioFormat_AAC,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0u32;
    MFTEnumEx(
        MFT_CATEGORY_AUDIO_ENCODER,
        MFT_ENUM_FLAG_SYNCMFT | MFT_ENUM_FLAG_SORTANDFILTER | MFT_ENUM_FLAG_TRANSCODE_ONLY,
        None,
        Some(&out_info),
        &mut activates,
        &mut count,
    )?;
    if count == 0 || activates.is_null() {
        return Err(PipelineError::Audio("no AAC encoder MFT".into()));
    }
    let slice = std::slice::from_raw_parts(activates, count as usize);
    let result = slice
        .iter()
        .flatten()
        .find_map(|a| a.ActivateObject::<IMFTransform>().ok());
    windows::Win32::System::Com::CoTaskMemFree(Some(activates as *const _));
    result.ok_or_else(|| PipelineError::Audio("failed to activate AAC MFT".into()))
}

unsafe fn set_input_pcm(t: &IMFTransform, sample_rate: u32, channels: u16) -> Result<()> {
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
    mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
    mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
    let block_align = 2 * channels as u32;
    mt.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align)?;
    mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, sample_rate * block_align)?;
    t.SetInputType(0, &mt, 0)
        .map_err(|e| PipelineError::TypeNegotiation(format!("aac input: {e}")))?;
    Ok(())
}

unsafe fn set_output_aac(
    t: &IMFTransform,
    sample_rate: u32,
    channels: u16,
    bitrate: u32,
) -> Result<IMFMediaType> {
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, sample_rate)?;
    mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels as u32)?;
    mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, bitrate / 8)?;
    // AAC profile level indication: 0x29 = AAC-LC.
    mt.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?;
    t.SetOutputType(0, &mt, 0)
        .map_err(|e| PipelineError::TypeNegotiation(format!("aac output: {e}")))?;
    Ok(mt)
}

/// The MF AAC output type carries an HEAACWAVEINFO blob in MF_MT_USER_DATA; the
/// AudioSpecificConfig is the tail of it (after the 12-byte HEAACWAVEINFO
/// header that follows the WAVEFORMATEX). If unavailable, callers fall back to
/// `default_asc`.
unsafe fn extract_asc(mt: &IMFMediaType) -> Option<Vec<u8>> {
    let blob_len = mt.GetBlobSize(&MF_MT_USER_DATA).ok()?;
    if blob_len == 0 {
        return None;
    }
    let mut buf = vec![0u8; blob_len as usize];
    mt.GetBlob(&MF_MT_USER_DATA, &mut buf, None).ok()?;
    // HEAACWAVEINFO wUserData starts 12 bytes in (payloadType..) -> the ASC is
    // the remainder after the 12-byte header.
    if buf.len() > 12 {
        Some(buf[12..].to_vec())
    } else {
        None
    }
}

/// Compute a 2-byte AAC-LC AudioSpecificConfig for the common sample rates.
pub fn default_asc(sample_rate: u32, channels: u16) -> Vec<u8> {
    // 5 bits object type (2 = AAC-LC), 4 bits freq index, 4 bits channel config.
    let freq_index = match sample_rate {
        96000 => 0,
        88200 => 1,
        64000 => 2,
        48000 => 3,
        44100 => 4,
        32000 => 5,
        24000 => 6,
        22050 => 7,
        16000 => 8,
        12000 => 9,
        11025 => 10,
        8000 => 11,
        _ => 3, // default 48k
    };
    let obj_type: u16 = 2;
    let chan: u16 = channels;
    let val: u16 = (obj_type << 11) | ((freq_index as u16) << 7) | (chan << 3);
    vec![(val >> 8) as u8, (val & 0xff) as u8]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asc_for_48k_stereo() {
        // AAC-LC(2), 48k(idx3), 2ch => 0b00010_0011_0010_000 = 0x11 0x90
        let asc = default_asc(48000, 2);
        assert_eq!(asc, vec![0x11, 0x90]);
    }

    #[test]
    fn asc_for_44k_mono() {
        // AAC-LC(2), 44.1k(idx4), 1ch => 0b00010_0100_0001_000
        let asc = default_asc(44100, 1);
        assert_eq!(asc, vec![0x12, 0x08]);
    }
}
