//! PR4 受け入れテスト: 結果の分類、キュー・再送・レート制限・自動停止、実際の HTTP 送信(設計書 §3.5・§5.6)。
//! 分類は純関数、ランタイムは偽の送信器 + 時刻停止(`start_paused = true`)、`sender_*` だけ実ソケット。
//!
//! 実装者が満たす契約(付録 D に加えて、このファイルが前提にする物):
//! - `webhook::{OutboundRequest, Sender, ReqwestSender, WebhookRuntime, WebhookStatus, TestSendResult}` は
//!   `webhook` の直下から取れる。`delivery::{Outcome, Decision, classify}` はそのモジュール。
//! - `Outcome` と `Decision` は `Debug + Clone + PartialEq`。`Decision::Retry.wait` が `None` なら通常の待ち(2 秒・8 秒・30 秒)。
//! - `OutboundRequest::new(url: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> OutboundRequest`(付録 D に無いので追加)。
//!   `headers` / `body` は pub。`Debug` は URL を出さない。
//! - `classify` の 429: Discord は `max(header, body) + 250ms`(片方だけなら、その方 + 250ms)。
//!   汎用は `header` があれば**そのまま**(余白なし)、無ければ `wait: None`。両形式とも `header`・`body` が無ければ `wait: None`。
//! - 3xx(301/302/307/308)は `DropAndPause { reason: "redirected" }`。理由の文字列は
//!   `"unauthorized" | "notFound" | "redirected" | "unreachable"`(`integrations.json` の `paused.reason` もこの値)。
//! - `Decision::Drop { uncertain: false }` は 400 などその他の 4xx。`ReadFailed` は `TimedOutAfterSend` と同じ扱い。
//! - ランタイムの時間はすべて `tokio::time`(`Instant` / `sleep`)。`new` と `reload` は tokio の実行時の中で呼ばれ、
//!   送り先ごとのワーカーは `tokio::spawn` する。バケットは満杯で始まる(容量 5)。
//!   再送の待ちの揺らぎは ±20%。再送 1 回ごとに同じ本文・同じ `eventId`。
//! - `WebhookRuntime::status()` の各要素の JSON: `id`、`state`(`"active" | "disabled" | "paused" | "noUrl"`)、
//!   `pausedReason`(`paused` の時だけ文字列)、`delivered`・`failed`・`dropped`・`uncertain`・`queued`(件数)、
//!   `last`(`null` か `{ atMs, httpStatus: 数値 | null, kind: 文字列 }`)。
//!   `failed` = 試行を尽くした/恒久失敗で諦めた配送。`dropped` = 送らずに捨てた物(溢れ・期限切れ・停止/無効化で破棄)。
//!   `uncertain` = Discord の送信後タイムアウト(届いたか不明)。
//! - `test_send` の結果の JSON: `{ "ok": bool, "status": 数値 | null, "errorKind": 文字列 | null }`。
//!   再送せず 1 回だけ送る。停止は解除しない。
//! - 汎用 JSON の署名鍵は、資格情報ストアの `signingSecret`(文字列)の UTF-8 バイト列。

use crate::webhook_common::*;
use fast_comment_lib::integrations::{self, LoadOutcome, WebhookFormat};
use fast_comment_lib::webhook::delivery::{classify, Decision, Outcome};
use fast_comment_lib::webhook::{OutboundRequest, ReqwestSender, Sender, WebhookRuntime};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn st(code: u16) -> Outcome {
    status(code)
}

fn st_retry_after(code: u16, header: Option<u64>, body: Option<u64>) -> Outcome {
    Outcome::Status {
        code,
        header_retry_after: header.map(Duration::from_secs),
        body_retry_after: body.map(Duration::from_secs),
        remaining_zero_reset_after: None,
    }
}

fn secs(d: Duration) -> f64 {
    d.as_secs_f64()
}

fn paused_in_file(dir: &std::path::Path) -> Option<(String, u64)> {
    match integrations::load(dir, NOW).unwrap() {
        LoadOutcome::Loaded(f) => f.webhooks[0].paused.clone().map(|p| (p.reason, p.at_ms)),
        _ => panic!("Loaded を期待"),
    }
}

// ---- 分類(純関数) ----

