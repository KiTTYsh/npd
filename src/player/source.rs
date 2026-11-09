//! Where the backend gets a song from: the local library copy when
//! configured and present, else the audio cache, else a stream from the
//! server.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::debug;

use crate::cache::AudioCache;
use crate::library::query::SongRow;
use crate::subsonic::{Client, Profile};

pub struct Sources {
    client: Arc<Client>,
    profile: Profile,
    /// Mount of the library, laid out like npd's URIs.
    local_root: Option<PathBuf>,
    cache: Option<Arc<AudioCache>>,
}

impl Sources {
    pub fn new(client: Arc<Client>, profile: Profile, local_root: Option<PathBuf>, cache: Option<Arc<AudioCache>>) -> Self {
        Self { client, profile, local_root, cache }
    }

    /// A path or URL the backend can open. Carries credentials when it is a
    /// stream, so it must not be logged at info level or above.
    pub fn locate(&self, song: &SongRow) -> String {
        if let Some(root) = &self.local_root {
            let path = root.join(&song.uri);
            // A file renamed since the last sync falls back to streaming.
            if path.is_file() {
                return path.to_string_lossy().into_owned();
            }
            debug!("{} missing locally", song.uri);
        }
        if let Some(path) = self.cache.as_ref().and_then(|c| c.lookup(&song.id)) {
            return path.to_string_lossy().into_owned();
        }
        self.client.stream_url(&song.id, &self.profile)
    }
}
