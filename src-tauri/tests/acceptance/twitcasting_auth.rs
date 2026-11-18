//! PR1(ツイキャス)受け入れテスト: OAuth(Implicit)・ループバック待受・トークン保存(設計書 twitcasting §1・§5.2、付録 A)。
//! 資格情報ストアは `MemorySecretStore`、偽のツイキャスは `127.0.0.1:0` の axum、ブラウザ役は生 TCP の HTTP。
//! トークンはすべて偽の文字列。実ネットワーク・実ブラウザは使わない。
//!
//! 付録 A に無く、このファイルが前提にする物(曖昧点。設計書 §1.3 の文言から最も文字通りに読んだ形):
//! - `TwitcastingAuth::bearer(&self, client_id: Option<&str>, now_ms: u64) -> Option<_>`(`status` と同じ引数。値の型は見ない)。
//! - `load` / `status` の `client_id` は「今の有効 ClientID」(`effective_client_id` の結果)。
//! - `TwitcastingAuth` / `TwitcastingAuthError` / `TwitcastingAuthErrorCode` は `Debug` を実装する(§5.2 の Debug テストが前提)。
//!   エラーコードの比較は `Serialize` 経由(camelCase 文字列)で行い、`PartialEq` には依存しない。
//! - 保存値の JSON は §1.3 のとおり camelCase(`clientId` / `accessToken` / `userId` / `screenId` / `name` / `expiresAtMs` / `obtainedAtMs`)。
//! - 偽 verify の `user.id` は文字列。`user_id()` は同じ文字列を返す。
#![allow(dead_code)]

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use fast_comment_lib::config::AppConfig;
use fast_comment_lib::integrations::{MemorySecretStore, SecretStore};
use fast_comment_lib::sources::twitcasting_auth::{
    build_authorize_url, effective_client_id, AuthOptions, AuthState, TwitcastingAuth, TwitcastingAuthError,
    TwitcastingAuthErrorCode, TwitcastingEndpoints, TwitcastingStatus, CALLBACK_PATH, CALLBACK_PORT,
    DEFAULT_CLIENT_ID, TOKEN_KEY, TOKEN_PATH,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const NOW: u64 = 1_731_600_000_000;
const EXPIRES_IN_MS: u64 = 15_552_000_000;
const CLIENT: &str = "test-client-id";
const TOKEN: &str = "tc-secret-token-abcdef0123456789";
const USER_ID: &str = "1234567";
const SCREEN_ID: &str = "fake_screen";
const NAME: &str = "Fake Name";
const CSP: &str = "default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'";

// ---------------------------------------------------------------- 偽のツイキャス

#[derive(Clone)]
struct FakeState {
    app_client_id: String,
    status: u16,
    seen: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

struct Fake {
    base: String,
    seen: Arc<Mutex<Vec<HashMap<String, String>>>>,
}

async fn verify(State(s): State<FakeState>, headers: HeaderMap) -> axum::response::Response {
    let h = headers
        .iter()
        .map(|(k, v)| (k.as_str().to_string(), String::from_utf8_lossy(v.as_bytes()).into_owned()))
        .collect();
    s.seen.lock().unwrap().push(h);
    let body = if s.status == 200 {
        json!({
            "app": { "client_id": s.app_client_id },
            "user": { "id": USER_ID, "screen_id": SCREEN_ID, "name": NAME }
        })
    } else {
        json!({ "error": { "code": 1000, "message": "Invalid token." } })
    };
    (StatusCode::from_u16(s.status).unwrap(), [("content-type", "application/json")], body.to_string()).into_response()
}

async fn fake(app_client_id: &str, status: u16) -> Fake {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let st = FakeState { app_client_id: app_client_id.to_string(), status, seen: seen.clone() };
    let app = Router::new().route("/verify_credentials", get(verify)).with_state(st);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Fake { base, seen }
}

// ---------------------------------------------------------------- 環境

fn free_port() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.local_addr().unwrap().port()
}

struct Env {
    auth: Arc<TwitcastingAuth<MemorySecretStore>>,
    store: MemorySecretStore,
    port: u16,
}

fn build(store: MemorySecretStore, api_base: String, login_timeout: Duration) -> Env {
    let port = free_port();
    let endpoints = TwitcastingEndpoints { authorize: "http://127.0.0.1:9/oauth2/authorize".into(), api_base };
    let options = AuthOptions { port, login_timeout, read_timeout: Duration::from_secs(2) };
    Env { auth: TwitcastingAuth::new(store.clone(), endpoints, options), store, port }
}

fn env_with(f: &Fake) -> Env {
    build(MemorySecretStore::new(), f.base.clone(), Duration::from_secs(10))
}

/// 検証先を使わない環境(`load` / `status` / `disconnect` 用)。
fn env_plain() -> Env {
    build(MemorySecretStore::new(), format!("http://127.0.0.1:{}", free_port()), Duration::from_secs(10))
}

fn record(client: &str, token: &str, expires: Option<u64>) -> String {
    json!({
        "clientId": client, "accessToken": token, "userId": USER_ID, "screenId": SCREEN_ID,
        "name": NAME, "expiresAtMs": expires, "obtainedAtMs": NOW - 1
    })
    .to_string()
}

async fn seeded(client: &str, expires: Option<u64>) -> Env {
    let env = env_plain();
    env.store.put(TOKEN_KEY, &record(client, TOKEN, expires)).await.unwrap();
    env
}

fn stored(env: &Env) -> Option<Value> {
    env.store.raw(TOKEN_KEY).map(|s| serde_json::from_str(&s).expect("保存値は JSON"))
}

// ---------------------------------------------------------------- 結果の読み取り

fn code_str(c: &TwitcastingAuthErrorCode) -> String {
    serde_json::to_value(c).unwrap().as_str().expect("コードは文字列").to_string()
}

fn err_of<T>(r: Result<T, TwitcastingAuthError>) -> TwitcastingAuthError {
    match r {
        Ok(_) => panic!("Err のはずが Ok"),
        Err(e) => e,
    }
}

fn ok_of<T>(r: Result<T, TwitcastingAuthError>) -> T {
    match r {
        Ok(v) => v,
        Err(e) => panic!("Ok のはずが Err(code={})", code_str(&e.code)),
    }
}

fn sj(s: &TwitcastingStatus) -> Value {
    serde_json::to_value(s).unwrap()
}

fn status_json(env: &Env, client: Option<&str>, now: u64) -> Value {
    sj(&env.auth.status(client, now))
}

// ---------------------------------------------------------------- ブラウザ役(生の HTTP)

struct Raw {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Raw {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }
}

fn http(method: &str, path: &str, host: &str, origin: Option<&str>, body: &str) -> String {
    let mut s = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    if let Some(o) = origin {
        s.push_str(&format!("Origin: {o}\r\n"));
    }
    s.push_str(&format!(
        "Content-Type: application/x-www-form-urlencoded\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    ));
    s
}

/// 接続できない・応答が無い時は `None`。途中で切られても読めた分は解釈する。
async fn raw_request(port: u16, request: String) -> Option<Raw> {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    let _ = s.write_all(request.as_bytes()).await;
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf).into_owned();
    let (head, body) = text.split_once("\r\n\r\n")?;
    let mut lines = head.split("\r\n");
    let status = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    Some(Raw { status, headers, body: body.to_string() })
}

fn local_host(port: u16) -> String {
    format!("localhost:{port}")
}

async fn post_token(port: u16, host: &str, origin: Option<&str>, body: &str) -> Option<Raw> {
    raw_request(port, http("POST", TOKEN_PATH, host, origin, body)).await
}

fn success_body(state: &str) -> String {
    format!("access_token={TOKEN}&token_type=bearer&expires_in=15552000&state={state}")
}

// ---------------------------------------------------------------- ログインの進行

type LoginResult = Result<TwitcastingStatus, TwitcastingAuthError>;

struct Login {
    handle: tokio::task::JoinHandle<LoginResult>,
    state: String,
}

fn state_of(url: &str) -> String {
    url::Url::parse(url)
        .expect("認可 URL は URL")
        .query_pairs()
        .find(|(k, _)| k == "state")
        .map(|(_, v)| v.into_owned())
        .expect("認可 URL に state がある")
}

/// `connect` を裏で走らせ、`open_browser` に渡った URL から `state` を取り出して返す。
async fn begin(env: &Env, client: &str, now: u64) -> Login {
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let auth = env.auth.clone();
    let client = client.to_string();
    let handle = tokio::spawn(async move {
        auth.connect(&client, now, move |url| {
            let _ = tx.send(url);
            Ok(())
        })
        .await
    });
    let url = tokio::time::timeout(Duration::from_secs(5), rx)
        .await
        .expect("open_browser が呼ばれる")
        .expect("open_browser が呼ばれる前に connect が終わった");
    Login { handle, state: state_of(&url) }
}

async fn wait(handle: tokio::task::JoinHandle<LoginResult>) -> LoginResult {
    tokio::time::timeout(Duration::from_secs(5), handle).await.expect("connect が終わる").expect("タスクが落ちない")
}

/// ログインを始め、`body(state)` を正しい Host で POST し、`connect` の結果を返す。
async fn run_login(env: &Env, client: &str, now: u64, body: impl FnOnce(&str) -> String) -> (LoginResult, String) {
    let login = begin(env, client, now).await;
    let state = login.state.clone();
    let b = body(&state);
    let _ = post_token(env.port, &local_host(env.port), None, &b).await;
    (wait(login.handle).await, state)
}

/// 直前の拒否のあともログインが待ち続けていて、正しい POST で成功することを確かめる。
async fn finish_ok(env: &Env, login: Login) -> TwitcastingStatus {
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!login.handle.is_finished(), "拒否された POST でログインが終わってはいけない");
    let r = post_token(env.port, &local_host(env.port), None, &success_body(&login.state))
        .await
        .expect("正しい POST に応答がある");
    assert_eq!(r.status / 100, 2, "正しい POST は成功する");
    ok_of(wait(login.handle).await)
}

async fn cancel_and_join(env: &Env, login: Login) {
    env.auth.cancel_login();
    assert_eq!(code_str(&err_of(wait(login.handle).await).code), "cancelled");
}

fn assert_port_free(port: u16) {
    std::net::TcpListener::bind(("127.0.0.1", port)).expect("ポートが解放されている");
}

// ================================================================ 認可 URL と ClientID

#[test]
fn authorize_url_has_only_client_id_response_type_token_and_state() {
    let endpoints = TwitcastingEndpoints {
        authorize: "https://apiv2.twitcasting.tv/oauth2/authorize".into(),
        api_base: "https://apiv2.twitcasting.tv".into(),
    };
    let url = url::Url::parse(&build_authorize_url(&endpoints, "cid-1", "st-1")).unwrap();
    let pairs: HashMap<String, String> = url.query_pairs().map(|(k, v)| (k.into_owned(), v.into_owned())).collect();
    let keys: HashSet<&str> = pairs.keys().map(|k| k.as_str()).collect();
    assert_eq!(keys, HashSet::from(["client_id", "response_type", "state"]));
    assert_eq!(pairs["client_id"], "cid-1");
    assert_eq!(pairs["response_type"], "token");
    assert_eq!(pairs["state"], "st-1");
    assert_eq!(url.query_pairs().count(), 3, "同じキーを重ねない");
}

#[test]
fn effective_client_id_prefers_trimmed_override_then_default() {
    assert_eq!(effective_client_id(Some("  abc ")), Some("abc".to_string()));
    let default = if DEFAULT_CLIENT_ID.is_empty() { None } else { Some(DEFAULT_CLIENT_ID.to_string()) };
    assert_eq!(effective_client_id(Some("")), default);
    assert_eq!(effective_client_id(None), default);
    // 設計書 §1.3: 「trimmed override」を文字通りに読み、空白だけの上書きは trim 後に空なので既定へ落とす。
    assert_eq!(effective_client_id(Some("   ")), default);
}

#[tokio::test]
async fn connect_without_client_id_fails_not_configured_without_binding_port() {
    let env = env_plain();
    let opened = Arc::new(AtomicBool::new(false));
    let o = opened.clone();
    // 設計書 §5.2: 「ClientID が無い」を空文字の `connect("")` で表す(`connect` の引数は `&str`)。
    let r = env
        .auth
        .connect("", NOW, move |_| {
            o.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert_eq!(code_str(&err_of(r).code), "notConfigured");
    assert!(!opened.load(Ordering::SeqCst), "ブラウザを開かない");
    assert_port_free(env.port);
}

// ================================================================ ループバック

#[tokio::test]
async fn callback_page_has_security_headers_and_no_state_in_body() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let r = raw_request(env.port, http("GET", CALLBACK_PATH, &local_host(env.port), None, ""))
        .await
        .expect("GET に応答がある");
    assert_eq!(r.status, 200);
    assert!(r.header("content-type").unwrap_or_default().starts_with("text/html"));
    assert_eq!(r.header("cache-control"), Some("no-store"));
    assert_eq!(r.header("referrer-policy"), Some("no-referrer"));
    assert_eq!(r.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(r.header("content-security-policy"), Some(CSP));
    assert!(!r.body.contains(&login.state), "本文に state を埋めない");
    assert!(!r.body.contains(TOKEN));
    cancel_and_join(&env, login).await;
}

#[tokio::test]
async fn valid_token_post_completes_login_and_stores_token() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    let status = sj(&ok_of(r));
    assert_eq!(status["state"], "ready");
    assert_eq!(status["screenId"], SCREEN_ID);
    // 期限の基準は grant 受領時刻(`obtainedAtMs`)。connect に渡した now 以上で、受領までの実時間ぶん後ろにずれうる。
    let expires = status["expiresAtMs"].as_u64().expect("expiresAtMs は数値");
    assert!((NOW + EXPIRES_IN_MS..=NOW + EXPIRES_IN_MS + 10_000).contains(&expires), "expiresAtMs={expires}");
    let rec = stored(&env).expect("ストアに保存済み");
    assert_eq!(rec["accessToken"], TOKEN);
    assert_eq!(rec["clientId"], CLIENT);
    assert_eq!(rec["screenId"], SCREEN_ID);
    assert_eq!(rec["expiresAtMs"].as_u64(), Some(expires));
    let obtained = rec["obtainedAtMs"].as_u64().expect("obtainedAtMs は数値");
    assert!(obtained >= NOW, "obtainedAtMs={obtained}");
    assert_eq!(obtained + EXPIRES_IN_MS, expires);
    assert_eq!(env.auth.user_id(), Some(USER_ID.to_string()));
}

#[tokio::test]
async fn wrong_state_is_rejected_400_and_login_keeps_waiting() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let r = post_token(env.port, &local_host(env.port), None, &success_body("not-the-state")).await.expect("応答");
    assert_eq!(r.status, 400);
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn missing_state_is_rejected_400_and_login_keeps_waiting() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let body = format!("access_token={TOKEN}&token_type=bearer&expires_in=15552000");
    let r = post_token(env.port, &local_host(env.port), None, &body).await.expect("応答");
    assert_eq!(r.status, 400);
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn token_in_query_string_is_never_accepted() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let q = format!("access_token={TOKEN}&token_type=bearer&expires_in=15552000&state={}", login.state);
    for path in [CALLBACK_PATH, TOKEN_PATH] {
        let _ = raw_request(env.port, http("GET", &format!("{path}?{q}"), &local_host(env.port), None, "")).await;
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!login.handle.is_finished(), "クエリのトークンでログインが進んではいけない");
    assert!(env.store.keys().is_empty(), "ストアは空");
    assert!(f.seen.lock().unwrap().is_empty(), "検証にも進まない");
    cancel_and_join(&env, login).await;
}

#[tokio::test]
async fn denied_result_fails_connect_with_denied_code() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    // 設計書 §1.2 ④: 拒否には state が付かない。state 無しの `result=denied` で `Denied` になる。
    let (r, _) = run_login(&env, CLIENT, NOW, |_| "result=denied".to_string()).await;
    assert_eq!(code_str(&err_of(r).code), "denied");
    assert!(env.store.keys().is_empty());
}

