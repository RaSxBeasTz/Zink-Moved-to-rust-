# Zink 5.0 migration inventory

Status date: 2026-11-05. The sibling C#/WinUI project in the repository root is
the source authority. Status values are **inventory**, **foundation**, **in
progress**, **verified**, or **blocked**. A visible Slint route is not evidence
that its feature is complete.

## Baseline and acceptance gates

The legacy app currently targets `net10.0-windows10.0.26100.0` despite the
request referring to .NET 8. It contains about 330 C# and 99 XAML files. The
Rust app is isolated in `zink-rust/`; no legacy file is deleted or rewritten.

Each migrated component must pass behavioral parity, data compatibility,
accessibility/keyboard operation, Windows 10 and 11 checks, a clean release
build, and a no-.NET deployment smoke test. Network features also require live
authentication and failure-path tests.

## UI surface inventory

| Legacy group | Discovered surfaces | Rust replacement | Status |
|---|---|---|---|
| Shell/lifecycle | App, Splash, MainWindow, Home, activation, fullscreen, tabs | Slint shell + Windows lifecycle adapter | in progress (shell launch verified) |
| Dashboard | HomeDashboard, FeatureTour, SearchResults, Notifications | Slint dashboard/search/notification models | in progress (legacy Home and Search structures copied; indexed Search is functional) |
| Music | MediaPlayer, MusicLibrary, Equalizer, Visualizer, YouTubeMusic, AmazonMusic | Slint + Windows Media Playback/Media Foundation/WASAPI graph | in progress (legacy player/library/equalizer/visualizer layouts, indexed browsing/search, embedded tags, activation, transport and SMTC metadata implemented) |
| Spotify | Login, Home, Beta, page/window widget | OAuth, Spotify API/Connect adapter, Slint widget | inventory |
| Radio | Radio, Discovery, Station, Likes, Widget/mini-widget | HTTP/ICY, SQLite stations/history/queue, Slint | in progress (legacy main Radio page structure and six direct native streams wired; ICY/likes/history/widgets remain) |
| Video | Player, Library, duplicate search, FilmInfo, OneDrive | Media Foundation, indexed library, Graph adapter | in progress (legacy Video Library layout, indexed cards, search, folder import and playback activation implemented) |
| Recording | Recorder, ScreenRecorder, background settings, library, area picker | WGC + D3D11 + WASAPI + Media Foundation H.264/AAC + Rust MP4 mux | in progress (manual primary-display recording and clean MP4 finalization are native; settings and library are wired; native rolling replay buffer and in-app target picker remain) |
| TV/film | BBC iPlayer, My5, Netflix, Prime, Disney+, Paramount+, Now TV | WebView2 HWND controlled from Rust | inventory |
| Gaming | Xbox, FPS Recorder, GeForce Now, Luna, Boosteroid, Shadow PC | Gaming APIs/ETW + WebView2 adapters | inventory |
| Zink Social | Login/register/locked, friends/profile/requests/messages/call/dev settings | Tokio HTTP/WebSocket, CPAL/WASAPI, pure-Rust Opus, Windows Graphics Capture, Media Foundation H.264, Slint | in progress (auth/friends/requests/messages, persisted profile details/avatar, password-gated developer settings and the exact call lifecycle are wired; native voice plus outbound and remote H.264 screen presentation are implemented; cross-device runtime validation remains) |
| Social web | Twitch, Discord, YouTube, TikTok, Instagram, X, Facebook, Telegram, WhatsApp, Messenger, LinkedIn, Threads, Bluesky, Mastodon, Pinterest, Tumblr, Reddit | shared Rust WebView2 host with isolated profiles | inventory |
| Live streaming | Twitch, YouTube, Kick, Instagram, TikTok, Facebook, X | WGC/WASAPI, Media Foundation H.264/AAC, Rust RTMP/RTMPS publisher, provider destinations | in progress (native capture/encode/audio/publish, encrypted RTMPS, provider-specific saved credentials, quality and low-latency settings are wired; interactive capture-source selection, live preview and measured telemetry remain) |
| Utilities | Connect browser/ad block, VPN, Tools, uTorrent mover | Rust browser host, Windows VPN, native utilities | inventory |
| Settings | customization/theme, localization, About, Privacy, Review, History, Feedback | Slint tokens, versioned Serde settings, native Windows links/toasts/startup integration | in progress (legacy structures and primary controls are copied and persisted, including reopen-last-page; full live localization and background-service activation remain) |
| Windows | tray, hotkeys, overlays, notifications, file/protocol activation | `zink-platform-windows` wrappers | inventory |

