use axum::{
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
pub fn response(status: StatusCode, code: &str, message: &str) -> Response {
    let mut response = (
        status,
        Json(serde_json::json!({"error":{"code":code,"message":message}})),
    )
        .into_response();
    if status == StatusCode::UNAUTHORIZED {
        response.headers_mut().insert(
            header::WWW_AUTHENTICATE,
            header::HeaderValue::from_static("Bearer"),
        );
    }
    response
}
pub async fn not_found() -> Response {
    response(
        StatusCode::NOT_FOUND,
        "not_found",
        "該当する API がありません",
    )
}
pub async fn method_not_allowed() -> Response {
    response(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "このメソッドは使えません",
    )
}
