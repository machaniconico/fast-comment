//! YouTube Live の実モデレーション(コメント削除 / BAN / タイムアウト / 解除)。
//!
//! 認証は既存の `YoutubeAuth`(投稿用 OAuth、スコープ `youtube.force-ssl`)を
//! そのまま使う。`liveChatId` は配信者自身の配信しか返さない `liveBroadcasts.list`
//! ではなく、`videos.list?part=liveStreamingDetails` から解決する。
//!
//! API 応答は固い struct にせず `serde_json::Value` のパス探索で読む。

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::youtube_send::{google_api_reason, SendError, YoutubeAuth};
use crate::moderation_error::{ModerationError, ModerationErrorCode};

const VIDEOS_URL: &str = "https://www.googleapis.com/youtube/v3/videos";
const LIVE_CHAT_MESSAGES_URL: &str = "https://www.googleapis.com/youtube/v3/liveChat/messages";
const LIVE_CHAT_BANS_URL: &str = "https://www.googleapis.com/youtube/v3/liveChat/bans";
const MODERATION_LIVE_CHAT_TTL: Duration = Duration::from_secs(600);
const TEMPORARY_BAN_MIN_SECS: u32 = 1;
/// アプリ側の上限(24 時間)。YouTube は最大値を明記しておらず、既定は 300 秒。
const TEMPORARY_BAN_MAX_SECS: u32 = 86_400;
const NON_DELETABLE_MESSAGE: &str =
    "この受信方式のコメントは削除できません(YouTube API キー設定時の受信分のみ削除可)。タイムアウト/BAN は使えます";
const UNBAN_NOT_IN_SESSION: &str =
    "このセッションでBANしたユーザーのみ解除できます。YouTube Studio で解除してください";
const BAN_ID_UNAVAILABLE: &str =
    "BAN は成立した可能性がありますが、解除情報を取得できませんでした。解除は YouTube Studio で行ってください";
const BAN_ALREADY_REMOVED: &str =
    "対象のBANが見つかりません(既に解除されている可能性があります)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModerationAction {
    DeleteMessage,
    Ban,
    Unban,
}

#[derive(Clone)]
struct CachedModerationChat {
    live_chat_id: String,
    valid_until: Instant,
}

static MODERATION_CHATS: OnceLock<Mutex<HashMap<String, CachedModerationChat>>> = OnceLock::new();
static BAN_IDS: OnceLock<Mutex<BanIdCache>> = OnceLock::new();
/// 認証の世代番号。接続成功(再認証含む)・切断・クライアントID変更で進める。
///
/// モデレーション要求は開始時の世代を控え、キャッシュの読み書きは世代が
/// 一致する時だけ行う。別アカウントへ切り替わった後に旧アカウントの応答が
/// banId を書き戻したり、旧アカウントの banId で解除したりするのを防ぐ。
static AUTH_GENERATION: AtomicU64 = AtomicU64::new(0);

fn current_auth_generation() -> u64 {
    AUTH_GENERATION.load(Ordering::SeqCst)
}

/// 要求開始時の世代 `request` が現在の世代 `current` のままか。
fn is_current_generation(request: u64, current: u64) -> bool {
    request == current
}

fn generation_still_current(request: u64) -> bool {
    is_current_generation(request, current_auth_generation())
}

fn moderation_chats() -> &'static Mutex<HashMap<String, CachedModerationChat>> {
    MODERATION_CHATS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn ban_ids() -> &'static Mutex<BanIdCache> {
    BAN_IDS.get_or_init(|| Mutex::new(BanIdCache::default()))
}

/// `(liveChatId, channelId) → banId` のセッション内メモリ保持。
///
/// banId は BAN の insert 応答でしか得られないため、このアプリで BAN した
/// ユーザーだけを同じセッション中に解除できる。
#[derive(Default)]
struct BanIdCache {
    entries: HashMap<(String, String), String>,
}

impl BanIdCache {
    fn insert(&mut self, live_chat_id: &str, channel_id: &str, ban_id: &str) {
        self.entries.insert(
            (live_chat_id.to_string(), channel_id.to_string()),
            ban_id.to_string(),
        );
    }

    fn get(&self, live_chat_id: &str, channel_id: &str) -> Option<String> {
        self.entries
            .get(&(live_chat_id.to_string(), channel_id.to_string()))
            .cloned()
    }

