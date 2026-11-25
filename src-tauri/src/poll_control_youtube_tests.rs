use super::*;
use crate::{
    config::ChannelConfig,
    poll::{PollDisplay, PollSource, PollStart, PollStatus, PreferenceChange},
    poll_control::PollControl,
    sources::twitch_auth::TwitchAuth,
};
use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::{json, Value};
use std::{collections::VecDeque, io, sync::Mutex as StdMutex};
use tokio::sync::Notify;

const CLIENT_ID: &str = "test-oauth-client";
const TOKEN: &str = "test-access-token";
const CHAT: &str = "test-live-chat";
const ID: &str = "message-1";
const STALE_TOKEN: &str = "test-stale-token";

fn request() -> PollStart {
    PollStart {
        source: PollSource::Youtube,
        question: " どちら？ ".into(),
        options: vec![" A ".into(), "B".into()],
        duration_secs: None,
        display: PollDisplay::BarH,
        show_percent: true,
        show_counts: true,
    }
}

fn item(id: &str, status: &str) -> Value {
    json!({
        "id": id,
        "snippet": {
            "publishedAt": chrono::Utc::now().to_rfc3339(),
            "pollDetails": {"metadata": {
                "status": status,
                "questionText": " どちら？ ",
                "options": [{"optionText": " A ", "tally": "7"}, {"optionText": "B", "tally": "3"}]
            }}
        }
    })
}

fn reply(status: StatusCode, reason: &str, message: &str) -> Response {
    (
        status,
        Json(json!({"error": {"message": message, "errors": [{"reason": reason}]}})),
    )
        .into_response()
}

fn interrupted() -> Response {
    let chunks = futures_util::stream::unfold(false, |sent| async move {
        if sent {
            time::sleep(Duration::from_millis(30)).await;
            Some((Err(io::Error::other("response interrupted")), true))
        } else {
            Some((Ok(bytes::Bytes::from_static(b"{\"id\":")), true))
        }
    });
    Body::from_stream(chunks).into_response()
}

#[derive(Default)]
struct Mock {
    broadcasts: StdMutex<VecDeque<Response>>,
    create: StdMutex<VecDeque<Response>>,
    active: StdMutex<VecDeque<Response>>,
    transition: StdMutex<VecDeque<Response>>,
    calls: StdMutex<Vec<&'static str>>,
    hold_get: Notify,
    get_entered: Notify,
    transition_entered: Notify,
    get_times: StdMutex<Vec<Instant>>,
    get_queries: StdMutex<Vec<String>>,
    transition_lengths: StdMutex<Vec<Option<String>>>,
    // token の応答で順に払い出す値。空なら TOKEN。
    issued: StdMutex<VecDeque<&'static str>>,
    // 失効済みとして扱い、GET /liveChat/messages で 401 を返す token。
    stale: StdMutex<Option<&'static str>>,
}

