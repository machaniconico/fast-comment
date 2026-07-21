//! YouTube Live への運営コメント投稿。
//!
//! 認証・API呼び出しはユーザー操作時だけ行う。コメント受信ループには接続せず、
//! access token と liveChatId を短時間だけメモリキャッシュして投稿の待ち時間と
//! Data API の呼び出し回数を抑える。

use std::time::{Duration, Instant};

use oauth2::{
    basic::BasicClient, AuthUrl, AuthorizationCode, ClientId, CsrfToken, PkceCodeChallenge,
    RedirectUrl, RefreshToken, Scope, TokenResponse, TokenUrl,
};
use reqwest::{StatusCode, Url};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const YOUTUBE_SCOPE: &str = "https://www.googleapis.com/auth/youtube.force-ssl";
const CHANNELS_URL: &str = "https://www.googleapis.com/youtube/v3/channels";
const ACTIVE_BROADCASTS_URL: &str = "https://www.googleapis.com/youtube/v3/liveBroadcasts";
const LIVE_CHAT_MESSAGES_URL: &str = "https://www.googleapis.com/youtube/v3/liveChat/messages";
const KEYRING_SERVICE: &str = "fast-comment";
const KEYRING_USER: &str = "youtube-oauth-refresh-token";
const CALLBACK_PATH: &str = "/";
const CALLBACK_TIMEOUT: Duration = Duration::from_secs(180);
const LIVE_CHAT_CACHE_TTL: Duration = Duration::from_secs(60);

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct YoutubeOauthStatus {
    pub configured: bool,
    pub connected: bool,
    pub channel_title: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct StoredRefreshToken {
    client_id: String,
    refresh_token: String,
    #[serde(default)]
    channel_title: Option<String>,
}

struct CachedAccessToken {
    client_id: String,
    value: String,
    valid_until: Instant,
}

struct CachedLiveChat {
    target: String,
    live_chat_id: String,
    valid_until: Instant,
}

/// OAuth・投稿専用の軽量状態。バックグラウンドタスクは持たない。
pub struct YoutubeAuth {
    http: reqwest::Client,
    login_lock: Mutex<()>,
    keyring_lock: Mutex<()>,
    access_token: Mutex<Option<CachedAccessToken>>,
    live_chat: Mutex<Option<CachedLiveChat>>,
}

impl YoutubeAuth {
    pub fn new() -> Result<Self, String> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            // OAuth token endpoints must not follow redirects automatically.
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|e| format!("YouTube投稿用HTTPクライアントの作成に失敗しました: {e}"))?;

        Ok(Self {
            http,
            login_lock: Mutex::new(()),
            keyring_lock: Mutex::new(()),
            access_token: Mutex::new(None),
            live_chat: Mutex::new(None),
        })
    }

    pub async fn status(&self, client_id: &str) -> Result<YoutubeOauthStatus, String> {
        let client_id = client_id.trim();
        let configured = !client_id.is_empty();
        let (connected, channel_title) = if configured {
            match self.load_refresh_token().await? {
                Some(stored) if stored.client_id == client_id => (true, stored.channel_title),
                _ => (false, None),
            }
        } else {
            (false, None)
        };
        Ok(YoutubeOauthStatus {
            configured,
            connected,
            channel_title,
        })
    }

    /// システムブラウザと loopback redirect を使う Authorization Code + PKCE。
    pub async fn connect(&self, client_id: &str) -> Result<YoutubeOauthStatus, String> {
        let client_id = client_id.trim();
        if client_id.is_empty() {
            return Err("YouTube OAuth クライアントIDを設定してください".to_string());
        }

        // 同時に複数ブラウザを開かず、単一のloopback listenerだけを待つ。
        let _login_guard = self.login_lock.lock().await;
        let listener = TcpListener::bind(("127.0.0.1", 0))
            .await
            .map_err(|e| format!("OAuthコールバック待受の開始に失敗しました: {e}"))?;
        let port = listener
            .local_addr()
            .map_err(|e| format!("OAuthコールバック先の取得に失敗しました: {e}"))?
            .port();
        let redirect_url = format!("http://127.0.0.1:{port}{CALLBACK_PATH}");

        let oauth = BasicClient::new(ClientId::new(client_id.to_string()))
            .set_auth_uri(AuthUrl::new(GOOGLE_AUTH_URL.to_string()).map_err(|e| e.to_string())?)
            .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).map_err(|e| e.to_string())?)
            .set_redirect_uri(
                RedirectUrl::new(redirect_url).map_err(|e| format!("redirect URL不正: {e}"))?,
            );
        let (pkce_challenge, pkce_verifier) = PkceCodeChallenge::new_random_sha256();
        let (auth_url, csrf) = oauth
            .authorize_url(CsrfToken::new_random)
            .add_scope(Scope::new(YOUTUBE_SCOPE.to_string()))
            .add_extra_param("access_type", "offline")
            .add_extra_param("prompt", "consent")
            .set_pkce_challenge(pkce_challenge)
            .url();

        crate::update::open_url(auth_url.to_string())?;
        let code = wait_for_oauth_callback(listener, csrf.secret()).await?;

        let token = oauth
            .exchange_code(AuthorizationCode::new(code))
            .set_pkce_verifier(pkce_verifier)
            .request_async(&self.http)
            .await
            .map_err(|e| format!("Google認証コードの交換に失敗しました: {e}"))?;
        let refresh_token = token.refresh_token().ok_or_else(|| {
            "Googleから更新トークンが返りませんでした。もう一度「Googleに接続」を実行してください"
                .to_string()
        })?;
        // 接続完了時だけ1unitの確認を行い、投稿アカウント名を以後はローカル表示する。
        let channel_title = self
            .connected_channel_title(token.access_token().secret())
            .await?;

        self.save_refresh_token(StoredRefreshToken {
            client_id: client_id.to_string(),
            refresh_token: refresh_token.secret().to_string(),
            channel_title: Some(channel_title.clone()),
        })
        .await?;
        self.cache_access_token(client_id, token.access_token().secret(), token.expires_in())
            .await;

        Ok(YoutubeOauthStatus {
            configured: true,
            connected: true,
            channel_title: Some(channel_title),
        })
    }

    /// ローカル資格情報だけを削除する。Googleアカウント全体の許可は勝手に失効させない。
    pub async fn disconnect(&self, client_id: &str) -> Result<YoutubeOauthStatus, String> {
        self.delete_refresh_token().await?;
        self.clear_caches().await;
        Ok(YoutubeOauthStatus {
            configured: !client_id.trim().is_empty(),
            connected: false,
            channel_title: None,
        })
    }

    pub async fn clear_caches(&self) {
        *self.access_token.lock().await = None;
        *self.live_chat.lock().await = None;
    }

    pub async fn send_message(
        &self,
        client_id: &str,
        target: &str,
        text: &str,
    ) -> Result<(), String> {
        let client_id = client_id.trim();
        let target = target.trim();
        let text = text.trim();
        if client_id.is_empty() {
            return Err(
                "設定でYouTube OAuthクライアントIDを入力し、Googleに接続してください".to_string(),
            );
        }
        if target.is_empty() {
            return Err("送信先のYouTube配信を指定してください".to_string());
        }
        if text.is_empty() {
            return Err("コメント本文が空です".to_string());
        }

        // 期限切れ/無効化されたaccess tokenだけ1回更新して再試行する。
        for attempt in 0..2 {
            let access_token = self.access_token(client_id).await?;
            match self.send_once(target, text, &access_token).await {
                Ok(()) => return Ok(()),
                Err(SendError::Unauthorized) if attempt == 0 => {
                    *self.access_token.lock().await = None;
                }
                Err(SendError::Message(message)) => return Err(message),
                Err(SendError::Unauthorized) => {
                    return Err(
                        "YouTube認証の有効期限が切れています。設定からGoogleへ再接続してください"
                            .to_string(),
                    )
                }
            }
        }
        unreachable!("YouTube投稿の再試行は最大2回")
    }

    async fn send_once(
        &self,
        target: &str,
        text: &str,
        access_token: &str,
    ) -> Result<(), SendError> {
        let live_chat_id = self.live_chat_id(target, access_token).await?;
        let response = self
            .http
            .post(LIVE_CHAT_MESSAGES_URL)
            .bearer_auth(access_token)
            .query(&[("part", "snippet")])
            .json(&live_chat_message_body(&live_chat_id, text))
            .send()
            .await
            .map_err(|e| {
                SendError::Message(format!("YouTubeへのコメント送信に失敗しました: {e}"))
            })?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if matches!(
            google_api_reason(&body).as_deref(),
            Some("liveChatEnded" | "liveChatNotFound")
        ) {
            *self.live_chat.lock().await = None;
        }
        Err(SendError::Message(map_google_api_error(status, &body)))
    }

    async fn live_chat_id(&self, target: &str, access_token: &str) -> Result<String, SendError> {
        {
            let cache = self.live_chat.lock().await;
            if let Some(cached) = cache.as_ref() {
                if cached.target == target && cached.valid_until > Instant::now() {
                    return Ok(cached.live_chat_id.clone());
                }
            }
        }

        let response = self
            .http
            .get(ACTIVE_BROADCASTS_URL)
            .bearer_auth(access_token)
            .query(&[
                ("part", "snippet"),
                ("broadcastStatus", "active"),
                ("broadcastType", "all"),
                ("maxResults", "50"),
            ])
            .send()
            .await
            .map_err(|e| {
                SendError::Message(format!("配信中のYouTubeチャット取得に失敗しました: {e}"))
            })?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(SendError::Message(map_google_api_error(status, &body)));
        }

        let body = response
            .json::<Value>()
            .await
            .map_err(|e| SendError::Message(format!("YouTube配信情報の解析に失敗しました: {e}")))?;
        let preferred_video_id = preferred_video_id(target);
        let live_chat_id = select_live_chat_id(&body, preferred_video_id.as_deref())
            .map_err(SendError::Message)?;
        *self.live_chat.lock().await = Some(CachedLiveChat {
            target: target.to_string(),
            live_chat_id: live_chat_id.clone(),
            valid_until: Instant::now() + LIVE_CHAT_CACHE_TTL,
        });
        Ok(live_chat_id)
    }

    async fn access_token(&self, client_id: &str) -> Result<String, String> {
        // refreshを同時実行しないため、更新完了まで同じmutexを保持する。
        let mut cache = self.access_token.lock().await;
        if let Some(cached) = cache.as_ref() {
            if cached.client_id == client_id && cached.valid_until > Instant::now() {
                return Ok(cached.value.clone());
            }
        }

        let stored = self
            .load_refresh_token()
            .await?
            .filter(|stored| stored.client_id == client_id)
            .ok_or_else(|| "設定からGoogleアカウントへ接続してください".to_string())?;
        let oauth = BasicClient::new(ClientId::new(client_id.to_string()))
            .set_token_uri(TokenUrl::new(GOOGLE_TOKEN_URL.to_string()).map_err(|e| e.to_string())?);
        let token = oauth
            .exchange_refresh_token(&RefreshToken::new(stored.refresh_token.clone()))
            .request_async(&self.http)
            .await
            .map_err(|_| {
                "YouTube認証を更新できませんでした。設定からGoogleへ再接続してください".to_string()
            })?;

        if let Some(rotated) = token.refresh_token() {
            self.save_refresh_token(StoredRefreshToken {
                client_id: client_id.to_string(),
                refresh_token: rotated.secret().to_string(),
                channel_title: stored.channel_title.clone(),
            })
            .await?;
        }
        let value = token.access_token().secret().to_string();
        let valid_until = safe_token_deadline(token.expires_in());
        *cache = Some(CachedAccessToken {
            client_id: client_id.to_string(),
            value: value.clone(),
            valid_until,
        });
        Ok(value)
    }

    async fn connected_channel_title(&self, access_token: &str) -> Result<String, String> {
        let response = self
            .http
            .get(CHANNELS_URL)
            .bearer_auth(access_token)
            .query(&[("part", "snippet"), ("mine", "true"), ("maxResults", "1")])
            .send()
            .await
            .map_err(|e| format!("接続したYouTubeチャンネルの確認に失敗しました: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(map_google_api_error(status, &body));
        }
        let body = response
            .json::<Value>()
            .await
            .map_err(|e| format!("YouTubeチャンネル情報の解析に失敗しました: {e}"))?;
        body.pointer("/items/0/snippet/title")
            .and_then(Value::as_str)
            .filter(|title| !title.trim().is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                "接続したGoogleアカウントにYouTubeチャンネルが見つかりません".to_string()
            })
    }

    async fn cache_access_token(
        &self,
        client_id: &str,
        access_token: &str,
        expires_in: Option<Duration>,
    ) {
        *self.access_token.lock().await = Some(CachedAccessToken {
            client_id: client_id.to_string(),
            value: access_token.to_string(),
            valid_until: safe_token_deadline(expires_in),
        });
    }

    async fn load_refresh_token(&self) -> Result<Option<StoredRefreshToken>, String> {
        let _guard = self.keyring_lock.lock().await;
        tokio::task::spawn_blocking(|| {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
            match entry.get_password() {
                Ok(value) => serde_json::from_str::<StoredRefreshToken>(&value)
                    .map(Some)
                    .map_err(|_| "保存済みYouTube認証情報が壊れています".to_string()),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(format!(
                    "OS資格情報ストアからYouTube認証を読めませんでした: {e}"
                )),
            }
        })
        .await
        .map_err(|e| format!("OS資格情報ストアの読込タスクに失敗しました: {e}"))?
    }

    async fn save_refresh_token(&self, token: StoredRefreshToken) -> Result<(), String> {
        let encoded = serde_json::to_string(&token)
            .map_err(|e| format!("YouTube認証情報の保存形式作成に失敗しました: {e}"))?;
        let _guard = self.keyring_lock.lock().await;
        tokio::task::spawn_blocking(move || {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
            entry
                .set_password(&encoded)
                .map_err(|e| format!("YouTube認証をOS資格情報ストアへ保存できませんでした: {e}"))
        })
        .await
        .map_err(|e| format!("OS資格情報ストアの保存タスクに失敗しました: {e}"))?
    }

    async fn delete_refresh_token(&self) -> Result<(), String> {
        let _guard = self.keyring_lock.lock().await;
        tokio::task::spawn_blocking(|| {
            let entry = keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER)
                .map_err(|e| format!("OS資格情報ストアを開けませんでした: {e}"))?;
            match entry.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(format!(
                    "YouTube認証をOS資格情報ストアから削除できませんでした: {e}"
                )),
            }
        })
        .await
        .map_err(|e| format!("OS資格情報ストアの削除タスクに失敗しました: {e}"))?
    }
}

