//! 実績 UI 用の IPC データ整形。集計・検証・組み合わせ判定はここ(Rust)で行い、UI は描画と入力だけにする。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::Platform;

use super::currency::{is_iso_code, normalize_currency, RateTable};
use super::engine::{AchievementEngine, AchievementUnlocked, UnlockRecord};
use super::model::{
    allowed_platforms, duplicate, reset_to_preset, Achievement, Announce, Condition, CurrencyMode,
    Metric, SubjectKind, Tier, ValidationError, Window,
};
use super::store::{read_progress, AchievementDefs, CommitError, DefsStore};

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
    /// 読み込んだファイル内容のトークン。保存時に返してもらい、外部編集を Stale にする。
    pub content_token: String,
    pub enabled: bool,
    pub rates: RateTable,
    pub rates_updated_at_ms: Option<u64>,
    pub symbol_overrides: BTreeMap<String, String>,
    pub achievements: Vec<AchievementRow>,
}

/// 解除の集計元。エンジンがあればそれ、無ければ進捗ファイルを読み取り専用で読む(書かない)。
fn unlock_source(
    engine: Option<&AchievementEngine>,
    dir: &Path,
) -> (BTreeMap<String, u64>, Vec<UnlockRecord>) {
    match engine {
        Some(e) => (e.viewer_unlock_counts(), e.streamer_unlocks()),
        None => read_progress(dir)
            .map(|p| (p.viewer_unlock_counts(), p.streamer_records()))
            .unwrap_or_default(),
    }
}

pub fn list(
    (defs, content_token): (AchievementDefs, String),
    engine: Option<&AchievementEngine>,
    dir: &Path,
) -> AchievementList {
    let (viewer_counts, streamer) = unlock_source(engine, dir);
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
        content_token,
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

/// メトリクスごとに選べる対象・期間・通貨モード・プラットフォーム。保存時の検証と一致する(性質テストで固定)。
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
            platforms: allowed_platforms(metric).to_vec(),
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
        ValidationError::PlatformNotAllowed => "platforms",
    }
}

