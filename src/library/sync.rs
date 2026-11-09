//! Full library sync from the server into a fresh SQLite file.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::tags;
use crate::subsonic::Client;
use crate::subsonic::model::{Album, ArtistRef, Song};

const PAGE_SIZE: usize = 500;

pub const SCHEMA: &str = "
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);

-- Every directory implied by a song URI; the root is ''.
CREATE TABLE dirs (path TEXT PRIMARY KEY, parent TEXT NOT NULL);
CREATE INDEX dirs_parent ON dirs(parent);

CREATE TABLE songs (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    uri TEXT NOT NULL UNIQUE,
    dir TEXT NOT NULL,
    album_id TEXT,
    cover_art TEXT,
    duration INTEGER,
    bitrate INTEGER,
    sample_rate INTEGER,
    bit_depth INTEGER,
    channels INTEGER,
    suffix TEXT,
    content_type TEXT,
    size INTEGER,
    added TEXT,
    starred TEXT,
    rating INTEGER,
    rg_track_gain REAL,
    rg_track_peak REAL,
    rg_album_gain REAL,
    rg_album_peak REAL,
    -- All tags, encoded by `tags::encode`, for printing the song.
    tags BLOB NOT NULL
);
CREATE INDEX songs_dir ON songs(dir);

-- Lookup side of `songs.tags`, for `find` and `list`. `tag` indexes
-- `tags::ALL`.
CREATE TABLE tags (
    tag INTEGER NOT NULL,
    value TEXT NOT NULL,
    song INTEGER NOT NULL REFERENCES songs(rowid),
    PRIMARY KEY (tag, value, song)
) WITHOUT ROWID;
";

#[derive(Debug)]
pub struct SyncReport {
    pub songs: usize,
    pub albums: usize,
    pub uri_collisions: usize,
}

/// Downloads the whole library into a new database at `path`, replacing any
/// file already there.
pub async fn full_sync(client: &Client, path: &Path) -> Result<SyncReport> {
    let started = Instant::now();
    let expected = match client.scan_status().await {
        Ok(status) => status.count,
        Err(e) => {
            debug!("getScanStatus failed: {e}");
            None
        }
    };

    let mut albums = HashMap::new();
    for offset in (0..).step_by(PAGE_SIZE) {
        let page = client.albums_page(offset, PAGE_SIZE).await.context("fetching albums")?;
        let done = page.len() < PAGE_SIZE;
        albums.extend(page.into_iter().map(|a| (a.id.clone(), a)));
        if done {
            break;
        }
    }
    let album_count = albums.len();
    let artists: HashMap<String, ArtistRef> = client
        .artists()
        .await
        .context("fetching artists")?
        .into_iter()
        .filter_map(|a| Some((a.id.clone()?, a)))
        .collect();
    info!("fetched {album_count} albums, {} artists", artists.len());

    // Fetching and writing overlap: the writer runs on a blocking thread and
    // takes pages as they arrive.
    let (tx, rx) = mpsc::channel::<Vec<Song>>(4);
    let path_owned = path.to_owned();
    let writer = tokio::task::spawn_blocking(move || write_db(&path_owned, albums, artists, rx));

    let mut fetched = 0;
    let fetch_result: Result<()> = async {
        for offset in (0..).step_by(PAGE_SIZE) {
            let page = client.songs_page(offset, PAGE_SIZE).await.context("fetching songs")?;
            let done = page.len() < PAGE_SIZE;
            fetched += page.len();
            if fetched % 10_000 < PAGE_SIZE {
                info!("fetched {fetched} songs");
            }
            if tx.send(page).await.is_err() {
                break; // writer failed; its error is reported below
            }
            if done {
                break;
            }
        }
        Ok(())
    }
    .await;
    drop(tx);

    let written = writer.await.context("sync writer panicked")?;
    fetch_result?;
    let mut report = written?;
    report.albums = album_count;

    if let Some(expected) = expected
        && expected as usize != report.songs
    {
        warn!(
            "server reports {expected} songs but sync got {}; pagination may have shifted during a scan",
            report.songs
        );
    }
    info!(
        "synced {} songs, {} albums in {:.1}s",
        report.songs,
        report.albums,
        started.elapsed().as_secs_f64()
    );
    Ok(report)
}

