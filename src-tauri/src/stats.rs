//! Goals overlay 用の統計集約。
//!
//! chat bus と YouTube メタデータを集約し、最新 `StatsSnapshot` を watch channel で
//! UI/OBS へ fan-out する。

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::achievements::currency::RateTable;
use crate::achievements::store::DefsStore;
use crate::config::{AppConfig, ChannelPlatform};
use crate::listeners::counts_as_donation;
use crate::model::{ChatMessage, MessageKind, Platform};
use crate::sources::niconico::extract_live_id;
use crate::sources::twitcasting::channel_name as twitcasting_channel_name;
use crate::sources::x::x_channel_name;
use crate::sources::youtube::{extract_video_id, is_channel_identifier};
use crate::sound_effects::jpy_milli;

/// 接続中チャンネルに表示する補助タイトル。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelTitle {
    pub platform: String,
    pub identifier: String,
    pub title: String,
}

/// 視聴者数が同時接続数か累計来場者数かを表す種別。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ViewerCountKind {
    Concurrent,
    Cumulative,
}

impl Default for ViewerCountKind {
    fn default() -> Self {
        ViewerCountKind::Concurrent
    }
}

/// 接続中チャンネルのライブ状態と表示メタデータ。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelStatus {
    pub platform: String,
    pub identifier: String,
    pub title: Option<String>,
    pub viewers: Option<u32>,
    pub viewers_kind: ViewerCountKind,
    pub live: Option<bool>,
    /// 待機中に出す状態メッセージ(cookie 未設定・認証失敗・アカウント不在など)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
}

/// OBS Goals overlay へ送る最新統計。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct StatsSnapshot {
    pub comments: u32,
    pub viewers: u32,
    pub viewers_max: u32,
    pub likes: u32,
    pub likes_available: bool,
    pub reactions: u32,
    pub reactions_available: bool,
    /// 投げ銭の円換算合計(円、切り捨て)。アプリ起動中の累計。
    #[serde(default)]
    pub donation: u32,
    /// コメントした人数(有効なチャンネルごとのユニーク発言者の和)。
    #[serde(default)]
    pub commenters: u32,
    pub goals_enabled: bool,
    pub goals_visible: GoalsVisibilitySnapshot,
    #[serde(default)]
    pub channel_titles: Vec<ChannelTitle>,
    #[serde(default)]
    pub channel_status: Vec<ChannelStatus>,
    pub goals: GoalsSnapshot,
    pub updated_at: u64,
}

/// OBS Timer overlay へ送るタイマー基準スナップショット。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimerSnapshot {
    pub state: String,
    pub mode: String,
    pub duration_sec: u32,
    pub base_elapsed_sec: u32,
    pub running_since_ms: u64,
    pub updated_at: u64,
}

impl Default for TimerSnapshot {
    fn default() -> Self {
        TimerSnapshot {
            state: "idle".to_string(),
            mode: "countdown".to_string(),
            duration_sec: 0,
            base_elapsed_sec: 0,
            running_since_ms: 0,
            updated_at: 0,
        }
    }
}

/// Tauri コマンドと HTTP API に共通のタイマー遷移。
pub fn next_timer_snapshot(
    current: &TimerSnapshot,
    action: crate::api::TimerAction,
    duration_sec: Option<u32>,
    default_duration_sec: u32,
    mode: &str,
    now_ms: u64,
) -> TimerSnapshot {
    let mut next = current.clone();
    match action {
        crate::api::TimerAction::Start => {
            let dur = duration_sec.unwrap_or(default_duration_sec);
            next = TimerSnapshot {
                state: "running".to_string(),
                mode: if mode == "elapsed" {
                    "elapsed"
                } else {
                    "countdown"
                }
                .to_string(),
                duration_sec: dur,
                base_elapsed_sec: 0,
                running_since_ms: now_ms,
                updated_at: now_ms,
            };
        }
        crate::api::TimerAction::Pause => {
            if next.state == "running" {
                let elapsed_sec = now_ms.saturating_sub(next.running_since_ms) / 1000;
                let elapsed_sec = u32::try_from(elapsed_sec).unwrap_or(u32::MAX);
                next.base_elapsed_sec = next.base_elapsed_sec.saturating_add(elapsed_sec);
            }
            next.state = "paused".to_string();
            next.running_since_ms = 0;
            next.updated_at = now_ms;
        }
        crate::api::TimerAction::Resume => {
            next.state = "running".to_string();
            next.running_since_ms = now_ms;
            next.updated_at = now_ms;
        }
        crate::api::TimerAction::Reset => {
            next.state = "idle".to_string();
            next.base_elapsed_sec = 0;
            next.running_since_ms = 0;
            next.updated_at = now_ms;
        }
    }
    next
}

/// 設定由来の目標値。0 は目標未設定として 0% のゲージを表示する。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalsSnapshot {
    pub comments: u32,
    pub viewers: u32,
    pub likes: u32,
    pub reactions: u32,
    /// 投げ銭の目標(円)。
    #[serde(default)]
    pub donation: u32,
    #[serde(default)]
    pub commenters: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalsVisibilitySnapshot {
    pub comments: bool,
    pub viewers: bool,
    pub likes: bool,
    pub reactions: bool,
    #[serde(default)]
    pub donation: bool,
    #[serde(default)]
    pub commenters: bool,
}

