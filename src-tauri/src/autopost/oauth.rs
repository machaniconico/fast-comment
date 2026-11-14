//! X(Twitter) OAuth 2.0 Authorization Code + PKCE(公開クライアント)。
//!
//! access token はメモリだけに置き、refresh token とユーザー情報だけを OS の
//! 資格情報ストアへ保存する。秘匿値はログ・エラー文・Debug に出さない。

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex as StdMutex;
use std::time::{Duration, Instant};

use oauth2::{CsrfToken, PkceCodeChallenge};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use super::error::{XPostError, XPostErrorCode};
use crate::secret_store;

pub const X_POST_TOKEN_ENTRY: &str = "x-post-oauth-token";

const CALLBACK_PATH: &str = "/callback";
const AUTHORIZE_URL: &str = "https://x.com/i/oauth2/authorize";
const TOKEN_URL: &str = "https://api.x.com/2/oauth2/token";
const TWEETS_URL: &str = "https://api.x.com/2/tweets";
const USERS_ME_URL: &str = "https://api.x.com/2/users/me";
const SCOPE: &str = "tweet.read tweet.write users.read offline.access";
const DEFAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(180);
const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(5);
const DEFAULT_EXPIRES_IN: u64 = 7200;
const EXPIRY_SLACK_SECS: u64 = 60;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredXToken {
    pub client_id: String,
    pub refresh_token: String,
    pub user_id: String,
    pub username: String,
}

impl std::fmt::Debug for StoredXToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredXToken")
            .field("client_id", &self.client_id)
            .field("refresh_token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("username", &self.username)
            .finish()
    }
}

/// 保存先。返す future は `Send`(駆動タスクから `tokio::spawn` で投稿するため)。
/// 実装側は `async fn` で書ける。
pub trait TokenStore: Send + Sync {
    fn load(&self) -> impl Future<Output = Result<Option<StoredXToken>, String>> + Send;
    fn save(&self, t: &StoredXToken) -> impl Future<Output = Result<(), String>> + Send;
    fn delete(&self) -> impl Future<Output = Result<(), String>> + Send;
}

/// OS の資格情報ストアを 1 エントリで使う実装。
pub struct KeyringTokenStore;

impl TokenStore for KeyringTokenStore {
    async fn load(&self) -> Result<Option<StoredXToken>, String> {
        secret_store::load(X_POST_TOKEN_ENTRY).await
    }

    async fn save(&self, t: &StoredXToken) -> Result<(), String> {
        secret_store::save(X_POST_TOKEN_ENTRY, t).await
    }

    async fn delete(&self) -> Result<(), String> {
        secret_store::delete(X_POST_TOKEN_ENTRY).await
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct XEndpoints {
    pub authorize: String,
    pub token: String,
    pub tweets: String,
    pub users_me: String,
}

impl Default for XEndpoints {
    fn default() -> Self {
        Self {
            authorize: AUTHORIZE_URL.to_string(),
            token: TOKEN_URL.to_string(),
            tweets: TWEETS_URL.to_string(),
            users_me: USERS_ME_URL.to_string(),
        }
    }
}

impl XEndpoints {
    pub fn with_overrides(map: &HashMap<String, String>) -> Self {
        let mut endpoints = Self::default();
        let apply = |key: &str, target: &mut String| {
            if let Some(value) = map.get(key).map(|v| v.trim()) {
                if !value.is_empty() {
                    *target = value.to_string();
                }
            }
        };
        apply("postAuthorizeUrl", &mut endpoints.authorize);
        apply("postTokenUrl", &mut endpoints.token);
        apply("postTweetsUrl", &mut endpoints.tweets);
        apply("postUsersMeUrl", &mut endpoints.users_me);
        endpoints
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct XPostStatus {
    pub configured: bool,
    pub connected: bool,
    pub username: Option<String>,
    pub callback_url: String,
    pub unsaved_token: bool,
}

pub fn callback_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

pub fn build_authorize_url(
    endpoints: &XEndpoints,
    client_id: &str,
    port: u16,
    state: &str,
    code_challenge: &str,
) -> String {
    let redirect = callback_url(port);
    // 設定由来の URL が壊れていても認可画面までは到達できるようにする。
    let mut url = Url::parse(&endpoints.authorize)
        .or_else(|_| Url::parse(AUTHORIZE_URL))
        .expect("既定の認可 URL は有効");
    url.query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", client_id)
        .append_pair("redirect_uri", &redirect)
        .append_pair("scope", SCOPE)
        .append_pair("state", state)
        .append_pair("code_challenge", code_challenge)
        .append_pair("code_challenge_method", "S256");
    url.to_string()
}

struct Inner {
    current: Option<StoredXToken>,
    access: Option<(String, Instant)>,
    unsaved: bool,
}

/// `status` が `inner` の lock(refresh の通信中ずっと持たれる)を待たずに読む写し。
#[derive(Default)]
struct StatusSnapshot {
    /// メモリ上の認証の (Client ID, username)。
    current: Option<(String, String)>,
    unsaved: bool,
}

struct LoginSession {
    id: u64,
    cancel: CancellationToken,
    /// 待受タスク。callback を受けた後は外して待ち終えるが、セッション自体は
    /// connect が確定するまでスロットに残し、取消(cancel_login・次の connect)を届かせる。
    handle: Option<tokio::task::JoinHandle<()>>,
}

pub struct XPostAuth<S: TokenStore> {
    store: S,
    endpoints: XEndpoints,
    http: reqwest::Client,
    inner: Mutex<Inner>,
    snapshot: StdMutex<StatusSnapshot>,
    login: StdMutex<Option<LoginSession>>,
    login_seq: AtomicU64,
    /// connect の取消→bind 区間だけを直列化する。
    setup: Mutex<()>,
    epoch: AtomicU64,
    login_timeout: Duration,
    read_timeout: Duration,
}

impl<S: TokenStore> std::fmt::Debug for XPostAuth<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("XPostAuth")
            .field("endpoints", &self.endpoints)
            .field("login_timeout", &self.login_timeout)
            .field("read_timeout", &self.read_timeout)
            .finish_non_exhaustive()
    }
}

impl<S: TokenStore> XPostAuth<S> {
    pub fn new(store: S, endpoints: XEndpoints) -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| "X認証用HTTPクライアントの作成に失敗しました".to_string())?;
        Ok(Self {
            store,
            endpoints,
            http,
            inner: Mutex::new(Inner {
                current: None,
                access: None,
                unsaved: false,
            }),
            snapshot: StdMutex::new(StatusSnapshot::default()),
            login: StdMutex::new(None),
            login_seq: AtomicU64::new(0),
            setup: Mutex::new(()),
            epoch: AtomicU64::new(0),
            login_timeout: DEFAULT_LOGIN_TIMEOUT,
            read_timeout: DEFAULT_READ_TIMEOUT,
        })
    }