#[tokio::test]
async fn post_with_foreign_origin_is_rejected_403_and_login_keeps_waiting() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let r = post_token(env.port, &local_host(env.port), Some("https://evil.example"), &success_body(&login.state))
        .await
        .expect("応答");
    assert_eq!(r.status, 403);
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn post_with_foreign_host_is_rejected_403_and_login_keeps_waiting() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let host = format!("evil.example:{}", env.port);
    let r = post_token(env.port, &host, None, &success_body(&login.state)).await.expect("応答");
    assert_eq!(r.status, 403);
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn post_accepts_localhost_ipv4_and_ipv6_loopback_hosts() {
    let f = fake(CLIENT, 200).await;
    // 成功は 1 回だけなので、Host ごとに新しい環境で試す。接続自体は 127.0.0.1 で、見るのは Host ヘッダの扱いだけ。
    for name in ["localhost", "127.0.0.1", "[::1]"] {
        let env = env_with(&f);
        let host = format!("{name}:{}", env.port);
        let login = begin(&env, CLIENT, NOW).await;
        let r = post_token(env.port, &host, None, &success_body(&login.state)).await.expect("応答");
        assert_eq!(r.status / 100, 2, "Host {host} を受ける");
        ok_of(wait(login.handle).await);
    }
}

#[tokio::test]
async fn post_body_over_4096_bytes_is_rejected_and_login_keeps_waiting() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    // 正しい state を含めたうえで本文だけ 4096 バイトを超える。
    let body = format!("{}&pad={}", success_body(&login.state), "a".repeat(5000));
    let r = post_token(env.port, &local_host(env.port), None, &body).await;
    // 設計書 §1.2 ③: 状態コードは未指定。4xx、または上限超過で接続を切られて応答無し、のどちらも許す。
    if let Some(r) = r {
        assert_eq!(r.status / 100, 4, "超過は 4xx");
    }
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn empty_overlong_or_control_char_access_token_is_rejected() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let s = &login.state;
    let bad = [
        format!("access_token=&token_type=bearer&expires_in=15552000&state={s}"),
        format!("access_token={}&token_type=bearer&expires_in=15552000&state={s}", "a".repeat(2049)),
        // フォームは URL エンコードされるので、CR LF はパーセントエンコードで本文に入れる。
        format!("access_token=abc%0D%0Adef&token_type=bearer&expires_in=15552000&state={s}"),
    ];
    for body in &bad {
        let r = post_token(env.port, &local_host(env.port), None, body).await.expect("応答");
        // 設計書 §1.2 ⑥: 状態コードは未指定。拒否(4xx)でログインは続く。
        assert_eq!(r.status / 100, 4);
    }
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, login).await;
}

