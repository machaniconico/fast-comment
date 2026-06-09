//! Twitch Helix から Goals 用 viewer_count を取得する poller。
//!
//! OAuth token から validate API で Client-Id を導出し、Helix streams API の
//! `data[0].viewer_count` を寛容に抽出する。

use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::model::Platform;
use crate::stats::YoutubeMetadataUpdate;

const POLL_INTERVAL: Duration = Duration::from_secs(20);
const VALIDATE_URL: &str = "https://id.twitch.tv/oauth2/validate";
const STREAMS_URL: &str = "https://api.twitch.tv/helix/streams";

pub fn spawn_twitch_viewer_poller(
    login: String,
    oauth: String,
    tx: mpsc::Sender<YoutubeMetadataUpdate>,
    cancel: CancellationToken,
) {
    let token = match oauth_token_without_prefix(&oauth) {
        Some(token) => token,
        None => return,
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
        let mut client_id: Option<String> = None;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            if client_id.is_none() {
                match fetch_client_id(&http, &token).await {
                    Ok(id) => client_id = Some(id),
                    Err(e) => {
                        tracing::debug!("twitch:{login} client_id 取得失敗: {e:#}");
                    }
                }
            }

            if let Some(id) = client_id.as_deref() {
                match fetch_viewer_count(&http, &login, &token, id).await {
                    Ok(viewers) => {
                        let update = YoutubeMetadataUpdate {
                            platform: Platform::Twitch,
                            channel: login.clone(),
                            concurrent_viewers: viewers,
                            likes: None,
                            title: None,
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
                        tracing::debug!("twitch:{login} viewer_count 取得失敗: {e:#}");
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

async fn fetch_viewer_count(
    http: &Client,
    login: &str,
    token: &str,
    client_id: &str,
) -> anyhow::Result<Option<u32>> {
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

    Ok(extract_viewer_count(&value))
}

fn oauth_token_without_prefix(oauth: &str) -> Option<String> {
    let oauth = oauth.trim();
    let token = oauth.strip_prefix("oauth:").unwrap_or(oauth);
    if token.is_empty() || token.chars().any(char::is_whitespace) {
        return None;
    }
    Some(token.to_string())
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
    let count = value
        .get("data")
        .and_then(Value::as_array)?
        .first()?
        .get("viewer_count")?;

    count
        .as_u64()
        .and_then(|v| u32::try_from(v).ok())
        .or_else(|| {
            count
                .as_str()
                .and_then(|s| s.trim().parse::<u32>().ok())
        })
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
    fn extracts_viewer_count_from_first_stream() {
        let value = json!({ "data": [{ "viewer_count": 1234 }] });
        assert_eq!(extract_viewer_count(&value), Some(1234));
    }

    #[test]
    fn missing_stream_data_means_not_live() {
        let value = json!({ "data": [] });
        assert_eq!(extract_viewer_count(&value), None);
    }
}
