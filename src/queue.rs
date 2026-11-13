//! The play queue ("current playlist" in MPD terms): ordering, IDs,
//! per-position change versions for `plchanges`, and play-order rules.

use std::ops::Range;
use std::sync::Arc;

use rand::seq::SliceRandom;

use crate::library::query::SongRow;

pub type ItemId = u32;

#[derive(Debug, Clone)]
pub struct Item {
    pub id: ItemId,
    pub song: Arc<SongRow>,
    /// Queued to play next (Spotify-style "add to queue") rather than part
    /// of the context (the album or playlist being played).
    pub up_next: bool,
    /// Queue version at which this position last changed.
    changed: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Single {
    #[default]
    Off,
    On,
    /// Stop after the current song, then turn single off.
    Oneshot,
}

/// The queue is one flat list, as MPD clients see it, made of a *context*
/// and an *up-next* block: songs queued to play next, kept right after the
/// current song. Up-next songs play in order before the context continues
/// (random and repeat apply to the context only), and are removed once
/// playback moves past them.
#[derive(Debug, Default)]
pub struct Queue {
    items: Vec<Item>,
    next_id: ItemId,
    version: u32,
    current: Option<usize>,
    pub repeat: bool,
    pub consume: bool,
    pub single: Single,
    random: bool,
    /// Random play order of the context songs, by ID: `order[..played]`
    /// have played this cycle, the last of them being the current context
    /// song, and the rest are still to play.
    order: Vec<ItemId>,
    played: usize,
}

impl Queue {
    pub fn new() -> Self {
        Self { next_id: 1, version: 1, ..Default::default() }
    }

