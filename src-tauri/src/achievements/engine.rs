//! Achievement counters and durable unlocks.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::{ChatMessage, LiveEvent, MessageKind, Platform};
use crate::stats::{StatsSnapshot, ViewerCountKind};

use super::currency::AmountTotals;
use super::model::{Achievement, Condition, CurrencyMode, Metric, SubjectKind, Tier, Window};
use super::session::{platform_key, Sessions, PLATFORMS};
use super::store::{AchievementDefs, CommitError, FsIo, ProgressFile, StoreIo, STORE_VERSION};

pub const EPOCH_IDLE_MS: u64 = 30 * 60 * 1000;
pub const STALE_SESSION_MS: u64 = 6 * 60 * 60 * 1000;
pub const DEDUP_CAPACITY: usize = 4096;
const LIFETIME: &str = "*";
const STREAMER: &str = "streamer";
const ALL_PLATFORMS: u8 = (1 << PLATFORMS.len()) - 1;

pub fn is_shared_anonymous(msg: &ChatMessage) -> bool {
    msg.author.id == "tc:anonymous"
        || (msg.platform == Platform::Twitch && msg.author.id == "274598607")
        || msg
            .author
            .badges
            .iter()
            .any(|badge| badge.kind == "anonymous")
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockSubject {
    pub kind: SubjectKind,
    pub platform: Option<Platform>,
    pub author_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementUnlocked {
    pub id: String,
    pub achievement_id: String,
    pub name: String,
    pub description: String,
    pub tier: Tier,
    pub icon: String,
    pub subject: UnlockSubject,
    pub value_milli: i64,
    pub threshold_milli: i64,
    pub currency: Option<String>,
    pub window_key: String,
    pub unlocked_at_ms: u64,
}

impl AchievementUnlocked {
    /// Webhook と SSE に送る外向きの形(決定 K12 の 11 キー)。説明・アイコン・内部 id は含めない。
    /// `currency` があれば金額系(ミリ単位のまま)、無ければ回数系(回数そのもの)。
    pub fn relay_json(&self) -> serde_json::Value {
        use serde_json::json;
        let subject = match (self.subject.kind, &self.subject.platform) {
            (SubjectKind::Viewer, platform) => json!({
                "kind": "viewer",
                "platform": platform,
                "authorId": self.subject.author_id,
                "name": self.subject.name,
            }),
            (SubjectKind::Streamer, _) => json!({"kind": "streamer"}),
        };
        let (value, threshold, unit) = if self.currency.is_some() {
            (self.value_milli, self.threshold_milli, "milli")
        } else {
            // しきい値は切り上げる(1.5 回は 2 回で解除されるため)。値はミリ単位でも回数の整数倍。
            (
                self.value_milli / 1000,
                (self.threshold_milli / 1000 + i64::from(self.threshold_milli % 1000 > 0)).max(1),
                "count",
            )
        };
        json!({
            "type": "achievement",
            "achievementId": self.achievement_id,
            "name": self.name,
            "tier": self.tier,
            "subject": subject,
            "value": value,
            "threshold": threshold,
            "currency": self.currency,
            "unit": unit,
            "windowKey": self.window_key,
            "unlockedAtMs": self.unlocked_at_ms,
        })
    }
}

#[cfg(test)]
mod relay_json_tests {
    use super::*;

    fn count_unlock(value_milli: i64, threshold_milli: i64) -> AchievementUnlocked {
        serde_json::from_value(serde_json::json!({
            "id": "i", "achievementId": "a", "name": "n", "description": "", "tier": "bronze",
            "icon": "", "subject": {"kind": "streamer"}, "valueMilli": value_milli,
            "thresholdMilli": threshold_milli, "currency": null, "windowKey": "*", "unlockedAtMs": 1
        }))
        .unwrap()
    }

    #[test]
    fn count_threshold_rounds_up() {
        let v = count_unlock(3000, 2500).relay_json();
        assert_eq!((v["value"].clone(), v["threshold"].clone()), (3.into(), 3.into()));
        assert_eq!(count_unlock(1000, 400).relay_json()["threshold"], 1);
        assert_eq!(count_unlock(2000, 2000).relay_json()["threshold"], 2);
    }

    #[test]
    fn count_threshold_does_not_overflow_at_i64_max() {
        let v = count_unlock(i64::MAX, i64::MAX).relay_json();
        assert_eq!(v["threshold"], i64::MAX / 1000 + 1);
        assert_eq!(v["value"], i64::MAX / 1000);
    }

    #[test]
    fn threshold_to_milli_keeps_fraction_as_ceil() {
        assert_eq!(threshold_to_milli(0.0004), 1);
        assert_eq!(threshold_to_milli(1.1), 1100);
        assert_eq!(threshold_to_milli(2.3), 2300);
        assert_eq!(threshold_to_milli(9223372036854776.0), i64::MAX);
        for (t, need) in [(0.0004, 1), (1.0004, 2), (2.5, 3), (0.4, 1), (2.0, 2), (1.1, 2)] {
            assert_eq!(count_unlock(need * 1000, threshold_to_milli(t)).relay_json()["threshold"], need, "{t}");
        }
    }
}

impl fmt::Debug for UnlockSubject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("UnlockSubject")
    }
}

