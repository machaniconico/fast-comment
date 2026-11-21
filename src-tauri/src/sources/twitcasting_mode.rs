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
    // auto pass the active movie and carry-over IDs to the official source on handoff.
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
        let mut notice_sent = false;
        let mut refresh_blocked_notice = false;

        {
            let run = unofficial.run(tx.clone(), unofficial_cancel.clone());
            tokio::pin!(run);
            loop {
                let auth_state = *auth.borrow_and_update();
                if counter.reached() && auth_state == AuthState::Ready {
                    unofficial_cancel.cancel();
                    break;
                }
                let blocked = counter.reached() && auth_state != AuthState::Loading;
                refresh_blocked_notice &= blocked;
                let publish_notice = (blocked != notice_sent || refresh_blocked_notice)
                    && self.metadata_tx.as_ref().is_some_and(|tx| !tx.is_closed());

                tokio::select! {
                    biased;
                    Some(signal) = signals.recv() => {
                        let was_reached = counter.reached();
                        counter.observe(signal);
                        if !was_reached && counter.reached() {
                            tracing::info!(channel = %self.channel, "ツイキャス非公式経路の切替閾値に到達");
                        }
                        refresh_blocked_notice |= matches!(signal, UnofficialSignal::Unreadable);
                    }
                    changed = auth.changed(), if auth_open => auth_open = changed.is_ok(),
                    permit = reserve_notice(&self.metadata_tx), if publish_notice => {
                        if let Some(permit) = permit {
                            permit.send(self.notice_update(blocked));
                            notice_sent = blocked;
                        }
                        refresh_blocked_notice = false;
                    }
                    result = &mut run => return result,
                    message = messages.recv() => {
                        match message {
                            Ok(message) => self.remember_id(message, &mut recent_ids),
                            Err(broadcast::error::RecvError::Lagged(count)) => {
                                tracing::debug!(channel = %self.channel, count, "ツイキャスの切替用コメント受信に遅延");
                            }
                            Err(broadcast::error::RecvError::Closed) => unreachable!("the sender is held by run_auto"),
                        }
                    }
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
        drop(messages);

        if notice_sent {
            if let Some(permit) = reserve_notice(&self.metadata_tx).await {
                permit.send(self.notice_update(false));
            }
        }

        tracing::info!(channel = %self.channel, "ツイキャス公式経路に切り替え");
        let ids: HashSet<_> = recent_ids.into_iter().collect();
        routes
            .official
            .with_initial_movie(unofficial.take_live_movie())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{integrations::MemorySecretStore, model::LiveEvent};
    use crate::sources::twitcasting::{TwitcastingOverrides, UnofficialTuning};
    use crate::sources::twitcasting_api::TwitcastingApi;
    use crate::sources::twitcasting_auth::{
        AuthOptions, TwitcastingAuth, TwitcastingEndpoints, TOKEN_KEY,
    };
    use crate::sources::twitcasting_official::PollTuning;
    use axum::{
        http::StatusCode,
        response::Html,
        routing::{get, post},
        Json, Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    };
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::{net::TcpListener, sync::Notify, time::timeout};

    async fn logged_in_api(
        endpoints: TwitcastingEndpoints,
    ) -> Arc<TwitcastingApi<MemorySecretStore>> {
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let store = MemorySecretStore::new();
        store
            .put(
                TOKEN_KEY,
                &serde_json::json!({
                    "clientId": "test-client", "accessToken": "test-token", "userId": "1",
                    "screenId": "alice", "name": "Alice", "obtainedAtMs": now_ms
                })
                .to_string(),
            )
            .await
            .unwrap();
        let credentials = TwitcastingAuth::new(store, endpoints.clone(), AuthOptions::default());
        credentials.load(Some("test-client"), now_ms).await;
        assert_eq!(*credentials.subscribe().borrow(), AuthState::Ready);
        TwitcastingApi::new(
            credentials,
            endpoints,
            Arc::new(|| Some("test-client".into())),
        )
    }

    #[tokio::test]
    async fn loading_then_ready_switches_without_blocked_notice() {
        check_loading_handoff(false).await;
    }

    #[tokio::test]
    async fn loading_then_logged_out_shows_notice_and_ready_clears_it_before_official_metadata() {
        check_loading_handoff(true).await;
    }

    #[tokio::test]
    async fn auto_handoff_ends_unofficial_movie_once_when_official_is_offline() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let pages = Arc::new(AtomicU32::new(0));
        let router = Router::new()
            .route(
                "/alice",
                get(move || {
                    let page = pages.fetch_add(1, Ordering::Relaxed);
                    async move {
                        if page == 0 {
                            Html(r#"<body data-is-onlive="true" data-movie-id="42"></body>"#)
                        } else {
                            Html("<html><body>maintenance</body></html>")
                        }
                    }
                }),
            )
            .route(
                "/eventpubsub",
                post(move || async move {
                    // The unavailable WS triggers a reconnect and the unreadable HTML.
                    Json(json!({ "url": format!("ws://{addr}/ws") }))
                }),
            )
            .route(
                "/users/alice",
                get(|| async { Json(json!({ "user": { "id": "1" } })) }),
            )
            .route(
                "/users/1/current_live",
                get(|| async { StatusCode::NOT_FOUND }),
            );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let api_base = format!("http://{addr}");
        let api = logged_in_api(TwitcastingEndpoints {
            api_base: api_base.clone(),
            ..Default::default()
        })
        .await;
        let (_auth, auth_rx) = watch::channel(AuthState::Ready);
        let (live_tx, mut events) = mpsc::channel(8);
        let unofficial = TwitcastingSource::new(
            "alice".into(),
            TwitcastingOverrides {
                endpoints: [
                    ("userPageBaseUrl".into(), format!("{api_base}/")),
                    ("commentHistoryUrl".into(), format!("{api_base}/")),
                    ("eventPubsubUrl".into(), format!("{api_base}/eventpubsub")),
                ]
                .into(),
                ..Default::default()
            },
            None,
        )
        .with_tuning(UnofficialTuning {
            unreadable_recheck: Duration::from_millis(10),
            jitter: false,
            ..Default::default()
        })
        .with_live_tx(Some(live_tx.clone()));
        let (metadata_tx, mut metadata) = mpsc::channel(8);
        let official = TwitcastingOfficialSource::new(
            "alice".into(),
            api,
            auth_rx.clone(),
            Some(metadata_tx),
            PollTuning {
                offline: Duration::from_millis(25),
                jitter: false,
                ..Default::default()
            },
        )
        .with_live_tx(Some(live_tx));
        let routed =
            TwitcastingRoutedSource::new(TwitcastingMode::Auto, unofficial, official, auth_rx, None);
        let (bus, _) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let run = tokio::spawn({
            let cancel = cancel.clone();
            async move { routed.run(bus, cancel).await.unwrap() }
        });
        timeout(Duration::from_secs(5), async {
            assert!(matches!(
                events.recv().await,
                Some(LiveEvent::Started(stream)) if stream.stream_id == "42"
            ));
            assert!(matches!(
                events.recv().await,
                Some(LiveEvent::Ended { channel_identifier, .. }) if channel_identifier == "alice"
            ));
            for _ in 0..3 {
                assert_eq!(metadata.recv().await.unwrap().live, Some(false));
            }
            assert!(matches!(
                events.try_recv(),
                Err(mpsc::error::TryRecvError::Empty)
            ));
        })
        .await
        .unwrap();
        cancel.cancel();
        run.await.unwrap();
        server.abort();
    }

    async fn check_loading_handoff(resolve_logged_out: bool) {
        let unofficial_waiting = Arc::new(Notify::new());
        let official_waiting = Arc::new(Notify::new());
        let router = Router::new()
            .route(
                "/alice",
                get({
                    let waiting = unofficial_waiting.clone();
                    let requests = Arc::new(AtomicU32::new(0));
                    move || {
                        let waiting = waiting.clone();
                        let requests = requests.clone();
                        async move {
                            // Park the next request so no unofficial signal can follow the auth change.
                            if requests.fetch_add(1, Ordering::Relaxed)
                                >= UNREADABLE_SWITCH_THRESHOLD
                            {
                                waiting.notify_one();
                                pending::<()>().await;
                            }
                            Html("<html><body>maintenance</body></html>")
                        }
                    }
                }),
            )
            .route(
                "/users/alice",
                get({
                    let waiting = official_waiting.clone();
                    move || async move {
                        waiting.notify_one();
                        // Keep the official source from publishing metadata during the assertion.
                        pending::<()>().await;
                    }
                }),
            );
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_base = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let endpoints = TwitcastingEndpoints {
            api_base: api_base.clone(),
            ..Default::default()
        };
        let api = logged_in_api(endpoints).await;

        let (auth, auth_rx) = watch::channel(AuthState::Loading);
        let (metadata_tx, mut metadata) = mpsc::channel(8);
        let unofficial = TwitcastingSource::new(
            "alice".into(),
            TwitcastingOverrides {
                endpoints: [("userPageBaseUrl".into(), format!("{api_base}/"))].into(),
                ..Default::default()
            },
            None,
        )
        .with_tuning(UnofficialTuning {
            unreadable_recheck: Duration::from_millis(10),
            jitter: false,
            ..Default::default()
        });
        let official = TwitcastingOfficialSource::new(
            "alice".into(),
            api,
            auth_rx.clone(),
            Some(metadata_tx.clone()),
            Default::default(),
        );
        let routed = TwitcastingRoutedSource::new(
            TwitcastingMode::Auto,
            unofficial,
            official,
            auth_rx,
            Some(metadata_tx),
        );
        let (bus, _) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let run = tokio::spawn({
            let (bus, cancel) = (bus.clone(), cancel.clone());
            async move { routed.run(bus, cancel).await.unwrap() }
        });
        let deadline = Duration::from_secs(5);
        timeout(deadline, unofficial_waiting.notified())
            .await
            .unwrap();
        assert_eq!(bus.receiver_count(), 1);
        assert!(matches!(
            metadata.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));

        if resolve_logged_out {
            auth.send(AuthState::LoggedOut).unwrap();
            let blocked = timeout(deadline, metadata.recv()).await.unwrap().unwrap();
            assert_eq!(blocked.notice.as_deref(), Some(UNOFFICIAL_BLOCKED_NOTICE));
        }
        auth.send(AuthState::Ready).unwrap();
        timeout(deadline, official_waiting.notified())
            .await
            .unwrap();
        assert_eq!(bus.receiver_count(), 0);
        if resolve_logged_out {
            let cleared = metadata.try_recv().unwrap();
            assert_eq!(cleared.notice.as_deref(), Some(""));
            assert_eq!(cleared.live, None);
        }
        assert!(matches!(
            metadata.try_recv(),
            Err(mpsc::error::TryRecvError::Empty)
        ));
        cancel.cancel();
        timeout(deadline, run).await.unwrap().unwrap();
        server.abort();
    }
}
