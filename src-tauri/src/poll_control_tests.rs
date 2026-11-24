use super::*;
use crate::{
    config::ChannelConfig,
    poll::{PollDisplay, PollPreferences, PollStart, PollStatus, PreferenceChange},
    sources::{
        twitch_auth::login_scopes, twitch_moderation::cache_poll_test_user,
        twitch_polls::validate_twitch,
    },
};
use axum::{
    body::Body,
    extract::Request,
    response::{IntoResponse, Response},
    Json, Router,
};
use serde_json::json;
use std::{
    future::Future,
    io::{self, Write},
    sync::{
        atomic::{AtomicUsize, Ordering::SeqCst},
        Mutex as StdMutex,
    },
};
use tokio::sync::Notify;
use tracing::instrument::WithSubscriber;

const CHANNEL: &str = "poll-review-test";
const BROADCASTER: &str = "42";
const TOKEN: &str = "test-secret-token";
const CLIENT_ID: &str = "test-secret-client-id";

fn request() -> PollStart {
    PollStart {
        source: PollSource::Twitch,
        question: " 今日の色は？ ".into(),
        options: vec![" 赤 ".into(), "青".into()],
        duration_secs: Some(60),
        display: PollDisplay::Pie,
        show_percent: false,
        show_counts: true,
    }
}

fn remote_poll(status: &str) -> serde_json::Value {
    json!({
        "id": "remote-42",
        "title": "今日の色は？",
        "status": status,
        "choices": [{"title": "赤", "votes": 3}, {"title": "青", "votes": 2}]
    })
}

fn response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"message": message}))).into_response()
}

async fn mock<F, Fut>(handler: F) -> (String, tokio::task::JoinHandle<()>)
where
    F: Fn(Request) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Response> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/helix", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        axum::serve(listener, Router::new().fallback(handler))
            .await
            .unwrap();
    });
    (base, task)
}

struct Fixture {
    control: Arc<PollControl>,
    config: watch::Sender<AppConfig>,
    saved: Arc<StdMutex<Vec<PollPreferences>>>,
}

impl Fixture {
    async fn new(base: String, user: &str) -> Self {
        cache_poll_test_user(CHANNEL, BROADCASTER).await;
        let auth = Arc::new(TwitchAuth::for_poll_test(TwitchAccess {
            client_id: CLIENT_ID.into(),
            token: TOKEN.into(),
            user_id: user.into(),
            login: CHANNEL.into(),
            scopes: login_scopes().into_iter().map(str::to_owned).collect(),
        }));
        let mut config = AppConfig::default();
        config.credentials.twitch_client_id = CLIENT_ID.into();
        config.channels = vec![ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: CHANNEL.into(),
            enabled: true,
            own_stream: true,
            overrides: Default::default(),
        }];
        let (config, rx) = watch::channel(config);
        let saved = Arc::new(StdMutex::new(Vec::new()));
        let captured = saved.clone();
        let poll = Arc::new(Poll::new(
            move |change| {
                if let PreferenceChange::Started(preferences) = change {
                    captured.lock().unwrap().push(preferences.clone());
                }
                Ok(())
            },
            |_| {},
        ));
        let mut control = PollControl::new(poll, auth, rx, CancellationToken::new());
        control.helix_base = base;
        Self {
            control: Arc::new(control),
            config,
            saved,
        }
    }

    async fn track(&self) -> (String, CancellationToken) {
        let open = self.control.poll.open(request().validate().unwrap());
        self.control.poll.apply_external(&open.id, &[8, 5], false);
        let cancel = self.control.app_cancel.child_token();
        *self.control.active.lock().await = Some(ActivePoll {
            local_id: open.id.clone(),
            target: RemoteTarget {
                remote_id: "remote-42".into(),
                broadcaster_id: BROADCASTER.into(),
            },
            _cancel: cancel.clone().drop_guard(),
        });
        (open.id, cancel)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.control.app_cancel.cancel();
    }
}

