use std::{
    mem::{ManuallyDrop, size_of},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, ensure};
use windows::{
    Win32::{
        Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT},
        Media::Audio::*,
        System::Com::{
            BLOB, COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize, IAgileObject,
            IAgileObject_Impl,
            StructuredStorage::{PROPVARIANT, PROPVARIANT_0, PROPVARIANT_0_0, PROPVARIANT_0_0_0},
        },
        System::Threading::{CreateEventW, WaitForSingleObject},
        System::Variant::VT_BLOB,
    },
    core::{HRESULT, Interface, Ref, implement},
};

use super::{CALL_FRAME_SAMPLES, VoiceFrames};
use crate::{SocialCallEvent, push_social_call_event};

const DESKTOP_SAMPLE_RATE: u32 = 44_100;

struct AudioEvent(HANDLE);

impl Drop for AudioEvent {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

pub(super) struct DesktopAudio(Arc<AtomicBool>);

impl DesktopAudio {
    pub(super) fn start(sender: SyncSender<[f32; CALL_FRAME_SAMPLES]>) -> anyhow::Result<Self> {
        let stopped = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stopped);
        thread::Builder::new()
            .name("zink-share-audio".into())
            .spawn(move || {
                let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED).ok() }
                    .context("initialize Windows audio capture")
                    .and_then(|()| {
                        let result = capture(&sender, &worker_stop);
                        unsafe { CoUninitialize() };
                        result
                    });
                if let Err(error) = result
                    && !worker_stop.load(Ordering::Acquire)
                {
                    tracing::warn!(%error, "screen share audio stopped");
                    push_social_call_event(SocialCallEvent::ScreenAudioFailed(format!(
                        "{error:#}"
                    )));
                }
            })?;
        Ok(Self(stopped))
    }
}

impl Drop for DesktopAudio {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

#[implement(IActivateAudioInterfaceCompletionHandler, IAgileObject)]
struct Activation(SyncSender<windows::core::Result<IAudioClient>>);

impl IAgileObject_Impl for Activation_Impl {}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let result = (|| {
            let mut status = HRESULT(0);
            let mut unknown = None;
            unsafe {
                operation
                    .ok()?
                    .GetActivateResult(&mut status, &mut unknown)?
            };
            status.ok()?;
            unknown
                .ok_or_else(|| windows::core::Error::from_hresult(HRESULT(0x80004003u32 as i32)))?
                .cast()
        })();
        let _ = self.0.try_send(result);
        Ok(())
    }
}

fn activate() -> anyhow::Result<IAudioClient> {
    let mut params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: std::process::id(),
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_EXCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    // The activation blob borrows stack storage. PROPVARIANT's Rust Drop
    // would call PropVariantClear, which assumes ownership of VT_BLOB data.
    let variant = ManuallyDrop::new(PROPVARIANT {
        Anonymous: PROPVARIANT_0 {
            Anonymous: ManuallyDrop::new(PROPVARIANT_0_0 {
                vt: VT_BLOB,
                Anonymous: PROPVARIANT_0_0_0 {
                    blob: BLOB {
                        cbSize: size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>() as u32,
                        pBlobData: (&mut params as *mut AUDIOCLIENT_ACTIVATION_PARAMS).cast(),
                    },
                },
                ..Default::default()
            }),
        },
    });
    let (result, receiver) = mpsc::sync_channel(1);
    let callback: IActivateAudioInterfaceCompletionHandler = Activation(result).into();
    let _operation = unsafe {
        ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&*variant),
            &callback,
        )
    }
    .context("activate Windows process loopback audio")?;
    receiver
        .recv_timeout(Duration::from_secs(3))
        .context("wait for Windows audio activation")?
        .context("open Windows process loopback audio")
}

fn capture(
    sender: &SyncSender<[f32; CALL_FRAME_SAMPLES]>,
    stopped: &AtomicBool,
) -> anyhow::Result<()> {
    let client = activate()?;
    if stopped.load(Ordering::Acquire) {
        return Ok(());
    }
    let format = WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: 2,
        nSamplesPerSec: DESKTOP_SAMPLE_RATE,
        nAvgBytesPerSec: DESKTOP_SAMPLE_RATE * 4,
        nBlockAlign: 4,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    let mut frames = VoiceFrames::new(DESKTOP_SAMPLE_RATE);
    unsafe {
        let ready = AudioEvent(CreateEventW(None, false, false, None)?);
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK
                | AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
            0,
            0,
            &format,
            None,
        )?;
        client.SetEventHandle(ready.0)?;
        let capture: IAudioCaptureClient = client.GetService()?;
        client.Start()?;
        while !stopped.load(Ordering::Acquire) {
            if capture.GetNextPacketSize()? == 0 {
                let wait = WaitForSingleObject(ready.0, 20);
                ensure!(
                    wait == WAIT_OBJECT_0 || wait == WAIT_TIMEOUT,
                    "wait for a Windows audio packet"
                );
                continue;
            }
            let (mut data, mut count, mut flags) = (std::ptr::null_mut(), 0, 0);
            capture.GetBuffer(&mut data, &mut count, &mut flags, None, None)?;
            let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
            if silent {
                for _ in 0..count {
                    frames.push(0.0, sender);
                }
            } else if !data.is_null() && count != 0 {
                let samples = std::slice::from_raw_parts(data.cast::<i16>(), count as usize * 2);
                for stereo in samples.chunks_exact(2) {
                    frames.push(
                        (f32::from(stereo[0]) + f32::from(stereo[1])) / 65_536.0,
                        sender,
                    );
                }
            }
            capture.ReleaseBuffer(count)?;
            ensure!(
                silent || !data.is_null() || count == 0,
                "Windows returned an empty audio buffer"
            );
        }
        client.Stop()?;
    }
    Ok(())
}