`WidgetHubPage`, `BackgroundRecordingSettingsPage`, `RecorderPage`,
`FpsRecorderPage`, `RadioDiscoveryPage`, `RadioStationPage`,
`FriendProfilePage`, `SocialLockedPage`, `OneDriveStreamingPage`, and overlay
window classes were not all in the requested list but remain in scope.

## Dependency replacement map

| Existing dependency | Role | Rust/native replacement | Compatibility note |
|---|---|---|---|
| WinUI 3 / Windows App SDK / XAML | UI/lifecycle/pickers/notifications | Slint 1.18.1 + `windows` 0.62.2 | Slint tests Win10 x64 and Win11 x64/ARM64. Win10 ARM64 needs separate qualification. |
| LibVLCSharp / LibVLC | playback/codec breadth | Media Foundation first; optional documented native codec module only for proven gaps | No managed wrapper; corpus parity test required. |
| NAudio | devices/capture | WASAPI and Core Audio COM | Preserve loopback, mic, endpoint and process exclusion. |
| SharpDX / Vortice | D3D/DXGI/MF | typed D3D11/12, DXGI and MF `windows` bindings | Unsafe code stays behind platform/media crates. |
| SIPSorcery / Concentus | RTC and Opus | `tokio-tungstenite` for the authenticated Zink signalling socket, `cpal` using Windows WASAPI for capture/playback, and `opus-rs` for dependency-free RFC 6716 packets | The implemented call lifecycle and audio packet DTOs preserve the existing server/client casing and message names. Camera negotiation remains to be migrated. |
| WebView2 | web experiences/OAuth | WebView2 COM from Rust | Native runtime remains; no .NET dependency. |
| MSAL | Microsoft/OneDrive auth | OAuth 2.0 PKCE/system broker + Graph | Store tokens with Credential Manager/DPAPI. |
| TagLibSharp / ZinkLyrics / YAML | tags/lyrics | Rust tag parser + Serde YAML | Golden-file metadata tests required. |
| BouncyCastle | cryptography | CNG/DPAPI and audited RustCrypto per use site | No blanket replacement without use-site audit. |
| fdk-aac / libAACenc | AAC encode | Media Foundation AAC MFT | Retain native codec only if parity tests require it. |
| ZinkRecorderMux.dll | mux/replay tracks | replaced by the vendored Rust MP4 mux in `win-native-media` | No managed or legacy DLL dependency is required for new manual recordings. |
| ZinkIntelVplEncoder.dll | Intel H.264/HEVC | Rust oneVPL FFI or hardware MFT | Existing C++ source may be an interim native module. |
| ZinkNvencEncoder.dll | NVIDIA HEVC | Rust NVENC FFI/native module | Existing source/header can be ported. |
| LocalSettings + JSON | settings/libraries/queues/social | versioned import into Serde/SQLite | Copy-first, idempotent, never delete legacy data. |
| MSIX identity | upgrades/associations/capabilities | full-trust Slint MSIX with same identity | Exact Publisher/Identity/App Id and Store flight tests required. |

No FFmpeg dependency is permitted. Media work uses Windows/GPU-native APIs and
Zink-owned native code.

## Data migration

Discovered stores include WinRT `LocalSettings`, `video_library.json`, liked
radio songs, listening history/queue, social messages/profile details,
sessions/tokens, recording preferences, streaming credentials, browser/filter
caches, diagnostics, replay segments, and file-mover history. Web experiences
also use separate WebView2 user-data folders under LocalAppData.

The importer will dynamically locate the installed package family, copy known
files into a timestamped import area, hash them, import inside a SQLite
transaction, and write a receipt. Secrets will be re-protected with Credential
Manager or DPAPI rather than copied into plain JSON. Legacy data remains
untouched and imports remain safe to rerun.

## Native Windows API plan

