// registered in main.rs by story TC3-304
//! PR3' 受け入れテスト 4.6: `UnreadableCounter` と `TwitcastingRoutedSource`(mode 切替)。
//! 非公式は偽サイト(視聴ページ・eventpubsuburl・listall)、公式は偽 API。信号の入力は §2 の表 H1〜H10。
//! 偽サイトは「1 回の視聴ページ取得 = 1 ステップ」で応答を順に返す(`Step`: 視聴ページ + その直後の eventpubsuburl)。
//!
//! 注意: H6・H8・H9 は既存(PR2')の Disconnected バックオフ(1 秒〜)を通るので、該当テストは数秒かかる。
#![allow(dead_code)]

use crate::twitcasting_common::*;
use crate::twitcasting_common_official::*;
use fast_comment_lib::config::TwitcastingMode;
use fast_comment_lib::sources::twitcasting::{channel_name, UnofficialSignal, UNAVAILABLE_NOTICE};
use fast_comment_lib::sources::twitcasting_mode::{
    TwitcastingRoutedSource, UnreadableCounter, CARRY_OVER_IDS, UNOFFICIAL_BLOCKED_NOTICE, UNREADABLE_SWITCH_THRESHOLD,
};
use fast_comment_lib::sources::twitcasting_official::{LOGIN_REQUIRED_NOTICE, RELOGIN_REQUIRED_NOTICE};
use serde_json::json;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const T5: Duration = Duration::from_secs(5);
const T15: Duration = Duration::from_secs(15);
const T30: Duration = Duration::from_secs(30);

const PAGE_H1: &str = "<html><body><p>maintenance</p></body></html>";
const PAGE_H2: &str = r#"<html><body data-is-onlive="true"></body></html>"#;
const PAGE_H3: &str = r#"<html><body data-is-onlive="maybe"><div data-movie-id="42"></div></body></html>"#;
const PAGE_H4: &str = r#"<html><body data-is-onlive="false"></body></html>"#;
const PAGE_LIVE: &str = r#"<html><body data-is-onlive="true"><div data-movie-id="42"></div></body></html>"#;

// ---------------------------------------------------------------- 純粋: カウンタ

#[test]
fn counter_switches_on_third_consecutive_unreadable() {
    assert_eq!(UNREADABLE_SWITCH_THRESHOLD, 3);
    assert_eq!(CARRY_OVER_IDS, 256);
    let mut c = UnreadableCounter::new();
    assert!(!c.reached());
    c.observe(UnofficialSignal::Unreadable);
    c.observe(UnofficialSignal::Unreadable);
    assert!(!c.reached(), "2 回では切り替えない");
    c.observe(UnofficialSignal::Unreadable);
    assert!(c.reached(), "3 回目で切り替える");
}

#[test]
fn counter_resets_on_healthy_and_ignores_neutral() {
    let mut c = UnreadableCounter::new();
    for s in [
        UnofficialSignal::Unreadable,
        UnofficialSignal::Unreadable,
        UnofficialSignal::Healthy,
        UnofficialSignal::Unreadable,
        UnofficialSignal::Unreadable,
    ] {
        c.observe(s);
    }
    assert!(!c.reached(), "Healthy で 0 に戻る");

    let mut c = UnreadableCounter::new();
    for s in [
        UnofficialSignal::Unreadable,
        UnofficialSignal::Neutral,
        UnofficialSignal::Unreadable,
        UnofficialSignal::Neutral,
        UnofficialSignal::Unreadable,
    ] {
        c.observe(s);
    }
    assert!(c.reached(), "Neutral は数えず、リセットもしない");
}

// ---------------------------------------------------------------- 偽サイトと環境

#[derive(Clone)]
struct Step {
    html: Reply,
    pubsub: Option<Reply>,
}

