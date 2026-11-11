//! 読み上げキューのコア(件数上限・待機上限・epoch 管理)。

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use super::{TtsQueueItem, TtsQueueStatePayload};
use crate::model::ChatMessage;

pub struct TtsJob {
    pub msg: ChatMessage,
    pub item: TtsQueueItem,
    pub enqueued_at: Instant,
}

#[derive(Debug, PartialEq, Eq)]
pub enum PushResult {
    Queued { dropped_oldest: usize },
    RejectedPaused,
}

pub struct Popped {
    pub job: TtsJob,
    pub epoch: u64,
    pub pending: usize,
}

/// 外側の共有ロックで保護する状態。取り出したジョブは epoch で無効化する。
#[derive(Default)]
pub struct TtsQueueCore {
    jobs: VecDeque<TtsJob>,
    epoch: u64,
    paused: bool,
    play_until: Option<Instant>,
}

impl TtsQueueCore {
    pub fn push(&mut self, job: TtsJob, max_pending: usize) -> PushResult {
        if self.paused {
            return PushResult::RejectedPaused;
        }
        // 設定の正規化に加え、直接の呼び出しでも最低 1 件は保持する。
        let limit = max_pending.max(1);
        let mut dropped_oldest = 0;
        while self.jobs.len() >= limit {
            self.jobs.pop_front();
            dropped_oldest += 1;
        }
        self.jobs.push_back(job);
        PushResult::Queued { dropped_oldest }
    }

    pub fn pop_fresh(
        &mut self,
        now: Instant,
        max_wait: Option<Duration>,
    ) -> (Option<Popped>, usize) {
        if self.paused {
            return (None, 0);
        }
        let mut expired = 0;
        while let Some(job) = self.jobs.pop_front() {
            if max_wait.is_some_and(|limit| now.saturating_duration_since(job.enqueued_at) > limit)
            {
                expired += 1;
                continue;
            }
            return (
                Some(Popped {
                    job,
                    epoch: self.epoch,
                    pending: self.jobs.len(),
                }),
                expired,
            );
        }
        (None, expired)
    }

    pub fn clear(&mut self) {
        self.jobs.clear();
        self.epoch = self.epoch.wrapping_add(1);
        self.play_until = None;
    }

    pub fn set_paused(&mut self, paused: bool) {
        if paused {
            self.clear();
        }
        self.paused = paused;
    }

    pub fn skip_current(&mut self, now: Instant) {
        if self.play_until.is_some_and(|until| until > now) {
            // 再生中なら先読み合成は生かし、再生待ちだけを解除する。
            self.play_until = None;
        } else {
            // 再生していない間は合成中の 1 件を無効化する。待ち行列は残す。
            self.epoch = self.epoch.wrapping_add(1);
            self.play_until = None;
        }
    }

    pub fn is_current(&self, epoch: u64) -> bool {
        !self.paused && self.epoch == epoch
    }

    /// dur は呼び出し側でペーシング余裕を加えた再生待ち時間。
    pub fn try_begin_playback(&mut self, epoch: u64, now: Instant, dur: Duration) -> bool {
        if !self.is_current(epoch) {
            return false;
        }
        self.play_until = now.checked_add(dur);
        true
    }

    pub fn play_until(&self) -> Option<Instant> {
        self.play_until
    }