    pub fn endpoints(&self) -> &XEndpoints {
        &self.endpoints
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }

    #[allow(dead_code)]
    pub(crate) fn with_login_timeout(mut self, d: Duration) -> Self {
        self.login_timeout = d;
        self
    }

    pub async fn status(&self, client_id: &str, port: u16) -> XPostStatus {
        let client_id = client_id.trim();
        let configured = !client_id.is_empty();
        let mut connected = false;
        let mut username = None;
        let mut unsaved = false;
        if configured {
            let in_memory = {
                let snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
                snap.current
                    .as_ref()
                    .filter(|(id, _)| id == client_id)
                    .map(|(_, name)| (name.clone(), snap.unsaved))
            };
            if let Some((name, unsaved_now)) = in_memory {
                connected = true;
                username = Some(name);
                unsaved = unsaved_now;
            } else if let Ok(Some(token)) = self.store.load().await {
                if token.client_id == client_id {
                    connected = true;
                    username = Some(token.username.clone());
                }
            }
        }
        XPostStatus {
            configured,
            connected,
            username,
            callback_url: callback_url(port),
            unsaved_token: unsaved,
        }
    }

    pub async fn connect(
        &self,
        client_id: &str,
        port: u16,
        open_browser: impl FnOnce(String) -> Result<(), String>,
    ) -> Result<XPostStatus, XPostError> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(XPostError::new(XPostErrorCode::Other));
        }

        let setup = self.setup.lock().await;
        // 進行中の待受を止め、listener が閉じるのを待ってから同じポートへ bind する。
        if let Some(previous) = self.take_login() {
            previous.cancel.cancel();
            if let Some(handle) = previous.handle {
                let _ = handle.await;
            }
        }

        // 世代はログイン完了時に進める(開始時に進めると、並行中の refresh の回転結果を捨ててしまう)。
        let epoch = self.epoch.load(Ordering::SeqCst);

        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|_| XPostError::new(XPostErrorCode::PortInUse))?;

        let (challenge, verifier) = PkceCodeChallenge::new_random_sha256();
        let state = CsrfToken::new_random();
        let url = build_authorize_url(
            &self.endpoints,
            client_id,
            port,
            state.secret(),
            challenge.as_str(),
        );

        let session_id = self.login_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let cancel = CancellationToken::new();
        let wait_cancel = cancel.clone();
        let check_cancel = cancel.clone();
        let expected_state = state.secret().to_string();
        let login_timeout = self.login_timeout;
        let read_timeout = self.read_timeout;
        // 結果は oneshot で受け取り、待受タスクの handle は取消・再 bind のため
        // スロットに残しておく(cancel_login がいつでも届くようにする)。
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let handle = tokio::spawn(async move {
            let result = wait_for_callback(
                listener,
                &expected_state,
                wait_cancel,
                login_timeout,
                read_timeout,
            )
            .await;
            let _ = result_tx.send(result);
        });
        {
            let mut slot = self.login.lock().unwrap_or_else(|e| e.into_inner());
            *slot = Some(LoginSession {
                id: session_id,
                cancel,
                handle: Some(handle),
            });
        }
        drop(setup);

        if open_browser(url).is_err() {
            if let Some(session) = self.remove_login(session_id) {
                session.cancel.cancel();
                if let Some(handle) = session.handle {
                    let _ = handle.await;
                }
            }
            return Err(XPostError::new(XPostErrorCode::Other));
        }

        let code = match result_rx.await {
            Ok(Ok(code)) => code,
            Ok(Err(error)) => {
                self.reap_login(session_id).await;
                return Err(error);
            }
            Err(_) => {
                self.reap_login(session_id).await;
                return Err(XPostError::new(XPostErrorCode::Other));
            }
        };
        // 交換・確定が終わるまでセッションはスロットに残す(取消を届かせる)。
        self.join_login_task(session_id).await;
        let result = self
            .finish_connect(client_id, port, &code, verifier.secret(), &check_cancel, epoch)
            .await;
        let _ = self.remove_login(session_id);
        result
    }

    async fn finish_connect(
        &self,
        client_id: &str,
        port: u16,
        code: &str,
        verifier: &str,
        check_cancel: &CancellationToken,
        epoch: u64,
    ) -> Result<XPostStatus, XPostError> {
        if check_cancel.is_cancelled() {
            return Err(XPostError::new(XPostErrorCode::Cancelled));
        }

        let redirect = callback_url(port);
        let (access, refresh, expires_in) = self
            .exchange_code(client_id, code, verifier, &redirect)
            .await?;
        let (user_id, username) = self.fetch_me(&access).await?;

        let stored = StoredXToken {
            client_id: client_id.to_string(),
            refresh_token: refresh,
            user_id,
            username,
        };
        {
            let mut inner = self.inner.lock().await;
            // 取消・切断・別の connect が先に進んでいたら結果を捨てる。
            if check_cancel.is_cancelled() || self.epoch.load(Ordering::SeqCst) != epoch {
                return Err(XPostError::new(XPostErrorCode::Cancelled));
            }
            self.epoch.fetch_add(1, Ordering::SeqCst);
            inner.current = Some(stored.clone());
            inner.access = Some((access, expiry_after(expires_in)));
            let saved = self.store.save(&stored).await.is_ok();
            inner.unsaved = !saved;
            self.publish(&inner);
        }
        Ok(self.status(client_id, port).await)
    }

    pub fn cancel_login(&self) {
        if let Some(session) = self
            .login
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            session.cancel.cancel();
        }
    }

    /// ローカルの認証を消すだけ(設計 §5.3、YouTube と同じ)。X 側の許可は取り消さない。
    pub async fn disconnect(&self) -> Result<(), XPostError> {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        self.cancel_login();
        let mut inner = self.inner.lock().await;
        inner.current = None;
        inner.access = None;
        inner.unsaved = false;
        self.publish(&inner);
        let result = self.store.delete().await;
        drop(inner);
        result.map_err(|_| XPostError::new(XPostErrorCode::TokenStore))
    }

    pub async fn access_token(&self, client_id: &str) -> Result<String, XPostError> {
        let client_id = client_id.trim();
        let mut inner = self.inner.lock().await;
        let result = self.access_token_locked(client_id, &mut inner).await;
        self.publish(&inner);
        result
    }

    /// `status` 用の写しを `inner` に揃える(`inner` の lock 中に呼ぶ)。
    fn publish(&self, inner: &Inner) {
        let mut snap = self.snapshot.lock().unwrap_or_else(|e| e.into_inner());
        snap.current = inner
            .current
            .as_ref()
            .map(|t| (t.client_id.clone(), t.username.clone()));
        snap.unsaved = inner.unsaved;
    }

    async fn access_token_locked(
        &self,
        client_id: &str,
        inner: &mut Inner,
    ) -> Result<String, XPostError> {

        // 未保存の回転済み refresh token があれば先に保存を試す。
        if inner.unsaved {
            if let Some(token) = inner.current.clone() {
                if self.store.save(&token).await.is_ok() {
                    inner.unsaved = false;
                }
            }
        }

        if inner.current.is_none() {
            match self.store.load().await {
                Ok(Some(token)) => inner.current = Some(token),
                Ok(None) => return Err(XPostError::new(XPostErrorCode::NotLoggedIn)),
                Err(_) => return Err(XPostError::new(XPostErrorCode::TokenStore)),
            }
        }

        let mut current = match inner.current.clone() {
            Some(token) => token,
            None => return Err(XPostError::new(XPostErrorCode::NotLoggedIn)),
        };
        if current.client_id != client_id {
            return Err(XPostError::new(XPostErrorCode::ReloginRequired));
        }

        if let Some((access, expires)) = inner.access.as_ref() {
            if *expires > Instant::now() {
                return Ok(access.clone());
            }
        }

        let epoch = self.epoch.load(Ordering::SeqCst);
        let mut used_refresh = current.refresh_token.clone();
        let mut retried = false;

        loop {
            if self.epoch.load(Ordering::SeqCst) != epoch {
                return Err(XPostError::new(XPostErrorCode::ReloginRequired));
            }
            match self.refresh_once(client_id, &used_refresh).await {
                RefreshOutcome::Ok {
                    access,
                    refresh,
                    expires_in,
                } => {
                    if self.epoch.load(Ordering::SeqCst) != epoch {
                        return Err(XPostError::new(XPostErrorCode::ReloginRequired));
                    }
                    let updated = StoredXToken {
                        client_id: client_id.to_string(),
                        refresh_token: refresh,
                        user_id: current.user_id.clone(),
                        username: current.username.clone(),
                    };
                    // 回転した refresh token を先に保存する(最大 3 回)。
                    let mut saved = false;
                    for _ in 0..3 {
                        if self.store.save(&updated).await.is_ok() {
                            saved = true;
                            break;
                        }
                    }
                    inner.current = Some(updated);
                    inner.access = Some((access.clone(), expiry_after(expires_in)));
                    inner.unsaved = !saved;
                    return Ok(access);
                }
                RefreshOutcome::Rejected => {
                    if !retried {
                        retried = true;
                        match self.store.load().await {
                            // 読めない時は保存を消さない(別プロセスの新しい token かもしれない)。
                            Err(_) => return Err(XPostError::new(XPostErrorCode::TokenStore)),
                            // 別の Client ID の保存は今回の拒否と無関係なので消さない。
                            Ok(Some(stored)) if stored.client_id != client_id => {
                                inner.current = None;
                                inner.access = None;
                                inner.unsaved = false;
                                return Err(XPostError::new(XPostErrorCode::ReloginRequired));
                            }
                            Ok(Some(stored)) if stored.refresh_token != used_refresh => {
                                used_refresh = stored.refresh_token.clone();
                                current = stored.clone();
                                inner.current = Some(stored);
                                continue;
                            }
                            Ok(_) => {}
                        }
                    }
                    // 長く使わず refresh token の寿命(約 6 か月)が切れた時もここに来る。
                    let _ = self.store.delete().await;
                    inner.current = None;
                    inner.access = None;
                    inner.unsaved = false;
                    return Err(XPostError::new(XPostErrorCode::ReloginRequired));
                }
                RefreshOutcome::Network => return Err(XPostError::new(XPostErrorCode::Network)),
                RefreshOutcome::Other => return Err(XPostError::new(XPostErrorCode::Other)),
            }
        }
    }

    pub async fn invalidate_access_token(&self) {
        self.inner.lock().await.access = None;
    }

    fn take_login(&self) -> Option<LoginSession> {
        self.login.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn remove_login(&self, id: u64) -> Option<LoginSession> {
        let mut slot = self.login.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_ref().is_some_and(|session| session.id == id) {
            slot.take()
        } else {
            None
        }
    }

    /// 自分の待受タスクをスロットから外し、終了を待つ。
    /// 新しい connect が先にスロットを取っていれば、終了はそちらに任せる。
    async fn reap_login(&self, id: u64) {
        if let Some(session) = self.remove_login(id) {
            if let Some(handle) = session.handle {
                let _ = handle.await;
            }
        }
    }

    /// 待受タスクの終了だけを待ち、セッション(取消トークン)はスロットに残す。
    async fn join_login_task(&self, id: u64) {
        let handle = {
            let mut slot = self.login.lock().unwrap_or_else(|e| e.into_inner());
            slot.as_mut()
                .filter(|session| session.id == id)
                .and_then(|session| session.handle.take())
        };
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    async fn exchange_code(
        &self,
        client_id: &str,
        code: &str,
        verifier: &str,
        redirect: &str,
    ) -> Result<(String, String, u64), XPostError> {
        let response = self
            .http
            .post(&self.endpoints.token)
            .form(&[
                ("grant_type", "authorization_code"),
                ("code", code),
                ("redirect_uri", redirect),
                ("code_verifier", verifier),
                ("client_id", client_id),
            ])
            .send()
            .await
            .map_err(|_| XPostError::new(XPostErrorCode::Network))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|_| XPostError::new(XPostErrorCode::Network))?;
        if !(200..300).contains(&status) {
            return Err(XPostError::new(XPostErrorCode::Other));
        }
        let value: Value =
            serde_json::from_str(&text).map_err(|_| XPostError::new(XPostErrorCode::Other))?;
        let access = value
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| XPostError::new(XPostErrorCode::Other))?;
        // refresh token が無い = offline.access が効いていない。
        let refresh = value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| XPostError::new(XPostErrorCode::Other))?;
        let expires_in = value
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_EXPIRES_IN);
        Ok((access.to_string(), refresh.to_string(), expires_in))
    }

    async fn fetch_me(&self, access: &str) -> Result<(String, String), XPostError> {
        let response = self
            .http
            .get(&self.endpoints.users_me)
            .bearer_auth(access)
            .send()
            .await
            .map_err(|_| XPostError::new(XPostErrorCode::Network))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|_| XPostError::new(XPostErrorCode::Network))?;
        if !(200..300).contains(&status) {
            return Err(XPostError::new(XPostErrorCode::Other));
        }
        let value: Value =
            serde_json::from_str(&text).map_err(|_| XPostError::new(XPostErrorCode::Other))?;
        let id = value
            .pointer("/data/id")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| XPostError::new(XPostErrorCode::Other))?;
        let username = value
            .pointer("/data/username")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| XPostError::new(XPostErrorCode::Other))?;
        Ok((id.to_string(), username.to_string()))
    }

    async fn refresh_once(&self, client_id: &str, refresh: &str) -> RefreshOutcome {
        let response = match self
            .http
            .post(&self.endpoints.token)
            .form(&[
                ("grant_type", "refresh_token"),
                ("refresh_token", refresh),
                ("client_id", client_id),
            ])
            .send()
            .await
        {
            Ok(response) => response,
            Err(_) => return RefreshOutcome::Network,
        };
        let status = response.status().as_u16();
        if status == 400 || status == 401 {
            return RefreshOutcome::Rejected;
        }
        if (500..600).contains(&status) {
            return RefreshOutcome::Network;
        }
        if !(200..300).contains(&status) {
            return RefreshOutcome::Other;
        }
        let text = match response.text().await {
            Ok(text) => text,
            Err(_) => return RefreshOutcome::Network,
        };
        let value: Value = match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(_) => return RefreshOutcome::Other,
        };
        let access = match value
            .get("access_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
        {
            Some(access) => access.to_string(),
            None => return RefreshOutcome::Other,
        };
        let refresh = value
            .get("refresh_token")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| refresh.to_string());
        let expires_in = value
            .get("expires_in")
            .and_then(|v| v.as_u64())
            .unwrap_or(DEFAULT_EXPIRES_IN);
        RefreshOutcome::Ok {
            access,
            refresh,
            expires_in,
        }
    }
}

