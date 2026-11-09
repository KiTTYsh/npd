//! Play queue handoff through the server's saved play queue, so playback
//! continues across devices: pause here, resume on the phone (or another
//! npd) where you left off, and the other way round.
//!
//! npd saves the queue when playback pauses or stops, and on shutdown. When
//! playback resumes, and at startup, it checks the server's queue; if it
//! differs from what npd last saved, another device changed it, and npd
//! takes it over. The server's `changedBy` names only the client ("npd" on
//! every machine), so changes are told apart by comparing contents instead.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result};
use md5::{Digest, Md5};
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info, warn};

use crate::events::{EventBus, Subsystem};
use crate::library::Library;
use crate::player::{Player, State};
use crate::subsonic::Client;
use crate::subsonic::model::PlayQueue;

/// How long a resume may wait on the server before playing what's here.
const CHECK_TIMEOUT: Duration = Duration::from_secs(3);

pub struct Handoff {
    client: Arc<Client>,
    player: Arc<Player>,
    library: Arc<Library>,
    /// Holds the fingerprint of the queue npd last saved or adopted.
    path: PathBuf,
    known: Mutex<Option<String>>,
    busy: tokio::sync::Mutex<()>,
}

/// Identifies a queue's contents: songs, current song and position.
fn fingerprint(ids: impl IntoIterator<Item = impl AsRef<str>>, current: Option<&str>, position_ms: u64) -> String {
    let mut f = String::new();
    for id in ids {
        f.push_str(id.as_ref());
        f.push(',');
    }
    f.push_str(&format!("|{}|{position_ms}", current.unwrap_or("")));
    hex::encode(Md5::digest(f.as_bytes()))
}

fn server_fingerprint(q: &PlayQueue) -> String {
    let position = if q.current.is_some() { q.position.unwrap_or(0) } else { 0 };
    fingerprint(q.entry.iter().map(|s| &s.id), q.current.as_deref(), position)
}

impl Handoff {
    pub fn new(client: Arc<Client>, player: Arc<Player>, library: Arc<Library>, state_dir: &std::path::Path) -> Arc<Self> {
        let path = state_dir.join("handoff");
        let known = std::fs::read_to_string(&path).ok().map(|s| s.trim().to_owned());
        Arc::new(Self { client, player, library, path, known: Mutex::new(known), busy: Default::default() })
    }

    fn remember(&self, fp: String) {
        if let Err(e) = std::fs::write(&self.path, &fp) {
            warn!("writing {}: {e}", self.path.display());
        }
        *self.known.lock().unwrap() = Some(fp);
    }

    /// Saves the queue to the server.
    pub async fn save(&self) -> Result<()> {
        let _busy = self.busy.lock().await;
        let snap = self.player.snapshot();
        let current = snap.current.and_then(|p| snap.songs.get(p)).cloned();
        let position_ms = current.as_ref().map_or(0, |_| (snap.elapsed.unwrap_or(0.0) * 1000.0) as u64);
        let fp = fingerprint(&snap.songs, current.as_deref(), position_ms);
        if self.known.lock().unwrap().as_ref() == Some(&fp) {
            return Ok(());
        }
        self.client.save_play_queue(&snap.songs, current.as_deref(), position_ms).await?;
        debug!("saved play queue ({} songs)", snap.songs.len());
        self.remember(fp);
        Ok(())
    }

    /// Takes over the server's queue if another device changed it. Returns
    /// whether it did.
    pub async fn adopt_if_changed(&self) -> Result<bool> {
        let _busy = self.busy.lock().await;
        let q = self.client.play_queue().await?;
        let fp = server_fingerprint(&q);
        let known = self.known.lock().unwrap().clone();
        match known {
            Some(k) if k == fp => return Ok(false),
            // First run: don't replace the local queue with whatever some
            // client saved long ago; just start tracking.
            None => {
                self.remember(fp);
                return Ok(false);
            }
            Some(_) => {}
        }
        let songs = self.library.resolve(Arc::new(q.entry)).await?;
        let current = q.current.as_ref().and_then(|c| songs.iter().position(|s| &s.id == c));
        let elapsed = q.position.unwrap_or(0) as f64 / 1000.0;
        info!(
            "taking over play queue from {} ({} songs)",
            q.changed_by.as_deref().unwrap_or("another device"),
            songs.len()
        );
        self.player
            .replace_queue(songs.into_iter().map(Arc::new).collect(), current, elapsed)
            .context("loading handed-off queue")?;
        self.remember(fp);
        Ok(true)
    }

    /// Before resuming playback: adopt a newer queue, without holding up
    /// playback for long if the server is slow or unreachable.
    pub async fn before_resume(&self) {
        match tokio::time::timeout(CHECK_TIMEOUT, self.adopt_if_changed()).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => warn!("checking play queue: {e:#}"),
            Err(_) => warn!("checking play queue: timed out"),
        }
    }

    /// Saves whenever playback pauses or stops.
    pub fn spawn_saver(self: &Arc<Self>, events: &EventBus) {
        let mut rx = events.subscribe();
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let mut was = this.player.state();
            loop {
                match rx.recv().await {
                    Ok(Subsystem::Player) | Err(RecvError::Lagged(_)) => {}
                    Ok(_) => continue,
                    Err(RecvError::Closed) => return,
                }
                let now = this.player.state();
                if was == State::Play && now != State::Play
                    && let Err(e) = this.save().await
                {
                    warn!("saving play queue: {e:#}");
                }
                was = now;
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::model::Song;

    #[test]
    fn fingerprints_match_server_form() {
        let q = PlayQueue {
            entry: vec![Song { id: "a".into(), ..Default::default() }, Song { id: "b".into(), ..Default::default() }],
            current: Some("b".into()),
            position: Some(1500),
            changed_by: None,
        };
        assert_eq!(server_fingerprint(&q), fingerprint(["a", "b"], Some("b"), 1500));
        assert_ne!(server_fingerprint(&q), fingerprint(["a", "b"], Some("b"), 1501));
    }
}