impl fmt::Debug for AchievementUnlocked {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("AchievementUnlocked")
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnlockRecord {
    pub achievement_id: String,
    pub window_key: String,
    pub unlocked_at_ms: u64,
}

fn zero(value: &u64) -> bool {
    *value == 0
}

/// 解除に要る最小のミリ値。浮動小数の誤差(1.1 * 1000 = 1100.0000000000002)は丸め、
/// 本当に端数がある時(0.0004 → 0.4)は切り上げて、回数系の必要回数を過小にしない。
fn threshold_to_milli(threshold: f64) -> i64 {
    let scaled = threshold * 1000.0;
    let nearest = scaled.round();
    if (scaled - nearest).abs() < 1e-6 { nearest as i64 } else { scaled.ceil() as i64 }
}

fn milli(count: u64) -> i64 {
    count.saturating_mul(1000).min(i64::MAX as u64) as i64
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub(super) struct Counters {
    #[serde(skip_serializing_if = "zero")]
    comments: u64,
    #[serde(skip_serializing_if = "zero")]
    super_chat_count: u64,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    donation: BTreeMap<String, i64>,
    #[serde(skip_serializing_if = "zero")]
    gift_count: u64,
    #[serde(skip_serializing_if = "zero")]
    membership_events: u64,
    #[serde(skip_serializing_if = "zero")]
    streams: u64,
    #[serde(skip_serializing_if = "zero")]
    peak_concurrent_viewers: u64,
}

impl Counters {
    fn add_message(&mut self, msg: &ChatMessage) {
        self.comments = self.comments.saturating_add(1);
        self.add_message_kind(msg);
    }

    fn add_message_kind(&mut self, msg: &ChatMessage) {
        let amount = msg.amount.as_ref();
        let subgift = amount.filter(|a| a.currency == "SUBGIFT");
        match msg.kind {
            MessageKind::SuperChat => {
                self.super_chat_count = self.super_chat_count.saturating_add(1)
            }
            MessageKind::Membership => {
                self.membership_events = self.membership_events.saturating_add(1)
            }
            MessageKind::Gift => {
                let count = subgift.map_or(1, |a| {
                    if a.value >= 0.0 && a.value < u64::MAX as f64 && a.value.fract() == 0.0 {
                        a.value as u64
                    } else {
                        0
                    }
                });
                self.gift_count = self.gift_count.saturating_add(count);
            }
            _ => {}
        }
        // Sources already normalize both display strings and official micros into Amount.
        if matches!(
            msg.kind,
            MessageKind::SuperChat | MessageKind::Bits | MessageKind::Gift
        ) && subgift.is_none()
        {
            if let Some(amount) = amount.filter(|a| a.value.is_finite()) {
                let value = (amount.value * 1000.0).round() as i64;
                add_amount(&mut self.donation, amount.currency.trim(), value);
            }
        }
    }

    fn count(&self, metric: Metric) -> u64 {
        match metric {
            Metric::Comments => self.comments,
            Metric::SuperChatCount => self.super_chat_count,
            Metric::GiftCount => self.gift_count,
            Metric::MembershipEvents => self.membership_events,
            Metric::Streams => self.streams,
            Metric::PeakConcurrentViewers => self.peak_concurrent_viewers,
            _ => 0,
        }
    }

    fn has_paid_or_membership_activity(&self) -> bool {
        self.super_chat_count != 0
            || self.gift_count != 0
            || self.membership_events != 0
            || !self.donation.is_empty()
    }
}

fn add_amount(totals: &mut BTreeMap<String, i64>, currency: &str, value: i64) {
    if value == 0 {
        return;
    }
    let total = totals.entry(currency.to_owned()).or_default();
    *total = total.saturating_add(value);
    if *total == 0 {
        totals.remove(currency);
    }
}

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct ViewerProgress {
    #[serde(flatten)]
    lifetime: Counters,
    #[serde(skip_serializing_if = "zero")]
    streams_attended: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_epoch: Option<String>,
    #[serde(skip_serializing_if = "zero")]
    epoch_comments: u64,
    #[serde(skip_serializing_if = "is_empty_epoch_counters")]
    epoch_counters: Counters,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
}

fn is_empty_epoch_counters(counters: &Counters) -> bool {
    !counters.has_paid_or_membership_activity()
}

impl ViewerProgress {
    fn observe(&mut self, msg: &ChatMessage, epoch: &str) {
        self.lifetime.add_message(msg);
        if self.last_epoch.as_deref() != Some(epoch) {
            self.streams_attended = self.streams_attended.saturating_add(1);
            self.last_epoch = Some(epoch.to_owned());
            self.epoch_comments = 0;
            self.epoch_counters = Counters::default();
        }
        self.epoch_comments = self.epoch_comments.saturating_add(1);
        self.epoch_counters.add_message_kind(msg);
        if self.name.as_deref() != Some(&msg.author.name) {
            self.name = Some(msg.author.name.clone());
        }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Progress {
    version: u32,
    enabled_at_ms: u64,
    #[serde(flatten)]
    sessions: Sessions,
    streamer: Counters,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    streamer_by_platform: BTreeMap<String, Counters>,
    viewers: BTreeMap<String, ViewerProgress>,
    unlocks: BTreeMap<String, u64>,
}

impl Progress {
    fn new(now_ms: u64) -> Self {
        Self {
            version: STORE_VERSION,
            enabled_at_ms: now_ms,
            sessions: Sessions::default(),
            streamer: Counters::default(),
            streamer_by_platform: BTreeMap::new(),
            viewers: BTreeMap::new(),
            unlocks: BTreeMap::new(),
        }
    }

    pub(super) fn remove_listener(&mut self, subject: &str) -> bool {
        let mut removed = self.viewers.remove(subject).is_some();
        if let Some(epoch) = &mut self.sessions.epoch {
            removed |= epoch.unique_commenters.remove(subject);
        }
        let subject = encode_key_part(subject);
        self.unlocks.retain(|key, _| {
            let matches = record_parts(key, &subject).is_some();
            removed |= matches;
            !matches
        });
        removed
    }

    fn records(&self, subject: &str) -> Vec<UnlockRecord> {
        let subject = encode_key_part(subject);
        self.unlocks
            .iter()
            .filter_map(|(key, &unlocked_at_ms)| {
                let (achievement_id, window_key) = record_parts(key, &subject)?;
                Some(UnlockRecord {
                    achievement_id: achievement_id.replace("%7C", "|").replace("%25", "%"),
                    window_key: window_key.to_owned(),
                    unlocked_at_ms,
                })
            })
            .collect()
    }
}

fn record_parts<'a>(key: &'a str, subject: &str) -> Option<(&'a str, &'a str)> {
    let (achievement, rest) = key.split_once('|')?;
    let (saved_subject, window_key) = rest.split_once('|')?;
    (saved_subject == subject).then_some((achievement, window_key))
}

fn encode_key_part(part: &str) -> Cow<'_, str> {
    // Keep the v1 spelling for ordinary IDs and reserve the separator for the tuple itself.
    if part.contains(['%', '|']) {
        return Cow::Owned(part.replace('%', "%25").replace('|', "%7C"));
    }
    Cow::Borrowed(part)
}

#[derive(Default)]
struct RecentIds {
    most_recent: HashMap<Arc<str>, u64>,
    ordered: BTreeMap<u64, Arc<str>>,
    sequence: u64,
}

impl RecentIds {
    fn insert(&mut self, id: &str) -> bool {
        self.sequence += 1;
        if let Some(previous) = self.most_recent.get_mut(id) {
            let previous = std::mem::replace(previous, self.sequence);
            if let Some(entry) = self.ordered.remove(&previous) {
                self.ordered.insert(self.sequence, entry);
            }
            return false;
        }
        if self.ordered.len() == DEDUP_CAPACITY {
            if let Some((_, oldest)) = self.ordered.pop_first() {
                self.most_recent.remove(&oldest);
            }
        }
        let id: Arc<str> = Arc::from(id);
        self.most_recent.insert(Arc::clone(&id), self.sequence);
        self.ordered.insert(self.sequence, id);
        true
    }
}

type Candidates = BTreeMap<String, AchievementUnlocked>;

struct State {
    defs: AchievementDefs,
    progress: Progress,
    recent: BTreeMap<(&'static str, String), RecentIds>,
    retry: BTreeSet<String>,
    closed_epoch_retries: Candidates,
    flushed_unlocks: Vec<AchievementUnlocked>,
    now_ms: u64,
}

pub struct AchievementEngine {
    file: ProgressFile,
    state: Mutex<State>,
}

impl AchievementEngine {
    pub fn open(dir: &Path, defs: AchievementDefs, now_ms: u64) -> Self {
        Self::open_with(dir, defs, now_ms, Arc::new(FsIo))
    }

    pub fn open_with(dir: &Path, defs: AchievementDefs, now_ms: u64, io: Arc<dyn StoreIo>) -> Self {
        let (file, progress) = ProgressFile::open(dir, now_ms, io);
        let mut progress = progress.unwrap_or_else(|| Progress::new(now_ms));
        progress.sessions.expire(now_ms);
        Self {
            file,
            state: Mutex::new(State {
                defs,
                progress,
                recent: BTreeMap::new(),
                retry: BTreeSet::new(),
                closed_epoch_retries: Candidates::new(),
                flushed_unlocks: Vec::new(),
                now_ms,
            }),
        }
    }

    pub fn set_definitions(&self, defs: AchievementDefs, now_ms: u64) -> Vec<AchievementUnlocked> {
        let mut state = self.lock();
        state.update_closed_epoch_retries(&defs);
        state.defs = defs;
        self.process(&mut state, now_ms, |state, candidates| {
            state.evaluate(STREAMER, candidates);
            for subject in state.progress.viewers.keys() {
                state.evaluate(subject, candidates);
            }
        })
    }

    pub fn observe(&self, msg: &ChatMessage, now_ms: u64) -> Vec<AchievementUnlocked> {
        let mut state = self.lock();
        self.process(&mut state, now_ms, |state, candidates| {
            let platform = platform_key(msg.platform);
            let recent = state
                .recent
                .entry((platform, msg.channel.clone()))
                .or_default();
            if !recent.insert(&msg.id)
                || msg.route.uncounted
                || msg.kind == MessageKind::System
                || msg.injected_test
                || msg.author.id.is_empty()
            {
                return;
            }

            let progress = &mut state.progress;
            let epoch = progress.sessions.observe(msg, state.now_ms);
            epoch.streamer.add_message(msg);
            epoch
                .by_platform
                .entry(platform.to_owned())
                .or_default()
                .add_message(msg);
            progress.streamer.add_message(msg);
            progress
                .streamer_by_platform
                .entry(platform.to_owned())
                .or_default()
                .add_message(msg);

            if !is_shared_anonymous(msg) {
                let subject = format!("{platform}:{}", msg.author.id);
                epoch.unique_commenters.insert(subject.clone());
                progress
                    .viewers
                    .entry(subject.clone())
                    .or_default()
                    .observe(msg, &epoch.key);
                state.evaluate(&subject, candidates);
            }
            state.evaluate(STREAMER, candidates);
        })
    }

    pub fn on_live_event(&self, ev: &LiveEvent, now_ms: u64) -> Vec<AchievementUnlocked> {
        let mut state = self.lock();
        self.process(&mut state, now_ms, |state, candidates| {
            let progress = &mut state.progress;
            if let Some(platform) = progress.sessions.on_live_event(ev, state.now_ms) {
                let platform = platform_key(platform);
                let counters = progress
                    .streamer_by_platform
                    .entry(platform.to_owned())
                    .or_default();
                counters.streams = counters.streams.saturating_add(1);
                progress.streamer.streams = progress.streamer.streams.saturating_add(1);

                if let Some(epoch) = &mut progress.sessions.epoch {
                    let counters = epoch.by_platform.entry(platform.to_owned()).or_default();
                    counters.streams = counters.streams.saturating_add(1);
                    epoch.streamer.streams = epoch.streamer.streams.saturating_add(1);
                }
                state.evaluate(STREAMER, candidates);
            }
        })
    }

    pub fn on_stats(&self, snapshot: &StatsSnapshot, now_ms: u64) -> Vec<AchievementUnlocked> {
        let mut state = self.lock();
        self.process(&mut state, now_ms, |state, candidates| {
            if let Some(epoch) = &mut state.progress.sessions.epoch {
                let peaks = &mut epoch.peak_by_platform_mask;
                let mut totals = [0u64; PLATFORMS.len()];
                for channel in &snapshot.channel_status {
                    if channel.live == Some(false)
                        || channel.viewers_kind != ViewerCountKind::Concurrent
                    {
                        continue;
                    }
                    if let Some(index) = PLATFORMS
                        .iter()
                        .position(|(_, key)| *key == channel.platform)
                    {
                        totals[index] = totals[index]
                            .saturating_add(u64::from(channel.viewers.unwrap_or_default()));
                    }
                }
                // Keep the peak of each simultaneous platform sum for later definition changes.
                for mask in 1..=ALL_PLATFORMS {
                    let sum = totals
                        .iter()
                        .enumerate()
                        .filter(|(index, _)| mask & (1 << index) != 0)
                        .fold(0u64, |sum, (_, viewers)| sum.saturating_add(*viewers));
                    if sum != 0 && mask != ALL_PLATFORMS {
                        let peak = peaks.entry(mask.to_string()).or_default();
                        *peak = (*peak).max(sum);
                    } else if mask == ALL_PLATFORMS {
                        let peak = &mut epoch.streamer.peak_concurrent_viewers;
                        *peak = (*peak).max(sum);
                    }
                }
                state.evaluate(STREAMER, candidates);
            }
        })
    }

    pub fn session_for(&self, msg: &ChatMessage) -> Option<String> {
        self.lock()
            .progress
            .sessions
            .session_for(msg)
            .map(str::to_owned)
    }

    pub fn current_epoch(&self) -> Option<String> {
        self.lock()
            .progress
            .sessions
            .epoch
            .as_ref()
            .map(|epoch| epoch.key.clone())
    }

    pub fn listener_unlocks(&self, platform: &str, author_id: &str) -> Vec<UnlockRecord> {
        self.lock()
            .progress
            .records(&format!("{platform}:{author_id}"))
    }

    pub fn streamer_unlocks(&self) -> Vec<UnlockRecord> {
        self.lock().progress.records(STREAMER)
    }

    pub fn forget_listener(&self, platform: &str, author_id: &str) -> anyhow::Result<()> {
        let mut state = self.lock();
        let subject = format!("{platform}:{author_id}");
        // Keep memory and disk in step when a deletion cannot be saved.
        let mut next = state.progress.clone();
        next.remove_listener(&subject);
        self.file.save(&next)?;
        state.progress = next;
        state.retry.remove(&subject);
        let other_listener = |unlock: &AchievementUnlocked| {
            unlock.subject.platform.map(platform_key) != Some(platform)
                || unlock.subject.author_id.as_deref() != Some(author_id)
        };
        state.flushed_unlocks.retain(other_listener);
        state
            .closed_epoch_retries
            .retain(|_, unlock| other_listener(unlock));
        Ok(())
    }

    pub fn enabled_at_ms(&self) -> u64 {
        self.lock().progress.enabled_at_ms
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        let mut state = self.lock();
        let mut candidates = if state.defs.enabled {
            std::mem::take(&mut state.closed_epoch_retries)
        } else {
            Candidates::new()
        };
        state.evaluate_retries(&mut candidates);
        let saved = self.persist(&mut state, candidates, true)?;
        // Publish via the next event since flush has no unlock return value.
        state.flushed_unlocks.extend(saved);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn process(
        &self,
        state: &mut State,
        now_ms: u64,
        update: impl FnOnce(&mut State, &mut Candidates),
    ) -> Vec<AchievementUnlocked> {
        state.now_ms = state.now_ms.max(now_ms);
        if !state.defs.enabled {
            return Vec::new();
        }
        let mut candidates = std::mem::take(&mut state.closed_epoch_retries);
        // Retry the previous window before applying any epoch rollover for this event.
        state.evaluate_retries(&mut candidates);
        state.progress.sessions.expire(state.now_ms);
        update(state, &mut candidates);
        let mut result = std::mem::take(&mut state.flushed_unlocks);
        if let Ok(saved) = self.persist(state, candidates, false) {
            result.extend(saved);
        }
        result
    }

    fn persist(
        &self,
        state: &mut State,
        candidates: Candidates,
        flush: bool,
    ) -> Result<Vec<AchievementUnlocked>, CommitError> {
        state.retry.clear();
        if candidates.is_empty() && !flush {
            return Ok(Vec::new());
        }
        for (key, unlock) in &candidates {
            state
                .progress
                .unlocks
                .insert(key.clone(), unlock.unlocked_at_ms);
        }
        if let Err(error) = self.file.save(&state.progress) {
            let current_window = state
                .progress
                .sessions
                .epoch
                .as_ref()
                .map(|e| e.key.as_str());
            for (key, unlock) in candidates {
                state.progress.unlocks.remove(&key);
                if unlock.window_key != LIFETIME
                    && Some(unlock.window_key.as_str()) != current_window
                {
                    state.closed_epoch_retries.insert(key, unlock);
                    continue;
                }
                let subject = match (&unlock.subject.platform, &unlock.subject.author_id) {
                    (Some(platform), Some(author_id)) => {
                        format!("{}:{author_id}", platform_key(*platform))
                    }
                    _ => STREAMER.to_owned(),
                };
                state.retry.insert(subject);
            }
            return Err(error);
        }
        Ok(candidates.into_values().collect())
    }
}

impl State {
    fn update_closed_epoch_retries(&mut self, defs: &AchievementDefs) {
        let previous = &self.defs;
        self.closed_epoch_retries.retain(|_, unlock| {
            let old = previous
                .achievements
                .iter()
                .find(|a| a.id == unlock.achievement_id);
            let new = defs
                .achievements
                .iter()
                .find(|a| a.id == unlock.achievement_id);
            let (Some(old), Some(new)) = (old, new) else {
                return false;
            };
            if !new.enabled || new.subject != old.subject || new.condition != old.condition {
                return false;
            }
            if unlock.currency.is_some()
                && (defs.rates != previous.rates
                    || defs.symbol_overrides != previous.symbol_overrides)
            {
                return false;
            }
            unlock.name.clone_from(&new.name);
            unlock.description.clone_from(&new.description);
            unlock.icon.clone_from(&new.icon);
            unlock.tier = new.tier;
            true
        });
    }

    fn evaluate_retries(&self, candidates: &mut Candidates) {
        if self.defs.enabled {
            for subject in &self.retry {
                self.evaluate(subject, candidates);
            }
        }
    }

    fn evaluate(&self, subject: &str, candidates: &mut Candidates) {
        let viewer = self.progress.viewers.get(subject);
        let (kind, platform, author_id) = if subject == STREAMER {
            (SubjectKind::Streamer, None, None)
        } else {
            let Some((platform, author_id)) = subject.split_once(':') else {
                return;
            };
            let Some((platform, _)) = PLATFORMS.iter().find(|(_, key)| *key == platform) else {
                return;
            };
            if viewer.is_none() {
                return;
            }
            (SubjectKind::Viewer, Some(*platform), Some(author_id))
        };
        let epoch = self.progress.sessions.epoch.as_ref();
        let achievements = self
            .defs
            .achievements
            .iter()
            .filter(|a| a.enabled && a.subject == kind);
        let unlocked = |key: &str, candidates: &Candidates| {
            self.progress.unlocks.contains_key(key) || candidates.contains_key(key)
        };
        let subject_key = encode_key_part(subject);
        let mut lifetime_total = 0;
        let mut lifetime_unlocked = 0;
        let mut platinum = None;
        let subject_info = || UnlockSubject {
            kind,
            platform,
            author_id: author_id.map(str::to_owned),
            name: viewer.and_then(|v| v.name.clone()),
        };

        for achievement in achievements {
            if achievement.tier == Tier::Platinum {
                platinum = Some(achievement);
                continue;
            }
            let Some(condition) = &achievement.condition else {
                continue;
            };
            let window_key = match condition.window {
                Window::Lifetime => LIFETIME,
                Window::PerStream => {
                    let Some(epoch) = epoch else {
                        continue;
                    };
                    &epoch.key
                }
            };
            let key = format!(
                "{}|{subject_key}|{window_key}",
                encode_key_part(&achievement.id)
            );
            if !unlocked(&key, candidates)
                && platform.is_none_or(|p| {
                    condition.platforms.is_empty() || condition.platforms.contains(&p)
                })
            {
                let value = if let Some(viewer) = viewer {
                    self.viewer_value(viewer, condition)
                } else {
                    self.streamer_value(condition)
                };
                if value as f64 / 1000.0 >= condition.threshold {
                    candidates.insert(
                        key.clone(),
                        self.unlock(
                            achievement,
                            subject_info(),
                            window_key,
                            value,
                            condition.threshold,
                            Some(&condition.currency),
                        ),
                    );
                }
            }
            if condition.window == Window::Lifetime {
                lifetime_total += 1;
                lifetime_unlocked += u64::from(unlocked(&key, candidates));
            }
        }

        if let Some(achievement) =
            platinum.filter(|_| lifetime_total > 0 && lifetime_total == lifetime_unlocked)
        {
            let key = format!(
                "{}|{subject_key}|{LIFETIME}",
                encode_key_part(&achievement.id)
            );
            if !unlocked(&key, candidates) {
                candidates.insert(
                    key,
                    self.unlock(
                        achievement,
                        subject_info(),
                        LIFETIME,
                        milli(lifetime_total),
                        lifetime_total as f64,
                        None,
                    ),
                );
            }
        }
    }

    fn viewer_value(&self, viewer: &ViewerProgress, condition: &Condition) -> i64 {
        let counters = if condition.window == Window::Lifetime {
            if condition.metric == Metric::StreamsAttended {
                return milli(viewer.streams_attended);
            }
            &viewer.lifetime
        } else {
            let epoch = self
                .progress
                .sessions
                .epoch
                .as_ref()
                .map(|e| e.key.as_str());
            if viewer.last_epoch.as_deref() != epoch || epoch.is_none() {
                return 0;
            }
            match condition.metric {
                Metric::Comments => return milli(viewer.epoch_comments),
                Metric::StreamsAttended => return milli(1),
                _ => &viewer.epoch_counters,
            }
        };
        self.value(std::iter::once(counters), condition)
    }

    fn streamer_value(&self, condition: &Condition) -> i64 {
        let platforms = &condition.platforms;
        let mask = if platforms.is_empty() {
            ALL_PLATFORMS
        } else {
            PLATFORMS
                .iter()
                .enumerate()
                .fold(0, |mask, (index, (platform, _))| {
                    mask | (u8::from(platforms.contains(platform)) << index)
                })
        };
        let (total, by_platform) = if condition.window == Window::Lifetime {
            (&self.progress.streamer, &self.progress.streamer_by_platform)
        } else {
            let Some(epoch) = &self.progress.sessions.epoch else {
                return 0;
            };
            match condition.metric {
                Metric::UniqueCommenters if mask == ALL_PLATFORMS => {
                    return milli(epoch.unique_commenters.len() as u64);
                }
                Metric::UniqueCommenters => {
                    let count = epoch
                        .unique_commenters
                        .iter()
                        .filter(|key| {
                            key.split_once(':').is_some_and(|(platform, _)| {
                                PLATFORMS.iter().enumerate().any(|(index, (_, name))| {
                                    mask & (1 << index) != 0 && *name == platform
                                })
                            })
                        })
                        .count();
                    return milli(count as u64);
                }
                Metric::PeakConcurrentViewers if mask != ALL_PLATFORMS => {
                    return milli(
                        epoch
                            .peak_by_platform_mask
                            .get(&mask.to_string())
                            .copied()
                            .unwrap_or_default(),
                    );
                }
                _ => {}
            }
            (&epoch.streamer, &epoch.by_platform)
        };
        if mask == ALL_PLATFORMS {
            return self.value(std::iter::once(total), condition);
        }
        let counters = PLATFORMS
            .iter()
            .enumerate()
            .filter(|(index, _)| mask & (1 << index) != 0)
            .filter_map(|(_, (_, key))| by_platform.get(*key));
        self.value(counters, condition)
    }

    fn value<'a>(
        &self,
        mut counters: impl Iterator<Item = &'a Counters>,
        condition: &Condition,
    ) -> i64 {
        if condition.metric != Metric::DonationAmount {
            return milli(counters.fold(0u64, |total, c| {
                total.saturating_add(c.count(condition.metric))
            }));
        }
        let Some(first) = counters.next() else {
            return 0;
        };
        let mut totals = AmountTotals(first.donation.clone());
        for counters in counters {
            for (currency, value) in &counters.donation {
                add_amount(&mut totals.0, currency, *value);
            }
        }
        match &condition.currency {
            CurrencyMode::None => 0,
            CurrencyMode::JpyConverted => {
                totals.jpy_milli(&self.defs.rates, &self.defs.symbol_overrides)
            }
            CurrencyMode::Only(code) => totals.only_milli(code, &self.defs.symbol_overrides),
        }
    }

    fn unlock(
        &self,
        achievement: &Achievement,
        subject: UnlockSubject,
        window_key: &str,
        value_milli: i64,
        threshold: f64,
        currency: Option<&CurrencyMode>,
    ) -> AchievementUnlocked {
        let currency = match currency {
            Some(CurrencyMode::JpyConverted) => Some("JPY".to_owned()),
            Some(CurrencyMode::Only(code)) => Some(code.clone()),
            _ => None,
        };
        AchievementUnlocked {
            id: Uuid::new_v4().to_string(),
            achievement_id: achievement.id.clone(),
            name: achievement.name.clone(),
            description: achievement.description.clone(),
            tier: achievement.tier,
            icon: achievement.icon.clone(),
            subject,
            value_milli,
            threshold_milli: threshold_to_milli(threshold),
            currency,
            window_key: window_key.to_owned(),
            unlocked_at_ms: self.now_ms,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LiveStream;
    use crate::stats::ChannelStatus;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FailTwice(AtomicUsize);

    impl StoreIo for FailTwice {
        fn write_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            if self.0.fetch_add(1, Ordering::Relaxed) < 2 {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            FsIo.write_atomic(path, bytes)
        }

        fn create_backup(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
            FsIo.create_backup(src, dst)
        }
    }

    #[test]
    fn failed_per_stream_unlock_survives_epoch_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let mut defs = AchievementDefs::default();
        let mut achievement = defs.achievements.remove(0);
        achievement.subject = SubjectKind::Streamer;
        achievement.tier = Tier::Bronze;
        achievement.enabled = true;
        achievement.condition = Some(Condition {
            window: Window::PerStream,
            metric: Metric::Streams,
            threshold: 1.0,
            currency: CurrencyMode::None,
            platforms: Vec::new(),
        });
        defs.enabled = true;
        defs.achievements = vec![achievement];

        let engine = AchievementEngine::open_with(
            dir.path(),
            defs,
            1,
            Arc::new(FailTwice(AtomicUsize::new(0))),
        );
        let event = LiveEvent::Started(LiveStream {
            platform: Platform::Youtube,
            channel_identifier: "channel".to_owned(),
            stream_id: "stream".to_owned(),
            title: None,
            url: String::new(),
            started_at_ms: None,
        });
        let stats = StatsSnapshot::default();
        assert!(engine.on_live_event(&event, 1).is_empty());
        assert!(engine.on_stats(&stats, STALE_SESSION_MS + 1).is_empty());
        assert_eq!(engine.current_epoch(), None);
        assert!(engine.streamer_unlocks().is_empty());

        let unlocks = engine.on_stats(&stats, STALE_SESSION_MS + 2);
        assert_eq!(unlocks.len(), 1);
        assert_eq!(unlocks[0].window_key, "e:1");
        assert!(engine.on_stats(&stats, STALE_SESSION_MS + 3).is_empty());
    }

    #[test]
    fn peak_and_unlock_survive_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let mut defs = AchievementDefs::default();
        let mut achievement = defs.achievements.remove(0);
        achievement.subject = SubjectKind::Streamer;
        achievement.tier = Tier::Bronze;
        achievement.enabled = true;
        achievement.condition = Some(Condition {
            window: Window::PerStream,
            metric: Metric::PeakConcurrentViewers,
            threshold: 3.0,
            currency: CurrencyMode::None,
            platforms: vec![Platform::Youtube],
        });
        defs.enabled = true;
        defs.achievements = vec![achievement];
        let engine = AchievementEngine::open(dir.path(), defs.clone(), 1);
        engine.on_live_event(
            &LiveEvent::Started(LiveStream {
                platform: Platform::Youtube,
                channel_identifier: "channel".to_owned(),
                stream_id: "stream".to_owned(),
                title: None,
                url: String::new(),
                started_at_ms: None,
            }),
            1,
        );
        let snapshot = StatsSnapshot {
            channel_status: vec![ChannelStatus {
                platform: "youtube".to_owned(),
                viewers: Some(3),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(engine.on_stats(&snapshot, 2).len(), 1);
        drop(engine);

        let reopened = AchievementEngine::open(dir.path(), defs.clone(), 3);
        assert_eq!(reopened.enabled_at_ms(), 1);
        assert_eq!(reopened.streamer_unlocks().len(), 1);
        defs.achievements[0].id = "added-after-reopen".to_owned();
        assert_eq!(reopened.set_definitions(defs, 3)[0].value_milli, 3_000);
    }

    #[test]
    fn deleting_one_listener_does_not_match_a_suffix_of_another_listener() {
        let mut progress = Progress::new(1);
        let achievement = encode_key_part("first|%");
        let subject = encode_key_part("twitch:a|twitch:b");
        progress
            .unlocks
            .insert(format!("{achievement}|{subject}|*"), 2);
        progress.unlocks.insert("first|twitch:b|*".to_owned(), 2);
        assert_eq!(progress.records("twitch:b").len(), 1);
        assert!(progress.remove_listener("twitch:b"));
        assert_eq!(
            progress.records("twitch:a|twitch:b")[0].achievement_id,
            "first|%"
        );
    }

    #[test]
    fn older_engine_handle_cannot_restore_deleted_progress() {
        let dir = tempfile::tempdir().unwrap();
        let defs = AchievementDefs::default();
        let engine = AchievementEngine::open(dir.path(), defs.clone(), 1);
        engine
            .lock()
            .progress
            .viewers
            .insert("twitch:u".to_owned(), ViewerProgress::default());
        engine.flush().unwrap();

        let older = AchievementEngine::open(dir.path(), defs, 2);
        assert!(super::super::store::delete_listener_progress(dir.path(), "twitch", "u").unwrap());
        assert!(older.flush().is_err());
        assert!(!super::super::store::delete_listener_progress(dir.path(), "twitch", "u").unwrap());
    }

    #[test]
    fn deletion_also_invalidates_unflushed_counters() {
        for has_file in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let engine = AchievementEngine::open(dir.path(), AchievementDefs::default(), 1);
            if has_file {
                engine.flush().unwrap();
            }
            engine
                .lock()
                .progress
                .viewers
                .insert("twitch:u".to_owned(), ViewerProgress::default());
            assert!(
                !super::super::store::delete_listener_progress(dir.path(), "twitch", "u").unwrap()
            );
            assert!(engine.flush().is_err());
        }
    }

    #[test]
    fn duplicate_ids_refresh_channel_recency() {
        let mut recent = RecentIds::default();
        for id in 0..DEDUP_CAPACITY {
            assert!(recent.insert(&id.to_string()));
        }
        assert!(!recent.insert("0"));
        assert!(recent.insert("new"));
        assert!(!recent.insert("0"));
        assert!(recent.insert("1"));
    }
}
