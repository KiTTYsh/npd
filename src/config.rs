use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub server: ServerConfig,
    #[serde(default)]
    pub mpd: MpdConfig,
    #[serde(default)]
    pub paths: PathsConfig,
    #[serde(default)]
    pub playlists: PlaylistsConfig,
    #[serde(default)]
    pub playback: PlaybackConfig,
    #[serde(default)]
    pub cache: CacheConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Base URL of the Navidrome server, e.g. `https://music.example.com`.
    pub url: String,
    pub username: String,
    /// Plain-text password. Prefer `password_command`.
    pub password: Option<String>,
    /// Shell command whose first line of stdout is the password.
    pub password_command: Option<String>,
    /// Precomputed `md5(password + salt)`, used with `salt`. Generate with
    /// `npd token`.
    pub token: Option<String>,
    pub salt: Option<String>,
    /// OpenSubsonic API key; replaces username/password auth when set.
    pub api_key: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct MpdConfig {
    /// TCP addresses to listen on.
    pub bind: Vec<String>,
    /// Unix socket path; disabled when unset. `~` and `$XDG_RUNTIME_DIR`
    /// are expanded.
    pub socket: Option<PathBuf>,
}

impl Default for MpdConfig {
    fn default() -> Self {
        Self {
            bind: vec!["127.0.0.1:6600".into()],
            socket: None,
        }
    }
}

