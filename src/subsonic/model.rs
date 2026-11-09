//! Subsonic / OpenSubsonic response types. Only fields npd uses are declared;
//! everything is optional or defaulted because servers vary.

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerInfo {
    pub version: String,
    #[serde(rename = "type")]
    pub server_type: Option<String>,
    pub server_version: Option<String>,
    #[serde(default)]
    pub open_subsonic: bool,
}

#[derive(Debug, Deserialize)]
pub struct Extension {
    pub name: String,
    #[serde(default)]
    pub versions: Vec<u32>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStatus {
    #[serde(default)]
    pub scanning: bool,
    pub count: Option<u64>,
    pub last_scan: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtistRef {
    pub id: Option<String>,
    pub name: String,
    pub sort_name: Option<String>,
    pub music_brainz_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedItem {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contributor {
    pub role: String,
    pub artist: ArtistRef,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReplayGain {
    pub track_gain: Option<f64>,
    pub track_peak: Option<f64>,
    pub album_gain: Option<f64>,
    pub album_peak: Option<f64>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ItemDate {
    pub year: Option<i32>,
    pub month: Option<u32>,
    pub day: Option<u32>,
}

impl ItemDate {
    /// `YYYY`, `YYYY-MM` or `YYYY-MM-DD`, as MPD clients expect for `Date`.
    pub fn to_mpd(&self) -> Option<String> {
        let year = self.year.filter(|&y| y > 0)?;
        Some(match (self.month, self.day) {
            (Some(m), Some(d)) if m > 0 && d > 0 => format!("{year:04}-{m:02}-{d:02}"),
            (Some(m), _) if m > 0 => format!("{year:04}-{m:02}"),
            _ => format!("{year:04}"),
        })
    }
}

/// A song (`Child` in the Subsonic schema).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Song {
    pub id: String,
    pub title: String,
    pub album: Option<String>,
    pub album_id: Option<String>,
    pub artist: Option<String>,
    pub track: Option<u32>,
    pub disc_number: Option<u32>,
    pub year: Option<i32>,
    pub genre: Option<String>,
    pub cover_art: Option<String>,
    pub size: Option<u64>,
    pub content_type: Option<String>,
    pub suffix: Option<String>,
    pub duration: Option<u32>,
    pub bit_rate: Option<u32>,
    pub sampling_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channel_count: Option<u32>,
    pub path: Option<String>,
    pub created: Option<String>,
    pub starred: Option<String>,
    pub user_rating: Option<u8>,
    pub comment: Option<String>,
    pub bpm: Option<u32>,
    pub sort_name: Option<String>,
    pub music_brainz_id: Option<String>,
    pub genres: Vec<NamedItem>,
    pub artists: Vec<ArtistRef>,
    pub display_artist: Option<String>,
    pub album_artists: Vec<ArtistRef>,
    pub display_album_artist: Option<String>,
    pub contributors: Vec<Contributor>,
    pub display_composer: Option<String>,
    pub replay_gain: Option<ReplayGain>,
}

/// An album (`AlbumID3` in the Subsonic schema).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Album {
    pub id: String,
    pub name: String,
    pub sort_name: Option<String>,
    pub music_brainz_id: Option<String>,
    pub year: Option<i32>,
    pub original_release_date: Option<ItemDate>,
    pub release_date: Option<ItemDate>,
    pub record_labels: Vec<NamedItem>,
    pub is_compilation: Option<bool>,
}

// Response bodies, keyed by their element name in the envelope.

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ExtensionsBody {
    #[serde(default)]
    pub open_subsonic_extensions: Vec<Extension>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct ScanStatusBody {
    pub scan_status: ScanStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct SearchResult3Body {
    pub search_result3: SearchResult3,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct SearchResult3 {
    pub song: Vec<Song>,
}

#[derive(Debug, Deserialize)]
pub(super) struct ArtistsBody {
    pub artists: ArtistsId3,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ArtistsId3 {
    pub index: Vec<ArtistIndex>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct ArtistIndex {
    pub artist: Vec<ArtistRef>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct AlbumList2Body {
    pub album_list2: AlbumList2,
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(super) struct AlbumList2 {
    pub album: Vec<Album>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn item_date() {
        let d = |year, month, day| ItemDate { year, month, day }.to_mpd();
        assert_eq!(d(Some(1999), Some(3), Some(7)).as_deref(), Some("1999-03-07"));
        assert_eq!(d(Some(1999), Some(3), None).as_deref(), Some("1999-03"));
        assert_eq!(d(Some(1999), None, None).as_deref(), Some("1999"));
        assert_eq!(d(Some(0), Some(1), Some(1)), None);
    }
}