impl Mock {
    async fn handle(&self, request: Request) -> Response {
        let route = match (request.method(), request.uri().path()) {
            (&Method::POST, "/oauth/token") => "token",
            (&Method::GET, "/liveBroadcasts") => "broadcasts",
            (&Method::POST, "/liveChat/messages") => "create",
            (&Method::GET, "/liveChat/messages") => "active",
            (&Method::POST, "/liveChat/messages/transition") => "transition",
            _ => panic!("unexpected request"),
        };
        self.calls.lock().unwrap().push(route);
        if route != "token" {
            let header = request.headers()["authorization"].to_str().unwrap();
            let stale = *self.stale.lock().unwrap();
            if stale.is_some_and(|stale| header == format!("Bearer {stale}")) {
                if route == "active" {
                    return reply(StatusCode::UNAUTHORIZED, "authError", "expired");
                }
            } else {
                assert_eq!(header, format!("Bearer {TOKEN}"));
            }
        }
        match route {
            "token" => {
                let token = self.issued.lock().unwrap().pop_front().unwrap_or(TOKEN);
                Json(json!({"access_token": token, "token_type": "Bearer", "expires_in": 3600}))
                    .into_response()
            }
            "broadcasts" => self
                .broadcasts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| {
                    Json(json!({"items": [{"snippet": {"liveChatId": CHAT}}]})).into_response()
                }),
            "create" => self
                .create
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Json(item(ID, "active")).into_response()),
            "active" => {
                self.get_times.lock().unwrap().push(Instant::now());
                self.get_queries
                    .lock()
                    .unwrap()
                    .push(request.uri().query().unwrap_or_default().to_owned());
                let reply = self.active.lock().unwrap().pop_front();
                if let Some(reply) = reply {
                    return reply;
                }
                self.get_entered.notify_one();
                self.hold_get.notified().await;
                Json(json!({"activePollItem": item(ID, "active")})).into_response()
            }
            "transition" => {
                self.transition_lengths.lock().unwrap().push(
                    request
                        .headers()
                        .get("content-length")
                        .map(|value| value.to_str().unwrap().to_owned()),
                );
                assert!(axum::body::to_bytes(request.into_body(), 1)
                    .await
                    .unwrap()
                    .is_empty());
                self.transition_entered.notify_one();
                self.transition
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(|| Json(item(ID, "closed")).into_response())
            }
            _ => unreachable!(),
        }
    }

    fn calls(&self) -> Vec<&'static str> {
        self.calls.lock().unwrap().clone()
    }

    fn push_active(&self, item: Value) {
        self.active
            .lock()
            .unwrap()
            .push_back(Json(json!({"activePollItem": item})).into_response());
    }
}

struct Fixture {
    control: Arc<PollControl>,
    mock: Arc<Mock>,
    poll: Arc<Poll>,
    config: watch::Sender<AppConfig>,
    saved: Arc<StdMutex<usize>>,
    // 設定の保存にかける時間。保存が遅い時の締め切りを確かめる。
    save_delay: Arc<StdMutex<Duration>>,
    notices: tokio::sync::mpsc::UnboundedReceiver<String>,
    cancel: CancellationToken,
    server: tokio::task::JoinHandle<()>,
}

impl Fixture {
    async fn new() -> Self {
        let mock = Arc::new(Mock::default());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handler = mock.clone();
        let server = tokio::spawn(async move {
            axum::serve(
                listener,
                Router::new().fallback(move |request| {
                    let handler = handler.clone();
                    async move { handler.handle(request).await }
                }),
            )
            .await
            .unwrap();
        });
        let mut config = AppConfig::default();
        config.credentials.youtube_oauth_client_id = CLIENT_ID.into();
        config.channels.push(ChannelConfig {
            platform: ChannelPlatform::Youtube,
            enabled: true,
            own_stream: true,
            identifier: "channel".into(),
            overrides: Default::default(),
        });
        let (config, rx) = watch::channel(config);
        let saved = Arc::new(StdMutex::new(0));
        let captured = saved.clone();
        let save_delay = Arc::new(StdMutex::new(Duration::ZERO));
        let delay = save_delay.clone();
        let poll = Arc::new(Poll::new(
            move |change| {
                if matches!(change, PreferenceChange::Started(_)) {
                    *captured.lock().unwrap() += 1;
                    std::thread::sleep(*delay.lock().unwrap());
                }
                Ok(())
            },
            |_| {},
        ));
        let (notice_tx, notices) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let mut control = PollControl::with_youtube_notice(
            poll.clone(),
            Arc::new(TwitchAuth::new().unwrap()),
            rx,
            cancel.clone(),
            move |message| {
                let _ = notice_tx.send(message.to_owned());
            },
        );
        let youtube = Arc::get_mut(&mut control.youtube).unwrap();
        youtube.auth = Arc::new(YoutubeAuth::for_poll_test(CLIENT_ID, &base));
        youtube.api = Arc::new(YoutubePolls::new(&base));
        Self {
            control: Arc::new(control),
            mock,
            poll,
            config,
            saved,
            save_delay,
            notices,
            cancel,
            server,
        }
    }

    async fn start(&self) -> Result<PollSnapshot, PollControlError> {
        self.control.start(request().validate().unwrap()).await
    }