enum SendError {
    Unauthorized,
    Message(String),
}

fn safe_token_deadline(expires_in: Option<Duration>) -> Instant {
    let ttl = expires_in.unwrap_or(Duration::from_secs(300));
    Instant::now() + ttl.saturating_sub(Duration::from_secs(60))
}

fn live_chat_message_body(live_chat_id: &str, text: &str) -> Value {
    json!({
        "snippet": {
            "liveChatId": live_chat_id,
            "type": "textMessageEvent",
            "textMessageDetails": { "messageText": text }
        }
    })
}

fn preferred_video_id(target: &str) -> Option<String> {
    let id = super::youtube::extract_video_id(target);
    super::youtube::is_video_id(&id).then_some(id)
}

fn select_live_chat_id(body: &Value, preferred_video_id: Option<&str>) -> Result<String, String> {
    let items = body
        .get("items")
        .and_then(Value::as_array)
        .ok_or_else(|| "YouTubeから配信一覧が返りませんでした".to_string())?;

    let get_chat = |item: &Value| {
        item.pointer("/snippet/liveChatId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
    };

    if let Some(video_id) = preferred_video_id {
        return items
            .iter()
            .find(|item| item.get("id").and_then(Value::as_str) == Some(video_id))
            .and_then(get_chat)
            .ok_or_else(|| "指定したYouTube配信は現在ライブ配信中ではありません".to_string());
    }

    let chats: Vec<String> = items.iter().filter_map(get_chat).collect();
    match chats.as_slice() {
        [only] => Ok(only.clone()),
        [] => Err("接続したGoogleアカウントで配信中のYouTube Liveが見つかりません".to_string()),
        _ => Err("複数のYouTube Liveが配信中です。送信先に配信URLを指定してください".to_string()),
    }
}

fn map_google_api_error(status: StatusCode, body: &str) -> String {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let reason = value
        .pointer("/error/errors/0/reason")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match reason {
        "rateLimitExceeded" => {
            "YouTubeの投稿制限に達しました。少し間隔を空けてから送信してください".to_string()
        }
        "liveChatDisabled" => "このYouTube配信ではチャットが無効です".to_string(),
        "liveChatEnded" => "このYouTubeライブチャットは終了しています".to_string(),
        "liveChatNotFound" => "YouTubeライブチャットが見つかりません".to_string(),
        "forbidden" | "insufficientPermissions" => {
            "このGoogleアカウントにはYouTubeチャットへ投稿する権限がありません".to_string()
        }
        "messageTextTooLong" => "YouTubeコメントが長すぎます".to_string(),
        _ if status == StatusCode::FORBIDDEN => {
            "YouTubeへの投稿が拒否されました。配信アカウントと権限を確認してください".to_string()
        }
        _ => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("詳細不明");
            format!("YouTube API エラー ({status}): {message}")
        }
    }
}

