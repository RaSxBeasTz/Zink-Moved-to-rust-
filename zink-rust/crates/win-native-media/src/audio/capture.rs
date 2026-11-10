//! WASAPI audio capture: system loopback and microphone.
//!
//! Both sources use a shared-mode `IAudioClient` polled on a dedicated thread.
//! Loopback captures the default *render* device with the loopback flag; the
//! mic captures the default *capture* device. The device mix format (commonly
//! 32-bit float, 48 kHz, stereo) is converted to interleaved 16-bit PCM, which
//! the AAC encoder accepts.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::Arc;
use windows::Win32::Media::Audio::*;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_MULTITHREADED,
};

use crate::{AudioBuffer, AudioSource, Result};

/// A running audio capture. Dropping it stops the capture thread.
pub struct AudioCapture {
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// Start capturing `source`. Returns the capture handle and a receiver of PCM
/// buffers. The format (sample rate / channels) is read from the device and
/// exposed on the handle so the encoder can be configured to match.
pub fn start(source: AudioSource, bound: usize) -> Result<(AudioCapture, Receiver<AudioBuffer>)> {
    // Probe the device format synchronously so we can report it to the caller.
    let (sample_rate, channels) = probe_format(&source)?;

    let (tx, rx) = std::sync::mpsc::sync_channel::<AudioBuffer>(bound);
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = stop.clone();

    let handle = std::thread::spawn(move || {
        if let Err(e) = capture_loop(source, tx, stop_thread) {
            tracing::error!("audio capture loop ended: {e}");
        }
    });

    Ok((
        AudioCapture {
            stop,
            handle: Some(handle),
            sample_rate,
            channels,
        },
        rx,
    ))
}

/// Open the device, read its mix format, close. Used to report format upfront.
fn probe_format(source: &AudioSource) -> Result<(u32, u16)> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = activate_client(source)?;
        let fmt = client.GetMixFormat()?;
        let sr = (*fmt).nSamplesPerSec;
        let ch = (*fmt).nChannels;
        windows::Win32::System::Com::CoTaskMemFree(Some(fmt as *const _));
        Ok((sr, ch))
    }
}

unsafe fn activate_client(source: &AudioSource) -> Result<IAudioClient> {
    let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
    let data_flow = match source {
        // Loopback captures the render endpoint.
        AudioSource::SystemLoopback | AudioSource::SystemLoopbackDevice(_) => eRender,
        AudioSource::Microphone | AudioSource::MicrophoneDevice(_) => eCapture,
    };
    let device = match source {
        AudioSource::SystemLoopbackDevice(id) | AudioSource::MicrophoneDevice(id) => {
            let wide: Vec<u16> = id.encode_utf16().chain(Some(0)).collect();
            enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr()))?
        }
        _ => enumerator.GetDefaultAudioEndpoint(data_flow, eConsole)?,
    };
    let client = device.Activate::<IAudioClient>(CLSCTX_ALL, None)?;
    Ok(client)
}

