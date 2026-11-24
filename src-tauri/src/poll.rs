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
    #[serde(default, skip_serializing_if = "PollSource::is_chat")]
    pub source: PollSource,
    pub question: String,
    pub options: Vec<String>,
    #[serde(flatten)]
    pub settings: PollDisplaySettings,
}

impl Default for PollPreferences {
    fn default() -> Self {
        Self {
            source: PollSource::Chat,
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
        let valid = match preferences.source {
            PollSource::Chat => preferences.validate_chat_text().is_ok(),
            PollSource::Twitch => {
                let valid = crate::sources::twitch_polls::validate_twitch_text(
                    &preferences.question,
                    &preferences.options,
                )
                .is_ok();
                preferences.trim_remote_text();
                valid
            }
            PollSource::Youtube => {
                let valid = crate::sources::youtube_polls::validate_youtube(
                    &preferences.question,
                    &preferences.options,
                )
                .is_ok();
                preferences.trim_remote_text();
                valid
            }
        };
        if !valid {
            return Ok(Self::default());
        }
        Ok(preferences)
    }

    fn trim_remote_text(&mut self) {
        self.question = self.question.trim().to_owned();
        for option in &mut self.options {
            *option = option.trim().to_owned();
        }
    }

    fn validate_chat_text(&mut self) -> Result<RegexSet, PollError> {
        self.question = self.question.trim().to_owned();
        if !(1..=100).contains(&self.question.chars().count()) {
            return Err(PollError::Invalid("質問は1〜100文字で入力してください"));
        }
        if !(2..=10).contains(&self.options.len()) {
            return Err(PollError::Invalid("選択肢は2〜10件で入力してください"));
        }
        for label in &mut self.options {
            // 投票と同じ前処理(見えない文字も落とす)。そろえないと「青\u{200B}」と「青」が別ラベルで登録できてしまう。
            *label = trim_vote(label).to_owned();
            if !(1..=40).contains(&label.chars().count()) {
                return Err(PollError::Invalid("各選択肢は1〜40文字で入力してください"));
            }
        }
        unique_option_labels(&self.options).map_err(PollError::Invalid)
    }
}

pub(crate) fn unique_option_labels(options: &[String]) -> Result<RegexSet, &'static str> {
    let labels = RegexSetBuilder::new(
        options
            .iter()
            .map(|label| format!(r"\A{}\z", regex::escape(trim_vote(label)))),
    )
    .case_insensitive(true)
    .build()
    .map_err(|_| "選択肢を読み取れませんでした")?;
    if options
        .iter()
        .any(|label| labels.matches(trim_vote(label)).iter().count() > 1)
    {
        return Err("同じ選択肢は指定できません");
    }
    Ok(labels)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PollStart {
    #[serde(default)]
    pub source: PollSource,
    pub question: String,
    pub options: Vec<String>,
    pub duration_secs: Option<u32>,
    pub display: PollDisplay,
    pub show_percent: bool,
    pub show_counts: bool,
}

impl PollStart {
    pub fn validate(self) -> Result<ValidatedStart, PollError> {
        if self.source == PollSource::Twitch {
            crate::sources::twitch_polls::validate_twitch(
                &self.question,
                &self.options,
                self.duration_secs,
            )
            .map_err(PollError::Invalid)?;
        } else if self
            .duration_secs
            .is_some_and(|secs| !(10..=3600).contains(&secs))
        {
            return Err(PollError::Invalid("投票時間は10〜3600秒で指定してください"));
        }
        if self.source == PollSource::Youtube {
            crate::sources::youtube_polls::validate_youtube(&self.question, &self.options)
                .map_err(PollError::Invalid)?;
        }
        let mut preferences = PollPreferences {
            source: self.source,
            question: self.question,
            options: self.options,
            settings: PollDisplaySettings {
                display: self.display,
                show_percent: self.show_percent,
                show_counts: self.show_counts,
            },
        };
        let labels = match self.source {
            PollSource::Chat => preferences.validate_chat_text()?,
            PollSource::Twitch | PollSource::Youtube => {
                preferences.trim_remote_text();
                RegexSet::empty()
            }
        };
        Ok(ValidatedStart {
            preferences,
            labels,
            duration_secs: self.duration_secs,
        })
    }
}

pub struct ValidatedStart {
    pub(crate) preferences: PollPreferences,
    labels: RegexSet,
    pub(crate) duration_secs: Option<u32>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PollSource {
    #[default]
    Chat,
    Twitch,
    Youtube,
}

impl PollSource {
    fn is_chat(&self) -> bool {
        *self == Self::Chat
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PollStatus {
    #[default]
    Idle,
    Open,
    Closed,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollOption {
    pub label: String,
    pub votes: u32,
    pub percent: f64,
    pub leading: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PollSnapshot {
    pub id: String,
    pub source: PollSource,
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
pub enum PollError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("受付中のアンケートがあります")]
    AlreadyOpen,
    #[error("アンケートの設定を保存できませんでした")]
    Save,
}

pub enum PreferenceChange<'a> {
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

pub struct Poll {
    // 永続化を操作間で直列化する。コメント処理と時計はこのロックを取らない。
    controls: Mutex<()>,
    current: Mutex<CurrentPoll>,
    tx: watch::Sender<PollSnapshot>,
    wake: Notify,
    save_preferences: Box<SavePreferences>,
    emit_snapshot: Box<EmitSnapshot>,
}

impl Poll {
    pub fn new(
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

    pub fn snapshot(&self) -> PollSnapshot {
        self.tx.borrow().clone()
    }

    pub fn start(&self, start: ValidatedStart) -> Result<PollSnapshot, PollError> {
        let _controls = self.controls.lock().unwrap();
        self.check_can_start()?;
        self.save_start_preferences(&start)?;
        Ok(self.open(start))
    }

    pub(crate) fn check_can_start(&self) -> Result<(), PollError> {
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
        Ok(())
    }

    // 呼び出し側で開始・終了・リセットと直列化する。
    pub(crate) fn save_start_preferences(&self, start: &ValidatedStart) -> Result<(), PollError> {
        (self.save_preferences)(PreferenceChange::Started(&start.preferences))
    }

    pub(crate) fn open(&self, start: ValidatedStart) -> PollSnapshot {
        let mut current = self.current.lock().unwrap();
        let started_at_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;
        let now = Instant::now();
        let PollPreferences {
            source,
            question,
            options,
            settings,
        } = start.preferences;
        *current = CurrentPoll {
            snapshot: PollSnapshot {
                id: Uuid::new_v4().to_string(),
                source,
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
            deadline: match source {
                PollSource::Chat => start
                    .duration_secs
                    .map(|secs| now + Duration::from_secs(secs.into())),
                PollSource::Twitch | PollSource::Youtube => None,
            },
            ..CurrentPoll::default()
        };
        self.publish(&mut current);
        self.wake.notify_one();
        current.snapshot.clone()
    }

    pub fn apply_external(&self, poll_id: &str, votes: &[u32], closed: bool) -> bool {
        let mut current = self.current.lock().unwrap();
        if current.snapshot.id != poll_id || current.snapshot.source == PollSource::Chat {
            return false;
        }
        for (index, option) in current.snapshot.options.iter_mut().enumerate() {
            option.votes = votes.get(index).copied().unwrap_or_default();
        }
        current.snapshot.total_voters = current
            .snapshot
            .options
            .iter()
            .fold(0u32, |total, option| total.saturating_add(option.votes));
        if closed {
            self.close(&mut current);
        } else {
            self.publish(&mut current);
        }
        true
    }

    /// 配信サービス側の開始・終了予定時刻に合わせる。変わった時だけ配信する。
    pub fn set_remote_timing(&self, poll_id: &str, started_at_ms: u64, ends_at_ms: u64) -> bool {
        let mut current = self.current.lock().unwrap();
        let snapshot = &mut current.snapshot;
        if snapshot.id != poll_id
            || snapshot.source == PollSource::Chat
            || snapshot.status != PollStatus::Open
            || (snapshot.started_at_ms, snapshot.ends_at_ms) == (started_at_ms, Some(ends_at_ms))
        {
            return false;
        }
        snapshot.started_at_ms = started_at_ms;
        snapshot.ends_at_ms = Some(ends_at_ms);
        self.publish(&mut current);
        true
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

    pub fn record(&self, msg: &ChatMessage, verdict: &Verdict) -> bool {
        if !eligible(msg, verdict) {
            return false;
        }
        let mut current = self.current.lock().unwrap();
        let now = Instant::now();
        if current.snapshot.source != PollSource::Chat
            || current.snapshot.status != PollStatus::Open
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
