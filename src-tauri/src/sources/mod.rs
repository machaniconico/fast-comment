//! Source 層 — 各プラットフォームの接続元を抽象化する trait と、
//! 設定に基づいて Source 群を起動するマネージャ。
//!
//! 各 Source は `run()` の中で自前の指数バックオフ再接続を持ち、
//! 正規化済み `ChatMessage` を `broadcast::Sender` へ流す。

pub mod twitch;
pub mod youtube;

use std::time::Duration;

use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::config::{ChannelConfig, ChannelPlatform};
use crate::model::ChatMessage;

/// 接続元の共通インタフェース。
///
/// `run` は与えられた `tx` にメッセージを流し続け、`cancel` 発火で速やかに終了する。
/// 再接続(指数バックオフ)は各実装の責務。正常な cancel では `Ok(())` を返す。
pub trait Source: Send + Sync {
    /// このソースの人間可読な名前(ログ用)。
    fn name(&self) -> String;

    /// メッセージ受信ループを回す。`cancel` で終了。
    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>>;
}

/// 指数バックオフの状態。各 Source の再接続ループで共有して使うヘルパ。
pub struct Backoff {
    current: Duration,
    min: Duration,
    max: Duration,
}

impl Backoff {
    /// 既定: 1秒から開始し最大30秒まで倍々で伸ばす。
    pub fn new() -> Self {
        Backoff {
            current: Duration::from_secs(1),
            min: Duration::from_secs(1),
            max: Duration::from_secs(30),
        }
    }

    /// 接続成功時に呼び、待機時間を最小へリセットする。
    pub fn reset(&mut self) {
        self.current = self.min;
    }

    /// 次の待機時間を返し、内部値を2倍(上限あり)に更新する。
    pub fn next_delay(&mut self) -> Duration {
        let d = self.current;
        self.current = (self.current * 2).min(self.max);
        d
    }
}

impl Default for Backoff {
    fn default() -> Self {
        Self::new()
    }
}

/// 設定された各チャンネルに対して Source を起動するマネージャ。
///
/// 起動したタスクは `CancellationToken` の子トークンで管理し、
/// `cancel` 一括で全 Source を停止できる。
pub struct SourceManager {
    tx: broadcast::Sender<ChatMessage>,
    /// YouTube overrides 等を渡すための設定スナップショット。
    youtube_overrides: crate::config::YoutubeOverrides,
}

impl SourceManager {
    pub fn new(
        tx: broadcast::Sender<ChatMessage>,
        youtube_overrides: crate::config::YoutubeOverrides,
    ) -> Self {
        SourceManager {
            tx,
            youtube_overrides,
        }
    }

    /// 1チャンネル分の Source を起動し、停止用トークンを返す。
    ///
    /// 呼び出し側は返ったトークンを保持し、チャンネル削除時に `cancel()` する。
    pub fn spawn_channel(&self, ch: &ChannelConfig) -> CancellationToken {
        let cancel = CancellationToken::new();
        if !ch.enabled {
            return cancel;
        }

        let tx = self.tx.clone();
        let child = cancel.clone();
        let identifier = ch.identifier.clone();
        let overrides = self.youtube_overrides.clone();

        match ch.platform {
            ChannelPlatform::Twitch => {
                let src = twitch::TwitchSource::new(identifier);
                tauri::async_runtime::spawn(async move {
                    run_with_logging(&src, tx, child).await;
                });
            }
            ChannelPlatform::Youtube => {
                let src = youtube::YoutubeSource::new(identifier, overrides);
                tauri::async_runtime::spawn(async move {
                    run_with_logging(&src, tx, child).await;
                });
            }
        }

        cancel
    }
}

/// Source を実行し、終了理由をログする小ヘルパ。
async fn run_with_logging<S: Source>(
    src: &S,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
) {
    let name = src.name();
    tracing::info!("Source 起動: {name}");
    match src.run(tx, cancel).await {
        Ok(()) => tracing::info!("Source 正常終了: {name}"),
        Err(e) => tracing::error!("Source 異常終了: {name}: {e:#}"),
    }
}