fn google_api_reason(body: &str) -> Option<String> {
    serde_json::from_str::<Value>(body)
        .ok()?
        .pointer("/error/errors/0/reason")?
        .as_str()
        .map(str::to_string)
}

enum CallbackTarget {
    Ignore,
    Code(String),
    Error(String),
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
    if let Some(error) = param("error") {
        return CallbackTarget::Error(error);
    }
    match param("code") {
        Some(code) if !code.is_empty() => CallbackTarget::Code(code),
        _ => CallbackTarget::Error("Googleから認証コードが返りませんでした".to_string()),
    }
}

async fn wait_for_oauth_callback(
    listener: TcpListener,
    expected_state: &str,
) -> Result<String, String> {
    let deadline = tokio::time::Instant::now() + CALLBACK_TIMEOUT;
    loop {
        let (mut stream, _) = tokio::time::timeout_at(deadline, listener.accept())
            .await
            .map_err(|_| "Google認証が時間切れになりました。もう一度接続してください".to_string())?
            .map_err(|e| format!("OAuthコールバックの受信に失敗しました: {e}"))?;

        let mut buffer = [0_u8; 8192];
        let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer))
            .await
            .map_err(|_| "OAuthコールバックの読込が時間切れになりました".to_string())?
            .map_err(|e| format!("OAuthコールバックを読めませんでした: {e}"))?;
        let request = String::from_utf8_lossy(&buffer[..read]);
        let target = request
            .lines()
            .next()
            .and_then(|line| {
                let mut parts = line.split_whitespace();
                (parts.next() == Some("GET"))
                    .then(|| parts.next())
                    .flatten()
            })
            .unwrap_or_default();

        match parse_callback_target(target, expected_state) {
            CallbackTarget::Ignore => {
                write_callback_response(
                    &mut stream,
                    "404 Not Found",
                    "認証コールバックを待っています",
                )
                .await;
            }
            CallbackTarget::Code(code) => {
                write_callback_response(
                    &mut stream,
                    "200 OK",
                    "Googleアカウントとの接続が完了しました。このタブを閉じてfast-commentへ戻ってください。",
                )
                .await;
                return Ok(code);
            }
            CallbackTarget::Error(error) => {
                write_callback_response(
                    &mut stream,
                    "400 Bad Request",
                    "Googleアカウントとの接続は完了しませんでした。このタブを閉じてfast-commentへ戻ってください。",
                )
                .await;
                return Err(format!("Google認証が完了しませんでした: {error}"));
            }
        }
    }
}

