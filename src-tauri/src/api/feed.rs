//! API 専用のメモリ履歴と、接続ごとの bounded queue。
use super::dto::api_comment_json;
use crate::model::{Author, ChatMessage, ModerationEvent};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
};

#[derive(Clone)]
pub struct ApiFeed(Arc<Mutex<FeedState>>);
struct FeedState {
    enabled: bool,
    seq: u64,
    capacity: usize,
    discarded_through: u64,
    history: VecDeque<Arc<Entry>>,
    clients: Vec<Weak<Mutex<Pending>>>,
}
pub(super) struct History {
    pub entries: Vec<Arc<Entry>>,
    pub discarded_through: u64,
    pub latest_seq: u64,
}
pub(super) struct Entry {
    pub seq: u64,
    pub value: Value,
    // Only identity and timestamp are retained for the shared moderation predicate.
    target: ChatMessage,
}
#[derive(Default)]
pub(super) struct Pending {
    pub chat: VecDeque<Arc<Entry>>,
    pub moderation: VecDeque<Value>,
    pub dropped: u64,
    pub order: VecDeque<bool>,
}
impl Pending {
    fn reserve(&mut self, chat: bool) {
        if self.order.len() == 256 {
            if self.order.pop_front() == Some(true) {
                self.chat.pop_front();
            } else {
                self.moderation.pop_front();
            }
            self.dropped += 1;
        }
        self.order.push_back(chat);
    }
}
impl ApiFeed {
    pub fn new(capacity: usize) -> Self {
        Self(Arc::new(Mutex::new(FeedState {
            enabled: false,
            seq: 0,
            capacity: capacity.max(1),
            discarded_through: 0,
            history: VecDeque::new(),
            clients: Vec::new(),
        })))
    }
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.0.lock().unwrap();
        if state.enabled != enabled {
            state.discarded_through = state.seq;
            state.history.clear();
            for client in state.clients.iter().filter_map(Weak::upgrade) {
                *client.lock().unwrap() = Pending::default();
            }
        }
        state.enabled = enabled;
    }
    pub fn push_chat(&self, msg: &ChatMessage) {
        let mut state = self.0.lock().unwrap();
        if !state.enabled {
            return;
        }
        state.seq += 1;
        let entry = Arc::new(Entry {
            seq: state.seq,
            value: api_comment_json(state.seq, msg),
            target: ChatMessage {
                id: msg.id.clone(),
                platform: msg.platform,
                channel: msg.channel.clone(),
                author: Author {
                    id: msg.author.id.clone(),
                    name: String::new(),
                    nickname: None,
                    avatar_url: None,
                    display_color: None,
                    badges: Vec::new(),
                    roles: Default::default(),
                },
                timestamp_ms: msg.timestamp_ms,
                fragments: Vec::new(),
                kind: msg.kind,
                amount: None,
                raw: None,
                listener: None,
                native_id: false,
                skip_tts: false,
                injected_test: false,
            },
        });
        if state.history.len() == state.capacity {
            state.discarded_through = state
                .discarded_through
                .max(state.history.pop_front().unwrap().seq);
        }
        state.history.push_back(entry.clone());
        state.clients.retain(|client| {
            let Some(client) = client.upgrade() else {
                return false;
            };
            let mut pending = client.lock().unwrap();
            pending.reserve(true);
            pending.chat.push_back(entry.clone());
            true
        });
    }
    pub fn push_moderation(&self, event: &ModerationEvent) {
        let mut state = self.0.lock().unwrap();
        if !state.enabled {
            return;
        }
        state.history.retain(|item| !event.matches(&item.target));
        let value = json!(event);
        state.clients.retain(|client| {
            let Some(client) = client.upgrade() else {
                return false;
            };
            let mut pending = client.lock().unwrap();
            pending.reserve(false);
            pending.moderation.push_back(value.clone());
            true
        });
    }
    pub(super) fn head(&self) -> Option<u64> {
        self.0.lock().unwrap().history.back().map(|e| e.seq)
    }
    pub(super) fn snapshot(&self) -> History {
        let state = self.0.lock().unwrap();
        History {
            entries: state.history.iter().cloned().collect(),
            discarded_through: state.discarded_through,
            latest_seq: state.seq,
        }
    }
    // 履歴確定とライブ購読を同じロックで行い、境界での欠落・重複を防ぐ。
    pub(super) fn subscribe(&self) -> Option<(History, Arc<Mutex<Pending>>)> {
        let mut state = self.0.lock().unwrap();
        state.clients.retain(|c| c.strong_count() > 0);
        if state.clients.len() >= 8 {
            return None;
        }
        let pending = Arc::new(Mutex::new(Pending::default()));
        state.clients.push(Arc::downgrade(&pending));
        Some((
            History {
                entries: state.history.iter().cloned().collect(),
                discarded_through: state.discarded_through,
                latest_seq: state.seq,
            },
            pending,
        ))
    }
}

