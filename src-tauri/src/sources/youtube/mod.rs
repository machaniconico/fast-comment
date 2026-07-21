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

use std::collections::{HashSet, VecDeque};
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use std::time::{Duration, Instant};

use super::{Backoff, Source};
use crate::config::YoutubeOverrides;
use crate::model::{ChatMessage, Platform};
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
}

impl RecentMessageIds {
    fn new() -> Self {
        Self {
            ids: HashSet::with_capacity(RECENT_MESSAGE_IDS),
            order: VecDeque::with_capacity(RECENT_MESSAGE_IDS),
        }
    }

    pub(super) fn insert(&mut self, id: &str) -> bool {
        if self.ids.contains(id) {
            return false;
        }
        let owned = id.to_string();
        self.ids.insert(owned.clone());
        self.order.push_back(owned);
        if self.order.len() > RECENT_MESSAGE_IDS {
            if let Some(oldest) = self.order.pop_front() {
                self.ids.remove(&oldest);
            }
        }
        true
    }
}

/// YouTube ライブ1配信を購読する Source。
pub struct YoutubeSource {
    /// videoId もしくは配信URL(URL からは videoId を抽出する)。
    video_input: String,
    overrides: YoutubeOverrides,
    official_api_key: String,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl YoutubeSource {
    pub fn new(
        video_input: String,
        overrides: YoutubeOverrides,
        official_api_key: String,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        YoutubeSource {
            video_input,
            overrides,
            official_api_key,
            metadata_tx,
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
                match official_stream::stream_live_chat(
                    &video_id,
                    self.official_api_key.trim(),
                    &tx,
                    &cancel,
                    &mut seen,
                )
                .await
                {
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
            let reactions_delta = parser::extract_reactions_delta(&resp, &self.overrides.paths);
            let had_activity = !actions.is_empty() || reactions_delta > 0;
            if reactions_delta > 0 {
                if let Some(metadata_tx) = &self.metadata_tx {
                    let _ = metadata_tx.try_send(YoutubeMetadataUpdate {
                        platform: Platform::Youtube,
                        channel: video_id.to_string(),
                        reactions_delta: Some(reactions_delta),
                        ..YoutubeMetadataUpdate::default()
                    });
                }
            }
            for action in &actions {
                if let Some(mut msg) = parser::parse_action(action, video_id) {
                    received_message = true;
                    msg.skip_tts = first_poll;
                    // raw は通常 None。デバッグ目的で残したい場合のみ付与する。
                    msg.raw = None;
                    if seen.insert(&msg.id) {
                        let _ = tx.send(msg);
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
}
