//! ツイキャス公式 API v2 のコメントポーリング。

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::{pending, Future};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use super::twitcasting::{
    channel_name, event_to_chat, extract_screen_id, MapContext, StartedMovies, INVALID_ID_NOTICE,
    NOT_FOUND_NOTICE, UNAVAILABLE_NOTICE,
};
use super::twitcasting_api::{
    comment_interval, ApiError, CommentsQuery, LiveGuard, TwitcastingApi,
};
use super::twitcasting_auth::AuthState;
use super::{send_live_event, Backoff, Source};
use crate::integrations::SecretStore;
use crate::model::{ChatMessage, LiveEvent, LiveStream, Platform};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

pub const MAX_PAGES_PER_POLL: u32 = 5;
pub const DEDUPE_CAPACITY: usize = 4096;
pub const LOGIN_REQUIRED_NOTICE: &str = "ツイキャスにログインしてください";
pub const RELOGIN_REQUIRED_NOTICE: &str = "ツイキャスに再ログインしてください";
pub const APP_DISABLED_NOTICE: &str = "このアプリはツイキャスで無効化されています";
const LOAD_FAILED_NOTICE: &str =
    "ツイキャスのログイン情報を読み込めませんでした。再ログインしてください";

const PAGE_SIZE: u32 = 50;
const HISTORY_LIMIT: usize = 20;
const STARTED_MOVIES_LIMIT: usize = 256;
const COMMENT_UNIT: Duration = Duration::from_secs(2);
const STALE_COMMENT_MS: i64 = 60_000;
// 取得時に決めた古さ。写像では読まれず、外へは出ない。
const STALE_FIELD: &str = "_fcStale";

#[derive(Default)]
pub struct CommentCursor {
    last_numeric_id: Option<u64>,
    seen: HashSet<String>,
    seen_order: VecDeque<String>,
}

impl CommentCursor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn slice_id(&self) -> Option<String> {
        self.last_numeric_id.map(|id| id.to_string())
    }

    pub fn begin_poll(&self) -> PollBatch {
        PollBatch {
            slice_id: self.slice_id(),
            pages: 0,
            ended_on_empty: false,
            entries: Vec::new(),
        }
    }

    pub fn commit(&mut self, batch: PollBatch) -> Vec<Value> {
        let mut numeric = Vec::new();
        let mut other = Vec::new();
        let mut batch_ids = HashSet::new();
        for value in batch.entries {
            let Some(id) = id_string(&value["id"]) else {
                continue;
            };
            let number = id.parse::<u64>().ok();
            if self.seen.contains(&id)
                || number
                    .zip(self.last_numeric_id)
                    .is_some_and(|(id, floor)| id <= floor)
                || !batch_ids.insert(id.clone())
            {
                continue;
            }
            if let Some(n) = number {
                numeric.push((n, id, value));
            } else {
                other.push((id, value));
            }
        }

        numeric.sort_by_key(|(n, _, _)| *n);
        if let Some((n, _, _)) = numeric.last().filter(|(n, _, _)| *n > 0) {
            self.last_numeric_id = Some(*n);
        }

        numeric
            .into_iter()
            .map(|(_, id, value)| (id, value))
            .chain(other.into_iter().rev())
            .map(|(id, value)| {
                if self.seen_order.len() == DEDUPE_CAPACITY {
                    if let Some(oldest) = self.seen_order.pop_front() {
                        self.seen.remove(&oldest);
                    }
                }
                self.seen.insert(id.clone());
                self.seen_order.push_back(id);
                value
            })
            .collect()
    }

    pub fn commit_mid_stream(&mut self, batch: PollBatch) -> Vec<Value> {
        let mut entries = self.commit(batch);
        if entries.len() > HISTORY_LIMIT {
            // commit orders numeric IDs first, so taking the tail alone could
            // discard newer numeric IDs in favor of non-numeric ones.
            let numeric_end = entries.partition_point(|value| {
                id_string(&value["id"]).is_some_and(|id| id.parse::<u64>().is_ok())
            });
            let numeric_kept = numeric_end.min(HISTORY_LIMIT);
            let other_kept = HISTORY_LIMIT - numeric_kept;
            entries.drain(numeric_end..entries.len() - other_kept);
            entries.drain(..numeric_end - numeric_kept);
        }
        entries
    }

    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

pub struct PollBatch {
    slice_id: Option<String>,
    pages: u32,
    ended_on_empty: bool,
    entries: Vec<Value>,
}

impl PollBatch {
    pub fn next_query(&self) -> Option<CommentsQuery> {
        if self.ended_on_empty || self.pages == MAX_PAGES_PER_POLL {
            return None;
        }
        Some(CommentsQuery {
            slice_id: self.slice_id.clone(),
            offset: self.pages * PAGE_SIZE,
        })
    }

    pub fn push_page(&mut self, page: Vec<Value>) {
        if !self.ended_on_empty && self.pages < MAX_PAGES_PER_POLL {
            self.pages += 1;
            self.ended_on_empty = page.is_empty();
            self.entries.extend(page);
        }
    }
}

pub struct OfficialMapContext<'a> {
    pub channel: &'a str,
    pub broadcaster_id: Option<&'a str>,
    pub now_ms: i64,
}