/// メタデータ poller から集約タスクへ渡す更新。
#[derive(Debug, Clone)]
pub struct YoutubeMetadataUpdate {
    pub platform: Platform,
    pub channel: String,
    pub concurrent_viewers: Option<u32>,
    pub viewers_kind: ViewerCountKind,
    pub likes: Option<u32>,
    pub title: Option<String>,
    pub live: Option<bool>,
    /// 状態メッセージ。`live` または `notice` を持つ更新で置き換わる(None で消える)。
    pub notice: Option<String>,
    pub reactions_delta: Option<u32>,
    /// `None` を「取得値なし」として反映する完全なメタデータスナップショットか。
    pub full_snapshot: bool,
}

#[derive(Debug, Clone, Default)]
struct MetadataState {
    platform: Option<Platform>,
    channel: String,
    concurrent_viewers: Option<u32>,
    viewers_kind: ViewerCountKind,
    likes: Option<u32>,
    title: Option<String>,
    live: Option<bool>,
    notice: Option<String>,
}

impl Default for YoutubeMetadataUpdate {
    fn default() -> Self {
        YoutubeMetadataUpdate {
            platform: Platform::Youtube,
            channel: String::new(),
            concurrent_viewers: None,
            viewers_kind: ViewerCountKind::Concurrent,
            likes: None,
            title: None,
            live: None,
            notice: None,
            reactions_delta: None,
            full_snapshot: false,
        }
    }
}

impl YoutubeMetadataUpdate {
    fn has_channel_metadata(&self) -> bool {
        self.concurrent_viewers.is_some()
            || self.likes.is_some()
            || self.title.is_some()
            || self.live.is_some()
            || self.full_snapshot
    }
}

/// watch::Sender を使う統計集約タスクを起動する。
///
/// `bus_rx` は chat bus の購読、`metadata_rx` は YouTube poller からの値、`config_rx`
/// は設定変更通知。どれも await 中に MutexGuard を保持しない形で受け取る。
/// `achievement_defs` は投げ銭の円換算用(効果音と同じレート表・通貨記号の上書き)。
/// 再読込を反映するため、投げ銭を受けるたびに最新の定義を読む。
pub fn spawn_stats_aggregator(
    mut bus_rx: broadcast::Receiver<crate::bus::SequencedChat>,
    stats_tx: watch::Sender<StatsSnapshot>,
    mut config_rx: watch::Receiver<AppConfig>,
    mut metadata_rx: mpsc::Receiver<YoutubeMetadataUpdate>,
    achievement_defs: Arc<DefsStore>,
    app: AppHandle,
    cancel: CancellationToken,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut comments = 0u32;
        let mut reactions = 0u32;
        // 投げ銭はミリ円で累計し、表示時に円へ切り捨てる(1 件ごとの丸め誤差を溜めない)。
        let mut donation_milli = 0i64;
        let mut viewers_max = 0u32;
        let mut viewer_scopes: HashMap<String, HashSet<String>> = HashMap::new();
        let mut metadata: HashMap<String, MetadataState> = HashMap::new();
        let mut config = config_rx.borrow().clone();

        let mut ticker = interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut snapshot = build_snapshot(
            comments,
            viewers_max,
            reactions,
            donation_milli,
            &viewer_scopes,
            &metadata,
            &config,
        );
        publish_snapshot(&stats_tx, &app, &snapshot);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = bus_rx.recv() => {
                    match recv {
                        Ok(chat) => {
                            comments = comments.saturating_add(1);
                            record_unique_viewer(&mut viewer_scopes, &chat.message);
                            if counts_as_donation(&chat.message) {
                                let defs = achievement_defs.current();
                                donation_milli = donation_milli.saturating_add(donation_milli_of(
                                    &chat.message,
                                    &defs.rates,
                                    &defs.symbol_overrides,
                                ));
                            }
                            snapshot = build_snapshot(
                                comments,
                                viewers_max,
                                reactions,
                                donation_milli,
                                &viewer_scopes,
                                &metadata,
                                &config,
                            );
                            viewers_max = viewers_max.max(snapshot.viewers);
                            snapshot.viewers_max = viewers_max;
                            publish_snapshot(&stats_tx, &app, &snapshot);
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!("stats aggregator が {n} 件 lag");
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
                update = metadata_rx.recv() => {
                    let Some(update) = update else {
                        break;
                    };
                    apply_metadata_update(&mut metadata, &mut reactions, update, &config);
                    snapshot = build_snapshot(
                        comments,
                        viewers_max,
                        reactions,
                        donation_milli,
                        &viewer_scopes,
                        &metadata,
                        &config,
                    );
                    viewers_max = viewers_max.max(snapshot.viewers);
                    snapshot.viewers_max = viewers_max;
                    publish_snapshot(&stats_tx, &app, &snapshot);
                }
                changed = config_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    config = config_rx.borrow_and_update().clone();
                    retain_enabled_scopes(&mut viewer_scopes, &config);
                    retain_enabled_metadata(&mut metadata, &config);
                    snapshot = build_snapshot(
                        comments,
                        viewers_max,
                        reactions,
                        donation_milli,
                        &viewer_scopes,
                        &metadata,
                        &config,
                    );
                    viewers_max = viewers_max.max(snapshot.viewers);
                    snapshot.viewers_max = viewers_max;
                    publish_snapshot(&stats_tx, &app, &snapshot);
                }
                _ = ticker.tick() => {
                    snapshot = build_snapshot(
                        comments,
                        viewers_max,
                        reactions,
                        donation_milli,
                        &viewer_scopes,
                        &metadata,
                        &config,
                    );
                    viewers_max = viewers_max.max(snapshot.viewers);
                    snapshot.viewers_max = viewers_max;
                    publish_snapshot(&stats_tx, &app, &snapshot);
                }
            }
        }
        tracing::info!("stats aggregator 終了");
    })
}

