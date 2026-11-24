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
            assert_eq!(
                request.headers()["authorization"],
                format!("Bearer {TOKEN}")
            );
        }
        match route {
            "token" => {
                Json(json!({"access_token": TOKEN, "token_type": "Bearer", "expires_in": 3600}))
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
                let reply = self.active.lock().unwrap().pop_front();
                if let Some(reply) = reply {
                    return reply;
                }
                self.get_entered.notify_one();
                self.hold_get.notified().await;
                Json(json!({"activePollItem": item(ID, "active")})).into_response()
            }
            "transition" => self
                .transition
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Json(item(ID, "closed")).into_response()),
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
        let poll = Arc::new(Poll::new(
            move |change| {
                if matches!(change, PreferenceChange::Started(_)) {
                    *captured.lock().unwrap() += 1;
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
        let mut state = self.poll.sender().subscribe();
        let closed = time::timeout(
            Duration::from_secs(15),
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
async fn poller_refreshes_once_and_closes_with_notice_if_tallies_are_missing() {
    let mut fixture = Fixture::new().await;
    fixture.mock.active.lock().unwrap().push_back(reply(
        StatusCode::UNAUTHORIZED,
        "authError",
        "expired",
    ));
    let mut without_tally = item(ID, "active");
    without_tally["snippet"]["pollDetails"]["metadata"]["options"][0] = json!({"optionText": "A"});
    fixture.mock.push_active(without_tally);
    fixture.seed_votes().await;
    let notice = time::timeout(Duration::from_secs(15), fixture.notices.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(notice.contains("所有者"));
    assert_eq!(fixture.closed().await.total_voters, 6);
    assert_eq!(
        fixture.mock.calls(),
        ["token", "broadcasts", "create", "active", "token", "active"]
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