#[tokio::test]
async fn second_token_post_after_success_is_not_accepted() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, state) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    ok_of(r);
    let body = format!("access_token=tc-other-token&token_type=bearer&expires_in=15552000&state={state}");
    let second = post_token(env.port, &local_host(env.port), None, &body).await;
    // 待受は閉じているので、接続拒否(None)か 2xx 以外のどちらか。
    if let Some(r) = second {
        assert_ne!(r.status / 100, 2);
    }
    assert_eq!(stored(&env).unwrap()["accessToken"], TOKEN);
}

#[tokio::test]
async fn unknown_paths_and_methods_return_404() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    let host = local_host(env.port);
    for (method, path) in [("GET", "/"), ("PUT", TOKEN_PATH), ("POST", CALLBACK_PATH)] {
        let r = raw_request(env.port, http(method, path, &host, None, "")).await.expect("応答");
        assert_eq!(r.status, 404, "{method} {path}");
    }
    assert!(!login.handle.is_finished());
    cancel_and_join(&env, login).await;
}

#[tokio::test]
async fn login_times_out_and_port_is_reusable() {
    let f = fake(CLIENT, 200).await;
    let env = build(MemorySecretStore::new(), f.base.clone(), Duration::from_millis(300));
    let login = begin(&env, CLIENT, NOW).await;
    let e = err_of(wait(login.handle).await);
    assert_eq!(code_str(&e.code), "loginTimeout");
    assert_port_free(env.port);
}

