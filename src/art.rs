//! Cover art, cached in memory (clients read images in chunks, one request
//! per chunk) and on disk, so browsing doesn't round-trip to the server for
//! every album. "No art" is cached too. The disk cache is size-capped and
//! evicts the least recently used images.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use tracing::{debug, warn};

use crate::subsonic::{self, Client};

/// Image bytes and MIME type.
pub type Art = Arc<(Vec<u8>, String)>;

const MEMORY_ENTRIES: usize = 16;

pub struct ArtStore {
    client: Arc<Client>,
    /// Disk cache directory; disabled when the cap is zero.
    dir: Option<PathBuf>,
    cap: u64,
    /// Bytes on disk, approximately.
    used: Mutex<u64>,
    memory: Mutex<VecDeque<(String, Option<Art>)>>,
}

impl ArtStore {
    pub fn new(client: Arc<Client>, dir: PathBuf, cap: u64) -> Self {
        let dir = (cap > 0).then_some(dir);
        let used = match &dir {
            Some(d) => {
                if let Err(e) = std::fs::create_dir_all(d) {
                    warn!("creating {}: {e}", d.display());
                }
                entries(d).iter().map(|e| e.1).sum()
            }
            None => 0,
        };
        Self { client, dir, cap, used: Mutex::new(used), memory: Mutex::new(VecDeque::new()) }
    }

    /// Art for a cover art ID, or `None` if the server has none.
    pub async fn get(&self, id: &str) -> Result<Option<Art>, subsonic::Error> {
        if let Some(hit) = self.memory_get(id) {
            return Ok(hit);
        }
        if let Some(hit) = self.disk_get(id) {
            self.memory_put(id, hit.clone());
            return Ok(hit);
        }
        let art = self.client.cover_art(id).await?.map(Arc::new);
        self.disk_put(id, art.as_ref());
        self.memory_put(id, art.clone());
        Ok(art)
    }

    fn memory_get(&self, id: &str) -> Option<Option<Art>> {
        let memory = self.memory.lock().unwrap();
        memory.iter().find(|(k, _)| k == id).map(|(_, v)| v.clone())
    }

    fn memory_put(&self, id: &str, art: Option<Art>) {
        let mut memory = self.memory.lock().unwrap();
        memory.retain(|(k, _)| k != id);
        if memory.len() == MEMORY_ENTRIES {
            memory.pop_front();
        }
        memory.push_back((id.to_owned(), art));
    }

    fn path(&self, id: &str) -> Option<PathBuf> {
        // IDs are server-generated; keep them to a safe file name anyway.
        let name: String = id.chars().map(|c| if c.is_ascii_alphanumeric() || "-_".contains(c) { c } else { '_' }).collect();
        self.dir.as_ref().map(|d| d.join(name))
    }

    /// An empty file records that the server has no art.
    fn disk_get(&self, id: &str) -> Option<Option<Art>> {
        let path = self.path(id)?;
        let data = std::fs::read(&path).ok()?;
        // Bump the modification time: eviction is least recently used.
        if let Ok(f) = std::fs::File::options().append(true).open(&path) {
            let _ = f.set_modified(SystemTime::now());
        }
        if data.is_empty() {
            return Some(None);
        }
        let mime = sniff(&data).to_owned();
        Some(Some(Arc::new((data, mime))))
    }

    fn disk_put(&self, id: &str, art: Option<&Art>) {
        let Some(path) = self.path(id) else { return };
        let data = art.map_or(&[][..], |a| &a.0[..]);
        let tmp = path.with_extension("tmp");
        if let Err(e) = std::fs::write(&tmp, data).and_then(|_| std::fs::rename(&tmp, &path)) {
            warn!("caching cover art: {e}");
            return;
        }
        let mut used = self.used.lock().unwrap();
        *used += data.len() as u64;
        if *used > self.cap {
            *used = evict(self.dir.as_ref().unwrap(), self.cap * 9 / 10);
        }
    }
}

/// `(path, size, mtime)` of cached files.
fn entries(dir: &Path) -> Vec<(PathBuf, u64, SystemTime)> {
    let Ok(read) = std::fs::read_dir(dir) else { return Vec::new() };
    read.filter_map(|e| {
        let e = e.ok()?;
        let meta = e.metadata().ok()?;
        meta.is_file().then(|| (e.path(), meta.len(), meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
    })
    .collect()
}

/// Deletes the least recently used files until at most `target` bytes
/// remain. Returns the bytes left.
fn evict(dir: &Path, target: u64) -> u64 {
    let mut files = entries(dir);
    files.sort_by_key(|f| f.2);
    let mut used: u64 = files.iter().map(|f| f.1).sum();
    let before = used;
    for (path, size, _) in files {
        if used <= target {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            used -= size;
        }
    }
    debug!("art cache evicted {} bytes", before - used);
    used
}

fn sniff(data: &[u8]) -> &'static str {
    match data {
        [0xFF, 0xD8, 0xFF, ..] => "image/jpeg",
        [0x89, b'P', b'N', b'G', ..] => "image/png",
        [b'G', b'I', b'F', b'8', ..] => "image/gif",
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => "image/webp",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_keeps_newest() {
        let dir = std::env::temp_dir().join(format!("npd-art-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (i, name) in ["old", "mid", "new"].iter().enumerate() {
            let path = dir.join(name);
            std::fs::write(&path, [0u8; 100]).unwrap();
            let t = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1000 + i as u64);
            std::fs::File::options().append(true).open(&path).unwrap().set_modified(t).unwrap();
        }
        assert_eq!(evict(&dir, 150), 100);
        assert!(dir.join("new").exists() && !dir.join("mid").exists());
        std::fs::remove_dir_all(&dir).unwrap();
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), "image/jpeg");
    }
}
