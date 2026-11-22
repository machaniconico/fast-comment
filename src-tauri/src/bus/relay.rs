//! Preserve chat dependencies across independent broadcast receivers.
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
    // None when chat is unavailable. The subscription baseline covers older chat.
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

    /// Bound both receiver polls and deliveries, including work carried over from earlier rounds.
    /// Returns the dropped count (broadcast lag and pending overflow) and receiver closure.
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
