//! chat と実績を別々に購読しても、原因の chat を先に処理する。
use super::SequencedAchievement;
use std::collections::VecDeque;
use tokio::sync::broadcast::{
    error::{RecvError, TryRecvError},
    Receiver,
};

const PENDING_CAPACITY: usize = 256;

pub(crate) enum Polled<T> {
    Item(T),
    Lagged(u64),
    Closed,
}

impl<T> From<Result<T, RecvError>> for Polled<T> {
    fn from(result: Result<T, RecvError>) -> Self {
        match result {
            Ok(item) => Self::Item(item),
            Err(RecvError::Lagged(n)) => Self::Lagged(n),
            Err(RecvError::Closed) => Self::Closed,
        }
    }
}

pub(crate) fn poll<T: Clone>(rx: &mut Receiver<T>) -> Option<Polled<T>> {
    match rx.try_recv() {
        Ok(item) => Some(Polled::Item(item)),
        Err(TryRecvError::Lagged(n)) => Some(Polled::Lagged(n)),
        Err(TryRecvError::Closed) => Some(Polled::Closed),
        Err(TryRecvError::Empty) => None,
    }
}

#[derive(Default)]
pub(crate) struct AchievementRelay {
    // chat を受信できない場合は None。購読開始前の chat は基準番号まで処理済みとみなす。
    processed_chat: Option<u64>,
    pending: VecDeque<SequencedAchievement>,
}

impl AchievementRelay {
    pub(crate) fn new(chat_baseline: u64) -> Self {
        Self {
            processed_chat: Some(chat_baseline),
            ..Self::default()
        }
    }

    pub(crate) fn processed_chat(&mut self, seq: u64) {
        if let Some(processed) = &mut self.processed_chat {
            *processed = (*processed).max(seq);
        }
    }

    pub(crate) fn close_chat(&mut self) {
        self.processed_chat = None;
    }

    pub(crate) fn has_ready(&self) -> bool {
        self.pending.front().is_some_and(|unlock| {
            self.processed_chat
                .is_none_or(|seq| unlock.after_seq <= seq)
        })
    }

    /// 終了前の最後の配信。受信済み・未受信の実績のうち、原因の chat が処理済みの分を送り出し、
    /// 送れない残りは捨てて欠落数に数える(受信の遅延分も含む)。
    pub(crate) fn finish(
        &mut self,
        rx: Option<&mut Receiver<SequencedAchievement>>,
        send: impl FnMut(SequencedAchievement),
    ) -> u64 {
        let (dropped, _) = self.drain(None, rx, usize::MAX, send);
        let rest = self.pending.len() as u64;
        self.pending.clear();
        dropped.saturating_add(rest)
    }

    /// 受信と配信をそれぞれ上限まで処理する。前回までの保留分も配信上限に含める。
    /// 欠落数 (購読の遅延と保留上限超過) と受信チャネルの終了有無を返す。
    pub(crate) fn drain(
        &mut self,
        mut first: Option<Polled<SequencedAchievement>>,
        mut rx: Option<&mut Receiver<SequencedAchievement>>,
        limit: usize,
        mut send: impl FnMut(SequencedAchievement),
    ) -> (u64, bool) {
        let mut received = 0;
        let mut sent = 0;
        let mut dropped = 0u64;
        loop {
            while sent < limit && self.has_ready() {
                send(self.pending.pop_front().unwrap());
                sent += 1;
            }
            if received == limit {
                return (dropped, false);
            }
            let Some(next) = first.take().or_else(|| poll(rx.as_deref_mut()?)) else {
                return (dropped, false);
            };
            received += 1;
            match next {
                Polled::Item(unlock) => {
                    if self.pending.len() == PENDING_CAPACITY {
                        self.pending.pop_front();
                        dropped = dropped.saturating_add(1);
                    }
                    self.pending.push_back(unlock);
                }
                Polled::Lagged(n) => dropped = dropped.saturating_add(n),
                Polled::Closed => return (dropped, true),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::broadcast;

    fn event(id: &str, after_seq: u64) -> SequencedAchievement {
        SequencedAchievement {
            after_seq,
            unlock: serde_json::from_value(serde_json::json!({
                "id": id, "achievementId": "a", "name": "n", "description": "", "tier": "bronze",
                "icon": "", "subject": {"kind": "streamer"}, "valueMilli": 1000,
                "thresholdMilli": 1000, "currency": null, "windowKey": "*", "unlockedAtMs": 1
            }))
            .unwrap(),
        }
    }

    #[test]
    fn finish_sends_eligible_pending_and_counts_the_rest() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut relay = AchievementRelay::new(0);
        tx.send(event("ready", 1)).unwrap();
        tx.send(event("waiting", 2)).unwrap();
        // 1 件は保留に入れ、もう 1 件は未受信のまま終了を迎える。
        relay.drain(poll(&mut rx), None, 1, |_| unreachable!("原因の chat が未処理"));
        relay.processed_chat(1);
        let mut sent = Vec::new();
        let dropped = relay.finish(Some(&mut rx), |e| sent.push(e.unlock.id));
        assert_eq!(sent, ["ready"]);
        assert_eq!(dropped, 1);
        assert!(!relay.has_ready());
    }
}