    fn has_channel(&self, channel_id: &str) -> bool {
        self.entries.keys().any(|(_, channel)| channel == channel_id)
    }

    fn remove(&mut self, live_chat_id: &str, channel_id: &str) {
        self.entries
            .remove(&(live_chat_id.to_string(), channel_id.to_string()));
    }
}

async fn cached_moderation_live_chat(generation: u64, video_id: &str) -> Option<String> {
    let mut cache = moderation_chats().lock().await;
    if !generation_still_current(generation) {
        return None;
    }
    let now = Instant::now();
    match cache.get(video_id) {
        Some(entry) if entry.valid_until > now => Some(entry.live_chat_id.clone()),
        Some(_) => {
            cache.remove(video_id);
            None
        }
        None => None,
    }
}

async fn cache_moderation_live_chat(generation: u64, video_id: &str, live_chat_id: &str) {
    let mut cache = moderation_chats().lock().await;
    if !generation_still_current(generation) {
        return;
    }
    cache.insert(
        video_id.to_string(),
        CachedModerationChat {
            live_chat_id: live_chat_id.to_string(),
            valid_until: Instant::now() + MODERATION_LIVE_CHAT_TTL,
        },
    );
}

async fn purge_moderation_live_chat(video_id: &str) {
    moderation_chats().lock().await.remove(video_id);
}

async fn store_ban_id(generation: u64, live_chat_id: &str, channel_id: &str, ban_id: &str) {
    let mut cache = ban_ids().lock().await;
    if generation_still_current(generation) {
        cache.insert(live_chat_id, channel_id, ban_id);
    }
}

async fn lookup_ban_id(generation: u64, live_chat_id: &str, channel_id: &str) -> Option<String> {
    let cache = ban_ids().lock().await;
    if !generation_still_current(generation) {
        return None;
    }
    cache.get(live_chat_id, channel_id)
}

async fn has_ban_for_channel(generation: u64, channel_id: &str) -> bool {
    let cache = ban_ids().lock().await;
    generation_still_current(generation) && cache.has_channel(channel_id)
}

/// 接続成功・切断・アカウント切替時に認証世代を進め、liveChatId と banId の
/// キャッシュを捨てる。
///
/// 世代を先に進めてから消去するので、消去前に旧世代の書き込みが入っても
/// 消去で捨てられ、消去後の旧世代の書き込みは世代不一致で拒否される。
pub(super) async fn clear_moderation_caches() {
    AUTH_GENERATION.fetch_add(1, Ordering::SeqCst);
    moderation_chats().lock().await.clear();
    *ban_ids().lock().await = BanIdCache::default();
}

async fn forget_ban_id(generation: u64, live_chat_id: &str, channel_id: &str) {
    let mut cache = ban_ids().lock().await;
    if generation_still_current(generation) {
        cache.remove(live_chat_id, channel_id);
    }
}

