// registered in main.rs by story TC3-303
//! PR3' 受け入れテスト 4.5: `TwitcastingOfficialSource` を偽 API に対して直接 `run` する。
//! 偽 API は `Script`(user / current_live / comments の 3 経路)。固定の偽サーバ列 C1〜C5 は §3.1。
//!
//! 共有レート予算は `rig()` が実質無制限(`RateBudget::with_capacity`)にする。429 の停止は予算の別機能で有効。
#![allow(dead_code)]

use crate::twitcasting_common::*;
use crate::twitcasting_common_official::*;
use fast_comment_lib::model::LiveEvent;
use fast_comment_lib::sources::twitcasting::{channel_name, NOT_FOUND_NOTICE, UNAVAILABLE_NOTICE};
use fast_comment_lib::sources::twitcasting_auth::AuthState;
use fast_comment_lib::sources::twitcasting_official::{
    PollTuning, APP_DISABLED_NOTICE, LOGIN_REQUIRED_NOTICE, RELOGIN_REQUIRED_NOTICE,
};
use serde_json::json;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

const T5: Duration = Duration::from_secs(5);
const T15: Duration = Duration::from_secs(15);

fn state(auth: &Auth) -> AuthState {
    *auth.subscribe().borrow()
}

fn slow_live_refresh() -> PollTuning {
    tuning()
}

/// movie 9 を「配信中にするかどうか」を旗で切り替える。
fn live_switch(script: &Arc<Script>, initially: bool) -> Arc<AtomicBool> {
    let flag = Arc::new(AtomicBool::new(initially));
    let f = flag.clone();
    script.on_live(move |_: &Req| if f.load(Ordering::SeqCst) { live_reply("9", "t", 3) } else { offline_reply() });
    flag
}

fn key(r: &Req) -> (Option<String>, u32) {
    (r.slice_id(), r.offset())
}

fn some(s: &str) -> Option<String> {
    Some(s.to_string())
}

fn saw_offline(chan: &Chan) -> bool {
    chan.metas().iter().any(|m| m.live == Some(false))
}

async fn start_logged_in() -> (Rig, Arc<Script>, Chan) {
    let r = rig_logged_in().await;
    let script = Script::new();
    script.install(&r.fake);
    (r, script, Chan::new())
}

// ---------------------------------------------------------------- 認証と起動

#[tokio::test]
async fn logged_out_source_sends_login_notice_and_makes_no_requests() {
    let r = rig().await;
    Script::new().install(&r.fake);
    let chan = Chan::new();
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("ログイン案内", T5, || chan.has_notice(LOGIN_REQUIRED_NOTICE)).await;
    settle(200).await;
    assert_eq!(r.fake.total(), 0, "未ログインで要求が届いた");
    run.stop().await;
}

#[tokio::test]
async fn source_starts_polling_after_login_without_respawn() {
    let r = rig().await;
    Script::new().install(&r.fake);
    let chan = Chan::new();
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("ログイン案内", T5, || chan.has_notice(LOGIN_REQUIRED_NOTICE)).await;
    assert_eq!(r.fake.total(), 0);
    login(&r.auth, r.port, TOK_A).await;
    wait_until("同じ Source が /users を叩く", T5, || !r.fake.user_calls().is_empty()).await;
    run.stop().await;
}

#[tokio::test]
async fn unknown_user_sends_not_found_notice() {
    let (r, script, chan) = start_logged_in().await;
    script.on_user(|_: &Req| error_reply(404, 404));
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("見つからない案内", T5, || chan.has_notice(NOT_FOUND_NOTICE)).await;
    wait_until("not_found_retry 後の再試行", T5, || r.fake.user_calls().len() >= 2).await;
    let calls = r.fake.user_calls();
    assert!(
        calls[1].at.duration_since(calls[0].at) >= ms(180),
        "再試行が not_found_retry(200ms) より早い: {:?}",
        calls[1].at.duration_since(calls[0].at)
    );
    run.stop().await;
}

// ---------------------------------------------------------------- 配信状態

#[tokio::test]
async fn offline_user_is_polled_and_reported_live_false() {
    let (r, _script, chan) = start_logged_in().await;
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("live=false", T5, || saw_offline(&chan)).await;
    wait_until("offline ごとの再取得", T5, || r.fake.live_calls().len() >= 3).await;
    run.stop().await;
}

