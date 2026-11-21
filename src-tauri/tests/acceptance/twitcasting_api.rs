// registered in main.rs by story TC3-301
//! PR3' 受け入れテスト 4.1: 公式 API クライアント(`TwitcastingApi`)。pr3-acceptance.md §1・§4.1 だけに基づく。
//! 偽 API は `twitcasting_common::FakeApi`(受信を記録、`set_down` でネットワーク断)。トークンは偽の文字列。
#![allow(dead_code)]

use crate::twitcasting_common::*;
use fast_comment_lib::config::AppConfig;
use fast_comment_lib::sources::twitcasting_api::{official_endpoints, ApiError, CommentsQuery};
use fast_comment_lib::sources::twitcasting_auth::{AuthState, TwitcastingEndpoints};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Notify;

fn q() -> CommentsQuery {
    CommentsQuery { slice_id: None, offset: 0 }
}

fn state(auth: &Auth) -> AuthState {
    *auth.subscribe().borrow()
}

/// ログイン済みの環境で、全要求に `reply` を返し、`user("alice")` の結果エラーを返す。
async fn one_error(reply: Reply) -> ApiError {
    let r = rig_logged_in().await;
    r.fake.set_responder(move |_: &Req| reply.clone());
    r.api.user("alice").await.unwrap_err()
}

// ---------------------------------------------------------------- 要求の形

#[tokio::test]
async fn requests_carry_bearer_api_version_and_accept() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| Reply::json(200, json!({"user": {"id": "1"}})));
    assert!(r.api.user("alice").await.is_ok());
    let req = r.fake.requests().pop().expect("要求が届く");
    assert_eq!(req.authorization(), Some("Bearer tok-A-SECRET"));
    assert_eq!(req.headers.get("x-api-version").map(String::as_str), Some("2.0"));
    assert_eq!(req.headers.get("accept").map(String::as_str), Some("application/json"));
}

#[tokio::test]
async fn user_keeps_input_case_in_path() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| Reply::json(200, json!({"user": {"id": "1"}})));
    assert!(r.api.user("C:Alice").await.is_ok());
    let req = r.fake.requests().pop().expect("要求が届く");
    assert!(
        req.path.ends_with("C:Alice") || req.path.ends_with("C%3AAlice"),
        "大小文字を保つ: {}",
        req.path
    );
}

#[tokio::test]
async fn comments_sends_slice_id_offset_and_limit_50() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    let query = CommentsQuery { slice_id: Some("100".into()), offset: 50 };
    assert!(r.api.comments("9", query).await.is_ok());
    let req = r.fake.requests().pop().expect("要求が届く");
    assert_eq!(req.query.get("slice_id").map(String::as_str), Some("100"));
    assert_eq!(req.query.get("offset").map(String::as_str), Some("50"));
    assert_eq!(req.query.get("limit").map(String::as_str), Some("50"));
}

#[tokio::test]
async fn comments_without_slice_id_omits_param() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    assert!(r.api.comments("9", q()).await.is_ok());
    let req = r.fake.requests().pop().expect("要求が届く");
    assert!(!req.query.contains_key("slice_id"), "slice_id が付いている: {:?}", req.query);
    assert_eq!(req.query.get("offset").map(String::as_str), Some("0"));
    assert_eq!(req.query.get("limit").map(String::as_str), Some("50"));
}

#[tokio::test]
async fn comments_without_array_is_empty_not_error() {
    for body in [json!({"comments": "x"}), json!({})] {
        let r = rig_logged_in().await;
        r.fake.set_responder(move |_: &Req| Reply::json(200, body.clone()));
        assert_eq!(r.api.comments("9", q()).await, Ok(vec![]));
    }
}

// ---------------------------------------------------------------- current_live

#[tokio::test]
async fn current_live_404_is_none() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| offline_reply());
    assert_eq!(r.api.current_live("1").await, Ok(None));
}

#[tokio::test]
async fn current_live_with_is_live_false_is_none() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| Reply::json(200, json!({"movie": {"id": "9", "is_live": false}})));
    assert_eq!(r.api.current_live("1").await, Ok(None));
}

#[tokio::test]
async fn current_live_without_movie_id_is_none() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| Reply::json(200, json!({"movie": {}})));
    assert_eq!(r.api.current_live("1").await, Ok(None));
}

