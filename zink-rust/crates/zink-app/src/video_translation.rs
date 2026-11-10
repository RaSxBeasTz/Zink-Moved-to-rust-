use anyhow::{Context, bail};
use directories::ProjectDirs;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use windows::Foundation::TimeSpan;
use windows::Media::MediaProperties::{AudioEncodingQuality, MediaEncodingProfile};
use windows::Media::Transcoding::MediaTranscoder;
use windows::Storage::StorageFile;
use windows::core::HSTRING;

const SEGMENT_MS: u64 = 10 * 60 * 1_000;
const TRANSLATIONS_ENDPOINT: &str = "https://api.openai.com/v1/audio/translations";

#[derive(Debug)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    text: String,
}

pub fn translate_to_vtt(
    video_path: &Path,
    duration_ms: u64,
    api_key: &str,
    mut progress: impl FnMut(String),
) -> anyhow::Result<(PathBuf, String)> {
    let cache_dir = ProjectDirs::from("com", "Zink", "Zink")
        .context("resolve Zink data directory")?
        .cache_dir()
        .join("AudioTranslations");
    std::fs::create_dir_all(&cache_dir).context("create translation cache")?;
    let cache_key = cache_key(video_path)?;
    let destination = cache_dir.join(format!("{cache_key}.uk-en.vtt"));
    if destination.is_file() {
        let contents = std::fs::read_to_string(&destination).context("read cached translation")?;
        return Ok((destination, contents));
    }

    let segment_dir = cache_dir.join(format!("{cache_key}.segments"));
    std::fs::create_dir_all(&segment_dir).context("create translation segment cache")?;
    let total = duration_ms.max(1).div_ceil(SEGMENT_MS);
    let mut cues = Vec::new();
    for index in 0..total {
        let start_ms = index * SEGMENT_MS;
        let stop_ms = duration_ms.min(start_ms.saturating_add(SEGMENT_MS));
        progress(format!("Preparing audio {}/{}", index + 1, total));
        let audio_path = segment_dir.join(format!("segment-{index:03}.mp3"));
        transcode_segment(video_path, &audio_path, start_ms, stop_ms)?;
        progress(format!("Translating {}/{}", index + 1, total));
        let segment_vtt = translate_segment(&audio_path, api_key)?;
        cues.extend(parse_vtt(&segment_vtt, start_ms));
        let _ = std::fs::remove_file(audio_path);
    }

    let vtt = build_vtt(&cues);
    std::fs::write(&destination, &vtt).context("save translated subtitles")?;
    let _ = std::fs::remove_dir(segment_dir);
    Ok((destination, vtt))
}

fn cache_key(path: &Path) -> anyhow::Result<String> {
    let metadata = std::fs::metadata(path).context("read video metadata")?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_secs());
    let mut hash = Sha256::new();
    hash.update(path.to_string_lossy().as_bytes());
    hash.update(metadata.len().to_le_bytes());
    hash.update(modified.to_le_bytes());
    hash.update(b"uk-en-native-v1");
    Ok(format!("{:x}", hash.finalize()))
}

fn transcode_segment(
    source: &Path,
    destination: &Path,
    start_ms: u64,
    stop_ms: u64,
) -> anyhow::Result<()> {
    std::fs::File::create(destination).context("create temporary audio segment")?;
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("create media translation runtime")?;
    runtime.block_on(async move {
        let source =
            StorageFile::GetFileFromPathAsync(&HSTRING::from(source.to_string_lossy().as_ref()))
                .context("open source video")?
                .await
                .context("open source video")?;
        let destination = StorageFile::GetFileFromPathAsync(&HSTRING::from(
            destination.to_string_lossy().as_ref(),
        ))
        .context("open audio destination")?
        .await
        .context("open audio destination")?;
        let transcoder = MediaTranscoder::new().context("create Windows media transcoder")?;
        transcoder
            .SetAlwaysReencode(true)
            .context("enable audio re-encoding")?;
        transcoder
            .SetTrimStartTime(TimeSpan {
                Duration: ticks(start_ms),
            })
            .context("set segment start")?;
        if stop_ms > start_ms {
            transcoder
                .SetTrimStopTime(TimeSpan {
                    Duration: ticks(stop_ms),
                })
                .context("set segment end")?;
        }
        let profile = MediaEncodingProfile::CreateMp3(AudioEncodingQuality::Low)
            .context("create MP3 encoding profile")?;
        let prepared = transcoder
            .PrepareFileTranscodeAsync(&source, &destination, &profile)
            .context("prepare native audio extraction")?
            .await
            .context("prepare native audio extraction")?;
        if !prepared.CanTranscode().context("check transcode support")? {
            bail!(
                "Windows Media Foundation could not extract this video's audio ({:?})",
                prepared.FailureReason()
            );
        }
        prepared
            .TranscodeAsync()
            .context("start native audio extraction")?
            .await
            .context("extract video audio")?;
        Ok::<(), anyhow::Error>(())
    })
}

