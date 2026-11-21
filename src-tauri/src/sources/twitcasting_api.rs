//! ツイキャス公式 API v2。認証とレート予算を公式経路の全行で共有する。

use std::{
    collections::VecDeque,
    fmt,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::{header::ACCEPT, Client, StatusCode, Url};
use serde_json::Value;
use tracing::instrument::WithSubscriber;

use super::twitcasting_auth::{TwitcastingAuth, TwitcastingEndpoints};
use crate::{config::AppConfig, integrations::SecretStore};

const WINDOW_MS: u64 = 60_000;
const PAGE_LIMIT: u32 = 50;

pub fn official_endpoints(_config: &AppConfig) -> TwitcastingEndpoints {
    TwitcastingEndpoints::default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApiError {
    Unauthorized,
    RateLimited { reset_epoch_s: Option<u64> },
    Protected,
    AppDisabled,
    NotFound,
    Server,
    Network,
    Other,
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Unauthorized => "ツイキャスの認証が必要です",
            Self::RateLimited { .. } => "ツイキャス公式 API の利用制限に達しました",
            Self::Protected => "合言葉付きの配信は読めません",
            Self::AppDisabled => "このアプリは無効化されています",
            Self::NotFound => "ツイキャスの対象が見つかりません",
            Self::Server => "ツイキャス公式 API でサーバーエラーが発生しました",
            Self::Network => "ツイキャス公式 API との通信に失敗しました",
            Self::Other => "ツイキャス公式 API の応答を処理できません",
        })
    }
}

impl std::error::Error for ApiError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentsQuery {
    pub slice_id: Option<String>,
    pub offset: u32,
}

pub struct RateBudget {
    capacity: usize,
    requests: VecDeque<u64>,
    paused_until_ms: u64,
}

impl RateBudget {
    pub fn new() -> Self {
        Self::with_capacity(54)
    }

    pub fn with_capacity(per_minute: u32) -> Self {
        Self {
            capacity: per_minute.max(1) as usize,
            requests: VecDeque::new(),
            paused_until_ms: 0,
        }
    }

    /// 待機が不要な場合だけ、移動窓の予算を 1 回分使う。
    pub fn delay_before_next(&mut self, now_ms: u64) -> Duration {
        while self
            .requests
            .front()
            .is_some_and(|&oldest| now_ms.saturating_sub(oldest) >= WINDOW_MS)
        {
            self.requests.pop_front();
        }

        let mut deadline = self.paused_until_ms;
        if self.requests.len() == self.capacity {
            deadline = deadline.max(self.requests[0].saturating_add(WINDOW_MS));
        }
        let delay_ms = deadline.saturating_sub(now_ms);
        if delay_ms == 0 {
            self.requests.push_back(now_ms);
        }
        Duration::from_millis(delay_ms)
    }

    pub fn observe_headers(&mut self, remaining: Option<&str>, reset: Option<&str>, now_ms: u64) {
        let remaining = remaining.and_then(|value| value.parse::<u64>().ok());
        let reset_epoch_s = reset.and_then(|value| value.parse::<u64>().ok());
        if let (Some(0..=2), Some(reset_epoch_s)) = (remaining, reset_epoch_s) {
            self.penalize(Some(reset_epoch_s), now_ms);
        }
    }

    pub fn penalize(&mut self, reset_epoch_s: Option<u64>, now_ms: u64) {
        let delay_ms = reset_epoch_s.map_or(WINDOW_MS, |reset| {
            reset
                .saturating_add(1)
                .saturating_mul(1_000)
                .saturating_sub(now_ms)
                .clamp(1_000, 120_000)
        });
        self.paused_until_ms = self.paused_until_ms.max(now_ms.saturating_add(delay_ms));
    }
}

impl Default for RateBudget {
    fn default() -> Self {
        Self::new()
    }
}

