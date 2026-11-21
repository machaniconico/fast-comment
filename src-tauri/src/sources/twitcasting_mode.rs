//! ツイキャスの接続経路を選び、auto の経路切替を管理する。

use std::collections::{HashSet, VecDeque};
use std::fmt;
use std::future::{pending, Future};
use std::pin::Pin;
use std::sync::Mutex;

use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use super::twitcasting::{StartedMovies, TwitcastingSource, UnofficialSignal};
use super::twitcasting_auth::AuthState;
use super::twitcasting_official::TwitcastingOfficialSource;
use super::Source;
use crate::config::TwitcastingMode;
use crate::integrations::SecretStore;
use crate::model::{ChatMessage, Platform};
use crate::stats::YoutubeMetadataUpdate;

pub const UNREADABLE_SWITCH_THRESHOLD: u32 = 3;
pub const CARRY_OVER_IDS: usize = 256;
pub const UNOFFICIAL_BLOCKED_NOTICE: &str =
    "非公式経路が使えません。ログインすると公式 API で取得できます";

#[derive(Default)]
pub struct UnreadableCounter(u32);

impl UnreadableCounter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe(&mut self, signal: UnofficialSignal) -> u32 {
        match signal {
            UnofficialSignal::Unreadable => self.0 = self.0.saturating_add(1),
            UnofficialSignal::Healthy => self.0 = 0,
            UnofficialSignal::Neutral => {}
        }
        self.0
    }

    pub fn reached(&self) -> bool {
        self.0 >= UNREADABLE_SWITCH_THRESHOLD
    }
}

struct Routes<S: SecretStore> {
    unofficial: TwitcastingSource,
    official: TwitcastingOfficialSource<S>,
}

/// One connection lifetime. Create a new source to reconnect with a fresh route.
pub struct TwitcastingRoutedSource<S: SecretStore> {
    mode: TwitcastingMode,
    channel: String,
    // A SourceManager task owns one connection lifetime. Taking the routes lets
    // auto pass the final carry-over IDs to the official source on handoff.
    routes: Mutex<Option<Routes<S>>>,
    auth_state: watch::Receiver<AuthState>,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl<S: SecretStore> fmt::Debug for TwitcastingRoutedSource<S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingRoutedSource")
            .field("channel", &self.channel)
            .field("mode", &self.mode)
            .finish_non_exhaustive()
    }
}

impl<S: SecretStore> TwitcastingRoutedSource<S> {
    pub fn new(
        mode: TwitcastingMode,
        unofficial: TwitcastingSource,
        official: TwitcastingOfficialSource<S>,
        auth_state: watch::Receiver<AuthState>,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        Self::new_with_started_movies(
            mode,
            unofficial,
            official,
            auth_state,
            metadata_tx,
            StartedMovies::default(),
        )
    }

    pub(crate) fn new_with_started_movies(
        mode: TwitcastingMode,
        unofficial: TwitcastingSource,
        official: TwitcastingOfficialSource<S>,
        auth_state: watch::Receiver<AuthState>,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
        started_movies: StartedMovies,
    ) -> Self {
        let name = unofficial.name();
        let channel = name
            .strip_prefix("twitcasting:")
            .unwrap_or(&name)
            .to_owned();
        Self {
            mode,
            channel,
            routes: Mutex::new(Some(Routes {
                unofficial: unofficial.with_started_movies(started_movies.clone()),
                official: official.with_started_movies(started_movies),
            })),
            auth_state,
            metadata_tx,
        }
    }

