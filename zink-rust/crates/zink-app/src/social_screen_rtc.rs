use std::{
    collections::VecDeque,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use rtc::{
    interceptor::{
        Attribute, BandwidthEstimator, EstimatorStats, Gcc, Interceptor, NackGeneratorBuilder,
        PacerBuilder, PacerInterceptor, Packet, PacketReport, Registry, Slot, StreamInfo,
        TaggedPacket,
    },
    media::io::sample_builder::SampleBuilder,
    media_stream::MediaStreamTrack,
    peer_connection::{
        configuration::{
            RTCConfigurationBuilder,
            interceptor_registry::{
                CongestionFeedback, configure_congestion_control, register_default_interceptors,
            },
            media_engine::{MIME_TYPE_AV1, MIME_TYPE_H264, MediaEngine},
            setting_engine::SettingEngineBuilder,
        },
        sdp::RTCSessionDescription,
        transport::RTCIceServer,
    },
    rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication,
    rtp::{
        codec::h264::H264Packet,
        packetizer::{Packetizer, new_packetizer},
        sequence::new_random_sequencer,
    },
    rtp_transceiver::rtp_sender::{
        RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
        RtpCodecKind,
    },
};
use sansio::Protocol;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use webrtc::{
    media_stream::{
        track_local::static_rtp::TrackLocalStaticRTP,
        track_remote::{TrackRemote, TrackRemoteEvent},
    },
    peer_connection::{
        PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
        RTCPeerConnectionState,
    },
};

#[path = "social_av1_rtp.rs"]
mod av1_rtp;

pub const CAPABILITY: &str = "zink-webrtc-screen-v1";
pub const AV1_CAPABILITY: &str = "zink-webrtc-av1-v1";
const MIN_RATE: u32 = 350_000;
const AV1_720P_RATE: u32 = 6_000_000;
const AV1_1080P30_RATE: u32 = 10_000_000;
const AV1_1440P30_RATE: u32 = 12_000_000;
const AV1_1440P60_RATE: u32 = 18_000_000;
const AV1_PEAK_PACING_RATE: f64 = 22_000_000.0;
const AV1_1440P_PEAK_PACING_RATE: f64 = 28_000_000.0;
const RECEIVE_ONLY_RATE: u32 = 6_000_000;
const NEGOTIATION_TIMEOUT: Duration = Duration::from_secs(10);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(350);

pub enum Event {
    Description(String),
    Frame {
        data: Vec<u8>,
        timestamp_ms: i64,
        received: Instant,
        key_frame: bool,
    },
    KeyframeRequested,
    Closed,
}

pub struct Frame {
    pub data: Vec<u8>,
    pub timestamp_ms: i64,
    pub _slot: Arc<tokio::sync::OwnedSemaphorePermit>,
}

pub struct Connection {
    pub id: String,
    pub av1: bool,
    pub ready: Arc<AtomicBool>,
    pub bitrate: Arc<AtomicU32>,
    pub frames: mpsc::Sender<Frame>,
    pub answer: Option<oneshot::Sender<String>>,
    stop: Option<oneshot::Sender<()>>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
struct IceConfig {
    ice_servers: Vec<IceServer>,
    stun_urls: Vec<String>,
    turn_urls: Vec<String>,
    turn_username: String,
    turn_password: String,
}

#[derive(Deserialize)]
struct IceServer {
    urls: Vec<String>,
    #[serde(default)]
    username: String,
    #[serde(default)]
    credential: String,
}

async fn ice_servers(token: String) -> Vec<RTCIceServer> {
    let response = tokio::task::spawn_blocking(move || {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(2)))
            .build()
            .into();
        let mut response = agent
            .get(format!("{}/api/rtc-config", crate::SOCIAL_BASE_URL))
            .header("Authorization", format!("Bearer {token}"))
            .call()
            .ok()?;
        let body = response.body_mut().read_to_string().ok()?;
        serde_json::from_str::<IceConfig>(&body).ok()
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default();
    let mut servers: Vec<_> = response
        .ice_servers
        .into_iter()
        .map(|server| RTCIceServer {
            urls: server.urls,
            username: server.username,
            credential: server.credential,
            ..Default::default()
        })
        .collect();
    if !response.stun_urls.is_empty() {
        servers.push(RTCIceServer {
            urls: response.stun_urls,
            ..Default::default()
        });
    }
    if !response.turn_urls.is_empty() {
        servers.push(RTCIceServer {
            urls: response.turn_urls,
            username: response.turn_username,
            credential: response.turn_password,
            ..Default::default()
        });
    }
    if servers.is_empty() {
        servers.push(RTCIceServer {
            urls: vec!["stun:stun.cloudflare.com:3478".into()],
            ..Default::default()
        });
    }
    servers
}

pub fn start(
    id: String,
    token: String,
    offer: Option<String>,
    av1: bool,
    profile_bitrate: u32,
    events: mpsc::Sender<(String, Event)>,
) -> Connection {
    start_with_ice(id, offer, av1, profile_bitrate, events, ice_servers(token))
}

fn start_with_ice(
    id: String,
    offer: Option<String>,
    av1: bool,
    profile_bitrate: u32,
    events: mpsc::Sender<(String, Event)>,
    servers: impl std::future::Future<Output = Vec<RTCIceServer>> + Send + 'static,
) -> Connection {
    let ready = Arc::new(AtomicBool::new(false));
    // The encoder uses 85% of the path budget. Leave room for RTP,
    // retransmissions and feedback without collapsing every preset to 3 Mbps.
    let profile_bitrate = if profile_bitrate == 0 {
        RECEIVE_ONLY_RATE
    } else {
        profile_bitrate
    };
    let path_budget = profile_bitrate * 100 / 85;
    let bitrate = Arc::new(AtomicU32::new(path_budget));
    let (frames, rx) = mpsc::channel(3);
    let (answer, answer_rx) = oneshot::channel();
    let (stop, mut stop_rx) = oneshot::channel();
    let handler = Arc::new(Handler {
        id: id.clone(),
        av1,
        events,
        ready: Arc::clone(&ready),
        gathering: tokio::sync::Notify::new(),
        changed: tokio::sync::Notify::new(),
        failed: AtomicBool::new(false),
    });
    let task_bitrate = Arc::clone(&bitrate);
    let initiator = offer.is_none();
    tokio::spawn(async move {
        let servers = tokio::select! { _ = &mut stop_rx => return, servers = servers => servers };
        let result = run(
            servers,
            offer,
            answer_rx,
            rx,
            stop_rx,
            Arc::clone(&handler),
            task_bitrate,
            path_budget,
        )
        .await;
        handler.ready.store(false, Ordering::Release);
        if let Err(error) = result {
            tracing::debug!(%error, "screen WebRTC ended");
        }
        handler.emit(Event::Closed);
    });
    Connection {
        id,
        av1,
        ready,
        bitrate,
        frames,
        answer: initiator.then_some(answer),
        stop: Some(stop),
    }
}

struct Handler {
    id: String,
    av1: bool,
    events: mpsc::Sender<(String, Event)>,
    ready: Arc<AtomicBool>,
    gathering: tokio::sync::Notify,
    changed: tokio::sync::Notify,
    failed: AtomicBool,
}

impl Handler {
    fn emit(&self, event: Event) {
        let _ = self.events.try_send((self.id.clone(), event));
    }
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            self.gathering.notify_one();
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        self.ready.store(
            state == RTCPeerConnectionState::Connected,
            Ordering::Release,
        );
        if matches!(
            state,
            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed
        ) {
            self.failed.store(true, Ordering::Release);
        }
        self.changed.notify_one();
    }

    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        let (events, id, av1) = (self.events.clone(), self.id.clone(), self.av1);
        tokio::spawn(async move {
            read_track(track, events, id, av1).await;
        });
    }
}

