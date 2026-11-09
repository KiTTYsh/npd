//! Mapping Subsonic songs onto MPD URIs and tags.

use crate::subsonic::model::{Album, Song};

/// Canonical MPD tag names. MPD matches tag names case-insensitively; the
/// spelling here is what npd sends to clients.
pub const TITLE: &str = "Title";
pub const ARTIST: &str = "Artist";
pub const ARTIST_SORT: &str = "ArtistSort";
pub const ALBUM: &str = "Album";
pub const ALBUM_SORT: &str = "AlbumSort";
pub const ALBUM_ARTIST: &str = "AlbumArtist";
pub const ALBUM_ARTIST_SORT: &str = "AlbumArtistSort";
pub const TRACK: &str = "Track";
pub const DISC: &str = "Disc";
pub const DATE: &str = "Date";
pub const ORIGINAL_DATE: &str = "OriginalDate";
pub const GENRE: &str = "Genre";
pub const COMPOSER: &str = "Composer";
pub const LABEL: &str = "Label";
pub const COMMENT: &str = "Comment";
pub const MB_TRACK_ID: &str = "MUSICBRAINZ_TRACKID";
pub const MB_ALBUM_ID: &str = "MUSICBRAINZ_ALBUMID";
pub const MB_ARTIST_ID: &str = "MUSICBRAINZ_ARTISTID";
pub const MB_ALBUM_ARTIST_ID: &str = "MUSICBRAINZ_ALBUMARTISTID";

/// Every tag npd can produce, in the order `tagtypes` lists them.
pub const ALL: &[&str] = &[
    ARTIST,
    ARTIST_SORT,
    ALBUM,
    ALBUM_SORT,
    ALBUM_ARTIST,
    ALBUM_ARTIST_SORT,
    TITLE,
    TRACK,
    GENRE,
    DATE,
    ORIGINAL_DATE,
    COMPOSER,
    DISC,
    LABEL,
    COMMENT,
    MB_ARTIST_ID,
    MB_ALBUM_ID,
    MB_ALBUM_ARTIST_ID,
    MB_TRACK_ID,
];

/// Looks up a tag name case-insensitively, returning its canonical spelling.
pub fn canonical(name: &str) -> Option<&'static str> {
    ALL.iter().copied().find(|t| t.eq_ignore_ascii_case(name))
}

fn nonempty(s: &Option<String>) -> Option<&str> {
    s.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The song's URI relative to the virtual music directory.
///
/// Navidrome reports the real file path only when "Report Real Path" is
/// enabled for the player; otherwise it reports a synthetic
/// `Artist/Album/NN - Title.ext` path. Either works; real paths are more
/// stable. Without any path, npd builds a similar synthetic one.
pub fn song_uri(song: &Song) -> String {
    if let Some(path) = nonempty(&song.path) {
        return path.trim_start_matches('/').to_owned();
    }
    let clean = |s: &str| s.replace('/', "_");
    let artist = nonempty(&song.display_album_artist)
        .or(nonempty(&song.artist))
        .unwrap_or("Unknown Artist");
    let album = nonempty(&song.album).unwrap_or("Unknown Album");
    let track = match (song.disc_number, song.track) {
        (Some(d), Some(t)) if d > 1 => format!("{d}-{t:02} - "),
        (_, Some(t)) => format!("{t:02} - "),
        _ => String::new(),
    };
    let suffix = nonempty(&song.suffix).unwrap_or("audio");
    format!(
        "{}/{}/{}{}.{}",
        clean(artist),
        clean(album),
        track,
        clean(&song.title),
        suffix
    )
}

/// Makes `uri` unique by inserting the song ID before the extension.
pub fn disambiguate(uri: &str, id: &str) -> String {
    let (stem, ext) = match uri.rfind('.') {
        Some(dot) if !uri[dot..].contains('/') => uri.split_at(dot),
        _ => (uri, ""),
    };
    format!("{stem} [{id}]{ext}")
}

/// Parent directory of a URI; `""` is the root.
pub fn parent_dir(uri: &str) -> &str {
    uri.rfind('/').map_or("", |i| &uri[..i])
}

/// The deepest directory containing all of `dirs`; `""` if none.
pub fn common_dir<'a>(mut dirs: impl Iterator<Item = &'a str>) -> String {
    let Some(first) = dirs.next() else {
        return String::new();
    };
    let mut common: Vec<&str> = first.split('/').filter(|c| !c.is_empty()).collect();
    for dir in dirs {
        let shared = common
            .iter()
            .zip(dir.split('/'))
            .take_while(|(a, b)| a == &b)
            .count();
        common.truncate(shared);
        if common.is_empty() {
            break;
        }
    }
    common.join("/")
}

