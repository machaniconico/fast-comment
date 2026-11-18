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
    future::Future,
    io,
    sync::{Arc, Mutex},
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::{
    net::TcpListener,
    sync::{mpsc, oneshot, watch, Mutex as AsyncMutex},
    task::JoinSet,
    time::Instant,
};
use tokio_util::sync::{CancellationToken, DropGuard};

pub const TOKEN_KEY: &str = "twitcasting-oauth-token";
pub const CALLBACK_PORT: u16 = 47822;
pub const CALLBACK_PATH: &str = "/twitcasting/callback";
pub const TOKEN_PATH: &str = "/twitcasting/token";
pub const DEFAULT_CLIENT_ID: &str = "";
const MAX_EXPIRY_SECONDS: u64 = 10 * 365 * 24 * 60 * 60;
const ROLLBACK_ATTEMPTS: u32 = 3;
const ROLLBACK_RETRY: Duration = Duration::from_millis(500);
// Largest instant JavaScript `Date` can represent.
const MAX_JS_DATE_MS: u64 = 8_640_000_000_000_000;
const WSAECONNRESET: i32 = 10054;

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
    operation: Arc<AsyncMutex<()>>,
    login: Mutex<Option<CancellationToken>>,
    // Tail of the serial store queue. A started OS write cannot be aborted, so every
    // operation on the token key runs after the previous one finishes, even when its
    // caller has already stopped waiting.
    store_queue: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
            operation: Arc::new(AsyncMutex::new(())),
            login: Mutex::new(None),
            store_queue: Mutex::new(None),
        })
    }
    fn queued<T, F>(&self, op: impl FnOnce(S) -> F) -> oneshot::Receiver<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (tx, rx) = oneshot::channel();
        let op = op(self.store.clone());
        let mut tail = self.store_queue.lock().unwrap();
        let previous = tail.take();
        *tail = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            let _ = tx.send(op.await);
        }));
        rx
    }
    /// Queues `op` and waits up to `read_timeout`; the operation still runs after a timeout.
    async fn queued_within<T, F>(&self, op: impl FnOnce(S) -> F) -> Option<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        tokio::time::timeout(self.options.read_timeout, self.queued(op))
            .await
            .ok()
            .and_then(Result::ok)
    }
    pub async fn load(&self, client_id: Option<&str>, now_ms: u64) {
        let observed_at = Instant::now();
        let _guard = self.operation.lock().await;
        let record = self
            .queued_within(|s| async move { s.get(TOKEN_KEY).await })
            .await
            .and_then(Result::ok)
            .flatten()
            .and_then(|s| serde_json::from_str::<Record>(&s).ok())
            .filter(|r| valid_token(&r.access_token))
            // Out-of-range expiries are corrupt; the UI cannot render them as a date.
            // Compare stored values only, so a moved wall clock does not matter.
            .filter(|r| {
                r.expires_at_ms.is_none_or(|e| {
                    e <= MAX_JS_DATE_MS
                        && e.saturating_sub(r.obtained_at_ms) <= MAX_EXPIRY_SECONDS * 1000
                })
            });
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
        // On timeout the delete stays queued and runs after any write still in progress.
        match self
            .queued_within(|s| async move { s.delete(TOKEN_KEY).await })
            .await
        {
            Some(Ok(())) => Ok(()),
            _ => Err(error(Code::TokenStore)),
        }
    }
    pub async fn connect(
        self: &Arc<Self>,
        client_id: &str,
        now_ms: u64,
        open_browser: impl FnOnce(String) -> Result<(), String>,
    ) -> Result<TwitcastingStatus, TwitcastingAuthError> {
        let observed_at = Instant::now();
        let deadline = observed_at + self.options.login_timeout;
        if client_id.trim().is_empty() {
            return Err(error(Code::NotConfigured));
        }
        let cancel = CancellationToken::new();
        let _cancel_on_drop = cancel.clone().drop_guard();
        if let Some(old) = self.login.lock().unwrap().replace(cancel.clone()) {
            old.cancel();
        }
        let operation = tokio::select! {
            biased;
            stop = login_ended(&cancel, deadline) => return Err(stop),
            lock = self.operation.clone().lock_owned() => lock,
        };
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
            let received_at = grant.received_at;
            let obtained_at_ms = now_ms
                .checked_add(received_at.duration_since(observed_at).as_millis() as u64)
                .ok_or_else(|| error(Code::Other))?;
            let expires_at_ms = grant
                .expires
                .map(|duration| {
                    obtained_at_ms
                        .checked_add(duration.as_millis() as u64)
                        .filter(|expiry| *expiry > obtained_at_ms)
                        .ok_or_else(|| error(Code::Other))
                })
                .transpose()?;
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
                expires_at_ms,
                obtained_at_ms,
            };
            Ok((record, received_at))
        };
        let mut result = tokio::select! {
            biased;
            stop = login_ended(&cancel, deadline) => Err(stop),
            result = flow => result,
        };
        if !matches!(&result, Err(e) if matches!(e.code, Code::Cancelled | Code::LoginTimeout)) {
            // Let the browser receive its response before dropping other in-flight callbacks.
            let flush = tokio::time::timeout(Duration::from_millis(250), async {
                while connections.join_next().await.is_some() {}
            });
            tokio::select! {
                stop = login_ended(&cancel, deadline) => result = Err(stop),
                _ = flush => {}
            }
        }
        connections.shutdown().await;
        let (record, received_at) = result?;
        let token_deadline = record
            .expires_at_ms
            .map(|expiry| received_at + Duration::from_millis(expiry - record.obtained_at_ms));
        let serialized = serde_json::to_string(&record).map_err(|_| error(Code::Other))?;
        let previous = tokio::select! {
            biased;
            stop = login_ended(&cancel, deadline) => return Err(stop),
            // An unreadable old value must not block a fresh login.
            previous = self.queued_within(|s| async move { s.get(TOKEN_KEY).await }) => {
                previous.and_then(Result::ok).flatten()
            }
        };

        let (reply, result) = oneshot::channel();
        let auth = self.clone();
        tokio::spawn(async move {
            // Keep subsequent store operations behind the write and its rollback.
            let _operation = operation;
            let interrupted = || {
                if cancel.is_cancelled() {
                    Some(error(Code::Cancelled))
                } else if Instant::now() >= deadline {
                    Some(error(Code::LoginTimeout))
                } else if token_deadline.is_some_and(|expiry| Instant::now() >= expiry) {
                    Some(error(Code::Other))
                } else {
                    None
                }
            };
            if let Some(stop) = interrupted() {
                let _ = reply.send(Err(stop));
                return;
            }

            let value = serialized.clone();
            let mut write = auth.queued(|s| async move { s.put(TOKEN_KEY, &value).await });
            let saved = tokio::select! {
                biased;
                stop = login_ended(&cancel, deadline) => Err(stop),
                saved = &mut write => interrupted().map_or(Ok(saved), Err),
            };
            let saved = match saved {
                Ok(saved) => saved,
                Err(stop) => {
                    let _ = reply.send(Err(stop));
                    // The rollback is queued behind the write, so it sees the write's result.
                    if !auth.rollback(previous, serialized).await {
                        // Give up so the operation lock is released. The store may keep the
                        // cancelled token until a queued rollback or a later logout removes it.
                        auth.replace_record(None, None, 0, Instant::now());
                    }
                    return;
                }
            };
            let outcome = saved.map_err(|_| ()).and_then(|r| r.map_err(|_| ()));
            let outcome = outcome.map_err(|_| error(Code::TokenStore)).map(|_| {
                let client_id = record.client_id.clone();
                let obtained_at_ms = record.obtained_at_ms;
                auth.replace_record(Some(record), Some(&client_id), obtained_at_ms, received_at);
                auth.status(Some(&client_id), obtained_at_ms)
            });
            let _ = reply.send(outcome);
        });
        result.await.map_err(|_| error(Code::TokenStore))?
    }

    /// Undoes a cancelled write only if it landed; an unreadable old value is left alone
    /// when the write did not land, and deleted when it did (it cannot be restored).
    async fn rollback(&self, previous: Option<String>, written: String) -> bool {
        for attempt in 0..ROLLBACK_ATTEMPTS {
            if attempt > 0 {
                tokio::time::sleep(ROLLBACK_RETRY).await;
            }
            let (previous, written) = (previous.clone(), written.clone());
            let undone = self
                .queued_within(|s| async move {
                    if s.get(TOKEN_KEY).await?.as_deref() != Some(written.as_str()) {
                        return Ok(());
                    }
                    match previous {
                        Some(value) => s.put(TOKEN_KEY, &value).await,
                        None => s.delete(TOKEN_KEY).await,
                    }
                })
                .await;
            match undone {
                Some(Ok(())) => return true,
                Some(Err(_)) => {}
                None => return false,
            }
        }
        false
    }
}