#[tokio::test]
async fn cancel_login_returns_cancelled_and_frees_port() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    env.auth.cancel_login();
    assert_eq!(code_str(&err_of(wait(login.handle).await).code), "cancelled");
    assert_port_free(env.port);
}

#[tokio::test]
async fn second_connect_cancels_first_and_old_state_is_rejected() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let first = begin(&env, CLIENT, NOW).await;
    let second = begin(&env, CLIENT, NOW).await;
    assert_ne!(first.state, second.state);
    assert_eq!(code_str(&err_of(wait(first.handle).await).code), "cancelled");
    let r = post_token(env.port, &local_host(env.port), None, &success_body(&first.state)).await.expect("応答");
    assert_eq!(r.status, 400, "古い state は拒否");
    assert!(env.store.raw(TOKEN_KEY).is_none());
    finish_ok(&env, second).await;
}

#[tokio::test]
async fn occupied_port_returns_port_in_use() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let _held = std::net::TcpListener::bind(("127.0.0.1", env.port)).unwrap();
    let opened = Arc::new(AtomicBool::new(false));
    let o = opened.clone();
    let r = env
        .auth
        .connect(CLIENT, NOW, move |_| {
            o.store(true, Ordering::SeqCst);
            Ok(())
        })
        .await;
    assert_eq!(code_str(&err_of(r).code), "portInUse");
    assert!(!opened.load(Ordering::SeqCst), "始められないのでブラウザは開かない");
}

