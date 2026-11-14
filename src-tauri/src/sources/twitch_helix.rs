//! Twitch Helix から Goals 用 viewer_count を取得する poller。
//!
//! UserToken では validate API から Client-Id を導出し、AppCredentials では
//! client_credentials で App Access Token を取得して、Helix streams API の
//! `data[0].viewer_count` を寛容に抽出する。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::twitch_auth::TwitchAuth;
use crate::model::{LiveEvent, LiveStream, Platform};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

const POLL_INTERVAL: Duration = Duration::from_secs(20);
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const APP_TOKEN_URL: &str = "https://id.twitch.tv/oauth2/token";
const STREAMS_URL: &str = "https://api.twitch.tv/helix/streams";
const DEFAULT_APP_TOKEN_EXPIRES_IN_SECS: u64 = 3600;
const TOKEN_REFRESH_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TwitchViewerAuth {
    /// チャット用ユーザートークン。validate API から client_id を導出する（従来経路）。
    UserToken(String),
    /// dev.twitch.tv 登録アプリの client_credentials。App Access Token を自動取得・自動更新する。
    AppCredentials {
        client_id: String,
        client_secret: String,
    },
    /// Device Code ログイン(`TwitchAuth`)のアクセストークン。呼び出し側が `TwitchAuth` を渡す。
    /// 未ログインの間はポーリングしても何も起きない(ログイン後に自動で動き出す)。
    DeviceLogin { client_id: String },
}