#[tokio::test]
async fn reset_only_terminates_active_polls_and_accepts_patch_races() {
    for (remote_status, patch_status) in [
        ("COMPLETED", StatusCode::OK),
        ("ACTIVE", StatusCode::OK),
        ("ACTIVE", StatusCode::BAD_REQUEST),
        ("ACTIVE", StatusCode::NOT_FOUND),
    ] {
        let patches = Arc::new(StdMutex::new(Vec::new()));
        let captured = patches.clone();
        let (base, server) = mock(move |req| {
            let captured = captured.clone();
            async move {
                if req.method() == "PATCH" {
                    let body = axum::body::to_bytes(req.into_body(), 4096).await.unwrap();
                    captured
                        .lock()
                        .unwrap()
                        .push(serde_json::from_slice::<serde_json::Value>(&body).unwrap());
                    response(patch_status, "poll ended")
                } else {
                    Json(json!({"data": [remote_poll(remote_status)]})).into_response()
                }
            }
        })
        .await;
        let fixture = Fixture::new(base, BROADCASTER).await;
        fixture.track().await;
        let result = fixture.control.reset().await.unwrap();
        assert_eq!(result.status, PollStatus::Idle);
        let patches = patches.lock().unwrap();
        if remote_status == "ACTIVE" {
            assert_eq!(
                *patches,
                [json!({
                    "status": "TERMINATED", "id": "remote-42", "broadcaster_id": BROADCASTER
                })]
            );
        } else {
            assert!(patches.is_empty());
        }
        server.abort();
    }
}

#[tokio::test]
async fn create_recovers_only_the_latest_matching_active_poll_after_body_disconnect() {
    let matching = remote_poll("ACTIVE");
    let mut other_title = matching.clone();
    other_title["title"] = json!("別の質問");
    let mut other_choices = matching.clone();
    other_choices["choices"].as_array_mut().unwrap().reverse();
    for (latest, recovered) in [
        (matching.clone(), true),
        (other_title, false),
        (other_choices, false),
        (remote_poll("COMPLETED"), false),
    ] {
        let methods = Arc::new(StdMutex::new(Vec::new()));
        let captured = methods.clone();
        let older = matching.clone();
        let (base, server) = mock(move |req| {
            let captured = captured.clone();
            let data = json!({"data": [latest, older]});
            async move {
                captured
                    .lock()
                    .unwrap()
                    .push((req.method().clone(), req.uri().query().map(str::to_owned)));
                if req.method() == "POST" {
                    let chunks = futures_util::stream::unfold(false, |sent| async move {
                        if sent {
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            Some((Err(io::Error::other("body interrupted")), true))
                        } else {
                            Some((Ok(bytes::Bytes::from_static(b"{\"data\":[")), true))
                        }
                    });
                    Body::from_stream(chunks).into_response()
                } else {
                    Json(data).into_response()
                }
            }
        })
        .await;
        let fixture = Fixture::new(base, BROADCASTER).await;
        let result = fixture.control.start(request().validate().unwrap()).await;
        if recovered {
            assert_eq!(result.unwrap().status, PollStatus::Open);
            assert_eq!(
                fixture
                    .control
                    .active
                    .lock()
                    .await
                    .as_ref()
                    .unwrap()
                    .target
                    .remote_id,
                "remote-42"
            );
            assert_eq!(fixture.saved.lock().unwrap().len(), 1);
        } else {
            assert!(result.unwrap_err().to_string().contains("作成を確認"));
            assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);
            assert!(fixture.saved.lock().unwrap().is_empty());
        }
        assert_eq!(
            *methods.lock().unwrap(),
            [
                (reqwest::Method::POST, None),
                (reqwest::Method::GET, Some("broadcaster_id=42".into())),
            ]
        );
        server.abort();
    }
}

#[tokio::test]
async fn stop_and_reset_update_local_state_on_remote_failure() {
    for (failure, user, signed_out, message) in [
        ("network", BROADCASTER, false, "接続できません"),
        ("signed-out", BROADCASTER, true, "ログインしてください"),
        ("other-user", "other", false, "開始したTwitchユーザー"),
        ("missing", BROADCASTER, false, "404"),
    ] {
        let (base, server) = mock(|_| async { response(StatusCode::NOT_FOUND, "missing") }).await;
        if failure == "network" {
            server.abort();
            let _ = server.await;
        } else {
            // The server runs until the test runtime shuts down.
            drop(server);
        }
        let fixture = Fixture::new(base, user).await;
        if signed_out {
            fixture.control.auth.sign_out_for_poll_test();
        }

        fixture.track().await;
        let error = fixture.control.stop().await.unwrap_err();
        let snapshot = fixture.control.snapshot();
        assert!(error.to_string().contains(message), "{failure}: {error}");
        assert_eq!(snapshot.status, PollStatus::Closed, "{failure}");
        assert_eq!(snapshot.total_voters, 13);
        assert!(fixture.control.active.lock().await.is_none());

        fixture.track().await;
        let error = fixture.control.reset().await.unwrap_err();
        assert!(error.to_string().contains(message), "{failure}: {error}");
        assert_eq!(
            fixture.control.snapshot().status,
            PollStatus::Idle,
            "{failure}"
        );
        assert!(fixture.control.active.lock().await.is_none());
    }
}

