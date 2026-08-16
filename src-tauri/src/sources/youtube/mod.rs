//! YouTube Live Chat Source(非公式 InnerTube 経由)。
//!
//! SPEC §4.2 の通り、仕様変更耐性を最重要とする:
//! - `innertube.rs`: 初期HTMLから API_KEY/clientVersion/continuation を抽出し、
//!   `youtubei/v1/live_chat/get_live_chat` をポーリング。
//! - `parser.rs`: 寛容パース。固い struct deserialize はせず `serde_json::Value` を
//!   パス探索し、欠落しても None で安全に劣化。解析不能アクションはログへ追記。

pub mod innertube;
pub mod live_resolve;
pub mod metadata;
mod official_stream;
pub mod parser;

use std::collections::{HashMap, HashSet, VecDeque};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use std::time::{Duration, Instant};

use super::{Backoff, Source};
use crate::config::YoutubeOverrides;
use crate::model::{ChatMessage, Platform, YoutubeReaction};
use crate::stats::YoutubeMetadataUpdate;

use innertube::InnerTubeClient;

const ACTIVE_POLL_MIN_MS: u64 = 700;
const ACTIVE_POLL_MAX_MS: u64 = 1500;
const QUIET_POLL_MIN_MS: u64 = 1000;
const QUIET_POLL_MAX_MS: u64 = 1500;
const DEFAULT_POLL_MS: u64 = 1000;
const RECENT_MESSAGE_IDS: usize = 8192;

/// 公式streamListからInnerTubeへ切り替わった際の履歴重複を抑止する。
/// 長時間配信で無制限に増えないよう直近IDだけを保持する。
pub(super) struct RecentMessageIds {
    ids: HashSet<String>,
    order: VecDeque<String>,
    gift_fingerprints: HashMap<String, String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum MessageDisposition {
    New,
    GiftUpdate,
    Duplicate,
}

impl RecentMessageIds {
    fn new() -> Self {
        Self {
            ids: HashSet::with_capacity(RECENT_MESSAGE_IDS),
            order: VecDeque::with_capacity(RECENT_MESSAGE_IDS),
            gift_fingerprints: HashMap::new(),
        }
    }

    pub(super) fn accept(&mut self, message: &ChatMessage) -> MessageDisposition {
        if self.ids.contains(&message.id) {
            if message.kind != crate::model::MessageKind::Gift {
                return MessageDisposition::Duplicate;
            }

            let fingerprint = gift_fingerprint(message);
            if self.gift_fingerprints.get(&message.id) == Some(&fingerprint) {
                return MessageDisposition::Duplicate;
            }
            self.gift_fingerprints
                .insert(message.id.clone(), fingerprint);
            return MessageDisposition::GiftUpdate;
        }

        let owned = message.id.clone();
        self.ids.insert(owned.clone());
        self.order.push_back(owned);
        if message.kind == crate::model::MessageKind::Gift {
            self.gift_fingerprints
                .insert(message.id.clone(), gift_fingerprint(message));
        }
        if self.order.len() > RECENT_MESSAGE_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
                self.gift_fingerprints.remove(&oldest);
            }
        }
        MessageDisposition::New
    }
}

fn gift_fingerprint(message: &ChatMessage) -> String {
    format!(
        "{}\0{}\0{}",
        message.author.id,
        message.author.name,
        message.plain_text()
    )
}

/// YouTube ライブ1配信を購読する Source。
pub struct YoutubeSource {
    /// videoId もしくは配信URL(URL からは videoId を抽出する)。
    video_input: String,
    overrides: YoutubeOverrides,
    official_api_key: String,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    reaction_tx: Option<mpsc::Sender<Vec<YoutubeReaction>>>,
}

impl YoutubeSource {
    pub fn new(
        video_input: String,
        overrides: YoutubeOverrides,
        official_api_key: String,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
        reaction_tx: Option<mpsc::Sender<Vec<YoutubeReaction>>>,
    ) -> Self {
        YoutubeSource {
            video_input,
            overrides,
            official_api_key,
            metadata_tx,
            reaction_tx,
        }
    }

