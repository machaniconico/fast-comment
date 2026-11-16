mod comments;
pub mod dto;
mod events;
mod feed;
pub use dto::api_comment_json;
pub use feed::ApiFeed;
pub mod error;
pub mod gate;
pub mod middleware;
pub mod status;
pub use gate::{generate_token, ApiGate};
use tokio_util::sync::CancellationToken;
#[derive(Clone)]
pub struct ApiDeps {
    pub(crate) gate: ApiGate,
    pub(crate) epoch: String,
    pub(crate) shutdown: CancellationToken,
    pub(crate) feed: ApiFeed,
}
impl ApiDeps {
    pub fn new(gate: ApiGate, shutdown: CancellationToken) -> Self {
        Self {
            gate,
            epoch: {
                static EPOCH: std::sync::OnceLock<String> = std::sync::OnceLock::new();
                EPOCH
                    .get_or_init(|| uuid::Uuid::new_v4().simple().to_string()[..12].to_owned())
                    .clone()
            },
            shutdown,
            feed: ApiFeed::new(1000),
        }
    }
    pub fn with_feed(mut self, feed: ApiFeed) -> Self {
        self.feed = feed;
        self
    }
}
pub fn router(deps: ApiDeps, port: u16) -> axum::Router {
    use axum::{
        middleware::from_fn_with_state,
        routing::{any, get},
    };
    axum::Router::new()
        .route(
            "/api/v1/status",
            get(status::status).fallback(error::method_not_allowed),
        )
        .route(
            "/api/v1/comments",
            get(comments::comments).fallback(error::method_not_allowed),
        )
        .route(
            "/api/v1/events",
            get(events::events).fallback(error::method_not_allowed),
        )
        .route("/api", any(error::not_found))
        .route("/api/", any(error::not_found))
        .route("/api/*path", any(error::not_found))
        .fallback(error::not_found)
        .with_state(deps.clone())
        .layer(from_fn_with_state(
            middleware::Boundary { deps, port },
            middleware::boundary,
        ))
}

#[cfg(test)]
mod query_tests {
    use super::*;
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use serde_json::json;
    use tower::ServiceExt;

    fn setup() -> (ApiDeps, crate::model::ChatMessage) {
        let gate = ApiGate::closed();
        gate.open("test-token");
        let feed = ApiFeed::new(1000);
        feed.set_enabled(true);
        let deps = ApiDeps::new(gate, CancellationToken::new()).with_feed(feed);
        let msg = serde_json::from_value(json!({
            "id":"m", "platform":"twitch", "channel":"chan",
            "author":{"id":"user","name":"Alice"}, "fragments":[],
            "kind":"normal", "timestampMs":1
        }))
        .unwrap();
        (deps, msg)
    }

