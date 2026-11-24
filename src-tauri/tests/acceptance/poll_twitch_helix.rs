//! Twitch 公式 Polls 受け入れテスト: Helix クライアント(poll-pr2-twitch.md「Helix」)。ローカルのモックを使う。
use crate::poll_twitch_common::*;
use fast_comment_lib::sources::twitch_polls::{HelixPolls, PollsError, RemoteStatus};
use serde_json::json;

fn choices() -> Vec<String> {
    vec!["はい".to_string(), "いいえ".to_string(), "どちらでも".to_string()]
}

async fn rig() -> (Mock, HelixPolls) {
    let m = Mock::start().await;
    let h = HelixPolls::new(&m.base, CLIENT_ID);
    (m, h)
}

fn get_reply(status: &str, votes: &[u32]) -> Reply {
    let cs: Vec<_> = votes.iter().enumerate().map(|(i, v)| json!({"id": format!("c{i}"), "title": format!("t{i}"), "votes": v})).collect();
    Reply::json(200, json!({"data": [{"id": "p1", "status": status, "choices": cs}]}))
}

#[tokio::test]
async fn create_sends_broadcaster_title_choices_and_duration() {
    let (m, h) = rig().await;
    m.always(Reply::json(200, json!({"data": [{"id": "poll-123", "status": "ACTIVE"}]})));
    let id = h.create(TOKEN, "b-42", "好きな色は?", &choices(), 90).await.unwrap();
    assert_eq!(id, "poll-123");
    let req = m.last();
    assert_eq!(req.method, "POST");
    assert!(req.path.ends_with("/polls"), "{}", req.path);
    assert_eq!(req.headers.get("authorization").map(String::as_str), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(req.headers.get("client-id").map(String::as_str), Some(CLIENT_ID));
    let body = req.body_json();
    assert_eq!(body["broadcaster_id"], "b-42");
    assert_eq!(body["title"], "好きな色は?");
    assert_eq!(body["choices"], json!([{"title": "はい"}, {"title": "いいえ"}, {"title": "どちらでも"}]));
    assert_eq!(body["duration"], 90);
}

#[tokio::test]
async fn end_uses_terminated_or_archived() {
    let (m, h) = rig().await;
    m.always(Reply::json(200, json!({"data": [{"id": "p1", "status": "TERMINATED"}]})));
    h.end(TOKEN, "b-42", "p1", false).await.unwrap();
    let req = m.last();
    assert_eq!(req.method, "PATCH");
    assert!(req.path.ends_with("/polls"), "{}", req.path);
    assert_eq!(req.body_json()["status"], "TERMINATED");
    h.end(TOKEN, "b-42", "p1", true).await.unwrap();
    let req = m.last();
    assert_eq!(req.method, "PATCH");
    assert_eq!(req.body_json()["status"], "ARCHIVED");
}

#[tokio::test]
async fn get_maps_votes_in_choice_order_and_status() {
    let (m, h) = rig().await;
    m.always(get_reply("ACTIVE", &[7, 0, 12]));
    let p = h.get(TOKEN, "b-42", "p1").await.unwrap();
    assert_eq!(p.votes, vec![7, 0, 12]);
    assert_eq!(m.last().method, "GET");
    assert!(m.last().path.ends_with("/polls"), "{}", m.last().path);
    for (s, want) in [
        ("ACTIVE", RemoteStatus::Active),
        ("COMPLETED", RemoteStatus::Completed),
        ("TERMINATED", RemoteStatus::Terminated),
        ("ARCHIVED", RemoteStatus::Archived),
        ("MODERATED", RemoteStatus::Moderated),
        ("INVALID", RemoteStatus::Invalid),
        ("SOMETHING_NEW", RemoteStatus::Invalid),
    ] {
        m.always(get_reply(s, &[1, 2]));
        let p = h.get(TOKEN, "b-42", "p1").await.unwrap();
        assert!(matches!((&p.status, &want), (a, b) if std::mem::discriminant(a) == std::mem::discriminant(b)), "{s}: {:?}", p.status);
    }
}

fn err_body(status: u16, message: &str) -> Reply {
    Reply::json(status, json!({"error": "x", "status": status, "message": message}))
}

#[tokio::test]
async fn forbidden_without_scope_is_scope_missing_otherwise_not_eligible() {
    let (m, h) = rig().await;
    m.always(err_body(403, "Missing scope: channel:manage:polls"));
    let e = h.create(TOKEN, "b", "q", &choices(), 60).await.unwrap_err();
    assert!(matches!(e, PollsError::ScopeMissing), "{e:?}");
    m.always(err_body(403, "The broadcaster must have partner or affiliate status."));
    let e = h.create(TOKEN, "b", "q", &choices(), 60).await.unwrap_err();
    assert!(matches!(e, PollsError::NotEligible), "{e:?}");
}

#[tokio::test]
async fn unauthorized_is_401() {
    let (m, h) = rig().await;
    m.always(err_body(401, "Invalid OAuth token"));
    let e = h.get(TOKEN, "b", "p1").await.unwrap_err();
    assert!(matches!(e, PollsError::Unauthorized), "{e:?}");
}

#[tokio::test]
async fn bad_request_keeps_helix_message() {
    let (m, h) = rig().await;
    m.always(err_body(400, "The title must be 60 characters or fewer"));
    let e = h.create(TOKEN, "b", "q", &choices(), 60).await.unwrap_err();
    match e {
        PollsError::BadRequest(msg) => assert_eq!(msg, "The title must be 60 characters or fewer"),
        other => panic!("BadRequest ではない: {other:?}"),
    }
}

#[tokio::test]
async fn rate_limited_reads_ratelimit_reset() {
    let (m, h) = rig().await;
    let reset = now_secs() + 30;
    m.always(err_body(429, "Too Many Requests").header("Ratelimit-Reset", &reset.to_string()));
    match h.get(TOKEN, "b", "p1").await.unwrap_err() {
        PollsError::RateLimited { retry_after_secs } => {
            assert!((28..=30).contains(&retry_after_secs), "{retry_after_secs}")
        }
        other => panic!("RateLimited ではない: {other:?}"),
    }
    // 過去の時刻でも最小 1
    let past = now_secs().saturating_sub(100);
    m.always(err_body(429, "Too Many Requests").header("Ratelimit-Reset", &past.to_string()));
    match h.get(TOKEN, "b", "p1").await.unwrap_err() {
        PollsError::RateLimited { retry_after_secs } => assert_eq!(retry_after_secs, 1),
        other => panic!("RateLimited ではない: {other:?}"),
    }
}

#[tokio::test]
async fn network_error_is_network() {
    let h = HelixPolls::new(&refused_base().await, CLIENT_ID);
    let e = h.get(TOKEN, "b", "p1").await.unwrap_err();
    assert!(matches!(e, PollsError::Network), "{e:?}");
}

#[tokio::test]
async fn errors_never_contain_token_or_client_id() {
    let (m, h) = rig().await;
    let secrets = [TOKEN.to_string(), CLIENT_ID.to_string(), m.base.clone(), m.base.replace("/helix", ""), "127.0.0.1".to_string()];
    let mut errs: Vec<PollsError> = Vec::new();
    for (status, msg) in [
        (401, "Invalid OAuth token"),
        (403, "Missing scope: channel:manage:polls"),
        (403, "The broadcaster must have partner or affiliate status."),
        (400, "bad title"),
        (500, "boom"),
    ] {
        m.always(err_body(status, msg));
        errs.push(h.create(TOKEN, "b", "q", &choices(), 60).await.unwrap_err());
    }
    m.always(err_body(429, "slow down").header("Ratelimit-Reset", &(now_secs() + 5).to_string()));
    errs.push(h.get(TOKEN, "b", "p1").await.unwrap_err());
    errs.push(HelixPolls::new(&refused_base().await, CLIENT_ID).get(TOKEN, "b", "p1").await.unwrap_err());
    // 直接組み立てた全バリアントも確かめる
    errs.extend([
        PollsError::NotEligible,
        PollsError::ScopeMissing,
        PollsError::Unauthorized,
        PollsError::RateLimited { retry_after_secs: 7 },
        PollsError::BadRequest("bad".into()),
        PollsError::Api(502),
        PollsError::Network,
    ]);
    for e in &errs {
        let shown = e.to_string();
        for s in &secrets {
            assert!(!shown.contains(s.as_str()), "{e:?} の Display に {s:?} が含まれる: {shown}");
        }
        let dbg = format!("{e:?}");
        for s in [TOKEN, CLIENT_ID] {
            assert!(!dbg.contains(s), "{e:?} の Debug に秘密が含まれる");
        }
        if !matches!(e, PollsError::BadRequest(_)) {
            assert!(!shown.is_ascii(), "Display が日本語でない: {shown}");
        }
    }
}