#[tokio::test]
async fn observed_stream_start_emits_all_comments_oldest_first_without_skip_tts() {
    let (r, script, chan) = start_logged_in().await;
    let live = live_switch(&script, false);
    script.on_comments(|req: &Req| {
        if req.slice_id().is_none() && req.offset() == 0 {
            page_reply(5, 1)
        } else {
            empty_reply()
        }
    });
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("未配信を見届ける", T5, || saw_offline(&chan)).await;
    live.store(true, Ordering::SeqCst);
    wait_until("5 件", T5, || chan.msgs().len() >= 5).await;
    settle(100).await;
    let msgs = chan.msgs();
    assert_eq!(ids_of(&msgs), tc_ids(1, 5));
    assert!(msgs.iter().all(|m| !m.skip_tts), "開始を見届けた movie は skip_tts=false");
    run.stop().await;
}

#[tokio::test]
async fn mid_stream_start_emits_only_newest_20_with_skip_tts() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    script.on_comments(|req: &Req| match (req.slice_id(), req.offset()) {
        (None, 0) => page_reply(150, 101),
        (None, 50) => page_reply(100, 51),
        _ => empty_reply(),
    });
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("20 件", T5, || chan.msgs().len() >= 20).await;
    settle(200).await;
    let msgs = chan.msgs();
    assert_eq!(ids_of(&msgs), tc_ids(131, 150));
    assert!(msgs.iter().all(|m| m.skip_tts), "途中参加は skip_tts=true");
    assert!(
        r.fake.comment_calls().iter().all(|q| q.offset() != 50),
        "途中参加の poll で offset=50 を送った"
    );
    run.stop().await;
}

#[tokio::test]
async fn live_state_reports_title_and_current_view_count() {
    let (r, script, chan) = start_logged_in().await;
    script.on_live(|_: &Req| live_reply("9", "t-title", 17));
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("タイトルと視聴者数", T5, || {
        chan.metas().iter().any(|m| m.title.as_deref() == Some("t-title") && m.concurrent_viewers == Some(17))
    })
    .await;
    run.stop().await;
}

// ---------------------------------------------------------------- 固定の偽サーバ列(§3.1)

#[tokio::test]
async fn poll_sequence_c1_emits_each_once_and_advances_slice_id() {
    let (r, script, chan) = start_logged_in().await;
    let live = live_switch(&script, false);
    script.on_comments(|req: &Req| match (req.slice_id().as_deref(), req.offset()) {
        (None, 0) => comments_reply(&[100]),
        (Some("100"), 0) => page_reply(170, 121),
        (Some("100"), 50) => page_reply(120, 101),
        _ => empty_reply(),
    });
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("未配信を見届ける", T5, || saw_offline(&chan)).await;
    live.store(true, Ordering::SeqCst);
    wait_until("slice_id=170 の要求", T5, || {
        r.fake.comment_calls().iter().any(|q| q.slice_id().as_deref() == Some("170") && q.offset() == 0)
    })
    .await;
    settle(100).await;
    assert_eq!(chan.msg_ids(), tc_ids(100, 170), "101..170 が各 1 回(先頭の 100 は準備の 1 件)");
    let keys: Vec<_> = r.fake.comment_calls().iter().map(key).collect();
    let expected_prefix = vec![
        (None, 0),
        (None, 50),
        (some("100"), 0),
        (some("100"), 50),
        (some("100"), 100),
        (some("170"), 0),
    ];
    assert_eq!(keys[..6], expected_prefix[..]);
    assert!(keys[6..].iter().all(|k| *k == (some("170"), 0)), "以後は slice_id=170&offset=0: {keys:?}");
    run.stop().await;
}

