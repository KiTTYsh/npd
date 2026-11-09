//! Queue, playback, output and album art commands.

use std::fmt::Write;
use std::ops::Range;
use std::str::FromStr;
use std::sync::Arc;

use super::commands::Session;
use super::format;
use super::parse::{filter_args, normalize_dir, range};
use super::{Ack, Out};
use crate::library::query::{self, Op, Query};
use crate::player::{self, State};
use crate::queue::{Item, Single};

/// Commands implemented here, for `commands`.
pub const COMMANDS: &[&str] = &[
    "add", "addid", "albumart", "clear", "consume", "crossfade", "currentsong", "delete",
    "deleteid", "disableoutput", "enableoutput", "findadd", "getvol", "move",
    "moveid", "next", "outputs", "pause", "play", "playid", "playlistid", "playlistinfo",
    "plchanges", "plchangesposid", "previous", "random", "readpicture", "repeat",
    "replay_gain_mode", "replay_gain_status", "searchadd", "seek", "seekcur", "seekid", "setvol",
    "shuffle", "single", "stop", "toggleoutput", "volume",
];

impl From<player::Error> for Ack {
    fn from(e: player::Error) -> Self {
        match e {
            player::Error::BadArg(m) => Ack::arg(m),
            player::Error::NoExist(m) => Ack::no_exist(m),
            player::Error::Backend(e) => Ack::system(format!("{e:#}")),
        }
    }
}

pub(super) fn arg<'a>(args: &'a [String], i: usize) -> Result<&'a str, Ack> {
    args.get(i).map(String::as_str).ok_or_else(|| Ack::arg("too few arguments"))
}

pub(super) fn parse<T: FromStr>(args: &[String], i: usize) -> Result<T, Ack> {
    let a = arg(args, i)?;
    a.parse().map_err(|_| Ack::arg(format!("Invalid argument: {a}")))
}

fn flag(args: &[String]) -> Result<bool, Ack> {
    match arg(args, 0)? {
        "0" => Ok(false),
        "1" => Ok(true),
        a => Err(Ack::arg(format!("Boolean (0/1) expected: {a}"))),
    }
}

/// A position range argument, clamped to `len` when open-ended.
pub(super) fn pos_range(arg: &str, len: usize) -> Result<Range<usize>, Ack> {
    let (start, end) = range(arg).map_err(Ack::arg)?;
    Ok(start..end.min(len))
}

fn queue_item(out: &mut Out, pos: usize, item: &Item, session: &Session) {
    format::song(out, &item.song, session.tag_mask);
    let _ = write!(out, "Pos: {pos}\nId: {}\n", item.id);
}