pub fn official_comment_to_chat(v: &Value, ctx: &OfficialMapContext<'_>) -> Option<ChatMessage> {
    let id = id_string(&v["id"])?;
    let message = v["message"].as_str()?;
    let user = &v["from_user"];
    let screen_id = id_string(&user["screen_id"])?;
    // 共通のコメント写像を使い、匿名判定と配信者 ID の扱いを揃える。
    event_to_chat(
        &json!({
            "type": "comment",
            "id": id,
            "message": message,
            "isAnonymous": v["isAnonymous"].as_bool(),
            "author": {
                "id": screen_id,
                "isAnonymous": user["isAnonymous"].as_bool(),
            },
        }),
        &MapContext {
            channel: ctx.channel,
            broadcaster_id: ctx.broadcaster_id,
            now_ms: timestamp_ms(&v["created"]).unwrap_or(ctx.now_ms),
        },
    )
    .map(|mut message| {
        if let Some(name) = user["name"].as_str().filter(|name| !name.trim().is_empty()) {
            message.author.name = name.to_owned();
        }
        message.author.avatar_url = user["image"].as_str().and_then(|image| {
            if image.starts_with("//") {
                Some(format!("https:{image}"))
            } else {
                image.starts_with("https://").then(|| image.to_owned())
            }
        });
        message
    })
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.trim().to_string()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

/// 応答の Date(サーバ時刻。無い・読めない時は PC の現在時刻)より 60 秒超古いコメントは
/// 読み上げない。`created` が無い・0 のコメントは読み上げる。
/// 未来の `created` は新しい扱い(読み上げる)。
fn is_stale_comment(created: &Value, server_now_ms: Option<i64>) -> bool {
    let now_ms = server_now_ms.unwrap_or_else(local_now_ms);
    timestamp_ms(created).is_some_and(|created| now_ms.saturating_sub(created) > STALE_COMMENT_MS)
}

/// 各コメントに、その応答の Date で決めた古さを持たせる。重複 ID はカーソルが採った
/// エントリの判定がそのまま使われる。
fn mark_staleness(page: &mut [Value], server_now_ms: Option<i64>, channel: &str) {
    if server_now_ms.is_none() {
        tracing::debug!(
            channel,
            "ツイキャス公式の応答に読める Date が無いため、PC の時刻でコメントの古さを判定"
        );
    }
    for comment in page {
        let stale = is_stale_comment(&comment["created"], server_now_ms);
        if let Some(fields) = comment.as_object_mut() {
            fields.insert(STALE_FIELD.to_owned(), Value::Bool(stale));
        }
    }
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    let integer = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok());
    if let Some(n) = integer {
        return match n {
            1..=999_999_999_999 => Some(n * 1000),
            1_000_000_000_000.. => Some(n),
            _ => None,
        };
    }
    // 小数の秒(例 1479579471.5)も受ける。
    let n = value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
        .filter(|n: &f64| n.is_finite())?;
    if (1.0..1e12).contains(&n) {
        Some((n * 1000.0).floor() as i64)
    } else if n >= 1e12 {
        Some(n as i64)
    } else {
        None
    }
}

#[derive(Clone)]
pub struct PollTuning {
    pub offline: Duration,
    pub live_refresh: Duration,
    pub comment_unit: Duration,
    pub not_found_retry: Duration,
    pub jitter: bool,
}

impl Default for PollTuning {
    fn default() -> Self {
        Self {
            offline: Duration::from_secs(30),
            live_refresh: Duration::from_secs(30),
            comment_unit: COMMENT_UNIT,
            not_found_retry: Duration::from_secs(300),
            jitter: true,
        }
    }
}

struct Movie {
    id: String,
    broadcaster_id: Option<String>,
    cursor: CommentCursor,
    mid_stream: bool,
    // 非公式経路の枠を引き継いだが、公式 API ではまだ確認していない。
    handed_off: bool,
    protected: bool,
    last_poll: Option<Instant>,
}

impl Movie {
    fn new(id: String, mid_stream: bool) -> Self {
        Self {
            id,
            broadcaster_id: None,
            cursor: CommentCursor::new(),
            mid_stream,
            handed_off: false,
            protected: false,
            last_poll: None,
        }
    }

    fn comment_due(&self, next_comment: Instant, interval: Duration) -> Instant {
        self.last_poll
            .map_or(next_comment, |last| next_comment.max(last + interval))
    }
}

#[derive(Default)]
struct LiveState {
    movie: Option<Movie>,
    guard: Option<LiveGuard>,
    observed_offline_at: Option<Instant>,
}

pub struct TwitcastingOfficialSource<S: SecretStore> {
    identifier: String,
    channel: String,
    api: Arc<TwitcastingApi<S>>,
    auth_state: watch::Receiver<AuthState>,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    live_tx: Option<mpsc::Sender<LiveEvent>>,
    started_movies: StartedMovies,
    initial_movie: Option<String>,
    carry_over_ids: HashSet<String>,
    tuning: PollTuning,
}

impl<S: SecretStore> fmt::Debug for TwitcastingOfficialSource<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingOfficialSource")
            .field("channel", &self.channel)
            .finish_non_exhaustive()
    }
}

impl<S: SecretStore> TwitcastingOfficialSource<S> {
    pub fn new(
        identifier: String,
        api: Arc<TwitcastingApi<S>>,
        auth_state: watch::Receiver<AuthState>,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
        tuning: PollTuning,
    ) -> Self {
        Self {
            channel: channel_name(&identifier),
            identifier,
            api,
            auth_state,
            metadata_tx,
            live_tx: None,
            started_movies: Default::default(),
            initial_movie: None,
            carry_over_ids: HashSet::new(),
            tuning,
        }
    }

    pub fn with_live_tx(mut self, live_tx: Option<mpsc::Sender<LiveEvent>>) -> Self {
        self.live_tx = live_tx;
        self
    }

    pub(crate) fn with_started_movies(mut self, started_movies: StartedMovies) -> Self {
        self.started_movies = started_movies;
        self
    }

    pub(crate) fn with_carry_over_ids(mut self, ids: HashSet<String>) -> Self {
        self.carry_over_ids = ids;
        self
    }

    pub(crate) fn with_initial_movie(mut self, movie: Option<String>) -> Self {
        self.initial_movie = movie;
        self
    }

