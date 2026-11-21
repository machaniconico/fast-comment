use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::model::ChatMessage;

/// 行ごとの送信端の容量。上流(SOURCE_CAPACITY)より小さくし、行数に比例するメモリを抑える。
const ROW_CAPACITY: usize = 512;

/// 行専用の送信端を作り、発生元のキーを付けて受信順にパイプラインへ流す。
pub fn spawn_origin_relay(
    origin_key: String,
    upstream: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
) -> broadcast::Sender<ChatMessage> {
    let (tx, mut rx) = broadcast::channel::<ChatMessage>(ROW_CAPACITY);

    // 呼び出し元の Tokio ランタイムで実行し、Tauri の同期 setup からはアプリのランタイムを使う。
    let runtime = tokio::runtime::Handle::try_current()
        .unwrap_or_else(|_| tauri::async_runtime::handle().inner().clone());
    runtime.spawn(async move {
        let mut skipped_total = 0u64;
        loop {
            tokio::select! {
                biased;
                // 行が止まると rx ごと捨てるので、未中継のメッセージは破棄される。
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(flavor = "current_thread")]
    async fn row_buffer_holds_row_capacity_before_lagging() {
        let (upstream, mut up_rx) = broadcast::channel::<ChatMessage>(super::super::SOURCE_CAPACITY);
        let cancel = CancellationToken::new();
        let tx = spawn_origin_relay("twitch:row".into(), upstream.clone(), cancel.clone());
        let base: ChatMessage = serde_json::from_value(serde_json::json!({
            "id": "0",
            "platform": "twitch",
            "channel": "row",
            "author": { "id": "u1", "name": "viewer" },
            "fragments": [],
            "kind": "normal",
            "timestampMs": 1
        }))
        .unwrap();
        // relay が動く前に容量 + 10 件送ると、古い 10 件だけ落ちる。
        for i in 0..ROW_CAPACITY + 10 {
            let mut msg = base.clone();
            msg.id = i.to_string();
            tx.send(msg).unwrap();
        }
        let mut got = Vec::new();
        while got.len() < ROW_CAPACITY {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), up_rx.recv())
                .await
                .expect("relay が中継する")
                .unwrap();
            got.push(msg.id);
        }
        assert_eq!(got.first().map(String::as_str), Some("10"));
        assert_eq!(got.last().cloned(), Some((ROW_CAPACITY + 9).to_string()));
        cancel.cancel();
    }
}
