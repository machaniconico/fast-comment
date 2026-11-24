//! IPC とローカル API で共有するアンケート操作。

use std::{sync::Arc, time::Duration};

use reqwest::StatusCode;
use tokio::sync::{watch, Mutex};
use tokio_util::sync::{CancellationToken, DropGuard};

use crate::{
    config::{AppConfig, ChannelPlatform},
    moderation_error::{ModerationError, ModerationErrorCode},
    poll::{Poll, PollDisplaySettings, PollError, PollSnapshot, PollSource, ValidatedStart},
    sources::{
        twitch_auth::{TwitchAccess, TwitchAuth, POLL_SCOPES},
        twitch_moderation::HelixError,
        twitch_polls::{HelixPolls, PollsError, RemotePoll, RemoteStatus, HELIX_BASE},
    },
};

#[cfg(test)]
#[path = "poll_control_tests.rs"]
mod tests;

const POLL_INTERVAL: Duration = Duration::from_secs(3);

#[derive(Debug, thiserror::Error)]
pub(crate) enum PollControlError {
    #[error(transparent)]
    Local(#[from] PollError),
    #[error(transparent)]
    Remote(#[from] PollsError),
}

impl From<ModerationError> for PollControlError {
    fn from(error: ModerationError) -> Self {
        match error.code {
            ModerationErrorCode::ScopeMissing => PollsError::ScopeMissing.into(),
            ModerationErrorCode::NotLoggedIn => {
                PollError::Invalid("設定からTwitchにログインしてください").into()
            }
            ModerationErrorCode::NotFound => {
                PollError::Invalid("「自分の配信」のTwitchチャンネルが見つかりません").into()
            }
            ModerationErrorCode::Network => PollsError::Network.into(),
            _ => PollsError::Authentication.into(),
        }
    }
}

impl From<HelixError> for PollControlError {
    fn from(error: HelixError) -> Self {
        match error {
            HelixError::Api {
                status: StatusCode::UNAUTHORIZED,
                ..
            } => PollsError::Unauthorized.into(),
            HelixError::Api { status, .. } => PollsError::Api(status.as_u16()).into(),
            HelixError::Network(_) => PollsError::Network.into(),
            HelixError::Moderation(error) => error.into(),
        }
    }
}

#[derive(Clone)]
struct RemoteTarget {
    remote_id: String,
    broadcaster_id: String,
}

struct ActivePoll {
    local_id: String,
    target: RemoteTarget,
    _cancel: DropGuard,
}

pub(crate) struct PollControl {
    poll: Arc<Poll>,
    auth: Arc<TwitchAuth>,
    config: watch::Receiver<AppConfig>,
    app_cancel: CancellationToken,
    #[cfg(test)]
    helix_base: String,
    // 操作と集計結果の反映を直列化する。ポーラーの HTTP 待ちには保持しない。
    active: Mutex<Option<ActivePoll>>,
}

impl PollControl {
    pub(crate) fn new(
        poll: Arc<Poll>,
        auth: Arc<TwitchAuth>,
        config: watch::Receiver<AppConfig>,
        app_cancel: CancellationToken,
    ) -> Self {
        Self {
            poll,
            auth,
            config,
            app_cancel,
            #[cfg(test)]
            helix_base: HELIX_BASE.to_owned(),
            active: Mutex::new(None),
        }
    }

    pub(crate) fn snapshot(&self) -> PollSnapshot {
        self.poll.snapshot()
    }

    pub(crate) async fn start(
        self: &Arc<Self>,
        start: ValidatedStart,
    ) -> Result<PollSnapshot, PollControlError> {
        let mut active = self.active.lock().await;
        if start.preferences.source == PollSource::Chat {
            return Ok(self.poll.start(start)?);
        }
        self.poll.check_can_start()?;

        let login = {
            let config = self.config.borrow();
            let mut own = config.channels.iter().filter(|channel| {
                channel.enabled && channel.platform == ChannelPlatform::Twitch && channel.own_stream
            });
            let channel = own.next().ok_or(PollError::Invalid(
                "Twitchチャンネルを「自分の配信」に設定してください",
            ))?;
            if own.next().is_some() {
                return Err(PollError::Invalid(
                    "「自分の配信」に設定するTwitchチャンネルは1件にしてください",
                )
                .into());
            }
            channel.identifier.clone()
        };
        let credentials = self.credentials();
        let status = self
            .auth
            .status(&credentials.0)
            .await
            .map_err(|_| PollError::Invalid("Twitchのログイン情報を確認できませんでした"))?;
        if !status.connected {
            return Err(PollError::Invalid("設定からTwitchにログインしてください").into());
        }
        if !status.polls_scope_ok {
            return Err(PollsError::ScopeMissing.into());
        }

        let login = &login;
        let start_ref = &start;
        let target = self
            .with_access(&credentials, None, |access, helix| async move {
                let generation = self.auth.generation();
                let broadcaster_id = self
                    .auth
                    .resolve_user_id(generation, login, &access)
                    .await?;
                if broadcaster_id != access.user_id {
                    return Err(PollError::Invalid(
                        "「自分の配信」のTwitchチャンネルとログイン中のユーザーが一致しません",
                    )
                    .into());
                }
                let title = &start_ref.preferences.question;
                let choices = &start_ref.preferences.options;
                let remote_id = match helix
                    .create(
                        &access.token,
                        &broadcaster_id,
                        title,
                        choices,
                        start_ref.duration_secs.expect("Twitchの投票時間は検証済み"),
                    )
                    .await
                {
                    Ok(id) => id,
                    Err(PollsError::Network) => {
                        // 作成済みの可能性があるので POST は繰り返さず、最新の投票を照合する。
                        helix
                            .find_matching_latest(&access.token, &broadcaster_id, title, choices)
                            .await
                            .ok()
                            .flatten()
                            .ok_or(PollsError::CreateUnconfirmed)?
                    }
                    Err(error) => return Err(error.into()),
                };
                Ok(RemoteTarget {
                    remote_id,
                    broadcaster_id,
                })
            })
            .await?;

        // 設定保存に失敗しても、作成済みの投票は追跡できるようにする。
        let saved = self.poll.save_start_preferences(&start);
        let snapshot = self.poll.open(start);
        let cancel = self.app_cancel.child_token();
        *active = Some(ActivePoll {
            local_id: snapshot.id.clone(),
            target,
            _cancel: cancel.clone().drop_guard(),
        });
        tauri::async_runtime::spawn(self.clone().run(snapshot.id.clone(), cancel));
        saved?;
        Ok(snapshot)
    }

    pub(crate) async fn stop(&self) -> Result<PollSnapshot, PollControlError> {
        let mut active = self.active.lock().await;
        let Some(poll) = active.as_ref() else {
            return Ok(self.poll.stop());
        };
        let result = self.stop_remote(&poll.target).await;
        match &result {
            Ok(votes) => {
                self.poll.apply_external(&poll.local_id, votes, true);
            }
            Err(_) => {
                self.poll.stop();
            }
        }
        *active = None;
        result?;
        Ok(self.snapshot())
    }

    async fn stop_remote(&self, target: &RemoteTarget) -> Result<Vec<u32>, PollControlError> {
        match self.end(target).await {
            Ok(()) => Ok(self.get(target).await?.votes),
            Err(error @ PollControlError::Remote(PollsError::BadRequest(_))) => {
                // PATCH の直前に終了した場合は最終値を使う。
                match self.get(target).await {
                    Ok(remote) if remote.status != RemoteStatus::Active => Ok(remote.votes),
                    _ => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn reset(&self) -> Result<PollSnapshot, PollControlError> {
        let mut active = self.active.lock().await;
        let result = match active.as_ref() {
            Some(poll) => self.reset_remote(&poll.target).await,
            None => Ok(()),
        };
        *active = None;
        let snapshot = self.poll.reset();
        result?;
        Ok(snapshot)
    }

    async fn reset_remote(&self, target: &RemoteTarget) -> Result<(), PollControlError> {
        if self.get(target).await?.status != RemoteStatus::Active {
            return Ok(());
        }
        match self.end(target).await {
            Err(PollControlError::Remote(PollsError::BadRequest(_) | PollsError::Api(404))) => {
                Ok(())
            }
            result => result,
        }
    }

    pub(crate) async fn set_display(
        &self,
        settings: PollDisplaySettings,
    ) -> Result<PollSnapshot, PollError> {
        let _active = self.active.lock().await;
        self.poll.set_display(settings)
    }

    async fn run(self: Arc<Self>, local_id: String, cancel: CancellationToken) {
        let mut ticks =
            tokio::time::interval_at(tokio::time::Instant::now() + POLL_INTERVAL, POLL_INTERVAL);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last_error = None;
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticks.tick() => {}
            }
            let target = {
                let Some(active) = cancel.run_until_cancelled(self.active.lock()).await else {
                    return;
                };
                let Some(poll) = active.as_ref().filter(|poll| poll.local_id == local_id) else {
                    return;
                };
                poll.target.clone()
            };

            // refresh token の回転・保存は途中で打ち切らず、終了時には結果の反映だけ止める。
            let result = self.get(&target).await;
            let Some(mut active) = cancel.run_until_cancelled(self.active.lock()).await else {
                return;
            };
            if cancel.is_cancelled()
                || !active
                    .as_ref()
                    .is_some_and(|poll| poll.local_id == local_id)
            {
                return;
            }

            match result {
                Ok(remote) => {
                    last_error = None;
                    let closed = remote.status != RemoteStatus::Active;
                    if !self.poll.apply_external(&local_id, &remote.votes, closed) || closed {
                        *active = None;
                        return;
                    }
                }
                Err(error) => {
                    let class = match &error {
                        PollControlError::Local(_) => "Local",
                        PollControlError::Remote(error) => error.class(),
                    };
                    if last_error != Some(class) {
                        tracing::warn!(error_class = class, "Twitchアンケートの集計に失敗しました");
                        last_error = Some(class);
                    }
                    match error {
                        PollControlError::Remote(PollsError::Api(404)) => {
                            self.poll.stop();
                            *active = None;
                            return;
                        }
                        PollControlError::Remote(PollsError::RateLimited { retry_after_secs }) => {
                            if let Some(retry_at) = tokio::time::Instant::now()
                                .checked_add(Duration::from_secs(retry_after_secs))
                            {
                                ticks.reset_at(retry_at);
                            } else {
                                drop(active);
                                cancel.cancelled().await;
                                return;
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    fn credentials(&self) -> (String, String) {
        let config = self.config.borrow();
        (
            config.credentials.twitch_client_id.trim().to_owned(),
            config.credentials.twitch_client_secret.trim().to_owned(),
        )
    }

    async fn with_access<T, F, Fut>(
        &self,
        credentials: &(String, String),
        broadcaster_id: Option<&str>,
        mut run: F,
    ) -> Result<T, PollControlError>
    where
        F: FnMut(TwitchAccess, HelixPolls) -> Fut,
        Fut: std::future::Future<Output = Result<T, PollControlError>>,
    {
        let (client_id, client_secret) = credentials;
        #[cfg(not(test))]
        let helix_base = HELIX_BASE;
        #[cfg(test)]
        let helix_base = &self.helix_base;
        self.auth
            .with_token_retry(
                client_id,
                client_secret,
                POLL_SCOPES,
                |error| matches!(error, PollControlError::Remote(PollsError::Unauthorized)),
                |access| {
                    let allowed = broadcaster_id.is_none_or(|id| id == access.user_id);
                    let helix = HelixPolls::with_client(
                        helix_base,
                        &access.client_id,
                        self.auth.http.clone(),
                    );
                    let request = allowed.then(|| run(access, helix));
                    async move {
                        match request {
                            Some(request) => request.await,
                            None => Err(PollError::Invalid(
                                "アンケートを開始したTwitchユーザーでログインしてください",
                            )
                            .into()),
                        }
                    }
                },
            )
            .await
    }

    async fn get(&self, target: &RemoteTarget) -> Result<RemotePoll, PollControlError> {
        self.with_access(
            &self.credentials(),
            Some(&target.broadcaster_id),
            |access, helix| async move {
                Ok(helix
                    .get(&access.token, &target.broadcaster_id, &target.remote_id)
                    .await?)
            },
        )
        .await
    }

    async fn end(&self, target: &RemoteTarget) -> Result<(), PollControlError> {
        self.with_access(
            &self.credentials(),
            Some(&target.broadcaster_id),
            |access, helix| async move {
                Ok(helix
                    .end(
                        &access.token,
                        &target.broadcaster_id,
                        &target.remote_id,
                        false,
                    )
                    .await?)
            },
        )
        .await
    }
}
