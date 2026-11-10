use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}

fn default_recorder_quality() -> String {
    "1080p 60 FPS".to_owned()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum ThemePreference {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CustomRadioStation {
    pub name: String,
    pub genre: String,
    pub stream_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[allow(clippy::struct_excessive_bools)]
pub struct AppSettings {
    pub schema_version: u32,
    pub theme: ThemePreference,
    pub accent_color: String,
    pub language: String,
    pub reopen_last_page: bool,
    pub last_route: String,
    pub home_space: String,
    pub home_show_flow: bool,
    pub home_show_live_activities: bool,
    pub home_show_insights: bool,
    pub home_show_devices: bool,
    pub home_pinned_routes: Vec<String>,
    pub home_route_history: Vec<String>,
    pub equalizer_enabled: bool,
    pub equalizer_master_gain: f32,
    pub equalizer_preset: String,
    pub equalizer_bands: Vec<f32>,
    pub visualizer_style: String,
    pub visualizer_color: String,
    pub visualizer_sensitivity: i32,
    pub visualizer_smoothing: i32,
    pub visualizer_paused: bool,
    pub liked_radio_stations: Vec<String>,
    #[serde(default)]
    pub media_library_folders: Vec<String>,
    #[serde(default = "default_recorder_quality")]
    pub recorder_quality: String,
    #[serde(default = "default_true")]
    pub recorder_capture_cursor: bool,
    pub recorder_output_device_id: String,
    pub recorder_microphone_device_id: String,
    pub social_call_output_device: String,
    pub social_call_input_device: String,
    pub custom_radio_stations: Vec<CustomRadioStation>,
    pub discord_rich_presence: bool,
    pub run_at_startup: bool,
    pub background_notifications: bool,
    pub low_resource_background: bool,
    pub background_replay: bool,
    pub clip_recording_hotkey: bool,
    pub diagnostic_log: bool,
    pub compact_navigation: bool,
    pub reduce_motion: bool,
    pub lighter_sdr_theme: bool,
    pub notifications_playback: bool,
    pub notifications_social: bool,
    pub notifications_updates: bool,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            schema_version: 1,
            theme: ThemePreference::System,
            accent_color: "#38ff66".into(),
            language: "system".into(),
            reopen_last_page: true,
            last_route: "home".into(),
            home_space: "Everything".into(),
            home_show_flow: true,
            home_show_live_activities: true,
            home_show_insights: true,
            home_show_devices: true,
            home_pinned_routes: vec![
                "music-player".into(),
                "zink-connect".into(),
                "screen-recorder".into(),
                "social".into(),
            ],
            home_route_history: Vec::new(),
            equalizer_enabled: true,
            equalizer_master_gain: 0.0,
            equalizer_preset: "Flat".into(),
            equalizer_bands: vec![0.0; 10],
            visualizer_style: "Bars".into(),
            visualizer_color: "Sky".into(),
            visualizer_sensitivity: 100,
            visualizer_smoothing: 82,
            visualizer_paused: false,
            liked_radio_stations: Vec::new(),
            media_library_folders: Vec::new(),
            recorder_quality: default_recorder_quality(),
            recorder_capture_cursor: true,
            recorder_output_device_id: String::new(),
            recorder_microphone_device_id: String::new(),
            social_call_output_device: String::new(),
            social_call_input_device: String::new(),
            custom_radio_stations: Vec::new(),
            discord_rich_presence: false,
            run_at_startup: false,
            background_notifications: false,
            low_resource_background: true,
            background_replay: false,
            clip_recording_hotkey: true,
            diagnostic_log: true,
            compact_navigation: false,
            reduce_motion: false,
            lighter_sdr_theme: false,
            notifications_playback: true,
            notifications_social: true,
            notifications_updates: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub id: &'static str,
    pub title: &'static str,
    pub group: &'static str,
}

pub const PRIMARY_ROUTES: &[Route] = &[
    Route {
        id: "home",
        title: "Home",
        group: "Zink",
    },
    Route {
        id: "search",
        title: "Search",
        group: "Zink",
    },
    Route {
        id: "zink-connect",
        title: "Zink Connect",
        group: "Zink",
    },
    Route {
        id: "zink-vpn",
        title: "Zink VPN",
        group: "Zink",
    },
    Route {
        id: "tools",
        title: "Tools",
        group: "Zink",
    },
    Route {
        id: "music-player",
        title: "Music Player",
        group: "Music",
    },
    Route {
        id: "music-library",
        title: "Music Library",
        group: "Music",
    },
    Route {
        id: "equalizer",
        title: "Equalizer",
        group: "Music",
    },
    Route {
        id: "visualizer",
        title: "Visualizer",
        group: "Music",
    },
    Route {
        id: "radio",
        title: "Internet Radio",
        group: "Music",
    },
    Route {
        id: "youtube-music",
        title: "YouTube Music",
        group: "Music",
    },
    Route {
        id: "amazon-music",
        title: "Amazon Music",
        group: "Music",
    },
    Route {
        id: "spotify",
        title: "Spotify",
        group: "Music",
    },
    Route {
        id: "spotify-widget",
        title: "Spotify Widget",
        group: "Music",
    },
    Route {
        id: "radio-discovery",
        title: "Radio Discovery",
        group: "Music",
    },
    Route {
        id: "liked-radio-songs",
        title: "Liked radio songs",
        group: "Music",
    },
    Route {
        id: "radio-widget",
        title: "Radio Widget",
        group: "Music",
    },
    Route {
        id: "video-player",
        title: "Video Player",
        group: "Video",
    },
    Route {
        id: "video-library",
        title: "Video Library",
        group: "Video",
    },
    Route {
        id: "tv",
        title: "TV",
        group: "Watch",
    },
    Route {
        id: "films",
        title: "Films",
        group: "Watch",
    },
    Route {
        id: "gaming",
        title: "Gaming",
        group: "Play",
    },
    Route {
        id: "duplicate-video-search",
        title: "Duplicate Video Searcher",
        group: "Video",
    },
    Route {
        id: "film-info",
        title: "Film Information",
        group: "Video",
    },
    Route {
        id: "onedrive-streaming",
        title: "OneDrive Streaming",
        group: "Video",
    },
    Route {
        id: "bbc-iplayer",
        title: "BBC iPlayer",
        group: "TV",
    },
    Route {
        id: "my5",
        title: "My5",
        group: "TV",
    },
    Route {
        id: "netflix",
        title: "Netflix",
        group: "Films",
    },
    Route {
        id: "prime-video",
        title: "Prime Video",
        group: "Films",
    },
    Route {
        id: "disney-plus",
        title: "Disney+",
        group: "Films",
    },
    Route {
        id: "paramount-plus",
        title: "Paramount+",
        group: "Films",
    },
    Route {
        id: "now-tv",
        title: "NOW TV",
        group: "Films",
    },
    Route {
        id: "xbox",
        title: "Xbox",
        group: "Gaming",
    },
    Route {
        id: "fps-recorder",
        title: "FPS Recorder",
        group: "Gaming",
    },
    Route {
        id: "geforce-now",
        title: "GeForce NOW",
        group: "Gaming",
    },
    Route {
        id: "amazon-luna",
        title: "Amazon Luna",
        group: "Gaming",
    },
    Route {
        id: "boosteroid",
        title: "Boosteroid",
        group: "Gaming",
    },
    Route {
        id: "shadow-pc",
        title: "Shadow PC",
        group: "Gaming",
    },
    Route {
        id: "screen-recorder",
        title: "Screen Recorder",
        group: "Create",
    },
    Route {
        id: "recordings",
        title: "Recordings Library",
        group: "Create",
    },
    Route {
        id: "social",
        title: "Zink Social",
        group: "Connect",
    },
    Route {
        id: "social-web",
        title: "Social",
        group: "Connect",
    },
    Route {
        id: "social-login",
        title: "Social Login",
        group: "Zink Social",
    },
    Route {
        id: "social-register",
        title: "Social Register",
        group: "Zink Social",
    },
    Route {
        id: "friends",
        title: "Friends",
        group: "Zink Social",
    },
    Route {
        id: "friend-requests",
        title: "Friend Requests",
        group: "Zink Social",
    },
    Route {
        id: "messages",
        title: "Messages",
        group: "Zink Social",
    },
    Route {
        id: "call",
        title: "Call",
        group: "Zink Social",
    },
    Route {
        id: "profile",
        title: "Profile",
        group: "Zink Social",
    },
    Route {
        id: "social-developer",
        title: "Social Developer Settings",
        group: "Zink Social",
    },
    Route {
        id: "twitch",
        title: "Twitch",
        group: "Social",
    },
    Route {
        id: "discord",
        title: "Discord",
        group: "Social",
    },
    Route {
        id: "youtube",
        title: "YouTube",
        group: "Social",
    },
    Route {
        id: "tiktok",
        title: "TikTok",
        group: "Social",
    },
    Route {
        id: "instagram",
        title: "Instagram",
        group: "Social",
    },
    Route {
        id: "x",
        title: "X",
        group: "Social",
    },
    Route {
        id: "facebook",
        title: "Facebook",
        group: "Social",
    },
    Route {
        id: "telegram",
        title: "Telegram",
        group: "Social",
    },
    Route {
        id: "whatsapp",
        title: "WhatsApp",
        group: "Social",
    },
    Route {
        id: "messenger",
        title: "Messenger",
        group: "Social",
    },
    Route {
        id: "linkedin",
        title: "LinkedIn",
        group: "Social",
    },
    Route {
        id: "threads",
        title: "Threads",
        group: "Social",
    },
    Route {
        id: "bluesky",
        title: "Bluesky",
        group: "Social",
    },
    Route {
        id: "mastodon",
        title: "Mastodon",
        group: "Social",
    },
    Route {
        id: "pinterest",
        title: "Pinterest",
        group: "Social",
    },
    Route {
        id: "tumblr",
        title: "Tumblr",
        group: "Social",
    },
    Route {
        id: "reddit",
        title: "Reddit",
        group: "Social",
    },
    Route {
        id: "streaming",
        title: "Streaming",
        group: "Connect",
    },
    Route {
        id: "twitch-streaming",
        title: "Twitch Streaming",
        group: "Streaming",
    },
    Route {
        id: "youtube-streaming",
        title: "YouTube Streaming",
        group: "Streaming",
    },
    Route {
        id: "kick-streaming",
        title: "Kick Streaming",
        group: "Streaming",
    },
    Route {
        id: "instagram-streaming",
        title: "Instagram Streaming",
        group: "Streaming",
    },
    Route {
        id: "tiktok-streaming",
        title: "TikTok Streaming",
        group: "Streaming",
    },
    Route {
        id: "facebook-streaming",
        title: "Facebook Streaming",
        group: "Streaming",
    },
    Route {
        id: "x-streaming",
        title: "X Streaming",
        group: "Streaming",
    },
    Route {
        id: "background-recording-settings",
        title: "Background Recording Settings",
        group: "Recording",
    },
    Route {
        id: "recorder",
        title: "Recorder",
        group: "Recording",
    },
    Route {
        id: "feedback",
        title: "Feedback",
        group: "Footer",
    },
    Route {
        id: "version-history",
        title: "Version History",
        group: "Footer",
    },
    Route {
        id: "app-theme",
        title: "App Theme",
        group: "Footer",
    },
    Route {
        id: "notifications",
        title: "Notifications",
        group: "Zink",
    },
    Route {
        id: "privacy",
        title: "Privacy Policy",
        group: "Zink",
    },
    Route {
        id: "feature-tour",
        title: "Try Zink",
        group: "Footer",
    },
    Route {
        id: "widget-hub",
        title: "Widget Hub",
        group: "Footer",
    },
    Route {
        id: "review",
        title: "Leave a review",
        group: "Footer",
    },
    Route {
        id: "customization",
        title: "App customisation",
        group: "Footer",
    },
    Route {
        id: "settings",
        title: "Settings",
        group: "Footer",
    },
    Route {
        id: "about",
        title: "About",
        group: "Footer",
    },
];

#[must_use]
pub fn route_by_id(id: &str) -> Option<Route> {
    PRIMARY_ROUTES.iter().copied().find(|route| route.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn route_ids_are_unique() {
        for (index, route) in PRIMARY_ROUTES.iter().enumerate() {
            assert!(
                !PRIMARY_ROUTES[..index]
                    .iter()
                    .any(|item| item.id == route.id)
            );
        }
    }

    #[test]
    fn defaults_deserialize_from_empty_object() {
        let settings: AppSettings = serde_json::from_str("{}").expect("default settings");
        assert_eq!(settings.last_route, "home");
    }
}