async fn read_track(
    track: Arc<dyn TrackRemote>,
    events: mpsc::Sender<(String, Event)>,
    id: String,
    av1: bool,
) {
    // The packet window must fit a complete high-detail keyframe, even on a
    // lossless link. Bound late media by RTP time as well as packet count.
    let mut samples = (!av1).then(|| {
        SampleBuilder::new(1024, H264Packet::default(), 90_000)
            .with_max_time_delay(Duration::from_millis(110))
    });
    let mut av1_frames = av1_rtp::Av1RtpFrames::default();
    let mut frame_ends = VecDeque::with_capacity(16);
    let mut last_timestamp = None;
    let mut clock_ticks = 0_i64;
    let mut needs_keyframe = true;
    let mut next_refresh = Instant::now() + KEYFRAME_REQUEST_INTERVAL;
    while let Some(event) = track.poll().await {
        let TrackRemoteEvent::OnRtpPacket(packet) = event else {
            continue;
        };
        let now = Instant::now();
        let media_ssrc = packet.header.ssrc;
        let mut emit = |frame: Option<(u32, Vec<u8>)>| {
            let Some((timestamp, data)) = frame else {
                needs_keyframe = true;
                return;
            };
            if let Some(previous) = last_timestamp.replace(timestamp) {
                clock_ticks += i64::from(timestamp.wrapping_sub(previous));
            }
            let key_frame = if av1 {
                av1_frame(&data).map(|header| header.is_some())
            } else {
                Some(
                    data.windows(4)
                        .any(|nal| nal[..3] == [0, 0, 1] && nal[3] & 0x1f == 5),
                )
            };
            let Some(key_frame) = key_frame else {
                needs_keyframe = true;
                return;
            };
            if needs_keyframe && !key_frame {
                return;
            }
            let frame = Event::Frame {
                data,
                timestamp_ms: clock_ticks / 90,
                received: now,
                key_frame,
            };
            needs_keyframe = events.try_send((id.clone(), frame)).is_err();
        };
        if let Some(samples) = &mut samples {
            if packet.header.marker {
                if frame_ends.len() == 16 {
                    frame_ends.pop_front();
                }
                frame_ends.push_back(packet.header.timestamp);
            }
            samples.push(now, packet);
            while let Some((sample, timestamp)) = samples.pop_with_timestamp(now) {
                // The generic builder can end an incomplete sample on a timestamp change.
                let Some(end) = frame_ends.iter().position(|&end| end == timestamp) else {
                    emit(None);
                    continue;
                };
                frame_ends.remove(end);
                if sample.prev_dropped_packets != 0 {
                    emit(None);
                }
                emit(Some((timestamp, sample.data.to_vec())));
            }
        } else {
            av1_frames.push(now, packet, emit);
        }
        if needs_keyframe && now >= next_refresh {
            next_refresh = now + KEYFRAME_REQUEST_INTERVAL;
            tracing::info!(av1, "screen receiver requested a recovery keyframe");
            let feedback = PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc,
            };
            if track.write_rtcp(vec![Box::new(feedback)]).await.is_err() {
                break;
            }
        }
    }
}

