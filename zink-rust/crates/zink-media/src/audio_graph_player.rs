use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use windows::Devices::Enumeration::DeviceInformation;
use windows::Foundation::{IReference, PropertyValue, TimeSpan, TypedEventHandler};
use windows::Media::Audio::{
    AudioDeviceNodeCreationStatus, AudioFileInputNode, AudioFileNodeCreationStatus, AudioGraph,
    AudioGraphCreationStatus, AudioGraphSettings, EqualizerEffectDefinition,
};
use windows::Media::Effects::IAudioEffectDefinition;
use windows::Media::Render::AudioRenderCategory;
use windows::Storage::StorageFile;
use windows::core::{HSTRING, Interface as _};

use crate::{MediaError, TransportSnapshot};

/// Native local-audio transport. Windows `AudioGraph` owns decoding, endpoint
/// rendering and its built-in ten-band equalizer; no external codec process or
/// separately registered `WinRT` component is required.
pub(super) struct AudioGraphPlayer {
    graph: AudioGraph,
    input: AudioFileInputNode,
    _output: windows::Media::Audio::AudioDeviceOutputNode,
    equalizers: Vec<EqualizerEffectDefinition>,
    path: PathBuf,
    is_playing: Arc<AtomicBool>,
    _file_completed_token: i64,
}

