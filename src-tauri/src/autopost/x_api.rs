//! X API v2 への投稿。
//!
//! 防御的な応答解釈と、送信前後で再送可否を分けることを重視する。
//! 誤って二重投稿しないよう、接続前の失敗だけを再送する。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use super::error::{XPostError, XPostErrorCode};
use super::oauth::{TokenStore, XPostAuth};

pub use super::coordinator::PostedTweet;

const DEFAULT_RETRY_DELAYS: [Duration; 2] = [Duration::from_secs(2), Duration::from_secs(8)];
const MAX_RATE_LIMIT_WAIT_SECS: u64 = 90;

pub async fn post_text<S: TokenStore>(
    auth: &XPostAuth<S>,
    client_id: &str,
    text: &str,
) -> Result<PostedTweet, XPostError> {
    post_text_with(auth, client_id, text, &DEFAULT_RETRY_DELAYS).await
}

#[allow(dead_code)]
pub(crate) async fn post_text_with<S: TokenStore>(
    auth: &XPostAuth<S>,
    client_id: &str,
    text: &str,
    retry_delays: &[Duration],
) -> Result<PostedTweet, XPostError> {
    let mut token = auth.access_token(client_id).await?;
    let mut retries = 0;
    let mut refreshed = false;
    let mut waited_rate_limit = false;

    loop {
        match send_tweet(auth.http(), &auth.endpoints().tweets, &token, text).await {
            Attempt::ConnectFailed => {
                if retries < retry_delays.len() {
                    tokio::time::sleep(retry_delays[retries]).await;
                    retries += 1;
                    continue;
                }
                return Err(XPostError::new(XPostErrorCode::Network));
            }
            // 送信後のタイムアウト・本文読取失敗は投稿済みかもしれない。
            Attempt::Uncertain => return Err(XPostError::new(XPostErrorCode::Uncertain)),
            // 送信できたか判別できない失敗は二重投稿を避けて不明扱い。
            Attempt::Failed => return Err(XPostError::new(XPostErrorCode::Uncertain)),
            Attempt::Response {
                status,
                headers,
                body,
            } => {
                if status == 401 {
                    if refreshed {
                        return Err(XPostError::new(XPostErrorCode::ReloginRequired));
                    }
                    auth.invalidate_access_token().await;
                    token = auth.access_token(client_id).await?;
                    refreshed = true;
                    retries = 0;
                    continue;
                }
                if status == 429 {
                    if !waited_rate_limit {
                        if let Some(reset) = rate_limit_reset(&headers) {
                            let wait = reset.saturating_sub(now_unix());
                            if wait <= MAX_RATE_LIMIT_WAIT_SECS {
                                tokio::time::sleep(Duration::from_secs(wait)).await;
                                waited_rate_limit = true;
                                retries = 0;
                                continue;
                            }
                        }
                    }
                    return Err(XPostError::new(XPostErrorCode::RateLimited));
                }
                return classify_post_response(status, &body);
            }
        }
    }
}

enum Attempt {
    Response {
        status: u16,
        headers: reqwest::header::HeaderMap,
        body: String,
    },
    ConnectFailed,
    Uncertain,
    Failed,
}

