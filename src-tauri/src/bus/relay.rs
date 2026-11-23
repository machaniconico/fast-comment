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

    /// 終了時に送らず捨てる実績の件数を数えて返す(保留中の全件と、未受信の分・受信の遅延分)。
    pub(crate) fn discard(&mut self, mut rx: Option<&mut Receiver<SequencedAchievement>>) -> u64 {
        let mut count = self.pending.len() as u64;
        self.pending.clear();
        while let Some(next) = rx.as_deref_mut().and_then(poll) {
            match next {
                Polled::Item(_) => count = count.saturating_add(1),
                Polled::Lagged(n) => count = count.saturating_add(n),
                Polled::Closed => break,
            }
        }
        count
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
    fn discard_counts_pending_and_unreceived_without_sending() {
        let (tx, mut rx) = broadcast::channel(8);
        let mut relay = AchievementRelay::new(0);
        tx.send(event("pending", 1)).unwrap();
        tx.send(event("unreceived", 1)).unwrap();
        relay.drain(poll(&mut rx), None, 1, |_| unreachable!("原因の chat が未処理"));
        relay.processed_chat(1);
        assert!(relay.has_ready());
        assert_eq!(relay.discard(Some(&mut rx)), 2);
        assert!(!relay.has_ready());
    }
}
