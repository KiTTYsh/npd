//! Audio cache: songs downloaded ahead of playback, so playback survives a
//! flaky connection and favorites can be kept offline.
//!
//! One worker downloads one song at a time, most urgent first: the current
//! song and the next `prefetch` in play order, then starred songs when
//! favorites are pinned. Pinned files are never evicted; the rest are capped
//! at `size` bytes and evicted least recently used. Playback picks up cached
//! files wherever they exist, whatever the mode.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tokio::sync::Notify;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::events::{EventBus, Subsystem};
use crate::player::Player;
use crate::playlists::Playlists;
use crate::subsonic::{Client, Profile};

/// How long to leave a song alone after its download failed.
const RETRY_AFTER: Duration = Duration::from_secs(600);

pub struct CacheConfig {
    pub dir: PathBuf,
    /// Byte cap for unpinned files; 0 disables the cache.
    pub size: u64,
    pub prefetch: usize,
    pub pin_favorites: bool,
    /// Songs available here aren't downloaded.
    pub local_root: Option<PathBuf>,
}

struct Entry {
    size: u64,
    used: SystemTime,
}

pub struct AudioCache {
    client: Arc<Client>,
    profile: Profile,
    config: CacheConfig,
    /// Cached songs of the current profile, by song ID.
    entries: Mutex<HashMap<String, Entry>>,
    failed: Mutex<HashMap<String, Instant>>,
    wake: Notify,
}

impl AudioCache {
    pub fn open(client: Arc<Client>, profile: Profile, config: CacheConfig) -> anyhow::Result<Arc<Self>> {
        std::fs::create_dir_all(&config.dir)?;
        let suffix = format!(".{}", profile.tag());
        let mut entries = HashMap::new();
        for e in std::fs::read_dir(&config.dir)?.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Ok(meta) = e.metadata() else { continue };
            match name.strip_suffix(&suffix) {
                Some(id) if meta.is_file() => {
                    let used = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                    entries.insert(id.to_owned(), Entry { size: meta.len(), used });
                }
                // Leftovers from an interrupted download.
                _ if name.ends_with(".part") => {
                    let _ = std::fs::remove_file(e.path());
                }
                _ => {}
            }
        }
        let bytes: u64 = entries.values().map(|e| e.size).sum();
        info!("audio cache: {} songs, {} MiB", entries.len(), bytes >> 20);
        Ok(Arc::new(Self {
            client,
            profile,
            config,
            entries: Mutex::new(entries),
            failed: Mutex::new(HashMap::new()),
            wake: Notify::new(),
        }))
    }

    fn path(&self, id: &str) -> PathBuf {
        self.config.dir.join(format!("{id}.{}", self.profile.tag()))
    }

    /// The cached file for a song, if any. Marks it recently used.
    pub fn lookup(&self, id: &str) -> Option<PathBuf> {
        let mut entries = self.entries.lock().unwrap();
        let entry = entries.get_mut(id)?;
        let path = self.path(id);
        if !path.is_file() {
            entries.remove(id);
            return None;
        }
        entry.used = SystemTime::now();
        if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(entry.used);
        }
        Some(path)
    }

    fn is_local(&self, uri: &str) -> bool {
        self.config.local_root.as_ref().is_some_and(|root| root.join(uri).is_file())
    }

    /// Starts the download worker, woken by queue, playback and star
    /// changes.
    pub fn spawn(self: &Arc<Self>, player: Arc<Player>, playlists: Arc<Playlists>, events: &EventBus) {
        let mut rx = events.subscribe();
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(Subsystem::Playlist | Subsystem::Player | Subsystem::Options | Subsystem::StoredPlaylist)
                    | Err(RecvError::Lagged(_)) => this.wake.notify_one(),
                    Ok(_) => {}
                    Err(RecvError::Closed) => return,
                }
            }
        });
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                match this.next_wanted(&player, &playlists).await {
                    Some((id, protected)) => this.fetch(&id, &protected).await,
                    None => this.wake.notified().await,
                }
            }
        });
        self.wake.notify_one();
    }

    /// The most urgent song to download, plus the songs eviction must
    /// spare (the prefetch window and pins).
    async fn next_wanted(&self, player: &Player, playlists: &Playlists) -> Option<(String, HashSet<String>)> {
        let pinned = if self.config.pin_favorites {
            match playlists.starred().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("audio cache: favorites unavailable: {e}");
                    Default::default()
                }
            }
        } else {
            Default::default()
        };
        // Songs that will play from the local mount needn't be prefetched,
        // but favorites are still pinned: that's how they get to devices
        // that leave home.
        let window: Vec<String> = if self.config.prefetch > 0 {
            player
                .upcoming(1 + self.config.prefetch)
                .into_iter()
                .filter(|s| !self.is_local(&s.uri))
                .map(|s| s.id.clone())
                .collect()
        } else {
            Vec::new()
        };
        let wanted: Vec<String> = window.into_iter().chain(pinned.iter().map(|s| s.id.clone())).collect();
        let protected: HashSet<String> = wanted.iter().cloned().collect();

        let entries = self.entries.lock().unwrap();
        let mut failed = self.failed.lock().unwrap();
        failed.retain(|_, at| at.elapsed() < RETRY_AFTER);
        let pick = wanted.into_iter().find(|id| !entries.contains_key(id) && !failed.contains_key(id))?;
        Some((pick, protected))
    }

    async fn fetch(&self, id: &str, protected: &HashSet<String>) {
        let path = self.path(id);
        let part = path.with_extension("part");
        let started = Instant::now();
        match self.client.download_to(id, &self.profile, &part).await {
            Ok(size) => {
                if let Err(e) = std::fs::rename(&part, &path) {
                    warn!("audio cache: {e}");
                    return;
                }
                debug!("cached {id} ({} KiB in {:.1}s)", size >> 10, started.elapsed().as_secs_f64());
                self.entries.lock().unwrap().insert(id.to_owned(), Entry { size, used: SystemTime::now() });
                self.evict(protected);
            }
            Err(e) => {
                warn!("audio cache: downloading {id}: {e}");
                let _ = std::fs::remove_file(&part);
                self.failed.lock().unwrap().insert(id.to_owned(), Instant::now());
            }
        }
    }

    /// Evicts unprotected files, least recently used first, until they fit
    /// the cap.
    fn evict(&self, protected: &HashSet<String>) {
        let mut entries = self.entries.lock().unwrap();
        let mut victims: Vec<(String, u64, SystemTime)> = entries
            .iter()
            .filter(|(id, _)| !protected.contains(*id))
            .map(|(id, e)| (id.clone(), e.size, e.used))
            .collect();
        let mut used: u64 = victims.iter().map(|v| v.1).sum();
        if used <= self.config.size {
            return;
        }
        victims.sort_by_key(|v| v.2);
        for (id, size, _) in victims {
            if used <= self.config.size {
                break;
            }
            if remove(&self.path(&id)) {
                entries.remove(&id);
                used -= size;
                debug!("evicted {id}");
            }
        }
    }
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(e) => {
            warn!("audio cache: removing {}: {e}", path.display());
            false
        }
    }
}
