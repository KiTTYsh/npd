//! Command dispatch.

use std::fmt::Write;
use std::sync::Arc;

use rusqlite::Connection;

use super::format::{self, TagMask};
use super::parse::{FilterArgs, filter_args, normalize_dir};
use super::{Ack, AckCode, Ctx};
use crate::library::query::{self, Entry, Filter, Op, Query, SongRow, TagSel};
use crate::library::tags;

/// Commands npd implements, for `commands`.
const COMMANDS: &[&str] = &[
    "binarylimit", "close", "commands", "count", "currentsong", "decoders", "find", "idle",
    "list", "listall", "listallinfo", "lsinfo", "noidle", "notcommands", "outputs", "password",
    "ping", "playlistinfo", "rescan", "search", "searchcount", "stats", "status", "tagtypes",
    "update", "urlhandlers",
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

/// Per-connection state.
pub struct Session {
    ctx: Arc<Ctx>,
    tag_mask: TagMask,
}

impl Session {
    pub fn new(ctx: Arc<Ctx>) -> Self {
        Self { ctx, tag_mask: TagMask::all() }
    }

    /// Runs `f` against the library. An unsynced library behaves as empty.
    async fn db<F, R>(&self, f: F) -> Result<R, Ack>
    where
        F: FnOnce(&Connection) -> rusqlite::Result<R> + Send + 'static,
        R: Default + Send + 'static,
    {
        Ok(self.ctx.library.read(f).await?.unwrap_or_default())
    }

    pub async fn exec(&mut self, name: &str, args: &[String], out: &mut String) -> Result<(), Ack> {
        match name {
            "ping" | "password" | "binarylimit" => Ok(()),
            "commands" => {
                for c in COMMANDS {
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
            "status" => {
                self.status(out);
                Ok(())
            }
            // Queue and player arrive with the playback engine.
            "currentsong" | "playlistinfo" | "outputs" => Ok(()),

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

            _ => Err(Ack::new(AckCode::Unknown, format!("unknown command \"{name}\""))),
        }
    }

    fn tagtypes(&mut self, args: &[String], out: &mut String) -> Result<(), Ack> {
        let names = |args: &[String]| -> Result<Vec<&'static str>, Ack> {
            if args.is_empty() {
                return Err(Ack::arg("not enough arguments"));
            }
            args.iter()
                .map(|a| tags::canonical(a).ok_or_else(|| Ack::arg(format!("unknown tag type: {a}"))))
                .collect()
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

    async fn stats(&self, out: &mut String) -> Result<(), Ack> {
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

    fn status(&self, out: &mut String) {
        out.push_str(
            "volume: 100\nrepeat: 0\nrandom: 0\nsingle: 0\nconsume: 0\nplaylist: 1\n\
             playlistlength: 0\nmixrampdb: 0\nstate: stop\n",
        );
        if let Some(job) = self.ctx.library.updating_job() {
            let _ = writeln!(out, "updating_db: {job}");
        }
    }

    async fn lsinfo(&self, args: &[String], out: &mut String) -> Result<(), Ack> {
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
            }
            None if path.is_empty() => {}
            None | Some(Found::Missing) => return Err(Ack::no_exist("No such directory")),
        }
        Ok(())
    }

    async fn listall(&self, args: &[String], out: &mut String, info: bool) -> Result<(), Ack> {
        let path = normalize_dir(args.first().map_or("", String::as_str));
        let p = path.clone();
        let entries = self
            .db(move |conn| {
                if let Some(song) = query::song_by_uri(conn, &p)? {
                    return Ok(Some(vec![Entry::Song(song)]));
                }
                if !query::dir_exists(conn, &p)? {
                    return Ok(None);
                }
                query::walk(conn, &p).map(Some)
            })
            .await?;
        let Some(entries) = entries else {
            return if path.is_empty() { Ok(()) } else { Err(Ack::no_exist("No such directory")) };
        };
        for entry in entries {
            match entry {
                Entry::Dir(d) => format::directory(out, &d),
                Entry::Song(s) if info => format::song(out, &s, self.tag_mask),
                Entry::Song(s) => {
                    let _ = writeln!(out, "file: {}", s.uri);
                }
            }
        }
        Ok(())
    }

    async fn find(&self, args: &[String], out: &mut String, fold: bool) -> Result<(), Ack> {
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

    async fn count(&self, args: &[String], out: &mut String, fold: bool) -> Result<(), Ack> {
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

    async fn list(&self, args: &[String], out: &mut String) -> Result<(), Ack> {
        let Some(tag_name) = args.first() else {
            return Err(Ack::arg("too few arguments for \"list\""));
        };
        let Some(tag) = tags::canonical(tag_name) else {
            return Err(Ack::arg(format!("unknown tag type: {tag_name}")));
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
