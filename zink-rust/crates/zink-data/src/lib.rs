use std::{
    fs, io,
    path::{Path, PathBuf},
};

use directories::ProjectDirs;
use lofty::{
    file::{AudioFile, TaggedFileExt},
    prelude::Accessor,
    probe::Probe,
};
use rusqlite::{Connection, params};
use thiserror::Error;
use zink_core::AppSettings;

#[derive(Debug, Error)]
pub enum DataError {
    #[error("Windows did not provide a Zink application data directory")]
    MissingDataDirectory,
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),
    #[error("invalid settings: {0}")]
    Json(#[from] serde_json::Error),
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
}

#[derive(Debug, Clone)]
pub struct DataPaths {
    pub root: PathBuf,
    pub settings: PathBuf,
    pub database: PathBuf,
    pub logs: PathBuf,
}

impl DataPaths {
    /// Locates Zink's per-user local data directory.
    ///
    /// # Errors
    ///
    /// Returns an error when the operating system has no usable data directory.
    pub fn discover() -> Result<Self, DataError> {
        let dirs =
            ProjectDirs::from("com", "Zink", "Zink").ok_or(DataError::MissingDataDirectory)?;
        let root = dirs.data_local_dir().to_path_buf();
        Ok(Self {
            settings: root.join("settings-v1.json"),
            database: root.join("zink-v1.sqlite3"),
            logs: root.join("Logs"),
            root,
        })
    }

    /// Creates the application data and logging directories.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when a directory cannot be created.
    pub fn ensure_exists(&self) -> Result<(), DataError> {
        fs::create_dir_all(&self.root)?;
        fs::create_dir_all(&self.logs)?;
        Ok(())
    }
}

/// Loads versioned settings, returning defaults when no settings file exists.
///
/// # Errors
///
/// Returns an error when the file cannot be read or contains invalid JSON.
pub fn load_settings(path: &Path) -> Result<AppSettings, DataError> {
    match fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(AppSettings::default()),
        Err(error) => Err(error.into()),
    }
}

/// Saves settings using a temporary file before replacing the prior file.
///
/// # Errors
///
/// Returns an error when serialization or file replacement fails.
pub fn save_settings(path: &Path, settings: &AppSettings) -> Result<(), DataError> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "settings path has no parent")
    })?;
    fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    fs::write(&temporary, serde_json::to_vec_pretty(settings)?)?;
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(temporary, path)?;
    Ok(())
}