fn s_html(page: &str) -> Step {
    Step { html: Reply::html(page), pubsub: None }
}
fn h1() -> Step {
    s_html(PAGE_H1)
}
fn h2() -> Step {
    s_html(PAGE_H2)
}
fn h3() -> Step {
    s_html(PAGE_H3)
}
fn h4() -> Step {
    s_html(PAGE_H4)
}
/// H5: 配信中のページ + eventpubsuburl が `{}`(限定配信 = Private)。
fn h5() -> Step {
    Step { html: Reply::html(PAGE_LIVE), pubsub: Some(Reply::json(200, json!({}))) }
}
/// H6: 配信中のページ + eventpubsuburl が 2xx で壊れた本文。
fn h6(body: &str) -> Step {
    Step { html: Reply::html(PAGE_LIVE), pubsub: Some(Reply::new(200, body)) }
}
/// H7: eventpubsuburl が HTTP 3xx/4xx。
fn h7(status: u16) -> Step {
    Step { html: Reply::html(PAGE_LIVE), pubsub: Some(Reply::new(status, "")) }
}
/// H8: eventpubsuburl が HTTP 5xx。
fn h8() -> Step {
    Step { html: Reply::html(PAGE_LIVE), pubsub: Some(Reply::new(503, "")) }
}
/// H9: 視聴ページが HTTP 5xx。
fn h9() -> Step {
    Step { html: Reply::new(503, ""), pubsub: None }
}
/// H10: 視聴ページが 404(ユーザー無し)。
fn h10() -> Step {
    Step { html: Reply::new(404, ""), pubsub: None }
}

struct Site {
    fake: FakeApi,
    queue: Arc<Mutex<VecDeque<Step>>>,
    then: Arc<Mutex<Step>>,
    listall: Arc<Mutex<Reply>>,
}

impl Site {
    /// `steps` を視聴ページの取得ごとに順に返し、尽きたら `then` を返し続ける。視聴ページの経路は `/alice`。
    async fn start(steps: Vec<Step>, then: Step) -> Site {
        let fake = FakeApi::start().await;
        let queue = Arc::new(Mutex::new(VecDeque::from(steps)));
        let then = Arc::new(Mutex::new(then));
        let listall = Arc::new(Mutex::new(Reply::json(200, json!({"comments": []}))));
        let current: Arc<Mutex<Option<Step>>> = Arc::new(Mutex::new(None));
        {
            let (queue, then, listall, current) = (queue.clone(), then.clone(), listall.clone(), current.clone());
            fake.set_responder(move |req: &Req| {
                if req.path == "/eventpubsuburl.php" {
                    let step = current.lock().unwrap().clone();
                    return step.and_then(|s| s.pubsub).unwrap_or_else(|| Reply::json(200, json!({})));
                }
                if req.path.ends_with("/userajax.php") {
                    return listall.lock().unwrap().clone();
                }
                if req.path != "/alice" {
                    return Reply::new(404, "");
                }
                let next = queue.lock().unwrap().pop_front();
                let step = match next {
                    Some(s) => s,
                    None => then.lock().unwrap().clone(),
                };
                *current.lock().unwrap() = Some(step.clone());
                step.html
            });
        }
        Site { fake, queue, then, listall }
    }
    /// 以後の視聴ページを `step` にする(未消化のステップは捨てる)。
    fn set_then(&self, step: Step) {
        self.queue.lock().unwrap().clear();
        *self.then.lock().unwrap() = step;
    }
    fn set_listall(&self, reply: Reply) {
        *self.listall.lock().unwrap() = reply;
    }
    fn html_count(&self) -> usize {
        self.fake.count(|q| q.path == "/alice")
    }
}

struct M {
    r: Rig,
    script: Arc<Script>,
    site: Site,
    chan: Chan,
}

async fn setup(logged_in: bool, steps: Vec<Step>, then: Step) -> M {
    let r = if logged_in { rig_logged_in().await } else { rig().await };
    let script = Script::new();
    script.install(&r.fake);
    let site = Site::start(steps, then).await;
    M { r, script, site, chan: Chan::new() }
}

fn routed(m: &M, mode: TwitcastingMode) -> TwitcastingRoutedSource<fast_comment_lib::integrations::MemorySecretStore> {
    let unofficial = unofficial_source("alice", &m.site.fake.base, &m.chan);
    let official = official_source(&m.r, "alice", &m.chan, tuning());
    TwitcastingRoutedSource::new(mode, unofficial, official, m.r.auth.subscribe(), Some(m.chan.meta_tx()))
}