impl AudioGraphPlayer {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        path: &Path,
        device: Option<&DeviceInformation>,
        equalizer_enabled: bool,
        bands: [f64; 10],
        volume: f64,
        playback_rate: f64,
        repeat: bool,
    ) -> Result<Self, MediaError> {
        if !path.is_file() {
            return Err(MediaError::InvalidPath(path.to_path_buf()));
        }

        let settings = AudioGraphSettings::Create(AudioRenderCategory::Media).map_err(win)?;
        settings.SetMaxPlaybackSpeedFactor(4.0).map_err(win)?;
        if let Some(device) = device {
            settings.SetPrimaryRenderDevice(device).map_err(win)?;
        }
        let graph_result = AudioGraph::CreateAsync(&settings)
            .map_err(win)?
            .join()
            .map_err(win)?;
        if graph_result.Status().map_err(win)? != AudioGraphCreationStatus::Success {
            return Err(MediaError::Windows(format!(
                "AudioGraph creation failed: {:?} ({:?})",
                graph_result.Status().map_err(win)?,
                graph_result.ExtendedError().ok()
            )));
        }
        let graph = graph_result.Graph().map_err(win)?;

        let output_result = graph
            .CreateDeviceOutputNodeAsync()
            .map_err(win)?
            .join()
            .map_err(win)?;
        if output_result.Status().map_err(win)? != AudioDeviceNodeCreationStatus::Success {
            return Err(MediaError::Windows(format!(
                "audio output creation failed: {:?} ({:?})",
                output_result.Status().map_err(win)?,
                output_result.ExtendedError().ok()
            )));
        }
        let output = output_result.DeviceOutputNode().map_err(win)?;

        let file = StorageFile::GetFileFromPathAsync(&HSTRING::from(path.as_os_str()))
            .map_err(win)?
            .join()
            .map_err(win)?;
        let input_result = graph
            .CreateFileInputNodeAsync(&file)
            .map_err(win)?
            .join()
            .map_err(win)?;
        if input_result.Status().map_err(win)? != AudioFileNodeCreationStatus::Success {
            return Err(MediaError::Windows(format!(
                "audio file input creation failed: {:?} ({:?})",
                input_result.Status().map_err(win)?,
                input_result.ExtendedError().ok()
            )));
        }
        let input = input_result.FileInputNode().map_err(win)?;
        input.AddOutgoingConnection(&output).map_err(win)?;

        // Windows exposes four parametric bands per built-in definition. Three
        // chained definitions provide Zink's ten UI bands (with two unused
        // native bands left neutral).
        let mut equalizers = Vec::with_capacity(3);
        for _ in 0..3 {
            let equalizer = EqualizerEffectDefinition::Create(&graph).map_err(win)?;
            let effect: IAudioEffectDefinition = equalizer.cast().map_err(win)?;
            input
                .EffectDefinitions()
                .map_err(win)?
                .Append(&effect)
                .map_err(win)?;
            equalizers.push(equalizer);
        }

        let is_playing = Arc::new(AtomicBool::new(false));
        let completion_state = Arc::clone(&is_playing);
        let completion_handler =
            TypedEventHandler::<AudioFileInputNode, windows::core::IInspectable>::new(
                move |_sender, _args| {
                    completion_state.store(false, Ordering::Release);
                    Ok(())
                },
            );
        let file_completed_token = input.FileCompleted(&completion_handler).map_err(win)?;
        let player = Self {
            graph,
            input,
            _output: output,
            equalizers,
            path: path.to_path_buf(),
            is_playing,
            _file_completed_token: file_completed_token,
        };
        player.set_equalizer(equalizer_enabled, bands)?;
        player.set_volume(volume)?;
        player.set_playback_rate(playback_rate)?;
        player.set_repeat(repeat)?;
        player.graph.Start().map_err(win)?;
        player.input.Stop().map_err(win)?;
        Ok(player)
    }

    pub(super) fn play(&self) -> Result<(), MediaError> {
        self.input.Start().map_err(win)?;
        self.is_playing.store(true, Ordering::Release);
        Ok(())
    }

    pub(super) fn pause(&self) -> Result<(), MediaError> {
        self.input.Stop().map_err(win)?;
        self.is_playing.store(false, Ordering::Release);
        Ok(())
    }

    pub(super) fn stop(&self) -> Result<(), MediaError> {
        self.pause()?;
        self.input.Seek(TimeSpan { Duration: 0 }).map_err(win)
    }

    pub(super) fn set_volume(&self, volume: f64) -> Result<(), MediaError> {
        self.input
            .SetOutgoingGain(volume.clamp(0.0, 1.0))
            .map_err(win)
    }

    pub(super) fn set_playback_rate(&self, rate: f64) -> Result<(), MediaError> {
        self.input
            .SetPlaybackSpeedFactor(rate.clamp(0.25, 4.0))
            .map_err(win)
    }

    pub(super) fn set_repeat(&self, enabled: bool) -> Result<(), MediaError> {
        let count: IReference<i32> = PropertyValue::CreateInt32(if enabled { -1 } else { 0 })
            .map_err(win)?
            .cast()
            .map_err(win)?;
        self.input.SetLoopCount(&count).map_err(win)
    }

    pub(super) fn set_equalizer(&self, enabled: bool, gains: [f64; 10]) -> Result<(), MediaError> {
        const FREQUENCIES: [f64; 10] = [
            31.0, 62.0, 125.0, 250.0, 500.0, 1_000.0, 2_000.0, 4_000.0, 8_000.0, 16_000.0,
        ];
        let mut ui_index = 0_usize;
        for equalizer in &self.equalizers {
            let native_bands = equalizer.Bands().map_err(win)?;
            for native_index in 0..native_bands.Size().map_err(win)? {
                let band = native_bands.GetAt(native_index).map_err(win)?;
                if ui_index < gains.len() {
                    band.SetFrequencyCenter(FREQUENCIES[ui_index])
                        .map_err(win)?;
                    band.SetBandwidth(1.0).map_err(win)?;
                    // WinRT exposes this XAPO gain as a linear multiplier
                    // (1.0 = 0 dB), while Zink's UI stores decibels.
                    band.SetGain(10_f64.powf(gains[ui_index].clamp(-12.0, 12.0) / 20.0))
                        .map_err(win)?;
                } else {
                    band.SetGain(1.0).map_err(win)?;
                }
                ui_index += 1;
            }
            let effect: IAudioEffectDefinition = equalizer.cast().map_err(win)?;
            if enabled {
                self.input.EnableEffectsByDefinition(&effect)
            } else {
                self.input.DisableEffectsByDefinition(&effect)
            }
            .map_err(win)?;
        }
        if ui_index < gains.len() {
            return Err(MediaError::Windows(format!(
                "Windows exposed only {ui_index} native equalizer bands"
            )));
        }
        Ok(())
    }

    pub(super) fn transport_snapshot(&self) -> Result<TransportSnapshot, MediaError> {
        let position = self.input.Position().map_err(win)?.Duration.max(0);
        let duration = self.input.Duration().map_err(win)?.Duration.max(0);
        Ok(TransportSnapshot {
            position_ms: u64::try_from(position / 10_000).unwrap_or_default(),
            duration_ms: u64::try_from(duration / 10_000).unwrap_or_default(),
            is_playing: self.is_playing.load(Ordering::Acquire),
        })
    }

    pub(super) fn seek_fraction(&self, fraction: f64) -> Result<(), MediaError> {
        let duration = self.input.Duration().map_err(win)?.Duration.max(0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let target = (duration as f64 * fraction.clamp(0.0, 1.0)).round() as i64;
        self.input.Seek(TimeSpan { Duration: target }).map_err(win)
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(test)]
    pub(super) fn equalizer_band_gains(&self) -> Result<Vec<f64>, MediaError> {
        let mut gains = Vec::with_capacity(10);
        for equalizer in &self.equalizers {
            let bands = equalizer.Bands().map_err(win)?;
            for index in 0..bands.Size().map_err(win)? {
                if gains.len() == 10 {
                    return Ok(gains);
                }
                let linear = bands.GetAt(index).map_err(win)?.Gain().map_err(win)?;
                gains.push(20.0 * linear.log10());
            }
        }
        Ok(gains)
    }
}

#[allow(clippy::needless_pass_by_value)]
fn win(error: windows::core::Error) -> MediaError {
    MediaError::Windows(error.to_string())
}
