//! Twitcasting Implicit OAuth. Secrets never cross the status / error boundary.
use crate::integrations::SecretStore;
use axum::{
    body::{to_bytes, Body},
    extract::Request,
    http::{Method, StatusCode},
    response::{Html, IntoResponse, Response},
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use hyper::{server::conn::http1, service::service_fn};
use hyper_util::rt::TokioIo;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    convert::Infallible,
    fmt,
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{mpsc, watch, Mutex as AsyncMutex},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::{CancellationToken, DropGuard};

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
    generation: u64,
    unauthorized: bool,
    expired: bool,
    expiry_task: Option<DropGuard>,
}
pub struct TwitcastingAuth<S: SecretStore> {
    store: S,
    endpoints: TwitcastingEndpoints,
    options: AuthOptions,
    memory: Arc<Mutex<Memory>>,
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
            memory: Arc::new(Mutex::new(Memory {
                record: None,
                generation: 0,
                unauthorized: false,
                expired: false,
                expiry_task: None,
            })),
            changes,
            operation: AsyncMutex::new(()),
            login: Mutex::new(None),
        })
    }
    pub async fn load(&self, client_id: Option<&str>, now_ms: u64) {
        let observed_at = Instant::now();
        let _guard = self.operation.lock().await;
        let record = self
            .store
            .get(TOKEN_KEY)
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<Record>(&s).ok())
            .filter(|r| valid_token(&r.access_token));
        self.replace_record(record, client_id, now_ms, observed_at);
    }
    fn replace_record(
        &self,
        record: Option<Record>,
        client_id: Option<&str>,
        now_ms: u64,
        observed_at: Instant,
    ) {
        let deadline = record
            .as_ref()
            .and_then(|r| r.expires_at_ms)
            .and_then(|expiry| {
                observed_at.checked_add(Duration::from_millis(expiry.saturating_sub(now_ms)))
            });
        let mut memory = self.memory.lock().unwrap();
        memory.generation = memory.generation.wrapping_add(1);
        memory.record = record;
        memory.unauthorized = false;
        memory.expired = deadline.is_some_and(|d| d <= Instant::now());
        memory.expiry_task = None;
        self.auth_state(&memory, client_id, now_ms);
        if let Some(deadline) = deadline.filter(|_| !memory.expired) {
            let generation = memory.generation;
            let current = Arc::downgrade(&self.memory);
            let changes = self.changes.clone();
            let stop = CancellationToken::new();
            memory.expiry_task = Some(stop.clone().drop_guard());
            tokio::spawn(async move {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep_until(deadline) => {}
                }
                if let Some(current) = current.upgrade() {
                    let mut memory = current.lock().unwrap();
                    if memory.generation == generation {
                        memory.expired = true;
                        changes.send_if_modified(|state| {
                            if *state == AuthState::Ready {
                                *state = AuthState::ReloginRequired;
                                true
                            } else {
                                false
                            }
                        });
                    }
                }
            });
        }
    }
    fn auth_state(&self, memory: &Memory, client_id: Option<&str>, now_ms: u64) -> AuthState {
        let state = match &memory.record {
            None => AuthState::LoggedOut,
            Some(r)
                if memory.unauthorized
                    || memory.expired
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
        self.bearer_with_generation(client_id, now_ms)
            .map(|(token, _)| token)
    }
    /// Retain the generation alongside the request and pass it to `report_unauthorized_for` on a 401.
    pub fn bearer_with_generation(
        &self,
        client_id: Option<&str>,
        now_ms: u64,
    ) -> Option<(String, u64)> {
        let memory = self.memory.lock().unwrap();
        (self.auth_state(&memory, client_id, now_ms) == AuthState::Ready).then(|| {
            let record = memory.record.as_ref().unwrap();
            (record.access_token.clone(), memory.generation)
        })
    }
    /// Returns the verified ID while auth is Ready. Check `status` or `bearer`
    /// with the current ClientID before using this ID for a new request.
    pub fn user_id(&self) -> Option<String> {
        let memory = self.memory.lock().unwrap();
        if *self.changes.borrow() != AuthState::Ready {
            return None;
        }
        memory.record.as_ref().map(|r| r.user_id.clone())
    }
    pub fn subscribe(&self) -> watch::Receiver<AuthState> {
        self.changes.subscribe()
    }
    /// Invalidates the current login. Use the generation-aware form for an in-flight request.
    pub fn report_unauthorized(&self) {
        self.invalidate(None);
    }
    /// Only invalidates the login used by the request that returned 401.
    pub fn report_unauthorized_for(&self, generation: u64) {
        self.invalidate(Some(generation));
    }
    fn invalidate(&self, generation: Option<u64>) {
        let mut memory = self.memory.lock().unwrap();
        if memory.record.is_some()
            && !memory.unauthorized
            && generation.is_none_or(|g| g == memory.generation)
        {
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
        self.replace_record(None, None, 0, Instant::now());
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
        let observed_at = Instant::now();
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
        let mut connections = JoinSet::new();
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
                cancel: cancel.clone(),
            });
            open_browser(build_authorize_url(&self.endpoints, client_id, &state))
                .map_err(|_| error(Code::Other))?;
            let grant = loop {
                tokio::select! {
                    grant = rx.recv() => break grant.ok_or_else(|| error(Code::Other))?,
                    stream = ipv4.accept() => spawn_callback(&mut connections, stream, &context)?,
                    stream = async {
                        match &ipv6 {
                            Some(listener) => listener.accept().await,
                            None => std::future::pending().await,
                        }
                    } => spawn_callback(&mut connections, stream, &context)?,
                    _ = connections.join_next(), if !connections.is_empty() => {}
                }
            };
            drop((ipv4, ipv6));
            let grant = grant?;
            if grant.expires == Some(0) {
                return Err(error(Code::Other));
            }
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
        if !matches!(&result, Err(e) if matches!(e.code, Code::Cancelled | Code::LoginTimeout)) {
            // Let the browser receive its response before dropping other in-flight callbacks.
            let flush = tokio::time::timeout(Duration::from_millis(250), async {
                while connections.join_next().await.is_some() {}
            });
            tokio::select! {
                _ = cancel.cancelled() => {}
                _ = flush => {}
            }
        }
        connections.shutdown().await;
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
        self.replace_record(Some(record), Some(client_id), now_ms, observed_at);
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
    cancel: CancellationToken,
}

