//! YouTube Data API の公式投票。

use std::{sync::LazyLock, time::Duration};

use reqwest::{header::CONTENT_LENGTH, RequestBuilder, StatusCode, Url};
use serde_json::{json, Value};

use super::youtube_send::google_api_reason;

pub(crate) const YOUTUBE_BASE: &str = "https://www.googleapis.com/youtube/v3";

pub struct YoutubePolls {
    messages_url: String,
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug)]
pub struct YtPollCreated {
    pub id: String,
    pub published_at_ms: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct YtPollState {
    pub id: String,
    pub status: YtPollStatus,
    pub question: String,
    pub options: Vec<String>,
    pub tallies: Option<Vec<u64>>,
    pub published_at_ms: Option<i64>,
    /// list 応答の `pollingIntervalMillis`。list 以外の応答では None。
    pub polling_interval_ms: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum YtPollStatus {
    Active,
    Closed,
    Unknown,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum YtPollsError {
    #[error("YouTube側ですでに投票が受付中です")]
    AlreadyActive,
    #[error("{}", forbidden_message(.0))]
    Forbidden(String),
    #[error("YouTube APIの利用上限に達しました。日次上限は翌日の割り当てまで回復しません")]
    QuotaExceeded,
    #[error("YouTube認証が切れています。設定からGoogleに再接続してください")]
    Unauthorized,
    #[error("{0}")]
    BadRequest(String),
    #[error("YouTubeのチャットまたは投票が見つかりません")]
    NotFound,
    #[error("YouTubeのアンケート操作に失敗しました (HTTP {0})")]
    Api(u16),
    #[error("YouTubeのアンケートとの通信に失敗しました")]
    Network,
}

fn forbidden_message(reason: &str) -> &'static str {
    match reason {
        "liveChatEnded" => "YouTube配信のチャットが終了しています",
        "liveChatDisabled" => "YouTube配信のチャットが無効になっています",
        _ => "YouTubeのアンケート操作に必要な権限がありません",
    }
}

impl YtPollsError {
    pub(crate) fn class(&self) -> &'static str {
        match self {
            Self::AlreadyActive => "AlreadyActive",
            Self::Forbidden(_) => "Forbidden",
            Self::QuotaExceeded => "QuotaExceeded",
            Self::Unauthorized => "Unauthorized",
            Self::BadRequest(_) => "BadRequest",
            Self::NotFound => "NotFound",
            Self::Api(_) => "Api",
            Self::Network => "Network",
        }
    }

    pub(crate) fn transient(&self) -> bool {
        matches!(self, Self::Network | Self::Api(500..=599))
    }
}

impl YoutubePolls {
    pub fn new(base_url: &str) -> Self {
        let base_url = base_url.trim_end_matches('/').to_owned();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()
            .expect("YouTubeアンケート用HTTPクライアントの作成に失敗しました");
        Self {
            messages_url: format!("{base_url}/liveChat/messages"),
            base_url,
            http,
        }
    }

    pub async fn create(
        &self,
        token: &str,
        live_chat_id: &str,
        question: &str,
        options: &[String],
    ) -> Result<YtPollCreated, YtPollsError> {
        let options: Vec<_> = options
            .iter()
            .map(|option| json!({"optionText": option}))
            .collect();
        let body = json!({"snippet": {
            "liveChatId": live_chat_id,
            "type": "pollEvent",
            "pollDetails": {"metadata": {"questionText": question, "options": options}}
        }});
        let value = self
            .send(
                self.http.post(&self.messages_url).json(&body),
                token,
                &[live_chat_id],
            )
            .await?;
        Ok(YtPollCreated {
            id: item_id(&value).ok_or(YtPollsError::Network)?.to_owned(),
            published_at_ms: published_at_ms(&value),
        })
    }

    pub async fn end(
        &self,
        token: &str,
        message_id: &str,
    ) -> Result<Option<YtPollState>, YtPollsError> {
        self.end_for_chat(token, message_id, "").await
    }

    pub(crate) async fn end_for_chat(
        &self,
        token: &str,
        message_id: &str,
        live_chat_id: &str,
    ) -> Result<Option<YtPollState>, YtPollsError> {
        let request = self
            .http
            .post(format!("{}/transition", self.messages_url))
            .header(CONTENT_LENGTH, "0")
            .query(&[("id", message_id), ("status", "closed")]);
        Ok(parse_state(
            &self
                .send(request, token, &[message_id, live_chat_id])
                .await?,
        ))
    }

    pub async fn active(
        &self,
        token: &str,
        live_chat_id: &str,
    ) -> Result<Option<YtPollState>, YtPollsError> {
        let request = self
            .http
            .get(&self.messages_url)
            .query(&[("liveChatId", live_chat_id), ("maxResults", "200")]);
        let value = self.send(request, token, &[live_chat_id]).await?;
        let interval = &value["pollingIntervalMillis"];
        let polling_interval_ms = interval
            .as_u64()
            .or_else(|| interval.as_str()?.parse().ok());
        Ok(value
            .get("activePollItem")
            .and_then(parse_state)
            .map(|state| YtPollState {
                polling_interval_ms,
                ..state
            }))
    }

