//! YouTube 公式投票の開始、終了、集計をローカルのアンケートと同期する。

use std::{future::Future, sync::Arc, time::Duration};

use futures_util::future::{BoxFuture, FutureExt, Shared};
use tokio::{
    sync::{watch, Mutex},
    time::{self, Instant},
};
use tokio_util::sync::CancellationToken;

use crate::{
    config::{AppConfig, ChannelPlatform},
    poll::{Poll, PollError, PollSnapshot, ValidatedStart},
    poll_control::PollControlError,
    sources::{
        youtube_polls::{
            self, YoutubePolls, YtPollCreated, YtPollState, YtPollStatus, YtPollsError,
        },
        youtube_send::{SendError, YoutubeAuth},
    },
};

const POLL_INTERVAL: Duration = Duration::from_secs(10);
// YouTube が `pollingIntervalMillis` で長い間隔を求めても、待つのは 60 秒まで。
const MAX_POLL_INTERVAL: Duration = Duration::from_secs(60);
// 作成応答を失った時の照合 GET は、各待機時間の後に 1 回まで送る。
const MATCH_RETRY_SECS: [u64; 3] = [1, 2, 4];

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum YoutubeControlError {
    #[error(transparent)]
    Api(#[from] YtPollsError),
    #[error("{0}")]
    Broadcast(String),
    #[error("YouTube認証を確認できませんでした。設定からGoogleに再接続してください")]
    Authentication,
    #[error("通信が途切れ、YouTubeでアンケートの作成を確認できませんでした。YouTube側に投票が作られた可能性があるので、YouTubeで確認・終了してください")]
    CreateUnconfirmed,
    #[error("YouTubeの票数を取得できませんでした。配信チャンネルの所有者でログインしてください")]
    MissingTallies,
}

impl YoutubeControlError {
    fn class(&self) -> &'static str {
        match self {
            Self::Api(error) => error.class(),
            Self::Broadcast(_) => "Broadcast",
            Self::Authentication => "Authentication",
            Self::CreateUnconfirmed => "CreateUnconfirmed",
            Self::MissingTallies => "MissingTallies",
        }
    }

    fn transient(&self) -> bool {
        // 429 (rateLimitExceeded) は短時間の制限なので、票を保ったまま次の周期を待つ。
        matches!(self, Self::Api(error) if error.transient() || matches!(error, YtPollsError::Api(429)))
    }

    fn should_end_remote(&self) -> bool {
        matches!(
            self,
            Self::MissingTallies
                | Self::Api(YtPollsError::Forbidden(_) | YtPollsError::Api(400..=499))
        )
    }
}

type Transition = Shared<BoxFuture<'static, Result<Option<YtPollState>, YoutubeControlError>>>;

#[derive(Clone)]
struct Target {
    client_id: String,
    live_chat_id: String,
    remote_id: String,
}

struct ActivePoll {
    local_id: String,
    target: Target,
    transition: Option<Transition>,
    cancel: CancellationToken,
}

impl Drop for ActivePoll {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub(crate) struct YoutubePollControl {
    pub(crate) auth: Arc<YoutubeAuth>,
    notify: Box<dyn Fn(&str) + Send + Sync>,
    api: Arc<YoutubePolls>,
    poll: Arc<Poll>,
    config: watch::Receiver<AppConfig>,
    app_cancel: CancellationToken,
    active: Mutex<Option<ActivePoll>>,
}

impl YoutubePollControl {
    pub(crate) fn new(
        poll: Arc<Poll>,
        config: watch::Receiver<AppConfig>,
        app_cancel: CancellationToken,
        notify: impl Fn(&str) + Send + Sync + 'static,
    ) -> Self {
        Self {
            auth: Arc::new(
                YoutubeAuth::new().expect("YouTube認証用HTTPクライアントの作成に失敗しました"),
            ),
            notify: Box::new(notify),
            api: Arc::new(YoutubePolls::new(youtube_polls::YOUTUBE_BASE)),
            poll,
            config,
            app_cancel,
            active: Mutex::new(None),
        }
    }

