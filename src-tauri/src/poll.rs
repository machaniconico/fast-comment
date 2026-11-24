//! コメント投票の集計と、IPC / OBS 共通のスナップショット。
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use regex::{RegexSet, RegexSetBuilder};
use serde::{Deserialize, Deserializer, Serialize};
use tokio::sync::{watch, Notify};
use tokio::time::{sleep_until, Instant};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::model::{ChatMessage, MessageKind, Platform};
use crate::moderation::Verdict;

const MAX_VOTERS: usize = 100_000;
const VOTE_BATCH: Duration = Duration::from_millis(100);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PollDisplay {
    #[default]
    BarH,
    BarV,
    Pie,
    Donut,
    Number,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PollDisplaySettings {
    pub display: PollDisplay,
    pub show_percent: bool,
    pub show_counts: bool,
}

impl Default for PollDisplaySettings {
    fn default() -> Self {
        Self {
            display: PollDisplay::BarH,
            show_percent: true,
            show_counts: true,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PollPreferences {
    pub question: String,
    pub options: Vec<String>,
    #[serde(flatten)]
    pub settings: PollDisplaySettings,
}

impl Default for PollPreferences {
    fn default() -> Self {
        Self {
            question: String::new(),
            options: vec![String::new(); 2],
            settings: PollDisplaySettings::default(),
        }
    }
}

impl PollPreferences {
    pub(crate) fn load<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = serde_json::Value::deserialize(deserializer)?;
        if !raw.is_object() {
            return Ok(Self::default());
        }
        let Ok(mut preferences) = serde_json::from_value::<Self>(raw) else {
            return Ok(Self::default());
        };
        // 未使用のフォームでは表示設定だけ保存されることもある。
        if preferences.question.is_empty() && preferences.options == Self::default().options {
            return Ok(preferences);
        }
        if preferences.validate_text().is_err() {
            return Ok(Self::default());
        }
        Ok(preferences)
    }

    fn validate_text(&mut self) -> Result<RegexSet, PollError> {
        self.question = self.question.trim().to_owned();
        if !(1..=100).contains(&self.question.chars().count()) {
            return Err(PollError::Invalid("質問は1〜100文字で入力してください"));
        }
        if !(2..=10).contains(&self.options.len()) {
            return Err(PollError::Invalid("選択肢は2〜10件で入力してください"));
        }
        for label in &mut self.options {
            *label = label.trim().to_owned();
            if !(1..=40).contains(&label.chars().count()) {
                return Err(PollError::Invalid("各選択肢は1〜40文字で入力してください"));
            }
        }
        let labels = RegexSetBuilder::new(
            self.options
                .iter()
                .map(|label| format!(r"\A{}\z", regex::escape(label))),
        )
        .case_insensitive(true)
        .build()
        .map_err(|_| PollError::Invalid("選択肢を読み取れませんでした"))?;
        if self
            .options
            .iter()
            .any(|label| labels.matches(label).iter().count() > 1)
        {
            return Err(PollError::Invalid("同じ選択肢は指定できません"));
        }
        Ok(labels)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PollStart {
    pub question: String,
    pub options: Vec<String>,
    pub duration_secs: Option<u32>,
    pub display: PollDisplay,
    pub show_percent: bool,
    pub show_counts: bool,
}

impl PollStart {
    pub(crate) fn validate(self) -> Result<ValidatedStart, PollError> {
        if self
            .duration_secs
            .is_some_and(|secs| !(10..=3600).contains(&secs))
        {
            return Err(PollError::Invalid("投票時間は10〜3600秒で指定してください"));
        }
        let mut preferences = PollPreferences {
            question: self.question,
            options: self.options,
            settings: PollDisplaySettings {
                display: self.display,
                show_percent: self.show_percent,
                show_counts: self.show_counts,
            },
        };
        let labels = preferences.validate_text()?;
        Ok(ValidatedStart {
            preferences,
            labels,
            duration_secs: self.duration_secs,
        })
    }
}

pub(crate) struct ValidatedStart {
    preferences: PollPreferences,
    labels: RegexSet,
    duration_secs: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum PollSource {
    #[default]
    Chat,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum PollStatus {
    #[default]
    Idle,
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PollOption {
    pub label: String,
    pub votes: u32,
    pub percent: f64,
    pub leading: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PollSnapshot {
    pub id: String,
    source: PollSource,
    pub status: PollStatus,
    pub question: String,
    pub options: Vec<PollOption>,
    pub total_voters: u32,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ends_at_ms: Option<u64>,
    #[serde(flatten)]
    pub settings: PollDisplaySettings,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PollError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("受付中のアンケートがあります")]
    AlreadyOpen,
    #[error("アンケートの設定を保存できませんでした")]
    Save,
}

pub(crate) enum PreferenceChange<'a> {
    Started(&'a PollPreferences),
    Display(PollDisplaySettings),
}

type SavePreferences = dyn Fn(PreferenceChange<'_>) -> Result<(), PollError> + Send + Sync;
type EmitSnapshot = dyn Fn(&PollSnapshot) + Send + Sync;

struct CurrentPoll {
    snapshot: PollSnapshot,
    labels: RegexSet,
    voters: HashMap<Platform, HashMap<String, usize>>,
    deadline: Option<Instant>,
    flush_at: Option<Instant>,
}

impl Default for CurrentPoll {
    fn default() -> Self {
        Self {
            snapshot: PollSnapshot::default(),
            labels: RegexSet::empty(),
            voters: HashMap::new(),
            deadline: None,
            flush_at: None,
        }
    }
}

pub(crate) struct Poll {
    // 永続化を操作間で直列化する。コメント処理と時計はこのロックを取らない。
    controls: Mutex<()>,
    current: Mutex<CurrentPoll>,
    tx: watch::Sender<PollSnapshot>,
    wake: Notify,
    save_preferences: Box<SavePreferences>,
    emit_snapshot: Box<EmitSnapshot>,
}

impl Poll {
    pub(crate) fn new(
        save_preferences: impl Fn(PreferenceChange<'_>) -> Result<(), PollError> + Send + Sync + 'static,
        emit_snapshot: impl Fn(&PollSnapshot) + Send + Sync + 'static,
    ) -> Self {
        Self {
            controls: Mutex::new(()),
            current: Mutex::new(CurrentPoll::default()),
            tx: watch::channel(PollSnapshot::default()).0,
            wake: Notify::new(),
            save_preferences: Box::new(save_preferences),
            emit_snapshot: Box::new(emit_snapshot),
        }
    }

    pub(crate) fn sender(&self) -> watch::Sender<PollSnapshot> {
        self.tx.clone()
    }

    pub(crate) fn snapshot(&self) -> PollSnapshot {
        self.tx.borrow().clone()
    }

    pub(crate) fn start(&self, start: ValidatedStart) -> Result<PollSnapshot, PollError> {
        let _controls = self.controls.lock().unwrap();
        {
            let mut current = self.current.lock().unwrap();
            if current
                .deadline
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                self.close(&mut current);
            }
            if current.snapshot.status == PollStatus::Open {
                return Err(PollError::AlreadyOpen);
            }
        }
        (self.save_preferences)(PreferenceChange::Started(&start.preferences))?;

        let mut current = self.current.lock().unwrap();
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let now = Instant::now();
        let PollPreferences {
            question,
            options,
            settings,
        } = start.preferences;
        *current = CurrentPoll {
            snapshot: PollSnapshot {
                id: Uuid::new_v4().to_string(),
                source: PollSource::Chat,
                status: PollStatus::Open,
                question,
                options: options
                    .into_iter()
                    .map(|label| PollOption {
                        label,
                        votes: 0,
                        percent: 0.0,
                        leading: false,
                    })
                    .collect(),
                total_voters: 0,
                started_at_ms,
                ends_at_ms: start
                    .duration_secs
                    .map(|secs| started_at_ms + u64::from(secs) * 1000),
                settings,
            },
            labels: start.labels,
            deadline: start
                .duration_secs
                .map(|secs| now + Duration::from_secs(secs.into())),
            ..CurrentPoll::default()
        };
        self.publish(&mut current);
        self.wake.notify_one();
        Ok(current.snapshot.clone())
    }

    pub(crate) fn stop(&self) -> PollSnapshot {
        let _controls = self.controls.lock().unwrap();
        let mut current = self.current.lock().unwrap();
        if current.snapshot.status == PollStatus::Open {
            self.close(&mut current);
            self.wake.notify_one();
        }
        current.snapshot.clone()
    }

    pub(crate) fn reset(&self) -> PollSnapshot {
        let _controls = self.controls.lock().unwrap();
        let mut current = self.current.lock().unwrap();
        if current.snapshot != PollSnapshot::default() {
            *current = CurrentPoll::default();
            self.publish(&mut current);
            self.wake.notify_one();
        }
        current.snapshot.clone()
    }

    pub(crate) fn set_display(
        &self,
        settings: PollDisplaySettings,
    ) -> Result<PollSnapshot, PollError> {
        let _controls = self.controls.lock().unwrap();
        (self.save_preferences)(PreferenceChange::Display(settings))?;
        let mut current = self.current.lock().unwrap();
        let expired = current
            .deadline
            .is_some_and(|deadline| deadline <= Instant::now());
        if settings == current.snapshot.settings && !expired {
            return Ok(self.snapshot());
        }
        current.snapshot.settings = settings;
        if expired {
            self.close(&mut current);
        } else {
            self.publish(&mut current);
        }
        Ok(current.snapshot.clone())
    }

    pub(crate) fn record(&self, msg: &ChatMessage, verdict: &Verdict) -> bool {
        if !eligible(msg, verdict) {
            return false;
        }
        let mut current = self.current.lock().unwrap();
        let now = Instant::now();
        if current.snapshot.status != PollStatus::Open
            || current.deadline.is_some_and(|deadline| deadline <= now)
        {
            return false;
        }
        let Some(index) = vote_index(&msg.plain_text(), &current.labels) else {
            return false;
        };

        let at_capacity = current.snapshot.total_voters as usize >= MAX_VOTERS;
        let voters = current.voters.entry(msg.platform).or_default();
        match voters.get_mut(&msg.author.id) {
            Some(previous) if *previous == index => return false,
            Some(previous) => {
                let previous = std::mem::replace(previous, index);
                current.snapshot.options[previous].votes -= 1;
            }
            None if at_capacity => return false,
            None => {
                voters.insert(msg.author.id.clone(), index);
                current.snapshot.total_voters += 1;
            }
        }
        current.snapshot.options[index].votes += 1;
        if current.flush_at.is_none() {
            current.flush_at = Some(now + VOTE_BATCH);
            self.wake.notify_one();
        }
        true
    }

    pub(crate) async fn run(self: Arc<Self>, cancel: CancellationToken) {
        loop {
            let next = {
                let mut current = self.current.lock().unwrap();
                let now = Instant::now();
                if current.deadline.is_some_and(|deadline| deadline <= now) {
                    self.close(&mut current);
                } else if current.flush_at.is_some_and(|flush| flush <= now) {
                    self.publish(&mut current);
                }
                current.deadline.into_iter().chain(current.flush_at).min()
            };

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = self.wake.notified() => {},
                _ = async {
                    match next {
                        Some(at) => sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {},
            }
        }
    }

    fn close(&self, current: &mut CurrentPoll) {
        current.snapshot.status = PollStatus::Closed;
        current.deadline = None;
        current.voters.clear();
        self.publish(current);
    }

    fn publish(&self, current: &mut CurrentPoll) {
        current.flush_at = None;
        let total = current.snapshot.total_voters;
        let max_votes = current
            .snapshot
            .options
            .iter()
            .map(|option| option.votes)
            .max()
            .unwrap_or_default();
        for option in &mut current.snapshot.options {
            option.percent = if total == 0 {
                0.0
            } else {
                f64::from(option.votes) * 100.0 / f64::from(total)
            };
            option.leading = option.votes == max_votes;
        }
        self.tx.send_replace(current.snapshot.clone());
        (self.emit_snapshot)(&current.snapshot);
    }
}

fn eligible(msg: &ChatMessage, verdict: &Verdict) -> bool {
    !matches!(verdict, Verdict::Hide)
        && msg.kind != MessageKind::System
        && !msg.route.uncounted
        && !msg.author.id.is_empty()
}

/// 投票の本文の前後を落とす。空白に加え、Twitch のクライアント(7TV・Chatterino など)が同じ文の連投を通すために
/// 末尾へ付ける U+E0000、BOM、ゼロ幅文字も落とす(付いたままだと 2 回目の「1」が数えられない)。
fn trim_vote(text: &str) -> &str {
    text.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '\u{E0000}' | '\u{FEFF}' | '\u{200B}' | '\u{200C}' | '\u{200D}' | '\u{2060}')
    })
}

fn vote_index(text: &str, labels: &RegexSet) -> Option<usize> {
    let text = trim_vote(text);
    let mut chars = text.chars();
    let number = match (chars.next(), chars.next(), chars.next()) {
        (Some(c @ '1'..='9'), None, None) => Some(c as usize - '1' as usize),
        (Some(c @ '１'..='９'), None, None) => Some(c as usize - '１' as usize),
        (Some('1' | '１'), Some('0' | '０'), None) => Some(9),
        _ => None,
    };
    number
        .filter(|&index| index < labels.len())
        .or_else(|| labels.matches(text).iter().next())
}

#[cfg(test)]
impl Default for Poll {
    fn default() -> Self {
        Self::new(|_| Ok(()), |_| {})
    }
}

#[cfg(test)]
#[path = "poll_tests.rs"]
mod tests;
