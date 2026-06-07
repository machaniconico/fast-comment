//! Goals overlay 用の統計集約。
//!
//! chat bus と YouTube メタデータを集約し、最新 `StatsSnapshot` を watch channel で
//! UI/OBS へ fan-out する。

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{interval, MissedTickBehavior};
use tokio_util::sync::CancellationToken;

use crate::config::{AppConfig, ChannelPlatform};
use crate::model::{ChatMessage, Platform};
use crate::sources::youtube::extract_video_id;

/// 接続中チャンネルに表示する補助タイトル。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ChannelTitle {
    pub platform: String,
    pub identifier: String,
    pub title: String,
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
    #[serde(default)]
    pub channel_titles: Vec<ChannelTitle>,
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

/// 設定由来の目標値。0 は該当ゲージ非表示。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GoalsSnapshot {
    pub comments: u32,
    pub viewers: u32,
    pub likes: u32,
}

/// YouTube メタデータ poller から集約タスクへ渡す更新。
#[derive(Debug, Clone, Default)]
pub struct YoutubeMetadataUpdate {
    pub channel: String,
    pub concurrent_viewers: Option<u32>,
    pub likes: Option<u32>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct YoutubeMetadataState {
    concurrent_viewers: Option<u32>,
    likes: Option<u32>,
    title: Option<String>,
}

/// watch::Sender を使う統計集約タスクを起動する。
///
/// `bus_rx` は chat bus の購読、`metadata_rx` は YouTube poller からの値、`config_rx`
/// は設定変更通知。どれも await 中に MutexGuard を保持しない形で受け取る。
pub fn spawn_stats_aggregator(
    mut bus_rx: broadcast::Receiver<ChatMessage>,
    stats_tx: watch::Sender<StatsSnapshot>,
    mut config_rx: watch::Receiver<AppConfig>,
    mut metadata_rx: mpsc::Receiver<YoutubeMetadataUpdate>,
    app: AppHandle,
    cancel: CancellationToken,
) -> tauri::async_runtime::JoinHandle<()> {
    tauri::async_runtime::spawn(async move {
        let mut comments = 0u32;
        let mut viewers_max = 0u32;
        let mut viewer_scopes: HashMap<String, HashSet<String>> = HashMap::new();
        let mut youtube_metadata: HashMap<String, YoutubeMetadataState> = HashMap::new();
        let mut config = config_rx.borrow().clone();

        let mut ticker = interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);

        let mut snapshot = build_snapshot(
            comments,
            viewers_max,
            &viewer_scopes,
            &youtube_metadata,
            &config,
        );
        publish_snapshot(&stats_tx, &app, &snapshot);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = bus_rx.recv() => {
                    match recv {
                        Ok(msg) => {
                            comments = comments.saturating_add(1);
                            record_unique_viewer(&mut viewer_scopes, &msg);
                            snapshot = build_snapshot(
                                comments,
                                viewers_max,
                                &viewer_scopes,
                                &youtube_metadata,
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
                    let entry = youtube_metadata
                        .entry(update.channel)
                        .or_insert_with(YoutubeMetadataState::default);
                    if let Some(v) = update.concurrent_viewers {
                        entry.concurrent_viewers = Some(v);
                    }
                    if let Some(v) = update.likes {
                        entry.likes = Some(v);
                    }
                    if let Some(title) = update.title {
                        entry.title = Some(title);
                    }
                    retain_enabled_youtube_metadata(&mut youtube_metadata, &config);
                    snapshot = build_snapshot(
                        comments,
                        viewers_max,
                        &viewer_scopes,
                        &youtube_metadata,
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
                    retain_enabled_youtube_metadata(&mut youtube_metadata, &config);
                    snapshot = build_snapshot(
                        comments,
                        viewers_max,
                        &viewer_scopes,
                        &youtube_metadata,
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
                        &viewer_scopes,
                        &youtube_metadata,
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
    viewer_scopes: &HashMap<String, HashSet<String>>,
    youtube_metadata: &HashMap<String, YoutubeMetadataState>,
    config: &AppConfig,
) -> StatsSnapshot {
    let unique_count = viewer_scopes
        .values()
        .map(|set| set.len() as u32)
        .fold(0u32, u32::saturating_add);
    let concurrent_total = youtube_metadata
        .values()
        .filter_map(|m| m.concurrent_viewers)
        .fold(0u32, u32::saturating_add);
    let viewers = resolve_viewers(concurrent_total, unique_count);
    let likes = youtube_metadata
        .values()
        .filter_map(|m| m.likes)
        .fold(0u32, u32::saturating_add);
    let mut channel_titles = youtube_metadata
        .iter()
        .filter_map(|(channel, metadata)| {
            metadata.title.as_ref().map(|title| ChannelTitle {
                platform: "youtube".to_string(),
                identifier: channel.clone(),
                title: title.clone(),
            })
        })
        .collect::<Vec<_>>();
    channel_titles.sort_by(|a, b| a.identifier.cmp(&b.identifier));

    StatsSnapshot {
        comments,
        viewers,
        viewers_max: viewers_max.max(viewers),
        likes,
        likes_available: has_enabled_youtube(config),
        channel_titles,
        goals: goals_from_config(config),
        updated_at: 0,
    }
}

fn record_unique_viewer(
    viewer_scopes: &mut HashMap<String, HashSet<String>>,
    msg: &ChatMessage,
) {
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

fn platform_key(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
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
    viewer_scopes.retain(|scope, _| enabled.contains(scope));
}

fn enabled_youtube_channels(config: &AppConfig) -> HashSet<String> {
    config
        .channels
        .iter()
        .filter(|ch| ch.enabled && ch.platform == ChannelPlatform::Youtube)
        .map(|ch| extract_video_id(&ch.identifier))
        .collect()
}

fn retain_enabled_youtube_metadata(
    metadata: &mut HashMap<String, YoutubeMetadataState>,
    config: &AppConfig,
) {
    let enabled = enabled_youtube_channels(config);
    if enabled.is_empty() {
        metadata.clear();
        return;
    }
    metadata.retain(|channel, _| enabled.contains(channel));
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
    }
}

fn resolve_viewers(concurrent_total: u32, unique_count: u32) -> u32 {
    if concurrent_total > 0 {
        concurrent_total
    } else {
        unique_count
    }
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
    use crate::model::{Author, Fragment, MessageKind, Roles};

    fn msg(platform: Platform, channel: &str, id: &str, name: &str) -> ChatMessage {
        ChatMessage {
            id: ChatMessage::new_id(),
            platform,
            channel: channel.to_string(),
            author: Author {
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
        }
    }

    #[test]
    fn goal_percent_handles_zero_and_over_goal() {
        assert_eq!(goal_percent(50, 0), 0);
        assert_eq!(goal_percent(25, 100), 25);
        assert_eq!(goal_percent(150, 100), 150);
    }

    #[test]
    fn viewers_prefer_concurrent_when_available() {
        assert_eq!(resolve_viewers(42, 7), 42);
        assert_eq!(resolve_viewers(0, 7), 7);
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
}
