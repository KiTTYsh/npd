//! Stored playlists, written through to the server, plus a virtual
//! playlist of starred songs.
//!
//! Playlist metadata and starred songs are cached and refreshed on an
//! interval (and after every write), so changes made in other clients show
//! up as `stored_playlist` events. Entries are fetched on demand and cached
//! until the playlist's `changed` timestamp moves.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::warn;

use crate::events::{EventBus, Subsystem};
use crate::subsonic::model::Song;
use crate::subsonic::{self, Client};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("No such playlist")]
    NoExist,
    #[error("Playlist already exists")]
    Exists,
    #[error("{0} is read-only")]
    ReadOnly(String),
    #[error("Bad song index")]
    BadIndex,
    #[error(transparent)]
    Server(#[from] subsonic::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A server playlist under its MPD name. Names are unique in MPD but not on
/// the server; duplicates get a ` (2)`-style suffix.
#[derive(Debug, Clone, PartialEq)]
struct Listed {
    id: String,
    name: String,
    changed: String,
    readonly: bool,
}

#[derive(Default)]
struct State {
    loaded: bool,
    lists: Vec<Listed>,
    /// Entries by playlist ID, with the `changed` they were fetched at.
    entries: HashMap<String, (String, Arc<Vec<Song>>)>,
    starred: Arc<Vec<Song>>,
    starred_ids: HashSet<String>,
}

/// How `save` treats an existing playlist.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SaveMode {
    Create,
    Append,
    Replace,
}

pub struct Playlists {
    client: Arc<Client>,
    events: EventBus,
    /// Name of the starred-songs playlist, if enabled.
    favorites: Option<String>,
    state: Mutex<State>,
    /// Serialises writes (several are read-modify-write) and refreshes.
    writing: tokio::sync::Mutex<()>,
}

/// What a playlist name refers to.
enum Target {
    Favorites,
    List(Listed),
}

impl Playlists {
    pub fn new(client: Arc<Client>, events: EventBus, favorites: &str) -> Arc<Self> {
        Arc::new(Self {
            client,
            events,
            favorites: (!favorites.is_empty()).then(|| favorites.to_owned()),
            state: Mutex::new(State::default()),
            writing: tokio::sync::Mutex::new(()),
        })
    }

