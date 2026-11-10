use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};

use thiserror::Error;

#[cfg(windows)]
mod audio_graph_player;

#[derive(Debug, Error)]
pub enum MediaError {
    #[error("unsupported media path: {0}")]
    InvalidPath(PathBuf),
    #[error("Windows media playback failed: {0}")]
    Windows(String),
    #[error("native media playback is supported only on Windows")]
    UnsupportedPlatform,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackState {
    Empty,
    Playing,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemMediaCommand {
    Play,
    Pause,
    Stop,
    Next,
    Previous,
    FastForward,
    Rewind,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TransportSnapshot {
    pub position_ms: u64,
    pub duration_ms: u64,
    pub is_playing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioOutputDevice {
    pub id: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrackInfo {
    pub index: u32,
    pub name: String,
    pub language: String,
    pub selected: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MediaTechnicalInfo {
    pub video_codec: String,
    pub width: u32,
    pub height: u32,
    pub frames_per_second: Option<f64>,
    pub video_bitrate: u32,
    pub audio_codec: String,
    pub audio_sample_rate: u32,
    pub audio_channels: u32,
    pub audio_bitrate: u32,
    pub decoder_status: String,
    pub dynamic_range: String,
    pub display_hdr_capability: String,
}

fn dynamic_range_label(transfer_function: Option<u32>) -> String {
    match transfer_function {
        Some(15) => "HDR10 / PQ (SMPTE ST 2084)".to_owned(),
        Some(16) => "HDR / HLG".to_owned(),
        Some(value) => format!("SDR or unspecified (transfer function {value})"),
        None => "Not reported by Windows".to_owned(),
    }
}

#[cfg(windows)]
fn active_display_hdr_capability() -> String {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709, DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020,
    };
    use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromWindow};
    use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;
    use windows::core::Interface as _;

    // SAFETY: these calls only query the foreground HWND, its monitor and DXGI
    // output descriptors. Returned COM interfaces own their lifetimes.
    unsafe {
        let monitor = MonitorFromWindow(GetForegroundWindow(), MONITOR_DEFAULTTONEAREST);
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else {
            return "Display HDR capability unavailable".to_owned();
        };
        for adapter_index in 0..32 {
            let Ok(adapter) = factory.EnumAdapters1(adapter_index) else {
                break;
            };
            for output_index in 0..32 {
                let Ok(output) = adapter.EnumOutputs(output_index) else {
                    break;
                };
                let Ok(output) = output.cast::<IDXGIOutput6>() else {
                    continue;
                };
                let Ok(description) = output.GetDesc1() else {
                    continue;
                };
                if description.Monitor != monitor {
                    continue;
                }
                let hdr = description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
                    || description.ColorSpace == DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709;
                return format!(
                    "{} • {}-bit • peak {:.0} nits • full-frame {:.0} nits",
                    if hdr { "HDR active" } else { "SDR output" },
                    description.BitsPerColor,
                    description.MaxLuminance,
                    description.MaxFullFrameLuminance
                );
            }
        }
    }
    "Active display was not found through DXGI".to_owned()
}

#[cfg(not(windows))]
fn active_display_hdr_capability() -> String {
    "Display HDR capability unavailable".to_owned()
}

/// A BGRA8 video frame produced by Windows Media Foundation's frame-server
/// path. The bounded receiver only retains the newest frame so decoding cannot
/// make the Slint event loop or application memory grow without limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub bgra: Vec<u8>,
}

pub struct MediaEngine {
    inner: PlatformMediaEngine,
    standby: PlatformMediaEngine,
    state: PlaybackState,
    current_path: Option<PathBuf>,
    prepared_path: Option<PathBuf>,
    volume: f64,
    playback_rate: f64,
    crossfade_active: bool,
    active_gain: f64,
    prepared_gain: f64,
    equalizer_gain: f64,
    equalizer_bands: [f64; 10],
    system_command_receiver: Receiver<SystemMediaCommand>,
    video_frame_receiver: Receiver<VideoFrame>,
}

impl MediaEngine {
    /// Creates the Windows native playback engine.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot activate its media player.
    pub fn new() -> Result<Self, MediaError> {
        let (system_command_sender, system_command_receiver) = mpsc::channel();
        let (video_frame_sender, video_frame_receiver) = mpsc::sync_channel(1);
        let inner = PlatformMediaEngine::new(
            system_command_sender.clone(),
            true,
            Some(video_frame_sender),
        )?;
        let standby = PlatformMediaEngine::new(system_command_sender, false, None)?;
        Ok(Self {
            inner,
            standby,
            state: PlaybackState::Empty,
            current_path: None,
            prepared_path: None,
            volume: 0.8,
            playback_rate: 1.0,
            crossfade_active: false,
            active_gain: 1.0,
            prepared_gain: 1.0,
            equalizer_gain: 1.0,
            equalizer_bands: [0.0; 10],
            system_command_receiver,
            video_frame_receiver,
        })
    }

    /// Returns the most recent decoded video frame, discarding superseded
    /// frames. This keeps presentation responsive when UI rendering is slower
    /// than the source frame rate.
    #[must_use]
    pub fn take_latest_video_frame(&self) -> Option<VideoFrame> {
        self.video_frame_receiver.try_iter().last()
    }

    /// Drains pending commands received from Windows media keys and the system
    /// media flyout. Commands are processed by the Slint event loop so native
    /// callbacks never mutate UI state from a Windows worker thread.
    #[must_use]
    pub fn drain_system_media_commands(&self) -> Vec<SystemMediaCommand> {
        self.system_command_receiver.try_iter().collect()
    }

    /// Enumerates currently available Windows audio-render endpoints.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows device enumeration fails.
    pub fn audio_output_devices() -> Result<Vec<AudioOutputDevice>, MediaError> {
        PlatformMediaEngine::audio_output_devices()
    }

    /// Routes both active and standby players to the selected Windows endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error when the endpoint is unavailable or Windows rejects routing.
    pub fn set_audio_output_device(&mut self, id: &str) -> Result<(), MediaError> {
        let device = PlatformMediaEngine::audio_output_device(id)?;
        self.inner.set_audio_output_device(&device)?;
        self.standby.set_audio_output_device(&device)
    }

    /// Lists embedded audio streams exposed by the active Media Foundation
    /// playback item. An empty list is valid while Windows is still opening the
    /// source or when the container has no selectable alternate stream.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot inspect the active playback item.
    pub fn audio_tracks(&self) -> Result<Vec<AudioTrackInfo>, MediaError> {
        self.inner.audio_tracks()
    }

    /// Reads the encoding properties Windows Media Foundation selected for the
    /// active playback item. `None` is expected while a source is still opening.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot inspect the active playback item.
    pub fn technical_info(&self) -> Result<Option<MediaTechnicalInfo>, MediaError> {
        self.inner.technical_info()
    }

    /// Selects an embedded audio stream without reopening the video.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is stale or Windows rejects the stream.
    pub fn select_audio_track(&self, index: u32) -> Result<(), MediaError> {
        self.inner.select_audio_track(index)
    }

    /// Opens a local media path and starts playback.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid file path or a Windows playback failure.
    pub fn open(&mut self, path: &Path) -> Result<(), MediaError> {
        self.active_gain = 1.0;
        self.inner
            .set_volume(self.effective_volume(self.active_gain))?;
        self.inner.open(path)?;
        self.current_path = Some(path.to_path_buf());
        self.prepared_path = None;
        self.crossfade_active = false;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    /// Preloads the next local item into the standby native player.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot create the standby media source.
    pub fn prepare_next(&mut self, path: &Path) -> Result<(), MediaError> {
        if self.prepared_path.as_deref() == Some(path) {
            return Ok(());
        }
        self.standby.prepare(path)?;
        self.prepared_gain = 1.0;
        self.prepared_path = Some(path.to_path_buf());
        Ok(())
    }

    /// Sets non-destructive `ReplayGain` amplification for the active item.
    /// The optional peak limits positive gain to avoid digital clipping.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the adjusted player volume.
    pub fn set_normalization_gain(
        &mut self,
        gain_db: f64,
        peak: Option<f64>,
    ) -> Result<f64, MediaError> {
        self.active_gain = gain_multiplier(gain_db, peak);
        self.inner
            .set_volume(self.effective_volume(self.active_gain))?;
        Ok(self.active_gain)
    }

    /// Stores `ReplayGain` amplification for the preloaded standby item.
    pub fn set_prepared_normalization_gain(&mut self, gain_db: f64, peak: Option<f64>) {
        self.prepared_gain = gain_multiplier(gain_db, peak);
    }

    /// Starts the preloaded item and atomically makes its player active.
    ///
    /// # Errors
    ///
    /// Returns an error if the requested item was not prepared or Windows rejects playback.
    pub fn play_prepared(&mut self, path: &Path) -> Result<(), MediaError> {
        if self.current_path.as_deref() == Some(path) && self.state == PlaybackState::Playing {
            self.prepared_path = None;
            self.crossfade_active = false;
            return Ok(());
        }
        if self.prepared_path.as_deref() != Some(path) {
            return self.open(path);
        }
        self.standby
            .set_volume(self.effective_volume(self.prepared_gain))?;
        self.standby.set_playback_rate(self.playback_rate)?;
        self.standby.update_system_media_metadata(path)?;
        self.standby.play()?;
        self.inner.pause()?;
        self.standby.set_system_controls_enabled(true)?;
        self.inner.set_system_controls_enabled(false)?;
        std::mem::swap(&mut self.inner, &mut self.standby);
        self.standby.set_volume(self.effective_volume(1.0))?;
        self.current_path = Some(path.to_path_buf());
        self.prepared_path = None;
        self.crossfade_active = false;
        self.active_gain = self.prepared_gain;
        self.prepared_gain = 1.0;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    /// Starts the prepared player silently so its volume can be ramped in.
    ///
    /// # Errors
    ///
    /// Returns an error when the item cannot be prepared or Windows rejects playback.
    pub fn begin_crossfade(&mut self, path: &Path) -> Result<(), MediaError> {
        if self.prepared_path.as_deref() != Some(path) {
            self.prepare_next(path)?;
        }
        self.standby.set_volume(0.0)?;
        self.standby.set_playback_rate(self.playback_rate)?;
        self.standby.update_system_media_metadata(path)?;
        self.standby.play()?;
        self.crossfade_active = true;
        Ok(())
    }

    /// Applies a normalized crossfade ramp and completes the player swap at one.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects either native volume change.
    pub fn set_crossfade_progress(&mut self, progress: f64) -> Result<bool, MediaError> {
        if !self.crossfade_active {
            return Ok(false);
        }
        let progress = progress.clamp(0.0, 1.0);
        self.inner.set_volume(
            (self.effective_volume(self.active_gain) * (1.0 - progress)).clamp(0.0, 1.0),
        )?;
        self.standby
            .set_volume((self.effective_volume(self.prepared_gain) * progress).clamp(0.0, 1.0))?;
        if progress < 1.0 {
            return Ok(false);
        }
        self.inner.pause()?;
        self.inner
            .set_volume(self.effective_volume(self.active_gain))?;
        self.standby.set_system_controls_enabled(true)?;
        self.inner.set_system_controls_enabled(false)?;
        std::mem::swap(&mut self.inner, &mut self.standby);
        self.current_path.clone_from(&self.prepared_path);
        self.active_gain = self.prepared_gain;
        self.prepared_gain = 1.0;
        self.crossfade_active = false;
        self.state = PlaybackState::Playing;
        Ok(true)
    }

    /// Opens an HTTP(S) media stream and starts playback.
    ///
    /// # Errors
    ///
    /// Returns an error when the URI is invalid or Windows rejects it.
    pub fn open_uri(&mut self, uri: &str) -> Result<(), MediaError> {
        self.inner.open_uri(uri)?;
        self.current_path = None;
        self.prepared_path = None;
        self.crossfade_active = false;
        self.state = PlaybackState::Playing;
        Ok(())
    }

    /// Publishes the current track details to Windows System Media Transport
    /// Controls so hardware keys and the Windows media flyout identify Zink's
    /// active song correctly.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the display metadata update.
    pub fn update_system_music_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
    ) -> Result<(), MediaError> {
        self.inner
            .update_system_music_metadata(title, artist, album)
    }

    /// Toggles between playing and paused when media is loaded.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the playback command.
    pub fn toggle(&mut self) -> Result<PlaybackState, MediaError> {
        self.state = match self.state {
            PlaybackState::Empty => PlaybackState::Empty,
            PlaybackState::Playing => {
                self.inner.pause()?;
                PlaybackState::Paused
            }
            PlaybackState::Paused => {
                self.inner.play()?;
                PlaybackState::Playing
            }
        };
        Ok(self.state)
    }

    /// Starts or resumes the loaded item.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the playback command.
    pub fn play(&mut self) -> Result<(), MediaError> {
        if self.state != PlaybackState::Empty {
            self.inner.play()?;
            self.state = PlaybackState::Playing;
        }
        Ok(())
    }

    /// Pauses the loaded item.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the pause command.
    pub fn pause(&mut self) -> Result<(), MediaError> {
        if self.state != PlaybackState::Empty {
            self.inner.pause()?;
            self.state = PlaybackState::Paused;
        }
        Ok(())
    }

    #[must_use]
    pub fn state(&self) -> PlaybackState {
        self.state
    }

    #[must_use]
    pub fn current_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    /// Adjusts the native player volume and returns the clamped value.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the volume change.
    pub fn adjust_volume(&mut self, delta: f64) -> Result<f64, MediaError> {
        self.set_volume(self.volume + delta)
    }

    /// Sets the native player volume and returns the clamped value.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the volume change.
    pub fn set_volume(&mut self, volume: f64) -> Result<f64, MediaError> {
        self.volume = volume.clamp(0.0, 1.0);
        self.inner
            .set_volume(self.effective_volume(self.active_gain))?;
        Ok(self.volume)
    }

    /// Applies the Equalizer page's master gain to native playback. Passing
    /// `None` bypasses the equalizer gain without discarding the saved value.
    /// `ReplayGain` and crossfade gains remain multiplicative and are preserved.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the new native player volume.
    pub fn set_equalizer_master_gain(&mut self, gain_db: Option<f64>) -> Result<(), MediaError> {
        self.set_equalizer_profile(gain_db, self.equalizer_bands)
    }

    /// Applies the complete saved equalizer profile to the native playback
    /// gain stage. The strongest positive band receives matching automatic
    /// preamp headroom, preventing a boosted curve from clipping before the
    /// platform-specific frequency processor is inserted.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects either player volume change.
    pub fn set_equalizer_profile(
        &mut self,
        master_gain_db: Option<f64>,
        bands_db: [f64; 10],
    ) -> Result<(), MediaError> {
        self.equalizer_bands = bands_db.map(|gain| gain.clamp(-12.0, 12.0));
        self.equalizer_gain = equalizer_preamp_multiplier(master_gain_db, &self.equalizer_bands);
        self.inner
            .set_equalizer_profile(master_gain_db.is_some(), self.equalizer_bands)?;
        self.standby
            .set_equalizer_profile(master_gain_db.is_some(), self.equalizer_bands)?;
        self.inner
            .set_volume(self.effective_volume(self.active_gain))?;
        self.standby
            .set_volume(self.effective_volume(self.prepared_gain))
    }

    /// Returns the automatic preamp attenuation currently protecting boosted
    /// equalizer curves from digital clipping.
    #[must_use]
    pub fn equalizer_headroom_db(&self) -> f64 {
        let strongest_boost = self.equalizer_bands.iter().copied().fold(0.0_f64, f64::max);
        -strongest_boost
    }

    fn effective_volume(&self, track_gain: f64) -> f64 {
        (self.volume * track_gain * self.equalizer_gain).clamp(0.0, 1.0)
    }

    /// Enables or disables looping of the currently loaded item.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the loop setting.
    pub fn set_repeat_current(&self, enabled: bool) -> Result<(), MediaError> {
        self.inner.set_repeat_current(enabled)
    }

    #[must_use]
    pub fn volume(&self) -> f64 {
        self.volume
    }

    /// Sets the native playback rate while preserving audio pitch when Windows supports it.
    ///
    /// # Errors
    ///
    /// Returns an error when the requested rate is unsupported or rejected by Windows.
    pub fn set_playback_rate(&mut self, rate: f64) -> Result<f64, MediaError> {
        let rate = rate.clamp(0.5, 2.0);
        self.inner.set_playback_rate(rate)?;
        self.playback_rate = rate;
        Ok(rate)
    }

    #[must_use]
    pub fn playback_rate(&self) -> f64 {
        self.playback_rate
    }

    /// Reads current position, duration and native playback state.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows cannot query the playback session.
    pub fn transport_snapshot(&self) -> Result<TransportSnapshot, MediaError> {
        self.inner.transport_snapshot()
    }

    /// Seeks to a normalized position between zero and one.
    ///
    /// # Errors
    ///
    /// Returns an error when the playback session rejects the seek.
    pub fn seek_fraction(&self, fraction: f64) -> Result<(), MediaError> {
        self.inner.seek_fraction(fraction.clamp(0.0, 1.0))
    }

    /// Seeks forward or backward by a relative number of seconds.
    ///
    /// # Errors
    ///
    /// Returns an error when the playback session cannot be queried or seeked.
    #[allow(clippy::cast_precision_loss)]
    pub fn seek_relative(&self, seconds: f64) -> Result<(), MediaError> {
        let snapshot = self.transport_snapshot()?;
        if snapshot.duration_ms == 0 {
            return Ok(());
        }
        let current = snapshot.position_ms as f64 / 1_000.0;
        let duration = snapshot.duration_ms as f64 / 1_000.0;
        self.inner
            .seek_fraction(((current + seconds) / duration).clamp(0.0, 1.0))
    }

    /// Stops playback and returns to the beginning of the current item.
    ///
    /// # Errors
    ///
    /// Returns an error when Windows rejects the pause or seek operation.
    pub fn stop(&mut self) -> Result<(), MediaError> {
        self.inner.stop()?;
        if self.state != PlaybackState::Empty {
            self.state = PlaybackState::Paused;
        }
        Ok(())
    }
}

fn gain_multiplier(gain_db: f64, peak: Option<f64>) -> f64 {
    let requested = db_multiplier(gain_db, -20.0, 12.0);
    peak.filter(|peak| peak.is_finite() && *peak > 0.0)
        .map_or(requested, |peak| requested.min(1.0 / peak))
        .clamp(0.1, 4.0)
}

fn db_multiplier(gain_db: f64, minimum: f64, maximum: f64) -> f64 {
    10_f64.powf(gain_db.clamp(minimum, maximum) / 20.0)
}

fn equalizer_preamp_multiplier(master_gain_db: Option<f64>, bands_db: &[f64; 10]) -> f64 {
    let Some(master_gain_db) = master_gain_db else {
        return 1.0;
    };
    let strongest_boost = bands_db.iter().copied().fold(0.0_f64, f64::max);
    db_multiplier(
        master_gain_db.clamp(-12.0, 12.0) - strongest_boost,
        -24.0,
        12.0,
    )
}

#[cfg(windows)]
struct PlatformMediaEngine {
    player: windows::Media::Playback::MediaPlayer,
    current_item: RefCell<Option<windows::Media::Playback::MediaPlaybackItem>>,
    audio_graph: RefCell<Option<audio_graph_player::AudioGraphPlayer>>,
    audio_device: RefCell<Option<windows::Devices::Enumeration::DeviceInformation>>,
    equalizer_enabled: std::cell::Cell<bool>,
    equalizer_bands: RefCell<[f64; 10]>,
    volume: std::cell::Cell<f64>,
    playback_rate: std::cell::Cell<f64>,
    repeat_current: std::cell::Cell<bool>,
    _button_pressed_token: i64,
    _video_frame_token: Option<i64>,
}

#[cfg(windows)]
#[allow(clippy::borrow_as_ptr, clippy::too_many_lines)]
fn install_video_frame_server(
    player: &windows::Media::Playback::MediaPlayer,
    sender: std::sync::mpsc::SyncSender<VideoFrame>,
) -> Result<i64, MediaError> {
    use windows::Foundation::TypedEventHandler;
    use windows::Graphics::DirectX::Direct3D11::IDirect3DSurface;
    use windows::Media::Playback::MediaPlayer;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11_BIND_RENDER_TARGET, D3D11_BIND_SHADER_RESOURCE, D3D11_CPU_ACCESS_READ,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE,
        D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT, D3D11_USAGE_STAGING,
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Multithread, ID3D11Texture2D,
    };
    use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
    use windows::Win32::Graphics::Dxgi::IDXGISurface;
    use windows::Win32::System::WinRT::Direct3D11::CreateDirect3D11SurfaceFromDXGISurface;
    use windows::core::{IInspectable, Interface};

    struct FrameServerResources {
        surface: IDirect3DSurface,
        output_texture: ID3D11Texture2D,
        staging: ID3D11Texture2D,
        context: ID3D11DeviceContext,
    }
    // The D3D device is explicitly multithread protected above. These COM
    // interfaces are therefore safe to move into MediaPlayer's frame callback.
    unsafe impl Send for FrameServerResources {}
    impl FrameServerResources {
        fn capture(&self, source: &MediaPlayer) -> Option<VideoFrame> {
            if source
                .CopyFrameToVideoSurfaceWithTargetRectangle(
                    &self.surface,
                    windows::Foundation::Rect {
                        X: 0.0,
                        Y: 0.0,
                        Width: 960.0,
                        Height: 540.0,
                    },
                )
                .is_err()
            {
                return None;
            }
            unsafe {
                self.context
                    .CopyResource(&self.staging, &self.output_texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                if self
                    .context
                    .Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .is_err()
                {
                    return None;
                }
                let row_bytes = 960_usize * 4;
                let row_pitch = usize::try_from(mapped.RowPitch).unwrap_or_default();
                let height = 540_usize;
                let source_bytes = std::slice::from_raw_parts(
                    mapped.pData.cast::<u8>(),
                    row_pitch.saturating_mul(height),
                );
                let mut pixels = vec![0_u8; row_bytes.saturating_mul(height)];
                for row in 0..height {
                    let source_start = row.saturating_mul(row_pitch);
                    let target_start = row.saturating_mul(row_bytes);
                    pixels[target_start..target_start + row_bytes]
                        .copy_from_slice(&source_bytes[source_start..source_start + row_bytes]);
                }
                self.context.Unmap(&self.staging, 0);
                Some(VideoFrame {
                    width: 960,
                    height: 540,
                    bgra: pixels,
                })
            }
        }
    }

    const FRAME_WIDTH: u32 = 960;
    const FRAME_HEIGHT: u32 = 540;

    let mut device = None;
    let mut context = None;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|error| MediaError::Windows(error.to_string()))?;
    let device: ID3D11Device = device.ok_or_else(|| {
        MediaError::Windows("D3D11 video device creation returned no device".into())
    })?;
    let context: ID3D11DeviceContext = context.ok_or_else(|| {
        MediaError::Windows("D3D11 video device creation returned no context".into())
    })?;
    if let Ok(multithread) = device.cast::<ID3D11Multithread>() {
        unsafe {
            let _ = multithread.SetMultithreadProtected(true);
        }
    }

    let output_desc = D3D11_TEXTURE2D_DESC {
        Width: FRAME_WIDTH,
        Height: FRAME_HEIGHT,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };
    let mut output_texture = None;
    unsafe { device.CreateTexture2D(&output_desc, None, Some(&mut output_texture)) }
        .map_err(|error| MediaError::Windows(error.to_string()))?;
    let output_texture: ID3D11Texture2D = output_texture
        .ok_or_else(|| MediaError::Windows("video output texture was not created".into()))?;
    let dxgi_surface: IDXGISurface = output_texture
        .cast()
        .map_err(|error| MediaError::Windows(error.to_string()))?;
    let inspectable = unsafe { CreateDirect3D11SurfaceFromDXGISurface(&dxgi_surface) }
        .map_err(|error| MediaError::Windows(error.to_string()))?;
    let surface: IDirect3DSurface = inspectable
        .cast()
        .map_err(|error| MediaError::Windows(error.to_string()))?;

    let staging_desc = D3D11_TEXTURE2D_DESC {
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
        MiscFlags: 0,
        ..output_desc
    };
    let mut staging = None;
    unsafe { device.CreateTexture2D(&staging_desc, None, Some(&mut staging)) }
        .map_err(|error| MediaError::Windows(error.to_string()))?;
    let staging: ID3D11Texture2D = staging
        .ok_or_else(|| MediaError::Windows("video staging texture was not created".into()))?;

    player
        .SetIsVideoFrameServerEnabled(true)
        .map_err(|error| MediaError::Windows(error.to_string()))?;
    let resources = FrameServerResources {
        surface,
        output_texture,
        staging,
        context,
    };
    let handler = TypedEventHandler::<MediaPlayer, IInspectable>::new(move |source, _| {
        let Some(source) = source.as_ref() else {
            return Ok(());
        };
        if let Some(frame) = resources.capture(source) {
            let _ = sender.try_send(frame);
        }
        Ok(())
    });
    player
        .VideoFrameAvailable(&handler)
        .map_err(|error| MediaError::Windows(error.to_string()))
}

#[cfg(windows)]
impl PlatformMediaEngine {
    fn audio_output_devices() -> Result<Vec<AudioOutputDevice>, MediaError> {
        use windows::Devices::Enumeration::{DeviceClass, DeviceInformation};

        let operation = DeviceInformation::FindAllAsyncDeviceClass(DeviceClass::AudioRender)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let collection = operation
            .join()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let size = collection
            .Size()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let mut devices = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        for index in 0..size {
            let device = collection
                .GetAt(index)
                .map_err(|error| MediaError::Windows(error.to_string()))?;
            devices.push(AudioOutputDevice {
                id: device
                    .Id()
                    .map_err(|error| MediaError::Windows(error.to_string()))?
                    .to_string(),
                name: device
                    .Name()
                    .map_err(|error| MediaError::Windows(error.to_string()))?
                    .to_string(),
            });
        }
        Ok(devices)
    }

    fn audio_output_device(
        id: &str,
    ) -> Result<windows::Devices::Enumeration::DeviceInformation, MediaError> {
        use windows::Devices::Enumeration::DeviceInformation;

        let operation = DeviceInformation::CreateFromIdAsync(&windows::core::HSTRING::from(id))
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        operation
            .join()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_audio_output_device(
        &self,
        device: &windows::Devices::Enumeration::DeviceInformation,
    ) -> Result<(), MediaError> {
        self.player
            .SetAudioDevice(device)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        self.audio_device.replace(Some(device.clone()));
        let previous = self.audio_graph.borrow_mut().take();
        if let Some(previous) = previous {
            let snapshot = previous.transport_snapshot()?;
            let replacement = audio_graph_player::AudioGraphPlayer::new(
                previous.path(),
                Some(device),
                self.equalizer_enabled.get(),
                *self.equalizer_bands.borrow(),
                self.volume.get(),
                self.playback_rate.get(),
                self.repeat_current.get(),
            );
            let replacement = match replacement {
                Ok(replacement) => replacement,
                Err(error) => {
                    self.audio_graph.replace(Some(previous));
                    return Err(error);
                }
            };
            if snapshot.duration_ms > 0 {
                #[allow(clippy::cast_precision_loss)]
                replacement
                    .seek_fraction(snapshot.position_ms as f64 / snapshot.duration_ms as f64)?;
            }
            if snapshot.is_playing {
                replacement.play()?;
            }
            self.audio_graph.replace(Some(replacement));
        }
        Ok(())
    }

    fn new(
        command_sender: Sender<SystemMediaCommand>,
        controls_enabled: bool,
        video_frame_sender: Option<std::sync::mpsc::SyncSender<VideoFrame>>,
    ) -> Result<Self, MediaError> {
        use windows::Foundation::TypedEventHandler;
        use windows::Media::{
            SystemMediaTransportControls, SystemMediaTransportControlsButton,
            SystemMediaTransportControlsButtonPressedEventArgs,
        };

        let player = windows::Media::Playback::MediaPlayer::new()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        player
            .SetAutoPlay(false)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        player
            .SetVolume(0.8)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let controls = player
            .SystemMediaTransportControls()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        controls
            .SetIsEnabled(controls_enabled)
            .and_then(|()| controls.SetIsPlayEnabled(true))
            .and_then(|()| controls.SetIsPauseEnabled(true))
            .and_then(|()| controls.SetIsStopEnabled(true))
            .and_then(|()| controls.SetIsNextEnabled(true))
            .and_then(|()| controls.SetIsPreviousEnabled(true))
            .and_then(|()| controls.SetIsFastForwardEnabled(true))
            .and_then(|()| controls.SetIsRewindEnabled(true))
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let handler = TypedEventHandler::<
            SystemMediaTransportControls,
            SystemMediaTransportControlsButtonPressedEventArgs,
        >::new(move |_sender, args| {
            let command = match args.ok()?.Button()? {
                SystemMediaTransportControlsButton::Play => SystemMediaCommand::Play,
                SystemMediaTransportControlsButton::Pause => SystemMediaCommand::Pause,
                SystemMediaTransportControlsButton::Stop => SystemMediaCommand::Stop,
                SystemMediaTransportControlsButton::Next => SystemMediaCommand::Next,
                SystemMediaTransportControlsButton::Previous => SystemMediaCommand::Previous,
                SystemMediaTransportControlsButton::FastForward => SystemMediaCommand::FastForward,
                SystemMediaTransportControlsButton::Rewind => SystemMediaCommand::Rewind,
                _ => return Ok(()),
            };
            let _ = command_sender.send(command);
            Ok(())
        });
        let button_pressed_token = controls
            .ButtonPressed(&handler)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let video_frame_token = if let Some(sender) = video_frame_sender {
            Some(install_video_frame_server(&player, sender)?)
        } else {
            None
        };
        Ok(Self {
            player,
            current_item: RefCell::new(None),
            audio_graph: RefCell::new(None),
            audio_device: RefCell::new(None),
            equalizer_enabled: std::cell::Cell::new(false),
            equalizer_bands: RefCell::new([0.0; 10]),
            volume: std::cell::Cell::new(0.8),
            playback_rate: std::cell::Cell::new(1.0),
            repeat_current: std::cell::Cell::new(false),
            _button_pressed_token: button_pressed_token,
            _video_frame_token: video_frame_token,
        })
    }

    fn set_system_controls_enabled(&self, enabled: bool) -> Result<(), MediaError> {
        self.player
            .SystemMediaTransportControls()
            .and_then(|controls| controls.SetIsEnabled(enabled))
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn open(&self, path: &Path) -> Result<(), MediaError> {
        self.prepare(path)?;
        self.update_system_media_metadata(path)?;
        self.play()
    }

    fn prepare(&self, path: &Path) -> Result<(), MediaError> {
        let url = url::Url::from_file_path(path)
            .map_err(|()| MediaError::InvalidPath(path.to_path_buf()))?;
        let uri = windows::Foundation::Uri::CreateUri(&windows::core::HSTRING::from(url.as_str()))
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        if matches!(
            extension.as_str(),
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v"
        ) {
            self.audio_graph.replace(None);
            self.set_media_uri(&uri)?;
        } else {
            self.current_item.replace(None);
            self.player.Pause().ok();
            let graph = audio_graph_player::AudioGraphPlayer::new(
                path,
                self.audio_device.borrow().as_ref(),
                self.equalizer_enabled.get(),
                *self.equalizer_bands.borrow(),
                self.volume.get(),
                self.playback_rate.get(),
                self.repeat_current.get(),
            )?;
            self.audio_graph.replace(Some(graph));
        }
        Ok(())
    }

    fn open_uri(&self, uri: &str) -> Result<(), MediaError> {
        let uri = windows::Foundation::Uri::CreateUri(&windows::core::HSTRING::from(uri))
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        self.current_item.replace(None);
        self.audio_graph.replace(None);
        self.player
            .SetUriSource(&uri)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        self.player
            .Play()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_media_uri(&self, uri: &windows::Foundation::Uri) -> Result<(), MediaError> {
        use windows::Media::Core::MediaSource;
        use windows::Media::Playback::MediaPlaybackItem;

        let source = MediaSource::CreateFromUri(uri)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let item = MediaPlaybackItem::Create(&source)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        self.player
            .SetSource(&item)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        self.current_item.replace(Some(item));
        Ok(())
    }

    fn audio_tracks(&self) -> Result<Vec<AudioTrackInfo>, MediaError> {
        let item = self.current_item.borrow();
        let Some(item) = item.as_ref() else {
            return Ok(Vec::new());
        };
        let tracks = item
            .AudioTracks()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let size = tracks
            .Size()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let selected = tracks.SelectedIndex().unwrap_or(-1);
        let mut result = Vec::with_capacity(usize::try_from(size).unwrap_or_default());
        for index in 0..size {
            let track = tracks
                .GetAt(index)
                .map_err(|error| MediaError::Windows(error.to_string()))?;
            let name = track
                .Name()
                .map(|value| value.to_string())
                .unwrap_or_default();
            let language = track
                .Language()
                .map(|value| value.to_string())
                .unwrap_or_default();
            result.push(AudioTrackInfo {
                index,
                name: if name.is_empty() {
                    format!("Audio {}", index + 1)
                } else {
                    name
                },
                language,
                selected: i32::try_from(index) == Ok(selected),
            });
        }
        Ok(result)
    }

    fn technical_info(&self) -> Result<Option<MediaTechnicalInfo>, MediaError> {
        use windows::Foundation::IPropertyValue;
        use windows::Win32::Media::MediaFoundation::MF_MT_TRANSFER_FUNCTION;
        use windows::core::Interface as _;

        let item = self.current_item.borrow();
        let Some(item) = item.as_ref() else {
            return Ok(None);
        };
        let video_tracks = item
            .VideoTracks()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let selected_video = video_tracks.SelectedIndex().unwrap_or(-1);
        if selected_video < 0 {
            return Ok(None);
        }
        let video_track = video_tracks
            .GetAt(u32::try_from(selected_video).unwrap_or_default())
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let video = video_track
            .GetEncodingProperties()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let frame_rate = video.FrameRate().ok();
        let frames_per_second = frame_rate.and_then(|ratio| {
            let numerator = ratio.Numerator().ok()?;
            let denominator = ratio.Denominator().ok()?;
            (denominator != 0).then_some(f64::from(numerator) / f64::from(denominator))
        });

        let audio_tracks = item
            .AudioTracks()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let selected_audio = audio_tracks.SelectedIndex().unwrap_or(-1);
        let audio = if selected_audio >= 0 {
            audio_tracks
                .GetAt(u32::try_from(selected_audio).unwrap_or_default())
                .ok()
                .and_then(|track| track.GetEncodingProperties().ok())
        } else {
            None
        };
        let decoder_status = video_track
            .SupportInfo()
            .ok()
            .and_then(|support| support.DecoderStatus().ok())
            .map_or_else(|| "Unknown".to_owned(), |status| format!("{status:?}"));
        let transfer_function = video
            .Properties()
            .ok()
            .and_then(|properties| properties.Lookup(MF_MT_TRANSFER_FUNCTION).ok())
            .and_then(|value| value.cast::<IPropertyValue>().ok())
            .and_then(|value| {
                value.GetUInt32().ok().or_else(|| {
                    value
                        .GetInt32()
                        .ok()
                        .and_then(|number| u32::try_from(number).ok())
                })
            });

        Ok(Some(MediaTechnicalInfo {
            video_codec: video
                .Subtype()
                .map(|value| value.to_string())
                .unwrap_or_default(),
            width: video.Width().unwrap_or_default(),
            height: video.Height().unwrap_or_default(),
            frames_per_second,
            video_bitrate: video.Bitrate().unwrap_or_default(),
            audio_codec: audio
                .as_ref()
                .and_then(|properties| properties.Subtype().ok())
                .map(|value| value.to_string())
                .unwrap_or_default(),
            audio_sample_rate: audio
                .as_ref()
                .and_then(|properties| properties.SampleRate().ok())
                .unwrap_or_default(),
            audio_channels: audio
                .as_ref()
                .and_then(|properties| properties.ChannelCount().ok())
                .unwrap_or_default(),
            audio_bitrate: audio
                .as_ref()
                .and_then(|properties| properties.Bitrate().ok())
                .unwrap_or_default(),
            decoder_status,
            dynamic_range: dynamic_range_label(transfer_function),
            display_hdr_capability: active_display_hdr_capability(),
        }))
    }

    fn select_audio_track(&self, index: u32) -> Result<(), MediaError> {
        let item = self.current_item.borrow();
        let item = item
            .as_ref()
            .ok_or_else(|| MediaError::Windows("no active playback item".into()))?;
        let tracks = item
            .AudioTracks()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        if index >= tracks.Size().unwrap_or_default() {
            return Err(MediaError::Windows(
                "audio track is no longer available".into(),
            ));
        }
        tracks
            .SetSelectedIndex(i32::try_from(index).unwrap_or(i32::MAX))
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn update_system_media_metadata(&self, path: &Path) -> Result<(), MediaError> {
        use windows::Media::MediaPlaybackType;

        let controls = self
            .player
            .SystemMediaTransportControls()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let updater = controls
            .DisplayUpdater()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        let is_video = matches!(
            extension.as_str(),
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v"
        );
        updater
            .SetType(if is_video {
                MediaPlaybackType::Video
            } else {
                MediaPlaybackType::Music
            })
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let title = windows::core::HSTRING::from(
            path.file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Unknown media"),
        );
        if is_video {
            updater
                .VideoProperties()
                .and_then(|properties| properties.SetTitle(&title))
                .map_err(|error| MediaError::Windows(error.to_string()))?;
        } else {
            updater
                .MusicProperties()
                .and_then(|properties| properties.SetTitle(&title))
                .map_err(|error| MediaError::Windows(error.to_string()))?;
        }
        updater
            .Update()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn update_system_music_metadata(
        &self,
        title: &str,
        artist: &str,
        album: &str,
    ) -> Result<(), MediaError> {
        use windows::Media::MediaPlaybackType;

        let controls = self
            .player
            .SystemMediaTransportControls()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let updater = controls
            .DisplayUpdater()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        updater
            .SetType(MediaPlaybackType::Music)
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let properties = updater
            .MusicProperties()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        properties
            .SetTitle(&windows::core::HSTRING::from(title))
            .and_then(|()| properties.SetArtist(&windows::core::HSTRING::from(artist)))
            .and_then(|()| properties.SetAlbumTitle(&windows::core::HSTRING::from(album)))
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        updater
            .Update()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn play(&self) -> Result<(), MediaError> {
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.play();
        }
        self.player
            .Play()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn pause(&self) -> Result<(), MediaError> {
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.pause();
        }
        self.player
            .Pause()
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_volume(&self, volume: f64) -> Result<(), MediaError> {
        self.volume.set(volume);
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.set_volume(volume);
        }
        self.player
            .SetVolume(volume)
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_repeat_current(&self, enabled: bool) -> Result<(), MediaError> {
        self.repeat_current.set(enabled);
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.set_repeat(enabled);
        }
        self.player
            .SetIsLoopingEnabled(enabled)
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_playback_rate(&self, rate: f64) -> Result<(), MediaError> {
        self.playback_rate.set(rate);
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.set_playback_rate(rate);
        }
        let session = self
            .player
            .PlaybackSession()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        session
            .SetPlaybackRate(rate)
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn transport_snapshot(&self) -> Result<TransportSnapshot, MediaError> {
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.transport_snapshot();
        }
        let session = self
            .player
            .PlaybackSession()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let position = session
            .Position()
            .map_err(|error| MediaError::Windows(error.to_string()))?
            .Duration
            .max(0);
        let duration = session
            .NaturalDuration()
            .map_err(|error| MediaError::Windows(error.to_string()))?
            .Duration
            .max(0);
        let state = session
            .PlaybackState()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        Ok(TransportSnapshot {
            position_ms: u64::try_from(position / 10_000).unwrap_or_default(),
            duration_ms: u64::try_from(duration / 10_000).unwrap_or_default(),
            is_playing: state == windows::Media::Playback::MediaPlaybackState::Playing,
        })
    }

    fn seek_fraction(&self, fraction: f64) -> Result<(), MediaError> {
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.seek_fraction(fraction);
        }
        let session = self
            .player
            .PlaybackSession()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        let duration = session
            .NaturalDuration()
            .map_err(|error| MediaError::Windows(error.to_string()))?
            .Duration
            .max(0);
        #[allow(clippy::cast_possible_truncation, clippy::cast_precision_loss)]
        let target = (duration as f64 * fraction).round() as i64;
        session
            .SetPosition(windows::Foundation::TimeSpan { Duration: target })
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn stop(&self) -> Result<(), MediaError> {
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            return graph.stop();
        }
        self.pause()?;
        let session = self
            .player
            .PlaybackSession()
            .map_err(|error| MediaError::Windows(error.to_string()))?;
        session
            .SetPosition(windows::Foundation::TimeSpan { Duration: 0 })
            .map_err(|error| MediaError::Windows(error.to_string()))
    }

    fn set_equalizer_profile(&self, enabled: bool, bands: [f64; 10]) -> Result<(), MediaError> {
        self.equalizer_enabled.set(enabled);
        self.equalizer_bands.replace(bands);
        if let Some(graph) = self.audio_graph.borrow().as_ref() {
            graph.set_equalizer(enabled, bands)?;
        }
        Ok(())
    }

    #[cfg(test)]
    fn native_equalizer_band_gains(&self) -> Result<Option<Vec<f64>>, MediaError> {
        self.audio_graph
            .borrow()
            .as_ref()
            .map(audio_graph_player::AudioGraphPlayer::equalizer_band_gains)
            .transpose()
    }
}

#[cfg(not(windows))]
struct PlatformMediaEngine;

#[cfg(not(windows))]
impl PlatformMediaEngine {
    fn audio_output_devices() -> Result<Vec<AudioOutputDevice>, MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn audio_output_device(_id: &str) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_audio_output_device(&self, _device: &()) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_equalizer_profile(&self, _enabled: bool, _bands: [f64; 10]) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn audio_tracks(&self) -> Result<Vec<AudioTrackInfo>, MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn technical_info(&self) -> Result<Option<MediaTechnicalInfo>, MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn select_audio_track(&self, _index: u32) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn new(
        _command_sender: Sender<SystemMediaCommand>,
        _enabled: bool,
        _video_frame_sender: Option<std::sync::mpsc::SyncSender<VideoFrame>>,
    ) -> Result<Self, MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_system_controls_enabled(&self, _enabled: bool) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn open(&self, _path: &Path) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn prepare(&self, _path: &Path) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn update_system_media_metadata(&self, _path: &Path) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn update_system_music_metadata(
        &self,
        _title: &str,
        _artist: &str,
        _album: &str,
    ) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn open_uri(&self, _uri: &str) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn play(&self) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn pause(&self) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_volume(&self, _volume: f64) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_repeat_current(&self, _enabled: bool) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn set_playback_rate(&self, _rate: f64) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn transport_snapshot(&self) -> Result<TransportSnapshot, MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn seek_fraction(&self, _fraction: f64) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
    fn stop(&self) -> Result<(), MediaError> {
        Err(MediaError::UnsupportedPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::{db_multiplier, equalizer_preamp_multiplier, gain_multiplier};

    #[test]
    fn equalizer_master_gain_uses_decibels_and_clamps_safely() {
        assert!((db_multiplier(0.0, -12.0, 12.0) - 1.0).abs() < f64::EPSILON);
        assert!((db_multiplier(6.0, -12.0, 12.0) - 1.995).abs() < 0.001);
        assert!((db_multiplier(-6.0, -12.0, 12.0) - 0.501).abs() < 0.001);
        assert!((db_multiplier(100.0, -12.0, 12.0) - 3.981).abs() < 0.001);
    }

    #[test]
    fn replaygain_multiplier_respects_peak_and_limits() {
        let unity = gain_multiplier(0.0, None);
        assert!((unity - 1.0).abs() < f64::EPSILON);

        let peak_limited = gain_multiplier(6.0, Some(0.8));
        assert!((peak_limited - 1.25).abs() < 0.001);

        assert!((gain_multiplier(-100.0, None) - 0.1).abs() < 0.001);
        assert!((gain_multiplier(100.0, None) - 3.981).abs() < 0.001);
    }

    #[test]
    fn equalizer_curve_reserves_headroom_for_the_strongest_boost() {
        let mut bands = [0.0; 10];
        bands[0] = 6.0;
        bands[7] = 3.0;
        let compensated = equalizer_preamp_multiplier(Some(0.0), &bands);
        assert!((compensated - db_multiplier(-6.0, -24.0, 12.0)).abs() < f64::EPSILON);

        let bypassed = equalizer_preamp_multiplier(None, &bands);
        assert!((bypassed - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn dynamic_range_uses_authoritative_transfer_function_metadata() {
        assert_eq!(
            super::dynamic_range_label(Some(15)),
            "HDR10 / PQ (SMPTE ST 2084)"
        );
        assert_eq!(super::dynamic_range_label(Some(16)), "HDR / HLG");
        assert_eq!(super::dynamic_range_label(None), "Not reported by Windows");
        assert!(super::dynamic_range_label(Some(5)).starts_with("SDR or unspecified"));
    }

    #[cfg(windows)]
    fn write_silence_wav(path: &std::path::Path, duration_ms: u32) -> std::io::Result<()> {
        use std::io::Write;

        const SAMPLE_RATE: u32 = 44_100;
        const CHANNELS: u16 = 2;
        const BITS_PER_SAMPLE: u16 = 16;
        let frame_count = u64::from(SAMPLE_RATE) * u64::from(duration_ms) / 1_000;
        let block_align = CHANNELS * (BITS_PER_SAMPLE / 8);
        let data_size = u32::try_from(frame_count * u64::from(block_align))
            .expect("test WAV fits in a 32-bit RIFF chunk");
        let byte_rate = SAMPLE_RATE * u32::from(block_align);
        let mut file = std::fs::File::create(path)?;
        file.write_all(b"RIFF")?;
        file.write_all(&(36_u32 + data_size).to_le_bytes())?;
        file.write_all(b"WAVEfmt ")?;
        file.write_all(&16_u32.to_le_bytes())?;
        file.write_all(&1_u16.to_le_bytes())?;
        file.write_all(&CHANNELS.to_le_bytes())?;
        file.write_all(&SAMPLE_RATE.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&block_align.to_le_bytes())?;
        file.write_all(&BITS_PER_SAMPLE.to_le_bytes())?;
        file.write_all(b"data")?;
        file.write_all(&data_size.to_le_bytes())?;
        let silence = vec![0_u8; usize::try_from(data_size).expect("test data size fits usize")];
        file.write_all(&silence)
    }

    #[cfg(windows)]
    fn wait_for_duration(engine: &super::MediaEngine) -> Result<u64, super::MediaError> {
        for _ in 0..40 {
            let snapshot = engine.transport_snapshot()?;
            if snapshot.duration_ms > 0 {
                return Ok(snapshot.duration_ms);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        Ok(0)
    }

    #[cfg(windows)]
    #[test]
    fn windows_engine_handles_transport_gapless_and_crossfade()
    -> Result<(), Box<dyn std::error::Error>> {
        let folder =
            std::env::temp_dir().join(format!("zink-media-native-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder)?;
        let first = folder.join("first.wav");
        let second = folder.join("second.wav");
        write_silence_wav(&first, 2_000)?;
        write_silence_wav(&second, 2_000)?;

        let mut engine = super::MediaEngine::new()?;
        let mut bands = [0.0; 10];
        bands[5] = 6.0;
        engine.set_equalizer_profile(Some(0.0), bands)?;
        engine.open(&first)?;
        engine.update_system_music_metadata("First", "Zink Tests", "Native Playback")?;
        assert!(wait_for_duration(&engine)? >= 1_500);
        let native_gains = engine
            .inner
            .native_equalizer_band_gains()?
            .expect("local audio should use the native AudioGraph path");
        assert_eq!(native_gains.len(), 10);
        assert!((native_gains[5] - 6.0).abs() < 0.01);
        engine.pause()?;
        assert_eq!(engine.state(), super::PlaybackState::Paused);
        engine.play()?;
        engine.seek_fraction(0.5)?;
        engine.set_volume(0.35)?;
        assert!((engine.volume() - 0.35).abs() < f64::EPSILON);
        engine.prepare_next(&second)?;
        engine.play_prepared(&second)?;
        assert_eq!(engine.current_path.as_deref(), Some(second.as_path()));
        assert_eq!(engine.state(), super::PlaybackState::Playing);

        engine.prepare_next(&first)?;
        engine.begin_crossfade(&first)?;
        assert!(!engine.set_crossfade_progress(0.5)?);
        assert!(engine.set_crossfade_progress(1.0)?);
        assert_eq!(engine.current_path.as_deref(), Some(first.as_path()));
        engine.stop()?;
        assert_eq!(engine.state(), super::PlaybackState::Paused);

        drop(engine);
        let _ = std::fs::remove_dir_all(folder);
        Ok(())
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "requires a real local video file in ZINK_TEST_VIDEO"]
    fn windows_video_frame_server_decodes_real_file() -> Result<(), Box<dyn std::error::Error>> {
        let path = std::env::var_os("ZINK_TEST_VIDEO")
            .map(std::path::PathBuf::from)
            .ok_or("ZINK_TEST_VIDEO is not set")?;
        if !path.is_file() {
            return Err(format!("test video does not exist: {}", path.display()).into());
        }

        let mut engine = super::MediaEngine::new()?;
        engine.open(&path)?;
        let mut frame = None;
        for _ in 0..200 {
            frame = engine.take_latest_video_frame().or(frame);
            if frame.is_some() && engine.transport_snapshot()?.duration_ms > 0 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let frame = frame.ok_or_else(|| {
            format!(
                "MediaPlayer frame server produced no decoded frame; transport={:?}; technical={:?}",
                engine.transport_snapshot(),
                engine.technical_info()
            )
        })?;
        assert_eq!((frame.width, frame.height), (960, 540));
        assert_eq!(frame.bgra.len(), 960 * 540 * 4);
        assert!(engine.transport_snapshot()?.duration_ms > 0);

        engine.pause()?;
        engine.seek_fraction(0.5)?;
        engine.set_volume(0.4)?;
        engine.set_repeat_current(true)?;
        engine.play()?;
        assert_eq!(engine.state(), super::PlaybackState::Playing);
        engine.stop()?;
        Ok(())
    }
}
