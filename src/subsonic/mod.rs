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

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    credentials: Credentials,
}

impl Client {
    pub fn new(base_url: &str, credentials: Credentials) -> Result<Self> {
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
        let mut body: Value = resp.json().await?;
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

    /// All (album) artists, with sort names and MusicBrainz IDs.
    pub async fn artists(&self) -> Result<Vec<ArtistRef>> {
        let body: ArtistsBody = self.get("getArtists", &[]).await?;
        Ok(body.artists.index.into_iter().flat_map(|i| i.artist).collect())
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
