//! Twitch の Device Code Flow 認証。
//!
//! access token はメモリだけに保持し、refresh token と利用者情報だけを OS の
//! 資格情報ストアへ JSON で保存する。常駐の更新タスクは持たず、操作時に必要なら
//! refresh / validate する。応答は固い struct にせず `serde_json::Value` で読む。

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::moderation_error::{ModerationError, ModerationErrorCode};
use crate::secret_store;

const DEVICE_URL: &str = "https://id.twitch.tv/oauth2/device";
const TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const KEYRING_USER: &str = "twitch-oauth-refresh-token";
const DEVICE_GRANT_TYPE: &str = "urn:ietf:params:oauth:grant-type:device_code";
const REQUIRED_SCOPES: [&str; 2] = [
    "moderator:manage:banned_users",
    "moderator:manage:chat_messages",
];
const VALIDATE_TTL: Duration = Duration::from_secs(60 * 60);
const DEFAULT_ACCESS_TTL_SECS: u64 = 4 * 60 * 60;
const DEFAULT_DEVICE_EXPIRES_SECS: u64 = 1800;
const DEFAULT_POLL_INTERVAL_SECS: u64 = 5;

/// `begin_twitch_login` が UI に返すデバイス認証の案内。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TwitchDeviceLogin {
    pub user_code: String,
    pub verification_uri: String,
    pub expires_in_secs: u64,
}

/// Twitch モデレーション用 OAuth の状態。
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TwitchOauthStatus {
    pub configured: bool,
    pub connected: bool,
    pub login: Option<String>,
    pub scopes_ok: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct StoredTwitchToken {
    pub client_id: String,
    pub refresh_token: String,
    pub user_id: String,
    #[serde(default)]
    pub login: String,
    #[serde(default)]
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for StoredTwitchToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredTwitchToken")
            .field("client_id", &self.client_id)
            .field("refresh_token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("login", &self.login)
            .field("scopes", &self.scopes)
            .finish()
    }
}

/// Helix 呼び出しに必要な認証済み情報。
#[derive(Clone)]
pub(super) struct TwitchAccess {
    pub client_id: String,
    pub token: String,
    pub user_id: String,
    pub login: String,
    pub scopes: Vec<String>,
}

impl std::fmt::Debug for TwitchAccess {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TwitchAccess")
            .field("client_id", &self.client_id)
            .field("token", &"<redacted>")
            .field("user_id", &self.user_id)
            .field("login", &self.login)
            .field("scopes", &self.scopes)
            .finish()
    }
}

#[derive(Clone)]
struct CachedAccessToken {
    client_id: String,
    access: TwitchAccess,
    valid_until: Instant,
    /// キャッシュした時点の認証世代。世代が進んだら使わない。
    generation: u64,
}

impl CachedAccessToken {
    fn usable(&self, client_id: &str, generation: u64, now: Instant) -> bool {
        self.client_id == client_id && self.generation == generation && self.valid_until > now
    }
}

/// 認証世代と切断フラグの小さな状態機械。
///
/// 切断・refresh 拒否で `sign_out`、client_id 変更で `invalidate`、ログイン保存成功で
/// `sign_in` する。どれも世代を進めるので、それ以前に始まった refresh / ログインは保存しない。
/// `signed_out` は keyring の削除に失敗しても、メモリ上では切断として扱うために持つ。
#[derive(Debug, Default)]
struct AuthEpoch {
    generation: u64,
    signed_out: bool,
}

impl AuthEpoch {
    fn sign_out(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.signed_out = true;
    }

    fn invalidate(&mut self) {
        self.generation = self.generation.wrapping_add(1);
    }

    /// 新しいログインを確定し、その世代を返す。
    fn sign_in(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.signed_out = false;
        self.generation
    }

    /// `ticket` の時点から切断・変更が無く、refresh 結果を保存・使用してよいか。
    fn may_commit(&self, ticket: u64) -> bool {
        !self.signed_out && is_current_generation(ticket, self.generation)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum LoginPhase {
    #[default]
    Pending,
    Committed,
    Cancelled,
}

/// デバイス認証 1 件ごとの取消・置換・確定を決める状態機械。
///
/// 取消(`cancel`)・置換(`register`)と確定(`commit`)を同じ同期 lock の下で行い、
/// keyring 保存中に取り消されたログインは確定させない。確定後の取消は何もしない。
#[derive(Debug, Default)]
struct LoginGate {
    /// 最後に登録したログインの番号。これ以外の番号は置換済み。
    current: u64,
    phase: LoginPhase,
}

impl LoginGate {
    /// 新しいログインを登録し、その番号を返す。待機中の旧ログインは置換で無効になる。
    fn register(&mut self) -> u64 {
        self.current = self.current.wrapping_add(1);
        self.phase = LoginPhase::Pending;
        self.current
    }

    /// 待機中のログインを取り消す。確定済みなら何もしない。
    fn cancel(&mut self) {
        if self.phase == LoginPhase::Pending {
            self.phase = LoginPhase::Cancelled;
        }
    }

    /// `ticket` のログインが取消・置換されておらず、まだ確定もしていないか。
    fn is_live(&self, ticket: u64) -> bool {
        self.current == ticket && self.phase == LoginPhase::Pending
    }

    /// `ticket` のログインを確定する。取消・置換済みなら false。
    fn commit(&mut self, ticket: u64) -> bool {
        if !self.is_live(ticket) {
            return false;
        }
        self.phase = LoginPhase::Committed;
        true
    }
}

/// ログイン確定の結果。
#[derive(Debug, PartialEq, Eq)]
enum LoginCommit {
    /// 確定した。値は新しい認証世代。
    Committed(u64),
    /// 取消・置換された。
    Cancelled,
    /// 待機中に切断・client_id 変更があった。
    Changed,
}

/// refresh で回転した認証のうち、keyring へ保存できていないもの(「保存が未完了」フラグ)。
///
/// 保存に失敗したら `mark` し、次回の操作の冒頭で `due` を再保存する。成功したら `saved`。
/// 切断・client_id 変更で世代が進んだものは `due` が捨てる。
#[derive(Debug, Default)]
struct UnsavedRefresh {
    pending: Option<(u64, StoredTwitchToken)>,
}

impl UnsavedRefresh {
    fn mark(&mut self, generation: u64, token: StoredTwitchToken) {
        self.pending = Some((generation, token));
    }

    /// 現在の世代で再保存すべき認証。世代が違えば捨てて None を返す。
    fn due(&mut self, generation: u64) -> Option<StoredTwitchToken> {
        match &self.pending {
            Some((saved_at, token)) if is_current_generation(*saved_at, generation) => {
                Some(token.clone())
            }
            Some(_) => {
                self.pending = None;
                None
            }
            None => None,
        }
    }

    /// `generation` の認証を保存できた。
    fn saved(&mut self, generation: u64) {
        if self
            .pending
            .as_ref()
            .is_some_and(|(saved_at, _)| is_current_generation(*saved_at, generation))
        {
            self.pending = None;
        }
    }
}

/// 同じデバイス認証を二重に待たないための印。drop で解除する。
struct AwaitingLogin<'a> {
    slot: &'a std::sync::Mutex<Option<String>>,
    device_code: String,
}

impl Drop for AwaitingLogin<'_> {
    fn drop(&mut self) {
        let mut slot = self.slot.lock().unwrap_or_else(|e| e.into_inner());
        if slot.as_deref() == Some(self.device_code.as_str()) {
            *slot = None;
        }
    }
}

