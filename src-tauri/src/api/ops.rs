//! アプリ操作の HTTP 境界。内部エラーの内容は公開しない。
use super::{error, ApiDeps};
use crate::{
    poll::{PollError, PollStart},
    poll_control::PollControlError,
    stats::TimerSnapshot,
    tts::TtsQueueStatePayload,
};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use futures_util::StreamExt;
use serde::Deserialize;
use std::sync::Mutex;
use tokio::time::Instant;

pub trait AppOps: Send + Sync + 'static {
    fn inject_test_comments(&self, req: &TestCommentRequest) -> Result<Vec<String>, OpsError>;
    fn tts_state(&self) -> TtsQueueStatePayload;
    fn tts_set_paused(&self, paused: bool);
    fn tts_skip(&self);
    fn tts_clear(&self);
    fn timer_state(&self) -> TimerSnapshot;
    fn timer_control(
        &self,
        action: TimerAction,
        duration_sec: Option<u32>,
    ) -> Result<TimerSnapshot, OpsError>;
}
pub struct OpsError(pub String);
#[derive(Clone, Copy)]
pub enum TimerAction {
    Start,
    Pause,
    Resume,
    Reset,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestCommentRequest {
    pub platform: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub text: String,
    #[serde(default = "normal")]
    pub kind: String,
    pub amount: Option<f64>,
    #[serde(default = "one")]
    pub count: u32,
}
fn normal() -> String {
    "normal".into()
}
fn one() -> u32 {
    1
}

pub(crate) struct RateLimit(Mutex<(f64, Instant)>);
impl RateLimit {
    pub fn new() -> Self {
        Self(Mutex::new((20.0, Instant::now())))
    }
    fn take(&self) -> Result<(), Response> {
        let mut bucket = self.0.lock().unwrap();
        let now = Instant::now();
        bucket.0 = (bucket.0 + now.duration_since(bucket.1).as_secs_f64() * 10.0).min(20.0);
        bucket.1 = now;
        if bucket.0 >= 1.0 {
            bucket.0 -= 1.0;
            return Ok(());
        }
        let mut r = error::response(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "操作の頻度が上限を超えています",
        );
        r.headers_mut()
            .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
        Err(r)
    }
}
fn invalid(field: &str) -> Response {
    error::response(
        StatusCode::BAD_REQUEST,
        "invalid_request",
        &format!("{field} が不正です"),
    )
}
fn internal() -> Response {
    error::response(
        StatusCode::INTERNAL_SERVER_ERROR,
        "internal",
        "操作に失敗しました",
    )
}
fn unavailable() -> Response {
    error::response(
        StatusCode::SERVICE_UNAVAILABLE,
        "unavailable",
        "操作を利用できません",
    )
}

// 本文を持たない操作にも適用し、未知キーや意図しない本文を受け付けない。
async fn body(request: Request) -> Result<Option<serde_json::Value>, Response> {
    let json = request
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        });
    let mut stream = request.into_body().into_data_stream();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| invalid("body"))?;
        if bytes.len().saturating_add(chunk.len()) > 16 * 1024 {
            return Err(error::response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "payload_too_large",
                "本文が大きすぎます",
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if !json {
        return Err(error::response(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "unsupported_media_type",
            "本文には JSON を指定してください",
        ));
    }
    let value: serde_json::Value = serde_json::from_slice(&bytes).map_err(|_| invalid("body"))?;
    if !value.is_object() {
        return Err(invalid("body"));
    }
    Ok(Some(value))
}
fn validate(req: &TestCommentRequest) -> Result<(), Response> {
    if !matches!(
        req.platform.as_str(),
        "twitch" | "youtube" | "x" | "niconico" | "twitcasting"
    ) {
        return Err(invalid("platform"));
    }
    if !matches!(
        req.kind.as_str(),
        "normal" | "superChat" | "membership" | "bits" | "gift"
    ) {
        return Err(invalid("kind"));
    }
    if !(1..=20).contains(&req.count) {
        return Err(invalid("count"));
    }
    if req.name.chars().count() > 64 {
        return Err(invalid("name"));
    }
    if req.text.chars().count() > 500 {
        return Err(invalid("text"));
    }
    if req
        .amount
        .is_some_and(|a| !a.is_finite() || !(0.0..=1_000_000_000.0).contains(&a))
    {
        return Err(invalid("amount"));
    }
    Ok(())
}
pub async fn inject(State(deps): State<ApiDeps>, request: Request) -> Response {
    let parsed = match body(request).await {
        Ok(Some(v)) => v,
        Ok(None) => return invalid("body"),
        Err(r) => return r,
    };
    let req: TestCommentRequest = match serde_json::from_value(parsed) {
        Ok(v) => v,
        Err(_) => return invalid("platform/name/text/kind/amount/count"),
    };
    if let Err(r) = validate(&req) {
        return r;
    }
    let Some(ops) = deps.ops else {
        return unavailable();
    };
    if let Err(r) = deps.rate_limit.take() {
        return r;
    }
    match ops.inject_test_comments(&req) {
        Ok(ids) => (
            StatusCode::ACCEPTED,
            Json(serde_json::json!({"injected": ids.len(), "ids": ids})),
        )
            .into_response(),
        Err(_) => internal(),
    }
}
pub async fn tts(State(deps): State<ApiDeps>) -> Response {
    match deps.ops {
        Some(ops) => Json(ops.tts_state()).into_response(),
        None => unavailable(),
    }
}
pub async fn timer(State(deps): State<ApiDeps>) -> Response {
    match deps.ops {
        Some(ops) => Json(ops.timer_state()).into_response(),
        None => unavailable(),
    }
}
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TimerStart {
    duration_sec: Option<u32>,
}

pub async fn mutate(State(deps): State<ApiDeps>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let parsed = match body(request).await {
        Ok(v) => v,
        Err(r) => return r,
    };
    let duration = if path == "/api/v1/timer/start" {
        let start: TimerStart = match parsed {
            Some(v) => match serde_json::from_value(v) {
                Ok(v) => v,
                Err(_) => return invalid("durationSec"),
            },
            None => TimerStart::default(),
        };
        if start
            .duration_sec
            .is_some_and(|d| !(1..=359999).contains(&d))
        {
            return invalid("durationSec");
        }
        start.duration_sec
    } else {
        if parsed.is_some() {
            return invalid("body");
        }
        None
    };
    let Some(ops) = deps.ops else {
        return unavailable();
    };
    if let Err(r) = deps.rate_limit.take() {
        return r;
    }
    match path.as_str() {
        "/api/v1/tts/pause" => ops.tts_set_paused(true),
        "/api/v1/tts/resume" => ops.tts_set_paused(false),
        "/api/v1/tts/skip" => ops.tts_skip(),
        "/api/v1/tts/clear" => ops.tts_clear(),
        _ => {
            let action = match path.as_str() {
                "/api/v1/timer/start" => TimerAction::Start,
                "/api/v1/timer/pause" => TimerAction::Pause,
                "/api/v1/timer/resume" => TimerAction::Resume,
                "/api/v1/timer/reset" => TimerAction::Reset,
                _ => return error::not_found().await,
            };
            return match ops.timer_control(action, duration) {
                Ok(v) => Json(v).into_response(),
                Err(_) => internal(),
            };
        }
    }
    Json(ops.tts_state()).into_response()
}

pub async fn poll(State(deps): State<ApiDeps>) -> Response {
    match deps.poll {
        Some(poll) => Json(poll.snapshot()).into_response(),
        None => unavailable(),
    }
}

pub async fn poll_mutate(State(deps): State<ApiDeps>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let body = match body(request).await {
        Ok(value) => value,
        Err(response) => return response,
    };
    let start = if path == "/api/v1/poll/start" {
        let Some(body) = body else {
            return invalid("body");
        };
        let Ok(request) = serde_json::from_value::<PollStart>(body) else {
            return invalid("body");
        };
        match request.validate() {
            Ok(start) => Some(start),
            Err(error) => return poll_error(error),
        }
    } else {
        if body.is_some() {
            return invalid("body");
        }
        None
    };

    let Some(poll) = deps.poll else {
        return unavailable();
    };
    if let Err(response) = deps.rate_limit.take() {
        return response;
    }
    // HTTP クライアントが切断しても Twitch 側の操作と結果の反映は完了させる。
    let operation = tokio::spawn(async move {
        match (path.as_str(), start) {
            ("/api/v1/poll/start", Some(start)) => Some(poll.start(start).await),
            ("/api/v1/poll/stop", _) => Some(poll.stop().await),
            ("/api/v1/poll/reset", _) => Some(poll.reset().await),
            _ => None,
        }
    });
    match operation.await {
        Ok(Some(Ok(snapshot))) => Json(snapshot).into_response(),
        Ok(Some(Err(PollControlError::Local(error)))) => poll_error(error),
        Ok(Some(Err(PollControlError::Remote(error)))) => {
            error::response(StatusCode::BAD_GATEWAY, "twitch_poll_error", &error.to_string())
        }
        Ok(None) => error::not_found().await,
        Err(_) => internal(),
    }
}

fn poll_error(error: PollError) -> Response {
    match error {
        PollError::Invalid(message) => {
            error::response(StatusCode::BAD_REQUEST, "invalid_request", message)
        }
        PollError::AlreadyOpen => error::response(
            StatusCode::CONFLICT,
            "poll_already_open",
            &error.to_string(),
        ),
        PollError::Save => internal(),
    }
}

#[cfg(test)]
#[path = "poll_tests.rs"]
mod poll_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;

    #[tokio::test]
    async fn poll_routes_use_authenticated_operations() {
        let gate = crate::api::ApiGate::closed();
        gate.open("test-token");
        let app = crate::api::router(ApiDeps::new(gate, Default::default()), 1234);
        for (method, path, body) in [
            ("GET", "", ""),
            (
                "POST",
                "/start",
                r#"{"question":"Q","options":["a","b"],"display":"bar-h","showPercent":true,"showCounts":true}"#,
            ),
            ("POST", "/stop", ""),
            ("POST", "/reset", ""),
        ] {
            for (token, expected) in [
                ("wrong", StatusCode::UNAUTHORIZED),
                ("test-token", StatusCode::SERVICE_UNAVAILABLE),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method)
                            .uri(format!("/api/v1/poll{path}"))
                            .header(header::HOST, "127.0.0.1:1234")
                            .header(header::AUTHORIZATION, format!("Bearer {token}"))
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), expected, "{method} /poll{path}");
            }
        }
    }

    #[tokio::test]
    async fn body_read_failure_is_not_reported_as_a_size_limit() {
        let stream =
            futures_util::stream::iter([Err::<String, _>(std::io::Error::other("SECRET-DETAIL"))]);
        let request = Request::builder()
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from_stream(stream))
            .unwrap();
        let response = body(request).await.err().expect("read failure");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["code"], "invalid_request");
        assert!(!String::from_utf8_lossy(&bytes).contains("SECRET-DETAIL"));
    }

    #[tokio::test]
    async fn positional_arrays_are_rejected_before_ops_are_accessed() {
        let gate = crate::api::ApiGate::closed();
        gate.open("test-token");
        // 操作依存がなくても、本文の型違いは 503 より先に 400 にする。
        let app = crate::api::router(ApiDeps::new(gate, Default::default()), 1234);
        for (path, valid, array) in [
            (
                "test-comments",
                r#"{"platform":"twitch"}"#,
                r#"["twitch","","","normal",null,1]"#,
            ),
            ("timer/start", r#"{"durationSec":300}"#, "[300]"),
        ] {
            for (body, status) in [
                (valid, StatusCode::SERVICE_UNAVAILABLE),
                (array, StatusCode::BAD_REQUEST),
            ] {
                let response = app
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method("POST")
                            .uri(format!("/api/v1/{path}"))
                            .header(header::HOST, "127.0.0.1:1234")
                            .header(header::AUTHORIZATION, "Bearer test-token")
                            .header(header::CONTENT_TYPE, "application/json")
                            .body(Body::from(body))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(response.status(), status, "{path}: {body}");
            }
        }
    }
}
