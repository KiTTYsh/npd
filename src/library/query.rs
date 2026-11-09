//! Queries against a library snapshot.

use std::collections::BTreeMap;

use rusqlite::functions::FunctionFlags;
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, Row, params_from_iter};

use super::tags;
use crate::subsonic::model::Song;

/// Which tag(s) a condition looks at.
#[derive(Debug, Clone, PartialEq)]
pub enum TagSel {
    Tag(&'static str),
    /// Any tag, or the URI.
    Any,
    /// A valid MPD tag that no song has.
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Op {
    Eq,
    Ne,
    Contains,
    StartsWith,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Filter {
    Tag { tag: TagSel, op: Op, value: String },
    /// Exact URI.
    File(String),
    /// Everything under a directory; `""` is the root.
    Base(String),
    /// Added at or after this ISO 8601 timestamp.
    AddedSince(String),
    Not(Box<Filter>),
    And(Vec<Filter>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum SortKey {
    Tag(&'static str),
    Added,
}

#[derive(Debug, Clone, Default)]
pub struct Query {
    pub filter: Option<Filter>,
    /// Case-insensitive matching (`search`) rather than exact (`find`).
    pub fold: bool,
    pub sort: Option<(SortKey, bool)>,
    pub window: Option<(usize, usize)>,
}

/// A song as stored, with tags still encoded.
#[derive(Debug, Clone)]
pub struct SongRow {
    pub rowid: i64,
    pub id: String,
    pub uri: String,
    pub duration: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u32>,
    pub added: Option<String>,
    pub cover_art: Option<String>,
    pub tags: Vec<u8>,
}

impl SongRow {
    pub fn tags(&self) -> Vec<(&'static str, &str)> {
        tags::decode(&self.tags)
    }
}

const SONG_COLUMNS: &str =
    "s.rowid, s.id, s.uri, s.duration, s.sample_rate, s.bit_depth, s.channels, s.added, s.cover_art, s.tags";

fn song_row(row: &Row) -> rusqlite::Result<SongRow> {
    Ok(SongRow {
        rowid: row.get(0)?,
        id: row.get(1)?,
        uri: row.get(2)?,
        duration: row.get(3)?,
        sample_rate: row.get(4)?,
        bit_depth: row.get(5)?,
        channels: row.get(6)?,
        added: row.get(7)?,
        cover_art: row.get(8)?,
        tags: row.get(9)?,
    })
}

/// Registers `fold(text)` (Unicode lowercase) and `tagval(blob, id)` (first
/// value of a tag from `songs.tags`).
pub fn register_functions(conn: &Connection) -> rusqlite::Result<()> {
    let flags = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DETERMINISTIC;
    conn.create_scalar_function("fold", 1, flags, |ctx| {
        Ok(ctx.get::<Option<String>>(0)?.map(|s| s.to_lowercase()))
    })?;
    conn.create_scalar_function("tagval", 2, flags, |ctx| {
        let blob = ctx.get_raw(0).as_blob().unwrap_or_default();
        let id = ctx.get::<i64>(1)?;
        let tag = tags::ALL.get(id as usize).copied();
        Ok(tags::decode(blob)
            .into_iter()
            .find(|(t, _)| Some(*t) == tag)
            .map(|(_, v)| v.to_owned()))
    })
}

/// Builds a SQL condition on songs aliased `s`.
struct Sql {
    text: String,
    params: Vec<Value>,
}

impl Sql {
    fn new() -> Self {
        Self { text: String::new(), params: Vec::new() }
    }

    fn param(&mut self, v: impl Into<Value>) {
        self.text.push('?');
        self.params.push(v.into());
    }

    /// A string comparison of `column` against `value`.
    fn compare(&mut self, column: &str, op: Op, value: &str, fold: bool) {
        let col = if fold { format!("fold({column})") } else { column.to_owned() };
        let arg = |sql: &mut Self| {
            if fold {
                sql.text.push_str("fold(");
                sql.param(value.to_owned());
                sql.text.push(')');
            } else {
                sql.param(value.to_owned());
            }
        };
        match op {
            Op::Eq | Op::Ne => {
                self.text.push_str(&col);
                self.text.push_str(if op == Op::Eq { " = " } else { " <> " });
                arg(self);
            }
            Op::Contains | Op::StartsWith => {
                self.text.push_str("instr(");
                self.text.push_str(&col);
                self.text.push_str(", ");
                arg(self);
                self.text.push_str(if op == Op::Contains { ") > 0" } else { ") = 1" });
            }
        }
    }

    fn filter(&mut self, filter: &Filter, fold: bool) {
        match filter {
            Filter::Tag { tag, op: Op::Ne, value } => {
                self.text.push_str("NOT (");
                self.filter(&Filter::Tag { tag: tag.clone(), op: Op::Eq, value: value.clone() }, fold);
                self.text.push(')');
            }
            // MPD: `(Tag == "")` matches songs without the tag.
            Filter::Tag { tag: TagSel::Tag(tag), op: Op::Eq, value } if value.is_empty() => {
                self.text.push_str("s.rowid NOT IN (SELECT song FROM tags WHERE tag = ");
                self.param(tags::id(tag) as i64);
                self.text.push(')');
            }
            Filter::Tag { tag: TagSel::Tag(tag), op, value } => {
                self.text.push_str("s.rowid IN (SELECT song FROM tags WHERE tag = ");
                self.param(tags::id(tag) as i64);
                self.text.push_str(" AND ");
                self.compare("value", *op, value, fold);
                self.text.push(')');
            }
            // Only `(Tag == "")` matches a tag nobody has.
            Filter::Tag { tag: TagSel::Absent, op, value } => {
                self.text.push(if *op == Op::Eq && value.is_empty() { '1' } else { '0' });
            }
            Filter::Tag { tag: TagSel::Any, op, value } => {
                self.text.push_str("(s.rowid IN (SELECT song FROM tags WHERE ");
                self.compare("value", *op, value, fold);
                self.text.push_str(") OR ");
                self.compare("s.uri", *op, value, fold);
                self.text.push(')');
            }
            Filter::File(uri) => {
                self.text.push_str("s.uri = ");
                self.param(uri.clone());
            }
            Filter::Base(dir) if dir.is_empty() => self.text.push('1'),
            Filter::Base(dir) => {
                self.text.push_str("(s.dir = ");
                self.param(dir.clone());
                self.text.push_str(" OR instr(s.dir, ");
                self.param(format!("{dir}/"));
                self.text.push_str(") = 1)");
            }
            Filter::AddedSince(ts) => {
                self.text.push_str("s.added >= ");
                self.param(ts.clone());
            }
            Filter::Not(inner) => {
                self.text.push_str("NOT (");
                self.filter(inner, fold);
                self.text.push(')');
            }
            Filter::And(parts) if parts.is_empty() => self.text.push('1'),
            Filter::And(parts) => {
                for (i, part) in parts.iter().enumerate() {
                    if i > 0 {
                        self.text.push_str(" AND ");
                    }
                    self.text.push('(');
                    self.filter(part, fold);
                    self.text.push(')');
                }
            }
        }
    }

    fn where_clause(filter: Option<&Filter>, fold: bool) -> Self {
        let mut sql = Self::new();
        match filter {
            Some(f) => sql.filter(f, fold),
            None => sql.text.push('1'),
        }
        sql
    }
}

/// Songs matching a query, in URI order unless sorted.
pub fn find(conn: &Connection, query: &Query) -> rusqlite::Result<Vec<SongRow>> {
    let mut sql = Sql::where_clause(query.filter.as_ref(), query.fold);
    let order = match &query.sort {
        None => "s.uri".to_owned(),
        Some((key, desc)) => {
            let dir = if *desc { "DESC" } else { "ASC" };
            match key {
                SortKey::Tag(tag) => {
                    format!("tagval(s.tags, {}) {dir}, s.uri", tags::id(tag))
                }
                SortKey::Added => format!("s.added {dir}, s.uri"),
            }
        }
    };
    let mut text = format!("SELECT {SONG_COLUMNS} FROM songs s WHERE {} ORDER BY {order}", sql.text);
    if let Some((start, end)) = query.window {
        text.push_str(" LIMIT ? OFFSET ?");
        sql.params.push(Value::Integer(end.saturating_sub(start) as i64));
        sql.params.push(Value::Integer(start as i64));
    }
    let mut stmt = conn.prepare(&text)?;
    stmt.query_map(params_from_iter(sql.params), song_row)?.collect()
}

/// Songs by server ID, in the given order; missing ones are skipped.
pub fn songs_by_ids(conn: &Connection, ids: &[String]) -> rusqlite::Result<Vec<SongRow>> {
    let mut stmt = conn.prepare(&format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.id = ?"))?;
    let mut songs = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(song) = stmt.query_row([id], song_row).optional()? {
            songs.push(song);
        }
    }
    Ok(songs)
}

pub fn song_by_uri(conn: &Connection, uri: &str) -> rusqlite::Result<Option<SongRow>> {
    conn.query_row(
        &format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.uri = ?"),
        [uri],
        song_row,
    )
    .optional()
}

/// Distinct values of `tag` among matching songs. Each row is the group tag
/// values (in `groups` order) followed by the value. Grouping uses the first
/// value of each group tag.
pub fn list(
    conn: &Connection,
    tag: &'static str,
    filter: Option<&Filter>,
    groups: &[&'static str],
) -> rusqlite::Result<Vec<Vec<String>>> {
    let mut sql = Sql::new();
    let mut columns: Vec<String> = groups
        .iter()
        .map(|g| format!("COALESCE(tagval(s.tags, {}), '')", tags::id(g)))
        .collect();
    columns.push("t.value".into());
    let n = columns.len();
    sql.text = format!(
        "SELECT DISTINCT {} FROM tags t JOIN songs s ON s.rowid = t.song WHERE t.tag = ",
        columns.join(", ")
    );
    sql.param(tags::id(tag) as i64);
    if let Some(f) = filter {
        sql.text.push_str(" AND (");
        sql.filter(f, false);
        sql.text.push(')');
    }
    let order: Vec<String> = (1..=n).map(|i| i.to_string()).collect();
    sql.text.push_str(&format!(" ORDER BY {}", order.join(", ")));
    let mut stmt = conn.prepare(&sql.text)?;
    stmt.query_map(params_from_iter(sql.params), |row| {
        (0..n).map(|i| row.get::<_, String>(i)).collect()
    })?
    .collect()
}

#[derive(Debug, PartialEq)]
pub struct Count {
    pub group: Option<String>,
    pub songs: u64,
    pub playtime: u64,
}

pub fn count(
    conn: &Connection,
    filter: Option<&Filter>,
    fold: bool,
    group: Option<&'static str>,
) -> rusqlite::Result<Vec<Count>> {
    let sql = Sql::where_clause(filter, fold);
    let row = |r: &Row| {
        Ok(Count {
            group: r.get(0)?,
            songs: r.get::<_, i64>(1)? as u64,
            playtime: r.get::<_, i64>(2)? as u64,
        })
    };
    let text = match group {
        None => format!(
            "SELECT NULL, COUNT(*), COALESCE(SUM(s.duration), 0) FROM songs s WHERE {}",
            sql.text
        ),
        Some(g) => format!(
            "SELECT COALESCE(tagval(s.tags, {}), ''), COUNT(*), COALESCE(SUM(s.duration), 0)
             FROM songs s WHERE {} GROUP BY 1 ORDER BY 1",
            tags::id(g),
            sql.text
        ),
    };
    let mut stmt = conn.prepare(&text)?;
    stmt.query_map(params_from_iter(sql.params), row)?.collect()
}

/// The song at `uri`, or every song under it if it is a directory; `None`
/// if neither exists.
pub fn songs_for_uri(conn: &Connection, uri: &str) -> rusqlite::Result<Option<Vec<SongRow>>> {
    if let Some(song) = song_by_uri(conn, uri)? {
        return Ok(Some(vec![song]));
    }
    if !dir_exists(conn, uri)? {
        return Ok(None);
    }
    find(conn, &Query { filter: Some(Filter::Base(uri.to_owned())), ..Default::default() }).map(Some)
}

pub fn dir_exists(conn: &Connection, dir: &str) -> rusqlite::Result<bool> {
    if dir.is_empty() {
        return Ok(true);
    }
    conn.query_row("SELECT 1 FROM dirs WHERE path = ?", [dir], |_| Ok(()))
        .optional()
        .map(|r| r.is_some())
}

/// Immediate subdirectories and songs of `dir`.
pub fn list_dir(conn: &Connection, dir: &str) -> rusqlite::Result<(Vec<String>, Vec<SongRow>)> {
    let dirs = conn
        .prepare("SELECT path FROM dirs WHERE parent = ? ORDER BY path")?
        .query_map([dir], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let songs = conn
        .prepare(&format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.dir = ? ORDER BY s.uri"))?
        .query_map([dir], song_row)?
        .collect::<rusqlite::Result<_>>()?;
    Ok((dirs, songs))
}

pub enum Entry {
    Dir(String),
    Song(SongRow),
}

/// Everything under `dir`, depth first: each directory followed by its
/// songs, then its subdirectories, as MPD's `listall` orders it. Calls `f`
/// per entry so huge listings need not fit in memory; `f` returns false to
/// stop early.
pub fn walk(conn: &Connection, dir: &str, mut f: impl FnMut(Entry) -> bool) -> rusqlite::Result<()> {
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT path, parent FROM dirs
             WHERE ?1 = '' OR path = ?1 OR instr(path, ?1 || '/') = 1
             ORDER BY path",
        )?;
        for row in stmt.query_map([dir], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (path, parent) = row?;
            children.entry(parent).or_default().push(path);
        }
    }

    let mut songs = conn.prepare(&format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.dir = ? ORDER BY s.uri"))?;
    let mut stack = vec![dir.to_owned()];
    while let Some(d) = stack.pop() {
        if d != dir && !f(Entry::Dir(d.clone())) {
            return Ok(());
        }
        for song in songs.query_map([&d], song_row)? {
            if !f(Entry::Song(song?)) {
                return Ok(());
            }
        }
        if let Some(kids) = children.remove(&d) {
            stack.extend(kids.into_iter().rev());
        }
    }
    Ok(())
}

/// `(uri, id, rating)` of rated songs under `dir`, as of the last sync.
pub fn rated(conn: &Connection, dir: &str) -> rusqlite::Result<Vec<(String, String, u8)>> {
    let mut stmt = conn.prepare(
        "SELECT uri, id, rating FROM songs
         WHERE rating > 0 AND (?1 = '' OR instr(uri, ?1 || '/') = 1)
         ORDER BY uri",
    )?;
    let rows = stmt.query_map([dir], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
    rows.collect()
}

pub fn song_by_id(conn: &Connection, id: &str) -> rusqlite::Result<Option<SongRow>> {
    conn.query_row(&format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.id = ?"), [id], song_row)
        .optional()
}

/// The directory prefix the last sync stripped from server paths.
pub fn root(conn: &Connection) -> rusqlite::Result<String> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = 'root'", [], |r| r.get(0))
        .optional()?
        .unwrap_or_default())
}

/// Rows for songs the server returned (playlist entries, favorites): from
/// the mirror where present, otherwise built from the server's data, so
/// songs added since the last sync still show up and play.
pub fn resolve(conn: Option<&Connection>, songs: &[Song]) -> rusqlite::Result<Vec<SongRow>> {
    let root = match conn {
        Some(conn) => root(conn)?,
        None => String::new(),
    };
    let mut stmt = match conn {
        Some(conn) => Some(conn.prepare(&format!("SELECT {SONG_COLUMNS} FROM songs s WHERE s.id = ?"))?),
        None => None,
    };
    let mut rows = Vec::with_capacity(songs.len());
    for song in songs {
        let row = match stmt.as_mut() {
            Some(stmt) => stmt.query_row([&song.id], song_row).optional()?,
            None => None,
        };
        rows.push(row.unwrap_or_else(|| SongRow::from_song(song, &root)));
    }
    Ok(rows)
}

impl SongRow {
    /// A row for a song the mirror doesn't have.
    pub fn from_song(song: &Song, root: &str) -> Self {
        let uri = tags::song_uri(song);
        let uri = match uri.strip_prefix(root).and_then(|u| u.strip_prefix('/')) {
            Some(rest) if !root.is_empty() => rest.to_owned(),
            _ => uri,
        };
        Self {
            rowid: 0,
            id: song.id.clone(),
            uri,
            duration: song.duration,
            sample_rate: song.sampling_rate,
            bit_depth: song.bit_depth,
            channels: song.channel_count,
            added: song.created.clone(),
            cover_art: song.cover_art.clone(),
            tags: tags::encode(&tags::song_tags(song, None)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::library::sync::SCHEMA;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        register_functions(&conn).unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        let songs: &[(&str, &[(&str, &str)])] = &[
            ("A/X/1.flac", &[(tags::ARTIST, "Björk"), (tags::ALBUM, "X"), (tags::ALBUM_ARTIST, "Björk"), (tags::GENRE, "Pop"), (tags::GENRE, "Art")]),
            ("A/X/2.flac", &[(tags::ARTIST, "Björk feat. B"), (tags::ALBUM, "X"), (tags::ALBUM_ARTIST, "Björk")]),
            ("B/Y/1.mp3", &[(tags::ARTIST, "Bee"), (tags::ALBUM, "Y"), (tags::ALBUM_ARTIST, "Bee"), (tags::GENRE, "Rock")]),
            ("B/1.mp3", &[(tags::ARTIST, "Bee"), (tags::TITLE, "Loose")]),
        ];
        for (i, (uri, song_tags)) in songs.iter().enumerate() {
            let owned: Vec<(&'static str, String)> =
                song_tags.iter().map(|(t, v)| (tags::canonical(t).unwrap(), v.to_string())).collect();
            conn.execute(
                "INSERT INTO songs (id, uri, dir, duration, added, tags) VALUES (?, ?, ?, ?, ?, ?)",
                rusqlite::params![
                    format!("id{i}"),
                    uri,
                    tags::parent_dir(uri),
                    100 + i as i64,
                    format!("2020-01-0{}T00:00:00Z", i + 1),
                    tags::encode(&owned)
                ],
            )
            .unwrap();
            let rowid = conn.last_insert_rowid();
            for (t, v) in &owned {
                conn.execute(
                    "INSERT INTO tags (tag, value, song) VALUES (?, ?, ?)",
                    rusqlite::params![tags::id(t), v, rowid],
                )
                .unwrap();
            }
        }
        for dir in ["A", "A/X", "B", "B/Y"] {
            conn.execute(
                "INSERT INTO dirs (path, parent) VALUES (?, ?)",
                [dir, tags::parent_dir(dir)],
            )
            .unwrap();
        }
        conn
    }

    fn uris(songs: Vec<SongRow>) -> Vec<String> {
        songs.into_iter().map(|s| s.uri).collect()
    }

    fn tag(tag: &'static str, op: Op, value: &str) -> Filter {
        Filter::Tag { tag: TagSel::Tag(tag), op, value: value.into() }
    }

    fn q(filter: Filter, fold: bool) -> Query {
        Query { filter: Some(filter), fold, ..Default::default() }
    }

    #[test]
    fn find_exact_and_folded() {
        let conn = db();
        let exact = find(&conn, &q(tag(tags::ARTIST, Op::Eq, "Björk"), false)).unwrap();
        assert_eq!(uris(exact), ["A/X/1.flac"]);
        let none = find(&conn, &q(tag(tags::ARTIST, Op::Eq, "björk"), false)).unwrap();
        assert!(none.is_empty());
        let folded = find(&conn, &q(tag(tags::ARTIST, Op::Contains, "BJÖRK"), true)).unwrap();
        assert_eq!(uris(folded), ["A/X/1.flac", "A/X/2.flac"]);
    }

    #[test]
    fn find_operators() {
        let conn = db();
        let ne = find(&conn, &q(tag(tags::GENRE, Op::Ne, "Pop"), false)).unwrap();
        assert_eq!(uris(ne), ["A/X/2.flac", "B/1.mp3", "B/Y/1.mp3"]);
        let missing = find(&conn, &q(tag(tags::GENRE, Op::Eq, ""), false)).unwrap();
        assert_eq!(uris(missing), ["A/X/2.flac", "B/1.mp3"]);
        let starts = find(&conn, &q(tag(tags::ARTIST, Op::StartsWith, "Be"), false)).unwrap();
        assert_eq!(uris(starts), ["B/1.mp3", "B/Y/1.mp3"]);
        let absent = |op, v: &str| find(&conn, &q(Filter::Tag { tag: TagSel::Absent, op, value: v.into() }, false)).unwrap().len();
        assert_eq!((absent(Op::Eq, ""), absent(Op::Eq, "x"), absent(Op::Ne, "x")), (4, 0, 4));
        let any = find(&conn, &q(Filter::Tag { tag: TagSel::Any, op: Op::Contains, value: "y/1".into() }, true)).unwrap();
        assert_eq!(uris(any), ["B/Y/1.mp3"]);
        let and = Filter::And(vec![tag(tags::ARTIST, Op::Eq, "Bee"), Filter::Not(Box::new(Filter::Base("B/Y".into())))]);
        assert_eq!(uris(find(&conn, &q(and, false)).unwrap()), ["B/1.mp3"]);
        let base = find(&conn, &q(Filter::Base("A".into()), false)).unwrap();
        assert_eq!(uris(base), ["A/X/1.flac", "A/X/2.flac"]);
        let since = find(&conn, &q(Filter::AddedSince("2020-01-03".into()), false)).unwrap();
        assert_eq!(uris(since), ["B/1.mp3", "B/Y/1.mp3"]);
    }

    #[test]
    fn find_sort_window() {
        let conn = db();
        let query = Query { sort: Some((SortKey::Tag(tags::ARTIST), true)), window: Some((1, 3)), ..Default::default() };
        assert_eq!(uris(find(&conn, &query).unwrap()), ["A/X/1.flac", "B/1.mp3"]);
    }

    #[test]
    fn list_grouped() {
        let conn = db();
        let albums = list(&conn, tags::ALBUM, None, &[]).unwrap();
        assert_eq!(albums, [vec!["X"], vec!["Y"]]);
        let grouped = list(&conn, tags::ALBUM, None, &[tags::ALBUM_ARTIST]).unwrap();
        assert_eq!(grouped, [vec!["Bee", "Y"], vec!["Björk", "X"]]);
        let filtered = list(&conn, tags::GENRE, Some(&tag(tags::ARTIST, Op::Eq, "Bee")), &[]).unwrap();
        assert_eq!(filtered, [vec!["Rock"]]);
    }

    #[test]
    fn counts() {
        let conn = db();
        let all = count(&conn, None, false, None).unwrap();
        assert_eq!(all, [Count { group: None, songs: 4, playtime: 406 }]);
        let by_album = count(&conn, None, false, Some(tags::ALBUM)).unwrap();
        assert_eq!(by_album.iter().map(|c| (c.group.clone().unwrap(), c.songs)).collect::<Vec<_>>(),
            [("".into(), 1), ("X".into(), 2), ("Y".into(), 1)]);
    }

    #[test]
    fn dirs_and_walk() {
        let conn = db();
        let (dirs, songs) = list_dir(&conn, "B").unwrap();
        assert_eq!(dirs, ["B/Y"]);
        assert_eq!(uris(songs), ["B/1.mp3"]);
        assert!(dir_exists(&conn, "A/X").unwrap());
        assert!(!dir_exists(&conn, "A/Z").unwrap());
        let mut entries = Vec::new();
        walk(&conn, "", |e| {
            entries.push(e);
            true
        })
        .unwrap();
        let walked: Vec<String> = entries
            .into_iter()
            .map(|e| match e {
                Entry::Dir(d) => format!("d:{d}"),
                Entry::Song(s) => s.uri,
            })
            .collect();
        assert_eq!(walked, ["d:A", "d:A/X", "A/X/1.flac", "A/X/2.flac", "d:B", "B/1.mp3", "d:B/Y", "B/Y/1.mp3"]);
    }
}