impl MpdConfig {
    pub fn socket_path(&self) -> Result<Option<PathBuf>> {
        let Some(path) = &self.socket else { return Ok(None) };
        let text = path.to_str().context("mpd.socket is not UTF-8")?;
        let expanded = if let Some(rest) = text.strip_prefix("~/") {
            dirs::home_dir().context("cannot determine home directory")?.join(rest)
        } else if let Some(rest) = text.strip_prefix("$XDG_RUNTIME_DIR/") {
            dirs::runtime_dir().context("XDG_RUNTIME_DIR is not set")?.join(rest)
        } else {
            path.clone()
        };
        Ok(Some(expanded))
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PathsConfig {
    /// Library mirror and other persistent state. Defaults to `$XDG_STATE_HOME/npd`.
    pub state_dir: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlaylistsConfig {
    /// Name of the virtual playlist of starred songs; empty disables it.
    pub favorites: String,
    /// Seconds between checks for playlist and star changes made elsewhere;
    /// 0 disables polling.
    pub poll_interval: u64,
}

impl Default for PlaylistsConfig {
    fn default() -> Self {
        Self {
            favorites: "Favorites".into(),
            poll_interval: 120,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct PlaybackConfig {
    /// A mounted copy of the library, laid out like npd's URIs (the server
    /// paths with their common prefix removed). When set, songs play from
    /// here instead of streaming.
    pub local_root: Option<PathBuf>,
    /// Report plays to the server ("now playing", play counts, and any
    /// scrobbling the server forwards to Last.fm / ListenBrainz).
    pub scrobble: bool,
    /// Server transcoding for streams and the audio cache: `raw` (the
    /// original file) or a format the server is set up to transcode to.
    /// Prefer `opus`: it stays gapless, MP3 doesn't.
    pub format: String,
    /// kbps for transcoding; 0 for the server's default.
    pub max_bitrate: u32,
    /// Share the play queue with other devices through the server: save it
    /// on pause/stop, take over another device's on resume.
    pub handoff: bool,
}

impl Default for PlaybackConfig {
    fn default() -> Self {
        Self { local_root: None, scrobble: true, format: "raw".into(), max_bitrate: 0, handoff: false }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct CacheConfig {
    /// Defaults to `$XDG_CACHE_HOME/npd`.
    pub dir: Option<PathBuf>,
    /// Cover art cache size in MiB; 0 disables it.
    pub art_size: u64,
    /// Audio cache size in MiB, not counting pinned favorites; 0 disables
    /// the audio cache.
    pub audio_size: u64,
    /// Songs after the current one to download ahead; 0 streams only.
    pub prefetch: usize,
    /// Keep every starred song cached.
    pub pin_favorites: bool,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self { dir: None, art_size: 512, audio_size: 2048, prefetch: 0, pin_favorites: false }
    }
}

pub enum Credentials {
    Password { username: String, password: String },
    Token { username: String, token: String, salt: String },
    ApiKey(String),
}

impl Config {
    pub fn default_path() -> Result<PathBuf> {
        Ok(dirs::config_dir()
            .context("cannot determine config directory")?
            .join("npd/config.toml"))
    }

    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    pub fn cache_dir(&self) -> Result<PathBuf> {
        match &self.cache.dir {
            Some(dir) => Ok(dir.clone()),
            None => Ok(dirs::cache_dir()
                .context("cannot determine cache directory")?
                .join("npd")),
        }
    }

    pub fn state_dir(&self) -> Result<PathBuf> {
        match &self.paths.state_dir {
            Some(dir) => Ok(dir.clone()),
            None => Ok(dirs::state_dir()
                .context("cannot determine state directory")?
                .join("npd")),
        }
    }
}

impl ServerConfig {
    pub fn credentials(&self) -> Result<Credentials> {
        let methods = [
            self.api_key.is_some(),
            self.password.is_some(),
            self.password_command.is_some(),
            self.token.is_some() || self.salt.is_some(),
        ];
        match methods.iter().filter(|&&set| set).count() {
            1 => {}
            0 => bail!(
                "one of server.password, server.password_command, server.token + server.salt \
                 or server.api_key is required"
            ),
            _ => bail!(
                "set only one of server.password, server.password_command, \
                 server.token + server.salt and server.api_key"
            ),
        }

        let username = self.username.clone();
        if let Some(key) = &self.api_key {
            return Ok(Credentials::ApiKey(key.clone()));
        }
        if self.token.is_some() || self.salt.is_some() {
            let (Some(token), Some(salt)) = (&self.token, &self.salt) else {
                bail!("server.token and server.salt must be set together");
            };
            return Ok(Credentials::Token {
                username,
                token: token.clone(),
                salt: salt.clone(),
            });
        }
        let password = match &self.password {
            Some(p) => p.clone(),
            None => run_password_command(self.password_command.as_deref().unwrap())?,
        };
        Ok(Credentials::Password { username, password })
    }
}

fn run_password_command(cmd: &str) -> Result<String> {
    let out = std::process::Command::new("sh")
        .args(["-c", cmd])
        .stderr(std::process::Stdio::inherit())
        .output()
        .context("running server.password_command")?;
    if !out.status.success() {
        bail!("server.password_command exited with {}", out.status);
    }
    let stdout = String::from_utf8(out.stdout).context("password is not UTF-8")?;
    match stdout.lines().next() {
        Some(line) if !line.is_empty() => Ok(line.to_owned()),
        _ => bail!("server.password_command printed nothing"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn minimal_config() {
        let cfg: Config = toml::from_str(
            r#"
            [server]
            url = "https://music.example.com"
            username = "me"
            password = "secret"
            "#,
        )
        .unwrap();
        assert_eq!(cfg.mpd.bind, ["127.0.0.1:6600"]);
        assert_eq!(cfg.playlists.favorites, "Favorites");
        assert!(matches!(
            cfg.server.credentials().unwrap(),
            Credentials::Password { .. }
        ));
    }

    #[test]
    fn password_command() {
        let server = ServerConfig {
            url: String::new(),
            username: "me".into(),
            password: None,
            password_command: Some("printf 'hunter2\\nextra'".into()),
            token: None,
            salt: None,
            api_key: None,
        };
        match server.credentials().unwrap() {
            Credentials::Password { password, .. } => assert_eq!(password, "hunter2"),
            _ => panic!(),
        }
    }

    #[test]
    fn token_and_salt() {
        let parse = |extra: &str| {
            let cfg: Config = toml::from_str(&format!(
                "[server]\nurl = \"u\"\nusername = \"me\"\n{extra}"
            ))
            .unwrap();
            cfg.server.credentials()
        };
        assert!(matches!(
            parse("token = \"t\"\nsalt = \"s\"").unwrap(),
            Credentials::Token { .. }
        ));
        assert!(parse("token = \"t\"").is_err());
        assert!(parse("token = \"t\"\nsalt = \"s\"\npassword = \"p\"").is_err());
        assert!(parse("").is_err());
    }
}
