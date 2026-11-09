//! Playback: owns the queue and drives a backend.
//!
//! The backend's playlist always mirrors `[current, next]`, so transitions
//! between songs are gapless. npd decides what "next" is (repeat, random,
//! single, consume); the backend only ever sees two entries.

pub mod backend;
pub mod mpv;

use std::ops::Range;
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::events::{EventBus, Subsystem};
use crate::library::query::SongRow;
use crate::queue::{ItemId, Queue, Single};
use crate::subsonic::Client;
use backend::{AudioInfo, Backend, BackendEvent, EntryId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Stop,
    Play,
    Pause,
}

impl State {
    pub fn name(self) -> &'static str {
        match self {
            Self::Stop => "stop",
            Self::Play => "play",
            Self::Pause => "pause",
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}")]
    BadArg(String),
    #[error("{0}")]
    NoExist(String),
    #[error("{0:#}")]
    Backend(#[from] anyhow::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

fn bad_index() -> Error {
    Error::BadArg("Bad song index".into())
}

/// A snapshot for `status`.
#[derive(Debug, Clone)]
pub struct Status {
    pub state: State,
    pub volume: u8,
    pub repeat: bool,
    pub random: bool,
    pub single: Single,
    pub consume: bool,
    pub version: u32,
    pub length: usize,
    pub current: Option<(usize, ItemId)>,
    pub next: Option<(usize, ItemId)>,
    pub elapsed: Option<f64>,
    pub duration: Option<u32>,
    pub audio: AudioInfo,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub id: usize,
    pub name: String,
    pub device: String,
    pub enabled: bool,
}

/// What survives a restart.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SavedState {
    /// Server song IDs, in queue order.
    pub songs: Vec<String>,
    pub current: Option<usize>,
    pub elapsed: Option<f64>,
    pub repeat: bool,
    pub random: bool,
    pub single: Single,
    pub consume: bool,
    pub volume: u8,
    pub device: String,
    pub replay_gain: String,
}

/// Device name meaning "follow the system default".
const DEFAULT_DEVICE: &str = "auto";

struct Inner {
    queue: Queue,
    state: State,
    /// Mirror of the backend playlist.
    loaded: Vec<(EntryId, ItemId)>,
    volume: u8,
    device: String,
    replay_gain: String,
    error: Option<String>,
}

pub struct Player {
    inner: Mutex<Inner>,
    backend: Box<dyn Backend>,
    client: Arc<Client>,
    events: EventBus,
}

impl Player {
    /// Creates the player and starts handling backend events. `make_backend`
    /// receives the sender for those events.
    pub fn start(
        client: Arc<Client>,
        events: EventBus,
        make_backend: impl FnOnce(mpsc::UnboundedSender<BackendEvent>) -> anyhow::Result<Box<dyn Backend>>,
    ) -> anyhow::Result<Arc<Self>> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let backend = make_backend(tx)?;
        backend.set_volume(100)?;
        let player = Arc::new(Self {
            inner: Mutex::new(Inner {
                queue: Queue::new(),
                state: State::Stop,
                loaded: Vec::new(),
                volume: 100,
                device: DEFAULT_DEVICE.into(),
                replay_gain: "off".into(),
                error: None,
            }),
            backend,
            client,
            events,
        });
        let weak = Arc::downgrade(&player);
        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                let Some(player) = weak.upgrade() else { return };
                player.handle_backend(event);
            }
        });
        Ok(player)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap()
    }

    fn emit(&self, subsystems: &[Subsystem]) {
        for &s in subsystems {
            self.events.emit(s);
        }
    }

    fn url(&self, song: &SongRow) -> String {
        self.client.stream_url(&song.id)
    }

    /// Brings the backend playlist in line with `[current, next]`. With
    /// `restart`, reloads the current song even if it is already loaded.
    fn sync(&self, inner: &mut Inner, restart: bool, start: Option<f64>) -> Result<()> {
        let current = match (inner.state, inner.queue.current_item()) {
            (State::Stop, _) | (_, None) => {
                if inner.state != State::Stop {
                    inner.state = State::Stop;
                }
                if !inner.loaded.is_empty() {
                    inner.loaded.clear();
                    self.backend.stop()?;
                }
                return Ok(());
            }
            (_, Some(item)) => (item.id, Arc::clone(&item.song)),
        };

        if restart || inner.loaded.first().map(|l| l.1) != Some(current.0) {
            self.backend.set_pause(inner.state == State::Pause)?;
            let entry = self.backend.load(&self.url(&current.1), true, start)?;
            inner.loaded = vec![(entry, current.0)];
            inner.error = None;
        }

        let next = inner.queue.next().and_then(|p| inner.queue.get(p)).map(|i| (i.id, Arc::clone(&i.song)));
        if inner.loaded.get(1).map(|l| l.1) != next.as_ref().map(|n| n.0) {
            while inner.loaded.len() > 1 {
                self.backend.remove(1)?;
                inner.loaded.remove(1);
            }
            if let Some((id, song)) = next {
                let entry = self.backend.load(&self.url(&song), false, None)?;
                inner.loaded.push((entry, id));
            }
        }
        Ok(())
    }

    fn handle_backend(&self, event: BackendEvent) {
        debug!("backend: {event:?}");
        let mut inner = self.lock();
        let mut changed = vec![Subsystem::Player];
        match event {
            BackendEvent::Playing(entry) => {
                let Some(idx) = inner.loaded.iter().position(|l| l.0 == entry) else { return };
                if idx == 0 {
                    return;
                }
                // The entries before `idx` finished; the queue moves on.
                let target = inner.loaded[idx].1;
                let len = inner.queue.len();
                let single = inner.queue.single;
                for _ in 0..idx {
                    inner.queue.advance();
                }
                if inner.queue.current_item().map(|i| i.id) != Some(target) {
                    let pos = inner.queue.position_of(target);
                    inner.queue.set_current(pos);
                }
                if inner.queue.len() != len {
                    changed.push(Subsystem::Playlist);
                }
                if inner.queue.single != single {
                    changed.push(Subsystem::Options);
                }
                for _ in 0..idx {
                    if let Err(e) = self.backend.remove(0) {
                        warn!("{e:#}");
                    }
                }
                inner.loaded.drain(..idx);
                if let Err(e) = self.sync(&mut inner, false, None) {
                    warn!("{e}");
                }
            }
            BackendEvent::Idle => {
                if inner.state == State::Stop || inner.loaded.is_empty() {
                    return;
                }
                // The last loaded song finished.
                inner.loaded.clear();
                let len = inner.queue.len();
                let next = inner.queue.advance();
                if inner.queue.len() != len {
                    changed.push(Subsystem::Playlist);
                }
                if next.is_none() {
                    inner.state = State::Stop;
                    inner.queue.set_current(None);
                }
                if let Err(e) = self.sync(&mut inner, false, None) {
                    warn!("{e}");
                }
            }
            BackendEvent::Error(message) => inner.error = Some(message),
            BackendEvent::Pause(paused) => {
                let state = match (inner.state, paused) {
                    (State::Play, true) => State::Pause,
                    (State::Pause, false) => State::Play,
                    _ => return,
                };
                inner.state = state;
            }
        }
        drop(inner);
        self.emit(&changed);
    }

    // --- Persistence -----------------------------------------------------

    pub fn snapshot(&self) -> SavedState {
        let inner = self.lock();
        let q = &inner.queue;
        SavedState {
            songs: q.items().iter().map(|i| i.song.id.clone()).collect(),
            current: q.current(),
            elapsed: if inner.state == State::Stop { None } else { self.backend.position() },
            repeat: q.repeat,
            random: q.random(),
            single: q.single,
            consume: q.consume,
            volume: inner.volume,
            device: inner.device.clone(),
            replay_gain: inner.replay_gain.clone(),
        }
    }

    /// Restores a saved state, paused at the saved position. `songs` are the
    /// saved songs still in the library, in queue order.
    pub fn restore(&self, saved: SavedState, songs: Vec<Arc<SongRow>>) -> Result<()> {
        // Map the saved current position past any songs that disappeared.
        let current_id = saved.current.and_then(|p| saved.songs.get(p)).cloned();
        let current = current_id.and_then(|id| songs.iter().position(|s| s.id == id));
        self.set_volume(i32::from(saved.volume))?;
        if saved.replay_gain != "off" {
            self.set_replay_gain(&saved.replay_gain)?;
        }
        if saved.device != DEFAULT_DEVICE {
            self.backend.set_device(&saved.device)?;
            self.lock().device = saved.device;
        }
        let mut inner = self.lock();
        inner.queue.add(songs, None);
        inner.queue.repeat = saved.repeat;
        inner.queue.single = saved.single;
        inner.queue.consume = saved.consume;
        inner.queue.set_current(current);
        inner.queue.set_random(saved.random);
        if current.is_some() {
            inner.state = State::Pause;
            self.sync(&mut inner, true, saved.elapsed.filter(|&e| e > 0.0))?;
        }
        Ok(())
    }

    // --- Queue ---------------------------------------------------------

    pub fn with_queue<R>(&self, f: impl FnOnce(&Queue) -> R) -> R {
        f(&self.lock().queue)
    }

    /// Resolves an MPD position argument: absolute, or `+N` / `-N`
    /// relative to the current song.
    pub fn resolve_position(&self, arg: &str) -> Result<usize> {
        let inner = self.lock();
        let len = inner.queue.len();
        let parse = |s: &str| s.parse::<usize>().map_err(|_| Error::BadArg(format!("Invalid position: {arg}")));
        let pos = if let Some(rel) = arg.strip_prefix('+') {
            let cur = inner.queue.current().ok_or_else(|| Error::BadArg("No current song".into()))?;
            cur + 1 + parse(rel)?
        } else if let Some(rel) = arg.strip_prefix('-') {
            let cur = inner.queue.current().ok_or_else(|| Error::BadArg("No current song".into()))?;
            cur.checked_sub(parse(rel)?).ok_or_else(bad_index)?
        } else {
            parse(arg)?
        };
        if pos > len { Err(bad_index()) } else { Ok(pos) }
    }

    pub fn add(&self, songs: Vec<Arc<SongRow>>, at: Option<usize>) -> Result<Vec<ItemId>> {
        let mut inner = self.lock();
        let ids = inner.queue.add(songs, at);
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Playlist]);
        Ok(ids)
    }

    pub fn delete(&self, range: Range<usize>) -> Result<()> {
        let mut inner = self.lock();
        if range.end > inner.queue.len() || range.is_empty() {
            return Err(bad_index());
        }
        let removed_current = inner.queue.delete(range);
        // Deleting the playing song moves on to the one that replaced it.
        self.sync(&mut inner, removed_current, None)?;
        drop(inner);
        self.emit(&[Subsystem::Playlist, Subsystem::Player]);
        Ok(())
    }

    pub fn delete_id(&self, id: ItemId) -> Result<()> {
        let pos = self.with_queue(|q| q.position_of(id)).ok_or_else(|| Error::NoExist("No such song".into()))?;
        self.delete(pos..pos + 1)
    }

    pub fn clear(&self) -> Result<()> {
        let mut inner = self.lock();
        inner.queue.clear();
        inner.state = State::Stop;
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Playlist, Subsystem::Player]);
        Ok(())
    }

    pub fn move_range(&self, range: Range<usize>, to: usize) -> Result<()> {
        let mut inner = self.lock();
        inner.queue.move_range(range, to).map_err(Error::BadArg)?;
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Playlist]);
        Ok(())
    }

    pub fn move_id(&self, id: ItemId, to: usize) -> Result<()> {
        let pos = self.with_queue(|q| q.position_of(id)).ok_or_else(|| Error::NoExist("No such song".into()))?;
        self.move_range(pos..pos + 1, to)
    }

    pub fn shuffle(&self, range: Option<Range<usize>>) -> Result<()> {
        let mut inner = self.lock();
        let len = inner.queue.len();
        let range = range.unwrap_or(0..len);
        if range.end > len {
            return Err(bad_index());
        }
        inner.queue.shuffle(range);
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Playlist]);
        Ok(())
    }

    // --- Playback --------------------------------------------------------

    pub fn play(&self, pos: Option<usize>) -> Result<()> {
        let mut inner = self.lock();
        match pos {
            Some(p) if p >= inner.queue.len() => return Err(bad_index()),
            Some(p) => inner.queue.set_current(Some(p)),
            None if inner.state == State::Pause => {
                inner.state = State::Play;
                self.backend.set_pause(false)?;
                drop(inner);
                self.emit(&[Subsystem::Player]);
                return Ok(());
            }
            None if inner.state == State::Play => return Ok(()),
            None if inner.queue.current().is_none() => {
                if inner.queue.len() == 0 {
                    return Ok(());
                }
                let len = inner.queue.len();
                let first = if inner.queue.random() { rand::random_range(0..len) } else { 0 };
                inner.queue.set_current(Some(first));
            }
            None => {}
        }
        inner.state = State::Play;
        self.sync(&mut inner, true, None)?;
        drop(inner);
        self.emit(&[Subsystem::Player]);
        Ok(())
    }

    pub fn play_id(&self, id: Option<ItemId>) -> Result<()> {
        let pos = match id {
            Some(id) => Some(self.with_queue(|q| q.position_of(id)).ok_or_else(|| Error::NoExist("No such song".into()))?),
            None => None,
        };
        self.play(pos)
    }

    /// `None` toggles.
    pub fn pause(&self, pause: Option<bool>) -> Result<()> {
        let mut inner = self.lock();
        let paused = match (inner.state, pause) {
            (State::Stop, _) => return Ok(()),
            (State::Play, None | Some(true)) => true,
            (State::Pause, None | Some(false)) => false,
            _ => return Ok(()),
        };
        self.backend.set_pause(paused)?;
        inner.state = if paused { State::Pause } else { State::Play };
        drop(inner);
        self.emit(&[Subsystem::Player]);
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        let mut inner = self.lock();
        inner.state = State::Stop;
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Player]);
        Ok(())
    }

    /// Skips to the next (or previous) song in play order. Single mode does
    /// not stop a manual skip.
    pub fn skip(&self, forward: bool) -> Result<()> {
        let mut inner = self.lock();
        if inner.state == State::Stop {
            return Ok(());
        }
        let Some(cur) = inner.queue.current() else { return Ok(()) };
        let single = std::mem::replace(&mut inner.queue.single, Single::Off);
        let target = if forward { inner.queue.next_after(cur) } else { inner.queue.previous_before(cur) };
        inner.queue.single = single;
        let target_id = target.and_then(|p| inner.queue.get(p)).map(|i| i.id);
        let mut changed = vec![Subsystem::Player];
        if inner.queue.consume {
            inner.queue.delete(cur..cur + 1);
            changed.push(Subsystem::Playlist);
        }
        match target_id.and_then(|id| inner.queue.position_of(id)) {
            Some(p) => inner.queue.set_current(Some(p)),
            None => {
                inner.state = State::Stop;
                inner.queue.set_current(None);
            }
        }
        self.sync(&mut inner, true, None)?;
        drop(inner);
        self.emit(&changed);
        Ok(())
    }

    pub fn seek(&self, pos: usize, seconds: f64) -> Result<()> {
        let mut inner = self.lock();
        if pos >= inner.queue.len() {
            return Err(bad_index());
        }
        let seeked = inner.queue.current() == Some(pos)
            && inner.state != State::Stop
            && !inner.loaded.is_empty()
            // Fails while the file is still loading; reload at the offset then.
            && self.backend.seek(seconds).is_ok();
        if !seeked {
            inner.queue.set_current(Some(pos));
            if inner.state == State::Stop {
                inner.state = State::Play;
            }
            self.sync(&mut inner, true, Some(seconds))?;
        }
        drop(inner);
        self.emit(&[Subsystem::Player]);
        Ok(())
    }

    pub fn seek_id(&self, id: ItemId, seconds: f64) -> Result<()> {
        let pos = self.with_queue(|q| q.position_of(id)).ok_or_else(|| Error::NoExist("No such song".into()))?;
        self.seek(pos, seconds)
    }

    pub fn seek_current(&self, seconds: f64, relative: bool) -> Result<()> {
        let pos = self.with_queue(|q| q.current()).ok_or_else(|| Error::NoExist("Not playing".into()))?;
        let target = if relative { self.backend.position().unwrap_or(0.0) + seconds } else { seconds };
        self.seek(pos, target.max(0.0))
    }

    // --- Options ---------------------------------------------------------

    pub fn set_options(&self, f: impl FnOnce(&mut Queue)) -> Result<()> {
        let mut inner = self.lock();
        f(&mut inner.queue);
        self.sync(&mut inner, false, None)?;
        drop(inner);
        self.emit(&[Subsystem::Options]);
        Ok(())
    }

    pub fn volume(&self) -> u8 {
        self.lock().volume
    }

    pub fn set_volume(&self, volume: i32) -> Result<()> {
        let volume = volume.clamp(0, 100) as u8;
        let mut inner = self.lock();
        self.backend.set_volume(volume)?;
        inner.volume = volume;
        drop(inner);
        self.emit(&[Subsystem::Mixer]);
        Ok(())
    }

    pub fn replay_gain(&self) -> String {
        self.lock().replay_gain.clone()
    }

    pub fn set_replay_gain(&self, mode: &str) -> Result<()> {
        let mode = match mode {
            "off" | "track" | "album" => mode,
            // MPD's "auto" picks album when the queue plays in order.
            "auto" => "album",
            _ => return Err(Error::BadArg("Unrecognized replay gain mode".into())),
        };
        self.backend.set_replay_gain(mode)?;
        self.lock().replay_gain = mode.into();
        self.emit(&[Subsystem::Options]);
        Ok(())
    }

    // --- Outputs ---------------------------------------------------------

    /// Output 0 follows the system default; the rest are PipeWire sinks.
    pub fn outputs(&self) -> Vec<Output> {
        let selected = self.lock().device.clone();
        let mut devices = self.backend.devices();
        devices.sort_by(|a, b| a.name.cmp(&b.name));
        std::iter::once((DEFAULT_DEVICE.to_owned(), "Default output".to_owned()))
            .chain(devices.into_iter().map(|d| (d.name, d.description)))
            .enumerate()
            .map(|(id, (device, name))| Output { id, enabled: device == selected, name, device })
            .collect()
    }

    /// Enables one output exclusively, or (when disabling the selected one)
    /// falls back to the default.
    pub fn set_output(&self, id: usize, enable: Option<bool>) -> Result<()> {
        let outputs = self.outputs();
        let output = outputs.get(id).ok_or_else(|| Error::NoExist("No such audio output".into()))?;
        let enable = enable.unwrap_or(!output.enabled);
        let device = match (enable, output.enabled) {
            (true, _) => output.device.clone(),
            (false, true) => DEFAULT_DEVICE.to_owned(),
            (false, false) => return Ok(()),
        };
        self.backend.set_device(&device)?;
        self.lock().device = device;
        self.emit(&[Subsystem::Output]);
        Ok(())
    }

    // --- Status ----------------------------------------------------------

    pub fn status(&self) -> Status {
        let inner = self.lock();
        let q = &inner.queue;
        let playing = inner.state != State::Stop;
        let current = q.current().and_then(|p| q.get(p).map(|i| (p, i.id)));
        Status {
            state: inner.state,
            volume: inner.volume,
            repeat: q.repeat,
            random: q.random(),
            single: q.single,
            consume: q.consume,
            version: q.version(),
            length: q.len(),
            current,
            next: q.next().and_then(|p| q.get(p).map(|i| (p, i.id))),
            elapsed: if playing { self.backend.position() } else { None },
            duration: q.current_item().and_then(|i| i.song.duration),
            audio: if playing { self.backend.audio_info() } else { AudioInfo::default() },
            error: inner.error.clone(),
        }
    }
}
