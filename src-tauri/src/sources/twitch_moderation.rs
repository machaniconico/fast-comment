//! Twitch Helix による実モデレーション(コメント削除 / BAN / タイムアウト / 解除)。
//!
//! 認証は `TwitchAuth`(Device Code Flow)の access token と保存済み `user_id` を使う。
//! broadcaster_id は `/helix/users?login=` で解決し、認証世代付きで無期限キャッシュする。
//! API 応答は固い struct にせず `serde_json::Value` のパス探索で寛容に読む。

use std::collections::HashMap;
use std::sync::OnceLock;

use reqwest::StatusCode;
use serde_json::{json, Value};
use tokio::sync::Mutex;

use super::twitch_auth::{is_current_generation, scopes_satisfied, TwitchAccess, TwitchAuth};
use crate::moderation_error::{ModerationError, ModerationErrorCode};

const SCOPE_MISSING_MESSAGE: &str =
    "Twitchの認証にモデレーション用スコープがありません。設定からTwitchへ再接続してください";

const USERS_URL: &str = "https://api.twitch.tv/helix/users";
const BANS_URL: &str = "https://api.twitch.tv/helix/moderation/bans";
const CHAT_URL: &str = "https://api.twitch.tv/helix/moderation/chat";
const MIN_TIMEOUT_SECS: u32 = 1;
const MAX_TIMEOUT_SECS: u32 = 1_209_600;
const MAX_REASON_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModerationAction {
    DeleteMessage,
    Ban,
    Unban,
}

#[derive(Clone)]
struct CachedUserId {
    id: String,
    generation: u64,
}

static USER_IDS: OnceLock<Mutex<HashMap<String, CachedUserId>>> = OnceLock::new();

fn user_ids() -> &'static Mutex<HashMap<String, CachedUserId>> {
    USER_IDS.get_or_init(|| Mutex::new(HashMap::new()))
}

async fn cached_user_id(generation: u64, login: &str) -> Option<String> {
    user_ids()
        .lock()
        .await
        .get(login)
        .filter(|entry| entry.generation == generation)
        .map(|entry| entry.id.clone())
}

async fn cache_user_id(generation: u64, login: &str, id: &str) {
    user_ids().lock().await.insert(
        login.to_string(),
        CachedUserId {
            id: id.to_string(),
            generation,
        },
    );
}

enum HelixError {
    Api { status: StatusCode, body: String },
    Network(String),
    Moderation(ModerationError),
}

