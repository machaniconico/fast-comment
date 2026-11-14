pub mod coordinator;
pub mod error;
pub mod ledger;
pub mod oauth;
pub mod template;
pub mod x_api;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub use coordinator::{Action, AutopostSettings, AutopostStatus, Coordinator, Input, PostedTweet};
pub use error::{XPostError, XPostErrorCode};
pub use ledger::{Ledger, LedgerEntry, LedgerState, LoadOutcome};
pub use oauth::{
    build_authorize_url, callback_url, KeyringTokenStore, StoredXToken, TokenStore, XEndpoints,
    XPostAuth, XPostStatus, X_POST_TOKEN_ENTRY,
};
pub use template::{render, Rendered};
pub use x_api::{classify_post_response, post_text};

use crate::config::AppConfig;
use crate::model::LiveEvent;

/// 台帳ファイル名(`config.json` と同じディレクトリに置く)。
pub const LEDGER_FILE: &str = "x-autopost-ledger.json";

/// 秒読み・承認期限を進める間隔。
const TICK: Duration = Duration::from_secs(1);

/// UI からの操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserCommand {
    /// 取消。`id` はバナーに出ていた告知の `AutopostStatus::id`。
    Cancel(u64),
    Retry,
    /// 承認。`id` はバナーに出ていた告知の `AutopostStatus::id`。
    Approve(u64),
    /// X への接続・切断が終わった(API モードで投稿できるかを読み直す)。
    AuthChanged,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn settings_from(config: &AppConfig, connected: bool) -> AutopostSettings {
    AutopostSettings {
        x_post: config.x_post.clone(),
        channels: config.channels.clone(),
        connected,
    }
}

/// 設定の API モード用 Client ID(空白のみは未設定)。
pub fn post_client_id(config: &AppConfig) -> String {
    config
        .credentials
        .x_post_client_id
        .as_deref()
        .unwrap_or("")
        .trim()
        .to_string()
}

/// 今の Client ID で投稿できる認証があるか(通信はしない。保存先は読む)。
async fn api_connected<S: TokenStore>(auth: &XPostAuth<S>, config: &AppConfig) -> bool {
    let client_id = post_client_id(config);
    !client_id.is_empty()
        && auth
            .status(&client_id, config.x_post.callback_port)
            .await
            .connected
}

/// 認証の状態が変わったと分かる失敗(以後は未接続として扱う)。
fn auth_lost(code: XPostErrorCode) -> bool {
    matches!(
        code,
        XPostErrorCode::ReloginRequired | XPostErrorCode::NotLoggedIn
    )
}

/// 変化した時だけ watch を更新する(UI への emit を変化時に限るため)。
fn publish(status_tx: &watch::Sender<AutopostStatus>, status: AutopostStatus) {
    status_tx.send_if_modified(|current| {
        if *current == status {
            false
        } else {
            *current = status;
            true
        }
    });
}

/// `Action` を返された順に実行する。`PersistLedger` に失敗したら後続は実行せず、
/// `LedgerFailed` を coordinator に戻してその結果を実行する。
///
/// `Post` と `PrewarmToken` は通信を伴うので、ここでは非同期処理を起こすだけにする
/// (`post` の結果は駆動タスクが `Input::PostFinished` として戻す)。
fn run_actions<F>(
    coordinator: &mut Coordinator,
    actions: Vec<Action>,
    ledger_path: &std::path::Path,
    status_tx: &watch::Sender<AutopostStatus>,
    open_intent: &F,
    api: &dyn ApiRunner,
) where
    F: Fn(&str) -> Result<(), String>,
{
    let mut queue = actions;
    loop {
        let mut feedback = None;
        for action in queue {
            match action {
                Action::PersistLedger => {
                    if let Err(e) = coordinator.ledger().save(ledger_path) {
                        tracing::error!("X 自動告知の台帳保存に失敗: {e}");
                        feedback = Some(Input::LedgerFailed(e.to_string()));
                        break;
                    }
                }
                Action::OpenIntent { url, .. } => {
                    if let Err(e) = open_intent(&url) {
                        tracing::warn!("X 告知画面を開けませんでした: {e}");
                        // 台帳を opened のままにしない。残りの Publish は実行してから戻す。
                        feedback = Some(Input::IntentOpenFailed);
                    }
                }
                // 台帳(posting)はこの前の PersistLedger で保存済み。保存に失敗した時は
                // 上の break でここまで来ない(HTTP は 1 回も呼ばれない)。
                Action::Post { text } => api.post(text),
                Action::PrewarmToken => api.prewarm(),
                Action::Publish(status) => publish(status_tx, status),
            }
        }
        match feedback {
            Some(input) => queue = coordinator.handle(input, now_ms()),
            None => break,
        }
    }
}

