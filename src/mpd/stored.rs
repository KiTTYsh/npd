//! Stored playlist and sticker commands.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Arc;

use super::commands::Session;
use super::format;
use super::parse::normalize_dir;
use super::playback::{arg, parse, pos_range};
use super::{Ack, AckCode, Out};
use crate::library::query::{self, SongRow};
use crate::playlists::{self, SaveMode};
use crate::stickers::{self, FAVORITE, RATING};

/// Commands implemented here, for `commands`.
pub const COMMANDS: &[&str] = &[
    "listplaylist", "listplaylistinfo", "listplaylists", "load", "playlistadd", "playlistclear",
    "playlistdelete", "playlistmove", "rename", "rm", "save", "sticker",
];

impl From<playlists::Error> for Ack {
    fn from(e: playlists::Error) -> Self {
        use playlists::Error::*;
        let code = match &e {
            NoExist => AckCode::NoExist,
            Exists => AckCode::Exist,
            ReadOnly(_) => AckCode::Permission,
            BadIndex => AckCode::Arg,
            Server(_) => AckCode::System,
        };
        Ack::new(code, e)
    }
}

impl Session {
    pub(super) async fn exec_stored(&mut self, name: &str, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let playlists = Arc::clone(&self.ctx.playlists);
        match name {
            "listplaylists" => self.list_playlists(out).await,
            "listplaylist" | "listplaylistinfo" => {
                let songs = self.playlist_songs(arg(args, 0)?, args.get(1)).await?;
                for song in &songs {
                    if name == "listplaylist" {
                        let _ = writeln!(out, "file: {}", song.uri);
                    } else {
                        format::song(out, song, self.tag_mask);
                    }
                }
                Ok(())
            }
            "load" => {
                let songs = self.playlist_songs(arg(args, 0)?, args.get(1)).await?;
                let at = self.ctx.player.placement(args.get(2).map(String::as_str))?;
                self.ctx.player.add(songs.into_iter().map(Arc::new).collect(), at)?;
                Ok(())
            }
            "save" => {
                let mode = match args.get(1).map(String::as_str) {
                    None | Some("create") => SaveMode::Create,
                    Some("append") => SaveMode::Append,
                    Some("replace") => SaveMode::Replace,
                    Some(m) => return Err(Ack::arg(format!("Unrecognized save mode: {m}"))),
                };
                let ids: Vec<String> =
                    self.ctx.player.with_queue(|q| q.items().iter().map(|i| i.song.id.clone()).collect());
                Ok(playlists.save(arg(args, 0)?, &ids, mode).await?)
            }
            "playlistadd" => {
                let uri = normalize_dir(arg(args, 1)?);
                let pos = args.get(2).map(|_| parse::<usize>(args, 2)).transpose()?;
                let u = uri.clone();
                let songs = self
                    .db(move |conn| query::songs_for_uri(conn, &u))
                    .await?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Ack::no_exist("No such directory"))?;
                let ids: Vec<String> = songs.into_iter().map(|s| s.id).collect();
                Ok(playlists.add(arg(args, 0)?, &ids, pos).await?)
            }
            "playlistdelete" => {
                let name = arg(args, 0)?;
                let len = playlists.songs(name).await?.len();
                Ok(playlists.delete(name, pos_range(arg(args, 1)?, len)?).await?)
            }
            "playlistmove" => {
                let name = arg(args, 0)?;
                let len = playlists.songs(name).await?.len();
                let from = pos_range(arg(args, 1)?, len)?;
                Ok(playlists.move_range(name, from, parse(args, 2)?).await?)
            }
            "playlistclear" => Ok(playlists.clear(arg(args, 0)?).await?),
            "rename" => Ok(playlists.rename(arg(args, 0)?, arg(args, 1)?).await?),
            "rm" => Ok(playlists.remove(arg(args, 0)?).await?),

            "sticker" => self.sticker(args, out).await,

            _ => Err(Ack::new(AckCode::Unknown, format!("unknown command \"{name}\""))),
        }
    }

    /// `listplaylists`, which root `lsinfo` also includes.
    pub(super) async fn list_playlists(&self, out: &mut Out) -> Result<(), Ack> {
        for (name, changed) in self.ctx.playlists.list().await? {
            let _ = writeln!(out, "playlist: {name}");
            if !changed.is_empty() {
                let _ = writeln!(out, "Last-Modified: {}", format::timestamp(&changed));
            }
        }
        Ok(())
    }

    /// A playlist's songs, optionally restricted to a position range.
    async fn playlist_songs(&self, name: &str, window: Option<&String>) -> Result<Vec<SongRow>, Ack> {
        let songs = self.ctx.playlists.songs(name).await?;
        let mut rows = self.ctx.library.resolve(songs).await?;
        if let Some(w) = window {
            let range = pos_range(w, rows.len())?;
            if range.start > rows.len() {
                return Err(Ack::arg("Bad song index"));
            }
            rows.truncate(range.end);
            rows.drain(..range.start);
        }
        Ok(rows)
    }

    async fn sticker(&self, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let sub = arg(args, 0)?;
        if arg(args, 1)? != "song" {
            return Err(Ack::arg("unknown sticker domain"));
        }
        let uri = arg(args, 2)?.to_owned();
        let stickers = &self.ctx.stickers;
        if sub == "find" {
            return self.sticker_find(&normalize_dir(&uri), arg(args, 3)?, &args[4..], out).await;
        }

        let u = uri.clone();
        let song = self
            .db(move |conn| query::song_by_uri(conn, &u))
            .await?
            .ok_or_else(|| Ack::no_exist("No such song"))?;
        let no_sticker = || Ack::no_exist("no such sticker");
        match sub {
            "get" => {
                let name = arg(args, 3)?;
                let value = self.sticker_value(&song, name).await?.ok_or_else(no_sticker)?;
                let _ = writeln!(out, "sticker: {name}={value}");
            }
            "list" => {
                let mut all = BTreeMap::new();
                for name in [RATING, FAVORITE] {
                    if let Some(v) = self.sticker_value(&song, name).await? {
                        all.insert(name.to_owned(), v);
                    }
                }
                all.extend(stickers.list(&uri)?);
                for (name, value) in all {
                    let _ = writeln!(out, "sticker: {name}={value}");
                }
            }
            "set" => {
                let (name, value) = (arg(args, 3)?, arg(args, 4)?);
                self.set_sticker(&song, name, Some(value)).await?;
            }
            "delete" => match args.get(3) {
                Some(name) => {
                    if !self.set_sticker(&song, name, None).await? {
                        return Err(no_sticker());
                    }
                }
                None => {
                    // Deleting everything: the server-backed stickers too.
                    let mut any = stickers.delete(&uri, None)?;
                    for name in [RATING, FAVORITE] {
                        any |= self.set_sticker(&song, name, None).await?;
                    }
                    if !any {
                        return Err(no_sticker());
                    }
                }
            },
            _ => return Err(Ack::arg("bad request")),
        }
        Ok(())
    }

    async fn sticker_value(&self, song: &SongRow, name: &str) -> Result<Option<String>, Ack> {
        match name {
            RATING => {
                // Ask the server: ratings may have changed since the last sync.
                let s = self.ctx.client.song(&song.id).await.map_err(Ack::system)?;
                Ok(s.user_rating.filter(|&r| r > 0).map(stickers::rating_to_sticker))
            }
            FAVORITE => Ok(self.ctx.playlists.is_starred(&song.id).await?.then(|| "1".to_owned())),
            _ => Ok(self.ctx.stickers.get(&song.uri, name)?),
        }
    }

    /// Sets or (with `value` unset) deletes a sticker. Returns whether a
    /// deleted sticker existed.
    async fn set_sticker(&self, song: &SongRow, name: &str, value: Option<&str>) -> Result<bool, Ack> {
        let existed = match value {
            None => self.sticker_value(song, name).await?.is_some(),
            Some(_) => true,
        };
        match name {
            RATING => {
                let stars = match value {
                    None => 0,
                    Some(v) => stickers::sticker_to_rating(v).ok_or_else(|| Ack::arg("rating must be 0-10"))?,
                };
                if value.is_some() || existed {
                    self.ctx.client.set_rating(&song.id, stars).await.map_err(Ack::system)?;
                    self.ctx.stickers.note_rating(&song.id, stars);
                }
            }
            FAVORITE => {
                let on = match value {
                    None => false,
                    Some("1") => true,
                    Some("0") => false,
                    Some(_) => return Err(Ack::arg("favorite must be 0 or 1")),
                };
                if value.is_some() || existed {
                    self.ctx.playlists.star(std::slice::from_ref(&song.id), on).await?;
                }
            }
            _ => {
                return match value {
                    Some(v) => {
                        self.ctx.stickers.set(&song.uri, name, v)?;
                        self.ctx.events.emit(crate::events::Subsystem::Sticker);
                        Ok(true)
                    }
                    None => {
                        let deleted = self.ctx.stickers.delete(&song.uri, Some(name))?;
                        if deleted {
                            self.ctx.events.emit(crate::events::Subsystem::Sticker);
                        }
                        Ok(deleted)
                    }
                };
            }
        }
        if name == RATING {
            self.ctx.events.emit(crate::events::Subsystem::Sticker);
        }
        Ok(existed)
    }

    /// `sticker find song DIR NAME [OP VALUE]`.
    async fn sticker_find(&self, dir: &str, name: &str, cond: &[String], out: &mut Out) -> Result<(), Ack> {
        let cond = match cond {
            [] => None,
            [op, value] if matches!(op.as_str(), "=" | "<" | ">") => Some((op.as_str(), value.as_str())),
            _ => return Err(Ack::arg("bad request")),
        };
        let found: Vec<(String, String)> = match name {
            RATING => {
                let overrides = self.ctx.stickers.rating_overrides();
                let d = dir.to_owned();
                self.db(move |conn| {
                    let mut by_uri: BTreeMap<String, u8> =
                        query::rated(conn, &d)?.into_iter().map(|(uri, id, r)| (uri, overrides.get(&id).copied().unwrap_or(r))).collect();
                    for (id, stars) in &overrides {
                        if let Some(song) = query::song_by_id(conn, id)?
                            && (d.is_empty() || song.uri.starts_with(&format!("{d}/")))
                        {
                            by_uri.insert(song.uri, *stars);
                        }
                    }
                    Ok(by_uri.into_iter().filter(|(_, r)| *r > 0).map(|(u, r)| (u, stickers::rating_to_sticker(r))).collect())
                })
                .await?
            }
            FAVORITE => {
                let starred = self.ctx.playlists.starred().await?;
                let mut rows = self.ctx.library.resolve(starred).await?;
                rows.retain(|s| dir.is_empty() || s.uri.starts_with(&format!("{dir}/")));
                rows.sort_by(|a, b| a.uri.cmp(&b.uri));
                rows.into_iter().map(|s| (s.uri, "1".to_owned())).collect()
            }
            _ => self.ctx.stickers.find(dir, name)?,
        };
        for (uri, value) in found {
            if let Some((op, want)) = cond
                && !compare(&value, op, want)
            {
                continue;
            }
            let _ = write!(out, "file: {uri}\nsticker: {name}={value}\n");
        }
        Ok(())
    }
}

/// MPD compares sticker values numerically when both are integers.
fn compare(value: &str, op: &str, want: &str) -> bool {
    let ord = match (value.parse::<i64>(), want.parse::<i64>()) {
        (Ok(a), Ok(b)) => a.cmp(&b),
        _ => value.cmp(want),
    };
    match op {
        "=" => ord.is_eq(),
        "<" => ord.is_lt(),
        _ => ord.is_gt(),
    }
}