fn official_started(m: &M) -> bool {
    !m.r.fake.requests().is_empty()
}

fn has_blocked(m: &M) -> bool {
    m.chan.has_notice(UNOFFICIAL_BLOCKED_NOTICE)
}

fn listall_41_42() -> Reply {
    let item = |id: u64| {
        json!({
            "type": "comment", "id": id, "message": format!("m{id}"), "createdAt": 1_700_000_000_000u64 + id,
            "author": {"id": format!("u{id}"), "name": format!("N{id}")}
        })
    };
    Reply::json(200, json!({ "comments": [item(41), item(42)] }))
}

// ---------------------------------------------------------------- auto: 切替の条件

#[tokio::test]
async fn auto_switches_to_official_after_3_unreadable_when_logged_in() {
    let m = setup(true, vec![h1(), h2(), h3()], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式への切替", T5, || !m.r.fake.user_calls().is_empty()).await;
    settle(200).await;
    assert_eq!(m.site.html_count(), 3, "視聴ページの取得はちょうど 3 回で止まる");
    run.stop().await;
}

#[tokio::test]
async fn auto_explicit_offline_resets_count() {
    // H1,H1,H4,H1 → 公式要求 0。続けて H1,H1 で H4 の後 3 回連続 → 切替
    let m = setup(true, vec![h1(), h1(), h4(), h1(), h1(), h1()], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式への切替", T5, || official_started(&m)).await;
    let first_official = m.r.fake.requests()[0].at;
    let before = m.site.fake.count(|q| q.path == "/alice" && q.at < first_official);
    assert_eq!(before, 6, "公式の最初の要求までに視聴ページを 6 回取っているはず");
    settle(200).await;
    assert_eq!(m.site.html_count(), 6);
    run.stop().await;
}

#[tokio::test]
async fn neutral_outcomes_do_not_count_toward_switch() {
    // H5・H7・H8・H9・H10 を各 2 回ずつ、H1 を間に 2 回だけ挟む。H8・H9 の間には H5 を置いて PR2' のバックオフを戻す
    let steps = vec![
        h5(),
        h5(),
        h7(404),
        h7(302),
        h1(),
        h1(),
        h8(),
        h5(),
        h8(),
        h5(),
        h9(),
        h5(),
        h9(),
        h5(),
        h10(),
        h10(),
    ];
    let n = steps.len();
    let m = setup(true, steps, h5()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("全ステップを消化", T30, || m.site.html_count() >= n + 3).await;
    assert_eq!(m.r.fake.total(), 0, "Neutral・2 回の Unreadable で公式へ切り替わった");
    assert!(!has_blocked(&m), "閾値に達していないのに blocked の案内が出た");
    run.stop().await;
}

#[tokio::test]
async fn malformed_eventpubsub_counts_toward_switch() {
    let m = setup(true, vec![h6("not json"), h6("[1]"), h6(r#"{"url":1}"#)], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式への切替", T15, || !m.r.fake.user_calls().is_empty()).await;
    assert_eq!(m.site.html_count(), 3, "Malformed 3 回で切り替わる");
    run.stop().await;
}

#[tokio::test]
async fn private_eventpubsub_never_switches() {
    let m = setup(true, vec![], h5()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("5 回以上の確認", T5, || m.site.html_count() >= 6).await;
    assert_eq!(m.r.fake.total(), 0, "限定配信(Private)で公式へ切り替わった");
    assert_eq!(m.chan.notices().last().map(String::as_str), Some(UNAVAILABLE_NOTICE));
    assert!(!has_blocked(&m));
    run.stop().await;
}

#[tokio::test]
async fn neutral_does_not_reset_count() {
    let m = setup(true, vec![h1(), h9(), h1(), h5(), h1()], h5()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式への切替", T15, || !m.r.fake.user_calls().is_empty()).await;
    assert_eq!(m.site.html_count(), 5, "H1,H9,H1,H5,H1 の 5 回目で切り替わる");
    run.stop().await;
}

// ---------------------------------------------------------------- auto: 未ログイン

#[tokio::test]
async fn auto_logged_out_shows_blocked_notice_and_keeps_unofficial() {
    let m = setup(false, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("blocked の案内", T5, || has_blocked(&m)).await;
    settle(50).await;
    assert_eq!(m.r.fake.total(), 0, "未ログインで公式へ要求が届いた");
    let n1 = m.site.html_count();
    settle(200).await;
    assert!(m.site.html_count() >= n1 + 3, "視聴ページの取得が unreadable_recheck ごとに続くはず");
    run.stop().await;
}

#[tokio::test]
async fn auto_blocked_then_login_switches_to_official() {
    let m = setup(false, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("blocked の案内", T5, || has_blocked(&m)).await;
    login(&m.r.auth, m.r.port, TOK_A).await;
    // 契約: unreadable_recheck(30ms) + 1 秒以内
    let within = ms(30) + Duration::from_secs(1) + SLACK;
    wait_until("公式の要求", within, || !m.r.fake.user_calls().is_empty()).await;
    run.stop().await;
}

#[tokio::test]
async fn auto_blocked_then_unofficial_recovers_stays_unofficial() {
    let m = setup(false, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("blocked の案内", T5, || has_blocked(&m)).await;
    m.site.set_then(h4());
    wait_until("live=false で notice 無し", T5, || {
        let metas = m.chan.metas();
        let last_blocked = metas.iter().rposition(|x| x.notice.as_deref() == Some(UNOFFICIAL_BLOCKED_NOTICE));
        match last_blocked {
            Some(i) => metas.iter().skip(i + 1).any(|x| x.live == Some(false) && x.notice.is_none()),
            None => false,
        }
    })
    .await;
    settle(100).await;
    assert_eq!(m.r.fake.total(), 0, "非公式が回復したのに公式へ切り替わった");
    run.stop().await;
}

// ---------------------------------------------------------------- auto: 切替後

#[tokio::test]
async fn auto_stays_official_after_switch() {
    let m = setup(true, vec![h1(), h1(), h1()], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式への切替", T5, || !m.r.fake.user_calls().is_empty()).await;
    for page in [h4(), h5()] {
        m.site.set_then(page);
        let n1 = m.site.html_count();
        settle(200).await;
        assert_eq!(m.site.html_count(), n1, "切替後に非公式の視聴ページを取りに行った");
    }
    assert!(!m.r.fake.live_calls().is_empty(), "公式は動き続けるはず");
    run.stop().await;
}

#[tokio::test]
async fn auto_official_401_does_not_fall_back_to_unofficial() {
    let m = setup(true, vec![h1(), h1(), h1()], h1()).await;
    m.script.on_live(|_: &Req| live_reply("42", "t", 1));
    m.script.on_comments(|_: &Req| error_reply(401, 1000));
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("再ログイン案内", T5, || m.chan.has_notice(RELOGIN_REQUIRED_NOTICE)).await;
    settle(50).await;
    let n1 = m.site.html_count();
    settle(200).await;
    assert_eq!(m.site.html_count(), n1, "401 の後に非公式へ落ちた");
    assert_eq!(n1, 3);
    run.stop().await;
}

#[tokio::test]
async fn switch_carries_over_emitted_ids_and_emits_each_comment_once() {
    // 1. H5 + listall が 41・42 を返す → 2. eventpubsuburl は {}(Neutral) → 3. H1 ×3 で切替
    let m = setup(true, vec![h5(), h1(), h1(), h1()], h1()).await;
    m.site.set_listall(listall_41_42());
    // 4. 公式の途中参加 offset=0 が [43,42,41,40]
    m.script.on_live(|_: &Req| live_reply("42", "t", 1));
    m.script.on_comments(|req: &Req| {
        if req.slice_id().is_none() && req.offset() == 0 {
            comments_reply(&[43, 42, 41, 40])
        } else {
            empty_reply()
        }
    });
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("4 件", T15, || m.chan.msgs().len() >= 4).await;
    settle(300).await;
    let mut ids = m.chan.msg_ids();
    ids.sort();
    assert_eq!(ids, tc_ids(40, 43), "40〜43 が各 1 件(41・42 は非公式で出た 1 回だけ)");
    run.stop().await;
}

// ---------------------------------------------------------------- mode の固定

#[tokio::test]
async fn mode_unofficial_never_uses_token() {
    let m = setup(true, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Unofficial));
    wait_until("10 回の確認", T5, || m.site.html_count() >= 10).await;
    assert_eq!(m.r.fake.total(), 0, "mode=unofficial で公式 API に要求した");
    assert!(!has_blocked(&m));
    run.stop().await;
}

#[tokio::test]
async fn mode_official_without_login_shows_login_notice_and_no_unofficial_fallback() {
    let m = setup(false, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Official));
    wait_until("ログイン案内", T5, || m.chan.has_notice(LOGIN_REQUIRED_NOTICE)).await;
    settle(200).await;
    assert_eq!(m.site.fake.total(), 0, "mode=official が非公式に落ちた");
    assert_eq!(m.r.fake.total(), 0);
    run.stop().await;
}

#[tokio::test]
async fn mode_official_logged_in_never_touches_unofficial() {
    let m = setup(true, vec![], h1()).await;
    let run = m.chan.run(routed(&m, TwitcastingMode::Official));
    wait_until("公式の要求", T5, || !m.r.fake.user_calls().is_empty()).await;
    settle(200).await;
    assert_eq!(m.site.fake.total(), 0, "mode=official が非公式に触れた");
    run.stop().await;
}

// ---------------------------------------------------------------- 引き継ぎ

#[tokio::test]
async fn switch_reuses_channel_name_and_live_event_dedupe() {
    // 非公式が movie 42 の開始を告知する(eventpubsuburl が返す WS の接続先は閉じたポート)。その後 H1 ×3 で切替
    let announce = Step {
        html: Reply::html(PAGE_LIVE),
        pubsub: Some(Reply::json(200, json!({"url": "ws://127.0.0.1:1/ws"}))),
    };
    let m = setup(true, vec![announce, h1(), h1(), h1()], h1()).await;
    m.site.set_listall(Reply::json(
        200,
        json!({"comments": [{"type": "comment", "id": 41, "message": "m41", "createdAt": 1_700_000_000_041u64,
                             "author": {"id": "u41", "name": "N41"}}]}),
    ));
    m.script.on_live(|_: &Req| live_reply("42", "t", 1));
    m.script.on_comments(|req: &Req| {
        if req.slice_id().is_none() && req.offset() == 0 {
            comments_reply(&[42, 41])
        } else {
            empty_reply()
        }
    });
    let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
    wait_until("公式のコメント取得", T15, || !m.r.fake.comment_calls().is_empty()).await;
    settle(300).await;
    assert_eq!(m.chan.started_count(), 1, "LiveEvent::Started は 1 回だけ: {:?}", m.chan.lives());
    let expected = channel_name("alice");
    assert!(m.chan.msgs().iter().all(|x| x.channel == expected), "channel が変わった");
    assert!(m.chan.metas().iter().all(|x| x.channel == expected), "metadata の channel が変わった");
    run.stop().await;
}

// ---------------------------------------------------------------- 停止

#[tokio::test]
async fn cancel_stops_routed_source_within_1s() {
    // 非公式中
    {
        let m = setup(true, vec![], h1()).await;
        let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
        wait_until("非公式の取得", T5, || m.site.html_count() >= 1).await;
        run.stop().await;
    }
    // blocked 中
    {
        let m = setup(false, vec![], h1()).await;
        let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
        wait_until("blocked の案内", T5, || has_blocked(&m)).await;
        run.stop().await;
    }
    // 公式中
    {
        let m = setup(true, vec![h1(), h1(), h1()], h1()).await;
        let run = m.chan.run(routed(&m, TwitcastingMode::Auto));
        wait_until("公式への切替", T5, || !m.r.fake.user_calls().is_empty()).await;
        run.stop().await;
    }
}