    pub fn snapshot(&self) -> TtsQueueStatePayload {
        TtsQueueStatePayload {
            depth: u32::try_from(self.jobs.len()).unwrap_or(u32::MAX),
            paused: self.paused,
            items: self.jobs.iter().map(|job| job.item.clone()).collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: &str, enqueued_at: Instant) -> TtsJob {
        let msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "id": id, "platform": "twitch", "channel": "channel",
            "author": {"id": "user", "name": "Alice"},
            "fragments": [], "kind": "normal", "timestampMs": 0
        }))
        .unwrap();
        TtsJob {
            item: TtsQueueItem::from_message(&msg),
            msg,
            enqueued_at,
        }
    }

    #[test]
    fn push_over_limit_drops_oldest_first() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        for id in ["a", "b", "c", "d"] {
            core.push(job(id, now), 4);
        }
        assert_eq!(
            core.push(job("e", now), 2),
            PushResult::Queued { dropped_oldest: 3 }
        );
        let snapshot = core.snapshot();
        assert_eq!(snapshot.depth, 2);
        assert!(!snapshot.paused);
        assert_eq!(
            snapshot
                .items
                .iter()
                .map(|i| i.id.as_str())
                .collect::<Vec<_>>(),
            ["d", "e"]
        );
        assert_eq!(core.pop_fresh(now, None).0.unwrap().job.msg.id, "d");
        assert_eq!(core.pop_fresh(now, None).0.unwrap().job.msg.id, "e");
        assert!(core.pop_fresh(now, None).0.is_none());
        // 不正な 0 上限でも新着を保持し、ループしない。
        core.push(job("f", now), 0);
        assert_eq!(
            core.push(job("g", now), 0),
            PushResult::Queued { dropped_oldest: 1 }
        );
    }

    #[test]
    fn push_is_rejected_while_paused() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        core.push(job("old", now), 64);
        let epoch = core.pop_fresh(now, None).0.unwrap().epoch;
        core.push(job("pending", now), 64);
        core.set_paused(true);
        assert!(!core.is_current(epoch));
        assert_eq!(core.push(job("new", now), 64), PushResult::RejectedPaused);
        assert!(core.snapshot().paused);
        assert_eq!(core.snapshot().depth, 0);
        assert!(core.pop_fresh(now, None).0.is_none());
        core.set_paused(false);
        assert!(!core.is_current(epoch));
        assert_eq!(
            core.push(job("resumed", now), 64),
            PushResult::Queued { dropped_oldest: 0 }
        );
    }

    #[test]
    fn pop_fresh_skips_expired_and_reports_count() {
        let start = Instant::now();
        let now = start + Duration::from_secs(10);
        let mut core = TtsQueueCore::default();
        core.push(job("old1", start), 64);
        core.push(job("old2", start + Duration::from_secs(1)), 64);
        core.push(job("boundary", start + Duration::from_secs(5)), 64);
        let (popped, expired) = core.pop_fresh(now, Some(Duration::from_secs(5)));
        assert_eq!(expired, 2);
        assert_eq!(popped.unwrap().job.msg.id, "boundary");
        core.push(job("expired", start), 64);
        let (popped, expired) = core.pop_fresh(now, Some(Duration::from_secs(5)));
        assert!(popped.is_none());
        assert_eq!(expired, 1);
        core.push(job("unlimited", start), 64);
        assert_eq!(core.pop_fresh(now, None).0.unwrap().job.msg.id, "unlimited");
    }

    #[test]
    fn pop_returns_remaining_pending_count() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        for id in ["a", "b", "c"] {
            core.push(job(id, now), 64);
        }
        for remaining in [2, 1, 0] {
            assert_eq!(core.pop_fresh(now, None).0.unwrap().pending, remaining);
            assert_eq!(core.snapshot().depth, remaining as u32);
        }
    }

    #[test]
    fn clear_bumps_epoch_and_resets_playback() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        core.push(job("a", now), 64);
        let epoch = core.pop_fresh(now, None).0.unwrap().epoch;
        assert!(core.try_begin_playback(epoch, now, Duration::from_secs(1)));
        assert_eq!(core.play_until(), Some(now + Duration::from_secs(1)));
        core.push(job("b", now), 64);
        core.clear();
        assert_eq!(core.snapshot().depth, 0);
        assert_eq!(core.play_until(), None);
        assert!(!core.is_current(epoch));
        core.push(job("c", now), 64);
        assert_eq!(core.pop_fresh(now, None).0.unwrap().epoch, epoch + 1);
    }

    #[test]
    fn skip_while_playing_keeps_epoch_and_ends_playback() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        core.push(job("playing", now), 64);
        let epoch = core.pop_fresh(now, None).0.unwrap().epoch;
        core.try_begin_playback(epoch, now, Duration::from_secs(1));
        core.push(job("next", now), 64);
        core.skip_current(now);
        assert!(core.is_current(epoch));
        assert_eq!(core.play_until(), None);
        assert_eq!(core.pop_fresh(now, None).0.unwrap().job.msg.id, "next");
    }

    #[test]
    fn skip_while_idle_invalidates_inflight_epoch() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        core.push(job("inflight", now), 64);
        let epoch = core.pop_fresh(now, None).0.unwrap().epoch;
        core.push(job("next", now), 64);
        core.skip_current(now);
        assert!(!core.is_current(epoch));
        let next = core.pop_fresh(now, None).0.unwrap();
        assert_eq!(next.job.msg.id, "next");
        assert!(core.is_current(next.epoch));
        // 既に期限が過ぎた再生も idle として扱う。
        core.try_begin_playback(next.epoch, now, Duration::from_secs(1));
        core.skip_current(now + Duration::from_secs(1));
        assert!(!core.is_current(next.epoch));
        assert_eq!(core.play_until(), None);
    }

    #[test]
    fn try_begin_playback_rejects_stale_epoch_and_paused() {
        let now = Instant::now();
        let mut core = TtsQueueCore::default();
        core.push(job("a", now), 64);
        let epoch = core.pop_fresh(now, None).0.unwrap().epoch;
        core.clear();
        assert!(!core.try_begin_playback(epoch, now, Duration::from_secs(1)));
        assert_eq!(core.play_until(), None);
        assert!(core.try_begin_playback(epoch + 1, now, Duration::from_secs(1)));
        let until = core.play_until();
        assert!(!core.try_begin_playback(epoch, now, Duration::from_secs(2)));
        assert_eq!(core.play_until(), until);
        core.set_paused(true);
        assert!(!core.try_begin_playback(core.epoch, now, Duration::from_secs(1)));
        assert_eq!(core.play_until(), None);
    }
}
