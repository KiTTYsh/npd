//! MPD protocol server.

mod channels;
mod commands;
mod conn;
mod format;
mod parse;
mod playback;
mod stored;

use std::fmt;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::net::{TcpListener, UnixListener};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::config::MpdConfig;
use crate::events::EventBus;
use crate::library::Library;
use crate::art::ArtStore;
use crate::handoff::Handoff;
use crate::player::Player;
use crate::playlists::Playlists;
use crate::stickers::Stickers;
use crate::subsonic::Client;

/// Protocol version announced to clients. Clients enable features by
/// version; 0.23 covers filter expressions, `readpicture`, relative queue
/// positions and `getvol`.
pub const PROTOCOL_VERSION: &str = "0.23.0";

/// A response being built: text lines plus optional binary chunks.
#[derive(Default)]
pub struct Out {
    buf: Vec<u8>,
    /// The connection's writer, for flushing long responses early.
    sink: Option<mpsc::Sender<Vec<u8>>>,
}

impl Out {
    /// Flush threshold for `flush_large`.
    const CHUNK: usize = 64 * 1024;

    pub fn new(sink: mpsc::Sender<Vec<u8>>) -> Self {
        Self { buf: Vec::new(), sink: Some(sink) }
    }

    pub fn push_str(&mut self, s: &str) {
        self.buf.extend_from_slice(s.as_bytes());
    }

    /// `binary: N`, the bytes, and a newline.
    pub fn binary(&mut self, data: &[u8]) {
        self.push_str(&format!("binary: {}\n", data.len()));
        self.buf.extend_from_slice(data);
        self.buf.push(b'\n');
    }

    pub fn push_bytes(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }

    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    pub fn take(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.buf)
    }

    /// Sends the buffered output if it has grown large, so huge listings
    /// are never held in memory whole. Fails if the client went away.
    pub async fn flush_large(&mut self) -> Result<(), Ack> {
        if self.buf.len() < Self::CHUNK {
            return Ok(());
        }
        let Some(sink) = &self.sink else { return Ok(()) };
        let chunk = std::mem::take(&mut self.buf);
        sink.send(chunk).await.map_err(|_| Ack::system("client disconnected"))
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
    pub playlists: Arc<Playlists>,
    pub handoff: Option<Arc<Handoff>>,
    pub stickers: Stickers,
    pub art: ArtStore,
    pub channels: channels::Channels,
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
    Exist = 56,
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

    if let Some(path) = config.socket_path()? {
        let path = &path;
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

fn bind_unix(path: &Path) -> Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    // A stale socket from a previous run (ours or MPD's) would make bind
    // fail, but one something still listens on must be left alone.
    if std::os::unix::net::UnixStream::connect(path).is_ok() {
        anyhow::bail!("{} is in use by another server", path.display());
    }
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
            Err(e).with_context(|| format!("removing {}", path.display()))?
        }
        _ => {}
    }
    UnixListener::bind(path).with_context(|| format!("binding {}", path.display()))
}
