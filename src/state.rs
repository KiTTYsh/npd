//! Saving and restoring player state across restarts.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::sync::broadcast::error::RecvError;
use tracing::{info, warn};

use crate::events::{EventBus, Subsystem};
use crate::library::Library;
use crate::library::query;
use crate::player::{Player, SavedState};

/// Changes to these are worth saving.
const SAVED: [Subsystem; 5] = [
    Subsystem::Playlist,
    Subsystem::Player,
    Subsystem::Options,
    Subsystem::Mixer,
    Subsystem::Output,
];

/// Batches bursts of changes into one write.
const DEBOUNCE: Duration = Duration::from_secs(2);

pub fn path(state_dir: &Path) -> PathBuf {
    state_dir.join("state.json")
}

pub fn save(player: &Player, path: &Path) -> Result<()> {
    let json = serde_json::to_vec(&player.snapshot())?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

pub async fn restore(player: &Player, library: &Library, path: &Path) -> Result<()> {
    let saved: SavedState = match std::fs::read(path) {
        Ok(data) => serde_json::from_slice(&data).with_context(|| format!("parsing {}", path.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let ids = saved.songs.clone();
    let songs = library.read(move |conn| query::songs_by_ids(conn, &ids)).await?.unwrap_or_default();
    if songs.len() < saved.songs.len() {
        warn!("{} queued songs are no longer in the library", saved.songs.len() - songs.len());
    }
    info!("restoring queue of {} songs", songs.len());
    player.restore(saved, songs.into_iter().map(Arc::new).collect())?;
    Ok(())
}

/// Saves after changes, at most once per `DEBOUNCE`.
pub fn spawn_saver(player: Arc<Player>, events: &EventBus, path: PathBuf) {
    let mut rx = events.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(s) if !SAVED.contains(&s) => continue,
                Err(RecvError::Closed) => return,
                _ => {}
            }
            tokio::time::sleep(DEBOUNCE).await;
            while !rx.is_empty() {
                let _ = rx.try_recv();
            }
            if let Err(e) = save(&player, &path) {
                warn!("saving state: {e:#}");
            }
        }
    });
}