fn publish_snapshot(
    stats_tx: &watch::Sender<StatsSnapshot>,
    app: &AppHandle,
    snapshot: &StatsSnapshot,
) {
    let mut out = snapshot.clone();
    out.updated_at = now_ms();
    let _ = stats_tx.send_replace(out.clone());
    if let Err(e) = app.emit("stats", &out) {
        tracing::warn!("stats emit に失敗: {e}");
    }
}

fn build_snapshot(
    comments: u32,
    viewers_max: u32,
    reactions: u32,
    donation_milli: i64,
    viewer_scopes: &HashMap<String, HashSet<String>>,
    metadata: &HashMap<String, MetadataState>,
    config: &AppConfig,
) -> StatsSnapshot {
    let unique_count = commenters_count(viewer_scopes);
    let concurrent_total = metadata
        .values()
        .filter(|m| m.viewers_kind == ViewerCountKind::Concurrent)
        .filter_map(|m| m.concurrent_viewers)
        .fold(0u32, u32::saturating_add);
    let viewers = resolve_viewers(concurrent_total, unique_count);
    let likes = metadata
        .values()
        .filter_map(|m| m.likes)
        .fold(0u32, u32::saturating_add);
    let mut channel_titles = metadata
        .iter()
        .filter_map(|(_key, metadata)| {
            metadata.title.as_ref().map(|title| ChannelTitle {
                platform: metadata
                    .platform
                    .map(platform_key)
                    .unwrap_or("youtube")
                    .to_string(),
                identifier: metadata.channel.clone(),
                title: title.clone(),
            })
        })
        .collect::<Vec<_>>();
    channel_titles.sort_by(|a, b| {
        a.platform
            .cmp(&b.platform)
            .then_with(|| a.identifier.cmp(&b.identifier))
    });
    let mut channel_status = metadata
        .values()
        .map(|metadata| ChannelStatus {
            platform: metadata
                .platform
                .map(platform_key)
                .unwrap_or("youtube")
                .to_string(),
            identifier: metadata.channel.clone(),
            title: metadata.title.clone(),
            viewers: metadata.concurrent_viewers,
            viewers_kind: metadata.viewers_kind,
            live: metadata.live,
            notice: metadata.notice.clone(),
        })
        .collect::<Vec<_>>();
    channel_status.sort_by(|a, b| {
        a.platform
            .cmp(&b.platform)
            .then_with(|| a.identifier.cmp(&b.identifier))
    });

    StatsSnapshot {
        comments,
        viewers,
        viewers_max: viewers_max.max(viewers),
        likes,
        likes_available: has_enabled_youtube(config),
        reactions,
        reactions_available: has_enabled_youtube(config),
        donation: donation_yen(donation_milli),
        commenters: unique_count,
        goals_enabled: config.goals.enabled,
        goals_visible: goals_visibility_from_config(config),
        channel_titles,
        channel_status,
        goals: goals_from_config(config),
        updated_at: 0,
    }
}

fn apply_metadata_update(
    metadata: &mut HashMap<String, MetadataState>,
    reactions: &mut u32,
    update: YoutubeMetadataUpdate,
    config: &AppConfig,
) {
    let has_channel_metadata = update.has_channel_metadata();
    if let Some(delta) = update.reactions_delta {
        *reactions = (*reactions).saturating_add(delta);
    }
    if has_channel_metadata {
        merge_metadata_update(metadata, update);
        retain_enabled_metadata(metadata, config);
    } else if update.notice.as_deref().is_some_and(|n| n.trim().is_empty()) {
        // notice だけの明示的な消去。視聴者数などは触らず notice だけ消す。
        if let Some(entry) = metadata.get_mut(&metadata_key(update.platform, &update.channel)) {
            entry.notice = None;
        }
    }
}

fn merge_metadata_update(
    metadata: &mut HashMap<String, MetadataState>,
    update: YoutubeMetadataUpdate,
) {
    let key = metadata_key(update.platform, &update.channel);
    let entry = metadata.entry(key).or_insert_with(MetadataState::default);
    entry.platform = Some(update.platform);
    entry.channel = update.channel;
    entry.concurrent_viewers = update.concurrent_viewers;
    entry.viewers_kind = update.viewers_kind;
    if let Some(v) = update.likes {
        entry.likes = Some(v);
    }
    if let Some(title) = update.title {
        entry.title = Some(title);
    }
    if update.live.is_some() || update.notice.is_some() {
        // 空・空白だけの状態メッセージは「なし」(OBS の /stats で "" を出さない)。
        entry.notice = update.notice.filter(|n| !n.trim().is_empty());
    }
    if let Some(live) = update.live {
        entry.live = Some(live);
        if !live {
            entry.title = None;
            entry.concurrent_viewers = None;
            entry.likes = None;
        }
    }
}

/// 1 件の投げ銭のミリ円。効果音と同じ換算(`sound_effects::jpy_milli`)で、
/// 数えない種別(`listeners::counts_as_donation`)・換算できない通貨は 0。
fn donation_milli_of(
    msg: &ChatMessage,
    rates: &RateTable,
    symbols: &BTreeMap<String, String>,
) -> i64 {
    if !counts_as_donation(msg) {
        return 0;
    }
    jpy_milli(msg, rates, symbols).unwrap_or(0).max(0)
}

/// ミリ円の累計を円の整数へ切り捨てる。
fn donation_yen(milli: i64) -> u32 {
    u32::try_from(milli.max(0) / 1000).unwrap_or(u32::MAX)
}

/// コメントした人数。スコープ(プラットフォーム+チャンネル)ごとのユニーク発言者数の和。
fn commenters_count(viewer_scopes: &HashMap<String, HashSet<String>>) -> u32 {
    viewer_scopes
        .values()
        .map(|set| u32::try_from(set.len()).unwrap_or(u32::MAX))
        .fold(0u32, u32::saturating_add)
}