fn spawn_callback(
    connections: &mut JoinSet<()>,
    accepted: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    context: &Arc<Loopback>,
) -> Result<(), TwitcastingAuthError> {
    let (stream, _) = accepted.map_err(|_| error(Code::Other))?;
    let context = context.clone();
    connections.spawn(async move {
        let service = service_fn(|request: hyper::Request<hyper::body::Incoming>| async {
            Ok::<_, Infallible>(callback(&context, request.map(Body::new)).await)
        });
        let _ = http1::Builder::new()
            .keep_alive(false)
            .serve_connection(TokioIo::new(stream), service)
            .await;
    });
    Ok(())
}
const PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p id="message">認証しています…</p><script>
const body = location.hash.slice(1) ||
  (new URLSearchParams(location.search).get('result') === 'denied' ? 'result=denied' : '');
history.replaceState(null, '', location.pathname);
const message = document.getElementById('message');
if (!body) { message.textContent = '認証情報が見つかりません'; }
else { fetch('/twitcasting/token', {method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body}).then(r => { message.textContent = r.ok ? 'アプリに戻ってください。この画面は閉じられます。' : '認証に失敗しました。アプリでやり直してください。'; }).catch(() => { message.textContent = 'アプリでログインをやり直してください。'; }); }
</script></html>"#;
async fn callback(context: &Loopback, req: Request) -> Response {
    let mut response = handle_callback(context, req).await;
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
    if *accepted || context.cancel.is_cancelled() {
        return StatusCode::CONFLICT.into_response();
    }
    *accepted = true;
    let _ = context.tx.try_send(grant);
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::MemorySecretStore;
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpStream,
        sync::oneshot,
        time::timeout,
    };

    async fn seed_record(store: &MemorySecretStore, expiry: Option<u64>) {
        let record = Record {
            client_id: "client".into(),
            access_token: "test-token".into(),
            user_id: "user".into(),
            screen_id: "screen".into(),
            name: "name".into(),
            expires_at_ms: expiry,
            obtained_at_ms: 0,
        };
        store
            .put(TOKEN_KEY, &serde_json::to_string(&record).unwrap())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn stale_unauthorized_response_does_not_invalidate_new_session() {
        let store = MemorySecretStore::new();
        seed_record(&store, None).await;
        let auth = TwitcastingAuth::new(
            store,
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        auth.load(Some("client"), 0).await;
        let (_, old_generation) = auth.bearer_with_generation(Some("client"), 0).unwrap();
        auth.load(Some("client"), 0).await;
        let (_, generation) = auth.bearer_with_generation(Some("client"), 0).unwrap();
        let mut changes = auth.subscribe();
        assert_eq!(*changes.borrow_and_update(), AuthState::Ready);

        auth.report_unauthorized_for(old_generation);
        assert!(!changes.has_changed().unwrap());
        assert_eq!(auth.user_id().as_deref(), Some("user"));

        auth.report_unauthorized_for(generation);
        assert_eq!(*changes.borrow_and_update(), AuthState::ReloginRequired);
        assert!(auth.user_id().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn expiry_notifies_idle_subscribers_for_the_current_record_only() {
        let store = MemorySecretStore::new();
        seed_record(&store, Some(1000)).await;
        let auth = TwitcastingAuth::new(
            store.clone(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        auth.load(Some("client"), 0).await;
        let mut changes = auth.subscribe();
        assert_eq!(*changes.borrow_and_update(), AuthState::Ready);
        seed_record(&store, Some(3000)).await;
        auth.load(Some("client"), 0).await;

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!changes.has_changed().unwrap());
        timeout(Duration::from_secs(2), changes.changed())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(*changes.borrow_and_update(), AuthState::ReloginRequired);
        assert!(auth.user_id().is_none());
        assert!(auth.bearer(Some("client"), 0).is_none());
        assert!(store.raw(TOKEN_KEY).is_some());
    }

    #[tokio::test]
    async fn cancel_closes_inflight_callback_and_releases_port() {
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let auth = TwitcastingAuth::new(
            MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions {
                port: addr.port(),
                read_timeout: Duration::from_secs(15),
                ..AuthOptions::default()
            },
        );
        let (started, listening) = oneshot::channel();
        let login = tokio::spawn({
            let auth = auth.clone();
            async move {
                auth.connect("client", 0, |_| {
                    started.send(()).unwrap();
                    Ok(())
                })
                .await
            }
        });
        let wait = Duration::from_secs(2);
        timeout(wait, listening).await.unwrap().unwrap();

        let mut client = BufReader::new(TcpStream::connect(addr).await.unwrap());
        client
            .write_all(
                format!(
                    "POST {TOKEN_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: 100\r\nExpect: 100-continue\r\n\r\n"
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let mut response = String::new();
        timeout(wait, client.read_line(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, "HTTP/1.1 100 Continue\r\n");
        response.clear();
        timeout(wait, client.read_line(&mut response))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(response, "\r\n");

        auth.cancel_login();
        let result = timeout(wait, login).await.unwrap().unwrap();
        assert!(matches!(result.unwrap_err().code, Code::Cancelled));
        let rebound = TcpListener::bind(addr).await;
        let closed = timeout(wait, client.read_u8())
            .await
            .expect("callback connection stayed open after cancellation");
        assert!(matches!(
            closed.unwrap_err().kind(),
            std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset
        ));
        rebound.expect("callback port could not be rebound after cancellation");
    }
}