impl TwitchAuth {
    /// 配信側のコメントを削除する。`message_id` は必ず送る(空なら拒否)。
    pub async fn delete_message(
        &self,
        client_id: &str,
        client_secret: &str,
        channel: &str,
        message_id: &str,
    ) -> Result<(), ModerationError> {
        let channel = normalize_login(channel);
        let message_id = message_id.trim().to_string();
        if channel.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "削除対象のTwitchチャンネルが空です",
            ));
        }
        if message_id.is_empty() {
            // 省略するとチャット全消去になるため、絶対にリクエストを送らない。
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "削除するメッセージIDが空です",
            ));
        }

        let generation = self.generation();
        let this = self;
        let channel_ref = channel.as_str();
        let message_id_ref = message_id.as_str();
        let result = self
            .with_token_retry(client_id, client_secret, move |access| {
                let this = this;
                async move {
                    this.delete_message_once(generation, channel_ref, message_id_ref, access)
                        .await
                }
            })
            .await;
        result.map_err(|error| map_helix_error(error, ModerationAction::DeleteMessage))
    }

    /// ユーザーを BAN する。`duration_secs: None` は永久 BAN。
    ///
    /// `Some(0)` は不正として拒否し、1〜1,209,600 秒へクランプする。
    pub async fn ban_user(
        &self,
        client_id: &str,
        client_secret: &str,
        channel: &str,
        author_id: &str,
        duration_secs: Option<u32>,
        reason: Option<&str>,
    ) -> Result<(), ModerationError> {
        let channel = normalize_login(channel);
        let author_id = author_id.trim().to_string();
        if channel.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN対象のTwitchチャンネルが空です",
            ));
        }
        if author_id.is_empty() {
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
        let reason = reason.map(str::to_string);

        let generation = self.generation();
        let this = self;
        let channel_ref = channel.as_str();
        let author_ref = author_id.as_str();
        let reason_ref = reason.as_deref();
        let result = self
            .with_token_retry(client_id, client_secret, move |access| {
                let this = this;
                async move {
                    this.ban_user_once(
                        generation,
                        channel_ref,
                        author_ref,
                        duration_secs,
                        reason_ref,
                        access,
                    )
                    .await
                }
            })
            .await;
        result.map_err(|error| map_helix_error(error, ModerationAction::Ban))
    }

    /// ユーザーの BAN を解除する。
    pub async fn unban_user(
        &self,
        client_id: &str,
        client_secret: &str,
        channel: &str,
        author_id: &str,
    ) -> Result<(), ModerationError> {
        let channel = normalize_login(channel);
        let author_id = author_id.trim().to_string();
        if channel.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN解除対象のTwitchチャンネルが空です",
            ));
        }
        if author_id.is_empty() {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "BAN解除対象のユーザーIDが空です",
            ));
        }

        let generation = self.generation();
        let this = self;
        let channel_ref = channel.as_str();
        let author_ref = author_id.as_str();
        let result = self
            .with_token_retry(client_id, client_secret, move |access| {
                let this = this;
                async move {
                    this.unban_user_once(generation, channel_ref, author_ref, access)
                        .await
                }
            })
            .await;
        result.map_err(|error| map_helix_error(error, ModerationAction::Unban))
    }

    async fn with_token_retry<F, Fut, T>(
        &self,
        client_id: &str,
        client_secret: &str,
        mut action: F,
    ) -> Result<T, HelixError>
    where
        F: FnMut(TwitchAccess) -> Fut,
        Fut: std::future::Future<Output = Result<T, HelixError>>,
    {
        for attempt in 0..2 {
            let mut access = match self.access(client_id, client_secret).await {
                Ok(access) => access,
                Err(error) => return Err(HelixError::Moderation(error)),
            };
            self.maybe_validate(client_id.trim(), &mut access).await;
            if !scopes_satisfied(&access.scopes) {
                return Err(HelixError::Moderation(ModerationError::new(
                    ModerationErrorCode::ScopeMissing,
                    SCOPE_MISSING_MESSAGE,
                )));
            }

            match action(access).await {
                Ok(value) => return Ok(value),
                Err(HelixError::Api { status, .. })
                    if attempt == 0 && status == StatusCode::UNAUTHORIZED =>
                {
                    self.invalidate_access_token().await;
                }
                Err(error) => return Err(error),
            }
        }
        Err(HelixError::Moderation(ModerationError::new(
            ModerationErrorCode::NotLoggedIn,
            "Twitchの認証が切れています。設定からTwitchへ再接続してください",
        )))
    }

    async fn delete_message_once(
        &self,
        generation: u64,
        channel: &str,
        message_id: &str,
        access: TwitchAccess,
    ) -> Result<(), HelixError> {
        let broadcaster_id = self.resolve_user_id(generation, channel, &access).await?;
        let response = self
            .http
            .delete(CHAT_URL)
            .query(&[
                ("broadcaster_id", broadcaster_id.as_str()),
                ("moderator_id", access.user_id.as_str()),
                ("message_id", message_id),
            ])
            .header("Client-Id", access.client_id.as_str())
            .bearer_auth(&access.token)
            .send()
            .await
            .map_err(|e| HelixError::Network(format!("Twitchのコメント削除に失敗しました: {e}")))?;
        finish_empty_response(response).await
    }

    async fn ban_user_once(
        &self,
        generation: u64,
        channel: &str,
        author_id: &str,
        duration_secs: Option<u32>,
        reason: Option<&str>,
        access: TwitchAccess,
    ) -> Result<(), HelixError> {
        let broadcaster_id = self.resolve_user_id(generation, channel, &access).await?;
        let target_id = self
            .resolve_target_id(generation, author_id, &access)
            .await?;
        let body = helix_ban_body(&target_id, duration_secs, reason)
            .map_err(HelixError::Moderation)?;

        let response = self
            .http
            .post(BANS_URL)
            .query(&[
                ("broadcaster_id", broadcaster_id.as_str()),
                ("moderator_id", access.user_id.as_str()),
            ])
            .header("Client-Id", access.client_id.as_str())
            .bearer_auth(&access.token)
            .json(&body)
            .send()
            .await
            .map_err(|e| HelixError::Network(format!("TwitchのBANに失敗しました: {e}")))?;
        finish_empty_response(response).await
    }

    async fn unban_user_once(
        &self,
        generation: u64,
        channel: &str,
        author_id: &str,
        access: TwitchAccess,
    ) -> Result<(), HelixError> {
        let broadcaster_id = self.resolve_user_id(generation, channel, &access).await?;
        let target_id = self
            .resolve_target_id(generation, author_id, &access)
            .await?;

        let response = self
            .http
            .delete(BANS_URL)
            .query(&[
                ("broadcaster_id", broadcaster_id.as_str()),
                ("moderator_id", access.user_id.as_str()),
                ("user_id", target_id.as_str()),
            ])
            .header("Client-Id", access.client_id.as_str())
            .bearer_auth(&access.token)
            .send()
            .await
            .map_err(|e| HelixError::Network(format!("TwitchのBAN解除に失敗しました: {e}")))?;
        finish_empty_response(response).await
    }

    /// login を Helix の user id へ解決する。世代が変わっていたらキャッシュを使わない。
    async fn resolve_user_id(
        &self,
        generation: u64,
        login: &str,
        access: &TwitchAccess,
    ) -> Result<String, HelixError> {
        let login = normalize_login(login);
        if login.is_empty() {
            return Err(HelixError::Moderation(ModerationError::new(
                ModerationErrorCode::Other,
                "Twitchのチャンネルまたはユーザー名が空です",
            )));
        }
        if is_current_generation(generation, self.generation()) {
            if let Some(id) = cached_user_id(generation, &login).await {
                return Ok(id);
            }
        }

        let response = self
            .http
            .get(USERS_URL)
            .query(&[("login", login.as_str())])
            .header("Client-Id", access.client_id.as_str())
            .bearer_auth(&access.token)
            .send()
            .await
            .map_err(|e| HelixError::Network(format!("Twitchユーザー情報の取得に失敗しました: {e}")))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(HelixError::Api { status, body });
        }
        let value = serde_json::from_str::<Value>(&body).map_err(|e| {
            HelixError::Network(format!("Twitchユーザー情報の解析に失敗しました: {e}"))
        })?;
        let id = value
            .pointer("/data/0/id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|id| !id.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                HelixError::Moderation(ModerationError::new(
                    ModerationErrorCode::NotFound,
                    "Twitchのチャンネルまたはユーザーが見つかりません",
                ))
            })?;
        if is_current_generation(generation, self.generation()) {
            cache_user_id(generation, &login, &id).await;
        }
        Ok(id)
    }

    async fn resolve_target_id(
        &self,
        generation: u64,
        author_id: &str,
        access: &TwitchAccess,
    ) -> Result<String, HelixError> {
        if let Some(id) = select_user_id(author_id) {
            return Ok(id);
        }
        if author_id.trim().is_empty() {
            return Err(HelixError::Moderation(ModerationError::new(
                ModerationErrorCode::Other,
                "対象ユーザーIDが空です",
            )));
        }
        // user-id タグが欠けて login にフォールバックしたメッセージは Helix で解決する。
        self.resolve_user_id(generation, author_id, access).await
    }
}