    /// 入力(URL or 生 videoId)から videoId を取り出す。
    fn video_id(&self) -> String {
        extract_video_id(&self.video_input)
    }
}

impl Source for YoutubeSource {
    fn name(&self) -> String {
        format!("youtube:{}", self.video_id())
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let video_id = self.video_id();
            let mut seen = RecentMessageIds::new();

            if !self.official_api_key.trim().is_empty() {
                tracing::info!("youtube:{video_id} 公式streamListで低遅延接続を開始");
                let official_result = if self.metadata_tx.is_some() || self.reaction_tx.is_some() {
                    // 公開streamListには匿名リアクションが含まれないため、コメント受信とは
                    // 独立したInnerTube sidecarを同時に回す。公式側が終わればfutureをdropし、
                    // 通常のInnerTube fallbackへ渡して二重pollを残さない。
                    let official = official_stream::stream_live_chat(
                        &video_id,
                        self.official_api_key.trim(),
                        &tx,
                        &cancel,
                        &mut seen,
                    );
                    let reaction_sidecar = self.run_reaction_sidecar(&video_id, &cancel);
                    tokio::pin!(official);
                    tokio::pin!(reaction_sidecar);
                    tokio::select! {
                        result = &mut official => Some(result),
                        _ = &mut reaction_sidecar => None,
                    }
                } else {
                    Some(
                        official_stream::stream_live_chat(
                            &video_id,
                            self.official_api_key.trim(),
                            &tx,
                            &cancel,
                            &mut seen,
                        )
                        .await,
                    )
                };
                let Some(official_result) = official_result else {
                    return Ok(());
                };
                match official_result {
                    Ok(()) if cancel.is_cancelled() => return Ok(()),
                    Ok(()) => tracing::warn!(
                        "youtube:{video_id} 公式streamListが終了したためInnerTubeへ切替"
                    ),
                    Err(e) => tracing::warn!(
                        "youtube:{video_id} 公式streamList接続失敗: {e:#}; InnerTubeへ自動切替"
                    ),
                }
            } else {
                tracing::info!("youtube:{video_id} APIキー未設定のためInnerTubeで接続");
            }

            let mut backoff = Backoff::new();

            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                let mut no_progress = false;
                match self.poll_session(&video_id, &tx, &cancel, &mut seen).await {
                    Ok(made_progress) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        // 配信終了/continuation 枯渇など。少し待って再ブートストラップ。
                        if made_progress {
                            backoff.reset();
                        } else {
                            no_progress = true;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("youtube:{video_id} ポーリングエラー: {e:#}");
                    }
                }

                let delay = if no_progress {
                    backoff.next_delay().max(Duration::from_secs(3))
                } else {
                    backoff.next_delay()
                };
                tracing::info!("youtube:{video_id} {}ms 後に再接続", delay.as_millis());
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }
}

impl YoutubeSource {
    /// 1セッション: ブートストラップ → continuation を辿りつつポーリング。
    async fn poll_session(
        &self,
        video_id: &str,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
        seen: &mut RecentMessageIds,
    ) -> anyhow::Result<bool> {
        let client = InnerTubeClient::new(self.overrides.clone())?;
        let session_started = Instant::now();
        let mut received_message = false;

        // 初期HTMLから API_KEY / clientVersion / 初期 continuation を取得。
        let mut session = client.bootstrap(video_id).await?;
        tracing::info!(
            "youtube:{video_id} bootstrap 完了 (clientVersion={}, continuation 取得={})",
            session.client_version,
            !session.continuation.is_empty()
        );

        let mut first_poll = true;
        loop {
            if cancel.is_cancelled() {
                return Ok(session_made_progress(received_message, session_started));
            }
            if session.continuation.is_empty() {
                // これ以上辿れない(配信終了 or 抽出失敗)。セッション終了。
                return Ok(session_made_progress(received_message, session_started));
            }

            let resp = tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(session_made_progress(received_message, session_started));
                }
                resp = client.get_live_chat(&session) => resp?,
            };