/// UI からの入力。しきい値が空欄(null)でも受け取り、対象 id とフィールド付きのエラーにする。
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConditionInput {
    pub window: Window,
    pub metric: Metric,
    pub threshold: Option<f64>,
    pub currency: CurrencyMode,
    pub platforms: Vec<Platform>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementInput {
    pub id: String,
    pub name: String,
    pub description: String,
    pub icon: String,
    pub tier: Tier,
    pub subject: SubjectKind,
    pub enabled: bool,
    pub builtin_id: Option<String>,
    pub edited: bool,
    pub condition: Option<ConditionInput>,
    pub announce: Announce,
}

impl AchievementInput {
    fn into_achievement(self) -> Result<Achievement, CommitReply> {
        let condition = match self.condition {
            None => None,
            Some(c) => Some(Condition {
                window: c.window,
                metric: c.metric,
                threshold: c.threshold.ok_or_else(|| CommitReply::Invalid {
                    id: self.id.clone(),
                    field: "threshold",
                })?,
                currency: c.currency,
                platforms: c.platforms,
            }),
        };
        Ok(Achievement {
            id: self.id,
            name: self.name,
            description: self.description,
            icon: self.icon,
            tier: self.tier,
            subject: self.subject,
            enabled: self.enabled,
            builtin_id: self.builtin_id,
            edited: self.edited,
            condition,
            announce: self.announce,
        })
    }
}

pub fn build_achievements(inputs: Vec<AchievementInput>) -> Result<Vec<Achievement>, CommitReply> {
    inputs.into_iter().map(AchievementInput::into_achievement).collect()
}

/// レート表のキーを通貨コードに正規化(`$`→`USD`、`usd`→`USD`)し、空・重複・0 以下・非有限を弾く。
/// エラーの id は空にする(入力値を返すと、誤って貼った URL やトークンが画面に出る)。
/// エンジンは正規化後のコードで引くので、保存するキーもそれに揃える。
fn normalize_rates(
    rates: RateTable,
    overrides: &BTreeMap<String, String>,
) -> Result<RateTable, CommitReply> {
    let mut out = RateTable::new();
    for (raw, rate) in rates {
        let code = normalize_currency(&raw, overrides);
        if code.is_empty() {
            return Err(CommitReply::Invalid { id: String::new(), field: "rateCode" });
        }
        if !rate.is_finite() || rate <= 0.0 {
            return Err(CommitReply::Invalid { id: String::new(), field: "rateValue" });
        }
        if out.insert(code, rate).is_some() {
            return Err(CommitReply::Invalid { id: String::new(), field: "rateCode" });
        }
    }
    Ok(out)
}

const SYMBOL_MAX_CHARS: usize = 16;
const SYMBOL_MAX_ENTRIES: usize = 64;

/// 通貨記号の上書き(受信記号 → ISO コード)を正規化する。記号・コードとも trim して ASCII 大文字化する
/// (`normalize_currency` は大文字化した形で引く)。空の記号・コード、英大文字 3 字でないコード、正規化後の記号の重複、
/// 長すぎる記号、多すぎる件数は弾く。ISO 形式の記号は、`saved` に同じ組で保存済みの物だけ通す(実行時は使われない。
/// 以前の版や手編集で残った物が、関係ない保存を止めないようにする)。
/// エラーの id は空にする(記号は利用者が貼った文字列で、画面に返さない)。
fn normalize_symbol_overrides(
    raw: BTreeMap<String, String>,
    saved: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, CommitReply> {
    let invalid = || CommitReply::Invalid { id: String::new(), field: "symbolOverrides" };
    if raw.len() > SYMBOL_MAX_ENTRIES {
        return Err(invalid());
    }
    let mut out = BTreeMap::new();
    for (symbol, code) in raw {
        let symbol = symbol.trim().to_ascii_uppercase();
        let code = code.trim().to_ascii_uppercase();
        // ISO 形式の記号の新規追加・変更は不可(ISO コードは上書きされないので、保存しても効かない)。
        let kept = || {
            saved.iter().any(|(s, c)| {
                s.trim().eq_ignore_ascii_case(&symbol) && c.trim().eq_ignore_ascii_case(&code)
            })
        };
        if symbol.is_empty()
            || symbol.chars().count() > SYMBOL_MAX_CHARS
            || (is_iso_code(&symbol) && !kept())
            || !is_iso_code(&code)
        {
            return Err(invalid());
        }
        if out.insert(symbol, code).is_some() {
            return Err(invalid());
        }
    }
    Ok(out)
}

/// 新規の実績(未保存)。id は Rust が振る。条件は schema の先頭の指標・期間・通貨モードで決定的に作る。
pub fn new_achievement(subject: SubjectKind) -> Achievement {
    let rule = schema()
        .metrics
        .into_iter()
        .find(|rule| rule.subjects.contains(&subject))
        .expect("every subject has at least one metric");
    let currency = match rule.currency_modes.first() {
        Some(CurrencyKind::JpyConverted) => CurrencyMode::JpyConverted,
        _ => CurrencyMode::None,
    };
    Achievement {
        id: Uuid::new_v4().to_string(),
        name: "新しい実績".to_owned(),
        description: String::new(),
        icon: "🏆".to_owned(),
        tier: Tier::Bronze,
        subject,
        enabled: true,
        builtin_id: None,
        edited: false,
        condition: Some(Condition {
            window: rule.windows[0],
            metric: rule.metric,
            threshold: 1.0,
            currency,
            platforms: Vec::new(),
        }),
        announce: Announce { tts: false },
    }
}

/// 定義の状態に合わせてエンジンを開閉・再評価する。無効→有効では既存の進捗も再評価する。
/// 無効化は呼ぶ前に `close_engine` を済ませておく(ここでは破棄だけ)。
fn apply_to_engine(
    slot: &mut Option<Arc<AchievementEngine>>,
    dir: &Path,
    defs: AchievementDefs,
    now_ms: u64,
) -> Vec<AchievementUnlocked> {
    match (defs.enabled, slot.as_ref()) {
        (true, Some(engine)) => engine.set_definitions(defs, now_ms),
        (true, None) => {
            let engine = Arc::new(AchievementEngine::open(dir, defs.clone(), now_ms));
            let unlocks = engine.set_definitions(defs, now_ms);
            *slot = Some(engine);
            unlocks
        }
        (false, _) => {
            *slot = None;
            Vec::new()
        }
    }
}

/// 無効化の前処理(画面から・外部からの両方)。進捗を flush し、保存できた再試行待ちの解除を返す。
/// flush に失敗したらエンジンを保持して Io を返す(何も変えない)。
fn close_engine(
    slot: &Option<Arc<AchievementEngine>>,
) -> Result<Vec<AchievementUnlocked>, CommitError> {
    match slot {
        Some(engine) => engine.close().map_err(|_| {
            tracing::error!("実績の進捗を保存できませんでした");
            CommitError::Io
        }),
        None => Ok(Vec::new()),
    }
}

/// ディスクを読み直し、変わっていればエンジンへ反映する。外部で無効化されていたら先に
/// `close_engine` し、失敗したら取り込まずに Io を返す(次の再読込でやり直す)。
fn reload_and_apply(
    store: &DefsStore,
    slot: &mut Option<Arc<AchievementEngine>>,
    dir: &Path,
    now_ms: u64,
) -> Result<Vec<AchievementUnlocked>, CommitError> {
    let mut closed = Vec::new();
    let changed = store.reload_with(now_ms, |defs| {
        if !defs.enabled {
            closed = close_engine(slot)?;
        }
        Ok(())
    })?;
    if changed {
        closed.extend(apply_to_engine(slot, dir, store.current(), now_ms));
    }
    Ok(closed)
}

/// ディスクを読み直し、変わっていればエンジンへ反映する(一覧の表示・Stale のあとに呼ぶ)。
pub fn refresh(
    store: &DefsStore,
    engine_slot: &Mutex<Option<Arc<AchievementEngine>>>,
    dir: &Path,
    now_ms: u64,
) -> Result<Vec<AchievementUnlocked>, CommitError> {
    let mut slot = engine_slot.lock().unwrap_or_else(|e| e.into_inner());
    reload_and_apply(store, &mut slot, dir, now_ms)
}

/// 保存し、成功したらエンジンへ反映する。返した解除は呼び出し側が配信する。
/// 競合(Stale)を最初に、次に検証を判定し、通らなければ何も書かない(エンジンも触らない)。`base_token` は一覧の
/// `content_token`。無効化は次に進捗を flush し、失敗したら何も変えずに Io を返す。
pub fn commit(
    store: &DefsStore,
    engine_slot: &Mutex<Option<Arc<AchievementEngine>>>,
    dir: &Path,
    base_revision: u64,
    base_token: Option<&str>,
    mut next: AchievementDefs,
    now_ms: u64,
) -> (CommitReply, Vec<AchievementUnlocked>) {
    let mut slot = engine_slot.lock().unwrap_or_else(|e| e.into_inner());
    // 競合は入力の正規化・検証より先に返す(画面は Stale で再読込に入る)。
    if let Err(error) = store.check_base(base_revision, base_token) {
        return (reply_of(error), Vec::new());
    }
    let current = store.current();
    next.symbol_overrides = match normalize_symbol_overrides(next.symbol_overrides, &current.symbol_overrides) {
        Ok(overrides) => overrides,
        Err(reply) => return (reply, Vec::new()),
    };
    next.rates = match normalize_rates(next.rates, &next.symbol_overrides) {
        Ok(rates) => rates,
        Err(reply) => return (reply, Vec::new()),
    };
    next.rates_updated_at_ms =
        if next.rates != current.rates || next.symbol_overrides != current.symbol_overrides {
            Some(now_ms)
        } else {
            current.rates_updated_at_ms
        };
    let mut unlocks = Vec::new();
    let mut result = store.check(base_revision, base_token, &next);
    // 無効化の保存(close_engine の flush)の最中に定義ファイルが外部で編集されると、下の commit_at が
    // Stale を返しても進捗ファイルは書かれている。書かれるのは正常な進捗保存で値は正しいので、挙動は変えない。
    if result.is_ok() && !next.enabled {
        match close_engine(&slot) {
            Ok(closed) => unlocks = closed,
            Err(_) => return (CommitReply::Io, Vec::new()),
        }
    }
    if result.is_ok() {
        result = store.commit_at(base_revision, base_token, next).map(|_| ());
    }
    // Stale は何もせずに返す。取り込みは画面の再読込(list_achievements → refresh)に任せる。
    if let Err(error) = result {
        return (reply_of(error), unlocks);
    }
    let reply = CommitReply::Ok { revision: store.current().revision };
    unlocks.extend(apply_to_engine(&mut slot, dir, store.current(), now_ms));
    (reply, unlocks)
}

fn reply_of(error: CommitError) -> CommitReply {
    match error {
        CommitError::Stale => CommitReply::Stale,
        CommitError::Invalid(id, reason) => CommitReply::Invalid { id, field: field_of(reason) },
        CommitError::Io => CommitReply::Io,
        CommitError::SaveRefused => CommitReply::SaveRefused,
    }
}

/// 画面から届いた保存内容(Tauri コマンドの引数)。
pub struct AchievementDraft {
    pub enabled: bool,
    pub achievements: Vec<AchievementInput>,
    pub rates: RateTable,
    pub symbol_overrides: BTreeMap<String, String>,
}

/// 画面からの保存。競合判定を入力の変換(しきい値の空欄など)より先に行い、定義に組み立てて `commit` する。
pub fn commit_draft(
    store: &DefsStore,
    engine_slot: &Mutex<Option<Arc<AchievementEngine>>>,
    dir: &Path,
    base_revision: u64,
    base_token: Option<&str>,
    draft: AchievementDraft,
    now_ms: u64,
) -> (CommitReply, Vec<AchievementUnlocked>) {
    if let Err(error) = store.check_base(base_revision, base_token) {
        return (reply_of(error), Vec::new());
    }
    let achievements = match build_achievements(draft.achievements) {
        Ok(list) => list,
        Err(reply) => return (reply, Vec::new()),
    };
    let mut next = store.current();
    next.enabled = draft.enabled;
    next.achievements = achievements;
    next.rates = draft.rates;
    next.symbol_overrides = draft.symbol_overrides;
    commit(store, engine_slot, dir, base_revision, base_token, next, now_ms)
}

/// 複製。画面上のドラフトから作る(未保存の編集を含む)。id は Rust が振る。プラチナは複製不可。
pub fn duplicate_draft(input: AchievementInput) -> Option<Achievement> {
    let src = input.into_achievement().ok()?;
    (src.tier != Tier::Platinum).then(|| duplicate(&src, &Uuid::new_v4().to_string()))
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

/// 視聴者が獲得したトロフィー(新しい順)。定義が消えた解除は出さない。エンジンが無ければ進捗ファイルから読む。
pub fn listener_trophies(
    defs: &AchievementDefs,
    engine: Option<&AchievementEngine>,
    dir: &Path,
    platform: &str,
    author_id: &str,
) -> Vec<ListenerTrophy> {
    let records = match engine {
        Some(e) => e.listener_unlocks(platform, author_id),
        None => read_progress(dir)
            .map(|p| p.listener_records(platform, author_id))
            .unwrap_or_default(),
    };
    let mut trophies: Vec<ListenerTrophy> = records
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
    use crate::achievements::model::{Announce, Condition, CurrencyMode, validate_all, validate_for_save};

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
        let (reply, unlocks) = commit(&store, &slot, dir.path(), 7, None, next, 10);
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
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 10);
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
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 10);
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
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 99);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        assert!(slot.lock().unwrap().is_some());
        assert_eq!(store.current().rates_updated_at_ms, Some(99));

        let mut next = store.current();
        next.enabled = false;
        let (reply, _) = commit(&store, &slot, dir.path(), 1, None, next, 200);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
        assert!(slot.lock().unwrap().is_none());
        assert_eq!(store.current().rates_updated_at_ms, Some(99));
    }

    const ALL_PLATFORMS: [Platform; 5] = [
        Platform::Twitch, Platform::Youtube, Platform::X, Platform::Niconico, Platform::Twitcasting,
    ];

    fn input_of(a: &Achievement) -> AchievementInput {
        serde_json::from_value(serde_json::to_value(a).unwrap()).unwrap()
    }

    #[test]
    fn list_without_engine_or_progress_has_zero_counts_and_reset_uses_rust() {
        let dir = tempfile::tempdir().unwrap();
        let defs = AchievementDefs::default();
        let listed = list((defs.clone(), String::new()), None, dir.path());
        assert_eq!(listed.achievements.len(), defs.achievements.len());
        assert!(listed.achievements.iter().all(|r| r.viewer_unlock_count == 0 && r.streamer_last_unlocked_at_ms.is_none()));
        let id = &defs.achievements[0].id;
        assert!(reset_by_id(&defs, id).is_some());
        assert!(reset_by_id(&defs, "nope").is_none());
    }

    #[test]
    fn duplicate_uses_the_current_draft_and_refuses_platinum() {
        let defs = AchievementDefs::default();
        let mut draft = defs.achievements[0].clone();
        draft.name = "初参加".into();
        let copy = duplicate_draft(input_of(&draft)).unwrap();
        assert_eq!(copy.name, "初参加");
        assert_ne!(copy.id, draft.id);
        assert!(copy.builtin_id.is_none());
        let platinum = defs.achievements.iter().find(|a| a.tier == Tier::Platinum).unwrap();
        assert!(duplicate_draft(input_of(platinum)).is_none());
    }

    #[test]
    fn empty_threshold_input_is_a_field_error_for_that_id() {
        let defs = AchievementDefs::default();
        let mut value = serde_json::to_value(&defs.achievements[0]).unwrap();
        value["condition"]["threshold"] = serde_json::Value::Null;
        let id = defs.achievements[0].id.clone();
        let input: AchievementInput = serde_json::from_value(value).unwrap();
        assert_eq!(
            build_achievements(vec![input]).unwrap_err(),
            CommitReply::Invalid { id, field: "threshold" }
        );
    }

    #[test]
    fn stale_commit_changes_nothing_and_the_list_reload_lets_the_next_edit_succeed() {
        let (dir, store, slot) = setup(false);
        let other = DefsStore::load(dir.path(), 0);
        let mut external = other.current();
        external.achievements[0].name = "external".into();
        assert_eq!(other.commit(0, external), Ok(1));

        let mut mine = store.current();
        mine.achievements[0].name = "mine".into();
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, mine, 10);
        assert_eq!(reply, CommitReply::Stale);
        assert_eq!(store.current().revision, 0, "Stale の応答では取り込まない");
        // 画面の再読込(list_achievements → refresh)で取り込む。
        refresh(&store, &slot, dir.path(), 10).unwrap();
        assert_eq!(store.current().revision, 1);
        assert_eq!(store.current().achievements[0].name, "external");

        let mut again = store.current();
        again.achievements[0].name = "mine".into();
        let (reply, _) = commit(&store, &slot, dir.path(), 1, None, again, 11);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
    }

    #[test]
    fn refresh_picks_up_external_updates_and_ignores_unchanged_disk() {
        let (dir, store, slot) = setup(false);
        assert!(refresh(&store, &slot, dir.path(), 1).unwrap().is_empty());
        let other = DefsStore::load(dir.path(), 0);
        let mut external = other.current();
        external.enabled = true;
        assert_eq!(other.commit(0, external), Ok(1));
        refresh(&store, &slot, dir.path(), 2).unwrap();
        assert_eq!(store.current().revision, 1);
        assert!(slot.lock().unwrap().is_some());
    }

    #[test]
    fn disabled_engine_still_lists_unlocks_from_the_progress_file_read_only() {
        let dir = tempfile::tempdir().unwrap();
        let progress = r#"{"version":1,"enabledAtMs":1000,"sessions":{},"seenStreams":[],"streamer":{},"viewers":{},"unlocks":{"viewer-first-comment|youtube:UC1|*":1500,"viewer-first-comment|twitch:u2|*":1600,"streamer-first-stream|streamer|*":1700}}"#;
        let path = dir.path().join(crate::achievements::store::PROGRESS_FILE);
        std::fs::write(&path, progress).unwrap();
        let defs = AchievementDefs::default();
        let listed = list((defs.clone(), String::new()), None, dir.path());
        let first = listed.achievements.iter().find(|r| r.achievement.id == "viewer-first-comment").unwrap();
        assert_eq!(first.viewer_unlock_count, 2);
        let trophies = listener_trophies(&defs, None, dir.path(), "youtube", "UC1");
        assert_eq!(trophies.len(), 1);
        assert_eq!(trophies[0].unlocked_at_ms, 1500);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), progress);

        // 壊れた進捗は空として返し、退避ファイルも作らない。
        std::fs::write(&path, "{broken").unwrap();
        assert!(listener_trophies(&defs, None, dir.path(), "youtube", "UC1").is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    const PROGRESS_50: &str = r#"{"version":1,"enabledAtMs":1000,"sessions":{},"seenStreams":[],"streamer":{},"viewers":{"youtube:UC1":{"comments":50}},"unlocks":{}}"#;

    struct DenyWrites;
    impl crate::achievements::store::StoreIo for DenyWrites {
        fn write_atomic(&self, _: &Path, _: &[u8]) -> std::io::Result<()> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
        fn create_backup(&self, _: &Path, _: &Path) -> std::io::Result<()> {
            Err(std::io::ErrorKind::PermissionDenied.into())
        }
    }

    #[test]
    fn disable_with_failed_flush_changes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut defs = store.current();
        defs.enabled = true;
        let engine = AchievementEngine::open_with(dir.path(), defs, 0, Arc::new(DenyWrites));
        let slot = Mutex::new(Some(Arc::new(engine)));
        let mut next = store.current();
        next.enabled = false;
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 10);
        assert_eq!(reply, CommitReply::Io);
        assert_eq!(store.current().revision, 0);
        assert!(!store.current().enabled || store.current() == AchievementDefs::default());
        assert!(slot.lock().unwrap().is_some());
    }

    #[test]
    fn reenable_reevaluates_existing_progress() {
        let (dir, store, slot) = setup(false);
        std::fs::write(dir.path().join(crate::achievements::store::PROGRESS_FILE), PROGRESS_50).unwrap();
        let mut next = store.current();
        next.enabled = true;
        let target = next.achievements.iter_mut().find(|a| a.id == "viewer-comments-100").unwrap();
        target.condition.as_mut().unwrap().threshold = 40.0;
        let (reply, unlocks) = commit(&store, &slot, dir.path(), 0, None, next, 5000);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        assert!(unlocks.iter().any(|u| u.achievement_id == "viewer-comments-100"));
    }

    #[test]
    fn schema_platforms_match_save_validation() {
        let schema = schema();
        let (dir, store, slot) = setup(false);
        let mut revision = 0;
        for rule in &schema.metrics {
            for platform in ALL_PLATFORMS {
                for subject in rule.subjects.iter().copied() {
                    let mut a = probe(
                        rule.metric, subject, rule.windows[0],
                        match rule.currency_modes[0] {
                            CurrencyKind::None => CurrencyMode::None,
                            CurrencyKind::JpyConverted => CurrencyMode::JpyConverted,
                            CurrencyKind::Only => CurrencyMode::Only("USD".into()),
                        },
                    );
                    a.condition.as_mut().unwrap().platforms = vec![platform];
                    let mut next = store.current();
                    next.achievements = vec![a];
                    let (reply, _) = commit(&store, &slot, dir.path(), revision, None, next, 1);
                    let allowed = rule.platforms.contains(&platform);
                    match reply {
                        CommitReply::Ok { revision: r } => { assert!(allowed, "{:?} {:?}", rule.metric, platform); revision = r; }
                        CommitReply::Invalid { field, .. } => { assert!(!allowed, "{:?} {:?}", rule.metric, platform); assert_eq!(field, "platforms"); }
                        other => panic!("{other:?}"),
                    }
                }
            }
        }
        // 読み込み側の validate_all は platforms を見ない(v0.5.0 の保存済みファイルを退避させない)。
        let mut a = probe(Metric::DonationAmount, SubjectKind::Viewer, Window::Lifetime, CurrencyMode::JpyConverted);
        a.condition.as_mut().unwrap().platforms = vec![Platform::X];
        assert!(validate_all(&[a]).is_ok());
    }

    #[test]
    fn new_achievement_is_a_valid_unsaved_user_definition_for_each_subject() {
        for subject in [SubjectKind::Viewer, SubjectKind::Streamer] {
            let a = new_achievement(subject);
            let b = new_achievement(subject);
            assert_ne!(a.id, b.id);
            assert!(Uuid::parse_str(&a.id).is_ok());
            assert_eq!(a.subject, subject);
            assert_eq!(a.builtin_id, None);
            assert!(!a.edited && a.enabled && !a.announce.tts);
            assert_eq!(a.tier, Tier::Bronze);
            assert!(!a.name.trim().is_empty());
            assert!(a.condition.is_some());
            assert!(validate_for_save(&[a]).is_ok(), "{subject:?}");
        }
    }

    #[test]
    fn symbol_overrides_are_trimmed_uppercased_and_saved() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.symbol_overrides = BTreeMap::from([(" R$ ".to_owned(), " brl ".to_owned())]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        assert_eq!(store.current().symbol_overrides, BTreeMap::from([("R$".to_owned(), "BRL".to_owned())]));
    }

    #[test]
    fn symbol_override_errors_are_field_errors_without_write_or_echo() {
        let (dir, store, slot) = setup(false);
        let long = "x".repeat(17);
        let many: Vec<(String, String)> = (0..65).map(|i| (format!("s{i}"), "USD".to_owned())).collect();
        let cases: Vec<Vec<(String, String)>> = vec![
            vec![(" ".into(), "USD".into())],
            vec![("R$".into(), " ".into())],
            vec![("R$".into(), "BR".into())],
            vec![("R$".into(), "B1L".into())],
            vec![("R$".into(), "BRLL".into())],
            vec![("R$".into(), "BRL".into()), (" R$".into(), "USD".into())],
            vec![(long, "USD".into())],
            vec![("BRL".into(), "USD".into())],
            vec![(" brl ".into(), "EUR".into())],
            vec![("💰".repeat(17), "USD".into())],
            many,
        ];
        for case in cases {
            let mut next = store.current();
            next.symbol_overrides = case.iter().cloned().collect();
            let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
            assert_eq!(reply, CommitReply::Invalid { id: String::new(), field: "symbolOverrides" }, "{case:?}");
        }
        assert_eq!(store.current().revision, 0);
    }

    #[test]
    fn saving_the_same_rates_and_overrides_again_keeps_every_key() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.rates = BTreeMap::from([("R$".to_owned(), 30.0)]);
        next.symbol_overrides = BTreeMap::from([("R$".to_owned(), "BRL".to_owned())]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        let saved = store.current();
        assert_eq!(saved.rates.get("BRL"), Some(&30.0));
        let (reply, _) = commit(&store, &slot, dir.path(), 1, None, saved.clone(), 2);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
        assert_eq!(store.current().rates, saved.rates);
        assert_eq!(store.current().symbol_overrides, saved.symbol_overrides);
    }

    /// 手で編集されたファイルや以前の版が残した ISO 形式の上書きを、検証を通さずに保存済みにする。
    fn save_raw_overrides(store: &DefsStore, pairs: &[(&str, &str)]) {
        let mut defs = store.current();
        defs.symbol_overrides = pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
        store.commit_at(store.current().revision, None, defs).unwrap();
    }

    #[test]
    fn saved_iso_override_does_not_block_unrelated_saves() {
        let (dir, store, slot) = setup(false);
        save_raw_overrides(&store, &[("BRL", "USD")]);
        let mut next = store.current();
        next.rates = BTreeMap::from([("brl".to_owned(), 30.0)]);
        let (reply, _) = commit(&store, &slot, dir.path(), 1, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
        assert_eq!(store.current().symbol_overrides, BTreeMap::from([("BRL".to_owned(), "USD".to_owned())]));
        // ISO コードは上書きされないので、レートは BRL のまま。
        assert_eq!(store.current().rates, BTreeMap::from([("BRL".to_owned(), 30.0)]));
    }

    #[test]
    fn new_or_changed_iso_override_is_invalid_even_next_to_a_saved_one() {
        let (dir, store, slot) = setup(false);
        save_raw_overrides(&store, &[("BRL", "USD")]);
        let cases: [&[(&str, &str)]; 3] =
            [&[("BRL", "EUR")], &[("BRL", "USD"), ("EUR", "USD")], &[("BRL", "USD"), (" jpy ", "USD")]];
        for case in cases {
            let mut next = store.current();
            next.symbol_overrides = case.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            let (reply, _) = commit(&store, &slot, dir.path(), 1, None, next, 1);
            assert_eq!(reply, CommitReply::Invalid { id: String::new(), field: "symbolOverrides" }, "{case:?}");
        }
        assert_eq!(store.current().revision, 1);
    }

    #[test]
    fn rate_keys_stay_the_same_across_save_load_save() {
        let cases: [(&[(&str, f64)], &[(&str, &str)]); 5] = [
            (&[("pt", 20.0)], &[("PT", "USD")]),
            (&[("PT", 20.0)], &[("pt", "USD")]),
            (&[("r$", 30.0), ("brl", 31.0)], &[("R$", "EUR")]),
            (&[("￥", 1.0), ("$", 150.0)], &[("￥", "CNY")]),
            (&[("__proto__", 2.0), ("ｂｒｌ", 3.0)], &[("__proto__", "EUR")]),
        ];
        for (rates, overrides) in cases {
            let (dir, store, slot) = setup(false);
            let mut next = store.current();
            next.rates = rates.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
            next.symbol_overrides = overrides.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
            let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
            assert_eq!(reply, CommitReply::Ok { revision: 1 }, "{rates:?}");
            let first = store.current();
            let reloaded = DefsStore::load(dir.path(), 0).current();
            assert_eq!(reloaded.rates, first.rates, "{rates:?}");
            let (reply, _) = commit(&store, &slot, dir.path(), 1, None, reloaded, 2);
            assert_eq!(reply, CommitReply::Ok { revision: 2 }, "{rates:?}");
            assert_eq!(store.current().rates, first.rates, "{rates:?}");
            assert_eq!(store.current().symbol_overrides, first.symbol_overrides, "{rates:?}");
        }
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.rates = BTreeMap::from([("pt".to_owned(), 20.0)]);
        next.symbol_overrides = BTreeMap::from([("pt".to_owned(), "usd".to_owned())]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        assert_eq!(store.current().rates, BTreeMap::from([("USD".to_owned(), 20.0)]));
        assert_eq!(store.current().symbol_overrides, BTreeMap::from([("PT".to_owned(), "USD".to_owned())]));
    }

    #[test]
    fn symbol_override_limits_allow_the_boundary() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.symbol_overrides = (0..64).map(|i| (format!("{i:0>16}"), "USD".to_owned())).collect();
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.symbol_overrides = BTreeMap::from([(format!(" {} ", "💰".repeat(16)), "usd".to_owned())]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
    }

    #[test]
    fn rate_table_errors_are_field_errors_without_write() {
        let (dir, store, slot) = setup(false);
        for (rates, field) in [
            (vec![("USD", 150.0), ("$", 151.0)], "rateCode"),
            (vec![("usd", 150.0), ("USD", 151.0)], "rateCode"),
            (vec![(" ", 150.0)], "rateCode"),
            (vec![("USD", 0.0)], "rateValue"),
            (vec![("USD", -1.0)], "rateValue"),
            (vec![("USD", f64::NAN)], "rateValue"),
            (vec![("USD", f64::INFINITY)], "rateValue"),
        ] {
            let mut next = store.current();
            next.rates = rates.iter().map(|(k, v)| ((*k).to_owned(), *v)).collect();
            let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
            assert!(matches!(&reply, CommitReply::Invalid { field: f, .. } if *f == field), "{rates:?} {reply:?}");
        }
        assert_eq!(store.current().revision, 0);
    }

    #[test]
    fn rate_keys_are_normalized_so_symbol_donations_convert() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.rates = BTreeMap::from([("$".to_owned(), 150.5), ("jpy".to_owned(), 1.0)]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        assert_eq!(reply, CommitReply::Ok { revision: 1 });
        let defs = store.current();
        assert_eq!(defs.rates.get("USD"), Some(&150.5));
        assert!(!defs.rates.contains_key("$") && defs.rates.contains_key("JPY"));
        let mut totals = crate::achievements::currency::AmountTotals::default();
        totals.add("$", 1.0);
        assert_eq!(totals.jpy_milli(&defs.rates, &defs.symbol_overrides), 150_500);
    }

    #[test]
    fn stale_disable_writes_nothing() {
        let (dir, store, slot) = setup(true);
        let other = DefsStore::load(dir.path(), 0);
        let mut external = other.current();
        external.enabled = true;
        assert_eq!(other.commit(0, external), Ok(1));
        let progress = dir.path().join(crate::achievements::store::PROGRESS_FILE);
        let mut next = store.current();
        next.enabled = false;
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 10);
        assert_eq!(reply, CommitReply::Stale);
        assert!(!progress.exists(), "Stale なのに進捗を書いた");
    }

    #[test]
    fn external_disable_with_failed_flush_keeps_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut defs = store.current();
        defs.enabled = true;
        assert_eq!(store.commit(0, defs.clone()), Ok(1));
        let engine = AchievementEngine::open_with(dir.path(), defs, 0, Arc::new(DenyWrites));
        let slot = Mutex::new(Some(Arc::new(engine)));
        let other = DefsStore::load(dir.path(), 0);
        let mut external = other.current();
        external.enabled = false;
        assert_eq!(other.commit(1, external), Ok(2));
        let _ = refresh(&store, &slot, dir.path(), 5);
        assert!(slot.lock().unwrap().is_some(), "flush 失敗でエンジンを破棄した");
        assert_eq!(store.current().revision, 1, "flush 失敗なのに外部の無効化を取り込んだ");
    }

    struct Toggle(std::sync::atomic::AtomicBool);
    impl crate::achievements::store::StoreIo for Toggle {
        fn write_atomic(&self, path: &Path, bytes: &[u8]) -> std::io::Result<()> {
            if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                return Err(std::io::ErrorKind::PermissionDenied.into());
            }
            crate::achievements::store::StoreIo::write_atomic(&crate::achievements::store::FsIo, path, bytes)
        }
        fn create_backup(&self, src: &Path, dst: &Path) -> std::io::Result<()> {
            crate::achievements::store::StoreIo::create_backup(&crate::achievements::store::FsIo, src, dst)
        }
    }

    #[test]
    fn disable_delivers_unlocks_waiting_for_retry() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(crate::achievements::store::PROGRESS_FILE), PROGRESS_50).unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut defs = store.current();
        defs.enabled = true;
        defs.achievements.iter_mut().find(|a| a.id == "viewer-comments-100").unwrap()
            .condition.as_mut().unwrap().threshold = 40.0;
        assert_eq!(store.commit(0, defs.clone()), Ok(1));
        let io = Arc::new(Toggle(std::sync::atomic::AtomicBool::new(true)));
        let engine = Arc::new(AchievementEngine::open_with(dir.path(), defs.clone(), 0, io.clone()));
        assert!(engine.set_definitions(defs, 10).is_empty(), "保存失敗の解除は返さない");
        io.0.store(false, std::sync::atomic::Ordering::SeqCst);
        let slot = Mutex::new(Some(engine));
        let mut next = store.current();
        next.enabled = false;
        let (reply, unlocks) = commit(&store, &slot, dir.path(), 1, None, next, 20);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
        assert!(unlocks.iter().any(|u| u.achievement_id == "viewer-comments-100"), "{unlocks:?}");
    }

    #[test]
    fn rate_errors_do_not_echo_the_input() {
        let (dir, store, slot) = setup(false);
        let mut next = store.current();
        next.rates = BTreeMap::from([("https://example.invalid/hook/TEST_ONLY".to_owned(), 0.0)]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 1);
        let CommitReply::Invalid { id, .. } = reply else { panic!("{reply:?}") };
        assert!(!id.contains("TEST_ONLY"), "{id}");
    }

    #[test]
    fn draft_read_before_a_same_revision_external_edit_is_stale() {
        let (dir, store, slot) = setup(false);
        assert_eq!(store.commit(0, store.current()), Ok(1));
        let (mut draft, token) = store.snapshot();
        let path = dir.path().join(crate::achievements::store::DEFS_FILE);
        let mut json: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        json["achievements"][0]["name"] = "external".into();
        std::fs::write(&path, serde_json::to_vec_pretty(&json).unwrap()).unwrap();
        // 別の画面の一覧取得で取り込まれる(revision は 1 のまま)。
        refresh(&store, &slot, dir.path(), 5).unwrap();
        assert_eq!(store.current().revision, 1);
        assert_eq!(store.current().achievements[0].name, "external");

        draft.achievements[1].name = "mine".into();
        let (reply, _) = commit(&store, &slot, dir.path(), 1, Some(&token), draft, 6);
        assert_eq!(reply, CommitReply::Stale);
        assert_eq!(store.current().achievements[0].name, "external");

        let (next, token) = store.snapshot();
        let (reply, _) = commit(&store, &slot, dir.path(), 1, Some(&token), next, 7);
        assert_eq!(reply, CommitReply::Ok { revision: 2 });
    }

    #[test]
    fn stale_commit_does_not_close_the_engine_or_write_progress() {
        let (dir, store, slot) = setup(true);
        let other = DefsStore::load(dir.path(), 0);
        let mut external = other.current();
        external.enabled = false;
        external.achievements[0].name = "external".into();
        assert_eq!(other.commit(0, external), Ok(1));
        let mut next = store.current();
        next.enabled = true;
        let (reply, unlocks) = commit(&store, &slot, dir.path(), 0, None, next, 10);
        assert_eq!(reply, CommitReply::Stale);
        assert!(unlocks.is_empty());
        assert!(!dir.path().join(crate::achievements::store::PROGRESS_FILE).exists(), "Stale で進捗を書いた");
        assert!(slot.lock().unwrap().is_some(), "Stale でエンジンを閉じた");
        assert_eq!(store.current().revision, 0);
    }

    #[test]
    fn stale_is_reported_before_rate_normalization_errors() {
        let (dir, store, slot) = setup(false);
        let other = DefsStore::load(dir.path(), 0);
        assert_eq!(other.commit(0, other.current()), Ok(1));
        let mut next = store.current();
        next.rates = BTreeMap::from([("$".to_owned(), 150.0), ("USD".to_owned(), 151.0)]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, None, next, 10);
        assert_eq!(reply, CommitReply::Stale);
        // revision が合っていても内容トークンが違えば、正規化より先に Stale。
        let mut next = store.current();
        next.rates = BTreeMap::from([("$".to_owned(), 150.0), ("USD".to_owned(), 151.0)]);
        let (reply, _) = commit(&store, &slot, dir.path(), 0, Some("not-the-token"), next, 10);
        assert_eq!(reply, CommitReply::Stale);
    }

    #[test]
    fn broken_defs_are_backed_up_once_even_while_the_flush_fails() {
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut defs = store.current();
        defs.enabled = true;
        assert_eq!(store.commit(0, defs.clone()), Ok(1));
        let engine = AchievementEngine::open_with(dir.path(), defs, 0, Arc::new(DenyWrites));
        let slot = Mutex::new(Some(Arc::new(engine)));
        std::fs::write(dir.path().join(crate::achievements::store::DEFS_FILE), "{broken").unwrap();
        for now in 1..4 {
            assert!(refresh(&store, &slot, dir.path(), now).is_err());
        }
        let backups = std::fs::read_dir(dir.path()).unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains(".broken-"))
            .count();
        assert_eq!(backups, 1);
        assert!(slot.lock().unwrap().is_some());
    }

    #[test]
    fn stale_is_reported_before_draft_conversion_errors() {
        let (dir, store, slot) = setup(false);
        let other = DefsStore::load(dir.path(), 0);
        assert_eq!(other.commit(0, other.current()), Ok(1));
        let defs = store.current();
        let mut inputs: Vec<AchievementInput> = defs.achievements.iter().map(input_of).collect();
        let blank = inputs.iter_mut().find(|a| a.condition.is_some()).unwrap();
        blank.condition.as_mut().unwrap().threshold = None;
        let draft = AchievementDraft {
            enabled: false,
            achievements: inputs,
            rates: defs.rates.clone(),
            symbol_overrides: defs.symbol_overrides.clone(),
        };
        let (reply, _) = commit_draft(&store, &slot, dir.path(), 0, None, draft, 10);
        assert_eq!(reply, CommitReply::Stale);
    }

    #[test]
    fn failed_backup_of_broken_defs_is_a_refresh_error() {
        let dir = tempfile::tempdir().unwrap();
        let writer = DefsStore::load(dir.path(), 0);
        assert_eq!(writer.commit(0, writer.current()), Ok(1));
        let store = DefsStore::load_with(dir.path(), 0, Arc::new(DenyWrites));
        assert_eq!(store.current().revision, 1);
        std::fs::write(dir.path().join(crate::achievements::store::DEFS_FILE), "{broken").unwrap();
        let slot = Mutex::new(None);
        assert!(refresh(&store, &slot, dir.path(), 1).is_err(), "退避に失敗したのに Ok を返した");
        assert_eq!(store.current().revision, 1);
    }
}
