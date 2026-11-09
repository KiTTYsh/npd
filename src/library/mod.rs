//! Local SQLite mirror of the server's library.
//!
//! Queries always run against the current snapshot. An update builds a new
//! database file beside it and swaps it in atomically when complete, so a
//! sync never blocks clients.

pub mod query;
pub mod sync;
pub mod tags;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tracing::{error, info};

use crate::events::{EventBus, Subsystem};
use crate::subsonic::Client;
use crate::subsonic::model::Song;
use query::SongRow;

struct Snapshot {
    path: PathBuf,
    conn: Mutex<Connection>,
}

#[derive(Debug, Default)]
pub struct Stats {
    pub songs: u64,
    pub artists: u64,
    pub albums: u64,
    pub playtime: u64,
    /// Unix time of the last completed sync.
    pub updated_at: u64,
}

pub struct Library {
    db_path: PathBuf,
    client: Arc<Client>,
    events: EventBus,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    /// Job ID of the running update, if any.
    updating: Mutex<Option<u32>>,
    next_job: Mutex<u32>,
}

impl Library {
    pub fn open(state_dir: &Path, client: Arc<Client>, events: EventBus) -> Result<Arc<Self>> {
        std::fs::create_dir_all(state_dir)
            .with_context(|| format!("creating {}", state_dir.display()))?;
        let db_path = state_dir.join("library.db");
        let snapshot = if db_path.exists() {
            Some(Arc::new(open_snapshot(&db_path)?))
        } else {
            None
        };
        Ok(Arc::new(Self {
            db_path,
            client,
            events,
            snapshot: RwLock::new(snapshot),
            updating: Mutex::new(None),
            next_job: Mutex::new(1),
        }))
    }

    pub fn is_empty(&self) -> bool {
        self.snapshot.read().unwrap().is_none()
    }

    pub fn updating_job(&self) -> Option<u32> {
        *self.updating.lock().unwrap()
    }

    /// Starts a background full sync and returns its job ID. If one is
    /// already running, returns that job's ID instead.
    pub fn start_update(self: &Arc<Self>) -> u32 {
        let mut updating = self.updating.lock().unwrap();
        if let Some(job) = *updating {
            return job;
        }
        let job = {
            let mut next = self.next_job.lock().unwrap();
            let job = *next;
            *next = next.checked_add(1).unwrap_or(1);
            job
        };
        *updating = Some(job);
        drop(updating);
        self.events.emit(Subsystem::Update);

        let this = Arc::clone(self);
        tokio::spawn(async move {
            info!("library update {job} started");
            match this.sync_and_swap().await {
                Ok(_) => {
                    info!("library update {job} finished");
                    this.events.emit(Subsystem::Database);
                }
                Err(e) => error!("library update {job} failed: {e:#}"),
            }
            *this.updating.lock().unwrap() = None;
            this.events.emit(Subsystem::Update);
        });
        job
    }

    /// Syncs in the foreground. For the `sync` subcommand.
    pub async fn update_now(&self) -> Result<sync::SyncReport> {
        self.sync_and_swap().await
    }

    async fn sync_and_swap(&self) -> Result<sync::SyncReport> {
        let staging = self.db_path.with_extension("db.new");
        let report = sync::full_sync(&self.client, &staging).await?;
        std::fs::rename(&staging, &self.db_path).context("replacing library database")?;
        // Readers holding the old snapshot keep their (now unlinked) file open
        // until they finish.
        let snapshot = open_snapshot(&self.db_path)?;
        *self.snapshot.write().unwrap() = Some(Arc::new(snapshot));
        Ok(report)
    }

    /// Runs `f` against the current snapshot on a blocking thread. Returns
    /// `Ok(None)` if no library has been synced yet.
    pub async fn read<F, R>(&self, f: F) -> Result<Option<R>>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let Some(snapshot) = self.snapshot.read().unwrap().clone() else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || {
            let conn = snapshot.conn.lock().unwrap();
            f(&conn)
        })
        .await
        .context("library query panicked")?
        .map(Some)
        .context("library query")
    }

    /// Like `read`, but on a connection of its own, for long reads (whole
    /// library listings) that shouldn't hold up other queries.
    pub async fn read_long<F, R>(&self, f: F) -> Result<Option<R>>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Send + 'static,
    {
        let Some(snapshot) = self.snapshot.read().unwrap().clone() else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || {
            let conn = open_snapshot(&snapshot.path).map_err(|e| {
                rusqlite::Error::InvalidPath(format!("{e:#}").into())
            })?;
            let conn = conn.conn.into_inner().unwrap();
            f(&conn)
        })
        .await
        .context("library query panicked")?
        .map(Some)
        .context("library query")
    }

    /// Rows for songs the server returned; see `query::resolve`.
    pub async fn resolve(&self, songs: Arc<Vec<Song>>) -> Result<Vec<SongRow>> {
        let s = Arc::clone(&songs);
        match self.read(move |conn| query::resolve(Some(conn), &s)).await? {
            Some(rows) => Ok(rows),
            None => Ok(query::resolve(None, &songs)?),
        }
    }

    pub async fn stats(&self) -> Result<Stats> {
        let stats = self
            .read(|conn| {
                let count = |sql: &str| conn.query_row(sql, [], |r| r.get::<_, i64>(0));
                Ok(Stats {
                    songs: count("SELECT COUNT(*) FROM songs")? as u64,
                    artists: count(&format!(
                        "SELECT COUNT(DISTINCT value) FROM tags WHERE tag = {}",
                        tags::id(tags::ARTIST)
                    ))? as u64,
                    albums: count(&format!(
                        "SELECT COUNT(DISTINCT value) FROM tags WHERE tag = {}",
                        tags::id(tags::ALBUM)
                    ))? as u64,
                    playtime: count("SELECT COALESCE(SUM(duration), 0) FROM songs")? as u64,
                    updated_at: conn
                        .query_row("SELECT value FROM meta WHERE key = 'synced_at'", [], |r| {
                            r.get::<_, String>(0)
                        })
                        .optional()?
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                })
            })
            .await?;
        Ok(stats.unwrap_or_default())
    }
}

fn open_snapshot(path: &Path) -> Result<Snapshot> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("opening {}", path.display()))?;
    query::register_functions(&conn)?;
    Ok(Snapshot {
        path: path.to_owned(),
        conn: Mutex::new(conn),
    })
}
