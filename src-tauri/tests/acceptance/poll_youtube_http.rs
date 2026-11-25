//! YouTube 公式投票 受け入れテスト: HTTP クライアント(poll-pr3-youtube.md「HTTP」)。ローカルのモックを使う。
use crate::poll_youtube_common::*;
use fast_comment_lib::sources::youtube_polls::{YoutubePolls, YtPollStatus, YtPollsError};
use serde_json::{json, Value};

fn options() -> Vec<String> {
    vec!["はい".to_string(), "いいえ".to_string(), "どちらでも".to_string()]
}

async fn rig() -> (Mock, YoutubePolls) {
    let m = Mock::start().await;
    let y = YoutubePolls::new(&m.base);
    (m, y)
}

/// liveChatMessage リソース(pollEvent)。`tallies` が None の要素は tally を持たない。
fn poll_item(id: &str, status: &str, q: &str, opts: &[(&str, Option<Value>)]) -> Value {
    let os: Vec<Value> = opts
        .iter()
        .map(|(t, tally)| {
            let mut o = json!({"optionText": t});
            if let Some(v) = tally {
                o["tally"] = v.clone();
            }
            o
        })
        .collect();
    json!({
        "kind": "youtube#liveChatMessage",
        "id": id,
        "snippet": {
            "type": "pollEvent",
            "liveChatId": LIVE_CHAT_ID,
            "publishedAt": "2026-11-25T01:02:03.456Z",
            "pollDetails": {"metadata": {"questionText": q, "options": os}, "status": status}
        }
    })
}

fn tally(s: &str) -> Option<Value> {
    Some(json!(s))
}

fn list_reply(active: Option<Value>) -> Reply {
    let mut v = json!({"kind": "youtube#liveChatMessageListResponse", "items": []});
    if let Some(a) = active {
        v["activePollItem"] = a;
    }
    Reply::json(200, v)
}

#[tokio::test]
async fn create_posts_poll_event_with_question_and_options() {
    let (m, y) = rig().await;
    m.always(Reply::json(200, poll_item("msg-1", "active", "好きな色は?", &[("はい", None), ("いいえ", None), ("どちらでも", None)])));
    let created = y.create(TOKEN, LIVE_CHAT_ID, "好きな色は?", &options()).await.unwrap();
    assert_eq!(created.id, "msg-1");
    assert_eq!(created.published_at_ms, Some(1795568523456));
    let req = m.last();
    assert_eq!(req.method, "POST");
    assert!(req.path.ends_with("/liveChat/messages"), "{}", req.path);
    assert_eq!(req.query.get("part").map(String::as_str), Some("snippet"));
    assert_eq!(req.headers.get("authorization").map(String::as_str), Some(format!("Bearer {TOKEN}").as_str()));
    let body = req.body_json();
    assert_eq!(body["snippet"]["liveChatId"], LIVE_CHAT_ID);
    assert_eq!(body["snippet"]["type"], "pollEvent");
    assert_eq!(body["snippet"]["pollDetails"]["metadata"]["questionText"], "好きな色は?");
    assert_eq!(
        body["snippet"]["pollDetails"]["metadata"]["options"],
        json!([{"optionText": "はい"}, {"optionText": "いいえ"}, {"optionText": "どちらでも"}])
    );
}

#[tokio::test]
async fn create_converts_rfc3339_offset_to_ms_and_tolerates_missing_published_at() {
    let (m, y) = rig().await;
    let mut item = poll_item("msg-2", "active", "q", &[("a", None), ("b", None)]);
    item["snippet"]["publishedAt"] = json!("2026-11-25T10:02:03+09:00");
    m.always(Reply::json(200, item));
    let created = y.create(TOKEN, LIVE_CHAT_ID, "q", &["a".to_string(), "b".to_string()]).await.unwrap();
    assert_eq!(created.published_at_ms, Some(1795568523000));
    m.always(Reply::json(200, json!({"id": "msg-3", "snippet": {"type": "pollEvent"}})));
    let created = y.create(TOKEN, LIVE_CHAT_ID, "q", &["a".to_string(), "b".to_string()]).await.unwrap();
    assert_eq!(created.id, "msg-3");
    assert_eq!(created.published_at_ms, None);
}

