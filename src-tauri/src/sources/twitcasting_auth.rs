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
    fmt, fs,
    future::Future,
    io,
    path::{Path, PathBuf},
    pin::Pin,
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
pub(crate) const LOGOUT_MARKER_FILE: &str = "twitcasting-logout-pending";
pub const DEFAULT_CLIENT_ID: &str = "";
// Preserve the callback envelope and existing records, but cap new persisted tokens separately.
const MAX_CALLBACK_TOKEN_BYTES: usize = 2048;
const MAX_NEW_TOKEN_BYTES: usize = 1024;
const STORE_QUEUE_CAPACITY: usize = 4;
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
    Loading,
    LoadFailed,
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
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub logout_pending: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    logout_issue: Option<LogoutIssue>,
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
#[derive(Default)]
enum RestoreState {
    #[default]
    Idle,
    Loading,
    Failed,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
enum LogoutIssue {
    MarkerUnavailable,
    MarkerInaccessible,
    MarkerCorrupt,
    DeleteFailed,
    TimedOut,
}

enum LogoutMarker {
    MemoryOnly,
    Unavailable,
    File(PathBuf),
}

#[derive(Default)]
struct Memory {
    record: Option<Record>,
    generation: u64,
    unauthorized: bool,
    expired: bool,
    expiry_task: Option<DropGuard>,
    restore: RestoreState,
    logout_pending: bool,
    logout_issue: Option<LogoutIssue>,
    logout_running: bool,
}
struct Session {
    memory: Mutex<Memory>,
    changes: watch::Sender<AuthState>,
    logout_marker: LogoutMarker,
    #[cfg(test)]
    marker_io_hook: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}
type StoreJob = Pin<Box<dyn Future<Output = ()> + Send>>;

pub struct TwitcastingAuth<S: SecretStore> {
    store: S,
    endpoints: TwitcastingEndpoints,
    options: AuthOptions,
    session: Arc<Session>,
    operation: AsyncMutex<()>,
    login: Mutex<Option<CancellationToken>>,
    // An in-progress OS call may outlive its caller's deadline. Only the worker advances
    // the queue, keeping a write and its compensation ahead of the next store operation.
    store_queue: Mutex<Option<mpsc::Sender<StoreJob>>>,
}
impl<S: SecretStore> fmt::Debug for TwitcastingAuth<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingAuth").finish_non_exhaustive()
    }
}
impl<S: SecretStore> TwitcastingAuth<S> {
    pub fn new(store: S, endpoints: TwitcastingEndpoints, options: AuthOptions) -> Arc<Self> {
        Self::with_marker(store, endpoints, options, LogoutMarker::MemoryOnly)
    }

    pub(crate) fn with_logout_marker(
        store: S,
        endpoints: TwitcastingEndpoints,
        options: AuthOptions,
        logout_marker: Option<PathBuf>,
    ) -> Arc<Self> {
        let marker = logout_marker.map_or(LogoutMarker::Unavailable, LogoutMarker::File);
        Self::with_marker(store, endpoints, options, marker)
    }

    fn with_marker(
        store: S,
        endpoints: TwitcastingEndpoints,
        options: AuthOptions,
        logout_marker: LogoutMarker,
    ) -> Arc<Self> {
        let (changes, _) = watch::channel(AuthState::LoggedOut);
        Arc::new(Self {
            store,
            endpoints,
            options,
            session: Arc::new(Session {
                memory: Mutex::new(Memory::default()),
                changes,
                logout_marker,
                #[cfg(test)]
                marker_io_hook: Mutex::new(None),
            }),
            operation: AsyncMutex::new(()),
            login: Mutex::new(None),
            store_queue: Mutex::new(None),
        })
    }

    fn enqueue(
        &self,
        job: impl Future<Output = ()> + Send + 'static,
    ) -> Result<(), TwitcastingAuthError> {
        let mut queue = self.store_queue.lock().unwrap();
        let queue = queue.get_or_insert_with(|| {
            let (tx, mut rx) = mpsc::channel::<StoreJob>(STORE_QUEUE_CAPACITY);
            tokio::spawn(async move {
                while let Some(job) = rx.recv().await {
                    job.await;
                }
            });
            tx
        });
        queue
            .try_send(Box::pin(job))
            .map_err(|_| error(Code::TokenStore))
    }