pub fn comment_interval(live_count: usize) -> Duration {
    Duration::from_secs((live_count as u64).saturating_mul(2).max(4))
}

pub type ClientIdFn = Arc<dyn Fn() -> Option<String> + Send + Sync>;

pub struct TwitcastingApi<S: SecretStore> {
    auth: Arc<TwitcastingAuth<S>>,
    api_base: Result<Url, ApiError>,
    client_id: ClientIdFn,
    http: Result<Client, ApiError>,
    budget: Mutex<RateBudget>,
    live_count: Arc<AtomicUsize>,
}

impl<S: SecretStore> fmt::Debug for TwitcastingApi<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingApi").finish_non_exhaustive()
    }
}

impl<S: SecretStore> TwitcastingApi<S> {
    pub fn new(
        auth: Arc<TwitcastingAuth<S>>,
        endpoints: TwitcastingEndpoints,
        client_id: ClientIdFn,
    ) -> Arc<Self> {
        Self::with_budget(auth, endpoints, client_id, RateBudget::new())
    }

    pub fn with_budget(
        auth: Arc<TwitcastingAuth<S>>,
        endpoints: TwitcastingEndpoints,
        client_id: ClientIdFn,
        budget: RateBudget,
    ) -> Arc<Self> {
        let api_base = Url::parse(&endpoints.api_base).map_err(|_| ApiError::Other);
        let http = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|_| ApiError::Other);

        Arc::new(Self {
            auth,
            api_base,
            client_id,
            http,
            budget: Mutex::new(budget),
            live_count: Arc::new(AtomicUsize::new(0)),
        })
    }

    pub async fn user(&self, screen_id: &str) -> Result<Value, ApiError> {
        self.get(&["users", screen_id], &[]).await
    }

    pub async fn current_live(&self, user_id: &str) -> Result<Option<Value>, ApiError> {
        let value = match self
            .get(&["users", user_id, "current_live"], &[("live", "false")])
            .await
        {
            Ok(value) => value,
            Err(ApiError::NotFound) => return Ok(None),
            Err(error) => return Err(error),
        };
        let has_id = match value.pointer("/movie/id") {
            Some(Value::String(id)) => !id.trim().is_empty(),
            Some(Value::Number(_)) => true,
            _ => false,
        };
        let is_offline = value.pointer("/movie/is_live").and_then(Value::as_bool) == Some(false);
        Ok((has_id && !is_offline).then_some(value))
    }

    pub async fn comments(
        &self,
        movie_id: &str,
        query: CommentsQuery,
    ) -> Result<Vec<Value>, ApiError> {
        let offset = query.offset.to_string();
        let limit = PAGE_LIMIT.to_string();
        let mut params = vec![("offset", offset.as_str()), ("limit", limit.as_str())];
        if let Some(slice_id) = query.slice_id.as_deref() {
            params.push(("slice_id", slice_id));
        }

        let mut value = self.get(&["movies", movie_id, "comments"], &params).await?;
        match value.get_mut("comments").map(Value::take) {
            Some(Value::Array(comments)) => Ok(comments),
            _ => Ok(Vec::new()),
        }
    }

    pub fn live_guard(self: &Arc<Self>) -> LiveGuard {
        self.live_count.fetch_add(1, Ordering::Relaxed);
        LiveGuard(Arc::clone(&self.live_count))
    }

    pub fn live_count(&self) -> usize {
        self.live_count.load(Ordering::Relaxed)
    }

    async fn get(&self, segments: &[&str], params: &[(&str, &str)]) -> Result<Value, ApiError> {
        loop {
            let delay = self.budget.lock().unwrap().delay_before_next(epoch_ms());
            if delay.is_zero() {
                break;
            }
            tokio::time::sleep(delay).await;
        }

        let cid = (self.client_id)();
        let (bearer, generation) = self
            .auth
            .bearer_with_generation(cid.as_deref(), epoch_ms())
            .ok_or(ApiError::Unauthorized)?;

        // HTTP ライブラリの TRACE にも接続先やヘッダを流さない。
        async {
            let mut url = self.api_base.clone()?;
            url.path_segments_mut()
                .map_err(|_| ApiError::Other)?
                .pop_if_empty()
                .extend(segments.iter().copied());
            let response = self
                .http
                .as_ref()
                .map_err(Clone::clone)?
                .get(url)
                .query(params)
                .bearer_auth(bearer)
                .header("X-Api-Version", "2.0")
                .header(ACCEPT, "application/json")
                .send()
                .await
                .map_err(|_| ApiError::Network)?;

            let status = response.status();
            if status == StatusCode::UNAUTHORIZED {
                self.auth.report_unauthorized_for(generation);
            }

            let headers = response.headers();
            let remaining = headers
                .get("X-RateLimit-Remaining")
                .and_then(|v| v.to_str().ok());
            let reset = headers
                .get("X-RateLimit-Reset")
                .and_then(|v| v.to_str().ok());
            let reset_epoch_s = reset.and_then(|value| value.parse::<u64>().ok());
            {
                let mut budget = self.budget.lock().unwrap();
                let now_ms = epoch_ms();
                budget.observe_headers(remaining, reset, now_ms);
                if status == StatusCode::TOO_MANY_REQUESTS {
                    budget.penalize(reset_epoch_s, now_ms);
                }
            }

            if status.is_success() {
                return response.json().await.map_err(|error| {
                    if error.is_decode() {
                        ApiError::Other
                    } else {
                        ApiError::Network
                    }
                });
            }

            let code = if status == StatusCode::FORBIDDEN {
                response
                    .json::<Value>()
                    .await
                    .ok()
                    .and_then(|value| value.pointer("/error/code").and_then(Value::as_u64))
            } else {
                None
            };
            let error = classify_status(status, code, reset_epoch_s);
            if status == StatusCode::FORBIDDEN && code == Some(2000) {
                self.budget
                    .lock()
                    .unwrap()
                    .penalize(reset_epoch_s, epoch_ms());
            }
            Err(error)
        }
        .with_subscriber(tracing::subscriber::NoSubscriber::default())
        .await
    }
}

