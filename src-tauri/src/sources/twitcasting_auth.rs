//! Twitcasting Implicit OAuth. Secrets never cross the status / error boundary.
use crate::integrations::SecretStore;
use axum::{
    body::to_bytes,
    extract::{Request, State},
    http::{Method, StatusCode},
    response::{Html, IntoResponse, Response},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinSet,
};
use tokio_util::sync::CancellationToken;

pub const TOKEN_KEY: &str = "twitcasting-oauth-token";
pub const CALLBACK_PORT: u16 = 47822;
pub const CALLBACK_PATH: &str = "/twitcasting/callback";
pub const TOKEN_PATH: &str = "/twitcasting/token";
pub const DEFAULT_CLIENT_ID: &str = "";
pub fn effective_client_id(override_id: Option<&str>) -> Option<String> {
    override_id
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or_else(|| (!DEFAULT_CLIENT_ID.is_empty()).then_some(DEFAULT_CLIENT_ID))
        .map(str::to_owned)
}
#[derive(Clone)]
pub struct TwitcastingEndpoints {
    pub authorize: String,
    pub api_base: String,
}
impl Default for TwitcastingEndpoints {
    fn default() -> Self {
        Self {
            authorize: "https://apiv2.twitcasting.tv/oauth2/authorize".into(),
            api_base: "https://apiv2.twitcasting.tv".into(),
        }
    }
}
pub fn build_authorize_url(
    endpoints: &TwitcastingEndpoints,
    client_id: &str,
    state: &str,
) -> String {
    let query = url::form_urlencoded::Serializer::new(String::new())
        .append_pair("client_id", client_id)
        .append_pair("response_type", "token")
        .append_pair("state", state)
        .finish();
    format!("{}?{}", endpoints.authorize, query)
}
#[derive(Clone)]
pub struct AuthOptions {
    pub port: u16,
    pub login_timeout: Duration,
    pub read_timeout: Duration,
}
impl Default for AuthOptions {
    fn default() -> Self {
        Self {
            port: CALLBACK_PORT,
            login_timeout: Duration::from_secs(180),
            read_timeout: Duration::from_secs(5),
        }
    }
}
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AuthState {
    LoggedOut,
    Ready,
    ReloginRequired,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TwitcastingStatus {
    pub configured: bool,
    pub state: String,
    pub screen_id: Option<String>,
    pub name: Option<String>,
    pub expires_at_ms: Option<u64>,
    pub callback_url: String,
    pub custom_client_id: bool,
}
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum TwitcastingAuthErrorCode {
    NotConfigured,
    PortInUse,
    LoginTimeout,
    Cancelled,
    Denied,
    ClientMismatch,
    TokenStore,
    Network,
    Other,
}
#[derive(Debug, Serialize)]
pub struct TwitcastingAuthError {
    pub code: TwitcastingAuthErrorCode,
    pub message: String,
}
impl TwitcastingAuthError {
    pub fn new(code: TwitcastingAuthErrorCode) -> Self {
        let message = match code {
            TwitcastingAuthErrorCode::NotConfigured => "ツイキャスの ClientID を設定してください",
            TwitcastingAuthErrorCode::PortInUse => {
                "認証ポートを使用中です。他のログイン待受を閉じてください"
            }
            TwitcastingAuthErrorCode::LoginTimeout => "ログインが時間切れになりました",
            TwitcastingAuthErrorCode::Cancelled => "ログインを取り消しました",
            TwitcastingAuthErrorCode::Denied => "許可されませんでした",
            TwitcastingAuthErrorCode::ClientMismatch => "認証先の ClientID が一致しません",
            TwitcastingAuthErrorCode::TokenStore => "ツイキャスの認証情報を保存・削除できません",
            TwitcastingAuthErrorCode::Network => "ツイキャスに接続できません",
            TwitcastingAuthErrorCode::Other => "ツイキャスの認証に失敗しました",
        };
        Self {
            code,
            message: message.into(),
        }
    }
}
impl fmt::Display for TwitcastingAuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for TwitcastingAuthError {}
use TwitcastingAuthErrorCode as Code;
fn error(code: Code) -> TwitcastingAuthError {
    TwitcastingAuthError::new(code)
}
// Deliberately no Debug: this is the only persistent secret record.
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Record {
    client_id: String,
    access_token: String,
    user_id: String,
    screen_id: String,
    name: String,
    expires_at_ms: Option<u64>,
    obtained_at_ms: u64,
}
struct Memory {
    record: Option<Record>,
    unauthorized: bool,
}
pub struct TwitcastingAuth<S: SecretStore> {
    store: S,
    endpoints: TwitcastingEndpoints,
    options: AuthOptions,
    memory: Mutex<Memory>,
    changes: watch::Sender<AuthState>,
    operation: AsyncMutex<()>,
    login: Mutex<Option<CancellationToken>>,
}
impl<S: SecretStore> fmt::Debug for TwitcastingAuth<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingAuth").finish_non_exhaustive()
    }
}
impl<S: SecretStore> TwitcastingAuth<S> {
    pub fn new(store: S, endpoints: TwitcastingEndpoints, options: AuthOptions) -> Arc<Self> {
        let (changes, _) = watch::channel(AuthState::LoggedOut);
        Arc::new(Self {
            store,
            endpoints,
            options,
            memory: Mutex::new(Memory {
                record: None,
                unauthorized: false,
            }),
            changes,
            operation: AsyncMutex::new(()),
            login: Mutex::new(None),
        })
    }
    pub async fn load(&self, client_id: Option<&str>, now_ms: u64) {
        let _guard = self.operation.lock().await;
        let record = self
            .store
            .get(TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Record>(&s).ok())
            .filter(|r| valid_token(&r.access_token));
        *self.memory.lock().unwrap() = Memory {
            record,
            unauthorized: false,
        };
        self.status(client_id, now_ms);
    }
    fn auth_state(&self, memory: &Memory, client_id: Option<&str>, now_ms: u64) -> AuthState {
        let state = match &memory.record {
            None => AuthState::LoggedOut,
            Some(r)
                if memory.unauthorized
                    || client_id != Some(r.client_id.as_str())
                    || r.expires_at_ms.is_some_and(|e| now_ms >= e) =>
            {
                AuthState::ReloginRequired
            }
            Some(_) => AuthState::Ready,
        };
        self.changes.send_if_modified(|old| {
            if *old == state {
                false
            } else {
                *old = state;
                true
            }
        });
        state
    }
    pub fn status(&self, client_id: Option<&str>, now_ms: u64) -> TwitcastingStatus {
        let memory = self.memory.lock().unwrap();
        let state = self.auth_state(&memory, client_id, now_ms);
        let r = memory.record.as_ref();
        TwitcastingStatus {
            configured: client_id.is_some_and(|c| !c.trim().is_empty()),
            state: match state {
                AuthState::LoggedOut => "loggedOut",
                AuthState::Ready => "ready",
                AuthState::ReloginRequired => "reloginRequired",
            }
            .into(),
            screen_id: r.map(|r| r.screen_id.clone()),
            name: r.map(|r| r.name.clone()),
            expires_at_ms: r.and_then(|r| r.expires_at_ms),
            callback_url: format!("http://localhost:{}{CALLBACK_PATH}", self.options.port),
            custom_client_id: client_id
                .is_some_and(|c| !c.trim().is_empty() && c != DEFAULT_CLIENT_ID),
        }
    }
    pub fn bearer(&self, client_id: Option<&str>, now_ms: u64) -> Option<String> {
        let memory = self.memory.lock().unwrap();
        (self.auth_state(&memory, client_id, now_ms) == AuthState::Ready)
            .then(|| memory.record.as_ref().unwrap().access_token.clone())
    }
    pub fn user_id(&self) -> Option<String> {
        self.memory
            .lock()
            .unwrap()
            .record
            .as_ref()
            .map(|r| r.user_id.clone())
    }
    pub fn subscribe(&self) -> watch::Receiver<AuthState> {
        self.changes.subscribe()
    }
    pub fn report_unauthorized(&self) {
        let mut memory = self.memory.lock().unwrap();
        if memory.record.is_some() {
            memory.unauthorized = true;
            self.changes.send_replace(AuthState::ReloginRequired);
        }
    }
    pub fn cancel_login(&self) {
        if let Some(cancel) = self.login.lock().unwrap().as_ref() {
            cancel.cancel();
        }
    }
    pub async fn disconnect(&self) -> Result<(), TwitcastingAuthError> {
        self.cancel_login();
        let _guard = self.operation.lock().await;
        *self.memory.lock().unwrap() = Memory {
            record: None,
            unauthorized: false,
        };
        self.changes.send_replace(AuthState::LoggedOut);
        self.store
            .delete(TOKEN_KEY)
            .await
            .map_err(|_| error(Code::TokenStore))
    }
    pub async fn connect(
        &self,
        client_id: &str,
        now_ms: u64,
        open_browser: impl FnOnce(String) -> Result<(), String>,
    ) -> Result<TwitcastingStatus, TwitcastingAuthError> {
        if client_id.trim().is_empty() {
            return Err(error(Code::NotConfigured));
        }
        let cancel = CancellationToken::new();
        if let Some(old) = self.login.lock().unwrap().replace(cancel.clone()) {
            old.cancel();
        }
        let _guard = self.operation.lock().await;
        if cancel.is_cancelled() {
            return Err(error(Code::Cancelled));
        }
        let mut servers = JoinSet::new();
        let shutdown = CancellationToken::new();
        let _shutdown_guard = shutdown.clone().drop_guard();
        let flow = async {
            let ipv4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, self.options.port))
                .await
                .map_err(|_| error(Code::PortInUse))?;
            let ipv6 =
                match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, self.options.port)).await {
                    Ok(l) => Some(l),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::AddrNotAvailable
                            || matches!(e.raw_os_error(), Some(10047 | 97)) =>
                    {
                        None
                    }
                    Err(_) => return Err(error(Code::PortInUse)),
                };
            let mut random = [0u8; 32];
            getrandom::getrandom(&mut random).map_err(|_| error(Code::Other))?;
            let state = URL_SAFE_NO_PAD.encode(random);
            let (tx, mut rx) = mpsc::channel(1);
            let context = Arc::new(Loopback {
                state: state.clone(),
                port: self.options.port,
                read_timeout: self.options.read_timeout,
                tx,
                accepted: Mutex::new(false),
                shutdown: shutdown.clone(),
                cancel: cancel.clone(),
            });
            let app = Router::new().fallback(callback).with_state(context);
            for listener in std::iter::once(ipv4).chain(ipv6) {
                let stop = shutdown.clone();
                let app = app.clone();
                servers.spawn(async move {
                    let _ = axum::serve(listener, app)
                        .with_graceful_shutdown(stop.cancelled_owned())
                        .await;
                });
            }
            open_browser(build_authorize_url(&self.endpoints, client_id, &state))
                .map_err(|_| error(Code::Other))?;
            let grant = rx.recv().await.ok_or_else(|| error(Code::Other))??;
            // Stop accepting callbacks before verification; graceful shutdown flushes the response.
            shutdown.cancel();
            let response = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(self.options.read_timeout)
                .build()
                .map_err(|_| error(Code::Other))?
                .get(format!(
                    "{}/verify_credentials",
                    self.endpoints.api_base.trim_end_matches('/')
                ))
                .bearer_auth(&grant.token)
                .header("X-Api-Version", "2.0")
                .header("Accept", "application/json")
                .send()
                .await
                .map_err(|_| error(Code::Network))?;
            if !response.status().is_success() {
                return Err(error(Code::Other));
            }
            let v: serde_json::Value = response.json().await.map_err(|_| error(Code::Other))?;
            if v.pointer("/app/client_id").and_then(|v| v.as_str()) != Some(client_id) {
                return Err(error(Code::ClientMismatch));
            }
            let field = |path| {
                v.pointer(path)
                    .and_then(|v| v.as_str())
                    .map(str::to_owned)
                    .ok_or_else(|| error(Code::Other))
            };
            let record = Record {
                client_id: client_id.into(),
                access_token: grant.token,
                user_id: field("/user/id")?,
                screen_id: field("/user/screen_id")?,
                name: field("/user/name")?,
                expires_at_ms: grant
                    .expires
                    .and_then(|s| s.checked_mul(1000))
                    .and_then(|ms| now_ms.checked_add(ms)),
                obtained_at_ms: now_ms,
            };
            Ok(record)
        };
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error(Code::Cancelled)),
            _ = tokio::time::sleep(self.options.login_timeout) => Err(error(Code::LoginTimeout)),
            result = flow => result,
        };
        shutdown.cancel();
        // Do not let a partial HTTP request keep a callback port alive indefinitely.
        if tokio::time::timeout(Duration::from_millis(250), async {
            while servers.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            servers.abort_all();
            while servers.join_next().await.is_some() {}
        }
        let record = result?;
        if cancel.is_cancelled() {
            return Err(error(Code::Cancelled));
        }
        // Commit is serialized with disconnect and subsequent logins. Do not drop a
        // credential-store future: the OS write may continue after cancellation.
        let serialized = serde_json::to_string(&record).map_err(|_| error(Code::Other))?;
        self.store
            .put(TOKEN_KEY, &serialized)
            .await
            .map_err(|_| error(Code::TokenStore))?;
        *self.memory.lock().unwrap() = Memory {
            record: Some(record),
            unauthorized: false,
        };
        Ok(self.status(Some(client_id), now_ms))
    }
}
fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 2048 && token.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
struct Grant {
    token: String,
    expires: Option<u64>,
}
struct Loopback {
    state: String,
    port: u16,
    read_timeout: Duration,
    tx: mpsc::Sender<Result<Grant, TwitcastingAuthError>>,
    accepted: Mutex<bool>,
    shutdown: CancellationToken,
    cancel: CancellationToken,
}
const PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p id="message">認証しています…</p><script>
const hash = location.hash;
history.replaceState(null, '', location.pathname);
const message = document.getElementById('message');
if (!hash || hash.length < 2) { message.textContent = '認証情報が見つかりません'; }
else { fetch('/twitcasting/token', {method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body:hash.slice(1)}).then(r => { message.textContent = r.ok ? 'アプリに戻ってください。この画面は閉じられます。' : '認証に失敗しました。アプリでやり直してください。'; }).catch(() => { message.textContent = 'アプリでログインをやり直してください。'; }); }
</script></html>"#;
async fn callback(State(context): State<Arc<Loopback>>, req: Request) -> Response {
    let mut response = handle_callback(&context, req).await;
    let headers = response.headers_mut();
    for (k,v) in [("cache-control","no-store"), ("referrer-policy","no-referrer"), ("x-content-type-options","nosniff"), ("content-security-policy","default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'")] { headers.insert(k, v.parse().unwrap()); }
    response
}
async fn handle_callback(context: &Loopback, req: Request) -> Response {
    let allowed: Vec<String> = ["localhost", "127.0.0.1", "[::1]"]
        .iter()
        .map(|host| format!("{host}:{}", context.port))
        .collect();
    if !req
        .headers()
        .get("host")
        .and_then(|h| h.to_str().ok())
        .is_some_and(|h| allowed.iter().any(|a| a == h))
    {
        return StatusCode::FORBIDDEN.into_response();
    }
    if req.headers().get("origin").is_some_and(|h| {
        !h.to_str()
            .ok()
            .is_some_and(|h| allowed.iter().any(|a| h == format!("http://{a}")))
    }) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if req.method() == Method::GET && req.uri().path() == CALLBACK_PATH {
        return Html(PAGE).into_response();
    }
    if req.method() != Method::POST || req.uri().path() != TOKEN_PATH {
        return StatusCode::NOT_FOUND.into_response();
    }
    let body =
        match tokio::time::timeout(context.read_timeout, to_bytes(req.into_body(), 4096)).await {
            Ok(Ok(body)) => body,
            _ => return StatusCode::BAD_REQUEST.into_response(),
        };
    let params: HashMap<String, String> = url::form_urlencoded::parse(&body).into_owned().collect();
    let grant = if params.get("result").is_some_and(|s| s == "denied") {
        Err(error(Code::Denied))
    } else {
        let state = params
            .get("state")
            .map(String::as_bytes)
            .unwrap_or_default();
        if !bool::from(context.state.as_bytes().ct_eq(state)) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Some(token) = params.get("access_token").filter(|t| valid_token(t)) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        Ok(Grant {
            token: token.clone(),
            expires: params.get("expires_in").and_then(|s| s.parse().ok()),
        })
    };
    let mut accepted = context.accepted.lock().unwrap();
    if *accepted || context.shutdown.is_cancelled() || context.cancel.is_cancelled() {
        return StatusCode::CONFLICT.into_response();
    }
    *accepted = true;
    let _ = context.tx.try_send(grant);
    StatusCode::OK.into_response()
}
