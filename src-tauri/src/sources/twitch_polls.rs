//! Twitch Helix Polls の HTTP 境界。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::{Method, RequestBuilder, StatusCode};
use serde_json::{json, Value};

pub(crate) const HELIX_BASE: &str = "https://api.twitch.tv/helix";

pub struct HelixPolls {
    base_url: String,
    client_id: String,
    http: reqwest::Client,
}

#[derive(Debug, thiserror::Error)]
pub enum PollsError {
    #[error("Twitchのアンケートはアフィリエイトまたはパートナーが利用できます")]
    NotEligible,
    #[error("Twitch で再ログインしてください")]
    ScopeMissing,
    #[error("Twitchの認証が切れています。再ログインしてください")]
    Unauthorized,
    #[error("Twitchの操作回数制限に達しました。{retry_after_secs}秒後に再試行してください")]
    RateLimited { retry_after_secs: u64 },
    #[error("Twitchがアンケートの操作を受け付けませんでした")]
    BadRequest(String),
    #[error("Twitchのアンケート操作に失敗しました (HTTP {0})")]
    Api(u16),
    #[error("Twitchのアンケートに接続できませんでした")]
    Network,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RemoteStatus {
    Active,
    Completed,
    Terminated,
    Archived,
    Moderated,
    Invalid,
}

#[derive(Debug, PartialEq, Eq)]
pub struct RemotePoll {
    pub status: RemoteStatus,
    pub votes: Vec<u32>,
}

impl HelixPolls {
    pub fn new(base_url: &str, client_id: &str) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .expect("Twitchアンケート用HTTPクライアントの作成に失敗しました");
        Self::with_client(base_url, client_id, http)
    }

    pub(crate) fn with_client(base_url: &str, client_id: &str, http: reqwest::Client) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            client_id: client_id.to_owned(),
            http,
        }
    }

    pub async fn create(
        &self,
        token: &str,
        broadcaster_id: &str,
        title: &str,
        choices: &[String],
        duration_secs: u32,
    ) -> Result<String, PollsError> {
        let choices: Vec<_> = choices
            .iter()
            .map(|title| json!({ "title": title }))
            .collect();
        let request = self.request(Method::POST, token).json(&json!({
            "broadcaster_id": broadcaster_id,
            "title": title,
            "choices": choices,
            "duration": duration_secs,
        }));
        let response = self.send(request, token).await?;
        let poll = first_poll(response).await?;
        poll.get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or(PollsError::Api(200))
    }

    pub async fn end(
        &self,
        token: &str,
        broadcaster_id: &str,
        poll_id: &str,
        archive: bool,
    ) -> Result<(), PollsError> {
        let status = if archive { "ARCHIVED" } else { "TERMINATED" };
        let request = self.request(Method::PATCH, token).json(&json!({
            "broadcaster_id": broadcaster_id,
            "id": poll_id,
            "status": status,
        }));
        self.send(request, token).await?;
        Ok(())
    }

    pub async fn get(
        &self,
        token: &str,
        broadcaster_id: &str,
        poll_id: &str,
    ) -> Result<RemotePoll, PollsError> {
        let request = self
            .request(Method::GET, token)
            .query(&[("broadcaster_id", broadcaster_id), ("id", poll_id)]);
        let response = self.send(request, token).await?;
        let poll = first_poll(response).await?;
        if poll.get("id").and_then(Value::as_str) != Some(poll_id) {
            return Err(PollsError::Api(200));
        }
        let status = match poll.get("status").and_then(Value::as_str) {
            Some("ACTIVE") => RemoteStatus::Active,
            Some("COMPLETED") => RemoteStatus::Completed,
            Some("TERMINATED") => RemoteStatus::Terminated,
            Some("ARCHIVED") => RemoteStatus::Archived,
            Some("MODERATED") => RemoteStatus::Moderated,
            _ => RemoteStatus::Invalid,
        };
        let votes = poll
            .get("choices")
            .and_then(Value::as_array)
            .ok_or(PollsError::Api(200))?
            .iter()
            .map(|choice| {
                choice
                    .get("votes")
                    .and_then(Value::as_u64)
                    .and_then(|votes| u32::try_from(votes).ok())
                    .ok_or(PollsError::Api(200))
            })
            .collect::<Result<_, _>>()?;
        Ok(RemotePoll { status, votes })
    }

    fn request(&self, method: Method, token: &str) -> RequestBuilder {
        self.http
            .request(method, format!("{}/polls", self.base_url))
            .header("Client-Id", &self.client_id)
            .bearer_auth(token)
    }

    async fn send(
        &self,
        request: RequestBuilder,
        token: &str,
    ) -> Result<reqwest::Response, PollsError> {
        let response = request.send().await.map_err(|_| PollsError::Network)?;
        let status = response.status();
        match status {
            _ if status.is_success() => Ok(response),
            StatusCode::UNAUTHORIZED => Err(PollsError::Unauthorized),
            StatusCode::TOO_MANY_REQUESTS => {
                let now = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default();
                let retry_after_secs = response
                    .headers()
                    .get("Ratelimit-Reset")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map_or(1, |reset| reset.saturating_sub(now.as_secs()).max(1));
                Err(PollsError::RateLimited { retry_after_secs })
            }
            StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST => {
                let body = response.json::<Value>().await.unwrap_or_default();
                let message = body
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if status == StatusCode::FORBIDDEN {
                    return Err(if message.to_ascii_lowercase().contains("scope") {
                        PollsError::ScopeMissing
                    } else {
                        PollsError::NotEligible
                    });
                }
                // API の診断用にメッセージを保持するが、資格情報や URL を含む本文は捨てる。
                let sensitive = [
                    token,
                    self.client_id.as_str(),
                    self.base_url.as_str(),
                    "://",
                ]
                .into_iter()
                .any(|part| !part.is_empty() && message.contains(part))
                    || message
                        .split_whitespace()
                        .any(|word| word.contains('?') && word.contains('='));
                let safe = !sensitive;
                Err(PollsError::BadRequest(if safe {
                    message.to_owned()
                } else {
                    String::new()
                }))
            }
            _ => Err(PollsError::Api(status.as_u16())),
        }
    }
}

async fn first_poll(response: reqwest::Response) -> Result<Value, PollsError> {
    let status = response.status().as_u16();
    let mut body = response.json::<Value>().await.map_err(|error| {
        if error.is_decode() {
            PollsError::Api(status)
        } else {
            PollsError::Network
        }
    })?;
    body.pointer_mut("/data/0")
        .map(Value::take)
        .ok_or(PollsError::Api(status))
}

pub fn validate_twitch(
    question: &str,
    options: &[String],
    duration_secs: Option<u32>,
) -> Result<(), &'static str> {
    validate_twitch_text(question, options)?;
    if !duration_secs.is_some_and(|secs| (15..=1800).contains(&secs)) {
        return Err("Twitchの投票時間は15〜1800秒で指定してください");
    }
    Ok(())
}

pub(crate) fn validate_twitch_text(question: &str, options: &[String]) -> Result<(), &'static str> {
    if !(1..=60).contains(&question.trim().chars().count()) {
        return Err("Twitchの質問は1〜60文字で入力してください");
    }
    if !(2..=5).contains(&options.len()) {
        return Err("Twitchの選択肢は2〜5件で入力してください");
    }
    if options
        .iter()
        .any(|option| !(1..=25).contains(&option.trim().chars().count()))
    {
        return Err("Twitchの各選択肢は1〜25文字で入力してください");
    }
    Ok(())
}