/// Locate the sequence header in a low-overhead AV1 OBU stream.
pub fn av1_sequence_header(data: &[u8]) -> Option<&[u8]> {
    inspect_av1(data)?.0
}

/// A sequence header can arrive separately from the picture it describes.
pub fn av1_frame(data: &[u8]) -> Option<Option<&[u8]>> {
    let (header, picture) = inspect_av1(data)?;
    picture.then_some(header)
}

pub(super) fn inspect_av1(mut data: &[u8]) -> Option<(Option<&[u8]>, bool)> {
    let mut sequence_header = None;
    let mut frame = false;
    let mut frame_header = false;
    let mut tile_group = false;
    while let Some(&header) = data.first() {
        if header & 0x82 != 0x02 {
            return None;
        }
        let mut offset: usize = if header & 4 != 0 { 2 } else { 1 };
        let mut size = 0_usize;
        let mut complete = false;
        for shift in (0..56).step_by(7) {
            let byte = *data.get(offset)?;
            offset += 1;
            size |= usize::from(byte & 0x7f).checked_shl(shift)?;
            if byte & 0x80 == 0 {
                complete = true;
                break;
            }
        }
        if !complete {
            return None;
        }
        let end = offset.checked_add(size)?;
        let obu = data.get(..end)?;
        match (header >> 3) & 15 {
            1 => sequence_header = Some(obu),
            3 if size != 0 => frame_header = true,
            4 if size != 0 => tile_group = true,
            6 if size != 0 => frame = true,
            _ => {}
        }
        data = &data[end..];
    }
    // A header-only inter frame can display an existing reference. A
    // random-access frame must also include the actual picture data.
    let picture = frame || (frame_header && (tile_group || sequence_header.is_none()));
    Some((sequence_header, picture))
}