/// API モードの通信を起こす口(テストで差し替える)。
trait ApiRunner {
    /// 1 回だけ投稿する。結果は駆動タスクへ戻す。
    fn post(&self, text: String);
    /// 猶予の開始時に access token を先に用意する(認証切れを投稿前に知るため)。
    fn prewarm(&self);
}

/// 投稿の結果と、認証が失われたことの通知を駆動タスクへ戻す。
struct XApiRunner<S: TokenStore + 'static> {
    auth: Arc<XPostAuth<S>>,
    client_id: String,
    done_tx: mpsc::Sender<Result<PostedTweet, XPostError>>,
    auth_tx: mpsc::Sender<()>,
}

impl<S: TokenStore + 'static> ApiRunner for XApiRunner<S> {
    fn post(&self, text: String) {
        let auth = self.auth.clone();
        let client_id = self.client_id.clone();
        let done_tx = self.done_tx.clone();
        tauri::async_runtime::spawn(async move {
            let result = post_text(&auth, &client_id, &text).await;
            // 受け手が居ない(アプリ終了)なら捨てる。台帳は posting のまま残り、次回起動で uncertain になる。
            let _ = done_tx.send(result).await;
        });
    }

    fn prewarm(&self) {
        let auth = self.auth.clone();
        let client_id = self.client_id.clone();
        let auth_tx = self.auth_tx.clone();
        tauri::async_runtime::spawn(async move {
            if let Err(error) = auth.access_token(&client_id).await {
                if auth_lost(error.code) {
                    let _ = auth_tx.try_send(());
                }
            }
        });
    }
}

