//! Song stickers. `rating` and `favorite` map to the server's rating and
//! star; any other name is kept in a local database, as MPD would.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};

/// MPD-style ratings run 0–10 (Cantata, myMPD); the server's run 1–5.
pub const RATING: &str = "rating";
pub const FAVORITE: &str = "favorite";

pub fn rating_to_sticker(stars: u8) -> String {
    (u32::from(stars) * 2).to_string()
}

/// A 0–10 sticker value as 0–5 stars, rounding half stars up.
pub fn sticker_to_rating(value: &str) -> Option<u8> {
    let v: u8 = value.trim().parse().ok().filter(|&v| v <= 10)?;
    Some(v.div_ceil(2))
}

pub struct Stickers {
    db: Mutex<Connection>,
    /// Ratings set through npd since the last library sync, by song ID; the
    /// mirror's copy is stale for those.
    ratings: Mutex<HashMap<String, u8>>,
}

impl Stickers {
    pub fn open(state_dir: &Path) -> Result<Self> {
        let path = state_dir.join("stickers.db");
        let conn = Connection::open(&path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS stickers (
                uri TEXT NOT NULL,
                name TEXT NOT NULL,
                value TEXT NOT NULL,
                PRIMARY KEY (uri, name)
            )",
        )?;
        Ok(Self { db: Mutex::new(conn), ratings: Mutex::new(HashMap::new()) })
    }

    pub fn get(&self, uri: &str, name: &str) -> Result<Option<String>> {
        let db = self.db.lock().unwrap();
        Ok(db
            .query_row("SELECT value FROM stickers WHERE uri = ? AND name = ?", [uri, name], |r| r.get(0))
            .optional()?)
    }

    pub fn list(&self, uri: &str) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare("SELECT name, value FROM stickers WHERE uri = ? ORDER BY name")?;
        let rows = stmt.query_map([uri], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn set(&self, uri: &str, name: &str, value: &str) -> Result<()> {
        let db = self.db.lock().unwrap();
        db.execute(
            "INSERT INTO stickers (uri, name, value) VALUES (?1, ?2, ?3)
             ON CONFLICT (uri, name) DO UPDATE SET value = ?3",
            params![uri, name, value],
        )?;
        Ok(())
    }

    /// Deletes one sticker, or all of a song's with `name` unset. Returns
    /// whether anything was deleted.
    pub fn delete(&self, uri: &str, name: Option<&str>) -> Result<bool> {
        let db = self.db.lock().unwrap();
        let n = match name {
            Some(name) => db.execute("DELETE FROM stickers WHERE uri = ? AND name = ?", [uri, name])?,
            None => db.execute("DELETE FROM stickers WHERE uri = ?", [uri])?,
        };
        Ok(n > 0)
    }

    /// `(uri, value)` for songs under `dir` with sticker `name`.
    pub fn find(&self, dir: &str, name: &str) -> Result<Vec<(String, String)>> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT uri, value FROM stickers
             WHERE name = ?1 AND (?2 = '' OR instr(uri, ?2 || '/') = 1)
             ORDER BY uri",
        )?;
        let rows = stmt.query_map([name, dir], |r| Ok((r.get(0)?, r.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn note_rating(&self, id: &str, stars: u8) {
        self.ratings.lock().unwrap().insert(id.to_owned(), stars);
    }

    /// Ratings changed since the last sync.
    pub fn rating_overrides(&self) -> HashMap<String, u8> {
        self.ratings.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rating_scale() {
        assert_eq!(sticker_to_rating("10"), Some(5));
        assert_eq!(sticker_to_rating("7"), Some(4));
        assert_eq!(sticker_to_rating("0"), Some(0));
        assert_eq!(sticker_to_rating("11"), None);
        assert_eq!(rating_to_sticker(3), "6");
    }

    #[test]
    fn local_store() {
        let dir = std::env::temp_dir().join(format!("npd-stickers-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let s = Stickers::open(&dir).unwrap();
        s.set("A/1.flac", "playcount", "3").unwrap();
        s.set("A/1.flac", "playcount", "4").unwrap();
        s.set("B/1.flac", "playcount", "1").unwrap();
        assert_eq!(s.get("A/1.flac", "playcount").unwrap().as_deref(), Some("4"));
        assert_eq!(s.find("A", "playcount").unwrap(), [("A/1.flac".to_owned(), "4".to_owned())]);
        assert_eq!(s.find("", "playcount").unwrap().len(), 2);
        assert!(s.delete("A/1.flac", None).unwrap());
        assert!(s.list("A/1.flac").unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