async fn send_tweet(client: &reqwest::Client, url: &str, token: &str, text: &str) -> Attempt {
    let response = match client
        .post(url)
        .bearer_auth(token)
        .json(&json!({ "text": text }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(error) => {
            if error.is_connect() {
                return Attempt::ConnectFailed;
            }
            if error.is_timeout() {
                return Attempt::Uncertain;
            }
            return Attempt::Failed;
        }
    };
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    match response.text().await {
        Ok(body) => Attempt::Response {
            status,
            headers,
            body,
        },
        Err(_) => Attempt::Uncertain,
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn rate_limit_reset(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get("x-rate-limit-reset")?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

pub fn classify_post_response(status: u16, body: &str) -> Result<PostedTweet, XPostError> {
    let value: Option<Value> = serde_json::from_str(body).ok();

    // 成功の判定を最優先する(成功を失敗と誤判定すると再試行で二重投稿になる)。
    if (200..300).contains(&status) {
        let id = value
            .as_ref()
            .and_then(|value| value.pointer("/data/id"))
            .and_then(|id| id.as_str())
            .unwrap_or("")
            .to_string();
        return Ok(PostedTweet { id });
    }

    // クレジット切れは本文の title/type/detail でも判定する(HTTP 402 と同扱い)。
    let credits_depleted = value
        .as_ref()
        .map(|value| {
            ["title", "type", "detail"].iter().any(|key| {
                value
                    .get(*key)
                    .and_then(|field| field.as_str())
                    .map(|text| text.to_ascii_lowercase().contains("credits"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    // 5xx は投稿済みの可能性があるので本文で失敗扱いにしない。
    if status == 402 || ((400..500).contains(&status) && credits_depleted) {
        return Err(XPostError::new(XPostErrorCode::InsufficientCredits));
    }

    if status == 403 {
        let duplicate = value
            .as_ref()
            .and_then(|value| value.get("detail"))
            .and_then(|detail| detail.as_str())
            .map(|detail| detail.to_ascii_lowercase().contains("duplicate"))
            .unwrap_or(false);
        return Err(XPostError::new(if duplicate {
            XPostErrorCode::Duplicate
        } else {
            XPostErrorCode::Forbidden
        }));
    }

    let code = match status {
        401 => XPostErrorCode::ReloginRequired,
        429 => XPostErrorCode::RateLimited,
        500..=599 => XPostErrorCode::Uncertain,
        _ => XPostErrorCode::Other,
    };
    Err(XPostError::new(code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopost::error::XPostErrorCode;
    use crate::autopost::oauth::{MemoryTokenStore, StoredXToken, XEndpoints};
    use axum::{
        extract::State,
        http::{HeaderMap, HeaderValue, StatusCode},
        response::{IntoResponse, Response},
        routing::post,
        Json, Router,
    };
    use serde_json::Value;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex as StdMutex};
    use tokio::net::TcpListener;

    #[derive(Default)]
    struct ApiServer {
        tweets_calls: AtomicUsize,
        token_calls: AtomicUsize,
        token_forms: StdMutex<Vec<String>>,
        tweets_bodies: StdMutex<Vec<String>>,
        tweets_auth: StdMutex<Vec<Option<String>>>,
        tweets_queue: StdMutex<VecDeque<(u16, Value, Option<String>)>>,
        token_queue: StdMutex<VecDeque<(u16, Value)>>,
    }

    async fn tweets_handler(
        State(server): State<Arc<ApiServer>>,
        headers: HeaderMap,
        body: String,
    ) -> Response {
        server.tweets_calls.fetch_add(1, Ordering::SeqCst);
        server.tweets_bodies.lock().unwrap().push(body);
        server.tweets_auth.lock().unwrap().push(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        );
        match server.tweets_queue.lock().unwrap().pop_front() {
            Some((status, value, reset)) => {
                let mut response =
                    (StatusCode::from_u16(status).unwrap(), Json(value)).into_response();
                if let Some(reset) = reset {
                    let value: HeaderValue = reset.parse().unwrap();
                    response.headers_mut().insert("x-rate-limit-reset", value);
                }
                response
            }
            None => (StatusCode::OK, Json(json!({"data": {"id": "tweet-1"}}))).into_response(),
        }
    }

    async fn token_handler(
        State(server): State<Arc<ApiServer>>,
        body: String,
    ) -> (StatusCode, Json<Value>) {
        server.token_calls.fetch_add(1, Ordering::SeqCst);
        server.token_forms.lock().unwrap().push(body);
        match server.token_queue.lock().unwrap().pop_front() {
            Some((status, value)) => (StatusCode::from_u16(status).unwrap(), Json(value)),
            None => (
                StatusCode::OK,
                Json(json!({
                    "access_token": "access-new",
                    "refresh_token": "refresh-new",
                    "expires_in": 7200
                })),
            ),
        }
    }

    async fn spawn_api_server() -> (String, Arc<ApiServer>) {
        let server = Arc::new(ApiServer::default());
        let app = Router::new()
            .route("/tweets", post(tweets_handler))
            .route("/token", post(token_handler))
            .with_state(server.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base, server)
    }

    fn endpoints(base: &str) -> XEndpoints {
        XEndpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            tweets: format!("{base}/tweets"),
            users_me: format!("{base}/users/me"),
        }
    }

    fn stored() -> StoredXToken {
        StoredXToken {
            client_id: "client-1".to_string(),
            refresh_token: "r1".to_string(),
            user_id: "42".to_string(),
            username: "alice".to_string(),
        }
    }

    async fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    #[tokio::test]
    async fn post_401_refreshes_and_resends_once() {
        let (base, server) = spawn_api_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store.clone(), endpoints(&base)).unwrap();
        assert_eq!(auth.access_token("client-1").await.unwrap(), "access-new");
        server.token_calls.store(0, Ordering::SeqCst);
        server.token_forms.lock().unwrap().clear();

        server.tweets_queue.lock().unwrap().push_back((
            401,
            json!({"title": "Unauthorized"}),
            None,
        ));
        server.tweets_queue.lock().unwrap().push_back((
            200,
            json!({"data": {"id": "tweet-42"}}),
            None,
        ));

        let posted = post_text(&auth, "client-1", "hello").await.unwrap();
        assert_eq!(posted.id, "tweet-42");
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 2);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);

        let bodies = server.tweets_bodies.lock().unwrap();
        assert!(bodies[0].contains("hello"));
        let auths = server.tweets_auth.lock().unwrap();
        assert_eq!(auths[0].as_deref(), Some("Bearer access-new"));
        let forms = server.token_forms.lock().unwrap();
        assert_eq!(forms.len(), 1);
        assert!(forms[0].contains("grant_type=refresh_token"));
    }

    #[tokio::test]
    async fn post_server_error_is_uncertain_without_retry() {
        let (base, server) = spawn_api_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store, endpoints(&base)).unwrap();
        auth.access_token("client-1").await.unwrap();
        server.tweets_queue.lock().unwrap().push_back((
            503,
            json!({"title": "Service Unavailable"}),
            None,
        ));

        let error = post_text(&auth, "client-1", "hello").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Uncertain);
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn post_connect_failure_retries_then_network() {
        let (base, _server) = spawn_api_server().await;
        let closed = free_port().await;
        let mut endpoints = endpoints(&base);
        endpoints.tweets = format!("http://127.0.0.1:{closed}/tweets");
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store, endpoints).unwrap();
        auth.access_token("client-1").await.unwrap();

        let delays = [Duration::from_millis(100), Duration::from_millis(100)];
        let started = std::time::Instant::now();
        let error = post_text_with(&auth, "client-1", "hello", &delays)
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Network);
        assert!(started.elapsed() >= Duration::from_millis(190));
    }

    #[tokio::test]
    async fn rate_limit_near_reset_retries_once() {
        let (base, server) = spawn_api_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store, endpoints(&base)).unwrap();
        auth.access_token("client-1").await.unwrap();

        server.tweets_queue.lock().unwrap().push_back((
            429,
            json!({"title": "Too Many Requests"}),
            Some(now_unix().to_string()),
        ));
        server.tweets_queue.lock().unwrap().push_back((
            200,
            json!({"data": {"id": "tweet-ok"}}),
            None,
        ));

        let posted = post_text(&auth, "client-1", "hello").await.unwrap();
        assert_eq!(posted.id, "tweet-ok");
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rate_limit_far_reset_is_rate_limited() {
        let (base, server) = spawn_api_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store, endpoints(&base)).unwrap();
        auth.access_token("client-1").await.unwrap();

        server.tweets_queue.lock().unwrap().push_back((
            429,
            json!({"title": "Too Many Requests"}),
            Some((now_unix() + 1000).to_string()),
        ));

        let error = post_text(&auth, "client-1", "hello").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::RateLimited);
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn second_401_after_refresh_is_relogin_without_third_send() {
        let (base, server) = spawn_api_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored()));
        let auth = XPostAuth::new(store.clone(), endpoints(&base)).unwrap();
        auth.access_token("client-1").await.unwrap();
        for _ in 0..2 {
            server.tweets_queue.lock().unwrap().push_back((
                401,
                json!({"title": "Unauthorized"}),
                None,
            ));
        }
        let error = post_text(&auth, "client-1", "hello").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::ReloginRequired);
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 2);
        // refresh は single-use: 回転後の refresh token が先に保存されている。
        assert_eq!(store.token().unwrap().refresh_token, "refresh-new");
    }

    #[tokio::test]
    async fn not_logged_in_never_sends() {
        let (base, server) = spawn_api_server().await;
        let auth = XPostAuth::new(Arc::new(MemoryTokenStore::new()), endpoints(&base)).unwrap();
        let error = post_text(&auth, "client-1", "hello").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::NotLoggedIn);
        assert_eq!(server.tweets_calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn server_error_mentioning_credits_stays_uncertain() {
        let body = r#"{"title":"CreditsDepleted","detail":"credits"}"#;
        assert_eq!(
            classify_post_response(503, body).unwrap_err().code,
            XPostErrorCode::Uncertain
        );
        // 2xx は本文に credits とあっても成功(成功を失敗扱いにすると再試行で二重投稿になる)。
        assert_eq!(
            classify_post_response(201, r#"{"data":{"id":"t"},"title":"credits"}"#)
                .unwrap()
                .id,
            "t"
        );
    }

    #[test]
    fn classify_maps_known_responses() {
        let payment = r#"{"title":"Payment Required","detail":"credits depleted","type":"https://api.x.com/2/problems/credits-depleted","status":402}"#;
        assert_eq!(
            classify_post_response(402, payment).unwrap_err().code,
            XPostErrorCode::InsufficientCredits
        );
        let depleted = r#"{"title":"CreditsDepleted","detail":"Your enrolled account [id] does not have any credits to fulfill this request.","type":""}"#;
        assert_eq!(
            classify_post_response(402, depleted).unwrap_err().code,
            XPostErrorCode::InsufficientCredits
        );
        let duplicate = r#"{"detail":"You are not allowed to create a Tweet with duplicate content.","type":"about:blank","title":"Forbidden","status":403}"#;
        assert_eq!(
            classify_post_response(403, duplicate).unwrap_err().code,
            XPostErrorCode::Duplicate
        );
        assert_eq!(
            classify_post_response(403, "{}").unwrap_err().code,
            XPostErrorCode::Forbidden
        );
        assert_eq!(
            classify_post_response(200, r#"{"data":{"id":"abc"}}"#)
                .unwrap()
                .id,
            "abc"
        );
        assert_eq!(
            classify_post_response(200, r#"{"data":{}}"#).unwrap().id,
            ""
        );
        assert_eq!(classify_post_response(200, "not json").unwrap().id, "");
        assert_eq!(
            classify_post_response(401, "{}").unwrap_err().code,
            XPostErrorCode::ReloginRequired
        );
        assert_eq!(
            classify_post_response(429, "{}").unwrap_err().code,
            XPostErrorCode::RateLimited
        );
        assert_eq!(
            classify_post_response(500, "{}").unwrap_err().code,
            XPostErrorCode::Uncertain
        );
        assert_eq!(
            classify_post_response(404, "{}").unwrap_err().code,
            XPostErrorCode::Other
        );
        assert_eq!(
            classify_post_response(403, r#"{"detail":"credits depleted"}"#)
                .unwrap_err()
                .code,
            XPostErrorCode::InsufficientCredits
        );
    }
}