#[tokio::test]
async fn poll_sequence_c2_503_mid_loop_retries_without_loss_or_duplicate() {
    let (r, script, chan) = start_logged_in().await;
    let live = live_switch(&script, false);
    let fails = Arc::new(AtomicUsize::new(0));
    let emitted_before_503: Arc<Mutex<Option<usize>>> = Arc::new(Mutex::new(None));
    {
        let handle = chan.msgs_handle();
        let snapshot = emitted_before_503.clone();
        script.on_comments(move |req: &Req| match (req.slice_id().as_deref(), req.offset()) {
            (None, 0) => comments_reply(&[100]),
            (Some("100"), 0) => page_reply(170, 121),
            (Some("100"), 50) => {
                if fails.fetch_add(1, Ordering::SeqCst) == 0 {
                    // 503 を返す時点で、この回の出力が 1 件も流れていないこと
                    let n = handle.lock().unwrap().iter().filter(|m| m.id != "tc-100").count();
                    *snapshot.lock().unwrap() = Some(n);
                    error_reply(503, 0)
                } else {
                    page_reply(120, 101)
                }
            }
            _ => empty_reply(),
        });
    }
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("未配信を見届ける", T5, || saw_offline(&chan)).await;
    live.store(true, Ordering::SeqCst);
    wait_until("70 件が流れる", T15, || chan.msgs().len() >= 71).await;
    wait_until("slice_id=170 の要求", T5, || {
        r.fake.comment_calls().iter().any(|q| q.slice_id().as_deref() == Some("170"))
    })
    .await;
    assert_eq!(*emitted_before_503.lock().unwrap(), Some(0), "503 の回に出力が流れた");
    let ids = chan.msg_ids();
    let rest: Vec<String> = ids.into_iter().filter(|i| i != "tc-100").collect();
    assert_eq!(rest, tc_ids(101, 170), "101..170 が各 1 回");
    let keys: Vec<_> = r.fake.comment_calls().iter().map(key).collect();
    let last_100 = keys.iter().rposition(|k| k.0.as_deref() == Some("100")).expect("slice_id=100 の要求がある");
    assert!(
        keys[last_100 + 1..].iter().all(|k| k.0.as_deref() == Some("170")),
        "成功後の要求は slice_id=170: {keys:?}"
    );
    run.stop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn poll_sequence_c4_caps_at_5_pages() {
    let (logs, _guard) = capture_logs();
    let (r, script, chan) = start_logged_in().await;
    logs.clear();
    let live = live_switch(&script, false);
    script.on_comments(|req: &Req| match (req.slice_id().as_deref(), req.offset()) {
        (None, 0) => comments_reply(&[100]),
        (Some("100"), o) if o <= 200 => {
            let k = (o / 50) as u64;
            page_reply(350 - k * 50, 301 - k * 50)
        }
        _ => empty_reply(),
    });
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("未配信を見届ける", T5, || saw_offline(&chan)).await;
    live.store(true, Ordering::SeqCst);
    wait_until("slice_id=350 の要求", T5, || {
        r.fake.comment_calls().iter().any(|q| q.slice_id().as_deref() == Some("350"))
    })
    .await;
    settle(100).await;
    let offsets: Vec<u32> = r
        .fake
        .comment_calls()
        .iter()
        .filter(|q| q.slice_id().as_deref() == Some("100"))
        .map(|q| q.offset())
        .collect();
    assert_eq!(offsets, [0, 50, 100, 150, 200], "slice_id=100 の要求はちょうど 5 件");
    let rest: Vec<String> = chan.msg_ids().into_iter().filter(|i| i != "tc-100").collect();
    assert_eq!(rest, tc_ids(101, 350));
    let warns = logs.lines().iter().filter(|l| l.contains("WARN")).count();
    assert_eq!(warns, 1, "5 ページ上限の warn が 1 行: {}", logs.text());
    assert_absent("ログ", &logs.text(), &["msg-", TOK_A, "Bearer"]);
    run.stop().await;
}

#[tokio::test]
async fn movie_change_resets_cursor_and_keeps_channel_name() {
    let (r, script, chan) = start_logged_in().await;
    let movie = Arc::new(Mutex::new("9".to_string()));
    {
        let movie = movie.clone();
        script.on_live(move |_: &Req| live_reply(&movie.lock().unwrap(), "t", 1));
    }
    script.on_comments(|req: &Req| {
        let fresh = req.slice_id().is_none() && req.offset() == 0;
        match (req.movie_id().as_str(), fresh) {
            ("9", true) => page_reply(3, 1),
            ("10", true) => page_reply(13, 11),
            _ => empty_reply(),
        }
    });
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("movie 9 の 3 件", T5, || chan.msgs().len() >= 3).await;
    *movie.lock().unwrap() = "10".to_string();
    wait_until("movie 10 の 3 件", T5, || chan.msgs().len() >= 6).await;
    settle(100).await;
    let mut expected = tc_ids(1, 3);
    expected.extend(tc_ids(11, 13));
    assert_eq!(chan.msg_ids(), expected);
    let first_of_10 = r.fake.comment_calls().into_iter().find(|q| q.movie_id() == "10").expect("movie 10 の要求");
    assert_eq!(first_of_10.slice_id(), None, "新 movie は slice_id 無しで始める");
    let expected_channel = channel_name("alice");
    assert!(chan.msgs().iter().all(|m| m.channel == expected_channel), "channel が変わった");
    run.stop().await;
}

#[tokio::test]
async fn stream_end_reports_live_false_and_resumes_offline_polling() {
    let (r, script, chan) = start_logged_in().await;
    let live = live_switch(&script, true);
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("配信開始", T5, || chan.started_count() >= 1).await;
    assert!(chan.metas().iter().any(|m| m.live == Some(true)));
    let before = chan.metas().len();
    live.store(false, Ordering::SeqCst);
    wait_until("LiveEvent::Ended(開始の後)", T5, || {
        let events = chan.lives();
        let started = events.iter().position(|e| matches!(e, LiveEvent::Started(_)));
        let ended = events.iter().rposition(|e| matches!(e, LiveEvent::Ended { .. }));
        matches!((started, ended), (Some(s), Some(e)) if e > s)
    })
    .await;
    wait_until("live=false", T5, || chan.metas().iter().skip(before).any(|m| m.live == Some(false))).await;
    let n = r.fake.live_calls().len();
    wait_until("未配信の再取得が続く", T5, || r.fake.live_calls().len() >= n + 3).await;
    run.stop().await;
}

// ---------------------------------------------------------------- 認証の変化

#[tokio::test]
async fn unauthorized_stops_requests_and_sends_relogin_notice() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    script.on_comments(|_: &Req| error_reply(401, 1000));
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("再ログイン案内", T5, || chan.has_notice(RELOGIN_REQUIRED_NOTICE)).await;
    settle(50).await;
    let n1 = r.fake.total();
    settle(200).await;
    assert_eq!(r.fake.total(), n1, "401 の後も要求が続いた");
    assert_eq!(state(&r.auth), AuthState::ReloginRequired);
    run.stop().await;
}