#[test]
fn classify_200_and_204_are_delivered() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        assert_eq!(classify(f.clone(), &st(200)), Decision::Delivered);
        assert_eq!(classify(f, &st(204)), Decision::Delivered);
    }
}

#[test]
fn classify_connect_failure_is_retry_for_both_formats() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        assert_eq!(classify(f, &Outcome::ConnectFailed), Decision::Retry { wait: None });
    }
}

#[test]
fn classify_timeout_after_send_is_uncertain_drop_for_discord() {
    assert_eq!(classify(WebhookFormat::Discord, &Outcome::TimedOutAfterSend), Decision::Drop { uncertain: true });
    assert_eq!(classify(WebhookFormat::Discord, &Outcome::ReadFailed), Decision::Drop { uncertain: true });
}

#[test]
fn classify_timeout_after_send_is_retry_for_generic() {
    assert_eq!(classify(WebhookFormat::Generic, &Outcome::TimedOutAfterSend), Decision::Retry { wait: None });
    assert_eq!(classify(WebhookFormat::Generic, &Outcome::ReadFailed), Decision::Retry { wait: None });
}

#[test]
fn classify_429_uses_larger_of_body_and_header_retry_after_plus_margin_for_discord() {
    let margin = Duration::from_millis(250);
    assert_eq!(
        classify(WebhookFormat::Discord, &st_retry_after(429, Some(2), Some(5))),
        Decision::Retry { wait: Some(Duration::from_secs(5) + margin) }
    );
    assert_eq!(
        classify(WebhookFormat::Discord, &st_retry_after(429, Some(9), Some(5))),
        Decision::Retry { wait: Some(Duration::from_secs(9) + margin) }
    );
    assert_eq!(
        classify(WebhookFormat::Discord, &st_retry_after(429, None, Some(4))),
        Decision::Retry { wait: Some(Duration::from_secs(4) + margin) }
    );
    assert_eq!(
        classify(WebhookFormat::Discord, &st_retry_after(429, Some(3), None)),
        Decision::Retry { wait: Some(Duration::from_secs(3) + margin) }
    );
}

#[test]
fn classify_429_uses_retry_after_header_for_generic_or_backoff_when_absent() {
    assert_eq!(
        classify(WebhookFormat::Generic, &st_retry_after(429, Some(7), None)),
        Decision::Retry { wait: Some(Duration::from_secs(7)) }
    );
    assert_eq!(classify(WebhookFormat::Generic, &st_retry_after(429, None, None)), Decision::Retry { wait: None });
    assert_eq!(classify(WebhookFormat::Discord, &st_retry_after(429, None, None)), Decision::Retry { wait: None });
}

#[test]
fn classify_408_425_500_502_503_504_are_retry() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        for code in [408, 425, 500, 502, 503, 504] {
            assert_eq!(classify(f.clone(), &st(code)), Decision::Retry { wait: None }, "{code}");
        }
    }
}

#[test]
fn classify_401_403_pause_as_unauthorized() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        for code in [401, 403] {
            assert_eq!(classify(f.clone(), &st(code)), Decision::DropAndPause { reason: "unauthorized" }, "{code}");
        }
    }
}

#[test]
fn classify_404_410_pause_as_not_found() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        for code in [404, 410] {
            assert_eq!(classify(f.clone(), &st(code)), Decision::DropAndPause { reason: "notFound" }, "{code}");
        }
    }
}

#[test]
fn classify_301_302_307_308_pause_as_redirected() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        for code in [301, 302, 307, 308] {
            assert_eq!(classify(f.clone(), &st(code)), Decision::DropAndPause { reason: "redirected" }, "{code}");
        }
    }
}

#[test]
fn classify_400_and_422_are_dropped_without_pause() {
    for f in [WebhookFormat::Discord, WebhookFormat::Generic] {
        for code in [400, 422] {
            assert_eq!(classify(f.clone(), &st(code)), Decision::Drop { uncertain: false }, "{code}");
        }
    }
}

// ---- ランタイム(偽の送信器 + 時刻停止) ----