/// X 自動告知の駆動タスク。coordinator は純粋な状態機械で、I/O はここで行う。
///
/// 起動時に台帳が壊れていた・読めなかった場合は、この実行中の自動告知を止め
/// `failed`(`ledgerFailed`)を通知する。
#[allow(clippy::too_many_arguments)]
pub fn spawn_autopost<S, F>(
    mut live_rx: mpsc::Receiver<LiveEvent>,
    mut config_rx: watch::Receiver<AppConfig>,
    mut user_rx: mpsc::Receiver<UserCommand>,
    auth: Arc<XPostAuth<S>>,
    ledger_path: PathBuf,
    status_tx: watch::Sender<AutopostStatus>,
    open_intent: F,
    cancel: CancellationToken,
) -> tauri::async_runtime::JoinHandle<()>
where
    S: TokenStore + 'static,
    F: Fn(&str) -> Result<(), String> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let (done_tx, mut done_rx) = mpsc::channel::<Result<PostedTweet, XPostError>>(4);
        let (auth_tx, mut auth_rx) = mpsc::channel::<()>(1);
        let mut config = config_rx.borrow_and_update().clone();
        let mut connected = api_connected(&auth, &config).await;
        let runner = |config: &AppConfig| XApiRunner {
            auth: auth.clone(),
            client_id: post_client_id(config),
            done_tx: done_tx.clone(),
            auth_tx: auth_tx.clone(),
        };
        let settings = settings_from(&config, connected);
        let (ledger, load_error) = match Ledger::load(&ledger_path, now_ms()) {
            Ok(LoadOutcome::Loaded(ledger)) => (ledger, None),
            Ok(LoadOutcome::Corrupt { backup_path }) => {
                tracing::error!(
                    "X 自動告知の台帳が壊れていたため退避しました: {}",
                    backup_path.display()
                );
                (Ledger::default(), Some("corrupt".to_string()))
            }
            Err(e) => {
                tracing::error!("X 自動告知の台帳を読めませんでした: {e}");
                (Ledger::default(), Some(e.to_string()))
            }
        };
        let mut coordinator = Coordinator::new(ledger, settings);
        publish(&status_tx, coordinator.status().clone());
        if let Some(reason) = load_error {
            let actions = coordinator.handle(Input::LedgerFailed(reason), now_ms());
            run_actions(
                &mut coordinator,
                actions,
                &ledger_path,
                &status_tx,
                &open_intent,
                &runner(&config),
            );
        }

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut user_open = true;
        let mut config_open = true;
        loop {
            let input = tokio::select! {
                // 上から順に見る。待機中に溜まった取消などを Tick より先に処理する。
                biased;
                _ = cancel.cancelled() => break,
                event = live_rx.recv() => match event {
                    Some(event) => Input::Live(event),
                    None => break,
                },
                changed = config_rx.changed(), if config_open => match changed {
                    Ok(()) => {
                        config = config_rx.borrow_and_update().clone();
                        connected = api_connected(&auth, &config).await;
                        Input::Settings(settings_from(&config, connected))
                    }
                    Err(_) => {
                        config_open = false;
                        continue;
                    }
                },
                command = user_rx.recv(), if user_open => match command {
                    Some(UserCommand::Cancel(id)) => Input::UserCancel { id },
                    Some(UserCommand::Retry) => Input::UserRetry,
                    Some(UserCommand::Approve(id)) => Input::UserApprove { id },
                    Some(UserCommand::AuthChanged) => {
                        connected = api_connected(&auth, &config).await;
                        Input::Settings(settings_from(&config, connected))
                    }
                    None => {
                        user_open = false;
                        continue;
                    }
                },
                Some(result) = done_rx.recv() => {
                    if let Err(error) = &result {
                        if auth_lost(error.code) {
                            let _ = auth_tx.try_send(());
                        }
                    }
                    Input::PostFinished(result)
                }
                Some(()) = auth_rx.recv() => {
                    let now_connected = api_connected(&auth, &config).await;
                    if now_connected == connected {
                        continue;
                    }
                    connected = now_connected;
                    Input::Settings(settings_from(&config, connected))
                }
                _ = tick.tick() => Input::Tick,
            };
            let actions = coordinator.handle(input, now_ms());
            run_actions(
                &mut coordinator,
                actions,
                &ledger_path,
                &status_tx,
                &open_intent,
                &runner(&config),
            );
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, ChannelPlatform};
    use crate::model::{LiveStream, Platform};
    use crate::autopost::oauth::MemoryTokenStore;
    use std::sync::Mutex;

    /// 通信しない runner。呼ばれた回数だけ数える。
    #[derive(Default)]
    struct CountingRunner {
        posts: Mutex<Vec<String>>,
        prewarms: Mutex<usize>,
    }

    impl ApiRunner for CountingRunner {
        fn post(&self, text: String) {
            self.posts.lock().unwrap().push(text);
        }
        fn prewarm(&self) {
            *self.prewarms.lock().unwrap() += 1;
        }
    }

    fn temp_ledger() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fast-comment-autopost-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(LEDGER_FILE)
    }

    fn config() -> AppConfig {
        let mut cfg = AppConfig::default();
        cfg.x_post.enabled = true;
        cfg.x_post.mode = "intent".into();
        cfg.x_post.intent_open = "auto".into();
        cfg.x_post.delay_secs = 0;
        cfg.channels = vec![ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "mine".into(),
            enabled: true,
            own_stream: true,
        }];
        cfg
    }

    fn stream(id: &str) -> LiveStream {
        LiveStream {
            platform: Platform::Twitch,
            channel_identifier: "mine".into(),
            stream_id: id.into(),
            title: Some("テスト配信".into()),
            url: "https://www.twitch.tv/mine".into(),
            started_at_ms: Some(now_ms()),
        }
    }

    struct Harness {
        live_tx: mpsc::Sender<LiveEvent>,
        user_tx: mpsc::Sender<UserCommand>,
        status_rx: watch::Receiver<AutopostStatus>,
        opened: Arc<Mutex<Vec<String>>>,
        cancel: CancellationToken,
        handle: tauri::async_runtime::JoinHandle<()>,
    }

    fn start(cfg: AppConfig, ledger_path: PathBuf) -> Harness {
        let auth = Arc::new(
            XPostAuth::new(Arc::new(MemoryTokenStore::new()), XEndpoints::default()).unwrap(),
        );
        start_with(cfg, ledger_path, auth)
    }

    fn start_with(
        cfg: AppConfig,
        ledger_path: PathBuf,
        auth: Arc<XPostAuth<Arc<MemoryTokenStore>>>,
    ) -> Harness {
        let (live_tx, live_rx) = mpsc::channel(8);
        let (_config_tx, config_rx) = watch::channel(cfg);
        let (user_tx, user_rx) = mpsc::channel(8);
        let (status_tx, status_rx) = watch::channel(AutopostStatus::new("idle"));
        let opened = Arc::new(Mutex::new(Vec::new()));
        let sink = opened.clone();
        let cancel = CancellationToken::new();
        let handle = spawn_autopost(
            live_rx,
            config_rx,
            user_rx,
            auth,
            ledger_path,
            status_tx,
            move |url: &str| {
                sink.lock().unwrap().push(url.to_string());
                Ok(())
            },
            cancel.clone(),
        );
        Harness {
            live_tx,
            user_tx,
            status_rx,
            opened,
            cancel,
            handle,
        }
    }

    async fn wait_state(rx: &mut watch::Receiver<AutopostStatus>, state: &str) -> AutopostStatus {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if rx.borrow_and_update().state == state {
                    return rx.borrow().clone();
                }
                rx.changed().await.expect("status sender alive");
            }
        })
        .await
        .unwrap_or_else(|_| panic!("state {state} not reached: {:?}", rx.borrow()))
    }

    #[tokio::test]
    async fn started_event_opens_intent_exactly_once_and_persists_ledger() {
        let path = temp_ledger();
        let mut h = start(config(), path.clone());
        h.live_tx
            .send(LiveEvent::Started(stream("s1")))
            .await
            .unwrap();
        // 同じ配信の重複通知では開かない。
        h.live_tx
            .send(LiveEvent::Started(stream("s1")))
            .await
            .unwrap();
        let status = wait_state(&mut h.status_rx, "opened").await;
        assert!(status.text.unwrap().contains("テスト配信"));
        tokio::time::sleep(Duration::from_millis(1500)).await;
        let opened = h.opened.lock().unwrap().clone();
        assert_eq!(opened.len(), 1);
        assert!(opened[0].starts_with("https://x.com/intent/post?text="));
        match Ledger::load(&path, now_ms()).unwrap() {
            LoadOutcome::Loaded(ledger) => {
                assert_eq!(ledger.entries().len(), 1);
                assert_eq!(ledger.entries()[0].state, LedgerState::Opened);
            }
            other => panic!("unexpected ledger: {other:?}"),
        }
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_ledger_at_startup_disables_autopost_and_reports_status() {
        let path = temp_ledger();
        std::fs::write(&path, b"{not json").unwrap();
        let mut h = start(config(), path);
        let status = wait_state(&mut h.status_rx, "failed").await;
        assert_eq!(status.reason.as_deref(), Some("ledgerFailed"));
        h.live_tx
            .send(LiveEvent::Started(stream("s2")))
            .await
            .unwrap();
        h.user_tx.send(UserCommand::Approve(1)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert!(h.opened.lock().unwrap().is_empty());
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[tokio::test]
    async fn persist_failure_stops_following_actions() {
        // 台帳の置き場がディレクトリだと保存に失敗する。
        let path = temp_ledger();
        std::fs::create_dir_all(&path).unwrap();
        let mut coordinator = Coordinator::new(Ledger::default(), settings_from(&config(), false));
        let (status_tx, status_rx) = watch::channel(coordinator.status().clone());
        let opened = Mutex::new(0usize);
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("s3"))), now_ms());
        assert!(actions.iter().any(|a| matches!(a, Action::OpenIntent { .. })));
        run_actions(
            &mut coordinator,
            actions,
            &path,
            &status_tx,
            &|_: &str| {
                *opened.lock().unwrap() += 1;
                Ok(())
            },
            &CountingRunner::default(),
        );
        assert_eq!(*opened.lock().unwrap(), 0);
        let status = status_rx.borrow().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.reason.as_deref(), Some("ledgerFailed"));
    }

    #[test]
    fn open_failure_records_failed_and_retry_reopens() {
        let path = temp_ledger();
        let mut coordinator = Coordinator::new(Ledger::default(), settings_from(&config(), false));
        let (status_tx, status_rx) = watch::channel(coordinator.status().clone());
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("s4"))), now_ms());
        run_actions(
            &mut coordinator,
            actions,
            &path,
            &status_tx,
            &|_: &str| Err("no browser".to_string()),
            &CountingRunner::default(),
        );
        let status = status_rx.borrow().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.reason.as_deref(), Some("openFailed"));
        assert!(status.can_retry);
        let saved = match Ledger::load(&path, now_ms()).unwrap() {
            LoadOutcome::Loaded(ledger) => ledger,
            other => panic!("unexpected ledger: {other:?}"),
        };
        assert_eq!(saved.entries()[0].state, LedgerState::Failed);
        let opened = Mutex::new(0usize);
        let actions = coordinator.handle(Input::UserRetry, now_ms());
        run_actions(
            &mut coordinator,
            actions,
            &path,
            &status_tx,
            &|_: &str| {
                *opened.lock().unwrap() += 1;
                Ok(())
            },
            &CountingRunner::default(),
        );
        assert_eq!(*opened.lock().unwrap(), 1);
        assert_eq!(status_rx.borrow().state, "opened");
        assert_eq!(coordinator.ledger().entries()[0].state, LedgerState::Opened);
    }

    // ── API モード ─────────────────────────────────────────────

    use crate::autopost::oauth::StoredXToken;
    use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::collections::VecDeque;

    #[derive(Default)]
    struct FakeX {
        tweets: Mutex<Vec<String>>,
        replies: Mutex<VecDeque<(u16, Value)>>,
    }

    async fn fake_tweets(State(x): State<Arc<FakeX>>, body: String) -> (StatusCode, Json<Value>) {
        x.tweets.lock().unwrap().push(body);
        match x.replies.lock().unwrap().pop_front() {
            Some((status, value)) => (StatusCode::from_u16(status).unwrap(), Json(value)),
            None => (StatusCode::CREATED, Json(json!({"data": {"id": "1234"}}))),
        }
    }

    async fn fake_token() -> Json<Value> {
        Json(json!({"access_token": "a", "refresh_token": "r-next", "expires_in": 7200}))
    }

    async fn fake_x() -> (XEndpoints, Arc<FakeX>) {
        let x = Arc::new(FakeX::default());
        let app = Router::new()
            .route("/tweets", post(fake_tweets))
            .route("/token", post(fake_token))
            .with_state(x.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let endpoints = XEndpoints {
            authorize: format!("{base}/authorize"),
            token: format!("{base}/token"),
            tweets: format!("{base}/tweets"),
            users_me: format!("{base}/users/me"),
        };
        (endpoints, x)
    }

    fn api_config() -> AppConfig {
        let mut cfg = config();
        cfg.x_post.mode = "api".into();
        cfg.credentials.x_post_client_id = Some("client-1".into());
        cfg
    }

    fn connected_auth(endpoints: XEndpoints) -> Arc<XPostAuth<Arc<MemoryTokenStore>>> {
        let store = Arc::new(MemoryTokenStore::with_token(StoredXToken {
            client_id: "client-1".into(),
            refresh_token: "r1".into(),
            user_id: "42".into(),
            username: "alice".into(),
        }));
        Arc::new(XPostAuth::new(store, endpoints).unwrap())
    }

    fn saved_ledger(path: &std::path::Path) -> Ledger {
        match Ledger::load(path, now_ms()).unwrap() {
            LoadOutcome::Loaded(ledger) => ledger,
            other => panic!("unexpected ledger: {other:?}"),
        }
    }

    #[tokio::test]
    async fn api_mode_posts_once_and_records_tweet_id() {
        let (endpoints, x) = fake_x().await;
        let path = temp_ledger();
        let mut h = start_with(api_config(), path.clone(), connected_auth(endpoints));
        h.live_tx
            .send(LiveEvent::Started(stream("a1")))
            .await
            .unwrap();
        h.live_tx
            .send(LiveEvent::Started(stream("a1")))
            .await
            .unwrap();
        let status = wait_state(&mut h.status_rx, "posted").await;
        assert_eq!(status.tweet_id.as_deref(), Some("1234"));
        tokio::time::sleep(Duration::from_millis(300)).await;
        let tweets = x.tweets.lock().unwrap().clone();
        assert_eq!(tweets.len(), 1);
        assert!(tweets[0].contains("テスト配信"));
        let ledger = saved_ledger(&path);
        assert_eq!(ledger.entries()[0].state, LedgerState::Posted);
        assert_eq!(ledger.entries()[0].tweet_id.as_deref(), Some("1234"));
        assert!(h.opened.lock().unwrap().is_empty());
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[tokio::test]
    async fn api_mode_server_error_is_uncertain_and_not_resent() {
        let (endpoints, x) = fake_x().await;
        x.replies
            .lock()
            .unwrap()
            .push_back((503, json!({"title": "Service Unavailable"})));
        let path = temp_ledger();
        let mut h = start_with(api_config(), path.clone(), connected_auth(endpoints));
        h.live_tx
            .send(LiveEvent::Started(stream("a2")))
            .await
            .unwrap();
        let status = wait_state(&mut h.status_rx, "uncertain").await;
        assert!(status.can_retry);
        tokio::time::sleep(Duration::from_millis(1500)).await;
        assert_eq!(x.tweets.lock().unwrap().len(), 1);
        assert_eq!(saved_ledger(&path).entries()[0].state, LedgerState::Uncertain);
        // 再送は利用者の操作でだけ、1 回。
        h.user_tx.send(UserCommand::Retry).await.unwrap();
        wait_state(&mut h.status_rx, "posted").await;
        assert_eq!(x.tweets.lock().unwrap().len(), 2);
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[tokio::test]
    async fn api_mode_without_connection_skips_without_http() {
        let (endpoints, x) = fake_x().await;
        let auth = Arc::new(
            XPostAuth::new(Arc::new(MemoryTokenStore::new()), endpoints).unwrap(),
        );
        let path = temp_ledger();
        let mut h = start_with(api_config(), path.clone(), auth);
        h.live_tx
            .send(LiveEvent::Started(stream("a3")))
            .await
            .unwrap();
        let status = wait_state(&mut h.status_rx, "skipped").await;
        assert_eq!(status.reason.as_deref(), Some("notConnected"));
        assert!(x.tweets.lock().unwrap().is_empty());
        assert!(!path.exists());
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[tokio::test]
    async fn auth_changed_rereads_connection() {
        let (endpoints, x) = fake_x().await;
        let store = Arc::new(MemoryTokenStore::new());
        let auth = Arc::new(XPostAuth::new(store.clone(), endpoints).unwrap());
        let path = temp_ledger();
        let mut h = start_with(api_config(), path, auth);
        store.set_token(Some(StoredXToken {
            client_id: "client-1".into(),
            refresh_token: "r1".into(),
            user_id: "42".into(),
            username: "alice".into(),
        }));
        h.user_tx.send(UserCommand::AuthChanged).await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        h.live_tx
            .send(LiveEvent::Started(stream("a4")))
            .await
            .unwrap();
        wait_state(&mut h.status_rx, "posted").await;
        assert_eq!(x.tweets.lock().unwrap().len(), 1);
        h.cancel.cancel();
        h.handle.await.unwrap();
    }

    #[test]
    fn api_persist_failure_never_starts_http() {
        let path = temp_ledger();
        std::fs::create_dir_all(&path).unwrap();
        let mut coordinator =
            Coordinator::new(Ledger::default(), settings_from(&api_config(), true));
        let (status_tx, status_rx) = watch::channel(coordinator.status().clone());
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("a5"))), now_ms());
        assert!(actions.iter().any(|a| matches!(a, Action::Post { .. })));
        let runner = CountingRunner::default();
        run_actions(
            &mut coordinator,
            actions,
            &path,
            &status_tx,
            &|_: &str| Ok(()),
            &runner,
        );
        assert!(runner.posts.lock().unwrap().is_empty());
        assert_eq!(status_rx.borrow().reason.as_deref(), Some("ledgerFailed"));
    }

    #[test]
    fn api_post_runs_after_persisting_posting() {
        let path = temp_ledger();
        let mut cfg = api_config();
        cfg.x_post.delay_secs = 30;
        let mut coordinator = Coordinator::new(Ledger::default(), settings_from(&cfg, true));
        let (status_tx, _status_rx) = watch::channel(coordinator.status().clone());
        let runner = CountingRunner::default();
        let now = now_ms();
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("a6"))), now);
        run_actions(&mut coordinator, actions, &path, &status_tx, &|_: &str| Ok(()), &runner);
        assert_eq!(*runner.prewarms.lock().unwrap(), 1);
        assert!(runner.posts.lock().unwrap().is_empty());
        let actions = coordinator.handle(Input::Tick, now + 31_000);
        run_actions(&mut coordinator, actions, &path, &status_tx, &|_: &str| Ok(()), &runner);
        assert_eq!(runner.posts.lock().unwrap().len(), 1);
        // 投稿の前に posting が書かれている(読み込みは posting を uncertain に直すので生で見る)。
        assert_eq!(coordinator.ledger().entries()[0].state, LedgerState::Posting);
        assert!(std::fs::read_to_string(&path).unwrap().contains("\"posting\""));
    }

    #[test]
    fn publish_marks_watch_changed_only_on_difference() {
        let (tx, mut rx) = watch::channel(AutopostStatus::new("idle"));
        // borrow のガードを publish の前に落とす(watch のロックを保持したまま送らない)。
        let current = rx.borrow_and_update().clone();
        publish(&tx, current.clone());
        assert!(!rx.has_changed().unwrap());
        let mut next = current;
        next.state = "pending".into();
        publish(&tx, next.clone());
        assert!(rx.has_changed().unwrap());
        rx.borrow_and_update();
        publish(&tx, next);
        assert!(!rx.has_changed().unwrap());
    }
}