#[tokio::test]
async fn current_live_200_returns_movie_and_broadcaster() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| {
        Reply::json(
            200,
            json!({"movie": {"id": "9", "is_live": true, "title": "t"}, "broadcaster": {"id": "1"}}),
        )
    });
    let v = r.api.current_live("1").await.expect("ok").expect("配信中は Some");
    assert_eq!(v["movie"]["id"], "9");
    assert_eq!(v["broadcaster"]["id"], "1");
}

// ---------------------------------------------------------------- 状態コードの分類

#[tokio::test]
async fn status_401_is_unauthorized_and_marks_auth_relogin_required() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| error_reply(401, 1000));
    assert_eq!(r.api.user("alice").await, Err(ApiError::Unauthorized));
    assert_eq!(state(&r.auth), AuthState::ReloginRequired);
}

#[tokio::test]
async fn status_403_code_2000_is_rate_limited_with_reset() {
    let reply = error_reply(403, 2000).header("X-RateLimit-Reset", "1900000000");
    assert_eq!(one_error(reply).await, ApiError::RateLimited { reset_epoch_s: Some(1_900_000_000) });
}

#[tokio::test]
async fn status_429_is_rate_limited() {
    let e = one_error(error_reply(429, 0)).await;
    assert!(matches!(e, ApiError::RateLimited { .. }), "{e:?}");
}

#[tokio::test]
async fn status_403_code_2002_is_protected() {
    assert_eq!(one_error(error_reply(403, 2002)).await, ApiError::Protected);
}

#[tokio::test]
async fn status_403_code_2001_is_app_disabled() {
    assert_eq!(one_error(error_reply(403, 2001)).await, ApiError::AppDisabled);
}

#[tokio::test]
async fn status_403_unknown_code_or_non_json_is_other() {
    assert_eq!(one_error(error_reply(403, 9)).await, ApiError::Other);
    assert_eq!(one_error(Reply::html("<html>BODY-MARKER</html>").status(403)).await, ApiError::Other);
}

#[tokio::test]
async fn status_5xx_is_server() {
    assert_eq!(one_error(error_reply(503, 0)).await, ApiError::Server);
}

#[tokio::test]
async fn connection_refused_is_network() {
    let r = rig_logged_in().await;
    r.fake.set_down(true);
    assert_eq!(r.api.user("alice").await, Err(ApiError::Network));
}

// ---------------------------------------------------------------- 送らない条件

#[tokio::test]
async fn no_request_is_sent_when_not_ready() {
    async fn all_unauthorized(api: &Api) {
        assert_eq!(api.user("a").await, Err(ApiError::Unauthorized));
        assert_eq!(api.current_live("1").await, Err(ApiError::Unauthorized));
        assert_eq!(api.comments("9", q()).await, Err(ApiError::Unauthorized));
    }

    // 未ログイン
    let r = rig().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    let before = state(&r.auth);
    all_unauthorized(&r.api).await;
    assert_eq!(r.fake.total(), 0, "未ログインで送った");
    assert_eq!(state(&r.auth), before);

    // ReloginRequired
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    r.auth.report_unauthorized();
    let before = state(&r.auth);
    assert_eq!(before, AuthState::ReloginRequired);
    all_unauthorized(&r.api).await;
    assert_eq!(r.fake.total(), 0, "ReloginRequired で送った");
    assert_eq!(state(&r.auth), before);

    // ClientID 不一致
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    let other = fast_comment_lib::sources::twitcasting_api::TwitcastingApi::new(
        r.auth.clone(),
        endpoints(&r.fake.base),
        Arc::new(|| Some("other-client".to_string())),
    );
    all_unauthorized(&other).await;
    assert_eq!(r.fake.total(), 0, "ClientID 不一致で送った");
    // ClientID が違うログインは再ログイン要求になる(PR #43 の既定の挙動)
    assert_eq!(state(&r.auth), AuthState::ReloginRequired);
}

// ---------------------------------------------------------------- 秘密

