use tokio::{
    sync::mpsc,
    task::{JoinHandle, JoinSet},
};

use crate::model::LiveEvent;

/// 配信イベントを入力順で各受信側へ渡す。
///
/// 受信側ごとに待ち行列と転送タスクを持つので、片方が満杯でも、もう片方には遅れずに届く。
/// 閉じた受信側は読み飛ばす。入力が閉じたら、残りを渡し終えてから終わる。
/// 返したタスクを abort すると、転送タスクも JoinSet の drop で止まる。
pub fn spawn_live_fanout(
    mut rx: mpsc::Receiver<LiveEvent>,
    outs: Vec<mpsc::Sender<LiveEvent>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut forwarders = JoinSet::new();
        let mut queues = Vec::with_capacity(outs.len());
        for out in outs {
            // 配信イベントは開始・終了など低頻度なので、上限なしの待ち行列で足りる。
            let (queue, mut pending) = mpsc::unbounded_channel::<LiveEvent>();
            queues.push(queue);
            forwarders.spawn(async move {
                while let Some(event) = pending.recv().await {
                    if out.send(event).await.is_err() {
                        break;
                    }
                }
            });
        }
        while let Some(event) = rx.recv().await {
            // 転送タスクが終わった(受信側が閉じた)出力は外す。
            queues.retain(|queue| queue.send(event.clone()).is_ok());
        }
        drop(queues);
        while forwarders.join_next().await.is_some() {}
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Platform;
    use std::time::Duration;

    fn ended(n: usize) -> LiveEvent {
        LiveEvent::Ended {
            platform: Platform::Twitch,
            channel_identifier: format!("ch{n}"),
        }
    }

    #[tokio::test]
    async fn full_output_does_not_stall_the_other_output() {
        let (tx, rx) = mpsc::channel(128);
        // autopost 側は読まずに満杯のまま置く。
        let (stuck_tx, _stuck_rx) = mpsc::channel(64);
        let (engine_tx, mut engine_rx) = mpsc::channel(64);
        let handle = spawn_live_fanout(rx, vec![stuck_tx, engine_tx]);
        let events: Vec<_> = (0..65).map(ended).collect();
        for ev in &events {
            tx.send(ev.clone()).await.unwrap();
        }
        let mut got = Vec::new();
        for _ in 0..events.len() {
            let ev = tokio::time::timeout(Duration::from_secs(2), engine_rx.recv())
                .await
                .expect("満杯の出力があっても、もう片方には届く")
                .unwrap();
            got.push(ev);
        }
        assert_eq!(got, events, "順序も保つ");
        handle.abort();
    }
}
