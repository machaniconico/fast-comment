//! ツイキャス公式 API v2 のコメントポーリング。

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::{pending, Future};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::{self, Instant};
use tokio_util::sync::CancellationToken;

use super::twitcasting::{
    channel_name, extract_screen_id, StartedMovies, INVALID_ID_NOTICE, NOT_FOUND_NOTICE,
    UNAVAILABLE_NOTICE,
};
use super::twitcasting_api::{ApiError, CommentsQuery, LiveGuard, TwitcastingApi};
use super::twitcasting_auth::AuthState;
use super::{send_live_event, Backoff, Source};
use crate::integrations::SecretStore;
use crate::model::{
    Author, ChatMessage, Fragment, LiveEvent, LiveStream, MessageKind, Platform, Roles,
};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

pub const MAX_PAGES_PER_POLL: u32 = 5;
pub const DEDUPE_CAPACITY: usize = 4096;
pub const LOGIN_REQUIRED_NOTICE: &str = "ツイキャスにログインしてください";
pub const RELOGIN_REQUIRED_NOTICE: &str = "ツイキャスに再ログインしてください";
pub const APP_DISABLED_NOTICE: &str = "このアプリはツイキャスで無効化されています";

const PAGE_SIZE: u32 = 50;
const HISTORY_LIMIT: usize = 20;
const STARTED_MOVIES_LIMIT: usize = 256;

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
        entries.drain(..entries.len().saturating_sub(HISTORY_LIMIT));
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
    let message = v["message"].as_str().filter(|s| !s.trim().is_empty())?;
    let user = &v["from_user"];
    let author_id = id_string(&user["id"])?;
    let name = user["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| user["screen_id"].as_str())
        .unwrap_or_default();
    let avatar_url = user["image"].as_str().and_then(|image| {
        if image.starts_with("//") {
            Some(format!("https:{image}"))
        } else {
            image.starts_with("https://").then(|| image.to_string())
        }
    });

    Some(ChatMessage {
        listener: None,
        id: format!("tc-{id}"),
        native_id: false,
        platform: Platform::Twitcasting,
        channel: ctx.channel.to_string(),
        author: Author {
            nickname: None,
            avatar_url,
            roles: Roles {
                broadcaster: ctx.broadcaster_id == Some(author_id.as_str()),
                ..Default::default()
            },
            id: author_id,
            name: name.to_string(),
            display_color: None,
            badges: Vec::new(),
        },
        fragments: vec![Fragment::text(message)],
        kind: MessageKind::Normal,
        amount: None,
        timestamp_ms: timestamp_ms(&v["created"]).unwrap_or(ctx.now_ms),
        raw: None,
        skip_tts: false,
        injected_test: false,
    })
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(id) if !id.trim().is_empty() => Some(id.trim().to_string()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

fn timestamp_ms(value: &Value) -> Option<i64> {
    let n = value
        .as_i64()
        .or_else(|| value.as_str()?.trim().parse().ok())?;
    match n {
        1..=999_999_999_999 => Some(n * 1000),
        1_000_000_000_000.. => Some(n),
        _ => None,
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
            comment_unit: Duration::from_secs(2),
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
    protected: bool,
    last_poll: Option<Instant>,
}

#[derive(Default)]
struct LiveState {
    movie: Option<Movie>,
    guard: Option<LiveGuard>,
    observed_offline: bool,
}

pub struct TwitcastingOfficialSource<S: SecretStore> {
    identifier: String,
    channel: String,
    api: Arc<TwitcastingApi<S>>,
    auth_state: watch::Receiver<AuthState>,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    live_tx: Option<mpsc::Sender<LiveEvent>>,
    started_movies: StartedMovies,
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

    async fn poll(&self, tx: broadcast::Sender<ChatMessage>) {
        let Some(screen_id) = extract_screen_id(&self.identifier) else {
            self.send_metadata(false, Some(INVALID_ID_NOTICE), None)
                .await;
            pending::<()>().await;
            return;
        };

        let mut auth = self.auth_state.clone();
        let mut user_id = None;
        let mut live = LiveState::default();
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
                let notice = if state == AuthState::ReloginRequired {
                    RELOGIN_REQUIRED_NOTICE
                } else {
                    LOGIN_REQUIRED_NOTICE
                };
                if auth_notice != Some(notice) {
                    self.send_metadata(false, Some(notice), None).await;
                    auth_notice = Some(notice);
                }
                if auth.changed().await.is_err() {
                    return;
                }
                continue;
            }
            if auth_notice.take().is_some() {
                next_live = Instant::now();
            }

            let comment_due = live
                .movie
                .as_ref()
                .filter(|movie| !movie.protected)
                .map(|movie| {
                    let live_count =
                        u32::try_from(self.api.live_count().max(2)).unwrap_or(u32::MAX);
                    let interval = self.tuning.comment_unit.saturating_mul(live_count);
                    movie
                        .last_poll
                        .map_or(next_comment, |last| next_comment.max(last + interval))
                });
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
                    if let Some(movie) = &mut live.movie {
                        movie.protected = true;
                    } else if user_id.is_none() {
                        *next = Instant::now() + self.tuning.not_found_retry;
                    }
                    self.send_metadata(live.movie.is_some(), Some(UNAVAILABLE_NOTICE), None)
                        .await;
                }
                Err(ApiError::AppDisabled) => {
                    request_after = Instant::now() + self.tuning.not_found_retry;
                    if !refresh_live {
                        next_live = request_after;
                    }
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
                    *next = Instant::now() + backoff.next_delay();
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
            if live.movie.take().is_some() {
                self.send_live_ended();
            }
            live.guard = None;
            live.observed_offline = true;
            self.send_metadata(false, None, None).await;
            return Ok(false);
        };

        let new_movie = live.movie.as_ref().is_none_or(|movie| movie.id != movie_id);
        if new_movie {
            live.movie = Some(Movie {
                id: movie_id,
                broadcaster_id: None,
                cursor: CommentCursor::new(),
                mid_stream: !live.observed_offline && live.movie.is_none(),
                protected: false,
                last_poll: None,
            });
        }
        let movie = live.movie.as_mut().expect("live snapshot has a movie");
        movie.broadcaster_id = id_string(&snapshot["broadcaster"]["id"]);
        live.guard.get_or_insert_with(|| self.api.live_guard());

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
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let context = OfficialMapContext {
            channel: &self.channel,
            broadcaster_id: movie.broadcaster_id.as_deref(),
            now_ms,
        };
        for mut message in entries
            .iter()
            .filter_map(|v| official_comment_to_chat(v, &context))
        {
            if skip_ids.is_some_and(|ids| ids.contains(&message.id)) {
                continue;
            }
            message.skip_tts = history;
            let _ = tx.send(message);
        }
    }

    async fn fetch_comments(&self, movie: &Movie) -> Result<PollBatch, ApiError> {
        let mut batch = movie.cursor.begin_poll();
        while let Some(query) = batch.next_query() {
            batch.push_page(self.api.comments(&movie.id, query).await?);
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
            channel_identifier: self.identifier.clone(),
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
                    channel_identifier: self.identifier.clone(),
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