#[derive(Clone)]
struct ActiveLogin {
    client_id: String,
    device_code: String,
    interval: Duration,
    expires_at: Instant,
    cancel: CancellationToken,
    /// `LoginGate` に登録した番号。
    ticket: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DevicePoll {
    Authorized,
    Pending,
    SlowDown,
    Failed(String),
}

#[derive(Debug)]
enum ValidateFailure {
    Unauthorized,
    Other(String),
}

struct ValidateInfo {
    user_id: String,
    login: String,
    scopes: Vec<String>,
}

/// Twitch 認証専用の軽量状態。バックグラウンドタスクは持たない。
pub struct TwitchAuth {
    pub(super) http: reqwest::Client,
    access_token: Mutex<Option<CachedAccessToken>>,
    login: Mutex<Option<ActiveLogin>>,
    /// begin 同士を直列化し、複数のブラウザ認証を同時に走らせない。cancel は取らない。
    login_lock: Mutex<()>,
    /// cancel の回数。begin は HTTP 前後で比べ、途中で取り消されていたら登録しない。
    cancel_count: AtomicU64,
    /// await 中のデバイスコード。同じログインを二重に待たない。
    awaiting: std::sync::Mutex<Option<String>>,
    /// 最後に validate が成功した (client_id, 時刻)。
    validated: Mutex<Option<(String, Instant)>>,
    /// client_id の変更検出用。変更時は認証世代を進めてキャッシュを捨てる。
    current_client: Mutex<Option<String>>,
    /// 認証世代と切断フラグ。await をまたいで保持しない。
    epoch: std::sync::Mutex<AuthEpoch>,
    /// ログインの取消・置換・確定。lock 順は gate → epoch。await をまたいで保持しない。
    login_gate: std::sync::Mutex<LoginGate>,
    /// refresh で回転した後、keyring へ保存できていない認証。access_token の lock 内で扱う。
    unsaved: std::sync::Mutex<UnsavedRefresh>,
}

impl TwitchAuth {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            // OAuth token endpoints must not follow redirects automatically.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("Twitch認証用HTTPクライアントの作成に失敗しました: {e}"))?;

        Ok(Self {
            http,
            access_token: Mutex::new(None),
            login: Mutex::new(None),
            login_lock: Mutex::new(()),
            cancel_count: AtomicU64::new(0),
            awaiting: std::sync::Mutex::new(None),
            validated: Mutex::new(None),
            current_client: Mutex::new(None),
            epoch: std::sync::Mutex::new(AuthEpoch::default()),
            login_gate: std::sync::Mutex::new(LoginGate::default()),
            unsaved: std::sync::Mutex::new(UnsavedRefresh::default()),
        })
    }

    fn epoch(&self) -> std::sync::MutexGuard<'_, AuthEpoch> {
        self.epoch.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn gate(&self) -> std::sync::MutexGuard<'_, LoginGate> {
        self.login_gate.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn unsaved(&self) -> std::sync::MutexGuard<'_, UnsavedRefresh> {
        self.unsaved.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// keyring 保存後、`ticket` のログインを確定する。取消・置換との判定は gate の lock 内で行う。
    fn try_commit_login(&self, ticket: u64, generation: u64) -> LoginCommit {
        let mut gate = self.gate();
        if !gate.is_live(ticket) {
            return LoginCommit::Cancelled;
        }
        let mut epoch = self.epoch();
        if !is_current_generation(generation, epoch.generation) {
            gate.cancel();
            return LoginCommit::Changed;
        }
        gate.commit(ticket);
        LoginCommit::Committed(epoch.sign_in())
    }

    pub fn generation(&self) -> u64 {
        self.epoch().generation
    }

    /// 保存済み認証を keyring から消す。失敗しても切断フラグは立てたまま warn に残す。
    async fn forget_stored(&self, context: &str) -> Result<(), String> {
        self.epoch().sign_out();
        let result = secret_store::delete(KEYRING_USER).await;
        if let Err(error) = &result {
            tracing::warn!("Twitch認証の削除に失敗しました({context}): {error}");
        }
        result
    }

    /// client_id が前回と変わっていれば未接続扱いにし、世代を進めてキャッシュを捨てる。
    async fn observe_client(&self, client_id: &str) {
        let client_id = client_id.trim();
        let changed = {
            let mut current = self.current_client.lock().await;
            match current.as_deref() {
                Some(previous) if previous == client_id => false,
                Some(_) => {
                    *current = Some(client_id.to_string());
                    true
                }
                None => {
                    *current = Some(client_id.to_string());
                    false
                }
            }
        };
        if changed {
            self.epoch().invalidate();
            *self.access_token.lock().await = None;
            *self.validated.lock().await = None;
        }
    }