    fn store_request<T: Send + 'static>(
        &self,
        job: impl Future<Output = T> + Send + 'static,
    ) -> Result<oneshot::Receiver<T>, TwitcastingAuthError> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(async move {
            let _ = tx.send(job.await);
        })?;
        Ok(rx)
    }

    pub(crate) async fn initialize(self: &Arc<Self>, client_id: Option<&str>, now_ms: u64) {
        let check = tokio::time::timeout(
            self.options.read_timeout,
            self.session.marker_io(inspect_marker),
        )
        .await
        .unwrap_or(Err(LogoutIssue::MarkerInaccessible));
        let (retry_logout, issue) = match check {
            Ok(None) => {
                self.load(client_id, now_ms).await;
                return;
            }
            Ok(Some(metadata)) => (
                true,
                (!metadata.is_file()).then_some(LogoutIssue::MarkerCorrupt),
            ),
            Err(issue) => (false, Some(issue)),
        };
        {
            let mut memory = self.session.memory.lock().unwrap();
            memory.logout_pending = true;
            memory.logout_issue = issue;
        }
        if retry_logout {
            let _ = self.disconnect().await;
        }
    }

    pub(crate) async fn reload(&self, client_id: Option<&str>, now_ms: u64) {
        self.restore(client_id, now_ms, true).await;
    }

    pub async fn load(&self, client_id: Option<&str>, now_ms: u64) {
        self.restore(client_id, now_ms, false).await;
    }

    async fn restore(&self, client_id: Option<&str>, now_ms: u64, retry_only: bool) {
        let observed_at = Instant::now();
        let deadline = observed_at + self.options.read_timeout;
        let session = self.session.clone();
        let client_id = client_id.map(str::to_owned);
        let (generation, read) = {
            let mut memory = session.memory.lock().unwrap();
            if memory.logout_pending
                || memory.unauthorized
                || matches!(memory.restore, RestoreState::Loading)
                || (retry_only && !matches!(memory.restore, RestoreState::Failed))
            {
                return;
            }
            let generation = memory.generation;
            memory.restore = RestoreState::Loading;
            session.auth_state(&memory, client_id.as_deref(), now_ms);

            let store = self.store.clone();
            let session = session.clone();
            let client_id = client_id.clone();
            let read = self.store_request(async move {
                if Instant::now() >= deadline {
                    return;
                }
                let Ok(value) = store.get(TOKEN_KEY).await else {
                    return;
                };
                let record = value.and_then(|s| parse_stored_record(&s));
                let mut memory = session.memory.lock().unwrap();
                if Instant::now() < deadline
                    && memory.generation == generation
                    && !memory.unauthorized
                {
                    session.replace_record(
                        &mut memory,
                        record,
                        client_id.as_deref(),
                        now_ms,
                        observed_at,
                    );
                }
            });
            (generation, read)
        };
        // End Loading even if the IPC caller stops waiting or the read never returns.
        let _ = tokio::spawn(async move {
            if let Ok(read) = read {
                let _ = tokio::time::timeout_at(deadline, read).await;
            }
            let mut memory = session.memory.lock().unwrap();
            if memory.generation == generation && matches!(memory.restore, RestoreState::Loading) {
                memory.restore = RestoreState::Failed;
                session.auth_state(&memory, client_id.as_deref(), now_ms);
            }
        })
        .await;
    }
    pub fn status(&self, client_id: Option<&str>, now_ms: u64) -> TwitcastingStatus {
        let memory = self.session.memory.lock().unwrap();
        let state = self.session.auth_state(&memory, client_id, now_ms);
        let r = memory.record.as_ref();
        TwitcastingStatus {
            configured: client_id.is_some_and(|c| !c.trim().is_empty()),
            state: match state {
                AuthState::LoggedOut => "loggedOut",
                AuthState::Loading => "loading",
                AuthState::LoadFailed => "loadFailed",
                AuthState::Ready => "ready",
                AuthState::ReloginRequired => "reloginRequired",
            }
            .into(),
            screen_id: r.map(|r| r.screen_id.clone()),
            name: r.map(|r| r.name.clone()),
            expires_at_ms: r.and_then(|r| r.expires_at_ms),
            callback_url: format!("http://localhost:{}{CALLBACK_PATH}", self.options.port),
            logout_pending: memory.logout_pending,
            logout_issue: memory.logout_issue,
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
        let memory = self.session.memory.lock().unwrap();
        (self.session.auth_state(&memory, client_id, now_ms) == AuthState::Ready).then(|| {
            let record = memory.record.as_ref().unwrap();
            (record.access_token.clone(), memory.generation)
        })
    }
    /// Returns the verified ID while auth is Ready. Check `status` or `bearer`
    /// with the current ClientID before using this ID for a new request.
    pub fn user_id(&self) -> Option<String> {
        let memory = self.session.memory.lock().unwrap();
        if *self.session.changes.borrow() != AuthState::Ready {
            return None;
        }
        memory.record.as_ref().map(|r| r.user_id.clone())
    }
    pub fn subscribe(&self) -> watch::Receiver<AuthState> {
        self.session.changes.subscribe()
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
        let mut memory = self.session.memory.lock().unwrap();
        if memory.record.is_some()
            && !memory.unauthorized
            && generation.is_none_or(|g| g == memory.generation)
        {
            memory.unauthorized = true;
            self.session
                .changes
                .send_replace(AuthState::ReloginRequired);
        }
    }
    pub fn cancel_login(&self) {
        if let Some(cancel) = self.login.lock().unwrap().as_ref() {
            cancel.cancel();
        }
    }
    pub async fn disconnect(self: &Arc<Self>) -> Result<(), TwitcastingAuthError> {
        let deadline = Instant::now() + self.options.read_timeout;
        let generation = {
            let login = self.login.lock().unwrap();
            if let Some(cancel) = login.as_ref() {
                cancel.cancel();
            }
            let mut memory = self.session.memory.lock().unwrap();
            if memory.logout_running {
                return Err(error(Code::TokenStore));
            }
            memory.logout_pending = true;
            memory.logout_running = true;
            self.session
                .replace_record(&mut memory, None, None, 0, Instant::now());
            memory.generation
        };
        // Keep the intent and deletion running after a caller timeout. Write the
        // marker ahead of the store queue, which can be waiting on an OS call.
        let auth = self.clone();
        let deletion = tokio::spawn(async move {
            let marked = auth.session.marker_io(mark_logout).await;
            if let Err(issue) = marked {
                auth.session.memory.lock().unwrap().logout_issue = Some(issue);
            }
            let store = auth.store.clone();
            let deleted = match auth.store_request(async move { store.delete(TOKEN_KEY).await }) {
                Ok(reply) => matches!(reply.await, Ok(Ok(()))),
                Err(_) => false,
            };
            let result = if deleted {
                auth.session.marker_io(clear_logout_marker).await
            } else {
                Err(marked.err().unwrap_or(LogoutIssue::DeleteFailed))
            };
            let mut memory = auth.session.memory.lock().unwrap();
            memory.logout_running = false;
            memory.logout_pending = result.is_err();
            memory.logout_issue = result.err();
            result
        });
        match tokio::time::timeout_at(deadline, deletion).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(_) => Err(error(Code::TokenStore)),
            Err(_) => {
                let mut memory = self.session.memory.lock().unwrap();
                if memory.generation == generation && memory.logout_running {
                    memory.logout_issue.get_or_insert(LogoutIssue::TimedOut);
                }
                Err(error(Code::TokenStore))
            }
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
        {
            let mut login = self.login.lock().unwrap();
            if self.session.memory.lock().unwrap().logout_pending {
                return Err(error(Code::TokenStore));
            }
            if let Some(old) = login.replace(cancel.clone()) {
                old.cancel();
            }
        }
        let _operation = tokio::select! {
            biased;
            stop = login_ended(&cancel, deadline) => return Err(stop),
            lock = self.operation.lock() => lock,
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
            let PendingGrant { grant, browser } = grant?;
            let received_at = grant.received_at;
            let verified = self
                .verify_grant(grant, client_id, now_ms, observed_at)
                .await;
            let status = if verified.is_ok() {
                StatusCode::OK
            } else {
                StatusCode::BAD_REQUEST
            };
            let _ = browser.send(status);
            let (record, serialized) = verified?;
            Ok((record, serialized, received_at))
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
        let (record, serialized, received_at) = result?;
        let obtained_at_ms = record.obtained_at_ms;
        let token_deadline = record
            .expires_at_ms
            .map(|expiry| received_at + Duration::from_millis(expiry - record.obtained_at_ms));
        let (written, write_result) = oneshot::channel();
        let (commit, decision) = oneshot::channel();
        {
            let memory = self.session.memory.lock().unwrap();
            if memory.logout_pending || cancel.is_cancelled() {
                return Err(error(Code::Cancelled));
            }
            let session = self.session.clone();
            let store = self.store.clone();
            self.enqueue(async move {
                if written.is_closed() {
                    return;
                }
                // An unreadable old value must not block a fresh login.
                let previous = store.get(TOKEN_KEY).await.ok().flatten();
                if written.is_closed() {
                    return;
                }
                let generation = session.memory.lock().unwrap().generation;
                let saved = store.put(TOKEN_KEY, &serialized).await.is_ok();
                let _ = written.send(saved);
                // A caller that timed out, was cancelled, or dropped never commits the write.
                if decision.await.is_err()
                    && !rollback(&store, previous.as_deref(), &serialized).await
                {
                    let mut memory = session.memory.lock().unwrap();
                    if memory.generation == generation && !memory.logout_pending {
                        session.replace_record(&mut memory, None, None, 0, Instant::now());
                    }
                }
            })?;
        }
        let saved = tokio::select! {
            biased;
            stop = login_ended(&cancel, deadline) => return Err(stop),
            _ = tokio::time::sleep(self.options.read_timeout) => return Err(error(Code::TokenStore)),
            saved = write_result => saved.map_err(|_| error(Code::TokenStore))?,
        };
        {
            let mut memory = self.session.memory.lock().unwrap();
            if memory.logout_pending || cancel.is_cancelled() {
                return Err(error(Code::Cancelled));
            }
            if Instant::now() >= deadline {
                return Err(error(Code::LoginTimeout));
            }
            if !saved {
                let _ = commit.send(());
                return Err(error(Code::TokenStore));
            }
            if token_deadline.is_some_and(|expiry| Instant::now() >= expiry) {
                return Err(error(Code::Other));
            }
            self.session.replace_record(
                &mut memory,
                Some(record),
                Some(client_id),
                obtained_at_ms,
                received_at,
            );
            let _ = commit.send(());
        }
        Ok(self.status(Some(client_id), obtained_at_ms))
    }

    async fn verify_grant(
        &self,
        grant: Grant,
        client_id: &str,
        now_ms: u64,
        observed_at: Instant,
    ) -> Result<(Record, String), TwitcastingAuthError> {
        let received_at = grant.received_at;
        let obtained_at_ms = now_ms
            .checked_add(received_at.duration_since(observed_at).as_millis() as u64)
            .ok_or_else(|| error(Code::Other))?;
        let expires_at_ms = grant
            .expires
            .map(|duration| {
                obtained_at_ms
                    .checked_add(duration.as_millis() as u64)
                    .filter(|expiry| *expiry > obtained_at_ms && *expiry <= MAX_JS_DATE_MS)
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
        let serialized = serde_json::to_string(&record).map_err(|_| error(Code::Other))?;
        if !crate::integrations::token_fits_windows_credential(&serialized) {
            return Err(error(Code::TokenStore));
        }
        Ok((record, serialized))
    }
}

impl Session {
    fn replace_record(
        self: &Arc<Self>,
        memory: &mut Memory,
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
        memory.generation = memory.generation.wrapping_add(1);
        memory.record = record;
        memory.unauthorized = false;
        memory.expired = deadline.is_some_and(|d| d <= Instant::now());
        memory.expiry_task = None;
        memory.restore = RestoreState::Idle;
        self.auth_state(memory, client_id, now_ms);
        if let Some(deadline) = deadline.filter(|_| !memory.expired) {
            let generation = memory.generation;
            let current = Arc::downgrade(self);
            let stop = CancellationToken::new();
            memory.expiry_task = Some(stop.clone().drop_guard());
            tokio::spawn(async move {
                tokio::select! {
                    _ = stop.cancelled() => return,
                    _ = tokio::time::sleep_until(deadline) => {}
                }
                if let Some(current) = current.upgrade() {
                    let mut memory = current.memory.lock().unwrap();
                    if memory.generation == generation {
                        memory.expired = true;
                        current.changes.send_if_modified(|state| {
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
            None => match memory.restore {
                RestoreState::Idle => AuthState::LoggedOut,
                RestoreState::Loading => AuthState::Loading,
                RestoreState::Failed => AuthState::LoadFailed,
            },
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

    async fn marker_io<T: Default + Send + 'static>(
        &self,
        operation: fn(&Path) -> Result<T, LogoutIssue>,
    ) -> Result<T, LogoutIssue> {
        match &self.logout_marker {
            LogoutMarker::MemoryOnly => Ok(T::default()),
            LogoutMarker::Unavailable => Err(LogoutIssue::MarkerUnavailable),
            LogoutMarker::File(path) => {
                let path = path.clone();
                #[cfg(test)]
                let hook = self.marker_io_hook.lock().unwrap().clone();
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(hook) = hook {
                        hook();
                    }
                    operation(&path)
                })
                .await
                .unwrap_or(Err(LogoutIssue::MarkerInaccessible))
            }
        }
    }
}

fn inspect_marker(path: &Path) -> Result<Option<fs::Metadata>, LogoutIssue> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // On Windows a file in place of the parent can also report NotFound.
            let Some(parent) = path.parent() else {
                return Err(LogoutIssue::MarkerInaccessible);
            };
            match fs::metadata(parent) {
                Ok(metadata) if metadata.is_dir() => Ok(None),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
                _ => Err(LogoutIssue::MarkerInaccessible),
            }
        }
        Err(_) => Err(LogoutIssue::MarkerInaccessible),
    }
}

fn mark_logout(path: &Path) -> Result<(), LogoutIssue> {
    if let Some(metadata) = inspect_marker(path)? {
        return if metadata.is_file() {
            Ok(())
        } else {
            Err(LogoutIssue::MarkerCorrupt)
        };
    }
    let parent = path.parent().ok_or(LogoutIssue::MarkerInaccessible)?;
    fs::create_dir_all(parent).map_err(|_| LogoutIssue::MarkerInaccessible)?;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|_| LogoutIssue::MarkerInaccessible)
}

fn clear_logout_marker(path: &Path) -> Result<(), LogoutIssue> {
    let Some(metadata) = inspect_marker(path)? else {
        return Ok(());
    };
    let file_type = metadata.file_type();
    let directory = file_type.is_dir();
    #[cfg(windows)]
    let directory = {
        use std::os::windows::fs::FileTypeExt;
        directory || file_type.is_symlink_dir()
    };
    let removed = if directory {
        // Remove an empty directory or a Windows directory link without traversing it.
        fs::remove_dir(path)
    } else {
        fs::remove_file(path)
    };
    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => inspect_marker(path)?
            .is_none()
            .then_some(())
            .ok_or(LogoutIssue::MarkerInaccessible),
        Err(_) if file_type.is_file() => Err(LogoutIssue::MarkerInaccessible),
        Err(_) => Err(LogoutIssue::MarkerCorrupt),
    }
}

fn parse_stored_record(value: &str) -> Option<Record> {
    serde_json::from_str::<Record>(value)
        .ok()
        .filter(|r| valid_token(&r.access_token))
        .filter(|r| {
            r.expires_at_ms.is_none_or(|expiry| {
                expiry <= MAX_JS_DATE_MS
                    && expiry.saturating_sub(r.obtained_at_ms) <= MAX_EXPIRY_SECONDS * 1000
            })
        })
}

/// Runs on the store worker before the next job. Undo only a write that actually landed.
async fn rollback(store: &impl SecretStore, previous: Option<&str>, written: &str) -> bool {
    for attempt in 0..ROLLBACK_ATTEMPTS {
        if attempt > 0 {
            tokio::time::sleep(ROLLBACK_RETRY).await;
        }
        let undone = async {
            if store.get(TOKEN_KEY).await?.as_deref() != Some(written) {
                return Ok(());
            }
            match previous {
                Some(value) => store.put(TOKEN_KEY, value).await,
                None => store.delete(TOKEN_KEY).await,
            }
        }
        .await;
        if undone.is_ok() {
            return true;
        }
    }
    false
}

async fn login_ended(cancel: &CancellationToken, deadline: Instant) -> TwitcastingAuthError {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => error(Code::Cancelled),
        _ = tokio::time::sleep_until(deadline) => error(Code::LoginTimeout),
    }
}

fn valid_token(token: &str) -> bool {
    !token.is_empty()
        && token.len() <= MAX_CALLBACK_TOKEN_BYTES
        && token.bytes().all(|b| (0x21..=0x7e).contains(&b))
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
struct PendingGrant {
    grant: Grant,
    browser: oneshot::Sender<StatusCode>,
}
struct Loopback {
    state: String,
    port: u16,
    read_timeout: Duration,
    tx: mpsc::Sender<Result<PendingGrant, TwitcastingAuthError>>,
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
const IGNORED_PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p>このページは閉じてください。</p><script>history.replaceState(null, '', location.pathname);</script></html>"#;
const PAGE: &str = r#"<!doctype html><html lang="ja"><meta charset="utf-8"><title>ツイキャス認証</title><p id="message">認証しています…</p><script>
const fragment = location.hash.slice(1);
const query = location.search.slice(1);
history.replaceState(null, '', location.pathname);
function parse(raw) {
    const params = new Map();
    for (const pair of raw.split('&')) {
        // Decode the key and value, preserving literal plus signs and the original pair.
        const entry = new URLSearchParams('?' + pair.replaceAll('+', '%2B')).entries().next().value;
        if (entry) params.set(entry[0], {value: entry[1], pair});
    }
    return params;
}
const refused = params => params.get('result')?.value === 'denied' || params.has('error');
const queryParams = parse(query);
const fragmentParams = parse(fragment);
const denial = refused(queryParams) ? queryParams : refused(fragmentParams) ? fragmentParams : null;
const state = denial?.get('state');
const keys = ['access_token', 'expires_in', 'state'];
const grant = keys.map(key => fragmentParams.get(key)?.pair).filter(pair => pair !== undefined).join('&');
const body = denial ? 'result=denied' + (state ? '&' + state.pair : '') : grant;
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
        let query = parse_form(req.uri().query().unwrap_or_default().as_bytes());
        if refused(&query) {
            if let Some(state) = query.get("state") {
                let accepted =
                    state_matches(context, state) && complete(context, Err(error(Code::Denied)));
                return Html(if accepted { DENIED_PAGE } else { IGNORED_PAGE }).into_response();
            }
            // The browser posts a stateless refusal through the Origin-checked path.
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
    if refused(&params) {
        // Providers may omit state on refusal. A supplied state still belongs to one login.
        if params
            .get("state")
            .is_some_and(|s| !state_matches(context, s))
        {
            return StatusCode::BAD_REQUEST.into_response();
        }
        return complete_error(context, error(Code::Denied), StatusCode::OK);
    }
    if !params
        .get("state")
        .is_some_and(|s| state_matches(context, s))
    {
        return StatusCode::BAD_REQUEST.into_response();
    }
    let Some(token) = params.get("access_token").filter(|t| valid_token(t)) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    if token.len() > MAX_NEW_TOKEN_BYTES {
        return complete_error(context, error(Code::TokenStore), StatusCode::BAD_REQUEST);
    }
    let expires = match params
        .get("expires_in")
        .map(|s| parse_expiry(s))
        .transpose()
    {
        Ok(expires) => expires,
        Err(err) => return complete_error(context, err, StatusCode::BAD_REQUEST),
    };
    let (browser, verdict) = oneshot::channel();
    let pending = PendingGrant {
        grant: Grant {
            token: token.clone(),
            expires,
            received_at: Instant::now(),
        },
        browser,
    };
    if !complete(context, Ok(pending)) {
        return StatusCode::CONFLICT.into_response();
    }
    verdict
        .await
        .unwrap_or(StatusCode::BAD_REQUEST)
        .into_response()
}

fn complete_error(context: &Loopback, err: TwitcastingAuthError, status: StatusCode) -> Response {
    if complete(context, Err(err)) {
        status.into_response()
    } else {
        StatusCode::CONFLICT.into_response()
    }
}
/// Ends the login once; later results are rejected.
fn complete(context: &Loopback, grant: Result<PendingGrant, TwitcastingAuthError>) -> bool {
    let mut accepted = context.accepted.lock().unwrap();
    if *accepted || context.cancel.is_cancelled() {
        return false;
    }
    *accepted = true;
    context.tx.try_send(grant).is_ok()
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
        receive_callback_body_with_state(body, "test-state").await
    }

    async fn receive_callback_body_with_state(
        body: String,
        state: &str,
    ) -> (StatusCode, Result<Grant, TwitcastingAuthError>) {
        let (context, mut rx) = loopback();
        let context = Loopback {
            state: state.into(),
            ..context
        };
        let host = format!("localhost:{CALLBACK_PORT}");
        let request = Request::builder()
            .method(Method::POST)
            .uri(TOKEN_PATH)
            .header("host", &host)
            .header("origin", format!("http://{host}"))
            .body(Body::from(body))
            .unwrap();
        let (response, grant) = tokio::join!(handle_callback(&context, request), async {
            timeout(Duration::from_secs(1), rx.recv())
                .await
                .unwrap()
                .expect("callback did not complete the login")
                .map(|pending| {
                    pending.browser.send(StatusCode::OK).unwrap();
                    pending.grant
                })
        });
        (response.status(), grant)
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
        let body = run_callback_script(PAGE, &format!("{base}#{oauth_error}&state=test-state"));
        assert_eq!(body, "result=denied&state=test-state");
        assert_fixed_denial(body).await;
        assert_fixed_denial(format!("{oauth_error}&state=test-state")).await;
        let body = run_callback_script(PAGE, &format!("{base}#{oauth_error}"));
        assert_eq!(body, "result=denied");
        assert_fixed_denial(body).await;
        assert_fixed_denial(oauth_error.to_owned()).await;
    }

    fn loopback() -> (
        Loopback,
        mpsc::Receiver<Result<PendingGrant, TwitcastingAuthError>>,
    ) {
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

    async fn send(
        context: &Loopback,
        method: Method,
        uri: &str,
        body: &str,
    ) -> (StatusCode, String, String) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", format!("localhost:{CALLBACK_PORT}"))
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = callback(context, request).await;
        let status = response.status();
        let csp = response.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = to_bytes(response.into_body(), 1 << 16).await.unwrap();
        (status, csp, String::from_utf8(body.to_vec()).unwrap())
    }

    #[tokio::test]
    async fn get_alone_needs_matching_state_to_complete_refusal() {
        // A query refusal with the right state ends the login, and its page does not post.
        let (context, mut rx) = loopback();
        let uri = format!("{CALLBACK_PATH}?error=access_denied&state=test-state");
        let (status, csp, page) = send(&context, Method::GET, &uri, "").await;
        assert_eq!(status, StatusCode::OK);
        assert!(csp.ends_with("; frame-ancestors 'none'"));
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
        let (status, _, _) = send(
            &context,
            Method::POST,
            TOKEN_PATH,
            "result=denied&state=other-state",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            rx.try_recv().is_err(),
            "refusal for another state ended the login"
        );
    }

    #[tokio::test]
    async fn callback_page_posts_raw_fragment_within_limit() {
        // Re-encoding would expand each literal plus sign.
        let token = "+".repeat(MAX_NEW_TOKEN_BYTES);
        let state = "s".repeat(43);
        let url = format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}#access_token={token}&token_type=bearer&state={state}&expires_in=60");
        let body = run_callback_script(PAGE, &url);
        assert!(body.contains(&token), "the token was re-encoded");
        assert!(body.len() <= 4096, "{}", body.len());
        let (status, grant) = receive_callback_body_with_state(body, &state).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(grant.unwrap().token, token);
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
        let (login, status) = post_grant_response(auth, addr, delay, token, extra).await;
        assert_eq!(status, "HTTP/1.1 200 OK\r\n");
        login
    }

    async fn post_grant_response<S: SecretStore>(
        auth: Arc<TwitcastingAuth<S>>,
        addr: std::net::SocketAddr,
        delay: Duration,
        token: &str,
        extra: &str,
    ) -> (
        tokio::task::JoinHandle<Result<TwitcastingStatus, TwitcastingAuthError>>,
        String,
    ) {
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
        (login, status)
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
        timeout(wait, auth.store_request(async {}).unwrap())
            .await
            .unwrap()
            .unwrap();
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
        assert!(
            record.obtained_at_ms <= elapsed,
            "{} > {elapsed}",
            record.obtained_at_ms
        );
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
            (
                format!(
                    "#access_token=short&scope={}&state=test-state",
                    "@".repeat(1365)
                ),
                "short".to_string(),
            ),
            // A full-length plain token with every forwarded key.
            (
                format!(
                    "#access_token={}&token_type=bearer&state=test-state&expires_in=315360000",
                    "a".repeat(MAX_NEW_TOKEN_BYTES)
                ),
                "a".repeat(MAX_NEW_TOKEN_BYTES),
            ),
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
        let (auth, addr, server) =
            login_fixture_with(store.clone(), Duration::from_millis(500)).await;
        auth.load(Some("client"), 0).await;
        assert_eq!(auth.user_id().as_deref(), seeded.then_some("user"));
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        timeout(Duration::from_secs(2), store.writing.notified())
            .await
            .unwrap();
        auth.cancel_login();
        let result = timeout(Duration::from_secs(1), login)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result.unwrap_err().code, Code::Cancelled));
        store.resume.notify_one();

        // The rollback gives up instead of holding the operation lock forever.
        let guard = timeout(Duration::from_secs(5), auth.operation.lock())
            .await
            .expect("rollback kept the operation lock");
        drop(guard);
        if !hang {
            timeout(
                Duration::from_secs(5),
                auth.store_request(async {}).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
        }
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
        read_timeout: Duration,
    ) -> (
        Arc<TwitcastingAuth<DetachedWriteStore>>,
        std::net::SocketAddr,
        tokio::task::JoinHandle<()>,
    ) {
        let (auth, addr, server) = login_fixture_with(store.clone(), read_timeout).await;
        let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
        timeout(Duration::from_secs(2), store.writing.notified())
            .await
            .unwrap();
        auth.cancel_login();
        let result = timeout(Duration::from_secs(1), login)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result.unwrap_err().code, Code::Cancelled));
        (auth, addr, server)
    }

    #[tokio::test]
    async fn late_cancelled_write_does_not_overwrite_next_login() {
        let store = DetachedWriteStore::default();
        let (auth, addr, server) = cancel_detached_write(&store, Duration::from_secs(2)).await;
        let next = post_grant_as(auth.clone(), addr, Duration::ZERO, "token-b", "").await;
        let status = timeout(Duration::from_secs(5), next)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(status.state, "ready");
        tokio::time::sleep(Duration::from_secs(2)).await;
        let record: Record = serde_json::from_str(&store.inner.raw(TOKEN_KEY).unwrap()).unwrap();
        assert_eq!(record.access_token, "token-b");
        server.abort();
    }

    #[tokio::test]
    async fn late_cancelled_write_does_not_survive_disconnect() {
        let store = DetachedWriteStore::default();
        let (auth, _, server) = cancel_detached_write(&store, Duration::from_millis(500)).await;
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
        drop(
            timeout(Duration::from_secs(1), auth.operation.lock())
                .await
                .unwrap(),
        );
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

    mod revision7 {
        use super::*;

        #[tokio::test]
        async fn revision8_reload_preserves_unauthorized() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::new(
                store,
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            auth.load(Some("client"), 0).await;
            auth.report_unauthorized();
            auth.reload(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "reloginRequired");
            assert!(auth.bearer(Some("client"), 0).is_none());
        }

        #[tokio::test]
        async fn revision8_directory_marker_can_be_cleared() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::create_dir(&marker).unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.disconnect().await.unwrap();
            assert!(!marker.exists());
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        #[tokio::test]
        async fn revision8_marker_io_is_bounded_by_logout_deadline() {
            use std::sync::atomic::{AtomicUsize, Ordering};

            for stalled_call in [0, 1] {
                let dir = tempfile::tempdir().unwrap();
                let auth = TwitcastingAuth::with_logout_marker(
                    MemorySecretStore::new(),
                    TwitcastingEndpoints::default(),
                    AuthOptions {
                        read_timeout: Duration::from_millis(50),
                        ..AuthOptions::default()
                    },
                    Some(dir.path().join(LOGOUT_MARKER_FILE)),
                );
                let calls = AtomicUsize::new(0);
                *auth.session.marker_io_hook.lock().unwrap() = Some(Arc::new(move || {
                    if calls.fetch_add(1, Ordering::Relaxed) == stalled_call {
                        std::thread::sleep(Duration::from_millis(400));
                    }
                }));
                let start = Instant::now();
                let result = auth.disconnect().await;
                assert!(start.elapsed() < Duration::from_millis(350));
                assert!(matches!(result.unwrap_err().code, Code::TokenStore));
                assert!(auth.status(Some("client"), 0).logout_pending);
                timeout(Duration::from_secs(3), async {
                    while auth.status(Some("client"), 0).logout_pending {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                })
                .await
                .unwrap();
            }
        }

        #[tokio::test]
        async fn revision8_marker_io_leaves_memory_and_login_locks_available() {
            let dir = tempfile::tempdir().unwrap();
            let auth = auth_with_marker(
                MemorySecretStore::new(),
                dir.path().join(LOGOUT_MARKER_FILE),
            );
            let results = Arc::new(Mutex::new(Vec::new()));
            let weak = Arc::downgrade(&auth);
            *auth.session.marker_io_hook.lock().unwrap() = Some(Arc::new({
                let results = Arc::clone(&results);
                move || {
                    let auth = weak.upgrade().unwrap();
                    results.lock().unwrap().push((
                        auth.session.memory.try_lock().is_ok(),
                        auth.login.try_lock().is_ok(),
                    ));
                }
            }));
            auth.disconnect().await.unwrap();
            assert_eq!(*results.lock().unwrap(), [(true, true), (true, true)]);
        }

        #[tokio::test]
        async fn revision8_oversize_record_returns_browser_error() {
            let store = MemorySecretStore::new();
            let (auth, addr, server) = login_fixture(store.clone()).await;
            let token = "%22".repeat(400);
            let (login, status) = post_grant_response(auth, addr, Duration::ZERO, &token, "").await;
            assert!(matches!(
                login.await.unwrap().unwrap_err().code,
                Code::TokenStore
            ));
            assert!(store.raw(TOKEN_KEY).is_none());
            assert_eq!(status, "HTTP/1.1 400 Bad Request\r\n");
            server.abort();
        }

        #[tokio::test]
        async fn revision8_query_denial_requires_the_pages_post() {
            for query in ["result=denied", "error=&error_description=private"] {
                let (context, mut rx) = loopback();
                let uri = format!("{CALLBACK_PATH}?{query}");
                let (_, _, page) = send(&context, Method::GET, &uri, "").await;
                assert!(rx.try_recv().is_err());
                let body =
                    run_callback_script(&page, &format!("http://localhost:{CALLBACK_PORT}{uri}"));
                assert_eq!(body, "result=denied");
                assert_fixed_denial(body).await;
            }
        }

        #[tokio::test(start_paused = true)]
        async fn revision8_hung_read_leaves_loading_at_deadline() {
            let store = PausedReadStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::new(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            auth.load(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            store.resume.notify_one();
            timeout(
                Duration::from_secs(1),
                auth.store_request(async {}).unwrap(),
            )
            .await
            .unwrap()
            .unwrap();
            assert!(auth.bearer(Some("client"), 0).is_none());
            auth.disconnect().await.unwrap();
            assert!(store.inner.raw(TOKEN_KEY).is_none());
        }

        #[tokio::test]
        async fn revision8_inaccessible_marker_has_safe_reason_and_can_be_retried() {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("config");
            fs::write(&parent, []).unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), parent.join(LOGOUT_MARKER_FILE));
            auth.initialize(Some("client"), 0).await;
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutIssue"], "markerInaccessible");
            assert!(!status
                .to_string()
                .contains(&parent.to_string_lossy().to_string()));
            assert!(auth.bearer(Some("client"), 0).is_none());
            fs::remove_file(parent).unwrap();
            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        #[tokio::test]
        async fn revision8_startup_retries_pending_logout() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!marker.exists());
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        #[tokio::test]
        async fn unpersistable_token_fails_at_callback() {
            let (context, mut rx) = loopback();
            let body = format!("access_token={}&state=test-state", "+".repeat(2048));
            let (status, _, _) = send(&context, Method::POST, TOKEN_PATH, &body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(matches!(rx.try_recv(), Ok(Err(e)) if matches!(e.code, Code::TokenStore)));
        }

        #[tokio::test]
        async fn wrapped_record_overflow_fails_before_accessing_store() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let old_record = store.raw(TOKEN_KEY);
            let (auth, addr, server) = login_fixture(store.clone()).await;
            auth.load(Some("client"), 0).await;
            store.fail_next_get();
            // The token and inner JSON fit. Escaping those quotes again in the OS
            // adapter's outer JSON makes the saved record too large.
            let token = "%22".repeat(400);
            let (login, status) =
                post_grant_response(auth.clone(), addr, Duration::ZERO, &token, "").await;
            assert_eq!(status, "HTTP/1.1 400 Bad Request\r\n");
            assert!(matches!(
                login.await.unwrap().unwrap_err().code,
                Code::TokenStore
            ));
            assert!(store.get(TOKEN_KEY).await.is_err(), "login read the store");
            assert!(store.raw(TOKEN_KEY) == old_record);
            assert_eq!(auth.user_id().as_deref(), Some("user"));
            server.abort();
        }

        #[tokio::test]
        async fn percent_encoded_expiry_key_is_validated_end_to_end() {
            let body = run_callback_script(PAGE, "http://localhost:47822/twitcasting/callback#access%5Ftoken=t&expires%5Fin=%30&state=test-state");
            let (status, result) = receive_callback_body(body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(matches!(result, Err(e) if matches!(e.code, Code::Other)));
        }

        #[tokio::test]
        async fn duplicate_keys_use_the_last_decoded_value() {
            let base = "http://localhost:47822/twitcasting/callback";
            let body = run_callback_script(
                PAGE,
                &format!("{base}#result=denied&state=stale&st%61te=test-state"),
            );
            assert_fixed_denial(body).await;
            let body = run_callback_script(PAGE, &format!("{base}#result=denied&res%75lt=ok&access_token=old&access%5Ftoken=new&state=test-state"));
            let (status, result) = receive_callback_body(body).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(result.unwrap().token, "new");
        }

        #[tokio::test(start_paused = true)]
        async fn repeated_disconnects_do_not_queue_behind_a_hung_delete() {
            let auth = TwitcastingAuth::new(
                DetachedWriteStore {
                    never_delete: true,
                    ..Default::default()
                },
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            assert!(matches!(
                auth.disconnect().await.unwrap_err().code,
                Code::TokenStore
            ));
            let start = Instant::now();
            for _ in 0..32 {
                assert!(matches!(
                    auth.disconnect().await.unwrap_err().code,
                    Code::TokenStore
                ));
            }
            assert!(start.elapsed() < Duration::from_secs(1));
        }

        #[tokio::test]
        async fn failed_delete_exposes_pending_logout_for_retry() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::new(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            auth.load(Some("client"), 0).await;
            store.fail_next_delete();
            assert!(matches!(
                auth.disconnect().await.unwrap_err().code,
                Code::TokenStore
            ));
            assert_eq!(
                serde_json::to_value(auth.status(Some("client"), 0)).unwrap()["logoutPending"],
                true
            );
            assert!(auth.bearer(Some("client"), 0).is_none());
            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
            assert_ne!(
                serde_json::to_value(auth.status(Some("client"), 0)).unwrap()["logoutPending"],
                true
            );
        }

        #[tokio::test]
        async fn stateless_denial_is_accepted_only_as_a_valid_post() {
            let (context, mut rx) = loopback();
            let uri = format!("{CALLBACK_PATH}?error=access_denied");
            send(&context, Method::GET, &uri, "").await;
            assert!(rx.try_recv().is_err());
            let request = |origin| {
                Request::builder()
                    .method(Method::POST)
                    .uri(TOKEN_PATH)
                    .header("host", "localhost:47822")
                    .header("origin", origin)
                    .body(Body::from("result=denied"))
                    .unwrap()
            };
            assert_eq!(
                handle_callback(&context, request("https://example.test"))
                    .await
                    .status(),
                StatusCode::FORBIDDEN
            );
            assert!(rx.try_recv().is_err());
            assert_eq!(
                handle_callback(&context, request("http://localhost:47822"))
                    .await
                    .status(),
                StatusCode::OK
            );
            assert!(matches!(rx.try_recv(), Ok(Err(e)) if matches!(e.code, Code::Denied)));
        }

        #[tokio::test]
        async fn hanging_put_returns_token_store_before_login_timeout() {
            let store = PausedWriteStore::default();
            let (auth, addr, server) =
                login_fixture_with(store.clone(), Duration::from_millis(450)).await;
            let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
            timeout(Duration::from_secs(2), store.writing.notified())
                .await
                .unwrap();
            let result = timeout(Duration::from_secs(1), login)
                .await
                .expect("store wait did not time out")
                .unwrap();
            assert!(matches!(result.unwrap_err().code, Code::TokenStore));
            store.resume.notify_one();
            auth.load(Some("client"), 0).await;
            assert!(store.inner.raw(TOKEN_KEY).is_none());
            server.abort();
        }

        #[tokio::test]
        async fn logout_does_not_wait_for_cancelled_logins_operation_lock() {
            let store = PausedWriteStore::default();
            let (auth, addr, server) =
                login_fixture_with(store.clone(), Duration::from_millis(500)).await;
            let login = post_grant(auth.clone(), addr, Duration::ZERO, "").await;
            timeout(Duration::from_secs(2), store.writing.notified())
                .await
                .unwrap();
            auth.cancel_login();
            assert!(matches!(
                login.await.unwrap().unwrap_err().code,
                Code::Cancelled
            ));
            let result = timeout(Duration::from_millis(800), auth.disconnect())
                .await
                .expect("logout waited for rollback before timing its own wait");
            assert!(matches!(result.unwrap_err().code, Code::TokenStore));
            store.resume.notify_one();
            server.abort();
        }

        #[derive(Clone, Default)]
        struct PausedReadStore {
            inner: MemorySecretStore,
            resume: Arc<Notify>,
        }

        impl SecretStore for PausedReadStore {
            async fn get(&self, key: &str) -> Result<Option<String>, String> {
                self.resume.notified().await;
                self.inner.get(key).await
            }
            async fn put(&self, key: &str, value: &str) -> Result<(), String> {
                self.inner.put(key, value).await
            }
            async fn delete(&self, key: &str) -> Result<(), String> {
                self.inner.delete(key).await
            }
        }

        #[tokio::test]
        async fn failed_read_can_be_retried() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::new(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            store.fail_next_get();
            auth.load(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            auth.reload(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
        }

        fn auth_with_marker(
            store: MemorySecretStore,
            path: PathBuf,
        ) -> Arc<TwitcastingAuth<MemorySecretStore>> {
            TwitcastingAuth::with_logout_marker(
                store,
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(path),
            )
        }

        #[tokio::test]
        async fn failed_logout_is_not_restored_after_restart() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join("logout-pending");
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            {
                let auth = auth_with_marker(store.clone(), marker.clone());
                auth.initialize(Some("client"), 0).await;
                store.fail_next_delete();
                assert!(auth.disconnect().await.is_err());
            }
            store.fail_next_delete();
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert!(auth.bearer(Some("client"), 0).is_none());
            assert!(marker.exists());
            assert_eq!(
                serde_json::to_value(auth.status(Some("client"), 0)).unwrap()["logoutPending"],
                true
            );
            auth.disconnect().await.unwrap();
            assert!(!marker.exists());
            assert!(store.raw(TOKEN_KEY).is_none());
        }

        #[tokio::test]
        async fn ignored_get_refusals_show_neutral_page() {
            for suffix in ["&state=stale", "&state="] {
                let (context, mut rx) = loopback();
                let uri = format!("{CALLBACK_PATH}?error=access_denied{suffix}");
                let (_, _, page) = send(&context, Method::GET, &uri, "").await;
                assert!(rx.try_recv().is_err());
                assert!(page.contains("このページは閉じてください"));
                assert!(!page.contains("許可されませんでした"));
                assert!(!page.contains("fetch"));
            }
        }
    }
}