#[tokio::test(flavor = "current_thread")]
async fn errors_never_contain_token_or_response_body() {
    let (logs, _guard) = capture_logs();
    let mut hosts: Vec<String> = Vec::new();
    let reset = epoch_s().to_string();
    let cases: Vec<Reply> = vec![
        error_reply(401, 1000),
        error_reply(403, 2000).header("X-RateLimit-Reset", &reset),
        error_reply(429, 0),
        error_reply(403, 2002),
        error_reply(403, 2001),
        error_reply(403, 9),
        Reply::html("<html>BODY-MARKER</html>").status(403),
        error_reply(404, 404),
        error_reply(503, 0),
    ];
    let mut errors: Vec<(ApiError, String)> = Vec::new();
    let mut log = String::new();
    for reply in cases {
        let r = rig_logged_in().await;
        logs.clear(); // ログイン手順(PR #43)のログは API クライアントのものではない
        hosts.push(r.fake.host());
        r.fake.set_responder(move |_: &Req| reply.clone());
        errors.push((r.api.user("alice").await.unwrap_err(), r.fake.host()));
        log.push_str(&logs.take());
    }
    {
        let r = rig_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        r.fake.set_down(true);
        errors.push((r.api.user("alice").await.unwrap_err(), r.fake.host()));
        log.push_str(&logs.take());
    }
    for (e, host) in &errors {
        for (place, text) in [("Display", format!("{e}")), ("Debug", format!("{e:?}"))] {
            assert_absent(&format!("ApiError {place}"), &text, &[TOK_A, "BODY-MARKER", host.as_str()]);
        }
    }
    assert_absent("ログ", &log, &[TOK_A, "BODY-MARKER"]);
    for h in &hosts {
        assert_absent("ログ", &log, &[h.as_str()]);
    }
}

#[tokio::test]
async fn api_debug_never_contains_token() {
    let r = rig_logged_in().await;
    r.fake.set_responder(|_: &Req| empty_reply());
    let _ = r.api.comments("9", q()).await;
    let text = format!("{:?}", r.api);
    assert_absent("TwitcastingApi の Debug", &text, &[TOK_A, r.fake.base.as_str(), r.fake.host().as_str()]);
}

#[tokio::test(flavor = "current_thread")]
async fn no_log_line_contains_token_or_url() {
    let (logs, _guard) = capture_logs();
    let mut hosts: Vec<String> = Vec::new();
    let mut log = String::new();
    // 成功
    {
        let r = rig_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        r.fake.set_responder(|_: &Req| Reply::json(200, json!({"user": {"id": "1"}})));
        assert!(r.api.user("alice").await.is_ok());
        log.push_str(&logs.take());
    }
    // 401
    {
        let r = rig_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        r.fake.set_responder(|_: &Req| error_reply(401, 1000));
        assert_eq!(r.api.user("alice").await, Err(ApiError::Unauthorized));
        log.push_str(&logs.take());
    }
    // 5xx
    {
        let r = rig_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        r.fake.set_responder(|_: &Req| error_reply(503, 0));
        assert_eq!(r.api.user("alice").await, Err(ApiError::Server));
        log.push_str(&logs.take());
    }
    // Network
    {
        let r = rig_logged_in().await;
        logs.clear();
        hosts.push(r.fake.host());
        r.fake.set_down(true);
        assert_eq!(r.api.user("alice").await, Err(ApiError::Network));
        log.push_str(&logs.take());
    }
    assert_absent("ログ", &log, &[TOK_A, "Bearer"]);
    for h in &hosts {
        assert_absent("ログ", &log, &[h.as_str()]);
    }
}

// ---------------------------------------------------------------- 予算と認証の順序(B5・B6)

#[tokio::test]
async fn budget_wait_reevaluates_auth_before_send() {
    let r = rig_logged_in_default_budget().await;
    let reset_s = epoch_s() + 1;
    let first = Arc::new(AtomicBool::new(true));
    {
        let first = first.clone();
        r.fake.set_responder(move |_: &Req| {
            if first.swap(false, Ordering::SeqCst) {
                error_reply(429, 0).header("X-RateLimit-Reset", &reset_s.to_string())
            } else {
                Reply::json(200, json!({"comments": [], "user": {"id": "1"}}))
            }
        });
    }
    // 1. 行 X が 429 を受ける
    let x = r.api.user("alice").await;
    assert!(matches!(x, Err(ApiError::RateLimited { .. })), "{x:?}");
    let n_after_429 = r.fake.total();
    assert_eq!(n_after_429, 1);
    // 2. 行 Y(同じ Arc<TwitcastingApi>)が予算待ちに入る
    let api = r.api.clone();
    let y = tokio::spawn(async move { api.comments("9", CommentsQuery { slice_id: None, offset: 0 }).await });
    settle(100).await;
    assert!(!y.is_finished(), "Y は予算待ちのはず");
    // 3. 待ちの間にログアウト
    assert!(r.auth.disconnect().await.is_ok());
    // 4. 期限(Reset + 1 秒)を 300ms 過ぎるまで待つ。Y は送らずに Unauthorized
    let deadline_ms = (reset_s + 1) * 1000 + 300;
    wait_until("期限を過ぎる", Duration::from_secs(10), || epoch_ms() >= deadline_ms).await;
    let y_result = tokio::time::timeout(Duration::from_secs(5), y)
        .await
        .expect("Y が終わる")
        .expect("Y が panic しない");
    assert_eq!(y_result, Err(ApiError::Unauthorized));
    assert_eq!(r.fake.total(), n_after_429, "ログアウト後に要求が送られた");
    // 5. tok-C で再ログインして呼ぶと、新トークンだけが送られる
    login(&r.auth, r.port, TOK_C).await;
    assert!(r.api.comments("9", q()).await.is_ok());
    let after: Vec<Req> = r.fake.requests().into_iter().skip(n_after_429).collect();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].authorization(), Some("Bearer tok-C-SECRET"));
    assert!(
        after.iter().all(|req| req.authorization() != Some("Bearer tok-A-SECRET")),
        "旧トークンが 429 の後に送られた"
    );
}