impl Session {
    pub(super) async fn exec_playback(&mut self, name: &str, args: &[String], out: &mut Out) -> Result<(), Ack> {
        let player = Arc::clone(&self.ctx.player);
        match name {
            "status" => {
                self.status(out);
                Ok(())
            }
            "currentsong" => {
                player.with_queue(|q| {
                    if let Some(pos) = q.current() {
                        queue_item(out, pos, &q.items()[pos], self);
                    }
                });
                Ok(())
            }
            "playlistinfo" => {
                let window = args.first().map(String::as_str);
                player.with_queue(|q| {
                    let range = match window {
                        None => 0..q.len(),
                        Some(w) => pos_range(w, q.len())?,
                    };
                    if range.start >= q.len() && window.is_some() {
                        return Err(Ack::arg("Bad song index"));
                    }
                    for pos in range {
                        queue_item(out, pos, &q.items()[pos], self);
                    }
                    Ok(())
                })
            }
            "playlistid" => {
                let id: Option<u32> = if args.is_empty() { None } else { Some(parse(args, 0)?) };
                player.with_queue(|q| {
                    for (pos, item) in q.items().iter().enumerate() {
                        if id.is_none_or(|id| id == item.id) {
                            queue_item(out, pos, item, self);
                        }
                    }
                    if let Some(id) = id
                        && q.position_of(id).is_none()
                    {
                        return Err(Ack::no_exist("No such song"));
                    }
                    Ok(())
                })
            }
            "plchanges" | "plchangesposid" => {
                let version: u32 = parse(args, 0)?;
                let window = args.get(1).map(String::as_str);
                player.with_queue(|q| {
                    let range = match window {
                        None => 0..q.len(),
                        Some(w) => pos_range(w, q.len())?,
                    };
                    for (pos, item) in q.changes_since(version).filter(|(p, _)| range.contains(p)) {
                        if name == "plchanges" {
                            queue_item(out, pos, item, self);
                        } else {
                            let _ = write!(out, "cpos: {pos}\nId: {}\n", item.id);
                        }
                    }
                    Ok(())
                })
            }

            "add" | "addid" => {
                let uri = normalize_dir(arg(args, 0)?);
                let at = match args.get(1) {
                    Some(p) => Some(player.resolve_position(p)?),
                    None => None,
                };
                let u = uri.clone();
                let songs = self.db(move |conn| query::songs_for_uri(conn, &u)).await?;
                let songs = match songs {
                    Some(s) if name == "addid" && s.len() != 1 => {
                        return Err(Ack::no_exist("addid takes a single song"));
                    }
                    Some(s) if !s.is_empty() || uri.is_empty() => s,
                    _ => return Err(Ack::no_exist("No such directory")),
                };
                let ids = player.add(songs.into_iter().map(Arc::new).collect(), at)?;
                if name == "addid" {
                    let _ = writeln!(out, "Id: {}", ids[0]);
                }
                Ok(())
            }
            "findadd" | "searchadd" => {
                let fold = name == "searchadd";
                let parsed = filter_args(args, if fold { Op::Contains } else { Op::Eq }).map_err(Ack::arg)?;
                if parsed.filter.is_none() {
                    return Err(Ack::arg("incorrect arguments"));
                }
                let at = match &parsed.position {
                    Some(p) => Some(player.resolve_position(p)?),
                    None => None,
                };
                let query = Query { filter: parsed.filter, fold, sort: parsed.sort, window: parsed.window };
                let songs = self.db(move |conn| query::find(conn, &query)).await?;
                player.add(songs.into_iter().map(Arc::new).collect(), at)?;
                Ok(())
            }
            "delete" => {
                let len = player.with_queue(|q| q.len());
                Ok(player.delete(pos_range(arg(args, 0)?, len)?)?)
            }
            "deleteid" => Ok(player.delete_id(parse(args, 0)?)?),
            "clear" => Ok(player.clear()?),
            "move" => {
                let len = player.with_queue(|q| q.len());
                let from = pos_range(arg(args, 0)?, len)?;
                let to = player.resolve_position(arg(args, 1)?)?;
                Ok(player.move_range(from, to)?)
            }
            "moveid" => {
                let to = player.resolve_position(arg(args, 1)?)?;
                Ok(player.move_id(parse(args, 0)?, to)?)
            }
            "shuffle" => {
                let len = player.with_queue(|q| q.len());
                let range = args.first().map(|a| pos_range(a, len)).transpose()?;
                Ok(player.shuffle(range)?)
            }

            "play" => {
                let pos = if args.is_empty() { None } else { Some(parse(args, 0)?) };
                Ok(player.play(pos)?)
            }
            "playid" => {
                let id = if args.is_empty() { None } else { Some(parse(args, 0)?) };
                Ok(player.play_id(id)?)
            }
            "pause" => {
                let pause = if args.is_empty() { None } else { Some(flag(args)?) };
                Ok(player.pause(pause)?)
            }
            "stop" => Ok(player.stop()?),
            "next" => Ok(player.skip(true)?),
            "previous" => Ok(player.skip(false)?),
            "seek" => Ok(player.seek(parse(args, 0)?, parse(args, 1)?)?),
            "seekid" => Ok(player.seek_id(parse(args, 0)?, parse(args, 1)?)?),
            "seekcur" => {
                let t = arg(args, 0)?;
                let relative = t.starts_with('+') || t.starts_with('-');
                let secs: f64 = t.parse().map_err(|_| Ack::arg(format!("Invalid time: {t}")))?;
                Ok(player.seek_current(secs, relative)?)
            }

            "random" => {
                let on = flag(args)?;
                Ok(player.set_options(|q| q.set_random(on))?)
            }
            "repeat" => {
                let on = flag(args)?;
                Ok(player.set_options(|q| q.repeat = on)?)
            }
            "consume" => {
                let on = flag(args)?;
                Ok(player.set_options(|q| q.consume = on)?)
            }
            "single" => {
                let single = match arg(args, 0)? {
                    "0" => Single::Off,
                    "1" => Single::On,
                    "oneshot" => Single::Oneshot,
                    a => return Err(Ack::arg(format!("Invalid single mode: {a}"))),
                };
                Ok(player.set_options(|q| q.single = single)?)
            }
            // Crossfade and MixRamp would break gapless playback; accept and
            // ignore them so clients don't error.
            "crossfade" | "mixrampdb" | "mixrampdelay" => Ok(()),

            "setvol" => Ok(player.set_volume(parse(args, 0)?)?),
            "volume" => {
                let delta: i32 = parse(args, 0)?;
                Ok(player.set_volume(i32::from(player.volume()) + delta)?)
            }
            "getvol" => {
                let _ = writeln!(out, "volume: {}", player.volume());
                Ok(())
            }
            "replay_gain_mode" => Ok(player.set_replay_gain(arg(args, 0)?)?),
            "replay_gain_status" => {
                let _ = writeln!(out, "replay_gain_mode: {}", player.replay_gain());
                Ok(())
            }

            "outputs" => {
                for o in player.outputs() {
                    let _ = write!(
                        out,
                        "outputid: {}\noutputname: {}\nplugin: pipewire\noutputenabled: {}\n",
                        o.id,
                        o.name,
                        u8::from(o.enabled)
                    );
                }
                Ok(())
            }
            "enableoutput" => Ok(player.set_output(parse(args, 0)?, Some(true))?),
            "disableoutput" => Ok(player.set_output(parse(args, 0)?, Some(false))?),
            "toggleoutput" => Ok(player.set_output(parse(args, 0)?, None)?),

            "albumart" => self.art(args, out, false).await,
            "readpicture" => self.art(args, out, true).await,

            _ => self.exec_stored(name, args, out).await,
        }
    }

