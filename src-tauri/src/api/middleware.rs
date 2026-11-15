use super::{error, ApiDeps};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
#[derive(Clone)]
pub struct Boundary {
    pub deps: ApiDeps,
    pub port: u16,
}
pub async fn boundary(State(state): State<Boundary>, request: Request, next: Next) -> Response {
    if !state.deps.gate.is_open() {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut response = if !host_allowed(&request, state.port) {
        error::response(
            StatusCode::FORBIDDEN,
            "host_not_allowed",
            "Host が許可されていません",
        )
    } else if request.headers().contains_key(header::ORIGIN) {
        error::response(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "ブラウザからは利用できません",
        )
    } else if !authorized(&request, &state.deps.gate) {
        warn_unauthorized();
        error::response(StatusCode::UNAUTHORIZED, "unauthorized", "認証が必要です")
    } else {
        next.run(request).await
    };
    let headers = response.headers_mut();
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        header::HeaderValue::from_static("nosniff"),
    );
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
    {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
}
// Process-wide: rebuilding the router after a port change must not reset the limit.
fn warn_unauthorized() {
    static LAST_WARNING: std::sync::Mutex<Option<std::time::Instant>> = std::sync::Mutex::new(None);
    if take_warning_slot(&mut LAST_WARNING.lock().unwrap(), std::time::Instant::now()) {
        // Never attach request details (headers, token, URI or query) to this log.
        tracing::warn!("ローカル API の認証に失敗しました");
    }
}
fn take_warning_slot(last: &mut Option<std::time::Instant>, now: std::time::Instant) -> bool {
    if last.is_some_and(|at| now.saturating_duration_since(at) < std::time::Duration::from_secs(60))
    {
        return false;
    }
    *last = Some(now);
    true
}
fn host_allowed(request: &Request, port: u16) -> bool {
    let mut hosts = request.headers().get_all(header::HOST).iter();
    let host = match hosts.next() {
        Some(value) => {
            if hosts.next().is_some() {
                return false;
            }
            value.to_str().ok()
        }
        None => request.uri().authority().map(|a| a.as_str()),
    };
    host.is_some_and(|host| {
        [
            format!("127.0.0.1:{port}"),
            format!("localhost:{port}"),
            format!("[::1]:{port}"),
        ]
        .iter()
        .any(|allowed| host.eq_ignore_ascii_case(allowed))
    })
}
fn authorized(request: &Request, gate: &super::ApiGate) -> bool {
    let mut values = request.headers().get_all(header::AUTHORIZATION).iter();
    let Some(value) = values.next() else {
        return false;
    };
    if values.next().is_some() || value.as_bytes().len() > 512 {
        return false;
    }
    let Ok(value) = value.to_str() else {
        return false;
    };
    let Some((scheme, token)) = value.split_once(' ') else {
        return false;
    };
    scheme.eq_ignore_ascii_case("Bearer")
        && !token.is_empty()
        && !token.chars().any(char::is_whitespace)
        && gate.verify(token)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, middleware::from_fn_with_state, routing::get, Router};
    use tower::ServiceExt;

    #[tokio::test]
    async fn boundary_preserves_sse_and_text_content_types_and_normalizes_json() {
        let gate = super::super::ApiGate::closed();
        let token = super::super::generate_token();
        gate.open(&token);
        let router = Router::new()
            .route(
                "/events",
                get(|| async {
                    (
                        [(header::CONTENT_TYPE, "text/event-stream")],
                        "data: {}\n\n",
                    )
                }),
            )
            .route("/text", get(|| async { "hello" }))
            .route(
                "/json",
                get(|| async { axum::Json(serde_json::json!({"ok":true})) }),
            )
            .layer(from_fn_with_state(
                Boundary {
                    deps: ApiDeps::new(gate, Default::default()),
                    port: 1234,
                },
                boundary,
            ));
        for (path, expected) in [
            ("/events", "text/event-stream"),
            ("/text", "text/plain; charset=utf-8"),
            ("/json", "application/json; charset=utf-8"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header(header::HOST, "127.0.0.1:1234")
                        .header(header::AUTHORIZATION, format!("Bearer {token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], expected);
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
        }
    }

    #[test]
    fn unauthorized_warning_is_limited_to_once_per_minute() {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut last = None;
        assert!(take_warning_slot(&mut last, start));
        for ms in [0, 1, 1000, 59_999] {
            assert!(!take_warning_slot(
                &mut last,
                start + Duration::from_millis(ms)
            ));
        }
        assert!(take_warning_slot(
            &mut last,
            start + Duration::from_secs(60)
        ));
        assert!(!take_warning_slot(
            &mut last,
            start + Duration::from_secs(119)
        ));
        assert!(take_warning_slot(
            &mut last,
            start + Duration::from_secs(120)
        ));
    }
}