#[tokio::test(start_paused = true)]
async fn runtime_retries_at_2s_8s_30s_within_20_percent_then_gives_up_after_4_attempts() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(500); 10]);

    env.rt.handle_message(&superchat("r1", "¥500"));
    advance(200).await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 4, "初回 + 再送 3 回");
    let gaps: Vec<f64> = calls.windows(2).map(|w| secs(w[1].at - w[0].at)).collect();
    for (gap, base) in gaps.iter().zip([2.0, 8.0, 30.0]) {
        assert!(*gap >= base * 0.8 - 0.05 && *gap <= base * 1.2 + 0.05, "待ち {gap} 秒(基準 {base} 秒)");
    }
    assert_eq!(env.status_of(&id)["failed"], 1);
}

#[tokio::test(start_paused = true)]
async fn runtime_stops_retrying_after_success() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(500), st(204)]);

    env.rt.handle_message(&superchat("r1", "¥500"));
    advance(200).await;

    assert_eq!(env.sender.calls().len(), 2);
    assert_eq!(env.status_of(&id)["delivered"], 1);
}

#[tokio::test(start_paused = true)]
async fn runtime_reuses_same_event_id_and_body_on_retry() {
    let env = wh_env().await;
    env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(500), st(500), st(204)]);

    env.rt.handle_message(&superchat("r1", "¥500"));
    advance(100).await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 3);
    let event_id = calls[0].json()["eventId"].as_str().unwrap().to_string();
    assert!(!event_id.is_empty());
    for c in &calls {
        assert_eq!(c.body, calls[0].body, "本文はバイト単位で同じ");
        assert_eq!(c.header("x-fastcomment-event-id"), Some(event_id.as_str()));
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_sends_one_at_a_time_in_arrival_order_per_webhook() {
    let env = wh_env().await;
    env.add_generic(GENERIC_URL_A).await;
    env.sender.delay("hook-a", Duration::from_secs(3));

    for id in ["a", "b", "c"] {
        env.rt.handle_message(&superchat(id, "¥500"));
    }
    advance(60).await;

    let calls = env.sender.calls();
    let ids: Vec<String> = calls.iter().map(|c| c.comment_id()).collect();
    assert_eq!(ids, ["a", "b", "c"]);
    assert_eq!(env.sender.max_in_flight(), 1, "同時 1 送信");
    for w in calls.windows(2) {
        assert!(secs(w[1].at - w[0].at) >= 3.0, "前の応答を待ってから次を送る");
    }
}

#[tokio::test(start_paused = true)]
async fn runtime_slow_webhook_does_not_delay_other_webhooks() {
    let env = wh_env().await;
    let slow = env.add_generic(GENERIC_URL_A).await;
    let fast = env.add_generic(GENERIC_URL_B).await;
    env.sender.delay("hook-a", Duration::from_secs(100));

    let t0 = tokio::time::Instant::now();
    env.rt.handle_message(&superchat("x1", "¥500"));
    advance(1).await;

    let fast_calls = env.sender.calls_for("hook-b");
    assert_eq!(fast_calls.len(), 1);
    assert!(secs(fast_calls[0].at - t0) < 1.0);
    assert_eq!(env.status_of(&fast)["delivered"], 1);
    assert_eq!(env.status_of(&slow)["delivered"], 0, "遅い方はまだ応答待ち");
    advance(120).await;
    assert_eq!(env.status_of(&slow)["delivered"], 1);
}

#[tokio::test(start_paused = true)]
async fn runtime_bucket_allows_burst_of_5_then_one_per_2s() {
    let env = wh_env().await;
    env.add_generic(GENERIC_URL_A).await;

    let t0 = tokio::time::Instant::now();
    for i in 0..7 {
        env.rt.handle_message(&superchat(&format!("b{i}"), "¥500"));
    }
    advance(30).await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 7);
    let at: Vec<f64> = calls.iter().map(|c| secs(c.at - t0)).collect();
    assert!(at[..5].iter().all(|t| *t < 1.0), "最初の 5 件は即時: {at:?}");
    assert!((1.9..=3.0).contains(&at[5]), "6 件目は約 2 秒後: {at:?}");
    assert!((3.9..=5.0).contains(&at[6]), "7 件目は約 4 秒後: {at:?}");
}

