//! Twitch 公式 Polls 受け入れテストの共通部。ローカルの HTTP モック(ポート 0 で bind)。
//! 正本: .omc/plans/poll-pr2-twitch.md
#![allow(dead_code)]

use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, Method, Uri};
use axum::response::IntoResponse;
use axum::Router;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const TOKEN: &str = "tok-SECRET-9f3a1c";
pub const CLIENT_ID: &str = "cid-SECRET-77be02";

#[derive(Clone, Debug)]
pub struct Req {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    /// キーは小文字。
    pub headers: HashMap<String, String>,
    pub body: String,
}

impl Req {
    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or_else(|e| panic!("本文が JSON でない: {e}: {:?}", self.body))
    }
}

#[derive(Clone)]
pub struct Reply {
    pub status: u16,
    pub body: String,
    pub headers: Vec<(String, String)>,
}

impl Reply {
    pub fn json(status: u16, v: serde_json::Value) -> Reply {
        Reply { status, body: v.to_string(), headers: vec![("content-type".into(), "application/json".into())] }
    }
    pub fn header(mut self, k: &str, v: &str) -> Reply {
        self.headers.push((k.to_string(), v.to_string()));
        self
    }
}

type Responder = Box<dyn FnMut(&Req) -> Reply + Send>;

struct Inner {
    reqs: Mutex<Vec<Req>>,
    responder: Mutex<Responder>,
}

pub struct Mock {
    /// 本番の `https://api.twitch.tv/helix` に相当(末尾に /helix を付けたローカル URL)。
    pub base: String,
    inner: Arc<Inner>,
}

async fn handle(State(inner): State<Arc<Inner>>, method: Method, uri: Uri, headers: HeaderMap, body: Bytes) -> axum::response::Response {
    let query = uri
        .query()
        .map(|q| url::form_urlencoded::parse(q.as_bytes()).into_owned().collect::<HashMap<_, _>>())
        .unwrap_or_default();
    let headers = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_ascii_lowercase(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect::<HashMap<_, _>>();
    let req = Req {
        method: method.to_string(),
        path: uri.path().to_string(),
        query,
        headers,
        body: String::from_utf8_lossy(&body).into_owned(),
    };
    inner.reqs.lock().unwrap().push(req.clone());
    let reply = (inner.responder.lock().unwrap())(&req);
    let mut resp = (axum::http::StatusCode::from_u16(reply.status).unwrap(), reply.body).into_response();
    for (k, v) in reply.headers {
        resp.headers_mut().insert(
            axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            axum::http::HeaderValue::from_str(&v).unwrap(),
        );
    }
    resp
}

impl Mock {
    pub async fn start() -> Mock {
        let inner = Arc::new(Inner {
            reqs: Mutex::new(Vec::new()),
            responder: Mutex::new(Box::new(|_: &Req| Reply::json(404, serde_json::json!({})))),
        });
        let app = Router::new().fallback(handle).with_state(inner.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/helix", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Mock { base, inner }
    }

    pub fn respond<F: FnMut(&Req) -> Reply + Send + 'static>(&self, f: F) {
        *self.inner.responder.lock().unwrap() = Box::new(f);
    }

    /// 全要求に同じ応答を返す。
    pub fn always(&self, reply: Reply) {
        self.respond(move |_| reply.clone());
    }

    pub fn requests(&self) -> Vec<Req> {
        self.inner.reqs.lock().unwrap().clone()
    }

    pub fn last(&self) -> Req {
        self.requests().pop().expect("要求が 1 件も届いていない")
    }
}

/// 接続拒否になるベース URL(bind してすぐ閉じたポート)。
pub async fn refused_base() -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    drop(l);
    format!("http://{addr}/helix")
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}