    async fn send(
        &self,
        request: RequestBuilder,
        token: &str,
        identifiers: &[&str],
    ) -> Result<Value, YtPollsError> {
        let response = request
            .query(&[("part", "snippet")])
            .bearer_auth(token)
            .send()
            .await
            .map_err(|_| YtPollsError::Network)?;
        let status = response.status();
        if status.is_success() {
            return response.json().await.map_err(|_| YtPollsError::Network);
        }
        let body = response.text().await.unwrap_or_default();
        let host = Url::parse(&self.base_url)
            .ok()
            .and_then(|url| url.host_str().map(str::to_owned));
        let secrets: Vec<_> = [token, &self.base_url, host.as_deref().unwrap_or_default()]
            .into_iter()
            .chain(identifiers.iter().copied())
            .collect();
        Err(map_error(status, &body, &secrets))
    }
}

pub(crate) fn map_error(status: StatusCode, body: &str, secrets: &[&str]) -> YtPollsError {
    let reason = google_api_reason(body);
    match (status, reason.as_deref()) {
        (StatusCode::BAD_REQUEST, Some("preconditionCheckFailed")) => YtPollsError::AlreadyActive,
        (StatusCode::BAD_REQUEST, _) => {
            let value = serde_json::from_str::<Value>(body).unwrap_or(Value::Null);
            let message = value.pointer("/error/message").and_then(Value::as_str);
            YtPollsError::BadRequest(
                safe_detail(message, secrets)
                    .or_else(|| safe_detail(reason.as_deref(), secrets))
                    .unwrap_or("YouTubeがアンケートの操作を受け付けませんでした")
                    .to_owned(),
            )
        }
        (
            StatusCode::FORBIDDEN,
            Some("quotaExceeded" | "dailyLimitExceeded" | "rateLimitExceeded"),
        ) => YtPollsError::QuotaExceeded,
        (StatusCode::FORBIDDEN, _) => YtPollsError::Forbidden(
            safe_detail(reason.as_deref(), secrets)
                .unwrap_or("")
                .to_owned(),
        ),
        (StatusCode::UNAUTHORIZED, _) => YtPollsError::Unauthorized,
        (StatusCode::NOT_FOUND, _) => YtPollsError::NotFound,
        _ => YtPollsError::Api(status.as_u16()),
    }
}

fn safe_detail<'a>(detail: Option<&'a str>, secrets: &[&str]) -> Option<&'a str> {
    static URL_OR_QUERY: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(r"(?i)://|www\.|\b(bearer|authorization)\b|[?&][^\s=]+\s*=|\b(liveChatId|access_token)\s*[:=]").unwrap()
    });
    let detail = detail?.trim();
    (!detail.is_empty()
        && detail.chars().count() <= 240
        && !detail.chars().any(char::is_control)
        && !URL_OR_QUERY.is_match(detail)
        && !secrets.iter().any(|secret| {
            !secret.is_empty()
                && (detail.contains(secret)
                    || detail.contains(
                        &url::form_urlencoded::byte_serialize(secret.as_bytes())
                            .collect::<String>(),
                    ))
        }))
    .then_some(detail)
}

fn item_id(item: &Value) -> Option<&str> {
    item.get("id")?.as_str().filter(|id| !id.is_empty())
}

fn published_at_ms(item: &Value) -> Option<i64> {
    let raw = item.pointer("/snippet/publishedAt")?.as_str()?;
    chrono::DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|timestamp| timestamp.timestamp_millis())
}

fn parse_state(item: &Value) -> Option<YtPollState> {
    let id = item_id(item)?.to_owned();
    let snippet = item.get("snippet")?.as_object()?;
    let details = snippet.get("pollDetails").unwrap_or(&Value::Null);
    let metadata = &details["metadata"];
    let options = metadata["options"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let status = match metadata["status"]
        .as_str()
        .or_else(|| details["status"].as_str())
    {
        Some("active") => YtPollStatus::Active,
        Some("closed") => YtPollStatus::Closed,
        _ => YtPollStatus::Unknown,
    };
    Some(YtPollState {
        id,
        status,
        question: metadata["questionText"]
            .as_str()
            .unwrap_or_default()
            .to_owned(),
        options: options
            .iter()
            .map(|option| option["optionText"].as_str().unwrap_or_default().to_owned())
            .collect(),
        tallies: (!options.is_empty())
            .then(|| {
                options
                    .iter()
                    .map(|option| {
                        let tally = &option["tally"];
                        tally.as_u64().or_else(|| tally.as_str()?.parse().ok())
                    })
                    .collect()
            })
            .flatten(),
        published_at_ms: published_at_ms(item),
        polling_interval_ms: None,
    })
}

pub fn validate_youtube(question: &str, options: &[String]) -> Result<(), &'static str> {
    if !(1..=100).contains(&question.trim().chars().count()) {
        return Err("質問は1〜100文字で入力してください");
    }
    if !(2..=4).contains(&options.len()) {
        return Err("選択肢は2〜4件で入力してください");
    }
    if options
        .iter()
        .any(|option| !(1..=40).contains(&option.trim().chars().count()))
    {
        return Err("各選択肢は1〜40文字で入力してください");
    }
    crate::poll::unique_option_labels(options).map(|_| ())
}