#[tokio::test]
async fn end_transitions_to_closed_without_body() {
    let (m, y) = rig().await;
    m.always(Reply::json(200, poll_item("msg-1", "closed", "好きな色は?", &[("はい", tally("7")), ("いいえ", tally("3"))])));
    let st = y.end(TOKEN, "msg-1").await.unwrap().expect("応答に snippet があれば状態が返る");
    let req = m.last();
    assert_eq!(req.method, "POST");
    assert!(req.path.ends_with("/liveChat/messages/transition"), "{}", req.path);
    assert_eq!(req.query.get("id").map(String::as_str), Some("msg-1"));
    assert_eq!(req.query.get("status").map(String::as_str), Some("closed"));
    assert_eq!(req.query.get("part").map(String::as_str), Some("snippet"));
    assert_eq!(req.headers.get("authorization").map(String::as_str), Some(format!("Bearer {TOKEN}").as_str()));
    assert!(req.body.is_empty(), "本文が付いている: {:?}", req.body);
    assert_eq!(st.id, "msg-1");
    assert_eq!(st.status, YtPollStatus::Closed);
    assert_eq!(st.question, "好きな色は?");
    assert_eq!(st.options, vec!["はい".to_string(), "いいえ".to_string()]);
    assert_eq!(st.tallies, Some(vec![7, 3]));
}

#[tokio::test]
async fn end_without_snippet_in_response_is_none() {
    let (m, y) = rig().await;
    m.always(Reply::json(200, json!({"kind": "youtube#liveChatMessage", "id": "msg-1"})));
    assert!(y.end(TOKEN, "msg-1").await.unwrap().is_none());
}