            // 寛容パース。actions を ChatMessage 群へ。
            // 抽出パスは overrides.paths で差し替え可能(欠落時は既定)。
            let actions = parser::extract_actions(&resp, &self.overrides.paths);
            let reaction_update =
                parse_reaction_update(&resp, &self.overrides.paths, video_id, first_poll);
            let reactions_delta = reaction_update.delta;
            let had_activity = !actions.is_empty() || reactions_delta > 0;
            self.publish_reaction_update(video_id, reaction_update);
            for action in &actions {
                if let Some(mut msg) = parser::parse_action(action, video_id) {
                    received_message = true;
                    msg.skip_tts = first_poll;
                    // raw は通常 None。デバッグ目的で残したい場合のみ付与する。
                    msg.raw = None;
                    match seen.accept(&msg) {
                        MessageDisposition::New => {
                            let _ = tx.send(msg);
                        }
                        MessageDisposition::GiftUpdate => {
                            // コンボ更新は同じ行へ上書きする。更新のたびに読み上げない。
                            msg.skip_tts = true;
                            let _ = tx.send(msg);
                        }
                        MessageDisposition::Duplicate => {}
                    }
                } else if parser::is_chat_item_action(action) {
                    // 解析できなかった addChatItemAction はログへ1行追記。
                    parser::log_unparsed(action);
                }
            }

            // 次の continuation と timeoutMs を取得。
            let (next_cont, timeout_ms) = parser::next_continuation(&resp, &self.overrides.paths);
            match next_cont {
                Some(c) if !c.is_empty() => session.continuation = c,
                _ => {
                    // 次が取れない=ライブ終了等。セッションを閉じる。
                    tracing::debug!(
                        "youtube:{video_id} continuation なし (actions={}, reactions_delta={reactions_delta})",
                        actions.len()
                    );
                    return Ok(session_made_progress(received_message, session_started));
                }
            }
            first_poll = false;

