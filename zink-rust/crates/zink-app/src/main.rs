#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::{
    cell::RefCell,
    collections::{BTreeMap, HashSet, VecDeque, hash_map::DefaultHasher},
    fs::{self, File},
    hash::{Hash, Hasher},
    io::{Read, Write},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::Context;
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::{Accessor, ItemKey},
    probe::Probe,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use slint::{Color, Model, ModelRc, SharedString, Timer, TimerMode, VecModel};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;
use zink_core::{AppSettings, CustomRadioStation, PRIMARY_ROUTES, ThemePreference, route_by_id};
use zink_data::{
    DataPaths, MediaItem, finish_playback, import_media_directory, list_media, list_playlists,
    list_recently_played, list_rediscovery_tracks, list_top_tracks, listening_summary,
    load_playlist, load_settings, open_database, prune_missing_media, record_playback,
    remove_media_directory, save_playlist, save_settings,
};
use zink_media::{MediaEngine, PlaybackState, SystemMediaCommand};
use zink_platform_windows::{
    WindowsRuntime, acquire_single_instance, pick_files_to_move, pick_image_file, pick_media_file,
    pick_media_folder, pick_media_folders, pick_music_files, pick_subtitle_file,
};

#[cfg(windows)]
use base64::Engine as _;
#[cfg(windows)]
use futures_util::{SinkExt, StreamExt};
#[cfg(windows)]
use tokio_tungstenite::tungstenite::Message;

#[cfg(windows)]
mod global_replay_hotkey;
#[cfg(windows)]
mod social_hdr_view;
#[cfg(windows)]
mod social_screen_share;
#[cfg(windows)]
mod social_screen_rtc;
#[cfg(windows)]
mod social_video_decoder;
#[cfg(windows)]
mod social_voice;
#[cfg(windows)]
mod ui_translation;
mod utorrent_mover;
#[cfg(windows)]
mod video_translation;
#[cfg(windows)]
mod zink_ad_block;
#[cfg(windows)]
mod zink_connect_embedded;

slint::include_modules!();

struct SplashProcess(std::process::Child);

impl Drop for SplashProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn start_splash_process(software_ui: bool) -> Option<SplashProcess> {
    let executable = std::env::current_exe().ok()?;
    let mut command = std::process::Command::new(executable);
    command.arg("--zink-splash");
    if software_ui {
        command.arg("--software-ui");
    }
    command.spawn().ok().map(SplashProcess)
}

fn run_splash_mode() -> anyhow::Result<()> {
    let splash = SplashWindow::new().context("create Zink splash window")?;
    let weak = splash.as_weak();
    let timeout = Timer::default();
    timeout.start(TimerMode::SingleShot, Duration::from_secs(20), move || {
        if let Some(splash) = weak.upgrade() {
            let _ = splash.hide();
        }
        let _ = slint::quit_event_loop();
    });
    splash.run().context("run Zink splash window")
}

#[derive(Clone, Serialize, Deserialize)]
struct MusicTrackState {
    path: PathBuf,
    title: String,
    artist: String,
    album: String,
    technical_detail: String,
    #[serde(default)]
    replay_gain_db: Option<f64>,
    #[serde(default)]
    replay_peak: Option<f64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SavedMusicSession {
    queue: Vec<MusicTrackState>,
    current_index: Option<usize>,
    position_ms: u64,
    volume: Option<f64>,
    shuffle: bool,
    repeat_current: bool,
    background: bool,
    playback_rate: Option<f64>,
    crossfade_seconds: Option<u32>,
    audio_device_id: Option<String>,
    normalization_enabled: Option<bool>,
    normalization_preamp_db: Option<f64>,
}

#[derive(Clone, Serialize, Deserialize)]
struct ArtistReleaseData {
    id: String,
    title: String,
    release_type: String,
    release_date: String,
    release_ids: Vec<String>,
    artwork_path: Option<PathBuf>,
}

#[derive(Clone)]
struct ScannedDuplicateVideo {
    group: String,
    name: String,
    path: PathBuf,
    size: u64,
    copies: usize,
}

struct FilmInformation {
    title: String,
    date: String,
    overview: String,
    source_url: String,
    poster_path: Option<PathBuf>,
}

#[cfg(windows)]
#[derive(Clone)]
struct OneDriveAccessToken {
    value: String,
    expires_at: Instant,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct OneDriveDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[cfg(windows)]
#[derive(Deserialize)]
struct OneDriveTokenResponse {
    access_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

#[cfg(windows)]
struct OneDriveResolvedVideo {
    url: String,
    name: String,
}

struct OnlineLyricsResult {
    path: PathBuf,
    lyrics: Vec<(u64, String)>,
    status: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct SpotifyCredentials {
    access_token: String,
    #[serde(default)]
    refresh_token: String,
}

struct SpotifyAuthResult {
    credentials: Option<SpotifyCredentials>,
    display_name: String,
    product: String,
    status: String,
    dashboard: Option<SpotifyDashboardData>,
}

struct SpotifySearchResult {
    query: String,
    result: Result<Vec<SpotifyTrackData>, String>,
}

#[derive(Clone, Default)]
struct SpotifyTrackData {
    id: String,
    title: String,
    artist: String,
    album: String,
}

#[derive(Clone, Default)]
struct SpotifyDeviceData {
    id: String,
    name: String,
    detail: String,
    active: bool,
}

#[derive(Clone, Default)]
struct SpotifyDashboardData {
    current: Option<SpotifyTrackData>,
    is_playing: bool,
    device_name: String,
    recent: Vec<SpotifyTrackData>,
    top: Vec<SpotifyTrackData>,
    queue: Vec<SpotifyTrackData>,
    devices: Vec<SpotifyDeviceData>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct LikedRadioSongState {
    id: String,
    title: String,
    artist: String,
    album: String,
    artwork_url: String,
    station_name: String,
    liked_at_utc: String,
    #[serde(default)]
    spotify_track_id: String,
    #[serde(default)]
    spotify_track_url: String,
    #[serde(default)]
    added_to_spotify_liked_songs: bool,
    #[serde(default)]
    spotify_synced_at_utc: Option<String>,
    #[serde(default)]
    youtube_video_url: String,
    #[serde(default)]
    youtube_matched_at_utc: Option<String>,
}

const SOCIAL_BASE_URL: &str = "https://zink-free.francehayle.workers.dev";
static RADIO_METADATA_GENERATION: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocialSession {
    user_id: i64,
    username: String,
    display_name: String,
    token: String,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SocialProfileDetails {
    user_id: i64,
    #[serde(default)]
    avatar_path: String,
    #[serde(default)]
    signed_up: String,
    #[serde(default)]
    links: Vec<SocialProfileLink>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SocialProfileLink {
    service: String,
    url: String,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocialPersonData {
    #[serde(alias = "UserId")]
    user_id: i64,
    #[serde(default, alias = "Username")]
    username: String,
    #[serde(default, alias = "DisplayName")]
    display_name: String,
    #[serde(default, alias = "AvatarUrl")]
    avatar_url: String,
    #[serde(default, alias = "Presence")]
    presence: String,
    #[serde(default, alias = "IsOnline")]
    is_online: bool,
    #[serde(default, alias = "IsFriend")]
    is_friend: bool,
}

#[derive(Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SocialRequestData {
    request_id: i64,
    #[serde(default)]
    from_username: String,
    #[serde(default)]
    from_display_name: String,
    #[serde(default)]
    created_utc: String,
}

#[cfg(windows)]
#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "PascalCase")]
struct SocialSignalEnvelope {
    #[serde(rename = "Type")]
    message_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    call_id: Option<String>,
    #[serde(default)]
    from_user_id: i64,
    #[serde(default)]
    from_username: String,
    #[serde(default)]
    from_display_name: String,
    #[serde(default)]
    target_user_id: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    debug_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    audio_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_frame_base64: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_frame_width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_frame_height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_frame_timestamp: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_width: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_height: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp_utc_ms: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_codec: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_codecs: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_frame_is_key_frame: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sdp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    screen_rtc_id: Option<String>,
}

#[cfg(windows)]
enum SocialCallCommand {
    Probe,
    Invite(i64),
    Accept {
        user_id: i64,
        call_id: String,
    },
    Decline {
        user_id: i64,
        call_id: String,
    },
    End,
    ScreenCapabilities(bool),
    ScreenRequestSdr,
    ScreenRequestH264,
    Audio(Vec<u8>),
    ScreenStarted {
        width: u32,
        height: u32,
        hdr10: bool,
        av1: bool,
        control: Arc<social_screen_share::ShareControl>,
    },
    ScreenFrame {
        data: Vec<u8>,
        width: u32,
        height: u32,
        timestamp_ms: i64,
        key_frame: bool,
        slot: Arc<tokio::sync::OwnedSemaphorePermit>,
    },
    ScreenStopped,
    Shutdown,
}

#[cfg(windows)]
enum SocialCallEvent {
    Status(String),
    Incoming {
        user_id: i64,
        call_id: String,
        display_name: String,
    },
    Connected,
    Audio(Vec<u8>),
    RemoteScreenStarted {
        width: u32,
        height: u32,
        hdr10: bool,
        av1: bool,
        rtc: bool,
    },
    RemoteScreenPacket {
        data: Vec<u8>,
        timestamp_ms: Option<i64>,
        received: Instant,
    },
    RemoteScreenStopped,
    ScreenShareFailed(String),
    ScreenAudioFailed(String),
    ScreenShareMode { hdr10: bool, av1: bool },
    PeerHdrAvailable(bool),
    PeerRequestsSdr,
    HdrReceiveFailed(String),
    Av1ReceiveFailed(String),
    HdrPresented,
    HdrShareRate(f64),
    SentShareRate { fps: f64, mbps: f64, rtc: bool, av1: bool },
    RemoteFrame {
        rgba: Vec<u8>,
        width: u32,
        height: u32,
    },
    Ended(String),
    Failed(String),
}

#[cfg(windows)]
static SOCIAL_CALL_EVENTS: OnceLock<Mutex<VecDeque<SocialCallEvent>>> = OnceLock::new();

#[cfg(windows)]
thread_local! {
    static SOCIAL_CALL_EVENT_HANDLER: RefCell<Option<Box<dyn FnMut()>>> = RefCell::new(None);
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SavedSocialConversation {
    user_id: i64,
    username: String,
    display_name: String,
    updated_unix: u64,
    messages: Vec<SavedSocialMessage>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct SavedSocialMessage {
    text: String,
    sent_unix: u64,
    is_from_me: bool,
}

#[derive(Default)]
struct SocialMessageState {
    conversations: Vec<SavedSocialConversation>,
    active_user_id: Option<i64>,
}

#[derive(Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
struct ReplayPreferences {
    duration_seconds: i32,
    system_audio: bool,
    microphone: bool,
    separate_tracks: bool,
    #[serde(default = "default_true")]
    separate_desktop_track: bool,
    #[serde(default = "default_true")]
    separate_microphone_track: bool,
    audio_bitrate: i32,
}

#[derive(Clone, Default, Serialize, Deserialize)]
struct StreamingDestination {
    server: String,
    stream_key: String,
}

#[derive(Clone, Serialize, Deserialize)]
struct StreamingPreferences {
    #[serde(default)]
    destinations: BTreeMap<String, StreamingDestination>,
    quality: String,
    system_audio: bool,
    microphone: bool,
    #[serde(default = "default_stream_audio_gain")]
    desktop_gain: f32,
    #[serde(default = "default_stream_audio_gain")]
    microphone_gain: f32,
    low_latency: bool,
    #[serde(default)]
    twitch_channel: String,
    #[serde(default)]
    twitch_client_id: String,
    #[serde(default)]
    twitch_access_token: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[allow(clippy::struct_excessive_bools)]
struct FpsPreferences {
    target_fps: i32,
    sample_window_seconds: i32,
    record_csv: bool,
    show_widget: bool,
    widget_text_only: bool,
    widget_opacity: i32,
    include_cursor: bool,
    hide_capture_border: bool,
}

impl Default for FpsPreferences {
    fn default() -> Self {
        Self {
            target_fps: 60,
            sample_window_seconds: 3,
            record_csv: true,
            show_widget: true,
            widget_text_only: false,
            widget_opacity: 92,
            include_cursor: false,
            hide_capture_border: true,
        }
    }
}

impl Default for StreamingPreferences {
    fn default() -> Self {
        Self {
            destinations: BTreeMap::new(),
            quality: "1080p 60 FPS".to_owned(),
            system_audio: true,
            microphone: false,
            desktop_gain: 1.0,
            microphone_gain: 1.0,
            low_latency: true,
            twitch_channel: String::new(),
            twitch_client_id: "9orim957s3wno703jjnlz3inq48m8f".to_owned(),
            twitch_access_token: String::new(),
        }
    }
}

const fn default_true() -> bool {
    true
}

const fn default_stream_audio_gain() -> f32 {
    1.0
}

impl Default for ReplayPreferences {
    fn default() -> Self {
        Self {
            duration_seconds: 45,
            system_audio: true,
            microphone: false,
            separate_tracks: false,
            separate_desktop_track: true,
            separate_microphone_track: true,
            audio_bitrate: 192,
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SocialDeveloperSettings {
    offline_mode: bool,
    mock_calls: bool,
    display_name: String,
    username: String,
    email: String,
    call_server: String,
}

impl Default for SocialDeveloperSettings {
    fn default() -> Self {
        Self {
            offline_mode: false,
            mock_calls: false,
            display_name: "Local tester".to_owned(),
            username: "zink.local".to_owned(),
            email: "tester@zink.local".to_owned(),
            call_server: "http://localhost:5000".to_owned(),
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
struct SavedNotification {
    id: String,
    title: String,
    message: String,
    timestamp_unix: u64,
    #[serde(default)]
    action_label: String,
    #[serde(default)]
    action_uri: String,
}

enum SocialResult {
    Auth(Result<SocialSession, String>),
    Friends(Result<(Vec<SocialPersonData>, Vec<SocialRequestData>), String>),
    Search(Result<Vec<SocialPersonData>, String>),
    Action(Result<String, String>),
    Profile(i64, Result<String, String>),
}

#[cfg(windows)]
enum StreamPipelineResult {
    Started(Result<String, String>),
    Stopped(Result<(), String>),
}

#[cfg(windows)]
struct TwitchTelemetryResult {
    viewers: i32,
    is_live: bool,
    status: String,
}

#[cfg(windows)]
struct StreamPreviewResult {
    rgba: Vec<u8>,
    width: u32,
    height: u32,
    source_pid: i32,
    error: Option<String>,
}

#[cfg(windows)]
enum RecorderPipelineResult {
    Started(Result<(PathBuf, String), String>),
    Stopped(Result<PathBuf, String>),
}

#[derive(Clone)]
struct LibraryArtworkRequest {
    path: PathBuf,
    title: String,
    artist: String,
    album: String,
}

static ONLINE_LYRICS_RESULTS: OnceLock<Mutex<Vec<OnlineLyricsResult>>> = OnceLock::new();
static LIBRARY_ARTWORK_SCANS: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
static SPOTIFY_AUTH_RESULTS: OnceLock<Mutex<Vec<SpotifyAuthResult>>> = OnceLock::new();
static SPOTIFY_SEARCH_RESULTS: OnceLock<Mutex<Vec<SpotifySearchResult>>> = OnceLock::new();
static SOCIAL_RESULTS: OnceLock<Mutex<Vec<SocialResult>>> = OnceLock::new();
#[cfg(windows)]
static STREAM_PIPELINE_RESULTS: OnceLock<Mutex<Vec<StreamPipelineResult>>> = OnceLock::new();
#[cfg(windows)]
static TWITCH_TELEMETRY_RESULTS: OnceLock<Mutex<Vec<TwitchTelemetryResult>>> = OnceLock::new();
#[cfg(windows)]
static STREAM_PREVIEW_RESULTS: OnceLock<Mutex<Vec<StreamPreviewResult>>> = OnceLock::new();
#[cfg(windows)]
static STREAM_PREVIEW_BUSY: AtomicBool = AtomicBool::new(false);
#[cfg(windows)]
static RECORDER_PIPELINE_RESULTS: OnceLock<Mutex<Vec<RecorderPipelineResult>>> = OnceLock::new();
#[cfg(windows)]
static REPLAY_PIPELINE_RESULTS: OnceLock<Mutex<Vec<ReplayPipelineResult>>> = OnceLock::new();

#[cfg(windows)]
enum ReplayPipelineResult {
    Started(Result<win_native_media::ReplayBuffer, String>),
    Saved(Result<(PathBuf, Duration), String>),
    Stopped(Result<(), String>),
}

#[derive(Default)]
#[allow(clippy::struct_excessive_bools)]
struct MusicPlayerState {
    queue: Vec<MusicTrackState>,
    current_index: Option<usize>,
    shuffle: bool,
    repeat_current: bool,
    background: bool,
    playback_rate: f64,
    crossfade_seconds: u32,
    crossfade_started: Option<Instant>,
    crossfade_target_index: Option<usize>,
    audio_device_id: String,
    normalization_enabled: bool,
    normalization_preamp_db: f64,
    auto_advanced: bool,
    lyrics: Vec<(u64, String)>,
    liked_paths: HashSet<PathBuf>,
    liked_paths_file: PathBuf,
    session_file: PathBuf,
    pending_restore_ms: u64,
    prepared_index: Option<usize>,
}

#[derive(Default)]
struct FpsMonitorState {
    target_pid: u32,
    target_name: String,
    started: Option<Instant>,
    samples: Vec<f64>,
    frame_count: u64,
    last_sample_key: u64,
    recording: Option<File>,
    recording_path: Option<PathBuf>,
}

#[derive(Clone, Copy, Default)]
struct RtssFpsSample {
    fps: f64,
    frame_time_ms: f64,
    sample_key: u64,
}

#[cfg(windows)]
fn streaming_video_config(quality: &str) -> win_native_media::VideoConfig {
    match quality {
        "720p 30 FPS" => win_native_media::VideoConfig {
            width: 1280,
            height: 720,
            fps: 30,
            bitrate: 4_000_000,
            keyframe_interval: 60,
        },
        "1440p 60 FPS" => win_native_media::VideoConfig {
            width: 2560,
            height: 1440,
            fps: 60,
            bitrate: 14_000_000,
            keyframe_interval: 120,
        },
        _ => win_native_media::VideoConfig {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 8_000_000,
            keyframe_interval: 120,
        },
    }
}

fn default_streaming_server(route: &str) -> &'static str {
    match route {
        "twitch-streaming" => "rtmps://live.twitch.tv/app/",
        "youtube-streaming" => "rtmps://a.rtmps.youtube.com/live2",
        "kick-streaming" => "rtmps://fa723fc1b171.global-contribute.live-video.net:443/app",
        "instagram-streaming" => "rtmps://live-upload.instagram.com:443/rtmp/",
        "facebook-streaming" => "rtmps://live-api-s.facebook.com:443/rtmp/",
        _ => "",
    }
}

fn apply_streaming_destination(ui: &AppWindow, route: &str, preferences: &StreamingPreferences) {
    if let Some(destination) = preferences.destinations.get(route) {
        ui.set_stream_server(SharedString::from(destination.server.clone()));
        ui.set_stream_key(SharedString::from(destination.stream_key.clone()));
    } else {
        ui.set_stream_server(SharedString::from(default_streaming_server(route)));
        ui.set_stream_key(SharedString::default());
    }
}

fn save_streaming_preferences(
    path: &Path,
    preferences: &StreamingPreferences,
) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(preferences).context("serialize streaming settings")?;
    fs::write(path, bytes).context("save streaming settings")
}

const SETTINGS_LANGUAGES: &[(&str, &str)] = &[
    ("System default", "system"),
    ("English", "en-GB"),
    ("French", "fr-FR"),
    ("German", "de-DE"),
    ("Spanish", "es-ES"),
    ("Italian", "it-IT"),
    ("Portuguese", "pt-PT"),
    ("Dutch", "nl-NL"),
    ("Polish", "pl-PL"),
    ("Turkish", "tr-TR"),
    ("Arabic", "ar-SA"),
    ("Hindi", "hi-IN"),
    ("Simplified Chinese", "zh-Hans"),
    ("Japanese", "ja-JP"),
    ("Korean", "ko-KR"),
];

fn language_tag(display_name: &str) -> &str {
    SETTINGS_LANGUAGES
        .iter()
        .find(|(name, _)| *name == display_name)
        .map_or(display_name, |(_, tag)| *tag)
}

fn language_display_name(tag: &str) -> &str {
    SETTINGS_LANGUAGES
        .iter()
        .find(|(_, candidate)| *candidate == tag)
        .or_else(|| {
            let neutral = tag.split(['-', '_']).next()?;
            SETTINGS_LANGUAGES
                .iter()
                .find(|(_, candidate)| candidate.split(['-', '_']).next() == Some(neutral))
        })
        .map_or(tag, |(name, _)| *name)
}

#[cfg(windows)]
fn system_language_tag() -> String {
    windows::System::UserProfile::GlobalizationPreferences::Languages()
        .ok()
        .and_then(|languages| languages.GetAt(0).ok())
        .map_or_else(|| "en-GB".to_owned(), |language| language.to_string())
}

#[cfg(not(windows))]
fn system_language_tag() -> String {
    "en-GB".to_owned()
}

fn effective_language_tag(preference: &str) -> String {
    if preference == "system" {
        system_language_tag()
    } else {
        preference.to_owned()
    }
}

#[allow(clippy::too_many_lines)]
fn core_translation<'a>(language: &str, source: &'a str) -> &'a str {
    let neutral = language
        .split(['-', '_'])
        .next()
        .unwrap_or(language)
        .to_ascii_lowercase();
    let index = [
        "Settings",
        "Language",
        "Use Windows language",
        "Background",
        "On",
        "Off",
        "Check for updates",
        "Reset defaults",
        "Home",
        "Music",
        "Video",
        "Gaming",
        "Social",
        "Notifications",
        "Privacy",
        "About",
        "Personalise and control Zink",
        "Appearance",
        "Integrations",
        "Background and startup",
        "Run Zink at sign-in",
        "Reopen last page",
        "Low memory and CPU mode",
        "Replay and hotkeys",
        "Game clip recording hotkey",
        "Replay settings",
        "Diagnostics",
        "Maintenance",
        "Diagnostic log file",
        "Open log folder",
        "Clear log",
        "Run health check",
        "Open health report",
        "Upload support bundle",
        "Open Store updates",
    ]
    .iter()
    .position(|label| *label == source);
    let Some(index) = index else { return source };
    let translations: Option<[&'static str; 35]> = match neutral.as_str() {
        "es" => Some([
            "Configuración",
            "Idioma",
            "Usar el idioma de Windows",
            "Fondo",
            "Activado",
            "Desactivado",
            "Buscar actualizaciones",
            "Restablecer valores predeterminados",
            "Inicio",
            "Música",
            "Vídeo",
            "Juegos",
            "Social",
            "Notificaciones",
            "Privacidad",
            "Acerca de",
            "Personaliza y controla Zink",
            "Apariencia",
            "Integraciones",
            "Segundo plano e inicio",
            "Ejecutar Zink al iniciar sesión",
            "Reabrir la última página",
            "Modo de bajo uso de memoria y CPU",
            "Repetición y atajos",
            "Atajo para grabar clips de juego",
            "Configuración de repetición",
            "Diagnóstico",
            "Mantenimiento",
            "Archivo de registro de diagnóstico",
            "Abrir carpeta de registros",
            "Borrar registro",
            "Ejecutar comprobación de estado",
            "Abrir informe de estado",
            "Subir paquete de soporte",
            "Abrir actualizaciones de Store",
        ]),
        "fr" => Some([
            "Paramètres",
            "Langue",
            "Utiliser la langue de Windows",
            "Arrière-plan",
            "Activé",
            "Désactivé",
            "Rechercher des mises à jour",
            "Rétablir les paramètres par défaut",
            "Accueil",
            "Musique",
            "Vidéo",
            "Jeux",
            "Réseaux sociaux",
            "Notifications",
            "Confidentialité",
            "À propos",
            "Personnaliser et contrôler Zink",
            "Apparence",
            "Intégrations",
            "Arrière-plan et démarrage",
            "Lancer Zink à la connexion",
            "Rouvrir la dernière page",
            "Mode mémoire et processeur réduits",
            "Replay et raccourcis",
            "Raccourci d'enregistrement de jeu",
            "Paramètres du replay",
            "Diagnostics",
            "Maintenance",
            "Fichier journal de diagnostic",
            "Ouvrir le dossier des journaux",
            "Effacer le journal",
            "Exécuter le contrôle d'intégrité",
            "Ouvrir le rapport d'intégrité",
            "Téléverser le paquet d'assistance",
            "Ouvrir les mises à jour du Store",
        ]),
        "de" => Some([
            "Einstellungen",
            "Sprache",
            "Windows-Sprache verwenden",
            "Hintergrund",
            "Ein",
            "Aus",
            "Nach Updates suchen",
            "Standardeinstellungen wiederherstellen",
            "Startseite",
            "Musik",
            "Video",
            "Spiele",
            "Soziales",
            "Benachrichtigungen",
            "Datenschutz",
            "Info",
            "Zink personalisieren und steuern",
            "Darstellung",
            "Integrationen",
            "Hintergrund und Start",
            "Zink bei der Anmeldung starten",
            "Letzte Seite erneut öffnen",
            "Modus für wenig Arbeitsspeicher und CPU",
            "Replay und Tastenkürzel",
            "Tastenkürzel für Spielclips",
            "Replay-Einstellungen",
            "Diagnose",
            "Wartung",
            "Diagnoseprotokolldatei",
            "Protokollordner öffnen",
            "Protokoll löschen",
            "Integritätsprüfung ausführen",
            "Integritätsbericht öffnen",
            "Supportpaket hochladen",
            "Store-Updates öffnen",
        ]),
        "it" => Some([
            "Impostazioni",
            "Lingua",
            "Usa la lingua di Windows",
            "Sfondo",
            "Attivo",
            "Disattivo",
            "Controlla aggiornamenti",
            "Ripristina impostazioni predefinite",
            "Home",
            "Musica",
            "Video",
            "Giochi",
            "Social",
            "Notifiche",
            "Privacy",
            "Informazioni",
            "Personalizza e controlla Zink",
            "Aspetto",
            "Integrazioni",
            "Sfondo e avvio",
            "Avvia Zink all'accesso",
            "Riapri l'ultima pagina",
            "Modalità memoria e CPU ridotte",
            "Replay e scorciatoie",
            "Scorciatoia registrazione clip di gioco",
            "Impostazioni replay",
            "Diagnostica",
            "Manutenzione",
            "File di registro diagnostico",
            "Apri cartella dei registri",
            "Cancella registro",
            "Esegui controllo integrità",
            "Apri rapporto integrità",
            "Carica pacchetto di supporto",
            "Apri aggiornamenti dello Store",
        ]),
        "pt" => Some([
            "Definições",
            "Idioma",
            "Usar o idioma do Windows",
            "Segundo plano",
            "Ligado",
            "Desligado",
            "Procurar atualizações",
            "Repor predefinições",
            "Início",
            "Música",
            "Vídeo",
            "Jogos",
            "Social",
            "Notificações",
            "Privacidade",
            "Acerca de",
            "Personalizar e controlar o Zink",
            "Aspeto",
            "Integrações",
            "Segundo plano e arranque",
            "Executar o Zink ao iniciar sessão",
            "Reabrir a última página",
            "Modo de pouca memória e CPU",
            "Repetição e atalhos",
            "Atalho para gravar clipes de jogos",
            "Definições de repetição",
            "Diagnóstico",
            "Manutenção",
            "Ficheiro de registo de diagnóstico",
            "Abrir pasta de registos",
            "Limpar registo",
            "Executar verificação de estado",
            "Abrir relatório de estado",
            "Carregar pacote de suporte",
            "Abrir atualizações da Store",
        ]),
        "nl" => Some([
            "Instellingen",
            "Taal",
            "Windows-taal gebruiken",
            "Achtergrond",
            "Aan",
            "Uit",
            "Controleren op updates",
            "Standaardinstellingen herstellen",
            "Start",
            "Muziek",
            "Video",
            "Games",
            "Sociaal",
            "Meldingen",
            "Privacy",
            "Over",
            "Zink personaliseren en beheren",
            "Uiterlijk",
            "Integraties",
            "Achtergrond en opstarten",
            "Zink uitvoeren bij aanmelden",
            "Laatste pagina opnieuw openen",
            "Modus voor weinig geheugen en CPU",
            "Herhaling en sneltoetsen",
            "Sneltoets voor gameclipopname",
            "Herhalingsinstellingen",
            "Diagnostiek",
            "Onderhoud",
            "Diagnostisch logbestand",
            "Logmap openen",
            "Logboek wissen",
            "Statuscontrole uitvoeren",
            "Statusrapport openen",
            "Ondersteuningspakket uploaden",
            "Store-updates openen",
        ]),
        "pl" => Some([
            "Ustawienia",
            "Język",
            "Użyj języka systemu Windows",
            "Tło",
            "Wł.",
            "Wył.",
            "Sprawdź aktualizacje",
            "Przywróć ustawienia domyślne",
            "Strona główna",
            "Muzyka",
            "Wideo",
            "Gry",
            "Społeczność",
            "Powiadomienia",
            "Prywatność",
            "Informacje",
            "Personalizuj i kontroluj Zink",
            "Wygląd",
            "Integracje",
            "Tło i uruchamianie",
            "Uruchamiaj Zink przy logowaniu",
            "Otwórz ponownie ostatnią stronę",
            "Tryb niskiego użycia pamięci i CPU",
            "Powtórka i skróty",
            "Skrót nagrywania klipów z gry",
            "Ustawienia powtórki",
            "Diagnostyka",
            "Konserwacja",
            "Plik dziennika diagnostycznego",
            "Otwórz folder dzienników",
            "Wyczyść dziennik",
            "Uruchom kontrolę kondycji",
            "Otwórz raport kondycji",
            "Prześlij pakiet pomocy technicznej",
            "Otwórz aktualizacje Store",
        ]),
        "tr" => Some([
            "Ayarlar",
            "Dil",
            "Windows dilini kullan",
            "Arka plan",
            "Açık",
            "Kapalı",
            "Güncellemeleri denetle",
            "Varsayılanları geri yükle",
            "Ana Sayfa",
            "Müzik",
            "Video",
            "Oyunlar",
            "Sosyal",
            "Bildirimler",
            "Gizlilik",
            "Hakkında",
            "Zink'i kişiselleştir ve denetle",
            "Görünüm",
            "Entegrasyonlar",
            "Arka plan ve başlangıç",
            "Oturum açarken Zink'i çalıştır",
            "Son sayfayı yeniden aç",
            "Düşük bellek ve CPU modu",
            "Tekrar ve kısayollar",
            "Oyun klibi kayıt kısayolu",
            "Tekrar ayarları",
            "Tanılama",
            "Bakım",
            "Tanılama günlük dosyası",
            "Günlük klasörünü aç",
            "Günlüğü temizle",
            "Sistem durumu denetimini çalıştır",
            "Sistem durumu raporunu aç",
            "Destek paketini yükle",
            "Store güncellemelerini aç",
        ]),
        "ar" => Some([
            "الإعدادات",
            "اللغة",
            "استخدام لغة Windows",
            "الخلفية",
            "تشغيل",
            "إيقاف",
            "التحقق من وجود تحديثات",
            "استعادة الإعدادات الافتراضية",
            "الرئيسية",
            "الموسيقى",
            "الفيديو",
            "الألعاب",
            "اجتماعي",
            "الإشعارات",
            "الخصوصية",
            "حول",
            "تخصيص Zink والتحكم فيه",
            "المظهر",
            "عمليات التكامل",
            "الخلفية وبدء التشغيل",
            "تشغيل Zink عند تسجيل الدخول",
            "إعادة فتح الصفحة الأخيرة",
            "وضع الذاكرة والمعالج المنخفض",
            "الإعادة والاختصارات",
            "اختصار تسجيل مقاطع الألعاب",
            "إعدادات الإعادة",
            "التشخيص",
            "الصيانة",
            "ملف سجل التشخيص",
            "فتح مجلد السجلات",
            "مسح السجل",
            "تشغيل فحص السلامة",
            "فتح تقرير السلامة",
            "تحميل حزمة الدعم",
            "فتح تحديثات Store",
        ]),
        "hi" => Some([
            "सेटिंग्स",
            "भाषा",
            "Windows की भाषा इस्तेमाल करें",
            "पृष्ठभूमि",
            "चालू",
            "बंद",
            "अपडेट जाँचें",
            "डिफ़ॉल्ट पुनर्स्थापित करें",
            "होम",
            "संगीत",
            "वीडियो",
            "गेम",
            "सोशल",
            "सूचनाएँ",
            "गोपनीयता",
            "परिचय",
            "Zink को अनुकूलित और नियंत्रित करें",
            "दिखावट",
            "एकीकरण",
            "पृष्ठभूमि और स्टार्टअप",
            "साइन-इन पर Zink चलाएँ",
            "पिछला पृष्ठ फिर खोलें",
            "कम मेमोरी और CPU मोड",
            "रीप्ले और शॉर्टकट",
            "गेम क्लिप रिकॉर्डिंग शॉर्टकट",
            "रीप्ले सेटिंग्स",
            "निदान",
            "रखरखाव",
            "निदान लॉग फ़ाइल",
            "लॉग फ़ोल्डर खोलें",
            "लॉग साफ़ करें",
            "स्वास्थ्य जाँच चलाएँ",
            "स्वास्थ्य रिपोर्ट खोलें",
            "सहायता बंडल अपलोड करें",
            "Store अपडेट खोलें",
        ]),
        "zh" => Some([
            "设置",
            "语言",
            "使用 Windows 语言",
            "后台",
            "开",
            "关",
            "检查更新",
            "恢复默认设置",
            "主页",
            "音乐",
            "视频",
            "游戏",
            "社交",
            "通知",
            "隐私",
            "关于",
            "个性化和控制 Zink",
            "外观",
            "集成",
            "后台和启动",
            "登录时运行 Zink",
            "重新打开上次页面",
            "低内存和 CPU 模式",
            "回放和快捷键",
            "游戏片段录制快捷键",
            "回放设置",
            "诊断",
            "维护",
            "诊断日志文件",
            "打开日志文件夹",
            "清除日志",
            "运行运行状况检查",
            "打开运行状况报告",
            "上传支持包",
            "打开 Store 更新",
        ]),
        "ja" => Some([
            "設定",
            "言語",
            "Windows の言語を使用",
            "バックグラウンド",
            "オン",
            "オフ",
            "更新プログラムを確認",
            "既定値に戻す",
            "ホーム",
            "音楽",
            "ビデオ",
            "ゲーム",
            "ソーシャル",
            "通知",
            "プライバシー",
            "バージョン情報",
            "Zink のカスタマイズと制御",
            "外観",
            "統合",
            "バックグラウンドと起動",
            "サインイン時に Zink を実行",
            "最後のページを再度開く",
            "低メモリ・低 CPU モード",
            "リプレイとショートカット",
            "ゲームクリップ録画ショートカット",
            "リプレイ設定",
            "診断",
            "メンテナンス",
            "診断ログファイル",
            "ログフォルダーを開く",
            "ログを消去",
            "正常性チェックを実行",
            "正常性レポートを開く",
            "サポートバンドルをアップロード",
            "Store の更新を開く",
        ]),
        "ko" => Some([
            "설정",
            "언어",
            "Windows 언어 사용",
            "백그라운드",
            "켜기",
            "끄기",
            "업데이트 확인",
            "기본값 복원",
            "홈",
            "음악",
            "비디오",
            "게임",
            "소셜",
            "알림",
            "개인 정보",
            "정보",
            "Zink 개인 설정 및 제어",
            "모양",
            "통합",
            "백그라운드 및 시작",
            "로그인할 때 Zink 실행",
            "마지막 페이지 다시 열기",
            "낮은 메모리 및 CPU 모드",
            "리플레이 및 바로 가기",
            "게임 클립 녹화 바로 가기",
            "리플레이 설정",
            "진단",
            "유지 관리",
            "진단 로그 파일",
            "로그 폴더 열기",
            "로그 지우기",
            "상태 검사 실행",
            "상태 보고서 열기",
            "지원 번들 업로드",
            "Store 업데이트 열기",
        ]),
        _ => None,
    };
    translations.map_or(source, |values| values[index])
}

#[cfg(windows)]
fn refresh_twitch_telemetry(
    channel: &str,
    client_id: &str,
    access_token: &str,
) -> anyhow::Result<TwitchTelemetryResult> {
    let mut endpoint = url::Url::parse("https://api.twitch.tv/helix/streams")?;
    endpoint
        .query_pairs_mut()
        .append_pair("user_login", channel);
    let response = ureq::get(endpoint.as_str())
        .header("Client-Id", client_id)
        .header("Authorization", format!("Bearer {access_token}"))
        .call()
        .context("request Twitch channel telemetry")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Twitch channel telemetry")?;
    parse_twitch_telemetry(&body, channel)
}

#[cfg(windows)]
fn parse_twitch_telemetry(body: &str, channel: &str) -> anyhow::Result<TwitchTelemetryResult> {
    let json: serde_json::Value =
        serde_json::from_str(body).context("parse Twitch channel telemetry")?;
    let Some(stream) = json
        .get("data")
        .and_then(serde_json::Value::as_array)
        .and_then(|items| items.first())
    else {
        return Ok(TwitchTelemetryResult {
            viewers: 0,
            is_live: false,
            status: format!("@{channel} is offline on Twitch."),
        });
    };
    let viewers = stream
        .get("viewer_count")
        .and_then(serde_json::Value::as_i64)
        .and_then(|value| i32::try_from(value).ok())
        .unwrap_or_default();
    let title = stream
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Live on Twitch");
    Ok(TwitchTelemetryResult {
        viewers,
        is_live: true,
        status: format!("LIVE • {viewers} viewers • {title}"),
    })
}

fn format_build_timestamp(value: &str) -> String {
    let Ok(timestamp) = value.parse::<u64>() else {
        return "Unknown build time".to_owned();
    };
    let days = i64::try_from(timestamp / 86_400).unwrap_or_default();
    let seconds = timestamp % 86_400;
    let (year, month, day) = civil_date_from_unix_days(days);
    let hour = seconds / 3_600;
    let minute = (seconds % 3_600) / 60;
    let second = seconds % 60;
    format!("{day:02} {month} {year}, {hour:02}:{minute:02}:{second:02} UTC")
}

fn civil_date_from_unix_days(days: i64) -> (i64, &'static str, i64) {
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month_number = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month_number <= 2);
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ]
    .get(usize::try_from(month_number - 1).unwrap_or_default())
    .copied()
    .unwrap_or("Unknown");
    (year, month, day)
}

fn current_installation_summary() -> String {
    let architecture = std::env::consts::ARCH;
    let packaged = std::env::current_exe().is_ok_and(|path| {
        path.to_string_lossy()
            .to_ascii_lowercase()
            .contains("\\windowsapps\\")
    });
    if packaged {
        format!("Microsoft Store / MSIX package • {architecture}")
    } else {
        format!("Native unpackaged desktop build • {architecture}")
    }
}

fn version_history_items() -> Vec<VersionReleaseItem> {
    [
        (
            "5.0.0-alpha.1",
            "Native Rust and Slint migration",
            "In development",
        ),
        (
            "4.0.9.0",
            "WinUI tabs, themes and Twitch fixes",
            "23 Oct 2026",
        ),
        ("4.0.8.0", "Browser fullscreen title-bar fix", "18 Oct 2026"),
        ("4.0.6.0", "OneDrive video streaming", "18 Oct 2026"),
        (
            "4.0.0.0",
            "Fourth-generation feature release",
            "28 Jun 2026",
        ),
        (
            "3.0.33.0",
            "Visualizer and Zink Connect updates",
            "24 May 2026",
        ),
        ("3.0.0.0", "Third-generation Zink", "03 May 2026"),
        (
            "2.4.1.0",
            "Dashboard customisation and search",
            "11 Feb 2026",
        ),
        ("2.3.5.0", "Home dashboard and power tools", "16 Jan 2026"),
        (
            "2.2.8.0",
            "ARM, subtitles and video controls",
            "12 Jan 2026",
        ),
        ("2.1.6.0", "Windows 10 launch fix", "25 Nov 2025"),
        ("2.1.5.0", "Windows 10 and radio expansion", "25 Nov 2025"),
        ("2.0.0.0", "Second-generation Zink", "05 Sep 2025"),
        ("1.0.0.0", "The original release", "24 Jul 2025"),
    ]
    .into_iter()
    .map(|(version, summary, date)| VersionReleaseItem {
        version: SharedString::from(version),
        date: SharedString::from(date),
        summary: SharedString::from(summary),
    })
    .collect()
}

fn version_release_details(version: &str) -> (&'static str, &'static str) {
    match version {
        "4.0.9.0" => (
            "Released 23 October 2026",
            "• Fixed radio player notifications.\n• Added the lighter non-HDR theme and saved theme choice.\n• Fixed Twitch fullscreen and added smoother enter/exit animation.\n• Added independent page tabs with retained state, Fluent glass styling and hover thumbnails.\n• Added independent Twitch tabs and restored muted playback correctly after tab and focus changes.",
        ),
        "4.0.8.0" => (
            "Released 18 October 2026",
            "• Fixed the Zink Connect title bar remaining visible during YouTube fullscreen playback.",
        ),
        "4.0.6.0" => (
            "Released 18 October 2026",
            "• Added OneDrive video streaming.",
        ),
        "4.0.0.0" => (
            "Released 28 June 2026",
            "• Major playback, library, radio, browser, packaging and performance fixes.\n• Added live Music Library search, lyrics, repeat playback and improved glass navigation.\n• Added HDR background replay, Explorer media activation and live video scrubbing.\n• Added film artwork, duplicate-video search, browser profile persistence and recording compatibility detection.\n• Added automatic Windows language detection and manual language selection.\n• Improved Discord presence, subtitle selection, audio-track fallback and large-library performance.",
        ),
        "3.0.33.0" => (
            "Released 24 May 2026",
            "• Rebuilt the Visualizer around real system-output audio.\n• Added Bars, Wave, Circle, Mirror bars, Dots, Tunnel and Pulse designs.\n• Added colour themes, sensitivity, smoothing, saved settings and reset controls.\n• Kept Zink Connect focused on its browser launch experience.",
        ),
        "3.0.0.0" => (
            "Released 03 May 2026",
            "• Rebranded the complete product as Zink and rebuilt the dashboard.\n• Added background controls, Zink Connect, native calls, screen sharing and diagnostics.\n• Added refreshed recording, Twitch streaming, Spotify Beta and RTSS FPS tools.\n• Expanded themes, radio, video fullscreen, packaging and Store readiness.",
        ),
        "2.4.1.0" => (
            "Released 11 February 2026",
            "• Added Twitch, customisation and dashboard customisation.\n• Added app theme controls, review flow, Search, Photo Viewer and improved library dialogs.",
        ),
        "2.3.5.0" => (
            "Released 16 January 2026",
            "• Added the Home Dashboard, power tools, insights and recent activity.\n• Added open-file, resume and open-folder cards.\n• Fixed sidebar selection and added artist pictures to dashboard playback.",
        ),
        "2.2.8.0" => (
            "Released 12 January 2026",
            "• Added ARM support, subtitles, video seeking and elapsed/remaining time.\n• Refreshed video controls and reorganised Video Player and Video Library navigation.",
        ),
        "2.1.6.0" => (
            "Released 25 November 2025",
            "• Fixed Zink failing to launch on Windows 10 x86 and x64 systems.",
        ),
        "2.1.5.0" => (
            "Released 25 November 2025",
            "• Added Windows 10 support.\n• Improved runtime stability.\n• Added radio song likes and the liked-radio-songs page.",
        ),
        "2.0.0.0" => (
            "Released 05 September 2025",
            "• Introduced the simplified sidebar and Video Library with thumbnails.\n• Expanded music formats, radio stations, Radio Widget, TV, film, gaming and social services.\n• Fixed shutdown, navigation audio, radio playback, TikTok sound and YouTube fullscreen.",
        ),
        "1.0.0.0" => (
            "Released 24 July 2025",
            "• Initial Zink release.\n• Core music, radio and video foundations.\n• First sidebar, navigation, player controls and page structure.",
        ),
        _ => (
            "In development",
            "• Replacing the complete .NET and WinUI application with native Rust and Slint.\n• Rebuilding media, radio, browser, recording, streaming, social, settings and Windows integrations.\n• The shipped application will not require the .NET runtime.",
        ),
    }
}

fn normalize_accent_color(accent: &str) -> Option<String> {
    let preset = match accent {
        "Zink green" => Some("#38FF66"),
        "Cyan" | "Ocean" => Some("#14D8E8"),
        "Emerald" => Some("#1EDC8A"),
        "Lime" => Some("#9EFF4F"),
        "Forest" => Some("#0E7B5A"),
        "Zink teal" => Some("#06404A"),
        "Ocean blue" => Some("#123B64"),
        "Graphite" => Some("#252A35"),
        "Violet" => Some("#9A77FF"),
        "Sunset" => Some("#FF7B68"),
        "Windows" => Some("#60A5FA"),
        _ => None,
    };
    if let Some(preset) = preset {
        return Some(preset.to_owned());
    }
    let value = accent.trim();
    let hex = value.strip_prefix('#')?;
    (hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()))
        .then(|| format!("#{}", hex.to_ascii_uppercase()))
}

fn slint_color_from_hex(value: &str) -> Color {
    let hex = value.trim().trim_start_matches('#');
    if hex.len() == 6
        && let (Ok(red), Ok(green), Ok(blue)) = (
            u8::from_str_radix(&hex[0..2], 16),
            u8::from_str_radix(&hex[2..4], 16),
            u8::from_str_radix(&hex[4..6], 16),
        )
    {
        return Color::from_rgb_u8(red, green, blue);
    }
    Color::from_rgb_u8(0x38, 0xff, 0x66)
}

fn available_move_destination(folder: &Path, file_name: &std::ffi::OsStr) -> PathBuf {
    let requested = folder.join(file_name);
    if !requested.exists() {
        return requested;
    }
    let source = Path::new(file_name);
    let stem = source
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("moved file");
    let extension = source.extension().and_then(|value| value.to_str());
    for number in 2.. {
        let candidate = extension.map_or_else(
            || format!("{stem} ({number})"),
            |extension| format!("{stem} ({number}).{extension}"),
        );
        let destination = folder.join(candidate);
        if !destination.exists() {
            return destination;
        }
    }
    unreachable!("an unused file name is always available")
}

fn move_files_recoverably(
    files: &[PathBuf],
    destination: &Path,
    clean_movie_names: bool,
) -> Result<usize, String> {
    let mut moved = 0;
    for source in files {
        let Some(file_name) = source.file_name() else {
            continue;
        };
        let clean_name;
        let requested_name = if clean_movie_names {
            clean_name = utorrent_mover::clean_movie_file_name(source);
            std::ffi::OsStr::new(&clean_name)
        } else {
            file_name
        };
        let target = available_move_destination(destination, requested_name);
        if fs::rename(source, &target).is_err() {
            fs::copy(source, &target)
                .map_err(|error| format!("Could not copy {}: {error}", source.display()))?;
            if let Err(error) = fs::remove_file(source) {
                let _ = fs::remove_file(&target);
                return Err(format!(
                    "The original could not be removed, so the copied file was rolled back: {error}"
                ));
            }
        }
        moved += 1;
    }
    Ok(moved)
}

fn apply_torrent_move_model(
    ui: &AppWindow,
    items: &[utorrent_mover::TorrentMoveItem],
    selected: &HashSet<String>,
) {
    let model = items
        .iter()
        .map(|item| TorrentMoveEntry {
            key: SharedString::from(item.torrent_key.clone()),
            name: SharedString::from(item.name.clone()),
            detail: SharedString::from(format!(
                "{} • {} video file(s)",
                item.release_year()
                    .map_or_else(|| "Completed".to_owned(), |year| year.to_string()),
                item.files.len()
            )),
            selected: selected.contains(&item.torrent_key),
        })
        .collect::<Vec<_>>();
    ui.set_torrent_move_items(ModelRc::from(Rc::new(VecModel::from(model))));
}

fn escape_toast_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(windows)]
fn system_output_peak() -> Option<f32> {
    use windows::Win32::{
        Media::Audio::{
            Endpoints::IAudioMeterInformation, IMMDeviceEnumerator, MMDeviceEnumerator, eConsole,
            eRender,
        },
        System::Com::{CLSCTX_ALL, CoCreateInstance},
    };

    thread_local! {
        static OUTPUT_METER: RefCell<Option<IAudioMeterInformation>> = const { RefCell::new(None) };
    }
    OUTPUT_METER.with(|slot: &RefCell<Option<IAudioMeterInformation>>| {
        if slot.borrow().is_none() {
            // SAFETY: WindowsRuntime initializes COM on the Slint UI thread,
            // and the endpoint meter remains owned by this same UI thread.
            let meter = unsafe {
                let enumerator: IMMDeviceEnumerator =
                    CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
                let endpoint = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).ok()?;
                endpoint
                    .Activate::<IAudioMeterInformation>(CLSCTX_ALL, None)
                    .ok()?
            };
            *slot.borrow_mut() = Some(meter);
        }
        // SAFETY: The cached COM interface is used only on its owning UI
        // thread and writes the peak value to a Rust-managed return value.
        unsafe {
            slot.borrow()
                .as_ref()?
                .GetPeakValue()
                .ok()
                .map(|value: f32| value.clamp(0.0, 1.0))
        }
    })
}

#[cfg(not(windows))]
fn system_output_peak() -> Option<f32> {
    None
}

#[cfg(windows)]
fn show_windows_toast(title: &str, message: &str) -> anyhow::Result<()> {
    use windows::{
        Data::Xml::Dom::XmlDocument,
        UI::Notifications::{ToastNotification, ToastNotificationManager},
        core::HSTRING,
    };

    let xml = format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        escape_toast_xml(title),
        escape_toast_xml(message)
    );
    let document = XmlDocument::new().context("create toast XML document")?;
    document
        .LoadXml(&HSTRING::from(xml))
        .context("load toast XML")?;
    let notification =
        ToastNotification::CreateToastNotification(&document).context("create Windows toast")?;
    let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from("Zink"))
        .context("create Zink toast notifier")?;
    notifier.Show(&notification).context("show Windows toast")?;
    Ok(())
}

#[cfg(windows)]
fn play_incoming_call_ringtone() {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Media::Audio::{PlaySoundW, SND_ALIAS, SND_ASYNC, SND_FILENAME, SND_LOOP, SND_NODEFAULT},
        core::{PCWSTR, w},
    };

    if let Some(windows_directory) = std::env::var_os("WINDIR") {
        let sound = PathBuf::from(windows_directory).join("Media").join("Ring01.wav");
        let wide = sound.as_os_str().encode_wide().chain(std::iter::once(0)).collect::<Vec<_>>();
        // PlaySoundW accepts a null-terminated filename and loads the sound itself.
        if unsafe { PlaySoundW(PCWSTR(wide.as_ptr()), None, SND_FILENAME | SND_ASYNC | SND_LOOP | SND_NODEFAULT) }.as_bool() {
            return;
        }
    }
    // Use the Windows notification sound when the system ringtone file is absent.
    let _ = unsafe { PlaySoundW(w!("SystemNotification"), None, SND_ALIAS | SND_ASYNC) };
}

#[cfg(windows)]
fn stop_incoming_call_ringtone() {
    use windows::{Win32::Media::Audio::{PlaySoundW, SND_FLAGS}, core::PCWSTR};
    // A null sound cancels playback started by PlaySoundW in this process.
    let _ = unsafe { PlaySoundW(PCWSTR::null(), None, SND_FLAGS(0)) };
}

#[cfg(windows)]
fn clear_call_ui(ui: &AppWindow) {
    ui.set_call_send_stats(SharedString::default());
    ui.set_call_view_stats(SharedString::default());
    ui.set_call_connected(false);
    ui.set_call_in_progress(false);
    ui.set_call_incoming(false);
    ui.set_call_incoming_name(SharedString::default());
    ui.set_call_screen_sharing(false);
    ui.set_call_auto_share(false);
    ui.set_call_remote_visible(false);
    ui.set_call_peer_hdr(false);
    ui.set_call_sharing_hdr10(false);
    ui.set_call_viewing_hdr10(false);
    ui.set_call_remote_image(slint::Image::default());
}

#[cfg(windows)]
fn call_owner_window(ui: &AppWindow) -> Option<isize> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let owner = ui.window().window_handle();
    match owner.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(window) => Some(window.hwnd.get()),
        _ => None,
    }
}

#[cfg(not(windows))]
fn show_windows_toast(_title: &str, _message: &str) -> anyhow::Result<()> {
    anyhow::bail!("Windows notifications are only available on Windows")
}

// The entry point intentionally keeps Slint callback ownership in one place;
// splitting it would obscure the lifetime relationships between weak UI handles
// and the single-threaded Windows media/database objects.
#[cfg(windows)]
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn capture_primary_desktop() -> Result<image::RgbaImage, String> {
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC,
        DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, HGDIOBJ, ReleaseDC, SRCCOPY,
        SelectObject,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) };
    if width <= 0 || height <= 0 {
        return Err("Windows did not report a usable primary display size.".to_owned());
    }

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err("Could not open the Windows desktop surface.".to_owned());
        }
        let memory_dc = CreateCompatibleDC(Some(screen_dc));
        if memory_dc.is_invalid() {
            let _ = ReleaseDC(None, screen_dc);
            return Err("Could not create a screenshot surface.".to_owned());
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, width, height);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory_dc);
            let _ = ReleaseDC(None, screen_dc);
            return Err("Could not allocate the desktop screenshot.".to_owned());
        }
        let previous = SelectObject(memory_dc, HGDIOBJ(bitmap.0));
        let copied = BitBlt(
            memory_dc,
            0,
            0,
            width,
            height,
            Some(screen_dc),
            0,
            0,
            SRCCOPY,
        );

        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bgra = vec![0u8; width as usize * height as usize * 4];
        let scan_lines = if copied.is_ok() {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                height as u32,
                Some(bgra.as_mut_ptr().cast()),
                &raw mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };

        let _ = SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        let _ = ReleaseDC(None, screen_dc);
        if scan_lines == 0 {
            return Err("Windows could not copy the desktop pixels.".to_owned());
        }
        for pixel in bgra.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        image::RgbaImage::from_raw(width as u32, height as u32, bgra)
            .ok_or_else(|| "The desktop screenshot had an invalid pixel layout.".to_owned())
    }
}

#[cfg(windows)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::too_many_lines
)]
fn capture_stream_preview(source_pid: i32) -> Result<(Vec<u8>, u32, u32), String> {
    use windows::Win32::{
        Foundation::{HWND, RECT},
        Graphics::Gdi::{
            BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleBitmap, CreateCompatibleDC,
            DIB_RGB_COLORS, DeleteDC, DeleteObject, GetDC, GetDIBits, GetWindowDC, HALFTONE,
            HGDIOBJ, ReleaseDC, SRCCOPY, SelectObject, SetStretchBltMode, StretchBlt,
        },
        UI::WindowsAndMessaging::{GetSystemMetrics, GetWindowRect, SM_CXSCREEN, SM_CYSCREEN},
    };

    const PREVIEW_WIDTH: i32 = 960;
    const PREVIEW_HEIGHT: i32 = 540;
    let source_window = u32::try_from(source_pid)
        .ok()
        .filter(|pid| *pid > 0)
        .and_then(window_handle_for_pid)
        .map(|handle| HWND(handle as *mut _));
    let (source_width, source_height) = if let Some(window) = source_window {
        let mut bounds = RECT::default();
        if unsafe { GetWindowRect(window, &raw mut bounds) }.is_err() {
            return Err("The selected window is no longer available.".to_owned());
        }
        (
            (bounds.right - bounds.left).max(1),
            (bounds.bottom - bounds.top).max(1),
        )
    } else {
        (
            unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1),
            unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1),
        )
    };

    unsafe {
        let source_dc =
            source_window.map_or_else(|| GetDC(None), |window| GetWindowDC(Some(window)));
        if source_dc.is_invalid() {
            return Err("Windows could not open the selected preview surface.".to_owned());
        }
        let memory_dc = CreateCompatibleDC(Some(source_dc));
        if memory_dc.is_invalid() {
            let _ = ReleaseDC(source_window, source_dc);
            return Err("Windows could not create the preview surface.".to_owned());
        }
        let bitmap = CreateCompatibleBitmap(source_dc, PREVIEW_WIDTH, PREVIEW_HEIGHT);
        if bitmap.is_invalid() {
            let _ = DeleteDC(memory_dc);
            let _ = ReleaseDC(source_window, source_dc);
            return Err("Windows could not allocate the preview bitmap.".to_owned());
        }
        let previous = SelectObject(memory_dc, HGDIOBJ(bitmap.0));
        let _ = SetStretchBltMode(memory_dc, HALFTONE);
        let copied = StretchBlt(
            memory_dc,
            0,
            0,
            PREVIEW_WIDTH,
            PREVIEW_HEIGHT,
            Some(source_dc),
            0,
            0,
            source_width,
            source_height,
            SRCCOPY,
        );
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: PREVIEW_WIDTH,
                biHeight: -PREVIEW_HEIGHT,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut rgba = vec![0u8; PREVIEW_WIDTH as usize * PREVIEW_HEIGHT as usize * 4];
        let scan_lines = if copied.as_bool() {
            GetDIBits(
                memory_dc,
                bitmap,
                0,
                PREVIEW_HEIGHT as u32,
                Some(rgba.as_mut_ptr().cast()),
                &raw mut info,
                DIB_RGB_COLORS,
            )
        } else {
            0
        };
        let _ = SelectObject(memory_dc, previous);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = DeleteDC(memory_dc);
        let _ = ReleaseDC(source_window, source_dc);
        if scan_lines == 0 {
            return Err("Windows could not copy the selected preview pixels.".to_owned());
        }
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        Ok((rgba, PREVIEW_WIDTH as u32, PREVIEW_HEIGHT as u32))
    }
}

#[cfg(windows)]
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn normalized_screenshot_crop(
    left: f32,
    top: f32,
    width: f32,
    height: f32,
    image_width: u32,
    image_height: u32,
) -> (u32, u32, u32, u32) {
    let x = ((left.clamp(0.0, 1.0) * image_width as f32).round() as u32)
        .min(image_width.saturating_sub(1));
    let y = ((top.clamp(0.0, 1.0) * image_height as f32).round() as u32)
        .min(image_height.saturating_sub(1));
    let crop_width = (width.clamp(0.0, 1.0) * image_width as f32)
        .round()
        .max(1.0) as u32;
    let crop_height = (height.clamp(0.0, 1.0) * image_height as f32)
        .round()
        .max(1.0) as u32;
    (
        x,
        y,
        crop_width.min(image_width.saturating_sub(x)).max(1),
        crop_height.min(image_height.saturating_sub(y)).max(1),
    )
}

#[cfg(windows)]
fn screenshot_output_path() -> Result<PathBuf, String> {
    let pictures = directories::UserDirs::new()
        .and_then(|dirs| dirs.picture_dir().map(Path::to_path_buf))
        .ok_or_else(|| "Windows Pictures folder could not be found.".to_owned())?;
    let folder = pictures.join("Zink Screenshots");
    fs::create_dir_all(&folder).map_err(|error| error.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    Ok(folder.join(format!("Zink Screenshot {stamp}.png")))
}

#[allow(clippy::too_many_lines)]
fn main() -> anyhow::Result<()> {
    let arguments = std::env::args_os().collect::<Vec<_>>();
    let software_ui = arguments.iter().any(|argument| argument == "--software-ui");
    if software_ui {
        slint::BackendSelector::new()
            .backend_name("winit".into())
            .renderer_name("software".into())
            .select()
            .context("select Zink software UI renderer")?;
    }
    if arguments.iter().any(|argument| argument == "--zink-splash") {
        return run_splash_mode();
    }
    let browser_only = arguments
        .iter()
        .any(|argument| argument == "--zink-connect");
    let mut splash_process = (!browser_only)
        .then(|| start_splash_process(software_ui))
        .flatten();
    let paths = DataPaths::discover().context("locate Zink data folders")?;
    paths.ensure_exists().context("create Zink data folders")?;

    let file_appender = tracing_appender::rolling::daily(&paths.logs, "zink.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("zink=info,info")),
        )
        .with_writer(file_appender)
        .try_init()
        .ok();
    std::panic::set_hook(Box::new(|panic| {
        let location = panic
            .location()
            .map_or_else(|| "unknown location".to_owned(), ToString::to_string);
        let message = panic
            .payload()
            .downcast_ref::<&str>()
            .copied()
            .unwrap_or_else(|| {
                panic
                    .payload()
                    .downcast_ref::<String>()
                    .map_or("non-string panic", String::as_str)
            });
        error!(%location, %message, "unhandled Zink panic");
    }));

    let _windows = WindowsRuntime::initialize().context("initialize Windows runtime")?;
    let Some(_single_instance) = acquire_single_instance().context("claim Zink instance")? else {
        return Ok(());
    };
    let database = Rc::new(RefCell::new(
        open_database(&paths.database).context("open Zink database")?,
    ));
    let settings = Arc::new(std::sync::Mutex::new(
        load_settings(&paths.settings).context("load settings")?,
    ));
    #[cfg(windows)]
    let replay_hotkey_enabled = Arc::new(AtomicBool::new(
        settings
            .lock()
            .is_ok_and(|settings| settings.clip_recording_hotkey),
    ));
    let liked_radio_songs_path = paths.root.join("liked_radio_songs.json");
    let liked_radio_songs = Arc::new(Mutex::new(load_liked_radio_songs(&liked_radio_songs_path)));

    let ui = AppWindow::new().context("create Zink window")?;
    ui.set_current_app_version(SharedString::from(env!("CARGO_PKG_VERSION")));
    ui.set_current_build_time(SharedString::from(format_build_timestamp(env!(
        "ZINK_BUILD_TIMESTAMP"
    ))));
    ui.set_current_runtime_summary(SharedString::from(
        "Native Rust + Slint • Windows Media Foundation • No .NET runtime",
    ));
    ui.set_current_installation_summary(SharedString::from(current_installation_summary()));
    ui.set_current_data_path(SharedString::from(paths.root.to_string_lossy().as_ref()));
    if let Ok(settings) = settings.lock() {
        ui.set_library_source_folders(ModelRc::from(Rc::new(VecModel::from(
            settings
                .media_library_folders
                .iter()
                .map(SharedString::from)
                .collect::<Vec<_>>(),
        ))));
    }
    ui.set_version_releases(ModelRc::from(Rc::new(VecModel::from(
        version_history_items(),
    ))));
    let (version_date, version_notes) = version_release_details("5.0.0-alpha.1");
    ui.set_selected_version_date(SharedString::from(version_date));
    ui.set_selected_version_notes(SharedString::from(version_notes));
    let weak = ui.as_weak();
    ui.on_select_version(move |version| {
        let Some(ui) = weak.upgrade() else { return };
        let (date, notes) = version_release_details(version.as_str());
        ui.set_selected_version_date(SharedString::from(date));
        ui.set_selected_version_notes(SharedString::from(notes));
    });
    let window_placement_path = paths.root.join("window-placement-v1.json");
    place_window_on_active_monitor(ui.window());
    let media = Rc::new(RefCell::new(
        MediaEngine::new().context("create native media engine")?,
    ));
    let spotify_widget = Rc::new(RefCell::new(None::<SpotifyWidgetWindow>));
    let radio_widget = Rc::new(RefCell::new(None::<RadioWidgetWindow>));
    let fps_widget = Rc::new(RefCell::new(None::<FpsWidgetWindow>));

    let weak = ui.as_weak();
    let spotify_widget_for_open = Rc::clone(&spotify_widget);
    ui.on_open_spotify_widget(move || {
        let Some(ui) = weak.upgrade() else { return };
        if spotify_widget_for_open.borrow().is_none() {
            let Ok(widget) = SpotifyWidgetWindow::new() else {
                ui.set_spotify_status(SharedString::from("Could not create the Spotify widget."));
                return;
            };
            let weak_ui = ui.as_weak();
            widget.on_open_spotify(move || {
                let _ = open_external_url("https://open.spotify.com/");
            });
            widget.on_refresh(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.invoke_spotify_refresh();
                }
            });
            let weak_ui = ui.as_weak();
            widget.on_previous(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.invoke_spotify_previous();
                }
            });
            let weak_ui = ui.as_weak();
            widget.on_toggle_playback(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.invoke_spotify_play_pause();
                }
            });
            let weak_ui = ui.as_weak();
            widget.on_next(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.invoke_spotify_next();
                }
            });
            let weak_widget = widget.as_weak();
            widget.on_hide_widget(move || {
                if let Some(widget) = weak_widget.upgrade() {
                    let _ = widget.hide();
                }
            });
            *spotify_widget_for_open.borrow_mut() = Some(widget);
        }
        if let Some(widget) = spotify_widget_for_open.borrow().as_ref() {
            widget.set_connected(ui.get_spotify_connected());
            widget.set_account_name(ui.get_spotify_display_name());
            widget.set_account_detail(ui.get_spotify_product());
            widget.set_track_title(ui.get_spotify_now_title());
            widget.set_track_artist(ui.get_spotify_now_artist());
            widget.set_track_album(ui.get_spotify_now_album());
            widget.set_device_name(ui.get_spotify_device_name());
            widget.set_playing(ui.get_spotify_is_playing());
            if let Err(error) = widget.show() {
                ui.set_spotify_status(SharedString::from(format!(
                    "Could not show the Spotify widget: {error}"
                )));
            }
        }
    });

    let spotify_widget_for_hide = Rc::clone(&spotify_widget);
    ui.on_hide_spotify_widget(move || {
        if let Some(widget) = spotify_widget_for_hide.borrow().as_ref() {
            let _ = widget.hide();
        }
    });

    let weak = ui.as_weak();
    let spotify_widget_for_sync = Rc::clone(&spotify_widget);
    let spotify_widget_sync_timer = Timer::default();
    spotify_widget_sync_timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else { return };
        let widget = spotify_widget_for_sync.borrow();
        let Some(widget) = widget.as_ref() else {
            return;
        };
        widget.set_connected(ui.get_spotify_connected());
        widget.set_account_name(ui.get_spotify_display_name());
        widget.set_account_detail(ui.get_spotify_product());
        widget.set_track_title(ui.get_spotify_now_title());
        widget.set_track_artist(ui.get_spotify_now_artist());
        widget.set_track_album(ui.get_spotify_now_album());
        widget.set_device_name(ui.get_spotify_device_name());
        widget.set_playing(ui.get_spotify_is_playing());
    });

    let weak = ui.as_weak();
    let radio_widget_for_open = Rc::clone(&radio_widget);
    let media_for_radio_widget = Rc::clone(&media);
    let settings_for_radio_widget = Arc::clone(&settings);
    ui.on_open_radio_widget(move || {
        let Some(ui) = weak.upgrade() else { return };
        if radio_widget_for_open.borrow().is_none() {
            let Ok(widget) = RadioWidgetWindow::new() else {
                ui.set_playback_error(SharedString::from("Could not create the radio widget."));
                return;
            };
            let weak_ui = ui.as_weak();
            let media = Rc::clone(&media_for_radio_widget);
            widget.on_toggle_playback(move || {
                let Some(ui) = weak_ui.upgrade() else { return };
                if ui.get_is_playing() {
                    let _ = media.borrow_mut().pause();
                    ui.set_is_playing(false);
                } else if media.borrow_mut().play().is_ok() {
                    ui.set_is_playing(true);
                }
            });
            let weak_ui = ui.as_weak();
            let media = Rc::clone(&media_for_radio_widget);
            widget.on_stop(move || {
                let _ = media.borrow_mut().stop();
                if let Some(ui) = weak_ui.upgrade() {
                    ui.set_is_playing(false);
                }
            });
            let weak_ui = ui.as_weak();
            let media = Rc::clone(&media_for_radio_widget);
            let settings = Arc::clone(&settings_for_radio_widget);
            widget.on_previous(move || {
                let Some(ui) = weak_ui.upgrade() else { return };
                let station = settings.lock().ok().and_then(|settings| {
                    adjacent_radio_station(&settings, ui.get_track_title().as_str(), -1)
                });
                if let Some((name, uri)) = station
                    && media.borrow_mut().open_uri(&uri).is_ok()
                {
                    ui.set_track_title(SharedString::from(name));
                    ui.set_track_detail(SharedString::from("Internet radio"));
                    ui.set_is_playing(true);
                }
            });
            let weak_ui = ui.as_weak();
            let media = Rc::clone(&media_for_radio_widget);
            let settings = Arc::clone(&settings_for_radio_widget);
            widget.on_next(move || {
                let Some(ui) = weak_ui.upgrade() else { return };
                let station = settings.lock().ok().and_then(|settings| {
                    adjacent_radio_station(&settings, ui.get_track_title().as_str(), 1)
                });
                if let Some((name, uri)) = station
                    && media.borrow_mut().open_uri(&uri).is_ok()
                {
                    ui.set_track_title(SharedString::from(name));
                    ui.set_track_detail(SharedString::from("Internet radio"));
                    ui.set_is_playing(true);
                }
            });
            let weak_ui = ui.as_weak();
            let media = Rc::clone(&media_for_radio_widget);
            widget.on_set_volume(move |value| {
                let clamped = f64::from(value).clamp(0.0, 1.0);
                if media.borrow_mut().set_volume(clamped).is_ok()
                    && let Some(ui) = weak_ui.upgrade()
                {
                    let percentage = format!("{:.0}", clamped * 100.0)
                        .parse::<i32>()
                        .unwrap_or(80);
                    ui.set_volume_percent(percentage);
                }
            });
            *radio_widget_for_open.borrow_mut() = Some(widget);
        }
        if let Some(widget) = radio_widget_for_open.borrow().as_ref() {
            widget.set_station_name(if ui.get_track_detail().as_str() == "Internet radio" {
                ui.get_track_title()
            } else {
                SharedString::from("No station selected")
            });
            widget.set_playing(ui.get_is_playing());
            widget.set_song_title(ui.get_current_radio_track_title());
            widget.set_artist_name(ui.get_current_radio_track_artist());
            widget.set_volume_percent(ui.get_volume_percent());
            if let Err(error) = widget.show() {
                ui.set_playback_error(SharedString::from(format!(
                    "Could not show the radio widget: {error}"
                )));
            }
        }
    });

    let radio_widget_for_hide = Rc::clone(&radio_widget);
    ui.on_hide_radio_widget(move || {
        if let Some(widget) = radio_widget_for_hide.borrow().as_ref() {
            let _ = widget.hide();
        }
    });

    let weak = ui.as_weak();
    let radio_widget_for_sync = Rc::clone(&radio_widget);
    let radio_widget_sync_timer = Timer::default();
    radio_widget_sync_timer.start(TimerMode::Repeated, Duration::from_millis(500), move || {
        let Some(ui) = weak.upgrade() else { return };
        let widget = radio_widget_for_sync.borrow();
        let Some(widget) = widget.as_ref() else {
            return;
        };
        widget.set_station_name(if ui.get_track_detail().as_str() == "Internet radio" {
            ui.get_track_title()
        } else {
            SharedString::from("No station selected")
        });
        widget.set_song_title(ui.get_current_radio_track_title());
        widget.set_artist_name(ui.get_current_radio_track_artist());
        widget.set_volume_percent(ui.get_volume_percent());
        widget.set_playing(ui.get_is_playing());
    });
    let liked_paths_file = paths.root.join("liked-music-v1.json");
    let session_file = paths.root.join("music-session-v1.json");
    let saved_session = load_music_session(&session_file);
    let saved_session_for_restore = Rc::new(saved_session.clone());
    let music = Rc::new(RefCell::new(MusicPlayerState {
        queue: Vec::new(),
        current_index: None,
        shuffle: saved_session.shuffle,
        repeat_current: saved_session.repeat_current,
        background: saved_session.background,
        playback_rate: saved_session.playback_rate.unwrap_or(1.0),
        crossfade_seconds: saved_session.crossfade_seconds.unwrap_or(4).min(12),
        audio_device_id: saved_session.audio_device_id.clone().unwrap_or_default(),
        normalization_enabled: saved_session.normalization_enabled.unwrap_or(false),
        normalization_preamp_db: saved_session.normalization_preamp_db.unwrap_or(0.0),
        liked_paths: load_liked_paths(&liked_paths_file),
        liked_paths_file,
        session_file,
        ..MusicPlayerState::default()
    }));
    if let Some(volume) = saved_session.volume
        && let Err(error) = media.borrow_mut().set_volume(volume)
    {
        error!(%error, "failed to restore music volume");
    }
    if let Some(rate) = saved_session.playback_rate
        && let Err(error) = media.borrow_mut().set_playback_rate(rate)
    {
        error!(%error, "failed to restore music playback rate");
    }
    let routes = PRIMARY_ROUTES
        .iter()
        .map(|route| NavItem {
            id: SharedString::from(route.id),
            title: SharedString::from(route.title),
            group: SharedString::from(route.group),
        })
        .collect::<Vec<_>>();
    ui.set_nav_items(ModelRc::from(Rc::new(VecModel::from(routes))));
    ui.set_artist_releases(ModelRc::from(Rc::new(VecModel::default())));
    ui.set_recording_items(ModelRc::from(Rc::new(VecModel::default())));
    ui.set_recorder_target_items(ModelRc::from(Rc::new(VecModel::default())));
    if let Ok(settings) = settings.lock() {
        apply_home_settings(&ui, &settings);
        apply_equalizer_settings(&ui, &settings);
        if let Err(error) = media.borrow_mut().set_equalizer_profile(
            settings
                .equalizer_enabled
                .then_some(f64::from(settings.equalizer_master_gain)),
            equalizer_band_array(&settings.equalizer_bands),
        ) {
            error!(%error, "failed to restore native equalizer gain profile");
        }
        apply_visualizer_settings(&ui, &settings);
        apply_radio_likes(&ui, &settings);
        apply_radio_stations(&ui, &settings);
        apply_general_settings(&ui, &settings);
        #[cfg(windows)]
        request_settings_copy_translation(
            ui.as_weak(),
            paths.root.join("translation-cache-v1.json"),
            effective_language_tag(&settings.language),
        );
    }
    if let Ok(songs) = liked_radio_songs.lock() {
        apply_liked_radio_songs(&ui, &songs);
    }
    ui.set_has_restorable_session(!saved_session.queue.is_empty());
    refresh_library(&ui, &database, "", None);
    if let Ok(settings) = settings.lock() {
        apply_search_radio_stations(&ui, &settings, "");
    }
    refresh_music_collections(&ui, &database);

    let initial_route = settings.lock().map_or_else(
        |_| "home".into(),
        |value| {
            if value.reopen_last_page {
                value.last_route.clone()
            } else {
                "home".into()
            }
        },
    );
    apply_route(&ui, &initial_route);
    match initial_route.as_str() {
        "music-library" => refresh_library(&ui, &database, "", Some("audio")),
        "video-library" => refresh_library(&ui, &database, "", Some("video")),
        _ => {}
    }
    sync_music_models_shared(&ui, &music);
    ui.set_music_shuffle(music.borrow().shuffle);
    ui.set_music_repeat(music.borrow().repeat_current);
    ui.set_music_background(music.borrow().background);
    #[allow(clippy::cast_possible_truncation)]
    ui.set_playback_rate(music.borrow().playback_rate as f32);
    ui.set_crossfade_seconds(i32::try_from(music.borrow().crossfade_seconds).unwrap_or(4));
    ui.set_normalization_enabled(music.borrow().normalization_enabled);
    #[allow(clippy::cast_possible_truncation)]
    ui.set_normalization_preamp_db(music.borrow().normalization_preamp_db.round() as i32);
    refresh_audio_devices(&ui, &music.borrow().audio_device_id);
    if !music.borrow().audio_device_id.is_empty()
        && let Err(error) = media
            .borrow_mut()
            .set_audio_output_device(&music.borrow().audio_device_id)
    {
        error!(%error, "failed to restore audio output device");
    }
    #[allow(clippy::cast_possible_truncation)]
    ui.set_volume_percent((media.borrow().volume() * 100.0).round() as i32);
    let streaming_preferences_path = paths.root.join("streaming-preferences-v1.json");
    let streaming_preferences = Rc::new(RefCell::new(
        fs::read(&streaming_preferences_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<StreamingPreferences>(&bytes).ok())
            .unwrap_or_default(),
    ));
    {
        let preferences = streaming_preferences.borrow();
        ui.set_stream_quality(SharedString::from(preferences.quality.clone()));
        ui.set_stream_system_audio(preferences.system_audio);
        ui.set_stream_microphone(preferences.microphone);
        ui.set_stream_desktop_volume(preferences.desktop_gain);
        ui.set_stream_microphone_volume(preferences.microphone_gain);
        ui.set_stream_low_latency(preferences.low_latency);
        ui.set_stream_twitch_channel(SharedString::from(preferences.twitch_channel.clone()));
        ui.set_stream_twitch_client_id(SharedString::from(preferences.twitch_client_id.clone()));
        ui.set_stream_twitch_access_token(SharedString::from(
            preferences.twitch_access_token.clone(),
        ));
    }
    let weak = ui.as_weak();
    let settings_for_navigation = Arc::clone(&settings);
    let settings_path = paths.settings.clone();
    let database_for_navigation = Rc::clone(&database);
    let media_for_navigation = Rc::clone(&media);
    let music_for_navigation = Rc::clone(&music);
    let streaming_for_navigation = Rc::clone(&streaming_preferences);
    ui.on_navigate(move |route_id| {
        let route_id = route_id.to_string();
        if let Some(ui) = weak.upgrade() {
            if route_id != "settings" {
                ui.set_settings_reset_armed(false);
            }
            let leaving_music =
                ui.get_current_route().as_str() == "music-player" && route_id != "music-player";
            if leaving_music && !music_for_navigation.borrow().background {
                if let Err(error) = media_for_navigation.borrow_mut().pause() {
                    error!(%error, "failed to pause music when leaving player");
                }
                ui.set_is_playing(false);
            }
            apply_route(&ui, &route_id);
            if let Some(url) = route_web_service_url(&route_id) {
                ui.invoke_open_web_service(SharedString::from(url));
            }
            match route_id.as_str() {
                "music-library" => {
                    refresh_library(&ui, &database_for_navigation, "", Some("audio"));
                }
                "video-library" => {
                    refresh_library(&ui, &database_for_navigation, "", Some("video"));
                }
                "duplicate-video-search" => ui.invoke_scan_duplicate_videos(),
                "recordings" => ui.invoke_refresh_recordings(),
                "screen-recorder" => ui.invoke_refresh_recorder_targets(),
                "tools" => ui.invoke_refresh_torrent_mover(),
                "fps-recorder" => ui.invoke_refresh_fps_processes(),
                "twitch-streaming"
                | "youtube-streaming"
                | "kick-streaming"
                | "instagram-streaming"
                | "facebook-streaming"
                | "tiktok-streaming"
                | "x-streaming" => {
                    apply_streaming_destination(&ui, &route_id, &streaming_for_navigation.borrow());
                    ui.invoke_refresh_recorder_targets();
                }
                _ => {}
            }
            if settings_for_navigation
                .lock()
                .is_ok_and(|settings| settings.discord_rich_presence)
            {
                let route_for_presence = route_id.clone();
                let title_for_presence = ui.get_page_title().to_string();
                std::thread::spawn(move || {
                    let _ = publish_discord_page_presence(&route_for_presence, &title_for_presence);
                });
            }
        }
        if let Ok(mut settings) = settings_for_navigation.lock() {
            settings.last_route.clone_from(&route_id);
            if route_id != "home" && settings.home_route_history.last() != Some(&route_id) {
                settings.home_route_history.push(route_id.clone());
                if settings.home_route_history.len() > 12 {
                    let excess = settings.home_route_history.len() - 12;
                    settings.home_route_history.drain(..excess);
                }
            }
            if let Err(error) = save_settings(&settings_path, &settings) {
                error!(%error, "failed to save navigation state");
            }
            if let Some(ui) = weak.upgrade() {
                apply_home_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_space = Arc::clone(&settings);
    let settings_path_for_space = paths.settings.clone();
    ui.on_set_home_space(move |space| {
        let space = space.to_string();
        if let Ok(mut settings) = settings_for_space.lock() {
            settings.home_space.clone_from(&space);
            let _ = save_settings(&settings_path_for_space, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_home_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_section = Arc::clone(&settings);
    let settings_path_for_section = paths.settings.clone();
    ui.on_toggle_home_section(move |section| {
        if let Ok(mut settings) = settings_for_section.lock() {
            match section.as_str() {
                "flow" => settings.home_show_flow = !settings.home_show_flow,
                "live" => {
                    settings.home_show_live_activities = !settings.home_show_live_activities;
                }
                "insights" => settings.home_show_insights = !settings.home_show_insights,
                "devices" => settings.home_show_devices = !settings.home_show_devices,
                _ => return,
            }
            let _ = save_settings(&settings_path_for_section, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_home_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_pin = Arc::clone(&settings);
    let settings_path_for_pin = paths.settings.clone();
    ui.on_toggle_home_pin(move |route| {
        let route = route.to_string();
        if route_by_id(&route).is_none() {
            return;
        }
        if let Ok(mut settings) = settings_for_pin.lock() {
            if let Some(index) = settings
                .home_pinned_routes
                .iter()
                .position(|item| item == &route)
            {
                settings.home_pinned_routes.remove(index);
            } else {
                if settings.home_pinned_routes.len() >= 8 {
                    settings.home_pinned_routes.remove(0);
                }
                settings.home_pinned_routes.push(route);
            }
            let _ = save_settings(&settings_path_for_pin, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_home_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_execute_home_command(move |query| {
        let Some(ui) = weak.upgrade() else { return };
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return;
        }
        if query == "play" || query == "pause" || query.contains("resume") {
            ui.invoke_toggle_playback();
        } else if query.contains("record") || query.contains("capture") {
            ui.invoke_navigate(SharedString::from("screen-recorder"));
        } else if query.contains("browser") || query.contains("connect") {
            ui.invoke_navigate(SharedString::from("zink-connect"));
        } else if query.contains("radio") {
            ui.invoke_navigate(SharedString::from("radio"));
        } else if query.contains("friend")
            || query.contains("message")
            || query.contains("social")
            || query.contains("call")
        {
            ui.invoke_navigate(SharedString::from("social"));
        } else if query.contains("video") || query.contains("watch") {
            ui.invoke_navigate(SharedString::from("video-player"));
        } else if query.contains("setting") || query.contains("theme") {
            ui.invoke_navigate(SharedString::from("settings"));
        } else if query.contains("library") || query.contains("album") {
            ui.invoke_navigate(SharedString::from("music-library"));
        } else if query.contains("music") || query.contains("song") {
            ui.invoke_navigate(SharedString::from("music-player"));
        } else {
            ui.invoke_navigate(SharedString::from("search"));
        }
        ui.set_home_command_open(false);
        ui.set_home_command_query(SharedString::default());
    });

    #[cfg(windows)]
    let zink_connect = Rc::new(RefCell::new(zink_connect_embedded::ZinkConnect::new()));
    #[cfg(windows)]
    let zink_connect_for_callback = Rc::clone(&zink_connect);
    #[cfg(windows)]
    let weak = ui.as_weak();
    #[cfg(windows)]
    ui.on_open_zink_connect(move || {
        if let Err(error) = zink_connect_for_callback.borrow_mut().open() {
            error!(%error, "failed to open Zink Connect");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    #[cfg(windows)]
    let zink_connect_for_internal_page = Rc::clone(&zink_connect);
    #[cfg(windows)]
    let weak = ui.as_weak();
    #[cfg(windows)]
    ui.on_open_zink_connect_page(move |page| {
        if let Err(error) = zink_connect_for_internal_page
            .borrow_mut()
            .open_internal_page(page.as_str())
        {
            error!(%error, "failed to open Zink Connect internal page");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    #[cfg(windows)]
    let zink_connect_for_services = Rc::clone(&zink_connect);
    #[cfg(windows)]
    let weak = ui.as_weak();
    #[cfg(windows)]
    ui.on_open_web_service(move |url| {
        if let Err(error) = zink_connect_for_services
            .borrow_mut()
            .open_url(url.as_str())
        {
            error!(%error, "failed to open web music service");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    #[cfg(not(windows))]
    ui.on_open_zink_connect(|| {});
    #[cfg(not(windows))]
    ui.on_open_zink_connect_page(|_| {});
    #[cfg(not(windows))]
    ui.on_open_web_service(|_| {});

    let spotify_credentials_path = paths.root.join("spotify-credentials-v1.json");
    let spotify_verifier_path = paths.root.join("spotify-pkce-verifier-v1.txt");
    let weak = ui.as_weak();
    let verifier_path_for_login = spotify_verifier_path.clone();
    ui.on_spotify_start_login(move || {
        let Some(ui) = weak.upgrade() else { return };
        match spotify_authorization_url(&verifier_path_for_login) {
            Ok(url) => {
                ui.set_spotify_status(SharedString::from(
                    "Spotify sign-in opened. Paste the callback address here when it finishes.",
                ));
                if let Err(error) = open_external_url(&url) {
                    ui.set_spotify_status(SharedString::from(format!(
                        "Could not open Spotify sign-in: {error}"
                    )));
                }
            }
            Err(error) => ui.set_spotify_status(SharedString::from(format!(
                "Could not prepare Spotify sign-in: {error}"
            ))),
        }
    });

    let weak = ui.as_weak();
    let verifier_path_for_finish = spotify_verifier_path.clone();
    let credentials_path_for_finish = spotify_credentials_path.clone();
    ui.on_spotify_finish_login(move |callback_value| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(code) = spotify_code_from_callback(callback_value.as_str()) else {
            ui.set_spotify_status(SharedString::from(
                "Paste the complete Spotify callback address or its code value.",
            ));
            return;
        };
        ui.set_spotify_status(SharedString::from("Finishing secure Spotify sign-in…"));
        let verifier_path = verifier_path_for_finish.clone();
        let credentials_path = credentials_path_for_finish.clone();
        std::thread::spawn(move || {
            let result = spotify_exchange_code(&code, &verifier_path)
                .and_then(|credentials| {
                    save_spotify_credentials(&credentials_path, &credentials)?;
                    spotify_profile_result(credentials)
                })
                .unwrap_or_else(|error| SpotifyAuthResult {
                    credentials: None,
                    display_name: String::new(),
                    product: String::new(),
                    status: format!("Spotify sign-in failed: {error}"),
                    dashboard: None,
                });
            if let Ok(mut results) = SPOTIFY_AUTH_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
            {
                results.push(result);
            }
        });
    });

    let weak = ui.as_weak();
    let credentials_path_for_refresh = spotify_credentials_path.clone();
    ui.on_spotify_refresh(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_spotify_status(SharedString::from("Refreshing Spotify account…"));
        let credentials_path = credentials_path_for_refresh.clone();
        std::thread::spawn(move || {
            let result = load_spotify_credentials(&credentials_path)
                .and_then(spotify_profile_result)
                .unwrap_or_else(|error| SpotifyAuthResult {
                    credentials: None,
                    display_name: String::new(),
                    product: String::new(),
                    status: format!("Spotify refresh failed: {error}"),
                    dashboard: None,
                });
            if let Ok(mut results) = SPOTIFY_AUTH_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
            {
                results.push(result);
            }
        });
    });

    let weak = ui.as_weak();
    let credentials_path_for_disconnect = spotify_credentials_path.clone();
    let verifier_path_for_disconnect = spotify_verifier_path.clone();
    ui.on_spotify_disconnect(move || {
        let _ = std::fs::remove_file(&credentials_path_for_disconnect);
        let _ = std::fs::remove_file(&verifier_path_for_disconnect);
        if let Some(ui) = weak.upgrade() {
            ui.set_spotify_connected(false);
            ui.set_spotify_display_name(SharedString::from("Spotify"));
            ui.set_spotify_product(SharedString::from("Not connected"));
            ui.set_spotify_auth_input(SharedString::default());
            ui.set_spotify_search_query(SharedString::default());
            ui.set_spotify_search_results(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_spotify_recent_tracks(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_spotify_top_tracks(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_spotify_queue(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_spotify_devices(ModelRc::from(Rc::new(VecModel::default())));
            ui.set_spotify_status(SharedString::from(
                "Spotify disconnected. Local Spotify tokens were removed.",
            ));
        }
    });

    let credentials_for_play_pause = spotify_credentials_path.clone();
    let weak = ui.as_weak();
    ui.on_spotify_play_pause(move || {
        let Some(ui) = weak.upgrade() else { return };
        let command = if ui.get_spotify_is_playing() {
            "pause"
        } else {
            "play"
        };
        ui.set_spotify_status(SharedString::from("Updating Spotify playback…"));
        queue_spotify_command(credentials_for_play_pause.clone(), command, String::new());
    });

    let credentials_for_previous = spotify_credentials_path.clone();
    ui.on_spotify_previous(move || {
        queue_spotify_command(credentials_for_previous.clone(), "previous", String::new());
    });

    let credentials_for_next = spotify_credentials_path.clone();
    ui.on_spotify_next(move || {
        queue_spotify_command(credentials_for_next.clone(), "next", String::new());
    });

    let credentials_for_track = spotify_credentials_path.clone();
    ui.on_spotify_play_track(move |track_id| {
        queue_spotify_command(credentials_for_track.clone(), "track", track_id.to_string());
    });

    let credentials_for_device = spotify_credentials_path.clone();
    ui.on_spotify_select_device(move |device_id| {
        queue_spotify_command(
            credentials_for_device.clone(),
            "device",
            device_id.to_string(),
        );
    });

    let weak = ui.as_weak();
    let credentials_for_save_track = spotify_credentials_path.clone();
    ui.on_spotify_save_track(move |track_id| {
        let result =
            load_spotify_credentials(&credentials_for_save_track).and_then(|credentials| {
                ureq::put(format!(
                    "https://api.spotify.com/v1/me/tracks?ids={track_id}"
                ))
                .header(
                    "Authorization",
                    format!("Bearer {}", credentials.access_token),
                )
                .send_empty()
                .context("save Spotify track")?;
                Ok(())
            });
        if let Some(ui) = weak.upgrade() {
            ui.set_spotify_status(SharedString::from(match result {
                Ok(()) => "Added to Spotify Liked Songs.".to_owned(),
                Err(error) => format!("Could not save Spotify track: {error}"),
            }));
        }
    });

    let credentials_for_search = spotify_credentials_path.clone();
    let weak = ui.as_weak();
    ui.on_spotify_search(move |query| {
        let query = query.trim().to_owned();
        let Some(ui) = weak.upgrade() else { return };
        if query.is_empty() {
            ui.set_spotify_search_results(ModelRc::from(Rc::new(VecModel::default())));
            return;
        }
        ui.set_spotify_status(SharedString::from(format!(
            "Searching Spotify for “{query}”…"
        )));
        let credentials_path = credentials_for_search.clone();
        std::thread::spawn(move || {
            let result = load_spotify_credentials(&credentials_path)
                .and_then(|credentials| spotify_search_tracks(&credentials, &query))
                .map_err(|error| error.to_string());
            if let Ok(mut results) = SPOTIFY_SEARCH_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
            {
                results.push(SpotifySearchResult { query, result });
            }
        });
    });

    let spotify_result_timer = Timer::default();
    let weak = ui.as_weak();
    spotify_result_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let result = SPOTIFY_AUTH_RESULTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .ok()
            .and_then(|mut results| results.pop());
        let Some(ui) = weak.upgrade() else { return };
        if let Some(result) = result {
            ui.set_spotify_status(SharedString::from(result.status));
            if let Some(dashboard) = result.dashboard {
                apply_spotify_dashboard(&ui, dashboard);
            }
            if result.credentials.is_some() {
                ui.set_spotify_connected(true);
                ui.set_spotify_display_name(SharedString::from(result.display_name));
                ui.set_spotify_product(SharedString::from(result.product));
                ui.set_spotify_auth_input(SharedString::default());
            }
        }
        let search_result = SPOTIFY_SEARCH_RESULTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .ok()
            .and_then(|mut results| results.pop());
        if let Some(search_result) = search_result
            && search_result.query == ui.get_spotify_search_query().as_str()
        {
            match search_result.result {
                Ok(tracks) => {
                    let count = tracks.len();
                    ui.set_spotify_search_results(spotify_track_model(tracks));
                    ui.set_spotify_status(SharedString::from(format!(
                        "Found {count} Spotify tracks for “{}”.",
                        search_result.query
                    )));
                }
                Err(error) => ui.set_spotify_status(SharedString::from(format!(
                    "Spotify search failed: {error}"
                ))),
            }
        }
    });

    if spotify_credentials_path.is_file() {
        ui.set_spotify_status(SharedString::from("Restoring your Spotify account…"));
        let credentials_path = spotify_credentials_path.clone();
        std::thread::spawn(move || {
            let result = load_spotify_credentials(&credentials_path)
                .and_then(spotify_profile_result)
                .unwrap_or_else(|error| SpotifyAuthResult {
                    credentials: None,
                    display_name: String::new(),
                    product: String::new(),
                    status: format!("Spotify session needs attention: {error}"),
                    dashboard: None,
                });
            if let Ok(mut results) = SPOTIFY_AUTH_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
            {
                results.push(result);
            }
        });
    }

    let stream_duration_timer = Timer::default();
    let weak = ui.as_weak();
    let mut stream_started = None::<Instant>;
    stream_duration_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_stream_live() {
            let started = stream_started.get_or_insert_with(Instant::now);
            let elapsed = started.elapsed().as_secs();
            ui.set_stream_duration(SharedString::from(format!(
                "{:02}:{:02}",
                elapsed / 60,
                elapsed % 60
            )));
        } else {
            stream_started = None;
            ui.set_stream_duration(SharedString::from("00:00"));
        }
    });

    #[cfg(windows)]
    {
        let area_window = Rc::new(RefCell::new(None::<AreaSelectionWindow>));
        let area_window_for_open = Rc::clone(&area_window);
        let weak = ui.as_weak();
        ui.on_capture_area(move || {
            let Some(ui) = weak.upgrade() else { return };
            let desktop = match capture_primary_desktop() {
                Ok(desktop) => Rc::new(desktop),
                Err(error) => {
                    ui.set_tools_status(SharedString::from(format!(
                        "Screenshot could not start: {error}"
                    )));
                    return;
                }
            };
            let selector = match AreaSelectionWindow::new() {
                Ok(selector) => selector,
                Err(error) => {
                    ui.set_tools_status(SharedString::from(format!(
                        "Area selector could not open: {error}"
                    )));
                    return;
                }
            };
            let pixel_buffer = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                desktop.as_raw(),
                desktop.width(),
                desktop.height(),
            );
            selector.set_desktop_preview(slint::Image::from_rgba8(pixel_buffer));
            selector.window().set_fullscreen(true);

            let selector_weak = selector.as_weak();
            let ui_weak = ui.as_weak();
            let desktop_for_crop = Rc::clone(&desktop);
            selector.on_completed(move |left, top, width, height| {
                let image_width = desktop_for_crop.width();
                let image_height = desktop_for_crop.height();
                let (x, y, crop_width, crop_height) =
                    normalized_screenshot_crop(left, top, width, height, image_width, image_height);
                let result = screenshot_output_path().and_then(|path| {
                    let cropped = image::imageops::crop_imm(
                        desktop_for_crop.as_ref(),
                        x,
                        y,
                        crop_width,
                        crop_height,
                    )
                    .to_image();
                    cropped.save(&path).map_err(|error| error.to_string())?;
                    Ok(path)
                });
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_tools_status(SharedString::from(match result {
                        Ok(path) => format!("Screenshot saved to {}", path.display()),
                        Err(error) => format!("Screenshot could not be saved: {error}"),
                    }));
                }
                if let Some(selector) = selector_weak.upgrade() {
                    let _ = selector.hide();
                }
            });
            let selector_weak = selector.as_weak();
            let ui_weak = ui.as_weak();
            selector.on_canceled(move || {
                if let Some(selector) = selector_weak.upgrade() {
                    let _ = selector.hide();
                }
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_tools_status(SharedString::from("Screenshot selection cancelled."));
                }
            });
            if let Err(error) = selector.show() {
                ui.set_tools_status(SharedString::from(format!(
                    "Area selector could not be shown: {error}"
                )));
                return;
            }
            *area_window_for_open.borrow_mut() = Some(selector);
            ui.set_tools_status(SharedString::from(
                "Drag over the desktop area to save as a PNG; press Esc to cancel.",
            ));
        });
    }

    #[cfg(not(windows))]
    ui.on_capture_area(|| {});

    let weak = ui.as_weak();
    ui.on_launch_native_recorder(move || {
        let result = open_external_url("ms-screenclip:?source=QuickActions&action=Record");
        if let Some(ui) = weak.upgrade() {
            ui.set_recorder_status(SharedString::from(if result.is_ok() {
                "Windows capture picker opened. Choose the region or window to record."
            } else {
                "The Windows capture picker could not be opened."
            }));
        }
        if let Err(error) = result {
            error!(%error, "failed to launch the Windows screen recorder");
        }
    });

    let weak = ui.as_weak();
    ui.on_refresh_recorder_targets(move || {
        let Some(ui) = weak.upgrade() else { return };
        let targets = enumerate_fps_windows()
            .into_iter()
            .filter_map(|(pid, title)| {
                i32::try_from(pid).ok().map(|pid| FpsProcessItem {
                    pid,
                    title: title.into(),
                })
            })
            .collect::<Vec<_>>();
        ui.set_recorder_target_items(ModelRc::from(Rc::new(VecModel::from(targets))));
        if ui.get_recorder_selected_pid() > 0
            && window_handle_for_pid(u32::try_from(ui.get_recorder_selected_pid()).unwrap_or(0))
                .is_none()
        {
            ui.set_recorder_selected_pid(0);
            ui.set_recorder_selected_target(SharedString::from("Primary display"));
        }
    });
    ui.on_select_recorder_target(|_, _| {});
    ui.invoke_refresh_recorder_targets();

    let weak = ui.as_weak();
    ui.on_refresh_recorder_audio_devices(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_recorder_audio_devices(&ui);
        }
    });
    let settings_for_audio_devices = Arc::clone(&settings);
    let settings_path_for_audio_devices = paths.settings.clone();
    ui.on_save_recorder_audio_devices(move |output_id, input_id| {
        if let Ok(mut settings) = settings_for_audio_devices.lock() {
            settings.recorder_output_device_id = output_id.to_string();
            settings.recorder_microphone_device_id = input_id.to_string();
            if let Err(error) = save_settings(&settings_path_for_audio_devices, &settings) {
                error!(%error, "failed to save recorder audio endpoints");
            }
        }
    });
    ui.invoke_refresh_recorder_audio_devices();

    let weak = ui.as_weak();
    ui.on_refresh_call_audio_devices(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_call_audio_devices(&ui);
        }
    });
    let weak = ui.as_weak();
    let settings_for_call_devices = Arc::clone(&settings);
    let settings_path_for_call_devices = paths.settings.clone();
    ui.on_cycle_call_audio_device(move |output| {
        let Some(ui) = weak.upgrade() else { return };
        let model = if output {
            ui.get_call_output_devices()
        } else {
            ui.get_call_input_devices()
        };
        let current = if output {
            ui.get_call_output_device_name().to_string()
        } else {
            ui.get_call_input_device_name().to_string()
        };
        let next = cycle_audio_device_name(&model, &current);
        if output {
            ui.set_call_output_device_name(next.clone().into());
        } else {
            ui.set_call_input_device_name(next.clone().into());
        }
        if let Ok(mut settings) = settings_for_call_devices.lock() {
            settings.social_call_output_device = ui.get_call_output_device_name().to_string();
            settings.social_call_input_device = ui.get_call_input_device_name().to_string();
            if let Err(error) = save_settings(&settings_path_for_call_devices, &settings) {
                error!(%error, "failed to save call audio devices");
            }
        }
        ui.set_call_status(
            format!(
                "{} will be used for the next call.",
                if next.is_empty() {
                    "Windows default"
                } else {
                    next.as_str()
                }
            )
            .into(),
        );
    });
    ui.invoke_refresh_call_audio_devices();

    #[cfg(windows)]
    let recorder_pipeline = Arc::new(Mutex::new(None::<(win_native_media::Pipeline, PathBuf)>));
    #[cfg(windows)]
    let replay_pipeline = Arc::new(Mutex::new(None::<win_native_media::ReplayBuffer>));
    let weak = ui.as_weak();
    #[cfg(windows)]
    let recorder_for_command = Arc::clone(&recorder_pipeline);
    #[cfg(windows)]
    let replay_for_command = Arc::clone(&replay_pipeline);
    let settings_for_recorder = Arc::clone(&settings);
    let settings_path_for_recorder = paths.settings.clone();
    ui.on_recorder_command(move |command| {
        let command = command.as_str();
        let Some(ui) = weak.upgrade() else { return };
        match command {
            "toggle-recording" => {
                #[cfg(windows)]
                {
                    let existing = recorder_for_command
                        .lock()
                        .ok()
                        .and_then(|mut slot| slot.take());
                    if let Some((mut pipeline, output_path)) = existing {
                        ui.set_recorder_status(SharedString::from(
                            "Stopping and finalizing the native MP4 recording…",
                        ));
                        std::thread::spawn(move || {
                            let result = tokio::runtime::Builder::new_current_thread()
                                .enable_all()
                                .build()
                                .map_err(|error| error.to_string())
                                .and_then(|runtime| {
                                    runtime
                                        .block_on(pipeline.stop())
                                        .map_err(|error| error.to_string())
                                })
                                .map(|()| output_path);
                            if let Ok(mut results) = RECORDER_PIPELINE_RESULTS
                                .get_or_init(|| Mutex::new(Vec::new()))
                                .lock()
                            {
                                results.push(RecorderPipelineResult::Stopped(result));
                            }
                        });
                    } else {
                        let output_directory = recordings_folder();
                        if let Err(error) = fs::create_dir_all(&output_directory) {
                            ui.set_recorder_status(
                                format!("Could not create the recording folder: {error}").into(),
                            );
                            return;
                        }
                        let timestamp = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs();
                        let output_path =
                            output_directory.join(format!("Zink Recording {timestamp}.mp4"));
                        let audio = if ui.get_replay_system_audio() || ui.get_replay_microphone() {
                            Some(win_native_media::AudioConfig {
                                loopback: ui.get_replay_system_audio(),
                                microphone: ui.get_replay_microphone(),
                                loopback_device_id: non_empty_device_id(
                                    ui.get_recorder_output_device_id().as_str(),
                                ),
                                microphone_device_id: non_empty_device_id(
                                    ui.get_recorder_input_device_id().as_str(),
                                ),
                                loopback_gain: 1.0,
                                microphone_gain: 1.0,
                                bitrate: u32::try_from(ui.get_replay_audio_bitrate())
                                    .unwrap_or(192)
                                    .saturating_mul(1_000),
                            })
                        } else {
                            None
                        };
                        let pipeline_slot = Arc::clone(&recorder_for_command);
                        let path_for_result = output_path.clone();
                        let capture_cursor = ui.get_recorder_capture_cursor();
                        let selected_pid =
                            u32::try_from(ui.get_recorder_selected_pid()).unwrap_or(0);
                        let selected_target = ui.get_recorder_selected_target().to_string();
                        let capture_target = if selected_pid == 0 {
                            win_native_media::CaptureTarget::Monitor(0)
                        } else if let Some(hwnd) = window_handle_for_pid(selected_pid) {
                            win_native_media::CaptureTarget::Window(hwnd)
                        } else {
                            ui.set_recorder_selected_pid(0);
                            ui.set_recorder_selected_target(SharedString::from("Primary display"));
                            ui.set_recorder_status(SharedString::from(
                                "The selected window closed. Select another target or record the primary display.",
                            ));
                            return;
                        };
                        let recording_quality = ui.get_recorder_quality().to_string();
                        if let Ok(mut settings) = settings_for_recorder.lock() {
                            settings.recorder_quality.clone_from(&recording_quality);
                            settings.recorder_capture_cursor = capture_cursor;
                            let _ = save_settings(&settings_path_for_recorder, &settings);
                        }
                        ui.set_recorder_status(
                            format!("Starting native capture of {selected_target}…").into(),
                        );
                        std::thread::spawn(move || {
                            let result = (|| -> Result<win_native_media::Pipeline, String> {
                                let mut pipeline = win_native_media::Pipeline::new(
                                    win_native_media::PipelineConfig {
                                        capture_target,
                                        capture_cursor,
                                        video: streaming_video_config(&recording_quality),
                                        record: Some(win_native_media::RecordConfig {
                                            output_path: output_path.clone(),
                                        }),
                                        stream: None,
                                        audio,
                                    },
                                )
                                .map_err(|error| error.to_string())?;
                                let runtime = tokio::runtime::Builder::new_current_thread()
                                    .enable_all()
                                    .build()
                                    .map_err(|error| error.to_string())?;
                                runtime
                                    .block_on(pipeline.start())
                                    .map_err(|error| error.to_string())?;
                                Ok(pipeline)
                            })();
                            let status = result.and_then(|pipeline| {
                                pipeline_slot.lock().map_err(|error| error.to_string()).map(
                                    |mut slot| {
                                        *slot = Some((pipeline, path_for_result.clone()));
                                        (path_for_result, selected_target)
                                    },
                                )
                            });
                            if let Ok(mut results) = RECORDER_PIPELINE_RESULTS
                                .get_or_init(|| Mutex::new(Vec::new()))
                                .lock()
                            {
                                results.push(RecorderPipelineResult::Started(status));
                            }
                        });
                    }
                }
                #[cfg(not(windows))]
                ui.set_recorder_status(SharedString::from(
                    "Native recording requires Windows 10 or Windows 11.",
                ));
            }
            "start-replay" => {
                #[cfg(windows)]
                {
                    if replay_for_command.lock().is_ok_and(|slot| slot.is_some()) {
                        ui.set_replay_status(SharedString::from(
                            "Native replay buffer is already running.",
                        ));
                        return;
                    }
                    let config = win_native_media::ReplayConfig {
                        capture_target: win_native_media::CaptureTarget::Monitor(0),
                        capture_cursor: ui.get_recorder_capture_cursor(),
                        video: streaming_video_config(ui.get_recorder_quality().as_str()),
                        duration: Duration::from_secs(
                            u64::try_from(ui.get_replay_duration_seconds()).unwrap_or(45),
                        ),
                        system_audio: ui.get_replay_system_audio(),
                        microphone: ui.get_replay_microphone(),
                        system_audio_device_id: non_empty_device_id(
                            ui.get_recorder_output_device_id().as_str(),
                        ),
                        microphone_device_id: non_empty_device_id(
                            ui.get_recorder_input_device_id().as_str(),
                        ),
                        separate_audio_tracks: ui.get_replay_separate_tracks(),
                        audio_bitrate: u32::try_from(ui.get_replay_audio_bitrate())
                            .unwrap_or(192)
                            .saturating_mul(1_000),
                    };
                    ui.set_replay_status(SharedString::from(
                        "Starting native WGC replay capture…",
                    ));
                    std::thread::spawn(move || {
                        let result = win_native_media::ReplayBuffer::start(config)
                            .map_err(|error| error.to_string());
                        if let Ok(mut results) = REPLAY_PIPELINE_RESULTS
                            .get_or_init(|| Mutex::new(Vec::new()))
                            .lock()
                        {
                            results.push(ReplayPipelineResult::Started(result));
                        }
                    });
                }
            }
            "stop-replay" => {
                #[cfg(windows)]
                {
                    let replay = replay_for_command
                        .lock()
                        .ok()
                        .and_then(|mut slot| slot.take());
                    if let Some(mut replay) = replay {
                        ui.set_replay_status(SharedString::from("Stopping replay capture…"));
                        std::thread::spawn(move || {
                            let result = replay.stop().map_err(|error| error.to_string());
                            if let Ok(mut results) = REPLAY_PIPELINE_RESULTS
                                .get_or_init(|| Mutex::new(Vec::new()))
                                .lock()
                            {
                                results.push(ReplayPipelineResult::Stopped(result));
                            }
                        });
                    } else {
                        ui.set_replay_status(SharedString::from("Replay buffer is not running."));
                    }
                }
            }
            save_command if save_command.starts_with("save-replay") => {
                #[cfg(windows)]
                {
                    let requested_duration = save_command
                        .strip_prefix("save-replay:")
                        .and_then(|seconds| seconds.parse::<u64>().ok())
                        .map(Duration::from_secs);
                    let output_directory = recordings_folder();
                    if let Err(error) = fs::create_dir_all(&output_directory) {
                        ui.set_replay_status(
                            format!("Could not create the recordings folder: {error}").into(),
                        );
                        return;
                    }
                    let timestamp = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs();
                    let duration_label = requested_duration
                        .map(|duration| format!(" {}s", duration.as_secs()))
                        .unwrap_or_default();
                    let output_path = output_directory
                        .join(format!("Zink Replay{duration_label} {timestamp}.mp4"));
                    let replay_slot = Arc::clone(&replay_for_command);
                    if replay_slot.lock().is_ok_and(|slot| slot.is_none()) {
                        ui.set_replay_status(SharedString::from(
                            "Start the native replay buffer before saving a clip.",
                        ));
                        return;
                    }
                    ui.set_replay_status(SharedString::from(
                        requested_duration.map_or_else(
                            || "Muxing the retained H.264/AAC replay into MP4…".to_owned(),
                            |duration| format!(
                                "Muxing the last {} seconds into a keyframe-aligned MP4…",
                                duration.as_secs()
                            ),
                        ),
                    ));
                    std::thread::spawn(move || {
                        let result = replay_slot
                            .lock()
                            .map_err(|error| error.to_string())
                            .and_then(|slot| {
                                slot.as_ref()
                                    .ok_or_else(|| "replay buffer stopped".to_owned())?
                                    .save_duration(&output_path, requested_duration)
                                    .map(|duration| (output_path, duration))
                                    .map_err(|error| error.to_string())
                            });
                        if let Ok(mut results) = REPLAY_PIPELINE_RESULTS
                            .get_or_init(|| Mutex::new(Vec::new()))
                            .lock()
                        {
                            results.push(ReplayPipelineResult::Saved(result));
                        }
                    });
                }
            }
            "open-settings" => match open_external_url("ms-settings:gaming-gamedvr") {
                Ok(()) => {
                    ui.set_recorder_status(SharedString::from("Windows capture settings opened."));
                }
                Err(error) => {
                    error!(%error, "failed to open Windows capture settings");
                    ui.set_recorder_status(SharedString::from(
                        "Windows capture settings could not be opened.",
                    ));
                }
            },
            _ => ui.set_recorder_status(SharedString::from("Unknown recorder command.")),
        }
    });

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let recorder_result_timer = Timer::default();
        recorder_result_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = weak.upgrade() else { return };
            let results = RECORDER_PIPELINE_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .map(|mut results| std::mem::take(&mut *results))
                .unwrap_or_default();
            for result in results {
                match result {
                    RecorderPipelineResult::Started(Ok((path, target))) => {
                        ui.set_recorder_active(true);
                        ui.set_recorder_status(
                            format!("Recording {target} to {}", path.display()).into(),
                        );
                    }
                    RecorderPipelineResult::Started(Err(error)) => {
                        ui.set_recorder_active(false);
                        ui.set_recorder_status(
                            format!("Native recording could not start: {error}").into(),
                        );
                    }
                    RecorderPipelineResult::Stopped(Ok(path)) => {
                        ui.set_recorder_active(false);
                        ui.set_recorder_status(
                            format!("Recording saved to {}", path.display()).into(),
                        );
                        ui.invoke_refresh_recordings();
                    }
                    RecorderPipelineResult::Stopped(Err(error)) => {
                        ui.set_recorder_active(false);
                        ui.set_recorder_status(
                            format!("Recording stopped with an error: {error}").into(),
                        );
                    }
                }
            }
        });

        let weak = ui.as_weak();
        let replay_pipeline = Arc::clone(&replay_pipeline);
        let settings_for_replay_results = Arc::clone(&settings);
        let settings_path_for_replay_results = paths.settings.clone();
        let replay_result_timer = Timer::default();
        replay_result_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = weak.upgrade() else { return };
            let results = REPLAY_PIPELINE_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .map(|mut results| std::mem::take(&mut *results))
                .unwrap_or_default();
            for result in results {
                match result {
                    ReplayPipelineResult::Started(Ok(replay)) => {
                        if let Ok(mut slot) = replay_pipeline.lock() {
                            *slot = Some(replay);
                        }
                        ui.set_settings_background_replay(true);
                        ui.set_replay_status(SharedString::from(
                            "Native replay is running. The buffer is filling with H.264 video and selected AAC tracks.",
                        ));
                    }
                    ReplayPipelineResult::Started(Err(error)) => {
                        if let Ok(mut settings) = settings_for_replay_results.lock() {
                            settings.background_replay = false;
                            let _ = save_settings(&settings_path_for_replay_results, &settings);
                        }
                        ui.set_settings_background_replay(false);
                        ui.set_replay_status(
                            format!("Native replay could not start: {error}").into(),
                        );
                    }
                    ReplayPipelineResult::Saved(Ok((path, duration))) => {
                        ui.set_replay_status(
                            format!(
                                "Saved {:.0} seconds of native replay to {}",
                                duration.as_secs_f64(),
                                path.display()
                            )
                            .into(),
                        );
                        ui.invoke_refresh_recordings();
                    }
                    ReplayPipelineResult::Saved(Err(error)) => {
                        ui.set_replay_status(format!("Replay could not be saved: {error}").into());
                    }
                    ReplayPipelineResult::Stopped(Ok(())) => {
                        ui.set_settings_background_replay(false);
                        ui.set_replay_status(SharedString::from("Native replay buffer stopped."));
                    }
                    ReplayPipelineResult::Stopped(Err(error)) => {
                        ui.set_settings_background_replay(false);
                        ui.set_replay_status(
                            format!("Replay stopped with an error: {error}").into(),
                        );
                    }
                }
            }
            if let Ok(slot) = replay_pipeline.lock()
                && let Some(replay) = slot.as_ref()
            {
                let seconds = replay.buffered_duration().as_secs();
                if seconds > 0 {
                    ui.set_replay_status(
                        format!(
                            "Native replay ready • {seconds}/{} seconds buffered • H.264 + AAC",
                            ui.get_replay_duration_seconds()
                        )
                        .into(),
                    );
                }
            }
        });
    }

    ui.on_open_recordings_folder(|| {
        let recordings = recordings_folder();
        if let Err(error) = std::fs::create_dir_all(&recordings) {
            error!(%error, path = %recordings.display(), "failed to create recordings folder");
            return;
        }
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg(&recordings)
            .spawn()
        {
            error!(%error, path = %recordings.display(), "failed to open recordings folder");
        }
    });

    let weak = ui.as_weak();
    ui.on_open_recording_external(move |path| {
        let path = PathBuf::from(path.as_str());
        if !path.is_file() {
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(
                    "That recording no longer exists. Refresh the recordings library.",
                ));
            }
            return;
        }
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg(&path)
            .spawn()
        {
            error!(%error, path = %path.display(), "failed to open recording with Windows");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(format!(
                    "Could not open that recording: {error}"
                )));
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_delete_recording(move |path| {
        let Some(ui) = weak.upgrade() else { return };
        let path = PathBuf::from(path.as_str());
        match recycle_file(&path) {
            Ok(()) => {
                ui.set_recorder_status(
                    format!("Moved {} to the Recycle Bin.", path.display()).into(),
                );
                ui.invoke_refresh_recordings();
            }
            Err(error) => {
                error!(%error, path = %path.display(), "failed to recycle recording");
                ui.set_recorder_status(format!("Could not recycle recording: {error}").into());
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_refresh_recordings(move || {
        let Some(ui) = weak.upgrade() else { return };
        refresh_recordings(&ui);
    });
    refresh_recordings(&ui);

    let replay_preferences_path = paths.root.join("replay-preferences-v1.json");
    let replay_preferences = std::fs::read(&replay_preferences_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<ReplayPreferences>(&bytes).ok())
        .unwrap_or_default();
    ui.set_replay_duration_seconds(replay_preferences.duration_seconds);
    ui.set_replay_system_audio(replay_preferences.system_audio);
    ui.set_replay_microphone(replay_preferences.microphone);
    ui.set_replay_separate_tracks(replay_preferences.separate_tracks);
    ui.set_replay_separate_desktop_track(replay_preferences.separate_desktop_track);
    ui.set_replay_separate_microphone_track(replay_preferences.separate_microphone_track);
    ui.set_replay_audio_bitrate(replay_preferences.audio_bitrate);
    let weak = ui.as_weak();
    ui.on_save_replay_settings(
        move |duration, system_audio, microphone, separate_tracks, bitrate| {
            let (desktop_track, microphone_track) = weak.upgrade().map_or((false, false), |ui| {
                (
                    ui.get_replay_separate_desktop_track(),
                    ui.get_replay_separate_microphone_track(),
                )
            });
            let preferences = ReplayPreferences {
                duration_seconds: duration.clamp(15, 600),
                system_audio,
                microphone,
                separate_tracks,
                separate_desktop_track: desktop_track && system_audio,
                separate_microphone_track: microphone_track && microphone,
                audio_bitrate: bitrate.clamp(128, 384),
            };
            let result = serde_json::to_vec_pretty(&preferences)
                .context("serialize replay preferences")
                .and_then(|bytes| {
                    std::fs::write(&replay_preferences_path, bytes)
                        .context("save replay preferences")
                });
            if let Some(ui) = weak.upgrade() {
                ui.set_replay_status(SharedString::from(if result.is_ok() {
                    "Replay settings saved. They will be used by the native replay pipeline."
                } else {
                    "Replay settings could not be saved."
                }));
            }
            if let Err(error) = result {
                error!(%error, "failed to save replay preferences");
            }
        },
    );
    if settings
        .lock()
        .is_ok_and(|settings| settings.background_replay)
    {
        ui.invoke_recorder_command(SharedString::from("start-replay"));
    }

    let fps_preferences_path = paths.root.join("fps-preferences-v1.json");
    let fps_preferences = std::fs::read(&fps_preferences_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<FpsPreferences>(&bytes).ok())
        .unwrap_or_default();
    ui.set_fps_target_rate(fps_preferences.target_fps.clamp(30, 144));
    ui.set_fps_sample_window(fps_preferences.sample_window_seconds.clamp(1, 10));
    ui.set_fps_record_csv(fps_preferences.record_csv);
    ui.set_fps_show_widget(fps_preferences.show_widget);
    ui.set_fps_widget_text_only(fps_preferences.widget_text_only);
    ui.set_fps_widget_opacity(fps_preferences.widget_opacity.clamp(50, 100));
    ui.set_fps_include_cursor(fps_preferences.include_cursor);
    ui.set_fps_hide_capture_border(fps_preferences.hide_capture_border);

    let weak = ui.as_weak();
    let fps_preferences_path_for_save = fps_preferences_path.clone();
    ui.on_save_fps_settings(move || {
        let Some(ui) = weak.upgrade() else { return };
        let preferences = FpsPreferences {
            target_fps: ui.get_fps_target_rate().clamp(30, 144),
            sample_window_seconds: ui.get_fps_sample_window().clamp(1, 10),
            record_csv: ui.get_fps_record_csv(),
            show_widget: ui.get_fps_show_widget(),
            widget_text_only: ui.get_fps_widget_text_only(),
            widget_opacity: ui.get_fps_widget_opacity().clamp(50, 100),
            include_cursor: ui.get_fps_include_cursor(),
            hide_capture_border: ui.get_fps_hide_capture_border(),
        };
        match serde_json::to_vec_pretty(&preferences)
            .context("serialize FPS preferences")
            .and_then(|bytes| {
                std::fs::write(&fps_preferences_path_for_save, bytes)
                    .context("save FPS preferences")
            }) {
            Ok(()) => ui.set_fps_status(SharedString::from("FPS settings saved.")),
            Err(error) => {
                error!(%error, "failed to save FPS settings");
                ui.set_fps_status(SharedString::from("FPS settings could not be saved."));
            }
        }
    });

    let weak = ui.as_weak();
    let fps_widget_for_open = Rc::clone(&fps_widget);
    ui.on_open_fps_widget(move || {
        let Some(ui) = weak.upgrade() else { return };
        if fps_widget_for_open.borrow().is_none() {
            let Ok(widget) = FpsWidgetWindow::new() else {
                ui.set_fps_status(SharedString::from("Could not create the FPS overlay."));
                return;
            };
            let weak_ui = ui.as_weak();
            widget.on_close_widget(move || {
                if let Some(ui) = weak_ui.upgrade() {
                    ui.invoke_hide_fps_widget();
                }
            });
            *fps_widget_for_open.borrow_mut() = Some(widget);
        }
        if let Some(widget) = fps_widget_for_open.borrow().as_ref() {
            widget.set_target_name(ui.get_fps_target_name());
            widget.set_current_fps(ui.get_fps_current());
            widget.set_frame_time(ui.get_fps_frame_time());
            widget.set_text_only(ui.get_fps_widget_text_only());
            widget.set_glass_opacity(ui.get_fps_widget_opacity().clamp(50, 100));
            if let Err(error) = widget.show() {
                ui.set_fps_status(SharedString::from(format!(
                    "Could not show the FPS overlay: {error}"
                )));
            }
        }
    });
    let fps_widget_for_hide = Rc::clone(&fps_widget);
    ui.on_hide_fps_widget(move || {
        if let Some(widget) = fps_widget_for_hide.borrow().as_ref() {
            let _ = widget.hide();
        }
    });

    let fps_monitor = Rc::new(RefCell::new(FpsMonitorState::default()));
    let weak = ui.as_weak();
    ui.on_refresh_fps_processes(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_fps_processes(&ui);
        }
    });
    refresh_fps_processes(&ui);

    let weak = ui.as_weak();
    let monitor_for_select = Rc::clone(&fps_monitor);
    ui.on_select_fps_process(move |pid, title| {
        let Ok(pid) = u32::try_from(pid) else { return };
        let mut monitor = monitor_for_select.borrow_mut();
        if let Some(mut recording) = monitor.recording.take() {
            let _ = std::io::Write::flush(&mut recording);
        }
        monitor.target_pid = pid;
        monitor.target_name = title.to_string();
        monitor.started = Some(Instant::now());
        monitor.samples.clear();
        monitor.frame_count = 0;
        monitor.recording_path = None;
        if let Some(ui) = weak.upgrade() {
            ui.set_fps_selected_pid(i32::try_from(pid).unwrap_or(i32::MAX));
            ui.set_fps_target_name(title);
            ui.set_fps_status(SharedString::from(
                "Waiting for RTSS to publish this game's frame data…",
            ));
            reset_fps_ui(&ui);
            if ui.get_fps_show_widget() {
                ui.invoke_open_fps_widget();
            }
        }
    });

    let weak = ui.as_weak();
    let monitor_for_stop = Rc::clone(&fps_monitor);
    ui.on_stop_fps_monitor(move || {
        let mut monitor = monitor_for_stop.borrow_mut();
        if let Some(mut recording) = monitor.recording.take() {
            let _ = std::io::Write::flush(&mut recording);
        }
        *monitor = FpsMonitorState::default();
        if let Some(ui) = weak.upgrade() {
            ui.set_fps_selected_pid(0);
            ui.set_fps_target_name(SharedString::from("No game selected"));
            ui.set_fps_status(SharedString::from("Monitoring stopped."));
            ui.set_fps_recording(false);
            reset_fps_ui(&ui);
            ui.invoke_hide_fps_widget();
        }
    });

    let weak = ui.as_weak();
    let monitor_for_recording = Rc::clone(&fps_monitor);
    ui.on_toggle_fps_recording(move || {
        let mut monitor = monitor_for_recording.borrow_mut();
        if monitor.target_pid == 0 || !weak.upgrade().is_some_and(|ui| ui.get_fps_record_csv()) {
            return;
        }
        if let Some(mut recording) = monitor.recording.take() {
            let _ = std::io::Write::flush(&mut recording);
            let path = monitor.recording_path.clone();
            if let Some(ui) = weak.upgrade() {
                ui.set_fps_recording(false);
                ui.set_fps_status(SharedString::from(path.map_or_else(
                    || "FPS recording stopped.".to_owned(),
                    |path| format!("FPS recording saved to {}", path.display()),
                )));
            }
            return;
        }
        match create_fps_recording(&monitor.target_name) {
            Ok((file, path)) => {
                monitor.recording = Some(file);
                monitor.recording_path = Some(path.clone());
                if let Some(ui) = weak.upgrade() {
                    ui.set_fps_recording(true);
                    ui.set_fps_status(SharedString::from(format!(
                        "Recording performance to {}",
                        path.display()
                    )));
                }
            }
            Err(error) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_fps_status(SharedString::from(format!(
                        "Could not start FPS recording: {error}"
                    )));
                }
            }
        }
    });

    ui.on_open_rtss_download(|| {
        if let Err(error) = open_external_url(
            "https://www.guru3d.com/download/rtss-rivatuner-statistics-server-download/",
        ) {
            error!(%error, "failed to open RTSS download page");
        }
    });

    let fps_timer = Timer::default();
    let weak = ui.as_weak();
    let monitor_for_timer = Rc::clone(&fps_monitor);
    let fps_widget_for_timer = Rc::clone(&fps_widget);
    fps_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
        let Some(ui) = weak.upgrade() else { return };
        let mut monitor = monitor_for_timer.borrow_mut();
        if monitor.target_pid == 0 {
            return;
        }
        match read_rtss_fps(monitor.target_pid) {
            Ok(Some(sample)) => update_fps_monitor(&ui, &mut monitor, sample),
            Ok(None) => ui.set_fps_status(SharedString::from(
                "RTSS is running, but the selected game has not published frame data yet.",
            )),
            Err(error) => ui.set_fps_status(SharedString::from(error)),
        }
        if let Some(widget) = fps_widget_for_timer.borrow().as_ref() {
            widget.set_target_name(ui.get_fps_target_name());
            widget.set_current_fps(ui.get_fps_current());
            widget.set_frame_time(ui.get_fps_frame_time());
            widget.set_text_only(ui.get_fps_widget_text_only());
            widget.set_glass_opacity(ui.get_fps_widget_opacity().clamp(50, 100));
        }
    });

    let visualizer_meter_timer = Timer::default();
    let weak = ui.as_weak();
    let visualizer_analyzer = Rc::new(RefCell::new((None, false)));
    let visualizer_analyzer_for_timer = Rc::clone(&visualizer_analyzer);
    visualizer_meter_timer.start(TimerMode::Repeated, Duration::from_millis(50), move || {
        let Some(ui) = weak.upgrade() else { return };
        if !matches!(ui.get_current_route().as_str(), "equalizer" | "visualizer") {
            let mut analyzer = visualizer_analyzer_for_timer.borrow_mut();
            analyzer.0 = None;
            analyzer.1 = false;
            return;
        }
        if ui.get_current_route().as_str() == "visualizer" && ui.get_visualizer_paused() {
            return;
        }
        {
            let mut analyzer = visualizer_analyzer_for_timer.borrow_mut();
            if analyzer.0.is_none() && !analyzer.1 {
                analyzer.1 = true;
                match win_native_media::VisualizerAnalyzer::start() {
                    Ok(active) => analyzer.0 = Some(active),
                    Err(error) => {
                        error!(%error, "failed to start WASAPI visualizer capture");
                        ui.set_visualizer_status(SharedString::from(
                            "Windows loopback capture is unavailable",
                        ));
                    }
                }
            }
        }
        let (frame, native_available) = visualizer_analyzer_for_timer
            .borrow()
            .0
            .as_ref()
            .map_or_else(
                || (win_native_media::VisualizerFrame::default(), false),
                |analyzer| (analyzer.latest(), true),
            );
        let measured = if native_available {
            frame.peak
        } else {
            system_output_peak().unwrap_or(0.0)
        };
        #[allow(clippy::cast_precision_loss)]
        let sensitivity = ui.get_visualizer_sensitivity() as f32 / 100.0;
        #[allow(clippy::cast_precision_loss)]
        let smoothing = ui.get_visualizer_smoothing() as f32 / 100.0;
        let target = (measured * (0.4 + sensitivity * 1.6)).clamp(0.0, 1.0);
        let previous = ui.get_visualizer_level();
        let response = (1.0 - smoothing * 0.88).clamp(0.08, 1.0);
        ui.set_visualizer_level(previous + (target - previous) * response);
        let spectrum = frame
            .spectrum
            .into_iter()
            .map(|value| (value * (0.4 + sensitivity * 1.6)).clamp(0.0, 1.0))
            .collect::<Vec<_>>();
        let waveform = frame
            .waveform
            .into_iter()
            .map(|value| (value.abs() * (0.4 + sensitivity * 1.6)).clamp(0.0, 1.0))
            .collect::<Vec<_>>();
        ui.set_visualizer_spectrum(ModelRc::new(VecModel::from(spectrum)));
        ui.set_visualizer_waveform(ModelRc::new(VecModel::from(waveform)));
        if native_available {
            ui.set_visualizer_status(SharedString::from("WASAPI • 24 live frequency bands"));
        }
    });

    let social_session_path = paths.root.join("social-session-v1.json");
    let social_session = Rc::new(RefCell::new(social_load_session(&social_session_path).ok()));
    let social_message_state = Rc::new(RefCell::new(SocialMessageState::default()));
    #[cfg(windows)]
    let social_call_sender = Rc::new(RefCell::new(
        None::<tokio::sync::mpsc::Sender<SocialCallCommand>>,
    ));
    #[cfg(windows)]
    let social_incoming_call = Rc::new(RefCell::new(None::<(i64, String)>));
    #[cfg(windows)]
    let social_voice = Rc::new(RefCell::new(None::<social_voice::SocialVoiceEngine>));
    #[cfg(windows)]
    let social_screen_share = Rc::new(RefCell::new(None::<social_screen_share::SocialScreenShare>));
    #[cfg(windows)]
    let social_video_receiver = Rc::new(RefCell::new(
        None::<social_video_decoder::SocialVideoReceiver>,
    ));
    #[cfg(windows)]
    if let Some(session) = social_session.borrow().as_ref().cloned() {
        *social_call_sender.borrow_mut() = Some(start_social_call_worker(session));
    }
    if let Some(session) = social_session.borrow().as_ref() {
        social_message_state.borrow_mut().conversations =
            load_social_messages(&social_messages_path(&paths.root, session.user_id));
        ui.set_social_signed_in(true);
        ui.set_social_username(SharedString::from(session.username.clone()));
        ui.set_social_display_name(SharedString::from(session.display_name.clone()));
        ui.set_social_status(SharedString::from("Restoring your Zink Social circle…"));
        let session = session.clone();
        std::thread::spawn(move || {
            push_social_result(SocialResult::Friends(
                social_load_people(&session).map_err(|error| error.to_string()),
            ));
        });
    }
    refresh_social_message_models(&ui, &social_message_state.borrow());

    let social_profile_path = paths.root.join("zink-social-profile-details-v1.json");
    let profile_user_id = social_session
        .borrow()
        .as_ref()
        .map_or(0, |session| session.user_id);
    let social_profile = Rc::new(RefCell::new(
        load_social_profile(&social_profile_path)
            .filter(|profile| profile.user_id == profile_user_id)
            .unwrap_or_else(|| SocialProfileDetails {
                user_id: profile_user_id,
                signed_up: "Not available".to_owned(),
                ..SocialProfileDetails::default()
            }),
    ));
    apply_social_profile_to_ui(&ui, &social_profile.borrow());

    let social_developer_path = paths.root.join("social-developer-settings-v1.json");
    let social_developer = std::fs::read(&social_developer_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<SocialDeveloperSettings>(&bytes).ok())
        .unwrap_or_default();
    ui.set_social_dev_offline(social_developer.offline_mode);
    ui.set_social_dev_mock_calls(social_developer.mock_calls);
    ui.set_social_dev_display_name(SharedString::from(social_developer.display_name));
    ui.set_social_dev_username(SharedString::from(social_developer.username));
    ui.set_social_dev_email(SharedString::from(social_developer.email));
    ui.set_social_dev_call_server(SharedString::from(social_developer.call_server));

    let weak = ui.as_weak();
    ui.on_unlock_social_developer(move |password| {
        let Some(ui) = weak.upgrade() else { return };
        if password.as_str() == "++773317" {
            ui.set_social_dev_unlocked(true);
            ui.set_social_dev_unlock_password(SharedString::default());
            ui.set_social_status(SharedString::from("Developer settings unlocked."));
        } else {
            ui.set_social_dev_unlocked(false);
            ui.set_social_status(SharedString::from("The developer password is incorrect."));
        }
    });

    let weak = ui.as_weak();
    let developer_path = social_developer_path.clone();
    ui.on_save_social_developer_settings(
        move |offline, mock_calls, display_name, username, email, call_server| {
            let settings = SocialDeveloperSettings {
                offline_mode: offline,
                mock_calls,
                display_name: display_name.trim().to_owned(),
                username: username.trim().to_owned(),
                email: email.trim().to_owned(),
                call_server: call_server.trim().to_owned(),
            };
            let result = serde_json::to_vec_pretty(&settings)
                .context("serialize social developer settings")
                .and_then(|bytes| {
                    std::fs::write(&developer_path, bytes).context("save social developer settings")
                });
            if let Some(ui) = weak.upgrade() {
                ui.set_social_status(SharedString::from(if result.is_ok() {
                    "Developer settings saved locally."
                } else {
                    "Developer settings could not be saved."
                }));
            }
            if let Err(error) = result {
                error!(%error, "failed to save social developer settings");
            }
        },
    );

    let weak = ui.as_weak();
    let developer_session_path = social_session_path.clone();
    ui.on_register_social_developer(move |display_name, username, password| {
        let Some(ui) = weak.upgrade() else { return };
        if username.trim().is_empty() || password.len() < 6 {
            ui.set_social_status(SharedString::from(
                "Enter a test username and a password of at least 6 characters.",
            ));
            return;
        }
        ui.set_social_status(SharedString::from("Registering the developer account…"));
        let display_name = display_name.to_string();
        let username = username.to_string();
        let password = password.to_string();
        let session_path = developer_session_path.clone();
        std::thread::spawn(move || {
            let result = social_register(&username, &password, &display_name).and_then(|session| {
                social_save_session(&session_path, &session)?;
                Ok(session)
            });
            push_social_result(SocialResult::Auth(
                result.map_err(|error| error.to_string()),
            ));
        });
    });

    let weak = ui.as_weak();
    let session_path = social_session_path.clone();
    ui.on_social_login(move |identity, password| {
        let Some(ui) = weak.upgrade() else { return };
        if identity.trim().is_empty() || password.is_empty() {
            ui.set_social_status(SharedString::from(
                "Enter your username or email and password.",
            ));
            return;
        }
        ui.set_social_status(SharedString::from("Signing in to Zink Social…"));
        let identity = identity.to_string();
        let password = password.to_string();
        let session_path = session_path.clone();
        std::thread::spawn(move || {
            let result = social_login(&identity, &password).and_then(|session| {
                social_save_session(&session_path, &session)?;
                Ok(session)
            });
            push_social_result(SocialResult::Auth(
                result.map_err(|error| error.to_string()),
            ));
        });
    });

    let weak = ui.as_weak();
    let session_path = social_session_path.clone();
    ui.on_social_register(move |display_name, username, _email, password| {
        let Some(ui) = weak.upgrade() else { return };
        if display_name.trim().is_empty() || username.trim().is_empty() || password.encode_utf16().count() < 8 {
            ui.set_social_status(SharedString::from(
                "Enter a display name and username, and use a password of at least 8 characters.",
            ));
            return;
        }
        ui.set_social_status(SharedString::from("Creating your Zink Social account…"));
        let display_name = display_name.to_string();
        let username = username.to_string();
        let password = password.to_string();
        let session_path = session_path.clone();
        std::thread::spawn(move || {
            let result = social_register(&username, &password, &display_name).and_then(|session| {
                social_save_session(&session_path, &session)?;
                Ok(session)
            });
            push_social_result(SocialResult::Auth(
                result.map_err(|error| error.to_string()),
            ));
        });
    });

    let weak = ui.as_weak();
    let session_path = social_session_path.clone();
    let session_for_logout = Rc::clone(&social_session);
    let messages_for_logout = Rc::clone(&social_message_state);
    #[cfg(windows)]
    let call_sender_for_logout = Rc::clone(&social_call_sender);
    #[cfg(windows)]
    let voice_for_logout = Rc::clone(&social_voice);
    #[cfg(windows)]
    let share_for_logout = Rc::clone(&social_screen_share);
    #[cfg(windows)]
    let video_for_logout = Rc::clone(&social_video_receiver);
    #[cfg(windows)]
    let incoming_for_logout = Rc::clone(&social_incoming_call);
    ui.on_social_logout(move || {
        #[cfg(windows)]
        if let Some(sender) = call_sender_for_logout.borrow_mut().take() {
            let _ = sender.try_send(SocialCallCommand::Shutdown);
        }
        #[cfg(windows)]
        {
            *voice_for_logout.borrow_mut() = None;
            *share_for_logout.borrow_mut() = None;
            *video_for_logout.borrow_mut() = None;
            *incoming_for_logout.borrow_mut() = None;
        }
        let session = session_for_logout.borrow().clone();
        *session_for_logout.borrow_mut() = None;
        let _ = std::fs::remove_file(&session_path);
        if let Some(session) = session {
            std::thread::spawn(move || {
                let _ = social_post(&session, "/api/logout", None);
            });
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_social_signed_in(false);
            #[cfg(windows)]
            clear_call_ui(&ui);
            ui.set_social_password(SharedString::default());
            ui.set_social_friends(ModelRc::default());
            ui.set_social_search_results(ModelRc::default());
            ui.set_social_requests(ModelRc::default());
            ui.set_friend_request_alert(SharedString::default());
            ui.set_call_incoming(false);
            ui.set_call_incoming_name(SharedString::default());
            *messages_for_logout.borrow_mut() = SocialMessageState::default();
            refresh_social_message_models(&ui, &messages_for_logout.borrow());
            ui.set_social_active_name(SharedString::default());
            ui.set_social_active_username(SharedString::default());
            ui.set_social_avatar_set(false);
            ui.set_social_avatar_image(slint::Image::default());
            ui.set_social_online_count(0);
            ui.set_call_remote_visible(false);
            ui.set_call_remote_image(slint::Image::default());
            ui.set_social_status(SharedString::from("Signed out of Zink Social."));
            ui.invoke_navigate(SharedString::from("social-login"));
        }
    });

    let weak = ui.as_weak();
    let session_for_profile = Rc::clone(&social_session);
    ui.on_social_save_profile(move |display_name| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_profile.borrow().clone() else { return };
        let name = display_name.trim().to_owned();
        if name.is_empty() || name.encode_utf16().count() > 80 {
            ui.set_social_status(SharedString::from("Display name must be 1–80 characters."));
            return;
        }
        ui.set_social_status(SharedString::from("Saving display name…"));
        std::thread::spawn(move || {
            let result = social_post(&session, "/api/me/profile", Some(serde_json::json!({
                "displayName": name
            }))).map(|()| name).map_err(|error| error.to_string());
            push_social_result(SocialResult::Profile(session.user_id, result));
        });
    });

    let weak = ui.as_weak();
    let profile_for_avatar = Rc::clone(&social_profile);
    let profile_path_for_avatar = social_profile_path.clone();
    let profile_root_for_avatar = paths.root.clone();
    ui.on_social_choose_avatar(move || match pick_image_file() {
        Ok(Some(source)) => {
            let avatar_directory = profile_root_for_avatar.join("profile-pictures");
            let result = fs::create_dir_all(&avatar_directory)
                .context("create the profile-picture directory")
                .and_then(|()| {
                    let extension = source
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or("png");
                    let user_id = profile_for_avatar.borrow().user_id.max(0);
                    let destination = avatar_directory.join(format!("{user_id}.{extension}"));
                    fs::copy(&source, &destination).context("copy the selected profile picture")?;
                    Ok(destination)
                });
            if let Some(ui) = weak.upgrade() {
                match result {
                    Ok(destination) => {
                        profile_for_avatar.borrow_mut().avatar_path =
                            destination.to_string_lossy().into_owned();
                        let save_result = save_social_profile(
                            &profile_path_for_avatar,
                            &profile_for_avatar.borrow(),
                        );
                        apply_social_profile_to_ui(&ui, &profile_for_avatar.borrow());
                        ui.set_social_status(SharedString::from(if save_result.is_ok() {
                            "Profile picture updated."
                        } else {
                            "The picture changed, but its profile record could not be saved."
                        }));
                    }
                    Err(error) => ui.set_social_status(SharedString::from(error.to_string())),
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_social_status(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let profile_for_links = Rc::clone(&social_profile);
    let profile_path_for_links = social_profile_path.clone();
    ui.on_social_save_profile_details(
        move |service_one, url_one, service_two, url_two, service_three, url_three| {
            let Some(ui) = weak.upgrade() else { return };
            let values = [
                (service_one.as_str(), url_one.as_str()),
                (service_two.as_str(), url_two.as_str()),
                (service_three.as_str(), url_three.as_str()),
            ];
            let mut links = Vec::new();
            for (service, url) in values {
                if service.trim().is_empty() && url.trim().is_empty() {
                    continue;
                }
                let valid = url::Url::parse(url.trim())
                    .is_ok_and(|value| matches!(value.scheme(), "http" | "https"));
                if !valid {
                    ui.set_social_status(SharedString::from(format!(
                        "Enter a valid http or https URL for {}.",
                        if service.trim().is_empty() {
                            "the social link"
                        } else {
                            service.trim()
                        }
                    )));
                    return;
                }
                links.push(SocialProfileLink {
                    service: if service.trim().is_empty() {
                        "Social link".to_owned()
                    } else {
                        service.trim().to_owned()
                    },
                    url: url.trim().to_owned(),
                });
            }
            profile_for_links.borrow_mut().links = links;
            match save_social_profile(&profile_path_for_links, &profile_for_links.borrow()) {
                Ok(()) => {
                    ui.set_social_status(SharedString::from("Profile details saved locally."));
                }
                Err(error) => ui.set_social_status(SharedString::from(error.to_string())),
            }
        },
    );

    let weak = ui.as_weak();
    let session_for_message = Rc::clone(&social_session);
    let messages_for_send = Rc::clone(&social_message_state);
    let social_root = paths.root.clone();
    ui.on_social_send_message(move |message| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_message.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in before sending messages."));
            return;
        };
        if message.trim().is_empty() {
            ui.set_social_status(SharedString::from("Write a message first."));
            return;
        }
        let mut state = messages_for_send.borrow_mut();
        let Some(active_id) = state.active_user_id else {
            ui.set_social_status(SharedString::from(
                "Choose a friend or saved conversation first.",
            ));
            return;
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|item| item.user_id == active_id)
        else {
            ui.set_social_status(SharedString::from(
                "The selected conversation is unavailable.",
            ));
            return;
        };
        conversation.messages.push(SavedSocialMessage {
            text: message.trim().to_owned(),
            sent_unix: now,
            is_from_me: true,
        });
        conversation.updated_unix = now;
        if let Err(error) = save_social_messages(
            &social_messages_path(&social_root, session.user_id),
            &state.conversations,
        ) {
            ui.set_social_status(SharedString::from(format!(
                "Message could not be saved: {error}"
            )));
            return;
        }
        refresh_social_message_models(&ui, &state);
        ui.set_social_message(SharedString::default());
        ui.set_social_status(SharedString::from("Message saved on this device."));
    });

    let weak = ui.as_weak();
    let session_for_select = Rc::clone(&social_session);
    let messages_for_select = Rc::clone(&social_message_state);
    let social_root = paths.root.clone();
    ui.on_social_select_friend(move |user_id, username, display_name| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_select.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in before opening a conversation."));
            return;
        };
        let user_id = i64::from(user_id);
        let mut state = messages_for_select.borrow_mut();
        if let Some(conversation) = state
            .conversations
            .iter_mut()
            .find(|item| item.user_id == user_id)
        {
            if !username.is_empty() {
                conversation.username = username.to_string();
            }
            if !display_name.is_empty() {
                conversation.display_name = display_name.to_string();
            }
        } else {
            state.conversations.push(SavedSocialConversation {
                user_id,
                username: username.to_string(),
                display_name: display_name.to_string(),
                updated_unix: SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
                messages: Vec::new(),
            });
        }
        state.active_user_id = Some(user_id);
        let selected = state
            .conversations
            .iter()
            .find(|item| item.user_id == user_id);
        ui.set_social_active_name(SharedString::from(selected.map_or("", |item| {
            if item.display_name.is_empty() {
                item.username.as_str()
            } else {
                item.display_name.as_str()
            }
        })));
        ui.set_social_active_username(SharedString::from(
            selected.map_or("", |item| item.username.as_str()),
        ));
        let _ = save_social_messages(
            &social_messages_path(&social_root, session.user_id),
            &state.conversations,
        );
        refresh_social_message_models(&ui, &state);
        ui.set_social_status(SharedString::from("Conversation ready."));
    });

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let session_for_call = Rc::clone(&social_session);
        let messages_for_call = Rc::clone(&social_message_state);
        let sender_for_call = Rc::clone(&social_call_sender);
        ui.on_social_start_call(move |requested_user_id| {
            let Some(ui) = weak.upgrade() else { return };
            let Some(session) = session_for_call.borrow().clone() else {
                ui.set_call_status(SharedString::from("Sign in before starting a call."));
                return;
            };
            let target_user_id = if requested_user_id > 0 {
                Some(i64::from(requested_user_id))
            } else {
                messages_for_call.borrow().active_user_id
            };
            let Some(target_user_id) = target_user_id else {
                ui.set_call_status(SharedString::from(
                    "Choose a friend before starting a call.",
                ));
                return;
            };
            if target_user_id <= 0 {
                ui.set_call_status(SharedString::from(
                    "That friend does not have a valid Zink ID.",
                ));
                return;
            }
            let queued = sender_for_call.borrow().as_ref().is_some_and(|sender| {
                sender.try_send(SocialCallCommand::Invite(target_user_id)).is_ok()
            });
            if !queued {
                if let Some(sender) = sender_for_call.borrow_mut().take() {
                    let _ = sender.try_send(SocialCallCommand::Shutdown);
                }
                *sender_for_call.borrow_mut() = Some(start_social_call_worker(session));
                if let Some(sender) = sender_for_call.borrow().as_ref() {
                    let _ = sender.try_send(SocialCallCommand::Invite(target_user_id));
                }
            }
            ui.set_call_in_progress(true);
            ui.set_call_status(SharedString::from("Starting secure Zink Call…"));
        });

        let weak = ui.as_weak();
        let sender_for_end = Rc::clone(&social_call_sender);
        let voice_for_end = Rc::clone(&social_voice);
        let share_for_end = Rc::clone(&social_screen_share);
        let video_for_end = Rc::clone(&social_video_receiver);
        let incoming_for_end = Rc::clone(&social_incoming_call);
        ui.on_social_end_call(move || {
            *voice_for_end.borrow_mut() = None;
            *share_for_end.borrow_mut() = None;
            *video_for_end.borrow_mut() = None;
            *incoming_for_end.borrow_mut() = None;
            if let Some(sender) = sender_for_end.borrow().as_ref() {
                let _ = sender.try_send(SocialCallCommand::End);
            }
            if let Some(ui) = weak.upgrade() {
                clear_call_ui(&ui);
                ui.set_call_status(SharedString::from("Call ended."));
            }
        });

        let weak = ui.as_weak();
        let sender_for_answer = Rc::clone(&social_call_sender);
        let incoming_for_answer = Rc::clone(&social_incoming_call);
        ui.on_social_answer_call(move |accept| {
            let Some(ui) = weak.upgrade() else { return };
            let mut incoming = incoming_for_answer.borrow_mut();
            let Some((user_id, call_id)) = incoming.as_ref() else {
                return;
            };
            let user_id = *user_id;
            let command = if accept {
                SocialCallCommand::Accept { user_id, call_id: call_id.clone() }
            } else {
                SocialCallCommand::Decline { user_id, call_id: call_id.clone() }
            };
            let queued = sender_for_answer.borrow().as_ref()
                .is_some_and(|sender| sender.try_send(command).is_ok());
            if !queued {
                ui.set_call_status("Could not send your answer. Try again.".into());
                return;
            }
            *incoming = None;
            drop(incoming);
            if accept {
                let caller = ui.get_call_incoming_name();
                if let Ok(user_id) = i32::try_from(user_id) {
                    ui.invoke_social_select_friend(user_id, SharedString::default(), caller);
                }
                if ui.get_music_fullscreen() {
                    ui.set_music_fullscreen(false);
                    ui.invoke_set_app_fullscreen(false);
                }
                ui.invoke_navigate(SharedString::from("call"));
            }
            ui.set_call_incoming(false);
            ui.set_call_incoming_name(SharedString::default());
            ui.set_call_in_progress(accept);
            ui.set_call_status(SharedString::from(if accept {
                "Joining call…"
            } else {
                "Call declined."
            }));
        });

        let weak = ui.as_weak();
        let session_for_controls = Rc::clone(&social_session);
        let messages_for_controls = Rc::clone(&social_message_state);
        let voice_for_controls = Rc::clone(&social_voice);
        let share_for_controls = Rc::clone(&social_screen_share);
        let video_for_controls = Rc::clone(&social_video_receiver);
        let incoming_for_controls = Rc::clone(&social_incoming_call);
        let sender_for_controls = Rc::clone(&social_call_sender);
        ui.on_social_call_control(move |control, enabled| {
            let Some(ui) = weak.upgrade() else { return };
            match control.as_str() {
                "mute" => {
                    if let Some(voice) = voice_for_controls.borrow().as_ref() {
                        voice.set_muted(enabled || ui.get_call_deafened());
                    }
                    ui.set_call_muted(enabled);
                    ui.set_call_status(SharedString::from(if enabled {
                        "Microphone muted locally."
                    } else {
                        "Microphone unmuted locally."
                    }));
                }
                "deafen" => {
                    if let Some(voice) = voice_for_controls.borrow().as_ref() {
                        voice.set_muted(enabled || ui.get_call_muted());
                        voice.set_deafened(enabled);
                    }
                    ui.set_call_deafened(enabled);
                    ui.set_call_status(SharedString::from(if enabled {
                        "Deafened • incoming audio and microphone paused."
                    } else {
                        "Incoming call audio restored."
                    }));
                }
                "share-audio" => {
                    ui.set_call_share_audio(enabled);
                    ui.set_call_share_audio_error(SharedString::default());
                    if let Some(voice) = voice_for_controls.borrow_mut().as_mut()
                        && let Err(error) = voice.set_screen_audio(enabled && ui.get_call_screen_sharing())
                    {
                        ui.set_call_share_audio_error(error.to_string().into());
                    }
                }
                "share" => {
                    if !ui.get_call_connected() {
                        ui.set_call_status(SharedString::from(
                            "Connect the call before sharing your screen.",
                        ));
                    } else if enabled {
                        let quality = ui.get_call_share_quality().to_string();
                        let result = sender_for_controls
                            .borrow()
                            .as_ref()
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("call signalling is not connected"))
                            .and_then(|sender| {
                                social_screen_share::SocialScreenShare::start(sender, &quality, ui.get_call_peer_hdr())
                            });
                        match result {
                            Ok(share) => {
                                *share_for_controls.borrow_mut() = Some(share);
                                ui.set_call_screen_sharing(true);
                                ui.set_call_status(
                                    format!(
                                        "Starting screen sharing at {quality}…"
                                    )
                                    .into(),
                                );
                            }
                            Err(error) => ui.set_call_status(
                                format!("Screen sharing could not start: {error}").into(),
                            ),
                        }
                    } else {
                        *share_for_controls.borrow_mut() = None;
                        if let Some(voice) = voice_for_controls.borrow_mut().as_mut() {
                            let _ = voice.set_screen_audio(false);
                        }
                        ui.set_call_screen_sharing(false);
                        ui.set_call_send_stats(SharedString::default());
                        ui.set_call_sharing_hdr10(false);
                        ui.set_call_status(SharedString::from("Screen sharing stopped."));
                    }
                }
                "reconnect" => {
                    let Some(session) = session_for_controls.borrow().clone() else {
                        ui.set_call_status(SharedString::from(
                            "Sign in before reconnecting the call.",
                        ));
                        return;
                    };
                    let Some(target_user_id) = messages_for_controls.borrow().active_user_id else {
                        ui.set_call_status(SharedString::from(
                            "Choose a friend before reconnecting the call.",
                        ));
                        return;
                    };
                    if let Some(sender) = sender_for_controls.borrow_mut().take() {
                        let _ = sender.try_send(SocialCallCommand::End);
                        let _ = sender.try_send(SocialCallCommand::Shutdown);
                    }
                    *voice_for_controls.borrow_mut() = None;
                    *share_for_controls.borrow_mut() = None;
                    *video_for_controls.borrow_mut() = None;
                    let sender = start_social_call_worker(session);
                    let _ = sender.try_send(SocialCallCommand::Invite(target_user_id));
                    *sender_for_controls.borrow_mut() = Some(sender);
                    ui.set_call_connected(false);
                    ui.set_call_peer_hdr(false);
                    ui.set_call_sharing_hdr10(false);
                    ui.set_call_viewing_hdr10(false);
                    ui.set_call_in_progress(true);
                    ui.set_call_remote_visible(false);
                    ui.set_call_remote_image(slint::Image::default());
                    ui.set_call_screen_sharing(false);
                    ui.set_call_send_stats(SharedString::default());
                    ui.set_call_view_stats(SharedString::default());
                    ui.set_call_status(SharedString::from(
                        "Reconnecting secure Zink Call…",
                    ));
                }
                "reset" => {
                    if let Some(sender) = sender_for_controls.borrow().as_ref() {
                        let _ = sender.try_send(SocialCallCommand::End);
                    }
                    *voice_for_controls.borrow_mut() = None;
                    *share_for_controls.borrow_mut() = None;
                    *video_for_controls.borrow_mut() = None;
                    *incoming_for_controls.borrow_mut() = None;
                    clear_call_ui(&ui);
                    ui.set_call_muted(false);
                    ui.set_call_deafened(false);
                    ui.set_call_status(SharedString::from(
                        "Call controls reset. Choose Call when you are ready.",
                    ));
                }
                _ => {}
            }
        });

        let weak = ui.as_weak();
        let incoming_for_events = Rc::clone(&social_incoming_call);
        let sender_for_events = Rc::clone(&social_call_sender);
        let voice_for_events = Rc::clone(&social_voice);
        let share_for_events = Rc::clone(&social_screen_share);
        let video_for_events = Rc::clone(&social_video_receiver);
        let session_for_listener = Rc::clone(&social_session);
        let call_event_timer = Timer::default();
        let mut ringing_since = None;
        let mut viewer_rate = social_screen_share::ShareRate::default();
        let mut viewer_rtc = false;
        let mut next_listener_check = Instant::now() + Duration::from_secs(5);
        let handle_call_events = move || {
            let Some(ui) = weak.upgrade() else { return };
            if Instant::now() >= next_listener_check {
                next_listener_check = Instant::now() + Duration::from_secs(5);
                let disconnected = sender_for_events.borrow().as_ref().is_none_or(|sender| {
                    matches!(
                        sender.try_send(SocialCallCommand::Probe),
                        Err(tokio::sync::mpsc::error::TrySendError::Closed(_))
                    )
                });
                if disconnected && let Some(session) = session_for_listener.borrow().clone() {
                    *sender_for_events.borrow_mut() = Some(start_social_call_worker(session));
                }
            }
            if ui.get_call_incoming() {
                let started = ringing_since.get_or_insert_with(|| {
                    play_incoming_call_ringtone();
                    Instant::now()
                });
                if started.elapsed() >= Duration::from_secs(45) {
                    ui.invoke_social_answer_call(false);
                    ui.set_call_status("Missed incoming call.".into());
                }
            } else if ringing_since.take().is_some() {
                stop_incoming_call_ringtone();
            }
            let (events, has_more) = SOCIAL_CALL_EVENTS
                .get_or_init(|| Mutex::new(VecDeque::new()))
                .lock()
                .map(|mut queue| {
                    let count = queue.len().min(32);
                    let events = queue.drain(..count).collect::<Vec<_>>();
                    (events, !queue.is_empty())
                })
                .unwrap_or_default();
            for event in events {
                match event {
                    SocialCallEvent::Status(status) => ui.set_call_status(status.into()),
                    SocialCallEvent::Incoming {
                        user_id,
                        call_id,
                        display_name,
                    } => {
                        if !ui.get_social_signed_in() {
                            continue;
                        }
                        if ui.get_call_incoming() || ui.get_call_in_progress() {
                            if let Some(sender) = sender_for_events.borrow().as_ref() {
                                let _ = sender.try_send(SocialCallCommand::Decline { user_id, call_id });
                            }
                            continue;
                        }
                        *incoming_for_events.borrow_mut() = Some((user_id, call_id));
                        ui.set_call_incoming(true);
                        ui.set_call_incoming_name(display_name.clone().into());
                        ui.set_call_status(format!("Incoming call from {display_name}").into());
                        if ui.window().is_minimized() {
                            ui.window().set_minimized(false);
                        }
                        if ui.get_notifications_social()
                            && let Err(error) = show_windows_toast("Incoming Zink call", &display_name)
                        {
                            tracing::debug!(%error, "Windows could not show the incoming call toast");
                        }
                    }
                    SocialCallEvent::Connected => {
                        ui.set_call_in_progress(true);
                        ui.set_call_connected(true);
                        if let Some(sender) = sender_for_events.borrow().as_ref() {
                            let hdr = call_owner_window(&ui).is_some_and(social_hdr_view::supports_display)
                                && social_video_decoder::hdr_decoder_available();
                            let _ = sender.try_send(SocialCallCommand::ScreenCapabilities(hdr));
                        }
                        let voice_result = sender_for_events
                            .borrow()
                            .as_ref()
                            .cloned()
                            .ok_or_else(|| anyhow::anyhow!("call signalling is not connected"))
                            .and_then(|sender| {
                                social_voice::SocialVoiceEngine::start(
                                    sender,
                                    non_empty_str(ui.get_call_input_device_name().as_str()),
                                    non_empty_str(ui.get_call_output_device_name().as_str()),
                                    ui.get_call_muted(),
                                    ui.get_call_deafened(),
                                )
                            });
                        match voice_result {
                            Ok(voice) => {
                                *voice_for_events.borrow_mut() = Some(voice);
                                ui.set_call_status(SharedString::from(
                                    "Connected • native WASAPI voice active",
                                ));
                            }
                            Err(error) => ui.set_call_status(
                                format!("Connected, but voice could not start: {error}").into(),
                            ),
                        }
                        if ui.get_call_auto_share() {
                            let quality = ui.get_call_share_quality().to_string();
                            let share_result = sender_for_events
                                .borrow()
                                .as_ref()
                                .cloned()
                                .ok_or_else(|| anyhow::anyhow!("call signalling is not connected"))
                                .and_then(|sender| {
                                    social_screen_share::SocialScreenShare::start(sender, &quality, ui.get_call_peer_hdr())
                                });
                            match share_result {
                                Ok(share) => {
                                    *share_for_events.borrow_mut() = Some(share);
                                    ui.set_call_screen_sharing(true);
                                    ui.set_call_status(
                                        format!("Connected • sharing display 1 at {quality}").into(),
                                    );
                                }
                                Err(error) => ui.set_call_status(
                                    format!("Connected, but screen sharing could not start: {error}")
                                        .into(),
                                ),
                            }
                            ui.set_call_auto_share(false);
                        }
                    }
                    SocialCallEvent::Audio(packet) => {
                        if !ui.get_call_deafened()
                            && let Some(voice) = voice_for_events.borrow_mut().as_mut()
                            && let Err(error) = voice.play_packet(&packet)
                        {
                            ui.set_call_status(
                                format!("Connected, but incoming audio failed: {error}").into(),
                            );
                        }
                    }
                    SocialCallEvent::RemoteScreenStarted { width, height, hdr10, av1, rtc } => {
                        if !ui.get_call_connected() {
                            continue;
                        }
                        *video_for_events.borrow_mut() = None;
                        viewer_rate = Default::default();
                        viewer_rtc = rtc;
                        ui.set_call_view_stats(SharedString::default());
                        if hdr10 || !ui.get_call_remote_visible() {
                            ui.set_call_remote_visible(false);
                            ui.set_call_remote_image(slint::Image::default());
                        }
                        ui.set_call_viewing_hdr10(false);
                        let result = if hdr10 {
                            call_owner_window(&ui)
                                .filter(|&owner| social_hdr_view::supports_display(owner))
                                .context("Windows HDR is disabled on the viewing display")
                                .and_then(|owner| social_video_decoder::SocialVideoReceiver::start_hdr10(width, height, owner))
                        } else if av1 {
                            social_video_decoder::SocialVideoReceiver::start_av1(width, height)
                        } else {
                            social_video_decoder::SocialVideoReceiver::start(width, height)
                        };
                        match result {
                            Ok(receiver) => {
                                *video_for_events.borrow_mut() = Some(receiver);
                                ui.set_call_status(SharedString::from("Receiving the remote screen…"));
                            }
                            Err(error) if hdr10 => push_social_call_event(SocialCallEvent::HdrReceiveFailed(error.to_string())),
                            Err(error) if av1 => push_social_call_event(SocialCallEvent::Av1ReceiveFailed(error.to_string())),
                            Err(error) => ui.set_call_status(format!("Remote screen decoder could not start: {error}").into()),
                        }
                    }
                    SocialCallEvent::RemoteScreenPacket { data, timestamp_ms, received } => {
                        if let Some(receiver) = video_for_events.borrow().as_ref() {
                            receiver.submit(data, timestamp_ms, received);
                        }
                    }
                    SocialCallEvent::RemoteScreenStopped => {
                        *video_for_events.borrow_mut() = None;
                        ui.set_call_view_stats(SharedString::default());
                        ui.set_call_remote_visible(false);
                        ui.set_call_viewing_hdr10(false);
                        ui.set_call_remote_image(slint::Image::default());
                        if ui.get_call_connected() {
                            ui.set_call_status(SharedString::from("Remote screen sharing stopped."));
                        }
                    }
                    SocialCallEvent::ScreenShareFailed(error) => {
                        *share_for_events.borrow_mut() = None;
                        if let Some(voice) = voice_for_events.borrow_mut().as_mut() {
                            let _ = voice.set_screen_audio(false);
                        }
                        ui.set_call_send_stats(SharedString::default());
                        ui.set_call_screen_sharing(false);
                        ui.set_call_sharing_hdr10(false);
                        if ui.get_call_connected() {
                            ui.set_call_status(format!("Screen sharing stopped: {error}").into());
                        }
                    }
                    SocialCallEvent::ScreenAudioFailed(error) => {
                        if ui.get_call_screen_sharing() && ui.get_call_share_audio() {
                            if let Some(voice) = voice_for_events.borrow_mut().as_mut() {
                                let _ = voice.set_screen_audio(false);
                            }
                            ui.set_call_share_audio_error(error.into());
                        }
                    }
                    SocialCallEvent::ScreenShareMode { hdr10, av1 } => {
                        if ui.get_call_connected() && ui.get_call_screen_sharing() {
                            ui.set_call_share_audio_error(SharedString::default());
                            if let Some(voice) = voice_for_events.borrow_mut().as_mut()
                                && let Err(error) = voice.set_screen_audio(ui.get_call_share_audio())
                            {
                                ui.set_call_share_audio_error(error.to_string().into());
                            }
                            ui.set_call_sharing_hdr10(hdr10);
                            let mode = if hdr10 { "HDR10 • HEVC Main10" } else if av1 { "SDR • AV1" } else { "SDR • H.264" };
                            ui.set_call_status(format!("Sharing display 1 at {} • {mode}", ui.get_call_share_quality()).into());
                        }
                    }
                    SocialCallEvent::PeerHdrAvailable(supported) => ui.set_call_peer_hdr(supported),
                    SocialCallEvent::Av1ReceiveFailed(error) => {
                        tracing::info!(%error, "AV1 viewer requested H.264");
                        *video_for_events.borrow_mut() = None;
                        if let Some(sender) = sender_for_events.borrow().as_ref() {
                            let _ = sender.try_send(SocialCallCommand::ScreenRequestH264);
                        }
                        ui.set_call_status("Switching the remote screen to H.264…".into());
                    }
                    SocialCallEvent::PeerRequestsSdr => {
                        ui.set_call_peer_hdr(false);
                        if let Some(share) = share_for_events.borrow().as_ref() { share.fallback_to_sdr(); }
                    }
                    SocialCallEvent::HdrReceiveFailed(error) => {
                        if video_for_events.borrow().as_ref().is_some_and(|receiver| !receiver.is_hdr10()) {
                            continue;
                        }
                        *video_for_events.borrow_mut() = None;
                        ui.set_call_view_stats(SharedString::default());
                        ui.set_call_viewing_hdr10(false);
                        if ui.get_call_connected() {
                            tracing::info!(%error, "HDR10 receiver requesting SDR");
                            ui.set_call_status("HDR10 unavailable. Switching to SDR…".into());
                            if let Some(sender) = sender_for_events.borrow().as_ref() {
                                let _ = sender.try_send(SocialCallCommand::ScreenRequestSdr);
                            }
                        }
                    }
                    SocialCallEvent::HdrPresented => {
                        if ui.get_call_connected()
                            && video_for_events.borrow().as_ref().is_some_and(social_video_decoder::SocialVideoReceiver::is_hdr10)
                        {
                            ui.set_call_viewing_hdr10(true);
                            ui.set_call_status("Watching remote screen • HDR10".into());
                        }
                    }
                    SocialCallEvent::HdrShareRate(fps) => {
                        if ui.get_call_viewing_hdr10() {
                            ui.set_call_view_stats(format!("Viewing {fps:.1} FPS").into());
                        }
                    }
                    SocialCallEvent::SentShareRate { fps, mbps, rtc, av1 } => {
                        if ui.get_call_screen_sharing() {
                            let transport = if rtc && av1 { "WebRTC / AV1" } else if rtc { "WebRTC / H.264" } else { "Relay" };
                            ui.set_call_send_stats(format!("{transport} • {fps:.1} FPS • {mbps:.1} Mbps").into());
                        }
                    }
                    SocialCallEvent::RemoteFrame {
                        rgba,
                        width,
                        height,
                    } => {
                        if !ui.get_call_connected() || video_for_events.borrow().is_none() {
                            continue;
                        }
                        let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                            &rgba, width, height,
                        );
                        ui.set_call_remote_image(slint::Image::from_rgba8(pixels));
                        ui.set_call_remote_visible(true);
                        if let Some((fps, _)) = viewer_rate.frame(0) {
                            let transport = if viewer_rtc { "WebRTC" } else { "Relay" };
                            ui.set_call_view_stats(format!("{transport} • {width}×{height} • {fps:.1} FPS").into());
                        }
                        let av1 = video_for_events.borrow().as_ref().is_some_and(|receiver| receiver.is_av1());
                        ui.set_call_status(if av1 { "Watching remote screen • AV1" } else { "Watching remote screen • H.264" }.into());
                    }
                    SocialCallEvent::Ended(status) | SocialCallEvent::Failed(status) => {
                        *incoming_for_events.borrow_mut() = None;
                        *voice_for_events.borrow_mut() = None;
                        *share_for_events.borrow_mut() = None;
                        *video_for_events.borrow_mut() = None;
                        clear_call_ui(&ui);
                        if status == "Call ended." && ui.get_notifications_social()
                            && ui.get_current_route() != "call"
                        {
                            let _ = show_windows_toast("Zink Call", &status);
                        }
                        ui.set_call_status(status.into());
                    }
                }
            }
            if has_more {
                let _ = slint::invoke_from_event_loop(dispatch_social_call_events);
            }
        };
        SOCIAL_CALL_EVENT_HANDLER.with(|handler| {
            *handler.borrow_mut() = Some(Box::new(handle_call_events));
        });
        // Incoming call timeouts and listener checks still need a clock. Video
        // and signalling wake the UI as soon as an event is queued.
        call_event_timer.start(TimerMode::Repeated, Duration::from_millis(250), dispatch_social_call_events);
        dispatch_social_call_events();
        std::mem::forget(call_event_timer);
    }

    let weak = ui.as_weak();
    let session_for_search = Rc::clone(&social_session);
    ui.on_social_search_friends(move |query| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_search.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in before searching for people."));
            return;
        };
        if query.trim().is_empty() {
            ui.set_social_status(SharedString::from("Enter a username or display name."));
            return;
        }
        ui.set_social_status(SharedString::from("Searching Zink Social…"));
        let query = query.to_string();
        std::thread::spawn(move || {
            push_social_result(SocialResult::Search(
                social_search(&session, &query).map_err(|error| error.to_string()),
            ));
        });
    });

    let weak = ui.as_weak();
    let session_for_refresh = Rc::clone(&social_session);
    ui.on_social_refresh(move || {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_refresh.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in to refresh Zink Social."));
            return;
        };
        ui.set_social_status(SharedString::from("Refreshing friends and requests…"));
        std::thread::spawn(move || {
            push_social_result(SocialResult::Friends(
                social_load_people(&session).map_err(|error| error.to_string()),
            ));
        });
    });

    let weak = ui.as_weak();
    let session_for_add = Rc::clone(&social_session);
    ui.on_social_add_friend(move |username| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_add.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in before adding a friend."));
            return;
        };
        ui.set_social_status(SharedString::from(format!("Adding @{}…", username.trim())));
        let username = username.to_string();
        std::thread::spawn(move || {
            let result = social_post(
                &session,
                "/api/friends/requests",
                Some(serde_json::json!({ "Username": username.trim() })),
            )
            .map(|()| format!("Friend request sent to @{}.", username.trim()))
            .map_err(|error| error.to_string());
            push_social_result(SocialResult::Action(result));
        });
    });

    let weak = ui.as_weak();
    let session_for_response = Rc::clone(&social_session);
    ui.on_social_respond_request(move |request_id, accept| {
        let Some(ui) = weak.upgrade() else { return };
        let Some(session) = session_for_response.borrow().clone() else {
            ui.set_social_status(SharedString::from("Sign in before responding to requests."));
            return;
        };
        ui.set_social_status(SharedString::from(if accept {
            "Accepting friend request…"
        } else {
            "Declining friend request…"
        }));
        std::thread::spawn(move || {
            let action = if accept { "accept" } else { "decline" };
            let result = social_post(
                &session,
                &format!("/api/friends/requests/{request_id}/{action}"),
                None,
            )
            .and_then(|()| social_load_people(&session))
            .map_err(|error| error.to_string());
            push_social_result(SocialResult::Friends(result));
        });
    });

    // Poll on every page so a signed-in recipient can see new requests while
    // using other parts of Zink.
    let social_presence_timer = Timer::default();
    let session_for_presence = Rc::clone(&social_session);
    social_presence_timer.start(TimerMode::Repeated, Duration::from_secs(15), move || {
        let Some(session) = session_for_presence.borrow().clone() else {
            return;
        };
        std::thread::spawn(move || {
            push_social_result(SocialResult::Friends(
                social_load_people(&session).map_err(|error| error.to_string()),
            ));
        });
    });

    let social_result_timer = Timer::default();
    let weak = ui.as_weak();
    let session_for_results = Rc::clone(&social_session);
    let session_path_for_results = social_session_path.clone();
    let messages_for_results = Rc::clone(&social_message_state);
    let profile_for_results = Rc::clone(&social_profile);
    let profile_path_for_results = social_profile_path.clone();
    #[cfg(windows)]
    let call_sender_for_results = Rc::clone(&social_call_sender);
    let social_root = paths.root.clone();
    social_result_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
        let result = SOCIAL_RESULTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
            .ok()
            .and_then(|mut results| results.pop());
        let Some(result) = result else { return };
        let Some(ui) = weak.upgrade() else { return };
        match result {
            SocialResult::Auth(Ok(session)) => {
                ui.set_social_signed_in(true);
                ui.set_social_username(SharedString::from(session.username.clone()));
                ui.set_social_display_name(SharedString::from(session.display_name.clone()));
                ui.set_social_password(SharedString::default());
                ui.set_social_status(SharedString::from("Signed in. Loading your Zink circle…"));
                *session_for_results.borrow_mut() = Some(session.clone());
                let profile = load_social_profile(&profile_path_for_results)
                    .filter(|profile| profile.user_id == session.user_id)
                    .unwrap_or_else(|| SocialProfileDetails {
                        user_id: session.user_id,
                        signed_up: "Not available".to_owned(),
                        ..SocialProfileDetails::default()
                    });
                *profile_for_results.borrow_mut() = profile;
                apply_social_profile_to_ui(&ui, &profile_for_results.borrow());
                #[cfg(windows)]
                {
                    if let Some(sender) = call_sender_for_results.borrow_mut().take() {
                        let _ = sender.try_send(SocialCallCommand::Shutdown);
                    }
                    *call_sender_for_results.borrow_mut() =
                        Some(start_social_call_worker(session.clone()));
                }
                {
                    let mut message_state = messages_for_results.borrow_mut();
                    message_state.conversations =
                        load_social_messages(&social_messages_path(&social_root, session.user_id));
                    message_state.active_user_id = None;
                    refresh_social_message_models(&ui, &message_state);
                }
                ui.set_social_active_name(SharedString::default());
                ui.set_social_active_username(SharedString::default());
                ui.invoke_navigate(SharedString::from("friends"));
                std::thread::spawn(move || {
                    push_social_result(SocialResult::Friends(
                        social_load_people(&session).map_err(|error| error.to_string()),
                    ));
                });
            }
            SocialResult::Auth(Err(error)) => {
                ui.set_social_signed_in(false);
                ui.set_social_status(SharedString::from(format!("Sign in failed: {error}")));
            }
            SocialResult::Friends(Ok((friends, requests))) => {
                if !ui.get_social_signed_in() {
                    return;
                }
                let previous = ui.get_social_requests();
                let new_requests = requests.iter().filter(|request| {
                    !previous.iter().any(|old| i64::from(old.request_id) == request.request_id)
                }).collect::<Vec<_>>();
                if ui.get_notifications_social() && let Some(latest) = new_requests.first() {
                    let message = if new_requests.len() == 1 {
                        format!("@{} sent you a friend request.", latest.from_username)
                    } else {
                        format!("You have {} new friend requests.", new_requests.len())
                    };
                    ui.set_friend_request_alert(message.clone().into());
                    if let Err(error) = show_windows_toast("Zink friend request", &message) {
                        tracing::debug!(%error, "Windows could not show the friend request toast");
                    }
                }
                let online = friends.iter().filter(|friend| friend.is_online).count();
                ui.set_social_online_count(i32::try_from(online).unwrap_or(i32::MAX));
                ui.set_social_friends(ModelRc::from(Rc::new(VecModel::from(
                    friends
                        .into_iter()
                        .map(social_friend_item)
                        .collect::<Vec<_>>(),
                ))));
                ui.set_social_requests(ModelRc::from(Rc::new(VecModel::from(
                    requests
                        .into_iter()
                        .map(social_request_item)
                        .collect::<Vec<_>>(),
                ))));
                ui.set_social_status(SharedString::from("Zink Social is up to date."));
            }
            SocialResult::Friends(Err(error)) => ui.set_social_status(SharedString::from(format!(
                "Could not refresh Social: {error}"
            ))),
            SocialResult::Search(Ok(people)) => {
                let count = people.len();
                let friends = ui.get_social_friends();
                ui.set_social_search_results(ModelRc::from(Rc::new(VecModel::from(
                    people
                        .into_iter()
                        .map(social_friend_item)
                        .map(|mut person| {
                            person.is_friend |= friends.iter().any(|friend| friend.user_id == person.user_id);
                            person
                        })
                        .collect::<Vec<_>>(),
                ))));
                ui.set_social_status(SharedString::from(format!(
                    "Found {count} matching Zink Social account(s)."
                )));
            }
            SocialResult::Search(Err(error)) => {
                ui.set_social_status(SharedString::from(format!("Search failed: {error}")));
            }
            SocialResult::Action(Ok(status)) => ui.set_social_status(SharedString::from(status)),
            SocialResult::Action(Err(error)) => {
                ui.set_social_status(SharedString::from(format!("Social action failed: {error}")));
            }
            SocialResult::Profile(user_id, result) => {
                let mut current = session_for_results.borrow_mut();
                let Some(session) = current.as_mut().filter(|session| session.user_id == user_id) else { return };
                match result {
                    Ok(name) => {
                        session.display_name = name.clone();
                        ui.set_social_display_name(SharedString::from(name));
                        if let Err(error) = social_save_session(&session_path_for_results, session) {
                            error!(%error, "failed to cache the updated display name");
                        }
                        ui.set_social_status(SharedString::from("Display name saved to your account."));
                    }
                    Err(error) => ui.set_social_status(SharedString::from(format!("Could not save display name: {error}"))),
                }
            }
        }
    });

    let call_duration_timer = Timer::default();
    let weak = ui.as_weak();
    let mut call_started = None::<Instant>;
    call_duration_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_call_in_progress() {
            let started = call_started.get_or_insert_with(Instant::now);
            let elapsed = started.elapsed().as_secs();
            ui.set_call_duration(SharedString::from(format!(
                "{:02}:{:02}",
                elapsed / 60,
                elapsed % 60
            )));
        } else {
            call_started = None;
            ui.set_call_duration(SharedString::from("00:00"));
        }
    });

    #[cfg(windows)]
    let stream_pipeline = Arc::new(Mutex::new(None::<win_native_media::Pipeline>));

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let preferences_for_twitch_save = Rc::clone(&streaming_preferences);
        let preferences_path_for_twitch_save = streaming_preferences_path.clone();
        ui.on_save_twitch_streaming_settings(move |channel, client_id, access_token| {
            let mut preferences = preferences_for_twitch_save.borrow_mut();
            channel
                .trim()
                .trim_start_matches('@')
                .clone_into(&mut preferences.twitch_channel);
            client_id
                .trim()
                .clone_into(&mut preferences.twitch_client_id);
            access_token
                .trim()
                .clone_into(&mut preferences.twitch_access_token);
            let result =
                save_streaming_preferences(&preferences_path_for_twitch_save, &preferences);
            if let Some(ui) = weak.upgrade() {
                ui.set_stream_twitch_status(SharedString::from(if result.is_ok() {
                    "Twitch account settings saved locally."
                } else {
                    "Twitch settings could not be saved."
                }));
            }
        });

        let weak = ui.as_weak();
        let preferences_for_twitch_refresh = Rc::clone(&streaming_preferences);
        let preferences_path_for_twitch_refresh = streaming_preferences_path.clone();
        ui.on_refresh_twitch_telemetry(move || {
            let Some(ui) = weak.upgrade() else { return };
            let channel = ui
                .get_stream_twitch_channel()
                .trim()
                .trim_start_matches('@')
                .to_owned();
            let client_id = ui.get_stream_twitch_client_id().trim().to_owned();
            let access_token = ui.get_stream_twitch_access_token().trim().to_owned();
            if channel.is_empty() || client_id.is_empty() || access_token.is_empty() {
                ui.set_stream_twitch_status(SharedString::from(
                    "Enter the Twitch channel, Client ID and access token first.",
                ));
                return;
            }
            {
                let mut preferences = preferences_for_twitch_refresh.borrow_mut();
                preferences.twitch_channel.clone_from(&channel);
                preferences.twitch_client_id.clone_from(&client_id);
                preferences.twitch_access_token.clone_from(&access_token);
                if let Err(error) =
                    save_streaming_preferences(&preferences_path_for_twitch_refresh, &preferences)
                {
                    error!(%error, "failed to save Twitch telemetry settings");
                }
            }
            ui.set_stream_twitch_busy(true);
            ui.set_stream_twitch_status(SharedString::from("Checking Twitch channel…"));
            std::thread::spawn(move || {
                let result = refresh_twitch_telemetry(&channel, &client_id, &access_token)
                    .unwrap_or_else(|error| TwitchTelemetryResult {
                        viewers: 0,
                        is_live: false,
                        status: format!("Twitch status failed: {error}"),
                    });
                if let Ok(mut results) = TWITCH_TELEMETRY_RESULTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                {
                    results.push(result);
                }
            });
        });

        let weak = ui.as_weak();
        let preferences_for_twitch_disconnect = Rc::clone(&streaming_preferences);
        let preferences_path_for_twitch_disconnect = streaming_preferences_path.clone();
        ui.on_disconnect_twitch_streaming(move || {
            let mut preferences = preferences_for_twitch_disconnect.borrow_mut();
            preferences.twitch_channel.clear();
            preferences.twitch_access_token.clear();
            let _ =
                save_streaming_preferences(&preferences_path_for_twitch_disconnect, &preferences);
            if let Some(ui) = weak.upgrade() {
                ui.set_stream_twitch_channel(SharedString::default());
                ui.set_stream_twitch_access_token(SharedString::default());
                ui.set_stream_twitch_viewers(0);
                ui.set_stream_twitch_live(false);
                ui.set_stream_twitch_busy(false);
                ui.set_stream_twitch_status(SharedString::from("Twitch disconnected."));
            }
        });

        let weak = ui.as_weak();
        let twitch_result_timer = Timer::default();
        twitch_result_timer.start(TimerMode::Repeated, Duration::from_millis(200), move || {
            let result = TWITCH_TELEMETRY_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .ok()
                .and_then(|mut results| results.pop());
            let Some(result) = result else { return };
            let Some(ui) = weak.upgrade() else { return };
            ui.set_stream_twitch_viewers(result.viewers);
            ui.set_stream_twitch_live(result.is_live);
            ui.set_stream_twitch_busy(false);
            ui.set_stream_twitch_status(SharedString::from(result.status));
        });

        let weak = ui.as_weak();
        let stream_preview_timer = Timer::default();
        stream_preview_timer.start(TimerMode::Repeated, Duration::from_millis(800), move || {
            let Some(ui) = weak.upgrade() else { return };
            if let Some(result) = STREAM_PREVIEW_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .ok()
                .and_then(|mut results| results.pop())
                && result.source_pid == ui.get_stream_selected_pid()
            {
                if let Some(error) = result.error {
                    ui.set_stream_preview_available(false);
                    ui.set_stream_preview_status(SharedString::from(error));
                } else {
                    let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                        &result.rgba,
                        result.width,
                        result.height,
                    );
                    ui.set_stream_preview_image(slint::Image::from_rgba8(pixels));
                    ui.set_stream_preview_available(true);
                    ui.set_stream_preview_status(SharedString::from(
                        "Live Windows capture preview",
                    ));
                }
            }
            let route = ui.get_current_route();
            let preview_visible = matches!(
                route.as_str(),
                "twitch-streaming"
                    | "youtube-streaming"
                    | "kick-streaming"
                    | "instagram-streaming"
                    | "tiktok-streaming"
                    | "facebook-streaming"
                    | "x-streaming"
            );
            if !preview_visible {
                ui.set_stream_preview_available(false);
                ui.set_stream_preview_image(slint::Image::default());
                if let Ok(mut results) = STREAM_PREVIEW_RESULTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                {
                    results.clear();
                }
                return;
            }
            if STREAM_PREVIEW_BUSY
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                return;
            }
            let source_pid = ui.get_stream_selected_pid();
            std::thread::spawn(move || {
                let result = match capture_stream_preview(source_pid) {
                    Ok((rgba, width, height)) => StreamPreviewResult {
                        rgba,
                        width,
                        height,
                        source_pid,
                        error: None,
                    },
                    Err(error) => StreamPreviewResult {
                        rgba: Vec::new(),
                        width: 0,
                        height: 0,
                        source_pid,
                        error: Some(error),
                    },
                };
                if let Ok(mut results) = STREAM_PREVIEW_RESULTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                {
                    results.clear();
                    results.push(result);
                }
                STREAM_PREVIEW_BUSY.store(false, Ordering::Release);
            });
        });

        let weak = ui.as_weak();
        let start_pipeline = Arc::clone(&stream_pipeline);
        let preferences_for_stream = Rc::clone(&streaming_preferences);
        let preferences_path_for_stream = streaming_preferences_path.clone();
        ui.on_configure_stream_destination(move |service, server, stream_key| {
            let Some(ui) = weak.upgrade() else { return };
            let server = server.trim().to_owned();
            let stream_key = stream_key.trim().to_owned();
            if server.is_empty() || stream_key.is_empty() {
                ui.set_stream_status(SharedString::from(
                    "Enter the provider RTMP server and stream key before starting.",
                ));
                return;
            }
            if !server.starts_with("rtmp://") && !server.starts_with("rtmps://") {
                ui.set_stream_status(SharedString::from(
                    "The server must use an rtmp:// or rtmps:// address.",
                ));
                return;
            }
            if start_pipeline.lock().is_ok_and(|pipeline| pipeline.is_some()) {
                ui.set_stream_status(SharedString::from("A stream is already running."));
                return;
            }

            {
                let mut preferences = preferences_for_stream.borrow_mut();
                preferences.destinations.insert(
                    service.to_string(),
                    StreamingDestination {
                        server: server.clone(),
                        stream_key: stream_key.clone(),
                    },
                );
                preferences.quality = ui.get_stream_quality().to_string();
                preferences.system_audio = ui.get_stream_system_audio();
                preferences.microphone = ui.get_stream_microphone();
                preferences.desktop_gain = ui.get_stream_desktop_volume();
                preferences.microphone_gain = ui.get_stream_microphone_volume();
                preferences.low_latency = ui.get_stream_low_latency();
                if let Err(error) =
                    save_streaming_preferences(&preferences_path_for_stream, &preferences)
                {
                    error!(%error, "failed to save streaming preferences");
                }
            }

            let mut video = streaming_video_config(ui.get_stream_quality().as_str());
            if ui.get_stream_low_latency() {
                video.keyframe_interval = video.fps;
            }
            let audio = if ui.get_stream_system_audio() || ui.get_stream_microphone() {
                Some(win_native_media::AudioConfig {
                    loopback: ui.get_stream_system_audio(),
                    microphone: ui.get_stream_microphone(),
                    loopback_device_id: non_empty_device_id(
                        ui.get_recorder_output_device_id().as_str(),
                    ),
                    microphone_device_id: non_empty_device_id(
                        ui.get_recorder_input_device_id().as_str(),
                    ),
                    loopback_gain: ui.get_stream_desktop_volume(),
                    microphone_gain: ui.get_stream_microphone_volume(),
                    bitrate: 192_000,
                })
            } else {
                None
            };
            let selected_pid = u32::try_from(ui.get_stream_selected_pid()).unwrap_or(0);
            let selected_target = ui.get_stream_selected_target().to_string();
            let capture_target = if selected_pid == 0 {
                win_native_media::CaptureTarget::Monitor(0)
            } else if let Some(hwnd) = window_handle_for_pid(selected_pid) {
                win_native_media::CaptureTarget::Window(hwnd)
            } else {
                ui.set_stream_selected_pid(0);
                ui.set_stream_selected_target(SharedString::from("Primary display"));
                ui.set_stream_status(SharedString::from(
                    "The selected window closed. Choose another source before going live.",
                ));
                return;
            };
            let service = service.to_string();
            let pipeline_slot = Arc::clone(&start_pipeline);
            ui.set_stream_busy(true);
            ui.set_stream_status(SharedString::from(format!(
                "Starting {service}: capturing {selected_target}, native H.264 and encrypted RTMP publishing…"
            )));
            std::thread::spawn(move || {
                let result = (|| -> Result<win_native_media::Pipeline, String> {
                    let mut pipeline = win_native_media::Pipeline::new(
                        win_native_media::PipelineConfig {
                            capture_target,
                            capture_cursor: true,
                            video,
                            record: None,
                            stream: Some(win_native_media::StreamConfig {
                                url: server,
                                stream_key,
                            }),
                            audio,
                        },
                    )
                    .map_err(|error| error.to_string())?;
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| error.to_string())?;
                    runtime
                        .block_on(pipeline.start())
                        .map_err(|error| error.to_string())?;
                    Ok(pipeline)
                })();

                let status = match result {
                    Ok(pipeline) => match pipeline_slot.lock() {
                        Ok(mut slot) => {
                            *slot = Some(pipeline);
                            Ok(selected_target)
                        }
                        Err(error) => Err(format!("stream state lock failed: {error}")),
                    },
                    Err(error) => Err(error),
                };
                if let Ok(mut results) = STREAM_PIPELINE_RESULTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                {
                    results.push(StreamPipelineResult::Started(status));
                }
            });
        });

        let weak = ui.as_weak();
        let stop_pipeline = Arc::clone(&stream_pipeline);
        ui.on_stop_stream(move || {
            let Some(ui) = weak.upgrade() else { return };
            let pipeline = stop_pipeline.lock().ok().and_then(|mut slot| slot.take());
            let Some(mut pipeline) = pipeline else {
                ui.set_stream_live(false);
                ui.set_stream_status(SharedString::from("No native stream is running."));
                return;
            };
            ui.set_stream_busy(true);
            ui.set_stream_status(SharedString::from(
                "Stopping and draining the native stream…",
            ));
            std::thread::spawn(move || {
                let result = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| error.to_string())
                    .and_then(|runtime| {
                        runtime
                            .block_on(pipeline.stop())
                            .map_err(|error| error.to_string())
                    });
                if let Ok(mut results) = STREAM_PIPELINE_RESULTS
                    .get_or_init(|| Mutex::new(Vec::new()))
                    .lock()
                {
                    results.push(StreamPipelineResult::Stopped(result));
                }
            });
        });

        let weak = ui.as_weak();
        let stream_result_timer = Timer::default();
        stream_result_timer.start(TimerMode::Repeated, Duration::from_millis(150), move || {
            let Some(ui) = weak.upgrade() else { return };
            let results = STREAM_PIPELINE_RESULTS
                .get_or_init(|| Mutex::new(Vec::new()))
                .lock()
                .map(|mut results| std::mem::take(&mut *results))
                .unwrap_or_default();
            for result in results {
                match result {
                    StreamPipelineResult::Started(Ok(target)) => {
                        ui.set_stream_busy(false);
                        ui.set_stream_live(true);
                        ui.set_stream_status(SharedString::from(
                            format!("Native stream pipeline started: {target} → Media Foundation H.264 → RTMP/RTMPS."),
                        ));
                    }
                    StreamPipelineResult::Started(Err(error)) => {
                        ui.set_stream_busy(false);
                        ui.set_stream_live(false);
                        ui.set_stream_status(SharedString::from(format!(
                            "Could not start the native stream: {error}"
                        )));
                    }
                    StreamPipelineResult::Stopped(Ok(())) => {
                        ui.set_stream_busy(false);
                        ui.set_stream_live(false);
                        ui.set_stream_status(SharedString::from("Stream stopped cleanly."));
                    }
                    StreamPipelineResult::Stopped(Err(error)) => {
                        ui.set_stream_busy(false);
                        ui.set_stream_live(false);
                        ui.set_stream_status(SharedString::from(format!(
                            "The stream stopped with an error: {error}"
                        )));
                    }
                }
            }
        });

        let weak = ui.as_weak();
        let stream_pipeline = Arc::clone(&stream_pipeline);
        let stream_stats_timer = Timer::default();
        stream_stats_timer.start(TimerMode::Repeated, Duration::from_secs(1), move || {
            let Some(ui) = weak.upgrade() else { return };
            let stats = stream_pipeline
                .lock()
                .ok()
                .and_then(|slot| slot.as_ref().map(win_native_media::Pipeline::stats));
            if let Some(stats) = stats {
                ui.set_stream_measured_fps(SharedString::from(format!(
                    "{:.1}",
                    stats.encoded_fps()
                )));
                ui.set_stream_measured_rate(SharedString::from(format!(
                    "{:.1} Mbps",
                    stats.video_mbps()
                )));
                ui.set_stream_health(SharedString::from(if stats.dropped_stream_frames == 0 {
                    "GOOD".to_owned()
                } else {
                    format!("{} DROPPED", stats.dropped_stream_frames)
                }));
            } else {
                ui.set_stream_measured_fps(SharedString::from("--"));
                ui.set_stream_measured_rate(SharedString::from("--"));
                ui.set_stream_health(SharedString::from("READY"));
            }
        });
    }

    #[cfg(not(windows))]
    {
        let weak = ui.as_weak();
        ui.on_configure_stream_destination(move |_, _, _| {
            if let Some(ui) = weak.upgrade() {
                ui.set_stream_status(SharedString::from(
                    "Native streaming requires Windows 10 or Windows 11.",
                ));
            }
        });
        let weak = ui.as_weak();
        ui.on_stop_stream(move || {
            if let Some(ui) = weak.upgrade() {
                ui.set_stream_live(false);
            }
        });
    }

    let weak = ui.as_weak();
    ui.on_open_external_link(move |url| {
        let result = open_external_url(url.as_str()).or_else(|primary_error| {
            let Some(fallback) = store_web_fallback(url.as_str()) else {
                return Err(primary_error);
            };
            open_external_url(fallback).with_context(|| {
                format!("Microsoft Store and web fallback both failed: {primary_error}")
            })
        });
        if let Err(error) = result {
            error!(%error, %url, "failed to open external link");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_submit_feedback(move |name, email, message| {
        let Some(ui) = weak.upgrade() else { return };
        if message.trim().is_empty() {
            ui.set_feedback_status(SharedString::from("Please enter your feedback first."));
            return;
        }
        let body = format!(
            "From: {} ({})\n\n{}",
            name.trim(),
            email.trim(),
            message.trim()
        );
        let query = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("subject", "Zink Feedback")
            .append_pair("body", &body)
            .finish();
        let mailto = format!("mailto:francehayle@outlook.com?{query}");
        match open_external_url(&mailto) {
            Ok(()) => {
                ui.set_feedback_name(SharedString::default());
                ui.set_feedback_email(SharedString::default());
                ui.set_feedback_text(SharedString::default());
                ui.set_feedback_status(SharedString::from(
                    "Mail app opened. Review the message, then choose Send.",
                ));
            }
            Err(error) => {
                error!(%error, "failed to prepare feedback email");
                ui.set_feedback_status(SharedString::from(
                    "Couldn't open the mail app. Email francehayle@outlook.com directly.",
                ));
            }
        }
    });

    let settings_for_language = Arc::clone(&settings);
    let settings_path_for_language = paths.settings.clone();
    #[cfg(windows)]
    let translation_cache_for_language = paths.root.join("translation-cache-v1.json");
    let weak = ui.as_weak();
    ui.on_select_language(move |language| {
        let Some(ui) = weak.upgrade() else { return };
        let preference = language_tag(language.as_str());
        let mut saved = false;
        if let Ok(mut settings) = settings_for_language.lock() {
            preference.clone_into(&mut settings.language);
            saved = save_settings(&settings_path_for_language, &settings).is_ok();
            apply_general_settings(&ui, &settings);
        }
        #[cfg(windows)]
        request_settings_copy_translation(
            ui.as_weak(),
            translation_cache_for_language.clone(),
            effective_language_tag(preference),
        );
        ui.set_settings_status(SharedString::from(if saved {
            "Language preference saved for translated Zink surfaces."
        } else {
            "Language preference could not be saved."
        }));
    });

    let weak = ui.as_weak();
    ui.on_filter_settings_languages(move |query| {
        let Some(ui) = weak.upgrade() else { return };
        let query = query.trim().to_lowercase();
        let options = SETTINGS_LANGUAGES
            .iter()
            .filter(|(language, tag)| {
                query.is_empty()
                    || language.to_lowercase().contains(&query)
                    || tag.to_lowercase().contains(&query)
            })
            .map(|(language, _)| SharedString::from(*language))
            .collect::<Vec<_>>();
        ui.set_settings_language_options(ModelRc::from(Rc::new(VecModel::from(options))));
    });

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        ui.on_check_app_updates(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_settings_update_active() {
                return;
            }
            ui.set_settings_update_active(true);
            ui.set_settings_status(SharedString::from(
                "Checking Microsoft Store for Zink updates…",
            ));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = check_and_install_store_updates();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    ui.set_settings_update_active(false);
                    match result {
                        Ok(message) => ui.set_settings_status(SharedString::from(message)),
                        Err(error) => {
                            error!(%error, "Microsoft Store update check failed");
                            ui.set_settings_status(SharedString::from(format!(
                                "Store update check is unavailable for this build: {error:#}. Use Open Store updates as the fallback."
                            )));
                        }
                    }
                });
            });
        });
    }

    let settings_for_general = Arc::clone(&settings);
    let settings_path_for_general = paths.settings.clone();
    #[cfg(windows)]
    let replay_hotkey_for_general = Arc::clone(&replay_hotkey_enabled);
    let weak = ui.as_weak();
    ui.on_update_setting(move |name, enabled| {
        let Some(ui) = weak.upgrade() else { return };
        let mut saved = false;
        if let Ok(mut settings) = settings_for_general.lock() {
            match name.as_str() {
                "discord" => settings.discord_rich_presence = enabled,
                "startup" => settings.run_at_startup = enabled,
                "reopen-last-page" => settings.reopen_last_page = enabled,
                "background-notifications" => settings.background_notifications = enabled,
                "low-resource" => settings.low_resource_background = enabled,
                "background-replay" => settings.background_replay = enabled,
                "clip-hotkey" => settings.clip_recording_hotkey = enabled,
                "diagnostic-log" => settings.diagnostic_log = enabled,
                _ => {}
            }
            saved = save_settings(&settings_path_for_general, &settings).is_ok();
            apply_general_settings(&ui, &settings);
        }
        #[cfg(windows)]
        if name.as_str() == "clip-hotkey" {
            replay_hotkey_for_general.store(enabled, Ordering::Relaxed);
        }
        if name.as_str() == "startup"
            && let Err(error) = configure_startup_registration(enabled)
        {
            error!(%error, "failed to update Windows startup registration");
            if let Ok(mut settings) = settings_for_general.lock() {
                settings.run_at_startup = !enabled;
                let _ = save_settings(&settings_path_for_general, &settings);
                apply_general_settings(&ui, &settings);
            }
            ui.set_settings_status(SharedString::from(format!(
                "Startup registration failed and the setting was restored: {error}"
            )));
            return;
        }
        if name.as_str() == "background-replay" {
            ui.invoke_recorder_command(SharedString::from(if enabled {
                "start-replay"
            } else {
                "stop-replay"
            }));
        }
        if name.as_str() == "discord" {
            let message = if enabled {
                publish_discord_page_presence(
                    ui.get_current_route().as_str(),
                    ui.get_page_title().as_str(),
                )
            } else {
                clear_discord_presence()
            };
            ui.set_settings_status(SharedString::from(match message {
                Ok(()) if enabled => "Discord Rich Presence is enabled and connected.",
                Ok(()) => "Discord Rich Presence is disabled.",
                Err(_) if enabled => {
                    "Discord preference saved. Open Discord to publish Zink activity."
                }
                Err(_) => "Discord preference saved; the Discord client was not running.",
            }));
            return;
        }
        ui.set_settings_status(SharedString::from(if saved {
            "Setting saved locally."
        } else {
            "The setting could not be saved."
        }));
    });

    let settings_for_customization = Arc::clone(&settings);
    let settings_path_for_customization = paths.settings.clone();
    let weak = ui.as_weak();
    ui.on_save_customization(move |theme, accent, compact, reduce_motion, lighter_sdr| {
        let Some(ui) = weak.upgrade() else { return };
        let mut saved = false;
        if let Ok(mut settings) = settings_for_customization.lock() {
            settings.theme = match theme.as_str() {
                "Light" => ThemePreference::Light,
                "Dark" => ThemePreference::Dark,
                _ => ThemePreference::System,
            };
            let Some(accent_color) = normalize_accent_color(accent.as_str()) else {
                ui.set_settings_status(SharedString::from(
                    "Enter a valid six-digit RGB colour such as #06404A.",
                ));
                return;
            };
            settings.accent_color = accent_color;
            settings.compact_navigation = compact;
            settings.reduce_motion = reduce_motion;
            settings.lighter_sdr_theme = lighter_sdr;
            saved = save_settings(&settings_path_for_customization, &settings).is_ok();
            apply_general_settings(&ui, &settings);
        }
        ui.set_settings_status(SharedString::from(if saved {
            "Appearance saved. Restart Zink to apply renderer-wide theme changes."
        } else {
            "Appearance could not be saved."
        }));
    });

    let settings_for_notifications = Arc::clone(&settings);
    let settings_path_for_notifications = paths.settings.clone();
    let weak = ui.as_weak();
    ui.on_save_notification_settings(move |playback, social, updates| {
        let Some(ui) = weak.upgrade() else { return };
        let mut saved = false;
        if let Ok(mut settings) = settings_for_notifications.lock() {
            settings.notifications_playback = playback;
            settings.notifications_social = social;
            settings.notifications_updates = updates;
            saved = save_settings(&settings_path_for_notifications, &settings).is_ok();
            apply_general_settings(&ui, &settings);
        }
        ui.set_settings_status(SharedString::from(if saved {
            "Notification preferences saved."
        } else {
            "Notification preferences could not be saved."
        }));
    });

    let notification_history_path = paths.root.join("notifications-v1.json");
    let notification_history =
        Rc::new(RefCell::new(load_notifications(&notification_history_path)));
    refresh_notification_model(&ui, &notification_history.borrow());

    let weak = ui.as_weak();
    let history_for_test = Rc::clone(&notification_history);
    let history_path_for_test = notification_history_path.clone();
    ui.on_send_test_notification(move || {
        let toast_result = show_windows_toast(
            "Zink Test Alert",
            "Desktop notifications and the Zink notification history are working.",
        );
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut history = history_for_test.borrow_mut();
        history.insert(
            0,
            SavedNotification {
                id: format!("test-{now}"),
                title: "Test Alert".to_owned(),
                message: "If you see this in Zink, your notification history is working.".to_owned(),
                timestamp_unix: now,
                action_label: "Windows settings".to_owned(),
                action_uri: "ms-settings:notifications".to_owned(),
            },
        );
        history.truncate(100);
        if let Some(ui) = weak.upgrade() {
            refresh_notification_model(&ui, &history);
            ui.set_settings_status(SharedString::from(match &toast_result {
                Ok(()) => "Test alert added and sent to Windows.",
                Err(_) => "Test alert added to Zink. Windows toast delivery needs the installed app identity.",
            }));
        }
        if let Err(error) = toast_result {
            error!(%error, "failed to show test Windows notification");
        }
        if let Err(error) = save_notifications(&history_path_for_test, &history) {
            error!(%error, "failed to save test notification");
        }
    });

    let weak = ui.as_weak();
    let history_for_delete = Rc::clone(&notification_history);
    let history_path_for_delete = notification_history_path.clone();
    ui.on_delete_notification(move |id| {
        let mut history = history_for_delete.borrow_mut();
        history.retain(|item| item.id != id.as_str());
        if let Some(ui) = weak.upgrade() {
            refresh_notification_model(&ui, &history);
        }
        if let Err(error) = save_notifications(&history_path_for_delete, &history) {
            error!(%error, "failed to delete notification");
        }
    });

    let weak = ui.as_weak();
    let history_for_clear = Rc::clone(&notification_history);
    let history_path_for_clear = notification_history_path.clone();
    ui.on_clear_notifications(move || {
        history_for_clear.borrow_mut().clear();
        let _ = std::fs::remove_file(&history_path_for_clear);
        if let Some(ui) = weak.upgrade() {
            refresh_notification_model(&ui, &history_for_clear.borrow());
            ui.set_settings_status(SharedString::from("Notification history cleared."));
        }
    });

    let log_folder = paths.logs.clone();
    ui.on_open_log_folder(move || {
        if let Err(error) = std::fs::create_dir_all(&log_folder) {
            error!(%error, path = %log_folder.display(), "failed to create log folder");
            return;
        }
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg(&log_folder)
            .spawn()
        {
            error!(%error, path = %log_folder.display(), "failed to open log folder");
        }
    });

    let log_folder_for_clear = paths.logs.clone();
    let weak = ui.as_weak();
    ui.on_clear_log(move || {
        let mut removed = 0usize;
        if let Ok(entries) = std::fs::read_dir(&log_folder_for_clear) {
            for entry in entries.flatten() {
                if entry.path().is_file() && std::fs::remove_file(entry.path()).is_ok() {
                    removed += 1;
                }
            }
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_settings_status(SharedString::from(format!(
                "Cleared {removed} diagnostic log files."
            )));
        }
    });

    let health_root = paths.root.clone();
    let health_logs = paths.logs.clone();
    let weak = ui.as_weak();
    ui.on_run_health_check(move || {
        let root = health_root.clone();
        let logs = health_logs.clone();
        let weak = weak.clone();
        if let Some(ui) = weak.upgrade() {
            ui.set_settings_status(SharedString::from("Running Zink health check…"));
        }
        std::thread::spawn(move || {
            let message = match create_health_support_bundle(&root, &logs) {
                Ok((report, bundle)) => format!(
                    "Health check complete. Report: {} • Bundle: {}",
                    report.display(),
                    bundle.display()
                ),
                Err(error) => format!("Health report failed: {error}"),
            };
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_settings_status(SharedString::from(message));
                }
            });
        });
    });

    let health_report_for_open = paths.root.join("zink-health-report.txt");
    let weak = ui.as_weak();
    ui.on_open_health_report(move || {
        let message = if health_report_for_open.is_file() {
            match std::process::Command::new("explorer.exe")
                .arg(&health_report_for_open)
                .spawn()
            {
                Ok(_) => "Health report opened.".to_owned(),
                Err(error) => format!("Could not open health report: {error}"),
            }
        } else {
            "Run the health check first to create a report.".to_owned()
        };
        if let Some(ui) = weak.upgrade() {
            ui.set_settings_status(SharedString::from(message));
        }
    });

    let health_root_for_send = paths.root.clone();
    let health_logs_for_send = paths.logs.clone();
    let social_session_for_send = paths.root.join("social-session-v1.json");
    let weak = ui.as_weak();
    ui.on_send_health_report(move || {
        let Some(ui) = weak.upgrade() else { return };
        ui.set_settings_status(SharedString::from("Preparing diagnostics support bundle…"));
        let root = health_root_for_send.clone();
        let logs = health_logs_for_send.clone();
        let session_path = social_session_for_send.clone();
        let weak = weak.clone();
        std::thread::spawn(move || {
            let message = upload_health_support_bundle(&root, &logs, &session_path)
                .unwrap_or_else(|error| format!("Diagnostics upload failed: {error}"));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_settings_status(SharedString::from(message));
                }
            });
        });
    });

    let settings_for_reset = Arc::clone(&settings);
    let settings_path_for_reset = paths.settings.clone();
    #[cfg(windows)]
    let replay_hotkey_for_reset = Arc::clone(&replay_hotkey_enabled);
    let weak = ui.as_weak();
    ui.on_reset_settings(move || {
        if let Ok(mut settings) = settings_for_reset.lock() {
            let discord_was_enabled = settings.discord_rich_presence;
            *settings = settings_defaults_preserving_user_data(&settings);
            let startup_reset = configure_startup_registration(true).is_ok();
            if !startup_reset {
                settings.run_at_startup = false;
            }
            let saved = save_settings(&settings_path_for_reset, &settings).is_ok();
            #[cfg(windows)]
            replay_hotkey_for_reset.store(true, Ordering::Relaxed);
            if let Some(ui) = weak.upgrade() {
                apply_general_settings(&ui, &settings);
                ui.invoke_recorder_command(SharedString::from("stop-replay"));
                let _ = if discord_was_enabled {
                    publish_discord_page_presence(
                        ui.get_current_route().as_str(),
                        ui.get_page_title().as_str(),
                    )
                } else {
                    publish_discord_page_presence("settings", "Settings")
                };
                ui.set_settings_reset_armed(false);
                ui.set_settings_status(SharedString::from(if saved && startup_reset {
                    "Settings restored to defaults."
                } else if !startup_reset {
                    "Defaults applied, but Windows startup could not be enabled."
                } else {
                    "Defaults were applied but could not be saved."
                }));
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_set_app_fullscreen(move |fullscreen| {
        if let Some(ui) = weak.upgrade() {
            ui.set_app_fullscreen(fullscreen);
            ui.window().set_fullscreen(fullscreen);
        }
    });

    let weak = ui.as_weak();
    ui.on_request_artist_info(move |artist| {
        let artist = artist.trim().to_owned();
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_artist_info_name().as_str() == artist
            && (ui.get_artist_info_status().starts_with("Loading")
                || ui.get_artist_releases().row_count() > 0)
        {
            return;
        }
        if artist.is_empty()
            || artist.eq_ignore_ascii_case("unknown artist")
            || artist == "Choose music or video from this PC"
        {
            ui.set_artist_info_status(SharedString::from(
                "Play a tagged song so Zink can identify its artist.",
            ));
            return;
        }
        if let Some((name, summary, releases)) = load_cached_artist_information(&artist) {
            apply_artist_information(&ui, &artist, name, summary, releases);
            return;
        }
        ui.set_artist_releases(ModelRc::from(Rc::new(VecModel::default())));
        ui.set_artist_info_summary(SharedString::new());
        ui.set_artist_info_name(SharedString::from(artist.as_str()));
        ui.set_artist_info_status(SharedString::from("Loading artist information…"));
        let weak = ui.as_weak();
        std::thread::spawn(move || {
            let result = fetch_artist_information(&artist);
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_track_detail().as_str() != artist {
                    return;
                }
                match result {
                    Ok((name, summary, releases)) => {
                        apply_artist_information(&ui, &artist, name, summary, releases);
                    }
                    Err(error) => {
                        tracing::warn!(%error, %artist, "artist information lookup failed");
                        ui.set_artist_info_status(SharedString::from(
                            "Artist information is unavailable right now. Check your connection and try again.",
                        ));
                    }
                }
            });
        });
    });

    ui.on_open_artist_release(move |release_id, release_title| {
        let url = format!(
            "https://musicbrainz.org/release-group/{}",
            release_id.as_str()
        );
        if let Err(error) = open_external_url(&url) {
            tracing::warn!(%error, release = %release_title, "failed to open artist release");
        }
    });

    let weak = ui.as_weak();
    let music_for_liked = Rc::clone(&music);
    ui.on_set_track_liked(move |liked| {
        let save_result = {
            let mut state = music_for_liked.borrow_mut();
            let Some(path) = state
                .current_index
                .and_then(|index| state.queue.get(index))
                .map(|track| track.path.clone())
            else {
                return;
            };
            if liked {
                state.liked_paths.insert(path);
            } else {
                state.liked_paths.remove(&path);
            }
            save_liked_paths(&state.liked_paths_file, &state.liked_paths)
        };
        if let Some(ui) = weak.upgrade() {
            ui.set_current_track_liked(liked);
            if let Err(error) = save_result {
                error!(%error, "failed to save liked music");
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let database_for_refresh = Rc::clone(&database);
    let settings_for_refresh_library = Arc::clone(&settings);
    ui.on_refresh_library(move || {
        if let Some(ui) = weak.upgrade() {
            let folders = settings_for_refresh_library
                .lock()
                .map(|settings| settings.media_library_folders.clone())
                .unwrap_or_default();
            if !folders.is_empty() {
                ui.set_library_status(SharedString::from(format!(
                    "Rescanning {} saved folder(s)…",
                    folders.len()
                )));
                let mut imported = 0usize;
                for folder in folders {
                    if let Ok(summary) = import_media_directory(
                        &mut database_for_refresh.borrow_mut(),
                        Path::new(&folder),
                    ) {
                        imported += summary.audio_files + summary.video_files;
                    }
                }
                let removed = prune_missing_media(&database_for_refresh.borrow()).unwrap_or(0);
                ui.set_library_status(SharedString::from(format!(
                    "Rescan complete: {imported} media files found, {removed} missing entries removed."
                )));
            }
            let kind = match ui.get_current_route().as_str() {
                "music-library" => Some("audio"),
                "video-library" => Some("video"),
                _ => None,
            };
            let query = ui.get_library_search().to_string();
            refresh_library(&ui, &database_for_refresh, &query, kind);
        }
    });

    // Preserve the WinUI library watcher's useful behaviour without keeping a
    // second filesystem indexer alive. While either library page is open, the
    // existing authoritative rescan path periodically imports new files and
    // prunes entries whose source files were moved or deleted.
    let library_rescan_timer = Timer::default();
    let weak = ui.as_weak();
    let settings_for_periodic_library = Arc::clone(&settings);
    library_rescan_timer.start(TimerMode::Repeated, Duration::from_secs(300), move || {
        let Some(ui) = weak.upgrade() else { return };
        if !matches!(
            ui.get_current_route().as_str(),
            "music-library" | "video-library"
        ) {
            return;
        }
        let has_sources = settings_for_periodic_library
            .lock()
            .is_ok_and(|settings| !settings.media_library_folders.is_empty());
        if has_sources {
            ui.invoke_refresh_library();
        }
    });

    let weak = ui.as_weak();
    let database_for_clear = Rc::clone(&database);
    ui.on_clear_library_search(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_library_search(SharedString::new());
            let kind = match ui.get_current_route().as_str() {
                "music-library" => Some("audio"),
                "video-library" => Some("video"),
                _ => None,
            };
            refresh_library(&ui, &database_for_clear, "", kind);
        }
    });

    let weak = ui.as_weak();
    let media_for_open = Rc::clone(&media);
    ui.on_open_media(move || match pick_media_file() {
        Ok(Some(path)) => {
            if let Some(ui) = weak.upgrade() {
                open_media_path(&ui, &media_for_open, &path);
            }
        }
        Ok(None) => {}
        Err(error) => {
            error!(%error, "media picker failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    ui.on_browse_video_subtitles(move || match pick_subtitle_file() {
        Ok(Some(path)) => {
            let contents = std::fs::read_to_string(&path);
            if let Some(ui) = weak.upgrade() {
                match contents {
                    Ok(contents) => {
                        let cues = parse_sidecar_subtitles(&contents);
                        let count = cues.len();
                        ui.set_video_subtitle_items(ModelRc::from(Rc::new(VecModel::from(
                            cues.into_iter()
                                .map(|cue| VideoSubtitleItem {
                                    start_ms: i32::try_from(cue.start_ms).unwrap_or(i32::MAX),
                                    end_ms: i32::try_from(cue.end_ms).unwrap_or(i32::MAX),
                                    text: SharedString::from(cue.text),
                                })
                                .collect::<Vec<_>>(),
                        ))));
                        ui.set_video_subtitles_enabled(count > 0);
                        ui.set_video_subtitle_status(SharedString::from(format!(
                            "Loaded {count} cues from {}",
                            path.file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or("subtitle file")
                        )));
                    }
                    Err(error) => ui.set_video_subtitle_status(SharedString::from(format!(
                        "Could not read subtitles: {error}"
                    ))),
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            error!(%error, "subtitle picker failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_video_subtitle_status(SharedString::from(error.to_string()));
            }
        }
    });

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let media_for_translation = Rc::clone(&media);
        ui.on_translate_video_audio(move || {
            let Some(ui) = weak.upgrade() else { return };
            let path = PathBuf::from(ui.get_current_video_path().as_str());
            if !path.is_file() {
                ui.set_video_translation_status(SharedString::from(
                    "Open a local video before translating its audio.",
                ));
                return;
            }
            let Ok(api_key) = std::env::var("OPENAI_API_KEY") else {
                ui.set_video_translation_status(SharedString::from(
                    "OPENAI_API_KEY is not configured. Zink has not sent any audio.",
                ));
                ui.set_video_info_open(true);
                return;
            };
            if api_key.trim().is_empty() {
                ui.set_video_translation_status(SharedString::from(
                    "OPENAI_API_KEY is empty. Zink has not sent any audio.",
                ));
                ui.set_video_info_open(true);
                return;
            }
            let duration_ms = media_for_translation
                .borrow()
                .transport_snapshot()
                .map_or(0, |snapshot| snapshot.duration_ms);
            ui.set_video_translation_active(true);
            ui.set_video_translation_status(SharedString::from(
                "Preparing Windows-native audio extraction…",
            ));
            let weak_done = weak.clone();
            std::thread::spawn(move || {
                let weak_progress = weak_done.clone();
                let result = video_translation::translate_to_vtt(
                    &path,
                    duration_ms,
                    api_key.trim(),
                    move |status| {
                        let weak = weak_progress.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            if let Some(ui) = weak.upgrade() {
                                ui.set_video_translation_status(SharedString::from(status));
                            }
                        });
                    },
                );
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak_done.upgrade() else {
                        return;
                    };
                    ui.set_video_translation_active(false);
                    match result {
                        Ok((translation_path, contents)) => {
                            let cues = parse_sidecar_subtitles(&contents);
                            let count = cues.len();
                            ui.set_video_subtitle_items(ModelRc::from(Rc::new(VecModel::from(
                                cues.into_iter()
                                    .map(|cue| VideoSubtitleItem {
                                        start_ms: i32::try_from(cue.start_ms).unwrap_or(i32::MAX),
                                        end_ms: i32::try_from(cue.end_ms).unwrap_or(i32::MAX),
                                        text: SharedString::from(cue.text),
                                    })
                                    .collect::<Vec<_>>(),
                            ))));
                            ui.set_video_subtitles_enabled(count > 0);
                            ui.set_video_subtitle_status(SharedString::from(format!(
                                "Loaded {count} translated UK English cues"
                            )));
                            ui.set_video_translation_status(SharedString::from(format!(
                                "Translation cached at {}",
                                translation_path.display()
                            )));
                        }
                        Err(error) => {
                            error!(%error, "video audio translation failed");
                            ui.set_video_translation_status(SharedString::from(format!(
                                "Translation failed: {error:#}"
                            )));
                            ui.set_video_info_open(true);
                        }
                    }
                });
            });
        });
    }

    let weak = ui.as_weak();
    let media_for_browse = Rc::clone(&media);
    let music_for_browse = Rc::clone(&music);
    ui.on_browse_music_files(move || match pick_music_files() {
        Ok(paths) => {
            let tracks = paths
                .into_iter()
                .map(|path| music_track_from_path(&path))
                .collect::<Vec<_>>();
            if let Some(ui) = weak.upgrade() {
                let play_index = {
                    let mut state = music_for_browse.borrow_mut();
                    let existing = state
                        .queue
                        .iter()
                        .map(|track| track.path.clone())
                        .collect::<HashSet<_>>();
                    let first_new = state.queue.len();
                    state.queue.extend(
                        tracks
                            .into_iter()
                            .filter(|track| !existing.contains(&track.path)),
                    );
                    state.auto_advanced = false;
                    (first_new < state.queue.len()).then_some(first_new)
                };
                sync_music_models_shared(&ui, &music_for_browse);
                ui.set_playback_error(SharedString::new());
                if let Some(index) = play_index {
                    play_music_index(&ui, &media_for_browse, &music_for_browse, index);
                } else {
                    ui.set_playback_error(SharedString::from(
                        "Those songs are already in the queue.",
                    ));
                }
            }
        }
        Err(error) => {
            error!(%error, "music picker failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let torrent_move_items = Arc::new(Mutex::new(Vec::<utorrent_mover::TorrentMoveItem>::new()));
    let torrent_move_selection = Arc::new(Mutex::new(HashSet::<String>::new()));
    let torrent_data_root = paths.root.clone();
    let weak = ui.as_weak();
    let items_for_refresh = Arc::clone(&torrent_move_items);
    let selection_for_refresh = Arc::clone(&torrent_move_selection);
    ui.on_refresh_torrent_mover(move || {
        let result = utorrent_mover::read_completed(&torrent_data_root);
        let Some(ui) = weak.upgrade() else { return };
        match result {
            Ok(items) => {
                if let Ok(mut selected) = selection_for_refresh.lock() {
                    selected.retain(|key| items.iter().any(|item| item.torrent_key == *key));
                }
                if let Ok(mut stored) = items_for_refresh.lock() {
                    *stored = items;
                    apply_torrent_move_model(
                        &ui,
                        &stored,
                        &selection_for_refresh
                            .lock()
                            .map_or_else(|_| HashSet::new(), |selection| selection.clone()),
                    );
                    ui.set_tools_status(SharedString::from(format!(
                        "{} completed µTorrent video job(s) ready.",
                        stored.len()
                    )));
                }
            }
            Err(error) => {
                error!(%error, "failed to read µTorrent resume data");
                ui.set_tools_status(SharedString::from(format!(
                    "Could not read µTorrent jobs: {error}"
                )));
            }
        }
    });

    let weak = ui.as_weak();
    let items_for_toggle = Arc::clone(&torrent_move_items);
    let selection_for_toggle = Arc::clone(&torrent_move_selection);
    ui.on_toggle_torrent_move_selection(move |key| {
        let key = key.to_string();
        let Ok(mut selection) = selection_for_toggle.lock() else {
            return;
        };
        if !selection.insert(key.clone()) {
            selection.remove(&key);
        }
        if let (Some(ui), Ok(items)) = (weak.upgrade(), items_for_toggle.lock()) {
            apply_torrent_move_model(&ui, &items, &selection);
        }
    });

    let move_statuses = Arc::new(Mutex::new(vec!["Ready".to_owned(); 4]));
    let weak = ui.as_weak();
    let torrent_items_for_move = Arc::clone(&torrent_move_items);
    let torrent_selection_for_move = Arc::clone(&torrent_move_selection);
    let torrent_data_root_for_move = paths.root.clone();
    ui.on_move_files(move |slot| {
        let Ok(slot) = usize::try_from(slot) else {
            return;
        };
        if slot >= 4 {
            return;
        }
        let selected_torrents = torrent_items_for_move
            .lock()
            .ok()
            .zip(torrent_selection_for_move.lock().ok())
            .map(|(items, selection)| {
                items
                    .iter()
                    .filter(|item| selection.contains(&item.torrent_key))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let clean_movie_names = !selected_torrents.is_empty();
        let files = if clean_movie_names {
            selected_torrents
                .iter()
                .flat_map(|item| item.files.iter().cloned())
                .collect::<Vec<_>>()
        } else {
            match pick_files_to_move() {
                Ok(files) if !files.is_empty() => files,
                Ok(_) => return,
                Err(error) => {
                    if let Some(ui) = weak.upgrade() {
                        ui.set_tools_status(SharedString::from(format!(
                            "File selection failed: {error}"
                        )));
                    }
                    return;
                }
            }
        };
        let destination = match pick_media_folder() {
            Ok(Some(folder)) => folder,
            Ok(None) => return,
            Err(error) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_tools_status(SharedString::from(format!(
                        "Destination selection failed: {error}"
                    )));
                }
                return;
            }
        };
        if let Ok(mut statuses) = move_statuses.lock() {
            statuses[slot] = format!(
                "Moving {} file(s) to {}…",
                files.len(),
                destination.display()
            );
            if let Some(ui) = weak.upgrade() {
                ui.set_tools_move_statuses(ModelRc::from(Rc::new(VecModel::from(
                    statuses.iter().map(SharedString::from).collect::<Vec<_>>(),
                ))));
                ui.set_tools_status(SharedString::from(format!("Move {} is running.", slot + 1)));
            }
        }
        let statuses = Arc::clone(&move_statuses);
        let weak = weak.clone();
        let data_root = torrent_data_root_for_move.clone();
        let items_after_move = Arc::clone(&torrent_items_for_move);
        let selection_after_move = Arc::clone(&torrent_selection_for_move);
        std::thread::spawn(move || {
            let result = move_files_recoverably(&files, &destination, clean_movie_names);
            let succeeded = result.is_ok();
            if result.is_ok()
                && clean_movie_names
                && let Err(error) = utorrent_mover::mark_moved(&data_root, &selected_torrents)
            {
                error!(%error, "failed to update moved µTorrent history");
            }
            let _ = slint::invoke_from_event_loop(move || {
                let message = match result {
                    Ok(count) => format!("Complete • {count} file(s) moved"),
                    Err(error) => format!("Move failed • {error}"),
                };
                if let Ok(mut values) = statuses.lock() {
                    values[slot].clone_from(&message);
                    if let Some(ui) = weak.upgrade() {
                        if succeeded && clean_movie_names {
                            if let Ok(mut selected) = selection_after_move.lock() {
                                selected.clear();
                            }
                            if let Ok(mut items) = items_after_move.lock() {
                                items.retain(|item| {
                                    !selected_torrents
                                        .iter()
                                        .any(|moved| moved.torrent_key == item.torrent_key)
                                });
                                apply_torrent_move_model(&ui, &items, &HashSet::new());
                            }
                        }
                        ui.set_tools_move_statuses(ModelRc::from(Rc::new(VecModel::from(
                            values.iter().map(SharedString::from).collect::<Vec<_>>(),
                        ))));
                        ui.set_tools_status(SharedString::from(message));
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let media_for_restore = Rc::clone(&media);
    let music_for_restore = Rc::clone(&music);
    let saved_for_restore = Rc::clone(&saved_session_for_restore);
    ui.on_restore_last_music_session(move || {
        let queue = saved_for_restore
            .queue
            .iter()
            .filter(|track| track.path.is_file())
            .cloned()
            .collect::<Vec<_>>();
        if queue.is_empty() {
            if let Some(ui) = weak.upgrade() {
                ui.set_has_restorable_session(false);
                ui.set_playback_error(SharedString::from(
                    "The files from the previous session are no longer available.",
                ));
            }
            return;
        }
        let index = saved_for_restore
            .current_index
            .unwrap_or(0)
            .min(queue.len() - 1);
        {
            let mut state = music_for_restore.borrow_mut();
            state.queue = queue;
            state.current_index = None;
            state.shuffle = saved_for_restore.shuffle;
            state.repeat_current = saved_for_restore.repeat_current;
            state.background = saved_for_restore.background;
            state.playback_rate = saved_for_restore.playback_rate.unwrap_or(1.0);
            state.crossfade_seconds = saved_for_restore.crossfade_seconds.unwrap_or(4).min(12);
            state.normalization_enabled = saved_for_restore.normalization_enabled.unwrap_or(false);
            state.normalization_preamp_db =
                saved_for_restore.normalization_preamp_db.unwrap_or(0.0);
        }
        if let Some(ui) = weak.upgrade() {
            sync_music_models_shared(&ui, &music_for_restore);
            ui.set_music_shuffle(saved_for_restore.shuffle);
            ui.set_music_repeat(saved_for_restore.repeat_current);
            ui.set_music_background(saved_for_restore.background);
            #[allow(clippy::cast_possible_truncation)]
            ui.set_playback_rate(saved_for_restore.playback_rate.unwrap_or(1.0) as f32);
            ui.set_crossfade_seconds(
                i32::try_from(saved_for_restore.crossfade_seconds.unwrap_or(4)).unwrap_or(4),
            );
            ui.set_normalization_enabled(saved_for_restore.normalization_enabled.unwrap_or(false));
            #[allow(clippy::cast_possible_truncation)]
            ui.set_normalization_preamp_db(
                saved_for_restore
                    .normalization_preamp_db
                    .unwrap_or(0.0)
                    .round() as i32,
            );
            if let Some(volume) = saved_for_restore.volume {
                let _ = media_for_restore.borrow_mut().set_volume(volume);
                #[allow(clippy::cast_possible_truncation)]
                ui.set_volume_percent((volume * 100.0).round() as i32);
            }
            let _ = media_for_restore
                .borrow_mut()
                .set_playback_rate(saved_for_restore.playback_rate.unwrap_or(1.0));
            restore_music_index(
                &ui,
                &media_for_restore,
                &music_for_restore,
                index,
                saved_for_restore.position_ms,
            );
            ui.set_has_restorable_session(false);
        }
    });

    let weak = ui.as_weak();
    let media_for_queue = Rc::clone(&media);
    let music_for_queue = Rc::clone(&music);
    ui.on_play_music_index(move |index| {
        if index < 0 {
            return;
        }
        if let Some(ui) = weak.upgrade()
            && let Ok(index) = usize::try_from(index)
        {
            play_music_index(&ui, &media_for_queue, &music_for_queue, index);
        }
    });

    let weak = ui.as_weak();
    let music_for_remove = Rc::clone(&music);
    let media_for_remove = Rc::clone(&media);
    ui.on_remove_queue_item(move |index| {
        let Ok(index) = usize::try_from(index) else {
            return;
        };
        let (removed_current, replacement_index, queue_empty) = {
            let mut state = music_for_remove.borrow_mut();
            if index >= state.queue.len() {
                return;
            }
            let removed_current = state.current_index == Some(index);
            state.queue.remove(index);
            state.current_index = state.current_index.and_then(|current| {
                if state.queue.is_empty() {
                    None
                } else if index < current {
                    Some(current - 1)
                } else if index == current {
                    None
                } else {
                    Some(current)
                }
            });
            (
                removed_current,
                index.min(state.queue.len().saturating_sub(1)),
                state.queue.is_empty(),
            )
        };
        if let Some(ui) = weak.upgrade() {
            sync_music_models_shared(&ui, &music_for_remove);
            if removed_current {
                if queue_empty {
                    if let Err(error) = media_for_remove.borrow_mut().stop() {
                        error!(%error, "failed to stop after removing the final queue item");
                    }
                    reset_music_display(&ui);
                } else {
                    play_music_index(&ui, &media_for_remove, &music_for_remove, replacement_index);
                }
            }
        }
        save_music_session(
            &music_for_remove.borrow(),
            0,
            media_for_remove.borrow().volume(),
        );
    });

    let weak = ui.as_weak();
    let music_for_move = Rc::clone(&music);
    let media_for_move = Rc::clone(&media);
    ui.on_move_queue_item(move |from, to| {
        let (Ok(from), Ok(to)) = (usize::try_from(from), usize::try_from(to)) else {
            return;
        };
        let mut state = music_for_move.borrow_mut();
        if from >= state.queue.len() || to >= state.queue.len() || from == to {
            return;
        }
        let track = state.queue.remove(from);
        state.queue.insert(to, track);
        state.current_index = state.current_index.map(|current| {
            if current == from {
                to
            } else if from < current && current <= to {
                current - 1
            } else if to <= current && current < from {
                current + 1
            } else {
                current
            }
        });
        if let Some(ui) = weak.upgrade() {
            sync_music_models(&ui, &state);
        }
        save_music_session(&state, 0, media_for_move.borrow().volume());
    });

    let weak = ui.as_weak();
    let media_for_clear_queue = Rc::clone(&media);
    let music_for_clear_queue = Rc::clone(&music);
    ui.on_clear_music_queue(move || {
        if let Err(error) = media_for_clear_queue.borrow_mut().stop() {
            error!(%error, "failed to stop playback while clearing queue");
        }
        let mut state = music_for_clear_queue.borrow_mut();
        state.queue.clear();
        state.current_index = None;
        state.lyrics.clear();
        if let Some(ui) = weak.upgrade() {
            sync_music_models(&ui, &state);
            reset_music_display(&ui);
        }
        save_music_session(&state, 0, media_for_clear_queue.borrow().volume());
    });

    let weak = ui.as_weak();
    let music_for_save_queue = Rc::clone(&music);
    let database_for_save_queue = Rc::clone(&database);
    let playlists_folder = paths.root.join("Playlists");
    ui.on_save_music_queue(move || {
        let queue = music_for_save_queue.borrow();
        let playlist_paths = queue
            .queue
            .iter()
            .map(|track| track.path.clone())
            .collect::<Vec<_>>();
        let result = save_queue_playlist(&playlists_folder, &queue.queue).and_then(|path| {
            save_playlist(
                &mut database_for_save_queue.borrow_mut(),
                "Saved Queue",
                &playlist_paths,
            )
            .context("save playlist database record")?;
            Ok(path)
        });
        if let Some(ui) = weak.upgrade() {
            match result {
                Ok(path) => {
                    ui.set_playback_error(SharedString::from(format!(
                        "Queue saved to {}",
                        path.display()
                    )));
                    refresh_music_collections(&ui, &database_for_save_queue);
                }
                Err(error) => {
                    error!(%error, "failed to save music queue");
                    ui.set_playback_error(SharedString::from(error.to_string()));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let database_for_playlist_load = Rc::clone(&database);
    let media_for_playlist_load = Rc::clone(&media);
    let music_for_playlist_load = Rc::clone(&music);
    ui.on_load_music_playlist(move |name| {
        let result = load_playlist(&database_for_playlist_load.borrow(), name.as_str());
        let Some(ui) = weak.upgrade() else { return };
        match result {
            Ok(paths) => {
                let queue = paths
                    .into_iter()
                    .filter(|path| path.is_file())
                    .map(|path| music_track_from_path(&path))
                    .collect::<Vec<_>>();
                if queue.is_empty() {
                    ui.set_playback_error(SharedString::from(
                        "This playlist has no available local tracks.",
                    ));
                    return;
                }
                {
                    let mut state = music_for_playlist_load.borrow_mut();
                    state.queue = queue;
                    state.current_index = None;
                    state.auto_advanced = false;
                }
                sync_music_models_shared(&ui, &music_for_playlist_load);
                play_music_index(&ui, &media_for_playlist_load, &music_for_playlist_load, 0);
            }
            Err(error) => {
                error!(%error, playlist = %name, "failed to load playlist");
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let music_for_location = Rc::clone(&music);
    ui.on_open_current_file_location(move || {
        let path = {
            let state = music_for_location.borrow();
            state
                .current_index
                .and_then(|index| state.queue.get(index))
                .map(|track| track.path.clone())
        };
        if let Some(path) = path
            && let Err(error) = std::process::Command::new("explorer.exe")
                .arg(format!("/select,{}", path.display()))
                .spawn()
        {
            error!(%error, path = %path.display(), "failed to open track location");
        }
    });

    let sleep_timer = Rc::new(Timer::default());
    let weak = ui.as_weak();
    let media_for_sleep = Rc::clone(&media);
    let sleep_timer_for_callback = Rc::clone(&sleep_timer);
    ui.on_set_sleep_timer(move |minutes| {
        sleep_timer_for_callback.stop();
        let Some(ui) = weak.upgrade() else { return };
        if minutes <= 0 {
            ui.set_sleep_timer_status(SharedString::from("Off"));
            return;
        }
        ui.set_sleep_timer_status(SharedString::from(format!("{minutes} minutes")));
        let weak = ui.as_weak();
        let media = Rc::clone(&media_for_sleep);
        sleep_timer_for_callback.start(
            TimerMode::SingleShot,
            Duration::from_secs(u64::try_from(minutes).unwrap_or(1) * 60),
            move || {
                if let Err(error) = media.borrow_mut().pause() {
                    error!(%error, "sleep timer failed to pause playback");
                }
                if let Some(ui) = weak.upgrade() {
                    ui.set_is_playing(false);
                    ui.set_sleep_timer_status(SharedString::from("Off"));
                }
            },
        );
    });

    let weak = ui.as_weak();
    let media_for_previous = Rc::clone(&media);
    let music_for_previous = Rc::clone(&music);
    ui.on_previous_music(move || {
        let index = {
            let state = music_for_previous.borrow();
            if state.queue.is_empty() {
                None
            } else {
                Some(
                    state
                        .current_index
                        .unwrap_or_default()
                        .checked_sub(1)
                        .unwrap_or(state.queue.len() - 1),
                )
            }
        };
        if let (Some(ui), Some(index)) = (weak.upgrade(), index) {
            play_music_index(&ui, &media_for_previous, &music_for_previous, index);
        }
    });

    let weak = ui.as_weak();
    let media_for_next = Rc::clone(&media);
    let music_for_next = Rc::clone(&music);
    ui.on_next_music(move || {
        let index = {
            let state = music_for_next.borrow();
            next_music_index(&state)
        };
        if let (Some(ui), Some(index)) = (weak.upgrade(), index) {
            play_music_index(&ui, &media_for_next, &music_for_next, index);
        }
    });

    let music_for_shuffle = Rc::clone(&music);
    let media_for_shuffle = Rc::clone(&media);
    ui.on_set_music_shuffle(move |enabled| {
        let mut state = music_for_shuffle.borrow_mut();
        state.shuffle = enabled;
        save_music_session(&state, 0, media_for_shuffle.borrow().volume());
    });

    let weak = ui.as_weak();
    let media_for_repeat = Rc::clone(&media);
    let music_for_repeat = Rc::clone(&music);
    ui.on_set_music_repeat(move |enabled| {
        music_for_repeat.borrow_mut().repeat_current = enabled;
        if let Err(error) = media_for_repeat.borrow().set_repeat_current(enabled) {
            error!(%error, "failed to set repeat-current mode");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
        save_music_session(
            &music_for_repeat.borrow(),
            0,
            media_for_repeat.borrow().volume(),
        );
    });

    let music_for_background = Rc::clone(&music);
    let media_for_background = Rc::clone(&media);
    ui.on_set_music_background(move |enabled| {
        let mut state = music_for_background.borrow_mut();
        state.background = enabled;
        save_music_session(&state, 0, media_for_background.borrow().volume());
    });

    let weak = ui.as_weak();
    let media_for_rate = Rc::clone(&media);
    let music_for_rate = Rc::clone(&music);
    ui.on_set_playback_rate(move |rate| {
        match media_for_rate
            .borrow_mut()
            .set_playback_rate(f64::from(rate))
        {
            Ok(rate) => {
                music_for_rate.borrow_mut().playback_rate = rate;
                save_music_session(
                    &music_for_rate.borrow(),
                    0,
                    media_for_rate.borrow().volume(),
                );
                if let Some(ui) = weak.upgrade() {
                    #[allow(clippy::cast_possible_truncation)]
                    ui.set_playback_rate(rate as f32);
                    ui.set_playback_error(SharedString::new());
                }
            }
            Err(error) => {
                error!(%error, "failed to set playback rate");
                if let Some(ui) = weak.upgrade() {
                    ui.set_playback_error(SharedString::from(error.to_string()));
                }
            }
        }
    });

    let music_for_crossfade = Rc::clone(&music);
    let media_for_crossfade = Rc::clone(&media);
    ui.on_set_crossfade_seconds(move |seconds| {
        let seconds = u32::try_from(seconds).unwrap_or_default().min(12);
        let mut state = music_for_crossfade.borrow_mut();
        state.crossfade_seconds = seconds;
        save_music_session(&state, 0, media_for_crossfade.borrow().volume());
    });

    let weak = ui.as_weak();
    let music_for_device_refresh = Rc::clone(&music);
    ui.on_refresh_audio_devices(move || {
        if let Some(ui) = weak.upgrade() {
            refresh_audio_devices(&ui, &music_for_device_refresh.borrow().audio_device_id);
        }
    });

    let weak = ui.as_weak();
    let media_for_device = Rc::clone(&media);
    let music_for_device = Rc::clone(&music);
    ui.on_select_audio_device(move |id, name| {
        // Keep the mutable media borrow out of the match expression. Rust extends
        // temporaries in a match scrutinee through the entire match, which made the
        // success arm's volume read panic with `RefCell already mutably borrowed`.
        let device_result = {
            let mut media = media_for_device.borrow_mut();
            media.set_audio_output_device(id.as_str())
        };
        match device_result {
            Ok(()) => {
                let mut state = music_for_device.borrow_mut();
                state.audio_device_id = id.to_string();
                save_music_session(&state, 0, media_for_device.borrow().volume());
                if let Some(ui) = weak.upgrade() {
                    ui.set_selected_audio_device_id(id);
                    ui.set_selected_audio_device_name(name);
                    ui.set_playback_error(SharedString::new());
                }
            }
            Err(error) => {
                error!(%error, device_id = %id, "failed to select audio output device");
                if let Some(ui) = weak.upgrade() {
                    ui.set_playback_error(SharedString::from(error.to_string()));
                    refresh_audio_devices(&ui, &music_for_device.borrow().audio_device_id);
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_video_audio_track = Rc::clone(&media);
    ui.on_select_video_audio_track(move |index| {
        let result = u32::try_from(index).map_or_else(
            |_| Err(anyhow::anyhow!("invalid audio track index")),
            |index| {
                media_for_video_audio_track
                    .borrow()
                    .select_audio_track(index)
                    .map_err(anyhow::Error::from)
            },
        );
        if let Some(ui) = weak.upgrade() {
            match result {
                Ok(()) => refresh_video_audio_tracks(&ui, &media_for_video_audio_track.borrow()),
                Err(error) => ui.set_playback_error(SharedString::from(format!(
                    "Could not select that audio stream: {error}"
                ))),
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_normalization = Rc::clone(&media);
    let music_for_normalization = Rc::clone(&music);
    ui.on_set_normalization(move |enabled, preamp_db| {
        let (gain_db, peak) = {
            let mut state = music_for_normalization.borrow_mut();
            state.normalization_enabled = enabled;
            state.normalization_preamp_db = f64::from(preamp_db.clamp(-6, 6));
            let values = state
                .current_index
                .and_then(|index| state.queue.get(index))
                .map_or((0.0, None), |track| {
                    (
                        if enabled {
                            track.replay_gain_db.unwrap_or(0.0) + state.normalization_preamp_db
                        } else {
                            0.0
                        },
                        if enabled { track.replay_peak } else { None },
                    )
                });
            save_music_session(&state, 0, media_for_normalization.borrow().volume());
            values
        };
        match media_for_normalization
            .borrow_mut()
            .set_normalization_gain(gain_db, peak)
        {
            Ok(_) => {
                if let Some(ui) = weak.upgrade() {
                    ui.set_normalization_enabled(enabled);
                    ui.set_normalization_preamp_db(preamp_db.clamp(-6, 6));
                    ui.set_playback_error(SharedString::new());
                }
            }
            Err(error) => {
                error!(%error, "failed to apply ReplayGain normalization");
                if let Some(ui) = weak.upgrade() {
                    ui.set_playback_error(SharedString::from(error.to_string()));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_absolute_volume = Rc::clone(&media);
    let music_for_absolute_volume = Rc::clone(&music);
    ui.on_set_volume(move |volume| {
        match media_for_absolute_volume
            .borrow_mut()
            .set_volume(f64::from(volume))
        {
            Ok(volume) => {
                if let Some(ui) = weak.upgrade() {
                    #[allow(clippy::cast_possible_truncation)]
                    ui.set_volume_percent((volume * 100.0).round() as i32);
                }
                save_music_session(&music_for_absolute_volume.borrow(), 0, volume);
            }
            Err(error) => {
                error!(%error, "failed to set music volume");
                if let Some(ui) = weak.upgrade() {
                    ui.set_playback_error(SharedString::from(error.to_string()));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_eq_enabled = Rc::clone(&media);
    let settings_for_eq_enabled = Arc::clone(&settings);
    let settings_path_for_eq_enabled = paths.settings.clone();
    ui.on_set_equalizer_enabled(move |enabled| {
        if let Ok(mut settings) = settings_for_eq_enabled.lock() {
            settings.equalizer_enabled = enabled;
            let _ = save_settings(&settings_path_for_eq_enabled, &settings);
            let native_result = media_for_eq_enabled.borrow_mut().set_equalizer_profile(
                enabled.then_some(f64::from(settings.equalizer_master_gain)),
                equalizer_band_array(&settings.equalizer_bands),
            );
            if let Some(ui) = weak.upgrade() {
                apply_equalizer_settings(&ui, &settings);
                if let Err(error) = native_result {
                    error!(%error, "failed to change native equalizer bypass");
                    ui.set_equalizer_status(SharedString::from(
                        "Windows could not change the live equalizer gain.",
                    ));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_eq_master = Rc::clone(&media);
    let settings_for_eq_master = Arc::clone(&settings);
    let settings_path_for_eq_master = paths.settings.clone();
    ui.on_set_equalizer_master_gain(move |gain| {
        if let Ok(mut settings) = settings_for_eq_master.lock() {
            settings.equalizer_master_gain = gain.clamp(-12.0, 12.0);
            settings.equalizer_preset = "Custom".into();
            let _ = save_settings(&settings_path_for_eq_master, &settings);
            let native_result = media_for_eq_master.borrow_mut().set_equalizer_profile(
                settings
                    .equalizer_enabled
                    .then_some(f64::from(settings.equalizer_master_gain)),
                equalizer_band_array(&settings.equalizer_bands),
            );
            if let Some(ui) = weak.upgrade() {
                apply_equalizer_settings(&ui, &settings);
                if let Err(error) = native_result {
                    error!(%error, "failed to change native equalizer master gain");
                    ui.set_equalizer_status(SharedString::from(
                        "Windows could not apply the live master gain.",
                    ));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_eq_band = Rc::clone(&media);
    let settings_for_eq_band = Arc::clone(&settings);
    let settings_path_for_eq_band = paths.settings.clone();
    ui.on_set_equalizer_band(move |index, gain| {
        if let Ok(mut settings) = settings_for_eq_band.lock() {
            if settings.equalizer_bands.len() != 10 {
                settings.equalizer_bands.resize(10, 0.0);
            }
            if let Some(band) = usize::try_from(index)
                .ok()
                .and_then(|index| settings.equalizer_bands.get_mut(index))
            {
                *band = gain.clamp(-12.0, 12.0);
                settings.equalizer_preset = "Custom".into();
                let _ = save_settings(&settings_path_for_eq_band, &settings);
                let native_result = media_for_eq_band.borrow_mut().set_equalizer_profile(
                    settings
                        .equalizer_enabled
                        .then_some(f64::from(settings.equalizer_master_gain)),
                    equalizer_band_array(&settings.equalizer_bands),
                );
                if let Some(ui) = weak.upgrade() {
                    apply_equalizer_settings(&ui, &settings);
                    if let Err(error) = native_result {
                        error!(%error, "failed to apply equalizer curve headroom");
                        ui.set_equalizer_status(SharedString::from(
                            "Windows could not apply automatic curve headroom.",
                        ));
                    }
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_eq_preset = Rc::clone(&media);
    let settings_for_eq_preset = Arc::clone(&settings);
    let settings_path_for_eq_preset = paths.settings.clone();
    ui.on_apply_equalizer_preset(move |preset| {
        if let Ok(mut settings) = settings_for_eq_preset.lock() {
            settings.equalizer_preset = preset.to_string();
            settings.equalizer_bands = equalizer_preset_curve(preset.as_str()).to_vec();
            let _ = save_settings(&settings_path_for_eq_preset, &settings);
            let native_result = media_for_eq_preset.borrow_mut().set_equalizer_profile(
                settings
                    .equalizer_enabled
                    .then_some(f64::from(settings.equalizer_master_gain)),
                equalizer_band_array(&settings.equalizer_bands),
            );
            if let Some(ui) = weak.upgrade() {
                apply_equalizer_settings(&ui, &settings);
                if let Err(error) = native_result {
                    error!(%error, "failed to apply equalizer preset headroom");
                    ui.set_equalizer_status(SharedString::from(
                        "Preset saved, but Windows could not apply its safe preamp.",
                    ));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_eq_reset = Rc::clone(&media);
    let settings_for_eq_reset = Arc::clone(&settings);
    let settings_path_for_eq_reset = paths.settings.clone();
    ui.on_reset_equalizer(move || {
        if let Ok(mut settings) = settings_for_eq_reset.lock() {
            settings.equalizer_enabled = true;
            settings.equalizer_master_gain = 0.0;
            settings.equalizer_preset = "Flat".into();
            settings.equalizer_bands = vec![0.0; 10];
            let _ = save_settings(&settings_path_for_eq_reset, &settings);
            let native_result = media_for_eq_reset
                .borrow_mut()
                .set_equalizer_profile(Some(0.0), equalizer_band_array(&settings.equalizer_bands));
            if let Some(ui) = weak.upgrade() {
                apply_equalizer_settings(&ui, &settings);
                ui.set_equalizer_status(SharedString::from(if native_result.is_ok() {
                    "Reset to the Flat profile. Native master gain is active."
                } else {
                    "Profile reset, but Windows could not apply the live master gain."
                }));
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_eq_save = Arc::clone(&settings);
    let settings_path_for_eq_save = paths.settings.clone();
    ui.on_save_equalizer(move || {
        if let Ok(settings) = settings_for_eq_save.lock() {
            let saved = save_settings(&settings_path_for_eq_save, &settings).is_ok();
            if let Some(ui) = weak.upgrade() {
                ui.set_equalizer_status(SharedString::from(if saved {
                    "Sound profile saved locally."
                } else {
                    "Zink could not save this profile."
                }));
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_style = Arc::clone(&settings);
    let settings_path_for_visualizer_style = paths.settings.clone();
    ui.on_set_visualizer_style(move |style| {
        if let Ok(mut settings) = settings_for_visualizer_style.lock() {
            settings.visualizer_style = style.to_string();
            let _ = save_settings(&settings_path_for_visualizer_style, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_color = Arc::clone(&settings);
    let settings_path_for_visualizer_color = paths.settings.clone();
    ui.on_set_visualizer_color(move |color| {
        if let Ok(mut settings) = settings_for_visualizer_color.lock() {
            settings.visualizer_color = color.to_string();
            let _ = save_settings(&settings_path_for_visualizer_color, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_sensitivity = Arc::clone(&settings);
    let settings_path_for_visualizer_sensitivity = paths.settings.clone();
    ui.on_set_visualizer_sensitivity(move |value| {
        if let Ok(mut settings) = settings_for_visualizer_sensitivity.lock() {
            settings.visualizer_sensitivity = value.clamp(0, 100);
            let _ = save_settings(&settings_path_for_visualizer_sensitivity, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_smoothing = Arc::clone(&settings);
    let settings_path_for_visualizer_smoothing = paths.settings.clone();
    ui.on_set_visualizer_smoothing(move |value| {
        if let Ok(mut settings) = settings_for_visualizer_smoothing.lock() {
            settings.visualizer_smoothing = value.clamp(0, 100);
            let _ = save_settings(&settings_path_for_visualizer_smoothing, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_paused = Arc::clone(&settings);
    let settings_path_for_visualizer_paused = paths.settings.clone();
    ui.on_set_visualizer_paused(move |paused| {
        if let Ok(mut settings) = settings_for_visualizer_paused.lock() {
            settings.visualizer_paused = paused;
            let _ = save_settings(&settings_path_for_visualizer_paused, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_reset = Arc::clone(&settings);
    let settings_path_for_visualizer_reset = paths.settings.clone();
    ui.on_reset_visualizer(move || {
        if let Ok(mut settings) = settings_for_visualizer_reset.lock() {
            settings.visualizer_style = "Bars".into();
            settings.visualizer_color = "Sky".into();
            settings.visualizer_sensitivity = 100;
            settings.visualizer_smoothing = 82;
            settings.visualizer_paused = false;
            let _ = save_settings(&settings_path_for_visualizer_reset, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_visualizer_settings(&ui, &settings);
                ui.set_visualizer_status(SharedString::from("Reset to default"));
            }
        }
    });

    let weak = ui.as_weak();
    let settings_for_visualizer_save = Arc::clone(&settings);
    let settings_path_for_visualizer_save = paths.settings.clone();
    ui.on_save_visualizer(move || {
        if let Ok(settings) = settings_for_visualizer_save.lock() {
            let saved = save_settings(&settings_path_for_visualizer_save, &settings).is_ok();
            if let Some(ui) = weak.upgrade() {
                ui.set_visualizer_status(SharedString::from(if saved {
                    "Settings saved"
                } else {
                    "Save failed"
                }));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_toggle = Rc::clone(&media);
    ui.on_toggle_playback(move || match media_for_toggle.borrow_mut().toggle() {
        Ok(state) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_is_playing(state == PlaybackState::Playing);
            }
        }
        Err(error) => {
            error!(%error, "playback toggle failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_play = Rc::clone(&media);
    ui.on_play_media(move || match media_for_play.borrow_mut().play() {
        Ok(()) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_is_playing(true);
                ui.set_playback_error(SharedString::new());
            }
        }
        Err(error) => {
            error!(%error, "play failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_pause = Rc::clone(&media);
    ui.on_pause_media(move || match media_for_pause.borrow_mut().pause() {
        Ok(()) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_is_playing(false);
                ui.set_playback_error(SharedString::new());
            }
        }
        Err(error) => {
            error!(%error, "pause failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_relative_seek = Rc::clone(&media);
    ui.on_seek_relative(move |seconds| {
        if let Err(error) = media_for_relative_seek
            .borrow()
            .seek_relative(f64::from(seconds))
        {
            error!(%error, "relative seek failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_radio = Rc::clone(&media);
    let settings_for_radio = Arc::clone(&settings);
    ui.on_play_radio_station(move |station, stream_uri| {
        if let Some(ui) = weak.upgrade() {
            start_radio_station(
                &ui,
                &media_for_radio,
                &settings_for_radio,
                station.as_str(),
                stream_uri.as_str(),
            );
        }
    });

    let weak = ui.as_weak();
    let media_for_previous_station = Rc::clone(&media);
    let settings_for_previous_station = Arc::clone(&settings);
    ui.on_previous_radio_station(move || {
        if let Some(ui) = weak.upgrade() {
            let station = settings_for_previous_station
                .lock()
                .ok()
                .and_then(|settings| {
                    adjacent_radio_station(&settings, ui.get_track_title().as_str(), -1)
                });
            let Some((station, uri)) = station else {
                return;
            };
            start_radio_station(
                &ui,
                &media_for_previous_station,
                &settings_for_previous_station,
                &station,
                &uri,
            );
        }
    });

    let weak = ui.as_weak();
    let media_for_next_station = Rc::clone(&media);
    let settings_for_next_station = Arc::clone(&settings);
    ui.on_next_radio_station(move || {
        if let Some(ui) = weak.upgrade() {
            let station = settings_for_next_station.lock().ok().and_then(|settings| {
                adjacent_radio_station(&settings, ui.get_track_title().as_str(), 1)
            });
            let Some((station, uri)) = station else {
                return;
            };
            start_radio_station(
                &ui,
                &media_for_next_station,
                &settings_for_next_station,
                &station,
                &uri,
            );
        }
    });

    let weak = ui.as_weak();
    let settings_for_radio_like = Arc::clone(&settings);
    let settings_path_for_radio_like = paths.settings.clone();
    ui.on_toggle_radio_like(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_track_detail().as_str() != "Internet radio" {
            return;
        }
        let station = ui.get_track_title().to_string();
        if let Ok(mut settings) = settings_for_radio_like.lock() {
            if let Some(index) = settings
                .liked_radio_stations
                .iter()
                .position(|item| item == &station)
            {
                settings.liked_radio_stations.remove(index);
            } else {
                settings.liked_radio_stations.push(station);
            }
            let _ = save_settings(&settings_path_for_radio_like, &settings);
            apply_radio_likes(&ui, &settings);
        }
    });

    let weak = ui.as_weak();
    let settings_for_radio_remove = Arc::clone(&settings);
    let settings_path_for_radio_remove = paths.settings.clone();
    ui.on_remove_radio_like(move |station| {
        if let Ok(mut settings) = settings_for_radio_remove.lock() {
            settings
                .liked_radio_stations
                .retain(|item| item != station.as_str());
            let _ = save_settings(&settings_path_for_radio_remove, &settings);
            if let Some(ui) = weak.upgrade() {
                apply_radio_likes(&ui, &settings);
            }
        }
    });

    let weak = ui.as_weak();
    let songs_for_remove = Arc::clone(&liked_radio_songs);
    let songs_path_for_remove = liked_radio_songs_path.clone();
    ui.on_remove_liked_radio_song(move |id| {
        let Ok(mut songs) = songs_for_remove.lock() else {
            return;
        };
        songs.retain(|song| song.id != id.as_str());
        if let Err(error) = save_liked_radio_songs(&songs_path_for_remove, &songs) {
            error!(%error, "failed to remove liked radio song");
        }
        if let Some(ui) = weak.upgrade() {
            apply_liked_radio_songs(&ui, &songs);
        }
    });

    let weak = ui.as_weak();
    let songs_for_spotify_open = Arc::clone(&liked_radio_songs);
    ui.on_open_liked_radio_song_in_spotify(move |id| {
        let song = songs_for_spotify_open
            .lock()
            .ok()
            .and_then(|songs| songs.iter().find(|song| song.id == id.as_str()).cloned());
        let Some(song) = song else { return };
        let url = if song.spotify_track_url.is_empty() {
            web_search_url(
                "https://open.spotify.com/search/",
                &liked_radio_song_query(&song),
            )
        } else {
            song.spotify_track_url
        };
        if let Some(ui) = weak.upgrade() {
            ui.invoke_open_web_service(SharedString::from(url));
        }
    });

    let weak = ui.as_weak();
    let songs_for_youtube = Arc::clone(&liked_radio_songs);
    ui.on_open_liked_radio_song_in_youtube(move |id| {
        let song = songs_for_youtube
            .lock()
            .ok()
            .and_then(|songs| songs.iter().find(|song| song.id == id.as_str()).cloned());
        let Some(song) = song else { return };
        let url = if song.youtube_video_url.is_empty() {
            web_search_url(
                "https://www.youtube.com/results?search_query=",
                &liked_radio_song_query(&song),
            )
        } else {
            song.youtube_video_url
        };
        if let Some(ui) = weak.upgrade() {
            ui.invoke_open_web_service(SharedString::from(url));
        }
    });

    let weak = ui.as_weak();
    let songs_for_spotify_like = Arc::clone(&liked_radio_songs);
    let songs_path_for_spotify_like = liked_radio_songs_path.clone();
    let credentials_path_for_radio_like = spotify_credentials_path.clone();
    ui.on_add_liked_radio_song_to_spotify(move |id| {
        let Some(ui) = weak.upgrade() else { return };
        let song = songs_for_spotify_like
            .lock()
            .ok()
            .and_then(|songs| songs.iter().find(|song| song.id == id.as_str()).cloned());
        let Some(song) = song else { return };
        ui.set_playback_error(SharedString::from("Adding track to Spotify Liked Songs…"));
        let weak = weak.clone();
        let songs = Arc::clone(&songs_for_spotify_like);
        let songs_path = songs_path_for_spotify_like.clone();
        let credentials_path = credentials_path_for_radio_like.clone();
        std::thread::spawn(move || {
            let result = load_spotify_credentials(&credentials_path)
                .and_then(|credentials| spotify_like_radio_song(&credentials, &song));
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                match result {
                    Ok((track_id, track_url)) => {
                        if let Ok(mut values) = songs.lock()
                            && let Some(value) = values.iter_mut().find(|value| value.id == song.id)
                        {
                            value.spotify_track_id = track_id;
                            value.spotify_track_url = track_url;
                            value.added_to_spotify_liked_songs = true;
                            value.spotify_synced_at_utc = Some(unix_timestamp().to_string());
                            if let Err(error) = save_liked_radio_songs(&songs_path, &values) {
                                error!(%error, "failed to save Spotify radio like");
                            }
                            apply_liked_radio_songs(&ui, &values);
                        }
                        ui.set_playback_error(SharedString::from("Added to Spotify Liked Songs."));
                    }
                    Err(error) => ui.set_playback_error(SharedString::from(format!(
                        "Could not add this track to Spotify: {error}"
                    ))),
                }
            });
        });
    });

    let weak = ui.as_weak();
    let songs_for_save = Arc::clone(&liked_radio_songs);
    let songs_path_for_save = liked_radio_songs_path.clone();
    ui.on_save_current_radio_song(move || {
        let Some(ui) = weak.upgrade() else { return };
        let title = ui.get_current_radio_track_title().trim().to_owned();
        let artist = ui.get_current_radio_track_artist().trim().to_owned();
        if title.is_empty() && artist.is_empty() {
            ui.set_playback_error(SharedString::from(
                "Track metadata has not arrived yet. Try again in a moment.",
            ));
            return;
        }
        let Ok(mut songs) = songs_for_save.lock() else {
            return;
        };
        if let Some(index) = songs.iter().position(|song| {
            song.title.eq_ignore_ascii_case(&title) && song.artist.eq_ignore_ascii_case(&artist)
        }) {
            let mut existing = songs.remove(index);
            existing.station_name = ui.get_track_title().to_string();
            existing.liked_at_utc = unix_timestamp().to_string();
            songs.insert(0, existing);
        } else {
            let timestamp = unix_timestamp();
            let sequence = songs.len();
            songs.insert(
                0,
                LikedRadioSongState {
                    id: format!("rust-{timestamp}-{sequence}"),
                    title,
                    artist,
                    album: String::new(),
                    artwork_url: String::new(),
                    station_name: ui.get_track_title().to_string(),
                    liked_at_utc: timestamp.to_string(),
                    ..LikedRadioSongState::default()
                },
            );
        }
        if let Err(error) = save_liked_radio_songs(&songs_path_for_save, &songs) {
            error!(%error, "failed to save liked radio song");
            ui.set_playback_error(SharedString::from(error.to_string()));
            return;
        }
        apply_liked_radio_songs(&ui, &songs);
        ui.set_playback_error(SharedString::from("Saved to Liked Radio Songs."));
    });

    let weak = ui.as_weak();
    let settings_for_add_station = Arc::clone(&settings);
    let settings_path_for_add_station = paths.settings.clone();
    ui.on_add_radio_station(move |name, genre, stream_url| {
        let name = name.trim();
        let genre = genre.trim();
        let stream_url = stream_url.trim();
        let valid_url = url::Url::parse(stream_url)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some());
        let Some(ui) = weak.upgrade() else { return };
        if name.is_empty() || !valid_url {
            ui.set_playback_error(SharedString::from(
                "Enter a station name and a valid HTTP or HTTPS stream URL.",
            ));
            return;
        }
        if let Ok(mut settings) = settings_for_add_station.lock() {
            if settings
                .custom_radio_stations
                .iter()
                .any(|station| station.name.eq_ignore_ascii_case(name))
            {
                ui.set_playback_error(SharedString::from("That station already exists."));
                return;
            }
            settings.custom_radio_stations.push(CustomRadioStation {
                name: name.to_owned(),
                genre: if genre.is_empty() {
                    "Music".to_owned()
                } else {
                    genre.to_owned()
                },
                stream_url: stream_url.to_owned(),
            });
            let _ = save_settings(&settings_path_for_add_station, &settings);
            apply_radio_stations(&ui, &settings);
            apply_search_radio_stations(&ui, &settings, ui.get_library_search().as_str());
            ui.set_radio_new_name(SharedString::new());
            ui.set_radio_new_url(SharedString::new());
            ui.set_playback_error(SharedString::new());
        }
    });

    let weak = ui.as_weak();
    let settings_for_remove_station = Arc::clone(&settings);
    let settings_path_for_remove_station = paths.settings.clone();
    ui.on_remove_radio_station(move |name| {
        let Some(ui) = weak.upgrade() else { return };
        if let Ok(mut settings) = settings_for_remove_station.lock() {
            settings
                .custom_radio_stations
                .retain(|station| !station.name.eq_ignore_ascii_case(name.as_str()));
            let _ = save_settings(&settings_path_for_remove_station, &settings);
            apply_radio_stations(&ui, &settings);
            apply_search_radio_stations(&ui, &settings, ui.get_library_search().as_str());
        }
    });

    let weak = ui.as_weak();
    let database_for_duplicate_scan = Rc::clone(&database);
    ui.on_scan_duplicate_videos(move || {
        let Some(ui) = weak.upgrade() else { return };
        if ui.get_duplicate_scan_active() {
            return;
        }
        let candidates = match list_media(&database_for_duplicate_scan.borrow(), Some("video")) {
            Ok(items) => items.into_iter().map(|item| item.path).collect::<Vec<_>>(),
            Err(error) => {
                ui.set_duplicate_status(SharedString::from(format!("Unable to read the video library: {error}")));
                return;
            }
        };
        ui.set_duplicate_scan_active(true);
        ui.set_duplicate_status(SharedString::from(format!(
            "Checking {} indexed videos by size and full SHA-256 content…",
            candidates.len()
        )));
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let result = scan_duplicate_video_paths(&candidates);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_duplicate_scan_active(false);
                    match result {
                        Ok(items) => {
                            let group_count = items
                                .iter()
                                .map(|item| item.group.as_str())
                                .collect::<HashSet<_>>()
                                .len();
                            let model = items
                                .into_iter()
                                .map(|item| DuplicateVideoItem {
                                    group: SharedString::from(item.group),
                                    name: SharedString::from(item.name),
                                    path: SharedString::from(item.path.to_string_lossy().as_ref()),
                                    size: SharedString::from(format_bytes(item.size)),
                                    copies: i32::try_from(item.copies).unwrap_or(i32::MAX),
                                })
                                .collect::<Vec<_>>();
                            let duplicate_files = model.len();
                            ui.set_duplicate_video_items(ModelRc::from(Rc::new(VecModel::from(model))));
                            ui.set_duplicate_status(SharedString::from(if group_count == 0 {
                                "No verified duplicate videos found.".to_owned()
                            } else {
                                format!("Found {group_count} verified duplicate groups across {duplicate_files} files.")
                            }));
                        }
                        Err(error) => ui.set_duplicate_status(SharedString::from(error)),
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let media_for_duplicate_play = Rc::clone(&media);
    ui.on_play_duplicate_video(move |path| {
        if let Some(ui) = weak.upgrade() {
            open_media_path(&ui, &media_for_duplicate_play, Path::new(path.as_str()));
        }
    });

    let weak = ui.as_weak();
    ui.on_open_duplicate_location(move |path| {
        let selected = PathBuf::from(path.as_str());
        if let Err(error) = std::process::Command::new("explorer.exe")
            .arg("/select,")
            .arg(&selected)
            .spawn()
            && let Some(ui) = weak.upgrade()
        {
            ui.set_duplicate_status(SharedString::from(format!(
                "Unable to open File Explorer: {error}"
            )));
        }
    });

    let weak = ui.as_weak();
    let database_for_duplicate_delete = Rc::clone(&database);
    ui.on_delete_duplicate_video(move |path| {
        let Some(ui) = weak.upgrade() else { return };
        let selected = PathBuf::from(path.as_str());
        match delete_duplicate_path(&database_for_duplicate_delete, &selected) {
            Ok(()) => {
                ui.set_duplicate_status(SharedString::from(
                    "Duplicate video deleted. Refreshing verification…",
                ));
                ui.invoke_scan_duplicate_videos();
                refresh_library(&ui, &database_for_duplicate_delete, "", Some("video"));
            }
            Err(error) => ui.set_duplicate_status(SharedString::from(error)),
        }
    });

    let weak = ui.as_weak();
    let database_for_duplicate_bulk_delete = Rc::clone(&database);
    ui.on_delete_all_duplicate_videos(move || {
        let Some(ui) = weak.upgrade() else { return };
        let model = ui.get_duplicate_video_items();
        let mut groups = BTreeMap::<String, Vec<PathBuf>>::new();
        for index in 0..model.row_count() {
            if let Some(item) = model.row_data(index) {
                groups
                    .entry(item.group.to_string())
                    .or_default()
                    .push(PathBuf::from(item.path.as_str()));
            }
        }
        let mut deleted = 0usize;
        let mut failure = None;
        for paths in groups.values() {
            for path in paths.iter().skip(1) {
                match delete_duplicate_path(&database_for_duplicate_bulk_delete, path) {
                    Ok(()) => deleted += 1,
                    Err(error) => {
                        failure = Some(error);
                        break;
                    }
                }
            }
            if failure.is_some() {
                break;
            }
        }
        ui.set_duplicate_status(SharedString::from(failure.unwrap_or_else(|| {
            format!(
                "Deleted {deleted} extra duplicate copies. One verified copy was kept per group."
            )
        })));
        refresh_library(&ui, &database_for_duplicate_bulk_delete, "", Some("video"));
        ui.invoke_scan_duplicate_videos();
    });

    let weak = ui.as_weak();
    ui.on_lookup_film_info(move |query| {
        let Some(ui) = weak.upgrade() else { return };
        let query = query.trim().to_owned();
        if query.is_empty() || ui.get_film_info_loading() {
            return;
        }
        ui.set_film_query(SharedString::from(query.clone()));
        ui.set_film_info_title(SharedString::from(query.clone()));
        ui.set_film_info_date(SharedString::new());
        ui.set_film_info_overview(SharedString::from("Searching Wikipedia for film details…"));
        ui.set_film_info_source_url(SharedString::new());
        ui.set_film_info_loading(true);
        let ui_weak = ui.as_weak();
        std::thread::spawn(move || {
            let result = fetch_film_information(&query);
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_film_info_loading(false);
                    match result {
                        Ok(info) => {
                            ui.set_film_info_title(SharedString::from(info.title));
                            ui.set_film_info_date(SharedString::from(info.date));
                            ui.set_film_info_overview(SharedString::from(info.overview));
                            ui.set_film_info_source_url(SharedString::from(info.source_url));
                            if let Some(path) = info.poster_path
                                && let Ok(image) = slint::Image::load_from_path(&path)
                            {
                                ui.set_film_info_poster(image);
                            }
                        }
                        Err(error) => ui.set_film_info_overview(SharedString::from(format!(
                            "Zink could not load film details right now: {error}"
                        ))),
                    }
                }
            });
        });
    });

    let weak = ui.as_weak();
    let media_for_online_video = Rc::clone(&media);
    ui.on_play_resolved_online_video(move |link, title| {
        let Some(ui) = weak.upgrade() else { return };
        let link = link.trim();
        let Ok(parsed) = url::Url::parse(link) else {
            ui.set_online_video_status(SharedString::from(
                "Enter a valid HTTPS OneDrive or media link.",
            ));
            return;
        };
        if parsed.scheme() != "https" {
            ui.set_online_video_status(SharedString::from(
                "Only secure HTTPS video links are supported.",
            ));
            return;
        }
        let stream_url = parsed;
        match media_for_online_video
            .borrow_mut()
            .open_uri(stream_url.as_str())
        {
            Ok(()) => {
                let fallback_title = stream_url
                    .path_segments()
                    .and_then(Iterator::last)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("OneDrive video");
                ui.set_track_title(if title.trim().is_empty() {
                    SharedString::from(fallback_title)
                } else {
                    title
                });
                ui.set_track_detail(SharedString::from("Online video"));
                ui.set_is_playing(true);
                ui.set_playback_error(SharedString::new());
                ui.set_online_video_status(SharedString::from(
                    "Streaming the original file in Zink Player.",
                ));
            }
            Err(error) => {
                ui.set_playback_error(SharedString::from(error.to_string()));
                ui.set_online_video_status(SharedString::from(format!(
                    "Unable to start the shared video: {error}"
                )));
            }
        }
    });

    #[cfg(windows)]
    let onedrive_token = Arc::new(Mutex::new(None::<OneDriveAccessToken>));

    #[cfg(windows)]
    {
        let weak = ui.as_weak();
        let token = Arc::clone(&onedrive_token);
        ui.on_sign_in_onedrive(move || {
            let weak = weak.clone();
            let token = Arc::clone(&token);
            if let Some(ui) = weak.upgrade() {
                ui.set_online_video_status(SharedString::from(
                    "Starting secure Microsoft sign-in…",
                ));
            }
            std::thread::spawn(move || {
                let result = acquire_onedrive_device_token(&weak);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match result {
                            Ok(access) => {
                                if let Ok(mut saved) = token.lock() {
                                    *saved = Some(access);
                                }
                                ui.set_online_video_status(SharedString::from(
                                    "Signed in to Microsoft. Protected OneDrive links are ready.",
                                ));
                            }
                            Err(error) => ui.set_online_video_status(SharedString::from(format!(
                                "Microsoft sign-in failed: {error}"
                            ))),
                        }
                    }
                });
            });
        });
    }

    #[cfg(not(windows))]
    ui.on_sign_in_onedrive(|| {});

    let weak = ui.as_weak();
    #[cfg(windows)]
    let token_for_play = Arc::clone(&onedrive_token);
    ui.on_play_online_video(move |link| {
        let Some(ui) = weak.upgrade() else { return };
        let link = link.trim().to_owned();
        let Ok(parsed) = url::Url::parse(&link) else {
            ui.set_online_video_status(SharedString::from(
                "Enter a valid HTTPS OneDrive or media link.",
            ));
            return;
        };
        if parsed.scheme() != "https" {
            ui.set_online_video_status(SharedString::from(
                "Only secure HTTPS video links are supported.",
            ));
            return;
        }

        #[cfg(windows)]
        if is_onedrive_link(&parsed) {
            let token = token_for_play
                .lock()
                .ok()
                .and_then(|saved| saved.clone())
                .filter(|saved| saved.expires_at > Instant::now() + Duration::from_secs(60));
            let Some(token) = token else {
                ui.set_online_video_status(SharedString::from(
                    "Sign in to Microsoft first, then press Play video again.",
                ));
                return;
            };
            ui.set_online_video_status(SharedString::from(
                "Resolving the protected original through Microsoft Graph…",
            ));
            let weak = weak.clone();
            std::thread::spawn(move || {
                let result = resolve_onedrive_video(&link, &token.value);
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = weak.upgrade() {
                        match result {
                            Ok(video) => ui.invoke_play_resolved_online_video(
                                SharedString::from(video.url),
                                SharedString::from(video.name),
                            ),
                            Err(error) => ui.set_online_video_status(SharedString::from(format!(
                                "Microsoft Graph could not resolve this video: {error}"
                            ))),
                        }
                    }
                });
            });
            return;
        }

        let stream_url = normalize_onedrive_stream_url(parsed);
        ui.invoke_play_resolved_online_video(
            SharedString::from(stream_url.as_str()),
            SharedString::new(),
        );
    });

    let weak = ui.as_weak();
    let media_for_volume = Rc::clone(&media);
    let music_for_volume = Rc::clone(&music);
    ui.on_change_volume(move |delta| {
        match media_for_volume
            .borrow_mut()
            .adjust_volume(f64::from(delta))
        {
            Ok(volume) => {
                if let Some(ui) = weak.upgrade() {
                    #[allow(clippy::cast_possible_truncation)]
                    ui.set_volume_percent((volume * 100.0).round() as i32);
                }
                save_music_session(&music_for_volume.borrow(), 0, volume);
            }
            Err(error) => {
                error!(%error, "volume change failed");
                if let Some(ui) = weak.upgrade() {
                    ui.set_playback_error(SharedString::from(error.to_string()));
                }
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_seek = Rc::clone(&media);
    ui.on_seek(move |fraction| {
        if let Err(error) = media_for_seek.borrow().seek_fraction(f64::from(fraction)) {
            error!(%error, "seek failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_lyric_seek = Rc::clone(&media);
    ui.on_seek_to_lyric(move |start_ms| {
        let snapshot = media_for_lyric_seek.borrow().transport_snapshot();
        let result = snapshot.and_then(|snapshot| {
            if snapshot.duration_ms == 0 {
                return Ok(());
            }
            #[allow(clippy::cast_precision_loss)]
            let fraction = f64::from(start_ms.max(0)) / snapshot.duration_ms as f64;
            media_for_lyric_seek.borrow().seek_fraction(fraction)
        });
        if let Err(error) = result {
            error!(%error, "lyric seek failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_stop = Rc::clone(&media);
    ui.on_stop_playback(move || match media_for_stop.borrow_mut().stop() {
        Ok(()) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_is_playing(false);
                ui.set_position_ratio(0.0);
                ui.set_position_text(SharedString::from("0:00"));
            }
        }
        Err(error) => {
            error!(%error, "stop failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_playback_error(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let media_for_clear = Rc::clone(&media);
    ui.on_clear_playback(move || {
        if let Err(error) = media_for_clear.borrow_mut().stop() {
            error!(%error, "clear playback stop failed");
        }
        if let Some(ui) = weak.upgrade() {
            ui.set_track_title(SharedString::from("Nothing playing"));
            ui.set_track_detail(SharedString::from("Choose music or video from this PC"));
            ui.set_is_playing(false);
            ui.set_position_ratio(0.0);
            ui.set_position_text(SharedString::from("0:00"));
            ui.set_duration_text(SharedString::from("0:00"));
            ui.set_playback_error(SharedString::new());
        }
    });

    let weak = ui.as_weak();
    let database_for_import = Rc::clone(&database);
    let settings_for_import = Arc::clone(&settings);
    let settings_path_for_import = paths.settings.clone();
    ui.on_import_library(move || match pick_media_folder() {
        Ok(Some(folder)) => {
            if let Some(ui) = weak.upgrade() {
                ui.set_library_status(SharedString::from("Scanning media folder…"));
            }
            match import_media_directory(&mut database_for_import.borrow_mut(), &folder) {
                Ok(summary) => {
                    if let Ok(mut settings) = settings_for_import.lock() {
                        let folder_text = folder.to_string_lossy().into_owned();
                        if !settings
                            .media_library_folders
                            .iter()
                            .any(|saved| saved.eq_ignore_ascii_case(&folder_text))
                        {
                            settings.media_library_folders.push(folder_text);
                            let _ = save_settings(&settings_path_for_import, &settings);
                        }
                        if let Some(ui) = weak.upgrade() {
                            ui.set_library_source_folders(ModelRc::from(Rc::new(VecModel::from(
                                settings
                                    .media_library_folders
                                    .iter()
                                    .map(SharedString::from)
                                    .collect::<Vec<_>>(),
                            ))));
                        }
                    }
                    let status = format!(
                        "Imported {} audio and {} video files from {}",
                        summary.audio_files,
                        summary.video_files,
                        folder.display()
                    );
                    if let Some(ui) = weak.upgrade() {
                        ui.set_library_status(SharedString::from(status));
                        let kind = match ui.get_current_route().as_str() {
                            "music-library" => Some("audio"),
                            "video-library" => Some("video"),
                            _ => None,
                        };
                        refresh_library(&ui, &database_for_import, "", kind);
                    }
                    info!(
                        audio = summary.audio_files,
                        video = summary.video_files,
                        skipped = summary.skipped_files,
                        folder = %folder.display(),
                        "media library import complete"
                    );
                }
                Err(error) => {
                    error!(%error, folder = %folder.display(), "media library import failed");
                    if let Some(ui) = weak.upgrade() {
                        ui.set_library_status(SharedString::from(error.to_string()));
                    }
                }
            }
        }
        Ok(None) => {}
        Err(error) => {
            error!(%error, "media folder picker failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_library_status(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let database_for_import = Rc::clone(&database);
    let settings_for_multi_import = Arc::clone(&settings);
    let settings_path_for_multi_import = paths.settings.clone();
    ui.on_import_multiple_libraries(move || match pick_media_folders() {
        Ok(folders) if !folders.is_empty() => {
            if let Some(ui) = weak.upgrade() {
                ui.set_library_status(SharedString::from(format!(
                    "Scanning {} media folders…",
                    folders.len()
                )));
            }
            let mut audio_files = 0usize;
            let mut video_files = 0usize;
            let mut skipped_files = 0usize;
            let mut failures = Vec::new();
            for folder in &folders {
                match import_media_directory(&mut database_for_import.borrow_mut(), folder) {
                    Ok(summary) => {
                        audio_files += summary.audio_files;
                        video_files += summary.video_files;
                        skipped_files += summary.skipped_files;
                    }
                    Err(error) => {
                        error!(%error, folder = %folder.display(), "media library import failed");
                        failures.push(folder.display().to_string());
                    }
                }
            }
            if let Ok(mut settings) = settings_for_multi_import.lock() {
                for folder in &folders {
                    let folder_text = folder.to_string_lossy().into_owned();
                    if !settings
                        .media_library_folders
                        .iter()
                        .any(|saved| saved.eq_ignore_ascii_case(&folder_text))
                    {
                        settings.media_library_folders.push(folder_text);
                    }
                }
                let _ = save_settings(&settings_path_for_multi_import, &settings);
                if let Some(ui) = weak.upgrade() {
                    ui.set_library_source_folders(ModelRc::from(Rc::new(VecModel::from(
                        settings
                            .media_library_folders
                            .iter()
                            .map(SharedString::from)
                            .collect::<Vec<_>>(),
                    ))));
                }
            }
            if let Some(ui) = weak.upgrade() {
                let mut status = format!(
                    "Imported {audio_files} audio and {video_files} video files from {} folders ({skipped_files} skipped)",
                    folders.len()
                );
                if !failures.is_empty() {
                    status = format!("{status}; {} folder(s) failed", failures.len());
                }
                ui.set_library_status(SharedString::from(status));
                let kind = match ui.get_current_route().as_str() {
                    "music-library" => Some("audio"),
                    "video-library" => Some("video"),
                    _ => None,
                };
                refresh_library(&ui, &database_for_import, "", kind);
            }
        }
        Ok(_) => {}
        Err(error) => {
            error!(%error, "multiple media folder picker failed");
            if let Some(ui) = weak.upgrade() {
                ui.set_library_status(SharedString::from(error.to_string()));
            }
        }
    });

    let weak = ui.as_weak();
    let database_for_remove_source = Rc::clone(&database);
    let settings_for_remove_source = Arc::clone(&settings);
    let settings_path_for_remove_source = paths.settings.clone();
    ui.on_remove_library_source(move |folder| {
        let folder_path = PathBuf::from(folder.as_str());
        let result = remove_media_directory(&database_for_remove_source.borrow(), &folder_path);
        let Some(ui) = weak.upgrade() else { return };
        match result {
            Ok(removed) => {
                if let Ok(mut settings) = settings_for_remove_source.lock() {
                    settings
                        .media_library_folders
                        .retain(|saved| !saved.eq_ignore_ascii_case(folder.as_str()));
                    let _ = save_settings(&settings_path_for_remove_source, &settings);
                    ui.set_library_source_folders(ModelRc::from(Rc::new(VecModel::from(
                        settings
                            .media_library_folders
                            .iter()
                            .map(SharedString::from)
                            .collect::<Vec<_>>(),
                    ))));
                }
                ui.set_library_status(SharedString::from(format!(
                    "Removed source and {removed} indexed item(s). Your files were not deleted."
                )));
                let kind = match ui.get_current_route().as_str() {
                    "music-library" => Some("audio"),
                    "video-library" => Some("video"),
                    _ => None,
                };
                refresh_library(&ui, &database_for_remove_source, "", kind);
            }
            Err(error) => ui.set_library_status(SharedString::from(format!(
                "Could not remove library source: {error}"
            ))),
        }
    });

    let weak = ui.as_weak();
    let media_for_library = Rc::clone(&media);
    let music_for_library = Rc::clone(&music);
    ui.on_play_library_item(move |path| {
        let path = std::path::PathBuf::from(path.as_str());
        if let Some(ui) = weak.upgrade() {
            if is_audio_path(&path) {
                {
                    let mut state = music_for_library.borrow_mut();
                    state.queue = vec![music_track_from_path(&path)];
                    state.current_index = None;
                    state.auto_advanced = false;
                }
                sync_music_models_shared(&ui, &music_for_library);
                play_music_index(&ui, &media_for_library, &music_for_library, 0);
            } else {
                open_media_path(&ui, &media_for_library, &path);
            }
        }
    });

    let weak = ui.as_weak();
    let database_for_library_all = Rc::clone(&database);
    let media_for_library_all = Rc::clone(&media);
    let music_for_library_all = Rc::clone(&music);
    ui.on_play_library_all(move |shuffle| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Ok(mut items) = list_media(&database_for_library_all.borrow(), Some("audio")) else {
            ui.set_playback_error(SharedString::from("The music library could not be read."));
            return;
        };
        let query = ui.get_library_search().trim().to_lowercase();
        items.retain(|item| {
            query.is_empty()
                || item.title.to_lowercase().contains(&query)
                || item.artist.to_lowercase().contains(&query)
                || item.album.to_lowercase().contains(&query)
                || item.path.to_string_lossy().to_lowercase().contains(&query)
        });
        sort_library_items(&mut items, ui.get_library_sort_mode().as_str());
        if shuffle {
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| {
                    duration.as_secs() ^ u64::from(duration.subsec_nanos())
                });
            items.sort_by_key(|item| {
                let mut hasher = DefaultHasher::new();
                item.path.hash(&mut hasher);
                seed.hash(&mut hasher);
                hasher.finish()
            });
        }
        if items.is_empty() {
            ui.set_playback_error(SharedString::from(
                "No matching music is available to play.",
            ));
            return;
        }
        {
            let mut state = music_for_library_all.borrow_mut();
            state.queue = items
                .iter()
                .map(|item| music_track_from_path(&item.path))
                .collect();
            state.current_index = None;
            state.shuffle = shuffle;
            state.auto_advanced = false;
        }
        sync_music_models_shared(&ui, &music_for_library_all);
        apply_route(&ui, "music-player");
        play_music_index(&ui, &media_for_library_all, &music_for_library_all, 0);
    });

    let weak = ui.as_weak();
    let database_for_search_collection = Rc::clone(&database);
    let media_for_search_collection = Rc::clone(&media);
    let music_for_search_collection = Rc::clone(&music);
    ui.on_play_search_collection(move |kind, name| {
        let Some(ui) = weak.upgrade() else {
            return;
        };
        let Ok(mut items) = list_media(&database_for_search_collection.borrow(), Some("audio"))
        else {
            ui.set_playback_error(SharedString::from("The music library could not be read."));
            return;
        };
        let name = name.trim();
        items.retain(|item| match kind.as_str() {
            "album" => item.album.eq_ignore_ascii_case(name),
            "artist" => item.artist.eq_ignore_ascii_case(name),
            _ => false,
        });
        items.sort_by(|left, right| {
            left.album
                .to_lowercase()
                .cmp(&right.album.to_lowercase())
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        });
        if items.is_empty() {
            ui.set_playback_error(SharedString::from(format!(
                "No tracks were found for {name}."
            )));
            return;
        }
        {
            let mut state = music_for_search_collection.borrow_mut();
            state.queue = items
                .iter()
                .map(|item| music_track_from_path(&item.path))
                .collect();
            state.current_index = None;
            state.shuffle = false;
            state.auto_advanced = false;
        }
        sync_music_models_shared(&ui, &music_for_search_collection);
        apply_route(&ui, "music-player");
        play_music_index(
            &ui,
            &media_for_search_collection,
            &music_for_search_collection,
            0,
        );
    });

    let weak = ui.as_weak();
    let database_for_search = Rc::clone(&database);
    let settings_for_search = Arc::clone(&settings);
    ui.on_search_library(move |query| {
        if let Some(ui) = weak.upgrade() {
            let kind = match ui.get_current_route().as_str() {
                "music-library" => Some("audio"),
                "video-library" => Some("video"),
                _ => None,
            };
            refresh_library(&ui, &database_for_search, query.as_str(), kind);
            if kind.is_none()
                && let Ok(settings) = settings_for_search.lock()
            {
                apply_search_radio_stations(&ui, &settings, query.as_str());
            }
        }
    });

    if let Some(path) = std::env::args_os().nth(1).map(std::path::PathBuf::from)
        && path.is_file()
    {
        if is_audio_path(&path) {
            let track = music_track_from_path(&path);
            {
                let mut state = music.borrow_mut();
                state.queue = vec![track];
            }
            sync_music_models_shared(&ui, &music);
            apply_route(&ui, "music-player");
            play_music_index(&ui, &media, &music, 0);
        } else {
            open_media_path(&ui, &media, &path);
        }
    }

    let transport_timer = Timer::default();
    let last_session_save_ms = Rc::new(RefCell::new(0_u64));
    let last_history_path = Rc::new(RefCell::new(None::<PathBuf>));
    let last_history_position = Rc::new(RefCell::new(0_u64));
    let last_history_duration = Rc::new(RefCell::new(0_u64));
    let radio_stall_ticks = Rc::new(RefCell::new(0_u16));
    let video_track_refresh_ticks = Rc::new(RefCell::new(0_u8));
    let weak = ui.as_weak();
    let media_for_timer = Rc::clone(&media);
    let music_for_timer = Rc::clone(&music);
    let database_for_timer = Rc::clone(&database);
    let last_session_save_for_timer = Rc::clone(&last_session_save_ms);
    let last_history_path_for_timer = Rc::clone(&last_history_path);
    let last_history_position_for_timer = Rc::clone(&last_history_position);
    let last_history_duration_for_timer = Rc::clone(&last_history_duration);
    let radio_stall_ticks_for_timer = Rc::clone(&radio_stall_ticks);
    let video_track_refresh_ticks_for_timer = Rc::clone(&video_track_refresh_ticks);
    transport_timer.start(TimerMode::Repeated, Duration::from_millis(100), move || {
        let system_commands = media_for_timer.borrow().drain_system_media_commands();
        if let Some(ui) = weak.upgrade() {
            for command in system_commands {
                match command {
                    SystemMediaCommand::Play => ui.invoke_play_media(),
                    SystemMediaCommand::Pause => ui.invoke_pause_media(),
                    SystemMediaCommand::Stop => ui.invoke_stop_playback(),
                    SystemMediaCommand::Next => ui.invoke_next_music(),
                    SystemMediaCommand::Previous => ui.invoke_previous_music(),
                    SystemMediaCommand::FastForward => ui.invoke_seek_relative(10.0),
                    SystemMediaCommand::Rewind => ui.invoke_seek_relative(-10.0),
                }
            }
        }
        if let Some(mut frame) = media_for_timer.borrow().take_latest_video_frame() {
            for pixel in frame.bgra.chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            if let Some(ui) = weak.upgrade() {
                let pixels = slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                    &frame.bgra,
                    frame.width,
                    frame.height,
                );
                ui.set_video_frame(slint::Image::from_rgba8(pixels));
            }
        }
        let Ok(snapshot) = media_for_timer.borrow().transport_snapshot() else {
            return;
        };
        if let Some(ui) = weak.upgrade() {
            ui.set_playback_position_ms(
                i32::try_from(snapshot.position_ms).unwrap_or(i32::MAX),
            );
            let radio_should_be_playing = ui.get_track_detail().as_str() == "Internet radio"
                && ui.get_is_playing()
                && !ui.get_current_radio_stream_url().is_empty();
            if radio_should_be_playing && !snapshot.is_playing {
                let mut ticks = radio_stall_ticks_for_timer.borrow_mut();
                *ticks = ticks.saturating_add(1);
                if *ticks >= 30 {
                    *ticks = 0;
                    ui.set_playback_error(SharedString::from(
                        "Live stream interrupted. Reconnecting…",
                    ));
                    ui.invoke_play_radio_station(
                        ui.get_track_title(),
                        ui.get_current_radio_stream_url(),
                    );
                }
            } else {
                *radio_stall_ticks_for_timer.borrow_mut() = 0;
            }
            if ui.get_current_route().as_str() == "video-player" {
                let mut ticks = video_track_refresh_ticks_for_timer.borrow_mut();
                *ticks = ticks.saturating_add(1);
                if *ticks >= 10 {
                    *ticks = 0;
                    refresh_video_audio_tracks(&ui, &media_for_timer.borrow());
                    refresh_video_technical_info(&ui, &media_for_timer.borrow());
                }
            } else {
                *video_track_refresh_ticks_for_timer.borrow_mut() = 0;
            }
        }
        let pending_restore = music_for_timer.borrow().pending_restore_ms;
        if pending_restore > 0 && snapshot.duration_ms > 0 {
            #[allow(clippy::cast_precision_loss)]
            let fraction = pending_restore as f64 / snapshot.duration_ms as f64;
            if let Err(error) = media_for_timer.borrow().seek_fraction(fraction) {
                error!(%error, "failed to restore music position");
            }
            if let Err(error) = media_for_timer.borrow_mut().pause() {
                error!(%error, "failed to pause restored music session");
            }
            music_for_timer.borrow_mut().pending_restore_ms = 0;
        }
        if snapshot
            .position_ms
            .abs_diff(*last_session_save_for_timer.borrow())
            >= 5_000
        {
            save_music_session(
                &music_for_timer.borrow(),
                snapshot.position_ms,
                media_for_timer.borrow().volume(),
            );
            *last_session_save_for_timer.borrow_mut() = snapshot.position_ms;
        }
        let current_path = {
            let state = music_for_timer.borrow();
            state
                .current_index
                .and_then(|index| state.queue.get(index))
                .map(|track| track.path.clone())
        };
        let history_changed = current_path != *last_history_path_for_timer.borrow();
        if history_changed {
            if let Some(previous_path) = last_history_path_for_timer.borrow().as_deref() {
                let position_ms = *last_history_position_for_timer.borrow();
                let duration_ms = *last_history_duration_for_timer.borrow();
                let completed = duration_ms > 0 && position_ms.saturating_add(1_500) >= duration_ms;
                let skipped = position_ms >= 5_000 && !completed;
                if let Err(error) = finish_playback(
                    &mut database_for_timer.borrow_mut(),
                    previous_path,
                    position_ms,
                    completed,
                    skipped,
                ) {
                    error!(%error, path = %previous_path.display(), "failed to finalize playback history");
                }
            }
            if let Some(path) = current_path.as_deref()
                && let Err(error) = record_playback(
                    &mut database_for_timer.borrow_mut(),
                    path,
                    snapshot.position_ms,
                    false,
                )
            {
                error!(%error, path = %path.display(), "failed to record playback history");
            }
            *last_history_path_for_timer.borrow_mut() = current_path;
        }
        *last_history_position_for_timer.borrow_mut() = snapshot.position_ms;
        *last_history_duration_for_timer.borrow_mut() = snapshot.duration_ms;
        if let Some(ui) = weak.upgrade() {
            if history_changed {
                refresh_music_collections(&ui, &database_for_timer);
            }
            #[allow(clippy::cast_precision_loss)]
            let ratio = if snapshot.duration_ms == 0 {
                0.0
            } else {
                (snapshot.position_ms as f64 / snapshot.duration_ms as f64).clamp(0.0, 1.0)
            };
            #[allow(clippy::cast_possible_truncation)]
            ui.set_position_ratio(ratio as f32);
            ui.set_position_text(SharedString::from(format_media_time(snapshot.position_ms)));
            ui.set_duration_text(SharedString::from(format_media_time(snapshot.duration_ms)));
            ui.set_is_playing(snapshot.is_playing);
            let crossfade = {
                let state = music_for_timer.borrow();
                state.crossfade_started.map(|started| {
                    (
                        started,
                        state.crossfade_target_index,
                        state.crossfade_seconds,
                    )
                })
            };
            if let Some((started, target, seconds)) = crossfade {
                let progress = started.elapsed().as_secs_f64() / f64::from(seconds.max(1));
                let crossfade_result = {
                    let mut engine = media_for_timer.borrow_mut();
                    engine.set_crossfade_progress(progress)
                };
                match crossfade_result {
                    Ok(true) => {
                        {
                            let mut state = music_for_timer.borrow_mut();
                            state.crossfade_started = None;
                            state.crossfade_target_index = None;
                        }
                        if let Some(target) = target {
                            play_music_index(&ui, &media_for_timer, &music_for_timer, target);
                        }
                        return;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        error!(%error, "crossfade volume ramp failed");
                        let mut state = music_for_timer.borrow_mut();
                        state.crossfade_started = None;
                        state.crossfade_target_index = None;
                    }
                }
            } else {
                let crossfade_target = {
                    let state = music_for_timer.borrow();
                    let fade_ms = u64::from(state.crossfade_seconds) * 1_000;
                    if state.crossfade_seconds > 0
                        && snapshot.is_playing
                        && snapshot.duration_ms > fade_ms
                        && snapshot.position_ms.saturating_add(fade_ms) >= snapshot.duration_ms
                    {
                        state.prepared_index.and_then(|index| {
                            state
                                .queue
                                .get(index)
                                .map(|track| (index, track.path.clone()))
                        })
                    } else {
                        None
                    }
                };
                if let Some((index, path)) = crossfade_target {
                    let begin_result = {
                        let mut engine = media_for_timer.borrow_mut();
                        engine.begin_crossfade(&path)
                    };
                    match begin_result {
                        Ok(()) => {
                            let mut state = music_for_timer.borrow_mut();
                            state.crossfade_started = Some(Instant::now());
                            state.crossfade_target_index = Some(index);
                        }
                        Err(error) => {
                            error!(%error, path = %path.display(), "failed to start crossfade");
                        }
                    }
                }
            }
            if let Some(results) = ONLINE_LYRICS_RESULTS.get()
                && let Ok(mut results) = results.lock()
                && let Some(result) = results.pop()
            {
                let is_current = {
                    let state = music_for_timer.borrow();
                    state
                        .current_index
                        .and_then(|index| state.queue.get(index))
                        .is_some_and(|track| track.path == result.path)
                };
                if is_current {
                    let lyric_items = result
                        .lyrics
                        .iter()
                        .map(|(start_ms, text)| LyricLineItem {
                            start_ms: i32::try_from(*start_ms).unwrap_or(i32::MAX),
                            text: SharedString::from(text.as_str()),
                        })
                        .collect::<Vec<_>>();
                    music_for_timer.borrow_mut().lyrics = result.lyrics;
                    ui.set_lyric_lines(ModelRc::from(Rc::new(VecModel::from(lyric_items))));
                    ui.set_lyrics_status(SharedString::from(result.status));
                }
            }
            let active_lyric = {
                let state = music_for_timer.borrow();
                state
                    .lyrics
                    .iter()
                    .rposition(|(start, _)| *start <= snapshot.position_ms)
                    .map_or(-1, |index| i32::try_from(index).unwrap_or(i32::MAX))
            };
            ui.set_active_lyric_index(active_lyric);

            let should_advance = {
                let state = music_for_timer.borrow();
                !state.repeat_current
                    && !state.auto_advanced
                    && state.crossfade_started.is_none()
                    && state.current_index.is_some()
                    && snapshot.duration_ms > 0
                    && snapshot.position_ms.saturating_add(150) >= snapshot.duration_ms
            };
            if should_advance {
                music_for_timer.borrow_mut().auto_advanced = true;
                let next_index = {
                    let state = music_for_timer.borrow();
                    next_music_index(&state)
                };
                if let Some(index) = next_index {
                    play_music_index(&ui, &media_for_timer, &music_for_timer, index);
                }
            }
        }
    });
    info!(
        version = env!("CARGO_PKG_VERSION"),
        "Zink Rust shell started"
    );
    let weak = ui.as_weak();
    let media_for_close = Rc::clone(&media);
    let music_for_close = Rc::clone(&music);
    ui.window().on_close_requested(move || {
        let position_ms = media_for_close
            .borrow()
            .transport_snapshot()
            .map_or(0, |snapshot| snapshot.position_ms);
        save_music_session(
            &music_for_close.borrow(),
            position_ms,
            media_for_close.borrow().volume(),
        );
        if let Some(ui) = weak.upgrade()
            && let Err(error) = save_window_placement(ui.window(), &window_placement_path)
        {
            error!(%error, "failed to save window placement");
        }
        slint::CloseRequestResponse::HideWindow
    });
    #[cfg(windows)]
    if browser_only {
        ui.show().context("initialize Zink window")?;
        zink_connect
            .borrow_mut()
            .open_browser_only()
            .context("open Zink Connect browser-only mode")?;
        ui.hide().context("hide the main Zink window")?;
        return slint::run_event_loop().context("run Zink Connect browser-only mode");
    }

    #[cfg(windows)]
    global_replay_hotkey::start(ui.as_weak(), replay_hotkey_enabled);

    drop(splash_process.take());
    ui.run().context("run Zink UI")
}

fn store_web_fallback(url: &str) -> Option<&'static str> {
    url.starts_with("ms-windows-store://")
        .then_some("https://apps.microsoft.com/store/detail/9NX0L20X52L7")
}

fn format_media_time(milliseconds: u64) -> String {
    let total_seconds = milliseconds / 1_000;
    let seconds = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    if total_minutes >= 60 {
        format!(
            "{}:{:02}:{seconds:02}",
            total_minutes / 60,
            total_minutes % 60
        )
    } else {
        format!("{total_minutes}:{seconds:02}")
    }
}

fn place_window_on_active_monitor(window: &slint::Window) {
    #[cfg(windows)]
    if let Some((work_x, work_y, work_width, work_height)) = active_monitor_work_area() {
        // Keep the comfortable desktop size, but never restore a rectangle larger than the
        // current monitor.  Positions are deliberately re-centred: saved absolute coordinates
        // become invalid after docking, DPI changes, remote desktop, or monitor removal.
        let (x, y, width, height) = centred_window_bounds(work_x, work_y, work_width, work_height);
        window.set_size(slint::PhysicalSize::new(width, height));
        window.set_position(slint::PhysicalPosition::new(x, y));
        return;
    }

    window.set_size(slint::LogicalSize::new(1_600.0, 920.0));
}

fn centred_window_bounds(
    work_x: i32,
    work_y: i32,
    work_width: u32,
    work_height: u32,
) -> (i32, i32, u32, u32) {
    // Zink's richer pages are designed as full desktop workspaces. Give them almost the full
    // physical work area while retaining a visible, centred Windows frame on every side. Using
    // physical pixels avoids the pre-show Slint scale factor differing from the DPI Windows
    // selects after placing the window on its destination monitor.
    let default_width = (work_width as f32 * 0.94).round() as u32;
    let default_height = (work_height as f32 * 0.92).round() as u32;
    let maximum_width = work_width.saturating_sub(24).max(640);
    let maximum_height = work_height.saturating_sub(24).max(480);
    let width = default_width.clamp(1_100.min(maximum_width), maximum_width);
    let height = default_height.clamp(680.min(maximum_height), maximum_height);
    let x = work_x + i32::try_from(work_width.saturating_sub(width) / 2).unwrap_or(0);
    let y = work_y + i32::try_from(work_height.saturating_sub(height) / 2).unwrap_or(0);
    (x, y, width, height)
}

#[cfg(windows)]
fn active_monitor_work_area() -> Option<(i32, i32, u32, u32)> {
    use windows::Win32::{
        Foundation::POINT,
        Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint},
        UI::WindowsAndMessaging::GetCursorPos,
    };

    let mut cursor = POINT::default();
    // SAFETY: `cursor` and `monitor_info` are valid writable structures for the duration of
    // these synchronous Win32 calls.
    unsafe {
        GetCursorPos(&mut cursor).ok()?;
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
        if monitor.is_invalid() {
            return None;
        }
        let mut monitor_info = MONITORINFO {
            cbSize: u32::try_from(std::mem::size_of::<MONITORINFO>()).ok()?,
            ..Default::default()
        };
        GetMonitorInfoW(monitor, &mut monitor_info).ok().ok()?;
        let work = monitor_info.rcWork;
        let width = u32::try_from(work.right.saturating_sub(work.left)).ok()?;
        let height = u32::try_from(work.bottom.saturating_sub(work.top)).ok()?;
        (width > 0 && height > 0).then_some((work.left, work.top, width, height))
    }
}

fn save_window_placement(window: &slint::Window, path: &Path) -> anyhow::Result<()> {
    if window.is_fullscreen() || window.is_maximized() {
        return Ok(());
    }
    let size = window.size();
    let position = window.position();
    let value = serde_json::json!({
        "x": position.x,
        "y": position.y,
        "width": size.width,
        "height": size.height,
    });
    std::fs::write(path, serde_json::to_vec_pretty(&value)?)
        .with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn is_audio_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp3" | "m4a" | "aac" | "wav" | "flac" | "wma"
            )
        })
}

fn is_video_path(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "mp4" | "m4v" | "mkv" | "mov" | "avi" | "wmv" | "webm" | "ts" | "mts"
            )
        })
}

fn music_track_from_path(path: &Path) -> MusicTrackState {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        music_track_from_path_inner(path)
    }))
    .unwrap_or_else(|_| {
        error!(path = %path.display(), "ignored invalid music metadata that caused the tag reader to panic");
        music_track_from_filename(path)
    })
}

fn music_track_from_filename(path: &Path) -> MusicTrackState {
    let stem = path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("Unknown track")
        .trim();
    let (artist, title) = stem.split_once(" - ").map_or_else(
        || (String::new(), stem.to_owned()),
        |(artist, title)| (artist.trim().to_owned(), title.trim().to_owned()),
    );
    MusicTrackState {
        path: path.to_path_buf(),
        title,
        artist,
        album: String::new(),
        technical_detail: path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or("Audio")
            .to_ascii_uppercase(),
        replay_gain_db: None,
        replay_peak: None,
    }
}

fn music_track_from_path_inner(path: &Path) -> MusicTrackState {
    let mut track = music_track_from_filename(path);
    if let Ok(tagged) = Probe::open(path).and_then(Probe::read) {
        if let Some(tag) = tagged.primary_tag().or_else(|| tagged.first_tag()) {
            if let Some(title) = tag.title().filter(|value| !value.trim().is_empty()) {
                title.trim().clone_into(&mut track.title);
            }
            if let Some(artist) = tag.artist().filter(|value| !value.trim().is_empty()) {
                artist.trim().clone_into(&mut track.artist);
            }
            if let Some(album) = tag.album().filter(|value| !value.trim().is_empty()) {
                album.trim().clone_into(&mut track.album);
            }
            track.replay_gain_db = tag
                .get_string(ItemKey::ReplayGainTrackGain)
                .and_then(parse_replaygain_number);
            track.replay_peak = tag
                .get_string(ItemKey::ReplayGainTrackPeak)
                .and_then(parse_replaygain_number);
        }
        let properties = tagged.properties();
        let codec = track.technical_detail.clone();
        let bitrate = properties
            .audio_bitrate()
            .map(|value| format!("{value} kbps"));
        let rate = properties.sample_rate().map(|value| {
            let khz = f64::from(value) / 1_000.0;
            format!("{khz:.1} kHz")
        });
        let depth = properties.bit_depth().map(|value| format!("{value}-bit"));
        let channels = properties.channels().map(|value| match value {
            1 => "Mono".to_owned(),
            2 => "Stereo".to_owned(),
            value => format!("{value} channels"),
        });
        track.technical_detail = std::iter::once(Some(codec))
            .chain([depth, rate, bitrate, channels])
            .flatten()
            .collect::<Vec<_>>()
            .join(" • ");
    }
    track
}

fn parse_replaygain_number(value: &str) -> Option<f64> {
    value
        .trim()
        .split_ascii_whitespace()
        .next()
        .and_then(|number| number.parse::<f64>().ok())
        .filter(|number| number.is_finite())
}

fn music_track_item(track: &MusicTrackState) -> MusicTrackItem {
    MusicTrackItem {
        path: SharedString::from(track.path.to_string_lossy().as_ref()),
        title: SharedString::from(track.title.as_str()),
        artist: SharedString::from(track.artist.as_str()),
        album: SharedString::from(track.album.as_str()),
    }
}

fn sync_music_models(ui: &AppWindow, state: &MusicPlayerState) {
    sync_music_models_parts(ui, &state.queue, state.current_index);
}

fn sync_music_models_parts(
    ui: &AppWindow,
    queue: &[MusicTrackState],
    current_index: Option<usize>,
) {
    let tracks = ModelRc::from(Rc::new(VecModel::from(
        queue.iter().map(music_track_item).collect::<Vec<_>>(),
    )));
    ui.set_music_queue(tracks.clone());
    ui.set_selected_music(tracks);
    ui.set_music_current_index(
        current_index.map_or(-1, |index| i32::try_from(index).unwrap_or(i32::MAX)),
    );
}

fn sync_music_models_shared(ui: &AppWindow, music: &Rc<RefCell<MusicPlayerState>>) {
    // Build an owned snapshot first, then release the RefCell guard before
    // assigning Slint properties. Property updates can synchronously re-enter
    // application callbacks on Windows, so retaining a guard across them can
    // otherwise make the subsequent playback start fail or panic.
    let (queue, current_index) = {
        let state = music.borrow();
        (state.queue.clone(), state.current_index)
    };
    sync_music_models_parts(ui, &queue, current_index);
}

fn reset_music_display(ui: &AppWindow) {
    ui.set_track_title(SharedString::from("Nothing playing"));
    ui.set_track_detail(SharedString::from("Choose music from this PC"));
    ui.set_track_album(SharedString::from("Your music will appear here"));
    ui.set_current_music_path(SharedString::new());
    ui.set_current_track_liked(false);
    ui.set_is_playing(false);
    ui.set_position_ratio(0.0);
    ui.set_position_text(SharedString::from("0:00"));
    ui.set_duration_text(SharedString::from("0:00"));
    ui.set_lyric_lines(ModelRc::from(Rc::new(VecModel::default())));
    ui.set_lyrics_status(SharedString::from(
        "Play a song to view synchronized lyrics.",
    ));
}

fn save_queue_playlist(folder: &Path, queue: &[MusicTrackState]) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(folder)
        .with_context(|| format!("create playlist folder {}", folder.display()))?;
    let path = folder.join("Saved Queue.m3u8");
    let mut contents = String::from("#EXTM3U\n");
    for track in queue {
        contents.push_str(&track.path.to_string_lossy());
        contents.push('\n');
    }
    std::fs::write(&path, contents).with_context(|| format!("write {}", path.display()))?;
    Ok(path)
}

fn load_music_session(path: &Path) -> SavedMusicSession {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_music_session(state: &MusicPlayerState, position_ms: u64, volume: f64) {
    // A fresh launch intentionally starts with an empty player. Do not let that
    // empty transient state overwrite the last meaningful session that the user
    // may choose to restore.
    if state.queue.is_empty() {
        return;
    }
    let session = SavedMusicSession {
        queue: state.queue.clone(),
        current_index: state.current_index,
        position_ms,
        volume: Some(volume),
        shuffle: state.shuffle,
        repeat_current: state.repeat_current,
        background: state.background,
        playback_rate: Some(state.playback_rate),
        crossfade_seconds: Some(state.crossfade_seconds),
        audio_device_id: Some(state.audio_device_id.clone()),
        normalization_enabled: Some(state.normalization_enabled),
        normalization_preamp_db: Some(state.normalization_preamp_db),
    };
    let result = (|| -> anyhow::Result<()> {
        let parent = state
            .session_file
            .parent()
            .context("music session path has no parent")?;
        std::fs::create_dir_all(parent)?;
        let temporary = state.session_file.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_vec_pretty(&session)?)?;
        if state.session_file.exists() {
            std::fs::remove_file(&state.session_file)?;
        }
        std::fs::rename(temporary, &state.session_file)?;
        Ok(())
    })();
    if let Err(error) = result {
        error!(%error, "failed to persist music session");
    }
}

fn restore_music_index(
    ui: &AppWindow,
    media: &Rc<RefCell<MediaEngine>>,
    music: &Rc<RefCell<MusicPlayerState>>,
    index: usize,
    position_ms: u64,
) {
    play_music_index(ui, media, music, index);
    music.borrow_mut().pending_restore_ms = position_ms;
    if let Err(error) = media.borrow_mut().pause() {
        error!(%error, "failed to pause restored music session");
    }
    ui.set_is_playing(false);
}

#[allow(clippy::too_many_lines)]
fn play_music_index(
    ui: &AppWindow,
    media: &Rc<RefCell<MediaEngine>>,
    music: &Rc<RefCell<MusicPlayerState>>,
    index: usize,
) {
    let (track, use_prepared) = {
        let state = music.borrow();
        (
            state.queue.get(index).cloned(),
            state.prepared_index == Some(index),
        )
    };
    let Some(track) = track else { return };
    let playback_result = if use_prepared {
        media.borrow_mut().play_prepared(&track.path)
    } else {
        media.borrow_mut().open(&track.path)
    };
    match playback_result {
        Ok(()) => {
            if let Err(error) = media.borrow().update_system_music_metadata(
                &track.title,
                if track.artist.is_empty() {
                    "Unknown artist"
                } else {
                    &track.artist
                },
                if track.album.is_empty() {
                    "Unknown album"
                } else {
                    &track.album
                },
            ) {
                error!(%error, path = %track.path.display(), "failed to publish Windows media metadata");
            }
            let (normalization_gain, normalization_peak) = {
                let state = music.borrow();
                if state.normalization_enabled {
                    (
                        track.replay_gain_db.unwrap_or(0.0) + state.normalization_preamp_db,
                        track.replay_peak,
                    )
                } else {
                    (0.0, None)
                }
            };
            if let Err(error) = media
                .borrow_mut()
                .set_normalization_gain(normalization_gain, normalization_peak)
            {
                error!(%error, path = %track.path.display(), "failed to apply track normalization");
            }
            let lyrics = load_local_lyrics(&track.path);
            let lyric_items = lyrics
                .iter()
                .map(|(start_ms, text)| LyricLineItem {
                    start_ms: i32::try_from(*start_ms).unwrap_or(i32::MAX),
                    text: SharedString::from(text.as_str()),
                })
                .collect::<Vec<_>>();
            let lyric_count = lyric_items.len();
            let has_timed_lyrics = lyrics.iter().any(|(start, _)| *start > 0);
            match music.try_borrow_mut() {
                Ok(mut state) => {
                    state.current_index = Some(index);
                    state.prepared_index = None;
                    state.crossfade_started = None;
                    state.crossfade_target_index = None;
                    state.auto_advanced = false;
                    state.lyrics = lyrics;
                }
                Err(error) => {
                    // A Slint callback may be re-entered while a caller still has a
                    // read guard. Playback must not terminate the entire process in
                    // that case; the next timer tick will continue normally.
                    error!(%error, index, "music state was busy while starting a queue item");
                }
            }
            ui.set_music_current_index(i32::try_from(index).unwrap_or(i32::MAX));
            ui.set_current_track_liked(music.borrow().liked_paths.contains(&track.path));
            ui.set_track_title(SharedString::from(track.title.as_str()));
            ui.set_track_detail(SharedString::from(if track.artist.is_empty() {
                "Unknown artist"
            } else {
                track.artist.as_str()
            }));
            if ui.get_music_fullscreen() {
                ui.set_artist_panel_open(true);
            }
            // Begin artist/release discovery as soon as the track starts rather
            // than waiting for the user to enter full screen.
            ui.invoke_request_artist_info(SharedString::from(if track.artist.is_empty() {
                "Unknown artist"
            } else {
                track.artist.as_str()
            }));
            ui.set_track_album(SharedString::from(if track.album.is_empty() {
                "Unknown Album"
            } else {
                track.album.as_str()
            }));
            ui.set_current_music_path(SharedString::from(track.path.to_string_lossy().as_ref()));
            ui.set_track_technical_detail(SharedString::from(track.technical_detail.as_str()));
            if let Some(artwork) = load_embedded_artwork(&track.path) {
                ui.set_artwork_accent(dominant_artwork_color(&artwork));
                ui.set_album_art(artwork);
            } else {
                if let Ok(fallback) = slint::Image::load_from_path(Path::new(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../../Assets/QuickAccess/music-player.png"
                ))) {
                    ui.set_artwork_accent(dominant_artwork_color(&fallback));
                    ui.set_album_art(fallback);
                }
                start_online_artwork_lookup(ui, track.clone());
            }
            ui.set_is_playing(true);
            ui.set_playback_error(SharedString::new());
            ui.set_active_lyric_index(-1);
            ui.set_lyric_lines(ModelRc::from(Rc::new(VecModel::from(lyric_items))));
            ui.set_lyrics_status(SharedString::from(if lyric_count == 0 {
                start_online_lyrics_lookup(track.clone());
                "Searching LRCLIB for synchronized lyrics…".to_owned()
            } else if has_timed_lyrics {
                "Local timed lyrics".to_owned()
            } else {
                "Local lyrics".to_owned()
            }));
            let state = music.borrow();
            save_music_session(&state, 0, media.borrow().volume());
            drop(state);
            let next_index = {
                let state = music.borrow();
                next_music_index(&state).filter(|next| *next != index)
            };
            if let Some(next_index) = next_index {
                let next_path = music
                    .borrow()
                    .queue
                    .get(next_index)
                    .map(|next| next.path.clone());
                if let Some(next_path) = next_path {
                    // Drop the MediaEngine borrow before doing the remaining
                    // prepared-track setup. A `borrow_mut()` used directly as a
                    // match scrutinee lives through the selected match arm and the
                    // second mutable borrow used to apply ReplayGain would panic.
                    let prepare_result = {
                        let mut engine = media.borrow_mut();
                        engine.prepare_next(&next_path)
                    };
                    match prepare_result {
                        Ok(()) => {
                            let (gain, peak) = {
                                let state = music.borrow();
                                let next = &state.queue[next_index];
                                if state.normalization_enabled {
                                    (
                                        next.replay_gain_db.unwrap_or(0.0)
                                            + state.normalization_preamp_db,
                                        next.replay_peak,
                                    )
                                } else {
                                    (0.0, None)
                                }
                            };
                            media
                                .borrow_mut()
                                .set_prepared_normalization_gain(gain, peak);
                            match music.try_borrow_mut() {
                                Ok(mut state) => state.prepared_index = Some(next_index),
                                Err(error) => {
                                    error!(%error, next_index, "music state was busy after preloading a queue item");
                                }
                            }
                        }
                        Err(error) => {
                            error!(%error, path = %next_path.display(), "failed to preload next track");
                        }
                    }
                }
            }
            info!(path = %track.path.display(), index, "playing music queue item");
        }
        Err(error) => {
            error!(%error, path = %track.path.display(), "failed to play music queue item");
            ui.set_playback_error(SharedString::from(error.to_string()));
        }
    }
}

fn next_music_index(state: &MusicPlayerState) -> Option<usize> {
    if state.queue.is_empty() {
        return None;
    }
    if state.shuffle && state.queue.len() > 1 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |duration| duration.subsec_nanos() as usize);
        let current = state.current_index.unwrap_or_default();
        return Some((current + 1 + nanos % (state.queue.len() - 1)) % state.queue.len());
    }
    Some((state.current_index.unwrap_or_default() + 1) % state.queue.len())
}

fn load_local_lyrics(media_path: &Path) -> Vec<(u64, String)> {
    let lrc_path = media_path.with_extension("lrc");
    let text_path = media_path.with_extension("txt");
    let (contents, timed) = if let Ok(contents) = std::fs::read_to_string(&lrc_path) {
        (contents, true)
    } else if let Ok(contents) = std::fs::read_to_string(&text_path) {
        (contents, false)
    } else {
        return Vec::new();
    };
    if !timed {
        return contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| (0, line.trim().to_owned()))
            .collect();
    }
    let mut lines = Vec::new();
    for line in contents.lines() {
        let mut rest = line.trim();
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(end) = after_open.find(']') else {
                break;
            };
            let timestamp = &after_open[..end];
            if let Some((minutes, seconds)) = timestamp.split_once(':')
                && let (Ok(minutes), Ok(seconds)) = (minutes.parse::<u64>(), seconds.parse::<f64>())
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let start_ms = ((minutes as f64 * 60.0 + seconds) * 1_000.0).round() as u64;
                timestamps.push(start_ms);
            }
            rest = &after_open[end + 1..];
        }
        let text = rest.trim();
        if !text.is_empty() {
            lines.extend(
                timestamps
                    .into_iter()
                    .map(|timestamp| (timestamp, text.to_owned())),
            );
        }
    }
    lines.sort_by_key(|(timestamp, _)| *timestamp);
    lines
}

fn start_online_lyrics_lookup(track: MusicTrackState) {
    std::thread::spawn(move || {
        let result = fetch_online_lyrics(&track);
        let (lyrics, status) = match result {
            Some(lyrics) if !lyrics.is_empty() => {
                (lyrics, "Synchronized lyrics from LRCLIB".to_owned())
            }
            _ => (
                Vec::new(),
                "No synchronized lyrics were found locally or on LRCLIB.".to_owned(),
            ),
        };
        let queue = ONLINE_LYRICS_RESULTS.get_or_init(|| Mutex::new(Vec::new()));
        if let Ok(mut queue) = queue.lock() {
            queue.push(OnlineLyricsResult {
                path: track.path,
                lyrics,
                status,
            });
        }
    });
}

fn fetch_online_lyrics(track: &MusicTrackState) -> Option<Vec<(u64, String)>> {
    use std::hash::{Hash, Hasher};

    if track.artist.trim().is_empty() || track.title.trim().is_empty() {
        return None;
    }
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/LyricsCache");
    std::fs::create_dir_all(&cache).ok()?;
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    track.artist.to_lowercase().hash(&mut hasher);
    track.title.to_lowercase().hash(&mut hasher);
    let cache_path = cache.join(format!("{:016x}.lrc", hasher.finish()));
    if let Ok(contents) = std::fs::read_to_string(&cache_path) {
        return Some(parse_lrc_contents(&contents));
    }
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("track_name", &track.title)
        .append_pair("artist_name", &track.artist)
        .append_pair("album_name", &track.album)
        .finish();
    let endpoint = format!("https://lrclib.net/api/search?{query}");
    let mut body = ureq::get(&endpoint)
        .header("User-Agent", "ZinkMusic/5.0 (Windows music player)")
        .call()
        .ok()?
        .into_body();
    let response: serde_json::Value = serde_json::from_slice(&body.read_to_vec().ok()?).ok()?;
    let synced = response.as_array()?.iter().find_map(|record| {
        record
            .get("syncedLyrics")
            .and_then(serde_json::Value::as_str)
            .filter(|lyrics| !lyrics.trim().is_empty())
    })?;
    std::fs::write(&cache_path, synced).ok()?;
    Some(parse_lrc_contents(synced))
}

fn parse_lrc_contents(contents: &str) -> Vec<(u64, String)> {
    let mut lines = Vec::new();
    for line in contents.lines() {
        let mut rest = line.trim();
        let mut timestamps = Vec::new();
        while let Some(after_open) = rest.strip_prefix('[') {
            let Some(end) = after_open.find(']') else {
                break;
            };
            let timestamp = &after_open[..end];
            if let Some((minutes, seconds)) = timestamp.split_once(':')
                && let (Ok(minutes), Ok(seconds)) = (minutes.parse::<u64>(), seconds.parse::<f64>())
            {
                #[allow(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    clippy::cast_precision_loss
                )]
                let start_ms = ((minutes as f64 * 60.0 + seconds) * 1_000.0).round() as u64;
                timestamps.push(start_ms);
            }
            rest = &after_open[end + 1..];
        }
        let text = rest.trim();
        if !text.is_empty() {
            lines.extend(
                timestamps
                    .into_iter()
                    .map(|timestamp| (timestamp, text.to_owned())),
            );
        }
    }
    lines.sort_by_key(|(timestamp, _)| *timestamp);
    lines
}

fn load_embedded_artwork(media_path: &Path) -> Option<slint::Image> {
    let artwork_path = cache_embedded_artwork(media_path)?;
    slint::Image::load_from_path(&artwork_path).ok()
}

fn cache_embedded_artwork(media_path: &Path) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let tagged = Probe::open(media_path).ok()?.read().ok()?;
    let picture = tagged
        .tags()
        .iter()
        .find_map(|tag| tag.pictures().first())?;
    // Reject implausibly large embedded pictures before sending them to the UI
    // decoder/GPU. Corrupt tags can otherwise advertise album art large enough
    // to terminate the process while a batch is being loaded.
    if picture.data().is_empty() || picture.data().len() > 16 * 1024 * 1024 {
        tracing::warn!(
            path = %media_path.display(),
            bytes = picture.data().len(),
            "ignored invalid or oversized embedded artwork"
        );
        return None;
    }
    let extension = picture
        .mime_type()
        .and_then(|mime| mime.as_str().split('/').next_back())
        .filter(|value| matches!(*value, "png" | "jpeg" | "jpg" | "webp"))
        .unwrap_or("jpg");
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    media_path.hash(&mut hasher);
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache");
    std::fs::create_dir_all(&cache).ok()?;
    let artwork_path = cache.join(format!("{:016x}.{extension}", hasher.finish()));
    if !artwork_path.exists() {
        std::fs::write(&artwork_path, picture.data()).ok()?;
    }
    Some(artwork_path)
}

fn library_online_artwork_path(request: &LibraryArtworkRequest) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache");
    std::fs::create_dir_all(&cache).ok()?;
    let mut hasher = DefaultHasher::new();
    "library-online-v1".hash(&mut hasher);
    request.artist.trim().to_lowercase().hash(&mut hasher);
    if request.album.trim().is_empty() {
        request.title.trim().to_lowercase().hash(&mut hasher);
    } else {
        request.album.trim().to_lowercase().hash(&mut hasher);
    }
    Some(cache.join(format!("{:016x}-library-online.jpg", hasher.finish())))
}

/// Return the original cached artwork without decoding it. The music player
/// keeps using this full-quality source, while library cards use a bounded
/// thumbnail so hundreds of rows do not each retain a multi-megabyte bitmap.
fn cached_library_artwork_source(request: &LibraryArtworkRequest) -> Option<PathBuf> {
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache");
    let mut hasher = DefaultHasher::new();
    request.path.hash(&mut hasher);
    let stem = format!("{:016x}", hasher.finish());
    for extension in ["png", "jpeg", "jpg", "webp"] {
        let path = cache.join(format!("{stem}.{extension}"));
        if path.exists() {
            return Some(path);
        }
    }
    library_online_artwork_path(request).filter(|path| path.exists())
}

fn library_thumbnail_path(source: &Path) -> Option<PathBuf> {
    let metadata = source.metadata().ok()?;
    let mut hasher = DefaultHasher::new();
    "library-thumbnail-v1".hash(&mut hasher);
    source.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata.modified().ok().hash(&mut hasher);
    let folder = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache/Thumbnails");
    std::fs::create_dir_all(&folder).ok()?;
    Some(folder.join(format!("{:016x}.png", hasher.finish())))
}

fn create_library_thumbnail(source: &Path) -> Option<PathBuf> {
    static THUMBNAIL_GATE: OnceLock<Mutex<()>> = OnceLock::new();
    let destination = library_thumbnail_path(source)?;
    if destination.exists() {
        return Some(destination);
    }
    // Serialising decode/resize prevents several very large embedded covers
    // from causing a transient memory spike during a first library scan.
    let _guard = THUMBNAIL_GATE.get_or_init(|| Mutex::new(())).lock().ok()?;
    if destination.exists() {
        return Some(destination);
    }
    let image = image::ImageReader::open(source).ok()?.decode().ok()?;
    let thumbnail = image.resize(384, 384, image::imageops::FilterType::Lanczos3);
    let temporary = destination.with_extension("png.tmp");
    thumbnail
        .save_with_format(&temporary, image::ImageFormat::Png)
        .ok()?;
    if std::fs::rename(&temporary, &destination).is_err() {
        let _ = std::fs::remove_file(temporary);
        return None;
    }
    Some(destination)
}

fn clean_library_lookup_text(value: &str) -> String {
    let mut cleaned = value.replace('_', " ");
    for suffix in [
        " (Official Video)",
        " (Official Music Video)",
        " [Official Video]",
        " [Official Music Video]",
        " (Official Audio)",
        " [Official Audio]",
        " (Lyrics)",
        " [Lyrics]",
        " (2)",
    ] {
        cleaned = cleaned.replace(suffix, "");
    }
    if let Some(index) = cleaned.to_lowercase().rfind(" aac ")
        && cleaned[index + 5..]
            .trim()
            .chars()
            .all(|value| value.is_ascii_digit())
    {
        cleaned.truncate(index);
    }
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn library_artwork_request(item: &MediaItem) -> LibraryArtworkRequest {
    let filename = item
        .path
        .file_stem()
        .and_then(std::ffi::OsStr::to_str)
        .unwrap_or(&item.title);
    let mut artist = clean_library_lookup_text(&item.artist);
    let mut title = clean_library_lookup_text(&item.title);
    if artist.is_empty() {
        let source = clean_library_lookup_text(filename);
        if let Some((filename_artist, filename_title)) = source.split_once(" - ") {
            artist.clear();
            artist.push_str(filename_artist.trim());
            title.clear();
            title.push_str(filename_title.trim());
        } else if title.contains(" - ")
            && let Some((title_artist, song_title)) = title.split_once(" - ")
        {
            let recovered_artist = title_artist.trim().to_owned();
            let recovered_title = song_title.trim().to_owned();
            artist = recovered_artist;
            title = recovered_title;
        }
    }
    let mut album = clean_library_lookup_text(&item.album);
    if album.eq_ignore_ascii_case(&title) {
        album.clear();
    }
    LibraryArtworkRequest {
        path: item.path.clone(),
        title,
        artist,
        album,
    }
}

fn load_library_artwork(request: &LibraryArtworkRequest) -> slint::Image {
    if let Some(source) = cached_library_artwork_source(request)
        && let Some(path) = library_thumbnail_path(&source)
        && path.exists()
        && let Ok(image) = slint::Image::load_from_path(&path)
    {
        return image;
    }
    slint::Image::default()
}

fn video_thumbnail_path(media_path: &Path) -> Option<PathBuf> {
    let metadata = media_path.metadata().ok()?;
    let mut hasher = DefaultHasher::new();
    "video-thumbnail-v1".hash(&mut hasher);
    media_path.hash(&mut hasher);
    metadata.len().hash(&mut hasher);
    metadata.modified().ok().hash(&mut hasher);
    let folder = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache/VideoThumbnails");
    std::fs::create_dir_all(&folder).ok()?;
    Some(folder.join(format!("{:016x}.png", hasher.finish())))
}

fn load_video_thumbnail(media_path: &Path) -> slint::Image {
    let Some(destination) = video_thumbnail_path(media_path) else {
        return slint::Image::default();
    };
    if !destination.exists()
        && let Err(error) = create_video_thumbnail(media_path, &destination)
    {
        tracing::debug!(path = %media_path.display(), %error, "video thumbnail unavailable");
    }
    slint::Image::load_from_path(&destination).unwrap_or_default()
}

#[cfg(windows)]
fn create_video_thumbnail(media_path: &Path, destination: &Path) -> Result<(), String> {
    use windows::{
        Storage::{
            FileProperties::{ThumbnailMode, ThumbnailOptions},
            StorageFile,
            Streams::DataReader,
        },
        core::HSTRING,
    };

    static VIDEO_THUMBNAIL_GATE: OnceLock<Mutex<()>> = OnceLock::new();
    let _guard = VIDEO_THUMBNAIL_GATE
        .get_or_init(|| Mutex::new(()))
        .lock()
        .map_err(|_| "video thumbnail lock was poisoned".to_owned())?;
    if destination.exists() {
        return Ok(());
    }

    let source = StorageFile::GetFileFromPathAsync(&HSTRING::from(media_path.as_os_str()))
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|error| error.to_string())?;
    let thumbnail = source
        .GetThumbnailAsync(
            ThumbnailMode::VideosView,
            512,
            ThumbnailOptions::UseCurrentScale,
        )
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|error| error.to_string())?;
    let size = usize::try_from(thumbnail.Size().map_err(|error| error.to_string())?)
        .map_err(|_| "video thumbnail is too large".to_owned())?;
    if size == 0 || size > 16 * 1024 * 1024 {
        return Err("video thumbnail stream was empty or oversized".to_owned());
    }
    let input = thumbnail
        .GetInputStreamAt(0)
        .map_err(|error| error.to_string())?;
    let reader = DataReader::CreateDataReader(&input).map_err(|error| error.to_string())?;
    reader
        .LoadAsync(u32::try_from(size).map_err(|_| "video thumbnail is too large".to_owned())?)
        .map_err(|error| error.to_string())?
        .join()
        .map_err(|error| error.to_string())?;
    let mut bytes = vec![0; size];
    reader
        .ReadBytes(&mut bytes)
        .map_err(|error| error.to_string())?;

    let decoded = image::load_from_memory(&bytes).map_err(|error| error.to_string())?;
    let thumbnail = decoded.resize(512, 288, image::imageops::FilterType::Lanczos3);
    let temporary = destination.with_extension("png.tmp");
    thumbnail
        .save_with_format(&temporary, image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    std::fs::rename(&temporary, destination).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        error.to_string()
    })?;
    Ok(())
}

#[cfg(not(windows))]
fn create_video_thumbnail(_media_path: &Path, _destination: &Path) -> Result<(), String> {
    Err("native video thumbnails are only available on Windows".to_owned())
}

fn fetch_library_online_artwork(request: &LibraryArtworkRequest) -> Option<PathBuf> {
    if request.artist.trim().is_empty()
        || (request.album.trim().is_empty() && request.title.trim().is_empty())
    {
        return None;
    }
    let artwork_path = library_online_artwork_path(request)?;
    if artwork_path.exists() {
        return Some(artwork_path);
    }
    let subject = if request.album.trim().is_empty() {
        &request.title
    } else {
        &request.album
    };
    let query = format!("{} {}", request.artist, subject);
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let entity = if request.album.trim().is_empty() {
        "song"
    } else {
        "album"
    };
    let endpoint = format!(
        "https://itunes.apple.com/search?media=music&entity={entity}&limit=1&term={encoded}"
    );
    let mut search_bytes = None;
    for attempt in 0..3 {
        match ureq::get(&endpoint)
            .header("User-Agent", "ZinkMusic/5.0 (Windows music library)")
            .call()
        {
            Ok(response) => {
                let mut body = response.into_body();
                search_bytes = body.read_to_vec().ok();
                break;
            }
            Err(error) => {
                tracing::debug!(%error, attempt, "library artwork catalogue lookup delayed");
                std::thread::sleep(Duration::from_secs(1_u64 << attempt));
            }
        }
    }
    let response: serde_json::Value = serde_json::from_slice(&search_bytes?).ok()?;
    let source = response
        .get("results")?
        .as_array()?
        .first()?
        .get("artworkUrl100")?
        .as_str()?
        .replace("100x100bb", "400x400bb")
        .replace("100x100", "400x400");
    let mut artwork = ureq::get(&source)
        .header("User-Agent", "ZinkMusic/5.0 (Windows music library)")
        .call()
        .ok()?
        .into_body();
    let bytes = artwork.read_to_vec().ok()?;
    if !(1_024..=16 * 1024 * 1024).contains(&bytes.len()) {
        return None;
    }
    std::fs::write(&artwork_path, bytes).ok()?;
    Some(artwork_path)
}

fn start_library_artwork_scan(ui: &AppWindow, requests: Vec<LibraryArtworkRequest>) {
    let scans = LIBRARY_ARTWORK_SCANS.get_or_init(|| Mutex::new(HashSet::new()));
    let pending = scans.lock().map_or_else(
        |_| Vec::new(),
        |mut scanned| {
            requests
                .into_iter()
                .filter(|request| scanned.insert(request.path.clone()))
                .collect::<Vec<_>>()
        },
    );
    if pending.is_empty() {
        return;
    }
    // Workers pop from the end, so reverse once to prioritise the cards that
    // are currently at the top of the sorted library.
    let mut pending = pending;
    pending.reverse();
    let pending = Arc::new(Mutex::new(pending));
    for _ in 0..8 {
        let weak = ui.as_weak();
        let pending = Arc::clone(&pending);
        std::thread::spawn(move || {
            loop {
                let request = pending.lock().ok().and_then(|mut queue| queue.pop());
                let Some(request) = request else { break };
                let embedded = cache_embedded_artwork(&request.path);
                let shared_online_artwork = embedded.is_none();
                let Some(artwork_path) =
                    embedded.or_else(|| fetch_library_online_artwork(&request))
                else {
                    continue;
                };
                let Some(thumbnail_path) = create_library_thumbnail(&artwork_path) else {
                    continue;
                };
                let expected_path = request.path.to_string_lossy().into_owned();
                let expected_artist = request.artist.clone();
                let expected_album = request.album.clone();
                let expected_title = request.title.clone();
                let weak = weak.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = weak.upgrade() else { return };
                    let Ok(artwork) = slint::Image::load_from_path(&thumbnail_path) else {
                        return;
                    };
                    let model = ui.get_library_items();
                    for index in 0..model.row_count() {
                        let Some(mut item) = model.row_data(index) else {
                            continue;
                        };
                        let same_online_release = shared_online_artwork
                            && item.artist.as_str().eq_ignore_ascii_case(&expected_artist)
                            && if expected_album.trim().is_empty() {
                                item.title.as_str().eq_ignore_ascii_case(&expected_title)
                            } else {
                                item.album.as_str().eq_ignore_ascii_case(&expected_album)
                            };
                        if item.path.as_str() == expected_path || same_online_release {
                            item.artwork = artwork.clone();
                            model.set_row_data(index, item);
                            if !shared_online_artwork {
                                break;
                            }
                        }
                    }
                });
            }
        });
    }
}

fn dominant_artwork_color(image: &slint::Image) -> slint::Color {
    let Some(buffer) = image.to_rgba8() else {
        return slint::Color::from_rgb_u8(56, 255, 102);
    };
    let mut red = 0_u64;
    let mut green = 0_u64;
    let mut blue = 0_u64;
    let mut count = 0_u64;
    for pixel in buffer.as_slice().iter().step_by(24) {
        let max = pixel.r.max(pixel.g).max(pixel.b);
        let min = pixel.r.min(pixel.g).min(pixel.b);
        if pixel.a > 180 && max > 55 && min < 235 && max.saturating_sub(min) > 18 {
            red += u64::from(pixel.r);
            green += u64::from(pixel.g);
            blue += u64::from(pixel.b);
            count += 1;
        }
    }
    if count == 0 {
        return slint::Color::from_rgb_u8(56, 255, 102);
    }
    #[allow(clippy::cast_possible_truncation)]
    slint::Color::from_rgb_u8(
        (red / count) as u8,
        (green / count) as u8,
        (blue / count) as u8,
    )
}

fn start_online_artwork_lookup(ui: &AppWindow, track: MusicTrackState) {
    if track.artist.trim().is_empty() || track.title.trim().is_empty() {
        return;
    }
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let Some(artwork_path) = fetch_online_artwork(&track) else {
            tracing::debug!(path = %track.path.display(), "online artwork unavailable");
            return;
        };
        let expected_path = track.path.to_string_lossy().into_owned();
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = weak.upgrade() else { return };
            if ui.get_current_music_path().as_str() != expected_path {
                return;
            }
            match slint::Image::load_from_path(&artwork_path) {
                Ok(artwork) => {
                    ui.set_artwork_accent(dominant_artwork_color(&artwork));
                    ui.set_album_art(artwork);
                }
                Err(error) => {
                    tracing::warn!(%error, path = %artwork_path.display(), "downloaded artwork could not be decoded");
                }
            }
        });
    });
}

fn fetch_online_artwork(track: &MusicTrackState) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    track.path.hash(&mut hasher);
    "itunes-artwork-v1".hash(&mut hasher);
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache");
    std::fs::create_dir_all(&cache).ok()?;
    let artwork_path = cache.join(format!("{:016x}-online.jpg", hasher.finish()));
    if artwork_path.exists() {
        return Some(artwork_path);
    }
    let query = format!("{} {}", track.artist, track.title);
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let endpoint =
        format!("https://itunes.apple.com/search?media=music&entity=song&limit=1&term={encoded}");
    let mut body = ureq::get(&endpoint).call().ok()?.into_body();
    let response: serde_json::Value = serde_json::from_slice(&body.read_to_vec().ok()?).ok()?;
    let small = response
        .get("results")?
        .as_array()?
        .first()?
        .get("artworkUrl100")?
        .as_str()?;
    let large = small
        .replace("100x100bb", "600x600bb")
        .replace("100x100", "600x600");
    let mut artwork = ureq::get(&large).call().ok()?.into_body();
    let bytes = artwork.read_to_vec().ok()?;
    if bytes.len() < 1_024 {
        return None;
    }
    std::fs::write(&artwork_path, bytes).ok()?;
    Some(artwork_path)
}

fn load_liked_paths(path: &Path) -> HashSet<PathBuf> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Vec<PathBuf>>(&bytes).ok())
        .unwrap_or_default()
        .into_iter()
        .collect()
}

fn save_liked_paths(path: &Path, liked_paths: &HashSet<PathBuf>) -> anyhow::Result<()> {
    let mut paths = liked_paths.iter().collect::<Vec<_>>();
    paths.sort();
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(&paths)?)
        .with_context(|| format!("write {}", temporary.display()))?;
    if path.exists() {
        std::fs::remove_file(path).with_context(|| format!("replace {}", path.display()))?;
    }
    std::fs::rename(&temporary, path).with_context(|| format!("save {}", path.display()))?;
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn fetch_artist_information(
    artist_query: &str,
) -> anyhow::Result<(String, String, Vec<ArtistReleaseData>)> {
    if let Some(cached) = load_cached_artist_information(artist_query) {
        return Ok(cached);
    }

    let lookup_name = primary_artist_name(artist_query);
    let encoded =
        url::form_urlencoded::byte_serialize(format!("artist:\"{lookup_name}\"").as_bytes())
            .collect::<String>();
    let search_url =
        format!("https://musicbrainz.org/ws/2/artist/?query={encoded}&fmt=json&limit=1");
    let mut search_body = artist_http_agent()
        .get(&search_url)
        .header("User-Agent", "ZinkMusic/5.0 (artist information feature)")
        .call()
        .context("search MusicBrainz artists")?
        .into_body();
    let search: serde_json::Value =
        serde_json::from_slice(&search_body.read_to_vec().context("read artist response")?)
            .context("decode artist response")?;
    let artist = search
        .get("artists")
        .and_then(serde_json::Value::as_array)
        .and_then(|artists| artists.first())
        .context("no matching artist was found")?;
    let id = artist
        .get("id")
        .and_then(serde_json::Value::as_str)
        .context("artist response did not contain an id")?;
    let name = artist
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(artist_query)
        .to_owned();
    let artist_type = artist
        .get("type")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Artist");
    let country = artist
        .get("country")
        .and_then(serde_json::Value::as_str)
        .map(|value| format!(" from {value}"))
        .unwrap_or_default();
    let disambiguation = artist
        .get("disambiguation")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .map(|value| format!(" {value}."))
        .unwrap_or_default();
    let begin = artist
        .pointer("/life-span/begin")
        .and_then(serde_json::Value::as_str);
    let active = begin.map_or_else(String::new, |year| format!(" Active since {year}."));
    let summary = format!("{artist_type}{country}.{disambiguation}{active}");

    let releases_url = format!(
        "https://musicbrainz.org/ws/2/release-group?artist={id}&type=album%7Csingle&fmt=json&limit=30"
    );
    let mut releases_body = artist_http_agent()
        .get(&releases_url)
        .header("User-Agent", "ZinkMusic/5.0 (artist information feature)")
        .call()
        .context("load MusicBrainz releases")?
        .into_body();
    let response: serde_json::Value = serde_json::from_slice(
        &releases_body
            .read_to_vec()
            .context("read artist releases")?,
    )
    .context("decode artist releases")?;
    let mut releases = response
        .get("release-groups")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|release| {
            Some(ArtistReleaseData {
                id: release.get("id")?.as_str()?.to_owned(),
                title: release.get("title")?.as_str()?.to_owned(),
                release_type: release
                    .get("primary-type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Release")
                    .to_owned(),
                release_date: release
                    .get("first-release-date")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("Date unknown")
                    .to_owned(),
                release_ids: release
                    .get("releases")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|edition| edition.get("id")?.as_str().map(str::to_owned))
                    .collect(),
                artwork_path: None,
            })
        })
        .collect::<Vec<_>>();
    releases.sort_by(|left, right| right.release_date.cmp(&left.release_date));
    releases.truncate(12);
    let result = (name, summary, releases);
    save_cached_artist_information(artist_query, &result);
    Ok(result)
}

fn artist_http_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_global(Some(Duration::from_secs(6)))
            .build()
            .into()
    })
}

fn primary_artist_name(artist: &str) -> &str {
    let lower = artist.to_ascii_lowercase();
    let separators = [" feat.", " featuring ", " ft.", " with ", ",", ";", " / "];
    separators
        .iter()
        .filter_map(|separator| lower.find(separator))
        .min()
        .map_or(artist.trim(), |index| artist[..index].trim())
}

fn artist_information_cache_path(artist: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    primary_artist_name(artist)
        .to_ascii_lowercase()
        .trim()
        .hash(&mut hasher);
    let folder = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtistCache");
    std::fs::create_dir_all(&folder).ok()?;
    Some(folder.join(format!("{:016x}.json", hasher.finish())))
}

fn load_cached_artist_information(
    artist: &str,
) -> Option<(String, String, Vec<ArtistReleaseData>)> {
    let path = artist_information_cache_path(artist)?;
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

fn save_cached_artist_information(
    artist: &str,
    information: &(String, String, Vec<ArtistReleaseData>),
) {
    let Some(path) = artist_information_cache_path(artist) else {
        return;
    };
    let temporary = path.with_extension("json.tmp");
    if let Ok(bytes) = serde_json::to_vec(information)
        && std::fs::write(&temporary, bytes).is_ok()
    {
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        if std::fs::rename(&temporary, &path).is_err() {
            let _ = std::fs::remove_file(temporary);
        }
    }
}

fn apply_artist_information(
    ui: &AppWindow,
    artist: &str,
    name: String,
    summary: String,
    releases: Vec<ArtistReleaseData>,
) {
    // Text and cached metadata are committed synchronously. Cover retrieval is
    // independent so a slow or missing image never delays the shelf itself.
    let releases_for_artwork = releases.clone();
    let release_items = releases
        .into_iter()
        .map(|release| ArtistReleaseItem {
            id: SharedString::from(release.id),
            title: SharedString::from(release.title),
            release_type: SharedString::from(release.release_type),
            release_date: SharedString::from(release.release_date),
            artwork: release
                .artwork_path
                .and_then(|path| slint::Image::load_from_path(&path).ok())
                .unwrap_or_default(),
        })
        .collect::<Vec<_>>();
    ui.set_artist_info_name(SharedString::from(name));
    ui.set_artist_info_summary(SharedString::from(summary));
    ui.set_artist_info_status(SharedString::from(format!(
        "{} recent release{} found",
        release_items.len(),
        if release_items.len() == 1 { "" } else { "s" }
    )));
    ui.set_artist_releases(ModelRc::from(Rc::new(VecModel::from(release_items))));

    for (index, release) in releases_for_artwork.into_iter().enumerate() {
        let weak = ui.as_weak();
        let artwork_artist = artist.to_owned();
        std::thread::spawn(move || {
            let Some(path) = fetch_release_artwork(
                &release.id,
                &release.release_ids,
                &release.title,
                &artwork_artist,
            ) else {
                return;
            };
            let _ = slint::invoke_from_event_loop(move || {
                let Some(ui) = weak.upgrade() else { return };
                if ui.get_track_detail().as_str() != artwork_artist {
                    return;
                }
                let Ok(artwork) = slint::Image::load_from_path(&path) else {
                    return;
                };
                let model = ui.get_artist_releases();
                if index >= model.row_count() {
                    return;
                }
                model.set_row_data(
                    index,
                    ArtistReleaseItem {
                        id: SharedString::from(release.id),
                        title: SharedString::from(release.title),
                        release_type: SharedString::from(release.release_type),
                        release_date: SharedString::from(release.release_date),
                        artwork,
                    },
                );
            });
        });
    }
}

fn fetch_release_artwork(
    release_group_id: &str,
    release_ids: &[String],
    release_title: &str,
    artist: &str,
) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    release_group_id.hash(&mut hasher);
    let cache = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache/Releases");
    std::fs::create_dir_all(&cache).ok()?;
    let artwork_path = cache.join(format!("{:016x}.jpg", hasher.finish()));
    if !artwork_path.exists() {
        let group_endpoint =
            format!("https://coverartarchive.org/release-group/{release_group_id}/front-250");
        if !download_release_artwork(&group_endpoint, &artwork_path) {
            if download_itunes_release_artwork(release_title, artist, &artwork_path) {
                return Some(artwork_path);
            }
            // Some release groups do not have a group-level front image even
            // though one or more physical/digital editions do. Try those real
            // editions before falling back to Zink's generated release tile.
            let found = release_ids.iter().take(8).any(|release_id| {
                let endpoint =
                    format!("https://coverartarchive.org/release/{release_id}/front-250");
                download_release_artwork(&endpoint, &artwork_path)
            });
            if !found {
                return None;
            }
        }
    }
    Some(artwork_path)
}

fn download_itunes_release_artwork(title: &str, artist: &str, destination: &Path) -> bool {
    let query = format!("{artist} {title}");
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let endpoint =
        format!("https://itunes.apple.com/search?media=music&entity=album&limit=3&term={encoded}");
    let Ok(response) = ureq::get(&endpoint).call() else {
        return false;
    };
    let mut body = response.into_body();
    let Ok(bytes) = body.read_to_vec() else {
        return false;
    };
    let Ok(response) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    let Some(source) = response
        .get("results")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .find(|result| {
            result
                .get("collectionName")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| name.eq_ignore_ascii_case(title))
        })
        .or_else(|| response.get("results")?.as_array()?.first())
        .and_then(|result| result.get("artworkUrl100"))
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    let source = source
        .replace("100x100bb", "600x600bb")
        .replace("100x100", "600x600");
    download_release_artwork(&source, destination)
}

fn download_release_artwork(endpoint: &str, destination: &Path) -> bool {
    for attempt in 0..2 {
        let response = ureq::get(endpoint)
            .header("User-Agent", "ZinkMusic/5.0 (release artwork feature)")
            .call();
        if let Ok(response) = response {
            let mut body = response.into_body();
            if let Ok(bytes) = body.read_to_vec()
                && (1_024..=16 * 1024 * 1024).contains(&bytes.len())
            {
                let temporary = destination.with_extension("jpg.tmp");
                if std::fs::write(&temporary, bytes).is_ok()
                    && std::fs::rename(&temporary, destination).is_ok()
                {
                    return true;
                }
                let _ = std::fs::remove_file(temporary);
            }
        }
        if attempt == 0 {
            std::thread::sleep(Duration::from_millis(175));
        }
    }
    false
}

#[cfg(windows)]
fn open_external_url(url: &str) -> anyhow::Result<()> {
    use windows::{
        Win32::UI::{Shell::ShellExecuteW, WindowsAndMessaging::SW_SHOWNORMAL},
        core::PCWSTR,
    };

    let operation = "open\0".encode_utf16().collect::<Vec<_>>();
    let target = format!("{url}\0").encode_utf16().collect::<Vec<_>>();
    let result = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(operation.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    if result.0 as isize <= 32 {
        anyhow::bail!(
            "Windows rejected the release URL (code {})",
            result.0 as isize
        );
    }
    Ok(())
}

#[cfg(not(windows))]
fn open_external_url(_url: &str) -> anyhow::Result<()> {
    anyhow::bail!("opening release links is supported only on Windows")
}

#[cfg(windows)]
fn configure_startup_registration(enabled: bool) -> anyhow::Result<()> {
    let key = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
    let status = if enabled {
        let executable = std::env::current_exe().context("resolve Zink executable")?;
        let value = format!("\"{}\"", executable.display());
        std::process::Command::new("reg.exe")
            .args(["add", key, "/v", "Zink", "/t", "REG_SZ", "/d"])
            .arg(value)
            .arg("/f")
            .status()
            .context("register Zink startup entry")?
    } else {
        std::process::Command::new("reg.exe")
            .args(["delete", key, "/v", "Zink", "/f"])
            .status()
            .context("remove Zink startup entry")?
    };
    anyhow::ensure!(status.success(), "reg.exe returned {status}");
    Ok(())
}

#[cfg(not(windows))]
fn configure_startup_registration(_enabled: bool) -> anyhow::Result<()> {
    Ok(())
}

fn reset_fps_ui(ui: &AppWindow) {
    ui.set_fps_current(SharedString::from("--"));
    ui.set_fps_frame_time(SharedString::from("-- ms"));
    ui.set_fps_session_time(SharedString::from("00:00"));
    ui.set_fps_average(SharedString::from("--"));
    ui.set_fps_one_percent_low(SharedString::from("--"));
    ui.set_fps_point_one_percent_low(SharedString::from("--"));
    ui.set_fps_frame_count(0);
    ui.set_fps_recording(false);
}

fn refresh_fps_processes(ui: &AppWindow) {
    let entries = enumerate_fps_windows();
    let rows = entries
        .iter()
        .map(|(pid, title)| FpsProcessItem {
            pid: i32::try_from(*pid).unwrap_or(i32::MAX),
            title: SharedString::from(title.clone()),
        })
        .collect::<Vec<_>>();
    ui.set_fps_process_items(ModelRc::new(VecModel::from(rows)));
    if ui.get_fps_selected_pid() == 0 {
        ui.set_fps_status(SharedString::from(if entries.is_empty() {
            "No eligible game windows were found. Start a game, then refresh."
        } else {
            "Choose a running game or full-screen app to begin monitoring."
        }));
    }
}

fn fps_recordings_folder() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|directories| directories.video_dir().map(Path::to_path_buf))
        .unwrap_or_else(|| std::env::temp_dir().join("Zink"))
        .join("Zink FPS Recorder")
}

fn create_fps_recording(target: &str) -> anyhow::Result<(File, PathBuf)> {
    let folder = fps_recordings_folder();
    std::fs::create_dir_all(&folder).context("create FPS recording folder")?;
    let safe_target = target
        .chars()
        .map(|character| {
            if matches!(
                character,
                '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*'
            ) {
                '_'
            } else {
                character
            }
        })
        .collect::<String>();
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let path = folder.join(format!(
        "zink-fps-{}-{timestamp}.csv",
        safe_target.trim().trim_matches('.').replace(' ', "-")
    ));
    let mut file = File::create(&path).context("create FPS recording")?;
    writeln!(
        file,
        "elapsedMs,fps,averageFps,onePercentLowFps,pointOnePercentLowFps,frameTimeMs,frameCount,target"
    )
    .context("write FPS recording header")?;
    Ok((file, path))
}

fn update_fps_monitor(ui: &AppWindow, monitor: &mut FpsMonitorState, sample: RtssFpsSample) {
    if sample.sample_key == monitor.last_sample_key {
        return;
    }
    monitor.last_sample_key = sample.sample_key;
    monitor.samples.push(sample.fps);
    let sample_window = usize::try_from(ui.get_fps_sample_window().clamp(1, 10))
        .unwrap_or(3)
        .saturating_mul(5);
    if monitor.samples.len() > sample_window {
        let excess = monitor.samples.len().saturating_sub(sample_window);
        monitor.samples.drain(..excess);
    }
    monitor.frame_count = monitor.frame_count.saturating_add(1);

    let sample_count = u32::try_from(monitor.samples.len()).unwrap_or(u32::MAX);
    let average = monitor.samples.iter().sum::<f64>() / f64::from(sample_count);
    let mut ordered = monitor.samples.clone();
    ordered.sort_by(f64::total_cmp);
    let low_average = |divisor: usize| {
        let count = ordered.len().div_ceil(divisor).max(1).min(ordered.len());
        let count_u32 = u32::try_from(count).unwrap_or(u32::MAX);
        ordered[..count].iter().sum::<f64>() / f64::from(count_u32)
    };
    let one_percent_low = low_average(100);
    let point_one_percent_low = low_average(1_000);
    let elapsed = monitor
        .started
        .map_or(Duration::ZERO, |start| start.elapsed());
    let elapsed_ms = elapsed.as_millis();

    ui.set_fps_current(SharedString::from(format!("{:.0}", sample.fps)));
    ui.set_fps_frame_time(SharedString::from(format!(
        "{:.2} ms",
        sample.frame_time_ms
    )));
    ui.set_fps_session_time(SharedString::from(format!(
        "{:02}:{:02}",
        elapsed.as_secs() / 60,
        elapsed.as_secs() % 60
    )));
    ui.set_fps_average(SharedString::from(format!("{average:.0}")));
    ui.set_fps_one_percent_low(SharedString::from(format!("{one_percent_low:.0}")));
    ui.set_fps_point_one_percent_low(SharedString::from(format!("{point_one_percent_low:.0}")));
    ui.set_fps_frame_count(i32::try_from(monitor.frame_count).unwrap_or(i32::MAX));
    let target = f64::from(ui.get_fps_target_rate().clamp(30, 144));
    ui.set_fps_status(SharedString::from(if sample.fps + 0.5 >= target {
        format!("On target • monitoring through RTSS shared memory at {target:.0} FPS.")
    } else {
        format!(
            "Below the {target:.0} FPS target by {:.0} FPS.",
            target - sample.fps
        )
    }));

    if let Some(recording) = monitor.recording.as_mut() {
        let escaped_target = monitor.target_name.replace('"', "\"\"");
        if writeln!(
            recording,
            "{elapsed_ms},{:.3},{average:.3},{one_percent_low:.3},{point_one_percent_low:.3},{:.3},{},\"{escaped_target}\"",
            sample.fps,
            sample.frame_time_ms,
            monitor.frame_count
        )
        .is_err()
        {
            monitor.recording = None;
            ui.set_fps_recording(false);
            ui.set_fps_status(SharedString::from(
                "FPS capture stopped because the CSV file could not be written.",
            ));
        }
    }
}

#[cfg(windows)]
fn enumerate_fps_windows() -> Vec<(u32, String)> {
    use windows::Win32::{
        Foundation::{HWND, LPARAM},
        UI::WindowsAndMessaging::{
            EnumWindows, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
            IsWindowVisible,
        },
    };
    use windows::core::BOOL;

    unsafe extern "system" fn collect_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let length = unsafe { GetWindowTextLengthW(hwnd) };
        if length <= 0 {
            return BOOL(1);
        }
        let Ok(length) = usize::try_from(length) else {
            return BOOL(1);
        };
        let mut title = vec![0u16; length + 1];
        let copied = unsafe { GetWindowTextW(hwnd, &mut title) };
        if copied <= 0 {
            return BOOL(1);
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        if pid == 0 || pid == std::process::id() {
            return BOOL(1);
        }
        let Ok(copied) = usize::try_from(copied) else {
            return BOOL(1);
        };
        let title = String::from_utf16_lossy(&title[..copied]);
        let entries = unsafe { &mut *(parameter.0 as *mut Vec<(u32, String)>) };
        entries.push((pid, title));
        BOOL(1)
    }

    let mut entries = Vec::<(u32, String)>::new();
    let parameter = LPARAM(std::ptr::from_mut(&mut entries).cast::<()>() as isize);
    unsafe {
        let _ = EnumWindows(Some(collect_window), parameter);
    }
    entries.sort_by(|left, right| left.1.to_lowercase().cmp(&right.1.to_lowercase()));
    entries.dedup_by_key(|entry| entry.0);
    entries
}

#[cfg(windows)]
fn window_handle_for_pid(target_pid: u32) -> Option<isize> {
    use windows::Win32::{
        Foundation::{HWND, LPARAM},
        UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId, IsWindowVisible},
    };
    use windows::core::BOOL;

    struct Search {
        pid: u32,
        hwnd: Option<isize>,
    }

    unsafe extern "system" fn find_window(hwnd: HWND, parameter: LPARAM) -> BOOL {
        let search = unsafe { &mut *(parameter.0 as *mut Search) };
        if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
            return BOOL(1);
        }
        let mut pid = 0u32;
        unsafe { GetWindowThreadProcessId(hwnd, Some(&raw mut pid)) };
        if pid == search.pid {
            search.hwnd = Some(hwnd.0 as isize);
            return BOOL(0);
        }
        BOOL(1)
    }

    let mut search = Search {
        pid: target_pid,
        hwnd: None,
    };
    let parameter = LPARAM(std::ptr::from_mut(&mut search).cast::<()>() as isize);
    unsafe {
        let _ = EnumWindows(Some(find_window), parameter);
    }
    search.hwnd
}

#[cfg(not(windows))]
fn enumerate_fps_windows() -> Vec<(u32, String)> {
    Vec::new()
}

#[cfg(not(windows))]
fn window_handle_for_pid(_target_pid: u32) -> Option<isize> {
    None
}

#[cfg(windows)]
unsafe fn parse_rtss_fps_table(
    base: *const u8,
    target_pid: u32,
) -> Result<Option<RtssFpsSample>, String> {
    unsafe fn read_u32(base: *const u8, offset: usize) -> u32 {
        unsafe { std::ptr::read_unaligned(base.add(offset).cast::<u32>()) }
    }

    let signature = unsafe { read_u32(base, 0) };
    let version = unsafe { read_u32(base, 4) };
    if signature != 0x5254_5353 || version < 0x0002_0000 {
        return Err("The running RTSS shared-memory format is not supported.".to_owned());
    }
    let entry_size = unsafe { read_u32(base, 8) } as usize;
    let array_offset = unsafe { read_u32(base, 12) } as usize;
    let array_size = unsafe { read_u32(base, 16) } as usize;
    if !(284..=1_048_576).contains(&entry_size)
        || !(20..=67_108_864).contains(&array_offset)
        || array_size > 4_096
    {
        return Err("RTSS published an invalid application table.".to_owned());
    }
    for index in 0..array_size {
        let Some(offset) = array_offset.checked_add(index.saturating_mul(entry_size)) else {
            break;
        };
        if unsafe { read_u32(base, offset) } != target_pid {
            continue;
        }
        let time0 = unsafe { read_u32(base, offset + 268) };
        let time1 = unsafe { read_u32(base, offset + 272) };
        let frames = unsafe { read_u32(base, offset + 276) };
        let frame_time_microseconds = unsafe { read_u32(base, offset + 280) };
        let period_fps = if time1 > time0 {
            1000.0 * f64::from(frames) / f64::from(time1 - time0)
        } else {
            0.0
        };
        let instant_fps = if frame_time_microseconds > 0 {
            1_000_000.0 / f64::from(frame_time_microseconds)
        } else {
            0.0
        };
        let reasonable = |fps: f64| fps.is_finite() && (1.0..=500.0).contains(&fps);
        let fps = if reasonable(instant_fps)
            && (!reasonable(period_fps)
                || instant_fps <= period_fps * 2.0
                || instant_fps - period_fps <= 120.0)
        {
            instant_fps
        } else if reasonable(period_fps) {
            period_fps
        } else {
            return Ok(None);
        };
        let frame_time_ms = if frame_time_microseconds > 0 {
            f64::from(frame_time_microseconds) / 1000.0
        } else {
            1000.0 / fps
        };
        return Ok(Some(RtssFpsSample {
            fps,
            frame_time_ms,
            sample_key: (u64::from(time1) << 32) | u64::from(frames),
        }));
    }
    Ok(None)
}

#[cfg(windows)]
fn read_rtss_fps(target_pid: u32) -> Result<Option<RtssFpsSample>, String> {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Memory::{FILE_MAP_READ, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile},
        },
        core::w,
    };

    let mapping = unsafe { OpenFileMappingW(FILE_MAP_READ.0, false, w!("RTSSSharedMemoryV2")) }
        .map_err(|_| "RTSS shared memory was not found. Start RTSS, then try again.".to_owned())?;
    let view = unsafe { MapViewOfFile(mapping, FILE_MAP_READ, 0, 0, 0) };
    if view.Value.is_null() {
        unsafe {
            let _ = CloseHandle(mapping);
        }
        return Err("RTSS shared memory could not be mapped.".to_owned());
    }
    let result = unsafe { parse_rtss_fps_table(view.Value.cast::<u8>(), target_pid) };
    unsafe {
        let _ = UnmapViewOfFile(view);
        let _ = CloseHandle(mapping);
    }
    result
}

#[cfg(not(windows))]
fn read_rtss_fps(_target_pid: u32) -> Result<Option<RtssFpsSample>, String> {
    Err("FPS monitoring through RTSS is available only on Windows.".to_owned())
}

fn sort_library_items(items: &mut [MediaItem], mode: &str) {
    match mode {
        "Recently added" => items.sort_by_key(|item| {
            std::cmp::Reverse(
                item.path
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .unwrap_or(UNIX_EPOCH),
            )
        }),
        "Title A–Z" => {
            items.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()));
        }
        "Title Z–A" => {
            items.sort_by(|left, right| right.title.to_lowercase().cmp(&left.title.to_lowercase()));
        }
        "Longest first" => items.sort_by_key(|item| std::cmp::Reverse(item.duration_ms)),
        "Shortest first" => items.sort_by_key(|item| item.duration_ms),
        "File type" => items.sort_by(|left, right| {
            left.path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default()
                .to_lowercase()
                .cmp(
                    &right
                        .path
                        .extension()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default()
                        .to_lowercase(),
                )
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        }),
        "Artist" => items.sort_by(|left, right| {
            left.artist
                .to_lowercase()
                .cmp(&right.artist.to_lowercase())
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        }),
        "Album" => items.sort_by(|left, right| {
            left.album
                .to_lowercase()
                .cmp(&right.album.to_lowercase())
                .then_with(|| left.title.to_lowercase().cmp(&right.title.to_lowercase()))
        }),
        "Duration" => items.sort_by_key(|item| item.duration_ms),
        _ => {
            items.sort_by(|left, right| left.title.to_lowercase().cmp(&right.title.to_lowercase()));
        }
    }
}

fn format_library_duration(duration_ms: u64) -> String {
    let minutes = duration_ms / 60_000;
    if minutes >= 60 {
        format!("{}h {}m", minutes / 60, minutes % 60)
    } else {
        format!("{minutes} min")
    }
}

#[allow(clippy::too_many_lines)]
fn refresh_library(
    ui: &AppWindow,
    database: &Rc<RefCell<rusqlite::Connection>>,
    query: &str,
    media_kind: Option<&str>,
) {
    match list_media(&database.borrow(), media_kind) {
        Ok(mut items) => {
            if media_kind == Some("audio") || media_kind.is_none() {
                let audio_items = items
                    .iter()
                    .filter(|item| item.media_kind == "audio")
                    .collect::<Vec<_>>();
                let albums = audio_items
                    .iter()
                    .filter(|item| !item.album.trim().is_empty())
                    .map(|item| item.album.to_lowercase())
                    .collect::<HashSet<_>>()
                    .len();
                let artists = audio_items
                    .iter()
                    .filter(|item| !item.artist.trim().is_empty())
                    .map(|item| item.artist.to_lowercase())
                    .collect::<HashSet<_>>()
                    .len();
                let total_duration = audio_items.iter().map(|item| item.duration_ms).sum();
                ui.set_library_track_count(i32::try_from(audio_items.len()).unwrap_or(i32::MAX));
                ui.set_library_album_count(i32::try_from(albums).unwrap_or(i32::MAX));
                ui.set_library_artist_count(i32::try_from(artists).unwrap_or(i32::MAX));
                ui.set_library_total_duration(SharedString::from(format_library_duration(
                    total_duration,
                )));
            }
            let query = query.trim().to_lowercase();
            items.retain(|item| {
                query.is_empty()
                    || item.title.to_lowercase().contains(&query)
                    || item.artist.to_lowercase().contains(&query)
                    || item.album.to_lowercase().contains(&query)
                    || item.path.to_string_lossy().to_lowercase().contains(&query)
                    || format_media_time(item.duration_ms).contains(&query)
            });
            if media_kind.is_none() {
                let mut albums: BTreeMap<
                    String,
                    (String, String, PathBuf, LibraryArtworkRequest, usize),
                > = BTreeMap::new();
                let mut artists: BTreeMap<String, (String, PathBuf, LibraryArtworkRequest, usize)> =
                    BTreeMap::new();
                for item in items.iter().filter(|item| item.media_kind == "audio") {
                    let artwork = library_artwork_request(item);
                    if !item.album.trim().is_empty() {
                        let entry = albums.entry(item.album.to_lowercase()).or_insert_with(|| {
                            (
                                item.album.clone(),
                                item.artist.clone(),
                                item.path.clone(),
                                artwork.clone(),
                                0,
                            )
                        });
                        entry.4 += 1;
                    }
                    if !item.artist.trim().is_empty() {
                        let entry =
                            artists
                                .entry(item.artist.to_lowercase())
                                .or_insert_with(|| {
                                    (item.artist.clone(), item.path.clone(), artwork.clone(), 0)
                                });
                        entry.3 += 1;
                    }
                }
                ui.set_search_albums(ModelRc::from(Rc::new(VecModel::from(
                    albums
                        .into_values()
                        .map(|(name, artist, path, artwork, count)| SearchFacetItem {
                            name: SharedString::from(name),
                            detail: SharedString::from(format!(
                                "{} • {count} {}",
                                if artist.is_empty() {
                                    "Unknown artist"
                                } else {
                                    &artist
                                },
                                if count == 1 { "song" } else { "songs" }
                            )),
                            artwork: load_library_artwork(&artwork),
                            path: SharedString::from(path.to_string_lossy().as_ref()),
                        })
                        .collect::<Vec<_>>(),
                ))));
                ui.set_search_artists(ModelRc::from(Rc::new(VecModel::from(
                    artists
                        .into_values()
                        .map(|(name, path, artwork, count)| SearchFacetItem {
                            name: SharedString::from(name),
                            detail: SharedString::from(format!(
                                "{count} {} in your library",
                                if count == 1 { "song" } else { "songs" }
                            )),
                            artwork: load_library_artwork(&artwork),
                            path: SharedString::from(path.to_string_lossy().as_ref()),
                        })
                        .collect::<Vec<_>>(),
                ))));
                let stations = [
                    (
                        "Capital FM",
                        "Pop & chart",
                        "https://media-ssl.musicradio.com/CapitalUK",
                    ),
                    (
                        "Heart",
                        "Feel-good music",
                        "https://media-ssl.musicradio.com/HeartUK",
                    ),
                    (
                        "Classic FM",
                        "Classical",
                        "https://media-ssl.musicradio.com/ClassicFM",
                    ),
                    (
                        "LBC",
                        "News & talk",
                        "https://media-ssl.musicradio.com/LBCUK",
                    ),
                    (
                        "talkSPORT",
                        "Live sport",
                        "https://radio.talksport.com/stream",
                    ),
                    (
                        "Virgin Radio UK",
                        "Music",
                        "https://radio.virginradio.co.uk/stream",
                    ),
                ]
                .into_iter()
                .filter(|(name, genre, _)| {
                    query.is_empty()
                        || name.to_lowercase().contains(&query)
                        || genre.to_lowercase().contains(&query)
                })
                .map(|(name, genre, stream_url)| SearchStationItem {
                    name: SharedString::from(name),
                    genre: SharedString::from(genre),
                    stream_url: SharedString::from(stream_url),
                    user_defined: false,
                })
                .collect::<Vec<_>>();
                ui.set_search_stations(ModelRc::from(Rc::new(VecModel::from(stations))));
            }
            sort_library_items(&mut items, ui.get_library_sort_mode().as_str());
            let count = items.len();
            let artwork_requests = items
                .iter()
                .filter(|item| item.media_kind == "audio")
                .map(library_artwork_request)
                .collect::<Vec<_>>();
            let model = items
                .into_iter()
                .map(|item| {
                    let artwork_request = library_artwork_request(&item);
                    LibraryItem {
                        artwork: if item.media_kind == "video" {
                            load_video_thumbnail(&item.path)
                        } else {
                            load_library_artwork(&artwork_request)
                        },
                        path: SharedString::from(item.path.to_string_lossy().as_ref()),
                        title: SharedString::from(item.title),
                        artist: SharedString::from(item.artist),
                        album: SharedString::from(item.album),
                        media_kind: SharedString::from(item.media_kind),
                        duration: if item.duration_ms == 0 {
                            SharedString::default()
                        } else {
                            SharedString::from(format_media_time(item.duration_ms))
                        },
                    }
                })
                .collect::<Vec<_>>();
            ui.set_library_items(ModelRc::from(Rc::new(VecModel::from(model))));
            ui.set_library_status(SharedString::from(format!("{count} indexed media items")));
            if media_kind == Some("audio") {
                start_library_artwork_scan(ui, artwork_requests);
            }
        }
        Err(error) => {
            error!(%error, "failed to read media library");
            ui.set_library_status(SharedString::from(error.to_string()));
        }
    }
}

fn recordings_folder() -> PathBuf {
    directories::UserDirs::new()
        .and_then(|dirs| dirs.video_dir().map(Path::to_path_buf))
        .map_or_else(
            || std::env::temp_dir().join("Zink").join("Captures"),
            |videos| videos.join("Captures"),
        )
}

#[cfg(windows)]
fn recycle_file(path: &Path) -> anyhow::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::UI::Shell::{
        FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_NOERRORUI, FOF_SILENT, SHFILEOPSTRUCTW,
        SHFileOperationW,
    };
    use windows::core::PCWSTR;

    if !path.is_file() {
        anyhow::bail!("the recording no longer exists");
    }
    let mut source = path.as_os_str().encode_wide().collect::<Vec<_>>();
    source.extend([0, 0]);
    let flags = u16::try_from((FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_NOERRORUI | FOF_SILENT).0)
        .context("invalid Recycle Bin flags")?;
    let mut operation = SHFILEOPSTRUCTW {
        wFunc: FO_DELETE,
        pFrom: PCWSTR(source.as_ptr()),
        fFlags: flags,
        ..Default::default()
    };
    let result = unsafe { SHFileOperationW(&raw mut operation) };
    if result != 0 || operation.fAnyOperationsAborted.as_bool() {
        anyhow::bail!("Windows Recycle Bin operation failed with code {result}");
    }
    Ok(())
}

#[cfg(not(windows))]
fn recycle_file(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("the Recycle Bin is available on Windows only")
}

fn refresh_recordings(ui: &AppWindow) {
    let folder = recordings_folder();
    if let Err(error) = std::fs::create_dir_all(&folder) {
        ui.set_playback_error(SharedString::from(format!(
            "Could not open the recordings library: {error}"
        )));
        return;
    }
    let mut paths = std::fs::read_dir(&folder)
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_file() && is_video_path(path))
        .collect::<Vec<_>>();
    paths.sort_by(|left, right| {
        let left_modified = std::fs::metadata(left)
            .and_then(|value| value.modified())
            .ok();
        let right_modified = std::fs::metadata(right)
            .and_then(|value| value.modified())
            .ok();
        right_modified
            .cmp(&left_modified)
            .then_with(|| left.cmp(right))
    });
    let items = paths
        .into_iter()
        .map(|path| LibraryItem {
            artwork: load_video_thumbnail(&path),
            title: SharedString::from(
                path.file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Recording"),
            ),
            artist: SharedString::from("Zink recording"),
            album: SharedString::from("Recordings"),
            media_kind: SharedString::from("video"),
            duration: SharedString::from(std::fs::metadata(&path).map_or_else(
                |_| String::new(),
                |metadata| {
                    let size = format_file_size(metadata.len());
                    metadata.modified().map_or(size.clone(), |modified| {
                        format!("{size}  •  {}", format_recording_age(modified))
                    })
                },
            )),
            path: SharedString::from(path.to_string_lossy().as_ref()),
        })
        .collect::<Vec<_>>();
    ui.set_recording_items(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn format_recording_age(modified: SystemTime) -> String {
    let elapsed = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::ZERO);
    let minutes = elapsed.as_secs() / 60;
    if minutes < 1 {
        "Just now".into()
    } else if minutes < 60 {
        format!("{minutes} min ago")
    } else if minutes < 1_440 {
        format!("{} hr ago", minutes / 60)
    } else {
        format!("{} days ago", minutes / 1_440)
    }
}

#[allow(clippy::cast_precision_loss)]
fn format_file_size(bytes: u64) -> String {
    const KIB: u64 = 1_024;
    const MIB: u64 = KIB * 1_024;
    const GIB: u64 = MIB * 1_024;
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

fn refresh_music_collections(ui: &AppWindow, database: &Rc<RefCell<rusqlite::Connection>>) {
    match list_playlists(&database.borrow()) {
        Ok(playlists) => {
            let items = playlists
                .into_iter()
                .map(|playlist| PlaylistItem {
                    name: SharedString::from(playlist.name),
                    detail: SharedString::from(format!("{} tracks", playlist.track_count)),
                })
                .collect::<Vec<_>>();
            ui.set_music_playlists(ModelRc::from(Rc::new(VecModel::from(items))));
        }
        Err(error) => error!(%error, "failed to refresh playlists"),
    }
    match list_recently_played(&database.borrow(), 12) {
        Ok(items) => {
            let tracks = items
                .into_iter()
                .map(|item| MusicTrackItem {
                    path: SharedString::from(item.path.to_string_lossy().as_ref()),
                    title: SharedString::from(item.title),
                    artist: SharedString::from(item.artist),
                    album: SharedString::from(item.album),
                })
                .collect::<Vec<_>>();
            ui.set_recently_played(ModelRc::from(Rc::new(VecModel::from(tracks))));
        }
        Err(error) => error!(%error, "failed to refresh playback history"),
    }
    match listening_summary(&database.borrow()) {
        Ok(summary) => {
            let total_minutes = summary.listening_ms / 60_000;
            let time = if total_minutes >= 60 {
                format!("{}h {}m", total_minutes / 60, total_minutes % 60)
            } else {
                format!("{total_minutes} minutes")
            };
            ui.set_listening_time_summary(SharedString::from(time));
            ui.set_listening_plays_summary(SharedString::from(format!(
                "{} plays • {} skips",
                summary.total_plays, summary.total_skips
            )));
            ui.set_listening_tracks_summary(SharedString::from(format!(
                "{} tracks",
                summary.unique_tracks
            )));
        }
        Err(error) => error!(%error, "failed to refresh listening summary"),
    }
    match list_top_tracks(&database.borrow(), 12) {
        Ok(tracks) => {
            let items = tracks
                .into_iter()
                .map(|track| TrackStatisticItem {
                    path: SharedString::from(track.path.to_string_lossy().as_ref()),
                    title: SharedString::from(track.title),
                    artist: SharedString::from(if track.artist.is_empty() {
                        "Unknown artist".to_owned()
                    } else {
                        track.artist
                    }),
                    detail: SharedString::from(format!("{} plays", track.play_count)),
                })
                .collect::<Vec<_>>();
            ui.set_top_tracks(ModelRc::from(Rc::new(VecModel::from(items))));
        }
        Err(error) => error!(%error, "failed to refresh top tracks"),
    }
    match list_rediscovery_tracks(&database.borrow(), 12) {
        Ok(tracks) => {
            let items = tracks
                .into_iter()
                .map(|track| TrackStatisticItem {
                    path: SharedString::from(track.path.to_string_lossy().as_ref()),
                    title: SharedString::from(track.title),
                    artist: SharedString::from(if track.artist.is_empty() {
                        "Unknown artist".to_owned()
                    } else {
                        track.artist
                    }),
                    detail: SharedString::from(format!(
                        "{} plays • {} skips",
                        track.play_count, track.skip_count
                    )),
                })
                .collect::<Vec<_>>();
            ui.set_rediscovery_tracks(ModelRc::from(Rc::new(VecModel::from(items))));
        }
        Err(error) => error!(%error, "failed to refresh rediscovery tracks"),
    }
}

fn refresh_audio_devices(ui: &AppWindow, selected_id: &str) {
    match MediaEngine::audio_output_devices() {
        Ok(devices) => {
            let selected_name = devices
                .iter()
                .find(|device| device.id == selected_id)
                .map_or_else(|| "System default".to_owned(), |device| device.name.clone());
            let items = devices
                .into_iter()
                .map(|device| AudioDeviceItem {
                    id: SharedString::from(device.id),
                    name: SharedString::from(device.name),
                })
                .collect::<Vec<_>>();
            ui.set_audio_output_devices(ModelRc::from(Rc::new(VecModel::from(items))));
            ui.set_selected_audio_device_id(SharedString::from(selected_id));
            ui.set_selected_audio_device_name(SharedString::from(selected_name));
        }
        Err(error) => {
            error!(%error, "failed to enumerate audio output devices");
            ui.set_playback_error(SharedString::from(format!(
                "Audio device refresh failed: {error}"
            )));
        }
    }
}

#[cfg(windows)]
fn refresh_recorder_audio_devices(ui: &AppWindow) {
    let to_model = |devices: Vec<win_native_media::AudioEndpoint>| {
        ModelRc::from(Rc::new(VecModel::from(
            devices
                .into_iter()
                .map(|device| AudioDeviceItem {
                    id: SharedString::from(device.id),
                    name: SharedString::from(device.name),
                })
                .collect::<Vec<_>>(),
        )))
    };
    match win_native_media::render_endpoints() {
        Ok(devices) => ui.set_recorder_output_devices(to_model(devices)),
        Err(error) => error!(%error, "failed to enumerate recorder output endpoints"),
    }
    match win_native_media::capture_endpoints() {
        Ok(devices) => ui.set_recorder_input_devices(to_model(devices)),
        Err(error) => error!(%error, "failed to enumerate recorder input endpoints"),
    }
}

#[cfg(not(windows))]
fn refresh_recorder_audio_devices(ui: &AppWindow) {
    ui.set_recorder_output_devices(ModelRc::default());
    ui.set_recorder_input_devices(ModelRc::default());
}

fn non_empty_device_id(id: &str) -> Option<String> {
    (!id.is_empty()).then(|| id.to_owned())
}

fn non_empty_str(value: &str) -> Option<&str> {
    (!value.is_empty()).then_some(value)
}

fn refresh_call_audio_devices(ui: &AppWindow) {
    let model = |names: anyhow::Result<Vec<String>>| {
        ModelRc::from(Rc::new(VecModel::from(
            names
                .unwrap_or_default()
                .into_iter()
                .map(|name| AudioDeviceItem {
                    id: SharedString::from(name.clone()),
                    name: SharedString::from(name),
                })
                .collect::<Vec<_>>(),
        )))
    };
    ui.set_call_output_devices(model(social_voice::output_devices()));
    ui.set_call_input_devices(model(social_voice::input_devices()));
}

fn cycle_audio_device_name(model: &ModelRc<AudioDeviceItem>, current: &str) -> String {
    if model.row_count() == 0 {
        return String::new();
    }
    if current.is_empty() {
        return model
            .row_data(0)
            .map_or_else(String::new, |item| item.name.to_string());
    }
    let current_index = (0..model.row_count()).find(|index| {
        model
            .row_data(*index)
            .is_some_and(|item| item.name.as_str() == current)
    });
    current_index
        .and_then(|index| model.row_data(index + 1))
        .map_or_else(String::new, |item| item.name.to_string())
}

fn refresh_video_audio_tracks(ui: &AppWindow, media: &MediaEngine) {
    match media.audio_tracks() {
        Ok(tracks) => {
            let items = tracks
                .into_iter()
                .map(|track| AudioTrackItem {
                    index: i32::try_from(track.index).unwrap_or(i32::MAX),
                    name: SharedString::from(track.name),
                    language: SharedString::from(track.language),
                    selected: track.selected,
                })
                .collect::<Vec<_>>();
            ui.set_video_audio_tracks(ModelRc::from(Rc::new(VecModel::from(items))));
        }
        Err(error) => error!(%error, "failed to enumerate embedded audio tracks"),
    }
}

fn refresh_video_technical_info(ui: &AppWindow, media: &MediaEngine) {
    let Ok(Some(info)) = media.technical_info() else {
        return;
    };
    let mut video = Vec::new();
    if info.width > 0 && info.height > 0 {
        video.push(format!("{}×{}", info.width, info.height));
    }
    if !info.video_codec.is_empty() {
        video.push(info.video_codec);
    }
    if let Some(frames_per_second) = info.frames_per_second {
        video.push(format!("{frames_per_second:.2} FPS"));
    }
    if info.video_bitrate > 0 {
        video.push(format!(
            "{:.1} Mbps",
            f64::from(info.video_bitrate) / 1_000_000.0
        ));
    }
    video.push(format!("Decoder: {}", info.decoder_status));

    let mut audio = Vec::new();
    if !info.audio_codec.is_empty() {
        audio.push(info.audio_codec);
    }
    if info.audio_sample_rate > 0 {
        audio.push(format!(
            "{:.1} kHz",
            f64::from(info.audio_sample_rate) / 1_000.0
        ));
    }
    if info.audio_channels > 0 {
        audio.push(format!("{} ch", info.audio_channels));
    }
    if info.audio_bitrate > 0 {
        audio.push(format!("{} kbps", info.audio_bitrate / 1_000));
    }

    let audio_line = if audio.is_empty() {
        "Audio: no decoded track information".to_owned()
    } else {
        format!("Audio: {}", audio.join(" • "))
    };
    ui.set_track_technical_detail(SharedString::from(format!(
        "Video: {}\n{audio_line}\nDynamic range: {}\nDisplay: {}",
        video.join(" • "),
        info.dynamic_range,
        info.display_hdr_capability
    )));
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod music_tests {
    use super::*;

    #[test]
    fn startup_window_is_dpi_aware_centered_and_inside_the_work_area() {
        let (x, y, width, height) = centred_window_bounds(0, 0, 3_840, 2_080);
        assert_eq!((width, height), (3_610, 1_914));
        assert_eq!((x, y), (115, 83));

        let (x, y, width, height) = centred_window_bounds(-1_920, 40, 1_920, 1_000);
        assert!(x >= -1_920 && y >= 40);
        assert!(x + i32::try_from(width).unwrap() <= 0);
        assert!(y + i32::try_from(height).unwrap() <= 1_040);
    }

    #[test]
    fn every_layout_backed_route_expands_to_the_available_page_surface() {
        let source = include_str!("../ui/app-window.slint");
        let page_surface_start = source
            .find("page-surface := Rectangle {")
            .expect("missing shared page surface");
        let page_surface_header =
            &source[page_surface_start..source.len().min(page_surface_start + 220)];
        assert!(
            page_surface_header.contains("horizontal-stretch: 1;")
                && page_surface_header.contains("vertical-stretch: 1;"),
            "all routes must share one full-size page surface"
        );
        for route in ["screen-recorder", "recordings", "social", "video-library"] {
            let marker = format!("if root.current-route == \"{route}\":");
            let route_start = source
                .find(&marker)
                .unwrap_or_else(|| panic!("missing route layout for {route}"));
            let route_header = &source[route_start..source.len().min(route_start + 220)];
            assert!(
                route_header.contains("horizontal-stretch: 1;")
                    && route_header.contains("vertical-stretch: 1;"),
                "{route} must fill the complete content surface"
            );
        }

        let messages = "if root.current-route == \"messages\": HorizontalLayout { horizontal-stretch: 1; vertical-stretch: 1;";
        assert!(source.contains(messages), "messages must fill the complete content surface");
    }

    #[test]
    fn about_build_timestamp_is_stable_and_human_readable() {
        assert_eq!(format_build_timestamp("0"), "01 Jan 1970, 00:00:00 UTC");
        assert_eq!(
            format_build_timestamp("1767225599"),
            "31 Dec 2025, 23:59:59 UTC"
        );
        assert_eq!(format_build_timestamp("invalid"), "Unknown build time");
    }

    #[test]
    fn theme_accent_accepts_presets_and_valid_custom_rgb() {
        assert_eq!(
            normalize_accent_color("Zink teal").as_deref(),
            Some("#06404A")
        );
        assert_eq!(
            normalize_accent_color("#7c3aed").as_deref(),
            Some("#7C3AED")
        );
        assert_eq!(normalize_accent_color("7C3AED"), None);
        assert_eq!(normalize_accent_color("#12345G"), None);
        let violet = slint_color_from_hex("#7C3AED");
        assert_eq!(
            (violet.red(), violet.green(), violet.blue()),
            (0x7c, 0x3a, 0xed)
        );
        let fallback = slint_color_from_hex("invalid");
        assert_eq!(
            (fallback.red(), fallback.green(), fallback.blue()),
            (0x38, 0xff, 0x66)
        );
    }

    #[test]
    fn store_links_keep_the_original_browser_fallback() {
        assert_eq!(
            store_web_fallback("ms-windows-store://review/?ProductId=9NX0L20X52L7"),
            Some("https://apps.microsoft.com/store/detail/9NX0L20X52L7")
        );
        assert_eq!(store_web_fallback("https://example.com"), None);
    }

    #[test]
    fn notification_text_is_safe_for_windows_toast_xml() {
        assert_eq!(
            escape_toast_xml("Zink <Music> & 'Video' \"Player\""),
            "Zink &lt;Music&gt; &amp; &apos;Video&apos; &quot;Player&quot;"
        );
    }

    #[test]
    fn call_audio_device_cycle_includes_windows_default() {
        let devices = ModelRc::from(Rc::new(VecModel::from(vec![
            AudioDeviceItem {
                id: "speakers".into(),
                name: "Speakers".into(),
            },
            AudioDeviceItem {
                id: "headset".into(),
                name: "Headset".into(),
            },
        ])));
        assert_eq!(cycle_audio_device_name(&devices, ""), "Speakers");
        assert_eq!(cycle_audio_device_name(&devices, "Speakers"), "Headset");
        assert_eq!(cycle_audio_device_name(&devices, "Headset"), "");
        assert_eq!(cycle_audio_device_name(&devices, "Removed device"), "");
    }

    #[cfg(windows)]
    #[test]
    fn screenshot_selection_scales_and_clamps_to_native_pixels() {
        assert_eq!(
            normalized_screenshot_crop(0.25, 0.25, 0.5, 0.5, 3840, 2160),
            (960, 540, 1920, 1080)
        );
        assert_eq!(
            normalized_screenshot_crop(1.0, 1.0, 0.5, 0.5, 1920, 1080),
            (1919, 1079, 1, 1)
        );
    }

    #[test]
    fn radio_registry_preserves_legacy_station_catalogue() {
        let stations = radio_station_registry(&AppSettings::default());
        assert!(stations.len() >= 60);
        for expected in [
            "Capital FM",
            "BBC Radio 1",
            "Greatest Hits Radio",
            "Kisstory",
            "Virgin Radio Chilled",
        ] {
            assert!(stations.iter().any(|station| station.0 == expected));
        }
    }

    #[test]
    fn icy_metadata_extracts_artist_and_title() {
        assert_eq!(
            parse_icy_stream_title(b"StreamTitle='Norah Jones - Come Away With Me';\0\0"),
            Some(("Norah Jones".to_owned(), "Come Away With Me".to_owned()))
        );
        assert_eq!(
            parse_icy_stream_title(b"StreamTitle='BBC News';"),
            Some((String::new(), "BBC News".to_owned()))
        );
    }

    #[test]
    fn radio_web_players_are_routed_to_zink_connect() {
        assert!(radio_uses_web_player(
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_one"
        ));
        assert!(radio_uses_web_player(
            "https://planetradio.co.uk/magic/player/"
        ));
        assert!(!radio_uses_web_player(
            "https://media-ssl.musicradio.com/CapitalUK"
        ));
    }

    #[cfg(windows)]
    #[test]
    fn streaming_quality_choices_map_to_native_encoder_profiles() {
        let compact = streaming_video_config("720p 30 FPS");
        assert_eq!(
            (compact.width, compact.height, compact.fps),
            (1280, 720, 30)
        );
        assert_eq!(compact.keyframe_interval, compact.fps * 2);

        let default = streaming_video_config("1080p 60 FPS");
        assert_eq!(
            (default.width, default.height, default.fps),
            (1920, 1080, 60)
        );
        assert_eq!(default.bitrate, 8_000_000);

        let high = streaming_video_config("1440p 60 FPS");
        assert_eq!((high.width, high.height, high.fps), (2560, 1440, 60));
        assert!(high.bitrate > default.bitrate);
    }

    #[cfg(windows)]
    #[test]
    fn twitch_telemetry_distinguishes_live_and_offline_channels() {
        let live = parse_twitch_telemetry(
            r#"{"data":[{"viewer_count":321,"title":"Zink launch stream"}]}"#,
            "zink",
        )
        .expect("parse live Twitch response");
        assert!(live.is_live);
        assert_eq!(live.viewers, 321);
        assert!(live.status.contains("Zink launch stream"));

        let offline = parse_twitch_telemetry(r#"{"data":[]}"#, "zink")
            .expect("parse offline Twitch response");
        assert!(!offline.is_live);
        assert_eq!(offline.viewers, 0);
        assert!(offline.status.contains("@zink"));
    }

    #[test]
    fn settings_languages_preserve_the_original_core_translations() {
        assert_eq!(language_tag("Simplified Chinese"), "zh-Hans");
        assert_eq!(language_display_name("pt-PT"), "Portuguese");
        assert_eq!(language_display_name("fr"), "French");
        assert_eq!(core_translation("de-DE", "Settings"), "Einstellungen");
        assert_eq!(core_translation("ja-JP", "Home"), "ホーム");
        assert_eq!(core_translation("fr-FR", "Appearance"), "Apparence");
        assert_eq!(core_translation("ko-KR", "Diagnostics"), "진단");
        assert_eq!(
            core_translation("de-DE", "Upload support bundle"),
            "Supportpaket hochladen"
        );
        assert_eq!(core_translation("en-GB", "Settings"), "Settings");
        assert_eq!(core_translation("fr-FR", "Music Player"), "Music Player");
    }

    fn test_folder(name: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after Unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("zink-{name}-{}-{nonce}", std::process::id()))
    }

    #[test]
    fn health_check_bundle_contains_report_and_recent_logs() {
        let root = test_folder("health-bundle");
        let logs = root.join("logs");
        std::fs::create_dir_all(&logs).expect("create test log folder");
        std::fs::write(logs.join("zink-diagnostics-test.txt"), "test diagnostics")
            .expect("write test diagnostic log");

        let (report, bundle) =
            create_health_support_bundle(&root, &logs).expect("create support bundle");
        assert!(report.is_file());
        assert!(bundle.is_file());
        let file = File::open(bundle).expect("open support bundle");
        let mut archive = zip::ZipArchive::new(file).expect("read support bundle");
        assert!(archive.by_name("zink-health-report.txt").is_ok());
        assert!(archive.by_name("logs/zink-diagnostics-test.txt").is_ok());

        std::fs::remove_dir_all(root).expect("remove test support bundle");
    }

    #[test]
    fn settings_reset_preserves_library_and_radio_user_data() {
        let current = AppSettings {
            media_library_folders: vec!["D:\\Music".to_owned()],
            liked_radio_stations: vec!["BBC Radio 1".to_owned()],
            custom_radio_stations: vec![CustomRadioStation {
                name: "Zink Test Radio".to_owned(),
                genre: "Test".to_owned(),
                stream_url: "https://example.com/radio".to_owned(),
            }],
            home_route_history: vec!["music-player".to_owned()],
            run_at_startup: false,
            discord_rich_presence: false,
            background_notifications: false,
            low_resource_background: true,
            ..AppSettings::default()
        };

        let reset = settings_defaults_preserving_user_data(&current);
        assert_eq!(reset.media_library_folders, current.media_library_folders);
        assert_eq!(reset.liked_radio_stations, current.liked_radio_stations);
        assert_eq!(reset.custom_radio_stations.len(), 1);
        assert_eq!(reset.home_route_history, current.home_route_history);
        assert!(reset.run_at_startup);
        assert!(reset.discord_rich_presence);
        assert!(reset.background_notifications);
        assert!(!reset.low_resource_background);
        assert!(!reset.background_replay);
        assert!(reset.clip_recording_hotkey);
        assert!(reset.diagnostic_log);
    }

    fn test_track(path: PathBuf, title: &str) -> MusicTrackState {
        MusicTrackState {
            path,
            title: title.to_owned(),
            artist: "Zink Test Artist".to_owned(),
            album: "Zink Test Album".to_owned(),
            technical_detail: "WAV • 44.1 kHz • Stereo".to_owned(),
            replay_gain_db: Some(-3.5),
            replay_peak: Some(0.91),
        }
    }

    #[test]
    fn artwork_lookup_recovers_missing_tags_from_filename() {
        let item = MediaItem {
            path: PathBuf::from(r"C:\Music\Beyoncé - Halo.m4a"),
            title: "Beyoncé - Halo".to_owned(),
            artist: String::new(),
            album: String::new(),
            media_kind: "audio".to_owned(),
            duration_ms: 0,
        };
        let request = library_artwork_request(&item);
        assert_eq!(request.artist, "Beyoncé");
        assert_eq!(request.title, "Halo");
        assert!(request.album.is_empty());
    }

    #[test]
    fn music_session_round_trips_all_player_settings() {
        let folder = test_folder("music-session");
        let session_file = folder.join("session.json");
        let first = test_track(folder.join("first.wav"), "First");
        let second = test_track(folder.join("second.wav"), "Second");
        let state = MusicPlayerState {
            queue: vec![first, second],
            current_index: Some(1),
            shuffle: true,
            repeat_current: true,
            background: true,
            playback_rate: 1.25,
            crossfade_seconds: 6,
            audio_device_id: "test-device".to_owned(),
            normalization_enabled: true,
            normalization_preamp_db: -3.0,
            session_file: session_file.clone(),
            ..MusicPlayerState::default()
        };

        save_music_session(&state, 42_750, 0.65);
        let restored = load_music_session(&session_file);
        assert_eq!(restored.queue.len(), 2);
        assert_eq!(restored.queue[1].title, "Second");
        assert_eq!(restored.queue[0].replay_gain_db, Some(-3.5));
        assert_eq!(restored.current_index, Some(1));
        assert_eq!(restored.position_ms, 42_750);
        assert_eq!(restored.volume, Some(0.65));
        assert!(restored.shuffle);
        assert!(restored.repeat_current);
        assert!(restored.background);
        assert_eq!(restored.playback_rate, Some(1.25));
        assert_eq!(restored.crossfade_seconds, Some(6));
        assert_eq!(restored.audio_device_id.as_deref(), Some("test-device"));
        assert_eq!(restored.normalization_enabled, Some(true));
        assert_eq!(restored.normalization_preamp_db, Some(-3.0));
        assert!(!session_file.with_extension("json.tmp").exists());

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn playlist_and_lyrics_are_written_and_parsed_in_order() {
        let folder = test_folder("playlist-lyrics");
        let tracks = vec![
            test_track(folder.join("Beyoncé - Halo.m4a"), "Halo"),
            test_track(folder.join("Second Song.wav"), "Second Song"),
        ];
        let playlist = save_queue_playlist(&folder, &tracks).expect("save test playlist");
        let contents = std::fs::read_to_string(playlist).expect("read test playlist");
        assert!(contents.starts_with("#EXTM3U\n"));
        assert!(contents.contains("Beyoncé - Halo.m4a"));
        assert!(
            contents.find("Beyoncé").expect("first path")
                < contents.find("Second Song").expect("second path")
        );

        let parsed = parse_lrc_contents(
            "[00:10.50]Later line\n[00:02.00][00:04.00]Repeated line\nmetadata only",
        );
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0], (2_000, "Repeated line".to_owned()));
        assert_eq!(parsed[1], (4_000, "Repeated line".to_owned()));
        assert_eq!(parsed[2], (10_500, "Later line".to_owned()));

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn queue_progression_wraps_and_shuffle_never_repeats_current() {
        let folder = test_folder("queue-progression");
        let mut state = MusicPlayerState::default();
        assert_eq!(next_music_index(&state), None);
        state.queue = vec![
            test_track(folder.join("one.wav"), "One"),
            test_track(folder.join("two.wav"), "Two"),
            test_track(folder.join("three.wav"), "Three"),
        ];
        state.current_index = Some(2);
        assert_eq!(next_music_index(&state), Some(0));
        state.current_index = Some(1);
        state.shuffle = true;
        for _ in 0..32 {
            assert_ne!(next_music_index(&state), Some(1));
        }
    }

    #[test]
    fn video_library_sort_modes_match_the_visible_choices() {
        let item = |name: &str, duration_ms: u64, extension: &str| MediaItem {
            path: PathBuf::from(format!("{name}.{extension}")),
            title: name.to_owned(),
            artist: String::new(),
            album: String::new(),
            media_kind: "video".to_owned(),
            duration_ms,
        };
        let original = vec![
            item("Beta", 20_000, "mkv"),
            item("Alpha", 50_000, "mp4"),
            item("Gamma", 10_000, "avi"),
        ];

        let mut items = original.clone();
        sort_library_items(&mut items, "Title A–Z");
        assert_eq!(items[0].title, "Alpha");
        let mut items = original.clone();
        sort_library_items(&mut items, "Title Z–A");
        assert_eq!(items[0].title, "Gamma");
        let mut items = original.clone();
        sort_library_items(&mut items, "Longest first");
        assert_eq!(items[0].duration_ms, 50_000);
        let mut items = original.clone();
        sort_library_items(&mut items, "Shortest first");
        assert_eq!(items[0].duration_ms, 10_000);
        let mut items = original;
        sort_library_items(&mut items, "File type");
        assert_eq!(
            items[0].path.extension().and_then(|value| value.to_str()),
            Some("avi")
        );
    }

    #[test]
    fn duplicate_scan_requires_matching_full_content() {
        let folder = test_folder("duplicate-video-scan");
        std::fs::create_dir_all(&folder).expect("create duplicate scan folder");
        let first = folder.join("first.mp4");
        let second = folder.join("second.mp4");
        let same_size_different = folder.join("different.mp4");
        std::fs::write(&first, b"verified duplicate video bytes").expect("write first");
        std::fs::write(&second, b"verified duplicate video bytes").expect("write second");
        std::fs::write(&same_size_different, b"different content same length!")
            .expect("write different");

        let result =
            scan_duplicate_video_paths(&[first.clone(), second.clone(), same_size_different])
                .expect("scan duplicates");
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|item| item.path == first));
        assert!(result.iter().any(|item| item.path == second));
        assert!(result.iter().all(|item| item.copies == 2));

        let _ = std::fs::remove_dir_all(folder);
    }

    #[test]
    fn spotify_callback_parsing_accepts_callback_and_raw_code() {
        assert_eq!(
            spotify_code_from_callback("https://example.com/callback?code=zink-test-code&state=ok")
                .as_deref(),
            Some("zink-test-code")
        );
        assert_eq!(
            spotify_code_from_callback("raw-code-value").as_deref(),
            Some("raw-code-value")
        );
        assert_eq!(spotify_code_from_callback("  "), None);
    }

    #[test]
    fn spotify_track_payload_maps_to_dashboard_item() {
        let payload = serde_json::json!({
            "id": "track-1",
            "name": "Come Away With Me",
            "artists": [{"name": "Norah Jones"}],
            "album": {"name": "Come Away With Me"}
        });
        let track = spotify_track_from_json(&payload).expect("track should parse");
        assert_eq!(track.id, "track-1");
        assert_eq!(track.title, "Come Away With Me");
        assert_eq!(track.artist, "Norah Jones");
        assert_eq!(track.album, "Come Away With Me");
    }

    #[test]
    fn spotify_pkce_encoding_is_url_safe_without_padding() {
        assert_eq!(base64_url_no_padding(b"Zink"), "Wmluaw");
        let encoded = base64_url_no_padding(&[0xfb, 0xff, 0xef]);
        assert_eq!(encoded, "-__v");
        assert!(!encoded.contains('='));
    }

    #[test]
    fn managed_service_routes_keep_the_original_page_destinations() {
        assert_eq!(route_web_service_url("youtube-music"), None);
        assert_eq!(
            route_web_service_url("bbc-iplayer"),
            Some("https://www.bbc.co.uk/iplayer")
        );
        assert_eq!(
            route_web_service_url("xbox"),
            Some("https://www.xbox.com/play")
        );
        assert_eq!(route_web_service_url("video-player"), None);
    }

    #[cfg(windows)]
    #[test]
    fn rtss_shared_memory_parser_handles_live_fallback_and_invalid_samples() {
        fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
            bytes[offset..offset + 4].copy_from_slice(&value.to_ne_bytes());
        }

        const ENTRY_SIZE: usize = 284;
        const ARRAY_OFFSET: usize = 20;
        const TARGET_PID: u32 = 42_424;
        let mut memory = vec![0_u8; ARRAY_OFFSET + ENTRY_SIZE * 2];
        write_u32(&mut memory, 0, 0x5254_5353);
        write_u32(&mut memory, 4, 0x0002_0000);
        write_u32(&mut memory, 8, 284);
        write_u32(&mut memory, 12, 20);
        write_u32(&mut memory, 16, 2);
        write_u32(&mut memory, ARRAY_OFFSET, 7);
        let entry = ARRAY_OFFSET + ENTRY_SIZE;
        write_u32(&mut memory, entry, TARGET_PID);
        write_u32(&mut memory, entry + 268, 1_000);
        write_u32(&mut memory, entry + 272, 2_000);
        write_u32(&mut memory, entry + 276, 60);
        write_u32(&mut memory, entry + 280, 16_667);

        // SAFETY: `memory` contains two complete synthetic RTSS V2 entries and
        // remains alive for the duration of every parser call.
        let live = unsafe { parse_rtss_fps_table(memory.as_ptr(), TARGET_PID) }
            .expect("parse valid RTSS table")
            .expect("find target process");
        assert!((live.fps - 60.0).abs() < 0.01);
        assert!((live.frame_time_ms - 16.667).abs() < 0.001);
        assert_eq!(live.sample_key, (2_000_u64 << 32) | 60);

        // An implausible instantaneous value must fall back to RTSS's measured
        // frame-period sample instead of displaying a 1,000 FPS spike.
        write_u32(&mut memory, entry + 280, 1_000);
        let fallback = unsafe { parse_rtss_fps_table(memory.as_ptr(), TARGET_PID) }
            .expect("parse fallback sample")
            .expect("retain valid period sample");
        assert!((fallback.fps - 60.0).abs() < f64::EPSILON);

        write_u32(&mut memory, entry + 272, 1_000);
        write_u32(&mut memory, entry + 276, 0);
        write_u32(&mut memory, entry + 280, 0);
        assert!(
            unsafe { parse_rtss_fps_table(memory.as_ptr(), TARGET_PID) }
                .expect("parse stale sample")
                .is_none()
        );

        write_u32(&mut memory, 0, 0);
        assert!(unsafe { parse_rtss_fps_table(memory.as_ptr(), TARGET_PID) }.is_err());
    }

    #[test]
    fn onedrive_links_request_the_original_download_stream() {
        let shared = url::Url::parse("https://1drv.ms/v/s!example").expect("parse share link");
        let normalized = normalize_onedrive_stream_url(shared);
        assert!(
            normalized
                .query_pairs()
                .any(|(key, value)| { key == "download" && value == "1" })
        );

        let direct =
            url::Url::parse("https://cdn.example.test/video.mp4").expect("parse direct link");
        assert_eq!(normalize_onedrive_stream_url(direct.clone()), direct);
    }

    #[test]
    fn sidecar_subtitles_parse_srt_and_webvtt_timestamps() {
        let cues = parse_sidecar_subtitles(
            "WEBVTT\n\n00:00:01.250 --> 00:00:03.500\nFirst line\nSecond line\n\n2\n00:04,000 --> 00:05,200\nFinal cue\n",
        );
        assert_eq!(cues.len(), 2);
        assert_eq!(cues[0].start_ms, 1_250);
        assert_eq!(cues[0].end_ms, 3_500);
        assert_eq!(cues[0].text, "First line\nSecond line");
        assert_eq!(cues[1].start_ms, 4_000);
        assert_eq!(cues[1].end_ms, 5_200);
    }
}

fn open_media_path(ui: &AppWindow, media: &Rc<RefCell<MediaEngine>>, path: &Path) {
    match media.borrow_mut().open(path) {
        Ok(()) => {
            let title = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("Unknown media");
            ui.set_track_title(SharedString::from(title));
            ui.set_track_detail(SharedString::from(path.to_string_lossy().as_ref()));
            ui.set_current_music_path(SharedString::from(path.to_string_lossy().as_ref()));
            if is_video_path(path) {
                ui.set_current_video_path(SharedString::from(path.to_string_lossy().as_ref()));
                ui.set_video_translation_status(SharedString::from(
                    "Translate spoken audio into UK English subtitles.",
                ));
                let (subtitle_path, subtitles) = load_sidecar_subtitles(path);
                let subtitle_items = subtitles
                    .into_iter()
                    .map(|cue| VideoSubtitleItem {
                        start_ms: i32::try_from(cue.start_ms).unwrap_or(i32::MAX),
                        end_ms: i32::try_from(cue.end_ms).unwrap_or(i32::MAX),
                        text: SharedString::from(cue.text),
                    })
                    .collect::<Vec<_>>();
                let count = subtitle_items.len();
                ui.set_video_subtitle_items(ModelRc::from(Rc::new(VecModel::from(subtitle_items))));
                ui.set_video_subtitles_enabled(count > 0);
                ui.set_video_subtitle_status(SharedString::from(subtitle_path.map_or_else(
                    || "No .srt or .vtt sidecar subtitles found".to_owned(),
                    |subtitle_path| {
                        format!(
                            "Loaded {count} cues from {}",
                            subtitle_path
                                .file_name()
                                .and_then(|value| value.to_str())
                                .unwrap_or("sidecar subtitles")
                        )
                    },
                )));
            }
            ui.set_is_playing(true);
            ui.set_playback_error(SharedString::default());
            info!(path = %path.display(), "opened local media");
        }
        Err(error) => {
            error!(%error, path = %path.display(), "failed to open media");
            ui.set_playback_error(SharedString::from(error.to_string()));
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct VideoSubtitleCue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

fn load_sidecar_subtitles(video_path: &Path) -> (Option<PathBuf>, Vec<VideoSubtitleCue>) {
    let stem = video_path
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    let Some(parent) = video_path.parent() else {
        return (None, Vec::new());
    };
    let mut candidates = [
        video_path.with_extension("srt"),
        video_path.with_extension("vtt"),
    ]
    .into_iter()
    .filter(|path| path.is_file())
    .collect::<Vec<_>>();
    if let Ok(entries) = std::fs::read_dir(parent) {
        candidates.extend(entries.flatten().map(|entry| entry.path()).filter(|path| {
            let extension = path
                .extension()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            let candidate_stem = path
                .file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or_default();
            matches!(extension.to_ascii_lowercase().as_str(), "srt" | "vtt")
                && (candidate_stem == stem || candidate_stem.starts_with(&format!("{stem}.")))
        }));
    }
    candidates.sort();
    candidates.dedup();
    for path in candidates {
        let Ok(contents) = std::fs::read_to_string(&path) else {
            continue;
        };
        let cues = parse_sidecar_subtitles(&contents);
        if !cues.is_empty() {
            return (Some(path), cues);
        }
    }
    (None, Vec::new())
}

fn parse_sidecar_subtitles(contents: &str) -> Vec<VideoSubtitleCue> {
    let normalized = contents.replace("\r\n", "\n").replace('\r', "\n");
    normalized
        .split("\n\n")
        .filter_map(|block| {
            let lines = block
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect::<Vec<_>>();
            let timing_index = lines.iter().position(|line| line.contains("-->"))?;
            let (start, end) = lines[timing_index].split_once("-->")?;
            let start_ms = parse_subtitle_timestamp(start.trim())?;
            let end_token = end.split_whitespace().next()?;
            let end_ms = parse_subtitle_timestamp(end_token)?;
            let text = lines
                .iter()
                .skip(timing_index + 1)
                .copied()
                .collect::<Vec<_>>()
                .join("\n");
            (!text.is_empty() && end_ms > start_ms).then_some(VideoSubtitleCue {
                start_ms,
                end_ms,
                text,
            })
        })
        .collect()
}

fn parse_subtitle_timestamp(value: &str) -> Option<u64> {
    let value = value.trim().replace(',', ".");
    let mut segments = value.split(':').collect::<Vec<_>>();
    let seconds = segments.pop()?;
    let minutes = segments.pop()?.parse::<u64>().ok()?;
    let hours = segments.pop().map_or(Some(0), |value| value.parse().ok())?;
    if !segments.is_empty() {
        return None;
    }
    let (whole_seconds, milliseconds) = seconds
        .split_once('.')
        .map_or((seconds, "0"), |(whole, fraction)| (whole, fraction));
    let whole_seconds = whole_seconds.parse::<u64>().ok()?;
    let mut fraction = milliseconds.chars().take(3).collect::<String>();
    while fraction.len() < 3 {
        fraction.push('0');
    }
    let milliseconds = fraction.parse::<u64>().ok()?;
    Some((((hours * 60 + minutes) * 60 + whole_seconds) * 1_000) + milliseconds)
}

fn scan_duplicate_video_paths(paths: &[PathBuf]) -> Result<Vec<ScannedDuplicateVideo>, String> {
    let mut by_size = BTreeMap::<u64, Vec<PathBuf>>::new();
    for path in paths {
        match std::fs::metadata(path) {
            Ok(metadata) if metadata.is_file() && metadata.len() > 0 => {
                by_size
                    .entry(metadata.len())
                    .or_default()
                    .push(path.clone());
            }
            Ok(_) => {}
            Err(error) => {
                info!(path = %path.display(), %error, "skipping unavailable video during duplicate scan");
            }
        }
    }

    let mut verified = BTreeMap::<(u64, String), Vec<PathBuf>>::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    for (size, candidates) in by_size.into_iter().filter(|(_, items)| items.len() > 1) {
        for path in candidates {
            let mut file = match File::open(&path) {
                Ok(file) => file,
                Err(error) => {
                    info!(path = %path.display(), %error, "unable to hash video candidate");
                    continue;
                }
            };
            let mut hasher = Sha256::new();
            loop {
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("Unable to verify {}: {error}", path.display()))?;
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            verified
                .entry((size, format!("{:x}", hasher.finalize())))
                .or_default()
                .push(path);
        }
    }

    let mut result = Vec::new();
    let groups = verified
        .into_iter()
        .filter(|(_, items)| items.len() > 1)
        .collect::<Vec<_>>();
    for (index, ((size, _), mut copies)) in groups.into_iter().enumerate() {
        copies.sort_by_key(|path| path.to_string_lossy().to_ascii_lowercase());
        let copy_count = copies.len();
        let group = format!("Duplicate group {}", index + 1);
        for path in copies {
            let name = path
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("Unknown video")
                .to_owned();
            result.push(ScannedDuplicateVideo {
                group: group.clone(),
                name,
                path,
                size,
                copies: copy_count,
            });
        }
    }
    Ok(result)
}

fn delete_duplicate_path(
    database: &Rc<RefCell<rusqlite::Connection>>,
    path: &Path,
) -> Result<(), String> {
    if !path.exists() {
        database
            .borrow()
            .execute(
                "DELETE FROM media_items WHERE path = ?1",
                [path.to_string_lossy().as_ref()],
            )
            .map_err(|error| {
                format!("Unable to remove the missing item from the library: {error}")
            })?;
        return Ok(());
    }
    recycle_file(path).map_err(|error| {
        format!(
            "Unable to move {} to the Recycle Bin: {error}",
            path.display()
        )
    })?;
    database
        .borrow()
        .execute(
            "DELETE FROM media_items WHERE path = ?1",
            [path.to_string_lossy().as_ref()],
        )
        .map_err(|error| {
            format!("The file was recycled, but its library entry could not be removed: {error}")
        })?;
    Ok(())
}

#[allow(clippy::cast_precision_loss)]
fn format_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    let bytes_float = bytes as f64;
    if bytes_float >= GIB {
        format!("{:.2} GB", bytes_float / GIB)
    } else if bytes_float >= MIB {
        format!("{:.1} MB", bytes_float / MIB)
    } else if bytes_float >= KIB {
        format!("{:.1} KB", bytes_float / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(windows)]
fn fetch_film_information(query: &str) -> anyhow::Result<FilmInformation> {
    let cleaned = query
        .replace(['.', '_'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let parameters = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("action", "query")
        .append_pair("list", "search")
        .append_pair("srsearch", &format!("{cleaned} film"))
        .append_pair("srlimit", "1")
        .append_pair("utf8", "1")
        .append_pair("format", "json")
        .finish();
    let search_url = format!("https://en.wikipedia.org/w/api.php?{parameters}");
    let mut search_body = artist_http_agent()
        .get(&search_url)
        .header("User-Agent", "Zink/5.0 film-information")
        .call()
        .context("search Wikipedia")?
        .into_body();
    let search: serde_json::Value = serde_json::from_slice(
        &search_body
            .read_to_vec()
            .context("read Wikipedia search response")?,
    )
    .context("decode Wikipedia search response")?;
    let title = search
        .pointer("/query/search/0/title")
        .and_then(serde_json::Value::as_str)
        .context("no matching film was found")?;
    let encoded_title = url::form_urlencoded::byte_serialize(title.as_bytes()).collect::<String>();
    let summary_url = format!("https://en.wikipedia.org/api/rest_v1/page/summary/{encoded_title}");
    let mut summary_body = artist_http_agent()
        .get(&summary_url)
        .header("User-Agent", "Zink/5.0 film-information")
        .call()
        .context("load Wikipedia film summary")?
        .into_body();
    let summary: serde_json::Value = serde_json::from_slice(
        &summary_body
            .read_to_vec()
            .context("read Wikipedia film summary")?,
    )
    .context("decode Wikipedia film summary")?;
    let resolved_title = summary
        .get("title")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(title)
        .to_owned();
    let overview = summary
        .get("extract")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("Wikipedia found this film, but no overview was available.")
        .to_owned();
    let source_url = summary
        .pointer("/content_urls/desktop/page")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(&summary_url)
        .to_owned();
    let date = summary
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Film information supplied by Wikipedia")
        .to_owned();
    let poster_path = summary
        .pointer("/originalimage/source")
        .or_else(|| summary.pointer("/thumbnail/source"))
        .and_then(serde_json::Value::as_str)
        .and_then(|source| cache_film_poster(&resolved_title, source));
    Ok(FilmInformation {
        title: resolved_title,
        date,
        overview,
        source_url,
        poster_path,
    })
}

#[cfg(not(windows))]
fn fetch_film_information(_query: &str) -> anyhow::Result<FilmInformation> {
    anyhow::bail!("film information lookup is available in the Windows build")
}

#[cfg(windows)]
fn cache_film_poster(title: &str, source: &str) -> Option<PathBuf> {
    use std::hash::{Hash, Hasher};

    let mut hasher = DefaultHasher::new();
    title.to_ascii_lowercase().hash(&mut hasher);
    let folder = std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)?
        .join("Zink/ArtworkCache/Films");
    std::fs::create_dir_all(&folder).ok()?;
    let destination = folder.join(format!("{:016x}.jpg", hasher.finish()));
    if destination.exists() {
        return Some(destination);
    }
    let mut body = artist_http_agent()
        .get(source)
        .header("User-Agent", "Zink/5.0 film-poster")
        .call()
        .ok()?
        .into_body();
    let bytes = body.read_to_vec().ok()?;
    if !(1_024..=16 * 1024 * 1024).contains(&bytes.len()) {
        return None;
    }
    let temporary = destination.with_extension("jpg.tmp");
    std::fs::write(&temporary, bytes).ok()?;
    if std::fs::rename(&temporary, &destination).is_err() {
        let _ = std::fs::remove_file(temporary);
        return None;
    }
    Some(destination)
}

fn normalize_onedrive_stream_url(mut url: url::Url) -> url::Url {
    let host = url.host_str().unwrap_or_default().to_ascii_lowercase();
    if host == "1drv.ms"
        || host.ends_with(".1drv.ms")
        || host == "onedrive.live.com"
        || host.ends_with(".sharepoint.com")
    {
        let has_download = url.query_pairs().any(|(key, _)| key == "download");
        if !has_download {
            url.query_pairs_mut().append_pair("download", "1");
        }
    }
    url
}

#[cfg(windows)]
fn is_onedrive_link(url: &url::Url) -> bool {
    url.host_str().is_some_and(|host| {
        host.eq_ignore_ascii_case("1drv.ms")
            || host.eq_ignore_ascii_case("onedrive.live.com")
            || host.ends_with(".sharepoint.com")
            || host.ends_with(".my.sharepoint.com")
    })
}

#[cfg(windows)]
fn acquire_onedrive_device_token(
    ui: &slint::Weak<AppWindow>,
) -> anyhow::Result<OneDriveAccessToken> {
    const CLIENT_ID: &str = "070089d3-2f30-498d-82c9-b4eb59878467";
    const SCOPE: &str = "https://graph.microsoft.com/Files.ReadWrite offline_access";
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .into();
    let mut response = agent
        .post("https://login.microsoftonline.com/common/oauth2/v2.0/devicecode")
        .send_form([("client_id", CLIENT_ID), ("scope", SCOPE)])?
        .into_body();
    let device: OneDriveDeviceCode = serde_json::from_reader(response.as_reader())?;
    let status = format!(
        "Microsoft sign-in opened. Enter code {} if the browser asks for it.",
        device.user_code
    );
    let weak = ui.clone();
    let _ = slint::invoke_from_event_loop(move || {
        if let Some(ui) = weak.upgrade() {
            ui.set_online_video_status(SharedString::from(status));
        }
    });
    open_external_url(&device.verification_uri)?;

    let deadline = Instant::now() + Duration::from_secs(device.expires_in.min(900));
    let mut interval = device.interval.unwrap_or(5).clamp(2, 15);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(interval));
        let response = agent
            .post("https://login.microsoftonline.com/common/oauth2/v2.0/token")
            .send_form([
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
                ("client_id", CLIENT_ID),
                ("device_code", device.device_code.as_str()),
            ])?;
        let mut body = response.into_body();
        let token: OneDriveTokenResponse = serde_json::from_reader(body.as_reader())?;
        if let Some(value) = token.access_token {
            return Ok(OneDriveAccessToken {
                value,
                expires_at: Instant::now() + Duration::from_secs(token.expires_in.unwrap_or(3600)),
            });
        }
        match token.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval = (interval + 5).min(30),
            Some(error) => anyhow::bail!("{}", token.error_description.as_deref().unwrap_or(error)),
            None => anyhow::bail!("Microsoft returned an incomplete sign-in response"),
        }
    }
    anyhow::bail!("Microsoft sign-in timed out")
}

#[cfg(windows)]
fn resolve_onedrive_video(
    sharing_link: &str,
    access_token: &str,
) -> anyhow::Result<OneDriveResolvedVideo> {
    let share_id = format!(
        "u!{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sharing_link.as_bytes())
    );
    let endpoint = format!("https://graph.microsoft.com/v1.0/shares/{share_id}/driveItem");
    let mut body = ureq::get(&endpoint)
        .header("Authorization", &format!("Bearer {access_token}"))
        .header("Prefer", "redeemSharingLinkIfNecessary")
        .call()?
        .into_body();
    let value: serde_json::Value = serde_json::from_reader(body.as_reader())?;
    let item = value.get("remoteItem").unwrap_or(&value);
    if item.get("folder").is_some() {
        anyhow::bail!("the link points to a folder instead of one video")
    }
    let name = item
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("OneDrive video")
        .to_owned();
    let is_video = item.get("video").is_some()
        || item
            .pointer("/file/mimeType")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|mime| mime.starts_with("video/"));
    if !is_video {
        anyhow::bail!("the shared item is not a video")
    }
    let original = item
        .get("@microsoft.graph.downloadUrl")
        .or_else(|| item.get("@content.downloadUrl"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Microsoft did not return an original-file URL"))?;
    let url = url::Url::parse(original)?;
    if url.scheme() != "https" {
        anyhow::bail!("Microsoft returned an insecure media URL")
    }
    Ok(OneDriveResolvedVideo {
        url: url.into(),
        name,
    })
}

const SPOTIFY_CLIENT_ID: &str = "2e88dcd486ec48aaaf54ac86e8c266a2";
const SPOTIFY_REDIRECT_URI: &str = "https://example.com/callback";
const SPOTIFY_SCOPE: &str = "streaming user-read-private user-read-email user-library-modify user-library-read user-read-playback-state user-read-currently-playing user-modify-playback-state playlist-read-private user-read-recently-played user-top-read";

fn spotify_authorization_url(verifier_path: &Path) -> anyhow::Result<String> {
    let mut random = [0_u8; 64];
    getrandom::fill(&mut random)
        .map_err(|error| anyhow::anyhow!("create Spotify PKCE verifier: {error}"))?;
    let verifier = base64_url_no_padding(&random);
    let challenge = base64_url_no_padding(&Sha256::digest(verifier.as_bytes()));
    std::fs::write(verifier_path, &verifier).context("save Spotify PKCE verifier")?;
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", SPOTIFY_CLIENT_ID)
        .append_pair("response_type", "code")
        .append_pair("redirect_uri", SPOTIFY_REDIRECT_URI)
        .append_pair("scope", SPOTIFY_SCOPE)
        .append_pair("code_challenge_method", "S256")
        .append_pair("code_challenge", &challenge)
        .append_pair("show_dialog", "true")
        .finish();
    Ok(format!("https://accounts.spotify.com/authorize?{query}"))
}

fn spotify_code_from_callback(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    if !value.contains("://") && !value.contains('?') && !value.contains('&') {
        return Some(value.to_owned());
    }
    url::Url::parse(value).ok().and_then(|url| {
        url.query_pairs()
            .find_map(|(key, value)| (key == "code").then(|| value.into_owned()))
    })
}

fn spotify_exchange_code(code: &str, verifier_path: &Path) -> anyhow::Result<SpotifyCredentials> {
    let verifier = std::fs::read_to_string(verifier_path).context("read Spotify PKCE verifier")?;
    let response = ureq::post("https://accounts.spotify.com/api/token")
        .send_form([
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", SPOTIFY_REDIRECT_URI),
            ("client_id", SPOTIFY_CLIENT_ID),
            ("code_verifier", verifier.trim()),
        ])
        .context("exchange Spotify authorization code")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Spotify token response")?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parse Spotify token response")?;
    let access_token = json
        .get("access_token")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Spotify response did not include an access token")?
        .to_owned();
    let refresh_token = json
        .get("refresh_token")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(SpotifyCredentials {
        access_token,
        refresh_token,
    })
}

fn spotify_profile_result(credentials: SpotifyCredentials) -> anyhow::Result<SpotifyAuthResult> {
    let response = ureq::get("https://api.spotify.com/v1/me")
        .header(
            "Authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .call()
        .context("load Spotify profile")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Spotify profile")?;
    let json: serde_json::Value = serde_json::from_str(&body).context("parse Spotify profile")?;
    let display_name = json
        .get("display_name")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("Spotify listener")
        .to_owned();
    let product = json
        .get("product")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Spotify")
        .to_owned();
    let dashboard = spotify_dashboard(&credentials).ok();
    Ok(SpotifyAuthResult {
        credentials: Some(credentials),
        display_name,
        product,
        status: "Spotify connected. Your account and playback permissions are ready.".to_owned(),
        dashboard,
    })
}

fn save_spotify_credentials(path: &Path, credentials: &SpotifyCredentials) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(credentials).context("serialize Spotify credentials")?;
    std::fs::write(path, bytes).context("save Spotify credentials")
}

fn load_spotify_credentials(path: &Path) -> anyhow::Result<SpotifyCredentials> {
    let bytes = std::fs::read(path).context("read Spotify credentials")?;
    serde_json::from_slice(&bytes).context("parse Spotify credentials")
}

fn spotify_get_json(
    credentials: &SpotifyCredentials,
    endpoint: &str,
) -> anyhow::Result<Option<serde_json::Value>> {
    let response = ureq::get(endpoint)
        .header(
            "Authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .call()
        .with_context(|| format!("load Spotify endpoint {endpoint}"))?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Spotify response")?;
    if body.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_str(&body).context("parse Spotify response")?,
    ))
}

fn spotify_track_from_json(value: &serde_json::Value) -> Option<SpotifyTrackData> {
    let id = value.get("id")?.as_str()?.to_owned();
    let title = value.get("name")?.as_str()?.to_owned();
    let artist = value
        .pointer("/artists/0/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("Unknown artist")
        .to_owned();
    let album = value
        .pointer("/album/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Some(SpotifyTrackData {
        id,
        title,
        artist,
        album,
    })
}

fn spotify_search_tracks(
    credentials: &SpotifyCredentials,
    query: &str,
) -> anyhow::Result<Vec<SpotifyTrackData>> {
    let encoded = url::form_urlencoded::byte_serialize(query.as_bytes()).collect::<String>();
    let endpoint = format!("https://api.spotify.com/v1/search?type=track&limit=12&q={encoded}");
    let json = spotify_get_json(credentials, &endpoint)?
        .context("Spotify returned an empty search response")?;
    Ok(json
        .pointer("/tracks/items")
        .and_then(serde_json::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(spotify_track_from_json)
        .collect())
}

fn spotify_dashboard(credentials: &SpotifyCredentials) -> anyhow::Result<SpotifyDashboardData> {
    let mut dashboard = SpotifyDashboardData::default();
    if let Some(current) = spotify_get_json(credentials, "https://api.spotify.com/v1/me/player")? {
        dashboard.current = current.get("item").and_then(spotify_track_from_json);
        dashboard.is_playing = current
            .get("is_playing")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        current
            .pointer("/device/name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("No active device")
            .clone_into(&mut dashboard.device_name);
    }
    if let Some(recent) = spotify_get_json(
        credentials,
        "https://api.spotify.com/v1/me/player/recently-played?limit=8",
    )? {
        dashboard.recent = recent
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("track").and_then(spotify_track_from_json))
            .collect();
    }
    if let Some(top) = spotify_get_json(
        credentials,
        "https://api.spotify.com/v1/me/top/tracks?limit=8&time_range=short_term",
    )? {
        dashboard.top = top
            .get("items")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(spotify_track_from_json)
            .collect();
    }
    if let Some(queue) =
        spotify_get_json(credentials, "https://api.spotify.com/v1/me/player/queue")?
    {
        dashboard.queue = queue
            .get("queue")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .take(12)
            .filter_map(spotify_track_from_json)
            .collect();
    }
    if let Some(devices) =
        spotify_get_json(credentials, "https://api.spotify.com/v1/me/player/devices")?
    {
        dashboard.devices = devices
            .get("devices")
            .and_then(serde_json::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|device| {
                Some(SpotifyDeviceData {
                    id: device.get("id")?.as_str()?.to_owned(),
                    name: device.get("name")?.as_str()?.to_owned(),
                    detail: format!(
                        "{} • {}%",
                        device
                            .get("type")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("Device"),
                        device
                            .get("volume_percent")
                            .and_then(serde_json::Value::as_i64)
                            .unwrap_or(0)
                    ),
                    active: device
                        .get("is_active")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(false),
                })
            })
            .collect();
    }
    Ok(dashboard)
}

fn apply_spotify_dashboard(ui: &AppWindow, dashboard: SpotifyDashboardData) {
    if let Some(current) = dashboard.current {
        ui.set_spotify_now_title(SharedString::from(current.title));
        ui.set_spotify_now_artist(SharedString::from(current.artist));
        ui.set_spotify_now_album(SharedString::from(current.album));
    } else {
        ui.set_spotify_now_title(SharedString::from("Nothing playing"));
        ui.set_spotify_now_artist(SharedString::from("Start Spotify on any connected device."));
        ui.set_spotify_now_album(SharedString::new());
    }
    ui.set_spotify_is_playing(dashboard.is_playing);
    ui.set_spotify_device_name(SharedString::from(if dashboard.device_name.is_empty() {
        "No active device".to_owned()
    } else {
        dashboard.device_name
    }));
    ui.set_spotify_recent_tracks(spotify_track_model(dashboard.recent));
    ui.set_spotify_top_tracks(spotify_track_model(dashboard.top));
    ui.set_spotify_queue(spotify_track_model(dashboard.queue));
    ui.set_spotify_devices(ModelRc::from(Rc::new(VecModel::from(
        dashboard
            .devices
            .into_iter()
            .map(|device| SpotifyDeviceItem {
                id: SharedString::from(device.id),
                name: SharedString::from(device.name),
                detail: SharedString::from(device.detail),
                active: device.active,
            })
            .collect::<Vec<_>>(),
    ))));
}

fn spotify_track_model(values: Vec<SpotifyTrackData>) -> ModelRc<SpotifyTrackItem> {
    ModelRc::from(Rc::new(VecModel::from(
        values
            .into_iter()
            .map(|track| SpotifyTrackItem {
                id: SharedString::from(track.id),
                title: SharedString::from(track.title),
                artist: SharedString::from(track.artist),
                album: SharedString::from(track.album),
            })
            .collect::<Vec<_>>(),
    )))
}

fn spotify_player_command(
    credentials: &SpotifyCredentials,
    command: &str,
    argument: &str,
) -> anyhow::Result<()> {
    let authorization = format!("Bearer {}", credentials.access_token);
    match command {
        "play" => {
            ureq::put("https://api.spotify.com/v1/me/player/play")
                .header("Authorization", &authorization)
                .send_empty()?;
        }
        "pause" => {
            ureq::put("https://api.spotify.com/v1/me/player/pause")
                .header("Authorization", &authorization)
                .send_empty()?;
        }
        "next" => {
            ureq::post("https://api.spotify.com/v1/me/player/next")
                .header("Authorization", &authorization)
                .send_empty()?;
        }
        "previous" => {
            ureq::post("https://api.spotify.com/v1/me/player/previous")
                .header("Authorization", &authorization)
                .send_empty()?;
        }
        "track" => {
            let body = serde_json::to_string(
                &serde_json::json!({"uris": [format!("spotify:track:{argument}")]}),
            )?;
            ureq::put("https://api.spotify.com/v1/me/player/play")
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(body)?;
        }
        "device" => {
            let body = serde_json::to_string(
                &serde_json::json!({"device_ids": [argument], "play": false}),
            )?;
            ureq::put("https://api.spotify.com/v1/me/player")
                .header("Authorization", &authorization)
                .header("Content-Type", "application/json")
                .send(body)?;
        }
        _ => anyhow::bail!("unknown Spotify player command"),
    }
    Ok(())
}

fn queue_spotify_command(credentials_path: PathBuf, command: &'static str, argument: String) {
    std::thread::spawn(move || {
        let result = load_spotify_credentials(&credentials_path)
            .and_then(|credentials| {
                spotify_player_command(&credentials, command, &argument)?;
                std::thread::sleep(Duration::from_millis(250));
                spotify_profile_result(credentials)
            })
            .unwrap_or_else(|error| SpotifyAuthResult {
                credentials: None,
                display_name: String::new(),
                product: String::new(),
                status: format!("Spotify command failed: {error}"),
                dashboard: None,
            });
        if let Ok(mut results) = SPOTIFY_AUTH_RESULTS
            .get_or_init(|| Mutex::new(Vec::new()))
            .lock()
        {
            results.push(result);
        }
    });
}

fn spotify_like_radio_song(
    credentials: &SpotifyCredentials,
    song: &LikedRadioSongState,
) -> anyhow::Result<(String, String)> {
    let mut endpoint = url::Url::parse("https://api.spotify.com/v1/search")?;
    endpoint
        .query_pairs_mut()
        .append_pair("q", &liked_radio_song_query(song))
        .append_pair("type", "track")
        .append_pair("limit", "5");
    let response = ureq::get(endpoint.as_str())
        .header(
            "Authorization",
            format!("Bearer {}", credentials.access_token),
        )
        .call()
        .context("search Spotify tracks")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Spotify search response")?;
    let json: serde_json::Value = serde_json::from_str(&body).context("parse Spotify search")?;
    let track = json
        .pointer("/tracks/items/0")
        .context("Spotify could not find a matching track")?;
    let track_id = track
        .get("id")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .context("Spotify result did not include a track id")?
        .to_owned();
    let track_url = track
        .pointer("/external_urls/spotify")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    ureq::put(format!(
        "https://api.spotify.com/v1/me/tracks?ids={track_id}"
    ))
    .header(
        "Authorization",
        format!("Bearer {}", credentials.access_token),
    )
    .send_empty()
    .context("save track to Spotify Liked Songs")?;
    Ok((track_id, track_url))
}

fn create_health_support_bundle(root: &Path, logs: &Path) -> anyhow::Result<(PathBuf, PathBuf)> {
    std::fs::create_dir_all(root).context("create Zink data directory")?;
    std::fs::create_dir_all(logs).context("create Zink diagnostic log directory")?;
    let report_path = root.join("zink-health-report.txt");
    let bundle_path = root.join("zink-support-latest.zip");
    let log_files = std::fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            path.is_file().then_some(path)
        })
        .collect::<Vec<_>>();
    let body = format!(
        "Zink health check report\nGenerated: {}\nVersion: {}\nOS: {}\nArchitecture: {}\nDevice: {}\nData directory: {}\nDiagnostic log files: {}\n",
        unix_timestamp(),
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH,
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows PC".to_owned()),
        root.display(),
        log_files.len()
    );
    std::fs::write(&report_path, body).context("write Zink health report")?;

    let bundle = File::create(&bundle_path).context("create diagnostics support bundle")?;
    let mut archive = zip::ZipWriter::new(bundle);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    archive
        .start_file("zink-health-report.txt", options)
        .context("add health report to support bundle")?;
    archive
        .write_all(&std::fs::read(&report_path).context("read health report")?)
        .context("write health report to support bundle")?;
    for log_path in log_files.into_iter().take(10) {
        let Some(file_name) = log_path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        archive
            .start_file(format!("logs/{file_name}"), options)
            .with_context(|| format!("add {} to support bundle", log_path.display()))?;
        archive
            .write_all(&std::fs::read(&log_path).context("read diagnostic log")?)
            .context("write diagnostic log to support bundle")?;
    }
    archive
        .finish()
        .context("finish diagnostics support bundle")?;
    Ok((report_path, bundle_path))
}

fn settings_defaults_preserving_user_data(current: &AppSettings) -> AppSettings {
    AppSettings {
        run_at_startup: true,
        discord_rich_presence: true,
        background_notifications: true,
        low_resource_background: false,
        media_library_folders: current.media_library_folders.clone(),
        liked_radio_stations: current.liked_radio_stations.clone(),
        custom_radio_stations: current.custom_radio_stations.clone(),
        home_route_history: current.home_route_history.clone(),
        ..AppSettings::default()
    }
}

#[cfg(windows)]
fn upload_health_support_bundle(
    root: &Path,
    logs: &Path,
    session_path: &Path,
) -> Result<String, String> {
    let (_, bundle_path) = create_health_support_bundle(root, logs)
        .map_err(|error| format!("Could not create the support bundle: {error}"))?;
    let session = social_load_session(session_path)
        .map_err(|_| "Sign in to Zink Social before uploading diagnostics.".to_owned())?;
    let device_name = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "Windows-PC".to_owned());
    let file_name = bundle_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("zink-support.zip");
    let mut upload_url = url::Url::parse(&format!("{SOCIAL_BASE_URL}/api/diagnostics/upload"))
        .map_err(|error| format!("Invalid diagnostics endpoint: {error}"))?;
    upload_url
        .query_pairs_mut()
        .append_pair("deviceName", &device_name)
        .append_pair("fileName", file_name);
    let bytes = std::fs::read(&bundle_path)
        .map_err(|error| format!("Could not read the support bundle: {error}"))?;
    let response = ureq::post(upload_url.as_str())
        .header("Authorization", &format!("Bearer {}", session.token))
        .header("Content-Type", "application/zip")
        .header(
            "X-Zink-App-Base",
            &std::env::current_exe()
                .unwrap_or_default()
                .display()
                .to_string(),
        )
        .header("X-Zink-Log-Path", &logs.display().to_string())
        .send(bytes)
        .map_err(|error| format!("The Zink diagnostics server rejected the upload: {error}"))?;
    let status = response.status().as_u16();
    let mut response = response.into_body();
    let response_text = response.read_to_string().unwrap_or_default();
    let json = serde_json::from_str::<serde_json::Value>(&response_text).unwrap_or_default();
    let report_id = json
        .get("reportId")
        .or_else(|| json.get("ReportId"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if report_id.is_empty() {
        Ok(format!(
            "Diagnostics uploaded successfully (HTTP {status}). Local copy: {}",
            bundle_path.display()
        ))
    } else {
        Ok(format!(
            "Diagnostics uploaded successfully. Report ID: {report_id} • Local copy: {}",
            bundle_path.display()
        ))
    }
}

fn unix_timestamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn social_save_session(path: &Path, session: &SocialSession) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(session).context("serialize Zink Social session")?;
    std::fs::write(path, bytes).context("save Zink Social session")
}

#[cfg(windows)]
fn push_social_call_event(event: SocialCallEvent) {
    let should_wake = SOCIAL_CALL_EVENTS
        .get_or_init(|| Mutex::new(VecDeque::new()))
        .lock()
        .is_ok_and(|mut events| {
            let was_empty = events.is_empty();
            if matches!(event, SocialCallEvent::RemoteFrame { .. }) {
                events.retain(|queued| !matches!(queued, SocialCallEvent::RemoteFrame { .. }));
            }
            events.push_back(event);
            was_empty
        });
    if should_wake {
        let _ = slint::invoke_from_event_loop(dispatch_social_call_events);
    }
}

#[cfg(windows)]
fn dispatch_social_call_events() {
    SOCIAL_CALL_EVENT_HANDLER.with(|handler| {
        if let Ok(mut handler) = handler.try_borrow_mut()
            && let Some(handler) = handler.as_mut()
        {
            handler();
        }
    });
}

#[cfg(windows)]
fn social_signal_envelope(
    message_type: &str,
    target_user_id: i64,
    call_id: String,
) -> SocialSignalEnvelope {
    SocialSignalEnvelope {
        message_type: message_type.to_owned(),
        call_id: Some(call_id),
        from_user_id: 0,
        from_username: String::new(),
        from_display_name: String::new(),
        target_user_id,
        debug_message: None,
        audio_base64: None,
        screen_frame_base64: None,
        screen_frame_width: None,
        screen_frame_height: None,
        screen_frame_timestamp: None,
        screen_width: None,
        screen_height: None,
        timestamp_utc_ms: None,
        screen_codec: None,
        screen_codecs: None,
        screen_frame_is_key_frame: None,
        sdp: None,
        screen_rtc_id: None,
    }
}

#[cfg(windows)]
#[allow(clippy::too_many_lines)]
fn start_social_call_worker(
    session: SocialSession,
) -> tokio::sync::mpsc::Sender<SocialCallCommand> {
    let (command_tx, mut command_rx) = tokio::sync::mpsc::channel(96);
    std::thread::spawn(move || {
        let runtime = match tokio::runtime::Runtime::new() {
            Ok(runtime) => runtime,
            Err(error) => {
                push_social_call_event(SocialCallEvent::Failed(format!(
                    "Could not start the call runtime: {error}"
                )));
                return;
            }
        };
        runtime.block_on(async move {
            let websocket_url = format!(
                "wss://zink-free.francehayle.workers.dev/ws?token={}",
                url::form_urlencoded::byte_serialize(session.token.as_bytes()).collect::<String>()
            );
            push_social_call_event(SocialCallEvent::Status(
                "Connecting to Zink Call…".to_owned(),
            ));
            let (stream, _) = match tokio::time::timeout(
                Duration::from_secs(12),
                tokio_tungstenite::connect_async_with_config(&websocket_url, None, true),
            ).await {
                Ok(Ok(result)) => result,
                Ok(Err(error)) => {
                    push_social_call_event(SocialCallEvent::Failed(format!(
                        "Call signalling failed: {error}"
                    )));
                    return;
                }
                Err(_) => {
                    push_social_call_event(SocialCallEvent::Failed(
                        "Connecting to Zink Call timed out.".to_owned(),
                    ));
                    return;
                }
            };
            let (mut writer, mut reader) = stream.split();
            let mut active_call: Option<(i64, String)> = None;
            let mut pending_invite: Option<(i64, String)> = None;
            let mut outgoing_hdr10 = false;
            let mut outgoing_av1 = false;
            let mut peer_screen_rtc = false;
            let mut peer_av1 = false;
            let can_av1_decode = social_video_decoder::av1_decoder_available();
            let can_av1_encode = win_native_media::encoder::mf_h264::MfH264Encoder::av1_call_available();
            let (rtc_events, mut rtc_rx) = tokio::sync::mpsc::channel(32);
            let mut outgoing_rtc: Option<social_screen_rtc::Connection> = None;
            let mut incoming_rtc: Option<(social_screen_rtc::Connection, u32, u32)> = None;
            let mut outgoing_rtc_active = false;
            let mut incoming_rtc_active = false;
            let mut resume_legacy = false;
            let mut outgoing_bitrate = None;
            let mut outgoing_size = (0, 0);
            let mut next_share_id = 0_u64;
            let mut send_rate = social_screen_share::ShareRate::default();
            let mut ready_command = None;
            let mut next_share_log = Instant::now();
            let mut next_ping = Instant::now() + Duration::from_secs(15);
            let mut last_pong = Instant::now();
            push_social_call_event(SocialCallEvent::Status("Zink Call is ready.".to_owned()));

            loop {
                if Instant::now() >= next_ping {
                    if last_pong.elapsed() >= Duration::from_secs(40)
                        || writer.send(Message::Ping(Vec::new().into())).await.is_err()
                    {
                        push_social_call_event(SocialCallEvent::Failed(
                            "Zink Call connection dropped. Reconnecting…".to_owned(),
                        ));
                        return;
                    }
                    next_ping = Instant::now() + Duration::from_secs(15);
                }
                // Drain a bounded batch before waiting for incoming frames.
                // A one-command loop with a 100 ms read timeout starves video,
                // audio and hang-up on a quiet or one-way connection.
                for _ in 0..16 {
                    let command = match ready_command.take().map_or_else(|| command_rx.try_recv(), Ok) {
                        Ok(command) => command,
                        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                        Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return,
                    };
                    let mut video_slot = None;
                    let envelope = match command {
                        SocialCallCommand::Probe => None,
                        SocialCallCommand::Shutdown => {
                            let _ = writer.close().await;
                            return;
                        }
                        SocialCallCommand::Invite(user_id) => {
                            peer_screen_rtc = false;
                            peer_av1 = false;
                            outgoing_bitrate = None;
                            outgoing_rtc = None;
                            incoming_rtc = None;
                            outgoing_rtc_active = false;
                            incoming_rtc_active = false;
                            let call_id = format!(
                                "rust-{}-{user_id}-{}",
                                session.user_id,
                                SystemTime::now()
                                    .duration_since(UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_millis()
                            );
                            active_call = Some((user_id, call_id.clone()));
                            push_social_call_event(SocialCallEvent::Status(
                                "Ringing… waiting for an answer".to_owned(),
                            ));
                            Some(social_signal_envelope("call-invite", user_id, call_id))
                        }
                        SocialCallCommand::Accept { user_id, call_id } => {
                            peer_screen_rtc = false;
                            peer_av1 = false;
                            outgoing_bitrate = None;
                            outgoing_rtc = None;
                            incoming_rtc = None;
                            outgoing_rtc_active = false;
                            incoming_rtc_active = false;
                            pending_invite = None;
                            active_call = Some((user_id, call_id.clone()));
                            push_social_call_event(SocialCallEvent::Connected);
                            Some(social_signal_envelope("call-accept", user_id, call_id))
                        }
                        SocialCallCommand::Decline { user_id, call_id } => {
                            if pending_invite.as_ref().is_some_and(|(id, pending)| {
                                *id == user_id && pending == &call_id
                            }) {
                                pending_invite = None;
                            }
                            Some(social_signal_envelope("call-decline", user_id, call_id))
                        }
                        SocialCallCommand::End => {
                            outgoing_rtc = None;
                            incoming_rtc = None;
                            outgoing_bitrate = None;
                            active_call.take().map(|(user_id, call_id)| {
                            let mut signal = social_signal_envelope("call-ended", user_id, call_id);
                            signal.debug_message = Some("Call ended.".to_owned());
                            signal
                            })
                        }
                        SocialCallCommand::Audio(packet) => {
                            active_call.as_ref().map(|(user_id, call_id)| {
                                let mut signal = social_signal_envelope(
                                    "audio-chunk",
                                    *user_id,
                                    call_id.clone(),
                                );
                                signal.audio_base64 =
                                    Some(base64::engine::general_purpose::STANDARD.encode(packet));
                                signal
                            })
                        }
                        SocialCallCommand::ScreenCapabilities(hdr10) => active_call.as_ref().map(|(user_id, call_id)| {
                            let mut signal = social_signal_envelope("screen-share-capabilities", *user_id, call_id.clone());
                            let mut codecs = vec!["h264".to_owned()];
                            codecs.push(social_screen_rtc::CAPABILITY.into());
                            if can_av1_decode { codecs.push(social_screen_rtc::AV1_CAPABILITY.into()); }
                            if hdr10 { codecs.push("hevc-hdr10".to_owned()); }
                            signal.screen_codecs = Some(codecs);
                            signal
                        }),
                        SocialCallCommand::ScreenRequestSdr => active_call.as_ref().map(|(user_id, call_id)| {
                            social_signal_envelope("screen-share-request-sdr", *user_id, call_id.clone())
                        }),
                        SocialCallCommand::ScreenRequestH264 => active_call.as_ref().map(|(user_id, call_id)| {
                            social_signal_envelope("screen-share-request-h264", *user_id, call_id.clone())
                        }),
                        SocialCallCommand::ScreenStarted { width, height, hdr10, av1, control } => {
                            outgoing_hdr10 = hdr10;
                            outgoing_av1 = av1;
                            if av1 {
                                outgoing_bitrate = Some(control);
                                continue;
                            }
                            outgoing_rtc = None;
                            outgoing_rtc_active = false;
                            resume_legacy = false;
                            control.bitrate.store(0, std::sync::atomic::Ordering::Relaxed);
                            outgoing_bitrate = Some(Arc::clone(&control));
                            outgoing_size = (width, height);
                            send_rate = Default::default();
                            next_share_log = Instant::now();
                            active_call.as_ref().map(|(user_id, call_id)| {
                                if peer_screen_rtc && !hdr10 {
                                    next_share_id += 1;
                                    let id = format!("{call_id}-{}-{next_share_id}", session.user_id);
                                    let offer_av1 = peer_av1 && can_av1_encode && !control.av1_failed.load(std::sync::atomic::Ordering::Relaxed);
                                    outgoing_rtc = Some(social_screen_rtc::start(id, session.token.clone(), None, offer_av1, control.profile_bitrate, rtc_events.clone()));
                                }
                                let mut signal = social_signal_envelope(
                                    "screen-share-started",
                                    *user_id,
                                    call_id.clone(),
                                );
                                let width = i32::try_from(width).unwrap_or(i32::MAX);
                                let height = i32::try_from(height).unwrap_or(i32::MAX);
                                signal.screen_frame_width = Some(width);
                                signal.screen_frame_height = Some(height);
                                signal.screen_width = Some(width);
                                signal.screen_height = Some(height);
                                signal.screen_codec = Some(if hdr10 { "hevc-hdr10" } else { "h264" }.to_owned());
                                signal
                            })
                        }
                        SocialCallCommand::ScreenFrame {
                            data,
                            width,
                            height,
                            timestamp_ms,
                            key_frame,
                            slot,
                        } => active_call.as_ref().map(|(user_id, call_id)| {
                            if let Some(rtc) = &outgoing_rtc
                                && rtc.av1 == outgoing_av1
                                && (outgoing_rtc_active || key_frame)
                                && rtc.ready.load(std::sync::atomic::Ordering::Acquire)
                                && let Ok(permit) = rtc.frames.try_reserve()
                            {
                                if !outgoing_rtc_active {
                                    tracing::info!("screen share switched to WebRTC");
                                }
                                outgoing_rtc_active = true;
                                if let Some(target) = &outgoing_bitrate {
                                    target.bitrate.store(rtc.bitrate.load(std::sync::atomic::Ordering::Relaxed), std::sync::atomic::Ordering::Relaxed);
                                }
                                video_slot = Some((slot.clone(), data.len()));
                                permit.send(social_screen_rtc::Frame { data, timestamp_ms, _slot: slot });
                                return None;
                            }
                            if outgoing_av1 {
                                if let Some(target) = &outgoing_bitrate {
                                    target.av1_failed.store(true, std::sync::atomic::Ordering::Relaxed);
                                    target.use_av1.store(false, std::sync::atomic::Ordering::Relaxed);
                                    target.bitrate.store(0, std::sync::atomic::Ordering::Relaxed);
                                }
                                outgoing_rtc = None;
                                outgoing_rtc_active = false;
                                return None;
                            }
                            if let Some(rtc) = &outgoing_rtc
                                && rtc.av1
                                && let Some(target) = &outgoing_bitrate
                            {
                                if target.av1_failed.load(std::sync::atomic::Ordering::Relaxed) {
                                    next_share_id += 1;
                                    let id = format!("{call_id}-{}-{next_share_id}", session.user_id);
                                    outgoing_rtc = Some(social_screen_rtc::start(id, session.token.clone(), None, false, target.profile_bitrate, rtc_events.clone()));
                                } else if rtc.ready.load(std::sync::atomic::Ordering::Acquire) {
                                    target.use_av1.store(true, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            if outgoing_rtc_active {
                                outgoing_rtc = None;
                                outgoing_rtc_active = false;
                                resume_legacy = true;
                                if let Some(target) = &outgoing_bitrate {
                                    target.bitrate.store(0, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            if resume_legacy && !key_frame {
                                return None;
                            }
                            video_slot = Some((slot, data.len()));
                            let mut signal = social_signal_envelope(
                                if outgoing_hdr10 { "screen-frame-hevc" } else { "screen-frame-h264" },
                                *user_id,
                                call_id.clone(),
                            );
                            let width = i32::try_from(width).unwrap_or(i32::MAX);
                            let height = i32::try_from(height).unwrap_or(i32::MAX);
                            signal.screen_frame_base64 =
                                Some(base64::engine::general_purpose::STANDARD.encode(data));
                            signal.screen_frame_width = Some(width);
                            signal.screen_frame_height = Some(height);
                            signal.screen_width = Some(width);
                            signal.screen_height = Some(height);
                            signal.screen_frame_timestamp = Some(timestamp_ms);
                            signal.timestamp_utc_ms = Some(timestamp_ms);
                            signal.screen_codec = Some(if outgoing_hdr10 { "hevc-hdr10" } else { "h264" }.to_owned());
                            signal.screen_frame_is_key_frame = Some(key_frame);
                            if std::mem::take(&mut resume_legacy) {
                                signal.screen_rtc_id = Some("legacy".into());
                            }
                            Some(signal)
                        }).flatten(),
                        SocialCallCommand::ScreenStopped => {
                            outgoing_rtc = None;
                            outgoing_bitrate = None;
                            outgoing_rtc_active = false;
                            resume_legacy = false;
                            active_call.as_ref().map(|(user_id, call_id)| {
                                social_signal_envelope(
                                    "screen-share-stopped",
                                    *user_id,
                                    call_id.clone(),
                                )
                            })
                        }
                    };
                    if let Some(envelope) = envelope
                        && let Ok(json) = serde_json::to_string(&envelope)
                        && let Err(error) = writer.send(Message::Text(json.into())).await
                    {
                        push_social_call_event(SocialCallEvent::Failed(format!(
                            "Could not send the call signal: {error}"
                        )));
                        return;
                    }
                    if let Some((_slot, bytes)) = video_slot
                        && let Some((fps, mbps)) = send_rate.frame(bytes)
                    {
                        push_social_call_event(SocialCallEvent::SentShareRate { fps, mbps, rtc: outgoing_rtc_active, av1: outgoing_av1 });
                        if Instant::now() >= next_share_log {
                            tracing::info!(fps, mbps, "screen share send rate");
                            next_share_log = Instant::now() + Duration::from_secs(5);
                        }
                    }
                }

                let message = tokio::select! {
                    message = reader.next() => message,
                    command = command_rx.recv() => {
                        let Some(command) = command else { return };
                        ready_command = Some(command);
                        continue;
                    }
                    Some((id, event)) = rtc_rx.recv() => {
                        let outgoing = outgoing_rtc.as_ref().is_some_and(|rtc| rtc.id == id);
                        let incoming = incoming_rtc.as_ref().is_some_and(|(rtc, _, _)| rtc.id == id);
                        match event {
                            social_screen_rtc::Event::Description(sdp) if outgoing || incoming => {
                                if let Some((user, call_id)) = &active_call {
                                    let mut signal = social_signal_envelope(if outgoing { "webrtc-offer" } else { "webrtc-answer" }, *user, call_id.clone());
                                    signal.sdp = Some(sdp);
                                    signal.screen_rtc_id = Some(id);
                                    if outgoing {
                                        signal.screen_frame_width = i32::try_from(outgoing_size.0).ok();
                                        signal.screen_frame_height = i32::try_from(outgoing_size.1).ok();
                                        signal.screen_codec = Some(if outgoing_rtc.as_ref().is_some_and(|rtc| rtc.av1) { "av1" } else { "h264" }.into());
                                    }
                                    if let Ok(json) = serde_json::to_string(&signal)
                                        && writer.send(Message::Text(json.into())).await.is_err()
                                    {
                                        push_social_call_event(SocialCallEvent::Failed("Could not send the screen connection details.".into()));
                                        return;
                                    }
                                }
                            }
                            social_screen_rtc::Event::Frame { data, timestamp_ms, received, key_frame } if incoming && active_call.is_some() => {
                                if !incoming_rtc_active {
                                    if !key_frame { continue; }
                                    let (rtc, width, height) = incoming_rtc.as_ref().unwrap();
                                    push_social_call_event(SocialCallEvent::RemoteScreenStarted { width: *width, height: *height, hdr10: false, av1: rtc.av1, rtc: true });
                                    incoming_rtc_active = true;
                                }
                                push_social_call_event(SocialCallEvent::RemoteScreenPacket { data, timestamp_ms: Some(timestamp_ms), received });
                            }
                            social_screen_rtc::Event::Closed if outgoing => outgoing_rtc = None,
                            social_screen_rtc::Event::KeyframeRequested if outgoing => {
                                if let Some(control) = &outgoing_bitrate {
                                    control.keyframe_requested.store(true, std::sync::atomic::Ordering::Release);
                                }
                            }
                            social_screen_rtc::Event::Closed if incoming && !incoming_rtc_active => incoming_rtc = None,
                            _ => {}
                        }
                        continue;
                    }
                    _ = tokio::time::sleep_until(tokio::time::Instant::from_std(next_ping)) => continue,
                };
                match message {
                    Some(Ok(Message::Pong(_))) => last_pong = Instant::now(),
                    Some(Ok(Message::Text(text))) => {
                        let received = Instant::now();
                        let Ok(signal) = serde_json::from_str::<SocialSignalEnvelope>(&text) else {
                            continue;
                        };
                        let matches_call = |call: &Option<(i64, String)>| {
                            call.as_ref().is_some_and(|(peer, id)| {
                                *peer == signal.from_user_id && signal.call_id.as_ref() == Some(id)
                            })
                        };
                        let from_active = matches_call(&active_call);
                        let from_pending = matches_call(&pending_invite);
                        match signal.message_type.as_str() {
                            "incoming-call" => {
                                if let Some(call_id) = signal.call_id {
                                    if active_call.is_none() && pending_invite.is_none() {
                                        pending_invite = Some((signal.from_user_id, call_id.clone()));
                                    }
                                    push_social_call_event(SocialCallEvent::Incoming {
                                        user_id: signal.from_user_id,
                                        call_id,
                                        display_name: if signal.from_display_name.is_empty() {
                                            signal.from_username
                                        } else {
                                            signal.from_display_name
                                        },
                                    });
                                }
                            }
                            "call-accepted" | "call-answer" if from_active => {
                                push_social_call_event(SocialCallEvent::Connected);
                            }
                            "audio-chunk" if from_active => {
                                if signal.from_user_id != session.user_id
                                    && let Some(encoded) = signal.audio_base64
                                    && let Ok(packet) =
                                        base64::engine::general_purpose::STANDARD.decode(encoded)
                                {
                                    push_social_call_event(SocialCallEvent::Audio(packet));
                                }
                            }
                            "screen-share-capabilities" if from_active => {
                                let codecs = signal.screen_codecs.unwrap_or_default();
                                let had_rtc_capability = peer_screen_rtc;
                                peer_screen_rtc = codecs.iter().any(|codec| codec == social_screen_rtc::CAPABILITY);
                                peer_av1 = codecs.iter().any(|codec| codec == social_screen_rtc::AV1_CAPABILITY);
                                let hdr10 = codecs.iter().any(|codec| codec == "hevc-hdr10");
                                push_social_call_event(SocialCallEvent::PeerHdrAvailable(hdr10));
                                if !had_rtc_capability && peer_screen_rtc && !outgoing_hdr10
                                    && let (Some((_, call_id)), Some(control)) = (&active_call, &outgoing_bitrate)
                                    && outgoing_rtc.is_none()
                                {
                                    next_share_id += 1;
                                    let id = format!("{call_id}-{}-{next_share_id}", session.user_id);
                                    let av1 = peer_av1 && can_av1_encode && !control.av1_failed.load(std::sync::atomic::Ordering::Relaxed);
                                    outgoing_rtc = Some(social_screen_rtc::start(id, session.token.clone(), None, av1, control.profile_bitrate, rtc_events.clone()));
                                }
                            }
                            "webrtc-offer" if from_active && peer_screen_rtc => {
                                let av1 = signal.screen_codec.as_deref() == Some("av1");
                                if let (Some(id), Some(sdp), Some(width), Some(height)) = (
                                    signal.screen_rtc_id, signal.sdp,
                                    signal.screen_frame_width.and_then(|value| u32::try_from(value).ok()),
                                    signal.screen_frame_height.and_then(|value| u32::try_from(value).ok()),
                                ) && (2..=4096).contains(&width) && (2..=2160).contains(&height)
                                    && width % 2 == 0 && height % 2 == 0
                                    && (!av1 || can_av1_decode)
                                    && id.len() <= 256 && sdp.len() <= 65_536
                                {
                                    incoming_rtc = Some((social_screen_rtc::start(id, session.token.clone(), Some(sdp), av1, 0, rtc_events.clone()), width, height));
                                    incoming_rtc_active = false;
                                }
                            }
                            "screen-share-request-h264" if from_active && outgoing_rtc.as_ref().is_some_and(|rtc| rtc.av1) => {
                                outgoing_rtc = None;
                                if let Some(control) = &outgoing_bitrate {
                                    control.av1_failed.store(true, std::sync::atomic::Ordering::Relaxed);
                                    control.use_av1.store(false, std::sync::atomic::Ordering::Relaxed);
                                    control.bitrate.store(0, std::sync::atomic::Ordering::Relaxed);
                                }
                            }
                            "webrtc-answer" if from_active => {
                                if let Some(rtc) = &mut outgoing_rtc
                                    && signal.screen_rtc_id.as_deref() == Some(rtc.id.as_str())
                                    && let (Some(answer), Some(sdp)) = (rtc.answer.take(), signal.sdp)
                                    && sdp.len() <= 65_536
                                {
                                    let _ = answer.send(sdp);
                                }
                            }
                            "screen-share-request-sdr" if from_active => push_social_call_event(SocialCallEvent::PeerRequestsSdr),
                            "screen-share-started" if from_active => {
                                incoming_rtc = None;
                                incoming_rtc_active = false;
                                let width = signal
                                    .screen_frame_width
                                    .or(signal.screen_width)
                                    .and_then(|value| u32::try_from(value).ok())
                                    .unwrap_or(1280);
                                let height = signal
                                    .screen_frame_height
                                    .or(signal.screen_height)
                                    .and_then(|value| u32::try_from(value).ok())
                                    .unwrap_or(720);
                                if width < 2 || height < 2 || width > 4096 || height > 2160 || width % 2 != 0 || height % 2 != 0 {
                                    continue;
                                }
                                let hdr10 = signal.screen_codec.as_deref() == Some("hevc-hdr10");
                                push_social_call_event(SocialCallEvent::RemoteScreenStarted {
                                    width,
                                    height,
                                    hdr10,
                                    av1: false,
                                    rtc: false,
                                });
                            }
                            "screen-frame-h264" | "screen-frame-hevc" if from_active => {
                                if incoming_rtc_active {
                                    if signal.screen_rtc_id.as_deref() != Some("legacy") { continue; }
                                    if let Some((_, width, height)) = incoming_rtc.take() {
                                        push_social_call_event(SocialCallEvent::RemoteScreenStarted { width, height, hdr10: false, av1: false, rtc: false });
                                    }
                                    incoming_rtc_active = false;
                                }
                                if signal.from_user_id != session.user_id
                                    && let Some(encoded) = signal.screen_frame_base64
                                    && let Ok(packet) =
                                        base64::engine::general_purpose::STANDARD.decode(encoded)
                                {
                                    push_social_call_event(SocialCallEvent::RemoteScreenPacket {
                                        data: packet,
                                        timestamp_ms: signal.screen_frame_timestamp,
                                        received,
                                    });
                                }
                            }
                            "screen-share-stopped" if from_active => {
                                incoming_rtc = None;
                                incoming_rtc_active = false;
                                push_social_call_event(SocialCallEvent::RemoteScreenStopped);
                            }
                            "call-decline" | "call-declined" | "call-rejected" if from_active => {
                                active_call = None;
                                outgoing_rtc = None;
                                incoming_rtc = None;
                                outgoing_bitrate = None;
                                push_social_call_event(SocialCallEvent::Ended(
                                    "The call was declined.".to_owned(),
                                ));
                            }
                            "call-ended" if from_active || from_pending => {
                                if from_active {
                                    active_call = None;
                                    outgoing_rtc = None;
                                    incoming_rtc = None;
                                    outgoing_bitrate = None;
                                }
                                if from_pending { pending_invite = None; }
                                push_social_call_event(SocialCallEvent::Ended(
                                    signal.debug_message.unwrap_or_else(|| {
                                        "Call ended.".to_owned()
                                    }),
                                ));
                            }
                            _ => {}
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        push_social_call_event(SocialCallEvent::Ended(
                            "Call signalling disconnected.".to_owned(),
                        ));
                        return;
                    }
                    Some(Err(error)) => {
                        push_social_call_event(SocialCallEvent::Failed(format!(
                            "Call signalling error: {error}"
                        )));
                        return;
                    }
                    Some(Ok(_)) => {}
                }
            }
        });
    });
    command_tx
}

fn social_load_session(path: &Path) -> anyhow::Result<SocialSession> {
    let bytes = std::fs::read(path).context("read Zink Social session")?;
    serde_json::from_slice(&bytes).context("parse Zink Social session")
}

fn load_social_profile(path: &Path) -> Option<SocialProfileDetails> {
    fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
}

fn save_social_profile(path: &Path, profile: &SocialProfileDetails) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(profile).context("serialize Zink Social profile")?;
    fs::write(path, bytes).context("save Zink Social profile")
}

fn apply_social_profile_to_ui(ui: &AppWindow, profile: &SocialProfileDetails) {
    ui.set_social_signed_up(SharedString::from(if profile.signed_up.is_empty() {
        "Not available"
    } else {
        profile.signed_up.as_str()
    }));
    let mut links = profile.links.iter();
    let first = links.next().cloned().unwrap_or_default();
    let second = links.next().cloned().unwrap_or_default();
    let third = links.next().cloned().unwrap_or_default();
    ui.set_social_link_one_service(SharedString::from(first.service));
    ui.set_social_link_one_url(SharedString::from(first.url));
    ui.set_social_link_two_service(SharedString::from(second.service));
    ui.set_social_link_two_url(SharedString::from(second.url));
    ui.set_social_link_three_service(SharedString::from(third.service));
    ui.set_social_link_three_url(SharedString::from(third.url));
    if profile.avatar_path.is_empty() {
        ui.set_social_avatar_set(false);
        ui.set_social_avatar_image(slint::Image::default());
    } else if let Ok(image) = slint::Image::load_from_path(Path::new(&profile.avatar_path)) {
        ui.set_social_avatar_image(image);
        ui.set_social_avatar_set(true);
    } else {
        ui.set_social_avatar_set(false);
        ui.set_social_avatar_image(slint::Image::default());
    }
}

fn social_messages_path(root: &Path, owner_id: i64) -> PathBuf {
    root.join(format!("zink-messages-{}.json", owner_id.max(0)))
}

fn load_social_messages(path: &Path) -> Vec<SavedSocialConversation> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_social_messages(
    path: &Path,
    conversations: &[SavedSocialConversation],
) -> anyhow::Result<()> {
    let bytes =
        serde_json::to_vec_pretty(conversations).context("serialize saved Social messages")?;
    std::fs::write(path, bytes).context("save Social messages")
}

fn refresh_social_message_models(ui: &AppWindow, state: &SocialMessageState) {
    let mut conversations = state.conversations.clone();
    conversations.sort_by_key(|conversation| std::cmp::Reverse(conversation.updated_unix));
    ui.set_social_conversations(ModelRc::from(Rc::new(VecModel::from(
        conversations
            .iter()
            .map(|conversation| SocialConversationItem {
                user_id: i32::try_from(conversation.user_id).unwrap_or_default(),
                username: SharedString::from(conversation.username.clone()),
                display_name: SharedString::from(conversation.display_name.clone()),
                preview: SharedString::from(
                    conversation
                        .messages
                        .last()
                        .map_or("", |message| message.text.as_str()),
                ),
                updated: SharedString::from(format_social_time(conversation.updated_unix)),
            })
            .collect::<Vec<_>>(),
    ))));
    let messages = state
        .active_user_id
        .and_then(|user_id| {
            state
                .conversations
                .iter()
                .find(|item| item.user_id == user_id)
        })
        .map(|conversation| {
            conversation
                .messages
                .iter()
                .map(|message| SocialMessageItem {
                    text: SharedString::from(message.text.clone()),
                    sent: SharedString::from(format_social_time(message.sent_unix)),
                    is_from_me: message.is_from_me,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    ui.set_social_messages(ModelRc::from(Rc::new(VecModel::from(messages))));
}

fn format_social_time(timestamp: u64) -> String {
    if timestamp == 0 {
        String::new()
    } else {
        format!("{timestamp}")
    }
}

fn load_notifications(path: &Path) -> Vec<SavedNotification> {
    std::fs::read(path)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_notifications(path: &Path, notifications: &[SavedNotification]) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(notifications).context("serialize notifications")?;
    std::fs::write(path, bytes).context("write notification history")
}

fn refresh_notification_model(ui: &AppWindow, notifications: &[SavedNotification]) {
    let mut notifications = notifications.to_vec();
    notifications.sort_by_key(|item| std::cmp::Reverse(item.timestamp_unix));
    ui.set_notification_items(ModelRc::from(Rc::new(VecModel::from(
        notifications
            .into_iter()
            .map(|item| NotificationItem {
                id: SharedString::from(item.id),
                title: SharedString::from(item.title),
                message: SharedString::from(item.message),
                timestamp: SharedString::from(format_social_time(item.timestamp_unix)),
                action_label: SharedString::from(item.action_label),
                action_uri: SharedString::from(item.action_uri),
            })
            .collect::<Vec<_>>(),
    ))));
}

fn social_login(identity: &str, password: &str) -> anyhow::Result<SocialSession> {
    let response = ureq::post(format!("{SOCIAL_BASE_URL}/api/login"))
        .header("Content-Type", "application/json")
        .send(
            serde_json::json!({
                "Username": identity.trim(),
                "Password": password,
            })
            .to_string(),
        )
        .context("contact Zink Social login")?;
    let body = response
        .into_body()
        .read_to_string()
        .context("read Zink Social login response")?;
    let json: serde_json::Value =
        serde_json::from_str(&body).context("parse Zink Social login response")?;
    let token = social_json_string(&json, &["token", "Token"]);
    if token.is_empty() {
        anyhow::bail!(social_api_message(
            &json,
            "The server did not return a session token"
        ));
    }
    Ok(SocialSession {
        user_id: social_json_i64(&json, &["userId", "UserId"]),
        username: social_json_string(&json, &["username", "Username"]),
        display_name: social_json_string(&json, &["displayName", "DisplayName"]),
        token,
    })
}

fn social_register(
    username: &str,
    password: &str,
    display_name: &str,
) -> anyhow::Result<SocialSession> {
    ureq::post(format!("{SOCIAL_BASE_URL}/api/register"))
        .header("Content-Type", "application/json")
        .send(serde_json::json!({
            "Username": username.trim(),
            "Password": password,
            "DisplayName": if display_name.trim().is_empty() { username.trim() } else { display_name.trim() },
        }).to_string())
        .context("create Zink Social account")?;
    social_login(username, password)
}

fn social_get_json(path: &str, token: &str) -> anyhow::Result<String> {
    let response = ureq::get(format!("{SOCIAL_BASE_URL}{path}"))
        .header("Authorization", format!("Bearer {token}"))
        .call()
        .with_context(|| format!("load Zink Social {path}"))?;
    response
        .into_body()
        .read_to_string()
        .with_context(|| format!("read Zink Social {path}"))
}

fn social_load_people(
    session: &SocialSession,
) -> anyhow::Result<(Vec<SocialPersonData>, Vec<SocialRequestData>)> {
    let friends = serde_json::from_str(&social_get_json("/api/friends", &session.token)?)
        .context("parse Zink Social friends")?;
    let requests = serde_json::from_str(&social_get_json("/api/friends/requests", &session.token)?)
        .context("parse Zink Social friend requests")?;
    Ok((friends, requests))
}

fn social_search(session: &SocialSession, query: &str) -> anyhow::Result<Vec<SocialPersonData>> {
    let encoded: String = url::form_urlencoded::byte_serialize(query.trim().as_bytes()).collect();
    serde_json::from_str(&social_get_json(
        &format!("/api/users/search?q={encoded}"),
        &session.token,
    )?)
    .context("parse Zink Social search results")
}

fn social_post(
    session: &SocialSession,
    path: &str,
    body: Option<serde_json::Value>,
) -> anyhow::Result<()> {
    let request = ureq::post(format!("{SOCIAL_BASE_URL}{path}"))
        .header("Authorization", format!("Bearer {}", session.token));
    if let Some(body) = body {
        request
            .header("Content-Type", "application/json")
            .send(body.to_string())
            .context("send Zink Social action")?;
    } else {
        request.send_empty().context("send Zink Social action")?;
    }
    Ok(())
}

fn social_json_string(json: &serde_json::Value, keys: &[&str]) -> String {
    keys.iter()
        .find_map(|key| json.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or_default()
        .to_owned()
}

fn social_json_i64(json: &serde_json::Value, keys: &[&str]) -> i64 {
    keys.iter()
        .find_map(|key| json.get(key).and_then(serde_json::Value::as_i64))
        .unwrap_or_default()
}

fn social_api_message(json: &serde_json::Value, fallback: &str) -> String {
    let message = social_json_string(json, &["message", "Message", "error", "Error"]);
    if message.is_empty() {
        fallback.to_owned()
    } else {
        message
    }
}

fn push_social_result(result: SocialResult) {
    if let Ok(mut results) = SOCIAL_RESULTS.get_or_init(|| Mutex::new(Vec::new())).lock() {
        results.push(result);
    }
}

fn social_friend_item(person: SocialPersonData) -> SocialFriendItem {
    SocialFriendItem {
        user_id: i32::try_from(person.user_id).unwrap_or_default(),
        username: SharedString::from(person.username),
        display_name: SharedString::from(person.display_name),
        avatar_url: SharedString::from(person.avatar_url),
        presence: SharedString::from(person.presence),
        is_online: person.is_online,
        is_friend: person.is_friend,
    }
}

fn social_request_item(request: SocialRequestData) -> SocialRequestItem {
    SocialRequestItem {
        request_id: i32::try_from(request.request_id).unwrap_or_default(),
        username: SharedString::from(request.from_username),
        display_name: SharedString::from(request.from_display_name),
        created: SharedString::from(request.created_utc),
    }
}

fn base64_url_no_padding(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        output.push(char::from(TABLE[usize::from(a >> 2)]));
        output.push(char::from(TABLE[usize::from(((a & 0x03) << 4) | (b >> 4))]));
        if chunk.len() > 1 {
            output.push(char::from(TABLE[usize::from(((b & 0x0f) << 2) | (c >> 6))]));
        }
        if chunk.len() > 2 {
            output.push(char::from(TABLE[usize::from(c & 0x3f)]));
        }
    }
    output
}

fn home_route_icon(route: &str) -> &'static str {
    match route {
        "music-player" | "music-library" => "",
        "video-player" | "video-library" | "tv" | "films" => "",
        "radio" | "radio-discovery" => "",
        "screen-recorder" | "recordings" | "fps-recorder" => "",
        "social" | "friends" | "messages" | "call" => "",
        "zink-connect" => "",
        "settings" | "customization" => "",
        "search" => "",
        _ => "",
    }
}

#[cfg(windows)]
fn local_hour() -> u16 {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let local = unsafe { GetLocalTime() };
    local.wHour
}

#[cfg(not(windows))]
fn local_hour() -> u16 {
    12
}

fn apply_home_settings(ui: &AppWindow, settings: &AppSettings) {
    ui.set_home_space(SharedString::from(settings.home_space.clone()));
    ui.set_home_show_flow(settings.home_show_flow);
    ui.set_home_show_live_activities(settings.home_show_live_activities);
    ui.set_home_show_insights(settings.home_show_insights);
    ui.set_home_show_devices(settings.home_show_devices);

    let hour = local_hour();
    let (greeting, mode) = match hour {
        5..=11 => (
            "Good morning",
            "Start your day with music, messages and what matters",
        ),
        12..=16 => ("Good afternoon", "Keep your day moving in one place"),
        17..=21 => (
            "Good evening",
            "Pick up your entertainment and conversations",
        ),
        _ => ("Welcome back", "A quieter Zink for late-night listening"),
    };
    ui.set_home_greeting(SharedString::from(greeting));
    ui.set_home_day_mode(SharedString::from(mode));

    let pinned = settings
        .home_pinned_routes
        .iter()
        .filter_map(|id| route_by_id(id))
        .map(|route| NavItem {
            id: SharedString::from(route.id),
            title: SharedString::from(route.title),
            group: SharedString::from(route.group),
        })
        .collect::<Vec<_>>();
    ui.set_home_pinned_actions(ModelRc::from(Rc::new(VecModel::from(pinned))));

    let activities = settings
        .home_route_history
        .iter()
        .rev()
        .filter_map(|id| route_by_id(id))
        .take(8)
        .map(|route| HomeActivityItem {
            title: SharedString::from(route.title),
            detail: SharedString::from(format!("Continue in {}", route.group)),
            route: SharedString::from(route.id),
            icon: SharedString::from(home_route_icon(route.id)),
        })
        .collect::<Vec<_>>();
    ui.set_home_activities(ModelRc::from(Rc::new(VecModel::from(activities))));
}

#[cfg(windows)]
fn check_and_install_store_updates() -> anyhow::Result<String> {
    use windows::Services::Store::{StoreContext, StorePackageUpdateState};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize, RoUninitialize};

    struct RoApartment;
    impl Drop for RoApartment {
        fn drop(&mut self) {
            unsafe { RoUninitialize() };
        }
    }

    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }.context("initialize Windows Runtime")?;
    let _apartment = RoApartment;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create Store update runtime")?;
    runtime.block_on(async {
        let context = StoreContext::GetDefault().context("open Microsoft Store context")?;
        let updates = context
            .GetAppAndOptionalStorePackageUpdatesAsync()
            .context("start Store update query")?
            .await
            .context("query Store updates")?;
        let count = updates.Size().context("count Store updates")?;
        if count == 0 {
            return Ok("Zink is up to date. No Microsoft Store updates are available.".to_owned());
        }
        let result = context
            .RequestDownloadAndInstallStorePackageUpdatesAsync(&updates)
            .context("start Store update installation")?
            .await
            .context("install Store updates")?;
        let state = result.OverallState().context("read Store update result")?;
        let message = match state {
            StorePackageUpdateState::Completed => {
                format!("Installed {count} Zink update(s). Restart Zink to finish applying them.")
            }
            StorePackageUpdateState::Canceled => {
                "The Microsoft Store update was cancelled.".to_owned()
            }
            StorePackageUpdateState::ErrorLowBattery => {
                "Windows postponed the update because the battery is low.".to_owned()
            }
            StorePackageUpdateState::ErrorWiFiRecommended => {
                "Windows recommends Wi-Fi before downloading this update.".to_owned()
            }
            StorePackageUpdateState::ErrorWiFiRequired => {
                "Windows requires Wi-Fi before downloading this update.".to_owned()
            }
            StorePackageUpdateState::OtherError => {
                "Microsoft Store could not install the available update.".to_owned()
            }
            StorePackageUpdateState::Pending
            | StorePackageUpdateState::Downloading
            | StorePackageUpdateState::Deploying => {
                format!("Microsoft Store is still processing {count} Zink update(s).")
            }
            _ => format!("Microsoft Store returned update state {}.", state.0),
        };
        Ok(message)
    })
}

fn apply_general_settings(ui: &AppWindow, settings: &AppSettings) {
    let active_language = effective_language_tag(&settings.language);
    let routes = PRIMARY_ROUTES
        .iter()
        .map(|route| NavItem {
            id: SharedString::from(route.id),
            title: SharedString::from(core_translation(&active_language, route.title)),
            group: SharedString::from(core_translation(&active_language, route.group)),
        })
        .collect::<Vec<_>>();
    ui.set_nav_items(ModelRc::from(Rc::new(VecModel::from(routes))));
    apply_localized_strings(ui, &active_language);
    ui.set_settings_language_options(ModelRc::from(Rc::new(VecModel::from(
        SETTINGS_LANGUAGES
            .iter()
            .map(|(name, _)| SharedString::from(*name))
            .collect::<Vec<_>>(),
    ))));
    ui.set_ui_theme_choice(SharedString::from(match settings.theme {
        ThemePreference::System => "System",
        ThemePreference::Light => "Light",
        ThemePreference::Dark => "Dark",
    }));
    let accent = settings.accent_color.to_ascii_uppercase();
    let accent_name = match accent.as_str() {
        "#38FF66" => "Zink green",
        "#14D8E8" | "#11D8E8" => "Cyan",
        "#1EDC8A" => "Emerald",
        "#9EFF4F" => "Lime",
        "#0E7B5A" => "Forest",
        "#06404A" => "Zink teal",
        "#123B64" => "Ocean blue",
        "#252A35" => "Graphite",
        "#9A77FF" => "Violet",
        "#FF7B68" => "Sunset",
        "#60A5FA" => "Windows",
        _ => accent.as_str(),
    };
    ui.set_ui_accent_choice(SharedString::from(accent_name));
    ui.set_ui_custom_accent_hex(SharedString::from(accent));
    ui.set_ui_accent_color(slint_color_from_hex(&settings.accent_color));
    ui.set_compact_navigation(settings.compact_navigation);
    ui.set_reduce_motion(settings.reduce_motion);
    ui.set_lighter_sdr_theme(settings.lighter_sdr_theme);
    ui.set_notifications_playback(settings.notifications_playback);
    ui.set_notifications_social(settings.notifications_social);
    ui.set_notifications_updates(settings.notifications_updates);
    ui.set_settings_language(SharedString::from(language_display_name(
        &settings.language,
    )));
    let current_route = ui.get_current_route();
    if !current_route.is_empty() {
        apply_route(ui, current_route.as_str());
    }
    ui.set_settings_discord_presence(settings.discord_rich_presence);
    ui.set_settings_run_startup(settings.run_at_startup);
    ui.set_settings_reopen_last_page(settings.reopen_last_page);
    ui.set_settings_background_notifications(settings.background_notifications);
    ui.set_settings_low_resource(settings.low_resource_background);
    ui.set_settings_background_replay(settings.background_replay);
    ui.set_settings_hotkey_recording(settings.clip_recording_hotkey);
    ui.set_settings_diagnostic_log(settings.diagnostic_log);
    ui.set_recorder_quality(SharedString::from(settings.recorder_quality.clone()));
    ui.set_recorder_capture_cursor(settings.recorder_capture_cursor);
    ui.set_recorder_output_device_id(SharedString::from(
        settings.recorder_output_device_id.clone(),
    ));
    ui.set_recorder_input_device_id(SharedString::from(
        settings.recorder_microphone_device_id.clone(),
    ));
    ui.set_call_output_device_name(SharedString::from(
        settings.social_call_output_device.clone(),
    ));
    ui.set_call_input_device_name(SharedString::from(
        settings.social_call_input_device.clone(),
    ));
}

fn apply_localized_strings(ui: &AppWindow, active_language: &str) {
    ui.set_localized_settings(SharedString::from(core_translation(
        active_language,
        "Settings",
    )));
    ui.set_localized_language(SharedString::from(core_translation(
        active_language,
        "Language",
    )));
    ui.set_localized_check_updates(SharedString::from(core_translation(
        active_language,
        "Check for updates",
    )));
    ui.set_localized_reset_defaults(SharedString::from(core_translation(
        active_language,
        "Reset defaults",
    )));
    ui.set_localized_background(SharedString::from(core_translation(
        active_language,
        "Background",
    )));
    ui.set_localized_on(SharedString::from(core_translation(active_language, "On")));
    ui.set_localized_off(SharedString::from(core_translation(active_language, "Off")));
    ui.set_localized_notifications(SharedString::from(core_translation(
        active_language,
        "Notifications",
    )));
    ui.set_localized_privacy(SharedString::from(core_translation(
        active_language,
        "Privacy",
    )));
    ui.set_localized_about(SharedString::from(core_translation(
        active_language,
        "About",
    )));
    apply_extended_localized_strings(ui, active_language);
}

fn apply_extended_localized_strings(ui: &AppWindow, active_language: &str) {
    ui.set_localized_personalise(SharedString::from(core_translation(
        active_language,
        "Personalise and control Zink",
    )));
    ui.set_localized_appearance(SharedString::from(core_translation(
        active_language,
        "Appearance",
    )));
    ui.set_localized_integrations(SharedString::from(core_translation(
        active_language,
        "Integrations",
    )));
    ui.set_localized_background_startup(SharedString::from(core_translation(
        active_language,
        "Background and startup",
    )));
    ui.set_localized_run_sign_in(SharedString::from(core_translation(
        active_language,
        "Run Zink at sign-in",
    )));
    ui.set_localized_reopen_last_page(SharedString::from(core_translation(
        active_language,
        "Reopen last page",
    )));
    ui.set_localized_low_resource(SharedString::from(core_translation(
        active_language,
        "Low memory and CPU mode",
    )));
    ui.set_localized_replay_hotkeys(SharedString::from(core_translation(
        active_language,
        "Replay and hotkeys",
    )));
    ui.set_localized_clip_hotkey(SharedString::from(core_translation(
        active_language,
        "Game clip recording hotkey",
    )));
    ui.set_localized_replay_settings(SharedString::from(core_translation(
        active_language,
        "Replay settings",
    )));
    ui.set_localized_diagnostics(SharedString::from(core_translation(
        active_language,
        "Diagnostics",
    )));
    ui.set_localized_maintenance(SharedString::from(core_translation(
        active_language,
        "Maintenance",
    )));
    ui.set_localized_diagnostic_log(SharedString::from(core_translation(
        active_language,
        "Diagnostic log file",
    )));
    ui.set_localized_open_log_folder(SharedString::from(core_translation(
        active_language,
        "Open log folder",
    )));
    ui.set_localized_clear_log(SharedString::from(core_translation(
        active_language,
        "Clear log",
    )));
    ui.set_localized_run_health_check(SharedString::from(core_translation(
        active_language,
        "Run health check",
    )));
    ui.set_localized_open_health_report(SharedString::from(core_translation(
        active_language,
        "Open health report",
    )));
    ui.set_localized_upload_support_bundle(SharedString::from(core_translation(
        active_language,
        "Upload support bundle",
    )));
    ui.set_localized_open_store_updates(SharedString::from(core_translation(
        active_language,
        "Open Store updates",
    )));
}

#[cfg(windows)]
fn request_settings_copy_translation(
    ui: slint::Weak<AppWindow>,
    cache_path: PathBuf,
    language: String,
) {
    const SOURCES: [&str; 18] = [
        "Control startup, replay capture, integrations, diagnostics and updates.",
        "Zink follows Windows unless you choose another language.",
        "Control how Zink connects with other applications.",
        "Publishes the active Zink page through Discord's native IPC. Private file paths are never included.",
        "Theme, colour, navigation and motion",
        "Playback, Social and update alerts",
        "Local data, permissions and browser controls",
        "Version, history, privacy and feedback",
        "Start quietly with Windows",
        "Return to your previous Zink destination",
        "Keep social and media alerts active",
        "Reduce background resource usage",
        "Background replay for gaming",
        "Keep recent gameplay ready to save",
        "Enable the global capture shortcut",
        "Confirm reset to defaults",
        "Select Confirm reset to defaults again to erase your customised settings.",
        "Checking Microsoft Store…",
    ];
    std::thread::spawn(move || {
        let translations = ui_translation::translate_batch(&cache_path, &language, &SOURCES);
        let _ = slint::invoke_from_event_loop(move || {
            let Some(ui) = ui.upgrade() else { return };
            let active = effective_language_tag(language_tag(ui.get_settings_language().as_str()));
            if active != language {
                return;
            }
            for (source, translated) in translations {
                let translated = SharedString::from(translated);
                match source.as_str() {
                    "Control startup, replay capture, integrations, diagnostics and updates." => {
                        ui.set_localized_settings_subtitle(translated);
                    }
                    "Zink follows Windows unless you choose another language." => {
                        ui.set_localized_language_description(translated);
                    }
                    "Control how Zink connects with other applications." => {
                        ui.set_localized_integrations_description(translated);
                    }
                    "Publishes the active Zink page through Discord's native IPC. Private file paths are never included." =>
                    {
                        ui.set_localized_discord_description(translated);
                    }
                    "Theme, colour, navigation and motion" => {
                        ui.set_localized_appearance_description(translated);
                    }
                    "Playback, Social and update alerts" => {
                        ui.set_localized_notifications_description(translated);
                    }
                    "Local data, permissions and browser controls" => {
                        ui.set_localized_privacy_description(translated);
                    }
                    "Version, history, privacy and feedback" => {
                        ui.set_localized_about_description(translated);
                    }
                    "Start quietly with Windows" => {
                        ui.set_localized_startup_description(translated);
                    }
                    "Return to your previous Zink destination" => {
                        ui.set_localized_reopen_description(translated);
                    }
                    "Keep social and media alerts active" => {
                        ui.set_localized_alerts_description(translated);
                    }
                    "Reduce background resource usage" => {
                        ui.set_localized_low_resource_description(translated);
                    }
                    "Background replay for gaming" => {
                        ui.set_localized_background_replay(translated);
                    }
                    "Keep recent gameplay ready to save" => {
                        ui.set_localized_background_replay_description(translated);
                    }
                    "Enable the global capture shortcut" => {
                        ui.set_localized_hotkey_description(translated);
                    }
                    "Confirm reset to defaults" => ui.set_localized_confirm_reset(translated),
                    "Select Confirm reset to defaults again to erase your customised settings." => {
                        ui.set_localized_reset_instruction(translated);
                    }
                    "Checking Microsoft Store…" => {
                        ui.set_localized_checking_store(translated);
                    }
                    _ => {}
                }
            }
        });
    });
}

fn equalizer_preset_curve(preset: &str) -> [f32; 10] {
    match preset {
        "Bass Boost" => [6.0, 5.0, 4.0, 2.0, 1.0, 0.0, -1.0, -2.0, -3.0, -4.0],
        "Treble Boost" => [-4.0, -3.0, -2.0, -1.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0],
        "Rock" => [4.0, 3.0, 1.0, 0.0, -1.0, -1.0, 1.0, 3.0, 4.0, 5.0],
        "Pop" => [-1.0, 2.0, 3.0, 2.0, 0.0, 1.0, 2.0, 3.0, 3.0, 2.0],
        "Classical" => [2.0, 1.0, 0.0, -1.0, -2.0, 0.0, 2.0, 3.0, 4.0, 4.0],
        _ => [0.0; 10],
    }
}

fn equalizer_band_array(values: &[f32]) -> [f64; 10] {
    let mut bands = [0.0_f64; 10];
    for (destination, source) in bands.iter_mut().zip(values) {
        *destination = f64::from(source.clamp(-12.0, 12.0));
    }
    bands
}

fn apply_equalizer_settings(ui: &AppWindow, settings: &AppSettings) {
    let mut bands = [0.0_f32; 10];
    for (destination, source) in bands.iter_mut().zip(&settings.equalizer_bands) {
        *destination = source.clamp(-12.0, 12.0);
    }
    ui.set_equalizer_enabled(settings.equalizer_enabled);
    ui.set_equalizer_master_gain(settings.equalizer_master_gain.clamp(-12.0, 12.0));
    ui.set_equalizer_preset(SharedString::from(settings.equalizer_preset.clone()));
    ui.set_equalizer_band_0(bands[0]);
    ui.set_equalizer_band_1(bands[1]);
    ui.set_equalizer_band_2(bands[2]);
    ui.set_equalizer_band_3(bands[3]);
    ui.set_equalizer_band_4(bands[4]);
    ui.set_equalizer_band_5(bands[5]);
    ui.set_equalizer_band_6(bands[6]);
    ui.set_equalizer_band_7(bands[7]);
    ui.set_equalizer_band_8(bands[8]);
    ui.set_equalizer_band_9(bands[9]);
    let strongest_boost = bands.iter().copied().fold(0.0_f32, f32::max);
    ui.set_equalizer_status(SharedString::from(if settings.equalizer_enabled {
        if strongest_boost > 0.0 {
            format!("Native gain is live with {strongest_boost:.0} dB automatic clipping headroom.")
        } else {
            "Native gain is live. The 10-band curve saves automatically.".to_owned()
        }
    } else {
        "Native gain bypassed. Your saved curve is preserved.".to_owned()
    }));
}

fn apply_visualizer_settings(ui: &AppWindow, settings: &AppSettings) {
    ui.set_visualizer_style(SharedString::from(settings.visualizer_style.clone()));
    ui.set_visualizer_color(SharedString::from(settings.visualizer_color.clone()));
    ui.set_visualizer_sensitivity(settings.visualizer_sensitivity.clamp(0, 100));
    ui.set_visualizer_smoothing(settings.visualizer_smoothing.clamp(0, 100));
    ui.set_visualizer_paused(settings.visualizer_paused);
    ui.set_visualizer_status(SharedString::from(if settings.visualizer_paused {
        "Paused"
    } else {
        "Live preview"
    }));
}

fn apply_radio_likes(ui: &AppWindow, settings: &AppSettings) {
    let items = settings
        .liked_radio_stations
        .iter()
        .map(|station| RadioLikeItem {
            station: SharedString::from(station.clone()),
            title: SharedString::from(station.clone()),
            artist: SharedString::from("Live radio station"),
        })
        .collect::<Vec<_>>();
    ui.set_liked_radio_items(ModelRc::from(Rc::new(VecModel::from(items))));
    let current = ui.get_track_title();
    ui.set_current_radio_liked(
        ui.get_track_detail().as_str() == "Internet radio"
            && settings
                .liked_radio_stations
                .iter()
                .any(|station| station == current.as_str()),
    );
}

fn load_liked_radio_songs(path: &Path) -> Vec<LikedRadioSongState> {
    let Ok(bytes) = fs::read(path) else {
        return Vec::new();
    };
    serde_json::from_slice::<Vec<LikedRadioSongState>>(&bytes).unwrap_or_else(|error| {
        tracing::warn!(%error, path = %path.display(), "could not read liked radio songs");
        Vec::new()
    })
}

fn save_liked_radio_songs(path: &Path, songs: &[LikedRadioSongState]) -> anyhow::Result<()> {
    let bytes = serde_json::to_vec_pretty(songs).context("serialize liked radio songs")?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, bytes).context("write liked radio songs")?;
    fs::rename(&temporary, path).context("commit liked radio songs")
}

fn liked_radio_artwork(value: &str) -> slint::Image {
    let trimmed = value.trim();
    let local = trimmed
        .strip_prefix("file:///")
        .map_or_else(|| trimmed.to_owned(), |path| path.replace('/', "\\"));
    if !local.is_empty() && Path::new(&local).is_file() {
        slint::Image::load_from_path(Path::new(&local)).unwrap_or_default()
    } else {
        slint::Image::default()
    }
}

fn apply_liked_radio_songs(ui: &AppWindow, songs: &[LikedRadioSongState]) {
    let items = songs
        .iter()
        .map(|song| LikedRadioSongItem {
            id: SharedString::from(song.id.clone()),
            title: SharedString::from(song.title.clone()),
            artist: SharedString::from(song.artist.clone()),
            album: SharedString::from(song.album.clone()),
            station: SharedString::from(song.station_name.clone()),
            artwork: liked_radio_artwork(&song.artwork_url),
            added_to_spotify: song.added_to_spotify_liked_songs,
        })
        .collect::<Vec<_>>();
    ui.set_liked_radio_songs(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn liked_radio_song_query(song: &LikedRadioSongState) -> String {
    [song.artist.trim(), song.title.trim(), song.album.trim()]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn web_search_url(base: &str, query: &str) -> String {
    let encoded = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("q", query)
        .finish();
    let value = encoded.strip_prefix("q=").unwrap_or(&encoded);
    format!("{base}{value}")
}

fn radio_uses_web_player(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.contains("bbc.co.uk/sounds/")
        || lower.contains("planetradio.co.uk/")
        || lower.contains("hellorayo.co.uk/")
        || lower.contains("globalplayer.com/")
        || lower.contains("premier.plus/")
        || lower.contains("mkfm.com/")
        || lower.contains("radioessex.com/")
}

fn parse_icy_stream_title(metadata: &[u8]) -> Option<(String, String)> {
    let text = metadata
        .iter()
        .take_while(|byte| **byte != 0)
        .map(|byte| char::from(*byte))
        .collect::<String>();
    let value = text
        .split(';')
        .find_map(|part| part.trim().strip_prefix("StreamTitle='"))?
        .trim_end_matches('\'')
        .trim();
    if value.is_empty() {
        return None;
    }
    if let Some((artist, title)) = value.split_once(" - ") {
        Some((artist.trim().to_owned(), title.trim().to_owned()))
    } else {
        Some((String::new(), value.to_owned()))
    }
}

fn start_icy_metadata_watcher(ui: slint::Weak<AppWindow>, uri: &str) {
    let generation = RADIO_METADATA_GENERATION.fetch_add(1, Ordering::SeqCst) + 1;
    let uri = uri.to_owned();
    std::thread::spawn(move || {
        let Ok(response) = ureq::get(&uri)
            .header("Icy-MetaData", "1")
            .header("User-Agent", "Zink/5.0")
            .call()
        else {
            return;
        };
        let Some(interval) = response
            .headers()
            .get("icy-metaint")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0 && *value <= 4 * 1024 * 1024)
        else {
            return;
        };
        let mut reader = response.into_body().into_reader();
        let mut audio = vec![0_u8; interval];
        while RADIO_METADATA_GENERATION.load(Ordering::SeqCst) == generation {
            if reader.read_exact(&mut audio).is_err() {
                return;
            }
            let mut length = [0_u8; 1];
            if reader.read_exact(&mut length).is_err() {
                return;
            }
            let metadata_length = usize::from(length[0]) * 16;
            if metadata_length == 0 {
                continue;
            }
            let mut metadata = vec![0_u8; metadata_length];
            if reader.read_exact(&mut metadata).is_err() {
                return;
            }
            let Some((artist, title)) = parse_icy_stream_title(&metadata) else {
                continue;
            };
            let weak = ui.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = weak.upgrade() {
                    ui.set_current_radio_track_artist(SharedString::from(artist));
                    ui.set_current_radio_track_title(SharedString::from(title));
                }
            });
        }
    });
}

fn start_radio_station(
    ui: &AppWindow,
    media: &RefCell<MediaEngine>,
    settings: &Arc<Mutex<AppSettings>>,
    station: &str,
    uri: &str,
) {
    RADIO_METADATA_GENERATION.fetch_add(1, Ordering::SeqCst);
    ui.set_current_radio_track_artist(SharedString::new());
    ui.set_current_radio_track_title(SharedString::new());
    ui.set_track_title(SharedString::from(station));
    ui.set_track_detail(SharedString::from("Internet radio"));
    ui.set_current_radio_stream_url(SharedString::from(if radio_uses_web_player(uri) {
        ""
    } else {
        uri
    }));
    ui.set_current_radio_liked(settings.lock().is_ok_and(|settings| {
        settings
            .liked_radio_stations
            .iter()
            .any(|item| item == station)
    }));
    if radio_uses_web_player(uri) {
        ui.set_is_playing(false);
        ui.set_playback_error(SharedString::from(
            "This station uses its official web player. Opening it in Zink Connect.",
        ));
        ui.invoke_open_web_service(SharedString::from(uri));
        return;
    }
    match media.borrow_mut().open_uri(uri) {
        Ok(()) => {
            ui.set_is_playing(true);
            ui.set_playback_error(SharedString::new());
            start_icy_metadata_watcher(ui.as_weak(), uri);
        }
        Err(error) => {
            error!(%error, %station, %uri, "radio playback failed");
            ui.set_is_playing(false);
            ui.set_playback_error(SharedString::from(error.to_string()));
        }
    }
}

#[allow(clippy::too_many_lines)] // The legacy station catalogue is intentionally kept together.
fn radio_station_registry(settings: &AppSettings) -> Vec<(String, String, String, bool)> {
    let defaults = [
        (
            "Capital FM",
            "Pop & chart",
            "https://media-ssl.musicradio.com/CapitalUK",
        ),
        (
            "Heart",
            "Feel-good music",
            "https://media-ssl.musicradio.com/HeartUK",
        ),
        (
            "Heart Milton Keynes",
            "Local pop",
            "https://media-ssl.musicradio.com/HeartMiltonKeynesMP3",
        ),
        (
            "Kiss FM",
            "Dance & R&B",
            "https://media-ssl.musicradio.com/Kiss",
        ),
        (
            "Smooth Radio",
            "Relaxing music",
            "https://media-ssl.musicradio.com/SmoothUK",
        ),
        (
            "Magic Radio",
            "More of the songs you love",
            "https://planetradio.co.uk/magic/player/",
        ),
        (
            "BBC Radio 1",
            "New music",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_one",
        ),
        (
            "BBC Radio 1Xtra",
            "Hip-hop & R&B",
            "https://www.bbc.co.uk/sounds/play/live:bbc_1xtra",
        ),
        (
            "BBC Radio 2",
            "Music & entertainment",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_two",
        ),
        (
            "BBC Radio 3",
            "Classical & culture",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_three",
        ),
        (
            "BBC Radio 4",
            "Speech & news",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_fourfm",
        ),
        (
            "BBC Radio 4 Extra",
            "Comedy & drama",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_four_extra",
        ),
        (
            "BBC Radio 5 Live",
            "News & sport",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_five_live",
        ),
        (
            "BBC Radio 5 Sports Extra",
            "Live sport",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_five_live_sports_extra",
        ),
        (
            "BBC Radio 6 Music",
            "Alternative music",
            "https://www.bbc.co.uk/sounds/play/live:bbc_6music",
        ),
        (
            "BBC Asian Network",
            "British Asian music",
            "https://www.bbc.co.uk/sounds/play/live:bbc_asian_network",
        ),
        (
            "BBC Radio Derby",
            "Local radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_derby",
        ),
        (
            "BBC Radio London",
            "Local radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_london",
        ),
        (
            "BBC Radio Manchester",
            "Local radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_manchester",
        ),
        (
            "BBC Radio Scotland",
            "Scottish radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_scotland_fm",
        ),
        (
            "BBC Radio Wales",
            "Welsh radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_wales_fm",
        ),
        (
            "BBC Radio Cymru",
            "Welsh-language radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_cymru",
        ),
        (
            "BBC Radio Ulster",
            "Northern Ireland radio",
            "https://www.bbc.co.uk/sounds/play/live:bbc_radio_ulster",
        ),
        (
            "BBC World Service",
            "World news",
            "https://www.bbc.co.uk/sounds/play/live:bbc_world_service",
        ),
        (
            "Hits Radio",
            "Hits & chart",
            "https://hellorayo.co.uk/hits-radio/play?stationId=352",
        ),
        (
            "Greatest Hits Radio",
            "Classic hits",
            "https://planetradio.co.uk/greatest-hits/player/",
        ),
        (
            "Absolute Radio",
            "Real music",
            "https://planetradio.co.uk/absolute/player/",
        ),
        (
            "Classic FM",
            "Classical",
            "https://media-ssl.musicradio.com/ClassicFM",
        ),
        (
            "Radio X",
            "Alternative & rock",
            "https://media-ssl.musicradio.com/RadioXUK",
        ),
        (
            "Radio X Classic Rock",
            "Classic rock",
            "https://media-ssl.musicradio.com/RadioXClassicRock",
        ),
        (
            "Radio X Chilled",
            "Chilled alternative",
            "https://media-ssl.musicradio.com/RadioXChilled",
        ),
        (
            "LBC",
            "News & talk",
            "https://media-ssl.musicradio.com/LBCUK",
        ),
        (
            "LBC News",
            "Rolling news",
            "https://media-ssl.musicradio.com/LBCNewsUK",
        ),
        (
            "Times Radio",
            "News & current affairs",
            "https://timesradio.wireless.radio/stream",
        ),
        (
            "talkSPORT",
            "Live sport",
            "https://radio.talksport.com/stream",
        ),
        (
            "talkRADIO",
            "News & opinion",
            "https://radio.talkradio.co.uk/stream",
        ),
        (
            "Capital Dance",
            "Dance",
            "https://media-ssl.musicradio.com/CapitalDance",
        ),
        (
            "Capital Anthems",
            "Pop anthems",
            "https://media-ssl.musicradio.com/CapitalAnthems",
        ),
        (
            "Capital Chill",
            "Chilled pop",
            "https://media-ssl.musicradio.com/CapitalChill",
        ),
        (
            "Capital Scotland",
            "Scottish pop",
            "https://media-ssl.musicradio.com/CapitalScotland",
        ),
        (
            "Capital Xtra",
            "Hip-hop & R&B",
            "https://www.globalplayer.com/live/capitalxtra/uk/",
        ),
        (
            "Heart 70s",
            "70s",
            "https://media-ssl.musicradio.com/Heart70s",
        ),
        (
            "Heart 80s",
            "80s",
            "https://media-ssl.musicradio.com/Heart80s",
        ),
        (
            "Heart 90s",
            "90s",
            "https://media-ssl.musicradio.com/Heart90s",
        ),
        (
            "Heart Dance",
            "Dance",
            "https://media-ssl.musicradio.com/HeartDance",
        ),
        (
            "Heart Love",
            "Love songs",
            "https://media-ssl.musicradio.com/HeartLove",
        ),
        (
            "Smooth Chill",
            "Chill",
            "https://media-ssl.musicradio.com/SmoothChill",
        ),
        (
            "Smooth Country",
            "Country",
            "https://media-ssl.musicradio.com/SmoothCountry",
        ),
        (
            "Smooth 70s",
            "70s",
            "https://media-ssl.musicradio.com/Smooth70s",
        ),
        (
            "Smooth 80s",
            "80s",
            "https://media-ssl.musicradio.com/Smooth80s",
        ),
        (
            "Smooth Relax",
            "Relaxing music",
            "https://media-ssl.musicradio.com/SmoothRelax",
        ),
        (
            "Gold Radio",
            "Classic hits",
            "https://media-ssl.musicradio.com/Gold",
        ),
        (
            "Kisstory",
            "Old-school dance & R&B",
            "https://listen-kisstory.sharp-stream.com/kisstory.mp3",
        ),
        (
            "Kiss Fresh",
            "New music",
            "https://listen-kissfresh.sharp-stream.com/kissfresh.mp3",
        ),
        (
            "Kerrang! Radio",
            "Rock",
            "https://listen-kerrang.sharp-stream.com/kerrang.mp3",
        ),
        (
            "Planet Rock",
            "Rock",
            "https://listen-planetrock.sharp-stream.com/planetrock.mp3",
        ),
        (
            "Scala Radio",
            "Classical",
            "https://listen-scalafm.sharp-stream.com/scalaradio.mp3",
        ),
        (
            "Union JACK Radio",
            "British music",
            "https://streaming.radio.co/sd8ab6ff39/listen",
        ),
        (
            "Virgin Radio UK",
            "Music",
            "https://radio.virginradio.co.uk/stream",
        ),
        (
            "Virgin Radio Anthems",
            "Anthems",
            "https://radio.virginradio.co.uk/anthems",
        ),
        (
            "Virgin Radio Chilled",
            "Chilled music",
            "https://radio.virginradio.co.uk/chilled",
        ),
        (
            "Gem 106",
            "East Midlands hits",
            "https://planetradio.co.uk/gem/player/",
        ),
        (
            "Premier Christian Radio",
            "Christian radio",
            "https://www.premier.plus/",
        ),
        (
            "Jazz FM",
            "Jazz & soul",
            "https://planetradio.co.uk/jazz-fm/player/",
        ),
        (
            "MKFM",
            "Milton Keynes local",
            "https://www.mkfm.com/on-air/radioplayer/",
        ),
        (
            "Radio Essex",
            "Essex local",
            "https://www.radioessex.com/player/",
        ),
    ];
    defaults
        .into_iter()
        .map(|(name, genre, stream_url)| {
            (
                name.to_owned(),
                genre.to_owned(),
                stream_url.to_owned(),
                false,
            )
        })
        .chain(settings.custom_radio_stations.iter().map(|station| {
            (
                station.name.clone(),
                station.genre.clone(),
                station.stream_url.clone(),
                true,
            )
        }))
        .collect()
}

fn apply_radio_stations(ui: &AppWindow, settings: &AppSettings) {
    let items = radio_station_registry(settings)
        .into_iter()
        .map(
            |(name, genre, stream_url, user_defined)| SearchStationItem {
                name: SharedString::from(name),
                genre: SharedString::from(genre),
                stream_url: SharedString::from(stream_url),
                user_defined,
            },
        )
        .collect::<Vec<_>>();
    ui.set_radio_stations(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn apply_search_radio_stations(ui: &AppWindow, settings: &AppSettings, query: &str) {
    let query = query.trim().to_lowercase();
    let items = radio_station_registry(settings)
        .into_iter()
        .filter(|(name, genre, _, _)| {
            query.is_empty()
                || name.to_lowercase().contains(&query)
                || genre.to_lowercase().contains(&query)
        })
        .map(
            |(name, genre, stream_url, user_defined)| SearchStationItem {
                name: SharedString::from(name),
                genre: SharedString::from(genre),
                stream_url: SharedString::from(stream_url),
                user_defined,
            },
        )
        .collect::<Vec<_>>();
    ui.set_search_stations(ModelRc::from(Rc::new(VecModel::from(items))));
}

fn adjacent_radio_station(
    settings: &AppSettings,
    current: &str,
    direction: i32,
) -> Option<(String, String)> {
    let stations = radio_station_registry(settings);
    if stations.is_empty() {
        return None;
    }
    let current_index = stations
        .iter()
        .position(|(name, _, _, _)| name == current)
        .unwrap_or(0);
    let next_index = if direction < 0 {
        current_index.checked_sub(1).unwrap_or(stations.len() - 1)
    } else {
        (current_index + 1) % stations.len()
    };
    let (name, _, stream_url, _) = &stations[next_index];
    Some((name.clone(), stream_url.clone()))
}

fn route_web_service_url(route: &str) -> Option<&'static str> {
    match route {
        "amazon-music" => Some("https://music.amazon.co.uk/"),
        "bbc-iplayer" => Some("https://www.bbc.co.uk/iplayer"),
        "my5" => Some("https://www.channel5.com"),
        "netflix" => Some("https://www.netflix.com/browse"),
        "prime-video" => Some("https://www.primevideo.com/"),
        "disney-plus" => Some("https://www.disneyplus.com/en-gb/home"),
        "paramount-plus" => Some("https://www.paramountplus.com/"),
        "now-tv" => Some("https://www.nowtv.com/gb/watch"),
        "xbox" => Some("https://www.xbox.com/play"),
        "geforce-now" => Some("https://play.geforcenow.com"),
        "amazon-luna" => Some("https://luna.amazon.com"),
        "boosteroid" => Some("https://play.boosteroid.com"),
        "shadow-pc" => Some("https://pc.shadow.tech/login"),
        "twitch" => Some("https://www.twitch.tv"),
        "discord" => Some("https://discord.com/app"),
        "youtube" => Some("https://www.youtube.com"),
        "tiktok" => Some("https://www.tiktok.com"),
        "instagram" => Some("https://www.instagram.com"),
        "x" => Some("https://x.com"),
        "facebook" => Some("https://www.facebook.com"),
        "telegram" => Some("https://web.telegram.org"),
        "whatsapp" => Some("https://web.whatsapp.com"),
        "messenger" => Some("https://www.messenger.com"),
        "linkedin" => Some("https://www.linkedin.com"),
        "threads" => Some("https://www.threads.net"),
        "bluesky" => Some("https://bsky.app"),
        "mastodon" => Some("https://mastodon.social"),
        "pinterest" => Some("https://www.pinterest.com"),
        "tumblr" => Some("https://www.tumblr.com"),
        "reddit" => Some("https://www.reddit.com"),
        _ => None,
    }
}

fn apply_route(ui: &AppWindow, id: &str) {
    let route = route_by_id(id).unwrap_or(PRIMARY_ROUTES[0]);
    let language_display = ui.get_settings_language();
    let language = effective_language_tag(language_tag(language_display.as_str()));
    ui.set_current_route(SharedString::from(route.id));
    ui.set_page_title(SharedString::from(core_translation(&language, route.title)));
}

#[cfg(windows)]
const DISCORD_APPLICATION_ID: &str = "1487472795767279857";

#[cfg(windows)]
fn publish_discord_page_presence(route: &str, title: &str) -> anyhow::Result<()> {
    let title = title.trim();
    let state = if title.is_empty() { route } else { title };
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let activity = serde_json::json!({
        "type": 3,
        "details": "Using Zink",
        "state": state,
        "timestamps": { "start": started },
        "assets": {
            "large_image": "zink_1024",
            "large_text": "Zink"
        }
    });
    send_discord_activity(Some(&activity))
}

#[cfg(not(windows))]
fn publish_discord_page_presence(_route: &str, _title: &str) -> anyhow::Result<()> {
    anyhow::bail!("Discord Rich Presence is available on Windows")
}

#[cfg(windows)]
fn clear_discord_presence() -> anyhow::Result<()> {
    send_discord_activity(None)
}

#[cfg(not(windows))]
fn clear_discord_presence() -> anyhow::Result<()> {
    anyhow::bail!("Discord Rich Presence is available on Windows")
}

#[cfg(windows)]
fn send_discord_activity(activity: Option<&serde_json::Value>) -> anyhow::Result<()> {
    let mut pipe = (0..10)
        .find_map(|index| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(format!(r"\\.\pipe\discord-ipc-{index}"))
                .ok()
        })
        .context("Discord IPC is not available")?;
    let handshake = serde_json::json!({ "v": 1, "client_id": DISCORD_APPLICATION_ID });
    write_discord_frame(&mut pipe, 0, &handshake)?;
    let command = serde_json::json!({
        "cmd": "SET_ACTIVITY",
        "args": {
            "pid": std::process::id(),
            "activity": activity
        },
        "nonce": format!(
            "zink-{}-{}",
            std::process::id(),
            SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos()
        )
    });
    write_discord_frame(&mut pipe, 1, &command)
}

#[cfg(windows)]
fn write_discord_frame(
    stream: &mut impl Write,
    opcode: u32,
    value: &serde_json::Value,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(value)?;
    let length = u32::try_from(payload.len()).context("Discord payload is too large")?;
    stream.write_all(&opcode.to_le_bytes())?;
    stream.write_all(&length.to_le_bytes())?;
    stream.write_all(&payload)?;
    stream.flush()?;
    Ok(())
}