async fn write_callback_response(stream: &mut TcpStream, status: &str, message: &str) {
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
mod tests {
    use super::*;

    #[test]
    fn callback_requires_matching_state_and_returns_decoded_code() {
        match parse_callback_target("/?state=known-state&code=a%2Fb%2Bc", "known-state") {
            CallbackTarget::Code(code) => assert_eq!(code, "a/b+c"),
            _ => panic!("matching callback must return code"),
        }
        assert!(matches!(
            parse_callback_target("/?state=wrong&code=secret", "known-state"),
            CallbackTarget::Ignore
        ));
        assert!(matches!(
            parse_callback_target("/favicon.ico", "known-state"),
            CallbackTarget::Ignore
        ));
    }

    #[test]
    fn selects_exact_broadcast_for_direct_video_and_first_for_channel_target() {
        let body = json!({
            "items": [
                {"id": "aaaaaaaaaaa", "snippet": {"liveChatId": "chat-a"}},
                {"id": "bbbbbbbbbbb", "snippet": {"liveChatId": "chat-b"}}
            ]
        });
        assert_eq!(
            select_live_chat_id(&body, Some("bbbbbbbbbbb")).unwrap(),
            "chat-b"
        );
        assert!(select_live_chat_id(&body, None).is_err());
        assert!(select_live_chat_id(&body, Some("ccccccccccc")).is_err());
    }

    #[test]
    fn selects_only_active_broadcast_for_channel_target() {
        let body = json!({
            "items": [{"id": "aaaaaaaaaaa", "snippet": {"liveChatId": "chat-a"}}]
        });
        assert_eq!(select_live_chat_id(&body, None).unwrap(), "chat-a");
    }

    #[test]
    fn legacy_stored_token_without_channel_title_still_deserializes() {
        let stored: StoredRefreshToken =
            serde_json::from_str(r#"{"client_id":"client","refresh_token":"refresh"}"#).unwrap();
        assert_eq!(stored.channel_title, None);
    }

    #[test]
    fn maps_rate_limit_and_permission_errors_to_actionable_messages() {
        let rate = json!({"error":{"errors":[{"reason":"rateLimitExceeded"}]}});
        assert!(map_google_api_error(StatusCode::FORBIDDEN, &rate.to_string()).contains("間隔"));
        let permission = json!({"error":{"errors":[{"reason":"insufficientPermissions"}]}});
        assert!(
            map_google_api_error(StatusCode::FORBIDDEN, &permission.to_string()).contains("権限")
        );
    }

    #[test]
    fn builds_official_text_message_event_body() {
        let body = live_chat_message_body("chat-id", "運営コメントです");
        assert_eq!(
            body.pointer("/snippet/liveChatId").and_then(Value::as_str),
            Some("chat-id")
        );
        assert_eq!(
            body.pointer("/snippet/type").and_then(Value::as_str),
            Some("textMessageEvent")
        );
        assert_eq!(
            body.pointer("/snippet/textMessageDetails/messageText")
                .and_then(Value::as_str),
            Some("運営コメントです")
        );
    }

    #[test]
    fn recognizes_video_targets_without_treating_channel_urls_as_video_ids() {
        assert_eq!(
            preferred_video_id("https://www.youtube.com/live/dQw4w9WgXcQ?feature=share"),
            Some("dQw4w9WgXcQ".to_string())
        );
        assert_eq!(
            preferred_video_id("https://www.youtube.com/@fastcomment"),
            None
        );
    }
}