    pub async fn status(&self, client_id: &str) -> Result<TwitchOauthStatus, String> {
        let client_id = client_id.trim();
        self.observe_client(client_id).await;
        if client_id.is_empty() {
            return Ok(TwitchOauthStatus {
                configured: false,
                connected: false,
                login: None,
                scopes_ok: false,
            });
        }
        if self.epoch().signed_out {
            // keyring の削除に失敗して残っていても、アプリ内では切断済みとして扱う。
            return Ok(TwitchOauthStatus {
                configured: true,
                connected: false,
                login: None,
                scopes_ok: false,
            });
        }

        match secret_store::load::<StoredTwitchToken>(KEYRING_USER).await? {
            Some(stored) if stored.client_id == client_id => Ok(status_from_stored(&stored)),
            _ => Ok(TwitchOauthStatus {
                configured: true,
                connected: false,
                login: None,
                scopes_ok: false,
            }),
        }
    }

    /// デバイス認証を開始し、UI に表示するコードと URI を返す。ブラウザは呼び出し側が開く。
    pub async fn begin_login(&self, client_id: &str) -> Result<TwitchDeviceLogin, String> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err("設定でTwitchのクライアントIDを入力してください".to_string());
        }
        self.observe_client(client_id).await;

        let _guard = self.login_lock.lock().await;
        let cancel_count = self.cancel_count.load(Ordering::SeqCst);
        // 進行中の認証があれば取り消して置き換える。保存中の旧ログインも確定させない。
        if let Some(previous) = self.login.lock().await.take() {
            self.gate().cancel();
            previous.cancel.cancel();
        }

        let scopes = REQUIRED_SCOPES.join(" ");
        let response = self
            .http
            .post(DEVICE_URL)
            .form(&[("client_id", client_id), ("scopes", scopes.as_str())])
            .send()
            .await
            .map_err(|e| format!("Twitchのデバイス認証を開始できませんでした: {e}"))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail =
                error_message(&body).unwrap_or_else(|| format!("Twitch API エラー ({status})"));
            return Err(format!("Twitchのデバイス認証を開始できませんでした: {detail}"));
        }
        let value = serde_json::from_str::<Value>(&body)
            .map_err(|e| format!("Twitchの応答を解析できませんでした: {e}"))?;
        let device_code = string_field(&value, "device_code")
            .ok_or_else(|| "Twitchからデバイスコードが返りませんでした".to_string())?;
        let user_code = string_field(&value, "user_code")
            .ok_or_else(|| "Twitchからユーザーコードが返りませんでした".to_string())?;
        let verification_uri = string_field(&value, "verification_uri")
            .ok_or_else(|| "Twitchから認証URLが返りませんでした".to_string())?;
        let interval = value
            .get("interval")
            .and_then(json_u64)
            .unwrap_or(DEFAULT_POLL_INTERVAL_SECS)
            .max(1);
        let expires_in = value
            .get("expires_in")
            .and_then(json_u64)
            .unwrap_or(DEFAULT_DEVICE_EXPIRES_SECS);

        let cancel = CancellationToken::new();
        let mut active = self.login.lock().await;
        // デバイス認証の開始中に取消・切断された場合は登録しない。
        if !is_current_generation(cancel_count, self.cancel_count.load(Ordering::SeqCst)) {
            return Err("Twitchログインをキャンセルしました".to_string());
        }
        let ticket = self.gate().register();
        *active = Some(ActiveLogin {
            client_id: client_id.to_string(),
            device_code,
            interval: Duration::from_secs(interval),
            expires_at: Instant::now() + Duration::from_secs(expires_in),
            cancel,
            ticket,
        });

        Ok(TwitchDeviceLogin {
            user_code,
            verification_uri,
            expires_in_secs: expires_in,
        })
    }

    /// 開始済みのデバイス認証を完了・期限切れ・取消までポーリングする。
    pub async fn await_login(&self) -> Result<TwitchOauthStatus, String> {
        let login = self
            .login
            .lock()
            .await
            .clone()
            .ok_or_else(|| "Twitchログインが開始されていません".to_string())?;
        let _awaiting = {
            let mut slot = self.awaiting.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_deref() == Some(login.device_code.as_str()) {
                return Err("Twitchのログイン待機中です".to_string());
            }
            *slot = Some(login.device_code.clone());
            AwaitingLogin {
                slot: &self.awaiting,
                device_code: login.device_code.clone(),
            }
        };
        let generation = self.generation();
        let scopes = REQUIRED_SCOPES.join(" ");
        let mut interval = login.interval;

        loop {
            tokio::select! {
                _ = login.cancel.cancelled() => {
                    return Err("Twitchログインをキャンセルしました".to_string());
                }
                _ = tokio::time::sleep(interval) => {}
            }

            if Instant::now() >= login.expires_at {
                self.clear_login(&login.device_code).await;
                return Err(
                    "Twitchの認証コードの有効期限が切れました。もう一度ログインしてください"
                        .to_string(),
                );
            }

            let response = match self
                .http
                .post(TOKEN_URL)
                .form(&[
                    ("client_id", login.client_id.as_str()),
                    ("scopes", scopes.as_str()),
                    ("device_code", login.device_code.as_str()),
                    ("grant_type", DEVICE_GRANT_TYPE),
                ])
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    // 一時的な通信失敗は、認証コードの期限内なら次の間隔で再試行する。
                    tracing::warn!("Twitchのログイン状態を確認できませんでした(再試行します): {error}");
                    continue;
                }
            };
            let status = response.status();
            let body = match response.text().await {
                Ok(body) => body,
                Err(error) => {
                    // 本文の読取り失敗も一時的な通信失敗として扱い、取消・期限を確認しつつ継続する。
                    tracing::warn!("Twitchのログイン応答を読めませんでした(再試行します): {error}");
                    continue;
                }
            };

            match classify_device_poll(status, &body) {
                DevicePoll::Pending => continue,
                DevicePoll::SlowDown => {
                    interval = interval.saturating_add(Duration::from_secs(5));
                    continue;
                }
                DevicePoll::Failed(message) => {
                    self.clear_login(&login.device_code).await;
                    return Err(message);
                }
                DevicePoll::Authorized => {
                    let value = serde_json::from_str::<Value>(&body)
                        .map_err(|e| format!("Twitchの認証応答を解析できませんでした: {e}"))?;
                    let access_token = string_field(&value, "access_token")
                        .ok_or_else(|| "Twitchからアクセストークンが返りませんでした".to_string())?;
                    let refresh_token = string_field(&value, "refresh_token").ok_or_else(|| {
                        "Twitchから更新トークンが返りませんでした。もう一度ログインしてください"
                            .to_string()
                    })?;

                    // 待機中に新しい begin / cancel が走っていたら validate もしない。
                    if !self.is_active_login(&login.device_code).await {
                        return Err("Twitchログインをキャンセルしました".to_string());
                    }

                    let info = match self.validate_token(&access_token).await {
                        Ok(info) => info,
                        Err(ValidateFailure::Unauthorized) => {
                            self.clear_login(&login.device_code).await;
                            return Err(
                                "Twitchの認証確認に失敗しました。もう一度ログインしてください"
                                    .to_string(),
                            );
                        }
                        Err(ValidateFailure::Other(message)) => {
                            self.clear_login(&login.device_code).await;
                            return Err(message);
                        }
                    };
                    let stored = StoredTwitchToken {
                        client_id: login.client_id.clone(),
                        refresh_token,
                        user_id: info.user_id,
                        login: info.login,
                        scopes: info.scopes,
                    };

                    // 切断・refresh と直列化するため、確認から確定まで access_token の lock を持つ。
                    // 切断は先に cancel してからこの lock を取るので、確認後に切断が割り込んでも
                    // 切断側の削除はこの保存より後に走る。
                    let mut cache = self.access_token.lock().await;
                    if !self.is_active_login(&login.device_code).await {
                        return Err("Twitchログインをキャンセルしました".to_string());
                    }
                    if !is_current_generation(generation, self.generation()) {
                        self.clear_login(&login.device_code).await;
                        return Err(
                            "Twitchの接続情報が変更されました。もう一度ログインしてください"
                                .to_string(),
                        );
                    }
                    if let Err(error) = secret_store::save(KEYRING_USER, &stored).await {
                        self.clear_login(&login.device_code).await;
                        return Err(error);
                    }
                    // 保存中の取消・置換・client_id 変更は、確定と同じ lock の下で判定する。
                    // 確定できなければ、このログインが保存した認証を巻き戻す。
                    let new_generation = match self.try_commit_login(login.ticket, generation) {
                        LoginCommit::Committed(new_generation) => new_generation,
                        LoginCommit::Cancelled => {
                            let _ = self.forget_stored("取消されたログインの巻き戻し").await;
                            *cache = None;
                            self.clear_login(&login.device_code).await;
                            return Err("Twitchログインをキャンセルしました".to_string());
                        }
                        LoginCommit::Changed => {
                            let _ = self.forget_stored("ログイン保存の巻き戻し").await;
                            *cache = None;
                            self.clear_login(&login.device_code).await;
                            return Err(
                                "Twitchの接続情報が変更されました。もう一度ログインしてください"
                                    .to_string(),
                            );
                        }
                    };

                    let expires_in = value
                        .get("expires_in")
                        .and_then(json_u64)
                        .unwrap_or(DEFAULT_ACCESS_TTL_SECS);
                    let access = TwitchAccess {
                        client_id: stored.client_id.clone(),
                        token: access_token,
                        user_id: stored.user_id.clone(),
                        login: stored.login.clone(),
                        scopes: stored.scopes.clone(),
                    };
                    // 接続成功は try_commit_login で世代を進めたので、旧アカウントのキャッシュは使われない。
                    *cache = Some(CachedAccessToken {
                        client_id: stored.client_id.clone(),
                        access,
                        valid_until: safe_deadline(expires_in),
                        generation: new_generation,
                    });
                    drop(cache);
                    *self.validated.lock().await = Some((stored.client_id.clone(), Instant::now()));
                    self.clear_login(&login.device_code).await;
                    return Ok(status_from_stored(&stored));
                }
            }
        }
    }

    pub async fn cancel_login(&self) {
        self.cancel_count.fetch_add(1, Ordering::SeqCst);
        // keyring 保存中のログインも、確定前ならここで確定不可になる。
        self.gate().cancel();
        if let Some(login) = self.login.lock().await.take() {
            login.cancel.cancel();
        }
    }

    pub async fn disconnect(&self, client_id: &str) -> Result<TwitchOauthStatus, String> {
        self.cancel_login().await;
        // refresh / ログイン確定と直列化する。lock 中に世代を進めて削除し、キャッシュを捨てる。
        let mut cache = self.access_token.lock().await;
        let deleted = self.forget_stored("切断").await;
        *cache = None;
        drop(cache);
        *self.validated.lock().await = None;
        deleted.map_err(|error| {
            format!("{error}(アプリ内では切断済みとして扱います)")
        })?;
        Ok(TwitchOauthStatus {
            configured: !client_id.trim().is_empty(),
            connected: false,
            login: None,
            scopes_ok: false,
        })
    }

    /// 有効な access token と利用者情報を返す。必要なら refresh して即保存する。
    pub(super) async fn access(
        &self,
        client_id: &str,
        client_secret: &str,
    ) -> Result<TwitchAccess, ModerationError> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::NotLoggedIn,
                "設定でTwitchのクライアントIDを入力し、Twitchにログインしてください",
            ));
        }
        self.observe_client(client_id).await;
        let client_secret = client_secret.trim();

        // refresh token は回転するので、更新から保存まで同じ mutex を保持する。
        // 切断・ログイン確定も同じ mutex を取るので、世代はこの lock の内側で読む。
        let mut cache = self.access_token.lock().await;
        let (generation, signed_out) = {
            let epoch = self.epoch();
            (epoch.generation, epoch.signed_out)
        };
        if signed_out {
            *cache = None;
            return Err(not_logged_in());
        }
        // 前回の refresh で回転した認証が未保存なら、操作の前に再保存する。
        // 失敗した回は Helix を呼ばずに止める(回転後の認証はメモリに残す)。
        let pending = self.unsaved().due(generation);
        if let Some(pending) = pending {
            if let Err(error) = secret_store::save(KEYRING_USER, &pending).await {
                tracing::warn!("Twitchの更新トークンを再保存できませんでした: {error}");
                return Err(refresh_save_failed());
            }
            if !self.epoch().may_commit(generation) {
                let _ = self.forget_stored("再保存中の切断").await;
                *cache = None;
                return Err(not_logged_in());
            }
            self.unsaved().saved(generation);
        }
        if let Some(cached) = cache.as_ref() {
            if cached.usable(client_id, generation, Instant::now()) {
                return Ok(cached.access.clone());
            }
        }
        *cache = None;

        let stored = secret_store::load::<StoredTwitchToken>(KEYRING_USER)
            .await
            .map_err(|e| ModerationError::new(ModerationErrorCode::Other, e))?
            .filter(|stored| stored.client_id == client_id)
            .ok_or_else(not_logged_in)?;

        let form = refresh_form(client_id, client_secret, &stored.refresh_token);
        let response = self
            .http
            .post(TOKEN_URL)
            .form(&form)
            .send()
            .await
            .map_err(|e| {
                ModerationError::new(
                    ModerationErrorCode::Network,
                    format!("Twitchの認証更新に失敗しました: {e}"),
                )
            })?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::BAD_REQUEST || status == StatusCode::UNAUTHORIZED {
            let _ = self.forget_stored("refresh 拒否").await;
            return Err(ModerationError::new(
                ModerationErrorCode::NotLoggedIn,
                refresh_rejected_message(&body),
            ));
        }
        if !status.is_success() {
            return Err(map_token_error(status, &body));
        }
        let value = serde_json::from_str::<Value>(&body).map_err(|e| {
            ModerationError::new(
                ModerationErrorCode::Other,
                format!("Twitchの認証応答を解析できませんでした: {e}"),
            )
        })?;
        let token = string_field(&value, "access_token").ok_or_else(|| {
            ModerationError::new(
                ModerationErrorCode::Other,
                "Twitchからアクセストークンが返りませんでした",
            )
        })?;
        let refresh_token = string_field(&value, "refresh_token")
            .unwrap_or_else(|| stored.refresh_token.clone());
        let expires_in = value
            .get("expires_in")
            .and_then(json_u64)
            .unwrap_or(DEFAULT_ACCESS_TTL_SECS);
        let updated = StoredTwitchToken {
            client_id: client_id.to_string(),
            refresh_token,
            user_id: stored.user_id,
            login: stored.login,
            scopes: stored.scopes,
        };

        // 更新中に切断・client_id 変更が起きていたら、回転後の認証を残さない。
        if !self.epoch().may_commit(generation) {
            let _ = self.forget_stored("refresh 中の切断").await;
            return Err(not_logged_in());
        }
        let saved = secret_store::save(KEYRING_USER, &updated).await;
        if !self.epoch().may_commit(generation) {
            let _ = self.forget_stored("refresh 保存中の切断").await;
            return Err(not_logged_in());
        }

        let access = TwitchAccess {
            client_id: updated.client_id.clone(),
            token,
            user_id: updated.user_id.clone(),
            login: updated.login.clone(),
            scopes: updated.scopes.clone(),
        };
        *cache = Some(CachedAccessToken {
            client_id: updated.client_id.clone(),
            access: access.clone(),
            valid_until: safe_deadline(expires_in),
            generation,
        });
        if let Err(error) = saved {
            // 回転済みの旧 refresh token はもう使えない。新しい認証はメモリに残し、次回の操作で再保存する。
            tracing::warn!("Twitchの更新トークンを保存できませんでした(次回の操作で再保存します): {error}");
            self.unsaved().mark(generation, updated);
            return Err(refresh_save_failed());
        }
        Ok(access)
    }

    pub(super) async fn invalidate_access_token(&self) {
        *self.access_token.lock().await = None;
    }

    /// 前回の成功から 1 時間を超えていれば validate する。失敗しても操作は続行する。
    ///
    /// 成功時は `access` とキャッシュ、保存済みの login / scopes を最新に揃える。
    pub(super) async fn maybe_validate(&self, client_id: &str, access: &mut TwitchAccess) {
        let generation = self.generation();
        let now = Instant::now();
        {
            let last = self.validated.lock().await;
            if let Some((validated_client, at)) = last.as_ref() {
                if validated_client == client_id
                    && now.saturating_duration_since(*at) < VALIDATE_TTL
                {
                    return;
                }
            }
        }

        match self.validate_token(&access.token).await {
            Ok(info) => {
                access.user_id = info.user_id;
                access.login = info.login;
                access.scopes = info.scopes;
                {
                    let mut cache = self.access_token.lock().await;
                    if !self.epoch().may_commit(generation) {
                        return;
                    }
                    if let Some(cached) = cache.as_mut() {
                        if cached.client_id == client_id && cached.access.token == access.token {
                            cached.access.user_id = access.user_id.clone();
                            cached.access.login = access.login.clone();
                            cached.access.scopes = access.scopes.clone();
                        }
                    }
                    self.sync_stored_profile(client_id, access).await;
                }
                *self.validated.lock().await = Some((client_id.to_string(), Instant::now()));
            }
            Err(ValidateFailure::Unauthorized) => {
                // 次の API 呼び出しで 401 になれば 1 回だけ refresh される。
                *self.access_token.lock().await = None;
            }
            Err(ValidateFailure::Other(_)) => {}
        }
    }

    /// validate で得た login / scopes を保存済み認証へ反映する。access_token の lock 内で呼ぶ。
    async fn sync_stored_profile(&self, client_id: &str, access: &TwitchAccess) {
        let stored = match secret_store::load::<StoredTwitchToken>(KEYRING_USER).await {
            Ok(Some(stored)) if stored.client_id == client_id => stored,
            Ok(_) => return,
            Err(error) => {
                tracing::warn!("Twitchの保存済み認証を読めませんでした: {error}");
                return;
            }
        };
        if stored.user_id != access.user_id
            || (stored.login == access.login && stored.scopes == access.scopes)
        {
            return;
        }
        let updated = StoredTwitchToken {
            login: access.login.clone(),
            scopes: access.scopes.clone(),
            ..stored
        };
        if let Err(error) = secret_store::save(KEYRING_USER, &updated).await {
            tracing::warn!("Twitchの保存済みスコープを更新できませんでした: {error}");
        }
    }

    async fn is_active_login(&self, device_code: &str) -> bool {
        self.login
            .lock()
            .await
            .as_ref()
            .is_some_and(|current| current.device_code == device_code)
    }

    async fn validate_token(&self, access_token: &str) -> Result<ValidateInfo, ValidateFailure> {
        let response = self
            .http
            .get(VALIDATE_URL)
            .bearer_auth(access_token)
            .send()
            .await
            .map_err(|e| ValidateFailure::Other(format!("Twitchの認証確認に失敗しました: {e}")))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status == StatusCode::UNAUTHORIZED {
            return Err(ValidateFailure::Unauthorized);
        }
        if !status.is_success() {
            return Err(ValidateFailure::Other(format!(
                "Twitchの認証確認に失敗しました (HTTP {status})"
            )));
        }
        let value = serde_json::from_str::<Value>(&body)
            .map_err(|e| ValidateFailure::Other(format!("Twitchの認証応答を解析できませんでした: {e}")))?;
        let user_id = string_field(&value, "user_id").ok_or_else(|| {
            ValidateFailure::Other("TwitchからユーザーIDが返りませんでした".to_string())
        })?;
        let login = string_field(&value, "login").unwrap_or_default();
        let scopes = value
            .get("scopes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default();
        Ok(ValidateInfo {
            user_id,
            login,
            scopes,
        })
    }

    async fn clear_login(&self, device_code: &str) {
        let mut login = self.login.lock().await;
        if login
            .as_ref()
            .is_some_and(|current| current.device_code == device_code)
        {
            *login = None;
        }
    }
}