    // IPC とローカル API の開始・終了・リセットは PollControl 側で直列化する。
    pub(crate) async fn start(
        self: &Arc<Self>,
        start: ValidatedStart,
    ) -> Result<PollSnapshot, PollControlError> {
        let client_id = {
            let config = self.config.borrow();
            let mut own = config.channels.iter().filter(|channel| {
                channel.enabled
                    && channel.platform == ChannelPlatform::Youtube
                    && channel.own_stream
            });
            if own.next().is_none() {
                return Err(PollError::Invalid(
                    "有効なYouTubeチャンネルを「自分の配信」に設定してください",
                )
                .into());
            }
            if own.next().is_some() {
                return Err(PollError::Invalid(
                    "「自分の配信」に設定する有効なYouTubeチャンネルは1件にしてください",
                )
                .into());
            }
            config.credentials.youtube_oauth_client_id.trim().to_owned()
        };
        let status = self
            .auth
            .status(&client_id)
            .await
            .map_err(|_| YoutubeControlError::Authentication)?;
        if !status.connected {
            return Err(PollError::Invalid("設定からGoogleアカウントへ接続してください").into());
        }

        // 認証再試行の対象は配信の解決まで。作成 POST は 401 を含め一度しか送らない。
        let (token, live_chat_id) = self
            .auth
            .with_token_retry(&client_id, |token| async {
                let live_chat_id = self.auth.poll_live_chat_id(&token).await?;
                Ok((token, live_chat_id))
            })
            .await
            .map_err(|error| match error {
                SendError::Message(message) => YoutubeControlError::Broadcast(message),
                SendError::Api { status, body } => {
                    match youtube_polls::map_error(status, &body, &[]) {
                        YtPollsError::BadRequest(_) | YtPollsError::AlreadyActive => {
                            YtPollsError::Api(status.as_u16()).into()
                        }
                        YtPollsError::Forbidden(_) => YtPollsError::Forbidden(String::new()).into(),
                        safe => safe.into(),
                    }
                }
                other => auth_error(other),
            })?;

        let question = &start.preferences.question;
        let options = &start.preferences.options;
        let sent_at_ms = chrono::Utc::now().timestamp_millis();
        let (remote_id, polling_interval_ms) = match self
            .api
            .create(&token, &live_chat_id, question, options)
            .await
        {
            Ok(YtPollCreated { id, .. }) => (id, None),
            Err(YtPollsError::Network) => {
                let found = self
                    .reconcile(
                        &client_id,
                        token,
                        &live_chat_id,
                        question,
                        options,
                        sent_at_ms,
                    )
                    .await?;
                (found.id, found.polling_interval_ms)
            }
            Err(error) => return Err(YoutubeControlError::from(error).into()),
        };
        // 締め切りと残り時間の起点は、作成の応答(照合で採用した時は採用時)を受けた時点。
        // publishedAt や壁時計の差は使わないので、時計がずれていても早く閉じない。
        // 起点は設定の保存より前に取り、保存が遅くても締め切りを延ばさない。
        let accepted_at = Instant::now();
        let accepted_at_ms = u64::try_from(chrono::Utc::now().timestamp_millis()).unwrap_or(0);
        let target = Target {
            client_id,
            live_chat_id,
            remote_id,
        };

        // 保存に失敗しても、YouTube に作成できた投票は追跡する。
        let saved = self.poll.save_start_preferences(&start);
        let duration_secs = start.duration_secs;
        let mut snapshot = self.poll.open(start);
        let deadline = duration_secs.map(|secs| {
            let duration = Duration::from_secs(u64::from(secs));
            self.poll.set_remote_timing(
                &snapshot.id,
                accepted_at_ms,
                accepted_at_ms + u64::from(secs) * 1000,
            );
            snapshot = self.poll.snapshot();
            accepted_at + duration
        });
        let cancel = self.app_cancel.child_token();
        *self.active.lock().await = Some(ActivePoll {
            local_id: snapshot.id.clone(),
            target: target.clone(),
            transition: None,
            cancel: cancel.clone(),
        });
        if let Some(deadline) = deadline {
            tauri::async_runtime::spawn(self.clone().expire(
                snapshot.id.clone(),
                deadline,
                cancel.clone(),
            ));
        }
        tauri::async_runtime::spawn(self.clone().run(
            snapshot.id.clone(),
            target,
            accepted_at + next_poll_delay(polling_interval_ms),
            deadline,
            cancel,
        ));
        saved?;
        Ok(snapshot)
    }

