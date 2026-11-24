use super::*;
use crate::{poll::Poll, poll_control::PollControl, sources::twitch_auth::TwitchAuth};
use axum::{
    body::{to_bytes, Body},
    http::Request,
};
use serde_json::{json, Value};
use std::sync::Arc;
use tower::ServiceExt;

#[tokio::test]
async fn poll_api_serves_the_shared_snapshot_and_validates_start() {
    let gate = crate::api::ApiGate::closed();
    gate.open("test-token");
    let poll = Arc::new(Poll::default());
    let (_, config) = tokio::sync::watch::channel(Default::default());
    let control = Arc::new(PollControl::new(
        poll.clone(),
        Arc::new(TwitchAuth::new().unwrap()),
        config,
        Default::default(),
    ));
    let app = crate::api::router(
        ApiDeps::new(gate, Default::default()).with_poll(control),
        1234,
    );
    let send = |method, suffix, body: Option<Value>| {
        app.clone().oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/v1/poll{suffix}"))
                .header(header::HOST, "127.0.0.1:1234")
                .header(header::AUTHORIZATION, "Bearer test-token")
                .header(header::CONTENT_TYPE, "application/json")
                .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
                .unwrap(),
        )
    };
    let start = json!({
        "question": " 質問 ", "options": ["A", "B"], "durationSecs": 30,
        "display": "donut", "showPercent": true, "showCounts": false
    });
    let mut duplicate = start.clone();
    duplicate["options"] = json!(["A", " a "]);
    let mut twitch = start.clone();
    twitch["source"] = json!("twitch");
    for (method, suffix, body, expected) in [
        ("GET", "", None, StatusCode::OK),
        ("POST", "/start", Some(json!({})), StatusCode::BAD_REQUEST),
        ("POST", "/start", Some(duplicate), StatusCode::BAD_REQUEST),
        ("POST", "/start", Some(twitch), StatusCode::BAD_REQUEST),
        ("POST", "/start", Some(start.clone()), StatusCode::OK),
        ("POST", "/start", Some(start.clone()), StatusCode::CONFLICT),
        ("GET", "", None, StatusCode::OK),
        ("POST", "/stop", Some(json!({})), StatusCode::BAD_REQUEST),
        ("POST", "/stop", None, StatusCode::OK),
        ("POST", "/reset", None, StatusCode::OK),
    ] {
        let response = send(method, suffix, body).await.unwrap();
        assert_eq!(response.status(), expected, "{method} /poll{suffix}");
        if expected == StatusCode::OK {
            let result: Value =
                serde_json::from_slice(&to_bytes(response.into_body(), 4096).await.unwrap())
                    .unwrap();
            assert_eq!(result, serde_json::to_value(poll.snapshot()).unwrap());
        }
        if suffix == "/start" && expected == StatusCode::OK {
            let snapshot = poll.snapshot();
            assert_eq!(snapshot.question, "質問");
            assert_eq!(snapshot.ends_at_ms, Some(snapshot.started_at_ms + 30_000));
            assert_eq!(snapshot.status, crate::poll::PollStatus::Open);
        }
        if suffix == "/stop" && expected == StatusCode::OK {
            assert_eq!(poll.snapshot().status, crate::poll::PollStatus::Closed);
        }
        if suffix == "/reset" {
            assert_eq!(poll.snapshot(), crate::poll::PollSnapshot::default());
        }
    }
}
