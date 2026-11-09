# npd roadmap

npd is an MPD-compatible daemon backed by Navidrome / OpenSubsonic. Existing MPD
clients (mpc, mpd-mpris, Cantata) talk to it unmodified; the library, playlists,
favorites, and ratings live on the Navidrome server.

## Goals

- **Low friction.** Runs as a systemd user service; MPRIS via mpd-mpris; play
  from a hotkey.
- **Navidrome is the source of truth** for playlists, favorites (stars), and
  ratings. Edits made in any MPD client write through; edits made elsewhere
  (web UI, phone) show up in MPD clients.
- **Gapless playback** (required).
- **PipeWire sink selection** from MPD clients (required).
- **Configurable cache:** stream only, prefetch N tracks ahead in the queue, or
  pin all favorites locally.

## Non-goals

- Full MPD protocol coverage. Target the command subset used by mpc,
  mpd-mpris, and Cantata; log unknown commands and add them as needed.
- A UI of any kind.
- Writing a decoder or audio pipeline (for now; see `Player` trait below).
- Local file libraries. Use real MPD for that.
- Working around client-side problems (e.g. Cantata's lag when scrolling
  album art). npd aims for MPD parity; parity-level performance is in scope,
  client quirks are not.

## Target environment

- Navidrome 0.64.2, ~107k tracks.
- Clients: mpc, mpd-mpris, Cantata.
- Rust, Linux, PipeWire, libmpv 2.x.

## Architecture

```
            mpc / mpd-mpris / Cantata
                       │  MPD protocol (TCP / unix socket)
┌──────────────────────┴───────────────────────────────┐
│ mpd/        parser, dispatch, command lists, idle    │
├──────────────┬──────────────┬──────────────┬─────────┤
│ queue/       │ library/     │ playlists/   │ cache/  │
│ versioned    │ SQLite       │ write-through│ LRU +   │
│ queue        │ mirror       │ + Favorites  │ pinned  │
├──────────────┴──────┬───────┴──────────────┴─────────┤
│ player/  Player trait → mpv backend (libmpv)         │
├─────────────────────┴────────────────────────────────┤
│ subsonic/  API client, auth, extension detection     │
└──────────────────────────────────────────────────────┘
```

### Key decisions

- **Playback via libmpv**, behind a `Player` trait. mpv already handles HTTP
  streaming with range seeking, gapless transitions, ReplayGain, and PipeWire
  output. A native backend (symphonia + pipewire-rs) stays possible later.
- **Gapless:** npd owns the queue and keeps mpv's internal playlist at
  `current + next`, with `gapless-audio=yes` and `prefetch-playlist=yes`.
- **Sinks as MPD outputs:** enumerate via mpv's `audio-device-list`, switch
  at runtime via `audio-device`. One synthetic "System default" output that
  follows the PipeWire default. Outputs are exclusive (enabling one disables
  the others), which matches `mpc enable only N`. Primary use: route to a
  dedicated GoXLR channel for a hardware volume fader.
- **Volume is mpv's** software volume (`setvol` ↔ mpv `volume`), not the
  PipeWire stream volume. It's an application-level control; hardware
  volume is handled by sink choice.
- **Sync is manual, full, and non-blocking.** No automatic incremental sync.
  `update` / `rescan` start a background full sync into a staging copy;
  clients keep querying the current mirror until an atomic swap. Progress
  shows as `updating_db: <job>` in `status`; completion emits the `database`
  and `update` idle events. Runs automatically only on first start (empty
  mirror).
- **Local SQLite metadata mirror.** MPD clients expect fast arbitrary tag
  queries (`list`, `find`, `search`) and full dumps (`listallinfo`); the
  Subsonic API is not built for that. The mirror also lets the library work
  offline when favorites are pinned.
- **URIs use Navidrome's `path` field** so they are stable, browsable with
  `lsinfo`, and resemble the existing MPD library layout. The mirror maps
  path ↔ Subsonic ID.
- **Favorites are a virtual stored playlist** (`Favorites`) backed by
  `getStarred2`. `playlistadd` stars, removal unstars.
- **Stickers map to Subsonic:** `rating` (Cantata, 0–10) ↔ `setRating`
  (1–5); `favorite` ↔ `star`/`unstar`. Lets a hotkey star the current track
  via `mpc sticker`.
- **Async Rust** (tokio). Config is TOML at `$XDG_CONFIG_HOME/npd/config.toml`;
  state/cache under `$XDG_STATE_HOME` / `$XDG_CACHE_HOME`.

## Phases

### Phase 0 — Skeleton

- [ ] Cargo project, config loading, logging (`tracing`).
- [ ] Subsonic client: token+salt auth (API key if offered), `ping`,
      `getOpenSubsonicExtensions`, error mapping.
- [ ] systemd user unit.

### Phase 1 — mpc and mpd-mpris

Goal: hotkey and Plasma widget work against Navidrome.