enum RefreshOutcome {
    Ok {
        access: String,
        refresh: String,
        expires_in: u64,
    },
    Rejected,
    Network,
    Other,
}

fn expiry_after(expires_in: u64) -> Instant {
    let ttl = expires_in.saturating_sub(EXPIRY_SLACK_SECS);
    Instant::now() + Duration::from_secs(ttl)
}

enum CallbackTarget {
    Ignore,
    Code(String),
    Error,
}

fn parse_callback_target(target: &str, expected_state: &str) -> CallbackTarget {
    let Ok(url) = Url::parse(&format!("http://127.0.0.1{target}")) else {
        return CallbackTarget::Ignore;
    };
    if url.path() != CALLBACK_PATH {
        return CallbackTarget::Ignore;
    }
    let param = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if param("state").as_deref() != Some(expected_state) {
        return CallbackTarget::Ignore;
    }
    if param("error").is_some() {
        return CallbackTarget::Error;
    }
    match param("code") {
        Some(code) if !code.is_empty() => CallbackTarget::Code(code),
        _ => CallbackTarget::Ignore,
    }
}

async fn wait_for_callback(
    listener: TcpListener,
    expected_state: &str,
    cancel: CancellationToken,
    timeout: Duration,
    read_timeout: Duration,
) -> Result<String, XPostError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let accepted = tokio::select! {
            _ = cancel.cancelled() => return Err(XPostError::new(XPostErrorCode::Cancelled)),
            result = tokio::time::timeout_at(deadline, listener.accept()) => result,
        };
        let (mut stream, _) = match accepted {
            Err(_) => return Err(XPostError::new(XPostErrorCode::LoginTimeout)),
            Ok(Ok(pair)) => pair,
            Ok(Err(_)) => continue,
        };

        let mut buffer = [0_u8; 8192];
        let read = tokio::select! {
            _ = cancel.cancelled() => return Err(XPostError::new(XPostErrorCode::Cancelled)),
            result = tokio::time::timeout(read_timeout, stream.read(&mut buffer)) => result,
        };
        let read = match read {
            // 1 接続の読み取りが遅いだけなら捨てて待ち続ける。
            Err(_) => continue,
            Ok(Ok(0)) | Ok(Err(_)) => continue,
            Ok(Ok(read)) => read,
        };
        let request = String::from_utf8_lossy(&buffer[..read]);
        let mut parts = request
            .lines()
            .next()
            .unwrap_or_default()
            .split_whitespace();
        let method = parts.next().unwrap_or_default();
        let target = parts.next().unwrap_or_default();

        if method != "GET" {
            respond(
                &mut stream,
                "404 Not Found",
                "認証コールバックを待っています",
            )
            .await;
            continue;
        }
        match parse_callback_target(target, expected_state) {
            CallbackTarget::Ignore => {
                respond(
                    &mut stream,
                    "404 Not Found",
                    "認証コールバックを待っています",
                )
                .await;
            }
            CallbackTarget::Error => {
                respond(
                    &mut stream,
                    "400 Bad Request",
                    "ログインできませんでした。このタブを閉じてください",
                )
                .await;
                return Err(XPostError::new(XPostErrorCode::Other));
            }
            CallbackTarget::Code(code) => {
                respond(
                    &mut stream,
                    "200 OK",
                    "ログインしました。このタブを閉じてください",
                )
                .await;
                return Ok(code);
            }
        }
    }
}

