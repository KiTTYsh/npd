//! Command dispatch.

use std::fmt::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;

use super::channels::SubscribeError;
use super::format::{self, TagMask};
use super::parse::{FilterArgs, filter_args, normalize_dir};
use super::{Ack, AckCode, Ctx, Out};
use crate::library::query::{self, Entry, Filter, Op, Query, SongRow, TagSel};
use crate::library::tags;

/// Commands npd implements, for `commands`.
const COMMANDS: &[&str] = &[
    "binarylimit", "channels", "close", "commands", "count", "decoders", "find", "idle", "list",
    "listall", "listallinfo", "listpartitions", "lsinfo", "noidle", "notcommands", "partition",
    "password", "ping", "readmessages", "rescan", "search", "searchcount", "sendmessage", "stats",
    "status", "subscribe", "tagtypes", "unsubscribe", "update", "urlhandlers",
];

const DECODER_SUFFIXES: &[(&str, &str)] = &[
    ("flac", "audio/flac"),
    ("mp3", "audio/mpeg"),
    ("ogg", "audio/ogg"),
    ("opus", "audio/opus"),
    ("m4a", "audio/mp4"),
    ("aac", "audio/aac"),
    ("wav", "audio/wav"),
    ("wv", "audio/x-wavpack"),
    ("ape", "audio/x-ape"),
    ("aiff", "audio/aiff"),
    ("dsf", "audio/x-dsf"),
];

static NEXT_SESSION: AtomicU64 = AtomicU64::new(1);

/// Per-connection state.
pub struct Session {
    pub(super) ctx: Arc<Ctx>,
    /// Identifies the client for channel subscriptions.
    pub(super) id: u64,
    pub(super) tag_mask: TagMask,
    /// Largest binary chunk to send (`binarylimit`).
    pub(super) binary_limit: usize,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.ctx.channels.remove_client(self.id, &self.ctx.events);
    }
}