async fn finish_empty_response(response: reqwest::Response) -> Result<(), HelixError> {
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    let body = response.text().await.unwrap_or_default();
    Err(HelixError::Api { status, body })
}

fn map_helix_error(error: HelixError, action: ModerationAction) -> ModerationError {
    match error {
        HelixError::Api { status, body } => map_helix_moderation_error(status, &body, action),
        HelixError::Network(message) => {
            ModerationError::new(ModerationErrorCode::Network, message)
        }
        HelixError::Moderation(error) => error,
    }
}

pub(crate) fn normalize_login(value: &str) -> String {
    value.trim().trim_start_matches('#').to_lowercase()
}

/// 数値の user id だけをそのまま使う。login は Helix で解決する。
pub(crate) fn select_user_id(author_id: &str) -> Option<String> {
    let id = author_id.trim();
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())).then(|| id.to_string())
}

/// Helix の BAN リクエスト本文。duration は 1〜1,209,600 秒へクランプし、
/// 0 は不正として拒否する。reason は 500 文字(Unicode の char 単位)で切る。
pub(crate) fn helix_ban_body(
    user_id: &str,
    duration_secs: Option<u32>,
    reason: Option<&str>,
) -> Result<Value, ModerationError> {
    let mut data = json!({ "user_id": user_id });
    if let Some(seconds) = duration_secs {
        if seconds == 0 {
            return Err(ModerationError::new(
                ModerationErrorCode::Other,
                "タイムアウト時間が不正です",
            ));
        }
        data["duration"] = json!(seconds.clamp(MIN_TIMEOUT_SECS, MAX_TIMEOUT_SECS));
    }
    if let Some(reason) = reason {
        let reason = reason.trim();
        if !reason.is_empty() {
            let truncated: String = reason.chars().take(MAX_REASON_CHARS).collect();
            data["reason"] = json!(truncated);
        }
    }
    Ok(json!({ "data": data }))
}