#[derive(Debug, Clone)]
struct CachedAppAccessToken {
    value: String,
    valid_until: Instant,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AppAccessToken {
    access_token: String,
    expires_in: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TwitchStreamMetadata {
    viewer_count: Option<u32>,
    live: Option<bool>,
    /// Helix `streams[0].id`(配信 1 回を表す ID)。
    stream_id: Option<String>,
    title: Option<String>,
    started_at_ms: Option<u64>,
}

/// 配信 ID の変化から `LiveEvent` を決める(ポーラーごとに 1 つ)。
#[derive(Debug, Default)]
enum LiveTracker {
    /// まだ確認していない。
    #[default]
    Unknown,
    Live(String),
    Offline,
}

impl LiveTracker {
    /// 1 回の確認結果を取り込み、送るべきイベントを返す。
    /// - 配信 ID が変わった時だけ Started(ID が取れなければ送らない)
    /// - 未配信確認は Unknown / Live からの遷移時だけ Ended(初回を含む。未配信が続く間は送らない)
    /// `identifier` は設定の識別子そのまま(イベントの照合キー)、`login` は正規化済みで URL 用。
    fn observe(
        &mut self,
        identifier: &str,
        login: &str,
        meta: &TwitchStreamMetadata,
    ) -> Option<LiveEvent> {
        match meta.live {
            Some(true) => {
                let id = meta.stream_id.as_deref()?;
                if matches!(self, LiveTracker::Live(prev) if prev == id) {
                    return None;
                }
                *self = LiveTracker::Live(id.to_string());
                Some(LiveEvent::Started(LiveStream {
                    platform: Platform::Twitch,
                    channel_identifier: identifier.to_string(),
                    stream_id: id.to_string(),
                    title: meta.title.clone(),
                    url: format!("https://www.twitch.tv/{login}"),
                    started_at_ms: meta.started_at_ms,
                }))
            }
            Some(false) => {
                if matches!(self, LiveTracker::Offline) {
                    return None;
                }
                *self = LiveTracker::Offline;
                Some(LiveEvent::Ended {
                    platform: Platform::Twitch,
                    channel_identifier: identifier.to_string(),
                })
            }
            None => None,
        }
    }
}

pub fn spawn_twitch_viewer_poller(
    login: String,
    auth: TwitchViewerAuth,
    device_auth: Arc<TwitchAuth>,
    tx: mpsc::Sender<YoutubeMetadataUpdate>,
    live_tx: Option<mpsc::Sender<LiveEvent>>,
    cancel: CancellationToken,
) {
    let auth = match auth {
        TwitchViewerAuth::UserToken(token) => {
            let Some(token) = oauth_token_without_prefix(&token) else {
                return;
            };
            TwitchViewerAuth::UserToken(token)
        }
        TwitchViewerAuth::AppCredentials {
            client_id,
            client_secret,
        } => {
            let client_id = client_id.trim();
            let client_secret = client_secret.trim();
            if client_id.is_empty() || client_secret.is_empty() {
                return;
            }
            TwitchViewerAuth::AppCredentials {
                client_id: client_id.to_string(),
                client_secret: client_secret.to_string(),
            }
        }
        TwitchViewerAuth::DeviceLogin { client_id } => {
            let client_id = client_id.trim();
            if client_id.is_empty() {
                return;
            }
            TwitchViewerAuth::DeviceLogin {
                client_id: client_id.to_string(),
            }
        }
    };

    // SourceManager に資格情報を追加せず、同じチャンネルの Helix 起動経路を共有。
    // ユーザートークンだけの場合はアバターのリゾルバを起動しない。
    if let TwitchViewerAuth::AppCredentials {
        client_id,
        client_secret,
    } = &auth
    {
        spawn_avatar_resolver(
            &login,
            client_id.clone(),
            client_secret.clone(),
            cancel.clone(),
        );
    }

    tauri::async_runtime::spawn(async move {
        let identifier = login.clone();
        let login = login.trim().to_string();
        if login.is_empty() {
            return;
        }

        let http = match Client::builder().build() {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("twitch:{login} helix HTTP client 初期化失敗: {e}");
                return;
            }
        };
        let mut user_client_id: Option<String> = None;
        let mut app_access_token: Option<CachedAppAccessToken> = None;
        let mut client_id_failures = 0usize;
        let mut metadata_failures = 0usize;
        let mut live_tracker = LiveTracker::default();

        loop {
            if cancel.is_cancelled() {
                break;
            }

            let metadata_result: Option<anyhow::Result<TwitchStreamMetadata>> = match &auth {
                TwitchViewerAuth::UserToken(token) => {
                    if user_client_id.is_none() {
                        match fetch_client_id(&http, token).await {
                            Ok(id) => {
                                user_client_id = Some(id);
                                client_id_failures = 0;
                            }
                            Err(e) => {
                                client_id_failures += 1;
                                if client_id_failures >= 3 {
                                    tracing::warn!(
                                        "twitch:{login} client_id 取得失敗が連続しています({client_id_failures}回): {e:#}"
                                    );
                                } else {
                                    tracing::debug!("twitch:{login} client_id 取得失敗: {e:#}");
                                }
                            }
                        }
                    }

                    match user_client_id.as_deref() {
                        Some(id) => {
                            // `fetch_stream_metadata` は UserToken/AppCredentials で共通利用する。
                            // この分岐では従来どおり、validate で得た client_id を使う。
                            Some(fetch_stream_metadata(&http, &login, token, id).await)
                        }
                        None => None,
                    }
                }
                TwitchViewerAuth::AppCredentials {
                    client_id,
                    client_secret,
                } => Some(
                    fetch_app_stream_metadata(
                        &http,
                        &login,
                        client_id,
                        client_secret,
                        &mut app_access_token,
                    )
                    .await,
                ),
                TwitchViewerAuth::DeviceLogin { client_id } => {
                    fetch_device_login_stream_metadata(&http, &login, client_id, &device_auth)
                        .await
                }
            };

            if let Some(result) = metadata_result {
                match result {
                    Ok(values) => {
                        metadata_failures = 0;
                        if let (Some(live_tx), Some(event)) =
                            (&live_tx, live_tracker.observe(&identifier, &login, &values))
                        {
                            super::send_live_event(live_tx, event);
                        }
                        let update = YoutubeMetadataUpdate {
                            platform: Platform::Twitch,
                            channel: login.clone(),
                            concurrent_viewers: values.viewer_count,
                            viewers_kind: ViewerCountKind::Concurrent,
                            likes: None,
                            title: None,
                            live: values.live,
                            notice: None,
                            reactions_delta: None,
                            full_snapshot: true,
                        };
                        tokio::select! {
                            _ = cancel.cancelled() => break,
                            sent = tx.send(update) => {
                                if sent.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                    Err(e) => {
                        metadata_failures += 1;
                        if metadata_failures >= 3 {
                            tracing::warn!(
                                "twitch:{login} viewer_count 取得失敗が連続しています({metadata_failures}回): {e:#}"
                            );
                        } else {
                            tracing::debug!("twitch:{login} viewer_count 取得失敗: {e:#}");
                        }
                    }
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }

        tracing::info!("twitch:{login} viewer_count poller 終了");
    });
}

pub fn viewer_auth(oauth: &str, client_id: &str, client_secret: &str) -> Option<TwitchViewerAuth> {
    let client_id = client_id.trim();
    let client_secret = client_secret.trim();
    if !client_id.is_empty() && !client_secret.is_empty() {
        return Some(TwitchViewerAuth::AppCredentials {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        });
    }

    // チャット用トークンも App 資格情報も無く Client ID だけある場合は、
    // Device Code ログイン(PR #17)のトークンで検知する(未ログインなら何もしない)。
    oauth_token_without_prefix(oauth)
        .map(TwitchViewerAuth::UserToken)
        .or_else(|| {
            (!client_id.is_empty()).then(|| TwitchViewerAuth::DeviceLogin {
                client_id: client_id.to_string(),
            })
        })
}

async fn fetch_client_id(http: &Client, token: &str) -> anyhow::Result<String> {
    let value = http
        .get(VALIDATE_URL)
        .header("Authorization", format!("OAuth {token}"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;

    extract_client_id(&value).ok_or_else(|| anyhow::anyhow!("validate response missing client_id"))
}

async fn fetch_app_access_token(
    http: &Client,
    client_id: &str,
    client_secret: &str,
) -> anyhow::Result<AppAccessToken> {
    let value = http
        .post(APP_TOKEN_URL)
        .form(&[
            ("client_id", client_id),
            ("client_secret", client_secret),
            ("grant_type", "client_credentials"),
        ])
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;

    extract_app_access_token(&value)
}

async fn fetch_app_stream_metadata(
    http: &Client,
    login: &str,
    client_id: &str,
    client_secret: &str,
    cached_token: &mut Option<CachedAppAccessToken>,
) -> anyhow::Result<TwitchStreamMetadata> {
    let mut retried_after_auth_failure = false;

    loop {
        let token = match cached_token.as_ref() {
            Some(cached) if cached.valid_until > Instant::now() => cached.value.clone(),
            _ => {
                let fetched = fetch_app_access_token(http, client_id, client_secret).await?;
                let ttl =
                    Duration::from_secs(fetched.expires_in).saturating_sub(TOKEN_REFRESH_MARGIN);
                let token = fetched.access_token.clone();
                *cached_token = Some(CachedAppAccessToken {
                    value: fetched.access_token,
                    valid_until: Instant::now() + ttl,
                });
                token
            }
        };

        match fetch_stream_metadata(http, login, &token, client_id).await {
            Ok(values) => return Ok(values),
            Err(e) if !retried_after_auth_failure && is_auth_failure(&e) => {
                *cached_token = None;
                retried_after_auth_failure = true;
                tracing::debug!(
                    "twitch:{login} Helix 認証エラーのため App Access Token を再取得して再試行します"
                );
            }
            Err(e) => return Err(e),
        }
    }
}

/// Device Code ログインのトークンで取得する。未ログイン(取得不能)は `None`(今回は何もしない)。
async fn fetch_device_login_stream_metadata(
    http: &Client,
    login: &str,
    client_id: &str,
    device_auth: &TwitchAuth,
) -> Option<anyhow::Result<TwitchStreamMetadata>> {
    // client_secret は public client の refresh では不要。
    let access = match device_auth.access(client_id, "").await {
        Ok(access) => access,
        Err(e) => {
            tracing::debug!("twitch:{login} Device Code ログインのトークンを取得できません: {}", e.message);
            return None;
        }
    };
    let result = fetch_stream_metadata(http, login, &access.token, &access.client_id).await;
    if matches!(&result, Err(e) if is_auth_failure(e)) {
        device_auth.invalidate_access_token().await;
    }
    Some(result)
}

async fn fetch_stream_metadata(
    http: &Client,
    login: &str,
    token: &str,
    client_id: &str,
) -> anyhow::Result<TwitchStreamMetadata> {
    let value = http
        .get(STREAMS_URL)
        .query(&[("user_login", login)])
        .header("Client-Id", client_id)
        .header("Authorization", format!("Bearer {token}"))
        .send()
        .await?
        .error_for_status()?
        .json::<Value>()
        .await?;

    Ok(extract_stream_metadata(&value))
}

fn oauth_token_without_prefix(oauth: &str) -> Option<String> {
    let oauth = oauth.trim();
    let token = oauth.strip_prefix("oauth:").unwrap_or(oauth);
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return None;
    }
    Some(token.to_string())
}

fn extract_app_access_token(value: &Value) -> anyhow::Result<AppAccessToken> {
    let access_token = value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty() && !token.chars().any(char::is_whitespace))
        .map(ToString::to_string)
        .ok_or_else(|| anyhow::anyhow!("App Access Token response missing access_token"))?;
    let expires_in = value
        .get("expires_in")
        .and_then(extract_expires_in)
        .unwrap_or(DEFAULT_APP_TOKEN_EXPIRES_IN_SECS);

    Ok(AppAccessToken {
        access_token,
        expires_in,
    })
}

fn extract_expires_in(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| {
        value
            .as_str()
            .and_then(|seconds| seconds.trim().parse::<u64>().ok())
    })
}

fn is_auth_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .and_then(reqwest::Error::status)
        .is_some_and(|status| matches!(status.as_u16(), 401 | 403))
}

fn extract_client_id(value: &Value) -> Option<String> {
    value
        .get("client_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(ToString::to_string)
}

fn extract_viewer_count(value: &Value) -> Option<u32> {
    let count = value.get("viewer_count")?;

    count
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| count.as_str().and_then(|s| s.trim().parse::<u32>().ok()))
}

fn extract_stream_metadata(value: &Value) -> TwitchStreamMetadata {
    let Some(streams) = value.get("data").and_then(Value::as_array) else {
        return TwitchStreamMetadata::default();
    };
    let Some(stream) = streams.first() else {
        return TwitchStreamMetadata {
            live: Some(false),
            ..Default::default()
        };
    };
    TwitchStreamMetadata {
        viewer_count: extract_viewer_count(stream),
        live: Some(true),
        stream_id: extract_stream_id(stream),
        title: stream
            .get("title")
            .and_then(Value::as_str)
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty()),
        started_at_ms: stream
            .get("started_at")
            .and_then(Value::as_str)
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
            .and_then(|t| u64::try_from(t.timestamp_millis()).ok()),
    }
}

fn extract_stream_id(value: &Value) -> Option<String> {
    let s = match value.get("id")? {
        Value::String(s) => s.trim().to_string(),
        Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

const AVATAR_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const AVATAR_LIMIT: usize = 5000;
const AVATAR_BATCH: usize = 100;
const USERS_URL: &str = "https://api.twitch.tv/helix/users";

#[derive(Default)]
struct AvatarCache {
    entries: HashMap<String, (Option<String>, Instant)>,
    pending: HashSet<String>,
}

impl AvatarCache {
    fn lookup(&mut self, id: &str, now: Instant) -> Option<Option<String>> {
        if let Some((url, at)) = self.entries.get(id) {
            if now.saturating_duration_since(*at) < AVATAR_TTL {
                return Some(url.clone());
            }
        }
        self.entries.remove(id);
        None
    }

    fn insert_batch(&mut self, batch: Vec<(String, Option<String>)>, now: Instant) {
        // Sweep and evict once per response, rather than scanning 5000 entries for
        // each of the (up to) 100 returned IDs while holding the shared mutex.
        self.entries
            .retain(|_, (_, at)| now.saturating_duration_since(*at) < AVATAR_TTL);
        self.entries
            .extend(batch.into_iter().map(|(id, url)| (id, (url, now))));
        let excess = self.entries.len().saturating_sub(AVATAR_LIMIT);
        if excess > 0 {
            let mut oldest: Vec<_> = self.entries.iter().collect();
            // Linear selection once per batch; only clone the keys to evict.
            oldest.select_nth_unstable_by_key(excess, |(_, (_, at))| *at);
            let evicted: Vec<_> = oldest[..excess]
                .iter()
                .map(|(id, _)| (*id).clone())
                .collect();
            for id in evicted {
                self.entries.remove(&id);
            }
        }
    }
}

#[derive(Clone)]
struct AvatarResolver {
    cache: Arc<Mutex<AvatarCache>>,
    tx: mpsc::Sender<String>,
    cancel: CancellationToken,
}

impl AvatarResolver {
    fn avatar(&self, id: &str) -> Option<String> {
        // Twitch user-id は数字。匿名/欠落時のログイン名は Helix に送らない。
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) || self.cancel.is_cancelled() {
            return None;
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some(url) = cache.lookup(id, Instant::now()) {
            return url;
        }
        if cache.pending.insert(id.to_string()) && self.tx.try_send(id.to_string()).is_err() {
            cache.pending.remove(id);
        }
        None
    }
}

// 各チャンネルの既存 poller と同じキャンセル寿命。終了した設定のキューは参照しない。
static AVATAR_RESOLVERS: OnceLock<Mutex<HashMap<String, AvatarResolver>>> = OnceLock::new();

pub(super) fn avatar_for_user(channel: &str, id: &str) -> Option<String> {
    let resolver = AVATAR_RESOLVERS
        .get()?
        .lock()
        .unwrap()
        .get(channel)
        .cloned()?;
    resolver.avatar(id)
}

fn spawn_avatar_resolver(
    login: &str,
    client_id: String,
    client_secret: String,
    cancel: CancellationToken,
) {
    // No credentials means no resolver, HTTP client, token fetch or users request.
    if client_id.trim().is_empty() || client_secret.trim().is_empty() {
        return;
    }
    let (tx, mut rx) = mpsc::channel(AVATAR_LIMIT);
    let cache = Arc::new(Mutex::new(AvatarCache::default()));
    let resolver = AvatarResolver {
        cache: cache.clone(),
        tx,
        cancel: cancel.clone(),
    };
    {
        let mut registry = AVATAR_RESOLVERS
            .get_or_init(Default::default)
            .lock()
            .unwrap();
        registry.retain(|_, resolver| !resolver.cancel.is_cancelled());
        registry.insert(
            login.trim().trim_start_matches('#').to_lowercase(),
            resolver,
        );
    }
    tauri::async_runtime::spawn(async move {
        let Ok(http) = Client::builder().timeout(Duration::from_secs(15)).build() else {
            return;
        };
        let mut token: Option<CachedAppAccessToken> = None;
        let mut retry = AvatarRetry::default();
        loop {
            let first = tokio::select! {
                _ = cancel.cancelled() => break,
                id = rx.recv() => match id { Some(id) => id, None => break },
            };
            // 最初の ID から 200ms 集める。受信/送出経路には await を持ち込まない。
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(Duration::from_millis(200)) => {},
            }
            let mut ids = vec![first];
            while ids.len() < AVATAR_BATCH {
                match rx.try_recv() {
                    Ok(id) => ids.push(id),
                    Err(_) => break,
                }
            }
            let result = tokio::select! {
                _ = cancel.cancelled() => break,
                result = fetch_avatars(&http, &ids, &client_id, &client_secret, &mut token) => result,
            };
            let (batch, delay) = match result {
                Ok(mut urls) => {
                    retry = AvatarRetry::default();
                    // Prepare owned values outside the mutex. Missing/deleted users
                    // are negatively cached to avoid repeated queries.
                    let batch = ids.iter().map(|id| (id.clone(), urls.remove(id))).collect();
                    (batch, None)
                }
                Err(error) => {
                    let (delay, warn) = retry.failed();
                    if warn {
                        tracing::warn!("Twitch avatar resolver failed; backing off: {error:#}");
                    }
                    (Vec::new(), Some(delay))
                }
            };
            {
                let mut cache = cache.lock().unwrap();
                for id in &ids {
                    cache.pending.remove(id);
                }
                if delay.is_none() {
                    cache.insert_batch(batch, Instant::now());
                }
            }
            if let Some(delay) = delay {
                // Persistent auth/rate-limit/server failures back off independently
                // of chat volume; warn only on entry into the failed state.
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(delay) => {},
                }
            }
        }
    });
}

#[derive(Default)]
struct AvatarRetry {
    failures: u32,
}

impl AvatarRetry {
    fn failed(&mut self) -> (Duration, bool) {
        let warn = self.failures == 0;
        self.failures = self.failures.saturating_add(1);
        let seconds = (20u64 << self.failures.saturating_sub(1).min(5)).min(600);
        (Duration::from_secs(seconds), warn)
    }
}

async fn fetch_avatars(
    http: &Client,
    ids: &[String],
    client_id: &str,
    client_secret: &str,
    cached: &mut Option<CachedAppAccessToken>,
) -> anyhow::Result<HashMap<String, String>> {
    for attempt in 0..2 {
        if !cached
            .as_ref()
            .is_some_and(|t| t.valid_until > Instant::now())
        {
            let fetched = fetch_app_access_token(http, client_id, client_secret).await?;
            *cached = Some(CachedAppAccessToken {
                value: fetched.access_token,
                valid_until: Instant::now()
                    + Duration::from_secs(fetched.expires_in).saturating_sub(TOKEN_REFRESH_MARGIN),
            });
        }
        let response = http
            .get(USERS_URL)
            .query(
                &ids.iter()
                    .take(AVATAR_BATCH)
                    .map(|id| ("id", id))
                    .collect::<Vec<_>>(),
            )
            .header("Client-Id", client_id)
            .bearer_auth(&cached.as_ref().unwrap().value)
            .send()
            .await?
            .error_for_status();
        match response {
            Err(e) if attempt == 0 && matches!(e.status().map(|s| s.as_u16()), Some(401 | 403)) => {
                *cached = None;
            }
            result => {
                let value = result?.json::<Value>().await?;
                return Ok(extract_avatars(&value));
            }
        }
    }
    unreachable!()
}

fn extract_avatars(value: &Value) -> HashMap<String, String> {
    value
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|user| {
            let id = user.get("id")?.as_str()?;
            let url = user.get("profile_image_url")?.as_str()?.trim();
            if id.is_empty() || url.is_empty() {
                return None;
            }
            Some((id.to_string(), url.to_string()))
        })
        .collect()
}

#[cfg(test)]
pub(super) fn test_avatar_resolver(channel: &str) -> mpsc::Receiver<String> {
    let (tx, rx) = mpsc::channel(10);
    let mut cache = AvatarCache::default();
    cache.insert_batch(
        vec![(
            "12345".into(),
            Some("https://example.com/avatar.png".into()),
        )],
        Instant::now(),
    );
    AVATAR_RESOLVERS
        .get_or_init(Default::default)
        .lock()
        .unwrap()
        .insert(
            channel.into(),
            AvatarResolver {
                cache: Arc::new(Mutex::new(cache)),
                tx,
                cancel: CancellationToken::new(),
            },
        );
    rx
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn avatar_cache_ttl_and_capacity() {
        let mut cache = AvatarCache::default();
        let now = Instant::now();
        cache.insert_batch(vec![("expired".into(), Some("old".into()))], now);
        assert_eq!(
            cache.lookup("expired", now + AVATAR_TTL - Duration::from_millis(1)),
            Some(Some("old".into()))
        );
        assert_eq!(cache.lookup("expired", now + AVATAR_TTL), None);
        for id in 0..AVATAR_LIMIT {
            cache.insert_batch(
                vec![(id.to_string(), Some("url".into()))],
                now + Duration::from_millis(id as u64),
            );
        }
        cache.insert_batch(
            vec![("new".into(), Some("new-url".into()))],
            now + Duration::from_secs(10),
        );
        assert_eq!(cache.entries.len(), AVATAR_LIMIT);
        assert!(!cache.entries.contains_key("0"));
        assert_eq!(
            cache.lookup("new", now + Duration::from_secs(10)),
            Some(Some("new-url".into()))
        );
        cache.insert_batch(
            vec![("later".into(), None)],
            now + AVATAR_TTL + Duration::from_secs(11),
        );
        assert_eq!(cache.entries.len(), 1);
        assert_eq!(
            cache.lookup("later", now + AVATAR_TTL + Duration::from_secs(12)),
            Some(None)
        );
    }

    #[test]
    fn avatar_batch_evicts_oldest_and_refreshes_existing_entries() {
        let now = Instant::now();
        let mut cache = AvatarCache::default();
        cache.insert_batch(
            (0..AVATAR_LIMIT)
                .map(|id| (id.to_string(), Some("old".into())))
                .collect(),
            now,
        );
        let later = now + Duration::from_secs(1);
        let mut batch: Vec<_> = (AVATAR_LIMIT..AVATAR_LIMIT + AVATAR_BATCH - 1)
            .map(|id| (id.to_string(), Some("new".into())))
            .collect();
        batch.push(("0".into(), Some("refreshed".into())));
        cache.insert_batch(batch, later);
        assert_eq!(cache.entries.len(), AVATAR_LIMIT);
        assert_eq!(cache.lookup("0", later), Some(Some("refreshed".into())));
        for id in AVATAR_LIMIT..AVATAR_LIMIT + AVATAR_BATCH - 1 {
            assert_eq!(
                cache.lookup(&id.to_string(), later),
                Some(Some("new".into()))
            );
        }
        // Refreshing an existing ID must not evict any additional entry.
        assert_eq!(
            cache.entries.values().filter(|(_, at)| *at == now).count(),
            AVATAR_LIMIT - AVATAR_BATCH
        );
        // Expired entries are swept together on the next batch, even if not looked up.
        cache.insert_batch(vec![("last".into(), None)], later + AVATAR_TTL);
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn avatar_retry_backs_off_caps_and_warns_only_on_failure_transition() {
        let mut retry = AvatarRetry::default();
        for (attempt, seconds) in [20, 40, 80, 160, 320, 600, 600, 600]
            .into_iter()
            .enumerate()
        {
            assert_eq!(retry.failed(), (Duration::from_secs(seconds), attempt == 0));
        }
        // Success resets the failure state in the resolver, permitting one new warning.
        retry = AvatarRetry::default();
        assert_eq!(retry.failed(), (Duration::from_secs(20), true));
        retry.failures = u32::MAX;
        assert_eq!(retry.failed(), (Duration::from_secs(600), false));
    }

    #[test]
    fn avatar_resolver_without_app_credentials_never_starts() {
        // These calls return before runtime/HTTP-client creation, even without a runtime.
        for (id, secret) in [("", ""), ("client", " "), (" ", "secret")] {
            spawn_avatar_resolver(
                "no-app-credentials",
                id.into(),
                secret.into(),
                CancellationToken::new(),
            );
        }
        assert!(AVATAR_RESOLVERS
            .get()
            .is_none_or(|registry| !registry.lock().unwrap().contains_key("no-app-credentials")));
        assert_eq!(avatar_for_user("no-app-credentials", "12345"), None);
    }

    #[test]
    fn avatar_queue_is_bounded_and_cancelled_resolvers_are_inactive() {
        let (tx, mut rx) = mpsc::channel(1);
        let resolver = AvatarResolver {
            cache: Default::default(),
            tx,
            cancel: CancellationToken::new(),
        };
        assert_eq!(resolver.avatar("1"), None);
        assert_eq!(resolver.avatar("1"), None);
        assert_eq!(resolver.avatar("2"), None);
        assert_eq!(rx.try_recv().unwrap(), "1");
        assert!(rx.try_recv().is_err());
        assert_eq!(resolver.cache.lock().unwrap().pending.len(), 1);
        resolver.avatar("2");
        assert_eq!(rx.try_recv().unwrap(), "2");
        resolver.cancel.cancel();
        resolver.avatar("3");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn extracts_avatar_urls_tolerantly() {
        assert_eq!(
            extract_avatars(&json!({"data": [
                {"id": "1", "profile_image_url": "https://example.com/1.png"},
                {"id": "2"}, {"id": "3", "profile_image_url": ""}, null
            ]})),
            HashMap::from([("1".into(), "https://example.com/1.png".into())])
        );
        assert!(extract_avatars(&json!({})).is_empty());
    }

    #[test]
    fn extracts_client_id_from_validate_response() {
        let value = json!({ "client_id": "abc123", "login": "streamer" });
        assert_eq!(extract_client_id(&value).as_deref(), Some("abc123"));
    }

    #[test]
    fn extracts_app_access_token_and_expiry() {
        let value = json!({
            "access_token": "app-token",
            "expires_in": 1800,
            "token_type": "bearer"
        });
        assert_eq!(
            extract_app_access_token(&value).expect("app token response"),
            AppAccessToken {
                access_token: "app-token".to_string(),
                expires_in: 1800,
            }
        );
    }

    #[test]
    fn extracts_app_access_token_expiry_from_string() {
        let value = json!({
            "access_token": "app-token",
            "expires_in": "1800"
        });
        assert_eq!(
            extract_app_access_token(&value)
                .expect("app token response")
                .expires_in,
            1800
        );
    }

    #[test]
    fn missing_app_token_expiry_uses_default() {
        let value = json!({ "access_token": "app-token" });
        assert_eq!(
            extract_app_access_token(&value)
                .expect("app token response")
                .expires_in,
            DEFAULT_APP_TOKEN_EXPIRES_IN_SECS
        );
    }

    #[test]
    fn viewer_auth_prefers_app_credentials() {
        assert_eq!(
            viewer_auth("oauth:user-token", " app-client ", " app-secret "),
            Some(TwitchViewerAuth::AppCredentials {
                client_id: "app-client".to_string(),
                client_secret: "app-secret".to_string(),
            })
        );
    }

    #[test]
    fn viewer_auth_uses_user_token_when_app_credentials_are_incomplete() {
        assert_eq!(
            viewer_auth("oauth:user-token", "app-client", ""),
            Some(TwitchViewerAuth::UserToken("user-token".to_string()))
        );
    }

    #[test]
    fn live_event_uses_original_identifier_not_normalized_login() {
        let mut t = LiveTracker::default();
        let ev = t.observe(" Streamer ", "streamer", &meta(Some("a"), Some(true)));
        match ev {
            Some(LiveEvent::Started(s)) => {
                assert_eq!(s.channel_identifier, " Streamer ");
                assert_eq!(s.url, "https://www.twitch.tv/streamer");
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert_eq!(
            t.observe(" Streamer ", "streamer", &meta(None, Some(false))),
            Some(LiveEvent::Ended {
                platform: Platform::Twitch,
                channel_identifier: " Streamer ".to_string(),
            })
        );
    }

    #[test]
    fn viewer_auth_falls_back_to_device_login_with_client_id_only() {
        assert_eq!(
            viewer_auth("", " app-client ", ""),
            Some(TwitchViewerAuth::DeviceLogin {
                client_id: "app-client".to_string(),
            })
        );
    }

    #[test]
    fn viewer_auth_returns_none_when_all_credentials_are_empty() {
        assert_eq!(viewer_auth("", "", ""), None);
    }

    #[test]
    fn extracts_viewer_count_from_first_stream() {
        let value = json!({ "data": [{ "viewer_count": 1234 }] });
        assert_eq!(
            extract_stream_metadata(&value),
            TwitchStreamMetadata {
                viewer_count: Some(1234),
                live: Some(true),
                ..Default::default()
            }
        );
    }

    #[test]
    fn missing_stream_data_means_not_live() {
        let value = json!({ "data": [] });
        assert_eq!(
            extract_stream_metadata(&value),
            TwitchStreamMetadata {
                viewer_count: None,
                live: Some(false),
                ..Default::default()
            }
        );
    }

    /// AC-1
    #[test]
    fn extracts_stream_id_title_and_started_at() {
        let value = json!({ "data": [{
            "id": "4001", "title": "  朝の配信  ", "viewer_count": 3,
            "started_at": "2026-11-14T01:02:03Z"
        }] });
        let meta = extract_stream_metadata(&value);
        assert_eq!(meta.stream_id.as_deref(), Some("4001"));
        assert_eq!(meta.title.as_deref(), Some("朝の配信"));
        assert_eq!(meta.started_at_ms, Some(1_794_618_123_000));
        assert_eq!(meta.live, Some(true));
    }

    #[test]
    fn missing_stream_fields_become_none() {
        let meta = extract_stream_metadata(&json!({ "data": [{ "viewer_count": 1 }] }));
        assert_eq!(meta.live, Some(true));
        assert_eq!((meta.stream_id, meta.title, meta.started_at_ms), (None, None, None));
        let meta = extract_stream_metadata(&json!({ "data": [{ "id": "", "title": 5, "started_at": "bad" }] }));
        assert_eq!((meta.stream_id, meta.title, meta.started_at_ms), (None, None, None));
        // data が配列でなければ判定不能(live も None)。
        assert_eq!(extract_stream_metadata(&json!({ "data": "x" })).live, None);
    }

    fn meta(id: Option<&str>, live: Option<bool>) -> TwitchStreamMetadata {
        TwitchStreamMetadata {
            live,
            stream_id: id.map(str::to_string),
            title: Some("t".to_string()),
            started_at_ms: Some(1),
            ..Default::default()
        }
    }

    fn started_id(ev: &Option<LiveEvent>) -> Option<&str> {
        match ev {
            Some(LiveEvent::Started(s)) => Some(s.stream_id.as_str()),
            _ => None,
        }
    }

    /// AC-2
    #[test]
    fn live_tracker_sends_started_only_when_stream_id_changes() {
        let mut t = LiveTracker::default();
        let ev = t.observe("ch", "ch", &meta(Some("a"), Some(true)));
        assert_eq!(started_id(&ev), Some("a"));
        if let Some(LiveEvent::Started(s)) = &ev {
            assert_eq!(s.platform, Platform::Twitch);
            assert_eq!(s.channel_identifier, "ch");
            assert_eq!(s.url, "https://www.twitch.tv/ch");
            assert_eq!(s.title.as_deref(), Some("t"));
            assert_eq!(s.started_at_ms, Some(1));
        }
        assert_eq!(t.observe("ch", "ch", &meta(Some("a"), Some(true))), None);
        assert_eq!(t.observe("ch", "ch", &meta(Some("a"), Some(true))), None);
        // 回線断などで配信 ID だけ変わる。
        assert_eq!(started_id(&t.observe("ch", "ch", &meta(Some("b"), Some(true)))), Some("b"));
    }

    #[test]
    fn live_tracker_sends_ended_on_first_and_transition_only() {
        let mut t = LiveTracker::default();
        // 初回の未配信確認でも Ended。続く間は送らない。
        let ended = Some(LiveEvent::Ended {
            platform: Platform::Twitch,
            channel_identifier: "ch".to_string(),
        });
        assert_eq!(t.observe("ch", "ch", &meta(None, Some(false))), ended);
        assert_eq!(t.observe("ch", "ch", &meta(None, Some(false))), None);
        assert_eq!(started_id(&t.observe("ch", "ch", &meta(Some("a"), Some(true)))), Some("a"));
        assert_eq!(t.observe("ch", "ch", &meta(None, Some(false))), ended);
        // 判定不能(live=None)や配信 ID の無い配信中は何も送らない。
        assert_eq!(t.observe("ch", "ch", &meta(None, None)), None);
        assert_eq!(t.observe("ch", "ch", &meta(None, Some(true))), None);
    }

    #[test]
    fn live_tracker_without_stream_id_never_sends_started() {
        let mut t = LiveTracker::default();
        assert_eq!(t.observe("ch", "ch", &meta(None, Some(true))), None);
        assert!(matches!(t, LiveTracker::Unknown));
    }
}