#[tokio::test]
async fn open_browser_failure_fails_connect_and_frees_port() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    // 設計書 §5.2: コードは未指定(`Other` 想定)。失敗することとポート解放だけを見る。
    let r = env.auth.connect(CLIENT, NOW, |_| Err("browser unavailable".to_string())).await;
    assert!(r.is_err());
    assert_port_free(env.port);
}

// ================================================================ 検証と保存

#[tokio::test]
async fn verify_request_sends_bearer_api_version_and_accept_headers() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    ok_of(r);
    let seen = f.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].get("authorization").map(String::as_str), Some(format!("Bearer {TOKEN}").as_str()));
    assert_eq!(seen[0].get("x-api-version").map(String::as_str), Some("2.0"));
    assert_eq!(seen[0].get("accept").map(String::as_str), Some("application/json"));
}

#[tokio::test]
async fn verify_client_id_mismatch_rejects_token_and_stores_nothing() {
    let f = fake("someone-elses-app", 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    assert_eq!(code_str(&err_of(r).code), "clientMismatch");
    assert!(env.store.keys().is_empty());
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut");
}

#[tokio::test]
async fn verify_401_rejects_token_and_stores_nothing() {
    let f = fake(CLIENT, 401).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    // 設計書 §5.2: コードは未指定。失敗とストア空だけを見る。
    assert!(r.is_err());
    assert!(env.store.keys().is_empty());
}

#[tokio::test]
async fn verify_network_failure_is_network_and_stores_nothing() {
    let env = env_plain(); // api_base は閉じたポート
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    assert_eq!(code_str(&err_of(r).code), "network");
    assert!(env.store.keys().is_empty());
}

#[tokio::test]
async fn store_put_failure_fails_connect_with_token_store_and_stays_logged_out() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    env.store.fail_next_put();
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    assert_eq!(code_str(&err_of(r).code), "tokenStore");
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut");
    assert!(env.store.keys().is_empty());
}