pub(crate) fn map_helix_moderation_error(
    status: StatusCode,
    body: &str,
    action: ModerationAction,
) -> ModerationError {
    let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
    let lower = body.to_ascii_lowercase();
    let detail = value
        .get("message")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|message| !message.is_empty());

    if status == StatusCode::UNAUTHORIZED {
        if lower.contains("scope") {
            return ModerationError::new(
                ModerationErrorCode::ScopeMissing,
                "Twitchの認証にモデレーション用スコープがありません。設定からTwitchへ再接続してください",
            );
        }
        return ModerationError::new(
            ModerationErrorCode::NotLoggedIn,
            "Twitchの認証が切れています。設定からTwitchへ再接続してください",
        );
    }

    if status == StatusCode::FORBIDDEN {
        if lower.contains("scope") {
            return ModerationError::new(
                ModerationErrorCode::ScopeMissing,
                "Twitchの認証にモデレーション用スコープがありません。設定からTwitchへ再接続してください",
            );
        }
        return ModerationError::new(
            ModerationErrorCode::NotModerator,
            "このTwitchアカウントにはモデレーションする権限がありません",
        );
    }

    if status == StatusCode::BAD_REQUEST {
        if lower.contains("already banned") {
            return ModerationError::new(
                ModerationErrorCode::Other,
                "このユーザーは既にBANされています",
            );
        }
        if lower.contains("not banned") || lower.contains("not currently banned") {
            return ModerationError::new(
                ModerationErrorCode::Other,
                "このユーザーはBANされていません",
            );
        }
        // broadcaster_id / moderator_id の引数エラーを拾わないよう、"broadcaster" や
        // "moderator" 単独では判定しない。
        if lower.contains("may not be banned")
            || lower.contains("may not be timed out")
            || lower.contains("may not be put in a timeout")
            || lower.contains("may not delete")
            || lower.contains("cannot ban")
            || lower.contains("can't ban")
            || lower.contains("not allowed")
            || lower.contains("protected")
        {
            return ModerationError::new(
                ModerationErrorCode::ProtectedTarget,
                "配信者・モデレーターなど、この操作を行えないユーザーです",
            );
        }
        let message = detail.unwrap_or("Twitchが操作を拒否しました");
        return ModerationError::new(
            ModerationErrorCode::Other,
            format!("Twitch API エラー (400): {message}"),
        );
    }

    if status == StatusCode::NOT_FOUND {
        return ModerationError::new(ModerationErrorCode::NotFound, not_found_message(action));
    }
    if status == StatusCode::CONFLICT {
        // Twitch は状態更新の競合で 409 を返し、再試行を案内している。
        return ModerationError::new(
            ModerationErrorCode::Conflict,
            "他の操作と競合しました。少し待ってから再試行してください",
        );
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        return ModerationError::new(
            ModerationErrorCode::RateLimited,
            "Twitchの操作制限に達しました。少し間隔を空けてから再試行してください",
        );
    }

    let message = detail.unwrap_or("詳細不明");
    ModerationError::new(
        ModerationErrorCode::Other,
        format!("Twitch API エラー ({status}): {message}"),
    )
}