    async fn poll(&self, tx: broadcast::Sender<ChatMessage>) {
        let Some(screen_id) = extract_screen_id(&self.identifier) else {
            self.send_metadata(false, Some(INVALID_ID_NOTICE), None)
                .await;
            pending::<()>().await;
            return;
        };

        let mut auth = self.auth_state.clone();
        let mut user_id = None;
        let mut live = LiveState {
            movie: self.initial_movie.clone().map(|id| Movie {
                handed_off: true,
                ..Movie::new(id, true)
            }),
            ..LiveState::default()
        };
        let mut carry_over = Some(&self.carry_over_ids);
        let mut auth_notice = None;
        let mut next_live = Instant::now();
        let mut next_comment = next_live;
        let mut request_after = next_live;
        let mut live_backoff = Backoff::new();
        let mut comment_backoff = Backoff::new();
        let mut network_backoff = Backoff::new();
        let mut jitter = super::jitter_seed();

        loop {
            let state = *auth.borrow_and_update();
            if state != AuthState::Ready {
                live.guard = None;
                let notice = match state {
                    AuthState::Loading => None,
                    AuthState::ReloginRequired => Some(RELOGIN_REQUIRED_NOTICE),
                    AuthState::LoadFailed => Some(LOAD_FAILED_NOTICE),
                    _ => Some(LOGIN_REQUIRED_NOTICE),
                };
                if notice != auth_notice {
                    if let Some(notice) = notice {
                        self.send_metadata(false, Some(notice), None).await;
                    }
                    auth_notice = notice;
                }
                if auth.changed().await.is_err() {
                    return;
                }
                next_live = Instant::now();
                continue;
            }
            auth_notice = None;

            let interval = self.comment_interval();
            let comment_due = live
                .movie
                .as_ref()
                .filter(|_| live.guard.is_some())
                .map(|movie| movie.comment_due(next_comment, interval));
            let refresh_live = comment_due.is_none_or(|due| next_live <= due);
            let due = if refresh_live {
                next_live
            } else {
                comment_due.unwrap()
            };
            let due = due.max(request_after);
            if due > Instant::now() {
                tokio::select! {
                    changed = auth.changed() => {
                        if changed.is_err() { return; }
                    }
                    _ = time::sleep_until(due) => {}
                }
                continue;
            }

            let result = if let Some(id) = user_id.as_deref() {
                if refresh_live {
                    let result = self
                        .refresh_movie(&screen_id, id, &mut live, &mut auth)
                        .await;
                    let interval = if live.movie.is_some() {
                        self.tuning.live_refresh
                    } else {
                        self.tuning.offline
                    };
                    next_live = Instant::now() + self.refresh_delay(interval, &mut jitter);
                    result.map(|new_movie| {
                        if new_movie {
                            next_comment = Instant::now();
                            comment_backoff.reset();
                        }
                    })
                } else {
                    let movie = live.movie.as_mut().expect("comment poll needs a movie");
                    let result = while_ready(&mut auth, self.fetch_comments(movie)).await;
                    // A poll delayed by the shared budget still leaves a full interval
                    // before the next poll.
                    movie.last_poll = Some(Instant::now());
                    result.map(|batch| {
                        if *auth.borrow() == AuthState::Ready {
                            self.publish_comments(movie, batch, carry_over.take(), &tx);
                        }
                    })
                }
            } else {
                while_ready(&mut auth, self.api.user(&screen_id))
                    .await
                    .and_then(|user| {
                        user_id = id_string(&user["user"]["id"]);
                        user_id.as_ref().map(|_| ()).ok_or(ApiError::NotFound)
                    })
            };

            let (next, backoff) = if refresh_live {
                (&mut next_live, &mut live_backoff)
            } else {
                (&mut next_comment, &mut comment_backoff)
            };
            match result {
                Ok(()) => {
                    backoff.reset();
                    network_backoff.reset();
                }
                Err(ApiError::Unauthorized | ApiError::RateLimited { .. }) => {
                    // A late 401 leaves the watch at Ready. Use the next poll
                    // with the new credentials instead of waiting for a change.
                    if user_id.is_none() {
                        *next = Instant::now() + self.tuning.comment_unit;
                    }
                }
                Err(ApiError::Protected) => {
                    live.guard = None;
                    if let Some(movie) = &mut live.movie {
                        movie.protected = true;
                    } else if user_id.is_none() {
                        *next = Instant::now() + self.tuning.not_found_retry;
                    }
                    self.send_metadata(live.movie.is_some(), Some(UNAVAILABLE_NOTICE), None)
                        .await;
                }
                Err(ApiError::AppDisabled) => {
                    live.guard = None;
                    request_after = Instant::now() + self.tuning.not_found_retry;
                    next_live = request_after;
                    self.send_metadata(false, Some(APP_DISABLED_NOTICE), None)
                        .await;
                }
                Err(ApiError::NotFound) if user_id.is_none() => {
                    *next = Instant::now() + self.tuning.not_found_retry;
                    self.send_metadata(false, Some(NOT_FOUND_NOTICE), None)
                        .await;
                }
                Err(ApiError::NotFound) => {
                    next_live = Instant::now();
                }
                Err(ApiError::Network) => {
                    // Both schedules use the same connection. During an outage
                    // neither request should cut short the other's backoff.
                    request_after = Instant::now() + network_backoff.next_delay();
                }
                Err(ApiError::Server | ApiError::Other) => {
                    *next = (*next).max(Instant::now() + backoff.next_delay());
                }
            }
        }
    }

    async fn refresh_movie(
        &self,
        screen_id: &str,
        user_id: &str,
        live: &mut LiveState,
        auth: &mut watch::Receiver<AuthState>,
    ) -> Result<bool, ApiError> {
        let snapshot = while_ready(auth, self.api.current_live(user_id)).await?;
        let movie_id = snapshot.as_ref().and_then(|v| id_string(&v["movie"]["id"]));
        let Some((snapshot, movie_id)) = snapshot.as_ref().zip(movie_id) else {
            // 初回が未配信の場合も、自動告知に開始前の観測として伝える。
            if live.movie.take().is_some() || live.observed_offline_at.is_none() {
                self.send_live_ended();
            }
            live.guard = None;
            live.observed_offline_at = Some(Instant::now());
            self.send_metadata(false, None, None).await;
            return Ok(false);
        };

        let new_movie = live.movie.as_ref().is_none_or(|movie| movie.id != movie_id);
        if new_movie {
            let handed_off = live.movie.as_ref().is_some_and(|movie| movie.handed_off);
            if handed_off {
                self.send_live_ended();
            }
            let recently_offline = live.observed_offline_at.take().is_some_and(|offline_at| {
                offline_at.elapsed() <= self.tuning.offline.saturating_mul(2)
            });
            let mid_stream = !recently_offline && (live.movie.is_none() || handed_off);
            live.movie = Some(Movie::new(movie_id, mid_stream));
        }
        let movie = live.movie.as_mut().expect("live snapshot has a movie");
        movie.handed_off = false;
        movie.broadcaster_id = id_string(&snapshot["broadcaster"]["screen_id"]);
        if let Some(protected) = snapshot["movie"]["is_protected"].as_bool() {
            movie.protected = protected;
        }
        if movie.protected {
            live.guard = None;
        } else {
            live.guard.get_or_insert_with(|| self.api.live_guard());
        }

        let notice = movie.protected.then_some(UNAVAILABLE_NOTICE);
        self.send_metadata(true, notice, Some(&snapshot["movie"]))
            .await;
        self.send_live_started(screen_id, &movie.id, &snapshot["movie"]);
        Ok(new_movie)
    }