async fn login_ended(cancel: &CancellationToken, deadline: Instant) -> TwitcastingAuthError {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => error(Code::Cancelled),
        _ = tokio::time::sleep_until(deadline) => error(Code::LoginTimeout),
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 2048 && token.bytes().all(|b| (0x21..=0x7e).contains(&b))
}
fn parse_expiry(value: &str) -> Result<Duration, TwitcastingAuthError> {
    let seconds = value
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| value.parse::<u64>().ok())
        .flatten()
        .filter(|seconds| (1..=MAX_EXPIRY_SECONDS).contains(seconds))
        .ok_or_else(|| error(Code::Other))?;
    Ok(Duration::from_secs(seconds))
}
struct Grant {
    token: String,
    expires: Option<Duration>,
    received_at: Instant,
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
    accepted: io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
    context: &Arc<Loopback>,
) -> Result<(), TwitcastingAuthError> {
    let (stream, _) = match accepted {
        Ok(accepted) => accepted,
        Err(e)
            if matches!(
                e.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::Interrupted
                    | io::ErrorKind::WouldBlock
            ) || e.raw_os_error() == Some(WSAECONNRESET) =>
        {
            return Ok(());
        }
        Err(_) => return Err(error(Code::Other)),
    };
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
const DENIED_PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p>許可されませんでした。アプリでやり直してください。</p><script>history.replaceState(null, '', location.pathname);</script></html>"#;
const PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p id="message">認証しています…</p><script>
const fragment = location.hash.slice(1);
const search = location.search;
history.replaceState(null, '', location.pathname);
// Preserve literal plus signs in the fragment before form decoding.
const params = new URLSearchParams(fragment.replaceAll('+', '%2B'));
const query = new URLSearchParams(search);
const denied = p => p.get('result') === 'denied' || p.has('error');
// Forward the raw pairs the app reads without re-encoding, so the body never outgrows the URL.
const keys = ['access_token', 'expires_in', 'state'];
const grant = fragment.split('&').filter(pair => keys.includes(pair.split('=')[0])).join('&');
const state = params.get('state') ?? query.get('state');
const refusal = 'result=denied' + (state ? '&state=' + encodeURIComponent(state) : '');
const body = denied(params) || denied(query) ? refusal : grant;
const message = document.getElementById('message');
if (!body) { message.textContent = '認証情報が見つかりません'; }
else { fetch('/twitcasting/token', {method:'POST',headers:{'Content-Type':'application/x-www-form-urlencoded'},body}).then(r => { message.textContent = r.ok ? 'アプリに戻ってください。この画面は閉じられます。' : '認証に失敗しました。アプリでやり直してください。'; }).catch(() => { message.textContent = 'アプリでログインをやり直してください。'; }); }
</script></html>"#;
async fn callback(context: &Loopback, req: Request) -> Response {
    let mut response = handle_callback(context, req).await;
    let headers = response.headers_mut();
    for (k,v) in [("cache-control","no-store"), ("referrer-policy","no-referrer"), ("x-content-type-options","nosniff"), ("content-security-policy","default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'")] { headers.insert(k, v.parse().unwrap()); }
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
        // An OAuth error in the query with this login's state ends the login at once. The
        // page served for any state-bearing query error never posts, so a stale page from an
        // earlier login cannot end the next one.
        let query = parse_form(req.uri().query().unwrap_or_default().as_bytes());
        if refused(&query) {
            if let Some(state) = query.get("state") {
                if state_matches(context, state) {
                    complete(context, Err(error(Code::Denied)));
                }
                return Html(DENIED_PAGE).into_response();
            }
        }
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
    let params = parse_form(&body);
    let (grant, response) = if refused(&params) {
        // A refusal carries no state per the provider flow; one that does must match.
        if params.get("state").is_some_and(|s| !state_matches(context, s)) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        (Err(error(Code::Denied)), StatusCode::OK)
    } else {
        if !params.get("state").is_some_and(|s| state_matches(context, s)) {
            return StatusCode::BAD_REQUEST.into_response();
        }
        let Some(token) = params.get("access_token").filter(|t| valid_token(t)) else {
            return StatusCode::BAD_REQUEST.into_response();
        };
        match params
            .get("expires_in")
            .map(|s| parse_expiry(s))
            .transpose()
        {
            Ok(expires) => (
                Ok(Grant {
                    token: token.clone(),
                    expires,
                    received_at: Instant::now(),
                }),
                StatusCode::OK,
            ),
            Err(err) => (Err(err), StatusCode::BAD_REQUEST),
        }
    };
    if !complete(context, grant) {
        return StatusCode::CONFLICT.into_response();
    }
    response.into_response()
}
/// Ends the login once; later results are rejected.
fn complete(context: &Loopback, grant: Result<Grant, TwitcastingAuthError>) -> bool {
    let mut accepted = context.accepted.lock().unwrap();
    if *accepted || context.cancel.is_cancelled() {
        return false;
    }
    *accepted = true;
    let _ = context.tx.try_send(grant);
    true
}
/// Percent-decodes only: a literal `+` stays `+`, matching the raw fragment the page posts.
fn parse_form(raw: &[u8]) -> HashMap<String, String> {
    let mut escaped = Vec::with_capacity(raw.len());
    for &b in raw {
        if b == b'+' {
            escaped.extend_from_slice(b"%2B");
        } else {
            escaped.push(b);
        }
    }
    url::form_urlencoded::parse(&escaped).into_owned().collect()
}
fn refused(params: &HashMap<String, String>) -> bool {
    params.get("result").is_some_and(|s| s == "denied") || params.contains_key("error")
}
fn state_matches(context: &Loopback, state: &str) -> bool {
    bool::from(context.state.as_bytes().ct_eq(state.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::MemorySecretStore;
    use std::{
        io::Write,
        process::{Command, Stdio},
    };
    use tokio::{
        io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
        net::TcpStream,
        sync::Notify,
        time::timeout,
    };

    fn run_callback_script(page: &str, callback_url: &str) -> String {
        let script = page.split_once("<script>").unwrap().1;
        let script = script.split_once("</script>").unwrap().0;
        let mut node = Command::new("node")
            .args(["-", callback_url])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Node.js is required to run the callback page tests");
        let mut input = node.stdin.take().unwrap();
        input
            .write_all(
                br#"
const location = new URL(process.argv[2]);
const history = { replaceState() {} };
const document = { getElementById: () => ({}) };
const fetch = (_path, { body }) => {
    process.stdout.write(String(body));
    return Promise.resolve({ ok: true });
};
"#,
            )
            .unwrap();
        input.write_all(script.as_bytes()).unwrap();
        drop(input);
        let output = node.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    async fn receive_callback_body(
        body: String,
    ) -> (StatusCode, Result<Grant, TwitcastingAuthError>) {
        let (tx, mut rx) = mpsc::channel(1);
        let context = Loopback {
            state: "test-state".into(),
            port: CALLBACK_PORT,
            read_timeout: Duration::from_secs(1),
            tx,
            accepted: Mutex::new(false),
            cancel: CancellationToken::new(),
        };
        let host = format!("localhost:{CALLBACK_PORT}");
        let request = Request::builder()
            .method(Method::POST)
            .uri(TOKEN_PATH)
            .header("host", &host)
            .header("origin", format!("http://{host}"))
            .body(Body::from(body))
            .unwrap();
        let status = handle_callback(&context, request).await.status();
        (
            status,
            rx.try_recv().expect("callback did not complete the login"),
        )
    }

    #[tokio::test]
    async fn callback_page_round_trips_token_with_form_delimiters() {
        for encoded_token in ["token+%25%26=end", "token%2B%25%26%3Dend"] {
            let body = run_callback_script(
                PAGE,
                &format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}#access_token={encoded_token}&state=test-state"),
            );
            let (status, grant) = receive_callback_body(body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(grant.unwrap().token, "token+%&=end");
        }
    }

    #[tokio::test]
    async fn oauth_error_completes_callback_with_fixed_denial() {
        let oauth_error = "error=access_denied&error_description=private%20provider%20details";
        let base = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
        for (url, expected) in [
            (format!("{base}#{oauth_error}"), "result=denied"),
            (format!("{base}#{oauth_error}&state=test-state"), "result=denied&state=test-state"),
        ] {
            let body = run_callback_script(PAGE, &url);
            assert_eq!(body, expected);
            assert_fixed_denial(body).await;
        }
        assert_fixed_denial(oauth_error.into()).await;
    }

    fn loopback() -> (Loopback, mpsc::Receiver<Result<Grant, TwitcastingAuthError>>) {
        let (tx, rx) = mpsc::channel(1);
        let context = Loopback {
            state: "test-state".into(),
            port: CALLBACK_PORT,
            read_timeout: Duration::from_secs(1),
            tx,
            accepted: Mutex::new(false),
            cancel: CancellationToken::new(),
        };
        (context, rx)
    }

    async fn send(context: &Loopback, method: Method, uri: &str, body: &str) -> (StatusCode, String, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", format!("localhost:{CALLBACK_PORT}"))
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = callback(context, request).await;
        let status = response.status();
        let csp = response.headers()["content-security-policy"].to_str().unwrap().to_owned();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, csp, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn refusals_need_this_logins_state_and_never_repost() {
        // A query refusal with the right state ends the login, and its page does not post.
        let (context, mut rx) = loopback();
        let uri = format!("{CALLBACK_PATH}?error=access_denied&state=test-state");
        let (status, csp, page) = send(&context, Method::GET, &uri, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(csp.contains("frame-ancestors 'none'"));
        assert!(!page.contains("fetch"), "refusal page posts again");
        assert!(matches!(rx.try_recv(), Ok(Err(e)) if matches!(e.code, Code::Denied)));

        // Refusals without this login's state are ignored (stale page, cross-site <img>).
        for uri in [
            format!("{CALLBACK_PATH}?error=access_denied&state=other-state"),
            format!("{CALLBACK_PATH}?error=access_denied"),
        ] {
            let (context, mut rx) = loopback();
            send(&context, Method::GET, &uri, "").await;
            assert!(rx.try_recv().is_err(), "{uri} ended the login");
        }
        let (context, mut rx) = loopback();
        let (status, _, _) = send(&context, Method::POST, TOKEN_PATH, "result=denied&state=other-state").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(rx.try_recv().is_err(), "refusal for another state ended the login");
    }

    #[tokio::test]
    async fn callback_page_posts_raw_fragment_within_limit() {
        // Re-encoding would turn every `+` into `%2B` and exceed the 4096-byte limit.
        let token = "+".repeat(2048);
        let state = "s".repeat(43);
        let (context, mut rx) = loopback();
        let context = Loopback { state: state.clone(), ..context };
        let url = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}#access_token={token}&token_type=bearer&state={state}&expires_in=60");
        let body = run_callback_script(PAGE, &url);
        assert!(body.len() <= 4096, "{}", body.len());
        let (status, _, _) = send(&context, Method::POST, TOKEN_PATH, &body).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(rx.try_recv().unwrap().unwrap().token, token);
    }

    async fn assert_fixed_denial(body: String) {
        let (status, grant) = receive_callback_body(body).await;
        assert_eq!(status, StatusCode::OK);
        let Err(denied) = grant else {
            panic!("callback granted login after an OAuth error");
        };
        assert_eq!(
            serde_json::to_value(denied).unwrap(),
            serde_json::json!({"code": "denied", "message": "許可されませんでした"})
        );
    }

    #[derive(Clone, Default)]
    struct PausedWriteStore {
        inner: MemorySecretStore,
        writing: Arc<Notify>,
        resume: Arc<Notify>,
    }

    impl SecretStore for PausedWriteStore {
        async fn get(&self, key: &str) -> Result<Option<String>, String> {
            self.inner.get(key).await
        }

        async fn put(&self, key: &str, value: &str) -> Result<(), String> {
            self.writing.notify_one();
            self.resume.notified().await;
            self.inner.put(key, value).await
        }

        async fn delete(&self, key: &str) -> Result<(), String> {
            self.inner.delete(key).await
        }
    }

    async fn login_fixture<S: SecretStore>(
        store: S,
    ) -> (
        Arc<TwitcastingAuth<S>>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        login_fixture_with(store, AuthOptions::default().read_timeout).await
    }

    async fn login_fixture_with<S: SecretStore>(
        store: S,
        read_timeout: Duration,
    ) -> (
        Arc<TwitcastingAuth<S>>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let verify = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", verify.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let route = axum::Router::new().route(
                "/verify_credentials",
                axum::routing::get(|| async {
                    axum::Json(serde_json::json!({
                        "app": {"client_id": "client"},
                        "user": {"id": "new-user", "screen_id": "new-screen", "name": "new"}
                    }))
                }),
            );
            axum::serve(verify, route).await.unwrap();
        });
        let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = probe.local_addr().unwrap();
        drop(probe);
        let auth = TwitcastingAuth::new(
            store,
            TwitcastingEndpoints {
                api_base,
                ..TwitcastingEndpoints::default()
            },
            AuthOptions {
                port: addr.port(),
                login_timeout: Duration::from_secs(3),
                read_timeout,
                ..AuthOptions::default()
            },
        );
        (auth, addr, server)
    }

    async fn post_grant<S: SecretStore>(
        auth: Arc<TwitcastingAuth<S>>,
        addr: std::net::SocketAddr,
        delay: Duration,
        extra: &str,
    ) -> tokio::task::JoinHandle<Result<TwitcastingStatus, TwitcastingAuthError>> {
        post_grant_as(auth, addr, delay, "new-token", extra).await
    }

    async fn post_grant_as<S: SecretStore>(
        auth: Arc<TwitcastingAuth<S>>,
        addr: std::net::SocketAddr,
        delay: Duration,
        token: &str,
        extra: &str,
    ) -> tokio::task::JoinHandle<Result<TwitcastingStatus, TwitcastingAuthError>> {
        let (started, listening) = oneshot::channel();
        let login = tokio::spawn(async move {
            auth.connect("client", 0, |url| {
                let state = url::Url::parse(&url)
                    .unwrap()
                    .query_pairs()
                    .find(|(name, _)| name == "state")
                    .unwrap()
                    .1
                    .into_owned();
                started.send(state).ok();
                Ok(())
            })
            .await
        });
        let state = timeout(Duration::from_secs(2), listening)
            .await
            .unwrap()
            .unwrap();
        tokio::time::sleep(delay).await;
        let mut client = BufReader::new(TcpStream::connect(addr).await.unwrap());
        let body = format!("access_token={token}&state={state}{extra}");
        let request = format!(
            "POST {TOKEN_PATH} HTTP/1.1\r\nHost: {addr}\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        client.write_all(request.as_bytes()).await.unwrap();
        let mut status = String::new();
        timeout(Duration::from_secs(2), client.read_line(&mut status))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        login
    }

    async fn abort_paused_write(timed_out: bool, seeded: bool) {
        let store = PausedWriteStore::default();
        if seeded {
            seed_record(&store.inner, None).await;
        }
        let previous = store.inner.raw(TOKEN_KEY);
        let (auth, addr, server) = login_fixture(store.clone()).await;
        auth.load(Some("client"), 0).await;
        let start = Instant::now();
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        timeout(Duration::from_secs(2), store.writing.notified())
            .await
            .unwrap();
        let wait = Duration::from_secs(1);
        let expected = if timed_out {
            tokio::time::sleep_until(start + Duration::from_secs(3)).await;
            Code::LoginTimeout
        } else {
            auth.cancel_login();
            Code::Cancelled
        };
        let result = timeout(wait, login)
            .await
            .expect("login stayed blocked on the credential write")
            .unwrap();
        assert!(matches!(
            (result.unwrap_err().code, expected),
            (Code::Cancelled, Code::Cancelled) | (Code::LoginTimeout, Code::LoginTimeout)
        ));
        assert!(store.inner.raw(TOKEN_KEY) == previous);
        assert_eq!(auth.user_id().as_deref(), seeded.then_some("user"));

        store.resume.notify_one();
        if seeded {
            // The compensating write restores the previous value.
            timeout(wait, store.writing.notified()).await.unwrap();
            store.resume.notify_one();
        }
        let _guard = timeout(wait, auth.operation.lock()).await.unwrap();
        assert!(store.inner.raw(TOKEN_KEY) == previous);
        assert_eq!(auth.user_id().as_deref(), seeded.then_some("user"));
        server.abort();
    }

    #[tokio::test]
    async fn cancel_during_write_deletes_late_token() {
        abort_paused_write(false, false).await;
    }

    #[tokio::test]
    async fn timeout_during_write_restores_previous_token() {
        abort_paused_write(true, true).await;
    }

    #[tokio::test]
    async fn short_expiry_starts_when_grant_is_received() {
        let store = MemorySecretStore::new();
        let (auth, addr, server) = login_fixture(store.clone()).await;
        let delay = Duration::from_millis(1100);
        let start = Instant::now();
        let login = post_grant(auth, addr, delay, "&expires_in=60").await;
        let status = timeout(Duration::from_secs(5), login)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let elapsed = start.elapsed().as_millis() as u64;
        assert_eq!(status.state, "ready");
        let record: Record = serde_json::from_str(&store.raw(TOKEN_KEY).unwrap()).unwrap();
        // connect() started after `start` with now_ms = 0, so the receipt time lies in between.
        assert!(record.obtained_at_ms >= delay.as_millis() as u64);
        assert!(record.obtained_at_ms <= elapsed, "{} > {elapsed}", record.obtained_at_ms);
        assert_eq!(status.expires_at_ms, Some(record.obtained_at_ms + 60_000));
        server.abort();
    }

    #[tokio::test]
    async fn invalid_expiry_fails_grant_instead_of_becoming_unknown() {
        for expiry in [
            "",
            "0",
            "-1",
            "3600.0",
            "%203600",
            "18446744073709551616",
            "9000000000000",
        ] {
            let (tx, mut rx) = mpsc::channel(1);
            let context = Loopback {
                state: "test-state".into(),
                port: CALLBACK_PORT,
                read_timeout: Duration::from_secs(1),
                tx,
                accepted: Mutex::new(false),
                cancel: CancellationToken::new(),
            };
            let request = Request::builder()
                .method(Method::POST)
                .uri(TOKEN_PATH)
                .header("host", format!("localhost:{CALLBACK_PORT}"))
                .body(Body::from(format!(
                    "access_token=test-token&state=test-state&expires_in={expiry}"
                )))
                .unwrap();
            assert_eq!(
                handle_callback(&context, request).await.status(),
                StatusCode::BAD_REQUEST
            );
            assert!(matches!(rx.try_recv(), Ok(Err(_))));
        }
    }

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
    async fn callback_page_forwards_only_grant_keys() {
        let base = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}");
        let cases = [
            // Unrelated provider fields must not push a short grant over the limit.
            (format!("#access_token=short&scope={}&state=test-state", "@".repeat(1365)), "short".to_string()),
            // A full-length plain token with every forwarded key.
            (format!("#access_token={}&token_type=bearer&state=test-state&expires_in=315360000", "a".repeat(2048)), "a".repeat(2048)),
        ];
        for (fragment, token) in cases {
            let body = run_callback_script(PAGE, &format!("{base}{fragment}"));
            assert!(!body.contains("scope"), "unrelated key was forwarded");
            let (status, grant) = receive_callback_body(body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(grant.unwrap().token, token);
        }
    }

    #[tokio::test]
    async fn load_treats_out_of_range_expiry_as_corrupt() {
        let store = MemorySecretStore::new();
        seed_record(&store, Some(9_001_731_600_000_000)).await;
        let auth = TwitcastingAuth::new(
            store,
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        let now_ms = 1_763_000_000_000;
        auth.load(Some("client"), now_ms).await;
        let status = auth.status(Some("client"), now_ms);
        assert_eq!(status.state, "loggedOut");
        assert_eq!(status.expires_at_ms, None);
    }

    #[derive(Clone, Default)]
    struct UnreadableStore(MemorySecretStore);

    impl SecretStore for UnreadableStore {
        async fn get(&self, _key: &str) -> Result<Option<String>, String> {
            Err("broken".into())
        }
        async fn put(&self, key: &str, value: &str) -> Result<(), String> {
            self.0.put(key, value).await
        }
        async fn delete(&self, key: &str) -> Result<(), String> {
            self.0.delete(key).await
        }
    }

    #[tokio::test]
    async fn unreadable_previous_value_does_not_block_login() {
        let store = UnreadableStore::default();
        let (auth, addr, server) = login_fixture(store.clone()).await;
        auth.load(Some("client"), 0).await;
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        let status = timeout(Duration::from_secs(5), login)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(status.state, "ready");
        assert!(store.0.raw(TOKEN_KEY).is_some());
        server.abort();
    }

    /// The first put pauses until resumed (or forever); every later write fails.
    #[derive(Clone, Default)]
    struct FailingRollbackStore {
        inner: MemorySecretStore,
        writing: Arc<Notify>,
        resume: Arc<Notify>,
        hang: bool,
        puts: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl SecretStore for FailingRollbackStore {
        async fn get(&self, key: &str) -> Result<Option<String>, String> {
            self.inner.get(key).await
        }
        async fn put(&self, key: &str, value: &str) -> Result<(), String> {
            if self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                return Err("rollback failed".into());
            }
            self.writing.notify_one();
            if self.hang {
                std::future::pending::<()>().await;
            }
            self.resume.notified().await;
            self.inner.put(key, value).await
        }
        async fn delete(&self, _key: &str) -> Result<(), String> {
            Err("rollback failed".into())
        }
    }

    async fn abort_with_failing_rollback(seeded: bool, hang: bool) {
        let store = FailingRollbackStore {
            hang,
            ..Default::default()
        };
        if seeded {
            seed_record(&store.inner, None).await;
        }
        let (auth, addr, server) = login_fixture_with(store.clone(), Duration::from_millis(500)).await;
        auth.load(Some("client"), 0).await;
        assert_eq!(auth.user_id().as_deref(), seeded.then_some("user"));
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        timeout(Duration::from_secs(2), store.writing.notified())
            .await
            .unwrap();
        auth.cancel_login();
        let result = timeout(Duration::from_secs(1), login).await.unwrap().unwrap();
        assert!(matches!(result.unwrap_err().code, Code::Cancelled));
        store.resume.notify_one();

        // The rollback gives up instead of holding the operation lock forever.
        let guard = timeout(Duration::from_secs(5), auth.operation.lock())
            .await
            .expect("rollback kept the operation lock");
        drop(guard);
        assert_eq!(auth.status(Some("client"), 0).state, "loggedOut");
        assert!(auth.user_id().is_none());
        if !hang {
            assert!(store.inner.raw(TOKEN_KEY).unwrap().contains("new-token"));
        }
        timeout(Duration::from_secs(5), auth.disconnect())
            .await
            .expect("disconnect waited on the abandoned rollback")
            .unwrap_err();
        server.abort();
    }

    #[tokio::test]
    async fn failing_delete_after_cancel_releases_lock() {
        abort_with_failing_rollback(false, false).await;
    }

    #[tokio::test]
    async fn failing_restore_after_cancel_releases_lock() {
        abort_with_failing_rollback(true, false).await;
    }

    #[tokio::test]
    async fn hanging_write_after_cancel_releases_lock() {
        abort_with_failing_rollback(false, true).await;
    }

    /// Like the OS store: the first put keeps writing in another task even if its caller
    /// stops waiting, and lands after `delay`.
    #[derive(Clone, Default)]
    struct DetachedWriteStore {
        inner: MemorySecretStore,
        writing: Arc<Notify>,
        puts: Arc<std::sync::atomic::AtomicUsize>,
        never_delete: bool,
    }

    impl SecretStore for DetachedWriteStore {
        async fn get(&self, key: &str) -> Result<Option<String>, String> {
            self.inner.get(key).await
        }
        async fn put(&self, key: &str, value: &str) -> Result<(), String> {
            if self.puts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) > 0 {
                return self.inner.put(key, value).await;
            }
            self.writing.notify_one();
            let (inner, key, value) = (self.inner.clone(), key.to_owned(), value.to_owned());
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(1500)).await;
                inner.put(&key, &value).await
            })
            .await
            .unwrap()
        }
        async fn delete(&self, key: &str) -> Result<(), String> {
            if self.never_delete {
                std::future::pending::<()>().await;
            }
            self.inner.delete(key).await
        }
    }

    async fn cancel_detached_write(
        store: &DetachedWriteStore,
    ) -> (Arc<TwitcastingAuth<DetachedWriteStore>>, std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let (auth, addr, server) = login_fixture_with(store.clone(), Duration::from_millis(500)).await;
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        timeout(Duration::from_secs(2), store.writing.notified())
            .await
            .unwrap();
        auth.cancel_login();
        let result = timeout(Duration::from_secs(1), login).await.unwrap().unwrap();
        assert!(matches!(result.unwrap_err().code, Code::Cancelled));
        (auth, addr, server)
    }

    #[tokio::test]
    async fn late_cancelled_write_does_not_overwrite_next_login() {
        let store = DetachedWriteStore::default();
        let (auth, addr, server) = cancel_detached_write(&store).await;
        let next = post_grant_as(auth.clone(), addr, Duration::ZERO, "token-b", "").await;
        let status = timeout(Duration::from_secs(5), next).await.unwrap().unwrap().unwrap();
        assert_eq!(status.state, "ready");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let record: Record = serde_json::from_str(&store.inner.raw(TOKEN_KEY).unwrap()).unwrap();
        assert_eq!(record.access_token, "token-b");
        server.abort();
    }

    #[tokio::test]
    async fn late_cancelled_write_does_not_survive_disconnect() {
        let store = DetachedWriteStore::default();
        let (auth, _, server) = cancel_detached_write(&store).await;
        let _ = timeout(Duration::from_secs(3), auth.disconnect())
            .await
            .expect("disconnect did not return");
        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(store.inner.raw(TOKEN_KEY).is_none());
        server.abort();
    }

    #[tokio::test]
    async fn disconnect_returns_when_delete_never_completes() {
        let store = DetachedWriteStore {
            never_delete: true,
            ..Default::default()
        };
        seed_record(&store.inner, None).await;
        let auth = TwitcastingAuth::new(
            store,
            TwitcastingEndpoints::default(),
            AuthOptions {
                read_timeout: Duration::from_millis(500),
                ..AuthOptions::default()
            },
        );
        auth.load(Some("client"), 0).await;
        assert_eq!(auth.status(Some("client"), 0).state, "ready");
        let result = timeout(Duration::from_secs(2), auth.disconnect())
            .await
            .expect("disconnect waited on a delete that never completes");
        assert!(matches!(result.unwrap_err().code, Code::TokenStore));
        assert_eq!(auth.status(Some("client"), 0).state, "loggedOut");
        drop(timeout(Duration::from_secs(1), auth.operation.lock()).await.unwrap());
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