#[tokio::test(start_paused = true)]
async fn runtime_respects_discord_remaining_zero_reset_after() {
    let env = wh_env().await;
    env.add_discord().await;
    env.sender.script(
        "discord.com",
        vec![Outcome::Status {
            code: 204,
            header_retry_after: None,
            body_retry_after: None,
            remaining_zero_reset_after: Some(Duration::from_secs(10)),
        }],
    );

    env.rt.handle_message(&superchat("d1", "¥500"));
    env.rt.handle_message(&superchat("d2", "¥500"));
    advance(60).await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 2);
    let gap = secs(calls[1].at - calls[0].at);
    assert!((9.9..=13.0).contains(&gap), "間隔 {gap} 秒");
}

#[tokio::test(start_paused = true)]
async fn runtime_429_pauses_whole_queue_for_retry_after() {
    let env = wh_env().await;
    env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st_retry_after(429, Some(20), None)]);

    for id in ["a", "b", "c"] {
        env.rt.handle_message(&superchat(id, "¥500"));
    }
    advance(15).await;
    assert_eq!(env.sender.calls().len(), 1, "待ちの間は後続も送らない");

    advance(60).await;
    let calls = env.sender.calls();
    let ids: Vec<String> = calls.iter().map(|c| c.comment_id()).collect();
    assert_eq!(ids, ["a", "a", "b", "c"]);
    assert!(secs(calls[1].at - calls[0].at) >= 19.9);
}

#[tokio::test(start_paused = true)]
async fn runtime_queue_overflow_drops_oldest_beyond_100_and_counts_dropped() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.delay_next("hook-a", Duration::from_secs(60));

    env.rt.handle_message(&superchat("m0", "¥500"));
    settle().await; // m0 はワーカーが取って応答待ち
    for i in 1..=105 {
        env.rt.handle_message(&superchat(&format!("m{i}"), "¥500"));
    }
    settle().await;
    assert_eq!(env.status_of(&id)["queued"], 100);
    assert_eq!(env.status_of(&id)["dropped"], 5);

    advance(500).await;
    let ids: Vec<String> = env.sender.calls().iter().map(|c| c.comment_id()).collect();
    let mut expected: Vec<String> = vec!["m0".into()];
    expected.extend((6..=105).map(|i| format!("m{i}")));
    assert_eq!(ids, expected, "古い m1..m5 が捨てられ、残りは到着順");
}

#[tokio::test(start_paused = true)]
async fn runtime_item_older_than_10_minutes_is_expired_not_sent() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.delay_next("hook-a", Duration::from_secs(660));

    env.rt.handle_message(&superchat("old1", "¥500"));
    settle().await;
    env.rt.handle_message(&superchat("old2", "¥500"));
    advance(700).await;

    let ids: Vec<String> = env.sender.calls().iter().map(|c| c.comment_id()).collect();
    assert_eq!(ids, ["old1"], "10 分を過ぎた old2 は送らない");
    assert_eq!(env.status_of(&id)["dropped"], 1);
}

#[tokio::test(start_paused = true)]
async fn runtime_permanent_failure_pauses_webhook_persists_it_and_discards_queue() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(401)]);

    for i in ["a", "b", "c"] {
        env.rt.handle_message(&superchat(i, "¥500"));
    }
    advance(100).await;

    assert_eq!(env.sender.calls().len(), 1, "401 の後は溜まっていた分も送らない");
    let s = env.status_of(&id);
    assert_eq!(s["state"], "paused");
    assert_eq!(s["pausedReason"], "unauthorized");
    assert_eq!(s["queued"], 0);
    let (reason, at_ms) = paused_in_file(env.dir.path()).expect("integrations.json に paused が残る");
    assert_eq!(reason, "unauthorized");
    assert!(at_ms > 0);

    env.rt.handle_message(&superchat("d", "¥500"));
    advance(30).await;
    assert_eq!(env.sender.calls().len(), 1, "停止後に来た物も送らない");
}

#[tokio::test(start_paused = true)]
async fn runtime_ten_consecutive_exhausted_deliveries_pause_as_unreachable() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(500); 100]);

    for i in 0..11 {
        env.rt.handle_message(&superchat(&format!("u{i}"), "¥500"));
    }
    advance(700).await;

    assert_eq!(env.sender.calls().len(), 40, "10 件 × 4 回で止まり、11 件目は送らない");
    let s = env.status_of(&id);
    assert_eq!(s["state"], "paused");
    assert_eq!(s["pausedReason"], "unreachable");
    assert_eq!(paused_in_file(env.dir.path()).unwrap().0, "unreachable");
}

