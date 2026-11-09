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
    /// Play order (positions) when random; `order[0]` is the current song.
    order: Vec<usize>,
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

    fn reshuffle(&mut self) {
        if !self.random {
            self.order.clear();
            return;
        }
        let mut order: Vec<usize> = (0..self.items.len()).filter(|&p| Some(p) != self.current).collect();
        order.shuffle(&mut rand::rng());
        if let Some(cur) = self.current {
            order.insert(0, cur);
        }
        self.order = order;
    }

    /// Inserts songs at `at` (default: the end). Returns their IDs.
    pub fn add(&mut self, songs: impl IntoIterator<Item = Arc<SongRow>>, at: Option<usize>) -> Vec<ItemId> {
        let at = at.unwrap_or(self.items.len()).min(self.items.len());
        let new: Vec<Item> = songs
            .into_iter()
            .map(|song| {
                let id = self.next_id;
                self.next_id = self.next_id.wrapping_add(1).max(1);
                Item { id, song, changed: 0 }
            })
            .collect();
        let ids = new.iter().map(|i| i.id).collect();
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
        self.reshuffle();
        ids
    }

    /// Removes positions in `range`. Returns whether the current song was
    /// among them; the current position then points at the song that took
    /// its place, if any.
    pub fn delete(&mut self, range: Range<usize>) -> bool {
        let range = range.start.min(self.items.len())..range.end.min(self.items.len());
        if range.is_empty() {
            return false;
        }
        let n = range.len();
        self.items.drain(range.clone());
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
        self.reshuffle();
        removed_current
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.current = None;
        self.bump(0..0);
        self.order.clear();
    }

    /// Moves the songs in `range` so the first lands at `to` (a position in
    /// the queue as it is after removing them).
    pub fn move_range(&mut self, range: Range<usize>, to: usize) -> Result<(), String> {
        if range.end > self.items.len() || range.start > range.end {
            return Err("Bad song index".into());
        }
        if to + range.len() > self.items.len() {
            return Err("Bad song index".into());
        }
        let current_id = self.current_item().map(|i| i.id);
        let moved: Vec<Item> = self.items.drain(range.clone()).collect();
        self.items.splice(to..to, moved);
        self.current = current_id.and_then(|id| self.position_of(id));
        let lo = range.start.min(to);
        let hi = range.end.max(to + range.len());
        self.bump(lo..hi);
        self.reshuffle();
        Ok(())
    }

    pub fn shuffle(&mut self, range: Range<usize>) {
        let range = range.start.min(self.items.len())..range.end.min(self.items.len());
        let current_id = self.current_item().map(|i| i.id);
        self.items[range.clone()].shuffle(&mut rand::rng());
        self.current = current_id.and_then(|id| self.position_of(id));
        self.bump(range);
        self.reshuffle();
    }

    /// Makes `pos` current (or none).
    pub fn set_current(&mut self, pos: Option<usize>) {
        self.current = pos.filter(|&p| p < self.items.len());
        if self.random {
            // Keep the random order, but play the chosen song now.
            match self.current {
                Some(cur) => {
                    self.order.retain(|&p| p != cur);
                    self.order.insert(0, cur);
                }
                None => self.reshuffle(),
            }
        }
    }

    /// The position that follows `pos` in play order, honouring repeat,
    /// random and single.
    pub fn next_after(&self, pos: usize) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        if self.single != Single::Off {
            return self.repeat.then_some(pos);
        }
        if self.random {
            let idx = self.order.iter().position(|&p| p == pos)?;
            return match self.order.get(idx + 1) {
                Some(&p) => Some(p),
                None if self.repeat => self.order.first().copied(),
                None => None,
            };
        }
        if pos + 1 < self.items.len() {
            Some(pos + 1)
        } else if self.repeat {
            Some(0)
        } else {
            None
        }
    }

    /// The song after the current one, if playback will continue.
    pub fn next(&self) -> Option<usize> {
        self.current.and_then(|c| self.next_after(c))
    }

    /// The song before `pos` in play order.
    pub fn previous_before(&self, pos: usize) -> Option<usize> {
        if self.random {
            let idx = self.order.iter().position(|&p| p == pos)?;
            return idx.checked_sub(1).map(|i| self.order[i]).or(self.repeat.then(|| *self.order.last().unwrap()));
        }
        match pos.checked_sub(1) {
            Some(p) => Some(p),
            None if self.repeat => self.items.len().checked_sub(1),
            None => None,
        }
    }

    /// Advances after the current song finished naturally: applies consume
    /// and single-oneshot. Returns the new current position.
    pub fn advance(&mut self) -> Option<usize> {
        let cur = self.current?;
        let next = self.next_after(cur);
        if self.single == Single::Oneshot {
            self.single = Single::Off;
        }
        let next_id = next.map(|p| self.items[p].id);
        if self.consume {
            self.delete(cur..cur + 1);
        }
        self.current = next_id.and_then(|id| self.position_of(id));
        if self.random
            && let Some(cur) = self.current
        {
            // Drop the finished part of the order so `order[0]` is current.
            if let Some(idx) = self.order.iter().position(|&p| p == cur) {
                self.order.drain(..idx);
            }
        }
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn song(n: usize) -> Arc<SongRow> {
        Arc::new(SongRow {
            rowid: n as i64,
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

    fn queue(n: usize) -> Queue {
        let mut q = Queue::new();
        q.add((0..n).map(song), None);
        q
    }

    fn uris(q: &Queue) -> Vec<String> {
        q.items().iter().map(|i| i.song.uri.clone()).collect()
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
}