    async fn seed_votes(&self) -> PollSnapshot {
        let started = self.start().await.unwrap();
        assert!(self.poll.apply_external(&started.id, &[4, 2], false));
        self.poll.snapshot()
    }

    async fn closed(&self) -> PollSnapshot {
        self.closed_within(Duration::from_secs(15)).await
    }

    async fn closed_within(&self, timeout: Duration) -> PollSnapshot {
        let mut state = self.poll.sender().subscribe();
        let closed = time::timeout(
            timeout,
            state.wait_for(|state| state.status == PollStatus::Closed),
        )
        .await
        .unwrap()
        .unwrap();
        closed.clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.server.abort();
    }
}

#[tokio::test]
async fn reconciles_interrupted_create_without_resending_post() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::BAD_GATEWAY,
        "backendError",
        "retry",
    ));
    fixture.mock.push_active(item(ID, "active"));

    assert_eq!(fixture.start().await.unwrap().status, PollStatus::Open);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "active"]
    );
    assert_eq!(*fixture.saved.lock().unwrap(), 1);
}

#[tokio::test]
async fn stale_match_does_not_open_or_save_preferences() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    let mut candidate = item(ID, "active");
    candidate["snippet"]["publishedAt"] =
        json!((chrono::Utc::now() - chrono::Duration::seconds(11)).to_rfc3339());
    fixture.mock.push_active(candidate);

    assert!(matches!(
        fixture.start().await,
        Err(PollControlError::Youtube(
            YoutubeControlError::CreateUnconfirmed
        ))
    ));
    assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active"]
    );
    assert_eq!(*fixture.saved.lock().unwrap(), 0);
}

#[tokio::test]
async fn unauthorized_create_is_never_retried() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(reply(
        StatusCode::UNAUTHORIZED,
        "authError",
        "expired",
    ));
    assert!(matches!(
        fixture.start().await,
        Err(PollControlError::Youtube(YoutubeControlError::Api(
            YtPollsError::Unauthorized
        )))
    ));
    assert_eq!(fixture.mock.calls(), ["token", "broadcasts", "create"]);
    assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);
    assert_eq!(*fixture.saved.lock().unwrap(), 0);
}

#[tokio::test]
async fn checks_own_rows_before_oauth_and_requires_exactly_one_broadcast() {
    let fixture = Fixture::new().await;
    fixture.config.send_modify(|config| {
        config.credentials.youtube_oauth_client_id.clear();
        config.channels[0].enabled = false;
    });
    assert!(fixture
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("設定してください"));
    fixture.config.send_modify(|config| {
        config.channels[0].enabled = true;
        config.channels.push(config.channels[0].clone());
    });
    assert!(fixture
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("チャンネルは1件"));
    fixture.config.send_modify(|config| {
        config.channels.pop();
    });
    assert!(fixture
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("Googleアカウントへ接続"));
    assert!(fixture.mock.calls().is_empty());

    fixture
        .config
        .send_modify(|config| config.credentials.youtube_oauth_client_id = CLIENT_ID.into());
    fixture.mock.broadcasts.lock().unwrap().extend([
        Json(json!({"items": []})).into_response(),
        Json(json!({"items": [{"snippet": {"liveChatId": CHAT}}, {}]})).into_response(),
    ]);
    assert!(fixture
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("見つかりません"));
    assert!(fixture
        .start()
        .await
        .unwrap_err()
        .to_string()
        .contains("YouTube Liveを1件"));
    assert_eq!(fixture.mock.calls(), ["token", "broadcasts", "broadcasts"]);
    assert_eq!(*fixture.saved.lock().unwrap(), 0);
}

#[tokio::test]
async fn poller_closes_on_404_and_keeps_previous_votes() {
    let mut fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::NOT_FOUND,
        "liveChatNotFound",
        "ended",
    ));
    fixture.seed_votes().await;
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert!(fixture.notices.try_recv().is_err());
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active"]
    );
}

#[tokio::test]
async fn poller_closes_on_different_poll_and_keeps_previous_votes() {
    let fixture = Fixture::new().await;
    fixture.mock.push_active(item("different", "active"));
    fixture.seed_votes().await;
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active"]
    );
}