async fn respond(stream: &mut TcpStream, status: &str, message: &str) {
    let body = format!(
        "<!doctype html><html lang=\"ja\"><meta charset=\"utf-8\"><title>fast-comment</title><body style=\"font-family:system-ui;padding:2rem;background:#111;color:#eee\"><h1>fast-comment</h1><p>{message}</p></body></html>"
    );
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

#[cfg(test)]
use std::collections::VecDeque;

#[cfg(test)]
#[derive(Default)]
struct MemInner {
    token: Option<StoredXToken>,
    fail_save: bool,
    fail_load: bool,
    save_calls: usize,
    load_queue: VecDeque<Result<Option<StoredXToken>, ()>>,
}

#[cfg(test)]
pub(crate) struct MemoryTokenStore {
    inner: StdMutex<MemInner>,
}

#[cfg(test)]
impl MemoryTokenStore {
    pub(crate) fn new() -> Self {
        Self {
            inner: StdMutex::new(MemInner::default()),
        }
    }

    pub(crate) fn with_token(token: StoredXToken) -> Self {
        let store = Self::new();
        store.set_token(Some(token));
        store
    }

    pub(crate) fn set_token(&self, token: Option<StoredXToken>) {
        self.inner.lock().unwrap().token = token;
    }

    pub(crate) fn token(&self) -> Option<StoredXToken> {
        self.inner.lock().unwrap().token.clone()
    }

    pub(crate) fn set_fail_save(&self, fail: bool) {
        self.inner.lock().unwrap().fail_save = fail;
    }

    pub(crate) fn set_fail_load(&self, fail: bool) {
        self.inner.lock().unwrap().fail_load = fail;
    }

    pub(crate) fn save_calls(&self) -> usize {
        self.inner.lock().unwrap().save_calls
    }

    pub(crate) fn push_load(&self, token: Option<StoredXToken>) {
        self.inner.lock().unwrap().load_queue.push_back(Ok(token));
    }

    /// 次の読み込みを 1 回だけ失敗させる(`push_load` と順番を共有する)。
    pub(crate) fn push_load_error(&self) {
        self.inner.lock().unwrap().load_queue.push_back(Err(()));
    }
}

#[cfg(test)]
impl TokenStore for MemoryTokenStore {
    async fn load(&self) -> Result<Option<StoredXToken>, String> {
        let mut inner = self.inner.lock().unwrap();
        if inner.fail_load {
            return Err("load failed".to_string());
        }
        if let Some(next) = inner.load_queue.pop_front() {
            return next.map_err(|_| "load failed".to_string());
        }
        Ok(inner.token.clone())
    }

    async fn save(&self, token: &StoredXToken) -> Result<(), String> {
        let mut inner = self.inner.lock().unwrap();
        inner.save_calls += 1;
        if inner.fail_save {
            return Err("save failed".to_string());
        }
        inner.token = Some(token.clone());
        Ok(())
    }

    async fn delete(&self) -> Result<(), String> {
        self.inner.lock().unwrap().token = None;
        Ok(())
    }
}

#[cfg(test)]
impl TokenStore for std::sync::Arc<MemoryTokenStore> {
    async fn load(&self) -> Result<Option<StoredXToken>, String> {
        TokenStore::load(&**self).await
    }

    async fn save(&self, token: &StoredXToken) -> Result<(), String> {
        TokenStore::save(&**self, token).await
    }

    async fn delete(&self) -> Result<(), String> {
        TokenStore::delete(&**self).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopost::error::XPostErrorCode;
    use axum::{
        extract::State,
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use serde_json::{json, Value};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;
    use std::sync::Arc;

    #[derive(Default)]
    struct OauthServer {
        token_forms: StdMutex<Vec<String>>,
        token_headers: StdMutex<Vec<Option<String>>>,
        token_calls: AtomicUsize,
        refresh_calls: AtomicUsize,
        users_calls: AtomicUsize,
        token_queue: StdMutex<VecDeque<(u16, Value)>>,
        revoke_forms: StdMutex<Vec<(String, Option<String>)>>,
        /// token エンドポイントの応答を遅らせる(ms)。
        token_delay_ms: std::sync::atomic::AtomicU64,
    }

    async fn revoke_handler(
        State(server): State<Arc<OauthServer>>,
        headers: HeaderMap,
        body: String,
    ) -> StatusCode {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        server.revoke_forms.lock().unwrap().push((body, auth));
        StatusCode::OK
    }

    async fn token_handler(
        State(server): State<Arc<OauthServer>>,
        headers: HeaderMap,
        body: String,
    ) -> (StatusCode, Json<Value>) {
        server.token_calls.fetch_add(1, Ordering::SeqCst);
        let delay = server.token_delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay)).await;
        }
        server.token_forms.lock().unwrap().push(body.clone());
        server.token_headers.lock().unwrap().push(
            headers
                .get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string),
        );
        if body.contains("grant_type=refresh_token") {
            server.refresh_calls.fetch_add(1, Ordering::SeqCst);
        }
        match server.token_queue.lock().unwrap().pop_front() {
            Some((status, value)) => (StatusCode::from_u16(status).unwrap(), Json(value)),
            None => (
                StatusCode::OK,
                Json(json!({
                    "access_token": "access-1",
                    "refresh_token": "refresh-next",
                    "expires_in": 7200
                })),
            ),
        }
    }

    async fn users_me_handler(State(server): State<Arc<OauthServer>>) -> (StatusCode, Json<Value>) {
        server.users_calls.fetch_add(1, Ordering::SeqCst);
        (
            StatusCode::OK,
            Json(json!({"data": {"id": "42", "username": "alice"}})),
        )
    }

    async fn spawn_oauth_server() -> (u16, Arc<OauthServer>) {
        let server = Arc::new(OauthServer::default());
        let app = Router::new()
            .route("/token", post(token_handler))
            .route("/users/me", get(users_me_handler))
            .route("/revoke", post(revoke_handler))
            .with_state(server.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (port, server)
    }

    fn endpoints_for(port: u16) -> XEndpoints {
        XEndpoints {
            authorize: format!("http://127.0.0.1:{port}/authorize"),
            token: format!("http://127.0.0.1:{port}/token"),
            tweets: format!("http://127.0.0.1:{port}/tweets"),
            users_me: format!("http://127.0.0.1:{port}/users/me"),
        }
    }

    async fn free_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        port
    }

    fn stored(client_id: &str, refresh: &str) -> StoredXToken {
        StoredXToken {
            client_id: client_id.to_string(),
            refresh_token: refresh.to_string(),
            user_id: "42".to_string(),
            username: "alice".to_string(),
        }
    }

    fn state_of(url: &str) -> String {
        Url::parse(url)
            .unwrap()
            .query_pairs()
            .find(|(key, _)| key == "state")
            .map(|(_, value)| value.into_owned())
            .unwrap()
    }

    #[test]
    fn authorize_url_has_pkce_scopes_and_no_secret() {
        let url = build_authorize_url(&XEndpoints::default(), "client-1", 12345, "st", "ch");
        let parsed = Url::parse(&url).unwrap();
        let params: HashMap<String, String> = parsed
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert_eq!(params["response_type"], "code");
        assert_eq!(params["client_id"], "client-1");
        assert_eq!(params["redirect_uri"], "http://127.0.0.1:12345/callback");
        assert_eq!(
            params["scope"],
            "tweet.read tweet.write users.read offline.access"
        );
        assert_eq!(params["state"], "st");
        assert_eq!(params["code_challenge"], "ch");
        assert_eq!(params["code_challenge_method"], "S256");
        assert!(!params.contains_key("client_secret"));
        assert!(!url.contains("client_secret"));
    }

    #[test]
    fn endpoints_default_and_overrides() {
        let default = XEndpoints::default();
        assert_eq!(default.authorize, "https://x.com/i/oauth2/authorize");
        assert_eq!(default.token, "https://api.x.com/2/oauth2/token");
        assert_eq!(default.tweets, "https://api.x.com/2/tweets");
        assert_eq!(default.users_me, "https://api.x.com/2/users/me");

        let mut map = HashMap::new();
        map.insert("postAuthorizeUrl".to_string(), "http://a".to_string());
        map.insert("postTokenUrl".to_string(), "   ".to_string());
        map.insert("postTweetsUrl".to_string(), "http://t".to_string());
        map.insert("postUsersMeUrl".to_string(), "http://u".to_string());
        let overridden = XEndpoints::with_overrides(&map);
        assert_eq!(overridden.authorize, "http://a");
        assert_eq!(overridden.token, default.token);
        assert_eq!(overridden.tweets, "http://t");
        assert_eq!(overridden.users_me, "http://u");
    }

    #[tokio::test]
    async fn connect_exchanges_code_and_reports_username() {
        let (port, server) = spawn_oauth_server().await;
        let callback_port = free_port().await;
        let store = Arc::new(MemoryTokenStore::new());
        let auth = XPostAuth::new(store.clone(), endpoints_for(port))
            .unwrap()
            .with_login_timeout(Duration::from_secs(5));

        let wrong_status = Arc::new(AtomicUsize::new(0));
        let wrong = wrong_status.clone();
        let status = auth
            .connect("client-1", callback_port, move |url| {
                let state = state_of(&url);
                tokio::spawn(async move {
                    let client = reqwest::Client::new();
                    let response = client
                        .get(format!(
                            "http://127.0.0.1:{callback_port}/callback?state=wrong&code=bad"
                        ))
                        .send()
                        .await
                        .unwrap();
                    wrong.store(response.status().as_u16() as usize, Ordering::SeqCst);
                    client
                        .get(format!(
                            "http://127.0.0.1:{callback_port}/callback?state={state}&code=good-code"
                        ))
                        .send()
                        .await
                        .unwrap();
                });
                Ok(())
            })
            .await
            .unwrap();

        assert!(status.connected);
        assert!(status.configured);
        assert_eq!(status.username.as_deref(), Some("alice"));
        assert_eq!(
            status.callback_url,
            format!("http://127.0.0.1:{callback_port}/callback")
        );
        assert_eq!(wrong_status.load(Ordering::SeqCst), 404);
        assert_eq!(server.users_calls.load(Ordering::SeqCst), 1);

        let saved = store.token().unwrap();
        assert_eq!(saved.client_id, "client-1");
        assert_eq!(saved.refresh_token, "refresh-next");
        assert_eq!(saved.user_id, "42");

        let forms = server.token_forms.lock().unwrap();
        assert_eq!(forms.len(), 1);
        assert!(forms[0].contains("grant_type=authorization_code"));
        assert!(forms[0].contains("client_id=client-1"));
        assert!(forms[0].contains("code=good-code"));
        assert!(forms[0].contains("code_verifier="));
        assert!(server.token_headers.lock().unwrap()[0].is_none());
    }

    #[tokio::test]
    async fn callback_error_param_fails_connect() {
        let (port, _server) = spawn_oauth_server().await;
        let callback_port = free_port().await;
        let auth = XPostAuth::new(MemoryTokenStore::new(), endpoints_for(port))
            .unwrap()
            .with_login_timeout(Duration::from_secs(5));
        let error = auth
            .connect("client-1", callback_port, move |url| {
                let state = state_of(&url);
                tokio::spawn(async move {
                    let _ = reqwest::get(format!(
                        "http://127.0.0.1:{callback_port}/callback?state={state}&error=access_denied"
                    ))
                    .await;
                });
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Other);
    }

    #[tokio::test]
    async fn occupied_port_returns_port_in_use() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let auth = XPostAuth::new(MemoryTokenStore::new(), XEndpoints::default()).unwrap();
        let error = auth
            .connect("client-1", port, |_| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::PortInUse);
        drop(listener);
    }

    #[tokio::test]
    async fn login_times_out_or_cancels_and_port_is_reusable() {
        let port = free_port().await;
        let auth = XPostAuth::new(MemoryTokenStore::new(), XEndpoints::default())
            .unwrap()
            .with_login_timeout(Duration::from_millis(200));
        let error = auth
            .connect("client-1", port, |_| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::LoginTimeout);

        let auth = XPostAuth::new(MemoryTokenStore::new(), XEndpoints::default())
            .unwrap()
            .with_login_timeout(Duration::from_secs(5));
        let connect = auth.connect("client-1", port, |_| Ok(()));
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            auth.cancel_login();
        };
        let (result, ()) = tokio::join!(connect, cancel);
        assert_eq!(result.unwrap_err().code, XPostErrorCode::Cancelled);

        // 前の listener が閉じているので同じポートへすぐ bind できる。
        let auth = XPostAuth::new(MemoryTokenStore::new(), XEndpoints::default())
            .unwrap()
            .with_login_timeout(Duration::from_millis(200));
        let error = auth
            .connect("client-1", port, |_| Ok(()))
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::LoginTimeout);
    }

    #[tokio::test]
    async fn refresh_rotates_refresh_token_and_uses_latest() {
        let (port, server) = spawn_oauth_server().await;
        server.token_queue.lock().unwrap().push_back((
            200,
            json!({"access_token": "a2", "refresh_token": "r2", "expires_in": 7200}),
        ));
        server.token_queue.lock().unwrap().push_back((
            200,
            json!({"access_token": "a3", "refresh_token": "r3", "expires_in": 7200}),
        ));
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();

        assert_eq!(auth.access_token("client-1").await.unwrap(), "a2");
        assert_eq!(store.token().unwrap().refresh_token, "r2");

        auth.invalidate_access_token().await;
        assert_eq!(auth.access_token("client-1").await.unwrap(), "a3");
        assert_eq!(store.token().unwrap().refresh_token, "r3");

        let forms = server.token_forms.lock().unwrap();
        assert_eq!(forms.len(), 2);
        assert!(forms[0].contains("refresh_token=r1"));
        assert!(forms[1].contains("refresh_token=r2"));
        assert!(server
            .token_headers
            .lock()
            .unwrap()
            .iter()
            .all(|header| header.is_none()));
    }

    #[tokio::test]
    async fn unsaved_refresh_is_retried_on_next_call() {
        let (port, _server) = spawn_oauth_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        store.set_fail_save(true);
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();

        assert_eq!(auth.access_token("client-1").await.unwrap(), "access-1");
        let status = auth.status("client-1", 0).await;
        assert!(status.connected);
        assert!(status.unsaved_token);

        store.set_fail_save(false);
        let calls_before = store.save_calls();
        assert_eq!(auth.access_token("client-1").await.unwrap(), "access-1");
        assert!(store.save_calls() > calls_before);
        assert_eq!(store.token().unwrap().refresh_token, "refresh-next");
        assert!(!auth.status("client-1", 0).await.unsaved_token);
    }

    #[tokio::test]
    async fn rejected_refresh_retries_with_newer_stored_token() {
        let (port, server) = spawn_oauth_server().await;
        server
            .token_queue
            .lock()
            .unwrap()
            .push_back((400, json!({"error": "invalid_grant"})));
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        store.push_load(Some(stored("client-1", "r1")));
        store.push_load(Some(stored("client-1", "r2")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();

        assert_eq!(auth.access_token("client-1").await.unwrap(), "access-1");
        let forms = server.token_forms.lock().unwrap();
        assert_eq!(forms.len(), 2);
        assert!(forms[0].contains("refresh_token=r1"));
        assert!(forms[1].contains("refresh_token=r2"));
        assert_eq!(store.token().unwrap().refresh_token, "refresh-next");
    }

    #[tokio::test]
    async fn rejected_refresh_with_same_token_signs_out() {
        let (port, server) = spawn_oauth_server().await;
        server
            .token_queue
            .lock()
            .unwrap()
            .push_back((400, json!({"error": "invalid_grant"})));
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();

        let error = auth.access_token("client-1").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::ReloginRequired);
        assert!(store.token().is_none());
        assert_eq!(server.refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn refresh_server_error_keeps_stored_token() {
        let (port, server) = spawn_oauth_server().await;
        server
            .token_queue
            .lock()
            .unwrap()
            .push_back((500, json!({"error": "boom"})));
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();

        let error = auth.access_token("client-1").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Network);
        assert_eq!(store.token().unwrap().refresh_token, "r1");
    }

    #[tokio::test]
    async fn concurrent_access_token_refreshes_once() {
        let (port, server) = spawn_oauth_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store, endpoints_for(port)).unwrap();

        let (first, second) =
            tokio::join!(auth.access_token("client-1"), auth.access_token("client-1"));
        assert_eq!(first.unwrap(), "access-1");
        assert_eq!(second.unwrap(), "access-1");
        assert_eq!(server.refresh_calls.load(Ordering::SeqCst), 1);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn disconnect_clears_memory_and_store_without_revoking() {
        let (port, server) = spawn_oauth_server().await;
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();
        auth.access_token("client-1").await.unwrap();
        assert!(auth.status("client-1", 0).await.connected);

        auth.disconnect().await.unwrap();
        assert!(!auth.status("client-1", 0).await.connected);
        assert!(store.token().is_none());
        let error = auth.access_token("client-1").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::NotLoggedIn);
        // 設計 §5.3: 切断はローカルの保存を消すだけで、X 側の許可は取り消さない。
        assert!(server.revoke_forms.lock().unwrap().is_empty());
    }

    /// callback 受信後のコード交換中に取り消すと、保存せず Cancelled で終わる。
    #[tokio::test]
    async fn cancel_during_code_exchange_discards_login() {
        let (port, server) = spawn_oauth_server().await;
        server.token_delay_ms.store(600, Ordering::SeqCst);
        let callback_port = free_port().await;
        let store = Arc::new(MemoryTokenStore::new());
        let auth = Arc::new(
            XPostAuth::new(store.clone(), endpoints_for(port))
                .unwrap()
                .with_login_timeout(Duration::from_secs(5)),
        );
        let server_for_cancel = server.clone();
        let auth_for_cancel = auth.clone();
        tokio::spawn(async move {
            // 交換リクエストが届いてから取り消す。
            for _ in 0..200 {
                if server_for_cancel.token_calls.load(Ordering::SeqCst) > 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            auth_for_cancel.cancel_login();
        });
        let error = auth
            .connect("client-1", callback_port, move |url| {
                let state = state_of(&url);
                tokio::spawn(async move {
                    let _ = reqwest::Client::new()
                        .get(format!(
                            "http://127.0.0.1:{callback_port}/callback?state={state}&code=good-code"
                        ))
                        .send()
                        .await;
                });
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Cancelled);
        assert_eq!(server.token_calls.load(Ordering::SeqCst), 1);
        assert!(store.token().is_none());
        assert!(!auth.status("client-1", callback_port).await.connected);
    }

    /// refresh の通信中でも status は inner の lock を待たずに返る。
    #[tokio::test]
    async fn status_does_not_wait_for_slow_refresh() {
        let (port, server) = spawn_oauth_server().await;
        server.token_delay_ms.store(1500, Ordering::SeqCst);
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = Arc::new(XPostAuth::new(store.clone(), endpoints_for(port)).unwrap());
        let refreshing = auth.clone();
        let refresh = tokio::spawn(async move { refreshing.access_token("client-1").await });
        for _ in 0..200 {
            if server.token_calls.load(Ordering::SeqCst) > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let status = tokio::time::timeout(Duration::from_millis(500), auth.status("client-1", 0))
            .await
            .expect("status は refresh を待たない");
        assert!(status.connected);
        assert_eq!(status.username.as_deref(), Some("alice"));
        refresh.await.unwrap().unwrap();
        assert!(auth.status("client-1", 0).await.connected);
    }

    #[tokio::test]
    async fn rejected_refresh_keeps_store_when_reload_fails() {
        let (port, server) = spawn_oauth_server().await;
        server
            .token_queue
            .lock()
            .unwrap()
            .push_back((400, json!({"error": "invalid_grant"})));
        let store = Arc::new(MemoryTokenStore::with_token(stored("client-1", "r1")));
        let auth = XPostAuth::new(store.clone(), endpoints_for(port)).unwrap();
        // 1 回目(メモリが空なので保存から読む)は成功、拒否後の読み直しは失敗。
        store.push_load(Some(stored("client-1", "r1")));
        store.push_load_error();
        let error = auth.access_token("client-1").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::TokenStore);
        assert_eq!(store.token().unwrap().refresh_token, "r1");
        assert_eq!(server.refresh_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn network_failure_during_code_exchange_is_network() {
        let token_port = free_port().await;
        let store = Arc::new(MemoryTokenStore::new());
        let auth = XPostAuth::new(store.clone(), endpoints_for(token_port))
            .unwrap()
            .with_login_timeout(Duration::from_secs(5));
        let port = free_port().await;
        let error = auth
            .connect("client-1", port, |url| {
                let state = state_of(&url);
                tokio::spawn(async move {
                    let target = format!("http://127.0.0.1:{port}/callback?state={state}&code=c1");
                    for _ in 0..50 {
                        if reqwest::get(&target).await.is_ok() {
                            break;
                        }
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                });
                Ok(())
            })
            .await
            .unwrap_err();
        assert_eq!(error.code, XPostErrorCode::Network);
        assert!(store.token().is_none());
    }

    #[tokio::test]
    async fn store_load_failure_is_reported_safely() {
        let store = Arc::new(MemoryTokenStore::new());
        store.set_fail_load(true);
        let auth = XPostAuth::new(store, XEndpoints::default()).unwrap();
        assert!(!auth.status("client-1", 0).await.connected);
        let error = auth.access_token("client-1").await.unwrap_err();
        assert_eq!(error.code, XPostErrorCode::TokenStore);
    }

    #[test]
    fn debug_and_errors_do_not_leak_tokens() {
        let token = StoredXToken {
            client_id: "client-1".to_string(),
            refresh_token: "super-secret-refresh".to_string(),
            user_id: "42".to_string(),
            username: "alice".to_string(),
        };
        let debug = format!("{token:?}");
        assert!(!debug.contains("super-secret-refresh"));
        assert!(debug.contains("<redacted>"));

        let auth =
            XPostAuth::new(MemoryTokenStore::with_token(token), XEndpoints::default()).unwrap();
        assert!(!format!("{auth:?}").contains("super-secret-refresh"));

        let error = XPostError::new(XPostErrorCode::ReloginRequired);
        assert!(!format!("{error} {error:?}").contains("super-secret-refresh"));
    }
}