struct Estimator {
    gcc: Gcc,
    bitrate: Arc<AtomicU32>,
}

#[derive(Default)]
struct PacerBacklog {
    packets: AtomicUsize,
    drained: tokio::sync::Notify,
}

struct ObservedPacer {
    inner: PacerInterceptor,
    backlog: Arc<PacerBacklog>,
    handler: Arc<Handler>,
    local_ssrc: Option<u32>,
    next_refresh: Instant,
    av1_burst_budget: Option<f64>,
}

impl ObservedPacer {
    fn update(&self) {
        let queued = self.inner.queued();
        if self.backlog.packets.swap(queued, Ordering::AcqRel) > queued {
            self.backlog.drained.notify_one();
        }
    }
}

fn screen_pacing_rate(estimate: f64, budget: Option<f64>) -> f64 {
    // Allow large AV1 frames to drain quickly while the link sustains the
    // full profile. Remove the extra headroom as congestion lowers the estimate.
    let Some(budget) = budget else {
        return estimate;
    };
    let headroom = ((estimate / budget - 0.8) / 0.2).clamp(0.0, 1.0) * 1.5;
    let peak = if budget >= f64::from(AV1_1440P30_RATE * 100 / 85) {
        AV1_1440P_PEAK_PACING_RATE
    } else {
        AV1_PEAK_PACING_RATE
    };
    (estimate * (1.0 + headroom)).min(peak)
}

struct SharePacingTimer;

impl SharePacingTimer {
    fn start() -> Option<Self> {
        // Tokio's RTP release deadlines are shorter than the default Windows
        // timer tick. A finer timer lets us send smaller batches on schedule.
        if unsafe { windows::Win32::Media::timeBeginPeriod(1) } == 0 {
            Some(Self)
        } else {
            None
        }
    }
}