#[tokio::test]
async fn active_reads_active_poll_item_and_tallies() {
    let (m, y) = rig().await;
    m.always(list_reply(Some(poll_item("msg-9", "active", "質問", &[("A", tally("12")), ("B", tally("0")), ("C", tally("5"))]))));
    let st = y.active(TOKEN, LIVE_CHAT_ID).await.unwrap().expect("activePollItem がある");
    let req = m.last();
    assert_eq!(req.method, "GET");
    assert!(req.path.ends_with("/liveChat/messages"), "{}", req.path);
    assert_eq!(req.query.get("liveChatId").map(String::as_str), Some(LIVE_CHAT_ID));
    assert_eq!(req.query.get("part").map(String::as_str), Some("snippet"));
    assert_eq!(req.headers.get("authorization").map(String::as_str), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(st.id, "msg-9");
    assert_eq!(st.status, YtPollStatus::Active);
    assert_eq!(st.question, "質問");
    assert_eq!(st.options, vec!["A".to_string(), "B".to_string(), "C".to_string()]);
    assert_eq!(st.tallies, Some(vec![12, 0, 5]));
    assert_eq!(st.published_at_ms, Some(1795568523456));

    for (raw, want) in [("active", YtPollStatus::Active), ("closed", YtPollStatus::Closed), ("somethingNew", YtPollStatus::Unknown)] {
        m.always(list_reply(Some(poll_item("msg-9", raw, "質問", &[("A", tally("1")), ("B", tally("2"))]))));
        let st = y.active(TOKEN, LIVE_CHAT_ID).await.unwrap().unwrap();
        assert_eq!(st.status, want, "status={raw}");
    }
}

#[tokio::test]
async fn active_without_active_poll_item_is_none() {
    let (m, y) = rig().await;
    m.always(list_reply(None));
    assert!(y.active(TOKEN, LIVE_CHAT_ID).await.unwrap().is_none());
}

#[tokio::test]
async fn missing_or_non_numeric_tally_is_none() {
    let (m, y) = rig().await;
    let cases: Vec<(&str, Vec<(&str, Option<Value>)>)> = vec![
        ("全て無い", vec![("A", None), ("B", None)]),
        ("1 つだけ無い", vec![("A", tally("4")), ("B", None)]),
        ("数値でない", vec![("A", tally("4")), ("B", tally("many"))]),
        ("空文字", vec![("A", tally("")), ("B", tally("1"))]),
        ("負数", vec![("A", tally("-1")), ("B", tally("1"))]),
    ];
    for (name, opts) in cases {
        m.always(list_reply(Some(poll_item("msg-1", "active", "q", &opts))));
        let st = y.active(TOKEN, LIVE_CHAT_ID).await.unwrap().unwrap();
        assert_eq!(st.tallies, None, "{name}");
        assert_eq!(st.options.len(), 2, "{name}: tally が無くても選択肢は読める");
    }
}

fn google_err(status: u16, reason: &str, message: &str) -> Reply {
    Reply::json(
        status,
        json!({"error": {"code": status, "message": message, "errors": [{"message": message, "domain": "youtube.liveChat", "reason": reason}]}}),
    )
}

#[tokio::test]
async fn errors_map_by_status_and_reason() {
    let (m, y) = rig().await;
    let opts = options();

    m.always(google_err(400, "preconditionCheckFailed", "A pinned active poll already exists."));
    let e = y.create(TOKEN, LIVE_CHAT_ID, "q", &opts).await.unwrap_err();
    assert!(matches!(e, YtPollsError::AlreadyActive), "{e:?}");

    m.always(google_err(400, "invalidPollOption", "The poll option text is too long."));
    match y.create(TOKEN, LIVE_CHAT_ID, "q", &opts).await.unwrap_err() {
        YtPollsError::BadRequest(msg) => assert_eq!(msg, "The poll option text is too long."),
        other => panic!("BadRequest ではない: {other:?}"),
    }

    for reason in ["quotaExceeded", "dailyLimitExceeded", "rateLimitExceeded"] {
        m.always(google_err(403, reason, "limit"));
        let e = y.active(TOKEN, LIVE_CHAT_ID).await.unwrap_err();
        assert!(matches!(e, YtPollsError::QuotaExceeded), "{reason}: {e:?}");
    }

    m.always(google_err(403, "liveChatForbidden", "forbidden"));
    match y.end(TOKEN, "msg-1").await.unwrap_err() {
        YtPollsError::Forbidden(reason) => assert_eq!(reason, "liveChatForbidden"),
        other => panic!("Forbidden ではない: {other:?}"),
    }

    m.always(google_err(401, "authError", "Invalid Credentials"));
    let e = y.active(TOKEN, LIVE_CHAT_ID).await.unwrap_err();
    assert!(matches!(e, YtPollsError::Unauthorized), "{e:?}");

    m.always(google_err(404, "liveChatNotFound", "not found"));
    let e = y.end(TOKEN, "msg-1").await.unwrap_err();
    assert!(matches!(e, YtPollsError::NotFound), "{e:?}");

    m.always(google_err(500, "backendError", "boom"));
    let e = y.create(TOKEN, LIVE_CHAT_ID, "q", &opts).await.unwrap_err();
    assert!(matches!(e, YtPollsError::Api(500)), "{e:?}");

    let dead = YoutubePolls::new(&refused_base().await);
    let e = dead.active(TOKEN, LIVE_CHAT_ID).await.unwrap_err();
    assert!(matches!(e, YtPollsError::Network), "{e:?}");
}

#[tokio::test]
async fn errors_never_contain_token_or_url() {
    let (m, y) = rig().await;
    let secrets = [
        TOKEN.to_string(),
        LIVE_CHAT_ID.to_string(),
        m.base.clone(),
        m.base.replace("/youtube/v3", ""),
        "127.0.0.1".to_string(),
    ];
    let mut errs: Vec<YtPollsError> = Vec::new();
    for (status, reason, msg) in [
        (400, "preconditionCheckFailed", "A pinned active poll already exists."),
        (400, "invalidPollOption", "bad option"),
        (403, "quotaExceeded", "quota"),
        (403, "dailyLimitExceeded", "quota"),
        (403, "rateLimitExceeded", "quota"),
        (403, "liveChatForbidden", "forbidden"),
        (401, "authError", "Invalid Credentials"),
        (404, "liveChatNotFound", "not found"),
        (500, "backendError", "boom"),
    ] {
        m.always(google_err(status, reason, msg));
        errs.push(y.create(TOKEN, LIVE_CHAT_ID, "q", &options()).await.unwrap_err());
    }
    errs.push(YoutubePolls::new(&refused_base().await).active(TOKEN, LIVE_CHAT_ID).await.unwrap_err());
    // 直接組み立てた全バリアントも確かめる
    errs.extend([
        YtPollsError::AlreadyActive,
        YtPollsError::Forbidden("liveChatForbidden".into()),
        YtPollsError::QuotaExceeded,
        YtPollsError::Unauthorized,
        YtPollsError::BadRequest("bad".into()),
        YtPollsError::NotFound,
        YtPollsError::Api(502),
        YtPollsError::Network,
    ]);
    for e in &errs {
        let shown = e.to_string();
        for s in &secrets {
            assert!(!shown.contains(s.as_str()), "{e:?} の Display に {s:?} が含まれる: {shown}");
        }
        let dbg = format!("{e:?}");
        for s in [TOKEN, LIVE_CHAT_ID] {
            assert!(!dbg.contains(s), "{e:?} の Debug に秘密が含まれる");
        }
        if !matches!(e, YtPollsError::BadRequest(_)) {
            assert!(!shown.is_ascii(), "Display が日本語でない: {shown}");
        }
    }
}