    async fn status(deps: &ApiDeps, path: &str, last_event_id: Option<&str>) -> StatusCode {
        let mut request = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:1234")
            .header("authorization", "Bearer test-token");
        if let Some(value) = last_event_id {
            request = request.header("last-event-id", value);
        }
        router(deps.clone(), 1234)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn future_cursors_are_400_for_comments_and_both_sse_inputs() {
        let (deps, msg) = setup();
        for latest in [0, 1] {
            if latest == 1 {
                deps.feed.push_chat(&msg);
            }
            for seq in [latest + 1, u64::MAX] {
                let cursor = format!("{}.{seq}", deps.epoch);
                for route in ["comments", "events"] {
                    assert_eq!(
                        status(&deps, &format!("/api/v1/{route}?after={cursor}"), None).await,
                        StatusCode::BAD_REQUEST
                    );
                }
                assert_eq!(
                    status(&deps, "/api/v1/events", Some(&cursor)).await,
                    StatusCode::BAD_REQUEST
                );
            }
        }
    }

    #[tokio::test]
    async fn invalid_sse_cursor_is_400_even_when_all_slots_are_taken() {
        let (deps, _) = setup();
        let clients: Vec<_> = (0..8).map(|_| deps.feed.subscribe().unwrap().1).collect();
        for cursor in ["invalid".to_owned(), format!("{}.1", deps.epoch)] {
            assert_eq!(
                status(&deps, &format!("/api/v1/events?after={cursor}"), None).await,
                StatusCode::BAD_REQUEST
            );
            assert_eq!(
                status(&deps, "/api/v1/events", Some(&cursor)).await,
                StatusCode::BAD_REQUEST
            );
        }
        assert_eq!(
            status(&deps, "/api/v1/events", None).await,
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(clients);
        assert_eq!(status(&deps, "/api/v1/events", None).await, StatusCode::OK);
    }

    #[tokio::test]
    async fn deleted_head_cursor_remains_valid() {
        let (deps, msg) = setup();
        deps.feed.push_chat(&msg);
        deps.feed
            .push_moderation(&crate::model::ModerationEvent::MessageDeleted {
                platform: msg.platform,
                channel: msg.channel,
                message_id: msg.id,
            });
        for route in ["comments", "events"] {
            assert_eq!(
                status(
                    &deps,
                    &format!("/api/v1/{route}?after={}.1", deps.epoch),
                    None
                )
                .await,
                StatusCode::OK
            );
        }
    }

    #[tokio::test]
    async fn sse_emits_unbounded_clear_before_later_chat_in_arrival_order() {
        use futures_util::StreamExt;
        let (deps, mut msg) = setup();
        let request = Request::builder()
            .uri("/api/v1/events")
            .header("host", "127.0.0.1:1234")
            .header("authorization", "Bearer test-token");
        let response = router(deps.clone(), 1234)
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap();
        // Both land in one 16ms frame before the stream drains the queue.
        deps.feed
            .push_moderation(&crate::model::ModerationEvent::UserCleared {
                platform: crate::model::Platform::Twitch,
                channel: "chan".into(),
                author_id: "user".into(),
                duration_secs: Some(1),
                certain: true,
                before_ms: None,
            });
        msg.id = "brand-new".into();
        msg.timestamp_ms = 200;
        deps.feed.push_chat(&msg);
        let mut body = response.into_body().into_data_stream();
        let mut wire = String::new();
        while !(wire.contains("event: chat") && wire.contains("event: moderation")) {
            let bytes = tokio::time::timeout(std::time::Duration::from_secs(2), body.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            wire.push_str(std::str::from_utf8(&bytes).unwrap());
        }
        assert!(
            wire.find("event: moderation") < wire.find("event: chat"),
            "clear reordered after the later chat: {wire}"
        );
        assert_eq!(deps.feed.snapshot().entries.len(), 1);
    }

    #[tokio::test]
    async fn events_ignore_undocumented_parameters_like_comments_ignore_unknown_ones() {
        let (deps, _) = setup();
        for path in [
            "/api/v1/events?limit=0&kind=unknown&channel=chan",
            "/api/v1/events?limit=%2B5",
            "/api/v1/comments?unknown=0",
        ] {
            assert_eq!(status(&deps, path, None).await, StatusCode::OK);
        }
    }

    #[tokio::test]
    async fn empty_last_event_id_falls_back_to_query_and_nonempty_header_takes_precedence() {
        let (deps, _) = setup();
        let valid = format!("{}.0", deps.epoch);
        assert_eq!(
            status(&deps, "/api/v1/events", Some("")).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&deps, &format!("/api/v1/events?after={valid}"), Some("")).await,
            StatusCode::OK
        );
        assert_eq!(
            status(&deps, "/api/v1/events?after=invalid", Some("")).await,
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            status(&deps, "/api/v1/events?after=invalid", Some(&valid)).await,
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn comments_limit_accepts_only_ascii_digits() {
        let (deps, _) = setup();
        for limit in ["%2B5", "+5", "-5", "", "5.0", "%EF%BC%95"] {
            assert_eq!(
                status(&deps, &format!("/api/v1/comments?limit={limit}"), None).await,
                StatusCode::BAD_REQUEST,
                "limit={limit}"
            );
        }
        assert_eq!(
            status(&deps, "/api/v1/comments?limit=5", None).await,
            StatusCode::OK
        );
    }
}
