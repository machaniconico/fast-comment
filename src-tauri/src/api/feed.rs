//! API 専用のメモリ履歴と、接続ごとの bounded queue。
use super::dto::api_comment_json;
use crate::model::{Author, ChatMessage, ModerationEvent};
use serde_json::{json, Value};
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
};
use tokio::time::{Duration, Instant};

const RECENT_DELETION_LIMIT: usize = 4096;
const RECENT_DELETION_TTL: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ApiFeed(Arc<Mutex<FeedState>>);
struct FeedState {
    enabled: bool,
    seq: u64,
    capacity: usize,
    discarded_through: u64,
    history: VecDeque<Arc<Entry>>,
    clients: Vec<Weak<Mutex<Pending>>>,
    recent_deletions: VecDeque<(Instant, ModerationEvent)>,
}
impl FeedState {
    fn expire_deletions(&mut self) {
        let now = Instant::now();
        while self
            .recent_deletions
            .front()
            .is_some_and(|(at, _)| now.duration_since(*at) >= RECENT_DELETION_TTL)
        {
            self.recent_deletions.pop_front();
        }
    }
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
            recent_deletions: VecDeque::new(),
        })))
    }
    pub fn set_enabled(&self, enabled: bool) {
        let mut state = self.0.lock().unwrap();
        if state.enabled != enabled {
            state.discarded_through = state.seq;
            state.history.clear();
            state.recent_deletions.clear();
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
        state.expire_deletions();
        // Apply the same predicate before both history and live SSE enqueueing.
        // Keep seq monotonic even for comments suppressed by moderation.
        if state
            .recent_deletions
            .iter()
            .any(|(_, event)| event.matches(msg))
        {
            return;
        }
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
        state.expire_deletions();
        if state.recent_deletions.len() == RECENT_DELETION_LIMIT {
            state.recent_deletions.pop_front();
        }
        state
            .recent_deletions
            .push_back((Instant::now(), event.clone()));
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
                    feed.drain_bus(chat, moderation);
                }
            }
            *active.lock().unwrap() = None;
            feed.set_enabled(false);
        });
    }
    // One feed tick, also driven explicitly by tests to fix cross-tick ordering.
    fn drain_bus(
        &self,
        chat: &mut tokio::sync::broadcast::Receiver<ChatMessage>,
        moderation: &mut tokio::sync::broadcast::Receiver<ModerationEvent>,
    ) {
        use tokio::sync::broadcast::error::TryRecvError;
        // Expire even on idle ticks so stale conditions release their allocations.
        self.0.lock().unwrap().expire_deletions();
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
                    self.push_chat(&msg);
                    remaining -= 1;
                }
                Err(TryRecvError::Lagged(n)) => {
                    self.record_dropped(n, false);
                    remaining = remaining.saturating_sub(n as usize);
                }
                Err(_) => break,
            }
        }
        if moderation_lost > 0 {
            self.record_dropped(moderation_lost, true);
        }
        for event in events {
            self.push_moderation(&event);
        }
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
    fn bus() -> crate::bus::Bus {
        let (stats, _) = tokio::sync::watch::channel(Default::default());
        let (timer, _) = tokio::sync::watch::channel(Default::default());
        crate::bus::Bus::new(1234, stats, timer)
    }
    fn assert_bus_order(event: ModerationEvent, reverse: bool) {
        let bus = bus();
        let mut chat = bus.subscribe();
        let mut moderation = bus.subscribe_moderation();
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        // No background tick: each drain completes before the next publish.
        if reverse {
            bus.publish_moderation(event);
            feed.drain_bus(&mut chat, &mut moderation);
            assert_eq!(pending.lock().unwrap().moderation.len(), 1);
            bus.publish(message("target"));
        } else {
            bus.publish(message("target"));
            feed.drain_bus(&mut chat, &mut moderation);
            assert_eq!(feed.snapshot().entries.len(), 1);
            bus.publish_moderation(event);
        }
        feed.drain_bus(&mut chat, &mut moderation);
        assert!(feed.snapshot().entries.is_empty());
        if reverse {
            assert!(
                pending.lock().unwrap().chat.is_empty(),
                "deleted chat reached live SSE queue"
            );
        }
        // Same-ID combo updates arriving in a later tick are also suppressed.
        bus.publish(message("target"));
        feed.drain_bus(&mut chat, &mut moderation);
        assert!(feed.snapshot().entries.is_empty());
    }
    #[test]
    fn bus_deletion_before_and_after_comment_never_resurrects() {
        assert_bus_order(deleted("target"), false);
        assert_bus_order(deleted("target"), true);
    }
    #[test]
    fn bus_user_and_chat_clear_before_comment_never_resurrects() {
        for before_ms in [Some(1), None] {
            assert_bus_order(
                ModerationEvent::UserCleared {
                    platform: Platform::Twitch,
                    channel: "chan".into(),
                    author_id: "user".into(),
                    duration_secs: None,
                    certain: true,
                    before_ms,
                },
                true,
            );
            assert_bus_order(
                ModerationEvent::ChatCleared {
                    platform: Platform::Twitch,
                    channel: "chan".into(),
                    before_ms,
                },
                true,
            );
        }
    }
    #[tokio::test(start_paused = true)]
    async fn deletion_conditions_expire_on_idle_tick() {
        let bus = bus();
        let mut chat = bus.subscribe();
        let mut moderation = bus.subscribe_moderation();
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        bus.publish_moderation(deleted("target"));
        feed.drain_bus(&mut chat, &mut moderation);
        tokio::time::advance(RECENT_DELETION_TTL).await;
        feed.drain_bus(&mut chat, &mut moderation);
        assert!(feed.0.lock().unwrap().recent_deletions.is_empty());
        bus.publish(message("target"));
        feed.drain_bus(&mut chat, &mut moderation);
        assert_eq!(feed.snapshot().entries.len(), 1);
        assert_eq!(pending.lock().unwrap().chat.len(), 1);
    }
    #[test]
    fn deletion_conditions_are_bounded_and_cleared_when_disabled() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        for n in 0..=RECENT_DELETION_LIMIT {
            feed.push_moderation(&deleted(&format!("m{n}")));
        }
        assert_eq!(
            feed.0.lock().unwrap().recent_deletions.len(),
            RECENT_DELETION_LIMIT
        );
        feed.push_chat(&message("m0"));
        assert_eq!(feed.snapshot().entries.len(), 1);
        feed.set_enabled(false);
        assert!(feed.0.lock().unwrap().recent_deletions.is_empty());
    }
    #[test]
    fn retained_conditions_use_shared_matching_semantics() {
        let events = [
            deleted("target"),
            ModerationEvent::UserCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                author_id: "user".into(),
                duration_secs: None,
                certain: true,
                before_ms: Some(1),
            },
            ModerationEvent::ChatCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                before_ms: Some(1),
            },
        ];
        for event in events {
            let feed = ApiFeed::new(1000);
            feed.set_enabled(true);
            let (_, pending) = feed.subscribe().unwrap();
            feed.push_moderation(&event);
            let original = message("target");
            let mut variants = vec![original.clone(); 6];
            variants[1].platform = Platform::Youtube;
            variants[2].channel = "other".into();
            variants[3].author.id = "other".into();
            variants[4].timestamp_ms = 2;
            variants[5].id = "other".into();
            let expected = variants.iter().filter(|msg| !event.matches(msg)).count();
            for msg in variants {
                feed.push_chat(&msg);
            }
            assert_eq!(feed.snapshot().entries.len(), expected);
            assert_eq!(pending.lock().unwrap().chat.len(), expected);
        }
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