fn ticks(milliseconds: u64) -> i64 {
    i64::try_from(milliseconds.saturating_mul(10_000)).unwrap_or(i64::MAX)
}

fn translate_segment(path: &Path, api_key: &str) -> anyhow::Result<String> {
    let audio = std::fs::read(path).context("read extracted audio")?;
    let boundary = format!(
        "zink-{:x}",
        Sha256::digest(path.to_string_lossy().as_bytes())
    );
    let mut body = Vec::with_capacity(audio.len() + 1_024);
    for (name, value) in [
        ("model", "whisper-1"),
        ("response_format", "vtt"),
        (
            "prompt",
            "Translate all speech into natural UK English. Keep names, brands, and technical terms accurate.",
        ),
    ] {
        write_field(&mut body, &boundary, name, value);
    }
    body.extend_from_slice(format!("--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"segment.mp3\"\r\nContent-Type: audio/mpeg\r\n\r\n").as_bytes());
    body.extend_from_slice(&audio);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    let response = ureq::post(TRANSLATIONS_ENDPOINT)
        .header("Authorization", format!("Bearer {api_key}"))
        .header(
            "Content-Type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .send(body)
        .context("send audio translation request")?;
    response
        .into_body()
        .read_to_string()
        .context("read audio translation response")
}

fn write_field(body: &mut Vec<u8>, boundary: &str, name: &str, value: &str) {
    body.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
        )
        .as_bytes(),
    );
}

fn parse_vtt(value: &str, offset_ms: u64) -> Vec<Cue> {
    let mut cues = Vec::new();
    let lines = value.lines().collect::<Vec<_>>();
    let mut index = 0;
    while index < lines.len() {
        if let Some((start, end)) = lines[index].split_once(" --> ")
            && let (Some(start_ms), Some(end_ms)) = (
                parse_timestamp(start),
                parse_timestamp(end.split_whitespace().next().unwrap_or(end)),
            )
        {
            index += 1;
            let mut text = Vec::new();
            while index < lines.len() && !lines[index].trim().is_empty() {
                text.push(lines[index].trim());
                index += 1;
            }
            if !text.is_empty() {
                cues.push(Cue {
                    start_ms: start_ms.saturating_add(offset_ms),
                    end_ms: end_ms.saturating_add(offset_ms),
                    text: text.join("\n"),
                });
            }
        }
        index += 1;
    }
    cues
}

fn parse_timestamp(value: &str) -> Option<u64> {
    let value = value.trim().replace(',', ".");
    let parts = value.split(':').collect::<Vec<_>>();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] => (0, minutes.parse::<u64>().ok()?, *seconds),
        [hours, minutes, seconds] => (hours.parse().ok()?, minutes.parse().ok()?, *seconds),
        _ => return None,
    };
    let (seconds, millis) = seconds.split_once('.').unwrap_or((seconds, "0"));
    let millis = format!("{millis:0<3}")
        .chars()
        .take(3)
        .collect::<String>()
        .parse::<u64>()
        .ok()?;
    Some((((hours * 60 + minutes) * 60 + seconds.parse::<u64>().ok()?) * 1_000) + millis)
}

fn build_vtt(cues: &[Cue]) -> String {
    let mut output = String::from("WEBVTT\n\n");
    for cue in cues {
        let _ = writeln!(
            output,
            "{} --> {}\n{}\n",
            format_timestamp(cue.start_ms),
            format_timestamp(cue.end_ms),
            cue.text
        );
    }
    output
}

fn format_timestamp(milliseconds: u64) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = (milliseconds / 60_000) % 60;
    let seconds = (milliseconds / 1_000) % 60;
    let millis = milliseconds % 1_000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}
