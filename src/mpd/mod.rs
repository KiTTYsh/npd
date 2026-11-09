//! MPD protocol server.

mod commands;
mod conn;
mod format;
mod parse;
mod playback;

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, UnixListener};
use tracing::{info, warn};

use crate::config::MpdConfig;
use crate::events::EventBus;
use crate::library::Library;
use crate::player::Player;
use crate::subsonic::Client;

/// Recently fetched cover art. Clients read art in chunks (`albumart URI
/// OFFSET`), so the whole image is kept between requests.
#[derive(Default)]
pub struct ArtCache(std::sync::Mutex<std::collections::VecDeque<(String, Arc<(Vec<u8>, String)>)>>);

impl ArtCache {
    const CAPACITY: usize = 16;

    pub fn get(&self, id: &str) -> Option<Arc<(Vec<u8>, String)>> {
        let cache = self.0.lock().unwrap();
        cache.iter().find(|(k, _)| k == id).map(|(_, v)| Arc::clone(v))
    }

    pub fn insert(&self, id: String, art: Arc<(Vec<u8>, String)>) {
        let mut cache = self.0.lock().unwrap();
        cache.retain(|(k, _)| *k != id);
        if cache.len() == Self::CAPACITY {
            cache.pop_front();
        }
        cache.push_back((id, art));
    }
}

/// Protocol version announced to clients. Clients enable features by
/// version; 0.23 covers filter expressions, `readpicture`, relative queue
/// positions and `getvol`.
pub const PROTOCOL_VERSION: &str = "0.23.0";

/// A response being built: text lines plus optional binary chunks.
#[derive(Default)]
pub struct Out(Vec<u8>);

impl Out {
    pub fn push_str(&mut self, s: &str) {
        self.0.extend_from_slice(s.as_bytes());
    }

    /// `binary: N`, the bytes, and a newline.
    pub fn binary(&mut self, data: &[u8]) {
        self.push_str(&format!("binary: {}\n", data.len()));
        self.0.extend_from_slice(data);
        self.0.push(b'\n');
    }

    pub fn clear(&mut self) {
        self.0.clear();
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str(s);
        Ok(())
    }
}

/// Shared daemon state the protocol layer works on.
pub struct Ctx {
    pub library: Arc<Library>,
    pub player: Arc<Player>,
    pub client: Arc<Client>,
    pub art: ArtCache,
    pub events: EventBus,
    pub started: Instant,
}

/// MPD error codes (`ACK [code@index] {command} message`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AckCode {
    NotList = 1,
    Arg = 2,
    Permission = 4,
    Unknown = 5,
    NoExist = 50,
    System = 52,
}

#[derive(Debug)]
pub struct Ack {
    pub code: AckCode,
    pub message: String,
}

impl Ack {
    pub fn new(code: AckCode, message: impl fmt::Display) -> Self {
        Self { code, message: message.to_string() }
    }

    pub fn arg(message: impl fmt::Display) -> Self {
        Self::new(AckCode::Arg, message)
    }

    pub fn no_exist(message: impl fmt::Display) -> Self {
        Self::new(AckCode::NoExist, message)
    }

    pub fn system(message: impl fmt::Display) -> Self {
        Self::new(AckCode::System, message)
    }

    pub fn render(&self, index: usize, command: &str) -> String {
        // MPD leaves the command name out for unknown commands.
        let command = if self.code == AckCode::Unknown { "" } else { command };
        format!("ACK [{}@{index}] {{{command}}} {}\n", self.code as u32, self.message)
    }
}

impl From<anyhow::Error> for Ack {
    fn from(e: anyhow::Error) -> Self {
        Self::system(format!("{e:#}"))
    }
}

pub async fn serve(config: &MpdConfig, ctx: Arc<Ctx>) -> Result<()> {
    for addr in &config.bind {
        let listener = TcpListener::bind(addr)
            .await
            .with_context(|| format!("binding {addr}"))?;
        info!("listening on {addr}");
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        let _ = stream.set_nodelay(true);
                        let ctx = Arc::clone(&ctx);
                        tokio::spawn(conn::handle(stream, ctx, peer.to_string()));
                    }
                    Err(e) => warn!("accept failed: {e}"),
                }
            }
        });
    }

    if let Some(path) = &config.socket {
        let listener = bind_unix(path)?;
        info!("listening on {}", path.display());
        let ctx = Arc::clone(&ctx);
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, _)) => {
                        tokio::spawn(conn::handle(stream, Arc::clone(&ctx), "unix".into()));
                    }
                    Err(e) => warn!("accept failed: {e}"),
                }
            }
        });
    }
    Ok(())
}

fn bind_unix(path: &PathBuf) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A stale socket from a previous run would make bind fail.
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))?
        }
        _ => {}
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}