            // YouTube の timeoutMs はライブチャットだと数秒〜10秒と長めで、その間に届いた
            // コメントが次ポールまでバッファされ「まとめてドサッと表示」=遅延に感じる。
            // 静かな時間帯も長い timeoutMs をそのまま使うと、待機中に投稿されたコメントが
            // 次ポールまで最大10秒滞留する。低遅延を優先して上限1.5秒を維持しつつ、
            // 空ポール時の下限だけ1秒にして過剰な連打を避ける。
            let wait = poll_wait_ms(timeout_ms, had_activity);
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(session_made_progress(received_message, session_started));
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
            }
        }
    }

    /// 公式streamListと並行する、リアクション専用の再接続ループ。
    async fn run_reaction_sidecar(&self, video_id: &str, cancel: &CancellationToken) {
        let mut backoff = Backoff::new();

        loop {
            if cancel.is_cancelled() {
                return;
            }

            let mut no_progress = false;
            match self.poll_reaction_session(video_id, cancel).await {
                Ok(made_progress) => {
                    if cancel.is_cancelled() {
                        return;
                    }
                    if made_progress {
                        backoff.reset();
                    } else {
                        no_progress = true;
                    }
                }
                Err(e) => {
                    tracing::warn!("youtube:{video_id} reaction sidecarエラー: {e:#}");
                }
            }

            let delay = if no_progress {
                backoff.next_delay().max(Duration::from_secs(3))
            } else {
                backoff.next_delay()
            };
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(delay) => {}
            }
        }
    }

    /// reaction-only 1セッション。チャットactionsは読まず、専用バッチとGoalsだけ更新する。
    async fn poll_reaction_session(
        &self,
        video_id: &str,
        cancel: &CancellationToken,
    ) -> anyhow::Result<bool> {
        let client = InnerTubeClient::new(self.overrides.clone())?;
        let session_started = Instant::now();
        let mut received_reaction = false;
        let mut session = client.bootstrap(video_id).await?;
        let mut first_poll = true;

        loop {
            if cancel.is_cancelled() || session.continuation.is_empty() {
                return Ok(session_made_progress(received_reaction, session_started));
            }

            let response = tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(session_made_progress(received_reaction, session_started));
                }
                response = client.get_live_chat(&session) => response?,
            };
            let update =
                parse_reaction_update(&response, &self.overrides.paths, video_id, first_poll);
            let had_activity = update.delta > 0;
            received_reaction |= had_activity;
            self.publish_reaction_update(video_id, update);

            let (next_continuation, timeout_ms) =
                parser::next_continuation(&response, &self.overrides.paths);
            match next_continuation {
                Some(continuation) if !continuation.is_empty() => {
                    session.continuation = continuation;
                }
                _ => {
                    return Ok(session_made_progress(received_reaction, session_started));
                }
            }
            first_poll = false;

            let wait = poll_wait_ms(timeout_ms, had_activity);
            tokio::select! {
                _ = cancel.cancelled() => {
                    return Ok(session_made_progress(received_reaction, session_started));
                }
                _ = tokio::time::sleep(Duration::from_millis(wait)) => {}
            }
        }
    }

    fn publish_reaction_update(&self, video_id: &str, update: ReactionUpdate) {
        if update.delta > 0 {
            if let Some(metadata_tx) = &self.metadata_tx {
                if let Err(e) = metadata_tx.try_send(YoutubeMetadataUpdate {
                    platform: Platform::Youtube,
                    channel: video_id.to_string(),
                    reactions_delta: Some(update.delta),
                    ..YoutubeMetadataUpdate::default()
                }) {
                    tracing::debug!("youtube:{video_id} リアクション統計をdrop: {e}");
                }
            }
        }
        if !update.animation_batch.is_empty() {
            if let Some(reaction_tx) = &self.reaction_tx {
                if let Err(e) = reaction_tx.try_send(update.animation_batch) {
                    tracing::debug!("youtube:{video_id} リアクション演出をdrop: {e}");
                }
            }
        }
    }
}

#[derive(Debug)]
struct ReactionUpdate {
    delta: u32,
    animation_batch: Vec<YoutubeReaction>,
}

fn parse_reaction_update(
    response: &serde_json::Value,
    paths: &HashMap<String, String>,
    channel: &str,
    first_poll: bool,
) -> ReactionUpdate {
    if first_poll {
        // bootstrap直後は直前のrolling windowが再送される。再接続のたびにGoalsへ
        // 二重加算せず、画面にも過去分を再生しない。
        return ReactionUpdate {
            delta: 0,
            animation_batch: Vec::new(),
        };
    }
    let parsed = parser::extract_reaction_counts(response, paths, channel);
    ReactionUpdate {
        delta: parsed.total_delta,
        animation_batch: parsed.by_emoji,
    }
}

fn session_made_progress(received_message: bool, started_at: Instant) -> bool {
    const MEANINGFUL_SESSION_SECS: u64 = 30;
    received_message || started_at.elapsed() >= Duration::from_secs(MEANINGFUL_SESSION_SECS)
}

fn poll_wait_ms(timeout_ms: Option<u64>, had_activity: bool) -> u64 {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_POLL_MS);
    if had_activity {
        timeout_ms.clamp(ACTIVE_POLL_MIN_MS, ACTIVE_POLL_MAX_MS)
    } else {
        timeout_ms.clamp(QUIET_POLL_MIN_MS, QUIET_POLL_MAX_MS)
    }
}

