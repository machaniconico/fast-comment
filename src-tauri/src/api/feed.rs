//! API 専用のメモリ履歴と、接続ごとの bounded queue。
use super::dto::api_comment_json;
use crate::model::{Author, ChatMessage, ModerationEvent};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex, Weak},
};
use tokio::time::{Duration, Instant};

const RECENT_DELETION_LIMIT: usize = 4096;
// 1 tick で取り込む実績の上限。残りは次 tick に回し、chat と active ロックを占有させない。
const MAX_ACHIEVEMENTS_PER_TICK: usize = 256;
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
    pub(super) target: ChatMessage,
}
pub(super) enum Item {
    Chat(Arc<Entry>),
    Moderation(ModerationEvent),
    /// 実績解除(K12 の JSON)。履歴・再開カーソルには載せない。
    Achievement(Value),
}
// Kept in arrival (bus) order so SSE can tell which chats follow a clear.
#[derive(Default)]
pub(super) struct Pending {
    pub items: VecDeque<Item>,
    pub dropped: u64,
}
impl Pending {
    fn push(&mut self, item: Item) {
        if self.items.len() == 256 {
            self.items.pop_front();
            self.dropped += 1;
        }
        self.items.push_back(item);
    }
    #[cfg(test)]
    fn chats(&self) -> usize {
        self.items
            .iter()
            .filter(|i| matches!(i, Item::Chat(_)))
            .count()
    }
    #[cfg(test)]
    fn moderations(&self) -> usize {
        self.items.len() - self.chats()
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
                route: Default::default(),
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
            client.lock().unwrap().push(Item::Chat(entry.clone()));
            true
        });
    }
    pub fn push_moderation(&self, event: &ModerationEvent) {
        let mut state = self.0.lock().unwrap();
        if !state.enabled {
            return;
        }
        state.expire_deletions();
        // Unbounded clears (before_ms: None) would also hide comments posted
        // after the clear, so they apply only to what is already held.
        let bounded = !matches!(
            event,
            ModerationEvent::UserCleared {
                before_ms: None,
                ..
            } | ModerationEvent::ChatCleared {
                before_ms: None,
                ..
            }
        );
        if bounded {
            if state.recent_deletions.len() == RECENT_DELETION_LIMIT {
                state.recent_deletions.pop_front();
            }
            state
                .recent_deletions
                .push_back((Instant::now(), event.clone()));
        }
        // For unbounded clears, retain the IDs actually removed (as MessageDeleted
        // conditions) so same-ID redeliveries cannot resurrect them.
        let mut removed = Vec::new();
        let mut collect = |target: &ChatMessage| {
            if !bounded && event.matches(target) && !removed.iter().any(|id| id == &target.id) {
                removed.push(target.id.clone());
            }
        };
        state.history.retain(|item| {
            collect(&item.target);
            !event.matches(&item.target)
        });
        state.clients.retain(|client| {
            let Some(client) = client.upgrade() else {
                return false;
            };
            let mut pending = client.lock().unwrap();
            for item in &pending.items {
                if let Item::Chat(entry) = item {
                    collect(&entry.target);
                }
            }
            pending.push(Item::Moderation(event.clone()));
            true
        });
        if let ModerationEvent::UserCleared {
            platform, channel, ..
        }
        | ModerationEvent::ChatCleared {
            platform, channel, ..
        } = event
        {
            for message_id in removed {
                if state.recent_deletions.len() == RECENT_DELETION_LIMIT {
                    state.recent_deletions.pop_front();
                }
                let condition = ModerationEvent::MessageDeleted {
                    platform: *platform,
                    channel: channel.clone(),
                    message_id,
                };
                state
                    .recent_deletions
                    .push_back((Instant::now(), condition));
            }
        }
    }
    /// 実績解除を接続中の SSE へ流す。履歴には残さない(`id:` なし、再開不可)。
    pub fn push_achievement(&self, unlock: &crate::achievements::engine::AchievementUnlocked) {
        let mut state = self.0.lock().unwrap();
        if !state.enabled {
            return;
        }
        let value = unlock.relay_json();
        state.clients.retain(|client| {
            let Some(client) = client.upgrade() else {
                return false;
            };
            client.lock().unwrap().push(Item::Achievement(value.clone()));
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
            *receivers = enabled.then(|| {
                (
                    bus.subscribe(),
                    bus.subscribe_moderation(),
                    bus.achievement_sender().subscribe(),
                )
            });
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
                if let Some((chat, moderation, achievement)) = active.as_mut() {
                    feed.drain_bus(chat, moderation);
                    feed.drain_achievements(achievement);
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
    // 実績は履歴を持たないので、取りこぼしはチャットの欠落扱いにせず読み捨てる。
    fn drain_achievements(
        &self,
        achievement: &mut tokio::sync::broadcast::Receiver<crate::achievements::engine::AchievementUnlocked>,
    ) {
        use tokio::sync::broadcast::error::TryRecvError;
        let mut taken = 0;
        while taken < MAX_ACHIEVEMENTS_PER_TICK {
            match achievement.try_recv() {
                Ok(unlock) => {
                    self.push_achievement(&unlock);
                    taken += 1;
                }
                // Lagged も 1 回と数える(続けて返っても上限で tick を抜ける)。
                Err(TryRecvError::Lagged(n)) => {
                    self.record_achievement_dropped(n);
                    taken += 1;
                }
                Err(_) => break,
            }
        }
    }
    // 履歴の連番は進めず、接続中の SSE の lagged にだけ数える。
    fn record_achievement_dropped(&self, count: u64) {
        let state = self.0.lock().unwrap();
        for client in state.clients.iter().filter_map(Weak::upgrade) {
            client.lock().unwrap().dropped += count;
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
    use serde_json::json;
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
            assert_eq!(pending.lock().unwrap().moderations(), 1);
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
                pending.lock().unwrap().chats() == 0,
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
        assert_bus_order(
            ModerationEvent::UserCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                author_id: "user".into(),
                duration_secs: None,
                certain: true,
                before_ms: Some(1),
            },
            true,
        );
        assert_bus_order(
            ModerationEvent::ChatCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                before_ms: Some(1),
            },
            true,
        );
    }
    #[test]
    fn bus_unbounded_clear_does_not_hide_later_comments() {
        let events = [
            ModerationEvent::UserCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                author_id: "user".into(),
                duration_secs: None,
                certain: true,
                before_ms: None,
            },
            ModerationEvent::ChatCleared {
                platform: Platform::Twitch,
                channel: "chan".into(),
                before_ms: None,
            },
        ];
        for event in events {
            let bus = bus();
            let mut chat = bus.subscribe();
            let mut moderation = bus.subscribe_moderation();
            let feed = ApiFeed::new(1000);
            feed.set_enabled(true);
            let (_, pending) = feed.subscribe().unwrap();
            bus.publish(message("before"));
            feed.drain_bus(&mut chat, &mut moderation);
            bus.publish_moderation(event);
            feed.drain_bus(&mut chat, &mut moderation);
            assert!(feed.snapshot().entries.is_empty());
            bus.publish(message("after"));
            feed.drain_bus(&mut chat, &mut moderation);
            assert_eq!(feed.snapshot().entries.len(), 1);
            assert_eq!(pending.lock().unwrap().chats(), 2);
        }
    }
    #[test]
    fn unbounded_user_clear_keeps_later_comment() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        feed.push_moderation(&ModerationEvent::UserCleared {
            platform: Platform::Twitch,
            channel: "chan".into(),
            author_id: "user".into(),
            duration_secs: Some(1),
            certain: true,
            before_ms: None,
        });
        let mut m = message("after");
        m.timestamp_ms = i64::MAX / 2;
        feed.push_chat(&m);
        assert_eq!(feed.snapshot().entries.len(), 1);
    }
    #[test]
    fn unbounded_clear_retains_removed_ids_against_same_id_updates() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        let mut gift = message("old-gift");
        gift.platform = Platform::Youtube;
        feed.push_chat(&gift);
        feed.push_moderation(&ModerationEvent::UserCleared {
            platform: Platform::Youtube,
            channel: "chan".into(),
            author_id: "user".into(),
            duration_secs: Some(1),
            certain: true,
            before_ms: None,
        });
        assert!(feed.snapshot().entries.is_empty());
        let mut update = gift.clone();
        update.fragments = vec![crate::model::Fragment::text("gift combo x2")];
        feed.push_chat(&update);
        assert!(feed.snapshot().entries.is_empty(), "cleared ID resurrected");
        assert_eq!(pending.lock().unwrap().chats(), 1);
        // Only the removed ID is retained, not the user-wide condition.
        let mut fresh = gift.clone();
        fresh.id = "brand-new".into();
        feed.push_chat(&fresh);
        assert_eq!(feed.snapshot().entries.len(), 1);
    }
    #[test]
    fn pending_queue_keeps_arrival_order_across_types() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        feed.push_moderation(&ModerationEvent::ChatCleared {
            platform: Platform::Twitch,
            channel: "chan".into(),
            before_ms: None,
        });
        feed.push_chat(&message("after"));
        let pending = pending.lock().unwrap();
        assert!(matches!(
            pending.items.iter().collect::<Vec<_>>()[..],
            [Item::Moderation(_), Item::Chat(_)]
        ));
    }
    #[test]
    fn unbounded_chat_clear_keeps_later_comment() {
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        feed.push_chat(&message("before"));
        feed.push_moderation(&ModerationEvent::ChatCleared {
            platform: Platform::Twitch,
            channel: "chan".into(),
            before_ms: None,
        });
        assert!(feed.snapshot().entries.is_empty());
        feed.push_chat(&message("after"));
        assert_eq!(feed.snapshot().entries.len(), 1);
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
        assert_eq!(pending.lock().unwrap().chats(), 1);
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
            assert_eq!(pending.lock().unwrap().chats(), expected);
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
        assert_eq!(pending.moderations(), 127);
        assert_eq!(pending.chats(), 129);
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
    fn unlock(id: &str) -> crate::achievements::engine::AchievementUnlocked {
        serde_json::from_value(serde_json::json!({
            "id": id, "achievementId": "a", "name": "n", "description": "", "tier": "bronze",
            "icon": "", "subject": {"kind": "streamer"}, "valueMilli": 1000,
            "thresholdMilli": 1000, "currency": null, "windowKey": "*", "unlockedAtMs": 1
        }))
        .unwrap()
    }
    #[test]
    fn achievement_bus_lag_is_added_to_client_dropped() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(256);
        let feed = ApiFeed::new(10);
        feed.set_enabled(true);
        let (_, pending) = feed.subscribe().unwrap();
        for i in 0..1025 {
            tx.send(unlock(&i.to_string())).unwrap();
        }
        feed.drain_achievements(&mut rx);
        let p = pending.lock().unwrap();
        assert_eq!(p.dropped, 769, "bus 側の欠落 1025 - 256 件");
        assert_eq!(p.items.len(), 255, "Lagged も上限 256 の 1 回に数える");
        assert_eq!(rx.len(), 1);
    }
    #[test]
    fn achievement_lagged_counts_toward_tick_cap() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(MAX_ACHIEVEMENTS_PER_TICK);
        let feed = ApiFeed::new(10);
        feed.set_enabled(true);
        for i in 0..(MAX_ACHIEVEMENTS_PER_TICK + 5) {
            tx.send(unlock(&i.to_string())).unwrap();
        }
        feed.drain_achievements(&mut rx);
        assert_eq!(rx.len(), 1, "Lagged 1 回 + 受信 255 件で上限に達し、残りは次 tick");
    }
    #[test]
    fn achievement_drain_is_capped_per_tick() {
        let (tx, mut rx) = tokio::sync::broadcast::channel(1024);
        let feed = ApiFeed::new(10);
        feed.set_enabled(true);
        for i in 0..(MAX_ACHIEVEMENTS_PER_TICK + 44) {
            tx.send(unlock(&i.to_string())).unwrap();
        }
        feed.drain_achievements(&mut rx);
        assert_eq!(rx.len(), 44, "上限を超えた分は次 tick に残る(chat を飢えさせない)");
        feed.drain_achievements(&mut rx);
        assert_eq!(rx.len(), 0);
    }
    #[tokio::test]
    async fn bus_achievement_reaches_sse_pending_through_bind() {
        let (stats, _) = tokio::sync::watch::channel(Default::default());
        let (timer, _) = tokio::sync::watch::channel(Default::default());
        let bus = crate::bus::Bus::new(1234, stats, timer);
        let feed = ApiFeed::new(10);
        let gate = super::super::ApiGate::closed();
        let stop = tokio_util::sync::CancellationToken::new();
        feed.bind(&bus, &gate, stop.clone());
        gate.open("t");
        let (_, pending) = feed.subscribe().unwrap();
        bus.achievement_sender().send(unlock("x")).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while pending.lock().unwrap().items.is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        assert!(matches!(
            pending.lock().unwrap().items.front(),
            Some(Item::Achievement(v)) if v["achievementId"] == "a"
        ));
        stop.cancel();
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
