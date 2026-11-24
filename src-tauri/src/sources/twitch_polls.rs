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
    #[error("Twitchがアンケートの操作を受け付けませんでした。{0}")]
    BadRequest(String),
    #[error("Twitchのアンケート操作に失敗しました (HTTP {0})")]
    Api(u16),
    #[error("Twitchのアンケートに接続できませんでした")]
    Network,
    #[error("Twitchの認証情報を確認できませんでした")]
    Authentication,
    #[error("通信が途切れ、Twitchでアンケートの作成を確認できませんでした。Twitch側に投票が作られた可能性があるので、Twitchで確認・終了してください")]
    CreateUnconfirmed,
}

impl PollsError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::NotEligible => "NotEligible",
            Self::ScopeMissing => "ScopeMissing",
            Self::Unauthorized => "Unauthorized",
            Self::RateLimited { .. } => "RateLimited",
            Self::BadRequest(_) => "BadRequest",
            Self::Api(_) => "Api",
            Self::Network => "Network",
            Self::Authentication => "Authentication",
            Self::CreateUnconfirmed => "CreateUnconfirmed",
        }
    }
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
    /// Twitch 上の開始時刻と終了予定時刻(UNIX ミリ秒)。欠けたら None。
    pub timing: Option<RemoteTiming>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RemoteTiming {
    pub started_at_ms: u64,
    pub ends_at_ms: u64,
}

/// 作成応答を失った後の照合で採用した投票。
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MatchedPoll {
    pub id: String,
    pub timing: RemoteTiming,
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
        let response = self
            .send(request, token)
            .await
            .map_err(|error| match error {
                PollsError::Api(403) => PollsError::NotEligible,
                other => other,
            })?;
        first_poll(response)
            .await?
            .as_ref()
            .and_then(poll_id)
            .map(str::to_owned)
            .ok_or(PollsError::Api(200))
    }

    /// `sent_at_ms` は POST を送る直前の壁時計(UTC、UNIX ミリ秒)。
    pub(crate) async fn find_matching_latest(
        &self,
        token: &str,
        broadcaster_id: &str,
        title: &str,
        choices: &[String],
        duration_secs: u32,
        sent_at_ms: u64,
    ) -> Result<Option<MatchedPoll>, PollsError> {
        let request = self
            .request(Method::GET, token)
            .query(&[("broadcaster_id", broadcaster_id)]);
        let response = self.send(request, token).await?;
        let Some(latest) = first_poll(response).await? else {
            return Ok(None);
        };
        // 配信者が Twitch 側で先に始めた同じ内容の投票を拾わないよう、時間と開始時刻も照合する。
        let matching = latest["status"].as_str() == Some("ACTIVE")
            && latest["title"].as_str() == Some(title)
            && latest["choices"].as_array().is_some_and(|remote_choices| {
                remote_choices
                    .iter()
                    .map(|choice| choice["title"].as_str())
                    .eq(choices.iter().map(|choice| Some(choice.as_str())))
            })
            && latest["duration"].as_u64() == Some(u64::from(duration_secs));
        let timing = remote_timing(&latest)
            .filter(|timing| timing.started_at_ms > sent_at_ms.saturating_sub(10_000));
        Ok(match (matching, poll_id(&latest), timing) {
            (true, Some(id), Some(timing)) => Some(MatchedPoll {
                id: id.to_owned(),
                timing,
            }),
            _ => None,
        })
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
        let poll = first_poll(response).await?.ok_or(PollsError::Api(200))?;
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
        Ok(RemotePoll {
            status,
            votes,
            timing: remote_timing(&poll),
        })
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
                        PollsError::Api(status.as_u16())
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

fn poll_id(poll: &Value) -> Option<&str> {
    poll.get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
}

fn remote_timing(poll: &Value) -> Option<RemoteTiming> {
    let started_at_ms = chrono::DateTime::parse_from_rfc3339(poll.get("started_at")?.as_str()?)
        .ok()?
        .timestamp_millis();
    let started_at_ms = u64::try_from(started_at_ms).ok()?;
    let duration_ms = poll.get("duration")?.as_u64()?.checked_mul(1000)?;
    Some(RemoteTiming {
        started_at_ms,
        ends_at_ms: started_at_ms.checked_add(duration_ms)?,
    })
}

async fn first_poll(response: reqwest::Response) -> Result<Option<Value>, PollsError> {
    let status = response.status().as_u16();
    // JSON の構文エラーと、応答本文の途中で起きた通信エラーを区別する。
    let bytes = response.bytes().await.map_err(|_| PollsError::Network)?;
    let mut body: Value = serde_json::from_slice(&bytes).map_err(|_| PollsError::Api(status))?;
    let data = body["data"].as_array_mut().ok_or(PollsError::Api(status))?;
    Ok(data.first_mut().map(Value::take))
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
    crate::poll::unique_option_labels(options).map(|_| ())
}