fn record_unique_viewer(
    viewer_scopes: &mut HashMap<String, HashSet<String>>,
    msg: &ChatMessage,
) {
    // システム通知・テスト注入・集計対象外は「コメントした人」に数えない(投げ銭の判定と揃える)。
    if msg.kind == MessageKind::System || msg.is_injected_test_comment() || msg.route.uncounted {
        return;
    }
    let viewer = viewer_key(msg);
    if viewer.is_empty() {
        return;
    }
    let scope = scope_key(msg.platform, &msg.channel);
    viewer_scopes.entry(scope).or_default().insert(viewer);
}

fn viewer_key(msg: &ChatMessage) -> String {
    let id = msg.author.id.trim();
    if !id.is_empty() {
        return format!("id:{id}");
    }
    let name = msg.author.name.trim().to_lowercase();
    if name.is_empty() {
        String::new()
    } else {
        format!("name:{name}")
    }
}

fn scope_key(platform: Platform, channel: &str) -> String {
    format!("{}:{channel}", platform_key(platform))
}

fn metadata_key(platform: Platform, channel: &str) -> String {
    format!("{}:{channel}", platform_key(platform))
}

fn platform_key(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::X => "x",
        Platform::Niconico => "niconico",
        Platform::Twitcasting => "twitcasting",
    }
}

fn enabled_scope_keys(config: &AppConfig) -> HashSet<String> {
    config
        .channels
        .iter()
        .filter(|ch| ch.enabled)
        .map(|ch| match ch.platform {
            ChannelPlatform::Twitch => format!("twitch:{}", ch.identifier),
            ChannelPlatform::Youtube => format!("youtube:{}", extract_video_id(&ch.identifier)),
            // XSource は ChatMessage.channel に正規化済み broadcast ID(アカウントモードは @handle 小文字)を入れる。
            ChannelPlatform::X => format!("x:{}", x_channel_name(&ch.identifier)),
            // NiconicoSource も同様に正規化済み lv 番組 ID を入れる。
            ChannelPlatform::Niconico => {
                format!("niconico:{}", extract_live_id(&ch.identifier))
            }
            ChannelPlatform::Twitcasting => {
                format!("twitcasting:{}", twitcasting_channel_name(&ch.identifier))
            }
        })
        .collect()
}

fn retain_enabled_scopes(
    viewer_scopes: &mut HashMap<String, HashSet<String>>,
    config: &AppConfig,
) {
    let enabled = enabled_scope_keys(config);
    if enabled.is_empty() {
        viewer_scopes.clear();
        return;
    }
    // @handle / チャンネル URL で登録した YouTube は、発言が解決後の動画 ID で記録されるため
    // 設定からキーを作れない。その場合は YouTube のスコープを丸ごと残す(人数を消さない方を優先)。
    let keep_all_youtube = config.channels.iter().any(|ch| {
        ch.enabled
            && matches!(ch.platform, ChannelPlatform::Youtube)
            && is_channel_identifier(&ch.identifier)
    });
    viewer_scopes.retain(|scope, _| {
        enabled.contains(scope) || (keep_all_youtube && scope.starts_with("youtube:"))
    });
}

fn enabled_metadata_keys(config: &AppConfig) -> HashSet<String> {
    config
        .channels
        .iter()
        .filter(|ch| ch.enabled)
        .map(|ch| match ch.platform {
            ChannelPlatform::Twitch => metadata_key(Platform::Twitch, &ch.identifier),
            ChannelPlatform::Youtube if is_channel_identifier(&ch.identifier) => {
                metadata_key(Platform::Youtube, &ch.identifier)
            }
            ChannelPlatform::Youtube => {
                metadata_key(Platform::Youtube, &extract_video_id(&ch.identifier))
            }
            ChannelPlatform::X => {
                metadata_key(Platform::X, &x_channel_name(&ch.identifier))
            }
            ChannelPlatform::Niconico => {
                metadata_key(Platform::Niconico, &extract_live_id(&ch.identifier))
            }
            ChannelPlatform::Twitcasting => metadata_key(
                Platform::Twitcasting,
                &twitcasting_channel_name(&ch.identifier),
            ),
        })
        .collect()
}

fn retain_enabled_metadata(metadata: &mut HashMap<String, MetadataState>, config: &AppConfig) {
    let enabled = enabled_metadata_keys(config);
    if enabled.is_empty() {
        metadata.clear();
        return;
    }
    metadata.retain(|key, _| enabled.contains(key));
}

fn has_enabled_youtube(config: &AppConfig) -> bool {
    config
        .channels
        .iter()
        .any(|ch| ch.enabled && ch.platform == ChannelPlatform::Youtube)
}

fn goals_from_config(config: &AppConfig) -> GoalsSnapshot {
    if !config.goals.enabled {
        return GoalsSnapshot::default();
    }
    GoalsSnapshot {
        comments: config.goals.comments,
        viewers: config.goals.viewers,
        likes: config.goals.likes,
        reactions: config.goals.reactions,
        donation: config.goals.donation,
        commenters: config.goals.commenters,
    }
}

fn goals_visibility_from_config(config: &AppConfig) -> GoalsVisibilitySnapshot {
    GoalsVisibilitySnapshot {
        comments: config
            .goals
            .show_comments
            .unwrap_or(config.goals.comments > 0),
        viewers: config
            .goals
            .show_viewers
            .unwrap_or(config.goals.viewers > 0),
        likes: config.goals.show_likes.unwrap_or(config.goals.likes > 0),
        reactions: config
            .goals
            .show_reactions
            .unwrap_or(config.goals.reactions > 0),
        donation: config
            .goals
            .show_donation
            .unwrap_or(config.goals.donation > 0),
        commenters: config
            .goals
            .show_commenters
            .unwrap_or(config.goals.commenters > 0),
    }
}

