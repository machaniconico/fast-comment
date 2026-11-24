//! 受け入れテスト共通の小道具。付録 D の公開面(`api::router` / `ApiGate` など)だけに依存する。
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode};
use axum::Router;
use fast_comment_lib::api::{self, ApiDeps, ApiFeed, ApiGate};
use fast_comment_lib::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

/// テストで bind している体にするポート。実ソケットは開かない。
pub const PORT: u16 = 47891;

pub fn host_ok() -> String {
    format!("127.0.0.1:{PORT}")
}

/// 開いたゲートと、それを通るトークン。
pub fn open_gate() -> (ApiGate, String) {
    let gate = ApiGate::closed();
    let token = api::generate_token();
    gate.open(&token);
    (gate, token)
}

pub fn api_app(gate: &ApiGate) -> Router {
    api::router(ApiDeps::new(gate.clone(), CancellationToken::new()), PORT)
}

/// 要求の組み立て。既定は `Host: 127.0.0.1:<PORT>` のみ(Authorization・Origin は付けない)。
pub struct Req {
    method: Method,
    uri: String,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Vec<u8>,
}

pub fn req(method: Method, uri: &str) -> Req {
    Req { method, uri: uri.to_string(), headers: vec![], body: vec![] }
        .header("host", &host_ok())
}

pub fn get(uri: &str) -> Req {
    req(Method::GET, uri)
}

impl Req {
    /// 同名ヘッダを置き換える(複数付けたい時は `push_header`)。
    pub fn header(mut self, name: &str, value: &str) -> Self {
        let name = HeaderName::from_bytes(name.as_bytes()).unwrap();
        self.headers.retain(|(n, _)| *n != name);
        self.headers.push((name, HeaderValue::from_str(value).unwrap()));
        self
    }
    pub fn push_header(mut self, name: &str, value: HeaderValue) -> Self {
        self.headers.push((HeaderName::from_bytes(name.as_bytes()).unwrap(), value));
        self
    }
    pub fn raw_header(mut self, name: &str, value: &[u8]) -> Self {
        self.headers.retain(|(n, _)| n.as_str() != name);
        self.headers.push((
            HeaderName::from_bytes(name.as_bytes()).unwrap(),
            HeaderValue::from_bytes(value).unwrap(),
        ));
        self
    }
    pub fn without(mut self, name: &str) -> Self {
        self.headers.retain(|(n, _)| n.as_str() != name);
        self
    }
    pub fn host(self, value: &str) -> Self {
        self.header("host", value)
    }
    pub fn origin(self, value: &str) -> Self {
        self.header("origin", value)
    }
    pub fn bearer(self, token: &str) -> Self {
        self.header("authorization", &format!("Bearer {token}"))
    }
    pub fn authorization(self, value: &str) -> Self {
        self.header("authorization", value)
    }
    pub fn json_body(mut self, body: &str) -> Self {
        self.body = body.as_bytes().to_vec();
        self.header("content-type", "application/json")
    }
    fn build(self) -> Request<Body> {
        let mut b = Request::builder().method(self.method).uri(self.uri);
        for (n, v) in self.headers {
            b = b.header(n, v);
        }
        b.body(Body::from(self.body)).unwrap()
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: HeaderMap,
    pub body: Vec<u8>,
}

impl Resp {
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body)
            .unwrap_or_else(|e| panic!("本文が JSON でない: {e}: {:?}", String::from_utf8_lossy(&self.body)))
    }
    pub fn error_code(&self) -> String {
        self.json()["error"]["code"].as_str().unwrap_or_default().to_string()
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
    }
    pub fn cors_header_names(&self) -> Vec<String> {
        self.headers
            .keys()
            .map(|k| k.as_str().to_string())
            .filter(|k| k.starts_with("access-control-"))
            .collect()
    }
}

pub async fn send(app: &Router, r: Req) -> Resp {
    let resp = app.clone().oneshot(r.build()).await.expect("router は失敗しない");
    let (parts, body) = resp.into_parts();
    let body = body.collect().await.expect("本文を読める").to_bytes().to_vec();
    Resp { status: parts.status, headers: parts.headers, body }
}

// ---- PR2: コメント取得・SSE 用の小道具(付録 D の ApiFeed / with_feed だけに依存) ----

/// 開いたゲート + 有効化済みフィードを積んだルータ一式。
pub struct FeedEnv {
    pub app: Router,
    pub feed: ApiFeed,
    pub gate: ApiGate,
    pub token: String,
}

pub fn feed_env(capacity: usize) -> FeedEnv {
    let (gate, token) = open_gate();
    let feed = ApiFeed::new(capacity);
    feed.set_enabled(true);
    let deps = ApiDeps::new(gate.clone(), CancellationToken::new()).with_feed(feed.clone());
    FeedEnv { app: api::router(deps, PORT), feed, gate, token }
}

impl FeedEnv {
    /// 認証付き GET(`path` は `/api/v1/...` から)。
    pub async fn get(&self, path: &str) -> Resp {
        send(&self.app, get(path).bearer(&self.token)).await
    }
    /// 今の epoch(`GET /status`)。
    pub async fn epoch(&self) -> String {
        let r = self.get("/api/v1/status").await;
        assert_eq!(r.status, StatusCode::OK);
        r.json()["epoch"].as_str().expect("epoch は文字列").to_string()
    }
}

/// 既定の ChatMessage(twitch / チャンネル "chan" / 著者 "u1" / 本文 "hello <id>")。
pub fn msg(id: &str) -> ChatMessage {
    ChatMessage {
        listener: None,
        id: id.to_string(),
        native_id: false,
        platform: Platform::Twitch,
        channel: "chan".to_string(),
        author: Author {
            nickname: None,
            avatar_url: None,
            id: "u1".to_string(),
            name: "Alice".to_string(),
            display_color: None,
            badges: vec![],
            roles: Roles::default(),
        },
        fragments: vec![Fragment::text(format!("hello {id}"))],
        kind: MessageKind::Normal,
        amount: None,
        timestamp_ms: 1_731_600_000_000,
        raw: None,
        skip_tts: false,
        injected_test: false,
        route: Default::default(),
        sound: None,
    }
}