/// B4 の場面。世代 A の要求を保留 → 世代 B で再ログイン → 保留中の要求に 401。
/// 戻り値は (走行中の Source, 解放から tok-B の最初の要求が届くまでの時間)。
async fn late_401_scenario(r: &Rig, chan: &Chan) -> (Running, Duration) {
    let script = Script::new();
    script.install(&r.fake);
    script.on_live(|_: &Req| live_reply("9", "t", 1));
    let gate = Arc::new(Notify::new());
    let held = Arc::new(AtomicBool::new(false));
    {
        let gate = gate.clone();
        script.on_comments(move |req: &Req| {
            if req.authorization() == Some("Bearer tok-A-SECRET") {
                if !held.swap(true, Ordering::SeqCst) {
                    error_reply(401, 1000).hold(gate.clone())
                } else {
                    error_reply(401, 1000)
                }
            } else if req.slice_id().is_none() && req.offset() == 0 {
                comments_reply(&[7])
            } else {
                empty_reply()
            }
        });
    }
    let run = chan.run(official_source(r, "alice", chan, tuning()));
    // 1. 世代 A の要求が偽サーバで保留される
    wait_until("世代 A のコメント要求", T5, || {
        r.fake.comment_calls().iter().any(|q| q.authorization() == Some("Bearer tok-A-SECRET"))
    })
    .await;
    // 2. tok-B で再ログインして世代 B の Ready にする
    login(&r.auth, r.port, TOK_B).await;
    assert_eq!(state(&r.auth), AuthState::Ready);
    // 3. 保留中の要求に 401 を返す
    let released = Instant::now();
    gate.notify_one();
    wait_until("tok-B の要求", Duration::from_secs(3), || {
        r.fake.requests().iter().any(|q| q.authorization() == Some("Bearer tok-B-SECRET") && q.at >= released)
    })
    .await;
    let first_b = r
        .fake
        .requests()
        .into_iter()
        .find(|q| q.authorization() == Some("Bearer tok-B-SECRET") && q.at >= released)
        .expect("tok-B の要求");
    (run, first_b.at.duration_since(released))
}

#[tokio::test]
async fn late_401_from_old_generation_does_not_stall_source() {
    let (r, _script, chan) = start_logged_in().await;
    let (run, elapsed) = late_401_scenario(&r, &chan).await;
    // 契約: comment_unit × 3 以内(実時間の揺れ分の余裕 SLACK を足して測る)
    assert!(elapsed <= ms(20) * 3 + SLACK, "tok-B の要求まで {elapsed:?}");
    wait_until("世代 B の 1 件が流れる", T5, || chan.msg_ids().iter().any(|i| i == "tc-7")).await;
    assert_eq!(state(&r.auth), AuthState::Ready, "古い世代の 401 で auth が落ちた");
    let last = chan.metas().into_iter().last().expect("メタデータがある");
    assert_ne!(last.notice.as_deref(), Some(RELOGIN_REQUIRED_NOTICE), "再ログイン案内で止まっている");
    run.stop().await;
}