impl YoutubeAuth {
    /// コメントを配信側から削除する。
    ///
    /// `LCC.` 形式(YouTube Data API の公式 streamList 受信分)だけを対象にし、
    /// InnerTube 受信分は §10 のユーザー決定に従って無効表示にする。
    pub async fn delete_message(
        &self,
        client_id: &str,
        video_id: &str,
        message_id: &str,
    ) -> Result<(), ModerationError> {
        let video_id = video_id.trim();
        if video_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "削除対象のYouTube配信IDが空です",
            ));
        }
        let message_id = resolve_api_message_id(message_id).ok_or_else(|| {
            ModerationError::new(ModerationErrorCode::Unavailable, NON_DELETABLE_MESSAGE)
        })?;

        let this = self;
        let result = self
            .with_token_retry(client_id, move |access_token: String| {
                let this = this;
                let message_id = message_id.clone();
                async move {
                    this.delete_message_once(video_id, &message_id, &access_token)
                        .await
                }
            })
            .await;
        result.map_err(|error| map_send_error(error, ModerationAction::DeleteMessage))
    }

    /// ユーザーを BAN する。`duration_secs: None` は永久 BAN。
    ///
    /// YouTube の liveChatBans には reason 項目が無いため、呼び出し側の reason は
    /// ここでは扱わない。
    pub async fn ban_user(
        &self,
        client_id: &str,
        video_id: &str,
        channel_id: &str,
        duration_secs: Option<u32>,
    ) -> Result<(), ModerationError> {
        let video_id = video_id.trim();
        let channel_id = channel_id.trim();
        if video_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN対象のYouTube配信IDが空です",
            ));
        }
        if channel_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN対象のユーザーIDが空です",
            ));
        }

        if duration_secs == Some(0) {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "タイムアウト時間が不正です",
            ));
        }

        let generation = current_auth_generation();
        let this = self;
        let result = self
            .with_token_retry(client_id, move |access_token: String| {
                let this = this;
                async move {
                    this.ban_user_once(
                        generation,
                        video_id,
                        channel_id,
                        duration_secs,
                        &access_token,
                    )
                    .await
                }
            })
            .await;
        result.map_err(|error| map_send_error(error, ModerationAction::Ban))
    }

    /// このセッションで BAN したユーザーの BAN を解除する。
    pub async fn unban_user(
        &self,
        client_id: &str,
        video_id: &str,
        channel_id: &str,
    ) -> Result<(), ModerationError> {
        let video_id = video_id.trim();
        let channel_id = channel_id.trim();
        if video_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN解除対象のYouTube配信IDが空です",
            ));
        }
        if channel_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN解除対象のユーザーIDが空です",
            ));
        }

        let generation = current_auth_generation();
        let this = self;
        let result = self
            .with_token_retry(client_id, move |access_token: String| {
                let this = this;
                async move {
                    this.unban_user_once(generation, video_id, channel_id, &access_token)
                        .await
                }
            })
            .await;
        result.map_err(|error| map_send_error(error, ModerationAction::Unban))
    }

    async fn moderation_live_chat_id(
        &self,
        generation: u64,
        video_id: &str,
        access_token: &str,
    ) -> Result<String, SendError> {
        if let Some(live_chat_id) = cached_moderation_live_chat(generation, video_id).await {
            return Ok(live_chat_id);
        }

        let response = self
            .http
            .get(VIDEOS_URL)
            .bearer_auth(access_token)
            .query(&[("part", "liveStreamingDetails"), ("id", video_id)])
            .send()
            .await
            .map_err(|e| SendError::Network(format!("YouTube配信情報の取得に失敗しました: {e}")))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            if is_stale_live_chat_body(&body) {
                purge_moderation_live_chat(video_id).await;
            }
            return Err(SendError::Api { status, body });
        }

        let value = serde_json::from_str::<Value>(&body)
            .map_err(|e| SendError::Message(format!("YouTube配信情報の解析に失敗しました: {e}")))?;
        let live_chat_id = select_active_live_chat_id(&value).ok_or_else(|| {
            SendError::Moderation(ModerationError::new(
                ModerationErrorCode::Unavailable,
                "このYouTube配信はライブ配信中ではないか、ライブチャットが無効です",
            ))
        })?;
        cache_moderation_live_chat(generation, video_id, &live_chat_id).await;
        Ok(live_chat_id)
    }

    async fn delete_message_once(
        &self,
        video_id: &str,
        message_id: &str,
        access_token: &str,
    ) -> Result<(), SendError> {
        let response = self
            .http
            .delete(LIVE_CHAT_MESSAGES_URL)
            .bearer_auth(access_token)
            .query(&[("id", message_id)])
            .send()
            .await
            .map_err(|e| SendError::Network(format!("YouTubeコメントの削除に失敗しました: {e}")))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if status.is_success() {
            return Ok(());
        }
        if is_stale_live_chat_body(&body) {
            purge_moderation_live_chat(video_id).await;
        }
        Err(SendError::Api { status, body })
    }

    async fn ban_user_once(
        &self,
        generation: u64,
        video_id: &str,
        channel_id: &str,
        duration_secs: Option<u32>,
        access_token: &str,
    ) -> Result<(), SendError> {
        let live_chat_id = self
            .moderation_live_chat_id(generation, video_id, access_token)
            .await?;
        let request_body = ban_request_body(&live_chat_id, channel_id, duration_secs);

        let response = self
            .http
            .post(LIVE_CHAT_BANS_URL)
            .bearer_auth(access_token)
            .query(&[("part", "snippet")])
            .json(&request_body)
            .send()
            .await
            .map_err(|e| SendError::Network(format!("YouTubeのBANに失敗しました: {e}")))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            if is_stale_live_chat_body(&body) {
                purge_moderation_live_chat(video_id).await;
            }
            return Err(SendError::Api { status, body });
        }

        // BAN は成立済みなので、ここで失敗しても自動再試行はしない(二重 BAN 回避)。
        let body = response.text().await.ok();
        let ban_id = ban_id_from_response(body.as_deref()).map_err(SendError::Moderation)?;
        store_ban_id(generation, &live_chat_id, channel_id, &ban_id).await;
        Ok(())
    }

    async fn unban_user_once(
        &self,
        generation: u64,
        video_id: &str,
        channel_id: &str,
        access_token: &str,
    ) -> Result<(), SendError> {
        if !has_ban_for_channel(generation, channel_id).await {
            return Err(SendError::Moderation(ModerationError::new(
                ModerationErrorCode::Unavailable,
                UNBAN_NOT_IN_SESSION,
            )));
        }
        let live_chat_id = self
            .moderation_live_chat_id(generation, video_id, access_token)
            .await?;
        let ban_id = lookup_ban_id(generation, &live_chat_id, channel_id)
            .await
            .ok_or_else(|| {
                SendError::Moderation(ModerationError::new(
                    ModerationErrorCode::Unavailable,
                    UNBAN_NOT_IN_SESSION,
                ))
            })?;

        let response = self
            .http
            .delete(LIVE_CHAT_BANS_URL)
            .bearer_auth(access_token)
            .query(&[("id", ban_id.as_str())])
            .send()
            .await
            .map_err(|e| SendError::Network(format!("YouTubeのBAN解除に失敗しました: {e}")))?;

        if response.status() == StatusCode::UNAUTHORIZED {
            return Err(SendError::Unauthorized);
        }
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            if is_stale_live_chat_body(&body) {
                purge_moderation_live_chat(video_id).await;
            }
            if is_ban_not_found_body(&body) {
                forget_ban_id(generation, &live_chat_id, channel_id).await;
            }
            return Err(SendError::Api { status, body });
        }

        forget_ban_id(generation, &live_chat_id, channel_id).await;
        Ok(())
    }
}