fn not_found_message(action: ModerationAction) -> &'static str {
    match action {
        ModerationAction::DeleteMessage => {
            "削除対象のコメントが見つかりません(6時間を超えた可能性があります)"
        }
        ModerationAction::Ban | ModerationAction::Unban => "対象のユーザーが見つかりません",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn api_error(status: StatusCode, body: &str, action: ModerationAction) -> ModerationError {
        map_helix_moderation_error(status, body, action)
    }

    #[test]
    fn ban_body_handles_duration_boundaries() {
        let permanent = helix_ban_body("42", None, None).unwrap();
        assert_eq!(
            permanent.pointer("/data/user_id").and_then(Value::as_str),
            Some("42")
        );
        assert!(permanent.pointer("/data/duration").is_none());
        assert!(permanent.pointer("/data/reason").is_none());

        for (input, expected) in [
            (1_u32, 1_u64),
            (MAX_TIMEOUT_SECS, MAX_TIMEOUT_SECS as u64),
            (MAX_TIMEOUT_SECS + 1, MAX_TIMEOUT_SECS as u64),
        ] {
            let body = helix_ban_body("42", Some(input), None).unwrap();
            assert_eq!(
                body.pointer("/data/duration").and_then(Value::as_u64),
                Some(expected),
                "duration {input} should clamp to {expected}"
            );
        }

        let zero = helix_ban_body("42", Some(0), None).unwrap_err();
        assert_eq!(zero.code, ModerationErrorCode::Other);
    }

    #[test]
    fn ban_body_truncates_reason_by_chars_not_bytes() {
        let reason = "あ".repeat(501);
        let body = helix_ban_body("42", Some(60), Some(&reason)).unwrap();
        let stored = body
            .pointer("/data/reason")
            .and_then(Value::as_str)
            .unwrap();
        assert_eq!(stored.chars().count(), MAX_REASON_CHARS);
        assert_eq!(stored.chars().next(), Some('あ'));

        let blank = helix_ban_body("42", None, Some("   ")).unwrap();
        assert!(blank.pointer("/data/reason").is_none());
    }

    #[test]
    fn maps_helix_statuses_and_bad_request_variants() {
        assert_eq!(
            api_error(StatusCode::UNAUTHORIZED, "{}", ModerationAction::Ban).code,
            ModerationErrorCode::NotLoggedIn
        );
        assert_eq!(
            api_error(
                StatusCode::UNAUTHORIZED,
                r#"{"message":"missing scope: moderator:manage:banned_users"}"#,
                ModerationAction::Ban
            )
            .code,
            ModerationErrorCode::ScopeMissing
        );
        assert_eq!(
            api_error(StatusCode::FORBIDDEN, "{}", ModerationAction::Ban).code,
            ModerationErrorCode::NotModerator
        );
        // 以下の本文は Twitch 公式ドキュメント(Ban User / Unban User / Delete Chat Messages)の文言。
        let already = api_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":"Bad Request","status":400,"message":"The user specified in the user_id field is already banned."}"#,
            ModerationAction::Ban,
        );
        assert_eq!(already.code, ModerationErrorCode::Other);
        assert!(already.message.contains("既にBAN"));

        let not_banned = api_error(
            StatusCode::BAD_REQUEST,
            r#"{"error":"Bad Request","status":400,"message":"The user specified in the user_id field is not banned."}"#,
            ModerationAction::Unban,
        );
        assert_eq!(not_banned.code, ModerationErrorCode::Other);
        assert!(not_banned.message.contains("BANされていません"));

        for (body, action) in [
            (
                r#"{"error":"Bad Request","status":400,"message":"The user specified in the user_id field may not be banned."}"#,
                ModerationAction::Ban,
            ),
            (
                r#"{"error":"Bad Request","status":400,"message":"The user specified in the user_id field may not be put in a timeout."}"#,
                ModerationAction::Ban,
            ),
            (
                r#"{"error":"Bad Request","status":400,"message":"The user specified in the user_id field may not be timed out."}"#,
                ModerationAction::Ban,
            ),
            (
                r#"{"error":"Bad Request","status":400,"message":"You may not delete another moderator's messages."}"#,
                ModerationAction::DeleteMessage,
            ),
            (
                r#"{"error":"Bad Request","status":400,"message":"You may not delete the broadcaster's messages."}"#,
                ModerationAction::DeleteMessage,
            ),
        ] {
            assert_eq!(
                api_error(StatusCode::BAD_REQUEST, body, action).code,
                ModerationErrorCode::ProtectedTarget,
                "{body}"
            );
        }

        // 引数エラーは broadcaster / moderator を含んでも保護対象と誤判定しない。
        for body in [
            r#"{"error":"Bad Request","status":400,"message":"The broadcaster_id query parameter is required."}"#,
            r#"{"error":"Bad Request","status":400,"message":"The moderator_id query parameter is required."}"#,
        ] {
            let error = api_error(StatusCode::BAD_REQUEST, body, ModerationAction::Ban);
            assert_eq!(error.code, ModerationErrorCode::Other, "{body}");
            assert!(error.message.contains("query parameter is required"));
        }
        assert_eq!(
            api_error(StatusCode::NOT_FOUND, "{}", ModerationAction::DeleteMessage).code,
            ModerationErrorCode::NotFound
        );
        let conflict = api_error(StatusCode::CONFLICT, "{}", ModerationAction::Ban);
        assert_eq!(conflict.code, ModerationErrorCode::Conflict);
        assert!(conflict.message.contains("再試行"));
        assert_eq!(
            api_error(StatusCode::TOO_MANY_REQUESTS, "{}", ModerationAction::Ban).code,
            ModerationErrorCode::RateLimited
        );
        assert_eq!(
            api_error(StatusCode::INTERNAL_SERVER_ERROR, "", ModerationAction::Ban).code,
            ModerationErrorCode::Other
        );
    }

    #[test]
    fn selects_numeric_user_id_only() {
        assert_eq!(select_user_id("12345"), Some("12345".to_string()));
        assert_eq!(select_user_id(" 67890 "), Some("67890".to_string()));
        assert_eq!(select_user_id("alice"), None);
        assert_eq!(select_user_id("alice123"), None);
        assert_eq!(select_user_id(""), None);
        assert_eq!(select_user_id("   "), None);
    }

    #[test]
    fn normalizes_twitch_channel_logins() {
        assert_eq!(normalize_login(" #FastComment "), "fastcomment");
        assert_eq!(normalize_login("Alice"), "alice");
        assert_eq!(normalize_login("   "), "");
    }

    #[test]
    fn cached_user_id_is_valid_only_for_same_generation() {
        // このテスト専用のキーでキャッシュの世代判定だけを確認する。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            cache_user_id(1, "generation-check", "100").await;
            assert_eq!(
                cached_user_id(1, "generation-check").await,
                Some("100".to_string())
            );
            assert_eq!(cached_user_id(2, "generation-check").await, None);
        });
    }

    #[tokio::test]
    async fn delete_message_rejects_empty_message_id_without_request() {
        let auth = TwitchAuth::new().unwrap();
        for message_id in ["", "   ", "\t\n"] {
            let error = auth
                .delete_message("client-id", "", "channel", message_id)
                .await
                .unwrap_err();
            assert_eq!(error.code, ModerationErrorCode::Other);
            assert!(error.message.contains("メッセージID"));
        }
    }

    #[tokio::test]
    async fn ban_user_rejects_zero_duration_without_request() {
        let auth = TwitchAuth::new().unwrap();
        let error = auth
            .ban_user("client-id", "", "channel", "42", Some(0), None)
            .await
            .unwrap_err();
        assert_eq!(error.code, ModerationErrorCode::Other);
        assert!(error.message.contains("タイムアウト"));
    }
}