#[tokio::test]
async fn relogin_resumes_without_respawn() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    // 0 = 通常 / 1 = 現世代の 401 / 2 = 新しいコメント 4 が来る
    let mode = Arc::new(AtomicUsize::new(0));
    {
        let mode = mode.clone();
        script.on_comments(move |req: &Req| match (req.slice_id().as_deref(), req.offset()) {
            (None, 0) => page_reply(3, 1),
            (Some("3"), 0) => match mode.load(Ordering::SeqCst) {
                1 => error_reply(401, 1000),
                2 => comments_reply(&[4]),
                _ => empty_reply(),
            },
            _ => empty_reply(),
        });
    }
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("1..3", T5, || chan.msgs().len() >= 3).await;
    mode.store(1, Ordering::SeqCst);
    wait_until("再ログイン案内", T5, || chan.has_notice(RELOGIN_REQUIRED_NOTICE)).await;
    settle(100).await;
    let n = r.fake.comment_calls().len();
    mode.store(2, Ordering::SeqCst);
    login(&r.auth, r.port, TOK_B).await;
    wait_until("再開して 4 が流れる", T5, || chan.msg_ids().iter().any(|i| i == "tc-4")).await;
    settle(100).await;
    assert_eq!(chan.msg_ids(), tc_ids(1, 4), "重複・取りこぼしなし");
    let after: Vec<_> = r.fake.comment_calls().into_iter().skip(n).collect();
    assert!(!after.is_empty());
    assert!(after.iter().all(|q| q.slice_id().is_some()), "再開後にカーソルが失われた");
    run.stop().await;
}

#[tokio::test]
async fn logout_mid_stream_stops_requests_and_sends_login_notice() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("コメント要求", T5, || !r.fake.comment_calls().is_empty()).await;
    assert!(r.auth.disconnect().await.is_ok());
    wait_until("ログイン案内", T5, || chan.has_notice(LOGIN_REQUIRED_NOTICE)).await;
    settle(50).await;
    let n1 = r.fake.total();
    settle(200).await;
    assert_eq!(r.fake.total(), n1, "ログアウト後も要求が続いた");
    run.stop().await;
}

// ---------------------------------------------------------------- レート制限

#[tokio::test]
async fn rate_limited_pauses_every_twitcasting_source_until_reset() {
    let r = rig_logged_in().await;
    let script = Script::new();
    script.install(&r.fake);
    script.on_user(|req: &Req| {
        let id = if req.path.ends_with("/bob") { "2" } else { "1" };
        Reply::json(200, json!({"user": {"id": id, "screen_id": "x", "name": "X"}}))
    });
    script.on_live(|req: &Req| {
        let user_id = req.path.split('/').nth(2).unwrap_or("").to_string();
        live_reply(if user_id == "2" { "19" } else { "9" }, "t", 1)
    });
    // 最初のコメント要求だけが 429(Reset = 今 + 1 秒)
    let hit: Arc<Mutex<Option<(u64, u64)>>> = Arc::new(Mutex::new(None));
    {
        let hit = hit.clone();
        script.on_comments(move |req: &Req| {
            let mut h = hit.lock().unwrap();
            if h.is_none() {
                let reset = epoch_s() + 1;
                *h = Some((req.epoch_ms, reset));
                error_reply(429, 0).header("X-RateLimit-Reset", &reset.to_string())
            } else {
                empty_reply()
            }
        });
    }
    let (c1, c2) = (Chan::new(), Chan::new());
    let run1 = c1.run(official_source(&r, "alice", &c1, tuning()));
    let run2 = c2.run(official_source(&r, "bob", &c2, tuning()));
    wait_until("429 が返る", T5, || hit.lock().unwrap().is_some()).await;
    let (t429, reset) = hit.lock().unwrap().unwrap();
    let deadline_ms = (reset + 1) * 1000;
    // 両行とも期限後に再開する
    wait_until("両行が期限後に再開", Duration::from_secs(8), || {
        let calls = r.fake.comment_calls();
        let after = |movie: &str| calls.iter().any(|q| q.movie_id() == movie && q.epoch_ms >= deadline_ms);
        after("9") && after("19")
    })
    .await;
    // 429 の後に始まった要求は、どの行のものも期限まで届かない
    let early: Vec<_> = r
        .fake
        .requests()
        .into_iter()
        .filter(|q| q.epoch_ms > t429 + 30 && q.epoch_ms < deadline_ms)
        .map(|q| (q.path, deadline_ms - q.epoch_ms))
        .collect();
    assert!(early.is_empty(), "停止期限より前に届いた要求(path, 残りms): {early:?}");
    run1.stop().await;
    run2.stop().await;
}