#[tokio::test(start_paused = true)]
async fn runtime_success_resets_consecutive_failure_count() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    let mut script = vec![st(500); 36]; // 9 件分の失敗(4 回ずつ)
    script.push(st(204)); // 10 件目は成功
    script.extend(vec![st(500); 4]); // 11 件目は失敗
    env.sender.script("hook-a", script);

    for i in 0..11 {
        env.rt.handle_message(&superchat(&format!("s{i}"), "¥500"));
    }
    advance(700).await;

    assert_eq!(env.sender.calls().len(), 41);
    assert_eq!(env.status_of(&id)["state"], "active", "連続は 1 件に戻っている");
}

#[tokio::test(start_paused = true)]
async fn runtime_paused_webhook_sends_only_after_resume() {
    let mut d = def(WebhookFormat::Generic, triggers(true, false, false), &[]);
    d.id = "wh-a".into();
    d.paused = paused("notFound");
    let env = wh_env_with(vec![(d, Some(GENERIC_URL_A))]).await;

    env.rt.handle_message(&superchat("p1", "¥500"));
    advance(30).await;
    assert!(env.sender.calls().is_empty(), "停止中は送らない");

    env.it.resume_webhook("wh-a").await.expect("再開できる");
    env.rt.reload().await;
    env.rt.handle_message(&superchat("p2", "¥500"));
    advance(30).await;

    let ids: Vec<String> = env.sender.calls().iter().map(|c| c.comment_id()).collect();
    assert_eq!(ids, ["p2"], "停止中に来た p1 は後から送られない");
}

#[tokio::test(start_paused = true)]
async fn runtime_auto_paused_webhook_resumes_after_resume_and_reload() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(401), st(204)]);

    env.rt.handle_message(&superchat("a", "¥500"));
    advance(30).await;
    let s = env.status_of(&id);
    assert_eq!(s["state"], "paused");
    assert_eq!(s["pausedReason"], "unauthorized");

    env.it.resume_webhook(&id).await.expect("再開できる");
    env.rt.reload().await;
    env.rt.handle_message(&superchat("b", "¥500"));
    advance(30).await;

    assert_eq!(env.sender.calls().len(), 2, "再開後の b は再起動なしで送られる");
    assert_eq!(env.status_of(&id)["state"], "active");
}

#[tokio::test(start_paused = true)]
async fn runtime_disable_or_delete_discards_pending_items() {
    let env = wh_env().await;
    let a = env.add_generic(GENERIC_URL_A).await;
    let b = env.add_generic(GENERIC_URL_B).await;
    env.sender.delay_next("hook-a", Duration::from_secs(60));
    env.sender.delay_next("hook-b", Duration::from_secs(60));

    for i in ["x", "y", "z"] {
        env.rt.handle_message(&superchat(i, "¥500"));
    }
    settle().await;

    let mut off = input("汎用", WebhookFormat::Generic, None);
    off.id = Some(a.clone());
    off.enabled = false;
    off.triggers = triggers(true, false, false);
    env.it.save_webhook(off).await.expect("無効化できる");
    env.it.delete_webhook(&b).await.expect("削除できる");
    env.rt.reload().await;
    advance(300).await;

    assert_eq!(env.sender.calls_for("hook-a").len(), 1, "応答待ちの 1 件だけ。溜まっていた y・z は捨てる");
    assert_eq!(env.sender.calls_for("hook-b").len(), 1);
}

async fn assert_inactive(env: &WhEnv) {
    assert!(!env.rt.has_active_webhooks());
    env.rt.handle_message(&superchat("q", "¥500"));
    advance(30).await;
    assert!(env.sender.calls().is_empty(), "何も送らない");
}