#[tokio::test]
async fn quota_stops_polling_and_notifies_without_sensitive_details() {
    let mut fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::FORBIDDEN,
        "quotaExceeded",
        &format!("{TOKEN} https://example.test/?liveChatId={CHAT}"),
    ));
    fixture.seed_votes().await;
    let notice = time::timeout(Duration::from_secs(15), fixture.notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notice.contains("日次上限は翌日"));
    assert!(!notice.contains(CHAT) && !notice.contains(TOKEN) && !notice.contains('?'));
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert!(fixture.control.youtube.active.lock().await.is_none());
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active"]
    );
}

#[tokio::test]
async fn poller_closes_with_notice_if_tallies_are_missing() {
    let mut fixture = Fixture::new().await;
    let mut without_tally = item(ID, "active");
    without_tally["snippet"]["pollDetails"]["metadata"]["options"][0] = json!({"optionText": "A"});
    fixture.mock.push_active(without_tally);
    fixture.seed_votes().await;
    let notice = time::timeout(Duration::from_secs(15), fixture.notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notice.contains("所有者"));
    // transition で閉じられたので、手動終了の案内は出さない。
    assert!(notice.contains("YouTube 側でも終了しました"), "{notice}");
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "transition"]
    );
}

#[tokio::test]
async fn failed_stop_closes_locally_and_failed_reset_goes_idle() {
    for reset in [false, true] {
        let fixture = Fixture::new().await;
        fixture.mock.transition.lock().unwrap().push_back(reply(
            StatusCode::BAD_GATEWAY,
            "backendError",
            "failed",
        ));
        fixture.seed_votes().await;
        let result = if reset {
            fixture.control.reset().await
        } else {
            fixture.control.stop().await
        };
        assert!(matches!(
            result,
            Err(PollControlError::Youtube(YoutubeControlError::Api(
                YtPollsError::Api(502)
            )))
        ));
        let snapshot = fixture.control.snapshot();
        assert_eq!(
            snapshot.status,
            if reset {
                PollStatus::Idle
            } else {
                PollStatus::Closed
            }
        );
        assert_eq!(snapshot.total_voters, if reset { 0 } else { 6 });
        fixture.control.reset().await.unwrap();
        assert_eq!(
            fixture.mock.calls(),
            ["token", "broadcasts", "create", "transition"]
        );
    }
}

#[tokio::test]
async fn stop_can_transition_while_active_get_is_pending() {
    let fixture = Fixture::new().await;
    fixture.seed_votes().await;
    time::timeout(Duration::from_secs(15), fixture.mock.get_entered.notified())
        .await
        .unwrap();
    let stopped = time::timeout(Duration::from_secs(2), fixture.control.stop())
        .await
        .unwrap()
        .unwrap();
    fixture.mock.hold_get.notify_one();
    assert_eq!(stopped.status, PollStatus::Closed);
    assert_eq!(stopped.total_voters, 10);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "transition"]
    );
}

#[tokio::test]
async fn deadline_transitions_and_uses_final_tallies() {
    let fixture = Fixture::new().await;
    let mut start = request();
    start.duration_secs = Some(10);
    let opened = fixture
        .control
        .start(start.validate().unwrap())
        .await
        .unwrap();
    assert_eq!(opened.ends_at_ms, Some(opened.started_at_ms + 10_000));
    assert_eq!(fixture.closed().await.total_voters, 10);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "transition"]
    );
}

#[tokio::test]
async fn bad_request_uses_reason_when_api_message_echoes_sensitive_data() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(reply(
        StatusCode::BAD_REQUEST,
        "invalidPollOption",
        &format!("{TOKEN} https://example.test/?liveChatId={CHAT}"),
    ));
    let error = fixture.start().await.unwrap_err();
    assert_eq!(error.to_string(), "invalidPollOption");
    for secret in [TOKEN, CHAT, "example.test"] {
        assert!(!format!("{error:?}").contains(secret));
    }
    assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);
}