fn resolve_viewers(concurrent_total: u32, _unique_count: u32) -> u32 {
    // UI/OBS ではこの値を「同時接続」と表示するため、取得不能・0人時に
    // セッション累積のユニークコメント投稿者数へ置き換えない。
    concurrent_total
}

/// ゲージ進捗率。UI 側は 100% を超えた値を強調表示できる。
pub fn goal_percent(current: u32, target: u32) -> u32 {
    if target == 0 {
        return 0;
    }
    ((current as u64 * 100) / target as u64).min(u32::MAX as u64) as u32
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ChannelConfig;
    use crate::model::{Amount, Author, Fragment, MessageKind, Roles};

    fn msg(platform: Platform, channel: &str, id: &str, name: &str) -> ChatMessage {
        ChatMessage {
            listener: None,
            native_id: false,
            id: ChatMessage::new_id(),
            platform,
            channel: channel.to_string(),
            author: Author {
                nickname: None,
                avatar_url: None,
                id: id.to_string(),
                name: name.to_string(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::text("hello")],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms: 0,
            raw: None,
            skip_tts: false,
            injected_test: false,
            route: Default::default(),
            sound: None,
        }
    }

    #[test]
    fn youtube_handle_channel_keeps_commenters_on_config_change() {
        let config = config_with_youtube("@example");
        let mut scopes = HashMap::new();
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "dQw4w9WgXcQ", "u1", "a"));
        retain_enabled_scopes(&mut scopes, &config);
        assert_eq!(commenters_count(&scopes), 1);
    }

    #[test]
    fn commenters_skip_system_injected_and_uncounted() {
        let mut scopes = HashMap::new();
        record_unique_viewer(&mut scopes, &msg(Platform::Twitch, "ch", "u1", "a"));
        let mut system = msg(Platform::Twitch, "ch", "sys", "system");
        system.kind = MessageKind::System;
        record_unique_viewer(&mut scopes, &system);
        let mut injected = msg(Platform::Twitch, "ch", "t1", "test");
        injected.injected_test = true;
        record_unique_viewer(&mut scopes, &injected);
        let mut uncounted = msg(Platform::Twitch, "ch", "x1", "x");
        uncounted.route.uncounted = true;
        record_unique_viewer(&mut scopes, &uncounted);
        assert_eq!(commenters_count(&scopes), 1);
    }

    #[test]
    fn goal_percent_handles_zero_and_over_goal() {
        assert_eq!(goal_percent(50, 0), 0);
        assert_eq!(goal_percent(25, 100), 25);
        assert_eq!(goal_percent(150, 100), 150);
    }

    #[test]
    fn viewers_use_only_concurrent_count() {
        assert_eq!(resolve_viewers(42, 7), 42);
        assert_eq!(resolve_viewers(0, 7), 0);
    }

    #[test]
    fn full_metadata_snapshot_clears_missing_viewer_count() {
        let config = config_with_youtube("@example");
        let mut metadata = HashMap::new();
        let mut reactions = 0;
        apply_metadata_update(
            &mut metadata,
            &mut reactions,
            YoutubeMetadataUpdate {
                channel: "@example".to_string(),
                concurrent_viewers: Some(123),
                full_snapshot: true,
                ..YoutubeMetadataUpdate::default()
            },
            &config,
        );
        apply_metadata_update(
            &mut metadata,
            &mut reactions,
            YoutubeMetadataUpdate {
                channel: "@example".to_string(),
                full_snapshot: true,
                ..YoutubeMetadataUpdate::default()
            },
            &config,
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.viewers, 0);
        assert_eq!(snapshot.channel_status[0].viewers, None);
    }

    fn money(kind: MessageKind, value: f64, currency: &str) -> ChatMessage {
        let mut m = msg(Platform::Youtube, "video", "donor", "Donor");
        m.kind = kind;
        m.amount = Some(Amount {
            value,
            currency: currency.to_string(),
            raw_text: String::new(),
        });
        m
    }

    fn sum_donation(
        msgs: &[ChatMessage],
        rates: &RateTable,
        symbols: &BTreeMap<String, String>,
    ) -> u32 {
        let milli = msgs
            .iter()
            .map(|m| donation_milli_of(m, rates, symbols))
            .fold(0i64, i64::saturating_add);
        donation_yen(milli)
    }

    #[test]
    fn donation_sums_jpy_conversion_and_floors_to_yen() {
        let rates = BTreeMap::from([("JPY".to_string(), 1.0), ("USD".to_string(), 150.0)]);
        let msgs = [
            money(MessageKind::SuperChat, 500.0, "JPY"),
            money(MessageKind::SuperChat, 1.99, "USD"),
            money(MessageKind::Gift, 100.0, "JPY"),
        ];
        // 500 + 298.5 + 100 = 898.5 円 → 切り捨てで 898 円。
        assert_eq!(sum_donation(&msgs, &rates, &BTreeMap::new()), 898);
    }

    #[test]
    fn donation_ignores_unconvertible_currency_and_uncounted_kinds() {
        let rates = BTreeMap::from([("JPY".to_string(), 1.0)]);
        let mut uncounted = money(MessageKind::SuperChat, 300.0, "JPY");
        uncounted.route.uncounted = true;
        let mut injected = money(MessageKind::SuperChat, 300.0, "JPY");
        injected.injected_test = true;
        let msgs = [
            money(MessageKind::SuperChat, 1000.0, "JPY"),
            money(MessageKind::SuperChat, 10.0, "XYZ"),
            money(MessageKind::Bits, 100.0, "BITS"),
            money(MessageKind::Gift, 5.0, "SUBGIFT"),
            money(MessageKind::Normal, 200.0, "JPY"),
            money(MessageKind::System, 200.0, "JPY"),
            money(MessageKind::SuperChat, -50.0, "JPY"),
            uncounted,
            injected,
        ];
        assert_eq!(sum_donation(&msgs, &rates, &BTreeMap::new()), 1000);
    }

    #[test]
    fn donation_uses_symbol_overrides_and_rates() {
        let rates = BTreeMap::from([("USD".to_string(), 150.0)]);
        let symbols = BTreeMap::from([("ZZ$".to_string(), "USD".to_string())]);
        let msgs = [money(MessageKind::SuperChat, 2.0, "ZZ$")];
        assert_eq!(sum_donation(&msgs, &rates, &BTreeMap::new()), 0);
        assert_eq!(sum_donation(&msgs, &rates, &symbols), 300);
    }

    #[test]
    fn commenters_is_sum_of_unique_speakers_per_enabled_scope() {
        let mut config = config_with_youtube("video");
        config.channels.push(ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "streamer".to_string(),
            enabled: true,
            own_stream: false,
            overrides: Default::default(),
        });
        let mut scopes = HashMap::new();
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "video", "a", "A"));
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "video", "a", "A"));
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "video", "b", "B"));
        // 別スコープの同じ ID は別人として数える。
        record_unique_viewer(&mut scopes, &msg(Platform::Twitch, "streamer", "a", "A"));
        record_unique_viewer(&mut scopes, &msg(Platform::Twitch, "gone", "c", "C"));

        retain_enabled_scopes(&mut scopes, &config);
        let snapshot = build_snapshot(0, 0, 0, 0, &scopes, &HashMap::new(), &config);
        assert_eq!(snapshot.commenters, 3);
    }

    #[test]
    fn snapshot_exposes_donation_and_commenters_goals_and_visibility() {
        let mut config = AppConfig::default();
        config.goals.enabled = true;
        config.goals.donation = 5000;
        config.goals.commenters = 0;
        config.goals.show_donation = None;
        config.goals.show_commenters = None;

        let snapshot = build_snapshot(0, 0, 0, 1_234_999, &HashMap::new(), &HashMap::new(), &config);
        assert_eq!(snapshot.donation, 1234);
        assert_eq!(snapshot.goals.donation, 5000);
        assert_eq!(snapshot.goals.commenters, 0);
        assert!(snapshot.goals_visible.donation);
        assert!(!snapshot.goals_visible.commenters);

        let json = serde_json::to_value(&snapshot).expect("serialize snapshot");
        assert_eq!(json["donation"].as_u64(), Some(1234));
        assert_eq!(json["commenters"].as_u64(), Some(0));
        assert_eq!(json["goals"]["donation"].as_u64(), Some(5000));
        assert_eq!(json["goalsVisible"]["commenters"].as_bool(), Some(false));
    }

    #[test]
    fn goals_from_config_includes_reactions_when_enabled() {
        let mut config = AppConfig::default();
        config.goals.enabled = true;
        config.goals.comments = 10;
        config.goals.viewers = 20;
        config.goals.likes = 30;
        config.goals.reactions = 40;

        let goals = goals_from_config(&config);

        assert_eq!(goals.comments, 10);
        assert_eq!(goals.viewers, 20);
        assert_eq!(goals.likes, 30);
        assert_eq!(goals.reactions, 40);
    }

    #[test]
    fn snapshot_exposes_goals_enabled_separately_from_zero_targets() {
        let mut config = AppConfig::default();
        config.goals.enabled = true;

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &HashMap::new(), &config);

        assert!(snapshot.goals_enabled);
        assert_eq!(snapshot.goals.comments, 0);
        assert_eq!(snapshot.goals.viewers, 0);
        assert_eq!(snapshot.goals.likes, 0);
        assert_eq!(snapshot.goals.reactions, 0);
        assert!(snapshot.goals_visible.comments);
        assert!(snapshot.goals_visible.viewers);
        assert!(snapshot.goals_visible.likes);
        assert!(snapshot.goals_visible.reactions);
    }

    #[test]
    fn legacy_goal_visibility_follows_nonzero_targets() {
        let mut config = AppConfig::default();
        config.goals.show_comments = None;
        config.goals.show_viewers = None;
        config.goals.show_likes = None;
        config.goals.show_reactions = None;
        config.goals.comments = 10;

        let visible = goals_visibility_from_config(&config);

        assert!(visible.comments);
        assert!(!visible.viewers);
        assert!(!visible.likes);
        assert!(!visible.reactions);
    }

    #[test]
    fn unique_viewers_are_scoped_by_platform_and_channel() {
        let mut scopes = HashMap::new();
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "abc", "id1", "Alice"));
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "abc", "id1", "Alice"));
        record_unique_viewer(&mut scopes, &msg(Platform::Youtube, "def", "id1", "Alice"));
        record_unique_viewer(&mut scopes, &msg(Platform::Twitch, "abc", "", "ALICE"));
        record_unique_viewer(&mut scopes, &msg(Platform::Twitch, "abc", "", "alice"));

        let count: u32 = scopes.values().map(|set| set.len() as u32).sum();
        assert_eq!(count, 3);
    }

    #[test]
    fn concurrent_viewers_sum_across_platform_metadata() {
        let config = AppConfig {
            channels: vec![
                ChannelConfig {
                    platform: ChannelPlatform::Youtube,
                    identifier: "yt123".to_string(),
                    enabled: true,
                    own_stream: false,
                    overrides: Default::default(),
                },
                ChannelConfig {
                    platform: ChannelPlatform::Twitch,
                    identifier: "twlogin".to_string(),
                    enabled: true,
                    own_stream: false,
                    overrides: Default::default(),
                },
            ],
            ..AppConfig::default()
        };
        let mut metadata = HashMap::new();
        metadata.insert(
            metadata_key(Platform::Youtube, "yt123"),
            MetadataState {
                platform: Some(Platform::Youtube),
                channel: "yt123".to_string(),
                concurrent_viewers: Some(10),
                viewers_kind: ViewerCountKind::Concurrent,
                likes: Some(3),
                title: Some("YouTube Live".to_string()),
                live: Some(true),
                notice: None,
            },
        );
        metadata.insert(
            metadata_key(Platform::Twitch, "twlogin"),
            MetadataState {
                platform: Some(Platform::Twitch),
                channel: "twlogin".to_string(),
                concurrent_viewers: Some(7),
                viewers_kind: ViewerCountKind::Concurrent,
                likes: None,
                title: None,
                live: Some(true),
                notice: None,
            },
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.viewers, 17);
        assert_eq!(snapshot.likes, 3);
        assert_eq!(snapshot.reactions, 0);
        assert_eq!(snapshot.channel_titles.len(), 1);
        assert_eq!(snapshot.channel_titles[0].platform, "youtube");
        assert_eq!(snapshot.channel_status.len(), 2);
    }

    #[test]
    fn cumulative_viewers_are_excluded_from_total_and_exposed_per_channel() {
        let config = AppConfig {
            channels: vec![
                ChannelConfig {
                    platform: ChannelPlatform::Youtube,
                    identifier: "yt123".to_string(),
                    enabled: true,
                    own_stream: false,
                    overrides: Default::default(),
                },
                ChannelConfig {
                    platform: ChannelPlatform::Niconico,
                    identifier: "lv123".to_string(),
                    enabled: true,
                    own_stream: false,
                    overrides: Default::default(),
                },
            ],
            ..AppConfig::default()
        };
        let mut metadata = HashMap::new();
        metadata.insert(
            metadata_key(Platform::Youtube, "yt123"),
            MetadataState {
                platform: Some(Platform::Youtube),
                channel: "yt123".to_string(),
                concurrent_viewers: Some(10),
                viewers_kind: ViewerCountKind::Concurrent,
                live: Some(true),
                ..MetadataState::default()
            },
        );
        metadata.insert(
            metadata_key(Platform::Niconico, "lv123"),
            MetadataState {
                platform: Some(Platform::Niconico),
                channel: "lv123".to_string(),
                concurrent_viewers: Some(99),
                viewers_kind: ViewerCountKind::Cumulative,
                live: Some(true),
                ..MetadataState::default()
            },
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.viewers, 10);
        let niconico = snapshot
            .channel_status
            .iter()
            .find(|status| status.platform == "niconico")
            .expect("ニコ生のチャンネル状態がある");
        assert_eq!(niconico.viewers, Some(99));
        assert_eq!(niconico.viewers_kind, ViewerCountKind::Cumulative);
    }

    #[test]
    fn live_true_update_sets_channel_status_live() {
        let config = config_with_youtube("@example");
        let mut metadata = HashMap::new();
        merge_metadata_update(
            &mut metadata,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                live: Some(true),
                ..YoutubeMetadataUpdate::default()
            },
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.channel_status.len(), 1);
        assert_eq!(snapshot.channel_status[0].identifier, "@example");
        assert_eq!(snapshot.channel_status[0].live, Some(true));
    }

    #[test]
    fn live_false_update_clears_title_viewers_and_likes() {
        let config = config_with_youtube("@example");
        let mut metadata = HashMap::new();
        merge_metadata_update(
            &mut metadata,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                concurrent_viewers: Some(123),
                viewers_kind: ViewerCountKind::Concurrent,
                likes: Some(45),
                title: Some("Live title".to_string()),
                live: Some(true),
                notice: None,
                reactions_delta: None,
                full_snapshot: true,
            },
        );
        merge_metadata_update(
            &mut metadata,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                live: Some(false),
                ..YoutubeMetadataUpdate::default()
            },
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.viewers, 0);
        assert_eq!(snapshot.likes, 0);
        assert!(snapshot.channel_titles.is_empty());
        assert_eq!(snapshot.channel_status.len(), 1);
        assert_eq!(snapshot.channel_status[0].title, None);
        assert_eq!(snapshot.channel_status[0].viewers, None);
        assert_eq!(snapshot.channel_status[0].live, Some(false));
    }

    #[test]
    fn live_none_update_preserves_existing_live_state() {
        let config = config_with_youtube("@example");
        let mut metadata = HashMap::new();
        merge_metadata_update(
            &mut metadata,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                live: Some(true),
                ..YoutubeMetadataUpdate::default()
            },
        );
        merge_metadata_update(
            &mut metadata,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                concurrent_viewers: Some(9),
                title: Some("Updated title".to_string()),
                live: None,
                ..YoutubeMetadataUpdate::default()
            },
        );

        let snapshot = build_snapshot(0, 0, 0, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(snapshot.channel_status.len(), 1);
        assert_eq!(snapshot.channel_status[0].title.as_deref(), Some("Updated title"));
        assert_eq!(snapshot.channel_status[0].viewers, Some(9));
        assert_eq!(snapshot.channel_status[0].live, Some(true));
    }

    #[test]
    fn reaction_only_update_accumulates_without_touching_metadata() {
        let config = config_with_youtube("@example");
        let mut metadata = HashMap::new();
        let mut reactions = 0u32;

        apply_metadata_update(
            &mut metadata,
            &mut reactions,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "@example".to_string(),
                concurrent_viewers: Some(123),
                viewers_kind: ViewerCountKind::Concurrent,
                likes: Some(45),
                title: Some("Live title".to_string()),
                live: Some(true),
                notice: None,
                reactions_delta: None,
                full_snapshot: true,
            },
            &config,
        );
        apply_metadata_update(
            &mut metadata,
            &mut reactions,
            YoutubeMetadataUpdate {
                platform: Platform::Youtube,
                channel: "video-id".to_string(),
                reactions_delta: Some(7),
                ..YoutubeMetadataUpdate::default()
            },
            &config,
        );

        let snapshot = build_snapshot(0, 0, reactions, 0, &HashMap::new(), &metadata, &config);

        assert_eq!(reactions, 7);
        assert_eq!(metadata.len(), 1);
        assert_eq!(snapshot.reactions, 7);
        assert!(snapshot.reactions_available);
        assert_eq!(snapshot.viewers, 123);
        assert_eq!(snapshot.likes, 45);
        assert_eq!(snapshot.channel_titles.len(), 1);
        assert_eq!(snapshot.channel_titles[0].identifier, "@example");
        assert_eq!(snapshot.channel_status.len(), 1);
        assert_eq!(snapshot.channel_status[0].viewers, Some(123));
    }

    fn config_with_youtube(identifier: &str) -> AppConfig {
        AppConfig {
            channels: vec![ChannelConfig {
                platform: ChannelPlatform::Youtube,
                identifier: identifier.to_string(),
                enabled: true,
                own_stream: false,
                overrides: Default::default(),
            }],
            ..AppConfig::default()
        }
    }

    #[test]
    fn notice_is_set_by_update_and_cleared_by_next_live_state() {
        let mut metadata = HashMap::new();
        let upd = |live, notice: Option<&str>| YoutubeMetadataUpdate {
            platform: Platform::X,
            channel: "@a".to_string(),
            live,
            notice: notice.map(str::to_string),
            ..YoutubeMetadataUpdate::default()
        };
        merge_metadata_update(&mut metadata, upd(Some(false), Some("msg")));
        let key = metadata_key(Platform::X, "@a");
        assert_eq!(metadata[&key].notice.as_deref(), Some("msg"));
        // live を持たない更新(視聴者数ポーリング等)では消えない。
        merge_metadata_update(&mut metadata, upd(None, None));
        assert_eq!(metadata[&key].notice.as_deref(), Some("msg"));
        merge_metadata_update(&mut metadata, upd(Some(true), None));
        assert_eq!(metadata[&key].notice, None);
    }

    #[test]
    fn empty_notice_is_none_and_still_clears_an_earlier_one() {
        let mut metadata = HashMap::new();
        let upd = |notice: &str| YoutubeMetadataUpdate {
            platform: Platform::X,
            channel: "@a".to_string(),
            notice: Some(notice.to_string()),
            ..YoutubeMetadataUpdate::default()
        };
        let key = metadata_key(Platform::X, "@a");
        merge_metadata_update(&mut metadata, upd("msg"));
        assert_eq!(metadata[&key].notice.as_deref(), Some("msg"));
        merge_metadata_update(&mut metadata, upd(""));
        assert_eq!(metadata[&key].notice, None);
        merge_metadata_update(&mut metadata, upd("msg"));
        merge_metadata_update(&mut metadata, upd(" \u{3000}\n"));
        assert_eq!(metadata[&key].notice, None);
        merge_metadata_update(&mut metadata, upd(""));
        assert_eq!(metadata[&key].notice, None);
    }

    #[test]
    fn notice_only_blank_update_clears_through_apply_metadata_update() {
        let mut metadata = HashMap::new();
        let mut reactions = 0;
        let config = AppConfig {
            channels: vec![
                serde_json::from_value(serde_json::json!({"platform": "x", "identifier": "@a"})).unwrap(),
            ],
            ..AppConfig::default()
        };
        let upd = |live, notice: &str| YoutubeMetadataUpdate {
            platform: Platform::X,
            channel: "@a".to_string(),
            live,
            notice: Some(notice.to_string()),
            ..YoutubeMetadataUpdate::default()
        };
        let key = metadata_key(Platform::X, "@a");
        apply_metadata_update(&mut metadata, &mut reactions, upd(Some(false), "待機"), &config);
        assert_eq!(metadata[&key].notice.as_deref(), Some("待機"));
        apply_metadata_update(&mut metadata, &mut reactions, upd(None, " "), &config);
        assert_eq!(metadata[&key].notice, None);
        assert_eq!(metadata[&key].live, Some(false));
    }

    #[test]
    fn twitcasting_scope_and_metadata_keys_are_normalized() {
        let cfg = AppConfig {
            channels: vec![ChannelConfig {
                platform: ChannelPlatform::Twitcasting,
                identifier: "https://twitcasting.tv/Alice_1/movie/9".to_string(),
                enabled: true,
                own_stream: false,
                overrides: Default::default(),
            }],
            ..AppConfig::default()
        };
        assert!(enabled_scope_keys(&cfg).contains("twitcasting:alice_1"));
        assert!(enabled_metadata_keys(&cfg).contains(&metadata_key(Platform::Twitcasting, "alice_1")));
        assert_eq!(platform_key(Platform::Twitcasting), "twitcasting");
    }
}
