//! libmpv backend.

use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use libmpv2::events::{Event, PropertyData};
use libmpv2::{Format, Mpv};
use serde::Deserialize;
use tokio::sync::mpsc;
use tracing::{debug, warn};

use super::backend::{AudioInfo, Backend, BackendEvent, Device, EntryId};

pub struct MpvBackend {
    mpv: Arc<Mpv>,
}

const OBSERVE_PLAYING_POS: u64 = 1;
const OBSERVE_IDLE: u64 = 2;
const OBSERVE_PAUSE: u64 = 3;

impl MpvBackend {
    pub fn new(events: mpsc::UnboundedSender<BackendEvent>) -> Result<Self> {
        let mpv = Mpv::with_initializer(|init| {
            for (name, value) in [
                ("idle", "yes"),
                ("vid", "no"),
                ("audio-display", "no"),
                ("gapless-audio", "yes"),
                ("prefetch-playlist", "yes"),
                ("audio-client-name", "npd"),
                ("load-scripts", "no"),
                ("ytdl", "no"),
                ("replaygain", "no"),
                ("terminal", "no"),
            ] {
                init.set_option(name, value)?;
            }
            Ok(())
        })
        .map_err(|e| anyhow!("initialising mpv: {e}"))?;
        let mpv = Arc::new(mpv);

        mpv.observe_property("playlist-playing-pos", Format::Int64, OBSERVE_PLAYING_POS)
            .map_err(|e| anyhow!("observing mpv property: {e}"))?;
        mpv.observe_property("idle-active", Format::Flag, OBSERVE_IDLE)
            .map_err(|e| anyhow!("observing mpv property: {e}"))?;
        mpv.observe_property("pause", Format::Flag, OBSERVE_PAUSE)
            .map_err(|e| anyhow!("observing mpv property: {e}"))?;

        let thread_mpv = Arc::clone(&mpv);
        std::thread::Builder::new()
            .name("mpv-events".into())
            .spawn(move || event_loop(&thread_mpv, &events))
            .context("spawning mpv event thread")?;
        Ok(Self { mpv })
    }

    fn command(&self, name: &str, args: &[&str]) -> Result<()> {
        self.mpv.command(name, args).map_err(|e| anyhow!("mpv {name}: {e}"))
    }

    fn entry_id(&self, index: i64) -> Result<EntryId> {
        self.mpv
            .get_property::<i64>(&format!("playlist/{index}/id"))
            .map_err(|e| anyhow!("reading mpv playlist entry: {e}"))
    }
}

fn event_loop(mpv: &Mpv, events: &mpsc::UnboundedSender<BackendEvent>) {
    let mut playing: Option<EntryId> = None;
    loop {
        let Some(event) = mpv.wait_event(-1.0) else { continue };
        let send = |e| {
            let _ = events.send(e);
        };
        match event {
            Ok(Event::Shutdown) => return,
            Ok(Event::PropertyChange { reply_userdata: OBSERVE_PLAYING_POS, change, .. }) => {
                let PropertyData::Int64(pos) = change else { continue };
                if pos < 0 {
                    playing = None;
                    continue;
                }
                // Resolve against mpv's playlist as it is now, not as it
                // was when the event was queued.
                let Ok(pos) = mpv.get_property::<i64>("playlist-playing-pos") else { continue };
                if pos < 0 {
                    continue;
                }
                if let Ok(id) = mpv.get_property::<i64>(&format!("playlist/{pos}/id"))
                    && playing != Some(id)
                {
                    playing = Some(id);
                    send(BackendEvent::Playing(id));
                }
            }
            Ok(Event::PropertyChange { reply_userdata: OBSERVE_IDLE, change: PropertyData::Flag(true), .. }) => {
                playing = None;
                send(BackendEvent::Idle);
            }
            Ok(Event::PropertyChange { reply_userdata: OBSERVE_PAUSE, change: PropertyData::Flag(p), .. }) => {
                send(BackendEvent::Pause(p));
            }
            Ok(_) => {}
            // libmpv2 reports a failed END_FILE (and failed property events)
            // as an error.
            Err(e) => {
                warn!("mpv: {e}");
                send(BackendEvent::Error(e.to_string()));
            }
        }
    }
}