    fn status(&self, out: &mut Out) {
        let s = self.ctx.player.status();
        let single = match s.single {
            Single::Off => "0",
            Single::On => "1",
            Single::Oneshot => "oneshot",
        };
        let _ = write!(
            out,
            "volume: {}\nrepeat: {}\nrandom: {}\nsingle: {single}\nconsume: {}\nplaylist: {}\n\
             playlistlength: {}\nmixrampdb: 0\nstate: {}\n",
            s.volume,
            u8::from(s.repeat),
            u8::from(s.random),
            u8::from(s.consume),
            s.version,
            s.length,
            s.state.name(),
        );
        if let Some((pos, id)) = s.current {
            let _ = write!(out, "song: {pos}\nsongid: {id}\n");
        }
        if s.state != State::Stop {
            let elapsed = s.elapsed.unwrap_or(0.0);
            let duration = s.duration.unwrap_or(0);
            let _ = write!(
                out,
                "time: {}:{duration}\nelapsed: {elapsed:.3}\nduration: {duration}.000\n",
                elapsed as u64
            );
            if let Some(kbps) = s.audio.bitrate_kbps {
                let _ = writeln!(out, "bitrate: {kbps}");
            }
            if let (Some(rate), Some(bits), Some(ch)) = (s.audio.sample_rate, &s.audio.bits, s.audio.channels) {
                let _ = writeln!(out, "audio: {rate}:{bits}:{ch}");
            }
        }
        if let Some((pos, id)) = s.next {
            let _ = write!(out, "nextsong: {pos}\nnextsongid: {id}\n");
        }
        if let Some(job) = self.ctx.library.updating_job() {
            let _ = writeln!(out, "updating_db: {job}");
        }
        if let Some(error) = s.error {
            let _ = writeln!(out, "error: {}", error.replace('\n', " "));
        }
    }

    async fn art(&self, args: &[String], out: &mut Out, picture: bool) -> Result<(), Ack> {
        let uri = arg(args, 0)?.to_owned();
        let offset: usize = parse(args, 1)?;
        let song = self
            .db(move |conn| query::song_by_uri(conn, &uri))
            .await?
            .ok_or_else(|| Ack::no_exist("No such song"))?;
        // `albumart` is the album's cover (MPD reads the folder image), so
        // all of an album's songs share one cached image; `readpicture` is
        // the song's own embedded picture.
        let id = match (picture, &song.album_id) {
            (false, Some(album)) => Some(format!("al-{album}")),
            _ => song.cover_art.clone(),
        };
        let art = match id {
            Some(id) => self.ctx.art.get(&id).await.map_err(Ack::system)?,
            None => None,
        };
        let Some(art) = art else {
            // readpicture answers "no picture" with an empty response.
            return if picture { Ok(()) } else { Err(Ack::no_exist("No file exists")) };
        };
        let (data, mime) = &*art;
        if offset > data.len() {
            return Err(Ack::arg("Offset too large"));
        }
        let end = (offset + self.binary_limit).min(data.len());
        let _ = writeln!(out, "size: {}", data.len());
        if picture {
            let _ = writeln!(out, "type: {mime}");
        }
        out.binary(&data[offset..end]);
        Ok(())
    }
}