fn map_send_error(error: SendError, action: ModerationAction) -> ModerationError {
    match error {
        SendError::Unauthorized => ModerationError::new(
            ModerationErrorCode::NotLoggedIn,
            "YouTubeの認証が切れています。設定からGoogleへ再接続してください",
        ),
        SendError::NotLoggedIn(message) => {
            ModerationError::new(ModerationErrorCode::NotLoggedIn, message)
        }
        SendError::Api { status, body } => map_youtube_moderation_error(status, &body, action),
        SendError::Network(message) => ModerationError::new(ModerationErrorCode::Network, message),
        SendError::Message(message) => ModerationError::new(ModerationErrorCode::Other, message),
        SendError::Moderation(error) => error,
    }
}

pub(crate) fn map_youtube_moderation_error(
    status: StatusCode,
    body: &str,
    action: ModerationAction,
) -> ModerationError {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let reason = value
        .pointer("/error/errors/0/reason")
        .and_then(Value::as_str)
        .unwrap_or_default();

    if status == StatusCode::UNAUTHORIZED {
        return ModerationError::new(
            ModerationErrorCode::NotLoggedIn,
            "YouTubeの認証が切れています。設定からGoogleへ再接続してください",
        );
    }

    match reason {
        "forbidden" | "insufficientPermissions" => ModerationError::new(
            ModerationErrorCode::NotModerator,
            "このGoogleアカウントにはYouTubeチャットをモデレーションする権限がありません",
        ),
        "modificationNotAllowed"
        | "liveChatBanInsertionNotAllowed"
        | "liveChatBanDeletionNotAllowed" => ModerationError::new(
            ModerationErrorCode::ProtectedTarget,
            "配信者・モデレーターなど、この操作を行えないユーザーです",
        ),
        "rateLimitExceeded" | "userRateLimitExceeded" | "dailyLimitExceeded" => ModerationError::new(
            ModerationErrorCode::RateLimited,
            "YouTubeの操作制限に達しました。少し間隔を空けてから再試行してください",
        ),
        "quotaExceeded" => ModerationError::new(
            ModerationErrorCode::RateLimited,
            "YouTube API の割り当て(quota)を使い切りました。時間を置いて再試行してください",
        ),
        "liveChatBanNotFound" => {
            ModerationError::new(ModerationErrorCode::NotFound, BAN_ALREADY_REMOVED)
        }
        "liveChatMessageNotFound" | "liveChatUserNotFound" => {
            ModerationError::new(ModerationErrorCode::NotFound, not_found_message(action))
        }
        "liveChatNotFound" | "invalidLiveChatId" => ModerationError::new(
            ModerationErrorCode::Unavailable,
            "YouTubeライブチャットが見つかりません。配信が終了した可能性があります",
        ),
        "liveChatEnded" => ModerationError::new(
            ModerationErrorCode::Unavailable,
            "このYouTubeライブチャットは終了しています",
        ),
        "liveChatDisabled" => ModerationError::new(
            ModerationErrorCode::Unavailable,
            "このYouTube配信ではチャットが無効です",
        ),
        _ if status == StatusCode::CONFLICT => {
            ModerationError::new(ModerationErrorCode::Conflict, "YouTube側で既に処理済みです")
        }
        _ if status == StatusCode::TOO_MANY_REQUESTS => ModerationError::new(
            ModerationErrorCode::RateLimited,
            "YouTubeの操作制限に達しました。少し間隔を空けてから再試行してください",
        ),
        _ => {
            let message = value
                .pointer("/error/message")
                .and_then(Value::as_str)
                .filter(|message| !message.trim().is_empty())
                .unwrap_or("詳細不明");
            ModerationError::new(
                ModerationErrorCode::Other,
                format!("YouTube API エラー ({status}): {message}"),
            )
        }
    }
}