fn status_from_stored(stored: &StoredTwitchToken) -> TwitchOauthStatus {
    TwitchOauthStatus {
        configured: true,
        connected: true,
        login: Some(stored.login.clone()).filter(|login| !login.trim().is_empty()),
        scopes_ok: scopes_satisfied(&stored.scopes),
    }
}

fn safe_deadline(expires_in_secs: u64) -> Instant {
    let ttl = Duration::from_secs(expires_in_secs.max(120));
    Instant::now() + ttl.saturating_sub(Duration::from_secs(60))
}

fn string_field(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

fn json_u64(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|text| text.trim().parse::<u64>().ok())
    })
}

fn error_message(body: &str) -> Option<String> {
    let value = serde_json::from_str::<Value>(body).ok()?;
    value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .map(str::trim)
        .filter(|message| !message.is_empty())
        .map(str::to_string)
}

fn not_logged_in() -> ModerationError {
    ModerationError::new(
        ModerationErrorCode::NotLoggedIn,
        "設定からTwitchにログインしてください",
    )
}

/// refresh で回転した認証を keyring へ保存できなかったとき。操作は実行しない。
fn refresh_save_failed() -> ModerationError {
    ModerationError::new(
        ModerationErrorCode::Other,
        "Twitch のログイン情報を保存できませんでした。もう一度操作してください(解決しない場合は再ログイン)",
    )
}

