//! Twitch Helix から Goals 用 viewer_count を取得する poller。
//!
//! UserToken では validate API から Client-Id を導出し、AppCredentials では
//! client_credentials で App Access Token を取得して、Helix streams API の
//! `data[0].viewer_count` を寛容に抽出する。

use std::time::{Duration, Instant};

use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::Platform;
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
}

pub fn spawn_twitch_viewer_poller(
    login: String,
    auth: TwitchViewerAuth,
    tx: mpsc::Sender<YoutubeMetadataUpdate>,
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
    };

    tauri::async_runtime::spawn(async move {
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
            };

            if let Some(result) = metadata_result {
                match result {
                    Ok(values) => {
                        metadata_failures = 0;
                        let update = YoutubeMetadataUpdate {
                            platform: Platform::Twitch,
                            channel: login.clone(),
                            concurrent_viewers: values.viewer_count,
                            viewers_kind: ViewerCountKind::Concurrent,
                            likes: None,
                            title: None,
                            live: values.live,
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

pub fn viewer_auth(
    oauth: &str,
    client_id: &str,
    client_secret: &str,
) -> Option<TwitchViewerAuth> {
    let client_id = client_id.trim();
    let client_secret = client_secret.trim();
    if !client_id.is_empty() && !client_secret.is_empty() {
        return Some(TwitchViewerAuth::AppCredentials {
            client_id: client_id.to_string(),
            client_secret: client_secret.to_string(),
        });
    }

    oauth_token_without_prefix(oauth).map(TwitchViewerAuth::UserToken)
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
                let ttl = Duration::from_secs(fetched.expires_in)
                    .saturating_sub(TOKEN_REFRESH_MARGIN);
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
        .or_else(|| {
            count
                .as_str()
                .and_then(|s| s.trim().parse::<u32>().ok())
        })
}

fn extract_stream_metadata(value: &Value) -> TwitchStreamMetadata {
    let Some(streams) = value.get("data").and_then(Value::as_array) else {
        return TwitchStreamMetadata::default();
    };
    let Some(stream) = streams.first() else {
        return TwitchStreamMetadata {
            viewer_count: None,
            live: Some(false),
        };
    };
    TwitchStreamMetadata {
        viewer_count: extract_viewer_count(stream),
        live: Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
            }
        );
    }
}