    pub fn version(&self) -> u32 {
        self.version
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn get(&self, pos: usize) -> Option<&Item> {
        self.items.get(pos)
    }

    pub fn position_of(&self, id: ItemId) -> Option<usize> {
        self.items.iter().position(|i| i.id == id)
    }

    pub fn current(&self) -> Option<usize> {
        self.current
    }

    pub fn current_item(&self) -> Option<&Item> {
        self.current.and_then(|p| self.items.get(p))
    }

    pub fn random(&self) -> bool {
        self.random
    }

    pub fn set_random(&mut self, on: bool) {
        // Clients re-send their state; that mustn't lose the history.
        if self.random == on {
            return;
        }
        self.random = on;
        self.reshuffle();
    }

    /// Items changed after `version`, with their positions.
    pub fn changes_since(&self, version: u32) -> impl Iterator<Item = (usize, &Item)> {
        // A client with a future version (e.g. after a restart) gets everything.
        let version = if version > self.version { 0 } else { version };
        self.items.iter().enumerate().filter(move |(_, i)| i.changed > version)
    }

    fn bump(&mut self, positions: Range<usize>) {
        self.version = self.version.wrapping_add(1).max(1);
        let v = self.version;
        for item in &mut self.items[positions] {
            item.changed = v;
        }
    }

    /// The context song playback continues from: the current song, or when
    /// that is an up-next song, the context song before it.
    fn anchor(&self, pos: usize) -> Option<usize> {
        (0..=pos).rev().find(|&p| !self.items[p].up_next)
    }

    /// Where the up-next block ends (the position after it). Without a
    /// current song there is no block.
    pub fn up_next_end(&self) -> Option<usize> {
        let cur = self.current?;
        let mut end = cur + 1;
        while self.items.get(end).is_some_and(|i| i.up_next) {
            end += 1;
        }
        Some(end)
    }

    /// A fresh random order: the current context song, then the rest of the
    /// context shuffled.
    fn reshuffle(&mut self) {
        self.order.clear();
        if !self.random {
            return;
        }
        let anchor = self.current.and_then(|c| self.anchor(c)).map(|a| self.items[a].id);
        let mut rest: Vec<ItemId> =
            self.items.iter().filter(|i| !i.up_next && Some(i.id) != anchor).map(|i| i.id).collect();
        rest.shuffle(&mut rand::rng());
        self.order.extend(anchor);
        self.order.extend(rest);
        self.played = usize::from(anchor.is_some());
    }

    /// Adds context songs to the random order at random places among the
    /// songs still to play, leaving what's played (and the order so far)
    /// alone.
    fn order_insert(&mut self, ids: impl IntoIterator<Item = ItemId>) {
        if !self.random {
            return;
        }
        let first = self.played.min(self.order.len());
        for id in ids {
            let at = rand::random_range(first..=self.order.len());
            self.order.insert(at, id);
        }
    }

    /// Drops songs from the random order, played or not.
    fn order_remove(&mut self, ids: &[ItemId]) {
        self.played -= self.order[..self.played].iter().filter(|id| ids.contains(id)).count();
        self.order.retain(|id| !ids.contains(id));
    }

    /// Plays `id` next in the random order, wherever it was.
    fn order_push(&mut self, id: ItemId) {
        self.order_remove(&[id]);
        self.order.insert(self.played, id);
        self.played += 1;
    }

    /// Moves the random order onto `id`, which now plays after `old` (the
    /// context song playing before, if any).
    fn order_play(&mut self, id: ItemId, old: Option<ItemId>) {
        match self.order.iter().position(|&i| i == id) {
            // Round again after the whole cycle played: a fresh order,
            // keeping the song just played for `previous`.
            Some(0) if self.played == self.order.len() && self.order.len() > 1 => {
                let old = old.filter(|&o| o != id && self.order.contains(&o));
                let mut rest: Vec<ItemId> =
                    self.order.iter().copied().filter(|&i| i != id && Some(i) != old).collect();
                rest.shuffle(&mut rand::rng());
                self.order.clear();
                self.order.extend(old);
                self.order.push(id);
                self.played = self.order.len();
                self.order.extend(rest);
            }
            // Back to a song already played: the songs since come up again.
            Some(i) if i < self.played => self.played = i + 1,
            _ => self.order_push(id),
        }
    }

    /// Keeps the up-next flags on a contiguous block right after the current
    /// song; songs left elsewhere (moved, shuffled, current gone) rejoin the
    /// context.
    fn normalize(&mut self) {
        let block = match self.current {
            Some(cur) => {
                let start = if self.items[cur].up_next { self.anchor(cur).map_or(0, |a| a + 1) } else { cur + 1 };
                start..self.up_next_end().unwrap_or(start)
            }
            None => 0..0,
        };
        let mut rejoined = Vec::new();
        for (p, item) in self.items.iter_mut().enumerate() {
            if item.up_next && !block.contains(&p) && Some(p) != self.current {
                item.up_next = false;
                rejoined.push(item.id);
            }
        }
        self.order_insert(rejoined);
    }

    /// Inserts songs at `at` (default: the end). Returns their IDs.
    pub fn add(&mut self, songs: impl IntoIterator<Item = Arc<SongRow>>, at: Option<usize>) -> Vec<ItemId> {
        self.insert(songs, at, false)
    }

    /// Queues songs to play next, after any already queued. Without a
    /// current song they simply join the end of the context.
    pub fn add_up_next(&mut self, songs: impl IntoIterator<Item = Arc<SongRow>>) -> Vec<ItemId> {
        match self.up_next_end() {
            Some(end) => self.insert(songs, Some(end), true),
            None => self.insert(songs, None, false),
        }
    }

    fn insert(&mut self, songs: impl IntoIterator<Item = Arc<SongRow>>, at: Option<usize>, up_next: bool) -> Vec<ItemId> {
        let at = at.unwrap_or(self.items.len()).min(self.items.len());
        let new: Vec<Item> = songs
            .into_iter()
            .map(|song| {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                Item { id, song, up_next, changed: 0 }
            })
            .collect();
        let ids: Vec<ItemId> = new.iter().map(|i| i.id).collect();
        let n = new.len();
        if n == 0 {
            return ids;
        }
        self.items.splice(at..at, new);
        if let Some(cur) = self.current.as_mut()
            && *cur >= at
        {
            *cur += n;
        }
        let len = self.items.len();
        self.bump(at..len);
        if self.random && self.order.is_empty() && !up_next {
            self.reshuffle();
        } else if !up_next {
            self.order_insert(ids.iter().copied());
        }
        // Context songs inserted into the block split it.
        self.normalize();
        ids
    }

    /// Marks songs (by position) as up-next, e.g. when restoring state.
    pub fn mark_up_next(&mut self, positions: &[usize]) {
        for &p in positions {
            if let Some(item) = self.items.get_mut(p) {
                item.up_next = true;
            }
        }
        let marked: Vec<ItemId> = positions.iter().filter_map(|&p| self.items.get(p)).map(|i| i.id).collect();
        self.order_remove(&marked);
        self.normalize();
    }

    /// Removes positions in `range`. Returns whether the current song was
    /// among them; the current position then points at the song that took
    /// its place, if any, which playback continues from.
    pub fn delete(&mut self, range: Range<usize>) -> bool {
        let removed_current = self.remove_range(range);
        if removed_current
            && self.random
            && let Some(id) = self.current_item().filter(|i| !i.up_next).map(|i| i.id)
        {
            self.order_push(id);
        }
        removed_current
    }

    /// `delete` without re-anchoring the random order on the replacement.
    fn remove_range(&mut self, range: Range<usize>) -> bool {
        let range = range.start.min(self.items.len())..range.end.min(self.items.len());
        if range.is_empty() {
            return false;
        }
        let n = range.len();
        let removed: Vec<ItemId> = self.items.drain(range.clone()).map(|i| i.id).collect();
        self.order_remove(&removed);
        let mut removed_current = false;
        if let Some(cur) = self.current {
            if range.contains(&cur) {
                removed_current = true;
                self.current = (range.start < self.items.len()).then_some(range.start);
            } else if cur >= range.end {
                self.current = Some(cur - n);
            }
        }
        let len = self.items.len();
        self.bump(range.start.min(len)..len);
        self.normalize();
        removed_current
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.bump(0..0);
        self.order.clear();
        self.played = 0;
    }

    /// Moves the songs in `range` so the first lands at `to` (a position in
    /// the queue as it is after removing them). Songs moved right after the
    /// current song, or into the up-next block, become up-next.
    pub fn move_range(&mut self, range: Range<usize>, to: usize) -> Result<(), String> {
        if range.end > self.items.len() || range.start > range.end {
            return Err("Bad song index".into());
        }
        if to + range.len() > self.items.len() {
            return Err("Bad song index".into());
        }
        let current_id = self.current_item().map(|i| i.id);
        let mut moved: Vec<Item> = self.items.drain(range.clone()).collect();
        // What precedes the landing spot decides whether they join the block.
        let moves_current = moved.iter().any(|i| Some(i.id) == current_id);
        let joins = !moves_current
            && to.checked_sub(1).and_then(|p| self.items.get(p)).is_some_and(|prev| {
                Some(prev.id) == current_id || (prev.up_next && current_id.is_some())
            });
        // Context songs moved right after the current song are "play next"
        // (that's how `mpc insert` queues): they line up behind songs
        // already queued. Reordering within the block keeps the spot given.
        let mut to = to;
        let after_current = to.checked_sub(1).and_then(|p| self.items.get(p)).map(|i| i.id) == current_id;
        if joins && after_current && moved.iter().all(|i| !i.up_next) {
            while self.items.get(to).is_some_and(|i| i.up_next) {
                to += 1;
            }
        }
        let rejoining: Vec<ItemId> = moved.iter().filter(|i| i.up_next && !joins).map(|i| i.id).collect();
        for item in &mut moved {
            item.up_next = joins || item.up_next;
        }
        let joined: Vec<ItemId> = if joins { moved.iter().map(|i| i.id).collect() } else { Vec::new() };
        self.items.splice(to..to, moved);
        self.current = current_id.and_then(|id| self.position_of(id));
        self.order_remove(&joined);
        let lo = range.start.min(to);
        let hi = range.end.max(to + range.len()).min(self.items.len());
        self.bump(lo..hi);
        if !rejoining.is_empty() {
            for id in &rejoining {
                if let Some(p) = self.position_of(*id) {
                    self.items[p].up_next = false;
                }
            }
            self.order_insert(rejoining);
        }
        self.normalize();
        Ok(())
    }

    pub fn shuffle(&mut self, range: Range<usize>) {
        let range = range.start.min(self.items.len())..range.end.min(self.items.len());
        let current_id = self.current_item().map(|i| i.id);
        self.items[range.clone()].shuffle(&mut rand::rng());
        self.current = current_id.and_then(|id| self.position_of(id));
        self.bump(range);
        self.normalize();
    }

    /// Makes `pos` current (or none). Moving off an up-next song removes it.
    pub fn set_current(&mut self, pos: Option<usize>) {
        let target = pos.and_then(|p| self.items.get(p)).map(|i| i.id);
        let old_anchor = self.current.and_then(|c| self.anchor(c)).map(|a| self.items[a].id);
        if let Some(cur) = self.current
            && self.items[cur].up_next
            && Some(self.items[cur].id) != target
        {
            self.items.remove(cur);
            self.bump(cur..self.items.len());
        }
        self.current = target.and_then(|id| self.position_of(id));
        if self.random {
            match self.current_item().filter(|i| !i.up_next).map(|i| i.id) {
                Some(id) if Some(id) != old_anchor => self.order_play(id, old_anchor),
                Some(_) => {}
                None if self.current.is_none() => self.reshuffle(),
                None => {}
            }
        }
        self.normalize();
    }

    /// The position that follows `pos` in play order: the up-next block
    /// first, then the context, honouring repeat, random and single.
    pub fn next_after(&self, pos: usize) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if self.single != Single::Off {
            return self.repeat.then_some(pos);
        }
        if self.items.get(pos + 1).is_some_and(|i| i.up_next) {
            return Some(pos + 1);
        }
        let anchor = self.anchor(pos);
        let is_context = |p: &usize| !self.items[*p].up_next;
        if self.random {
            let next = self.order[self.played.min(self.order.len())..].iter().find_map(|&id| self.position_of(id));
            return match next {
                Some(p) => Some(p),
                None if self.repeat => self.order.first().and_then(|&id| self.position_of(id)),
                None => None,
            };
        }
        let start = anchor.map_or(0, |a| a + 1);
        (start..self.items.len())
            .find(is_context)
            .or_else(|| if self.repeat { (0..self.items.len()).find(is_context) } else { None })
    }