// ---------------------------------------------------------------- コメント周期

fn assert_gaps_at_least(calls: &[Req], min: Duration, skip: usize, label: &str) {
    let times: Vec<Instant> = calls.iter().map(|c| c.at).collect();
    assert!(times.len() > skip + 1, "{label}: 要求が足りない");
    for w in times[skip..].windows(2) {
        let gap = w[1].duration_since(w[0]);
        assert!(gap >= min, "{label}: poll 間隔 {gap:?} が下限 {min:?} 未満");
    }
}

#[tokio::test]
async fn single_channel_poll_interval_has_4s_floor() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    let run = chan.run(official_source(&r, "alice", &chan, slow_live_refresh()));
    wait_until("コメント要求 7 件", T5, || r.fake.comment_calls().len() >= 7).await;
    // 契約: comment_unit × 2 以上(測定の誤差 5ms を引く)
    assert_gaps_at_least(&r.fake.comment_calls(), ms(20) * 2 - ms(5), 0, "1 行");
    run.stop().await;
}

#[tokio::test]
async fn three_live_channels_triple_the_comment_interval() {
    let r = rig_logged_in().await;
    let script = Script::new();
    script.install(&r.fake);
    script.on_user(|req: &Req| {
        let id = match req.path.rsplit('/').next().unwrap_or("") {
            "ch2" => "2",
            "ch3" => "3",
            _ => "1",
        };
        Reply::json(200, json!({"user": {"id": id, "screen_id": "x", "name": "X"}}))
    });
    script.on_live(|req: &Req| {
        let movie = match req.path.split('/').nth(2).unwrap_or("") {
            "2" => "19",
            "3" => "29",
            _ => "9",
        };
        live_reply(movie, "t", 1)
    });
    let chans = [Chan::new(), Chan::new(), Chan::new()];
    let runs: Vec<Running> = ["ch1", "ch2", "ch3"]
        .iter()
        .zip(chans.iter())
        .map(|(id, c)| c.run(official_source(&r, id, c, slow_live_refresh())))
        .collect();
    wait_until("各行 6 件以上", T5, || {
        let calls = r.fake.comment_calls();
        ["9", "19", "29"].iter().all(|m| calls.iter().filter(|q| q.movie_id() == *m).count() >= 6)
    })
    .await;
    for movie in ["9", "19", "29"] {
        let calls: Vec<Req> = r.fake.comment_calls().into_iter().filter(|q| q.movie_id() == movie).collect();
        // 3 行そろうまでの最初の 2 間隔は除く。契約: comment_unit × 3 以上(誤差 5ms を引く)
        assert_gaps_at_least(&calls, ms(20) * 3 - ms(5), 2, movie);
    }
    for run in runs {
        run.stop().await;
    }
}

// ---------------------------------------------------------------- エラー種別ごとの挙動

#[tokio::test]
async fn protected_movie_sends_notice_and_stops_comment_polling_only() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    script.on_comments(|_: &Req| error_reply(403, 2002));
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("非公開の案内", T5, || chan.has_notice(UNAVAILABLE_NOTICE)).await;
    settle(50).await;
    let comments = r.fake.comment_calls().len();
    let lives = r.fake.live_calls().len();
    settle(200).await;
    assert_eq!(r.fake.comment_calls().len(), comments, "コメント取得が続いた");
    assert!(r.fake.live_calls().len() >= lives + 2, "current_live の確認は続くはず");
    run.stop().await;
}

#[tokio::test]
async fn app_disabled_sends_notice_and_slows_to_not_found_retry() {
    let (r, script, chan) = start_logged_in().await;
    script.on_user(|_: &Req| error_reply(403, 2001));
    let run = chan.run(official_source(&r, "alice", &chan, tuning()));
    wait_until("アプリ無効の案内", T5, || chan.has_notice(APP_DISABLED_NOTICE)).await;
    wait_until("次の要求", T5, || r.fake.user_calls().len() >= 2).await;
    let calls = r.fake.user_calls();
    let gap = calls[1].at.duration_since(calls[0].at);
    assert!(gap >= ms(180), "次の要求が not_found_retry(200ms) より早い: {gap:?}");
    run.stop().await;
}