impl Session {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
        Self { ctx, id, tag_mask: TagMask::all(), binary_limit: 8192 }
    }

    /// Runs `f` against the library. An unsynced library behaves as empty.
    pub(super) async fn db<F, R>(&self, f: F) -> Result<R, Ack>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Default + Send + 'static,
    {
        Ok(self.ctx.library.read(f).await?.unwrap_or_default())
    }

    pub async fn exec(&mut self, name: &str, args: &[String], out: &mut Out) -> Result<(), Ack> {
        match name {
            "ping" | "password" => Ok(()),
            "binarylimit" => {
                let limit = args
                    .first()
                    .and_then(|a| a.parse::<usize>().ok())
                    .ok_or_else(|| Ack::arg("binarylimit expects a number"))?;
                self.binary_limit = limit.max(64);
                Ok(())
            }
            "commands" => {
                let mut all: Vec<&str> = COMMANDS.iter().chain(super::playback::COMMANDS).chain(super::stored::COMMANDS).copied().collect();
                all.sort_unstable();
                for c in all {
                    let _ = writeln!(out, "command: {c}");
                }
                Ok(())
            }
            "notcommands" | "urlhandlers" => Ok(()),
            "decoders" => {
                out.push_str("plugin: mpv\n");
                for (suffix, _) in DECODER_SUFFIXES {
                    let _ = writeln!(out, "suffix: {suffix}");
                }
                for (_, mime) in DECODER_SUFFIXES {
                    let _ = writeln!(out, "mime_type: {mime}");
                }
                Ok(())
            }
            "tagtypes" => self.tagtypes(args, out),
            "config" | "kill" => Err(Ack::new(AckCode::Permission, "you don't have permission")),

            "stats" => self.stats(out).await,

            // Partitions aren't supported; there is only the default one.
            "listpartitions" => {
                out.push_str("partition: default\n");
                Ok(())
            }
            "partition" => match args.first().map(String::as_str) {
                Some("default") => Ok(()),
                Some(_) => Err(Ack::no_exist("partition does not exist")),
                None => Err(Ack::arg("too few arguments")),
            },

            "subscribe" => {
                let channel = args.first().ok_or_else(|| Ack::arg("too few arguments"))?;
                self.ctx.channels.subscribe(self.id, channel, &self.ctx.events).map_err(|e| match e {
                    SubscribeError::Invalid => Ack::arg("invalid channel name"),
                    SubscribeError::Already => Ack::new(AckCode::Exist, "already subscribed to this channel"),
                })
            }
            "unsubscribe" => {
                let channel = args.first().ok_or_else(|| Ack::arg("too few arguments"))?;
                if self.ctx.channels.unsubscribe(self.id, channel, &self.ctx.events) {
                    Ok(())
                } else {
                    Err(Ack::no_exist("not subscribed to this channel"))
                }
            }
            "channels" => {
                for channel in self.ctx.channels.list() {
                    let _ = writeln!(out, "channel: {channel}");
                }
                Ok(())
            }
            "readmessages" => {
                for (channel, message) in self.ctx.channels.read(self.id) {
                    let _ = write!(out, "channel: {channel}\nmessage: {message}\n");
                }
                Ok(())
            }
            "sendmessage" => {
                let [channel, message] = args else {
                    return Err(Ack::arg("wrong number of arguments"));
                };
                if self.ctx.channels.send(channel, message, &self.ctx.events) {
                    Ok(())
                } else {
                    Err(Ack::no_exist("nobody is subscribed to this channel"))
                }
            }

            "update" | "rescan" => {
                let job = self.ctx.library.start_update();
                let _ = writeln!(out, "updating_db: {job}");
                Ok(())
            }

            "lsinfo" => self.lsinfo(args, out).await,
            "listall" => self.listall(args, out, false).await,
            "listallinfo" => self.listall(args, out, true).await,
            "find" => self.find(args, out, false).await,
            "search" => self.find(args, out, true).await,
            "count" => self.count(args, out, false).await,
            "searchcount" => self.count(args, out, true).await,
            "list" => self.list(args, out).await,

            _ => self.exec_playback(name, args, out).await,
        }
    }

    fn tagtypes(&mut self, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let names = |args: &[String]| -> Result<Vec<&'static str>, Ack> {
            if args.is_empty() {
                return Err(Ack::arg("not enough arguments"));
            }
            let mut out = Vec::new();
            for a in args {
                match tags::canonical(a) {
                    Some(t) => out.push(t),
                    None if tags::is_unsupported(a) => {}
                    None => return Err(Ack::arg(format!("unknown tag type: {a}"))),
                }
            }
            Ok(out)
        };
        match args.first().map(String::as_str) {
            None => {
                for tag in tags::ALL.iter().filter(|t| self.tag_mask.contains(t)) {
                    let _ = writeln!(out, "tagtype: {tag}");
                }
            }
            Some("available") => {
                for tag in tags::ALL {
                    let _ = writeln!(out, "tagtype: {tag}");
                }
            }
            Some("all") => self.tag_mask = TagMask::all(),
            Some("clear") => self.tag_mask = TagMask::none(),
            Some("enable") => names(&args[1..])?.into_iter().for_each(|t| self.tag_mask.set(t, true)),
            Some("disable") => names(&args[1..])?.into_iter().for_each(|t| self.tag_mask.set(t, false)),
            Some(other) => return Err(Ack::arg(format!("unknown sub command \"{other}\""))),
        }
        Ok(())
    }

    async fn stats(&self, out: &mut Out) -> Result<(), Ack> {
        let stats = self.ctx.library.stats().await?;
        let _ = write!(
            out,
            "artists: {}\nalbums: {}\nsongs: {}\nuptime: {}\nplaytime: 0\ndb_playtime: {}\ndb_update: {}\n",
            stats.artists,
            stats.albums,
            stats.songs,
            self.ctx.started.elapsed().as_secs(),
            stats.playtime,
            stats.updated_at,
        );
        Ok(())
    }

    async fn lsinfo(&self, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let path = normalize_dir(args.first().map_or("", String::as_str));
        enum Found {
            Song(SongRow),
            Dir(Vec<String>, Vec<SongRow>),
            Missing,
        }
        let p = path.clone();
        let found = self
            .ctx
            .library
            .read(move |conn| {
                if let Some(song) = query::song_by_uri(conn, &p)? {
                    return Ok(Found::Song(song));
                }
                if query::dir_exists(conn, &p)? {
                    let (dirs, songs) = query::list_dir(conn, &p)?;
                    return Ok(Found::Dir(dirs, songs));
                }
                Ok(Found::Missing)
            })
            .await?;
        match found {
            Some(Found::Song(song)) => format::song(out, &song, self.tag_mask),
            Some(Found::Dir(dirs, songs)) => {
                for d in dirs {
                    format::directory(out, &d);
                }
                for s in songs {
                    format::song(out, &s, self.tag_mask);
                }
                if path.is_empty() {
                    self.list_playlists(out).await?;
                }
            }
            None if path.is_empty() => self.list_playlists(out).await?,
            None | Some(Found::Missing) => return Err(Ack::no_exist("No such directory")),
        }
        Ok(())
    }

    /// Streams the listing: a blocking thread walks the library and hands
    /// over chunks, which go to the client as they come.
    async fn listall(&self, args: &[String], out: &mut Out, info: bool) -> Result<(), Ack> {
        let path = normalize_dir(args.first().map_or("", String::as_str));
        let mask = self.tag_mask;
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let p = path.clone();
        let walk = self.ctx.library.read_long(move |conn| {
            let mut chunk = Out::default();
            let send = |chunk: &mut Out| tx.blocking_send(chunk.take()).is_ok();
            if let Some(song) = query::song_by_uri(conn, &p)? {
                format::song(&mut chunk, &song, mask);
                send(&mut chunk);
                return Ok(true);
            }
            if !query::dir_exists(conn, &p)? {
                return Ok(false);
            }
            query::walk(conn, &p, |entry| {
                match entry {
                    Entry::Dir(d) => format::directory(&mut chunk, &d),
                    Entry::Song(s) if info => format::song(&mut chunk, &s, mask),
                    Entry::Song(s) => {
                        let _ = writeln!(chunk, "file: {}", s.uri);
                    }
                }
                chunk.as_bytes().len() < 32 * 1024 || send(&mut chunk)
            })?;
            Ok(send(&mut chunk))
        });
        let forward = async {
            while let Some(chunk) = rx.recv().await {
                out.push_bytes(&chunk);
                out.flush_large().await?;
            }
            Ok::<_, Ack>(())
        };
        let (found, forwarded) = tokio::join!(walk, forward);
        forwarded?;
        match found? {
            Some(false) => Err(Ack::no_exist("No such directory")),
            None if !path.is_empty() => Err(Ack::no_exist("No such directory")),
            _ => Ok(()),
        }
    }

    async fn find(&self, args: &[String], out: &mut Out, fold: bool) -> Result<(), Ack> {
        let parsed = parse_filter(args, fold)?;
        if parsed.filter.is_none() {
            return Err(Ack::arg("incorrect arguments"));
        }
        let query = Query { filter: parsed.filter, fold, sort: parsed.sort, window: parsed.window };
        let songs = self.db(move |conn| query::find(conn, &query)).await?;
        for s in &songs {
            format::song(out, s, self.tag_mask);
        }
        Ok(())
    }

    async fn count(&self, args: &[String], out: &mut Out, fold: bool) -> Result<(), Ack> {
        let parsed = parse_filter(args, fold)?;
        if parsed.groups.len() > 1 {
            return Err(Ack::arg("only one group is supported"));
        }
        let group = parsed.groups.first().copied();
        let filter = parsed.filter;
        let counts = self.db(move |conn| query::count(conn, filter.as_ref(), fold, group)).await?;
        // An unsynced library still reports zero.
        if counts.is_empty() && group.is_none() {
            out.push_str("songs: 0\nplaytime: 0\n");
        }
        for c in counts {
            if let (Some(g), Some(value)) = (group, &c.group) {
                let _ = writeln!(out, "{g}: {value}");
            }
            let _ = write!(out, "songs: {}\nplaytime: {}\n", c.songs, c.playtime);
        }
        Ok(())
    }

    async fn list(&self, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let Some(tag_name) = args.first() else {
            return Err(Ack::arg("too few arguments for \"list\""));
        };
        let Some(tag) = tags::canonical(tag_name) else {
            return if tags::is_unsupported(tag_name) {
                Ok(())
            } else {
                Err(Ack::arg(format!("unknown tag type: {tag_name}")))
            };
        };
        let rest = &args[1..];
        // Legacy form: `list album ARTIST`.
        let parsed = if tag == tags::ALBUM && rest.len() == 1 && !rest[0].starts_with('(') {
            FilterArgs {
                filter: Some(Filter::Tag { tag: TagSel::Tag(tags::ARTIST), op: Op::Eq, value: rest[0].clone() }),
                ..Default::default()
            }
        } else {
            parse_filter(rest, false)?
        };
        let groups = parsed.groups.clone();
        let filter = parsed.filter;
        let g = groups.clone();
        let rows = self.db(move |conn| query::list(conn, tag, filter.as_ref(), &g)).await?;

        let mut previous: Vec<String> = Vec::new();
        for row in rows {
            let (group_values, value) = row.split_at(groups.len());
            // Print a group header whenever it, or an outer group, changes.
            let mut changed = previous.len() != groups.len();
            for (i, (group, v)) in groups.iter().zip(group_values).enumerate() {
                changed = changed || previous[i] != *v;
                if changed {
                    let _ = writeln!(out, "{group}: {v}");
                }
            }
            let _ = writeln!(out, "{tag}: {}", value[0]);
            previous = group_values.to_vec();
        }
        Ok(())
    }
}

fn parse_filter(args: &[String], fold: bool) -> Result<FilterArgs, Ack> {
    filter_args(args, if fold { Op::Contains } else { Op::Eq }).map_err(Ack::arg)
}