#[tokio::test]
async fn transition_sends_zero_content_length() {
    let fixture = Fixture::new().await;
    fixture.start().await.unwrap();
    assert_eq!(
        fixture.control.stop().await.unwrap().status,
        PollStatus::Closed
    );
    assert_eq!(
        *fixture.mock.transition_lengths.lock().unwrap(),
        [Some("0".into())]
    );
}

#[tokio::test]
async fn active_uses_minimum_page_size() {
    let fixture = Fixture::new().await;
    fixture.mock.push_active(item(ID, "active"));
    fixture
        .control
        .youtube
        .api
        .active(TOKEN, CHAT)
        .await
        .unwrap();
    let queries = fixture.mock.get_queries.lock().unwrap();
    assert!(url::form_urlencoded::parse(queries[0].as_bytes())
        .any(|(key, value)| key == "maxResults" && value == "200"));
}

#[tokio::test]
async fn accepts_unsigned_integer_tallies_and_rejects_invalid_values() {
    let fixture = Fixture::new().await;
    for (tally, expected) in [
        (json!(12), Some(vec![12, 3])),
        (json!(0), Some(vec![0, 3])),
        (json!(-1), None),
        (json!(1.5), None),
        (json!("1.5"), None),
        (json!("-1"), None),
        (json!("no votes"), None),
    ] {
        let mut sample = item(ID, "active");
        sample["snippet"]["pollDetails"]["metadata"]["options"][0]["tally"] = tally.clone();
        fixture.mock.push_active(sample);
        let state = fixture
            .control
            .youtube
            .api
            .active(TOKEN, CHAT)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(state.tallies, expected, "tally={tally}");
    }
}

fn timed_start() -> ValidatedStart {
    let mut start = request();
    start.duration_secs = Some(10);
    start.validate().unwrap()
}

/// 作成の応答を `delay` だけ遅らせる。publishedAt は `published` にする。
fn push_create(fixture: &Fixture, published: chrono::DateTime<chrono::Utc>, delay: Duration) {
    let mut created = item(ID, "active");
    created["snippet"]["publishedAt"] = json!(published.to_rfc3339());
    let body = serde_json::to_vec(&created).unwrap();
    let chunks = futures_util::stream::once(async move {
        time::sleep(delay).await;
        Ok::<_, io::Error>(bytes::Bytes::from(body))
    });
    fixture
        .mock
        .create
        .lock()
        .unwrap()
        .push_back(Body::from_stream(chunks).into_response());
}

#[tokio::test]
async fn deadline_counts_from_create_response_even_if_clock_is_off() {
    // publishedAt が PC の時計より 60 秒前・後でも、締め切りは応答から 10 秒後。
    for skew in [-60, 60] {
        let fixture = Fixture::new().await;
        push_create(
            &fixture,
            chrono::Utc::now() + chrono::Duration::seconds(skew),
            Duration::ZERO,
        );
        let before = Instant::now();
        let before_ms = chrono::Utc::now().timestamp_millis() as u64;
        let opened = fixture.control.start(timed_start()).await.unwrap();
        let after_ms = chrono::Utc::now().timestamp_millis() as u64;
        assert!(
            (before_ms..=after_ms).contains(&opened.started_at_ms),
            "{skew}: {before_ms} {} {after_ms}",
            opened.started_at_ms
        );
        assert_eq!(opened.ends_at_ms, Some(opened.started_at_ms + 10_000));
        fixture.closed_within(Duration::from_secs(12)).await;
        let elapsed = before.elapsed();
        assert!(elapsed >= Duration::from_secs(10), "{skew}: {elapsed:?}");
        assert!(
            elapsed < Duration::from_millis(11_500),
            "{skew}: {elapsed:?}"
        );
    }
}

#[tokio::test]
async fn delayed_create_response_still_gets_full_duration() {
    // PC の時計が 60 秒進んでいて、YouTube の公開が POST から 3 秒遅れ、公開直後に応答が届く。
    let fixture = Fixture::new().await;
    push_create(
        &fixture,
        chrono::Utc::now() - chrono::Duration::seconds(57),
        Duration::from_secs(3),
    );
    let before = Instant::now();
    let opened = fixture.control.start(timed_start()).await.unwrap();
    let responded = Instant::now();
    assert!(responded - before >= Duration::from_secs(3));
    assert_eq!(opened.ends_at_ms, Some(opened.started_at_ms + 10_000));
    fixture.closed_within(Duration::from_secs(12)).await;
    let elapsed = responded.elapsed();
    assert!(elapsed >= Duration::from_millis(9_900), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(11_500), "{elapsed:?}");
}