/// refresh が 400 / 401 で拒否されたときの文言。Twitch の本文があれば添える。
fn refresh_rejected_message(body: &str) -> String {
    let base = "Twitchの認証の有効期限が切れています。設定からTwitchへ再接続してください";
    match error_message(body) {
        Some(detail) => format!("{base}(Twitch: {detail})"),
        None => base.to_string(),
    }
}

fn map_token_error(status: StatusCode, body: &str) -> ModerationError {
    let detail = error_message(body).unwrap_or_else(|| format!("HTTP {status}"));
    ModerationError::new(
        ModerationErrorCode::Network,
        format!("Twitchの認証更新に失敗しました: {detail}"),
    )
}

/// デバイス認証ポーリングの応答分類。message の部分一致で寛容に判定する。
///
/// 5xx は一時障害として `Pending`(期限内は継続)、429 は `SlowDown`(間隔を延ばす)にする。
pub(crate) fn classify_device_poll(status: StatusCode, body: &str) -> DevicePoll {
    if status.is_success() {
        return DevicePoll::Authorized;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return DevicePoll::SlowDown;
    }
    if status.is_server_error() {
        return DevicePoll::Pending;
    }

    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let message = value
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("error").and_then(Value::as_str))
        .unwrap_or_default();
    let lower = message.to_ascii_lowercase();

    if lower.contains("authorization_pending") || lower.contains("authorization pending") {
        return DevicePoll::Pending;
    }
    if lower.contains("slow_down") || lower.contains("slow down") {
        return DevicePoll::SlowDown;
    }
    if lower.contains("invalid device code") {
        return DevicePoll::Failed(
            "Twitchの認証コードが無効です。もう一度ログインしてください".to_string(),
        );
    }
    if lower.contains("expired") || lower.contains("expire") {
        return DevicePoll::Failed(
            "Twitchの認証コードの有効期限が切れました。もう一度ログインしてください".to_string(),
        );
    }
    if lower.contains("access_denied") || lower.contains("denied") {
        return DevicePoll::Failed("Twitchのログインが拒否されました".to_string());
    }

    if message.trim().is_empty() {
        DevicePoll::Failed(format!("Twitchのログインに失敗しました (HTTP {status})"))
    } else {
        DevicePoll::Failed(format!("Twitchのログインに失敗しました: {message}"))
    }
}

