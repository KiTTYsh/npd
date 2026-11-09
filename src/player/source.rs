//! Where the backend gets a song from: the local library copy when
//! configured and present, otherwise a stream from the server.

use std::path::PathBuf;
use std::sync::Arc;

use tracing::debug;

use crate::library::query::SongRow;
use crate::subsonic::Client;

pub struct Sources {
    client: Arc<Client>,
    /// Mount of the library, laid out like npd's URIs.
    local_root: Option<PathBuf>,
}

impl Sources {
    pub fn new(client: Arc<Client>, local_root: Option<PathBuf>) -> Self {
        Self { client, local_root }
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
            debug!("{} missing locally; streaming", song.uri);
        }
        self.client.stream_url(&song.id)
    }
}