    async fn run_auto(
        &self,
        routes: Routes<S>,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> anyhow::Result<()> {
        let (signal_tx, mut signals) = mpsc::unbounded_channel();
        let unofficial = routes.unofficial.with_signal_tx(signal_tx);
        let unofficial_cancel = cancel.child_token();
        // Send directly to the application bus. A second broadcast queue would
        // drop part of a frame that does not fit before this task can drain it.
        let mut messages = tx.subscribe();
        let mut recent_ids = VecDeque::with_capacity(CARRY_OVER_IDS);
        let mut counter = UnreadableCounter::new();
        let mut auth = self.auth_state.clone();
        let mut auth_open = true;
        let mut notice_pending = None;
        let mut notice_sent = false;

        {
            let run = unofficial.run(tx.clone(), unofficial_cancel.clone());
            tokio::pin!(run);
            loop {
                tokio::select! {
                    biased;
                    Some(signal) = signals.recv() => {
                        let was_reached = counter.reached();
                        counter.observe(signal);
                        if !was_reached && counter.reached() {
                            tracing::info!(channel = %self.channel, "ツイキャス非公式経路の切替閾値に到達");
                        }
                        if self.metadata_tx.is_some() {
                            match signal {
                                UnofficialSignal::Unreadable if counter.reached() => notice_pending = Some(true),
                                UnofficialSignal::Healthy => notice_pending = notice_sent.then_some(false),
                                _ => {}
                            }
                        }
                    }
                    changed = auth.changed(), if auth_open => auth_open = changed.is_ok(),
                    permit = reserve_notice(&self.metadata_tx), if notice_pending.is_some() => {
                        if let (Some(permit), Some(blocked)) = (permit, notice_pending.take()) {
                            permit.send(self.notice_update(blocked));
                            notice_sent = blocked;
                        }
                    }
                    result = &mut run => return result,
                    message = messages.recv() => {
                        if let Ok(message) = message {
                            self.remember_id(message, &mut recent_ids);
                        }
                    }
                }

                if counter.reached() && *auth.borrow() == AuthState::Ready {
                    unofficial_cancel.cancel();
                    break;
                }
            }
        }
        // Capture the unread tail of the bus before starting the official route.
        // Bound the drain so other sources cannot keep the handoff waiting.
        for _ in 0..=messages.len().min(tx.len()) {
            match messages.try_recv() {
                Ok(message) => self.remember_id(message, &mut recent_ids),
                Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
                Err(_) => break,
            }
        }

        tracing::info!(channel = %self.channel, "ツイキャス公式経路に切り替え");
        let ids: HashSet<_> = recent_ids.into_iter().collect();
        routes
            .official
            .with_carry_over_ids(ids)
            .run(tx, cancel)
            .await
    }

    fn notice_update(&self, blocked: bool) -> YoutubeMetadataUpdate {
        YoutubeMetadataUpdate {
            platform: Platform::Twitcasting,
            channel: self.channel.clone(),
            // Offline HTML and valid WS arrays both yield Healthy. The
            // unofficial source already reports the actual live state.
            notice: Some(
                if blocked {
                    UNOFFICIAL_BLOCKED_NOTICE
                } else {
                    ""
                }
                .into(),
            ),
            full_snapshot: true,
            ..Default::default()
        }
    }

    fn remember_id(&self, message: ChatMessage, recent_ids: &mut VecDeque<String>) {
        if message.platform == Platform::Twitcasting
            && message.channel == self.channel
            && !message.is_injected_test_comment()
        {
            if recent_ids.len() == CARRY_OVER_IDS {
                recent_ids.pop_front();
            }
            recent_ids.push_back(message.id);
        }
    }
}

async fn reserve_notice(
    tx: &Option<mpsc::Sender<YoutubeMetadataUpdate>>,
) -> Option<mpsc::Permit<'_, YoutubeMetadataUpdate>> {
    match tx {
        Some(tx) => tx.reserve().await.ok(),
        None => pending().await,
    }
}

impl<S: SecretStore> Source for TwitcastingRoutedSource<S> {
    fn name(&self) -> String {
        format!("twitcasting:{}", self.channel)
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let run = async {
                let routes = self.routes.lock().unwrap_or_else(|e| e.into_inner()).take();
                let Some(routes) = routes else {
                    anyhow::bail!("ツイキャスの接続は起動済みです");
                };
                match self.mode {
                    TwitcastingMode::Auto => self.run_auto(routes, tx, cancel.clone()).await,
                    TwitcastingMode::Unofficial => routes.unofficial.run(tx, cancel.clone()).await,
                    TwitcastingMode::Official => routes.official.run(tx, cancel.clone()).await,
                }
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Ok(()),
                result = run => result,
            }
        })
    }
}
