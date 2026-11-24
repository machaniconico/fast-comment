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
            _ => PollsError::Unauthorized.into(),
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

struct ActivePoll {
    local_id: String,
    remote_id: String,
    broadcaster_id: String,
    terminated: bool,
    _cancel: DropGuard,
}

pub(crate) struct PollControl {
    poll: Arc<Poll>,
    auth: Arc<TwitchAuth>,
    config: watch::Receiver<AppConfig>,
    app_cancel: CancellationToken,
    // 集計・操作を直列化する。待ち時間にはロックを保持しない。
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
                channel.platform == ChannelPlatform::Twitch && channel.own_stream
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
        let (remote_id, broadcaster_id) = self
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
                // 保存できない場合は Twitch 上にも投票を作らない。
                self.poll.prepare_start(start_ref)?;
                let remote_id = helix
                    .create(
                        &access.token,
                        &broadcaster_id,
                        &start_ref.preferences.question,
                        &start_ref.preferences.options,
                        start_ref.duration_secs.expect("Twitchの投票時間は検証済み"),
                    )
                    .await?;
                Ok((remote_id, broadcaster_id))
            })
            .await?;

        let snapshot = self.poll.open(start);
        let cancel = self.app_cancel.child_token();
        *active = Some(ActivePoll {
            local_id: snapshot.id.clone(),
            remote_id,
            broadcaster_id,
            terminated: false,
            _cancel: cancel.clone().drop_guard(),
        });
        tauri::async_runtime::spawn(self.clone().run(snapshot.id.clone(), cancel));
        Ok(snapshot)
    }

    pub(crate) async fn stop(&self) -> Result<PollSnapshot, PollControlError> {
        let mut active = self.active.lock().await;
        let Some(poll) = active.as_mut() else {
            return Ok(self.poll.stop());
        };
        let votes = if poll.terminated {
            self.get(poll).await?.votes
        } else {
            match self.end(poll, false).await {
                Ok(()) => {
                    poll.terminated = true;
                    self.get(poll).await?.votes
                }
                Err(error @ PollControlError::Remote(PollsError::BadRequest(_))) => {
                    // PATCH の直前に時間切れや Twitch 側の操作で終了した場合は最終値を使う。
                    match self.get(poll).await {
                        Ok(remote) if remote.status != RemoteStatus::Active => remote.votes,
                        _ => return Err(error),
                    }
                }
                Err(error) => return Err(error),
            }
        };
        self.poll.apply_external(&poll.local_id, &votes, true);
        *active = None;
        Ok(self.snapshot())
    }

    pub(crate) async fn reset(&self) -> Result<PollSnapshot, PollControlError> {
        let mut active = self.active.lock().await;
        if let Some(poll) = active.as_ref() {
            if !poll.terminated && self.get(poll).await?.status == RemoteStatus::Active {
                self.end(poll, true).await?;
            }
        }
        *active = None;
        Ok(self.poll.reset())
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
        loop {
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = ticks.tick() => {}
            }
            let Some(mut active) = cancel.run_until_cancelled(self.active.lock()).await else {
                return;
            };
            let Some(poll) = active.as_ref().filter(|poll| poll.local_id == local_id) else {
                return;
            };
            // refresh token の回転・保存は途中で打ち切らず、終了時には結果の反映だけ止める。
            let result = self.get(poll).await;
            if cancel.is_cancelled() {
                return;
            }
            match result {
                Ok(remote) => {
                    let closed = poll.terminated || remote.status != RemoteStatus::Active;
                    if !self.poll.apply_external(&local_id, &remote.votes, closed) || closed {
                        *active = None;
                        return;
                    }
                }
                Err(PollControlError::Remote(PollsError::RateLimited { retry_after_secs })) => {
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
                Err(_) => {}
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
        self.auth
            .with_token_retry(
                client_id,
                client_secret,
                POLL_SCOPES,
                |error| matches!(error, PollControlError::Remote(PollsError::Unauthorized)),
                |access| {
                    let allowed = broadcaster_id.is_none_or(|id| id == access.user_id);
                    let helix = HelixPolls::with_client(
                        HELIX_BASE,
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

    async fn get(&self, poll: &ActivePoll) -> Result<RemotePoll, PollControlError> {
        self.with_access(
            &self.credentials(),
            Some(&poll.broadcaster_id),
            |access, helix| async move {
                Ok(helix
                    .get(&access.token, &poll.broadcaster_id, &poll.remote_id)
                    .await?)
            },
        )
        .await
    }

    async fn end(&self, poll: &ActivePoll, archive: bool) -> Result<(), PollControlError> {
        self.with_access(
            &self.credentials(),
            Some(&poll.broadcaster_id),
            |access, helix| async move {
                Ok(helix
                    .end(
                        &access.token,
                        &poll.broadcaster_id,
                        &poll.remote_id,
                        archive,
                    )
                    .await?)
            },
        )
        .await
    }
}
