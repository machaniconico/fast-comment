// registered in main.rs by story TC3-301
//! PR3' 受け入れテスト 4.2: 共有レート予算 `RateBudget`・`comment_interval`・`LiveGuard`。
//! `RateBudget` は純粋で、時刻(epoch ms)は引数で渡す。時間は進めない(sleep しない)。
#![allow(dead_code)]

use crate::twitcasting_common::*;
use fast_comment_lib::sources::twitcasting_api::{comment_interval, RateBudget};
use std::time::Duration;

/// 秒の境界に揃えた基準時刻(ms)。`now / 1000` が整数秒になる。
const NOW: u64 = 1_800_000_000_000;
const NOW_S: u64 = NOW / 1000;

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// 「約 expected 秒」: ±100ms を許す(Reset は秒単位の値なので実装の丸めを許容する)。
fn assert_about(actual: Duration, expected_s: u64) {
    let lo = Duration::from_millis(expected_s * 1000 - 100);
    let hi = Duration::from_millis(expected_s * 1000 + 100);
    assert!(actual >= lo && actual <= hi, "{actual:?} は約 {expected_s} 秒のはず");
}

#[test]
fn budget_allows_54_per_rolling_minute_then_delays() {
    let mut b = RateBudget::new();
    for i in 0..54 {
        assert_eq!(b.delay_before_next(NOW + i * 10), Duration::ZERO, "{} 回目", i + 1);
    }
    // 55 回目は「最古 + 60 秒 − now」
    let now = NOW + 540;
    assert_eq!(b.delay_before_next(now), Duration::from_millis(NOW + 60_000 - now));
    // 60 秒後(最古が窓から出る)には 0
    assert_eq!(b.delay_before_next(NOW + 60_000), Duration::ZERO);
}

#[test]
fn budget_blocks_until_reset_plus_1s_when_remaining_is_2_or_less() {
    let mut b = RateBudget::new();
    let reset = (NOW_S + 10).to_string();
    b.observe_headers(Some("2"), Some(reset.as_str()), NOW);
    assert_about(b.delay_before_next(NOW), 11);

    let mut b = RateBudget::new();
    b.observe_headers(Some("3"), Some(reset.as_str()), NOW);
    assert_eq!(b.delay_before_next(NOW), Duration::ZERO);
}

#[test]
fn budget_penalty_without_reset_waits_60s() {
    let mut b = RateBudget::new();
    b.penalize(None, NOW);
    assert_about(b.delay_before_next(NOW), 60);
}

#[test]
fn budget_clamps_reset_to_1s_through_120s() {
    let mut b = RateBudget::new();
    b.penalize(Some(NOW_S - 100), NOW);
    assert_about(b.delay_before_next(NOW), 1);

    let mut b = RateBudget::new();
    b.penalize(Some(NOW_S + 500), NOW);
    assert_about(b.delay_before_next(NOW), 120);
}

#[test]
fn budget_ignores_missing_or_garbage_headers() {
    let mut b = RateBudget::new();
    b.observe_headers(None, None, NOW);
    assert_eq!(b.delay_before_next(NOW), Duration::ZERO);
    b.observe_headers(Some("abc"), Some("abc"), NOW);
    assert_eq!(b.delay_before_next(NOW), Duration::ZERO);
    b.observe_headers(Some("-1"), Some("-1"), NOW);
    assert_eq!(b.delay_before_next(NOW), Duration::ZERO);
}

#[test]
fn delay_does_not_consume_when_nonzero() {
    let mut b = RateBudget::new();
    for _ in 0..54 {
        assert_eq!(b.delay_before_next(NOW), Duration::ZERO);
    }
    // 窓が埋まった後の「待ちが必要」な呼び出しは数えない
    for _ in 0..10 {
        assert!(b.delay_before_next(NOW + 1) > Duration::ZERO);
    }
    // 最初の 54 件が窓から出た時点で、また 54 回ちょうど通る(数えていれば 44 回で止まる)
    let later = NOW + 60_000;
    for i in 0..54 {
        assert_eq!(b.delay_before_next(later), Duration::ZERO, "{} 回目", i + 1);
    }
    assert!(b.delay_before_next(later) > Duration::ZERO);
}

#[test]
fn pause_deadline_only_extends() {
    let mut b = RateBudget::new();
    b.penalize(Some(NOW_S + 10), NOW);
    assert_about(b.delay_before_next(NOW), 11);
    // 遅れて届いた 200 の Remaining と、短い Reset の 429 は期限を縮めない
    let short = (NOW_S + 1).to_string();
    b.observe_headers(Some("50"), Some(short.as_str()), NOW);
    b.penalize(Some(NOW_S + 2), NOW);
    assert_about(b.delay_before_next(NOW), 11);
    // 長い Reset は延ばす
    b.penalize(Some(NOW_S + 20), NOW);
    assert_about(b.delay_before_next(NOW), 21);
}

#[test]
fn comment_interval_is_2s_times_live_count_with_4s_floor() {
    assert_eq!(comment_interval(0), secs(4));
    assert_eq!(comment_interval(1), secs(4));
    assert_eq!(comment_interval(2), secs(4));
    assert_eq!(comment_interval(3), secs(6));
    assert_eq!(comment_interval(5), secs(10));
}

#[tokio::test]
async fn dropping_live_guard_decrements_live_count() {
    let r = rig().await;
    assert_eq!(r.api.live_count(), 0);
    let g1 = r.api.live_guard();
    let g2 = r.api.live_guard();
    assert_eq!(r.api.live_count(), 2);
    drop(g1);
    assert_eq!(r.api.live_count(), 1);
    drop(g2);
    assert_eq!(r.api.live_count(), 0);
}