#[tokio::test]
async fn pause_deadline_is_not_shortened_by_straggling_200() {
    let r = rig_logged_in_default_budget().await;
    let gate = Arc::new(Notify::new());
    let reset_b = Arc::new(Mutex::new(0u64));
    {
        let gate = gate.clone();
        let reset_b = reset_b.clone();
        let a_seen = Arc::new(AtomicBool::new(false));
        r.fake.set_responder(move |req: &Req| match req.movie_id().as_str() {
            "1" => {
                if !a_seen.swap(true, Ordering::SeqCst) {
                    // A: 保留しておき、後で Remaining=50・短い Reset 付きの 200 で解放する
                    empty_reply()
                        .header("X-RateLimit-Remaining", "50")
                        .header("X-RateLimit-Reset", &(epoch_s() + 1).to_string())
                        .hold(gate.clone())
                } else {
                    empty_reply()
                }
            }
            "2" => {
                let reset = epoch_s() + 2;
                *reset_b.lock().unwrap() = reset;
                error_reply(429, 0).header("X-RateLimit-Reset", &reset.to_string())
            }
            _ => empty_reply(),
        });
    }
    // 1. 要求 A(行 1)が偽サーバで保留される
    let api = r.api.clone();
    let a = tokio::spawn(async move { api.comments("1", CommentsQuery { slice_id: None, offset: 0 }).await });
    wait_until("A が届く", Duration::from_secs(3), || r.fake.count(|x| x.movie_id() == "1") == 1).await;
    // 2. 要求 B(行 2)が 429 を受ける
    let b = r.api.comments("2", q()).await;
    assert!(matches!(b, Err(ApiError::RateLimited { .. })), "{b:?}");
    // 3. A を Remaining=50・Reset=now+1 の 200 で解放する
    gate.notify_one();
    let a_result = tokio::time::timeout(Duration::from_secs(3), a)
        .await
        .expect("A が終わる")
        .expect("A が panic しない");
    assert!(a_result.is_ok(), "{a_result:?}");
    // 4. C(行 1)は B の Reset + 1 秒より前に届かない
    assert!(r.api.comments("1", q()).await.is_ok());
    let c = r.fake.comment_calls().into_iter().filter(|x| x.movie_id() == "1").last().expect("C が届く");
    let deadline_ms = (*reset_b.lock().unwrap() + 1) * 1000;
    assert!(c.epoch_ms >= deadline_ms, "C が期限より {}ms 早く届いた", deadline_ms - c.epoch_ms.min(deadline_ms));
    assert!(c.epoch_ms <= deadline_ms + 1000, "C が期限 + 1 秒を超えて届いた: {}ms", c.epoch_ms - deadline_ms);
}

// ---------------------------------------------------------------- 送信先(B1)

#[test]
fn official_endpoints_ignore_config() {
    let default_base = TwitcastingEndpoints::default().api_base;
    assert_eq!(default_base, "https://apiv2.twitcasting.tv");
    assert_eq!(official_endpoints(&AppConfig::default()).api_base, default_base);

    let evil: AppConfig = serde_json::from_value(json!({
        "twitcastingOverrides": {"endpoints": {
            "officialApiBase": "https://evil.example",
            "apiBase": "http://127.0.0.1:1"
        }}
    }))
    .expect("未知のキーでも設定は読める");
    let e = official_endpoints(&evil);
    assert_eq!(e.api_base, default_base);
    assert_eq!(e.authorize, TwitcastingEndpoints::default().authorize);
}