#[tokio::test]
async fn slow_preference_save_does_not_extend_deadline() {
    let fixture = Fixture::new().await;
    *fixture.save_delay.lock().unwrap() = Duration::from_secs(3);
    let before = Instant::now();
    let before_ms = chrono::Utc::now().timestamp_millis() as u64;
    let opened = fixture.control.start(timed_start()).await.unwrap();
    assert!(before.elapsed() >= Duration::from_secs(3));
    // 表示の起点も保存の前(応答を受けた時点)。
    assert!(
        opened.started_at_ms < before_ms + 1_000,
        "{} {before_ms}",
        opened.started_at_ms
    );
    assert_eq!(opened.ends_at_ms, Some(opened.started_at_ms + 10_000));
    fixture.closed_within(Duration::from_secs(12)).await;
    let elapsed = before.elapsed();
    assert!(elapsed >= Duration::from_secs(10), "{elapsed:?}");
    assert!(elapsed < Duration::from_millis(11_500), "{elapsed:?}");
}

#[tokio::test]
async fn reconciled_poll_uses_its_polling_interval() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    fixture.mock.active.lock().unwrap().push_back(
        Json(json!({"activePollItem": item(ID, "active"), "pollingIntervalMillis": 13_000}))
            .into_response(),
    );
    fixture.mock.push_active(item(ID, "closed"));
    fixture.start().await.unwrap();
    fixture.closed_within(Duration::from_secs(20)).await;
    let times = fixture.mock.get_times.lock().unwrap();
    assert_eq!(times.len(), 2);
    assert!(
        times[1] - times[0] >= Duration::from_millis(12_950),
        "{times:?}"
    );
}

#[tokio::test]
async fn forgetting_a_rejected_token_keeps_a_newer_cached_one() {
    let fixture = Fixture::new().await;
    let auth = &fixture.control.youtube.auth;
    assert_eq!(auth.access_token(CLIENT_ID).await.unwrap(), TOKEN);
    // 401 を受けたのが古いトークンなら、ほかの処理が更新した TOKEN は残す。
    auth.forget_access_token(STALE_TOKEN).await;
    assert_eq!(auth.access_token(CLIENT_ID).await.unwrap(), TOKEN);
    assert_eq!(fixture.mock.calls(), ["token"]);
    auth.forget_access_token(TOKEN).await;
    assert_eq!(auth.access_token(CLIENT_ID).await.unwrap(), TOKEN);
    assert_eq!(fixture.mock.calls(), ["token", "token"]);
}

#[tokio::test]
async fn reconciliation_retries_empty_responses_within_three_gets() {
    for found in [true, false] {
        let fixture = Fixture::new().await;
        fixture.mock.create.lock().unwrap().push_back(interrupted());
        fixture
            .mock
            .active
            .lock()
            .unwrap()
            .extend((0..if found { 2 } else { 3 }).map(|_| Json(json!({})).into_response()));
        if found {
            fixture.mock.push_active(item(ID, "active"));
        }
        let result = fixture.start().await;
        if found {
            assert_eq!(result.unwrap().status, PollStatus::Open);
        } else {
            assert!(matches!(
                result,
                Err(PollControlError::Youtube(
                    YoutubeControlError::CreateUnconfirmed
                ))
            ));
            assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);
        }
        assert_eq!(
            fixture.mock.calls(),
            [
                "token",
                "broadcasts",
                "create",
                "active",
                "active",
                "active"
            ]
        );
    }
}