    fn publish_comments(
        &self,
        movie: &mut Movie,
        batch: PollBatch,
        skip_ids: Option<&HashSet<String>>,
        tx: &broadcast::Sender<ChatMessage>,
    ) {
        let history = std::mem::take(&mut movie.mid_stream);
        let entries = if history {
            movie.cursor.commit_mid_stream(batch)
        } else {
            movie.cursor.commit(batch)
        };
        let context = OfficialMapContext {
            channel: &self.channel,
            broadcaster_id: movie.broadcaster_id.as_deref(),
            now_ms: local_now_ms(),
        };
        for (mut message, stale) in entries.iter().filter_map(|v| {
            let stale = v[STALE_FIELD].as_bool() == Some(true);
            Some((official_comment_to_chat(v, &context)?, stale))
        }) {
            if skip_ids.is_some_and(|ids| ids.contains(&message.id)) {
                continue;
            }
            message.skip_tts = history || stale;
            message.route.uncounted = history || stale;
            let _ = tx.send(message);
        }
    }

    async fn fetch_comments(&self, movie: &Movie) -> Result<PollBatch, ApiError> {
        let mut batch = movie.cursor.begin_poll();
        while let Some(query) = batch.next_query() {
            let (mut page, date_ms) = self.api.comments_with_date(&movie.id, query).await?;
            mark_staleness(&mut page, date_ms, &self.channel);
            batch.push_page(page);
            if movie.mid_stream {
                break;
            }
        }
        if batch.pages == MAX_PAGES_PER_POLL && !batch.ended_on_empty {
            tracing::warn!(
                count = batch.entries.len(),
                "ツイキャス公式コメントのページ上限に到達"
            );
        }
        Ok(batch)
    }

    fn comment_interval(&self) -> Duration {
        let interval = comment_interval(self.api.live_count());
        if self.tuning.comment_unit == COMMENT_UNIT {
            return interval;
        }
        // Preserve the canonical interval's channel scaling for custom units.
        let units = interval.as_secs() / COMMENT_UNIT.as_secs();
        self.tuning
            .comment_unit
            .saturating_mul(u32::try_from(units).unwrap_or(u32::MAX))
    }

    fn refresh_delay(&self, delay: Duration, seed: &mut u64) -> Duration {
        if !self.tuning.jitter {
            return delay;
        }
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
        delay.mul_f64((90 + *seed % 21) as f64 / 100.0)
    }

    async fn send_metadata(&self, live: bool, notice: Option<&str>, movie: Option<&Value>) {
        let Some(tx) = &self.metadata_tx else { return };
        let concurrent_viewers = movie
            .and_then(|v| v["current_view_count"].as_u64())
            .and_then(|n| u32::try_from(n).ok());
        let title = movie.and_then(|v| v["title"].as_str()).map(str::to_string);
        let _ = tx
            .send(YoutubeMetadataUpdate {
                platform: Platform::Twitcasting,
                channel: self.channel.clone(),
                concurrent_viewers,
                viewers_kind: ViewerCountKind::Concurrent,
                likes: None,
                title,
                live: Some(live),
                notice: notice.map(str::to_string),
                reactions_delta: None,
                full_snapshot: movie.is_some() || !live,
            })
            .await;
    }

    fn send_live_started(&self, screen_id: &str, movie_id: &str, movie: &Value) {
        let Some(tx) = &self.live_tx else { return };
        let mut started = self
            .started_movies
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if started.iter().any(|id| id == movie_id) {
            return;
        }
        let mut url =
            reqwest::Url::parse("https://twitcasting.tv/").expect("fixed Twitcasting URL");
        url.path_segments_mut()
            .expect("Twitcasting URL has a path")
            .pop_if_empty()
            .extend([screen_id, "movie", movie_id]);
        let event = LiveEvent::Started(LiveStream {
            platform: Platform::Twitcasting,
            channel_identifier: self.channel.clone(),
            stream_id: movie_id.to_string(),
            title: movie["title"].as_str().map(str::to_string),
            url: url.to_string(),
            started_at_ms: timestamp_ms(&movie["created"]).map(|ms| ms as u64),
        });
        if send_live_event(tx, event) {
            if started.len() == STARTED_MOVIES_LIMIT {
                started.pop_front();
            }
            started.push_back(movie_id.to_string());
        }
    }

    fn send_live_ended(&self) {
        if let Some(tx) = &self.live_tx {
            send_live_event(
                tx,
                LiveEvent::Ended {
                    platform: Platform::Twitcasting,
                    channel_identifier: self.channel.clone(),
                },
            );
        }
    }
}

// Also stop waiting for a request's rate budget when credentials become unavailable.
async fn while_ready<T>(
    auth: &mut watch::Receiver<AuthState>,
    request: impl Future<Output = Result<T, ApiError>>,
) -> Result<T, ApiError> {
    tokio::select! {
        biased;
        _ = auth.wait_for(|state| *state != AuthState::Ready) => Err(ApiError::Unauthorized),
        result = request => result,
    }
}