/// refresh リクエストの form。クライアントシークレットは非空のときだけ付ける。
pub(crate) fn refresh_form(
    client_id: &str,
    client_secret: &str,
    refresh_token: &str,
) -> Vec<(&'static str, String)> {
    let mut form = vec![
        ("grant_type", "refresh_token".to_string()),
        ("client_id", client_id.trim().to_string()),
        ("refresh_token", refresh_token.trim().to_string()),
    ];
    let client_secret = client_secret.trim();
    if !client_secret.is_empty() {
        form.push(("client_secret", client_secret.to_string()));
    }
    form
}

/// 必須スコープをすべて持っているか。
pub(crate) fn scopes_satisfied(scopes: &[String]) -> bool {
    REQUIRED_SCOPES
        .iter()
        .all(|required| scopes.iter().any(|scope| scope == required))
}

/// 要求開始時の認証世代が現在も同じか。
pub(crate) fn is_current_generation(request: u64, current: u64) -> bool {
    request == current
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_device_poll_outcomes_from_partial_messages() {
        let pending = r#"{"status":400,"message":"authorization_pending"}"#;
        assert_eq!(
            classify_device_poll(StatusCode::BAD_REQUEST, pending),
            DevicePoll::Pending
        );
        let slow = r#"{"status":400,"message":"slow_down"}"#;
        assert_eq!(
            classify_device_poll(StatusCode::BAD_REQUEST, slow),
            DevicePoll::SlowDown
        );
        let invalid = r#"{"status":400,"message":"invalid device code"}"#;
        assert!(matches!(
            classify_device_poll(StatusCode::BAD_REQUEST, invalid),
            DevicePoll::Failed(_)
        ));
        let expired = r#"{"status":400,"message":"device code has expired"}"#;
        assert!(matches!(
            classify_device_poll(StatusCode::BAD_REQUEST, expired),
            DevicePoll::Failed(_)
        ));
        assert_eq!(
            classify_device_poll(StatusCode::OK, r#"{"access_token":"token"}"#),
            DevicePoll::Authorized
        );
        // 本文が読めない 4xx も panic せず失敗にする。
        assert!(matches!(
            classify_device_poll(StatusCode::FORBIDDEN, "not json"),
            DevicePoll::Failed(_)
        ));
    }

    #[test]
    fn device_poll_keeps_waiting_on_transient_server_errors() {
        // 5xx は期限内なら継続、429 は間隔を延ばして継続する。
        for status in [
            StatusCode::INTERNAL_SERVER_ERROR,
            StatusCode::BAD_GATEWAY,
            StatusCode::SERVICE_UNAVAILABLE,
        ] {
            assert_eq!(classify_device_poll(status, "not json"), DevicePoll::Pending);
        }
        assert_eq!(
            classify_device_poll(StatusCode::TOO_MANY_REQUESTS, "{}"),
            DevicePoll::SlowDown
        );
    }

    #[test]
    fn refresh_rejection_message_includes_twitch_detail() {
        let message =
            refresh_rejected_message(r#"{"status":400,"message":"Invalid refresh token"}"#);
        assert!(message.contains("再接続"));
        assert!(message.contains("Invalid refresh token"));

        let plain = refresh_rejected_message("");
        assert!(plain.contains("再接続"));
        assert!(!plain.contains("Twitch:"));
    }

    #[test]
    fn refresh_form_appends_secret_only_when_present() {
        let with_secret = refresh_form(" client ", " secret ", " refresh ");
        assert!(with_secret.contains(&("grant_type", "refresh_token".to_string())));
        assert!(with_secret.contains(&("client_id", "client".to_string())));
        assert!(with_secret.contains(&("refresh_token", "refresh".to_string())));
        assert!(with_secret.contains(&("client_secret", "secret".to_string())));

        let without_secret = refresh_form("client", "   ", "refresh");
        assert!(!without_secret
            .iter()
            .any(|(key, _)| *key == "client_secret"));
    }

    #[test]
    fn scopes_satisfied_requires_both_moderation_scopes() {
        let both = REQUIRED_SCOPES
            .iter()
            .map(|scope| scope.to_string())
            .collect::<Vec<_>>();
        assert!(scopes_satisfied(&both));

        let one = vec![REQUIRED_SCOPES[0].to_string()];
        assert!(!scopes_satisfied(&one));
        assert!(!scopes_satisfied(&[]));
        assert!(!scopes_satisfied(&["chat:read".to_string()]));
    }

    fn sample_access(token: &str) -> TwitchAccess {
        TwitchAccess {
            client_id: "client".to_string(),
            token: token.to_string(),
            user_id: "1".to_string(),
            login: "mod".to_string(),
            scopes: REQUIRED_SCOPES.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn refresh_started_before_disconnect_may_not_commit() {
        let mut epoch = AuthEpoch::default();
        let ticket = epoch.generation;
        assert!(epoch.may_commit(ticket));

        // refresh の HTTP 中に切断された → 回転後の認証を保存しない。
        epoch.sign_out();
        assert!(!epoch.may_commit(ticket));
        // 切断後に始めた refresh も、再ログインまでは保存しない。
        assert!(!epoch.may_commit(epoch.generation));

        // 再ログイン後も、切断前に始めた refresh は保存しない。
        let signed_in = epoch.sign_in();
        assert!(!epoch.may_commit(ticket));
        assert!(epoch.may_commit(signed_in));

        // client_id 変更も、それ以前の refresh を無効にする。
        epoch.invalidate();
        assert!(!epoch.may_commit(signed_in));
        assert!(epoch.may_commit(epoch.generation));
    }

    #[test]
    fn cached_access_is_dropped_when_generation_changes() {
        let now = Instant::now();
        let cached = CachedAccessToken {
            client_id: "client".to_string(),
            access: sample_access("token"),
            valid_until: now + Duration::from_secs(60),
            generation: 3,
        };
        assert!(cached.usable("client", 3, now));
        assert!(!cached.usable("client", 4, now), "世代が進んだら使わない");
        assert!(!cached.usable("other", 3, now), "client_id が違えば使わない");
        assert!(
            !cached.usable("client", 3, now + Duration::from_secs(61)),
            "期限切れは使わない"
        );
    }

    #[tokio::test]
    async fn signed_out_ignores_cache_and_keyring_residue() {
        // keyring の削除に失敗した場合を想定: 切断フラグだけで未接続として扱い、keyring は読まない。
        let auth = TwitchAuth::new().unwrap();
        auth.observe_client("client").await;
        *auth.access_token.lock().await = Some(CachedAccessToken {
            client_id: "client".to_string(),
            access: sample_access("token"),
            valid_until: Instant::now() + Duration::from_secs(600),
            generation: auth.generation(),
        });
        assert_eq!(auth.access("client", "").await.unwrap().token, "token");

        auth.epoch().sign_out();
        let error = auth.access("client", "").await.unwrap_err();
        assert_eq!(error.code, ModerationErrorCode::NotLoggedIn);
        assert!(auth.access_token.lock().await.is_none());

        let status = auth.status("client").await.unwrap();
        assert!(status.configured);
        assert!(!status.connected);
    }

    #[tokio::test]
    async fn second_await_of_same_login_fails_immediately() {
        let auth = TwitchAuth::new().unwrap();
        *auth.login.lock().await = Some(ActiveLogin {
            client_id: "client".to_string(),
            device_code: "device".to_string(),
            interval: Duration::from_secs(3600),
            expires_at: Instant::now() + Duration::from_secs(3600),
            cancel: CancellationToken::new(),
            ticket: 0,
        });

        let (first, second) = tokio::join!(auth.await_login(), async {
            tokio::task::yield_now().await;
            let second = auth.await_login().await;
            auth.cancel_login().await;
            second
        });
        assert!(second.unwrap_err().contains("待機中"));
        assert!(first.unwrap_err().contains("キャンセル"));
        // 待機の印は解除されている。
        assert!(auth.awaiting.lock().unwrap().is_none());
    }

    #[test]
    fn login_gate_blocks_commit_after_cancel_or_replacement() {
        let mut gate = LoginGate::default();
        // keyring 保存中に取り消された → 確定しない。
        let first = gate.register();
        assert!(gate.is_live(first));
        gate.cancel();
        assert!(!gate.commit(first));

        // 二重 begin で置き換えられた旧ログインは確定しない。新しい方は確定できる。
        let old = gate.register();
        let new = gate.register();
        assert!(!gate.commit(old));
        assert!(gate.commit(new));
        // 確定は 1 回だけ。確定後の取消は何もしない。
        assert!(!gate.commit(new));
        gate.cancel();
        assert_eq!(gate.phase, LoginPhase::Committed);
    }

    #[tokio::test]
    async fn cancel_during_save_prevents_sign_in() {
        let auth = TwitchAuth::new().unwrap();
        let generation = auth.generation();
        let ticket = auth.gate().register();
        // 保存の最中に取消が走った想定。
        auth.cancel_login().await;
        assert_eq!(auth.try_commit_login(ticket, generation), LoginCommit::Cancelled);
        assert_eq!(auth.generation(), generation, "取消されたログインは世代を進めない");

        // 保存中に client_id が変わった場合も確定しない。
        let ticket = auth.gate().register();
        auth.epoch().invalidate();
        assert_eq!(auth.try_commit_login(ticket, generation), LoginCommit::Changed);
        assert!(!auth.gate().is_live(ticket));

        // 取消・変更が無ければ確定し、新しい世代でサインインする。
        let generation = auth.generation();
        let ticket = auth.gate().register();
        let LoginCommit::Committed(signed_in) = auth.try_commit_login(ticket, generation) else {
            panic!("確定するはず");
        };
        assert_ne!(signed_in, generation);
        assert!(auth.epoch().may_commit(signed_in));
        // 確定後の取消はログイン状態を変えない。
        auth.cancel_login().await;
        assert!(auth.epoch().may_commit(signed_in));
    }

    #[test]
    fn unsaved_refresh_is_retried_until_saved_and_dropped_on_generation_change() {
        let token = StoredTwitchToken {
            client_id: "client".to_string(),
            refresh_token: "rotated".to_string(),
            user_id: "1".to_string(),
            login: "mod".to_string(),
            scopes: Vec::new(),
        };
        let mut unsaved = UnsavedRefresh::default();
        assert!(unsaved.due(1).is_none());

        // 保存失敗 → 次回の操作で再保存対象。再保存も失敗したら(saved を呼ばない)残る。
        unsaved.mark(1, token.clone());
        assert_eq!(unsaved.due(1).unwrap().refresh_token, "rotated");
        assert!(unsaved.due(1).is_some());
        // 再保存に成功したら消える。
        unsaved.saved(1);
        assert!(unsaved.due(1).is_none());

        // 切断・client_id 変更で世代が進んだら再保存しない。
        unsaved.mark(1, token.clone());
        assert!(unsaved.due(2).is_none());
        assert!(unsaved.pending.is_none());

        // 別世代の保存成功では消さない。
        unsaved.mark(3, token);
        unsaved.saved(2);
        assert!(unsaved.due(3).is_some());
    }

    #[test]
    fn debug_output_redacts_tokens() {
        let access = format!("{:?}", sample_access("secret-access"));
        assert!(!access.contains("secret-access"));
        let stored = StoredTwitchToken {
            client_id: "client".to_string(),
            refresh_token: "secret-refresh".to_string(),
            user_id: "1".to_string(),
            login: "mod".to_string(),
            scopes: Vec::new(),
        };
        assert!(!format!("{stored:?}").contains("secret-refresh"));
    }
}
