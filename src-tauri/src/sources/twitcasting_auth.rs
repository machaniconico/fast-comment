//! Twitcasting Implicit OAuth. Secrets never cross the status / error boundary.
use crate::integrations::{LogoutValue, SecretStore};
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
    collections::{BTreeSet, HashMap},
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
/// Outside the 44620-48715 range WSL mirrored networking reserves on the Windows side.
pub const CALLBACK_PORT: u16 = 39822;
pub const CALLBACK_PATH: &str = "/twitcasting/callback";
pub const TOKEN_PATH: &str = "/twitcasting/token";
pub(crate) const LOGOUT_MARKER_FILE: &str = "twitcasting-logout-pending";
pub(crate) const CREDENTIAL_LOCK_FILE: &str = "twitcasting-credentials.lock";
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
    /// Counts logouts that removed the stored record, so the UI reports only real deletions.
    #[serde(skip_serializing_if = "is_zero")]
    pub logouts_completed: u64,
}
/// Clears `Memory::listening_port` when the login flow ends on any path.
struct ListeningPort<'a>(&'a Session);
impl Drop for ListeningPort<'_> {
    fn drop(&mut self) {
        if let Ok(mut memory) = self.0.memory.lock() {
            memory.listening_port = None;
        }
    }
}
fn is_zero(n: &u64) -> bool {
    *n == 0
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
                "認証ポートを使用中です。他のログイン待受を閉じてください。WSL の mirrored モードは 44620〜48715 を確保する場合があります。設定でポートを変え、開発者ページのコールバック URL も合わせて登録し直してください"
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum LogoutIssue {
    MarkerUnavailable,
    MarkerInaccessible,
    MarkerCorrupt,
    DeleteFailed,
    MarkerAndDeleteFailed,
    TimedOut,
    // The startup marker check ran out of time; reloading checks again.
    CheckTimedOut,
    OtherLoginKept,
    RecordUnrecognized,
    UnreadableRecordDeleted,
    // Another session held the credential lock past the wait limit.
    LockTimedOut,
}

enum LogoutMarker {
    MemoryOnly,
    Unavailable,
    File {
        path: PathBuf,
        lock: Option<PathBuf>,
    },
}

#[derive(Clone, Copy)]
enum Restore {
    Load,
    Retry,
    // Startup shares one deadline with the marker check before it.
    Startup { deadline: Instant, generation: u64 },
}

enum Logout {
    Empty,
    Preserved(Option<Record>),
}

struct StartupRestore {
    client_id: Option<String>,
    now_ms: u64,
    started: Instant,
    deadline: Instant,
    generation: u64,
}

#[derive(Default)]
struct PendingLogout {
    targets: MarkerTargets,
    // A local record, a marker found by startup/reload, or a conclusive store operation
    // fixes the targets. Reading a marker during explicit logout alone does not.
    known: bool,
    requested_at_ms: Option<u64>,
    deleted: Option<DeletedCredential>,
}

#[derive(Default)]
struct DeletedCredential {
    unreadable: bool,
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
    pending_logout: PendingLogout,
    // The startup marker check has not finished, so login waits for its answer.
    initializing: bool,
    // The startup check failed without reading the marker; reload may check again.
    marker_unchecked: bool,
    logouts_completed: u64,
    // The port a login is listening on, so status reports it even if the config changes meanwhile.
    listening_port: Option<u16>,
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
    /// Keeps the logout intent in memory only (tests and embedders without a config dir policy).
    /// The app uses `with_logout_files`, where `None` means the config dir is missing.
    pub fn new(store: S, endpoints: TwitcastingEndpoints, options: AuthOptions) -> Arc<Self> {
        Self::with_marker(store, endpoints, options, LogoutMarker::MemoryOnly)
    }

    pub(crate) fn with_logout_files(
        store: S,
        endpoints: TwitcastingEndpoints,
        options: AuthOptions,
        path: Option<PathBuf>,
        lock: Option<PathBuf>,
    ) -> Arc<Self> {
        let marker = path.map_or(LogoutMarker::Unavailable, |path| LogoutMarker::File {
            path,
            lock,
        });
        Self::with_marker(store, endpoints, options, marker)
    }

    #[cfg(test)]
    fn with_logout_marker(
        store: S,
        endpoints: TwitcastingEndpoints,
        options: AuthOptions,
        path: Option<PathBuf>,
    ) -> Arc<Self> {
        let lock = path.as_ref().map(|path| path.with_extension("lock"));
        Self::with_logout_files(store, endpoints, options, path, lock)
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

    /// Shows `loading` at once; the returned future finishes the startup check within
    /// one `read_timeout` and can be spawned without blocking app startup.
    pub(crate) fn initialize(
        self: &Arc<Self>,
        client_id: Option<&str>,
        now_ms: u64,
    ) -> impl Future<Output = ()> + Send + 'static {
        let started = Instant::now();
        let generation = {
            let mut memory = self.session.memory.lock().unwrap();
            self.session.begin_check(&mut memory, client_id, now_ms);
            memory.generation
        };
        let auth = self.clone();
        let client_id = client_id.map(str::to_owned);
        async move {
            auth.check_marker(client_id.as_deref(), now_ms, started, generation)
                .await
        }
    }

    async fn check_marker(
        self: &Arc<Self>,
        client_id: Option<&str>,
        now_ms: u64,
        started: Instant,
        generation: u64,
    ) {
        let deadline = started + self.options.read_timeout;
        let check = tokio::time::timeout_at(deadline, self.session.check_startup_marker(deadline))
            .await
            .unwrap_or(Err(LogoutIssue::CheckTimedOut));
        let now = || now_ms.saturating_add(started.elapsed().as_millis() as u64);
        let retry_logout = match check {
            Ok(targets) if targets.is_empty() => {
                self.restore(
                    client_id,
                    now(),
                    Restore::Startup {
                        deadline,
                        generation,
                    },
                )
                .await;
                // A logout could have reached disk between the initial check and the read.
                let memory = self.session.memory.lock().unwrap();
                memory.generation == generation
                    && memory.logout_pending
                    && !memory.logout_running
                    && !memory.pending_logout.targets.is_empty()
            }
            check => {
                let mut memory = self.session.memory.lock().unwrap();
                if !memory.initializing || memory.generation != generation {
                    return;
                }
                memory.initializing = false;
                memory.restore = RestoreState::Idle;
                memory.logout_pending = true;
                let retry = match check {
                    Ok(targets) => {
                        memory.pending_logout.targets.extend(targets);
                        memory.pending_logout.known = true;
                        true
                    }
                    Err(issue) => {
                        memory.logout_issue = Some(issue);
                        memory.marker_unchecked = issue != LogoutIssue::MarkerCorrupt;
                        false
                    }
                };
                self.session.notify(&memory, client_id, now());
                retry
            }
        };
        if retry_logout {
            let _ = self
                .logout(Some(StartupRestore {
                    client_id: client_id.map(str::to_owned),
                    now_ms,
                    started,
                    deadline,
                    generation,
                }))
                .await;
        }
    }

    /// Retries a failed read, or a startup marker check that could not read the marker.
    pub(crate) async fn reload(self: &Arc<Self>, client_id: Option<&str>, now_ms: u64) {
        let started = Instant::now();
        let recheck = {
            let mut memory = self.session.memory.lock().unwrap();
            if memory.marker_unchecked && !memory.logout_running && !memory.initializing {
                // One lock: login must never see the pending flag cleared before the check starts.
                memory.marker_unchecked = false;
                memory.logout_pending = false;
                memory.logout_issue = None;
                self.session.begin_check(&mut memory, client_id, now_ms);
                Some(memory.generation)
            } else {
                None
            }
        };
        if let Some(generation) = recheck {
            self.check_marker(client_id, now_ms, started, generation)
                .await;
        } else {
            self.restore(client_id, now_ms, Restore::Retry).await;
        }
    }

    pub async fn load(&self, client_id: Option<&str>, now_ms: u64) {
        self.restore(client_id, now_ms, Restore::Load).await;
    }

    async fn restore(&self, client_id: Option<&str>, now_ms: u64, mode: Restore) {
        let observed_at = Instant::now();
        let deadline = match mode {
            Restore::Startup { deadline, .. } => deadline,
            _ => observed_at + self.options.read_timeout,
        };
        let session = self.session.clone();
        let client_id = client_id.map(str::to_owned);
        let (generation, read) = {
            let mut memory = session.memory.lock().unwrap();
            let startup = matches!(mode, Restore::Startup { .. });
            if let Restore::Startup { generation, .. } = mode {
                if !memory.initializing || memory.generation != generation {
                    return;
                }
                memory.initializing = false;
                if matches!(memory.restore, RestoreState::Loading) {
                    memory.restore = RestoreState::Idle;
                }
            }
            if memory.logout_pending
                || memory.unauthorized
                || memory.initializing
                || matches!(memory.restore, RestoreState::Loading)
                || (matches!(mode, Restore::Retry)
                    && !matches!(memory.restore, RestoreState::Failed))
            {
                if startup {
                    session.notify(&memory, client_id.as_deref(), now_ms);
                }
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
                let marker = async {
                    let lock = session.lock_credentials_until(deadline).await?;
                    let targets = session.marker_io(read_marker).await?;
                    Ok((lock, targets))
                }
                .await;
                let _lock = match marker {
                    Ok((lock, targets)) if targets.is_empty() => lock,
                    marker => {
                        let mut memory = session.memory.lock().unwrap();
                        if Instant::now() < deadline && memory.generation == generation {
                            if memory.record.is_some() {
                                session.replace_record(&mut memory, None, None, 0, Instant::now());
                            }
                            memory.restore = RestoreState::Idle;
                            memory.logout_pending = true;
                            match marker {
                                Ok((_, targets)) => {
                                    memory.pending_logout.targets.extend(targets);
                                    memory.pending_logout.known = true;
                                }
                                Err(issue) => {
                                    memory.logout_issue = Some(issue);
                                    memory.marker_unchecked = issue != LogoutIssue::MarkerCorrupt;
                                }
                            }
                            session.notify(&memory, client_id.as_deref(), now_ms);
                        }
                        return;
                    }
                };
                if Instant::now() >= deadline {
                    return;
                }
                let Ok(value) = store.get(TOKEN_KEY).await else {
                    return;
                };
                let record = value.as_deref().and_then(parse_stored_record);
                let mut memory = session.memory.lock().unwrap();
                if Instant::now() < deadline
                    && memory.generation == generation
                    && !memory.unauthorized
                {
                    if memory.logout_issue == Some(LogoutIssue::RecordUnrecognized) {
                        if value.is_some() && record.is_none() {
                            return;
                        }
                        memory.logout_issue = None;
                    }
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
                session.notify(&memory, client_id.as_deref(), now_ms);
            }
        })
        .await;
    }
    pub fn status(&self, client_id: Option<&str>, now_ms: u64) -> TwitcastingStatus {
        self.status_on(self.options.port, client_id, now_ms)
    }
    /// Like `status`, but reports the callback URL for `port` (the configured listener port).
    pub fn status_on(&self, port: u16, client_id: Option<&str>, now_ms: u64) -> TwitcastingStatus {
        let memory = self.session.memory.lock().unwrap();
        let state = self.session.auth_state(&memory, client_id, now_ms);
        let r = memory.record.as_ref();
        let port = memory.listening_port.unwrap_or(port);
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
            callback_url: format!("http://localhost:{port}{CALLBACK_PATH}"),
            logout_pending: memory.logout_pending,
            logout_issue: memory.logout_issue,
            logouts_completed: memory.logouts_completed,
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
        self.logout(None).await
    }

    async fn logout(
        self: &Arc<Self>,
        startup: Option<StartupRestore>,
    ) -> Result<(), TwitcastingAuthError> {
        let deadline = startup.as_ref().map_or_else(
            || Instant::now() + self.options.read_timeout,
            |s| s.deadline,
        );
        let (generation, first_request) = {
            let login = self.login.lock().unwrap();
            let mut memory = self.session.memory.lock().unwrap();
            if startup
                .as_ref()
                .is_some_and(|startup| startup.generation != memory.generation)
            {
                return Ok(());
            }
            if memory.logout_running {
                return Err(error(Code::TokenStore));
            }
            // Only the request that fixes the request time may select an unknown current value
            // outright. A retry cannot tell whether that value was written after the first request.
            let first_request =
                startup.is_none() && memory.pending_logout.requested_at_ms.is_none();
            if startup.is_none() {
                memory
                    .pending_logout
                    .requested_at_ms
                    .get_or_insert_with(crate::timer_now_ms);
            }
            if let Some(cancel) = login.as_ref() {
                cancel.cancel();
            }
            if let Some(record) = &memory.record {
                let target = MarkerTarget::Record(record_id(record));
                memory.pending_logout.targets.insert(target);
                memory.pending_logout.known = true;
            }
            memory.initializing = false;
            memory.logout_pending = true;
            memory.logout_running = true;
            memory.marker_unchecked = false;
            self.session
                .replace_record(&mut memory, None, None, 0, Instant::now());
            self.session.notify(&memory, None, 0);
            (memory.generation, first_request)
        };

        // The caller can time out while the marker and store operations finish in the background.
        let auth = self.clone();
        let deletion = tokio::spawn(async move {
            let result = auth
                .remove_credential(startup.is_some(), first_request, deadline)
                .await;
            let mut memory = auth.session.memory.lock().unwrap();
            memory.logout_running = false;
            memory.logout_pending = result.is_err();
            memory.logout_issue = result.as_ref().err().copied();
            if result.is_ok() {
                let pending = std::mem::take(&mut memory.pending_logout);
                if let Some(deleted) = pending.deleted {
                    memory.logouts_completed += 1;
                    if deleted.unreadable {
                        memory.logout_issue = Some(LogoutIssue::UnreadableRecordDeleted);
                    }
                }
            }

            if let Ok(Logout::Preserved(record)) = result {
                // The completed delete is reported through the counter; the reason is this result.
                if record.is_none() {
                    memory.logout_issue = Some(LogoutIssue::RecordUnrecognized);
                }
                if let Some(startup) = startup {
                    let client_id = startup.client_id.as_deref();
                    if record.is_some() && Instant::now() < startup.deadline {
                        auth.session.replace_record(
                            &mut memory,
                            record,
                            client_id,
                            startup.now_ms,
                            startup.started,
                        );
                    } else {
                        // A preserved credential must remain reachable through reload after timeout.
                        memory.restore = RestoreState::Failed;
                    }
                    auth.session.notify(&memory, client_id, startup.now_ms);
                    return Ok(());
                }
                if record.is_some() {
                    memory.logout_issue = Some(LogoutIssue::OtherLoginKept);
                }
                auth.session.notify(&memory, None, 0);
                return Ok(());
            }

            auth.session.notify(&memory, None, 0);
            result.map(|_| ())
        });
        match tokio::time::timeout_at(deadline, deletion).await {
            Ok(Ok(Ok(()))) => Ok(()),
            Ok(_) => Err(error(Code::TokenStore)),
            Err(_) => {
                let mut memory = self.session.memory.lock().unwrap();
                if memory.generation == generation && memory.logout_running {
                    memory.logout_issue.get_or_insert(LogoutIssue::TimedOut);
                    self.session.notify(&memory, None, 0);
                }
                Err(error(Code::TokenStore))
            }
        }
    }

    async fn remove_credential(
        self: &Arc<Self>,
        startup: bool,
        first_request: bool,
        deadline: Instant,
    ) -> Result<Logout, LogoutIssue> {
        let auth = self.clone();
        self.store_request(async move {
            // A job delayed in the queue waits for the lock only for the logout's remaining time.
            let _lock = auth.session.lock_credentials_until(deadline).await?;
            auth.reconcile_logout(startup, first_request).await
        })
        .map_err(|_| LogoutIssue::DeleteFailed)?
        .await
        .map_err(|_| LogoutIssue::DeleteFailed)?
    }

    // The shared file lock covers the marker and the credential together, including any
    // login write and its rollback. A matching credential cannot be replaced before delete.
    async fn reconcile_logout(
        &self,
        startup: bool,
        first_request: bool,
    ) -> Result<Logout, LogoutIssue> {
        let (mut targets, selection_cutoff, deleted) = {
            let memory = self.session.memory.lock().unwrap();
            let pending = &memory.pending_logout;
            let deleted = pending.deleted.is_some();
            let selection_cutoff = pending
                .requested_at_ms
                .filter(|_| !startup && !pending.known && !deleted);
            (pending.targets.clone(), selection_cutoff, deleted)
        };
        // Only the call that set the request time may select unconditionally. Every retry,
        // including one after a first request that ended before reading the store, decides by
        // the request time.
        let first_selection = first_request && selection_cutoff.is_some();
        // Only a wildcard still on disk can authorize a delete.
        targets.remove(&MarkerTarget::Any);
        let requested = targets.clone();
        let snapshot = self
            .session
            .marker_io(move |path| {
                let disk = read_marker(path);
                let issue = if startup || deleted || requested.is_empty() {
                    None
                } else {
                    append_marker_targets(path, &disk, requested).err()
                };
                Ok(Some((disk, issue)))
            })
            .await;
        let (disk, mut marker_issue) = match snapshot {
            Ok(Some((disk, issue))) => (disk, issue),
            Ok(None) => (Ok(MarkerTargets::new()), None),
            Err(issue) => (Err(issue), None),
        };
        match disk {
            Ok(disk) => {
                self.remember_logout_targets(&disk);
                targets.extend(disk);
            }
            Err(issue) if startup => return Err(issue),
            Err(issue) => {
                marker_issue.get_or_insert(issue);
            }
        }
        let store_issue = |marker_issue: Option<LogoutIssue>| {
            if marker_issue.is_some() {
                LogoutIssue::MarkerAndDeleteFailed
            } else {
                LogoutIssue::DeleteFailed
            }
        };

        let current = self.store.get_for_logout(TOKEN_KEY).await;
        if selection_cutoff.is_some() && current.is_ok() {
            self.session.memory.lock().unwrap().pending_logout.known = true;
        }
        let result = match current {
            Ok(None) => Logout::Empty,
            Ok(Some(value)) => {
                let (record, id) = match value {
                    LogoutValue::Decoded(value) => {
                        let record = parse_stored_record(&value);
                        let id = MarkerTarget::for_value(&value, record.as_ref());
                        (record, id)
                    }
                    LogoutValue::MalformedEnvelope(value) => {
                        (None, MarkerTarget::Envelope(value_id(&value)))
                    }
                };
                // On a retry, select a record obtained no later than the original request.
                // A later value may have come from another session. Every session writes a
                // parseable record, so an unparseable value predates any such login.
                // A request time of 0 means the wall clock read before 1970. Records then carry
                // only the elapsed login time, so the times cannot separate sessions; delete
                // rather than leave a logged-out credential to be restored.
                let select_current = first_selection
                    || selection_cutoff.is_some_and(|cutoff| {
                        record
                            .as_ref()
                            .is_none_or(|record| cutoff == 0 || record.obtained_at_ms <= cutoff)
                    });
                if select_current && !targets.contains(&id) {
                    targets.insert(id.clone());
                    self.remember_logout_targets(&targets);
                    let append = targets.clone();
                    marker_issue = self
                        .session
                        .marker_io(move |path| {
                            append_marker_targets(path, &read_marker(path), append)
                        })
                        .await
                        .err();
                }
                if targets.contains(&id) || targets.contains(&MarkerTarget::Any) {
                    self.delete_for_logout(record.is_none())
                        .await
                        .map_err(|()| store_issue(marker_issue))?;
                    Logout::Empty
                } else {
                    Logout::Preserved(record)
                }
            }
            // Only the first request may delete before the current value is identified.
            Err(_) if first_selection => {
                self.delete_for_logout(true)
                    .await
                    .map_err(|()| store_issue(marker_issue))?;
                self.session.memory.lock().unwrap().pending_logout.known = true;
                Logout::Empty
            }
            Err(_) => return Err(store_issue(marker_issue)),
        };

        // All matched entries are gone. Every other identifier in this snapshot was absent.
        // Re-read before removing only the entries this attempt actually reconciled.
        self.session
            .marker_io(move |path| finish_marker(path, &targets, !startup))
            .await?;
        Ok(result)
    }

    fn remember_logout_targets(&self, targets: &MarkerTargets) {
        self.session
            .memory
            .lock()
            .unwrap()
            .pending_logout
            .targets
            .extend(targets.iter().cloned());
    }

    async fn delete_for_logout(&self, unreadable: bool) -> Result<(), ()> {
        if self
            .store
            .delete_if_present(TOKEN_KEY)
            .await
            .map_err(|_| ())?
        {
            let mut memory = self.session.memory.lock().unwrap();
            let deleted = memory.pending_logout.deleted.get_or_insert_default();
            deleted.unreadable |= unreadable;
        }
        Ok(())
    }

    pub async fn connect(
        self: &Arc<Self>,
        client_id: &str,
        now_ms: u64,
        open_browser: impl FnOnce(String) -> Result<(), String>,
    ) -> Result<TwitcastingStatus, TwitcastingAuthError> {
        self.connect_on(self.options.port, client_id, now_ms, open_browser)
            .await
    }
    /// Like `connect`, but listens on `port` (the configured listener port).
    pub async fn connect_on(
        self: &Arc<Self>,
        port: u16,
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
            let memory = self.session.memory.lock().unwrap();
            // Until the startup check answers, a stale logout could delete this login.
            if memory.logout_pending || memory.initializing {
                return Err(error(Code::TokenStore));
            }
            drop(memory);
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
            let ipv4 = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
                .await
                .map_err(|_| error(Code::PortInUse))?;
            let ipv6 =
                match TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)).await {
                    Ok(l) => Some(l),
                    Err(e)
                        if e.kind() == std::io::ErrorKind::AddrNotAvailable
                            || matches!(e.raw_os_error(), Some(10047 | 97)) =>
                    {
                        None
                    }
                    Err(_) => return Err(error(Code::PortInUse)),
                };
            // Logins run one at a time under `operation`, so this login owns the field until it ends.
            self.session.memory.lock().unwrap().listening_port = Some(port);
            let _listening = ListeningPort(&self.session);
            let mut random = [0u8; 32];
            getrandom::getrandom(&mut random).map_err(|_| error(Code::Other))?;
            let state = URL_SAFE_NO_PAD.encode(random);
            let (tx, mut rx) = mpsc::channel(1);
            let context = Arc::new(Loopback {
                state: state.clone(),
                port,
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
        let store_deadline = Instant::now() + self.options.read_timeout;
        let lock_deadline = store_deadline.min(deadline);
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
                let Ok(_lock) = session.lock_credentials_until(lock_deadline).await else {
                    let _ = written.send(false);
                    return;
                };
                if written.is_closed() {
                    return;
                }
                // A legacy wildcard must be resolved before publishing a new login.
                if !session
                    .marker_io(read_marker)
                    .await
                    .is_ok_and(|targets| !targets.contains(&MarkerTarget::Any))
                {
                    let _ = written.send(false);
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
            _ = tokio::time::sleep_until(store_deadline) => return Err(error(Code::TokenStore)),
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
            memory.logout_issue = None;
            self.session.replace_record(
                &mut memory,
                Some(record),
                Some(client_id),
                obtained_at_ms,
                received_at,
            );
            let _ = commit.send(());
        }
        Ok(self.status_on(port, Some(client_id), obtained_at_ms))
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
    fn begin_check(&self, memory: &mut Memory, client_id: Option<&str>, now_ms: u64) {
        memory.initializing = true;
        memory.restore = RestoreState::Loading;
        self.notify(memory, client_id, now_ms);
    }

    /// Notifies subscribers even when only the logout or load progress changed.
    fn notify(&self, memory: &Memory, client_id: Option<&str>, now_ms: u64) {
        self.auth_state(memory, client_id, now_ms);
        self.changes.send_modify(|_| {});
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

    async fn check_startup_marker(&self, deadline: Instant) -> Result<MarkerTargets, LogoutIssue> {
        let lock = self.credential_lock_path()?.map(Path::to_owned);
        self.marker_io(move |path| {
            // This task owns the lock even if the startup caller stops waiting.
            let _lock = lock
                .as_deref()
                .map(|lock| lock_credential_file(lock, deadline))
                .transpose()?;
            cleanup_marker_temps(path);
            read_marker(path)
        })
        .await
    }

    fn credential_lock_path(&self) -> Result<Option<&Path>, LogoutIssue> {
        match &self.logout_marker {
            LogoutMarker::MemoryOnly => Ok(None),
            LogoutMarker::Unavailable => Err(LogoutIssue::MarkerUnavailable),
            LogoutMarker::File { lock, .. } => lock
                .as_deref()
                .map(Some)
                .ok_or(LogoutIssue::MarkerUnavailable),
        }
    }

    async fn lock_credentials_until(
        &self,
        deadline: Instant,
    ) -> Result<Option<fs::File>, LogoutIssue> {
        let Some(path) = self.credential_lock_path()?.map(Path::to_owned) else {
            return Ok(None);
        };
        tokio::task::spawn_blocking(move || lock_credential_file(&path, deadline))
            .await
            .unwrap_or(Err(LogoutIssue::MarkerInaccessible))
            .map(Some)
    }

    async fn marker_io<T: Default + Send + 'static>(
        &self,
        operation: impl FnOnce(&Path) -> Result<T, LogoutIssue> + Send + 'static,
    ) -> Result<T, LogoutIssue> {
        match &self.logout_marker {
            LogoutMarker::MemoryOnly => Ok(T::default()),
            LogoutMarker::Unavailable => Err(LogoutIssue::MarkerUnavailable),
            LogoutMarker::File { path, .. } => {
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

const LOCK_RETRY_INTERVAL: Duration = Duration::from_millis(20);

fn lock_credential_file(path: &Path, deadline: Instant) -> Result<fs::File, LogoutIssue> {
    let parent = path.parent().ok_or(LogoutIssue::MarkerInaccessible)?;
    fs::create_dir_all(parent).map_err(|_| LogoutIssue::MarkerInaccessible)?;
    let mut options = fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // Let every session open the lock file while preventing replacement.
        options.share_mode(3); // FILE_SHARE_READ | FILE_SHARE_WRITE
    }
    let file = options
        .open(path)
        .map_err(|_| LogoutIssue::MarkerInaccessible)?;
    // Another session may hold the lock indefinitely; give up instead of blocking the queue.
    let deadline = deadline.into_std();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(fs::TryLockError::WouldBlock) => {
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return Err(LogoutIssue::LockTimedOut);
                }
                std::thread::sleep(LOCK_RETRY_INTERVAL.min(remaining));
            }
            Err(fs::TryLockError::Error(_)) => return Err(LogoutIssue::MarkerInaccessible),
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

/// What a logout marker allows a later run to delete.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
enum MarkerTarget {
    Any,
    Record(String),
    Raw(String),
    Envelope(String),
}

type MarkerTargets = BTreeSet<MarkerTarget>;

impl MarkerTarget {
    fn for_value(value: &str, record: Option<&Record>) -> Self {
        record.map_or_else(
            || Self::Raw(value_id(value)),
            |record| Self::Record(record_id(record)),
        )
    }

    fn parse(line: &str) -> Result<Self, LogoutIssue> {
        if line == "*" {
            return Ok(Self::Any);
        }
        let (id, make): (_, fn(String) -> Self) = if let Some(id) = line.strip_prefix("raw:") {
            (id, Self::Raw)
        } else if let Some(id) = line.strip_prefix("envelope:") {
            (id, Self::Envelope)
        } else {
            (line, Self::Record)
        };
        if id.len() != 64 || !id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
            return Err(LogoutIssue::MarkerCorrupt);
        }
        Ok(make(id.to_owned()))
    }
}

fn value_id(value: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(value))
}

fn record_id(record: &Record) -> String {
    value_id(&serde_json::to_string(record).unwrap_or_default())
}

// A marker larger than this is not ours; it is treated as corrupt without reading it.
const MAX_MARKER_BYTES: u64 = 64 * 1024;

fn read_marker(path: &Path) -> Result<MarkerTargets, LogoutIssue> {
    let Some(metadata) = inspect_marker(path)? else {
        return Ok(MarkerTargets::new());
    };
    if !metadata.is_file() || metadata.len() > MAX_MARKER_BYTES {
        return Err(LogoutIssue::MarkerCorrupt);
    }
    let mut content = Vec::new();
    let read = fs::File::open(path).and_then(|file| {
        use io::Read;
        file.take(MAX_MARKER_BYTES + 1).read_to_end(&mut content)
    });
    match read {
        Ok(_) if content.len() as u64 > MAX_MARKER_BYTES => return Err(LogoutIssue::MarkerCorrupt),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(MarkerTargets::new()),
        Err(_) => return Err(LogoutIssue::MarkerInaccessible),
    }
    if content.is_empty() {
        return Ok(MarkerTargets::from([MarkerTarget::Any]));
    }
    let content = std::str::from_utf8(&content).map_err(|_| LogoutIssue::MarkerCorrupt)?;
    let content = content.trim();
    if content.is_empty() {
        return Err(LogoutIssue::MarkerCorrupt);
    }
    let targets = content
        .lines()
        .map(MarkerTarget::parse)
        .collect::<Result<MarkerTargets, _>>()?;
    if targets.len() == 1 && targets.contains(&MarkerTarget::Any) {
        return Err(LogoutIssue::MarkerCorrupt);
    }
    Ok(targets)
}

fn append_marker_targets(
    path: &Path,
    disk: &Result<MarkerTargets, LogoutIssue>,
    mut targets: MarkerTargets,
) -> Result<(), LogoutIssue> {
    match disk {
        Ok(disk) => {
            targets.extend(disk.iter().cloned());
            if targets == *disk {
                return Ok(());
            }
        }
        Err(LogoutIssue::MarkerCorrupt) => {
            if inspect_marker(path)?.is_some_and(|metadata| !metadata.is_file()) {
                clear_logout_marker(path)?;
            }
        }
        Err(issue) => return Err(*issue),
    }
    write_marker(path, &targets)
}

fn finish_marker(path: &Path, resolved: &MarkerTargets, explicit: bool) -> Result<(), LogoutIssue> {
    let remaining = match read_marker(path) {
        Ok(disk) if disk.is_empty() => return Ok(()),
        Ok(disk) => disk
            .difference(resolved)
            .cloned()
            .collect::<MarkerTargets>(),
        Err(LogoutIssue::MarkerCorrupt) if explicit => return clear_logout_marker(path),
        Err(issue) => return Err(issue),
    };
    if remaining.is_empty() {
        clear_logout_marker(path)
    } else {
        write_marker(path, &remaining)?;
        Err(LogoutIssue::DeleteFailed)
    }
}

const MARKER_TEMP_PREFIX: &str = ".twitcasting-logout-pending.";

fn cleanup_marker_temps(path: &Path) {
    let Some(parent) = path.parent() else { return };
    let Ok(entries) = fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_temp = name
            .to_str()
            .and_then(|name| name.strip_prefix(MARKER_TEMP_PREFIX))
            .and_then(|name| name.strip_suffix(".tmp"))
            .is_some_and(|id| id.len() == 36 && uuid::Uuid::parse_str(id).is_ok());
        if is_temp && entry.file_type().is_ok_and(|kind| kind.is_file()) {
            let _ = fs::remove_file(entry.path());
        }
    }
}

fn write_marker(path: &Path, targets: &MarkerTargets) -> Result<(), LogoutIssue> {
    use io::Write;

    let parent = path.parent().ok_or(LogoutIssue::MarkerInaccessible)?;
    fs::create_dir_all(parent).map_err(|_| LogoutIssue::MarkerInaccessible)?;
    // Publish only a complete marker. A partial write must not look like the legacy wildcard.
    let temp = parent.join(format!("{MARKER_TEMP_PREFIX}{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut output = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        for target in targets {
            match target {
                MarkerTarget::Any if targets.len() == 1 => {}
                MarkerTarget::Any => writeln!(output, "*")?,
                MarkerTarget::Record(id) => writeln!(output, "{id}")?,
                MarkerTarget::Raw(id) => writeln!(output, "raw:{id}")?,
                MarkerTarget::Envelope(id) => writeln!(output, "envelope:{id}")?,
            }
        }
        output.sync_all()?;
        drop(output);
        fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp);
    }
    result.map_err(|_| LogoutIssue::MarkerInaccessible)
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

    #[test]
    fn default_callback_port_is_outside_wsl_mirrored_reservation() {
        assert_eq!(CALLBACK_PORT, 39822);
        assert!(!(44620..=48715).contains(&CALLBACK_PORT));
        assert_eq!(AuthOptions::default().port, CALLBACK_PORT);
        let auth = TwitcastingAuth::new(
            MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        assert_eq!(
            auth.status(Some("client"), 0).callback_url,
            format!("http://localhost:{CALLBACK_PORT}{CALLBACK_PATH}")
        );
        assert_eq!(
            auth.status_on(40123, Some("client"), 0).callback_url,
            format!("http://localhost:40123{CALLBACK_PATH}")
        );
    }

    /// A free loopback port other than the default (so a pass proves the configured port was used).
    async fn free_non_default_port() -> u16 {
        loop {
            let probe = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = probe.local_addr().unwrap().port();
            if port != CALLBACK_PORT {
                return port;
            }
        }
    }

    #[tokio::test]
    async fn connect_on_listens_on_the_configured_port() {
        let auth = TwitcastingAuth::new(
            MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        auth.load(Some("client"), 0).await;
        // The probe is released before `connect_on` binds, so another process may take the port
        // in between; only that race (PortInUse) is retried.
        for _ in 0..5 {
            let port = free_non_default_port().await;
            let reached = Arc::new(Mutex::new(None));
            let seen = reached.clone();
            let result = auth
                .connect_on(port, "client", 0, move |_| {
                    *seen.lock().unwrap() =
                        Some(std::net::TcpStream::connect(("127.0.0.1", port)).is_ok());
                    Err("stop".into())
                })
                .await;
            if matches!(&result, Err(e) if matches!(e.code, Code::PortInUse)) {
                continue;
            }
            assert!(matches!(result, Err(e) if matches!(e.code, Code::Other)));
            assert_eq!(*reached.lock().unwrap(), Some(true));
            // The listener is released when the command returns.
            TcpListener::bind(("127.0.0.1", port)).await.unwrap();
            return;
        }
        panic!("no free port could be bound");
    }

    #[tokio::test]
    async fn status_during_login_reports_the_listening_port() {
        let auth = TwitcastingAuth::new(
            MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        auth.load(Some("client"), 0).await;
        for _ in 0..5 {
            let port = free_non_default_port().await;
            let configured = if port == 40123 { 40124 } else { 40123 };
            let during = Arc::new(Mutex::new(None));
            let seen = during.clone();
            let observer = auth.clone();
            let result = auth
                .connect_on(port, "client", 0, move |_| {
                    // The config changed elsewhere while the login is listening.
                    *seen.lock().unwrap() =
                        Some(observer.status_on(configured, Some("client"), 0).callback_url);
                    Err("stop".into())
                })
                .await;
            if matches!(&result, Err(e) if matches!(e.code, Code::PortInUse)) {
                continue;
            }
            assert_eq!(
                during.lock().unwrap().as_deref(),
                Some(format!("http://localhost:{port}{CALLBACK_PATH}").as_str())
            );
            // After the login ends, the configured port is reported again.
            assert_eq!(
                auth.status_on(configured, Some("client"), 0).callback_url,
                format!("http://localhost:{configured}{CALLBACK_PATH}")
            );
            return;
        }
        panic!("no free port could be bound");
    }

    #[tokio::test]
    async fn connect_on_reports_port_in_use_with_wsl_hint() {
        let busy = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = busy.local_addr().unwrap().port();
        let auth = TwitcastingAuth::new(
            MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        auth.load(Some("client"), 0).await;
        let err = auth
            .connect_on(port, "client", 0, |_| Ok(()))
            .await
            .unwrap_err();
        assert!(matches!(err.code, Code::PortInUse));
        assert!(err.message.contains("44620〜48715"), "{}", err.message);
        drop(busy);
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
                let store = MemorySecretStore::new();
                seed_record(&store, None).await;
                let auth = TwitcastingAuth::with_logout_marker(
                    store,
                    TwitcastingEndpoints::default(),
                    AuthOptions {
                        read_timeout: Duration::from_millis(50),
                        ..AuthOptions::default()
                    },
                    Some(dir.path().join(LOGOUT_MARKER_FILE)),
                );
                auth.load(Some("client"), 0).await;
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
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store, dir.path().join(LOGOUT_MARKER_FILE));
            auth.load(Some("client"), 0).await;
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
            store.resume.notify_one();
            auth.reload(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
            store.resume.notify_one();
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
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(parent.join(LOGOUT_MARKER_FILE)),
                Some(dir.path().join("credentials.lock")),
            );
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
            let body = run_callback_script(PAGE, &format!("http://localhost:{CALLBACK_PORT}/twitcasting/callback#access%5Ftoken=t&expires%5Fin=%30&state=test-state"));
            let (status, result) = receive_callback_body(body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
            assert!(matches!(result, Err(e) if matches!(e.code, Code::Other)));
        }

        #[tokio::test]
        async fn duplicate_keys_use_the_last_decoded_value() {
            let base = format!("http://localhost:{CALLBACK_PORT}/twitcasting/callback");
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
            let store = DetachedWriteStore {
                never_delete: true,
                ..Default::default()
            };
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::new(
                store,
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
                    .header("host", format!("localhost:{CALLBACK_PORT}"))
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
                handle_callback(&context, request(format!("http://localhost:{CALLBACK_PORT}").as_str()))
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

        #[derive(Clone, Default)]
        struct GatedDeleteStore {
            inner: MemorySecretStore,
            deleting: Arc<Notify>,
            resume: Arc<Notify>,
        }

        impl SecretStore for GatedDeleteStore {
            async fn get(&self, key: &str) -> Result<Option<String>, String> {
                self.inner.get(key).await
            }
            async fn put(&self, key: &str, value: &str) -> Result<(), String> {
                self.inner.put(key, value).await
            }
            async fn delete(&self, key: &str) -> Result<(), String> {
                self.deleting.notify_one();
                self.resume.notified().await;
                self.inner.delete(key).await
            }
        }

        fn short_options(read_timeout_ms: u64) -> AuthOptions {
            AuthOptions {
                read_timeout: Duration::from_millis(read_timeout_ms),
                ..AuthOptions::default()
            }
        }

        fn set_hook(
            auth: &TwitcastingAuth<impl SecretStore>,
            hook: impl Fn(usize) + Send + Sync + 'static,
        ) {
            use std::sync::atomic::{AtomicUsize, Ordering};
            let calls = AtomicUsize::new(0);
            *auth.session.marker_io_hook.lock().unwrap() =
                Some(Arc::new(move || hook(calls.fetch_add(1, Ordering::SeqCst))));
        }

        async fn wait_until(mut done: impl FnMut() -> bool) {
            timeout(Duration::from_secs(3), async {
                while !done() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
        }

        // 修正9-1: 別プロセスが削除・再ログインした後で、古い起動時再試行がそれを消さない。
        #[tokio::test]
        async fn revision9_startup_retry_rechecks_marker_before_deleting() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let store = MemorySecretStore::new();
            // 別プロセスが再ログインして保存した値の代わり。
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());
            let removed = marker.clone();
            set_hook(&auth, move |call| {
                if call == 1 {
                    // 起動時の検出後、削除前に別プロセスがログアウトを終えた。
                    let _ = fs::remove_file(&removed);
                }
            });
            auth.initialize(Some("client"), 0).await;
            assert!(
                store.raw(TOKEN_KEY).is_some(),
                "別プロセスの新しいログインを消した"
            );
            assert!(!marker.exists(), "マーカーを作り直した");
            wait_until(|| auth.status(Some("client"), 0).state == "ready").await;
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        // 修正9-2: マーカー検査にかかった時間を期限判定に含める。
        #[tokio::test]
        async fn revision9_marker_check_time_counts_toward_expiry() {
            let dir = tempfile::tempdir().unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, Some(150)).await;
            let auth = auth_with_marker(store, dir.path().join(LOGOUT_MARKER_FILE));
            set_hook(&auth, |_| std::thread::sleep(Duration::from_millis(250)));
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "reloginRequired");
        }

        // 修正9-3: 起動処理は呼び出し時点で loading になり、待ちは合計 read_timeout 以内。
        #[tokio::test]
        async fn revision9_initialize_is_loading_at_once_and_bounded_by_one_read_timeout() {
            let dir = tempfile::tempdir().unwrap();
            let store = PausedReadStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(300),
                Some(dir.path().join(LOGOUT_MARKER_FILE)),
            );
            set_hook(&auth, |_| std::thread::sleep(Duration::from_millis(200)));
            let start = Instant::now();
            let init = auth.initialize(Some("client"), 0);
            assert_eq!(auth.status(Some("client"), 0).state, "loading");
            init.await;
            assert!(
                start.elapsed() < Duration::from_millis(450),
                "{:?}",
                start.elapsed()
            );
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            store.resume.notify_one();
        }

        // 修正9-4: 起動時検査の時間切れは権限エラーと区別し、再読み込みで検査し直せる。
        #[tokio::test]
        async fn revision9_startup_check_timeout_is_distinct_and_reload_rechecks() {
            let dir = tempfile::tempdir().unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(100),
                Some(dir.path().join(LOGOUT_MARKER_FILE)),
            );
            set_hook(&auth, |call| {
                if call == 0 {
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            auth.initialize(Some("client"), 0).await;
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutIssue"], "checkTimedOut");
            assert_eq!(status["logoutPending"], true);
            auth.reload(Some("client"), 0).await;
            let status = auth.status(Some("client"), 0);
            assert_eq!(status.state, "ready");
            assert!(!status.logout_pending);
            assert!(store.raw(TOKEN_KEY).is_some());
        }

        // 修正9-5: 状態が同じでもログアウト保留の変化は購読者に通知する。
        #[tokio::test]
        async fn revision9_logout_pending_changes_notify_subscribers() {
            let store = MemorySecretStore::new();
            let auth = TwitcastingAuth::new(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
            );
            let mut rx = auth.subscribe();
            rx.mark_unchanged();
            auth.disconnect().await.unwrap();
            assert!(rx.has_changed().unwrap(), "ログアウト完了が通知されない");
            store.fail_next_get();
            rx.mark_unchanged();
            auth.load(Some("client"), 0).await;
            assert!(rx.has_changed().unwrap());
            assert_eq!(*rx.borrow_and_update(), AuthState::LoadFailed);
        }

        // 修正9-9: マーカーの記録と資格情報の削除が両方失敗したら、両方が分かる理由にする。
        #[tokio::test]
        async fn revision9_marker_and_delete_failures_are_both_reported() {
            let dir = tempfile::tempdir().unwrap();
            let parent = dir.path().join("config");
            fs::write(&parent, []).unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(parent.join(LOGOUT_MARKER_FILE)),
                Some(dir.path().join("credentials.lock")),
            );
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutIssue"], "markerAndDeleteFailed");
            assert_eq!(status["logoutPending"], true);
        }

        // 修正9-10a: 時間切れした起動時再試行が続く間の手動再試行は拒否し、重複登録しない。
        #[tokio::test]
        async fn revision9_manual_retry_during_timed_out_startup_retry_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let store = GatedDeleteStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(100),
                Some(marker.clone()),
            );
            auth.initialize(Some("client"), 0).await;
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutPending"], true);
            assert_eq!(status["logoutIssue"], "timedOut");
            let start = Instant::now();
            assert!(matches!(
                auth.disconnect().await.unwrap_err().code,
                Code::TokenStore
            ));
            assert!(start.elapsed() < Duration::from_millis(50));
            assert!(auth.status(Some("client"), 0).logout_pending);
            store.resume.notify_one();
        }

        // 修正9-10b: 背景のログアウト完了後は、通知が届き、手動再試行も通る。
        #[tokio::test]
        async fn revision9_manual_retry_after_background_completion_succeeds() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let store = GatedDeleteStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(100),
                Some(marker.clone()),
            );
            auth.initialize(Some("client"), 0).await;
            assert!(auth.status(Some("client"), 0).logout_pending);
            let mut rx = auth.subscribe();
            rx.mark_unchanged();
            store.resume.notify_one();
            timeout(Duration::from_secs(3), rx.changed())
                .await
                .expect("背景のログアウト完了が通知されない")
                .unwrap();
            wait_until(|| !auth.status(Some("client"), 0).logout_pending).await;
            assert!(!marker.exists());
            assert!(store.inner.raw(TOKEN_KEY).is_none());
            store.resume.notify_one();
            auth.disconnect().await.unwrap();
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        // 修正9-10c: 設定フォルダーが無い時は、起動 → 再試行の後も保留のまま。
        #[tokio::test]
        async fn revision9_unavailable_marker_stays_pending_after_retry() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                None,
            );
            auth.initialize(Some("client"), 0).await;
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutIssue"], "markerUnavailable");
            assert!(auth.bearer(Some("client"), 0).is_none());
            assert!(auth.disconnect().await.is_err());
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutPending"], true);
            assert_eq!(status["logoutIssue"], "markerUnavailable");
        }

        fn seeded(access_token: &str) -> Record {
            Record {
                client_id: "client".into(),
                access_token: access_token.into(),
                user_id: "user".into(),
                screen_id: "screen".into(),
                name: "name".into(),
                expires_at_ms: None,
                obtained_at_ms: 0,
            }
        }

        fn id_of(record: &Record) -> String {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(serde_json::to_string(record).unwrap()))
        }

        fn marked_auth(
            store: MemorySecretStore,
            content: &str,
            read_timeout_ms: u64,
        ) -> (
            tempfile::TempDir,
            PathBuf,
            Arc<TwitcastingAuth<MemorySecretStore>>,
        ) {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, content).unwrap();
            let auth = TwitcastingAuth::with_logout_marker(
                store,
                TwitcastingEndpoints::default(),
                short_options(read_timeout_ms),
                Some(marker.clone()),
            );
            (dir, marker, auth)
        }

        // 修正10-1: 別プロセスの新しいログインは、マーカーの識別子と一致しないので消さない。
        #[tokio::test]
        async fn revision10_startup_retry_keeps_a_login_the_marker_does_not_name() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let (_dir, marker, auth) =
                marked_auth(store.clone(), &id_of(&seeded("old-token")), 5000);
            auth.initialize(Some("client"), 0).await;
            assert!(store.raw(TOKEN_KEY).is_some(), "別のログインを削除した");
            assert!(!marker.exists());
            wait_until(|| auth.status(Some("client"), 0).state == "ready").await;
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        // 修正10-1: 識別子が一致する記録は起動時に削除する。
        #[tokio::test]
        async fn revision10_startup_retry_deletes_the_named_login() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let (_dir, marker, auth) =
                marked_auth(store.clone(), &id_of(&seeded("test-token")), 5000);
            auth.initialize(Some("client"), 0).await;
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!marker.exists());
            assert!(!auth.status(Some("client"), 0).logout_pending);
        }

        // 修正10-1: 手動ログアウトのマーカーには、削除する記録の識別子(トークンではない)を書く。
        #[tokio::test]
        async fn revision10_manual_logout_marker_names_the_record() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            let content = fs::read_to_string(&marker).unwrap();
            assert_eq!(content.trim(), id_of(&seeded("test-token")));
            assert!(!content.contains("test-token"));
        }

        // 修正10-4: 削除直前の再確認が失敗したら削除せず保留に戻す。
        #[tokio::test]
        async fn revision10_recheck_error_keeps_the_record_and_stays_pending() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().join("config");
            fs::create_dir(&root).unwrap();
            let marker = root.join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(marker),
                Some(dir.path().join("credentials.lock")),
            );
            let config = root.clone();
            set_hook(&auth, move |call| {
                if call == 1 {
                    // 設定フォルダーの位置にファイルが置かれ、再確認できなくなった。
                    fs::remove_dir_all(&config).unwrap();
                    fs::write(&config, []).unwrap();
                }
            });
            let mut rx = auth.subscribe();
            let init = auth.initialize(Some("client"), 0);
            rx.mark_unchanged();
            init.await;
            assert!(store.raw(TOKEN_KEY).is_some(), "再確認できないのに削除した");
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutPending"], true);
            assert_eq!(status["logoutIssue"], "markerInaccessible");
            assert!(rx.has_changed().unwrap());
            fs::remove_file(root).unwrap();
        }

        // 修正10-4: マーカーありの起動時再試行も、検査と合わせて read_timeout 1 回に収める。
        #[tokio::test]
        async fn revision10_startup_retry_shares_the_initialize_deadline() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, []).unwrap();
            let store = GatedDeleteStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(300),
                Some(marker),
            );
            set_hook(&auth, |call| {
                if call == 0 {
                    std::thread::sleep(Duration::from_millis(200));
                }
            });
            let start = Instant::now();
            auth.initialize(Some("client"), 0).await;
            assert!(
                start.elapsed() < Duration::from_millis(450),
                "{:?}",
                start.elapsed()
            );
            assert!(auth.status(Some("client"), 0).logout_pending);
            store.resume.notify_one();
        }

        // 修正10-7: 削除を伴うログアウトの完了だけを数え、削除しなかった経路では数えない。
        #[tokio::test]
        async fn revision10_only_completed_deletions_are_counted() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let (_dir, _marker, auth) =
                marked_auth(store.clone(), &id_of(&seeded("old-token")), 5000);
            auth.initialize(Some("client"), 0).await;
            let count = |auth: &TwitcastingAuth<MemorySecretStore>| {
                serde_json::to_value(auth.status(Some("client"), 0)).unwrap()["logoutsCompleted"]
                    .as_u64()
                    .unwrap_or(0)
            };
            assert_eq!(count(&auth), 0);
            auth.disconnect().await.unwrap();
            assert_eq!(count(&auth), 1);
        }

        #[tokio::test]
        async fn revision12_stale_session_cannot_erase_another_logout_intent() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let old = auth_with_marker(store.clone(), marker.clone());
            old.initialize(Some("client"), 0).await;

            let new_value = serde_json::to_string(&seeded("new-token")).unwrap();
            store.put(TOKEN_KEY, &new_value).await.unwrap();
            let new = auth_with_marker(store.clone(), marker.clone());
            new.initialize(Some("client"), 0).await;
            store.fail_next_delete();
            assert!(new.disconnect().await.is_err());
            drop(new);

            store.fail_next_delete();
            assert!(old.disconnect().await.is_err());
            let pending = fs::read_to_string(&marker).unwrap();
            assert!(pending.lines().any(|id| id == id_of(&seeded("new-token"))));
            assert!(pending.lines().any(|id| id == id_of(&seeded("test-token"))));
            drop(old);

            let restarted = auth_with_marker(store.clone(), marker.clone());
            restarted.initialize(Some("client"), 0).await;
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!marker.exists());
        }

        #[tokio::test]
        async fn revision12_startup_retains_a_target_after_recheck_fails() {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config");
            fs::create_dir(&config).unwrap();
            let marker = config.join(LOGOUT_MARKER_FILE);
            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(marker.clone()),
                Some(root.path().join("credentials.lock")),
            );
            set_hook(&auth, {
                let config = config.clone();
                move |call| {
                    if call == 1 {
                        fs::remove_dir_all(&config).unwrap();
                        fs::write(&config, []).unwrap();
                    }
                }
            });
            auth.initialize(Some("client"), 0).await;
            assert!(auth.status(Some("client"), 0).logout_pending);
            assert_eq!(
                auth.status(Some("client"), 0).logout_issue,
                Some(LogoutIssue::MarkerInaccessible)
            );

            fs::remove_file(config).unwrap();
            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_some());
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 0);
        }

        #[cfg(windows)]
        fn deny_marker_replacement(path: &Path) -> fs::File {
            use std::os::windows::fs::OpenOptionsExt;
            // Share reads and writes but deny delete/rename until this handle is closed.
            fs::OpenOptions::new()
                .read(true)
                .share_mode(3)
                .open(path)
                .unwrap()
        }

        #[cfg(windows)]
        #[tokio::test]
        async fn revision12_marker_cleanup_retry_reports_the_completed_delete_once() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let (_dir, marker, auth) = marked_auth(store.clone(), "", 5000);
            let held = Arc::new(Mutex::new(None));
            store.fail_next_delete();
            auth.initialize(Some("client"), 0).await;
            assert!(auth.status(Some("client"), 0).logout_pending);
            set_hook(&auth, {
                let held = held.clone();
                let marker = marker.clone();
                move |call| {
                    if call == 1 {
                        *held.lock().unwrap() = Some(deny_marker_replacement(&marker));
                    }
                }
            });
            assert!(auth.disconnect().await.is_err());
            assert!(store.raw(TOKEN_KEY).is_none());
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 0);
            drop(held.lock().unwrap().take());

            auth.disconnect().await.unwrap();
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 1);
            auth.disconnect().await.unwrap();
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 1);
        }

        #[derive(Clone, Default)]
        struct UnreadableStore(MemorySecretStore);

        impl SecretStore for UnreadableStore {
            async fn get(&self, key: &str) -> Result<Option<String>, String> {
                if self.0.raw(key).as_deref() == Some("invalid envelope") {
                    return Err("資格情報を読めません".into());
                }
                self.0.get(key).await
            }

            async fn put(&self, key: &str, value: &str) -> Result<(), String> {
                self.0.put(key, value).await
            }

            async fn delete(&self, key: &str) -> Result<(), String> {
                self.0.delete(key).await
            }
        }

        #[tokio::test]
        async fn revision12_explicit_logout_can_delete_an_unreadable_credential() {
            let dir = tempfile::tempdir().unwrap();
            let store = UnreadableStore::default();
            store.put(TOKEN_KEY, "invalid envelope").await.unwrap();
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(dir.path().join(LOGOUT_MARKER_FILE)),
            );
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            auth.disconnect().await.unwrap();
            assert!(store.0.raw(TOKEN_KEY).is_none());
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(status.logouts_completed, 1);
            assert_eq!(
                serde_json::to_value(status).unwrap()["logoutIssue"],
                "unreadableRecordDeleted"
            );
        }

        #[cfg(windows)]
        #[tokio::test]
        async fn revision12_append_failure_does_not_hide_a_completed_delete() {
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            fs::write(&marker, id_of(&seeded("other-token"))).unwrap();
            let held = Arc::new(Mutex::new(Some(deny_marker_replacement(&marker))));
            set_hook(&auth, move |call| {
                if call == 1 {
                    drop(held.lock().unwrap().take());
                }
            });

            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!marker.exists());
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 1);
        }

        #[tokio::test]
        async fn revision12_other_instances_cannot_write_between_compare_and_delete() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = GatedDeleteStore::default();
            seed_record(&store.inner, None).await;
            let (fixture, addr, server) = login_fixture(store.clone()).await;
            let instance = || {
                TwitcastingAuth::with_logout_marker(
                    store.clone(),
                    fixture.endpoints.clone(),
                    fixture.options.clone(),
                    Some(marker.clone()),
                )
            };
            let old = instance();
            let new = instance();
            old.initialize(Some("client"), 0).await;
            new.initialize(Some("client"), 0).await;

            let logout = tokio::spawn(async move { old.disconnect().await });
            timeout(Duration::from_secs(2), store.deleting.notified())
                .await
                .unwrap();
            let mut login = post_grant(new, addr, Duration::ZERO, "").await;
            let login_while_deleting = timeout(Duration::from_millis(150), &mut login).await;
            store.resume.notify_one();
            logout.await.unwrap().unwrap();
            assert!(
                login_while_deleting.is_err(),
                "login wrote before logout finished"
            );
            assert_eq!(login.await.unwrap().unwrap().state, "ready");
            let value = store.inner.raw(TOKEN_KEY).unwrap();
            assert_eq!(
                parse_stored_record(&value).unwrap().access_token,
                "new-token"
            );
            assert!(!marker.exists());
            server.abort();
        }

        #[cfg(windows)]
        #[test]
        fn revision12_windows_lock_is_shared_across_processes() {
            const CHILD_PATH: &str = "FAST_COMMENT_TEST_CHILD_CREDENTIAL_LOCK";
            if let Some(path) = std::env::var_os(CHILD_PATH) {
                let lock = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(path)
                    .unwrap();
                assert!(matches!(
                    lock.try_lock(),
                    Err(std::fs::TryLockError::WouldBlock)
                ));
                return;
            }

            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(CREDENTIAL_LOCK_FILE);
            let _lock = lock_credential_file(&path, Instant::now()).unwrap();
            let child = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    concat!(
                        module_path!(),
                        "::revision12_windows_lock_is_shared_across_processes"
                    )
                    .strip_prefix(concat!(env!("CARGO_CRATE_NAME"), "::"))
                    .unwrap(),
                    "--nocapture",
                ])
                .env(CHILD_PATH, path)
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&child.stdout);
            assert!(
                child.status.success() && stdout.contains("1 passed"),
                "{stdout}"
            );
        }

        #[tokio::test]
        async fn revision12_startup_removes_abandoned_marker_temps() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let abandoned = dir.path().join(format!(
                ".{LOGOUT_MARKER_FILE}.{}.tmp",
                uuid::Uuid::new_v4()
            ));
            fs::write(&abandoned, b"partial marker").unwrap();
            let unrelated = dir
                .path()
                .join(format!(".{LOGOUT_MARKER_FILE}.unrelated.tmp"));
            fs::write(&unrelated, b"unrelated").unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store, marker);
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
            assert!(!abandoned.exists());
            assert!(unrelated.exists());
        }

        // 修正14-1: 現在値を読めなくても、別の識別子だけのマーカーがある初回の明示操作で削除する。
        #[tokio::test]
        async fn revision14_first_logout_deletes_unreadable_value_despite_other_marker() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            store.fail_next_get();
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();

            store.fail_next_get();
            let first = auth.disconnect().await;
            assert!(first.is_ok());
            assert!(store.raw(TOKEN_KEY).is_none(), "現在の資格情報が残った");
            auth.initialize(Some("client"), 0).await;
            assert_ne!(auth.status(Some("client"), 0).state, "ready");
        }

        #[tokio::test]
        async fn revision15_retry_keeps_new_login_after_marked_credential_delete_fails() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            store.fail_next_get();
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");

            fs::write(&marker, id_of(&seeded("test-token"))).unwrap();
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            assert_eq!(
                auth.status(Some("client"), 0).logout_issue,
                Some(LogoutIssue::DeleteFailed)
            );
            assert!(store.raw(TOKEN_KEY).is_some());

            let newer = serde_json::to_string(&seeded("new-token")).unwrap();
            store.put(TOKEN_KEY, &newer).await.unwrap();
            auth.disconnect().await.unwrap();
            assert!(
                store.raw(TOKEN_KEY).as_deref() == Some(newer.as_str()),
                "再ログイン後の資格情報が失われた"
            );
            assert!(!marker.exists());
            let status = auth.status(Some("client"), 0);
            assert_eq!(status.logout_issue, Some(LogoutIssue::OtherLoginKept));
            assert_eq!(status.logouts_completed, 0);
        }

        #[tokio::test]
        async fn revision15_retry_keeps_new_login_after_empty_store_cleanup_fails() {
            let dir = tempfile::tempdir().unwrap();
            let config = dir.path().join("config");
            fs::create_dir(&config).unwrap();
            let marker = config.join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            store.fail_next_get();
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(marker.clone()),
                Some(dir.path().join("credentials.lock")),
            );
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");

            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();
            let backup = dir.path().join("config-backup");
            set_hook(&auth, {
                let config = config.clone();
                let backup = backup.clone();
                move |call| {
                    if call == 1 {
                        // The empty store was already read when marker cleanup starts.
                        fs::rename(&config, &backup).unwrap();
                        fs::write(&config, []).unwrap();
                    }
                }
            });
            assert!(auth.disconnect().await.is_err());
            assert_eq!(
                auth.status(Some("client"), 0).logout_issue,
                Some(LogoutIssue::MarkerInaccessible)
            );
            assert!(store.raw(TOKEN_KEY).is_none());
            fs::remove_file(&config).unwrap();
            fs::rename(&backup, &config).unwrap();

            let newer = serde_json::to_string(&seeded("new-token")).unwrap();
            store.put(TOKEN_KEY, &newer).await.unwrap();
            auth.disconnect().await.unwrap();
            assert!(
                store.raw(TOKEN_KEY).as_deref() == Some(newer.as_str()),
                "再ログイン後の資格情報が失われた"
            );
            assert!(!marker.exists());
            let status = auth.status(Some("client"), 0);
            assert_eq!(status.logout_issue, Some(LogoutIssue::OtherLoginKept));
            assert_eq!(status.logouts_completed, 0);
        }

        #[tokio::test]
        async fn revision16_unreadable_retry_does_not_delete_a_new_login() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());

            store.fail_next_get();
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            assert!(store.raw(TOKEN_KEY).is_some());
            assert!(!marker.exists());

            let after_first_request = crate::timer_now_ms();
            wait_until(|| crate::timer_now_ms() > after_first_request).await;
            let mut newer = seeded("new-token");
            newer.obtained_at_ms = crate::timer_now_ms();
            let value = serde_json::to_string(&newer).unwrap();
            store.put(TOKEN_KEY, &value).await.unwrap();

            store.fail_next_get();
            let retry = auth.disconnect().await;
            assert!(
                store.raw(TOKEN_KEY).as_deref() == Some(value.as_str()),
                "読込に失敗した再試行で別のログインが失われた"
            );
            assert!(retry.is_err());
            let status = auth.status(Some("client"), 0);
            assert!(status.logout_pending);
            assert_eq!(status.logout_issue, Some(LogoutIssue::DeleteFailed));
            assert_eq!(status.logouts_completed, 0);

            wait_until(|| crate::timer_now_ms() > newer.obtained_at_ms).await;
            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).as_deref() == Some(value.as_str()));
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(status.logout_issue, Some(LogoutIssue::OtherLoginKept));
            assert_eq!(status.logouts_completed, 0);
        }

        #[tokio::test]
        async fn revision16_readable_retry_deletes_the_original_login() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = auth_with_marker(store.clone(), marker.clone());

            store.fail_next_get();
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            assert!(store.raw(TOKEN_KEY).is_some());
            assert!(!marker.exists());

            auth.disconnect().await.unwrap();
            drop(auth);
            let restarted = auth_with_marker(store.clone(), marker.clone());
            restarted.initialize(Some("client"), 0).await;
            assert_ne!(
                restarted.status(Some("client"), 0).state,
                "ready",
                "ログアウトの再試行後に元の資格情報が復元された"
            );
            assert!(store.raw(TOKEN_KEY).is_none());
            assert!(!marker.exists());
        }

        // 修正17-1: 読込前にロック待ちで終わった初回の後の再試行は、受付時刻で判定する。
        #[tokio::test]
        async fn revision17_retry_after_lock_timeout_keeps_a_later_login() {
            for later_login in [true, false] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join(LOGOUT_MARKER_FILE);
                let store = MemorySecretStore::new();
                seed_record(&store, None).await;
                store.fail_next_get();
                let auth = TwitcastingAuth::with_logout_marker(
                    store.clone(),
                    TwitcastingEndpoints::default(),
                    short_options(200),
                    Some(marker.clone()),
                );
                auth.initialize(Some("client"), 0).await;
                assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
                let held = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(false)
                    .open(marker.with_extension("lock"))
                    .unwrap();
                held.lock().unwrap();

                assert!(auth.disconnect().await.is_err());
                wait_until(|| !auth.session.memory.lock().unwrap().logout_running).await;
                assert_eq!(
                    auth.status(Some("client"), 0).logout_issue,
                    Some(LogoutIssue::LockTimedOut)
                );
                drop(held);

                let cutoff = auth
                    .session
                    .memory
                    .lock()
                    .unwrap()
                    .pending_logout
                    .requested_at_ms
                    .unwrap();
                let mut newer = seeded("new-token");
                newer.obtained_at_ms = cutoff + 1;
                let value = serde_json::to_string(&newer).unwrap();
                if later_login {
                    store.put(TOKEN_KEY, &value).await.unwrap();
                }

                auth.disconnect().await.unwrap();
                let status = auth.status(Some("client"), 0);
                assert!(!status.logout_pending);
                if later_login {
                    assert!(
                        store.raw(TOKEN_KEY).as_deref() == Some(value.as_str()),
                        "受付後の別セッションのログインが失われた"
                    );
                    assert_eq!(status.logout_issue, Some(LogoutIssue::OtherLoginKept));
                    assert_eq!(status.logouts_completed, 0);
                } else {
                    assert!(store.raw(TOKEN_KEY).is_none(), "元の資格情報が残った");
                    assert_eq!(status.logouts_completed, 1);
                }
                assert!(!marker.exists());
            }
        }

        // 修正17-2: 受付時刻と同時刻の記録は削除する。時計が 0 を返す受付でも 0 の記録は残さない。
        #[tokio::test]
        async fn revision17_retry_deletes_a_login_obtained_at_the_request_time() {
            for clock_zero in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join(LOGOUT_MARKER_FILE);
                let store = MemorySecretStore::new();
                seed_record(&store, None).await;
                let auth = auth_with_marker(store.clone(), marker.clone());

                store.fail_next_get();
                store.fail_next_delete();
                assert!(auth.disconnect().await.is_err());
                let cutoff = {
                    let mut memory = auth.session.memory.lock().unwrap();
                    let requested = &mut memory.pending_logout.requested_at_ms;
                    if clock_zero {
                        *requested = Some(0);
                    }
                    requested.unwrap()
                };
                let mut same_time = seeded("same-time-token");
                same_time.obtained_at_ms = cutoff;
                let value = serde_json::to_string(&same_time).unwrap();
                store.put(TOKEN_KEY, &value).await.unwrap();

                auth.disconnect().await.unwrap();
                assert!(store.raw(TOKEN_KEY).is_none(), "受付と同時刻の記録が残った");
                let status = auth.status(Some("client"), 0);
                assert!(!status.logout_pending);
                assert_eq!(status.logouts_completed, 1);
                drop(auth);
                let restarted = auth_with_marker(store.clone(), marker.clone());
                restarted.initialize(Some("client"), 0).await;
                assert_ne!(restarted.status(Some("client"), 0).state, "ready");
            }
        }

        // 修正18: 時計が 0 を返す PC では取得時刻がログイン開始からの経過分だけ進む。
        // 受付時刻 0 の再試行でも、その経路で保存した元の記録を残さない。
        #[tokio::test]
        async fn revision18_clock_zero_retry_deletes_a_login_saved_with_elapsed_time() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            {
                // connect() receives now_ms = 0, as timer_now_ms() does before 1970.
                let (auth, addr, server) = super::login_fixture(store.clone()).await;
                let login = super::post_grant(auth, addr, Duration::from_millis(300), "").await;
                let status = timeout(Duration::from_secs(5), login)
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
                assert_eq!(status.state, "ready");
                server.abort();
            }
            let saved: Record = serde_json::from_str(&store.raw(TOKEN_KEY).unwrap()).unwrap();
            assert!(saved.obtained_at_ms >= 300);

            let auth = auth_with_marker(store.clone(), marker.clone());
            store.fail_next_get();
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            assert!(store.raw(TOKEN_KEY).is_some());
            // The request time comes from the same clock, which returns 0.
            auth.session
                .memory
                .lock()
                .unwrap()
                .pending_logout
                .requested_at_ms = Some(0);

            auth.disconnect().await.unwrap();
            assert!(
                store.raw(TOKEN_KEY).is_none(),
                "時計 0 の再試行で元の記録が残った"
            );
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(status.logouts_completed, 1);
            drop(auth);
            let restarted = auth_with_marker(store.clone(), marker.clone());
            restarted.initialize(Some("client"), 0).await;
            assert_ne!(restarted.status(Some("client"), 0).state, "ready");
        }

        // 修正18: 読込前に終わった初回の後の再試行でも、解析不能な値は取り残さない。
        #[tokio::test]
        async fn revision18_retry_after_lock_timeout_deletes_unrecognized_value() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            store.put(TOKEN_KEY, "not a record").await.unwrap();
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(200),
                Some(marker.clone()),
            );
            let held = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(marker.with_extension("lock"))
                .unwrap();
            held.lock().unwrap();
            assert!(auth.disconnect().await.is_err());
            wait_until(|| !auth.session.memory.lock().unwrap().logout_running).await;
            assert_eq!(
                auth.status(Some("client"), 0).logout_issue,
                Some(LogoutIssue::LockTimedOut)
            );
            drop(held);

            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none(), "解析できない値が残った");
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(
                status.logout_issue,
                Some(LogoutIssue::UnreadableRecordDeleted)
            );
            assert_eq!(status.logouts_completed, 1);
            assert!(!marker.exists());
        }

        // 修正17-3: 直接削除に失敗した後の再試行で読めた解析不能な値は、直接削除の対象として消す。
        #[tokio::test]
        async fn revision17_retry_after_failed_direct_delete_deletes_unrecognized_value() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            store.put(TOKEN_KEY, "not a record").await.unwrap();
            let auth = auth_with_marker(store.clone(), marker.clone());

            store.fail_next_get();
            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            assert!(store.raw(TOKEN_KEY).is_some());

            auth.disconnect().await.unwrap();
            assert!(
                store.raw(TOKEN_KEY).is_none(),
                "直接削除の対象だった値が残った"
            );
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(
                status.logout_issue,
                Some(LogoutIssue::UnreadableRecordDeleted)
            );
            assert_eq!(status.logouts_completed, 1);
            assert!(!marker.exists());
        }

        // 修正14-2: ストアとブロッキングタスクの両キューで待った分をロック期限に含める。
        #[test]
        fn revision14_queued_logout_waits_only_for_its_remaining_time() {
            tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .max_blocking_threads(1)
                .build()
                .unwrap()
                .block_on(async {
                    let dir = tempfile::tempdir().unwrap();
                    let marker = dir.path().join(LOGOUT_MARKER_FILE);
                    let store = MemorySecretStore::new();
                    seed_record(&store, None).await;
                    let auth = TwitcastingAuth::with_logout_marker(
                        store,
                        TwitcastingEndpoints::default(),
                        short_options(1000),
                        Some(marker.clone()),
                    );
                    auth.initialize(Some("client"), 0).await;
                    assert_eq!(auth.status(Some("client"), 0).state, "ready");
                    let held = fs::OpenOptions::new()
                        .read(true)
                        .write(true)
                        .open(marker.with_extension("lock"))
                        .unwrap();
                    held.lock().unwrap();

                    let (resume_blocking, blocking_gate) = oneshot::channel();
                    let blocker = tokio::task::spawn_blocking(move || {
                        let _ = blocking_gate.blocking_recv();
                    });
                    let (started, worker_started) = oneshot::channel();
                    let (resume_worker, worker_gate) = oneshot::channel();
                    auth.enqueue(async move {
                        let _ = started.send(());
                        let _ = worker_gate.await;
                    })
                    .unwrap();
                    worker_started.await.unwrap();
                    let queue = auth.store_queue.lock().unwrap().as_ref().unwrap().clone();

                    let logout = tokio::spawn({
                        let auth = auth.clone();
                        async move { auth.disconnect().await }
                    });
                    wait_until(|| queue.capacity() < STORE_QUEUE_CAPACITY).await;
                    resume_worker.send(()).unwrap();

                    assert!(logout.await.unwrap().is_err());
                    resume_blocking.send(()).unwrap();
                    blocker.await.unwrap();
                    timeout(Duration::from_millis(500), async {
                        while auth.session.memory.lock().unwrap().logout_running {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                        }
                    })
                    .await
                    .expect("ログアウトが期限を過ぎてもロックを待ち続けた");
                    assert_eq!(
                        auth.status(Some("client"), 0).logout_issue,
                        Some(LogoutIssue::LockTimedOut)
                    );
                    drop(held);
                });
        }

        // 修正14-5: ロック待ちの間の 2 回目のログアウトは二重に登録せず拒否する。
        #[tokio::test]
        async fn revision14_second_logout_during_lock_wait_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(2000),
                Some(marker.clone()),
            );
            auth.initialize(Some("client"), 0).await;
            let held = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(marker.with_extension("lock"))
                .unwrap();
            held.lock().unwrap();
            let first = tokio::spawn({
                let auth = auth.clone();
                async move { auth.disconnect().await }
            });
            wait_until(|| auth.session.memory.lock().unwrap().logout_running).await;

            let second = auth.disconnect().await.unwrap_err();
            assert!(matches!(second.code, Code::TokenStore));
            assert!(auth.session.memory.lock().unwrap().logout_running);
            drop(held);
            first.await.unwrap().unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
            let status = auth.status(Some("client"), 0);
            assert!(!status.logout_pending);
            assert_eq!(status.logouts_completed, 1);
            assert!(!marker.exists());
        }

        // 修正13-1: 初回の明示ログアウトは、別セッションのマーカーがあっても読めた現在値を消す。
        #[tokio::test]
        async fn revision13_first_explicit_logout_deletes_current_despite_other_marker() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            store.fail_next_get();
            let auth = auth_with_marker(store.clone(), marker.clone());
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            // Another session failed to reconcile an old record A and left it in the marker.
            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();

            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none(), "現在の資格情報が残った");
            auth.initialize(Some("client"), 0).await;
            assert_ne!(auth.status(Some("client"), 0).state, "ready");
        }

        // 修正13-2: 削除済みの通知はカウンター、理由は今回の結果(別ログインを保持)。
        #[tokio::test]
        async fn revision13_retry_after_unreadable_delete_reports_kept_login() {
            let root = tempfile::tempdir().unwrap();
            let config = root.path().join("config");
            fs::create_dir(&config).unwrap();
            let marker = config.join(LOGOUT_MARKER_FILE);
            let store = UnreadableStore::default();
            store.put(TOKEN_KEY, "invalid envelope").await.unwrap();
            let auth = TwitcastingAuth::with_logout_files(
                store.clone(),
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(marker),
                Some(root.path().join("credentials.lock")),
            );
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            set_hook(&auth, {
                let config = config.clone();
                move |call| {
                    // The delete succeeds, then removing the marker fails.
                    if call == 1 {
                        fs::remove_dir_all(&config).unwrap();
                        fs::write(&config, []).unwrap();
                    }
                }
            });
            assert!(auth.disconnect().await.is_err());
            assert!(store.0.raw(TOKEN_KEY).is_none());
            fs::remove_file(&config).unwrap();
            fs::create_dir(&config).unwrap();
            // Another session logs in as B before the retry.
            let other = serde_json::to_string(&seeded("other-token")).unwrap();
            store.put(TOKEN_KEY, &other).await.unwrap();

            auth.disconnect().await.unwrap();
            assert_eq!(store.0.raw(TOKEN_KEY).as_deref(), Some(other.as_str()));
            let status = auth.status(Some("client"), 0);
            assert_eq!(status.logouts_completed, 1);
            assert_eq!(
                serde_json::to_value(status).unwrap()["logoutIssue"],
                "otherLoginKept"
            );
        }

        // 修正13-3: 別セッションがロックを握ったままでも、ログアウトの待ちは上限で終わる。
        #[tokio::test]
        async fn revision13_held_credential_lock_ends_the_wait() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(200),
                Some(marker.clone()),
            );
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
            let held = fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(marker.with_extension("lock"))
                .unwrap();
            held.lock().unwrap();

            assert!(auth.disconnect().await.is_err());
            wait_until(|| !auth.session.memory.lock().unwrap().logout_running).await;
            let status = auth.status(Some("client"), 0);
            assert!(status.logout_pending);
            assert_eq!(
                serde_json::to_value(status).unwrap()["logoutIssue"],
                "lockTimedOut"
            );
            assert!(store.raw(TOKEN_KEY).is_some());
            drop(held);
            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
        }

        // 修正13-4: 上限を超えるマーカーは読み込まず破損扱い。
        #[test]
        fn revision13_oversized_marker_is_corrupt() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let lines = (0..1100)
                .map(|i| value_id(&i.to_string()))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(lines.len() > 64 * 1024);
            fs::write(&marker, lines).unwrap();
            assert!(matches!(
                read_marker(&marker),
                Err(LogoutIssue::MarkerCorrupt)
            ));
        }

        // 修正13-5: ローカルデータフォルダーだけ取れない時は「利用できない」を返す。
        #[tokio::test]
        async fn revision13_missing_lock_path_is_reported_as_unavailable() {
            let dir = tempfile::tempdir().unwrap();
            let store = MemorySecretStore::new();
            seed_record(&store, None).await;
            let auth = TwitcastingAuth::with_logout_files(
                store,
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                Some(dir.path().join(LOGOUT_MARKER_FILE)),
                None,
            );
            auth.initialize(Some("client"), 0).await;
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["logoutIssue"], "markerUnavailable");
        }

        #[tokio::test]
        async fn revision11_explicit_logout_keeps_another_sessions_login() {
            for retry in [false, true] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join(LOGOUT_MARKER_FILE);
                let store = MemorySecretStore::new();
                seed_record(&store, None).await;
                let auth = auth_with_marker(store.clone(), marker.clone());
                auth.initialize(Some("client"), 0).await;
                assert_eq!(auth.status(Some("client"), 0).state, "ready");
                if retry {
                    store.fail_next_delete();
                    assert!(auth.disconnect().await.is_err());
                }

                let newer = serde_json::to_string(&seeded("other-token")).unwrap();
                store.put(TOKEN_KEY, &newer).await.unwrap();
                auth.disconnect().await.unwrap();

                assert_eq!(store.raw(TOKEN_KEY).as_deref(), Some(newer.as_str()));
                assert!(!marker.exists());
                let status = auth.status(Some("client"), 0);
                assert_eq!(status.state, "loggedOut");
                assert!(!status.logout_pending);
                assert_eq!(status.logouts_completed, 0);
                assert_eq!(
                    serde_json::to_value(status).unwrap()["logoutIssue"],
                    "otherLoginKept"
                );
            }
        }

        #[tokio::test]
        async fn revision11_nonempty_whitespace_marker_cannot_authorize_deletion() {
            for content in ["\n", "\u{3000}"] {
                let store = MemorySecretStore::new();
                seed_record(&store, None).await;
                let (_dir, marker, auth) = marked_auth(store.clone(), content, 5000);
                auth.initialize(Some("client"), 0).await;
                assert!(store.raw(TOKEN_KEY).is_some(), "deleted for {content:?}");
                assert_eq!(fs::read_to_string(&marker).unwrap(), content);
                let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
                assert_eq!(status["logoutIssue"], "markerCorrupt");
            }
        }

        #[tokio::test]
        async fn revision11_record_marker_cannot_delete_unparseable_value() {
            let store = MemorySecretStore::new();
            store.put(TOKEN_KEY, "{}").await.unwrap();
            let (_dir, marker, auth) =
                marked_auth(store.clone(), &id_of(&seeded("old-token")), 5000);
            auth.initialize(Some("client"), 0).await;
            assert_eq!(store.raw(TOKEN_KEY).as_deref(), Some("{}"));
            assert!(!marker.exists());
            let status = serde_json::to_value(auth.status(Some("client"), 0)).unwrap();
            assert_eq!(status["state"], "loadFailed");
            assert_eq!(status["logoutIssue"], "recordUnrecognized");

            auth.disconnect().await.unwrap();
            assert!(store.raw(TOKEN_KEY).is_none());
            assert_eq!(auth.status(Some("client"), 0).logouts_completed, 1);
        }

        #[tokio::test]
        async fn revision11_explicit_logout_compares_a_snapshot_of_an_unparseable_value() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            let store = MemorySecretStore::new();
            store.put(TOKEN_KEY, "{}").await.unwrap();
            let auth = auth_with_marker(store.clone(), marker.clone());
            store.fail_next_get();
            auth.initialize(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");

            store.fail_next_delete();
            assert!(auth.disconnect().await.is_err());
            let saved_intent = fs::read_to_string(&marker).unwrap();
            assert!(saved_intent.starts_with("raw:"));
            assert!(!saved_intent.contains("{}"));
            let newer = serde_json::to_string(&seeded("other-token")).unwrap();
            store.put(TOKEN_KEY, &newer).await.unwrap();
            auth.disconnect().await.unwrap();
            assert_eq!(store.raw(TOKEN_KEY).as_deref(), Some(newer.as_str()));
            assert!(!marker.exists());
        }

        #[tokio::test]
        async fn revision11_raw_marker_retries_only_the_observed_value() {
            for current in ["{}", "{ }"] {
                let dir = tempfile::tempdir().unwrap();
                let marker = dir.path().join(LOGOUT_MARKER_FILE);
                let store = MemorySecretStore::new();
                store.put(TOKEN_KEY, "{}").await.unwrap();
                let auth = auth_with_marker(store.clone(), marker.clone());
                store.fail_next_delete();
                assert!(auth.disconnect().await.is_err());
                assert!(fs::read_to_string(&marker).unwrap().starts_with("raw:"));
                drop(auth);

                store.put(TOKEN_KEY, current).await.unwrap();
                let auth = auth_with_marker(store.clone(), marker.clone());
                auth.initialize(Some("client"), 0).await;
                let unchanged = current == "{}";
                assert_eq!(
                    store.raw(TOKEN_KEY).as_deref(),
                    (!unchanged).then_some(current)
                );
                assert_eq!(
                    auth.status(Some("client"), 0).logouts_completed,
                    u64::from(unchanged)
                );
                assert!(!marker.exists());
            }
        }

        #[tokio::test]
        async fn revision11_late_marker_mismatch_leaves_auth_reloadable() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();
            let store = PausedReadStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(150),
                Some(marker.clone()),
            );
            auth.initialize(Some("client"), 0).await;
            assert!(auth.status(Some("client"), 0).logout_pending);
            store.resume.notify_one();
            wait_until(|| !auth.status(Some("client"), 0).logout_pending).await;
            assert!(!marker.exists());
            assert!(store.inner.raw(TOKEN_KEY).is_some());
            assert_eq!(auth.status(Some("client"), 0).state, "loadFailed");
            store.resume.notify_one();
            auth.reload(Some("client"), 0).await;
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
        }

        #[tokio::test]
        async fn revision11_marker_mismatch_restore_uses_the_startup_deadline() {
            let dir = tempfile::tempdir().unwrap();
            let marker = dir.path().join(LOGOUT_MARKER_FILE);
            fs::write(&marker, id_of(&seeded("old-token"))).unwrap();
            let store = PausedReadStore::default();
            seed_record(&store.inner, None).await;
            let auth = TwitcastingAuth::with_logout_marker(
                store.clone(),
                TwitcastingEndpoints::default(),
                short_options(600),
                Some(marker),
            );
            let init = auth.initialize(Some("client"), 0);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(350)).await;
                store.resume.notify_one();
                // A second get, if any, remains blocked through the original deadline.
            });
            timeout(Duration::from_millis(800), init)
                .await
                .expect("restore exceeded the startup deadline");
            assert_eq!(auth.status(Some("client"), 0).state, "ready");
            assert_eq!(
                auth.bearer(Some("client"), 0).as_deref(),
                Some("test-token")
            );
        }

        #[tokio::test]
        async fn revision11_absent_credential_does_not_count_as_deleted() {
            for content in [id_of(&seeded("old-token")), String::new()] {
                let store = MemorySecretStore::new();
                let (_dir, marker, auth) = marked_auth(store.clone(), &content, 5000);
                // An unnecessary delete would fail and leave the marker in place.
                store.fail_next_delete();
                auth.initialize(Some("client"), 0).await;
                assert_eq!(auth.status(Some("client"), 0).logouts_completed, 0);
                assert!(!marker.exists());
            }
        }

        // 修正10-8: 起動時の検査中のログインは TokenStore で拒否する。
        #[tokio::test]
        async fn revision10_connect_during_startup_check_is_rejected() {
            let dir = tempfile::tempdir().unwrap();
            let auth = auth_with_marker(
                MemorySecretStore::new(),
                dir.path().join(LOGOUT_MARKER_FILE),
            );
            let _init = auth.initialize(Some("client"), 0);
            let result = auth.connect("client", 0, |_| Ok(())).await;
            assert!(matches!(result.unwrap_err().code, Code::TokenStore));
        }

        // 修正10-5/8: 再読込で検査し直している間のログインも拒否する。
        #[tokio::test]
        async fn revision10_connect_during_reload_recheck_is_rejected() {
            let store = MemorySecretStore::new();
            let dir = tempfile::tempdir().unwrap();
            let auth = TwitcastingAuth::with_logout_marker(
                store,
                TwitcastingEndpoints::default(),
                short_options(100),
                Some(dir.path().join(LOGOUT_MARKER_FILE)),
            );
            set_hook(&auth, |call| {
                if call <= 1 {
                    std::thread::sleep(Duration::from_millis(300));
                }
            });
            auth.initialize(Some("client"), 0).await;
            assert_eq!(
                serde_json::to_value(auth.status(Some("client"), 0)).unwrap()["logoutIssue"],
                "checkTimedOut"
            );
            let reload = tokio::spawn({
                let auth = auth.clone();
                async move { auth.reload(Some("client"), 0).await }
            });
            wait_until(|| auth.status(Some("client"), 0).state == "loading").await;
            let result = auth.connect("client", 0, |_| Ok(())).await;
            assert!(matches!(result.unwrap_err().code, Code::TokenStore));
            reload.await.unwrap();
        }
    }
}