/// `dir` relative to `root`, which must contain it.
pub fn strip_dir<'a>(dir: &'a str, root: &str) -> &'a str {
    if dir == root {
        ""
    } else {
        &dir[root.len() + 1..]
    }
}

/// All MPD tags for a song, as `(tag, value)` pairs. Multi-valued tags
/// (Genre, MusicBrainz artist IDs, Label) appear once per value.
///
/// Artist and AlbumArtist use Navidrome's display strings ("A feat. B"),
/// matching what a conventionally tagged file gives real MPD.
pub fn song_tags(song: &Song, album: Option<&Album>) -> Vec<(&'static str, String)> {
    let mut tags = Vec::new();
    let mut push = |tag, value: Option<&str>| {
        if let Some(v) = value.map(str::trim).filter(|v| !v.is_empty()) {
            tags.push((tag, v.to_owned()));
        }
    };

    push(TITLE, Some(&song.title));

    push(ARTIST, nonempty(&song.display_artist).or(nonempty(&song.artist)));
    if let [only] = song.artists.as_slice() {
        push(ARTIST_SORT, only.sort_name.as_deref());
    }
    for artist in &song.artists {
        push(MB_ARTIST_ID, artist.music_brainz_id.as_deref());
    }

    push(ALBUM, song.album.as_deref());
    push(ALBUM_ARTIST, nonempty(&song.display_album_artist));
    if let [only] = song.album_artists.as_slice() {
        push(ALBUM_ARTIST_SORT, only.sort_name.as_deref());
    }
    for artist in &song.album_artists {
        push(MB_ALBUM_ARTIST_ID, artist.music_brainz_id.as_deref());
    }

    push(TRACK, song.track.filter(|&t| t > 0).map(|t| t.to_string()).as_deref());
    push(DISC, song.disc_number.filter(|&d| d > 0).map(|d| d.to_string()).as_deref());

    if song.genres.is_empty() {
        push(GENRE, song.genre.as_deref());
    }
    for genre in &song.genres {
        push(GENRE, Some(&genre.name));
    }

    let composer = nonempty(&song.display_composer).map(str::to_owned).or_else(|| {
        let names: Vec<_> = song
            .contributors
            .iter()
            .filter(|c| c.role == "composer")
            .map(|c| c.artist.name.as_str())
            .collect();
        (!names.is_empty()).then(|| names.join(", "))
    });
    push(COMPOSER, composer.as_deref());

    push(COMMENT, song.comment.as_deref());
    push(MB_TRACK_ID, song.music_brainz_id.as_deref());

    let year = song.year.filter(|&y| y > 0).map(|y| y.to_string());
    match album {
        Some(album) => {
            push(ALBUM_SORT, album.sort_name.as_deref());
            push(MB_ALBUM_ID, album.music_brainz_id.as_deref());
            let date = album.release_date.as_ref().and_then(|d| d.to_mpd());
            push(DATE, date.as_deref().or(year.as_deref()));
            let original = album.original_release_date.as_ref().and_then(|d| d.to_mpd());
            if original != date {
                push(ORIGINAL_DATE, original.as_deref());
            }
            for label in &album.record_labels {
                push(LABEL, Some(&label.name));
            }
        }
        None => push(DATE, year.as_deref()),
    }

    tags
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::subsonic::model::{ArtistRef, ItemDate, NamedItem};

    fn song() -> Song {
        Song {
            id: "s1".into(),
            title: "Song".into(),
            album: Some("Album".into()),
            artist: Some("A".into()),
            display_artist: Some("A feat. B".into()),
            artists: vec![
                ArtistRef { name: "A".into(), music_brainz_id: Some("mb-a".into()), ..Default::default() },
                ArtistRef { name: "B".into(), ..Default::default() },
            ],
            display_album_artist: Some("A".into()),
            album_artists: vec![ArtistRef {
                name: "A".into(),
                sort_name: Some("A, The".into()),
                ..Default::default()
            }],
            track: Some(3),
            disc_number: Some(1),
            year: Some(2001),
            genres: vec![NamedItem { name: "Rock".into() }, NamedItem { name: "Pop".into() }],
            suffix: Some("flac".into()),
            ..Default::default()
        }
    }

    fn get<'a>(tags: &'a [(&str, String)], tag: &str) -> Vec<&'a str> {
        tags.iter().filter(|(t, _)| *t == tag).map(|(_, v)| v.as_str()).collect()
    }

    #[test]
    fn tags_without_album() {
        let tags = song_tags(&song(), None);
        assert_eq!(get(&tags, ARTIST), ["A feat. B"]);
        assert_eq!(get(&tags, ARTIST_SORT), Vec::<&str>::new());
        assert_eq!(get(&tags, ALBUM_ARTIST_SORT), ["A, The"]);
        assert_eq!(get(&tags, MB_ARTIST_ID), ["mb-a"]);
        assert_eq!(get(&tags, GENRE), ["Rock", "Pop"]);
        assert_eq!(get(&tags, TRACK), ["3"]);
        assert_eq!(get(&tags, DATE), ["2001"]);
    }

    #[test]
    fn tags_with_album() {
        let album = Album {
            id: "al1".into(),
            music_brainz_id: Some("mb-album".into()),
            release_date: Some(ItemDate { year: Some(2001), month: Some(5), day: Some(2) }),
            original_release_date: Some(ItemDate { year: Some(1999), month: None, day: None }),
            record_labels: vec![NamedItem { name: "Label".into() }],
            ..Default::default()
        };
        let tags = song_tags(&song(), Some(&album));
        assert_eq!(get(&tags, DATE), ["2001-05-02"]);
        assert_eq!(get(&tags, ORIGINAL_DATE), ["1999"]);
        assert_eq!(get(&tags, MB_ALBUM_ID), ["mb-album"]);
        assert_eq!(get(&tags, LABEL), ["Label"]);
    }

    #[test]
    fn uris() {
        let mut s = song();
        assert_eq!(song_uri(&s), "A/Album/03 - Song.flac");
        s.disc_number = Some(2);
        s.title = "AC/DC".into();
        assert_eq!(song_uri(&s), "A/Album/2-03 - AC_DC.flac");
        s.path = Some("/Music/x.flac".into());
        assert_eq!(song_uri(&s), "Music/x.flac");
        assert_eq!(disambiguate("a.b/c.flac", "id"), "a.b/c [id].flac");
        assert_eq!(disambiguate("a.b/c", "id"), "a.b/c [id]");
        assert_eq!(parent_dir("a/b/c.flac"), "a/b");
        assert_eq!(parent_dir("c.flac"), "");
    }

    #[test]
    fn common_dirs() {
        let common = |d: &[&str]| common_dir(d.iter().copied());
        assert_eq!(common(&["music/A/X", "music/B/Y", "music/A"]), "music");
        assert_eq!(common(&["music/A/X", "music/A/Y"]), "music/A");
        assert_eq!(common(&["music/A", "musicals/B"]), "");
        assert_eq!(common(&["music/A", ""]), "");
        assert_eq!(common(&[]), "");
        assert_eq!(strip_dir("music/A/X", "music"), "A/X");
        assert_eq!(strip_dir("music", "music"), "");
    }

    #[test]
    fn canonical_names() {
        assert_eq!(canonical("albumartist"), Some(ALBUM_ARTIST));
        assert_eq!(canonical("musicbrainz_trackid"), Some(MB_TRACK_ID));
        assert_eq!(canonical("nope"), None);
    }
}