#[tokio::test]
async fn server_error_backs_off_and_keeps_cursor() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    let fails = Arc::new(AtomicUsize::new(0));
    script.on_comments(move |req: &Req| match (req.slice_id().as_deref(), req.offset()) {
        (None, 0) => comments_reply(&[100]),
        (Some("100"), 0) => {
            if fails.fetch_add(1, Ordering::SeqCst) < 2 {
                error_reply(503, 0)
            } else {
                comments_reply(&[101])
            }
        }
        _ => empty_reply(),
    });
    let run = chan.run(official_source(&r, "alice", &chan, slow_live_refresh()));
    wait_until("回復後に 101 が流れる", T15, || chan.msg_ids().iter().any(|i| i == "tc-101")).await;
    let calls = r.fake.comment_calls();
    let fails: Vec<&Req> = calls.iter().filter(|q| q.slice_id().as_deref() == Some("100") && q.offset() == 0).collect();
    assert!(fails.len() >= 3, "503 が 2 回 + 成功 1 回の要求がある");
    let gap1 = fails[1].at.duration_since(fails[0].at);
    let gap2 = fails[2].at.duration_since(fails[1].at);
    assert!(gap2 > gap1, "間隔が伸びない: {gap1:?} → {gap2:?}");
    let first_fresh = calls.iter().position(|q| q.slice_id().is_none()).expect("最初の要求");
    assert!(
        calls.iter().skip(first_fresh + 1).all(|q| q.slice_id().is_some()),
        "再試行でカーソルが失われた"
    );
    assert_eq!(chan.msg_ids(), tc_ids(100, 101));
    run.stop().await;
}

#[tokio::test]
async fn network_error_backs_off_and_keeps_cursor() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    script.on_comments(|req: &Req| match (req.slice_id().as_deref(), req.offset()) {
        (None, 0) => comments_reply(&[100]),
        (Some("100"), 0) => comments_reply(&[101]),
        _ => empty_reply(),
    });
    let run = chan.run(official_source(&r, "alice", &chan, slow_live_refresh()));
    wait_until("100 が流れる", T5, || chan.msg_ids().iter().any(|i| i == "tc-100")).await;
    // 偽サーバを止める(接続試行だけが記録される)
    let down_at = Instant::now();
    r.fake.set_down(true);
    let gaps = |attempts: &[Instant]| -> Vec<Duration> {
        let after: Vec<Instant> = attempts.iter().copied().filter(|t| *t >= down_at).collect();
        after.windows(2).map(|w| w[1].duration_since(w[0])).filter(|g| *g > ms(100)).collect()
    };
    wait_until("バックオフしながらの再試行", T15, || gaps(&r.fake.connection_attempts()).len() >= 2).await;
    let g = gaps(&r.fake.connection_attempts());
    assert!(g[1] > g[0], "間隔が伸びない: {:?} → {:?}", g[0], g[1]);
    // 復旧後は slice_id=100 から再開し、取りこぼしも重複もない
    let up_at = Instant::now();
    r.fake.set_down(false);
    wait_until("101 が流れる", T15, || chan.msg_ids().iter().any(|i| i == "tc-101")).await;
    let first_after = r.fake.comment_calls().into_iter().find(|q| q.at >= up_at).expect("復旧後の要求");
    assert_eq!(first_after.slice_id().as_deref(), Some("100"), "復旧後にカーソルが失われた");
    assert_eq!(chan.msg_ids(), tc_ids(100, 101));
    run.stop().await;
}

// ---------------------------------------------------------------- 停止と秘匿

#[tokio::test]
async fn cancel_stops_source_within_1s() {
    // 認証待ち
    {
        let r = rig().await;
        Script::new().install(&r.fake);
        let chan = Chan::new();
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("認証待ち", T5, || chan.has_notice(LOGIN_REQUIRED_NOTICE)).await;
        run.stop().await;
    }
    // 未配信
    {
        let (r, _script, chan) = start_logged_in().await;
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("未配信", T5, || saw_offline(&chan)).await;
        run.stop().await;
    }
    // 配信中
    {
        let (r, script, chan) = start_logged_in().await;
        live_switch(&script, true);
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("配信中のコメント要求", T5, || !r.fake.comment_calls().is_empty()).await;
        run.stop().await;
    }
    // 予算待ち(Reset が遠い 429 の直後)
    {
        let (r, script, chan) = start_logged_in().await;
        script.on_user(|_: &Req| error_reply(429, 0).header("X-RateLimit-Reset", &(epoch_s() + 60).to_string()));
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("429 を受ける", T5, || r.fake.total() >= 1).await;
        settle(100).await;
        run.stop().await;
    }
    // ページ取得ループの途中(コメント要求が保留されたまま)
    {
        let (r, script, chan) = start_logged_in().await;
        live_switch(&script, true);
        let gate = Arc::new(Notify::new());
        script.on_comments(move |_: &Req| empty_reply().hold(gate.clone()));
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("保留されたコメント要求", T5, || !r.fake.comment_calls().is_empty()).await;
        run.stop().await;
    }
}