/// 配信URL もしくは生の videoId から videoId を抽出する。
///
/// 対応: `https://www.youtube.com/watch?v=ID`, `https://youtu.be/ID`,
/// `https://www.youtube.com/live/ID`, それ以外はそのまま videoId とみなす。
pub fn extract_video_id(input: &str) -> String {
    let s = input.trim();

    // youtu.be/<id>
    if let Some(idx) = s.find("youtu.be/") {
        let tail = &s[idx + "youtu.be/".len()..];
        return cut_id(tail);
    }
    // /live/<id>
    if let Some(idx) = s.find("/live/") {
        let tail = &s[idx + "/live/".len()..];
        return cut_id(tail);
    }
    // watch?v=<id>
    if let Some(idx) = s.find("v=") {
        let tail = &s[idx + 2..];
        return cut_id(tail);
    }

    // URL でなければそのまま。
    cut_id(s)
}

/// YouTube identifier がチャンネル指定かどうかを判定する。
///
/// 11桁 videoId / watch URL / youtu.be / /live/<videoId> は従来の配信単体扱いのまま。
pub fn is_channel_identifier(input: &str) -> bool {
    live_resolve::parse_channel_identifier(input).is_some()
}

pub(crate) fn is_video_id(value: &str) -> bool {
    let s = value.trim();
    s.len() == 11
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// クエリ/フラグメント/スラッシュ手前までを videoId として切り出す。
fn cut_id(s: &str) -> String {
    s.chars()
        .take_while(|&c| c != '&' && c != '?' && c != '/' && c != '#')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn classifies_direct_video_ids_as_video_mode() {
        assert!(!is_channel_identifier("dQw4w9WgXcQ"));
        assert!(!is_channel_identifier(
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ"
        ));
        assert!(!is_channel_identifier("https://youtu.be/dQw4w9WgXcQ?t=1"));
        assert!(!is_channel_identifier(
            "https://www.youtube.com/live/dQw4w9WgXcQ"
        ));
    }

    #[test]
    fn classifies_handles_and_channel_urls_as_channel_mode() {
        assert!(is_channel_identifier("@example_handle"));
        assert!(is_channel_identifier("youtube.com/@example_handle/live"));
        assert!(is_channel_identifier(
            "https://www.youtube.com/channel/UC1234567890123456789012"
        ));
        assert!(is_channel_identifier(
            "https://www.youtube.com/channel/UC1234567890123456789012/live"
        ));
    }

    #[test]
    fn poll_wait_keeps_low_latency_after_activity() {
        assert_eq!(poll_wait_ms(Some(500), true), 700);
        assert_eq!(poll_wait_ms(Some(1200), true), 1200);
        assert_eq!(poll_wait_ms(Some(10_000), true), 1500);
    }

    #[test]
    fn poll_wait_caps_quiet_streams_for_low_latency() {
        assert_eq!(poll_wait_ms(None, false), 1000);
        assert_eq!(poll_wait_ms(Some(500), false), 1000);
        assert_eq!(poll_wait_ms(Some(5000), false), 1500);
        assert_eq!(poll_wait_ms(Some(30_000), false), 1500);
    }

    #[test]
    fn first_reaction_poll_suppresses_rolling_window_for_stats_and_animation() {
        let payload = json!({
            "frameworkUpdates": {
                "entityBatchUpdate": {
                    "mutations": [{
                        "payload": {
                            "emojiFountainDataEntity": {
                                "reactionBuckets": [{
                                    "reactions": [
                                        { "key": "♥️", "value": 2 },
                                        { "key": "🎉", "value": 1 }
                                    ]
                                }]
                            }
                        }
                    }]
                }
            }
        });
        let paths = std::collections::HashMap::new();

        let initial = parse_reaction_update(&payload, &paths, "video-1", true);
        assert_eq!(initial.delta, 0);
        assert!(initial.animation_batch.is_empty());

        let live = parse_reaction_update(&payload, &paths, "video-1", false);
        assert_eq!(live.delta, 3);
        assert_eq!(live.animation_batch.len(), 2);
        assert_eq!(live.animation_batch[0].channel, "video-1");
        assert_eq!(live.animation_batch[0].emoji, "♥️");
        assert_eq!(live.animation_batch[0].count, 2);
    }
}