fn capture_loop(
    source: AudioSource,
    tx: SyncSender<AudioBuffer>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let client = activate_client(&source)?;
        let fmt_ptr = client.GetMixFormat()?;
        let fmt = *fmt_ptr;
        let sample_rate = fmt.nSamplesPerSec;
        let channels = fmt.nChannels;
        // Detect float vs PCM from the format tag / extensible subformat.
        let is_float = format_is_float(fmt_ptr);
        let bits = fmt.wBitsPerSample;

        // Shared mode; loopback flag for system audio. 200ms buffer.
        let stream_flags = match source {
            AudioSource::SystemLoopback | AudioSource::SystemLoopbackDevice(_) => {
                AUDCLNT_STREAMFLAGS_LOOPBACK
            }
            AudioSource::Microphone | AudioSource::MicrophoneDevice(_) => 0,
        };
        let buffer_duration_hns = 2_000_000i64; // 200ms in 100ns units
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            stream_flags,
            buffer_duration_hns,
            0,
            fmt_ptr,
            None,
        )?;

        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        windows::Win32::System::Com::CoTaskMemFree(Some(fmt_ptr as *const _));

        let start_qpc = std::time::Instant::now();
        // Poll interval ~ half the buffer.
        let poll = std::time::Duration::from_millis(10);

        while !stop.load(Ordering::SeqCst) {
            let packet_len = capture.GetNextPacketSize()?;
            if packet_len == 0 {
                std::thread::sleep(poll);
                continue;
            }

            let mut data_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            capture.GetBuffer(&mut data_ptr, &mut num_frames, &mut flags, None, None)?;

            let silent = (flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
            let frame_bytes = (bits / 8) as usize * channels as usize;
            let byte_len = num_frames as usize * frame_bytes;

            let pcm16 = if silent {
                vec![0u8; num_frames as usize * 2 * channels as usize]
            } else {
                let raw = std::slice::from_raw_parts(data_ptr, byte_len);
                to_pcm16(raw, is_float, bits, channels)
            };

            capture.ReleaseBuffer(num_frames)?;

            let timestamp = start_qpc.elapsed();
            // Bounded, drop on backpressure (audio glitch beats a stall).
            let _ = tx.try_send(AudioBuffer {
                data: pcm16,
                timestamp,
                sample_rate,
                channels,
            });
        }

        client.Stop()?;
        Ok(())
    }
}

/// Determine if the mix format is IEEE float (vs integer PCM), handling the
/// WAVE_FORMAT_EXTENSIBLE case by checking the subformat GUID.
unsafe fn format_is_float(fmt: *const WAVEFORMATEX) -> bool {
    const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
    const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;
    match (*fmt).wFormatTag {
        WAVE_FORMAT_IEEE_FLOAT => true,
        WAVE_FORMAT_EXTENSIBLE => {
            use windows::Win32::Media::Multimedia::KSDATAFORMAT_SUBTYPE_IEEE_FLOAT;
            let ext = fmt as *const WAVEFORMATEXTENSIBLE;
            // Copy out of the packed struct before comparing (unaligned read).
            let subformat = std::ptr::addr_of!((*ext).SubFormat).read_unaligned();
            subformat == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
        }
        _ => false,
    }
}

/// Convert raw device samples to interleaved 16-bit signed PCM.
fn to_pcm16(raw: &[u8], is_float: bool, bits: u16, _channels: u16) -> Vec<u8> {
    if is_float && bits == 32 {
        let n = raw.len() / 4;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            let f =
                f32::from_le_bytes([raw[i * 4], raw[i * 4 + 1], raw[i * 4 + 2], raw[i * 4 + 3]]);
            let clamped = f.clamp(-1.0, 1.0);
            let s = (clamped * 32767.0) as i16;
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    } else if bits == 16 {
        raw.to_vec() // already PCM16
    } else if bits == 32 {
        // 32-bit int PCM -> 16-bit (take high word).
        let n = raw.len() / 4;
        let mut out = Vec::with_capacity(n * 2);
        for i in 0..n {
            out.push(raw[i * 4 + 2]);
            out.push(raw[i * 4 + 3]);
        }
        out
    } else {
        raw.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn float_to_pcm16_maps_range() {
        // +1.0, 0.0, -1.0 as f32 LE.
        let mut raw = Vec::new();
        for f in [1.0f32, 0.0, -1.0] {
            raw.extend_from_slice(&f.to_le_bytes());
        }
        let pcm = to_pcm16(&raw, true, 32, 1);
        let s: Vec<i16> = pcm
            .chunks(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(s[0], 32767);
        assert_eq!(s[1], 0);
        assert_eq!(s[2], -32767);
    }

    #[test]
    fn pcm16_passthrough() {
        let raw = vec![0x11, 0x22, 0x33, 0x44];
        assert_eq!(to_pcm16(&raw, false, 16, 1), raw);
    }
}