#[tokio::test]
async fn missing_expires_in_stores_no_expiry() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| format!("access_token={TOKEN}&token_type=bearer&state={s}")).await;
    let status = sj(&ok_of(r));
    assert_eq!(status["state"], "ready");
    assert!(status["expiresAtMs"].is_null());
    let rec = stored(&env).unwrap();
    assert!(rec.get("expiresAtMs").map_or(true, |v| v.is_null()));
}

#[tokio::test]
async fn unreadable_expires_in_fails_login_and_stores_nothing() {
    let f = fake(CLIENT, 200).await;
    // 値があるのに 1〜315360000 の 10 進整数(ASCII 数字のみ)として読めない物。
    for value in ["abc", "-1", "3600.0", "%203600", "0", "", "315360001", "99999999999999999999999"] {
        let env = env_with(&f);
        let (r, _) = run_login(&env, CLIENT, NOW, |s| {
            format!("access_token={TOKEN}&token_type=bearer&expires_in={value}&state={s}")
        })
        .await;
        assert_eq!(code_str(&err_of(r).code), "other", "expires_in={value:?}");
        assert!(env.store.raw(TOKEN_KEY).is_none(), "expires_in={value:?}");
        assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut", "expires_in={value:?}");
        assert_port_free(env.port);
    }
}

#[tokio::test]
async fn expires_in_ten_years_is_accepted() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| {
        format!("access_token={TOKEN}&token_type=bearer&expires_in=315360000&state={s}")
    })
    .await;
    assert_eq!(sj(&ok_of(r))["state"], "ready");
    let rec = stored(&env).unwrap();
    let obtained = rec["obtainedAtMs"].as_u64().expect("obtainedAtMs は数値");
    assert_eq!(rec["expiresAtMs"].as_u64(), Some(obtained + 315_360_000_000));
}

#[tokio::test]
async fn unreadable_expires_in_keeps_previous_token() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    env.store.put(TOKEN_KEY, &record(CLIENT, "tc-old-token", Some(NOW + 1_000_000))).await.unwrap();
    env.auth.load(Some(CLIENT), NOW).await;
    let (r, _) = run_login(&env, CLIENT, NOW, |s| {
        format!("access_token={TOKEN}&token_type=bearer&expires_in=abc&state={s}")
    })
    .await;
    assert_eq!(code_str(&err_of(r).code), "other");
    assert_eq!(stored(&env).expect("旧レコードが残る")["accessToken"], "tc-old-token");
}

#[tokio::test]
async fn expiry_counts_from_grant_receipt_not_connect_start() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let login = begin(&env, CLIENT, NOW).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let body = format!("access_token={TOKEN}&token_type=bearer&expires_in=1&state={}", login.state);
    let _ = post_token(env.port, &local_host(env.port), None, &body).await;
    let status = sj(&ok_of(wait(login.handle).await));
    assert_eq!(status["state"], "ready");
    // 実時間の期限フラグで不安定になるので、このあとの status(now) は見ない。
    let rec = stored(&env).unwrap();
    let obtained = rec["obtainedAtMs"].as_u64().expect("obtainedAtMs は数値");
    assert!(obtained >= NOW + 1200, "obtainedAtMs={obtained}");
    assert_eq!(rec["expiresAtMs"].as_u64(), Some(obtained + 1000));
}

#[tokio::test]
async fn relogin_overwrites_previous_token() {
    let f = fake(CLIENT, 200).await;
    let env = env_with(&f);
    let (r, _) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    ok_of(r);
    let (r, _) = run_login(&env, CLIENT, NOW + 1000, |s| {
        format!("access_token=tc-second-token&token_type=bearer&expires_in=15552000&state={s}")
    })
    .await;
    ok_of(r);
    assert_eq!(stored(&env).unwrap()["accessToken"], "tc-second-token");
    assert_eq!(env.store.keys().len(), 1, "同じキーを上書きする");
}