fn write_db(
    path: &Path,
    albums: HashMap<String, Album>,
    artists: HashMap<String, ArtistRef>,
    mut rx: mpsc::Receiver<Vec<Song>>,
) -> Result<SyncReport> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e)?,
        _ => {}
    }
    let mut conn = Connection::open(path)?;
    conn.execute_batch("PRAGMA journal_mode = OFF; PRAGMA synchronous = OFF;")?;
    let tx = conn.transaction()?;
    tx.execute_batch(SCHEMA)?;

    let mut seen_ids = HashSet::new();
    let mut uris = HashSet::new();
    let mut dirs = HashSet::new();
    let mut collisions = 0;
    {
        let mut insert_song = tx.prepare(
            "INSERT INTO songs (id, uri, dir, album_id, cover_art, duration, bitrate,
                sample_rate, bit_depth, channels, suffix, content_type, size, added,
                starred, rating, rg_track_gain, rg_track_peak, rg_album_gain, rg_album_peak,
                tags)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                ?16, ?17, ?18, ?19, ?20, ?21)",
        )?;
        let mut insert_tag =
            tx.prepare("INSERT OR IGNORE INTO tags (tag, value, song) VALUES (?1, ?2, ?3)")?;

        while let Some(page) = rx.blocking_recv() {
            for mut song in page {
                if !seen_ids.insert(song.id.clone()) {
                    continue; // pages overlapped
                }
                let mut uri = tags::song_uri(&song);
                if !uris.insert(uri.clone()) {
                    collisions += 1;
                    uri = tags::disambiguate(&uri, &song.id);
                    uris.insert(uri.clone());
                }
                let dir = tags::parent_dir(&uri).to_owned();
                // Song entries carry only artist IDs and names.
                for artist in song.artists.iter_mut().chain(&mut song.album_artists) {
                    if let Some(full) = artist.id.as_ref().and_then(|id| artists.get(id)) {
                        artist.sort_name = full.sort_name.clone();
                        artist.music_brainz_id = full.music_brainz_id.clone();
                    }
                }
                let album = song.album_id.as_ref().and_then(|id| albums.get(id));
                let song_tags = tags::song_tags(&song, album);
                let rg = song.replay_gain.clone().unwrap_or_default();
                insert_song.execute(params![
                    song.id,
                    uri,
                    dir,
                    song.album_id,
                    song.cover_art,
                    song.duration,
                    song.bit_rate,
                    song.sampling_rate,
                    song.bit_depth,
                    song.channel_count,
                    song.suffix,
                    song.content_type,
                    song.size.map(|s| s as i64),
                    song.created,
                    song.starred,
                    song.user_rating,
                    rg.track_gain,
                    rg.track_peak,
                    rg.album_gain,
                    rg.album_peak,
                    tags::encode(&song_tags),
                ])?;
                let rowid = tx.last_insert_rowid();
                for (tag, value) in song_tags {
                    insert_tag.execute(params![tags::id(tag), value, rowid])?;
                }
                dirs.insert(dir);
            }
        }

        // Real paths carry the server's mount point (e.g. `music/`); strip
        // directories every song shares so URIs start at the library root.
        let root = tags::common_dir(dirs.iter().map(String::as_str));
        if !root.is_empty() {
            debug!("stripping common prefix {root:?}");
            tx.execute(
                "UPDATE songs SET
                    uri = substr(uri, length(?1) + 2),
                    dir = CASE WHEN dir = ?1 THEN '' ELSE substr(dir, length(?1) + 2) END",
                params![root],
            )?;
            dirs = dirs.iter().map(|d| tags::strip_dir(d, &root).to_owned()).collect();
        }

        // Add every ancestor of every song directory.
        let mut all_dirs = HashSet::new();
        for dir in dirs {
            let mut d = dir.as_str();
            while !d.is_empty() && all_dirs.insert(d.to_owned()) {
                d = tags::parent_dir(d);
            }
        }
        let mut insert_dir = tx.prepare("INSERT INTO dirs (path, parent) VALUES (?1, ?2)")?;
        for dir in &all_dirs {
            insert_dir.execute(params![dir, tags::parent_dir(dir)])?;
        }

        tx.execute("INSERT INTO meta (key, value) VALUES ('root', ?1)", params![root])?;
        let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        tx.execute(
            "INSERT INTO meta (key, value) VALUES ('synced_at', ?1)",
            params![now.to_string()],
        )?;
    }
    if collisions > 0 {
        warn!(
            "{collisions} songs had duplicate paths and were renamed; \
             enable \"Report Real Path\" for the npd player in Navidrome"
        );
    }
    tx.commit()?;
    conn.execute_batch("ANALYZE;")?;
    Ok(SyncReport {
        songs: seen_ids.len(),
        albums: 0,
        uri_collisions: collisions,
    })
}
