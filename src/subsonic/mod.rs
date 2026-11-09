//! Subsonic / OpenSubsonic API client.

pub mod model;

use std::time::Duration;

use md5::{Digest, Md5};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::config::Credentials;
use model::*;

const API_VERSION: &str = "1.16.1";
const CLIENT_NAME: &str = "npd";

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("server error {code}: {message}")]
    Api { code: u32, message: String },
    #[error("malformed response: {0}")]
    Decode(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// A fresh random salt and its token, `md5(password + salt)`.
pub fn new_token(password: &str) -> (String, String) {
    let salt = hex::encode(rand::random::<[u8; 8]>());
    let token = hex::encode(Md5::digest(format!("{password}{salt}")));
    (token, salt)
}

/// The `subsonic-response` object, or the API error it carries.
fn unwrap_envelope(mut body: Value) -> Result<Value> {
    let inner = body
        .get_mut("subsonic-response")
        .map(Value::take)
        .ok_or_else(|| Error::Decode("missing subsonic-response".into()))?;
    if inner.get("status").and_then(Value::as_str) != Some("ok") {
        let err = inner.get("error");
        return Err(Error::Api {
            code: err
                .and_then(|e| e.get("code"))
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            message: err
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("unknown error")
                .to_owned(),
        });
    }
    Ok(inner)
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
}

impl Client {
    pub fn new(base_url: &str, credentials: Credentials) -> Result<Self> {
        reqwest::Url::parse(base_url).map_err(|e| Error::Decode(format!("invalid server URL: {e}")))?;
        let http = reqwest::Client::builder()
            // Navidrome identifies players by (user, client, user agent), and
            // per-player settings like "Report Real Path" live on that
            // record. A versioned user agent would orphan them on upgrade.
            .user_agent(CLIENT_NAME)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            credentials,
        })
    }

    fn auth_params(&self) -> Vec<(&'static str, String)> {
        let mut params = vec![
            ("v", API_VERSION.to_owned()),
            ("c", CLIENT_NAME.to_owned()),
            ("f", "json".to_owned()),
        ];
        match &self.credentials {
            Credentials::ApiKey(key) => params.push(("apiKey", key.clone())),
            Credentials::Password { username, password } => {
                let (token, salt) = new_token(password);
                params.push(("u", username.clone()));
                params.push(("t", token));
                params.push(("s", salt));
            }
            Credentials::Token { username, token, salt } => {
                params.push(("u", username.clone()));
                params.push(("t", token.clone()));
                params.push(("s", salt.clone()));
            }
        }
        params
    }

    /// Calls `endpoint` and returns the unwrapped `subsonic-response` object.
    async fn call(&self, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/rest/{endpoint}", self.base_url);
        let resp = self
            .http
            .get(&url)
            .query(&self.auth_params())
            .query(params)
            .send()
            .await?
            .error_for_status()?;
        unwrap_envelope(resp.json().await?)
    }

    /// Like `call`, but sends parameters as a form body (the `formPost`
    /// extension), for writes whose song lists would overflow a URL.
    async fn post(&self, endpoint: &str, params: &[(&str, String)]) -> Result<Value> {
        let url = format!("{}/rest/{endpoint}", self.base_url);
        let mut form = self.auth_params();
        form.extend(params.iter().map(|(k, v)| (*k, v.clone())));
        let resp = self.http.post(&url).form(&form).send().await?.error_for_status()?;
        unwrap_envelope(resp.json().await?)
    }

    async fn get<T: DeserializeOwned>(&self, endpoint: &str, params: &[(&str, String)]) -> Result<T> {
        let value = self.call(endpoint, params).await?;
        serde_json::from_value(value).map_err(|e| Error::Decode(format!("{endpoint}: {e}")))
    }

    pub async fn ping(&self) -> Result<ServerInfo> {
        self.get("ping", &[]).await
    }

    pub async fn extensions(&self) -> Result<Vec<Extension>> {
        let body: ExtensionsBody = self.get("getOpenSubsonicExtensions", &[]).await?;
        Ok(body.open_subsonic_extensions)
    }

    pub async fn scan_status(&self) -> Result<ScanStatus> {
        let body: ScanStatusBody = self.get("getScanStatus", &[]).await?;
        Ok(body.scan_status)
    }

    /// One page of all songs, via `search3` with an empty query.
    pub async fn songs_page(&self, offset: usize, count: usize) -> Result<Vec<Song>> {
        let body: SearchResult3Body = self
            .get(
                "search3",
                &[
                    ("query", String::new()),
                    ("artistCount", "0".into()),
                    ("albumCount", "0".into()),
                    ("songCount", count.to_string()),
                    ("songOffset", offset.to_string()),
                ],
            )
            .await?;
        Ok(body.search_result3.song)
    }

    /// URL streaming a song's original file (no transcoding). Carries
    /// credentials, so it must not be logged at info level or above.
    pub fn stream_url(&self, id: &str) -> String {
        let mut url = reqwest::Url::parse(&format!("{}/rest/stream", self.base_url))
            .expect("base URL was validated by Client::new");
        url.query_pairs_mut()
            .extend_pairs(self.auth_params())
            .append_pair("id", id)
            .append_pair("format", "raw");
        url.into()
    }

    /// Cover art bytes and MIME type. `None` if the server has none.
    pub async fn cover_art(&self, id: &str) -> Result<Option<(Vec<u8>, String)>> {
        let resp = self
            .http
            .get(format!("{}/rest/getCoverArt", self.base_url))
            .query(&self.auth_params())
            .query(&[("id", id)])
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let resp = resp.error_for_status()?;
        let mime = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_owned();
        let body = resp.bytes().await?;
        // Errors come back as a JSON envelope with status 200.
        if mime.starts_with("application/json") || mime.starts_with("text/xml") {
            return Ok(None);
        }
        Ok(Some((body.to_vec(), mime)))
    }

    /// All (album) artists, with sort names and MusicBrainz IDs.
    pub async fn artists(&self) -> Result<Vec<ArtistRef>> {
        let body: ArtistsBody = self.get("getArtists", &[]).await?;
        Ok(body.artists.index.into_iter().flat_map(|i| i.artist).collect())
    }

    pub async fn song(&self, id: &str) -> Result<Song> {
        let body: SongBody = self.get("getSong", &[("id", id.to_owned())]).await?;
        Ok(body.song)
    }

    /// The user's playlists, without entries.
    pub async fn playlists(&self) -> Result<Vec<Playlist>> {
        let body: PlaylistsBody = self.get("getPlaylists", &[]).await?;
        Ok(body.playlists.playlist)
    }

    /// A playlist with its entries.
    pub async fn playlist(&self, id: &str) -> Result<Playlist> {
        let body: PlaylistBody = self.get("getPlaylist", &[("id", id.to_owned())]).await?;
        Ok(body.playlist)
    }

    /// Creates a playlist, or with `replace` set to a playlist ID, replaces
    /// that playlist's songs.
    pub async fn create_playlist(&self, name: Option<&str>, replace: Option<&str>, song_ids: &[String]) -> Result<()> {
        let mut params: Vec<(&str, String)> = Vec::new();
        if let Some(name) = name {
            params.push(("name", name.to_owned()));
        }
        if let Some(id) = replace {
            params.push(("playlistId", id.to_owned()));
        }
        params.extend(song_ids.iter().map(|id| ("songId", id.clone())));
        self.post("createPlaylist", &params).await.map(drop)
    }

    /// Renames a playlist, appends songs, and removes songs by position
    /// (positions refer to the playlist before the call).
    pub async fn update_playlist(
        &self,
        id: &str,
        name: Option<&str>,
        add: &[String],
        remove: &[usize],
    ) -> Result<()> {
        let mut params = vec![("playlistId", id.to_owned())];
        if let Some(name) = name {
            params.push(("name", name.to_owned()));
        }
        params.extend(add.iter().map(|id| ("songIdToAdd", id.clone())));
        params.extend(remove.iter().map(|i| ("songIndexToRemove", i.to_string())));
        self.post("updatePlaylist", &params).await.map(drop)
    }

    pub async fn delete_playlist(&self, id: &str) -> Result<()> {
        self.post("deletePlaylist", &[("id", id.to_owned())]).await.map(drop)
    }

    /// Starred songs, most recently starred first.
    pub async fn starred_songs(&self) -> Result<Vec<Song>> {
        let body: Starred2Body = self.get("getStarred2", &[]).await?;
        Ok(body.starred2.song)
    }

    pub async fn star(&self, song_ids: &[String], on: bool) -> Result<()> {
        let params: Vec<_> = song_ids.iter().map(|id| ("id", id.clone())).collect();
        self.post(if on { "star" } else { "unstar" }, &params).await.map(drop)
    }

    /// Sets a 1–5 rating; 0 removes it.
    pub async fn set_rating(&self, id: &str, rating: u8) -> Result<()> {
        self.post("setRating", &[("id", id.to_owned()), ("rating", rating.to_string())]).await.map(drop)
    }

    /// One page of all albums, alphabetically.
    pub async fn albums_page(&self, offset: usize, count: usize) -> Result<Vec<Album>> {
        let body: AlbumList2Body = self
            .get(
                "getAlbumList2",
                &[
                    ("type", "alphabeticalByName".into()),
                    ("size", count.to_string()),
                    ("offset", offset.to_string()),
                ],
            )
            .await?;
        Ok(body.album_list2.album)
    }
}