// ================================================================ 状態・失効・ログアウト

#[tokio::test]
async fn load_restores_ready_state_from_store() {
    let env = seeded(CLIENT, Some(NOW + 1_000_000)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    let s = status_json(&env, Some(CLIENT), NOW);
    assert_eq!(s["state"], "ready");
    assert_eq!(s["screenId"], SCREEN_ID);
    assert_eq!(s["name"], NAME);
    assert_eq!(env.auth.user_id(), Some(USER_ID.to_string()));
    assert!(env.auth.bearer(Some(CLIENT), NOW).is_some());
}

#[tokio::test]
async fn load_with_corrupt_record_is_logged_out() {
    let env = env_plain();
    env.store.put(TOKEN_KEY, "{not json").await.unwrap();
    env.auth.load(Some(CLIENT), NOW).await;
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut");
    assert!(env.auth.bearer(Some(CLIENT), NOW).is_none());
}

#[tokio::test]
async fn expired_token_is_relogin_required_and_bearer_is_none() {
    let expires = NOW + 1000;
    let env = seeded(CLIENT, Some(expires)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    assert_eq!(status_json(&env, Some(CLIENT), expires - 1)["state"], "ready");
    assert!(env.auth.bearer(Some(CLIENT), expires - 1).is_some());
    // 設計書 §1.3: `now_ms >= expiresAtMs` で期限切れ。ちょうど同時刻も切れている。
    assert_eq!(status_json(&env, Some(CLIENT), expires)["state"], "reloginRequired");
    assert!(env.auth.bearer(Some(CLIENT), expires).is_none());
    assert_eq!(status_json(&env, Some(CLIENT), expires + 1)["state"], "reloginRequired");
    assert!(env.auth.bearer(Some(CLIENT), expires + 1).is_none());
}

#[tokio::test]
async fn token_issued_for_other_client_id_is_relogin_required() {
    let env = seeded("old-client", Some(NOW + 1_000_000)).await;
    env.auth.load(Some("new-client"), NOW).await;
    assert_eq!(status_json(&env, Some("new-client"), NOW)["state"], "reloginRequired");
    assert!(env.auth.bearer(Some("new-client"), NOW).is_none());
}

#[tokio::test]
async fn report_unauthorized_moves_to_relogin_required_keeps_store_and_notifies() {
    let env = seeded(CLIENT, Some(NOW + 1_000_000)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    let mut rx = env.auth.subscribe();
    assert_eq!(*rx.borrow_and_update(), AuthState::Ready);
    env.auth.report_unauthorized();
    assert!(rx.has_changed().unwrap(), "購読者に変化が届く");
    assert_eq!(*rx.borrow_and_update(), AuthState::ReloginRequired);
    assert!(env.store.raw(TOKEN_KEY).is_some(), "エントリは消さない");
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "reloginRequired");
    assert!(env.auth.bearer(Some(CLIENT), NOW).is_none());
}

#[tokio::test]
async fn disconnect_deletes_store_and_notifies_subscribers() {
    let env = seeded(CLIENT, Some(NOW + 1_000_000)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    let mut rx = env.auth.subscribe();
    assert_eq!(*rx.borrow_and_update(), AuthState::Ready);
    ok_of(env.auth.disconnect().await);
    assert!(env.store.raw(TOKEN_KEY).is_none());
    assert!(rx.has_changed().unwrap());
    assert_eq!(*rx.borrow_and_update(), AuthState::LoggedOut);
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut");
}

#[tokio::test]
async fn disconnect_delete_failure_reports_token_store_but_clears_memory() {
    let env = seeded(CLIENT, Some(NOW + 1_000_000)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    env.store.fail_next_delete();
    let e = err_of(env.auth.disconnect().await);
    assert_eq!(code_str(&e.code), "tokenStore");
    assert_eq!(status_json(&env, Some(CLIENT), NOW)["state"], "loggedOut");
    assert!(env.auth.bearer(Some(CLIENT), NOW).is_none());
    assert_eq!(env.auth.user_id(), None);
}

#[tokio::test]
async fn status_json_has_expected_keys_and_never_contains_token() {
    let env = seeded(CLIENT, Some(NOW + 1_000_000)).await;
    env.auth.load(Some(CLIENT), NOW).await;
    let s = status_json(&env, Some(CLIENT), NOW);
    let keys: HashSet<&str> = s.as_object().unwrap().keys().map(|k| k.as_str()).collect();
    assert_eq!(
        keys,
        HashSet::from(["configured", "state", "screenId", "name", "expiresAtMs", "callbackUrl", "customClientId"])
    );
    assert!(!s.to_string().contains(TOKEN));
}

#[tokio::test]
async fn debug_and_error_text_never_contain_token_or_state() {
    let mut texts: Vec<String> = Vec::new();
    let mut states: Vec<String> = Vec::new();
    fn add_err(texts: &mut Vec<String>, e: &TwitcastingAuthError) {
        texts.push(format!("{e:?}"));
        texts.push(format!("{e}"));
        texts.push(format!("{:?}", e.code));
        texts.push(serde_json::to_string(e).unwrap());
    }

    // クライアント不一致(トークンを受け取った後の失敗)
    let f = fake("someone-elses-app", 200).await;
    let env = env_with(&f);
    let (r, s) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    states.push(s);
    add_err(&mut texts, &err_of(r));
    // 保存失敗
    let f_ok = fake(CLIENT, 200).await;
    let env = env_with(&f_ok);
    env.store.fail_next_put();
    let (r, s) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    states.push(s);
    add_err(&mut texts, &err_of(r));
    // 通信失敗
    let env = env_plain();
    let (r, s) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    states.push(s);
    add_err(&mut texts, &err_of(r));
    // 拒否
    let env = env_with(&f_ok);
    let (r, s) = run_login(&env, CLIENT, NOW, |_| "result=denied".to_string()).await;
    states.push(s);
    add_err(&mut texts, &err_of(r));
    // 未設定・キャンセル・タイムアウト
    add_err(&mut texts, &err_of(env.auth.connect("", NOW, |_| Ok(())).await));
    let login = begin(&env, CLIENT, NOW).await;
    states.push(login.state.clone());
    env.auth.cancel_login();
    add_err(&mut texts, &err_of(wait(login.handle).await));
    let short = build(MemorySecretStore::new(), f_ok.base.clone(), Duration::from_millis(200));
    let login = begin(&short, CLIENT, NOW).await;
    states.push(login.state.clone());
    add_err(&mut texts, &err_of(wait(login.handle).await));
    // 成功したあとの `TwitcastingAuth` 自身
    let env = env_with(&f_ok);
    let (r, s) = run_login(&env, CLIENT, NOW, |s| success_body(s)).await;
    states.push(s);
    let status = ok_of(r);
    texts.push(format!("{:?}", env.auth));
    texts.push(sj(&status).to_string());

    assert!(!states.is_empty() && states.iter().all(|s| s.len() >= 16), "state は十分長い乱数");
    for t in &texts {
        assert!(!t.contains(TOKEN), "トークンが出ている: {t}");
        for s in &states {
            assert!(!t.contains(s.as_str()), "state が出ている: {t}");
        }
    }
}

#[test]
fn status_reports_callback_url_and_custom_client_flag() {
    let auth = TwitcastingAuth::new(MemorySecretStore::new(), TwitcastingEndpoints::default(), AuthOptions::default());
    let own = sj(&auth.status(Some("my-own-client"), NOW));
    assert_eq!(own["callbackUrl"], "http://localhost:47822/twitcasting/callback");
    assert_eq!(own["callbackUrl"], format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}"));
    assert_eq!(own["customClientId"], true);
    assert_eq!(own["configured"], true);
    // 設計書 §1.3: 上書き無し(`None`)なら自分の ClientID ではない。
    assert_eq!(sj(&auth.status(None, NOW))["customClientId"], false);
}

// ================================================================ 設定(config の公開面: fast_comment_lib::config)

#[test]
fn config_roundtrips_twitcasting_client_id_and_omits_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = AppConfig::default();
    cfg.credentials.twitcasting_client_id = Some("abc".to_string());
    cfg.save(dir.path()).unwrap();
    let text = std::fs::read_to_string(AppConfig::path_in(dir.path())).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["credentials"]["twitcastingClientId"], "abc");
    assert_eq!(AppConfig::load(dir.path()).unwrap().credentials.twitcasting_client_id, Some("abc".to_string()));

    let dir = tempfile::tempdir().unwrap();
    AppConfig::default().save(dir.path()).unwrap();
    let text = std::fs::read_to_string(AppConfig::path_in(dir.path())).unwrap();
    let v: Value = serde_json::from_str(&text).unwrap();
    assert!(v["credentials"].get("twitcastingClientId").is_none(), "None は出力しない");
}

#[test]
fn config_without_twitcasting_fields_loads() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(AppConfig::path_in(dir.path()), r#"{"credentials":{"twitchUsername":"someone"}}"#).unwrap();
    let cfg = AppConfig::load(dir.path()).unwrap();
    assert_eq!(cfg.credentials.twitcasting_client_id, None);
    assert_eq!(cfg.credentials.twitch_username, "someone");
}
