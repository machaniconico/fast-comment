use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::model::ChatMessage;

/// 行専用の送信端を作り、発生元のキーを付けて受信順にパイプラインへ流す。
pub fn spawn_origin_relay(
    origin_key: String,
    upstream: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
) -> broadcast::Sender<ChatMessage> {
    let (tx, mut rx) = broadcast::channel::<ChatMessage>(super::SOURCE_CAPACITY);

    // 呼び出し元の Tokio ランタイムで実行し、Tauri の同期 setup からはアプリのランタイムを使う。
    let runtime = tokio::runtime::Handle::try_current()
        .unwrap_or_else(|_| tauri::async_runtime::handle().inner().clone());
    runtime.spawn(async move {
        let mut skipped_total = 0u64;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                result = rx.recv() => match result {
                    Ok(mut msg) => {
                        msg.route.origin_key = Some(origin_key.clone());
                        let _ = upstream.send(msg);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        skipped_total = skipped_total.saturating_add(n);
                        tracing::debug!(skipped = n, skipped_total, "Source relay lag");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });

    tx
}