impl ApiFeed {
    /// Bind once at setup. Gate transitions synchronously drop subscriptions and
    /// clear history; no receiver or retained comments exist while disabled.
    pub(crate) fn bind(
        &self,
        bus: &crate::bus::Bus,
        gate: &super::ApiGate,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        use tokio::sync::broadcast::error::TryRecvError;
        let active = Arc::new(Mutex::new(None));
        let notify = Arc::new(tokio::sync::Notify::new());
        let feed = self.clone();
        let bus = bus.clone();
        let receivers = active.clone();
        let wake = notify.clone();
        gate.on_transition(move |enabled| {
            let mut receivers = receivers.lock().unwrap();
            feed.set_enabled(enabled);
            *receivers = enabled.then(|| (bus.subscribe(), bus.subscribe_moderation()));
            wake.notify_one();
        });
        let feed = self.clone();
        tauri::async_runtime::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(16));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                let enabled = active.lock().unwrap().is_some();
                tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => break,
                    _ = notify.notified() => continue,
                    _ = tick.tick(), if enabled => {}
                }
                let mut active = active.lock().unwrap();
                if let Some((chat, moderation)) = active.as_mut() {
                    // Freeze actual moderation events BEFORE the chat boundary.
                    // Every chat published before these deletions must be consumed
                    // (or accounted for as lost) before applying the deletions.
                    // Merely freezing receiver lengths is insufficient if a lag
                    // advances the moderation receiver to newer events meanwhile.
                    let mut events = Vec::new();
                    let mut moderation_lost = 0;
                    for _ in 0..moderation.len() {
                        match moderation.try_recv() {
                            Ok(event) => events.push(event),
                            Err(TryRecvError::Lagged(n)) => moderation_lost += n,
                            Err(_) => break,
                        }
                    }
                    let mut remaining = chat.len();
                    while remaining > 0 {
                        match chat.try_recv() {
                            Ok(msg) => {
                                feed.push_chat(&msg);
                                remaining -= 1;
                            }
                            Err(TryRecvError::Lagged(n)) => {
                                feed.record_dropped(n, false);
                                remaining = remaining.saturating_sub(n as usize);
                            }
                            Err(_) => break,
                        }
                    }
                    if moderation_lost > 0 {
                        feed.record_dropped(moderation_lost, true);
                    }
                    for event in events {
                        feed.push_moderation(&event);
                    }
                }
            }
            *active.lock().unwrap() = None;
            feed.set_enabled(false);
        });
    }
    fn record_dropped(&self, count: u64, moderation_lost: bool) {
        let mut state = self.0.lock().unwrap();
        // Reserve missing sequence numbers so polling clients also observe the loss.
        state.seq += count;
        state.discarded_through = state.seq;
        // A missed deletion cannot be reconstructed; discard potentially stale history.
        if moderation_lost {
            state.history.clear();
        }
        for client in state.clients.iter().filter_map(Weak::upgrade) {
            client.lock().unwrap().dropped += count;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Platform;
    fn message(id: &str) -> ChatMessage {
        serde_json::from_value(json!({"id": id, "platform":"twitch", "channel":"chan",
            "author":{"id":"user","name":"Alice"}, "fragments":[],
            "kind":"normal", "timestampMs":1}))
        .unwrap()
    }
    fn deleted(id: &str) -> ModerationEvent {
        ModerationEvent::MessageDeleted {
            platform: Platform::Twitch,
            channel: "chan".into(),
            message_id: id.into(),
        }
    }
    #[tokio::test]
    async fn concurrent_bus_deletions_never_resurrect_in_history() {
        let (stats, _) = tokio::sync::watch::channel(Default::default());
        let (timer, _) = tokio::sync::watch::channel(Default::default());
        let bus = crate::bus::Bus::new(1234, stats, timer);
        let gate = super::super::ApiGate::closed();
        let feed = ApiFeed::new(1000);
        let stop = tokio_util::sync::CancellationToken::new();
        feed.bind(&bus, &gate, stop.clone());
        gate.open("regression-token");
        let publisher = std::thread::spawn(move || {
            for n in 0..20_000 {
                let id = format!("m{n}");
                bus.publish(message(&id));
                std::thread::yield_now();
                bus.publish_moderation(deleted(&id));
                std::thread::sleep(std::time::Duration::from_micros(50));
            }
            // A final marker proves that both receivers have been drained.
            bus.publish(message("finished"));
            bus.publish_moderation(deleted("finished"));
        });
        tokio::task::spawn_blocking(move || publisher.join().unwrap())
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                let state = feed.0.lock().unwrap();
                let drained = state.seq == 20_001
                    && !state
                        .history
                        .iter()
                        .any(|entry| entry.target.id == "finished");
                drop(state);
                if drained {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        stop.cancel();
        let remaining = feed.snapshot().entries.len();
        assert_eq!(remaining, 0, "deleted comments resurrected in API history");
    }
    #[test]
    fn mixed_events_share_one_queue_and_drop_oldest_across_types() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        for _ in 0..128 {
            feed.push_moderation(&deleted("other"));
            feed.push_chat(&message("m"));
        }
        feed.push_chat(&message("new"));
        let pending = pending.lock().unwrap();
        assert_eq!(pending.dropped, 1);
        assert_eq!(pending.moderation.len(), 127);
        assert_eq!(pending.chat.len(), 129);
    }
    #[test]
    fn moderation_holes_are_not_reported_as_capacity_loss() {
        let feed = ApiFeed::new(2);
        feed.set_enabled(true);
        feed.push_chat(&message("one"));
        feed.push_chat(&message("two"));
        feed.push_moderation(&deleted("one"));
        let (_, gap, dropped) =
            super::super::comments::select(feed.snapshot(), Some("abcdef012345.0"), "abcdef012345");
        assert!(!gap);
        assert_eq!(dropped, 0);
        feed.push_chat(&message("three"));
        feed.push_chat(&message("four"));
        let (_, gap, dropped) =
            super::super::comments::select(feed.snapshot(), Some("abcdef012345.0"), "abcdef012345");
        assert!(gap);
        assert_eq!(dropped, 2);
    }
    #[test]
    fn bus_loss_is_visible_to_polling_and_lost_moderation_clears_history() {
        let feed = ApiFeed::new(2);
        feed.set_enabled(true);
        feed.push_chat(&message("one"));
        feed.record_dropped(10, false);
        feed.push_chat(&message("two"));
        let (_, gap, dropped) =
            super::super::comments::select(feed.snapshot(), Some("abcdef012345.1"), "abcdef012345");
        assert!(gap);
        assert_eq!(dropped, 10);
        feed.push_chat(&message("three"));
        assert!(
            super::super::comments::select(feed.snapshot(), Some("abcdef012345.1"), "abcdef012345")
                .1
        );
        feed.record_dropped(1, true);
        assert!(feed.snapshot().entries.is_empty());
    }
    #[tokio::test]
    async fn gate_transitions_drop_bus_subscriptions_and_clear_history_synchronously() {
        let (stats, _) = tokio::sync::watch::channel(Default::default());
        let (timer, _) = tokio::sync::watch::channel(Default::default());
        let bus = crate::bus::Bus::new(1234, stats, timer);
        let feed = ApiFeed::new(1000);
        let gate = super::super::ApiGate::closed();
        let stop = tokio_util::sync::CancellationToken::new();
        feed.bind(&bus, &gate, stop.clone());
        bus.publish(message("disabled"));
        gate.open("first");
        bus.publish(message("enabled"));
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while feed.head().is_none() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert_eq!(feed.snapshot().entries[0].value["id"], "enabled");
        let seq = feed.head().unwrap();
        let session = gate.authenticate("first").unwrap();
        gate.open("first");
        assert!(!session.is_cancelled());
        gate.open("second");
        assert!(session.is_cancelled());
        assert_eq!(feed.head(), Some(seq));
        gate.close();
        assert!(feed.snapshot().entries.is_empty());
        bus.publish(message("disabled-again"));
        gate.open("third");
        feed.push_chat(&message("new"));
        assert_eq!(feed.snapshot().entries.len(), 1);
        assert!(feed.head().unwrap() > seq);
        stop.cancel();
    }
}
