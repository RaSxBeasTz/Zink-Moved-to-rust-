use std::{
    collections::{BTreeMap, HashSet},
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

const VIDEO_EXTENSIONS: &[&str] = &[
    "avi", "m2ts", "m4v", "mkv", "mov", "mp4", "mpeg", "mpg", "ts", "webm", "wmv",
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TorrentMoveItem {
    pub(crate) torrent_key: String,
    pub(crate) name: String,
    pub(crate) save_path: PathBuf,
    pub(crate) completed_on: i64,
    pub(crate) files: Vec<PathBuf>,
}

impl TorrentMoveItem {
    pub(crate) fn release_year(&self) -> Option<u16> {
        release_year(&self.name)
    }

    fn stable_id(&self) -> String {
        let key = Path::new(self.torrent_key.trim())
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or_else(|| self.torrent_key.trim());
        format!("torrent:{key}")
    }
}

#[derive(Debug)]
enum BencodeValue {
    Integer(i64),
    Bytes(Vec<u8>),
    List,
    Dictionary(BTreeMap<Vec<u8>, BencodeValue>),
}

pub(crate) fn read_completed(data_root: &Path) -> Result<Vec<TorrentMoveItem>> {
    let app_data = std::env::var_os("APPDATA").context("APPDATA is unavailable")?;
    read_completed_from(
        &PathBuf::from(app_data).join("uTorrent/resume.dat"),
        &data_root.join("FileMover/moved-torrents.json"),
    )
}

fn read_completed_from(resume_path: &Path, history_path: &Path) -> Result<Vec<TorrentMoveItem>> {
    if !resume_path.is_file() {
        return Ok(Vec::new());
    }
    let bytes = fs::read(resume_path)
        .with_context(|| format!("read µTorrent data from {}", resume_path.display()))?;
    let mut reader = BencodeReader::new(&bytes);
    let BencodeValue::Dictionary(root) = reader.read_value()? else {
        anyhow::bail!("µTorrent resume.dat does not contain a dictionary")
    };
    let moved = read_moved_ids(history_path)?;
    let mut items = Vec::new();
    for (raw_key, value) in root {
        let BencodeValue::Dictionary(values) = value else {
            continue;
        };
        let size = value_integer(&values, b"size");
        let downloaded = value_integer(&values, b"downloaded");
        let completed_on = value_integer(&values, b"completed_on");
        if completed_on <= 0 && (size <= 0 || downloaded < size) {
            continue;
        }
        if value_text(&values, b"dl_error")
            .to_ascii_lowercase()
            .contains("files missing from job")
        {
            continue;
        }
        let torrent_key = String::from_utf8_lossy(&raw_key).into_owned();
        let name = {
            let caption = value_text(&values, b"caption");
            if caption.trim().is_empty() {
                Path::new(&torrent_key)
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .unwrap_or("Completed download")
                    .to_owned()
            } else {
                caption
            }
        };
        let save_path = PathBuf::from(value_text(&values, b"path"));
        if save_path.as_os_str().is_empty() {
            continue;
        }
        let files = find_video_files(&save_path, &name);
        if files.is_empty() {
            continue;
        }
        let item = TorrentMoveItem {
            torrent_key,
            name,
            save_path,
            completed_on,
            files,
        };
        let caption_id = format!("caption:{}", item.name.trim());
        let raw_id = format!("torrent:{}", item.torrent_key.trim());
        let legacy_id = format!("{}\0{}", item.name, item.save_path.display());
        if moved.contains(&item.stable_id())
            || moved.contains(&caption_id)
            || moved.contains(&raw_id)
            || moved.contains(&legacy_id)
        {
            continue;
        }
        items.push(item);
    }
    items.sort_by(|left, right| {
        right
            .release_year()
            .cmp(&left.release_year())
            .then_with(|| right.completed_on.cmp(&left.completed_on))
            .then_with(|| left.name.to_lowercase().cmp(&right.name.to_lowercase()))
    });
    items.dedup_by(|left, right| left.stable_id().eq_ignore_ascii_case(&right.stable_id()));
    Ok(items)
}

pub(crate) fn mark_moved(data_root: &Path, items: &[TorrentMoveItem]) -> Result<()> {
    let history_path = data_root.join("FileMover/moved-torrents.json");
    let mut moved = read_moved_ids(&history_path)?;
    for item in items {
        moved.insert(item.stable_id());
        moved.insert(format!("torrent:{}", item.torrent_key.trim()));
        moved.insert(format!("caption:{}", item.name.trim()));
        moved.insert(format!("{}\0{}", item.name, item.save_path.display()));
    }
    let mut values = moved.into_iter().collect::<Vec<_>>();
    values.sort();
    if let Some(parent) = history_path.parent() {
        fs::create_dir_all(parent)?;
    }
    let temporary = history_path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec(&values)?)?;
    fs::rename(&temporary, &history_path).or_else(|_| {
        fs::copy(&temporary, &history_path)?;
        fs::remove_file(&temporary)
    })?;
    Ok(())
}

pub(crate) fn clean_movie_file_name(original: &Path) -> String {
    let extension = original
        .extension()
        .and_then(|value| value.to_str())
        .map(|value| format!(".{value}"))
        .unwrap_or_default();
    let stem = original
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("video");
    let mut words = stem
        .replace(['.', '_'], " ")
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if let Some(index) = words.iter().position(|word| {
        let lower = word.to_ascii_lowercase();
        matches!(
            lower.as_str(),
            "720p"
                | "1080p"
                | "1440p"
                | "2160p"
                | "4k"
                | "web-dl"
                | "webrip"
                | "bluray"
                | "brrip"
                | "hdtv"
                | "x264"
                | "x265"
                | "h264"
                | "h265"
                | "hevc"
        )
    }) {
        words.truncate(index);
    }
    let cleaned = words.join(" ").trim().to_owned();
    format!(
        "{}{extension}",
        if cleaned.is_empty() { stem } else { &cleaned }
    )
}

fn read_moved_ids(path: &Path) -> Result<HashSet<String>> {
    if !path.is_file() {
        return Ok(HashSet::new());
    }
    let bytes = fs::read(path)?;
    let values =
        serde_json::from_slice::<Vec<String>>(&bytes).context("read moved µTorrent history")?;
    Ok(values.into_iter().collect())
}

fn find_video_files(save_path: &Path, name: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if save_path.is_file() {
        candidates.push(save_path.to_owned());
    } else if save_path.is_dir() {
        let named = save_path.join(name);
        if named.is_file() {
            candidates.push(named);
        } else if named.is_dir() {
            collect_files(&named, &mut candidates);
        } else if let Ok(entries) = fs::read_dir(save_path) {
            let mut top_level_videos = Vec::new();
            let wanted = normalize_payload_name(name);
            for entry in entries.flatten() {
                let path = entry.path();
                let key = normalize_payload_name(
                    path.file_name()
                        .and_then(|value| value.to_str())
                        .unwrap_or_default(),
                );
                if path.is_file() && is_video(&path) {
                    top_level_videos.push(path.clone());
                    if key == wanted || key.starts_with(&wanted) || wanted.starts_with(&key) {
                        candidates.push(path);
                    }
                } else if path.is_dir()
                    && (key == wanted || key.starts_with(&wanted) || wanted.starts_with(&key))
                {
                    collect_files(&path, &mut candidates);
                }
            }
            if candidates.is_empty() && top_level_videos.len() == 1 {
                candidates.push(top_level_videos.remove(0));
            }
        }
    }
    candidates.retain(|path| is_video(path));
    candidates.sort();
    candidates.dedup();
    candidates
}

fn collect_files(folder: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(folder) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(&path, output);
        } else if is_video(&path) {
            output.push(path);
        }
    }
}

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| {
            VIDEO_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

fn normalize_payload_name(value: &str) -> String {
    let stem = Path::new(value)
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or(value);
    stem.chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn release_year(value: &str) -> Option<u16> {
    value
        .split(|character: char| !character.is_ascii_digit())
        .find_map(|part| {
            (part.len() == 4)
                .then(|| part.parse::<u16>().ok())
                .flatten()
                .filter(|year| (1900..=2099).contains(year))
        })
}

fn value_integer(values: &BTreeMap<Vec<u8>, BencodeValue>, key: &[u8]) -> i64 {
    match values.get(key) {
        Some(BencodeValue::Integer(value)) => *value,
        _ => 0,
    }
}

fn value_text(values: &BTreeMap<Vec<u8>, BencodeValue>, key: &[u8]) -> String {
    match values.get(key) {
        Some(BencodeValue::Bytes(value)) => String::from_utf8_lossy(value).into_owned(),
        _ => String::new(),
    }
}

struct BencodeReader<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> BencodeReader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn read_value(&mut self) -> Result<BencodeValue> {
        let marker = self.read_byte()?;
        match marker {
            b'd' => self.read_dictionary(),
            b'l' => self.read_list(),
            b'i' => self.read_integer(),
            b'0'..=b'9' => self.read_bytes(marker),
            _ => anyhow::bail!("invalid bencode marker"),
        }
    }

    fn read_dictionary(&mut self) -> Result<BencodeValue> {
        let mut values = BTreeMap::new();
        while self.peek_byte()? != b'e' {
            let BencodeValue::Bytes(key) = self.read_value()? else {
                anyhow::bail!("bencode dictionary key is not a string")
            };
            values.insert(key, self.read_value()?);
        }
        self.offset += 1;
        Ok(BencodeValue::Dictionary(values))
    }

    fn read_list(&mut self) -> Result<BencodeValue> {
        while self.peek_byte()? != b'e' {
            let _ = self.read_value()?;
        }
        self.offset += 1;
        Ok(BencodeValue::List)
    }

    fn read_integer(&mut self) -> Result<BencodeValue> {
        let text = self.read_until(b'e')?;
        let value = std::str::from_utf8(text)?.parse::<i64>()?;
        Ok(BencodeValue::Integer(value))
    }

    fn read_bytes(&mut self, first_digit: u8) -> Result<BencodeValue> {
        let mut digits = vec![first_digit];
        digits.extend_from_slice(self.read_until(b':')?);
        let length = std::str::from_utf8(&digits)?.parse::<usize>()?;
        let end = self
            .offset
            .checked_add(length)
            .context("bencode length overflow")?;
        let value = self
            .bytes
            .get(self.offset..end)
            .context("truncated bencode string")?
            .to_vec();
        self.offset = end;
        Ok(BencodeValue::Bytes(value))
    }

    fn read_until(&mut self, terminator: u8) -> Result<&'a [u8]> {
        let start = self.offset;
        let relative = self
            .bytes
            .get(start..)
            .context("unexpected end of bencode data")?
            .iter()
            .position(|value| *value == terminator)
            .context("unterminated bencode value")?;
        let end = start + relative;
        self.offset = end + 1;
        Ok(&self.bytes[start..end])
    }

    fn read_byte(&mut self) -> Result<u8> {
        let value = *self
            .bytes
            .get(self.offset)
            .context("unexpected end of bencode data")?;
        self.offset += 1;
        Ok(value)
    }

    fn peek_byte(&self) -> Result<u8> {
        self.bytes
            .get(self.offset)
            .copied()
            .context("unexpected end of bencode data")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_year_and_cleans_release_name() {
        assert_eq!(release_year("Film.Name.2024.1080p.WEB-DL"), Some(2024));
        assert_eq!(
            clean_movie_file_name(Path::new("Film.Name.2024.1080p.WEB-DL.x265.mkv")),
            "Film Name 2024.mkv"
        );
    }

    #[test]
    fn parses_utorrent_dictionary_values() {
        let bytes = b"d7:caption4:Film12:completed_oni123e4:path3:C:\\4:sizei10e10:downloadedi10ee";
        let mut reader = BencodeReader::new(bytes);
        let BencodeValue::Dictionary(values) = reader.read_value().expect("parse dictionary")
        else {
            panic!("expected dictionary")
        };
        assert_eq!(value_text(&values, b"caption"), "Film");
        assert_eq!(value_integer(&values, b"completed_on"), 123);
    }
}