fn not_found_message(action: ModerationAction) -> &'static str {
    match action {
        ModerationAction::DeleteMessage => {
            "削除対象のコメントが見つかりません(既に削除された可能性があります)"
        }
        ModerationAction::Ban | ModerationAction::Unban => "対象のユーザーが見つかりません",
    }
}

/// liveChatId が変わった/終了したことを示す応答か。true ならキャッシュを捨てる。
fn is_stale_live_chat_body(body: &str) -> bool {
    matches!(
        google_api_reason(body).as_deref(),
        Some("liveChatNotFound" | "liveChatEnded" | "invalidLiveChatId")
    )
}

/// 解除しようとした BAN が既に無い(期限切れ・Studio で解除済み等)応答か。
fn is_ban_not_found_body(body: &str) -> bool {
    google_api_reason(body).as_deref() == Some("liveChatBanNotFound")
}

/// liveChatBans.insert の成功応答から banId を取り出す。
///
/// 本文の読み取り失敗(`None`)・JSON 解析失敗・`id` 欠落は、BAN 自体は成立した
/// 可能性があるため「解除は Studio で」と案内するエラーにする。
pub(crate) fn ban_id_from_response(body: Option<&str>) -> Result<String, ModerationError> {
    body.and_then(|body| serde_json::from_str::<Value>(body).ok())
        .and_then(|value| {
            value
                .get("id")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string)
        })
        .ok_or_else(|| ModerationError::new(ModerationErrorCode::Other, BAN_ID_UNAVAILABLE))
}

pub(crate) fn ban_request_body(
    live_chat_id: &str,
    channel_id: &str,
    duration_secs: Option<u32>,
) -> Value {
    let mut snippet = json!({
        "liveChatId": live_chat_id,
        "bannedUserDetails": { "channelId": channel_id },
    });

    match duration_secs {
        None => snippet["type"] = json!("permanent"),
        Some(seconds) => {
            let seconds = seconds
                .max(TEMPORARY_BAN_MIN_SECS)
                .min(TEMPORARY_BAN_MAX_SECS);
            snippet["type"] = json!("temporary");
            snippet["banDurationSeconds"] = json!(seconds);
        }
    }

    json!({ "snippet": snippet })
}

pub(crate) fn select_active_live_chat_id(body: &Value) -> Option<String> {
    body.get("items")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(|item| {
            item.pointer("/liveStreamingDetails/activeLiveChatId")
                .and_then(Value::as_str)
        })
        .find(|id| !id.trim().is_empty())
        .map(str::to_string)
}

