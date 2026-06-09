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
pub mod parser;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::config::YoutubeOverrides;
use crate::model::ChatMessage;

use innertube::InnerTubeClient;

/// YouTube ライブ1配信を購読する Source。
pub struct YoutubeSource {
    /// videoId もしくは配信URL(URL からは videoId を抽出する)。
    video_input: String,
    overrides: YoutubeOverrides,
}

impl YoutubeSource {
    pub fn new(video_input: String, overrides: YoutubeOverrides) -> Self {
        YoutubeSource {
            video_input,
            overrides,
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
            let mut backoff = Backoff::new();

            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                match self.poll_session(&video_id, &tx, &cancel).await {
                    Ok(()) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        // 配信終了/continuation 枯渇など。少し待って再ブートストラップ。
                        backoff.reset();
                    }
                    Err(e) => {
                        tracing::warn!("youtube:{video_id} ポーリングエラー: {e:#}");
                    }
                }

                let delay = backoff.next_delay();
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
    ) -> anyhow::Result<()> {
        let client = InnerTubeClient::new(self.overrides.clone())?;

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
                return Ok(());
            }
            if session.continuation.is_empty() {
                // これ以上辿れない(配信終了 or 抽出失敗)。セッション終了。
                return Ok(());
            }

            let resp = client.get_live_chat(&session).await?;

            // 寛容パース。actions を ChatMessage 群へ。
            // 抽出パスは overrides.paths で差し替え可能(欠落時は既定)。
            let actions = parser::extract_actions(&resp, &self.overrides.paths);
            for action in &actions {
                if let Some(mut msg) = parser::parse_action(action, video_id) {
                    msg.skip_tts = first_poll;
                    // raw は通常 None。デバッグ目的で残したい場合のみ付与する。
                    msg.raw = None;
                    let _ = tx.send(msg);
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
                    return Ok(());
                }
            }
            first_poll = false;

            // YouTube の timeoutMs はライブチャットだと数秒〜10秒と長めで、その間に届いた
            // コメントが次ポールまでバッファされ「まとめてドサッと表示」=遅延に感じる。
            // 低遅延重視(SPEC: わんコメより低遅延)のため上限を短く抑えてこまめに取得する。
            // 下限はレート制限/空ポール回避のため 700ms。欠落時の既定も 1000ms。
            let wait = timeout_ms.unwrap_or(1000).clamp(700, 1500);
            tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep(std::time::Duration::from_millis(wait)) => {}
            }
        }
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
}
