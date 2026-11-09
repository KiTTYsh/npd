//! Change notifications, named after MPD's `idle` subsystems.

use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Subsystem {
    Database,
    Update,
    StoredPlaylist,
    Playlist,
    Player,
    Mixer,
    Output,
    Options,
    Sticker,
}

impl Subsystem {
    pub const ALL: [Subsystem; 9] = [
        Self::Database,
        Self::Update,
        Self::StoredPlaylist,
        Self::Playlist,
        Self::Player,
        Self::Mixer,
        Self::Output,
        Self::Options,
        Self::Sticker,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Update => "update",
            Self::StoredPlaylist => "stored_playlist",
            Self::Playlist => "playlist",
            Self::Player => "player",
            Self::Mixer => "mixer",
            Self::Output => "output",
            Self::Options => "options",
            Self::Sticker => "sticker",
        }
    }

    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.name() == name)
    }
}

#[derive(Clone)]
pub struct EventBus(broadcast::Sender<Subsystem>);

impl EventBus {
    pub fn new() -> Self {
        Self(broadcast::channel(256).0)
    }

    pub fn emit(&self, subsystem: Subsystem) {
        // No receivers is fine: nobody is idling.
        let _ = self.0.send(subsystem);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Subsystem> {
        self.0.subscribe()
    }
}