    /// The song after the current one, if playback will continue.
    pub fn next(&self) -> Option<usize> {
        self.current.and_then(|c| self.next_after(c))
    }

    /// The song before `pos` in play order. From an up-next song, that is
    /// the context song it was queued after.
    pub fn previous_before(&self, pos: usize) -> Option<usize> {
        if self.items[pos].up_next {
            return self.anchor(pos);
        }
        let is_context = |p: &usize| !self.items[*p].up_next;
        if self.random {
            let idx = self.order.iter().position(|&id| id == self.items[pos].id)?;
            return match idx.checked_sub(1) {
                Some(i) => self.position_of(self.order[i]),
                None if self.repeat => self.order.last().and_then(|&id| self.position_of(id)),
                None => None,
            };
        }
        (0..pos)
            .rev()
            .find(is_context)
            .or_else(|| if self.repeat { (0..self.items.len()).rev().find(is_context) } else { None })
    }

    /// Advances after the current song finished naturally: applies consume
    /// (always, for up-next songs) and single-oneshot. Returns the new
    /// current position.
    pub fn advance(&mut self) -> Option<usize> {
        let cur = self.current?;
        let next = self.next_after(cur);
        if self.single == Single::Oneshot {
            self.single = Single::Off;
        }
        let next_id = next.map(|p| self.items[p].id);
        let old_anchor = self.anchor(cur).map(|a| self.items[a].id);
        let finished_context = !self.items[cur].up_next;
        if self.consume || !finished_context {
            self.remove_range(cur..cur + 1);
        }
        self.current = next_id.and_then(|id| self.position_of(id));
        if self.random
            && let Some(id) = self.current_item().filter(|i| !i.up_next).map(|i| i.id)
            && Some(id) != old_anchor
        {
            self.order_play(id, old_anchor);
        }
        self.normalize();
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(n: usize) -> Arc<SongRow> {
        Arc::new(SongRow {
            album_id: None,
            id: format!("s{n}"),
            uri: format!("{n}.flac"),
            duration: Some(100),
            sample_rate: None,
            bit_depth: None,
            channels: None,
            added: None,
            cover_art: None,
            tags: Vec::new(),
        })
    }

    /// The `n` a test song was made from.
    fn n(item: &Item) -> usize {
        item.song.uri.trim_end_matches(".flac").parse().unwrap()
    }

    fn queue(n: usize) -> Queue {
        let mut q = Queue::new();
        q.add((0..n).map(song), None);
        q
    }

    fn uris(q: &Queue) -> Vec<String> {
        q.items().iter().map(|i| i.song.uri.clone()).collect()
    }

    fn up(q: &Queue) -> Vec<String> {
        q.items().iter().filter(|i| i.up_next).map(|i| i.song.uri.clone()).collect()
    }

    /// Plays to the end, returning the URIs in the order they played.
    fn play_out(q: &mut Queue) -> Vec<String> {
        let mut played = vec![q.current_item().unwrap().song.uri.clone()];
        while q.advance().is_some() {
            played.push(q.current_item().unwrap().song.uri.clone());
        }
        played
    }

    #[test]
    fn up_next_plays_first_in_order_and_is_consumed() {
        let mut q = queue(4);
        q.set_current(Some(1));
        q.add_up_next([song(10)]);
        q.add_up_next([song(11)]);
        assert_eq!(uris(&q), ["0.flac", "1.flac", "10.flac", "11.flac", "2.flac", "3.flac"]);
        assert_eq!(up(&q), ["10.flac", "11.flac"]);
        assert_eq!(play_out(&mut q), ["1.flac", "10.flac", "11.flac", "2.flac", "3.flac"]);
        assert_eq!(uris(&q), ["0.flac", "1.flac", "2.flac", "3.flac"], "up-next songs are gone after playing");
    }

    #[test]
    fn up_next_ignores_random_and_repeat() {
        let mut q = queue(10);
        q.set_current(Some(0));
        q.set_random(true);
        q.add_up_next([song(20), song(21)]);
        q.advance();
        assert_eq!(q.current_item().unwrap().song.uri, "20.flac");
        q.advance();
        assert_eq!(q.current_item().unwrap().song.uri, "21.flac");
        let mut played = play_out(&mut q);
        played.remove(0);
        played.push("0.flac".into());
        played.sort();
        let mut want: Vec<String> = (0..10).map(|n| format!("{n}.flac")).collect();
        want.sort();
        assert_eq!(played, want, "the context still plays each song once");
    }

    #[test]
    fn adding_in_random_mode_keeps_the_order_so_far() {
        let mut q = queue(20);
        q.set_current(Some(0));
        q.set_random(true);
        let mut played = vec![0];
        for _ in 0..10 {
            played.push(q.advance().map(|p| n(&q.items()[p])).unwrap());
        }
        q.add((20..25).map(song), None);
        while let Some(p) = q.advance() {
            played.push(n(&q.items()[p]));
        }
        played.sort();
        assert_eq!(played, (0..25).collect::<Vec<_>>(), "nothing repeats, nothing is skipped");
    }

    #[test]
    fn skipping_and_jumping_drop_up_next() {
        let mut q = queue(3);
        q.set_current(Some(0));
        q.add_up_next([song(10), song(11)]);
        q.advance();
        assert_eq!(q.current_item().unwrap().song.uri, "10.flac");
        // Previous from an up-next song returns to the context song and
        // drops the one that was playing.
        let prev = q.previous_before(q.current().unwrap());
        q.set_current(prev);
        assert_eq!(q.current_item().unwrap().song.uri, "0.flac");
        assert_eq!(uris(&q), ["0.flac", "11.flac", "1.flac", "2.flac"]);
        // Jumping into the context leaves the rest of the queue alone, but
        // the block is no longer after the current song.
        q.set_current(Some(3));
        assert!(up(&q).is_empty());
    }

    #[test]
    fn moves_join_and_leave_the_block() {
        let mut q = queue(5);
        q.set_current(Some(0));
        q.move_range(4..5, 1).unwrap();
        assert_eq!(up(&q), ["4.flac"], "moved right after current");
        q.move_range(3..4, 2).unwrap();
        assert_eq!(up(&q), ["4.flac", "2.flac"], "moved into the block's end");
        q.move_range(1..2, 4).unwrap();
        assert_eq!(up(&q), ["2.flac"], "moved out of the block");
        assert_eq!(q.next_after(0), Some(1));
    }

    #[test]
    fn mpc_insert_lines_up_behind_queued_songs() {
        // `mpc insert` is `add` then `move <end> <current + 1>`.
        let mut q = queue(3);
        q.set_current(Some(0));
        for n in [10, 11] {
            q.add([song(n)], None);
            let last = q.len() - 1;
            q.move_range(last..last + 1, 1).unwrap();
        }
        assert_eq!(uris(&q), ["0.flac", "10.flac", "11.flac", "1.flac", "2.flac"]);
        assert_eq!(up(&q), ["10.flac", "11.flac"]);
        // Moving a queued song to the front of the block reorders it.
        q.move_range(2..3, 1).unwrap();
        assert_eq!(up(&q), ["11.flac", "10.flac"]);
    }

    #[test]
    fn add_and_ids() {
        let mut q = queue(3);
        assert_eq!(q.items().iter().map(|i| i.id).collect::<Vec<_>>(), [1, 2, 3]);
        q.set_current(Some(1));
        let ids = q.add([song(9)], Some(0));
        assert_eq!(ids, [4]);
        assert_eq!(uris(&q), ["9.flac", "0.flac", "1.flac", "2.flac"]);
        assert_eq!(q.current(), Some(2), "current follows its song");
    }

    #[test]
    fn plchanges_versions() {
        let mut q = queue(4);
        let v = q.version();
        q.move_range(3..4, 1).unwrap();
        let changed: Vec<usize> = q.changes_since(v).map(|(p, _)| p).collect();
        assert_eq!(changed, [1, 2, 3]);
        let v = q.version();
        q.delete(0..1);
        let changed: Vec<usize> = q.changes_since(v).map(|(p, _)| p).collect();
        assert_eq!(changed, [0, 1, 2]);
        assert_eq!(q.changes_since(0).count(), 3);
        assert_eq!(q.changes_since(q.version() + 5).count(), 3);
    }

    #[test]
    fn delete_current() {
        let mut q = queue(3);
        q.set_current(Some(1));
        assert!(q.delete(1..2));
        assert_eq!(q.current(), Some(1), "the next song takes its place");
        assert_eq!(q.current_item().unwrap().song.uri, "2.flac");
        assert!(q.delete(1..2));
        assert_eq!(q.current(), None);
        assert!(!q.delete(5..9));
    }

    #[test]
    fn moving() {
        let mut q = queue(5);
        q.set_current(Some(0));
        q.move_range(0..2, 3).unwrap();
        assert_eq!(uris(&q), ["2.flac", "3.flac", "4.flac", "0.flac", "1.flac"]);
        assert_eq!(q.current(), Some(3));
        assert!(q.move_range(0..2, 4).is_err());
    }

    #[test]
    fn play_order() {
        let mut q = queue(3);
        assert_eq!(q.next_after(1), Some(2));
        assert_eq!(q.next_after(2), None);
        q.repeat = true;
        assert_eq!(q.next_after(2), Some(0));
        assert_eq!(q.previous_before(0), Some(2));
        q.single = Single::On;
        assert_eq!(q.next_after(1), Some(1));
        q.repeat = false;
        assert_eq!(q.next_after(1), None);
    }

    #[test]
    fn advance_consume_oneshot() {
        let mut q = queue(3);
        q.set_current(Some(0));
        q.consume = true;
        assert_eq!(q.advance(), Some(0));
        assert_eq!(uris(&q), ["1.flac", "2.flac"]);
        q.consume = false;
        q.single = Single::Oneshot;
        assert_eq!(q.advance(), None);
        assert_eq!(q.single, Single::Off);
    }

    #[test]
    fn random_visits_everything_once() {
        let mut q = queue(20);
        q.set_current(Some(5));
        q.set_random(true);
        let mut seen = vec![5];
        while let Some(p) = q.advance() {
            seen.push(p);
        }
        seen.sort();
        assert_eq!(seen, (0..20).collect::<Vec<_>>());
    }

    /// Advances, returning the song now playing.
    fn step(q: &mut Queue) -> usize {
        let p = q.advance().unwrap();
        n(&q.items()[p])
    }

    /// Skips back once, as `previous` does.
    fn back(q: &mut Queue) -> Option<usize> {
        let prev = q.previous_before(q.current()?)?;
        q.set_current(Some(prev));
        Some(n(&q.items()[prev]))
    }

    #[test]
    fn random_previous_retraces_the_order() {
        let mut q = queue(10);
        q.set_current(Some(0));
        q.set_random(true);
        let forward: Vec<usize> = (0..5).map(|_| step(&mut q)).collect();
        let backward: Vec<usize> = (0..5).map(|_| back(&mut q).unwrap()).collect();
        let mut want: Vec<usize> = [0].into_iter().chain(forward[..4].iter().copied()).collect();
        want.reverse();
        assert_eq!(backward, want, "back retraces the songs played");
        assert_eq!(back(&mut q), None, "nothing before the first song");
        let again: Vec<usize> = (0..5).map(|_| step(&mut q)).collect();
        assert_eq!(again, forward, "forward plays them again in the same order");
    }

    #[test]
    fn random_jumps_count_as_played() {
        let mut q = queue(10);
        q.set_current(Some(0));
        q.set_random(true);
        let upcoming = q.next().unwrap();
        // Jump to a song that wasn't next; it doesn't come round again.
        let jump = (1..10).find(|&p| p != upcoming).unwrap();
        q.set_current(Some(jump));
        assert_eq!(back(&mut q), Some(0));
        q.set_current(Some(jump));
        let mut played = vec![0, jump];
        while let Some(p) = q.advance() {
            played.push(p);
        }
        played.sort();
        assert_eq!(played, (0..10).collect::<Vec<_>>());
    }

    #[test]
    fn random_repeat_reshuffles_and_previous_crosses_the_cycle() {
        let mut q = queue(8);
        q.set_current(Some(0));
        q.set_random(true);
        q.repeat = true;
        let mut cycle = vec![0];
        for _ in 0..7 {
            cycle.push(q.advance().unwrap());
        }
        let last = *cycle.last().unwrap();
        let first = q.advance().unwrap();
        assert_eq!(first, cycle[0], "the cycle wraps to its first song");
        assert_eq!(q.previous_before(first), Some(last), "previous reaches the last cycle");
        // The new cycle plays everything but the song that just ended,
        // which then starts the one after.
        let mut next = vec![first];
        for _ in 0..6 {
            next.push(q.advance().unwrap());
        }
        assert_eq!(q.advance(), Some(last));
        next.push(last);
        next.sort();
        assert_eq!(next, (0..8).collect::<Vec<_>>());
    }

    #[test]
    fn random_history_survives_moving_a_played_song_up_next() {
        let mut q = queue(10);
        q.set_current(Some(0));
        q.set_random(true);
        let a = step(&mut q);
        q.advance();
        let upcoming = n(&q.items()[q.next().unwrap()]);
        // Queue song 0, already played, to play next.
        let cur = q.current().unwrap();
        q.move_range(0..1, cur).unwrap();
        assert_eq!(up(&q), ["0.flac"]);
        let cur = q.current().unwrap();
        assert_eq!(n(&q.items()[q.previous_before(cur).unwrap()]), a, "history skips the moved song");
        assert_eq!(step(&mut q), 0);
        assert_eq!(step(&mut q), upcoming, "the shuffle carries on");
    }

    #[test]
    fn setting_random_again_keeps_the_order() {
        let mut q = queue(20);
        q.set_current(Some(0));
        q.set_random(true);
        q.advance();
        let order = q.order.clone();
        q.set_random(true);
        assert_eq!(q.order, order);
        assert_eq!(q.played, 2);
    }
}