    async fn reconcile(
        &self,
        client_id: &str,
        mut token: String,
        live_chat_id: &str,
        question: &str,
        options: &[String],
        sent_at_ms: i64,
    ) -> Result<YtPollState, YoutubeControlError> {
        let mut refresh = false;
        for seconds in MATCH_RETRY_SECS {
            let found = self
                .app_cancel
                .run_until_cancelled(async {
                    time::sleep(Duration::from_secs(seconds)).await;
                    // 401 の後は、次の GET の前に refresh する(GET は合計 3 回まで)。
                    if refresh {
                        self.auth.forget_access_token(&token).await;
                        token = self
                            .auth
                            .access_token(client_id)
                            .await
                            .map_err(|_| YtPollsError::Unauthorized)?;
                    }
                    self.api.active(&token, live_chat_id).await
                })
                .await;
            refresh = matches!(found, Some(Err(YtPollsError::Unauthorized)));
            match found {
                Some(Ok(Some(found)))
                    if found.status == YtPollStatus::Active
                        && found
                            .published_at_ms
                            .is_some_and(|time| time > sent_at_ms - 10_000)
                        && found.question.trim() == question
                        && found
                            .options
                            .iter()
                            .map(|text| text.trim())
                            .eq(options.iter().map(String::as_str)) =>
                {
                    return Ok(found);
                }
                Some(Ok(None) | Err(YtPollsError::Unauthorized)) => {}
                Some(Err(error)) if error.transient() => {}
                _ => break,
            }
        }
        Err(YoutubeControlError::CreateUnconfirmed)
    }

    pub(crate) async fn finish(
        &self,
        expected_id: Option<&str>,
        reset: bool,
    ) -> Option<Result<PollSnapshot, YoutubeControlError>> {
        let (local_id, transition) = {
            let mut active = self.active.lock().await;
            let poll = active.as_mut()?;
            if expected_id.is_some_and(|id| id != poll.local_id) {
                return None;
            }
            // 期限と手動操作が重なった時は同じ transition を待つ。
            let transition = poll
                .transition
                .get_or_insert_with(|| {
                    let target = poll.target.clone();
                    let auth = self.auth.clone();
                    let api = self.api.clone();
                    async move {
                        with_access(&auth, &target.client_id, |token| {
                            let api = &api;
                            let target = &target;
                            async move {
                                api.end_for_chat(&token, &target.remote_id, &target.live_chat_id)
                                    .await
                            }
                        })
                        .await
                        .map(|state| state.filter(|state| state.id == target.remote_id))
                    }
                    .boxed()
                    .shared()
                })
                .clone();
            poll.cancel.cancel();
            (poll.local_id.clone(), transition)
        };
        let mut result = transition.await;
        let mut active = self.active.lock().await;
        let still_current = active
            .as_ref()
            .is_some_and(|poll| poll.local_id == local_id);
        if expected_id.is_some() && !still_current {
            return None;
        }
        let snapshot = if reset {
            self.poll.reset()
        } else if still_current {
            let snapshot = self.poll.snapshot();
            match &result {
                Ok(Some(state)) if snapshot.id == local_id => {
                    match snapshot_votes(state, snapshot.options.len()) {
                        Some(votes) => {
                            self.poll.apply_external(&local_id, &votes, true);
                            self.poll.snapshot()
                        }
                        None => {
                            result = Err(YoutubeControlError::MissingTallies);
                            self.poll.stop_if_current(&local_id)
                        }
                    }
                }
                _ => self.poll.stop_if_current(&local_id),
            }
        } else {
            self.poll.snapshot()
        };
        if still_current {
            *active = None;
        }
        Some(result.map(|_| snapshot))
    }