#[tokio::test]
async fn polling_404_closes_with_last_votes_and_stops() {
    let requests = Arc::new(AtomicUsize::new(0));
    let captured = requests.clone();
    let (base, server) = mock(move |_| {
        captured.fetch_add(1, SeqCst);
        async { response(StatusCode::NOT_FOUND, "missing") }
    })
    .await;
    let fixture = Fixture::new(base, BROADCASTER).await;
    let (id, cancel) = fixture.track().await;
    tokio::time::timeout(POLL_INTERVAL * 2, fixture.control.clone().run(id, cancel))
        .await
        .unwrap();
    let snapshot = fixture.control.snapshot();
    assert_eq!(snapshot.status, PollStatus::Closed);
    assert_eq!(snapshot.total_voters, 13);
    assert_eq!(requests.load(SeqCst), 1);
    assert!(fixture.control.active.lock().await.is_none());
    server.abort();
}

#[tokio::test]
async fn preferences_are_saved_only_after_create_succeeds() {
    let result_status = Arc::new(StdMutex::new(StatusCode::BAD_REQUEST));
    let status = result_status.clone();
    let (base, server) = mock(move |_| {
        let status = *status.lock().unwrap();
        async move {
            if status == StatusCode::OK {
                Json(json!({"data": [remote_poll("ACTIVE")]})).into_response()
            } else {
                response(status, "refused")
            }
        }
    })
    .await;
    let fixture = Fixture::new(base, BROADCASTER).await;
    assert!(fixture
        .control
        .start(request().validate().unwrap())
        .await
        .is_err());
    assert!(fixture.saved.lock().unwrap().is_empty());
    assert_eq!(fixture.control.snapshot().status, PollStatus::Idle);

    *result_status.lock().unwrap() = StatusCode::OK;
    let start = request().validate().unwrap();
    let expected = start.preferences.clone();
    fixture.control.start(start).await.unwrap();
    assert_eq!(*fixture.saved.lock().unwrap(), [expected]);
    server.abort();
}

#[tokio::test]
async fn slow_poll_response_does_not_block_reset_or_update_new_poll() {
    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let requests = Arc::new(AtomicUsize::new(0));
    let (base, server) = mock({
        let (started, release) = (started.clone(), release.clone());
        move |_| {
            let (started, release) = (started.clone(), release.clone());
            let slow = requests.fetch_add(1, SeqCst) == 0;
            async move {
                if slow {
                    started.notify_one();
                    release.notified().await;
                }
                let status = if slow { "ACTIVE" } else { "COMPLETED" };
                Json(json!({"data": [remote_poll(status)]})).into_response()
            }
        }
    })
    .await;
    let fixture = Fixture::new(base, BROADCASTER).await;
    let (old_id, cancel) = fixture.track().await;
    let poller = tokio::spawn(fixture.control.clone().run(old_id.clone(), cancel));
    tokio::time::timeout(POLL_INTERVAL * 2, started.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), fixture.control.reset())
        .await
        .unwrap()
        .unwrap();
    let (new_id, _) = fixture.track().await;
    assert_ne!(new_id, old_id);
    release.notify_one();
    tokio::time::timeout(Duration::from_secs(2), poller)
        .await
        .unwrap()
        .unwrap();
    let snapshot = fixture.control.snapshot();
    assert_eq!(snapshot.id, new_id);
    assert_eq!(snapshot.total_voters, 13);
    server.abort();
}

#[test]
fn only_http_401_maps_to_unauthorized() {
    for code in [
        ModerationErrorCode::NotLoggedIn,
        ModerationErrorCode::ScopeMissing,
        ModerationErrorCode::NotModerator,
        ModerationErrorCode::ProtectedTarget,
        ModerationErrorCode::NotFound,
        ModerationErrorCode::Unavailable,
        ModerationErrorCode::RateLimited,
        ModerationErrorCode::Conflict,
        ModerationErrorCode::Network,
        ModerationErrorCode::Other,
    ] {
        let error = PollControlError::from(ModerationError::new(code, "details"));
        assert!(
            !matches!(error, PollControlError::Remote(PollsError::Unauthorized)),
            "{code:?}"
        );
    }
    for status in [StatusCode::UNAUTHORIZED, StatusCode::FORBIDDEN] {
        let error = PollControlError::from(HelixError::Api {
            status,
            body: String::new(),
        });
        assert_eq!(
            matches!(error, PollControlError::Remote(PollsError::Unauthorized)),
            status == StatusCode::UNAUTHORIZED
        );
    }
}

