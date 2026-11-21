use tokio::{sync::mpsc, task::JoinHandle};

use crate::model::LiveEvent;

/// 配信イベントを入力順で各受信側へ渡す。閉じた受信側は読み飛ばす。
pub fn spawn_live_fanout(
    mut rx: mpsc::Receiver<LiveEvent>,
    outs: Vec<mpsc::Sender<LiveEvent>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            for out in &outs {
                let _ = out.send(event.clone()).await;
            }
        }
    })
}