impl<S: SecretStore> Source for TwitcastingOfficialSource<S> {
    fn name(&self) -> String {
        format!("twitcasting-official:{}", self.channel)
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => {}
                _ = self.poll(tx) => {}
            }
            Ok(())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        autopost::{
            coordinator::{Action, AutopostSettings, Coordinator, Input},
            ledger::Ledger,
        },
        config::{ChannelConfig, ChannelPlatform, XPostConfig},
        integrations::MemorySecretStore,
        sources::twitcasting_auth::{AuthOptions, TwitcastingAuth, TOKEN_KEY},
        TwitcastingEndpoints,
    };
    use axum::{http::StatusCode, routing::get, Json, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use tokio::{sync::Notify, task::JoinHandle, time::timeout};

    struct RunningSource {
        api: Arc<TwitcastingApi<MemorySecretStore>>,
        auth: watch::Sender<AuthState>,
        metadata: mpsc::Receiver<YoutubeMetadataUpdate>,
        messages: broadcast::Receiver<ChatMessage>,
        live_events: mpsc::Receiver<LiveEvent>,
        user_calls: Arc<AtomicUsize>,
        source: JoinHandle<()>,
        server: JoinHandle<()>,
    }

    impl RunningSource {
        async fn metadata_matching(
            &mut self,
            mut matches: impl FnMut(&YoutubeMetadataUpdate) -> bool,
        ) -> YoutubeMetadataUpdate {
            timeout(Duration::from_secs(5), async {
                loop {
                    let update = self.metadata.recv().await.unwrap();
                    if matches(&update) {
                        return update;
                    }
                }
            })
            .await
            .expect("expected metadata update")
        }
    }

    impl Drop for RunningSource {
        fn drop(&mut self) {
            self.source.abort();
            self.server.abort();
        }
    }

    async fn start_source(router: Router, tuning: PollTuning, state: AuthState) -> RunningSource {
        start_source_with_initial_movie(router, tuning, state, None).await
    }

    async fn start_source_with_initial_movie(
        router: Router,
        tuning: PollTuning,
        state: AuthState,
        initial_movie: Option<&str>,
    ) -> RunningSource {
        let user_calls = Arc::new(AtomicUsize::new(0));
        let router = router.route(
            "/users/Alice",
            get({
                let user_calls = Arc::clone(&user_calls);
                move || async move {
                    user_calls.fetch_add(1, Ordering::Relaxed);
                    Json(json!({ "user": { "id": "1" } }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoints = TwitcastingEndpoints {
            api_base: format!("http://{}", listener.local_addr().unwrap()),
            ..TwitcastingEndpoints::default()
        };
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemorySecretStore::new();
        store
            .put(
                TOKEN_KEY,
                &json!({
                    "clientId": "test-client",
                    "accessToken": "test-token",
                    "userId": "1",
                    "screenId": "alice",
                    "name": "Alice",
                    "expiresAtMs": null,
                    "obtainedAtMs": now_ms
                })
                .to_string(),
            )
            .await
            .unwrap();
        let credentials = TwitcastingAuth::new(store, endpoints.clone(), AuthOptions::default());
        credentials.load(Some("test-client"), now_ms).await;
        assert_eq!(*credentials.subscribe().borrow(), AuthState::Ready);
        let api = TwitcastingApi::new(
            credentials,
            endpoints,
            Arc::new(|| Some("test-client".into())),
        );
        let (auth, auth_rx) = watch::channel(state);
        let (metadata_tx, metadata) = mpsc::channel(32);
        let (tx, messages) = broadcast::channel(256);
        let (live_tx, live_events) = mpsc::channel(8);
        let official = TwitcastingOfficialSource::new(
            "https://twitcasting.tv/Alice#access_token=MARKER".into(),
            Arc::clone(&api),
            auth_rx,
            Some(metadata_tx),
            tuning,
        )
        .with_live_tx(Some(live_tx))
        .with_initial_movie(initial_movie.map(str::to_owned))
        .with_started_movies(Arc::new(std::sync::Mutex::new(
            initial_movie.into_iter().map(str::to_owned).collect(),
        )));
        let source = tokio::spawn(async move { official.poll(tx).await });
        RunningSource {
            api,
            auth,
            metadata,
            messages,
            live_events,
            user_calls,
            source,
            server,
        }
    }

    fn test_tuning() -> PollTuning {
        PollTuning {
            offline: Duration::from_millis(100),
            live_refresh: Duration::from_millis(100),
            comment_unit: Duration::from_millis(20),
            not_found_retry: Duration::from_millis(250),
            jitter: false,
        }
    }

    #[tokio::test]
    async fn first_offline_observation_allows_announcement_with_stale_minutes_zero() {
        const CREATED: u64 = 1_800_000_000;
        const START_MS: u64 = CREATED * 1000;
        let checks = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route(
            "/users/1/current_live",
            get(move || {
                let check = checks.fetch_add(1, Ordering::Relaxed);
                async move {
                    if check < 2 {
                        (StatusCode::NOT_FOUND, Json(Value::Null))
                    } else {
                        (
                            StatusCode::OK,
                            Json(json!({ "movie": { "id": "42", "created": CREATED } })),
                        )
                    }
                }
            }),
        );
        let mut source = start_source(router, test_tuning(), AuthState::Ready).await;
        let ended = timeout(Duration::from_secs(5), source.live_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &ended,
            LiveEvent::Ended { channel_identifier, .. } if channel_identifier == "alice"
        ));
        let started = timeout(Duration::from_secs(5), source.live_events.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(
            &started,
            LiveEvent::Started(stream)
                if stream.stream_id == "42"
                    && stream.channel_identifier == "alice"
                    && stream.started_at_ms == Some(START_MS)
        ));
        assert!(!format!("{ended:?} {started:?}").contains("MARKER"));

        let settings = AutopostSettings {
            x_post: XPostConfig {
                enabled: true,
                intent_open: "auto".into(),
                stale_minutes: 0,
                delay_secs: 0,
                ..Default::default()
            },
            channels: vec![ChannelConfig {
                platform: ChannelPlatform::Twitcasting,
                identifier: "https://twitcasting.tv/Alice#access_token=MARKER".into(),
                enabled: true,
                own_stream: true,
                overrides: Default::default(),
            }],
            connected: false,
        };
        let mut coordinator = Coordinator::new(Ledger::default(), settings);
        coordinator.handle(Input::Live(ended), START_MS - 1000);
        let actions = coordinator.handle(Input::Live(started), START_MS + 1000);
        assert!(actions
            .iter()
            .any(|action| matches!(action, Action::OpenIntent { .. })));
        assert!(!format!("{actions:?}").contains("MARKER"));
    }

    fn handoff_router(movie_id: &'static str) -> Router {
        Router::new()
            .route(
                "/users/1/current_live",
                get(move || async move { Json(json!({ "movie": { "id": movie_id } })) }),
            )
            .route(
                &format!("/movies/{movie_id}/comments"),
                get(
                    |axum::extract::Query(params): axum::extract::Query<
                        std::collections::HashMap<String, String>,
                    >| async move {
                        let ids = match (
                            params.get("slice_id").map(String::as_str),
                            params.get("offset").map(String::as_str),
                        ) {
                            (None, Some("0")) => (1..=50).rev().collect::<Vec<_>>(),
                            (Some("50"), Some("0")) => vec![51],
                            _ => Vec::new(),
                        };
                        let comments: Vec<_> = ids
                            .into_iter()
                            .map(|id| json!({ "id": id, "message": "chat", "from_user": { "screen_id": "alice" } }))
                            .collect();
                        Json(json!({ "comments": comments }))
                    },
                ),
            )
    }

    async fn assert_handoff_comments(messages: &mut broadcast::Receiver<ChatMessage>) {
        for id in 31..=50 {
            let message = messages.recv().await.unwrap();
            assert_eq!(message.id, format!("tc-{id}"));
            assert!(message.skip_tts);
        }
        let next = messages.recv().await.unwrap();
        assert_eq!(next.id, "tc-51");
        assert!(!next.skip_tts);
    }

    #[tokio::test]
    async fn official_history_is_uncounted_and_live_comments_are_counted() {
        let mut source = start_source(handoff_router("42"), test_tuning(), AuthState::Ready).await;
        for id in 31..=50 {
            assert_next_comment(&mut source, id, true).await;
        }
        assert_next_comment(&mut source, 51, false).await;
    }

    #[tokio::test]
    async fn handoff_to_different_movie_ends_old_movie_and_silences_history() {
        let mut source = start_source_with_initial_movie(
            handoff_router("43"),
            test_tuning(),
            AuthState::Ready,
            Some("42"),
        )
        .await;
        timeout(Duration::from_secs(5), async {
            assert!(matches!(
                source.live_events.recv().await,
                Some(LiveEvent::Ended { channel_identifier, .. }) if channel_identifier == "alice"
            ));
            assert!(matches!(
                source.live_events.recv().await,
                Some(LiveEvent::Started(stream))
                    if stream.stream_id == "43" && stream.channel_identifier == "alice"
            ));
            assert_handoff_comments(&mut source.messages).await;
            assert!(matches!(
                source.live_events.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn handoff_to_same_movie_keeps_polling_without_duplicate_live_events() {
        let mut source = start_source_with_initial_movie(
            handoff_router("42"),
            test_tuning(),
            AuthState::Ready,
            Some("42"),
        )
        .await;
        timeout(Duration::from_secs(5), async {
            assert_handoff_comments(&mut source.messages).await;
            assert!(matches!(
                source.live_events.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn current_live_server_error_keeps_scheduled_refresh() {
        let (calls, mut requests) = mpsc::unbounded_channel();
        let router = Router::new().route(
            "/users/1/current_live",
            get(move || async move {
                calls.send(Instant::now()).unwrap();
                StatusCode::SERVICE_UNAVAILABLE
            }),
        );
        let tuning = PollTuning {
            offline: Duration::from_secs(2),
            ..test_tuning()
        };
        let interval = tuning.offline;
        let _source = start_source(router, tuning, AuthState::Ready).await;
        let (first, second) = timeout(Duration::from_secs(5), async {
            (
                requests.recv().await.unwrap(),
                requests.recv().await.unwrap(),
            )
        })
        .await
        .unwrap();
        assert!(second.duration_since(first) >= interval);
    }

    #[tokio::test]
    async fn loading_auth_is_silent_until_its_state_changes() {
        let router = Router::new().route(
            "/users/1/current_live",
            get(|| async { StatusCode::NOT_FOUND }),
        );
        let mut source = start_source(router, test_tuning(), AuthState::Loading).await;
        assert!(timeout(Duration::from_millis(100), source.metadata.recv())
            .await
            .is_err());
        assert_eq!(source.user_calls.load(Ordering::Relaxed), 0);

        source.auth.send(AuthState::LoadFailed).unwrap();
        source
            .metadata_matching(|update| update.notice.as_deref() == Some(LOAD_FAILED_NOTICE))
            .await;
        assert_eq!(source.user_calls.load(Ordering::Relaxed), 0);

        source.auth.send(AuthState::Ready).unwrap();
        source
            .metadata_matching(|update| update.live == Some(false) && update.notice.is_none())
            .await;
        assert_eq!(source.user_calls.load(Ordering::Relaxed), 1);
    }

    async fn live_count_after_comment_error(
        code: u32,
        notice: &str,
        protection_after_error: Option<bool>,
        expected_after_refresh: usize,
    ) {
        let release_error = Arc::new(Notify::new());
        let sent_error = Arc::new(AtomicBool::new(false));
        let router = Router::new()
            .route(
                "/users/1/current_live",
                get({
                    let sent_error = Arc::clone(&sent_error);
                    move || async move {
                        let mut movie = json!({ "id": "9" });
                        if let Some(protected) =
                            protection_after_error.filter(|_| sent_error.load(Ordering::Relaxed))
                        {
                            movie["is_protected"] = json!(protected);
                        }
                        Json(json!({ "movie": movie }))
                    }
                }),
            )
            .route(
                "/movies/9/comments",
                get({
                    let release_error = Arc::clone(&release_error);
                    move || async move {
                        release_error.notified().await;
                        sent_error.store(true, Ordering::Relaxed);
                        (
                            StatusCode::FORBIDDEN,
                            Json(json!({ "error": { "code": code } })),
                        )
                    }
                }),
            );
        let mut source = start_source(router, test_tuning(), AuthState::Ready).await;
        source
            .metadata_matching(|update| update.live == Some(true))
            .await;
        assert_eq!(source.api.live_count(), 1);
        release_error.notify_one();

        source
            .metadata_matching(|update| update.notice.as_deref() == Some(notice))
            .await;
        assert_eq!(source.api.live_count(), 0);

        source
            .metadata_matching(|update| update.live == Some(true))
            .await;
        assert_eq!(source.api.live_count(), expected_after_refresh);
    }

    #[tokio::test]
    async fn protected_movie_stays_out_of_live_count_after_refresh() {
        live_count_after_comment_error(2002, UNAVAILABLE_NOTICE, Some(true), 0).await;
    }

    #[tokio::test]
    async fn protected_movie_resumes_when_refresh_reports_unprotected() {
        live_count_after_comment_error(2002, UNAVAILABLE_NOTICE, Some(false), 1).await;
    }

    #[tokio::test]
    async fn app_disabled_movie_leaves_live_count_until_refresh_recovers() {
        live_count_after_comment_error(2001, APP_DISABLED_NOTICE, None, 1).await;
    }

    fn comment(id: u32) -> Value {
        json!({ "id": id, "message": "chat", "from_user": { "screen_id": "alice" } })
    }

    #[test]
    fn comment_older_than_sixty_seconds_is_not_read_aloud() {
        let server_ms = 1_800_000_000_000;
        let server_s = server_ms / 1000;
        assert!(!is_stale_comment(&json!(server_s - 59), Some(server_ms)));
        assert!(!is_stale_comment(&json!(server_s - 60), Some(server_ms)));
        assert!(is_stale_comment(&json!(server_s - 61), Some(server_ms)));
        // ミリ秒の created もそのまま比べる。
        assert!(!is_stale_comment(
            &json!(server_ms - 59_000),
            Some(server_ms)
        ));
        assert!(is_stale_comment(
            &json!(server_ms - 61_000),
            Some(server_ms)
        ));
        assert!(is_stale_comment(
            &json!((server_s - 61).to_string()),
            Some(server_ms)
        ));
    }

    #[test]
    fn missing_date_compares_with_local_now() {
        let now_s = local_now_ms() / 1000;
        assert!(!is_stale_comment(&json!(now_s - 30), None));
        assert!(is_stale_comment(&json!(now_s - 120), None));
    }

    #[test]
    fn comment_without_created_is_read_aloud() {
        for created in [json!(0), Value::Null, json!(""), json!("abc"), json!(-5)] {
            assert!(
                !is_stale_comment(&created, Some(1_800_000_000_000)),
                "{created}"
            );
        }
    }

    #[test]
    fn extreme_or_future_created_does_not_overflow_and_is_read_aloud() {
        // Date "Wed, 31 Dec 1969 23:59:59 GMT" = -1000 ms.
        assert!(!is_stale_comment(&json!(i64::MAX), Some(-1000)));
        assert!(!is_stale_comment(&json!(i64::MAX), Some(i64::MIN)));
        assert!(is_stale_comment(&json!(1), Some(i64::MAX)));
        let server_ms = 1_800_000_000_000;
        assert!(!is_stale_comment(
            &json!(server_ms / 1000 + 3600),
            Some(server_ms)
        ));
    }

    #[test]
    fn decimal_created_is_parsed_as_seconds_or_milliseconds() {
        assert_eq!(timestamp_ms(&json!(1479579471.0)), Some(1_479_579_471_000));
        assert_eq!(
            timestamp_ms(&json!("1479579471.5")),
            Some(1_479_579_471_500)
        );
        assert_eq!(
            timestamp_ms(&json!(1479579471500.0)),
            Some(1_479_579_471_500)
        );
        assert_eq!(
            timestamp_ms(&json!("1479579471500.0")),
            Some(1_479_579_471_500)
        );
        assert_eq!(timestamp_ms(&json!(0.0)), None);
        assert_eq!(timestamp_ms(&json!("-1.5")), None);
        assert_eq!(timestamp_ms(&json!("NaN")), None);
        let server_ms = 1_479_579_471_000 + 61_000;
        assert!(is_stale_comment(&json!(1479579471.0), Some(server_ms)));
        assert!(!is_stale_comment(
            &json!("1479579471.5"),
            Some(server_ms - 2000)
        ));
    }

    #[test]
    fn fractional_created_just_over_60s_is_stale() {
        // 60.0004 秒前: 切り捨てで 60001 ms 差になり、四捨五入で 60 秒ちょうどに丸めない
        assert!(is_stale_comment(&json!(1799999939.9996), Some(1_800_000_000_000)));
        assert!(!is_stale_comment(&json!(1799999940.0), Some(1_800_000_000_000)));
    }

    #[test]
    fn duplicate_id_keeps_the_staleness_of_the_adopted_entry() {
        let server_ms = 1_800_000_000_000;
        let server_s = server_ms / 1000;
        let mut fresh = vec![json!({ "id": 5, "message": "a", "created": server_s - 1 })];
        let mut old = vec![json!({ "id": 5, "message": "b", "created": server_s - 600 })];
        mark_staleness(&mut fresh, Some(server_ms), "alice");
        mark_staleness(&mut old, Some(server_ms), "alice");
        let mut cursor = CommentCursor::new();
        let mut batch = cursor.begin_poll();
        batch.push_page(fresh);
        batch.push_page(old);
        let adopted = cursor.commit(batch);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0]["message"], "a");
        assert_eq!(adopted[0][STALE_FIELD], json!(false));
    }

    #[test]
    fn missing_date_logs_one_debug_line_with_channel_only() {
        let sink = crate::log_filter_tests::Sink::default();
        tracing::subscriber::with_default(
            crate::log_subscriber(Some("debug"), sink.clone()),
            || {
                mark_staleness(&mut [json!({ "id": 1 })], None, "alice");
                mark_staleness(&mut [json!({ "id": 2 })], Some(1), "alice");
            },
        );
        let output = sink.output();
        let lines: Vec<_> = output
            .lines()
            .filter(|line| line.contains("Date"))
            .collect();
        assert_eq!(lines.len(), 1, "{output}");
        assert!(lines[0].contains("alice"));
        assert!(!output.contains("http"));
    }

    #[test]
    fn official_comment_uses_shared_anonymous_rules() {
        for (screen_id, flag, expected_id) in [
            (" C:TW123 ", false, "tc:anonymous"),
            ("TwitcastinGuest", false, "tc:anonymous"),
            ("c:twinkle", false, "c:twinkle"),
            ("Bob", false, "Bob"),
            ("Bob", true, "tc:anonymous"),
        ] {
            let mut raw = comment(1);
            raw["from_user"]["screen_id"] = json!(screen_id);
            raw["from_user"]["isAnonymous"] = json!(flag);
            let mapped = official_comment_to_chat(
                &raw,
                &OfficialMapContext {
                    channel: "alice",
                    broadcaster_id: Some(screen_id.trim()),
                    now_ms: 0,
                },
            )
            .unwrap();
            assert_eq!(mapped.author.id, expected_id);
            assert_eq!(
                mapped.author.roles.broadcaster,
                expected_id != "tc:anonymous"
            );
        }
    }

    #[tokio::test]
    async fn transient_refresh_server_error_does_not_silence_comments() {
        let refresh_calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route(
                "/users/1/current_live",
                get({
                    let refresh_calls = Arc::clone(&refresh_calls);
                    move || async move {
                        if refresh_calls.fetch_add(1, Ordering::SeqCst) == 1 {
                            return (StatusCode::SERVICE_UNAVAILABLE, Json(Value::Null));
                        }
                        (StatusCode::OK, Json(json!({ "movie": { "id": "9" } })))
                    }
                }),
            )
            .merge(incremental_comments());
        let mut source = start_source(
            router,
            PollTuning {
                live_refresh: Duration::from_millis(125),
                comment_unit: Duration::from_millis(200),
                ..test_tuning()
            },
            AuthState::Ready,
        )
        .await;
        assert_next_comment(&mut source, 1, true).await;
        for id in 2..=3 {
            assert_next_comment(&mut source, id, false).await;
        }
        assert!(refresh_calls.load(Ordering::SeqCst) >= 2);
    }

    fn incremental_comments() -> Router {
        Router::new().route(
            "/movies/9/comments",
            get(
                |axum::extract::Query(params): axum::extract::Query<
                    std::collections::HashMap<String, String>,
                >| async move {
                    let entries = match (
                        params.get("slice_id").map(String::as_str),
                        params.get("offset").map(String::as_str),
                    ) {
                        (None, Some("0")) => vec![comment(1)],
                        (Some("1"), Some("0")) => vec![comment(2)],
                        (Some("2"), Some("0")) => vec![comment(3)],
                        _ => Vec::new(),
                    };
                    Json(json!({ "comments": entries }))
                },
            ),
        )
    }

    async fn assert_next_comment(source: &mut RunningSource, id: u32, skip_tts: bool) {
        let message = timeout(Duration::from_secs(5), source.messages.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(message.id, format!("tc-{id}"));
        assert_eq!(message.skip_tts, skip_tts, "comment {id}");
        assert_eq!(message.route.uncounted, skip_tts, "comment {id}");
    }

    #[test]
    fn mid_stream_history_prefers_newest_numeric_ids() {
        let entries: Vec<Value> = (1..=25)
            .rev()
            .flat_map(|id| [json!({ "id": id }), json!({ "id": format!("other-{id}") })])
            .collect();
        let mut cursor = CommentCursor::new();
        let mut batch = cursor.begin_poll();
        batch.push_page(entries);
        let history = cursor.commit_mid_stream(batch);
        assert_eq!(
            history
                .iter()
                .map(|entry| entry["id"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            (6..=25).collect::<Vec<_>>()
        );
        assert_eq!(cursor.slice_id().as_deref(), Some("25"));

        let mut batch = cursor.begin_poll();
        batch.push_page(vec![json!({ "id": "other-25" })]);
        assert!(cursor.commit(batch).is_empty());
    }

    #[tokio::test]
    async fn expired_offline_observation_treats_new_movie_as_mid_stream() {
        let checks = Arc::new(AtomicUsize::new(0));
        let router = Router::new()
            .route("/users/1/current_live", get(move || async move {
                if checks.fetch_add(1, Ordering::Relaxed) == 0 {
                    return (StatusCode::NOT_FOUND, Json(Value::Null));
                }
                time::sleep(Duration::from_millis(250)).await;
                (StatusCode::OK, Json(json!({ "movie": { "id": "9" } })))
            }))
            .route("/movies/9/comments", get(|axum::extract::Query(params): axum::extract::Query<std::collections::HashMap<String, String>>| async move {
                let comments: Vec<_> = if !params.contains_key("slice_id") && params.get("offset").is_some_and(|offset| offset == "0") {
                    (101..=150).rev().map(|id| json!({ "id": id, "message": "chat", "from_user": { "screen_id": "alice" } })).collect()
                } else {
                    Vec::new()
                };
                Json(json!({ "comments": comments }))
            }));
        let mut source = start_source(router, test_tuning(), AuthState::Ready).await;
        source
            .metadata_matching(|update| update.live == Some(false))
            .await;
        for id in 131..=150 {
            let message = timeout(Duration::from_secs(5), source.messages.recv())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(message.id, format!("tc-{id}"));
            assert!(message.skip_tts);
        }
        assert!(timeout(Duration::from_millis(100), source.messages.recv())
            .await
            .is_err());
    }
}