- [ ] Library mirror
  - [ ] Full sync (paginated `search3` with empty query) into a staging
        copy, then atomic swap. Never blocks queries or playback.
  - [ ] Triggered by `update` / `rescan` (and on first start). Report
        `updating_db` in `status`; emit `update` + `database` idle events.
  - [ ] Schema: songs, albums, artists, path ↔ ID, MBIDs, ReplayGain.
  - [ ] Queue entries and stored playlists survive a swap (keyed by Subsonic
        ID; tracks removed from the server become unplayable, not dropped
        silently).
- [ ] MPD protocol server
  - [ ] Listener on TCP and unix socket; greeting; `password` (optional).
  - [ ] Parser: quoting, command lists (`command_list_begin`/`_ok_begin`),
        `ACK` error codes.
  - [ ] `idle` / `noidle` with correct subsystem events.
  - [ ] Status: `status`, `currentsong`, `stats`, `ping`, `close`.
  - [ ] Library: `lsinfo`, `list`, `find`, `search`, `count`, `listall`.
  - [ ] Queue: `add`, `addid`, `delete`, `deleteid`, `clear`, `move`,
        `playlistinfo`, `playlistid`, `shuffle`.
  - [ ] Playback: `play`, `playid`, `pause`, `stop`, `next`, `previous`,
        `seek`, `seekid`, `seekcur`, `setvol`, `random`, `repeat`, `single`,
        `consume`.
  - [ ] Outputs: `outputs`, `enableoutput`, `disableoutput`, `toggleoutput`.
  - [ ] Art: `albumart`, `readpicture` via `getCoverArt`.
- [ ] Player
  - [ ] libmpv backend: load, gapless `current + next`, position/duration,
        end-of-track events, error handling.
  - [ ] Sink enumeration and switching.
  - [ ] ReplayGain mode from config.
  - [ ] Volume: `setvol` / `volume` / `getvol` ↔ mpv `volume`.

### Phase 2 — Cantata, playlists, favorites

- [ ] `listallinfo` streamed from SQLite (107k tracks is tens of MB; must not
      buffer the whole response).
- [ ] `plchanges` / `plchangesposid` (queue versioning).
- [ ] Stored playlists, write-through: `listplaylists`, `listplaylist`,
      `listplaylistinfo`, `load`, `save`, `playlistadd`, `playlistdelete`,
      `playlistmove`, `playlistclear`, `rename`, `rm`.
      `playlistmove` rewrites the whole list (`updatePlaylist` has no move).
      Smart playlists are read-only (`ACK` permission error).
- [ ] Poll Navidrome for playlist/star changes; emit `stored_playlist` idle.
- [ ] Virtual `Favorites` playlist.
- [ ] Stickers: `sticker get/set/delete/list/find` for `rating`, `favorite`.
- [ ] `tagtypes`, `decoders`, `commands`, `notcommands`, `urlhandlers`, and
      whatever else Cantata probes on connect.

### Phase 3 — Cache, scrobbling, handoff

- [ ] Cache store keyed by `(song id, format, bitrate)`; size-capped LRU plus
      a pinned set.
- [ ] Modes: `stream`, `prefetch = N`, `pin_favorites`. Cached files are
      used opportunistically in every mode.
- [ ] Originals via `download`; optional transcoded profile via `stream`
      (`format`, `maxBitRate`). Use Opus, not MP3, to keep transitions gapless.
- [ ] Scrobbling: `scrobble?submission=false` on start, `true` at 50% / 4 min.
- [ ] Play queue handoff via `savePlayQueue` / `getPlayQueue` (resume at home
      where you paused at work). Opt-in, on stop/pause and on startup.
- [ ] Local cover art cache so `albumart` / `readpicture` don't round-trip to
      Navidrome every time.

### Phase 4 — "Up next" queue (Spotify-style)

Separate the **context** (the album/playlist being played) from a user
**up-next** list that plays before the rest of the context. Clients still
see one flat MPD queue, so this needs no client changes:

- Songs inserted relative to the current song (`mpc insert`,
  `add <uri> +0` / `addid <uri> +N`, Cantata's "play next") are marked
  up-next. Plain `add` / `load` extend the context.
- Up-next songs form an ordered block right after the current song; new
  inserts go to the end of that block, not directly after current.
- Up-next songs are always consumed after playing, whatever the `consume`
  setting.
- `random` / `repeat` apply to the context only; up-next always plays next,
  in order.
- Replacing the context (`clear` + `load`/`add`) drops the up-next block
  too, which is plain MPD `clear` behaviour.

### Later / maybe

- Native playback backend (symphonia + PipeWire) to drop the mpv dependency.
- Multiple Navidrome servers / profiles (home vs. work network).
- `partition` support.
- `rescan` could also call Navidrome's `startScan` (admin only), wait for
  `getScanStatus` to finish, then sync. Until then, `update` and `rescan`
  behave the same.

## Open questions

- **Full sync cost.** ~215 `search3` requests at 500/page for 107k tracks.
  Measure wall time and server load; tune page size and concurrency.
- **Which library commands Cantata uses on connect** (`listallinfo` vs.
  recursive `lsinfo`). Determine from protocol logs once Phase 1 is up;
  only parity matters, not Cantata's own UX.
- **Path layout.** Check whether Navidrome's `path` values are unique and
  stable across rescans, and fall back to synthetic `Artist/Album/NN Title`
  paths if not.