#[tokio::test]
async fn reconciliation_counts_unauthorized_without_resending_get() {
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    fixture
        .mock
        .active
        .lock()
        .unwrap()
        .extend((0..3).map(|_| reply(StatusCode::UNAUTHORIZED, "authError", "expired")));
    assert!(matches!(
        fixture.start().await,
        Err(PollControlError::Youtube(
            YoutubeControlError::CreateUnconfirmed
        ))
    ));
    assert_eq!(
        fixture.mock.calls(),
        [
            "token",
            "broadcasts",
            "create",
            "active",
            "token",
            "active",
            "token",
            "active"
        ]
    );
}

#[tokio::test]
async fn reconciliation_refreshes_token_after_unauthorized() {
    let fixture = Fixture::new().await;
    fixture.mock.issued.lock().unwrap().push_back(STALE_TOKEN);
    *fixture.mock.stale.lock().unwrap() = Some(STALE_TOKEN);
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    fixture.mock.push_active(item(ID, "active"));
    assert_eq!(fixture.start().await.unwrap().status, PollStatus::Open);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "token", "active"]
    );
}

#[tokio::test]
async fn polling_spaces_gets_even_when_refreshing_an_unauthorized_token() {
    let fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().extend([
        reply(StatusCode::UNAUTHORIZED, "authError", "expired"),
        reply(StatusCode::BAD_GATEWAY, "backendError", "retry"),
    ]);
    fixture.mock.push_active(item(ID, "closed"));
    fixture.seed_votes().await;
    assert_eq!(
        fixture
            .closed_within(Duration::from_secs(35))
            .await
            .total_voters,
        10
    );
    assert_eq!(
        fixture.mock.calls(),
        [
            "token",
            "broadcasts",
            "create",
            "active",
            "token",
            "active",
            "active"
        ]
    );
    let times = fixture.mock.get_times.lock().unwrap();
    for pair in times.windows(2) {
        // HTTP の到着時刻は送信時刻と僅かにずれる。
        assert!(
            pair[1] - pair[0] >= Duration::from_millis(9_950),
            "{times:?}"
        );
    }
}

#[tokio::test]
async fn polling_terminal_errors_attempt_exactly_one_transition_and_keep_votes_on_failure() {
    for (status, reason, expected) in [
        (StatusCode::FORBIDDEN, "forbidden", "権限がありません"),
        (StatusCode::GONE, "gone", "HTTP 410"),
    ] {
        let mut fixture = Fixture::new().await;
        fixture
            .mock
            .active
            .lock()
            .unwrap()
            .push_back(reply(status, reason, "failed"));
        fixture.mock.transition.lock().unwrap().push_back(reply(
            StatusCode::UNAUTHORIZED,
            "authError",
            "expired",
        ));
        fixture.seed_votes().await;
        let notice = time::timeout(Duration::from_secs(15), fixture.notices.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(notice.contains(expected), "{notice}");
        assert!(
            notice.contains("YouTube 側で投票を終了してください"),
            "{notice}"
        );
        assert_eq!(fixture.closed().await.total_voters, 6);
        fixture.control.stop().await.unwrap();
        assert_eq!(
            fixture.mock.calls(),
            ["token", "broadcasts", "create", "active", "transition"]
        );
    }
}

#[tokio::test]
async fn ended_live_chat_has_its_own_error_message() {
    let fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::FORBIDDEN,
        "liveChatEnded",
        "chat has ended",
    ));
    let error = fixture
        .control
        .youtube
        .api
        .active(TOKEN, CHAT)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("配信のチャットが終了しています"),
        "{error}"
    );
}

#[tokio::test]
async fn auto_finish_cannot_stop_a_new_chat_poll() {
    let fixture = Fixture::new().await;
    let old = fixture.seed_votes().await;
    let held = Arc::new(Notify::new());
    let released = held.clone();
    fixture.mock.transition.lock().unwrap().push_back(
        Body::from_stream(futures_util::stream::once(async move {
            released.notified().await;
            Ok::<_, io::Error>(item(ID, "closed").to_string())
        }))
        .into_response(),
    );
    let youtube = fixture.control.youtube.clone();
    let old_id = old.id.clone();
    let expire = tokio::spawn(youtube.expire(old_id, Instant::now(), fixture.cancel.child_token()));
    time::timeout(
        Duration::from_secs(5),
        fixture.mock.transition_entered.notified(),
    )
    .await
    .unwrap();

    assert!(fixture.poll.apply_external(&old.id, &[7, 3], true));
    let mut chat = request();
    chat.source = PollSource::Chat;
    let new = fixture
        .control
        .start(chat.validate().unwrap())
        .await
        .unwrap();
    held.notify_one();
    time::timeout(Duration::from_secs(5), expire)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(fixture.control.snapshot(), new);
    assert_eq!(new.status, PollStatus::Open);
}

