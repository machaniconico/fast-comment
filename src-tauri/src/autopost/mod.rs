pub mod coordinator;
pub mod error;
pub mod ledger;
pub mod template;

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

pub use coordinator::{Action, AutopostSettings, AutopostStatus, Coordinator, Input, PostedTweet};
pub use error::{XPostError, XPostErrorCode};
pub use ledger::{Ledger, LedgerEntry, LedgerState, LoadOutcome};
pub use template::{render, Rendered};

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
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn settings_from(config: &AppConfig) -> AutopostSettings {
    AutopostSettings {
        x_post: config.x_post.clone(),
        channels: config.channels.clone(),
        // API 投稿(OAuth)は後続 PR。現状は Intent モードのみ発火できる。
        connected: false,
    }
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
fn run_actions<F>(
    coordinator: &mut Coordinator,
    actions: Vec<Action>,
    ledger_path: &std::path::Path,
    status_tx: &watch::Sender<AutopostStatus>,
    open_intent: &F,
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
                Action::Post { .. } => {
                    // API 投稿は未接続扱い(後続 PR)。in_flight を解くため失敗として戻す。
                    feedback = Some(Input::PostFinished(Err(XPostError::new(
                        XPostErrorCode::NotLoggedIn,
                    ))));
                }
                Action::PrewarmToken => {}
                Action::Publish(status) => publish(status_tx, status),
            }
        }
        match feedback {
            Some(input) => queue = coordinator.handle(input, now_ms()),
            None => break,
        }
    }
}

/// X 自動告知の駆動タスク。coordinator は純粋な状態機械で、I/O はここで行う。
///
/// 起動時に台帳が壊れていた・読めなかった場合は、この実行中の自動告知を止め
/// `failed`(`ledgerFailed`)を通知する。
pub fn spawn_autopost<F>(
    mut live_rx: mpsc::Receiver<LiveEvent>,
    mut config_rx: watch::Receiver<AppConfig>,
    mut user_rx: mpsc::Receiver<UserCommand>,
    ledger_path: PathBuf,
    status_tx: watch::Sender<AutopostStatus>,
    open_intent: F,
    cancel: CancellationToken,
) -> tauri::async_runtime::JoinHandle<()>
where
    F: Fn(&str) -> Result<(), String> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let settings = settings_from(&config_rx.borrow_and_update());
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
            run_actions(&mut coordinator, actions, &ledger_path, &status_tx, &open_intent);
        }

        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut user_open = true;
        let mut config_open = true;
        loop {
            let input = tokio::select! {
                _ = cancel.cancelled() => break,
                event = live_rx.recv() => match event {
                    Some(event) => Input::Live(event),
                    None => break,
                },
                changed = config_rx.changed(), if config_open => match changed {
                    Ok(()) => Input::Settings(settings_from(&config_rx.borrow_and_update())),
                    Err(_) => {
                        config_open = false;
                        continue;
                    }
                },
                command = user_rx.recv(), if user_open => match command {
                    Some(UserCommand::Cancel(id)) => Input::UserCancel { id },
                    Some(UserCommand::Retry) => Input::UserRetry,
                    Some(UserCommand::Approve(id)) => Input::UserApprove { id },
                    None => {
                        user_open = false;
                        continue;
                    }
                },
                _ = tick.tick() => Input::Tick,
            };
            let actions = coordinator.handle(input, now_ms());
            run_actions(&mut coordinator, actions, &ledger_path, &status_tx, &open_intent);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, ChannelPlatform};
    use crate::model::{LiveStream, Platform};
    use std::sync::{Arc, Mutex};

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
        let mut coordinator = Coordinator::new(Ledger::default(), settings_from(&config()));
        let (status_tx, status_rx) = watch::channel(coordinator.status().clone());
        let opened = Mutex::new(0usize);
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("s3"))), now_ms());
        assert!(actions.iter().any(|a| matches!(a, Action::OpenIntent { .. })));
        run_actions(&mut coordinator, actions, &path, &status_tx, &|_: &str| {
            *opened.lock().unwrap() += 1;
            Ok(())
        });
        assert_eq!(*opened.lock().unwrap(), 0);
        let status = status_rx.borrow().clone();
        assert_eq!(status.state, "failed");
        assert_eq!(status.reason.as_deref(), Some("ledgerFailed"));
    }

    #[test]
    fn open_failure_records_failed_and_retry_reopens() {
        let path = temp_ledger();
        let mut coordinator = Coordinator::new(Ledger::default(), settings_from(&config()));
        let (status_tx, status_rx) = watch::channel(coordinator.status().clone());
        let actions = coordinator.handle(Input::Live(LiveEvent::Started(stream("s4"))), now_ms());
        run_actions(&mut coordinator, actions, &path, &status_tx, &|_: &str| {
            Err("no browser".to_string())
        });
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
        run_actions(&mut coordinator, actions, &path, &status_tx, &|_: &str| {
            *opened.lock().unwrap() += 1;
            Ok(())
        });
        assert_eq!(*opened.lock().unwrap(), 1);
        assert_eq!(status_rx.borrow().state, "opened");
        assert_eq!(coordinator.ledger().entries()[0].state, LedgerState::Opened);
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