/// Opens the `SQLite` database and applies the foundation schema.
///
/// # Errors
///
/// Returns an error when `SQLite` cannot open or initialize the database.
pub fn open_database(path: &Path) -> Result<Connection, DataError> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS schema_migrations (
             version INTEGER PRIMARY KEY,
             applied_utc TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS media_items (
             id INTEGER PRIMARY KEY,
             path TEXT NOT NULL UNIQUE,
             title TEXT NOT NULL,
             artist TEXT NOT NULL DEFAULT '',
             album TEXT NOT NULL DEFAULT '',
             media_kind TEXT NOT NULL CHECK(media_kind IN ('audio', 'video')),
             duration_ms INTEGER NOT NULL DEFAULT 0,
             modified_unix INTEGER,
             imported_unix INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE IF NOT EXISTS playlists (
             id INTEGER PRIMARY KEY,
             name TEXT NOT NULL UNIQUE,
             artwork_path TEXT,
             created_unix INTEGER NOT NULL DEFAULT (unixepoch()),
             modified_unix INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE IF NOT EXISTS playlist_items (
             playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
             media_path TEXT NOT NULL,
             position INTEGER NOT NULL,
             added_unix INTEGER NOT NULL DEFAULT (unixepoch()),
             PRIMARY KEY (playlist_id, position)
         );
         CREATE TABLE IF NOT EXISTS play_history (
             id INTEGER PRIMARY KEY,
             media_path TEXT NOT NULL,
             played_unix INTEGER NOT NULL DEFAULT (unixepoch()),
             completed INTEGER NOT NULL DEFAULT 0 CHECK(completed IN (0, 1)),
             position_ms INTEGER NOT NULL DEFAULT 0
         );
         CREATE INDEX IF NOT EXISTS idx_play_history_path ON play_history(media_path);
         CREATE INDEX IF NOT EXISTS idx_play_history_recent ON play_history(played_unix DESC);
         CREATE TABLE IF NOT EXISTS track_statistics (
             media_path TEXT PRIMARY KEY,
             play_count INTEGER NOT NULL DEFAULT 0,
             skip_count INTEGER NOT NULL DEFAULT 0,
             listening_ms INTEGER NOT NULL DEFAULT 0,
             last_played_unix INTEGER
         );",
    )?;
    ensure_media_column(&connection, "artist", "TEXT NOT NULL DEFAULT ''")?;
    ensure_media_column(&connection, "album", "TEXT NOT NULL DEFAULT ''")?;
    ensure_media_column(&connection, "duration_ms", "INTEGER NOT NULL DEFAULT 0")?;
    Ok(connection)
}

/// Records a playback event and updates durable listening statistics.
///
/// # Errors
///
/// Returns a database error when the history transaction cannot be committed.
pub fn record_playback(
    connection: &mut Connection,
    path: &Path,
    position_ms: u64,
    completed: bool,
) -> Result<(), DataError> {
    let transaction = connection.transaction()?;
    let path = path.to_string_lossy();
    transaction.execute(
        "INSERT INTO play_history(media_path, completed, position_ms) VALUES (?1, ?2, ?3)",
        params![
            path,
            i64::from(completed),
            i64::try_from(position_ms).unwrap_or(i64::MAX)
        ],
    )?;
    transaction.execute(
        "INSERT INTO track_statistics(media_path, play_count, listening_ms, last_played_unix)
         VALUES (?1, 1, 0, unixepoch())
         ON CONFLICT(media_path) DO UPDATE SET
             play_count = play_count + 1,
             listening_ms = listening_ms + excluded.listening_ms,
             last_played_unix = unixepoch()",
        [path],
    )?;
    transaction.commit()?;
    Ok(())
}

/// Finalizes the latest playback event for a track and accumulates listening time.
///
/// # Errors
///
/// Returns a database error when the history/statistics update fails.
pub fn finish_playback(
    connection: &mut Connection,
    path: &Path,
    position_ms: u64,
    completed: bool,
    skipped: bool,
) -> Result<(), DataError> {
    let transaction = connection.transaction()?;
    let path = path.to_string_lossy();
    transaction.execute(
        "UPDATE play_history
         SET completed = ?1, position_ms = ?2
         WHERE id = (SELECT id FROM play_history WHERE media_path = ?3 ORDER BY id DESC LIMIT 1)",
        params![
            i64::from(completed),
            i64::try_from(position_ms).unwrap_or(i64::MAX),
            path
        ],
    )?;
    transaction.execute(
        "UPDATE track_statistics
         SET listening_ms = listening_ms + ?1,
             skip_count = skip_count + ?2,
             last_played_unix = unixepoch()
         WHERE media_path = ?3",
        params![
            i64::try_from(position_ms).unwrap_or(i64::MAX),
            i64::from(skipped),
            path
        ],
    )?;
    transaction.commit()?;
    Ok(())
}

/// Replaces a named playlist atomically with the supplied ordered paths.
///
/// # Errors
///
/// Returns a database error when the playlist transaction fails.
pub fn save_playlist(
    connection: &mut Connection,
    name: &str,
    paths: &[PathBuf],
) -> Result<(), DataError> {
    let transaction = connection.transaction()?;
    transaction.execute(
        "INSERT INTO playlists(name) VALUES (?1)
         ON CONFLICT(name) DO UPDATE SET modified_unix = unixepoch()",
        [name],
    )?;
    let playlist_id: i64 =
        transaction.query_row("SELECT id FROM playlists WHERE name = ?1", [name], |row| {
            row.get(0)
        })?;
    transaction.execute(
        "DELETE FROM playlist_items WHERE playlist_id = ?1",
        [playlist_id],
    )?;
    for (position, path) in paths.iter().enumerate() {
        transaction.execute(
            "INSERT INTO playlist_items(playlist_id, media_path, position) VALUES (?1, ?2, ?3)",
            params![
                playlist_id,
                path.to_string_lossy(),
                i64::try_from(position).unwrap_or(i64::MAX)
            ],
        )?;
    }
    transaction.commit()?;
    Ok(())
}

/// Returns playlists ordered by their most recent modification.
///
/// # Errors
///
/// Returns a database error when playlist rows cannot be read.
pub fn list_playlists(connection: &Connection) -> Result<Vec<PlaylistSummary>, DataError> {
    let mut statement = connection.prepare(
        "SELECT p.name, COUNT(i.media_path), p.modified_unix
         FROM playlists p
         LEFT JOIN playlist_items i ON i.playlist_id = p.id
         GROUP BY p.id
         ORDER BY p.modified_unix DESC, p.name COLLATE NOCASE",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(PlaylistSummary {
            name: row.get(0)?,
            track_count: usize::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
            modified_unix: row.get(2)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(DataError::from)
}

/// Returns the ordered paths stored in a named playlist.
///
/// # Errors
///
/// Returns a database error when playlist items cannot be read.
pub fn load_playlist(connection: &Connection, name: &str) -> Result<Vec<PathBuf>, DataError> {
    let mut statement = connection.prepare(
        "SELECT i.media_path
         FROM playlist_items i
         JOIN playlists p ON p.id = i.playlist_id
         WHERE p.name = ?1
         ORDER BY i.position",
    )?;
    let rows = statement.query_map([name], |row| Ok(PathBuf::from(row.get::<_, String>(0)?)))?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(DataError::from)
}

fn ensure_media_column(
    connection: &Connection,
    column: &str,
    declaration: &str,
) -> rusqlite::Result<()> {
    let mut statement = connection.prepare("PRAGMA table_info(media_items)")?;
    let columns = statement.query_map([], |row| row.get::<_, String>(1))?;
    for existing in columns {
        if existing? == column {
            return Ok(());
        }
    }
    connection.execute(
        &format!("ALTER TABLE media_items ADD COLUMN {column} {declaration}"),
        [],
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImportSummary {
    pub audio_files: usize,
    pub video_files: usize,
    pub skipped_files: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaItem {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub media_kind: String,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaylistSummary {
    pub name: String,
    pub track_count: usize,
    pub modified_unix: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ListeningSummary {
    pub total_plays: u64,
    pub total_skips: u64,
    pub listening_ms: u64,
    pub unique_tracks: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrackStatistic {
    pub path: PathBuf,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub play_count: u64,
    pub skip_count: u64,
    pub listening_ms: u64,
    pub last_played_unix: Option<i64>,
}

impl ImportSummary {
    #[must_use]
    pub const fn imported_files(self) -> usize {
        self.audio_files + self.video_files
    }
}

/// Recursively indexes supported music and video files into Zink's database.
/// Existing paths are updated, so repeated imports are safe.
///
/// # Errors
///
/// Returns an error when the folder cannot be traversed or the database update
/// cannot be committed.
pub fn import_media_directory(
    connection: &mut Connection,
    root: &Path,
) -> Result<ImportSummary, DataError> {
    let transaction = connection.transaction()?;
    let mut summary = ImportSummary::default();

    for entry in walkdir::WalkDir::new(root).follow_links(false) {
        let entry = entry.map_err(|error| io::Error::other(error.to_string()))?;
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let Some(extension) = path.extension().and_then(|value| value.to_str()) else {
            summary.skipped_files += 1;
            continue;
        };
        let extension = extension.to_ascii_lowercase();
        let media_kind = if matches!(
            extension.as_str(),
            "mp3" | "m4a" | "aac" | "wav" | "flac" | "wma" | "ogg" | "opus"
        ) {
            summary.audio_files += 1;
            "audio"
        } else if matches!(
            extension.as_str(),
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "wmv" | "m4v"
        ) {
            summary.video_files += 1;
            "video"
        } else {
            summary.skipped_files += 1;
            continue;
        };

        let fallback_title = path
            .file_stem()
            .and_then(|value| value.to_str())
            .unwrap_or("Unknown media");
        let metadata = if media_kind == "audio" {
            read_audio_metadata(path, fallback_title)
        } else {
            LocalMetadata::fallback(fallback_title)
        };
        let modified = entry
            .metadata()
            .ok()
            .and_then(|metadata| metadata.modified().ok())
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .and_then(|duration| i64::try_from(duration.as_secs()).ok());

        transaction.execute(
            "INSERT INTO media_items(path, title, artist, album, media_kind, duration_ms, modified_unix)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(path) DO UPDATE SET
                 title = excluded.title,
                 artist = excluded.artist,
                 album = excluded.album,
                 media_kind = excluded.media_kind,
                 duration_ms = excluded.duration_ms,
                 modified_unix = excluded.modified_unix",
            params![
                path.to_string_lossy(),
                metadata.title,
                metadata.artist,
                metadata.album,
                media_kind,
                i64::try_from(metadata.duration_ms).unwrap_or(i64::MAX),
                modified
            ],
        )?;
    }

    transaction.commit()?;
    Ok(summary)
}

/// Removes indexed media rows whose files no longer exist.
///
/// # Errors
///
/// Returns a database error when the cleanup statement cannot be completed.
pub fn prune_missing_media(connection: &Connection) -> Result<usize, DataError> {
    let mut statement = connection.prepare("SELECT path FROM media_items")?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let missing = paths
        .into_iter()
        .filter(|path| !Path::new(path).is_file())
        .collect::<Vec<_>>();
    let transaction = connection.unchecked_transaction()?;
    for path in &missing {
        transaction.execute("DELETE FROM media_items WHERE path = ?1", params![path])?;
    }
    transaction.commit()?;
    Ok(missing.len())
}

/// Removes indexed media entries that belong to a library source folder.
/// The files themselves are never changed.
///
/// # Errors
///
/// Returns a database error when indexed paths cannot be queried or removed.
pub fn remove_media_directory(connection: &Connection, root: &Path) -> Result<usize, DataError> {
    let root = root.to_string_lossy().replace('/', "\\");
    let root = root.trim_end_matches('\\').to_ascii_lowercase();
    let mut statement = connection.prepare("SELECT path FROM media_items")?;
    let paths = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let matches = paths
        .into_iter()
        .filter(|path| {
            let path = path.replace('/', "\\").to_ascii_lowercase();
            path == root
                || path
                    .strip_prefix(&root)
                    .is_some_and(|tail| tail.starts_with('\\'))
        })
        .collect::<Vec<_>>();
    let transaction = connection.unchecked_transaction()?;
    for path in &matches {
        transaction.execute(
            "DELETE FROM playlist_items WHERE media_path = ?1",
            params![path],
        )?;
        transaction.execute(
            "DELETE FROM play_history WHERE media_path = ?1",
            params![path],
        )?;
        transaction.execute(
            "DELETE FROM track_statistics WHERE media_path = ?1",
            params![path],
        )?;
        transaction.execute("DELETE FROM media_items WHERE path = ?1", params![path])?;
    }
    transaction.commit()?;
    Ok(matches.len())
}

struct LocalMetadata {
    title: String,
    artist: String,
    album: String,
    duration_ms: u64,
}

impl LocalMetadata {
    fn fallback(title: &str) -> Self {
        Self {
            title: title.to_owned(),
            artist: String::new(),
            album: String::new(),
            duration_ms: 0,
        }
    }
}

fn read_audio_metadata(path: &Path, fallback_title: &str) -> LocalMetadata {
    let Ok(tagged) = Probe::open(path).and_then(Probe::read) else {
        return LocalMetadata::fallback(fallback_title);
    };
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let duration_ms = u64::try_from(tagged.properties().duration().as_millis()).unwrap_or(u64::MAX);
    LocalMetadata {
        title: tag
            .and_then(Accessor::title)
            .map_or_else(|| fallback_title.to_owned(), std::borrow::Cow::into_owned),
        artist: tag
            .and_then(Accessor::artist)
            .map_or_else(String::new, std::borrow::Cow::into_owned),
        album: tag
            .and_then(Accessor::album)
            .map_or_else(String::new, std::borrow::Cow::into_owned),
        duration_ms,
    }
}

/// Returns indexed media ordered by title, optionally restricted by kind.
///
/// # Errors
///
/// Returns an error when the database query cannot be prepared or read.
pub fn list_media(
    connection: &Connection,
    media_kind: Option<&str>,
) -> Result<Vec<MediaItem>, DataError> {
    let mut items = Vec::new();
    if let Some(kind) = media_kind {
        let mut statement = connection.prepare(
            "SELECT path, title, artist, album, media_kind, duration_ms FROM media_items
             WHERE media_kind = ?1 ORDER BY title COLLATE NOCASE, path",
        )?;
        let rows = statement.query_map([kind], media_item_from_row)?;
        for row in rows {
            items.push(row?);
        }
    } else {
        let mut statement = connection.prepare(
            "SELECT path, title, artist, album, media_kind, duration_ms FROM media_items
             ORDER BY title COLLATE NOCASE, path",
        )?;
        let rows = statement.query_map([], media_item_from_row)?;
        for row in rows {
            items.push(row?);
        }
    }
    Ok(items)
}

/// Returns the most recently played indexed tracks, newest first.
///
/// # Errors
///
/// Returns a database error when history rows cannot be read.
pub fn list_recently_played(
    connection: &Connection,
    limit: usize,
) -> Result<Vec<MediaItem>, DataError> {
    let mut statement = connection.prepare(
        "SELECT m.path, m.title, m.artist, m.album, m.media_kind, m.duration_ms
         FROM play_history h
         JOIN media_items m ON m.path = h.media_path
         GROUP BY m.path
         ORDER BY MAX(h.played_unix) DESC
         LIMIT ?1",
    )?;
    let rows = statement.query_map(
        [i64::try_from(limit).unwrap_or(i64::MAX)],
        media_item_from_row,
    )?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(DataError::from)
}

/// Returns aggregate listening counters from the durable statistics table.
///
/// # Errors
///
/// Returns a database error when the aggregate query cannot be read.
pub fn listening_summary(connection: &Connection) -> Result<ListeningSummary, DataError> {
    connection
        .query_row(
            "SELECT COALESCE(SUM(play_count), 0), COALESCE(SUM(skip_count), 0),
                    COALESCE(SUM(listening_ms), 0), COUNT(*)
             FROM track_statistics",
            [],
            |row| {
                Ok(ListeningSummary {
                    total_plays: u64::try_from(row.get::<_, i64>(0)?).unwrap_or_default(),
                    total_skips: u64::try_from(row.get::<_, i64>(1)?).unwrap_or_default(),
                    listening_ms: u64::try_from(row.get::<_, i64>(2)?).unwrap_or_default(),
                    unique_tracks: u64::try_from(row.get::<_, i64>(3)?).unwrap_or_default(),
                })
            },
        )
        .map_err(DataError::from)
}

/// Returns the most frequently played indexed tracks.
///
/// # Errors
///
/// Returns a database error when statistics rows cannot be read.
pub fn list_top_tracks(
    connection: &Connection,
    limit: usize,
) -> Result<Vec<TrackStatistic>, DataError> {
    list_track_statistics(
        connection,
        "s.play_count DESC, s.listening_ms DESC, m.title COLLATE NOCASE",
        limit,
    )
}

/// Returns previously played tracks that have gone longest without being heard.
///
/// # Errors
///
/// Returns a database error when statistics rows cannot be read.
pub fn list_rediscovery_tracks(
    connection: &Connection,
    limit: usize,
) -> Result<Vec<TrackStatistic>, DataError> {
    list_track_statistics(
        connection,
        "s.last_played_unix ASC, s.play_count DESC, m.title COLLATE NOCASE",
        limit,
    )
}

fn list_track_statistics(
    connection: &Connection,
    order_by: &str,
    limit: usize,
) -> Result<Vec<TrackStatistic>, DataError> {
    let sql = format!(
        "SELECT m.path, m.title, m.artist, m.album, s.play_count, s.skip_count,
                s.listening_ms, s.last_played_unix
         FROM track_statistics s
         JOIN media_items m ON m.path = s.media_path
         ORDER BY {order_by}
         LIMIT ?1"
    );
    let mut statement = connection.prepare(&sql)?;
    let rows = statement.query_map([i64::try_from(limit).unwrap_or(i64::MAX)], |row| {
        Ok(TrackStatistic {
            path: PathBuf::from(row.get::<_, String>(0)?),
            title: row.get(1)?,
            artist: row.get(2)?,
            album: row.get(3)?,
            play_count: u64::try_from(row.get::<_, i64>(4)?).unwrap_or_default(),
            skip_count: u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
            listening_ms: u64::try_from(row.get::<_, i64>(6)?).unwrap_or_default(),
            last_played_unix: row.get(7)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(DataError::from)
}

fn media_item_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<MediaItem> {
    Ok(MediaItem {
        path: PathBuf::from(row.get::<_, String>(0)?),
        title: row.get(1)?,
        artist: row.get(2)?,
        album: row.get(3)?,
        media_kind: row.get(4)?,
        duration_ms: u64::try_from(row.get::<_, i64>(5)?).unwrap_or_default(),
    })
}

/// Copy-first primitive for idempotent legacy migration. Parsing/import is
/// separate so a failed import can be retried without changing legacy data.
///
/// # Errors
///
/// Returns an I/O error if the destination cannot be created or copied.
pub fn copy_legacy_file(source: &Path, destination: &Path) -> Result<bool, DataError> {
    if !source.is_file() || destination.exists() {
        return Ok(false);
    }
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::copy(source, destination)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_import_is_repeatable() {
        let root =
            std::env::temp_dir().join(format!("zink-rust-media-import-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove prior test directory");
        }
        fs::create_dir_all(root.join("album")).expect("create test directory");
        fs::write(root.join("album/song.mp3"), []).expect("create audio file");
        fs::write(root.join("clip.mp4"), []).expect("create video file");
        fs::write(root.join("notes.txt"), []).expect("create ignored file");

        let mut connection = Connection::open_in_memory().expect("open memory database");
        connection
            .execute_batch(
                "CREATE TABLE media_items (
                    id INTEGER PRIMARY KEY,
                    path TEXT NOT NULL UNIQUE,
                    title TEXT NOT NULL,
                    artist TEXT NOT NULL DEFAULT '',
                    album TEXT NOT NULL DEFAULT '',
                    media_kind TEXT NOT NULL,
                    duration_ms INTEGER NOT NULL DEFAULT 0,
                    modified_unix INTEGER,
                    imported_unix INTEGER NOT NULL DEFAULT (unixepoch())
                );",
            )
            .expect("create media schema");

        let first = import_media_directory(&mut connection, &root).expect("first import");
        let second = import_media_directory(&mut connection, &root).expect("repeat import");
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM media_items", [], |row| row.get(0))
            .expect("count imported rows");

        assert_eq!(first.audio_files, 1);
        assert_eq!(first.video_files, 1);
        assert_eq!(second.imported_files(), 2);
        assert_eq!(rows, 2);
        fs::remove_file(root.join("clip.mp4")).expect("remove indexed video");
        assert_eq!(
            prune_missing_media(&connection).expect("prune missing media"),
            1
        );
        let remaining: i64 = connection
            .query_row("SELECT COUNT(*) FROM media_items", [], |row| row.get(0))
            .expect("count remaining rows");
        assert_eq!(remaining, 1);
        fs::remove_dir_all(root).expect("remove test directory");
    }

    #[test]
    fn playlist_and_history_round_trip() {
        let root =
            std::env::temp_dir().join(format!("zink-rust-playlist-history-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root).expect("remove prior database directory");
        }
        fs::create_dir_all(&root).expect("create database directory");
        let database_path = root.join("library.sqlite3");
        let mut connection = open_database(&database_path).expect("open test database");
        let song = root.join("song.mp3");
        fs::write(&song, []).expect("create song");
        import_media_directory(&mut connection, &root).expect("index song");

        save_playlist(&mut connection, "Favourites", std::slice::from_ref(&song))
            .expect("save playlist");
        record_playback(&mut connection, &song, 12_500, false).expect("record playback");
        finish_playback(&mut connection, &song, 42_000, false, true).expect("finish playback");

        assert_eq!(
            load_playlist(&connection, "Favourites").expect("load playlist"),
            vec![song.clone()]
        );
        let playlists = list_playlists(&connection).expect("list playlists");
        assert_eq!(playlists[0].name, "Favourites");
        assert_eq!(playlists[0].track_count, 1);
        let recent = list_recently_played(&connection, 10).expect("list recent history");
        assert_eq!(recent[0].path, song);
        let (plays, skips, listening): (i64, i64, i64) = connection
            .query_row(
                "SELECT play_count, skip_count, listening_ms FROM track_statistics",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read listening statistics");
        assert_eq!((plays, skips, listening), (1, 1, 42_000));
        let summary = listening_summary(&connection).expect("read listening summary");
        assert_eq!(summary.total_plays, 1);
        assert_eq!(summary.total_skips, 1);
        assert_eq!(summary.listening_ms, 42_000);
        assert_eq!(summary.unique_tracks, 1);
        assert_eq!(
            list_top_tracks(&connection, 10)
                .expect("list top tracks")
                .len(),
            1
        );
        assert_eq!(
            list_rediscovery_tracks(&connection, 10)
                .expect("list rediscovery tracks")
                .len(),
            1
        );

        drop(connection);
        fs::remove_dir_all(root).expect("remove database directory");
    }
}