#[tokio::test]
async fn terminal_error_reports_when_youtube_also_closed_the_poll() {
    let mut fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::FORBIDDEN,
        "forbidden",
        "failed",
    ));
    fixture.seed_votes().await;
    let notice = time::timeout(Duration::from_secs(15), fixture.notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notice.contains("YouTube 側でも終了しました"), "{notice}");
    assert!(!notice.contains("終了してください"), "{notice}");
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "transition"]
    );
}

#[tokio::test]
async fn rate_limited_poll_keeps_votes_and_waits_for_next_cycle() {
    let mut fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::TOO_MANY_REQUESTS,
        "rateLimitExceeded",
        "slow down",
    ));
    fixture.mock.push_active(item(ID, "closed"));
    let started = fixture.seed_votes().await;
    time::sleep(Duration::from_secs(12)).await;
    let snapshot = fixture.poll.snapshot();
    assert_eq!(
        (snapshot.status, snapshot.total_voters),
        (PollStatus::Open, 6)
    );
    assert_eq!(snapshot.id, started.id);
    assert_eq!(
        fixture
            .closed_within(Duration::from_secs(15))
            .await
            .total_voters,
        10
    );
    assert!(fixture.notices.try_recv().is_err());
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "active"]
    );
}

#[tokio::test]
async fn polling_honors_longer_interval_from_youtube() {
    let fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(
        Json(json!({"activePollItem": item(ID, "active"), "pollingIntervalMillis": 13_000}))
            .into_response(),
    );
    fixture.mock.push_active(item(ID, "closed"));
    fixture.seed_votes().await;
    fixture.closed_within(Duration::from_secs(30)).await;
    let times = fixture.mock.get_times.lock().unwrap();
    assert_eq!(times.len(), 2);
    assert!(
        times[1] - times[0] >= Duration::from_millis(12_950),
        "{times:?}"
    );
}

#[test]
fn polling_interval_from_youtube_is_capped() {
    assert_eq!(next_poll_delay(None), POLL_INTERVAL);
    assert_eq!(next_poll_delay(Some(3_000)), POLL_INTERVAL);
    assert_eq!(next_poll_delay(Some(13_000)), Duration::from_secs(13));
    assert_eq!(next_poll_delay(Some(600_000)), Duration::from_secs(60));
}

#[tokio::test]
async fn adopted_tallies_survive_when_poll_ends_before_first_get() {
    // 照合で A=7・B=3 を採用し、初回の集計 GET の前に配信者が YouTube 側で終了した。
    let fixture = Fixture::new().await;
    fixture.mock.create.lock().unwrap().push_back(interrupted());
    fixture.mock.push_active(item(ID, "active"));
    fixture
        .mock
        .active
        .lock()
        .unwrap()
        .push_back(Json(json!({})).into_response());
    let opened = fixture.start().await.unwrap();
    let votes = |snapshot: &PollSnapshot| -> Vec<u32> {
        snapshot.options.iter().map(|option| option.votes).collect()
    };
    assert_eq!(votes(&opened), [7, 3]);
    let closed = fixture.closed_within(Duration::from_secs(15)).await;
    assert_eq!((votes(&closed), closed.total_voters), (vec![7, 3], 10));
}

#[tokio::test]
async fn create_response_tallies_are_applied_on_open() {
    let fixture = Fixture::new().await;
    let opened = fixture.start().await.unwrap();
    assert_eq!(
        opened
            .options
            .iter()
            .map(|option| option.votes)
            .collect::<Vec<_>>(),
        [7, 3]
    );
}
