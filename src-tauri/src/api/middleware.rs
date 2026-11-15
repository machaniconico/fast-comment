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
    let mut response = if !state.deps.gate.is_open() {
        StatusCode::NOT_FOUND.into_response()
    } else if !host_allowed(&request, state.port) {
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
    if headers.contains_key(header::CONTENT_TYPE) {
        headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json; charset=utf-8"),
        );
    }
    response
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
