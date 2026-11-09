//! Reports plays to the server: "now playing" when a song starts, and a
//! scrobble once it has been listened to for half its length or four
//! minutes, whichever comes first (the Last.fm rule). Listening time is
//! counted while playing, so seeking ahead doesn't earn a scrobble.

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use tracing::{debug, warn};

use crate::player::{Player, State};
use crate::subsonic::Client;

const TICK: Duration = Duration::from_secs(2);
const MAX_THRESHOLD: Duration = Duration::from_secs(240);

/// Listening time needed before a song of `duration` seconds scrobbles.
fn threshold(duration: Option<u32>) -> Duration {
    match duration {
        Some(d) if d > 0 => (Duration::from_secs(u64::from(d)) / 2).min(MAX_THRESHOLD),
        _ => MAX_THRESHOLD,
    }
}

struct Play {
    plays: u64,
    song_id: String,
    started_ms: u64,
    listened: Duration,
    needed: Duration,
    announced: bool,
    scrobbled: bool,
}

pub fn spawn(player: Arc<Player>, client: Arc<Client>) {
    tokio::spawn(async move {
        let mut current: Option<Play> = None;
        let mut last_tick = Instant::now();
        let mut interval = tokio::time::interval(TICK);
        loop {
            interval.tick().await;
            let elapsed = last_tick.elapsed();
            last_tick = Instant::now();

            let Some((plays, song, state)) = player.now_playing() else {
                current = None;
                continue;
            };
            if current.as_ref().is_none_or(|p| p.plays != plays) {
                current = Some(Play {
                    plays,
                    song_id: song.id.clone(),
                    started_ms: now_ms(),
                    listened: Duration::ZERO,
                    needed: threshold(song.duration),
                    announced: false,
                    scrobbled: false,
                });
            } else if state == State::Play {
                // Credit the time since the last tick only if this play was
                // already underway then.
                let p = current.as_mut().unwrap();
                p.listened += elapsed.min(TICK * 2);
            }
            let p = current.as_mut().unwrap();
            if state != State::Play {
                continue;
            }
            if !p.announced {
                p.announced = true;
                debug!("now playing {}", p.song_id);
                if let Err(e) = client.scrobble(&p.song_id, false, None).await {
                    warn!("now playing: {e}");
                }
            }
            if !p.scrobbled && p.listened >= p.needed {
                p.scrobbled = true;
                debug!("scrobbling {}", p.song_id);
                if let Err(e) = client.scrobble(&p.song_id, true, Some(p.started_ms)).await {
                    warn!("scrobble: {e}");
                }
            }
        }
    });
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thresholds() {
        assert_eq!(threshold(Some(200)), Duration::from_secs(100));
        assert_eq!(threshold(Some(3600)), Duration::from_secs(240));
        assert_eq!(threshold(None), Duration::from_secs(240));
    }
}