    /// Refreshes now and then every `interval` (if nonzero).
    pub fn start_polling(self: &Arc<Self>, interval: Duration) {
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let refreshed = {
                    let _guard = this.writing.lock().await;
                    this.refresh().await
                };
                if let Err(e) = refreshed {
                    warn!("refreshing playlists: {e}");
                }
                if interval.is_zero() {
                    break;
                }
                tokio::time::sleep(interval).await;
            }
        });
    }

    /// Fetches playlist metadata and starred songs, and announces changes.
    /// Callers hold `writing`.
    async fn refresh(&self) -> Result<()> {
        let (lists, starred) = tokio::try_join!(self.client.playlists(), self.client.starred_songs())?;

        let mut used: HashSet<String> = self.favorites.iter().cloned().collect();
        let lists: Vec<Listed> = lists
            .into_iter()
            .map(|p| {
                let base = p.name.replace(['\n', '\r'], " ");
                let mut name = base.clone();
                let mut n = 2;
                while !used.insert(name.clone()) {
                    name = format!("{base} ({n})");
                    n += 1;
                }
                Listed { id: p.id, name, changed: p.changed, readonly: p.readonly }
            })
            .collect();
        let starred_ids: HashSet<String> = starred.iter().map(|s| s.id.clone()).collect();

        let mut state = self.state.lock().unwrap();
        let first = !state.loaded;
        let lists_changed = state.lists != lists;
        let order_changed = state.starred.iter().map(|s| &s.id).ne(starred.iter().map(|s| &s.id));
        let stars_changed = state.starred_ids != starred_ids;
        state
            .entries
            .retain(|id, (changed, _)| lists.iter().any(|l| &l.id == id && &l.changed == changed));
        state.lists = lists;
        state.starred = Arc::new(starred);
        state.starred_ids = starred_ids;
        state.loaded = true;
        drop(state);

        if !first {
            if lists_changed || (order_changed && self.favorites.is_some()) {
                self.events.emit(Subsystem::StoredPlaylist);
            }
            if stars_changed {
                self.events.emit(Subsystem::Sticker);
            }
        }
        Ok(())
    }

    async fn ensure_loaded(&self) -> Result<()> {
        if self.state.lock().unwrap().loaded {
            return Ok(());
        }
        let _guard = self.writing.lock().await;
        if self.state.lock().unwrap().loaded {
            return Ok(());
        }
        self.refresh().await
    }

    fn target(&self, name: &str) -> Option<Target> {
        if self.favorites.as_deref() == Some(name) {
            return Some(Target::Favorites);
        }
        let state = self.state.lock().unwrap();
        state.lists.iter().find(|l| l.name == name).cloned().map(Target::List)
    }

    fn writable(&self, name: &str) -> Result<Listed> {
        match self.target(name) {
            None => Err(Error::NoExist),
            Some(Target::Favorites) => Err(Error::ReadOnly(name.to_owned())),
            Some(Target::List(l)) if l.readonly => Err(Error::ReadOnly(name.to_owned())),
            Some(Target::List(l)) => Ok(l),
        }
    }

    /// Playlist names with their last-modified time (ISO 8601).
    pub async fn list(&self) -> Result<Vec<(String, String)>> {
        self.ensure_loaded().await?;
        let state = self.state.lock().unwrap();
        let mut out = Vec::new();
        if let Some(name) = &self.favorites {
            let changed = state.starred.first().and_then(|s| s.starred.clone()).unwrap_or_default();
            out.push((name.clone(), changed));
        }
        out.extend(state.lists.iter().map(|l| (l.name.clone(), l.changed.clone())));
        Ok(out)
    }

    /// A playlist's songs, in order.
    pub async fn songs(&self, name: &str) -> Result<Arc<Vec<Song>>> {
        self.ensure_loaded().await?;
        match self.target(name).ok_or(Error::NoExist)? {
            Target::Favorites => Ok(Arc::clone(&self.state.lock().unwrap().starred)),
            Target::List(l) => self.entries(&l).await,
        }
    }

    async fn entries(&self, list: &Listed) -> Result<Arc<Vec<Song>>> {
        if let Some((changed, songs)) = self.state.lock().unwrap().entries.get(&list.id)
            && *changed == list.changed
        {
            return Ok(Arc::clone(songs));
        }
        let playlist = self.client.playlist(&list.id).await?;
        let songs = Arc::new(playlist.entry);
        self.state
            .lock()
            .unwrap()
            .entries
            .insert(list.id.clone(), (playlist.changed, Arc::clone(&songs)));
        Ok(songs)
    }

    /// Whether a song is starred, as of the last refresh.
    pub async fn is_starred(&self, id: &str) -> Result<bool> {
        self.ensure_loaded().await?;
        Ok(self.state.lock().unwrap().starred_ids.contains(id))
    }

    /// Starred songs, most recent first.
    pub async fn starred(&self) -> Result<Arc<Vec<Song>>> {
        self.ensure_loaded().await?;
        Ok(Arc::clone(&self.state.lock().unwrap().starred))
    }

    /// Stars or unstars songs. Songs already in that state are skipped:
    /// starring again would reset their starred date and reorder favorites.
    pub async fn star(&self, ids: &[String], on: bool) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        self.set_starred(ids, on).await?;
        self.refresh().await
    }

    /// Callers hold `writing`.
    async fn set_starred(&self, ids: &[String], on: bool) -> Result<()> {
        let ids: Vec<String> = {
            let state = self.state.lock().unwrap();
            ids.iter().filter(|id| state.starred_ids.contains(*id) != on).cloned().collect()
        };
        if !ids.is_empty() {
            self.client.star(&ids, on).await?;
        }
        Ok(())
    }

    /// Saves songs (the queue) as a playlist.
    pub async fn save(&self, name: &str, ids: &[String], mode: SaveMode) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        match (self.target(name), mode) {
            (None, _) => self.client.create_playlist(Some(name), None, ids).await?,
            (Some(_), SaveMode::Create) => return Err(Error::Exists),
            (Some(_), SaveMode::Append) => {
                let l = self.writable(name)?;
                self.client.update_playlist(&l.id, None, ids, &[]).await?;
            }
            (Some(_), SaveMode::Replace) => {
                let l = self.writable(name)?;
                self.replace(&l, ids).await?;
            }
        }
        self.refresh().await
    }

    /// Inserts songs at `pos` (default: the end), creating the playlist if
    /// needed. On the favorites playlist this stars them.
    pub async fn add(&self, name: &str, ids: &[String], pos: Option<usize>) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        match self.target(name) {
            None if pos.is_some_and(|p| p > 0) => return Err(Error::BadIndex),
            None => self.client.create_playlist(Some(name), None, ids).await?,
            Some(Target::Favorites) => self.set_starred(ids, true).await?,
            Some(Target::List(_)) => {
                let l = self.writable(name)?;
                match pos {
                    None => self.client.update_playlist(&l.id, None, ids, &[]).await?,
                    Some(pos) => {
                        let mut all = song_ids(&*self.entries(&l).await?);
                        if pos > all.len() {
                            return Err(Error::BadIndex);
                        }
                        all.splice(pos..pos, ids.iter().cloned());
                        self.replace(&l, &all).await?;
                    }
                }
            }
        }
        self.refresh().await
    }

    /// Removes songs by position. On the favorites playlist this unstars
    /// them.
    pub async fn delete(&self, name: &str, range: Range<usize>) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        match self.target(name).ok_or(Error::NoExist)? {
            Target::Favorites => {
                let starred = Arc::clone(&self.state.lock().unwrap().starred);
                let ids = song_ids(starred.get(range).ok_or(Error::BadIndex)?);
                self.set_starred(&ids, false).await?;
            }
            Target::List(_) => {
                let l = self.writable(name)?;
                let len = self.entries(&l).await?.len();
                if range.end > len || range.start >= range.end {
                    return Err(Error::BadIndex);
                }
                let positions: Vec<usize> = range.collect();
                self.client.update_playlist(&l.id, None, &[], &positions).await?;
            }
        }
        self.refresh().await
    }

    /// Moves the songs in `from` so the first lands at `to`. The server has
    /// no move, so this rewrites the playlist.
    pub async fn move_range(&self, name: &str, from: Range<usize>, to: usize) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        let l = self.writable(name)?;
        let mut all = song_ids(&*self.entries(&l).await?);
        if from.end > all.len() || from.start >= from.end || to + from.len() > all.len() {
            return Err(Error::BadIndex);
        }
        let moved: Vec<String> = all.drain(from).collect();
        all.splice(to..to, moved);
        self.replace(&l, &all).await?;
        self.refresh().await
    }

    pub async fn clear(&self, name: &str) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        let l = self.writable(name)?;
        self.replace(&l, &[]).await?;
        self.refresh().await
    }

    /// Replaces a playlist's songs.
    async fn replace(&self, list: &Listed, ids: &[String]) -> Result<()> {
        if !ids.is_empty() {
            return Ok(self.client.create_playlist(None, Some(&list.id), ids).await?);
        }
        // The server ignores `createPlaylist` without songs; remove them all.
        let len = self.entries(list).await?.len();
        if len > 0 {
            let all: Vec<usize> = (0..len).collect();
            self.client.update_playlist(&list.id, None, &[], &all).await?;
        }
        Ok(())
    }

    pub async fn rename(&self, from: &str, to: &str) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        let l = self.writable(from)?;
        if self.target(to).is_some() {
            return Err(Error::Exists);
        }
        self.client.update_playlist(&l.id, Some(to), &[], &[]).await?;
        self.refresh().await
    }

    pub async fn remove(&self, name: &str) -> Result<()> {
        self.ensure_loaded().await?;
        let _guard = self.writing.lock().await;
        let l = self.writable(name)?;
        self.client.delete_playlist(&l.id).await?;
        self.refresh().await
    }
}

fn song_ids(songs: &[Song]) -> Vec<String> {
    songs.iter().map(|s| s.id.clone()).collect()
}
