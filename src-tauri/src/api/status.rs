use super::ApiDeps;
use axum::{extract::State, Json};
pub async fn status(State(deps): State<ApiDeps>) -> Json<serde_json::Value> {
    Json(
        serde_json::json!({"app":"fast-comment", "version":env!("CARGO_PKG_VERSION"), "apiVersion":1, "epoch":deps.epoch, "headCursor":deps.feed.head().map(|seq| format!("{}.{}", deps.epoch, seq))}),
    )
}
