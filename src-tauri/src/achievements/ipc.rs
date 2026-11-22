//! 実績 UI 用の IPC データ整形。集計・検証・組み合わせ判定はここ(Rust)で行い、UI は描画と入力だけにする。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use uuid::Uuid;

use crate::model::Platform;

use super::currency::RateTable;
use super::engine::{AchievementEngine, AchievementUnlocked};
use super::model::{
    duplicate, reset_to_preset, Achievement, Metric, SubjectKind, Tier, ValidationError, Window,
};
use super::store::{AchievementDefs, CommitError, DefsStore};

/// 金額(DonationAmount)を持つプラットフォーム。X・ツイキャスの金額は取れない。
const AMOUNT_PLATFORMS: [Platform; 3] = [Platform::Youtube, Platform::Twitch, Platform::Niconico];
const ALL_PLATFORMS: [Platform; 5] = [
    Platform::Twitch,
    Platform::Youtube,
    Platform::X,
    Platform::Niconico,
    Platform::Twitcasting,
];

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementRow {
    #[serde(flatten)]
    pub achievement: Achievement,
    /// 視聴者向け: 解除した視聴者の人数。
    pub viewer_unlock_count: u64,
    /// 配信者向け: 解除回数と最新の解除日時。
    pub streamer_unlock_count: u64,
    pub streamer_last_unlocked_at_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementList {
    pub revision: u64,
    pub enabled: bool,
    pub rates: RateTable,
    pub rates_updated_at_ms: Option<u64>,
    pub symbol_overrides: BTreeMap<String, String>,
    pub achievements: Vec<AchievementRow>,
}

pub fn list(defs: AchievementDefs, engine: Option<&AchievementEngine>) -> AchievementList {
    let viewer_counts = engine.map(AchievementEngine::viewer_unlock_counts).unwrap_or_default();
    let streamer = engine.map(AchievementEngine::streamer_unlocks).unwrap_or_default();
    let achievements = defs
        .achievements
        .into_iter()
        .map(|achievement| {
            let own = streamer.iter().filter(|r| r.achievement_id == achievement.id);
            AchievementRow {
                viewer_unlock_count: viewer_counts.get(&achievement.id).copied().unwrap_or(0),
                streamer_unlock_count: own.clone().count() as u64,
                streamer_last_unlocked_at_ms: own.map(|r| r.unlocked_at_ms).max(),
                achievement,
            }
        })
        .collect();
    AchievementList {
        revision: defs.revision,
        enabled: defs.enabled,
        rates: defs.rates,
        rates_updated_at_ms: defs.rates_updated_at_ms,
        symbol_overrides: defs.symbol_overrides,
        achievements,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum CurrencyKind {
    None,
    JpyConverted,
    Only,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MetricRule {
    pub metric: Metric,
    pub subjects: Vec<SubjectKind>,
    pub windows: Vec<Window>,
    pub currency_modes: Vec<CurrencyKind>,
    pub platforms: Vec<Platform>,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementSchema {
    pub metrics: Vec<MetricRule>,
}

const ALL_METRICS: [Metric; 9] = [
    Metric::Comments,
    Metric::SuperChatCount,
    Metric::DonationAmount,
    Metric::GiftCount,
    Metric::MembershipEvents,
    Metric::StreamsAttended,
    Metric::UniqueCommenters,
    Metric::PeakConcurrentViewers,
    Metric::Streams,
];

/// メトリクスごとに選べる対象・期間・通貨モード・プラットフォーム。`validate_all` と一致する(性質テストで固定)。
pub fn schema() -> AchievementSchema {
    use Metric::*;
    let metrics = ALL_METRICS
        .into_iter()
        .map(|metric| MetricRule {
            metric,
            subjects: match metric {
                StreamsAttended => vec![SubjectKind::Viewer],
                UniqueCommenters | PeakConcurrentViewers | Streams => vec![SubjectKind::Streamer],
                _ => vec![SubjectKind::Viewer, SubjectKind::Streamer],
            },
            windows: match metric {
                UniqueCommenters | PeakConcurrentViewers => vec![Window::PerStream],
                _ => vec![Window::Lifetime, Window::PerStream],
            },
            currency_modes: match metric {
                DonationAmount => vec![CurrencyKind::JpyConverted, CurrencyKind::Only],
                _ => vec![CurrencyKind::None],
            },
            platforms: match metric {
                DonationAmount => AMOUNT_PLATFORMS.to_vec(),
                _ => ALL_PLATFORMS.to_vec(),
            },
        })
        .collect();
    AchievementSchema { metrics }
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "result", rename_all = "camelCase")]
pub enum CommitReply {
    Ok { revision: u64 },
    Stale,
    Invalid { id: String, field: &'static str },
    Io,
    SaveRefused,
}

fn field_of(reason: ValidationError) -> &'static str {
    match reason {
        ValidationError::EmptyName => "name",
        ValidationError::DuplicateId => "id",
        ValidationError::AmountMetricNeedsCurrency | ValidationError::CountMetricRejectsCurrency => "currency",
        ValidationError::MetricSubjectMismatch => "metric",
        ValidationError::WindowNotAllowed => "window",
        ValidationError::BadThreshold => "threshold",
        ValidationError::PlatinumHasCondition | ValidationError::MissingCondition => "condition",
        ValidationError::MultiplePlatinum => "tier",
    }
}

/// 保存し、成功したらエンジンへ反映する(無効→有効ならエンジンを開き、有効→無効なら閉じる)。
/// 返した解除は呼び出し側が配信する。
pub fn commit(
    store: &DefsStore,
    engine_slot: &Mutex<Option<Arc<AchievementEngine>>>,
    dir: &Path,
    base_revision: u64,
    mut next: AchievementDefs,
    now_ms: u64,
) -> (CommitReply, Vec<AchievementUnlocked>) {
    let mut slot = engine_slot.lock().unwrap_or_else(|e| e.into_inner());
    let current = store.current();
    if next.rates != current.rates || next.symbol_overrides != current.symbol_overrides {
        next.rates_updated_at_ms = Some(now_ms);
    } else {
        next.rates_updated_at_ms = current.rates_updated_at_ms;
    }
    let revision = match store.commit(base_revision, next) {
        Ok(revision) => revision,
        Err(CommitError::Stale) => return (CommitReply::Stale, Vec::new()),
        Err(CommitError::Invalid(id, reason)) => {
            return (CommitReply::Invalid { id, field: field_of(reason) }, Vec::new());
        }
        Err(CommitError::Io) => return (CommitReply::Io, Vec::new()),
        Err(CommitError::SaveRefused) => return (CommitReply::SaveRefused, Vec::new()),
    };
    let defs = store.current();
    let unlocks = match (defs.enabled, slot.as_ref()) {
        (true, Some(engine)) => engine.set_definitions(defs, now_ms),
        (true, None) => {
            *slot = Some(Arc::new(AchievementEngine::open(dir, defs, now_ms)));
            Vec::new()
        }
        (false, Some(engine)) => {
            if engine.flush().is_err() {
                tracing::error!("実績の進捗を保存できませんでした");
            }
            *slot = None;
            Vec::new()
        }
        (false, None) => Vec::new(),
    };
    (CommitReply::Ok { revision }, unlocks)
}

/// 複製(未保存の新しい定義を返す。id は Rust が振る)。
pub fn duplicate_by_id(defs: &AchievementDefs, id: &str) -> Option<Achievement> {
    let src = defs.achievements.iter().find(|a| a.id == id)?;
    Some(duplicate(src, &Uuid::new_v4().to_string()))
}

/// プリセットに戻した定義(未保存)。ビルトインでなければ None。
pub fn reset_by_id(defs: &AchievementDefs, id: &str) -> Option<Achievement> {
    reset_to_preset(defs.achievements.iter().find(|a| a.id == id)?)
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerTrophy {
    pub achievement_id: String,
    pub name: String,
    pub icon: String,
    pub tier: Tier,
    pub window_key: String,
    pub unlocked_at_ms: u64,
}

/// 視聴者が獲得したトロフィー(新しい順)。定義が消えた解除は出さない。
pub fn listener_trophies(
    defs: &AchievementDefs,
    engine: Option<&AchievementEngine>,
    platform: &str,
    author_id: &str,
) -> Vec<ListenerTrophy> {
    let mut trophies: Vec<ListenerTrophy> = engine
        .map(|e| e.listener_unlocks(platform, author_id))
        .unwrap_or_default()
        .into_iter()
        .filter_map(|record| {
            let a = defs.achievements.iter().find(|a| a.id == record.achievement_id)?;
            Some(ListenerTrophy {
                achievement_id: record.achievement_id,
                name: a.name.clone(),
                icon: a.icon.clone(),
                tier: a.tier,
                window_key: record.window_key,
                unlocked_at_ms: record.unlocked_at_ms,
            })
        })
        .collect();
    trophies.sort_by(|a, b| b.unlocked_at_ms.cmp(&a.unlocked_at_ms));
    trophies
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::achievements::model::{Announce, Condition, CurrencyMode, validate_all};

    fn probe(metric: Metric, subject: SubjectKind, window: Window, currency: CurrencyMode) -> Achievement {
        Achievement {
            id: "probe".into(),
            name: "probe".into(),
            description: String::new(),
            icon: String::new(),
            tier: Tier::Bronze,
            subject,
            enabled: true,
            builtin_id: None,
            edited: false,
            condition: Some(Condition { window, metric, threshold: 1.0, currency, platforms: vec![] }),
            announce: Announce::default(),
        }
    }

    #[test]
    fn schema_matches_validation_for_all_combinations() {
        let schema = schema();
        for rule in &schema.metrics {
            for subject in [SubjectKind::Viewer, SubjectKind::Streamer] {
                for window in [Window::Lifetime, Window::PerStream] {
                    for (kind, mode) in [
                        (CurrencyKind::None, CurrencyMode::None),
                        (CurrencyKind::JpyConverted, CurrencyMode::JpyConverted),
                        (CurrencyKind::Only, CurrencyMode::Only("USD".into())),
                    ] {
                        let allowed = rule.subjects.contains(&subject)
                            && rule.windows.contains(&window)
                            && rule.currency_modes.contains(&kind);
                        let result = validate_all(&[probe(rule.metric, subject, window, mode)]);
                        assert_eq!(
                            allowed,
                            result.is_ok(),
                            "{:?} {:?} {:?} {:?}",
                            rule.metric, subject, window, kind
                        );
                    }
                }
            }
        }
        assert_eq!(schema.metrics.len(), ALL_METRICS.len());
    }

    fn setup(enabled: bool) -> (tempfile::TempDir, DefsStore, Mutex<Option<Arc<AchievementEngine>>>) {
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let engine = enabled.then(|| {
            let mut defs = store.current();
            defs.enabled = true;
            Arc::new(AchievementEngine::open(dir.path(), defs, 0))
        });
        (dir, store, Mutex::new(engine))
    }

    #[test]
    fn commit_ipc_stale_returns_stale_without_write() {
        let (dir, store, slot) = setup(false);
        let before = std::fs::read(dir.path().join(crate::achievements::store::DEFS_FILE)).ok();
        let mut next = store.current();
        next.achievements[0].name = "x".into();
        let (reply, unlocks) = commit(&store, &slot, dir.path(), 7, next, 10);
        assert_eq!(reply, CommitReply::Stale);
        assert!(unlocks.is_empty());
        assert_eq!(store.current().revision, 0);
        assert_eq!(std::fs::read(dir.path().join(crate::achievements::store::DEFS_FILE)).ok(), before);
    }

    #[test]
    fn commit_ipc_invalid_returns_field_error_without_write() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        let id = next.achievements[0].id.clone();
        next.achievements[0].name = "  ".into();
        let (reply, _) = commit(&store, &slot, dir.path(), 0, next, 10);
        assert_eq!(reply, CommitReply::Invalid { id, field: "name" });
        assert_eq!(store.current().revision, 0);
        assert!(!dir.path().join(crate::achievements::store::DEFS_FILE).exists());
    }

    #[test]
    fn commit_ipc_applies_definitions_to_engine() {
        let (dir, store, slot) = setup(true);
        let mut next = store.current();
        next.enabled = true;
        next.achievements[0].name = "renamed".into();
        let (reply, _) = commit(&store, &slot, dir.path(), 0, next, 10);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        let engine = slot.lock().unwrap().clone().unwrap();
        assert_eq!(engine.definitions().achievements[0].name, "renamed");
        assert_eq!(engine.definitions().revision, 1);
    }

    #[test]
    fn commit_ipc_toggles_engine_with_enabled_and_stamps_rates() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.enabled = true;
        next.rates.insert("USD".into(), 150.0);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, next, 99);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        assert!(slot.lock().unwrap().is_some());
        assert_eq!(store.current().rates_updated_at_ms, Some(99));

        let mut next = store.current();
        next.enabled = false;
        let (reply, _) = commit(&store, &slot, dir.path(), 1, next, 200);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
        assert!(slot.lock().unwrap().is_none());
        assert_eq!(store.current().rates_updated_at_ms, Some(99));
    }

    #[test]
    fn list_without_engine_has_zero_counts_and_duplicate_reset_use_rust() {
        let defs = AchievementDefs::default();
        let listed = list(defs.clone(), None);
        assert_eq!(listed.achievements.len(), defs.achievements.len());
        assert!(listed.achievements.iter().all(|r| r.viewer_unlock_count == 0 && r.streamer_last_unlocked_at_ms.is_none()));
        let id = &defs.achievements[0].id;
        let copy = duplicate_by_id(&defs, id).unwrap();
        assert_ne!(&copy.id, id);
        assert!(copy.builtin_id.is_none());
        assert!(duplicate_by_id(&defs, "nope").is_none());
        assert!(reset_by_id(&defs, id).is_some());
        assert!(reset_by_id(&defs, "nope").is_none());
    }
}