#[tokio::test]
async fn forbidden_end_is_generic_except_for_missing_scope() {
    for (message, scope_missing) in [("MODERATED", false), ("Missing scope", true)] {
        let (base, server) =
            mock(move |_| async move { response(StatusCode::FORBIDDEN, message) }).await;
        let helix = HelixPolls::new(&base, CLIENT_ID);
        let error = helix
            .end(TOKEN, BROADCASTER, "remote-42", false)
            .await
            .unwrap_err();
        if scope_missing {
            assert!(matches!(error, PollsError::ScopeMissing));
        } else {
            assert!(matches!(error, PollsError::Api(403)));
        }
        server.abort();
    }
}

#[tokio::test]
async fn bad_request_displays_only_safe_helix_message() {
    for (message, visible) in [
        ("poll title is too long", true),
        (TOKEN, false),
        (CLIENT_ID, false),
        ("https://example.test/polls?other=1", false),
    ] {
        let (base, server) =
            mock(move |_| async move { response(StatusCode::BAD_REQUEST, message) }).await;
        let helix = HelixPolls::new(&base, CLIENT_ID);
        let error = helix
            .create(
                TOKEN,
                BROADCASTER,
                "question",
                &["a".into(), "b".into()],
                60,
            )
            .await
            .unwrap_err();
        assert_eq!(error.to_string().contains(message), visible);
        server.abort();
    }
}

#[test]
fn twitch_rejects_duplicate_choices_using_chat_rules() {
    for (first, second) in [
        (" 赤 ", "赤"),
        ("Blue", "bLUE"),
        ("Σ", "ς"),
        ("青\u{200b}", "青"),
    ] {
        let options = [first.into(), second.into()];
        assert_eq!(
            validate_twitch("質問", &options, Some(60)),
            Err("同じ選択肢は指定できません")
        );
    }
}

#[tokio::test]
async fn disabled_own_streams_do_not_count() {
    let (base, server) =
        mock(|_| async { Json(json!({"data": [remote_poll("ACTIVE")]})).into_response() }).await;
    let fixture = Fixture::new(base, BROADCASTER).await;
    fixture.config.send_modify(|cfg| {
        cfg.channels.push(ChannelConfig {
            enabled: false,
            identifier: "offline".into(),
            ..cfg.channels[0].clone()
        });
    });
    assert_eq!(
        fixture
            .control
            .start(request().validate().unwrap())
            .await
            .unwrap()
            .status,
        PollStatus::Open
    );
    server.abort();
}

struct LogBuffer(Arc<StdMutex<Vec<u8>>>);

impl Write for LogBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn poller_warns_once_per_consecutive_error_class_without_details() {
    let requests = Arc::new(AtomicUsize::new(0));
    let (base, server) = mock(move |_| {
        let index = requests.fetch_add(1, SeqCst);
        async move {
            match index {
                0 | 1 | 3 => response(StatusCode::BAD_REQUEST, "test detail"),
                4 => response(StatusCode::FORBIDDEN, TOKEN),
                5 => response(StatusCode::FORBIDDEN, CLIENT_ID),
                6 => Json(json!({"data": [remote_poll("COMPLETED")]})).into_response(),
                _ => Json(json!({"data": [remote_poll("ACTIVE")]})).into_response(),
            }
        }
    })
    .await;
    let output = Arc::new(StdMutex::new(Vec::new()));
    let captured = output.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_writer(move || LogBuffer(captured.clone()))
        .finish();

    let fixture = Fixture::new(base, BROADCASTER).await;
    let (id, cancel) = fixture.track().await;
    tokio::time::timeout(
        POLL_INTERVAL * 8,
        fixture
            .control
            .clone()
            .run(id, cancel)
            .with_subscriber(subscriber),
    )
    .await
    .unwrap();
    let output = String::from_utf8(output.lock().unwrap().clone()).unwrap();
    assert_eq!(output.matches("WARN").count(), 3, "{output}");
    assert_eq!(output.matches("BadRequest").count(), 2, "{output}");
    assert_eq!(output.matches("Api").count(), 1, "{output}");
    for detail in [
        TOKEN,
        CLIENT_ID,
        fixture.control.helix_base.as_str(),
        "test detail",
        "403",
    ] {
        assert!(!output.contains(detail), "{output}");
    }
    server.abort();
}
