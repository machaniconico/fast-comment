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
