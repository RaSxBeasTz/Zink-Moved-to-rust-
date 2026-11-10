//! MP4 recording via IMFSinkWriter (Media Foundation), pass-through.
//!
//! Consumes the encoder's `EncodedSample`s (H.264 Annex-B) and writes them to
//! an MP4 file without re-encoding: the sink writer's input and target media
//! types are both H.264, so it only muxes. The MP4 sink converts Annex-B to
//! AVCC framing internally and builds the avcC box from the sequence header.

use std::path::Path;
use std::time::Duration;

use windows::Win32::Media::MediaFoundation::*;

use crate::encoder::{EncodedSample, ParameterSets};
use crate::{PipelineError, Result, VideoConfig};

const HNS_PER_SEC: i64 = 10_000_000;

/// Descriptor for an AAC audio track added to the MP4 at construction time.
#[derive(Clone)]
pub struct AudioTrack {
    pub sample_rate: u32,
    pub channels: u16,
    /// AAC AudioSpecificConfig (from the AAC encoder), for the esds box.
    pub asc: Vec<u8>,
    pub bitrate: u32,
}

pub struct Mp4Recorder {
    writer: IMFSinkWriter,
    stream_index: u32,
    /// Sink stream indices for each audio track, in the order given to `new`.
    audio_indices: Vec<u32>,
    audio_rates: Vec<u32>,
    fps: u32,
    finalized: bool,
}

impl Mp4Recorder {
    /// Create a video-only MP4 recorder. Convenience wrapper over `with_audio`.
    pub fn new(path: impl AsRef<Path>, cfg: &VideoConfig, params: &ParameterSets) -> Result<Self> {
        Self::with_audio(path, cfg, params, &[])
    }

    /// Create an MP4 recorder with a video track and zero or more AAC audio
    /// tracks. All streams must be declared before writing begins, so audio
    /// tracks are fixed at construction. `params` carries the SPS/PPS for avcC.
    pub fn with_audio(
        path: impl AsRef<Path>,
        cfg: &VideoConfig,
        params: &ParameterSets,
        audio: &[AudioTrack],
    ) -> Result<Self> {
        unsafe {
            let _ = MFStartup(MF_VERSION, MFSTARTUP_FULL);

            let url = to_wide(path.as_ref());
            let attrs = create_mp4_attributes()?;
            let writer =
                MFCreateSinkWriterFromURL(windows::core::PCWSTR(url.as_ptr()), None, &attrs)?;

            // Video stream (pass-through H.264).
            let target_type = build_h264_type(cfg, params)?;
            let stream_index = writer.AddStream(&target_type)?;
            let input_type = build_h264_type(cfg, params)?;
            writer.SetInputMediaType(stream_index, &input_type, None)?;

            // Audio streams (pass-through AAC).
            let mut audio_indices = Vec::new();
            let mut audio_rates = Vec::new();
            for track in audio {
                let aac_type = build_aac_type(track)?;
                let idx = writer.AddStream(&aac_type)?;
                let aac_in = build_aac_type(track)?;
                writer.SetInputMediaType(idx, &aac_in, None)?;
                audio_indices.push(idx);
                audio_rates.push(track.sample_rate);
            }

            writer.BeginWriting()?;

            Ok(Self {
                writer,
                stream_index,
                audio_indices,
                audio_rates,
                fps: cfg.fps,
                finalized: false,
            })
        }
    }

    /// Write one encoded AAC frame to audio track `track` (0-based, in the order
    /// passed to `with_audio`).
    pub fn write_audio(
        &mut self,
        track: usize,
        data: &[u8],
        timestamp: std::time::Duration,
    ) -> Result<()> {
        let idx = *self
            .audio_indices
            .get(track)
            .ok_or_else(|| PipelineError::Audio(format!("no audio track {track}")))?;
        unsafe {
            let buffer = MFCreateMemoryBuffer(data.len() as u32)?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer.Lock(&mut ptr, None, None)?;
            std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(data.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(duration_to_hns(timestamp))?;
            // AAC frames are 1024 samples; give a duration so the muxer can
            // interleave (required once multiple streams exist).
            let rate = self.audio_rates.get(track).copied().unwrap_or(48000).max(1);
            sample.SetSampleDuration(1024 * HNS_PER_SEC / rate as i64)?;
            self.writer.WriteSample(idx, &sample)?;
        }
        let _ = &self.audio_rates;
        Ok(())
    }

    /// Write one encoded sample to the file.
    pub fn write(&mut self, sample: &EncodedSample) -> Result<()> {
        unsafe {
            let mf_sample = self.build_sample(sample)?;
            self.writer.WriteSample(self.stream_index, &mf_sample)?;
        }
        Ok(())
    }

    /// Finalize the MP4 (writes the moov atom). Required for a playable file.
    pub fn finalize(&mut self) -> Result<()> {
        if self.finalized {
            return Ok(());
        }
        unsafe {
            self.writer.Finalize()?;
        }
        self.finalized = true;
        Ok(())
    }

    unsafe fn build_sample(&self, s: &EncodedSample) -> Result<IMFSample> {
        let buffer = MFCreateMemoryBuffer(s.data.len() as u32)?;
        let mut ptr: *mut u8 = std::ptr::null_mut();
        buffer.Lock(&mut ptr, None, None)?;
        std::ptr::copy_nonoverlapping(s.data.as_ptr(), ptr, s.data.len());
        buffer.Unlock()?;
        buffer.SetCurrentLength(s.data.len() as u32)?;

        let sample = MFCreateSample()?;
        sample.AddBuffer(&buffer)?;
        let hns = duration_to_hns(s.timestamp);
        sample.SetSampleTime(hns)?;
        sample.SetSampleDuration(HNS_PER_SEC / self.fps.max(1) as i64)?;
        if s.is_keyframe {
            sample.SetUINT32(&MFSampleExtension_CleanPoint, 1)?;
        }
        Ok(sample)
    }
}

impl Drop for Mp4Recorder {
    fn drop(&mut self) {
        // Best-effort finalize so a dropped recorder still yields a playable
        // file. Explicit finalize() is preferred to surface errors.
        let _ = self.finalize();
    }
}

fn duration_to_hns(d: Duration) -> i64 {
    (d.as_nanos() as i64) / 100
}

unsafe fn create_mp4_attributes() -> Result<IMFAttributes> {
    let mut attrs: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut attrs, 1)?;
    let attrs = attrs.unwrap();
    attrs.SetGUID(&MF_TRANSCODE_CONTAINERTYPE, &MFTranscodeContainerType_MPEG4)?;
    Ok(attrs)
}