- Playback: Media Foundation, hardware MFTs and System Media Transport Controls.
- Audio: WASAPI, MMDevice and AudioClient3 with Zink-owned DSP.
- Capture: Windows Graphics Capture, D3D11 surfaces, WASAPI and MF encoders.
- GPU: D3D11/12 and DXGI with runtime feature detection and Windows 10 fallbacks.
- Shell: HWND/DWM backdrops with acrylic-like fallback, tray, hotkeys,
  activation and native notifications.
- Security: system OAuth where appropriate, Credential Manager/DPAPI, audited TLS.

## Design authority

The legacy XAML remains the visual and interaction authority for every page,
dialog, overlay and persistent control. The Rust shell now embeds the
existing `ZinkGeneratedThemeBackground.png` and reproduces the 320px glass
sidebar, cyan/green liquid-glass tokens, selected navigation accent, Zink title
and tagline, Continue Your Flow signal cards, Try This Next panel, Zink Pulse
tiles and persistent transport surface. The 1500x900 dashboard was inspected in
the running release build on 2026-11-05 after correcting right-edge clipping and
the sidebar-toggle glyph. Remaining pages must reuse these tokens and copy their
legacy XAML structure rather than introducing generic replacement layouts. The
Music Library is the first non-dashboard page converted under this rule: its
header action, search surface and album-card hierarchy now follow the original
page and display indexed title, artist, album and duration metadata. Artwork,
folder persistence/watching, scan-log UI, enrichment/renaming and Play All queue
behavior are still explicitly open and are not represented as complete.
Video Library now has its distinct legacy action row, search/sort/clear strip,
220x330 film-card geometry and dual film actions instead of reusing the music
layout. Multiple-folder persistence, sorting, artwork, film information and the
empty-library/loading overlays remain open until their behavior is ported and
verified.
Video Player Browse, Play, Pause, Rewind and Forward controls are wired to the
Windows native player, including relative ten-second seeking. Direct Internet
Radio playback is wired for Capital FM, Heart, Classic FM, LBC, talkSPORT and
Virgin Radio UK using the legacy station URLs. Web-only stations, ICY metadata,
likes, background playback and the mini-player remain open.
Music Player now follows the original three-panel Queue/Lyrics, Now Playing and
Browse composition and reuses the working native transport callbacks. Equalizer
now reproduces the header actions, master-gain/preset panel and ten legacy band
controls. Visualizer now reproduces the header, configuration strip and canvas
composition. EQ DSP, live WASAPI spectrum data, presets, queue management,
lyrics and sharing remain open behavior work and are not marked verified.

## Milestones

1. **Foundation:** workspace, Slint shell/navigation, settings, SQLite, logging
   and Windows initialization. Compiles, passes Clippy/tests, release launches.
2. **Local playback (current):** native file/folder pickers, Windows audio
   playback with play/pause/volume, command-line activation, recursive discovery,
   repeatable SQLite indexing, and Slint music/video library browsing are
   implemented. Position/duration polling, seeking, stopping, volume, embedded
   audio tag extraction, library search and SMTC title/type metadata are also
   implemented. Video rendering, artwork extraction/enrichment, packaged file
   activation, library watching/queue parity and audio-device selection remain.
3. **Radio:** stations, ICY metadata, likes, history, queues and widget.
4. **Recording:** manual WGC/WASAPI capture, then crash-safe 45-second replay and
   the hardware encoder matrix.
5. **Online/browser:** WebView2 host, Spotify, OneDrive, TV/social/cloud gaming
   and live streaming.
6. **Zink Social:** exact auth/signalling, presence/messages, voice and screen
   sharing quality policies.
7. **Parity/packaging:** remaining UI, localization/accessibility, migration UI,
   identity-preserving MSIX, release performance and no-.NET deployment tests.

## Current blockers and verification

- Rust 1.92 is installed. `cargo test --workspace`, strict workspace Clippy and
  the x64 release build pass; the release executable passed a five-second launch
  smoke test on 2026-11-05.
- Extensive pre-existing modified/untracked C# work is preserved and excluded.
- The legacy recorder mux source is absent beyond its header; the Rust migration replaces it with the vendored native MP4 mux, while rolling replay parity still requires its own bounded buffer implementation.
- Store upgrade, OAuth, DRM web services, codecs, GPU vendors, social server and
  physical capture devices need live tests; source inspection cannot verify them.