#[tokio::test(start_paused = true)]
async fn runtime_reports_no_active_webhooks_when_all_disabled_paused_or_without_url() {
    // 0 件
    assert_inactive(&wh_env().await).await;

    // 有効 → true、無効化 → false
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    assert!(env.rt.has_active_webhooks());
    let mut off = input("汎用", WebhookFormat::Generic, None);
    off.id = Some(id);
    off.enabled = false;
    env.it.save_webhook(off).await.unwrap();
    env.rt.reload().await;
    assert_inactive(&env).await;

    // 停止中
    let mut paused_def = def(WebhookFormat::Generic, triggers(true, false, false), &[]);
    paused_def.paused = paused("unauthorized");
    assert_inactive(&wh_env_with(vec![(paused_def, Some(GENERIC_URL_A))]).await).await;

    // URL なし(資格情報が無い)
    let no_url = def(WebhookFormat::Generic, triggers(true, false, false), &[]);
    let env = wh_env_with(vec![(no_url, None)]).await;
    assert_inactive(&env).await;
    assert_eq!(env.status_of("wh-test")["state"], "noUrl");
}

#[tokio::test(start_paused = true)]
async fn runtime_generic_request_has_event_id_timestamp_signature_and_user_agent_headers() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.rt.handle_message(&superchat("h1", "¥500"));
    settle().await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert!(c.header("content-type").unwrap_or_default().starts_with("application/json"));
    assert!(c.header("user-agent").unwrap_or_default().starts_with("fast-comment/"));
    let event_id = c.header("x-fastcomment-event-id").expect("Event-Id");
    assert_eq!(event_id.len(), 36);
    assert_eq!(c.json()["eventId"], event_id);

    let ts: u64 = c.header("x-fastcomment-timestamp").expect("Timestamp").parse().expect("UNIX 秒");
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert!(ts.abs_diff(now) < 120, "ts={ts} now={now}");

    let secret = env.it.reveal_signing_secret(&id).await.expect("署名シークレット");
    let mut signed = format!("{ts}.").into_bytes();
    signed.extend_from_slice(&c.body);
    assert_eq!(
        c.header("x-fastcomment-signature").expect("Signature"),
        format!("v1={}", hmac_sha256_hex(secret.as_bytes(), &signed))
    );
}