/// Build the H.264 media type describing the stored stream. Sets the sequence
/// header (SPS/PPS as an AVCC-style blob) if provided so the sink can write avcC.
unsafe fn build_h264_type(cfg: &VideoConfig, params: &ParameterSets) -> Result<IMFMediaType> {
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
    mt.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)?;
    mt.SetUINT64(
        &MF_MT_FRAME_SIZE,
        ((cfg.width as u64) << 32) | cfg.height as u64,
    )?;
    mt.SetUINT64(&MF_MT_FRAME_RATE, ((cfg.fps as u64) << 32) | 1)?;
    mt.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)?;
    mt.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;

    if !params.sps.is_empty() && !params.pps.is_empty() {
        let seq_header = build_avcc(&params.sps, &params.pps);
        mt.SetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &seq_header)
            .map_err(|e| PipelineError::TypeNegotiation(format!("seq header: {e}")))?;
    }

    Ok(mt)
}

/// Build the AAC media type for an MP4 audio stream. The MF AAC type carries
/// the AudioSpecificConfig in MF_MT_USER_DATA, prefixed by the 12-byte
/// HEAACWAVEINFO fields that follow WAVEFORMATEX (payload type, etc.).
unsafe fn build_aac_type(track: &super::mux::AudioTrack) -> Result<IMFMediaType> {
    let mt = MFCreateMediaType()?;
    mt.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
    mt.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
    mt.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
    mt.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, track.sample_rate)?;
    mt.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, track.channels as u32)?;
    mt.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, track.bitrate / 8)?;
    mt.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)?; // AAC-LC
                                                                    // HEAACWAVEINFO tail: 12 bytes of fields (wPayloadType=0, etc.) + ASC.
    let mut user_data = vec![0u8; 12];
    user_data.extend_from_slice(&track.asc);
    mt.SetBlob(&MF_MT_USER_DATA, &user_data)?;
    Ok(mt)
}

/// Build an AVCDecoderConfigurationRecord (avcC) blob from SPS/PPS NAL bodies.
/// This is what MF expects in MF_MT_MPEG_SEQUENCE_HEADER for H.264.
pub fn build_avcc(sps: &[u8], pps: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(11 + sps.len() + pps.len());
    v.push(1); // configurationVersion
    v.push(sps.get(1).copied().unwrap_or(0x64)); // AVCProfileIndication
    v.push(sps.get(2).copied().unwrap_or(0)); // profile_compatibility
    v.push(sps.get(3).copied().unwrap_or(0x1f)); // AVCLevelIndication
    v.push(0xff); // 6 bits reserved (1) + lengthSizeMinusOne (3) => 4-byte NAL len
    v.push(0xe1); // 3 bits reserved (1) + numOfSPS (1)
    v.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    v.extend_from_slice(sps);
    v.push(1); // numOfPPS
    v.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    v.extend_from_slice(pps);
    v
}

fn to_wide(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    path.as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn avcc_layout() {
        let sps = [0x67, 0x64, 0x00, 0x1f, 0xAA];
        let pps = [0x68, 0xCC];
        let avcc = build_avcc(&sps, &pps);
        assert_eq!(avcc[0], 1); // version
        assert_eq!(avcc[1], 0x64); // profile from sps[1]
        assert_eq!(avcc[3], 0x1f); // level from sps[3]
        assert_eq!(avcc[5], 0xe1); // one SPS
                                   // SPS length prefix at offset 6..8
        assert_eq!(&avcc[6..8], &(sps.len() as u16).to_be_bytes());
        // PPS count byte immediately after SPS
        let pps_count_off = 8 + sps.len();
        assert_eq!(avcc[pps_count_off], 1);
    }
}
