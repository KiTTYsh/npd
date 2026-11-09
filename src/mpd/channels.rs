//! Client-to-client messaging: `subscribe`, `sendmessage`, `readmessages`.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use crate::events::{EventBus, Subsystem};

/// Unread messages a client may hold, as in MPD.
const MAX_QUEUED: usize = 64;

#[derive(Default)]
struct Client {
    channels: HashSet<String>,
    queue: VecDeque<(String, String)>,
}

#[derive(Default)]
pub struct Channels {
    clients: Mutex<HashMap<u64, Client>>,
}

#[derive(Debug, PartialEq)]
pub enum SubscribeError {
    Invalid,
    Already,
}

/// MPD allows letters, digits and `_-.:` in channel names.
fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || "_-.:".contains(c))
}

impl Channels {
    fn all_channels(clients: &HashMap<u64, Client>) -> BTreeSet<String> {
        clients.values().flat_map(|c| c.channels.iter().cloned()).collect()
    }

    pub fn subscribe(&self, client: u64, channel: &str, events: &EventBus) -> Result<(), SubscribeError> {
        if !valid_name(channel) {
            return Err(SubscribeError::Invalid);
        }
        let mut clients = self.clients.lock().unwrap();
        let existed = Self::all_channels(&clients).contains(channel);
        if !clients.entry(client).or_default().channels.insert(channel.to_owned()) {
            return Err(SubscribeError::Already);
        }
        if !existed {
            events.emit(Subsystem::Subscription);
        }
        Ok(())
    }

    /// Returns whether the client was subscribed.
    pub fn unsubscribe(&self, client: u64, channel: &str, events: &EventBus) -> bool {
        let mut clients = self.clients.lock().unwrap();
        let removed = clients.get_mut(&client).is_some_and(|c| c.channels.remove(channel));
        if removed && !Self::all_channels(&clients).contains(channel) {
            events.emit(Subsystem::Subscription);
        }
        removed
    }

    /// Forgets a disconnected client.
    pub fn remove_client(&self, client: u64, events: &EventBus) {
        let mut clients = self.clients.lock().unwrap();
        let before = Self::all_channels(&clients);
        clients.remove(&client);
        if Self::all_channels(&clients) != before {
            events.emit(Subsystem::Subscription);
        }
    }

    pub fn list(&self) -> BTreeSet<String> {
        Self::all_channels(&self.clients.lock().unwrap())
    }

    /// Queues a message for every subscriber. Returns false if nobody is
    /// subscribed.
    pub fn send(&self, channel: &str, message: &str, events: &EventBus) -> bool {
        let mut clients = self.clients.lock().unwrap();
        let mut delivered = false;
        for c in clients.values_mut().filter(|c| c.channels.contains(channel)) {
            if c.queue.len() < MAX_QUEUED {
                c.queue.push_back((channel.to_owned(), message.to_owned()));
            }
            delivered = true;
        }
        if delivered {
            events.emit(Subsystem::Message);
        }
        delivered
    }

    pub fn has_messages(&self, client: u64) -> bool {
        self.clients.lock().unwrap().get(&client).is_some_and(|c| !c.queue.is_empty())
    }

    pub fn read(&self, client: u64) -> Vec<(String, String)> {
        let mut clients = self.clients.lock().unwrap();
        clients.get_mut(&client).map(|c| c.queue.drain(..).collect()).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messaging() {
        let ch = Channels::default();
        let events = EventBus::new();
        let mut rx = events.subscribe();
        assert_eq!(ch.subscribe(1, "bad name", &events), Err(SubscribeError::Invalid));
        ch.subscribe(1, "a", &events).unwrap();
        assert_eq!(rx.try_recv().unwrap(), Subsystem::Subscription);
        ch.subscribe(2, "a", &events).unwrap();
        assert!(rx.try_recv().is_err(), "channel already existed");
        assert_eq!(ch.subscribe(1, "a", &events), Err(SubscribeError::Already));
        assert!(ch.send("a", "hi", &events));
        assert!(!ch.send("b", "hi", &events));
        assert!(ch.has_messages(1) && ch.has_messages(2));
        assert_eq!(ch.read(1), [("a".to_owned(), "hi".to_owned())]);
        assert!(!ch.has_messages(1));
        ch.remove_client(2, &events);
        assert!(ch.unsubscribe(1, "a", &events));
        assert!(ch.list().is_empty());
    }
}