#[tokio::test(start_paused = true)]
async fn runtime_discord_request_has_no_signature_headers() {
    let env = wh_env().await;
    env.add_discord().await;
    env.rt.handle_message(&superchat("h1", "¥500"));
    settle().await;

    let calls = env.sender.calls();
    assert_eq!(calls.len(), 1);
    let c = &calls[0];
    assert!(c.header("content-type").unwrap_or_default().starts_with("application/json"));
    assert!(c.json()["embeds"].is_array());
    assert!(
        c.headers.iter().all(|(n, _)| !n.to_ascii_lowercase().starts_with("x-fastcomment")),
        "{:?}",
        c.headers.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
}

#[tokio::test(start_paused = true)]
async fn runtime_test_send_bypasses_rules_and_queue_and_reports_status() {
    let env = wh_env().await;
    let busy = env.add_generic(GENERIC_URL_A).await;
    let no_triggers = env
        .add("きっかけなし", WebhookFormat::Generic, GENERIC_URL_B, triggers(false, false, false))
        .await;
    env.sender.delay_next("hook-a", Duration::from_secs(100));

    // busy は応答待ちで、キューのワーカーは塞がっている。
    env.rt.handle_message(&superchat("q1", "¥500"));
    settle().await;

    let r = serde_json::to_value(env.rt.test_send(&busy).await).unwrap();
    assert_eq!(r["ok"], true);
    assert_eq!(r["status"], 204);
    assert!(r["errorKind"].is_null());
    let hook_a = env.sender.calls_for("hook-a");
    assert_eq!(hook_a.len(), 2, "塞がったキューを待たずに送る");
    assert_eq!(hook_a[1].json()["trigger"], "test");

    let r2 = serde_json::to_value(env.rt.test_send(&no_triggers).await).unwrap();
    assert_eq!(r2["ok"], true, "きっかけが全部オフでも送れる");
    assert_eq!(env.sender.calls_for("hook-b").len(), 1);

    env.sender.script("hook-b", vec![st(500)]);
    let r3 = serde_json::to_value(env.rt.test_send(&no_triggers).await).unwrap();
    assert_eq!(r3["ok"], false);
    assert_eq!(r3["status"], 500);
    assert!(!r3["errorKind"].is_null());
    advance(100).await;
    assert_eq!(env.sender.calls_for("hook-b").len(), 2, "テスト送信は再送しない");
}

#[tokio::test(start_paused = true)]
async fn runtime_test_send_works_while_paused_and_does_not_resume() {
    let mut d = def(WebhookFormat::Generic, triggers(true, false, false), &[]);
    d.id = "wh-a".into();
    d.paused = paused("notFound");
    let env = wh_env_with(vec![(d, Some(GENERIC_URL_A))]).await;

    let r = serde_json::to_value(env.rt.test_send("wh-a").await).unwrap();
    assert_eq!(r["ok"], true);
    assert_eq!(env.sender.calls().len(), 1);
    assert_eq!(env.status_of("wh-a")["state"], "paused");
    assert_eq!(paused_in_file(env.dir.path()).unwrap().0, "notFound");
}

#[tokio::test(start_paused = true)]
async fn runtime_status_counts_delivered_failed_dropped_and_last_result() {
    let env = wh_env().await;
    let id = env.add_generic(GENERIC_URL_A).await;
    env.sender.script("hook-a", vec![st(204), st(204), st(500), st(500), st(500), st(500)]);

    for i in ["a", "b", "c"] {
        env.rt.handle_message(&superchat(i, "¥500"));
    }
    advance(200).await;

    let s = env.status_of(&id);
    assert_eq!(s["state"], "active");
    assert_eq!(s["delivered"], 2);
    assert_eq!(s["failed"], 1);
    assert_eq!(s["dropped"], 0);
    assert_eq!(s["queued"], 0);
    assert_eq!(s["last"]["httpStatus"], 500);
    assert!(s["last"]["atMs"].as_u64().unwrap() > 0);
}

// ---- 実際の HTTP 送信(実ソケット、実時間) ----

fn post(url: &str) -> OutboundRequest {
    OutboundRequest::new(
        url,
        vec![("Content-Type".into(), "application/json".into())],
        br#"{"hello":"world"}"#.to_vec(),
    )
}

async fn serve(app: axum::Router) -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

/// 接続を受けるだけで何も返さない相手。
async fn silent_server() -> std::net::SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let mut held = Vec::new();
        while let Ok((sock, _)) = listener.accept().await {
            held.push(sock);
        }
    });
    addr
}

async fn closed_port() -> u16 {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    port
}

