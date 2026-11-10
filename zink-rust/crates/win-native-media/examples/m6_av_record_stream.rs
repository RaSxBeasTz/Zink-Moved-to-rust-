//! M6 (part 2) verification: full A/V pipeline.
//! Video (capture->H264) + audio (loopback + mic -> AAC) muxed into:
//!   - MP4 with a video track + both audio tracks (separate)
//!   - RTMP with video + a single mixed (loopback+mic) AAC track
//!
//! Prereq: mediamtx for the stream branch. Run:
//!   cargo run --example m6_av_record_stream
//! Verify: test_av.mp4 plays with audio; mediamtx shows H264+AAC tracks.

use std::time::Duration;

use win_native_media::audio::aac::AacEncoder;
use win_native_media::audio::{capture as acap, mix};
use win_native_media::capture::{self, CaptureConfig};
use win_native_media::encoder::mf_h264::MfH264Encoder;
use win_native_media::mux::{AudioTrack, Mp4Recorder};
use win_native_media::stream::RtmpPublisher;
use win_native_media::{AudioSource, CaptureTarget, StreamConfig, VideoConfig};

const RUN_SECS: u64 = 8;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let mut args = std::env::args().skip(1);
    let url = args
        .next()
        .unwrap_or_else(|| "rtmp://localhost:1935/live".into());
    let key = args.next().unwrap_or_else(|| "test".into());

    let vcfg = VideoConfig {
        width: 1920,
        height: 1080,
        fps: 30,
        bitrate: 4_000_000,
        keyframe_interval: 30,
    };

    // --- start captures ---
    let (session, vrx) = capture::start(
        CaptureConfig {
            target: CaptureTarget::Monitor(0),
            capture_cursor: true,
        },
        8,
    )
    .expect("video capture");
    let mut venc = MfH264Encoder::new(session.device(), vcfg).expect("h264");

    let (loop_cap, loop_rx) = acap::start(AudioSource::SystemLoopback, 64).expect("loopback");
    let sr = loop_cap.sample_rate;
    let ch = loop_cap.channels;
    // Mic is optional (may be absent).
    let mic = acap::start(AudioSource::Microphone, 64).ok();

    // AAC encoders: one per MP4 track (loopback, mic) + one for the RTMP mix.
    let mut aac_loop = AacEncoder::new(sr, ch, 128_000).expect("aac loop");
    let mut aac_mic = mic
        .as_ref()
        .map(|(c, _)| AacEncoder::new(c.sample_rate, c.channels, 128_000).expect("aac mic"));
    let mut aac_mix = AacEncoder::new(sr, ch, 128_000).expect("aac mix");

    // --- warm up video encoder for SPS/PPS ---
    let mut vpending = Vec::new();
    let mut vfed = 0;
    while venc.parameter_sets().sps.is_empty() && vfed < 60 {
        if let Ok(f) = vrx.recv_timeout(Duration::from_secs(5)) {
            let mut out = Vec::new();
            venc.encode(&f.texture, f.timestamp, &mut out).expect("enc");
            vfed += 1;
            vpending.extend(out);
        } else {
            break;
        }
    }
    let vparams = venc.parameter_sets().clone();

    // --- build MP4 with video + audio tracks (loopback track 0, mic track 1) ---
    let mut audio_tracks = vec![AudioTrack {
        sample_rate: sr,
        channels: ch,
        asc: aac_loop.audio_specific_config().to_vec(),
        bitrate: 128_000,
    }];
    if let Some(a) = &aac_mic {
        audio_tracks.push(AudioTrack {
            sample_rate: sr,
            channels: ch,
            asc: a.audio_specific_config().to_vec(),
            bitrate: 128_000,
        });
    }
    let mut recorder =
        Mp4Recorder::with_audio("test_av.mp4", &vcfg, &vparams, &audio_tracks).expect("mp4");

    // --- connect RTMP, send both sequence headers ---
    let mut pubr = RtmpPublisher::connect(
        StreamConfig {
            url: url.clone(),
            stream_key: key.clone(),
        },
        vcfg,
        vparams.clone(),
    )
    .await
    .expect("rtmp");
    pubr.send_audio_sequence_header(aac_mix.audio_specific_config())
        .await
        .expect("aac seqhdr");

    for s in &vpending {
        let _ = pubr.send_video(s).await;
        recorder.write(s).expect("rec v");
    }

    // --- main loop: pump video + audio for RUN_SECS ---
    let deadline = std::time::Instant::now() + Duration::from_secs(RUN_SECS);
    let mut counts = Counts::default();
    let mut last_loop_pcm: Vec<u8> = Vec::new();

    while std::time::Instant::now() < deadline {
        // video
        if let Ok(f) = vrx.recv_timeout(Duration::from_millis(5)) {
            let mut out = Vec::new();
            if venc.encode(&f.texture, f.timestamp, &mut out).is_ok() {
                for s in &out {
                    let _ = pubr.send_video(s).await;
                    recorder.write(s).expect("rec v");
                    counts.video += 1;
                }
            }
        }
        // loopback audio -> MP4 track 0 + hold for mixing
        while let Ok(buf) = loop_rx.try_recv() {
            let mut out = Vec::new();
            aac_loop
                .encode(&buf.data, buf.timestamp, &mut out)
                .expect("aac loop");
            for f in &out {
                recorder
                    .write_audio(0, &f.data, f.timestamp)
                    .expect("rec a0");
                counts.mp4_loop += 1;
            }
            last_loop_pcm = buf.data.clone();

            // mix with the most recent mic buffer (or silence) for RTMP
            let mic_pcm = mic.as_ref().and_then(|(_, r)| r.try_recv().ok());
            let (mixed, mic_data) = match mic_pcm {
                Some(m) => (mix::mix_pcm16(&buf.data, &m.data), Some(m)),
                None => (buf.data.clone(), None),
            };
            // mic -> MP4 track 1
            if let (Some(enc), Some(m)) = (aac_mic.as_mut(), &mic_data) {
                let mut mo = Vec::new();
                enc.encode(&m.data, m.timestamp, &mut mo).expect("aac mic");
                for f in &mo {
                    recorder
                        .write_audio(1, &f.data, f.timestamp)
                        .expect("rec a1");
                    counts.mp4_mic += 1;
                }
            }
            // mixed -> RTMP
            let mut xo = Vec::new();
            aac_mix
                .encode(&mixed, buf.timestamp, &mut xo)
                .expect("aac mix");
            for f in &xo {
                let _ = pubr.send_audio(&f.data, f.timestamp).await;
                counts.rtmp_audio += 1;
            }
        }
    }
    let _ = last_loop_pcm;

    // drain + finalize
    let mut vt = Vec::new();
    let _ = venc.drain(&mut vt);
    for s in &vt {
        let _ = pubr.send_video(s).await;
        recorder.write(s).ok();
    }
    recorder.finalize().expect("finalize");

    drop(session);
    drop(loop_cap);
    drop(mic);

    let mp4 = std::fs::metadata("test_av.mp4")
        .map(|m| m.len())
        .unwrap_or(0);
    println!("--- M6 A/V result ---");
    println!("  video frames:      {}", counts.video);
    println!("  MP4 loopback AAC:  {}", counts.mp4_loop);
    println!("  MP4 mic AAC:       {}", counts.mp4_mic);
    println!("  RTMP mixed AAC:    {}", counts.rtmp_audio);
    println!("  test_av.mp4:       {mp4} bytes");
    let ok = counts.video > 0 && counts.mp4_loop > 0 && counts.rtmp_audio > 0 && mp4 > 0;
    println!("  RESULT: {}", if ok { "OK" } else { "FAIL" });
    if !ok {
        std::process::exit(1);
    }
}

#[derive(Default)]
struct Counts {
    video: u64,
    mp4_loop: u64,
    mp4_mic: u64,
    rtmp_audio: u64,
}