pub struct LiveGuard(Arc<AtomicUsize>);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

fn classify_status(status: StatusCode, code: Option<u64>, reset_epoch_s: Option<u64>) -> ApiError {
    match (status, code) {
        (StatusCode::UNAUTHORIZED, _) => ApiError::Unauthorized,
        (StatusCode::TOO_MANY_REQUESTS, _) | (StatusCode::FORBIDDEN, Some(2000)) => {
            ApiError::RateLimited { reset_epoch_s }
        }
        (StatusCode::FORBIDDEN, Some(2001)) => ApiError::AppDisabled,
        (StatusCode::FORBIDDEN, Some(2002)) => ApiError::Protected,
        (StatusCode::NOT_FOUND, _) => ApiError::NotFound,
        _ if status.is_server_error() => ApiError::Server,
        _ => ApiError::Other,
    }
}

fn epoch_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_status_table() {
        let reset_epoch_s = Some(1_900_000_000);
        let limited = ApiError::RateLimited { reset_epoch_s };
        for (status, code, expected) in [
            (401, None, ApiError::Unauthorized),
            (403, Some(2000), limited.clone()),
            (403, Some(2001), ApiError::AppDisabled),
            (403, Some(2002), ApiError::Protected),
            (403, Some(9), ApiError::Other),
            (403, None, ApiError::Other),
            (404, None, ApiError::NotFound),
            (429, None, limited),
            (500, None, ApiError::Server),
            (503, None, ApiError::Server),
            (302, None, ApiError::Other),
        ] {
            let status = StatusCode::from_u16(status).unwrap();
            assert_eq!(classify_status(status, code, reset_epoch_s), expected);
        }
    }
}