#[tokio::test]
async fn sender_does_not_follow_redirects() {
    use axum::http::{header, StatusCode};
    use axum::routing::any;
    let hits = Arc::new(AtomicUsize::new(0));
    let h = hits.clone();
    let app = axum::Router::new()
        .route("/start", any(|| async { (StatusCode::FOUND, [(header::LOCATION, "/target")]) }))
        .route(
            "/target",
            any(move || {
                let h = h.clone();
                async move {
                    h.fetch_add(1, Ordering::SeqCst);
                    StatusCode::OK
                }
            }),
        );
    let addr = serve(app).await;

    let out = ReqwestSender::new().send(post(&format!("http://{addr}/start"))).await;

    assert!(matches!(out, Outcome::Status { code: 302, .. }), "{out:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(hits.load(Ordering::SeqCst), 0, "リダイレクト先は 1 度も呼ばれない");
}

#[tokio::test]
async fn sender_times_out_at_configured_total_timeout() {
    let addr = silent_server().await;
    let sender = ReqwestSender::with_timeouts(Duration::from_millis(200), Duration::from_millis(200));
    let t0 = std::time::Instant::now();

    let out = sender.send(post(&format!("http://{addr}/hook"))).await;

    assert!(matches!(out, Outcome::TimedOutAfterSend), "{out:?}");
    assert!(t0.elapsed() < Duration::from_secs(3), "{:?}", t0.elapsed());
}

#[tokio::test]
async fn sender_reports_connect_failure_for_closed_port() {
    let port = closed_port().await;
    let out = ReqwestSender::new().send(post(&format!("http://127.0.0.1:{port}/hook"))).await;
    assert!(matches!(out, Outcome::ConnectFailed), "{out:?}");
}

#[tokio::test]
async fn delivery_error_display_and_debug_never_contain_url() {
    use axum::http::{header, StatusCode};
    use axum::routing::any;
    const SECRET: &str = "SECRETPATH-xyz987";
    let redirect = serve(
        axum::Router::new()
            .route(&format!("/{SECRET}"), any(|| async { (StatusCode::FOUND, [(header::LOCATION, "/SECRETLOCATION-qq")]) })),
    )
    .await;
    let silent = silent_server().await;
    let closed = closed_port().await;
    let quick = ReqwestSender::with_timeouts(Duration::from_millis(200), Duration::from_millis(200));

    let outcomes = vec![
        ReqwestSender::new().send(post(&format!("http://127.0.0.1:{closed}/{SECRET}"))).await,
        quick.send(post(&format!("http://{silent}/{SECRET}"))).await,
        ReqwestSender::new().send(post(&format!("http://{redirect}/{SECRET}"))).await,
    ];
    for o in &outcomes {
        let shown = format!("{o:?}");
        for leak in [SECRET, "SECRETLOCATION", "127.0.0.1"] {
            assert!(!shown.contains(leak), "{shown} に {leak} が出ている");
        }
    }

    // 画面に返るテスト送信の結果にも出ない(接続拒否)。
    let dir = tempfile::tempdir().unwrap();
    let secrets = fast_comment_lib::integrations::MemorySecretStore::new();
    let it = fast_comment_lib::integrations::Integrations::open(dir.path().to_path_buf(), secrets, NOW).await;
    let view = it
        .save_webhook(input("実送信", WebhookFormat::Generic, Some(&format!("http://127.0.0.1:{closed}/{SECRET}"))))
        .await
        .expect("ループバックの http は保存できる");
    let rt = WebhookRuntime::new(it, ReqwestSender::new());
    rt.reload().await;
    let result = rt.test_send(&view_id(&view)).await;
    let json = serde_json::to_string(&result).unwrap();
    let dbg = format!("{result:?}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&json).unwrap()["ok"], false);
    for leak in [SECRET, "127.0.0.1"] {
        assert!(!json.contains(leak) && !dbg.contains(leak), "{json} / {dbg} に {leak} が出ている");
    }
}

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<OutboundRequest>>>);

impl Sender for Capture {
    async fn send(&self, request: OutboundRequest) -> Outcome {
        self.0.lock().unwrap().push(request);
        status(204)
    }
}

#[tokio::test(start_paused = true)]
async fn outbound_request_and_secret_debug_never_contain_url_or_signing_secret() {
    // 組み立てた要求
    let r = OutboundRequest::new("https://example.com/SECRETPATH-abcd?k=SECRETQUERY", vec![], b"{}".to_vec());
    let shown = format!("{r:?}");
    assert!(!shown.contains("SECRETPATH") && !shown.contains("SECRETQUERY"), "{shown}");

    // URL を持つ入力型
    let i = input("名前", WebhookFormat::Generic, Some("https://example.com/SECRETPATH-abcd"));
    assert!(!format!("{i:?}").contains("SECRETPATH"));

    // ランタイムが実際に作った要求(URL・署名シークレット)
    let dir = tempfile::tempdir().unwrap();
    let secrets = fast_comment_lib::integrations::MemorySecretStore::new();
    let it = fast_comment_lib::integrations::Integrations::open(dir.path().to_path_buf(), secrets, NOW).await;
    let cap = Capture::default();
    let rt = WebhookRuntime::new(it.clone(), cap.clone());
    let view = it
        .save_webhook({
            let mut i = input("汎用", WebhookFormat::Generic, Some(GENERIC_URL_A));
            i.triggers = triggers(true, false, false);
            i
        })
        .await
        .unwrap();
    rt.reload().await;
    let signing = it.reveal_signing_secret(&view_id(&view)).await.unwrap();
    rt.handle_message(&superchat("o1", "¥500"));
    settle().await;

    let reqs = cap.0.lock().unwrap();
    assert_eq!(reqs.len(), 1);
    let shown = format!("{:?}", reqs[0]);
    for leak in ["SECRETPATH", "hook-a.example.test", signing.as_str()] {
        assert!(!shown.contains(leak), "{shown} に {leak} が出ている");
    }
}
