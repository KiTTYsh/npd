//! Audio backend interface. The player keeps the backend's own playlist at
//! "current + next" so the backend can do gapless transitions.

use anyhow::Result;

/// Backend-assigned identifier of a loaded playlist entry.
pub type EntryId = i64;

#[derive(Debug, Clone, PartialEq)]
pub enum BackendEvent {
    /// A different entry started playing.
    Playing(EntryId),
    /// The backend's playlist ran out.
    Idle,
    /// An entry failed to load or play.
    Error(String),
    /// Paused or resumed, possibly by the backend itself.
    Pause(bool),
}

#[derive(Debug, Clone, Default)]
pub struct AudioInfo {
    pub sample_rate: Option<u32>,
    pub bits: Option<String>,
    pub channels: Option<u32>,
    pub bitrate_kbps: Option<u32>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Device {
    pub name: String,
    pub description: String,
}

pub trait Backend: Send + Sync {
    /// Loads `url`, replacing the playlist (and starting playback) or
    /// appending to it. Returns the new entry's ID.
    fn load(&self, url: &str, replace: bool, start: Option<f64>) -> Result<EntryId>;
    /// Removes the entry at `index` in the backend playlist.
    fn remove(&self, index: usize) -> Result<()>;
    fn stop(&self) -> Result<()>;
    fn set_pause(&self, paused: bool) -> Result<()>;
    fn seek(&self, seconds: f64) -> Result<()>;
    fn position(&self) -> Option<f64>;
    fn audio_info(&self) -> AudioInfo;
    fn set_volume(&self, volume: u8) -> Result<()>;
    /// Selectable output devices.
    fn devices(&self) -> Vec<Device>;
    /// `"auto"` follows the system default.
    fn set_device(&self, name: &str) -> Result<()>;
    /// `"off"`, `"track"` or `"album"`.
    fn set_replay_gain(&self, mode: &str) -> Result<()>;
}