/// `LCC.` 形式(Data API 受信分)だけを削除 API に渡す。
///
/// InnerTube 受信分は API の message ID と一致しないため `None` を返し、
/// 呼び出し側で「削除できません」の案内にする。
pub(crate) fn resolve_api_message_id(message_id: &str) -> Option<String> {
    let id = message_id.trim();
    (id.starts_with("LCC.") && id.len() > "LCC.".len()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(status: StatusCode, reason: &str) -> ModerationError {
        let body = json!({"error":{"errors":[{"reason":reason}]}}).to_string();
        map_youtube_moderation_error(status, &body, ModerationAction::DeleteMessage)
    }

    #[test]
    fn permanent_ban_body_has_no_duration() {
        let body = ban_request_body("chat-id", "channel-id", None);
        assert_eq!(
            body.pointer("/snippet/type").and_then(Value::as_str),
            Some("permanent")
        );
        assert!(body.pointer("/snippet/banDurationSeconds").is_none());
        assert_eq!(
            body.pointer("/snippet/liveChatId").and_then(Value::as_str),
            Some("chat-id")
        );
        assert_eq!(
            body.pointer("/snippet/bannedUserDetails/channelId")
                .and_then(Value::as_str),
            Some("channel-id")
        );
    }

    #[test]
    fn temporary_ban_body_clamps_duration_to_youtube_bounds() {
        let body = ban_request_body("chat-id", "channel-id", Some(600));
        assert_eq!(
            body.pointer("/snippet/type").and_then(Value::as_str),
            Some("temporary")
        );
        assert_eq!(
            body.pointer("/snippet/banDurationSeconds")
                .and_then(Value::as_u64),
            Some(600)
        );

        for (input, expected) in [(1, 1), (86_400, 86_400), (86_401, 86_400)] {
            let body = ban_request_body("chat-id", "channel-id", Some(input));
            assert_eq!(
                body.pointer("/snippet/banDurationSeconds")
                    .and_then(Value::as_u64),
                Some(expected),
                "duration {input} should clamp to {expected}"
            );
        }
    }

    #[test]
    fn google_error_without_errors_array_is_other() {
        let body = json!({"error":{"code":400,"message":"bad","status":"INVALID_ARGUMENT","details":[{"reason":"X"}]}}).to_string();
        let error =
            map_youtube_moderation_error(StatusCode::BAD_REQUEST, &body, ModerationAction::Ban);
        assert_eq!(error.code, ModerationErrorCode::Other);
    }

    #[test]
    fn maps_new_rate_limit_and_invalid_chat_reasons() {
        for reason in ["userRateLimitExceeded", "dailyLimitExceeded"] {
            assert_eq!(
                api_error(StatusCode::FORBIDDEN, reason).code,
                ModerationErrorCode::RateLimited
            );
        }
        assert_eq!(
            api_error(StatusCode::BAD_REQUEST, "invalidLiveChatId").code,
            ModerationErrorCode::Unavailable
        );
        assert!(is_stale_live_chat_body(
            &json!({"error":{"errors":[{"reason":"invalidLiveChatId"}]}}).to_string()
        ));
    }

    #[test]
    fn selects_first_non_empty_active_live_chat_id() {
        assert_eq!(select_active_live_chat_id(&json!({})), None);
        assert_eq!(select_active_live_chat_id(&json!({"items": []})), None);
        assert_eq!(
            select_active_live_chat_id(&json!({"items": [{"liveStreamingDetails": {}}]})),
            None
        );
        assert_eq!(
            select_active_live_chat_id(
                &json!({"items": [{"liveStreamingDetails": {"activeLiveChatId": ""}}]})
            ),
            None
        );
        assert_eq!(
            select_active_live_chat_id(
                &json!({"items": [{"liveStreamingDetails": {"activeLiveChatId": 123}}]})
            ),
            None
        );
        assert_eq!(
            select_active_live_chat_id(&json!({
                "items": [
                    {"liveStreamingDetails": {"activeLiveChatId": ""}},
                    {"liveStreamingDetails": {"activeLiveChatId": "chat-b"}}
                ]
            })),
            Some("chat-b".to_string())
        );
    }

    #[test]
    fn maps_moderation_errors_to_codes() {
        assert_eq!(
            api_error(StatusCode::UNAUTHORIZED, "unauthorized").code,
            ModerationErrorCode::NotLoggedIn
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "forbidden").code,
            ModerationErrorCode::NotModerator
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "insufficientPermissions").code,
            ModerationErrorCode::NotModerator
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "modificationNotAllowed").code,
            ModerationErrorCode::ProtectedTarget
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "liveChatBanInsertionNotAllowed").code,
            ModerationErrorCode::ProtectedTarget
        );
        assert_eq!(
            api_error(StatusCode::NOT_FOUND, "liveChatMessageNotFound").code,
            ModerationErrorCode::NotFound
        );
        assert_eq!(
            api_error(StatusCode::NOT_FOUND, "liveChatUserNotFound").code,
            ModerationErrorCode::NotFound
        );
        assert_eq!(
            api_error(StatusCode::NOT_FOUND, "liveChatNotFound").code,
            ModerationErrorCode::Unavailable
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "rateLimitExceeded").code,
            ModerationErrorCode::RateLimited
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "quotaExceeded").code,
            ModerationErrorCode::RateLimited
        );
        assert_eq!(
            api_error(StatusCode::CONFLICT, "somethingNew").code,
            ModerationErrorCode::Conflict
        );
        assert_eq!(
            api_error(StatusCode::BAD_REQUEST, "somethingNew").code,
            ModerationErrorCode::Other
        );
    }

    #[test]
    fn unknown_reason_is_reported_as_other() {
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "brandNewReason").code,
            ModerationErrorCode::Other
        );
    }

    #[test]
    fn resolves_only_lcc_message_ids() {
        assert_eq!(
            resolve_api_message_id("LCC.abc123"),
            Some("LCC.abc123".to_string())
        );
        assert_eq!(resolve_api_message_id("InnerTubeMessageId"), None);
        assert_eq!(resolve_api_message_id(""), None);
        assert_eq!(resolve_api_message_id("LCC."), None);
    }

    #[test]
    fn ban_id_cache_round_trips_per_live_chat_and_channel() {
        let mut cache = BanIdCache::default();
        assert_eq!(cache.get("chat-a", "user-a"), None);

        cache.insert("chat-a", "user-a", "ban-a");
        assert_eq!(cache.get("chat-a", "user-a"), Some("ban-a".to_string()));
        assert_eq!(cache.get("chat-b", "user-a"), None);
        assert_eq!(cache.get("chat-a", "user-b"), None);

        cache.remove("chat-a", "user-a");
        assert_eq!(cache.get("chat-a", "user-a"), None);
    }

    #[test]
    fn cache_access_requires_unchanged_auth_generation() {
        assert!(is_current_generation(0, 0));
        assert!(is_current_generation(7, 7));
        // 要求開始後に再接続・切断・クライアントID変更で世代が進んだ場合
        assert!(!is_current_generation(0, 1));
        assert!(!is_current_generation(7, 9));
        // 世代は単調増加だが、念のため逆向きも不一致扱い
        assert!(!is_current_generation(2, 1));
        assert!(is_current_generation(u64::MAX, u64::MAX));
    }

    #[test]
    fn ban_id_is_read_from_successful_insert_response() {
        assert_eq!(
            ban_id_from_response(Some(r#"{"kind":"youtube#liveChatBan","id":"ban-1"}"#)),
            Ok("ban-1".to_string())
        );
        assert_eq!(
            ban_id_from_response(Some(r#"{"id":"  ban-2  "}"#)),
            Ok("ban-2".to_string())
        );
    }

    #[test]
    fn unreadable_ban_response_reports_possible_ban_without_unban_info() {
        let cases: [Option<&str>; 7] = [
            None,
            Some(""),
            Some("not json"),
            Some("{}"),
            Some(r#"{"id":""}"#),
            Some(r#"{"id":"   "}"#),
            Some(r#"{"id":123}"#),
        ];
        for body in cases {
            let error = ban_id_from_response(body).unwrap_err();
            assert_eq!(error.code, ModerationErrorCode::Other, "{body:?}");
            assert!(error.message.contains("成立した可能性"), "{body:?}");
            assert!(error.message.contains("YouTube Studio"), "{body:?}");
        }
    }

    #[test]
    fn ban_not_found_on_unban_is_mapped_to_already_removed() {
        let body = json!({"error":{"code":404,"errors":[{"reason":"liveChatBanNotFound"}]}})
            .to_string();
        let error = map_youtube_moderation_error(StatusCode::NOT_FOUND, &body, ModerationAction::Unban);
        assert_eq!(error.code, ModerationErrorCode::NotFound);
        assert!(error.message.contains("既に解除されている可能性"));
        assert!(is_ban_not_found_body(&body));
        assert!(!is_stale_live_chat_body(&body));
        assert!(!is_ban_not_found_body(
            &json!({"error":{"errors":[{"reason":"liveChatNotFound"}]}}).to_string()
        ));
        assert!(!is_ban_not_found_body("not json"));
    }
}