#[tokio::test]
async fn emitted_channel_matches_stats_scope_key_for_mixed_case_identifier() {
    let (r, script, chan) = start_logged_in().await;
    live_switch(&script, true);
    script.on_comments(|req: &Req| {
        if req.slice_id().is_none() && req.offset() == 0 {
            comments_reply(&[1])
        } else {
            empty_reply()
        }
    });
    let run = chan.run(official_source(&r, "C:Alice", &chan, tuning()));
    wait_until("コメント 1 件", T5, || !chan.msgs().is_empty()).await;
    let expected = channel_name("C:Alice");
    assert!(chan.msgs().iter().all(|m| m.channel == expected), "ChatMessage.channel が {expected:?} ではない");
    assert!(chan.metas().iter().all(|m| m.channel == expected), "metadata.channel が {expected:?} ではない");
    run.stop().await;
}

/// 出力(ChatMessage の JSON・metadata の Debug)にトークン・URL が無いこと。
fn check_outputs(chan: &Chan, hosts: &[String]) {
    let mut texts: Vec<(&str, String)> = Vec::new();
    for m in chan.msgs() {
        texts.push(("ChatMessage の JSON", serde_json::to_string(&m).unwrap()));
    }
    for m in chan.metas() {
        texts.push(("YoutubeMetadataUpdate の Debug", format!("{m:?}")));
    }
    for (place, text) in texts {
        assert_absent(place, &text, &[TOK_A, TOK_B, TOK_C, "Bearer"]);
        for h in hosts {
            assert_absent(place, &text, &[h.as_str()]);
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn secrets_never_reach_logs_metadata_or_messages() {
    let (logs, _guard) = capture_logs();
    let mut hosts: Vec<String> = Vec::new();
    let mut log = String::new();
    let mut chans: Vec<Chan> = Vec::new();

    // 成功
    {
        let (r, script, chan) = start_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        live_switch(&script, true);
        script.on_comments(|req: &Req| {
            if req.slice_id().is_none() && req.offset() == 0 {
                comments_reply(&[1])
            } else {
                empty_reply()
            }
        });
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("成功: コメント 1 件", T5, || !chan.msgs().is_empty()).await;
        run.stop().await;
        log.push_str(&logs.take());
        chans.push(chan);
    }
    // 現世代の 401
    {
        let (r, script, chan) = start_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        live_switch(&script, true);
        script.on_comments(|_: &Req| error_reply(401, 1000));
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("401: 再ログイン案内", T5, || chan.has_notice(RELOGIN_REQUIRED_NOTICE)).await;
        run.stop().await;
        log.push_str(&logs.take());
        chans.push(chan);
    }
    // 世代違いの 401
    {
        let (r, _script, chan) = start_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        let (run, _) = late_401_scenario(&r, &chan).await;
        wait_until("世代違い 401: コメント", T5, || !chan.msgs().is_empty()).await;
        run.stop().await;
        log.push_str(&logs.take());
        chans.push(chan);
    }
    // 429(最初の要求だけ)
    {
        let (r, script, chan) = start_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        live_switch(&script, true);
        script.on_comments(|req: &Req| {
            if req.slice_id().is_none() && req.offset() == 0 {
                comments_reply(&[1])
            } else {
                empty_reply()
            }
        });
        let first = Arc::new(AtomicBool::new(true));
        script.on_user(move |_: &Req| {
            if first.swap(false, Ordering::SeqCst) {
                error_reply(429, 0).header("X-RateLimit-Reset", &(epoch_s() + 1).to_string())
            } else {
                Reply::json(200, json!({"user": {"id": "1", "screen_id": "alice", "name": "Alice"}}))
            }
        });
        let run = chan.run(official_source(&r, "alice", &chan, tuning()));
        wait_until("429 の後に再開してコメント", Duration::from_secs(8), || !chan.msgs().is_empty()).await;
        run.stop().await;
        log.push_str(&logs.take());
        chans.push(chan);
    }

    assert_absent("ログ", &log, &[TOK_A, TOK_B, TOK_C, "Bearer", "BODY-MARKER"]);
    for h in &hosts {
        assert_absent("ログ", &log, &[h.as_str()]);
    }
    for chan in &chans {
        check_outputs(chan, &hosts);
    }
}