    async fn run(
        self: Arc<Self>,
        local_id: String,
        target: Target,
        first_get: Instant,
        deadline: Option<Instant>,
        cancel: CancellationToken,
    ) {
        let next_get = Mutex::new(first_get);
        let mut last_error = None;
        loop {
            let scheduled = *next_get.lock().await;
            if deadline.is_some_and(|at| scheduled >= at)
                || cancel
                    .run_until_cancelled(time::sleep_until(scheduled))
                    .await
                    .is_none()
            {
                return;
            }
            // 401 の再試行も、直前の GET を送信してから 10 秒空ける。
            // 認証更新と保存は途中で打ち切らず、キャンセル後は送信と反映を止める。
            let (next_get, cancel) = (&next_get, &cancel);
            let (api, live_chat_id) = (&self.api, &target.live_chat_id);
            let result = with_access(&self.auth, &target.client_id, |token| async move {
                let mut scheduled = next_get.lock().await;
                let wake_at = deadline.map_or(*scheduled, |at| at.min(*scheduled));
                if cancel
                    .run_until_cancelled(time::sleep_until(wake_at))
                    .await
                    .is_none()
                    || deadline.is_some_and(|at| Instant::now() >= at)
                {
                    return Ok(None);
                }
                *scheduled = Instant::now() + POLL_INTERVAL;
                drop(scheduled);
                api.active(&token, live_chat_id).await
            })
            .await;

            let mut active = self.active.lock().await;
            if cancel.is_cancelled()
                || deadline.is_some_and(|at| Instant::now() >= at)
                || !active
                    .as_ref()
                    .is_some_and(|poll| poll.local_id == local_id && poll.transition.is_none())
            {
                return;
            }
            let error = match result {
                Ok(Some(state)) if state.id == target.remote_id => {
                    match snapshot_votes(&state, self.poll.snapshot().options.len()) {
                        Some(votes) => {
                            last_error = None;
                            let delay = next_poll_delay(state.polling_interval_ms);
                            if delay > POLL_INTERVAL {
                                let mut scheduled = next_get.lock().await;
                                *scheduled += delay - POLL_INTERVAL;
                            }
                            let closed = state.status == YtPollStatus::Closed;
                            self.poll.apply_external(&local_id, &votes, closed);
                            if closed {
                                *active = None;
                                return;
                            }
                            continue;
                        }
                        None => YoutubeControlError::MissingTallies,
                    }
                }
                Ok(_) => {
                    self.poll.stop_if_current(&local_id);
                    *active = None;
                    return;
                }
                Err(error) => error,
            };
            let class = error.class();
            if last_error != Some(class) {
                tracing::warn!(error_class = class, "YouTubeアンケートの集計に失敗しました");
                last_error = Some(class);
            }
            if !error.transient() {
                self.poll.stop_if_current(&local_id);
                *active = None;
                drop(active);
                if error.should_end_remote() {
                    let ended = match self.auth.access_token(&target.client_id).await {
                        Ok(token) => matches!(
                            self.api
                                .end_for_chat(&token, &target.remote_id, &target.live_chat_id)
                                .await,
                            Ok(Some(state))
                                if state.id == target.remote_id
                                    && state.status == YtPollStatus::Closed
                        ),
                        Err(_) => false,
                    };
                    if ended {
                        (self.notify)(&format!("{error}。YouTube 側でも終了しました"));
                    } else {
                        (self.notify)(&format!("{error}。YouTube 側で投票を終了してください"));
                    }
                } else if !matches!(error, YoutubeControlError::Api(YtPollsError::NotFound)) {
                    (self.notify)(&error.to_string());
                }
                return;
            }
        }
    }

    async fn expire(
        self: Arc<Self>,
        local_id: String,
        deadline: Instant,
        cancel: CancellationToken,
    ) {
        if cancel
            .run_until_cancelled(time::sleep_until(deadline))
            .await
            .is_some()
        {
            if let Some(Err(error)) = self.finish(Some(&local_id), false).await {
                tracing::warn!(
                    error_class = error.class(),
                    "YouTubeアンケートの終了に失敗しました"
                );
                (self.notify)(&error.to_string());
            }
        }
    }
}

async fn with_access<T, F, Fut>(
    auth: &YoutubeAuth,
    client_id: &str,
    mut run: F,
) -> Result<T, YoutubeControlError>
where
    F: FnMut(String) -> Fut,
    Fut: Future<Output = Result<T, YtPollsError>>,
{
    auth.with_token_retry(client_id, |token| {
        let result = run(token);
        async move {
            match result.await {
                Err(YtPollsError::Unauthorized) => Err(SendError::Unauthorized),
                other => Ok(other),
            }
        }
    })
    .await
    .map_err(auth_error)?
    .map_err(Into::into)
}

fn auth_error(error: SendError) -> YoutubeControlError {
    match error {
        SendError::Unauthorized => YtPollsError::Unauthorized.into(),
        SendError::Network(_) => YtPollsError::Network.into(),
        _ => YoutubeControlError::Authentication,
    }
}

fn next_poll_delay(interval_ms: Option<u64>) -> Duration {
    interval_ms
        .map(Duration::from_millis)
        .map_or(POLL_INTERVAL, |interval| {
            interval.clamp(POLL_INTERVAL, MAX_POLL_INTERVAL)
        })
}

fn snapshot_votes(state: &YtPollState, count: usize) -> Option<Vec<u32>> {
    state
        .tallies
        .as_ref()
        .filter(|tallies| tallies.len() == count)
        .map(|tallies| {
            tallies
                .iter()
                .map(|&votes| u32::try_from(votes).unwrap_or(u32::MAX))
                .collect()
        })
}

#[cfg(test)]
#[path = "poll_control_youtube_tests.rs"]
mod tests;