impl Backend for MpvBackend {
    fn load(&self, url: &str, replace: bool, start: Option<f64>) -> Result<EntryId> {
        let options = start.map(|s| format!("start={s:.3}")).unwrap_or_default();
        let flag = if replace { "replace" } else { "append" };
        self.command("loadfile", &[url, flag, "-1", &options])?;
        let count = self
            .mpv
            .get_property::<i64>("playlist-count")
            .map_err(|e| anyhow!("reading mpv playlist: {e}"))?;
        let index = if replace { 0 } else { count - 1 };
        self.entry_id(index)
    }

    fn remove(&self, index: usize) -> Result<()> {
        self.command("playlist-remove", &[&index.to_string()])
    }

    fn stop(&self) -> Result<()> {
        self.command("stop", &[])
    }

    fn set_pause(&self, paused: bool) -> Result<()> {
        self.mpv.set_property("pause", paused).map_err(|e| anyhow!("mpv pause: {e}"))
    }

    fn seek(&self, seconds: f64) -> Result<()> {
        self.command("seek", &[&format!("{seconds:.3}"), "absolute"])
    }

    fn position(&self) -> Option<f64> {
        self.mpv.get_property::<f64>("time-pos").ok()
    }

    fn audio_info(&self) -> AudioInfo {
        let int = |name| self.mpv.get_property::<i64>(name).ok().filter(|&v| v > 0);
        let bits = self.mpv.get_property::<String>("audio-params/format").ok().map(|f| {
            // MPD's notation: bit count for integer samples, `f` for float.
            match f.trim_end_matches('p') {
                "u8" => "8".into(),
                "s16" => "16".into(),
                "s24" => "24".into(),
                "s32" => "32".into(),
                _ => "f".into(),
            }
        });
        AudioInfo {
            sample_rate: int("audio-params/samplerate").map(|v| v as u32),
            bits,
            channels: int("audio-params/channel-count").map(|v| v as u32),
            bitrate_kbps: self
                .mpv
                .get_property::<f64>("audio-bitrate")
                .ok()
                .filter(|&b| b > 0.0)
                .map(|b| (b / 1000.0).round() as u32),
        }
    }

    fn set_volume(&self, volume: u8) -> Result<()> {
        self.mpv
            .set_property("volume", f64::from(volume))
            .map_err(|e| anyhow!("mpv volume: {e}"))
    }

    fn devices(&self) -> Vec<Device> {
        #[derive(Deserialize)]
        struct Raw {
            name: String,
            description: String,
        }
        let json = match self.mpv.get_property::<String>("audio-device-list") {
            Ok(json) => json,
            Err(e) => {
                warn!("reading mpv audio devices: {e}");
                return Vec::new();
            }
        };
        match serde_json::from_str::<Vec<Raw>>(&json) {
            Ok(list) => list
                .into_iter()
                .filter(|d| d.name.starts_with("pipewire/"))
                .map(|d| Device { name: d.name, description: d.description })
                .collect(),
            Err(e) => {
                debug!("unexpected audio-device-list: {json}");
                warn!("parsing mpv audio devices: {e}");
                Vec::new()
            }
        }
    }

    fn set_device(&self, name: &str) -> Result<()> {
        self.mpv
            .set_property("audio-device", name)
            .map_err(|e| anyhow!("mpv audio-device: {e}"))
    }

    fn set_replay_gain(&self, mode: &str) -> Result<()> {
        let mode = if mode == "off" { "no" } else { mode };
        self.mpv
            .set_property("replaygain", mode)
            .map_err(|e| anyhow!("mpv replaygain: {e}"))
    }
}
