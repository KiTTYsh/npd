//! Rendering library objects in MPD's `key: value` response format.

use std::fmt::Write;

use crate::library::query::SongRow;
use crate::library::tags;

/// Which tags a client wants (`tagtypes`), as a bitmask over `tags::ALL`.
#[derive(Debug, Clone, Copy)]
pub struct TagMask(u32);

impl TagMask {
    pub fn all() -> Self {
        Self((1u32 << tags::ALL.len()) - 1)
    }

    pub fn none() -> Self {
        Self(0)
    }

    pub fn contains(self, tag: &str) -> bool {
        self.0 & (1 << tags::id(tag)) != 0
    }

    pub fn set(&mut self, tag: &str, on: bool) {
        let bit = 1 << tags::id(tag);
        if on { self.0 |= bit } else { self.0 &= !bit }
    }
}

/// `Last-Modified` wants `YYYY-MM-DDTHH:MM:SSZ`; Navidrome sends fractional
/// seconds.
fn timestamp(iso: &str) -> String {
    match iso.get(..19) {
        Some(prefix) if iso.len() > 19 => format!("{prefix}Z"),
        _ => iso.to_owned(),
    }
}

pub fn song(out: &mut String, song: &SongRow, mask: TagMask) {
    let _ = writeln!(out, "file: {}", song.uri);
    if let Some(added) = &song.added {
        let _ = writeln!(out, "Last-Modified: {}", timestamp(added));
    }
    if let Some(rate) = song.sample_rate.filter(|&r| r > 0) {
        let bits = match song.bit_depth {
            Some(b) if b > 0 => b.to_string(),
            _ => "f".into(), // lossy: MPD decodes to float
        };
        let channels = song.channels.filter(|&c| c > 0).unwrap_or(2);
        let _ = writeln!(out, "Format: {rate}:{bits}:{channels}");
    }
    let mut song_tags = song.tags();
    song_tags.sort_by_key(|(t, _)| tags::id(t));
    for (tag, value) in song_tags {
        if mask.contains(tag) {
            let _ = writeln!(out, "{tag}: {}", value.replace('\n', " "));
        }
    }
    if let Some(d) = song.duration {
        let _ = writeln!(out, "Time: {d}\nduration: {d}.000");
    }
}

pub fn directory(out: &mut String, path: &str) {
    let _ = writeln!(out, "directory: {path}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn song_rendering() {
        let row = SongRow {
            rowid: 1,
            id: "x".into(),
            uri: "A/B/c.flac".into(),
            duration: Some(61),
            sample_rate: Some(44100),
            bit_depth: Some(16),
            channels: Some(2),
            added: Some("2024-05-01T10:20:30.123456Z".into()),
            cover_art: None,
            tags: tags::encode(&[(tags::TITLE, "T".into()), (tags::ARTIST, "A".into()), (tags::COMMENT, "a\nb".into())]),
        };
        let mut mask = TagMask::all();
        let mut out = String::new();
        song(&mut out, &row, mask);
        assert_eq!(
            out,
            "file: A/B/c.flac\nLast-Modified: 2024-05-01T10:20:30Z\nFormat: 44100:16:2\n\
             Artist: A\nTitle: T\nComment: a b\nTime: 61\nduration: 61.000\n"
        );
        mask.set(tags::ARTIST, false);
        out.clear();
        song(&mut out, &row, mask);
        assert!(!out.contains("Artist"));
        assert!(TagMask::none().contains(tags::TITLE) == false);
    }
}