impl Drop for SharePacingTimer {
    fn drop(&mut self) {
        unsafe { windows::Win32::Media::timeEndPeriod(1) };
    }
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for ObservedPacer {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = webrtc::error::Error;
    type Time = Instant;

    fn handle_read(&mut self, packet: TaggedPacket) -> webrtc::error::Result<()> {
        if packet.now >= self.next_refresh
            && let Some(ssrc) = self.local_ssrc
            && let Packet::Rtcp(packets) = &packet.message.packet
            && packets.iter().any(|packet| {
                packet
                    .as_any()
                    .downcast_ref::<PictureLossIndication>()
                    .is_some_and(|feedback| feedback.media_ssrc == ssrc)
            })
        {
            self.next_refresh = packet.now + KEYFRAME_REQUEST_INTERVAL;
            self.handler.emit(Event::KeyframeRequested);
        }
        let estimate = match packet.message.get(&Attribute::TargetBitrateChanged {
            bits_per_second: 0.0,
        }) {
            Some(Attribute::TargetBitrateChanged { bits_per_second }) => Some(*bits_per_second),
            _ => None,
        };
        self.inner.handle_read(packet)?;
        if let Some(estimate) = estimate
            && self.av1_burst_budget.is_some()
        {
            self.inner
                .pacer_mut()
                .set_target_bitrate(screen_pacing_rate(estimate, self.av1_burst_budget));
        }
        Ok(())
    }
    fn poll_read(&mut self) -> Option<TaggedPacket> {
        self.inner.poll_read()
    }
    fn handle_write(&mut self, packet: TaggedPacket) -> webrtc::error::Result<()> {
        let result = self.inner.handle_write(packet);
        self.update();
        result
    }
    fn poll_write(&mut self) -> Option<TaggedPacket> {
        self.inner.poll_write()
    }
    fn handle_timeout(&mut self, now: Instant) -> webrtc::error::Result<()> {
        let result = self.inner.handle_timeout(now);
        self.update();
        result
    }
    fn poll_timeout(&mut self) -> Option<Instant> {
        self.inner.poll_timeout()
    }
}

impl Interceptor for ObservedPacer {
    fn bind_local_stream(&mut self, info: &StreamInfo) {
        self.local_ssrc = Some(info.ssrc);
        self.inner.bind_local_stream(info);
    }
    fn unbind_local_stream(&mut self, info: &StreamInfo) {
        self.local_ssrc = None;
        self.inner.unbind_local_stream(info);
    }
    fn bind_remote_stream(&mut self, info: &StreamInfo) {
        self.inner.bind_remote_stream(info);
    }
    fn unbind_remote_stream(&mut self, info: &StreamInfo) {
        self.inner.unbind_remote_stream(info);
    }
}

impl BandwidthEstimator for Estimator {
    fn on_reports(&mut self, now: Instant, reports: &[PacketReport]) {
        self.gcc.on_reports(now, reports);
        self.bitrate
            .store(self.gcc.target_bitrate() as u32, Ordering::Relaxed);
    }
    fn target_bitrate(&self) -> f64 {
        self.gcc.target_bitrate()
    }
    fn handle_timeout(&mut self, now: Instant) {
        self.gcc.handle_timeout(now);
        self.bitrate
            .store(self.gcc.target_bitrate() as u32, Ordering::Relaxed);
    }
    fn poll_timeout(&self) -> Option<Instant> {
        self.gcc.poll_timeout()
    }
    fn stats(&self) -> EstimatorStats {
        self.gcc.stats()
    }
}

async fn run(
    servers: Vec<RTCIceServer>,
    offer: Option<String>,
    answer: oneshot::Receiver<String>,
    mut frames: mpsc::Receiver<Frame>,
    mut stop: oneshot::Receiver<()>,
    handler: Arc<Handler>,
    bitrate: Arc<AtomicU32>,
    path_budget: u32,
) -> Result<()> {
    let mime = if handler.av1 {
        MIME_TYPE_AV1
    } else {
        MIME_TYPE_H264
    };
    let codec = RTCRtpCodec {
        mime_type: mime.into(),
        clock_rate: 90_000,
        channels: 0,
        sdp_fmtp_line: if handler.av1 {
            "profile=0;level-idx=12;tier=0"
        } else {
            "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=4d0032"
        }
        .into(),
        rtcp_feedback: vec![],
    };
    let mut media = MediaEngine::default();
    media.register_codec(
        RTCRtpCodecParameters {
            rtp_codec: codec.clone(),
            payload_type: 102,
        },
        RtpCodecKind::Video,
    )?;
    let estimator = Estimator {
        gcc: Gcc::new(
            f64::from(path_budget),
            f64::from(MIN_RATE),
            f64::from(path_budget),
        ),
        bitrate: Arc::clone(&bitrate),
    };
    let registry = configure_congestion_control(
        Registry::new(),
        estimator,
        CongestionFeedback::Twcc,
        &mut media,
    )?;
    let backlog = Arc::new(PacerBacklog::default());
    let tuned_av1 = handler.av1
        && [
            AV1_720P_RATE,
            AV1_1080P30_RATE,
            AV1_1440P30_RATE,
            AV1_1440P60_RATE,
        ]
        .into_iter()
        .any(|rate| path_budget == rate * 100 / 85);
    let av1_burst_budget = tuned_av1.then_some(f64::from(path_budget));
    let _pacing_timer = if tuned_av1 && offer.is_none() {
        SharePacingTimer::start()
    } else {
        None
    };
    let burst_packets = if tuned_av1 && path_budget == AV1_1440P60_RATE * 100 / 85 {
        // Windows can coalesce timer wakes while the sender is occluded.
        // A 20-packet bucket then caps effective throughput below 18 Mbps.
        48.0
    } else if tuned_av1 && path_budget == AV1_1440P30_RATE * 100 / 85 {
        32.0
    } else if tuned_av1 {
        // Keep the old batch size if Windows declined the timer request.
        if _pacing_timer.is_some() { 20.0 } else { 32.0 }
    } else {
        16.0
    };
    let registry = register_default_interceptors(registry, &mut media)?
        .with(
            Slot::NackGenerator,
            NackGeneratorBuilder::new()
                .with_interval(Duration::from_millis(30))
                .with_skip_last_n(3)
                .with_max_nacks_per_packet(1)
                .build(),
        )
        .with(
            Slot::Pacer,
            ObservedPacer {
                inner: PacerBuilder::new()
                    .with_target_bitrate(screen_pacing_rate(
                        f64::from(path_budget),
                        av1_burst_budget,
                    ))
                    .with_burst_bits(burst_packets * 1200.0 * 8.0)
                    .build(),
                backlog: Arc::clone(&backlog),
                handler: Arc::clone(&handler),
                local_ssrc: None,
                next_refresh: Instant::now(),
                av1_burst_budget,
            },
        );
    let loopback = servers.is_empty();
    let bind_addresses = if loopback {
        vec!["127.0.0.1:0"]
    } else {
        vec!["0.0.0.0:0", "[::]:0"]
    };
    let peer = PeerConnectionBuilder::new()
        .with_setting_engine(
            SettingEngineBuilder::new()
                .with_include_loopback_candidate(loopback)
                // Keep replay protection enabled while allowing repair
                // packets as far back as the frame assembler can use them.
                .with_srtp_replay_protection_window(1024)
                .build(),
        )
        .with_configuration(
            RTCConfigurationBuilder::new()
                .with_ice_servers(servers)
                .build(),
        )
        .with_media_engine(media)
        .with_interceptor_registry(registry)
        .with_handler(handler.clone())
        .with_udp_addrs(bind_addresses)
        .build()
        .await?;
    let result = tokio::select! {
        _ = &mut stop => Ok(()),
        result = async {
            let initiator = offer.is_none();
            let mut track_and_sender = None;
            let description = if let Some(sdp) = offer {
                peer.set_remote_description(RTCSessionDescription::offer(sdp)?).await?;
                peer.create_answer(None).await?
            } else {
                let ssrc = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.subsec_nanos().max(1);
                let track = Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                    "zink-screen".into(), "zink-video".into(), "Zink Screen".into(), RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(ssrc), ..Default::default() },
                        codec,
                        ..Default::default()
                    }],
                )));
                let sender = peer.add_track(track.clone()).await?;
                track_and_sender = Some((track, sender, ssrc));
                peer.create_offer(None).await?
            };
            peer.set_local_description(description).await?;
            // Keep late STUN responses from delaying the compatibility path.
            let _ = tokio::time::timeout(Duration::from_secs(3), handler.gathering.notified()).await;
            let description = peer.local_description().await.context("missing local screen SDP")?;
            handler.events.send((handler.id.clone(), Event::Description(description.sdp))).await?;
            if initiator {
                let sdp = tokio::time::timeout(NEGOTIATION_TIMEOUT, answer).await??;
                peer.set_remote_description(RTCSessionDescription::answer(sdp)?).await?;
            }
            tokio::time::timeout(NEGOTIATION_TIMEOUT, async {
                while !handler.ready.load(Ordering::Acquire) && !handler.failed.load(Ordering::Acquire) {
                    handler.changed.notified().await;
                }
            }).await?;
            anyhow::ensure!(!handler.failed.load(Ordering::Acquire), "screen WebRTC connection failed");
            let Some((track, sender, ssrc)) = track_and_sender else {
                while !handler.failed.load(Ordering::Acquire) {
                    handler.changed.notified().await;
                }
                return Ok(());
            };
            let codec = sender.get_parameters().await?.rtp_parameters.codecs.into_iter()
                .find(|codec| codec.rtp_codec.mime_type.eq_ignore_ascii_case(mime))
                .context("no compatible screen media track")?;
            let mut packetizer = new_packetizer(
                Instant::now(), 1200, codec.payload_type, ssrc, codec.rtp_codec.payloader()?,
                Box::new(new_random_sequencer()), codec.rtp_codec.clock_rate,
            );
            let mut previous = None;
            let mut ticks = 0_u32;
            let mut next_pacing_log = Instant::now();
            loop {
                tokio::select! {
                    frame = frames.recv() => {
                        let Some(frame) = frame else { break };
                        if let Some(last) = previous.replace(frame.timestamp_ms) {
                            let elapsed_ms = frame.timestamp_ms.saturating_sub(last).max(1) as u32;
                            ticks = ticks.wrapping_add(elapsed_ms.wrapping_mul(codec.rtp_codec.clock_rate / 1_000));
                        }
                        // Apply pressure only when the real RTP queue exceeds
                        // roughly 30 ms. A large frame on an idle link never
                        // adds a second timer after the RTP pacer.
                        let pacing_started = Instant::now();
                        let initial_backlog = backlog.packets.load(Ordering::Acquire);
                        loop {
                            let queued = backlog.packets.load(Ordering::Acquire);
                            let allowed = (bitrate.load(Ordering::Relaxed) as usize / 320_000).max(2);
                            if queued <= allowed { break; }
                            tokio::select! {
                                _ = backlog.drained.notified() => {}
                                _ = handler.changed.notified() => {
                                    if !handler.ready.load(Ordering::Acquire) {
                                        anyhow::bail!("screen WebRTC disconnected while pacing");
                                    }
                                }
                            }
                        }
                        let now = Instant::now();
                        let pacing_wait = now.duration_since(pacing_started);
                        if now >= next_pacing_log && pacing_wait >= Duration::from_millis(45) {
                            tracing::info!(
                                av1 = handler.av1,
                                timestamp_ms = frame.timestamp_ms,
                                frame_bytes = frame.data.len(),
                                queued_packets = initial_backlog,
                                bitrate_bps = bitrate.load(Ordering::Relaxed),
                                pacing_wait_ms = pacing_wait.as_millis() as u64,
                                "screen sender waited for the RTP queue"
                            );
                            next_pacing_log = now + Duration::from_secs(1);
                        }
                        // Keep the packetizer's randomized base timestamp.
                        // Apply each capture interval to this frame, since the
                        // packetizer would otherwise apply it to the next one.
                        for mut packet in packetizer.packetize(now, &frame.data.into(), 0)? {
                            packet.header.timestamp = packet.header.timestamp.wrapping_add(ticks);
                            track.write_rtp_with_extensions(packet, &[]).await?;
                        }
                        drop(frame._slot);
                    }
                    _ = handler.changed.notified() => {
                        if handler.failed.load(Ordering::Acquire) { break; }
                    }
                }
            }
            Ok(())
        } => result,
    };
    let _ = peer.close().await;
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_pacing_headroom_tapers_off_when_bandwidth_falls() {
        let budget = 7_000_000.0;
        assert!((screen_pacing_rate(budget, Some(budget)) - budget * 2.5).abs() < 1.0);
        assert!((screen_pacing_rate(budget * 0.8, Some(budget)) - budget * 0.8).abs() < 1.0);
        assert_eq!(screen_pacing_rate(budget, None), budget);
        assert_eq!(
            screen_pacing_rate(11_750_000.0, Some(11_750_000.0)),
            AV1_PEAK_PACING_RATE
        );
    }

    #[tokio::test]
    async fn negotiates_and_delivers_h264_and_av1_as_web_rtc_media() {
        // AV1 sequence header + frame OBUs, and H.264 SPS/PPS + IDR NAL units.
        // These exercise transport and packetization, independently of the codecs.
        let h264 = vec![
            0, 0, 0, 1, 0x67, 0x4d, 0, 0x32, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88, 0x84,
        ];
        let av1 = vec![0x0a, 1, 0, 0x32, 2, 0x10, 0x12];
        let mut large_h264 = h264.clone();
        large_h264.resize(180_000, 0x81);
        let mut large_av1 = vec![0x0a, 1, 0, 0x32, 0xa0, 0xfe, 0x0a];
        large_av1.resize(180_007, 0x10);
        for (is_av1, bytes, keyframe) in [(false, h264, large_h264), (true, av1, large_av1)] {
            tokio::time::timeout(Duration::from_secs(12), async {
                let (tx, mut tx_events) = mpsc::channel(16);
                let (rx, mut rx_events) = mpsc::channel(16);
                let profile_bitrate = if is_av1 { 6_000_000 } else { 16_000_000 };
                let mut sender =
                    start_with_ice("test".into(), None, is_av1, profile_bitrate, tx, async {
                        vec![]
                    });
                assert_eq!(
                    sender.bitrate.load(Ordering::Relaxed),
                    profile_bitrate * 100 / 85
                );
                let Some((_, Event::Description(offer))) = tx_events.recv().await else {
                    panic!("no offer")
                };
                let receiver =
                    start_with_ice("test".into(), Some(offer), is_av1, 0, rx, async { vec![] });
                let Some((_, Event::Description(answer))) = rx_events.recv().await else {
                    panic!("no answer")
                };
                sender.answer.take().unwrap().send(answer).unwrap();
                while !sender.ready.load(Ordering::Acquire)
                    || !receiver.ready.load(Ordering::Acquire)
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let slots = Arc::new(tokio::sync::Semaphore::new(4));
                for (index, timestamp_ms) in [0, 33, 99, 132].into_iter().enumerate() {
                    let slot = Arc::clone(&slots).acquire_owned().await.unwrap();
                    assert!(
                        sender
                            .frames
                            .send(Frame {
                                data: if index == 0 {
                                    keyframe.clone()
                                } else {
                                    bytes.clone()
                                },
                                timestamp_ms,
                                _slot: Arc::new(slot)
                            })
                            .await
                            .is_ok()
                    );
                }
                let Some((
                    _,
                    Event::Frame {
                        data,
                        key_frame: true,
                        ..
                    },
                )) = rx_events.recv().await
                else {
                    panic!("peer received no keyframe for AV1={is_av1}");
                };
                assert!(
                    data.len() >= 180_000,
                    "the fragmented keyframe was truncated"
                );
                // A skipped capture must show up before the frame it delayed.
                for expected in [33, 99] {
                    let Some((_, Event::Frame { timestamp_ms, .. })) = rx_events.recv().await
                    else {
                        panic!("no screen frame at {expected} ms");
                    };
                    assert_eq!(timestamp_ms, expected);
                }
            })
            .await
            .expect("screen media must connect and arrive over loopback");
        }
    }
}
#[test]
fn av1_keyframe_needs_picture_data_after_its_sequence_header() {
    let sequence = [0x0a, 1, 0];
    assert_eq!(av1_sequence_header(&sequence), Some(sequence.as_slice()));
    assert_eq!(av1_frame(&sequence), None);
    assert_eq!(av1_frame(&[0x0a, 1, 0, 0x32, 2, 0x10]), None);
    assert_eq!(
        av1_frame(&[0x0a, 1, 0, 0x32, 2, 0x10, 0x12]),
        Some(Some(sequence.as_slice()))
    );
}
