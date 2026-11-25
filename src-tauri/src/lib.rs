//! Tauri 2 アプリのライブラリエントリ。
//!
//! 起動時に:
//!  1. config をロード
//!  2. Bus(broadcast + UI forwarder + OBS axum サーバ)を起動
//!  3. moderation+TTS パイプラインを起動
//!  4. 設定中の各チャンネルに対し Source を起動
//!
//! データ流れ:
//!   Source 群 ──(source_tx)──> パイプライン(moderation/TTS) ──(bus)──> UI/OBS
//!
//! Tauri コマンドで設定取得/更新・チャンネル追加削除・コメント非表示・OBS URL 取得を公開。

pub mod achievements;
pub mod api;
pub mod inject;
pub mod integrations;
pub mod webhook;
pub mod autopost;
pub mod backup;
pub mod bus;
pub mod chatlog;
pub mod config;
pub mod keyword_effects;
pub mod listeners;
pub mod live_fanout;
pub mod model;
pub mod poll;
mod poll_control;
mod poll_control_youtube;
pub mod moderation;
pub mod sound_effects;
mod moderation_error;
mod secret_store;
mod sounds;
mod template_import;
pub mod sources;
pub mod stats;
mod templates;
pub mod tts;
mod update;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::achievements::{
    announce::announce_message,
    engine::AchievementEngine,
    store::DefsStore,
};
use crate::autopost::{
    AutopostStatus, KeyringTokenStore, UserCommand as AutopostCommand, XEndpoints, XPostAuth,
    XPostError, XPostErrorCode, XPostStatus,
};
use crate::bus::{
    spawn_achievement_ui_forwarder, spawn_effect_ui_forwarder, spawn_moderation_ui_forwarder, spawn_reaction_ui_forwarder, Bus,
};
use crate::templates::{TemplateInfo, TemplateRef, TemplateStore};
use crate::config::{
    commit_channel_overrides, commit_config, replace_channels_in, ChannelEditError, ChannelOverrides,
    ConfigCommitError, ConfigLoadOutcome, CONFIG_CHANGED_ERROR,
};
use crate::config::{AppConfig, ChannelConfig, ChannelPlatform, TtsBackendKind, VoicevoxEngineKind};
use crate::model::{
    Author, Badge, ChatMessage, Fragment, LiveEvent, LiveStream, MessageKind, ModerationEvent, Participant,
    Platform, Roles, YoutubeReaction,
};
use crate::moderation::{Moderator, Verdict};
use crate::poll::{
    Poll, PollDisplay, PollDisplaySettings, PollError, PollSnapshot, PollSource, PollStart,
    PreferenceChange,
};
use crate::moderation_error::{ModerationError, ModerationErrorCode};
use crate::sources::twitcasting_auth::{
    effective_client_id, AuthOptions, TwitcastingAuth, TwitcastingAuthError,
    TwitcastingAuthErrorCode, TwitcastingEndpoints, TwitcastingStatus, CREDENTIAL_LOCK_FILE,
    LOGOUT_MARKER_FILE,
};
use crate::sources::twitch_auth::{TwitchAuth, TwitchDeviceLogin, TwitchOauthStatus};
use crate::sources::youtube_send::{YoutubeAuth, YoutubeOauthStatus};
use crate::sources::SourceManager;
use crate::stats::{spawn_stats_aggregator, StatsSnapshot, TimerSnapshot, YoutubeMetadataUpdate};
use crate::tts::{
    bouyomi, voicevox,
    worker::{run_tts_worker, TtsShared},
    TtsBackend, TtsDispatcher, TtsNoticePayload, TtsQueueStatePayload,
};
use crate::update::{check_for_update, open_url};


/// アプリ全体で共有する実行時状態。
pub struct AppState {
    integrations: integrations::Integrations<integrations::KeyringSecretStore>,
    api_feed: api::ApiFeed,
    api_rate_limit: Arc<api::RateLimit>,
    webhook: webhook::WebhookRuntime<integrations::KeyringSecretStore, webhook::ReqwestSender>,
    /// 現在の設定。
    config: Mutex<AppConfig>,
    /// メイン画面からの通知取得を待つ起動時ロード結果。
    startup_config_outcome: Mutex<Option<ConfigLoadOutcome>>,
    startup_restore_failed: std::sync::atomic::AtomicBool,
    /// 起動時の復元が失敗した。通知を閉じても変わらない。復元元を残すため自動作成と整理を止める。
    restore_failed_at_startup: bool,
    restore_blocked: Vec<String>,
    backup_lock: Mutex<()>,
    last_auto_backup_failure: Mutex<Option<String>>,
    /// 設定の revision。`config` のロック内で、保存に成功するたびに `commit_config` が 1 進める。
    /// プロセス内だけの値で永続化しない。全体保存の楽観的排他に使う。
    config_revision: AtomicU64,
    /// OAuth と全体保存で、検証から永続化までを直列化する。
    config_write: tokio::sync::Mutex<()>,
    x_post_login: OAuthLoginGate,
    twitcasting_login: OAuthLoginGate,
    listeners: Arc<listeners::ListenerStore>,
    /// 実績イベントは評価から Bus への発行まで、このロックで直列化する。
    achievement_engine: Mutex<Option<Arc<AchievementEngine>>>,
    // 定義の保存 IPC(DefsStore::commit)は PR3 で足す。それまでは読まない。
    #[allow(dead_code)]
    achievement_defs: Arc<DefsStore>,
    chatlog: chatlog::ChatLog,
    /// カスタム効果音の保管庫。
    sounds: sounds::SoundStore,
    /// 効果音のアプリ全体の最小間隔。
    sound_gate: Mutex<sound_effects::SoundGate>,
    /// キーワードエフェクトの判定器(規則のコンパイル結果とクールダウン)。
    keyword_effects: Mutex<keyword_effects::KeywordEffects>,
    /// stats 集約などへ設定変更を伝える watch 送信端。
    config_tx: watch::Sender<AppConfig>,
    /// Timer overlay へ基準スナップショットを配る watch 送信端。
    timer_tx: watch::Sender<TimerSnapshot>,
    poll: Arc<Poll>,
    poll_control: Arc<poll_control::PollControl>,
    /// 設定の保存先ディレクトリ(app config dir)。
    config_dir: PathBuf,
    /// Source → パイプラインへ流す内部 broadcast 送信端。
    source_tx: broadcast::Sender<ChatMessage>,
    /// UI/OBS へ流す下流 Bus。
    bus: Bus,
    /// OBS テンプレート(内蔵 + カスタム)の保存先。
    templates: TemplateStore,
    /// ユーザー作成のGoals PNG背景を置く書き込み可能なディレクトリ。
    goals_skin_dir: PathBuf,
    /// アプリ全体の停止トークン。
    app_cancel: CancellationToken,
    /// OBS サーバの現在の待受状態。
    obs_server: Mutex<ObsServerControl>,
    /// チャンネル識別キー → そのチャンネルの停止トークン。
    channels: Mutex<HashMap<String, CancellationToken>>,
    /// モデレータ(手動非表示状態も保持)。
    moderator: Mutex<Moderator>,
    /// ワーカーと操作コマンドが共有する唯一の読み上げキュー。
    tts: Arc<TtsShared>,
    /// YouTube メタデータ poller → stats 集約への bounded 送信端。
    metadata_tx: mpsc::Sender<YoutubeMetadataUpdate>,
    /// YouTubeリアクション Source → 1フレームIPC forwarder の bounded 送信端。
    reaction_tx: mpsc::Sender<Vec<YoutubeReaction>>,
    /// プラットフォーム側の削除・BAN 通知 Source → moderation forwarder の bounded 送信端。
    mod_tx: mpsc::Sender<ModerationEvent>,
    /// 配信開始/終了イベント Source → fanout の bounded 送信端(満杯なら Source 側で破棄する)。
    live_tx: mpsc::Sender<LiveEvent>,
    twitcasting_started_movies: sources::twitcasting::StartedMovies,
    /// UI 操作(取消・承認・再試行)→ X 自動告知の駆動タスク。
    autopost_user_tx: mpsc::Sender<AutopostCommand>,
    /// X 自動告知の現在状態(駆動タスクが変化時だけ更新する)。
    autopost_status: watch::Receiver<AutopostStatus>,
    /// X 自動告知(API モード)の OAuth 状態。access token はメモリのみ、refresh token は keyring。
    x_post_auth: Arc<XPostAuth<KeyringTokenStore>>,
    /// 参加型配信の参加者一覧。
    participants: Mutex<Vec<Participant>>,
    /// YouTube の投稿・モデレーション・公式投票で共有する OAuth 状態。
    youtube_auth: Arc<YoutubeAuth>,
    /// Twitchモデレーション用OAuth状態。access tokenはメモリのみ。
    twitch_auth: Arc<TwitchAuth>,
    twitcasting_auth: Arc<TwitcastingAuth<integrations::KeyringSecretStore>>,
    twitcasting_api: Arc<sources::twitcasting_api::TwitcastingApi<integrations::KeyringSecretStore>>,
}

#[tauri::command]
fn get_comment_log_dir(state: State<'_, AppState>) -> String {
    chatlog::log_dir(&state.config_dir)
        .to_string_lossy()
        .into_owned()
}

#[tauri::command]
async fn open_comment_log_dir(state: State<'_, AppState>) -> Result<(), String> {
    let dir = chatlog::log_dir(&state.config_dir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| e.to_string())?;
    tauri::async_runtime::spawn_blocking(move || open_directory(&dir))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())
}

fn open_directory(dir: &std::path::Path) -> std::io::Result<()> {
    #[cfg(target_os = "windows")]
    let mut command = std::process::Command::new("explorer.exe");
    #[cfg(target_os = "macos")]
    let mut command = std::process::Command::new("open");
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    let mut command = std::process::Command::new("xdg-open");
    command.arg(dir).spawn().map(|_| ())
}

#[tauri::command]
async fn list_backups(state: State<'_, AppState>) -> Result<Vec<backup::BackupEntry>, String> {
    let dir = state.config_dir.clone();
    tauri::async_runtime::spawn_blocking(move || backup::read_backups(&dir))
        .await
        .map_err(|_| "バックアップの一覧を取得できませんでした".to_owned())?
        .map_err(|error| error.to_string())
}

#[tauri::command]
fn get_last_auto_backup_failure(state: State<'_, AppState>) -> Option<String> {
    state.last_auto_backup_failure.lock().unwrap().clone()
}

fn flush_backup_stores(state: &AppState) -> Result<(), backup::BackupError> {
    state.listeners.flush().map_err(|_| backup::BackupError::Io("listeners".into()))?;
    let engine = state.lock_achievement_engine();
    if let Some(engine) = engine.as_deref() {
        engine.flush().map_err(|_| backup::BackupError::Io("achievements".into()))?;
    }
    Ok(())
}

use backup::BackupTrigger;

fn run_backup(app: &AppHandle, trigger: BackupTrigger) -> Result<bool, backup::BackupError> {
    let state = app.state::<AppState>();
    let _guard = state.backup_lock.lock().unwrap();
    let settings = state.config_tx.borrow().backup;
    if state.app_cancel.is_cancelled()
        || (!settings.enabled && !matches!(trigger, BackupTrigger::Manual))
    {
        return Ok(false);
    }
    if !state.restore_blocked.is_empty() {
        return Err(backup::BackupError::Invalid(backup::RESTORE_SAVE_BLOCKED));
    }
    backup::run_backup(
        &state.config_dir,
        settings,
        trigger,
        state.restore_failed_at_startup,
        timer_now_ms(),
        || flush_backup_stores(&state),
    )
}

fn spawn_auto_backup(app: AppHandle, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(24 * 60 * 60));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut trigger = BackupTrigger::Startup;
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                _ = interval.tick() => {
                    let worker_app = app.clone();
                    let failure = match tauri::async_runtime::spawn_blocking(move || run_backup(&worker_app, trigger)).await {
                        Ok(Ok(_)) => None,
                        Ok(Err(error)) => Some(error.to_string()),
                        Err(_) => Some("バックアップの処理が中断されました".to_owned()),
                    };
                    if let Some(reason) = &failure {
                        tracing::warn!("自動バックアップに失敗しました: {reason}");
                    }
                    // 成功したら前回の失敗理由を消す。
                    let state = app.state::<AppState>();
                    let changed = {
                        let mut last = state.last_auto_backup_failure.lock().unwrap();
                        let changed = *last != failure;
                        *last = failure.clone();
                        changed
                    };
                    if changed {
                        let _ = app.emit("auto-backup-status", failure);
                    }
                    trigger = BackupTrigger::Interval;
                }
            }
        }
    });
}

#[tauri::command]
async fn create_backup_now(app: AppHandle) -> Result<bool, String> {
    tauri::async_runtime::spawn_blocking(move || run_backup(&app, BackupTrigger::Manual))
        .await
        .map_err(|_| "バックアップの作成が中断されました".to_owned())?
        .map_err(|error| {
            tracing::warn!("バックアップの作成に失敗しました: {error}");
            error.to_string()
        })
}

#[tauri::command]
async fn restore_backup(
    app: AppHandle,
    window: tauri::WebviewWindow,
    file_name: String,
) -> Result<(), String> {
    if window.label() != "main" {
        return Err("復元はメイン画面の設定から実行してください".into());
    }
    let worker_app = app.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let state = worker_app.state::<AppState>();
        let _guard = state.backup_lock.lock().unwrap();
        if state.app_cancel.is_cancelled() {
            return Err("アプリを終了しています".to_owned());
        }
        flush_backup_stores(&state)
            .and_then(|()| backup::stage_restore(&state.config_dir, &file_name, timer_now_ms()))
            .map_err(|error| {
                tracing::warn!("復元の準備に失敗しました: {error}");
                error.to_string()
            })?;
        state.app_cancel.cancel();
        Ok(())
    })
    .await
    .map_err(|_| "復元の準備が中断されました".to_owned())??;
    // メインスレッドの外で起動し、RunEvent::Exit の終了処理を経て再起動する。
    tauri::async_runtime::spawn_blocking(move || app.restart());
    Ok(())
}

#[tauri::command]
async fn open_backup_dir(state: State<'_, AppState>) -> Result<(), String> {
    let config_dir = state.config_dir.clone();
    const MESSAGE: &str = "バックアップフォルダを開けませんでした";
    tauri::async_runtime::spawn_blocking(move || {
        let dir = backup::auto_dir(&config_dir).map_err(|_| MESSAGE)?;
        fs::create_dir_all(&dir).map_err(|_| MESSAGE)?;
        open_directory(&dir).map_err(|_| MESSAGE)
    })
    .await
    .map_err(|_| MESSAGE.to_owned())?
    .map_err(str::to_owned)
}

#[derive(Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
enum RestoreFailureNotice {
    Failed,
    SavesBlocked,
}

#[tauri::command]
fn take_startup_restore_failure(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Option<RestoreFailureNotice> {
    if window.label() != "main" {
        return None;
    }
    if !state.restore_blocked.is_empty() {
        return Some(RestoreFailureNotice::SavesBlocked);
    }
    state.startup_restore_failed.swap(false, Ordering::Relaxed).then_some(RestoreFailureNotice::Failed)
}

#[tauri::command]
async fn get_listener(
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
) -> Result<Option<listeners::ListenerRecord>, String> {
    Ok(state.listeners.get(&platform, &author_id))
}

#[tauri::command]
async fn listener_donation(
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
) -> Result<serde_json::Value, String> {
    let donation = state.listeners.donation(&platform, &author_id);
    let defs = state.achievement_defs.current();
    // Listener totals already use the currency assigned when each gift was observed.
    let summary = listeners::donation_summary(&donation, &defs.rates, &Default::default());
    let totals: Vec<_> = summary
        .totals
        .into_iter()
        .map(|(currency, milli)| serde_json::json!({ "currency": currency, "milli": milli }))
        .collect();
    Ok(serde_json::json!({
        "totals": totals,
        "jpyEstimateMilli": summary.jpy_estimate_milli,
        "unconvertible": summary.unconvertible,
    }))
}

#[tauri::command]
async fn reset_listener_donation(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
) -> Result<bool, String> {
    state.ensure_restore_writable("listeners.json")?;
    let reset = state.listeners.reset_donation(&platform, &author_id);
    if reset {
        emit_listener_donation_changed(&app);
    }
    Ok(reset)
}

#[tauri::command]
async fn reset_all_listener_donations(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<usize, String> {
    state.ensure_restore_writable("listeners.json")?;
    let count = state.listeners.reset_all_donations();
    if count > 0 {
        emit_listener_donation_changed(&app);
    }
    Ok(count)
}

fn emit_listener_donation_changed(app: &AppHandle) {
    if app.emit("listener-donation-changed", ()).is_err() {
        tracing::warn!("listener-donation-changed emit 失敗");
    }
}

#[tauri::command]
async fn update_listener(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
    nickname: Option<String>,
    label: Option<String>,
    memo: Option<String>,
    tts_muted: bool,
) -> Result<listeners::ListenerRecord, String> {
    state.ensure_restore_writable("listeners.json")?;
    let record = state
        .listeners
        .update(&platform, &author_id, nickname, label, memo, tts_muted);
    if let Err(e) = app.emit("listener-updated", &record) {
        tracing::warn!("listener-updated emit 失敗: {e}");
    }
    Ok(record)
}

#[tauri::command]
async fn set_listener_voice(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
    voice: Option<listeners::TtsVoiceOverride>,
) -> Result<listeners::ListenerRecord, String> {
    state.ensure_restore_writable("listeners.json")?;
    let record = state.listeners.set_voice(&platform, &author_id, voice);
    if let Err(e) = app.emit("listener-updated", &record) {
        tracing::warn!("listener-updated emit 失敗: {e}");
    }
    Ok(record)
}

#[tauri::command]
async fn list_listeners(
    state: State<'_, AppState>,
    query: Option<String>,
    limit: u32,
) -> Result<Vec<listeners::ListenerRecord>, String> {
    Ok(state.listeners.list(query.as_deref(), limit))
}

#[tauri::command]
async fn delete_listener(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
) -> Result<bool, String> {
    // 台帳と実績の進捗の片方だけを消して確定させない。
    state.ensure_restore_writable("listeners.json")?;
    state.ensure_restore_writable("achievement-progress.json")?;
    let deleted = state.listeners.delete(&platform, &author_id);
    if deleted {
        if let Err(e) = app.emit(
            "listener-updated",
            serde_json::json!({"platform": platform, "authorId": author_id, "deleted": true}),
        ) {
            tracing::warn!("listener-updated emit 失敗: {e}");
        }
    }
    tauri::async_runtime::spawn_blocking(move || {
        let state = app.state::<AppState>();
        let engine = state.lock_achievement_engine();
        delete_listener_achievement_progress(
            engine.as_deref(), &state.config_dir, &platform, &author_id,
        )
        .map(|()| deleted)
        .map_err(|_| "実績のリスナー記録を削除できませんでした".to_owned())
    })
    .await
    .map_err(|_| "実績のリスナー記録を削除できませんでした".to_owned())?
}

#[tauri::command]
fn list_achievements(state: State<'_, AppState>) -> Result<achievements::ipc::AchievementList, String> {
    // 外部で更新されていればディスクから読み直す(Stale のあとの再読込もここを通る)。
    // 外部の無効化で進捗を保存できない時・壊れた定義を退避できない時は取り込まず、エンジンを残したままエラーにする。
    let unlocks = achievements::ipc::refresh(
        &state.achievement_defs, &state.achievement_engine, &state.config_dir, timer_now_ms(),
    )
    .map_err(|_| "実績のデータを読み書きできませんでした。保存先を確認して再読込してください".to_owned())?;
    state.broadcast_achievement_unlocks(unlocks);
    let engine = state.lock_achievement_engine();
    Ok(achievements::ipc::list(state.achievement_defs.snapshot(), engine.as_deref(), &state.config_dir))
}

#[tauri::command]
fn achievement_schema() -> achievements::ipc::AchievementSchema {
    achievements::ipc::schema()
}

#[tauri::command]
fn commit_achievements(
    state: State<'_, AppState>,
    base_revision: u64,
    base_token: String,
    enabled: bool,
    achievements: Vec<achievements::ipc::AchievementInput>,
    rates: achievements::currency::RateTable,
    symbol_overrides: std::collections::BTreeMap<String, String>,
) -> achievements::ipc::CommitReply {
    if state.ensure_restore_writable("achievements.json").is_err()
        || state.ensure_restore_writable("achievement-progress.json").is_err()
    {
        return achievements::ipc::CommitReply::SaveRefused;
    }
    let draft = achievements::ipc::AchievementDraft { enabled, achievements, rates, symbol_overrides };
    let (reply, unlocks) = achievements::ipc::commit_draft(
        &state.achievement_defs,
        &state.achievement_engine,
        &state.config_dir,
        base_revision,
        Some(&base_token),
        draft,
        timer_now_ms(),
    );
    state.broadcast_achievement_unlocks(unlocks);
    reply
}

#[tauri::command]
fn new_achievement(subject: achievements::model::SubjectKind) -> achievements::model::Achievement {
    achievements::ipc::new_achievement(subject)
}

#[tauri::command]
fn duplicate_achievement(achievement: achievements::ipc::AchievementInput) -> Option<achievements::model::Achievement> {
    achievements::ipc::duplicate_draft(achievement)
}

#[tauri::command]
fn reset_achievement_to_preset(state: State<'_, AppState>, id: String) -> Option<achievements::model::Achievement> {
    achievements::ipc::reset_by_id(&state.achievement_defs.current(), &id)
}

#[tauri::command]
fn listener_achievements(
    state: State<'_, AppState>,
    platform: String,
    author_id: String,
) -> Vec<achievements::ipc::ListenerTrophy> {
    let engine = state.lock_achievement_engine();
    achievements::ipc::listener_trophies(
        &state.achievement_defs.current(), engine.as_deref(), &state.config_dir, &platform, &author_id,
    )
}

fn delete_listener_achievement_progress(
    engine: Option<&AchievementEngine>,
    dir: &std::path::Path,
    platform: &str,
    author_id: &str,
) -> anyhow::Result<()> {
    match engine {
        Some(engine) => engine.forget_listener(platform, author_id),
        None => achievements::store::delete_listener_progress(dir, platform, author_id).map(|_| ()),
    }
}

/// OBS サーバの再起動に必要な実行時ハンドル。
struct ObsServerControl {
    port: u16,
    cancel: CancellationToken,
}

enum AchievementEvent<'a> {
    Chat(ChatMessage),
    Live(&'a LiveEvent),
    Stats(&'a StatsSnapshot),
}

fn publish_achievement_event(
    engine: &Mutex<Option<Arc<AchievementEngine>>>,
    bus: &Bus,
    cancel: &CancellationToken,
    event: AchievementEvent<'_>,
    publish_chat: impl FnOnce(ChatMessage) -> u64,
) {
    // 保存結果と再試行状態は評価中に確定する。chat 発行まで同じロックを保持する。
    let engine = engine
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let unlocks = match engine.as_deref().filter(|_| !cancel.is_cancelled()) {
        Some(engine) => {
            let now_ms = timer_now_ms();
            match &event {
                AchievementEvent::Chat(msg) => engine.observe(msg, now_ms),
                AchievementEvent::Live(event) => engine.on_live_event(event, now_ms),
                AchievementEvent::Stats(snapshot) => engine.on_stats(snapshot, now_ms),
            }
        }
        None => Vec::new(),
    };

    // 下流(UI/OBS)へ。解除は原因の chat の後に流す。
    let chat_seq = match event {
        AchievementEvent::Chat(msg) => Some(publish_chat(msg)),
        _ => None,
    };
    if !unlocks.is_empty() {
        let after_seq = chat_seq.unwrap_or_else(|| bus.latest_chat_seq());
        for unlock in unlocks {
            bus.publish_achievement(unlock, after_seq);
        }
    }
}

impl AppState {
    fn ensure_restore_writable(&self, root: &str) -> Result<(), String> {
        if self.restore_blocked.iter().any(|blocked| blocked == root) {
            return Err(backup::RESTORE_SAVE_BLOCKED.into());
        }
        Ok(())
    }

    /// チャンネルの一意キー(プラットフォーム + 識別子)。
    fn channel_key(ch: &ChannelConfig) -> String {
        config::channel_key(ch)
    }

    /// エンジンの外側のロック。パニックで毒化しても、エンジン内部と同じく中身を取り出して続ける。
    fn lock_achievement_engine(&self) -> std::sync::MutexGuard<'_, Option<Arc<AchievementEngine>>> {
        self.achievement_engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn publish_achievement_event(&self, event: AchievementEvent<'_>) {
        publish_achievement_event(
            &self.achievement_engine,
            &self.bus,
            &self.app_cancel,
            event,
            |msg| self.bus.publish_sequenced(msg),
        );
    }

    /// 定義の保存・再読込・無効化で出た解除を流す。原因の chat が無いので、on_stats と同じく
    /// その時点の最新の chat 番号の後に並べる。
    fn broadcast_achievement_unlocks(
        &self,
        unlocks: Vec<crate::achievements::engine::AchievementUnlocked>,
    ) {
        if unlocks.is_empty() {
            return;
        }
        let after_seq = self.bus.latest_chat_seq();
        for unlock in unlocks {
            self.bus.publish_achievement(unlock, after_seq);
        }
    }

    fn flush_achievements(&self) {
        let engine = self.lock_achievement_engine();
        if let Some(engine) = engine.as_deref() {
            if engine.flush().is_err() {
                tracing::error!("実績の進捗を保存できませんでした");
            }
        }
    }
}

fn emit_tts_notice(app: &AppHandle, level: &'static str, message: String) {
    let payload = TtsNoticePayload { level, message };
    if let Err(e) = app.emit("tts-notice", payload) {
        tracing::warn!("tts-notice emit 失敗: {e}");
    }
}

fn current_tts_queue_state(state: &AppState) -> TtsQueueStatePayload {
    state.tts.snapshot()
}

fn try_enqueue_tts_message(
    sink: &dyn tts::TtsSink,
    config: &Mutex<AppConfig>,
    queue: &TtsShared,
    msg: ChatMessage,
) {
    enqueue_tts_inner(sink, config, queue, msg, false);
}

/// `skip_when_backend_none` は実績の読み上げ用。バックエンドの確認を投入と同じ設定ロック内で行い、
/// 確認後に OFF へ変わっても待ちを押し出さないようにする。
fn enqueue_tts_inner(
    sink: &dyn tts::TtsSink,
    config: &Mutex<AppConfig>,
    queue: &TtsShared,
    mut msg: ChatMessage,
    skip_when_backend_none: bool,
) {
    if !listeners::allows_tts(&msg) {
        return;
    }
    // 設定ロックを持ったまま投入する。枠の読み上げ OFF の保存(キューから除く)と順序を揃えるため。
    // UI への通知はロックを放してから出す。
    let result = {
        let config = config.lock().unwrap();
        if skip_when_backend_none && config.tts.backend == TtsBackendKind::None {
            return;
        }
        let overrides = tts::origin_overrides(&config, &msg);
        if !tts::overrides_allow_tts(overrides) {
            return;
        }
        msg.route.channel_tts_voice = overrides.and_then(|o| o.tts_voice.clone());
        let max_pending = config.tts.options.queue.max_pending as usize;
        queue.enqueue_silent(msg, max_pending)
    };
    queue.after_enqueue(sink, &result);
}

/// 実績の読み上げ。TTS バックエンドが `None` の時はキューにも通知にも触れない。
fn try_enqueue_achievement_tts(
    sink: &dyn tts::TtsSink,
    config: &Mutex<AppConfig>,
    queue: &TtsShared,
    msg: ChatMessage,
) {
    enqueue_tts_inner(sink, config, queue, msg, true);
}

/// 読み上げ OFF の枠の未読み上げコメントを捨てる。設定ロック中(保存の直後)に呼び、
/// true が返ったらロックを放してから `emit_queue_state` する。
fn purge_disabled_channel_tts(cfg: &AppConfig, queue: &TtsShared) -> bool {
    queue.remove_disabled_channels(cfg) > 0
}

fn emit_bouyomi_launch_notice(app: &AppHandle, outcome: bouyomi::LaunchOutcome) {
    match outcome {
        bouyomi::LaunchOutcome::Launched => emit_tts_notice(
            app,
            "info",
            "棒読みちゃんを自動起動しました".to_string(),
        ),
        bouyomi::LaunchOutcome::NeedsElevation => emit_tts_notice(
            app,
            "error",
            "棒読みちゃんの自動起動には管理者権限が必要です。棒読みちゃんを手動で管理者として起動するか、設定で「管理者として起動する」を有効化してください（有効化時はUAC確認が出ます）".to_string(),
        ),
        bouyomi::LaunchOutcome::Failed(e) => emit_tts_notice(
            app,
            "warn",
            format!("棒読みちゃんの自動起動に失敗しました: {e}（パス/権限を確認してください）"),
        ),
        bouyomi::LaunchOutcome::NoPath
        | bouyomi::LaunchOutcome::AlreadyRunning
        | bouyomi::LaunchOutcome::CooldownSkip => {}
    }
}

async fn ensure_bouyomi_launched_and_emit_notice(
    app: AppHandle,
    path: String,
    host: String,
    port: u16,
    elevated: bool,
) {
    let launch = tauri::async_runtime::spawn_blocking(move || {
        let outcome = bouyomi::ensure_launched(path, host, port, elevated);
        (app, outcome)
    });

    match launch.await {
        Ok((app, outcome)) => emit_bouyomi_launch_notice(&app, outcome),
        Err(e) => tracing::warn!("棒読みちゃん自動起動タスク失敗: {e}"),
    }
}

async fn wait_for_bouyomi_socket(host: &str, port: u16) -> bool {
    for _ in 0..20 {
        if TcpStream::connect((host, port)).is_ok() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    false
}

fn clear_tts_queue_impl(app: &AppHandle, state: &AppState) {
    state.tts.clear(app);
}
fn skip_current_tts_impl(app: &AppHandle, state: &AppState) {
    state.tts.skip_current(app);
}

struct TauriAppOps(AppHandle);
impl api::AppOps for TauriAppOps {
    fn inject_test_comments(
        &self,
        req: &api::TestCommentRequest,
    ) -> Result<Vec<String>, api::OpsError> {
        inject_test_comments_impl(&self.0.state::<AppState>(), req).map_err(api::OpsError)
    }
    fn tts_state(&self) -> TtsQueueStatePayload {
        current_tts_queue_state(&self.0.state::<AppState>())
    }
    fn tts_set_paused(&self, paused: bool) {
        set_tts_paused_impl(&self.0, &self.0.state::<AppState>(), paused);
    }
    fn tts_skip(&self) {
        skip_current_tts_impl(&self.0, &self.0.state::<AppState>());
    }
    fn tts_clear(&self) {
        clear_tts_queue_impl(&self.0, &self.0.state::<AppState>());
    }
    fn timer_state(&self) -> TimerSnapshot {
        self.0.state::<AppState>().timer_tx.borrow().clone()
    }
    fn timer_control(
        &self,
        action: api::TimerAction,
        duration_sec: Option<u32>,
    ) -> Result<TimerSnapshot, api::OpsError> {
        Ok(control_timer_impl(
            &self.0.state::<AppState>(),
            action,
            duration_sec,
        ))
    }
}

// ============ Tauri コマンド ============

impl AppState {
    fn commit_config(&self, current: &mut AppConfig, next: AppConfig) -> Result<u64, ConfigCommitError> {
        commit_config(&self.config_dir, &self.config_revision, &self.config_tx, current, next, None)
    }

    fn save_poll_preferences(&self, change: PreferenceChange<'_>) -> Result<(), PollError> {
        let mut current = self.config.lock().unwrap();
        let mut next = current.clone();
        match change {
            PreferenceChange::Started(preferences) => next.poll = preferences.clone(),
            PreferenceChange::Display(settings) => next.poll.settings = settings,
        }
        if current.poll != next.poll {
            self.commit_config(&mut current, next)
                .map_err(|_| PollError::Save)?;
        }
        Ok(())
    }
}

/// 書込 IPC の応答。`revision` は保存後の値、`saved` は書いた部分の正規化済みの値。
/// 画面は `revision` が自分の基準 + 1 ちょうどの時だけ `saved` を反映して基準を進める。
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigCommit<T> {
    revision: Option<u64>,
    #[serde(flatten)]
    saved: T,
}

impl<T> ConfigCommit<T> {
    fn new(revision: Option<u64>, saved: T) -> Self {
        Self { revision, saved }
    }
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthClientSaved {
    client_id: Option<String>,
    callback_port: u16,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigSnapshot {
    config: AppConfig,
    revision: u64,
}

/// 現在の設定と、その revision を取得する。
#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Result<ConfigSnapshot, String> {
    let config = state.config.lock().unwrap();
    Ok(ConfigSnapshot {
        config: config.clone(),
        revision: state.config_revision.load(Ordering::SeqCst),
    })
}

/// 通知の購読準備ができた画面へ、起動時の結果に対応する通知を一度だけ返す。
#[tauri::command]
fn take_startup_config_notice(state: State<'_, AppState>) -> Option<TtsNoticePayload> {
    take_config_load_notice(&state.startup_config_outcome)
}

fn take_config_load_notice(outcome: &Mutex<Option<ConfigLoadOutcome>>) -> Option<TtsNoticePayload> {
    let message = outcome.lock().unwrap().take()?.notice()?;
    Some(TtsNoticePayload {
        level: "warn",
        message: message.to_owned(),
    })
}

/// TTS 読み上げを一時停止/再開する。paused 中に届いたメッセージは保持せず破棄する。
#[tauri::command]
fn set_tts_paused(app: AppHandle, state: State<'_, AppState>, paused: bool) {
    set_tts_paused_impl(&app, &state, paused);
}

fn set_tts_paused_impl(app: &AppHandle, state: &AppState, paused: bool) {
    state.tts.set_paused(app, paused);

    let tts_config = state.config.lock().unwrap().tts.clone();
    if tts_config.backend == TtsBackendKind::Bouyomi {
        let options = tts_config.options;
        let backend = bouyomi::BouyomiBackend::new(
            options.bouyomi_host,
            options.bouyomi_port,
            options.bouyomi_speed,
            options.bouyomi_tone,
            options.bouyomi_volume,
            options.bouyomi_voice,
        );
        let app_for_notice = app.clone();
        tauri::async_runtime::spawn(async move {
            let result = if paused {
                backend.pause_and_clear().await
            } else {
                backend.resume().await
            };
            if let Err(e) = result {
                emit_tts_notice(
                    &app_for_notice,
                    "warn",
                    format!(
                        "棒読みちゃんの{}命令に失敗しました: {e}",
                        if paused { "停止" } else { "再開" }
                    ),
                );
            }
        });
    }
}

/// 現在の TTS 待ちキュー状態を取得する。
#[tauri::command]
fn get_tts_queue_state(state: State<'_, AppState>) -> TtsQueueStatePayload {
    current_tts_queue_state(&state)
}

/// 未送出の TTS キューと合成を破棄し、UI の現在発話も即時停止させる。
#[tauri::command]
fn clear_tts_queue(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    clear_tts_queue_impl(&app, &state);
    Ok(())
}

/// 再生中ならその発話を停止し、それ以外は合成中の一件を破棄する。
#[tauri::command]
fn skip_current_tts(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    skip_current_tts_impl(&app, &state);
    Ok(())
}

/// WebView の Clipboard API を介さず OS のクリップボードへ書き込む。
#[tauri::command]
fn copy_text_to_clipboard(text: String) -> Result<(), String> {
    arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.set_text(text))
        .map_err(|e| format!("クリップボード書き込み失敗: {e}"))
}

/// 任意テキストを TTS キューへ直接投入する。UI/OBS には流さない。
#[tauri::command]
fn tts_speak_text(app: AppHandle, state: State<'_, AppState>, text: String) -> Result<(), String> {
    if text.trim().is_empty() {
        return Ok(());
    }
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("現在時刻取得失敗: {e}"))?
        .as_millis();
    let timestamp_ms = i64::try_from(epoch_millis).unwrap_or(i64::MAX);
    let msg = ChatMessage {
        listener: None,
        native_id: false,
        id: format!("tts-text-{epoch_millis}"),
        platform: Platform::Youtube,
        channel: "system".to_string(),
        author: Author {
            nickname: None,
            avatar_url: None,
            id: String::new(),
            name: String::new(),
            display_color: None,
            badges: Vec::new(),
            roles: Roles::default(),
        },
        fragments: vec![Fragment::text(text)],
        kind: MessageKind::System,
        amount: None,
        timestamp_ms,
        raw: None,
        skip_tts: false,
        injected_test: false,
        route: Default::default(),
        sound: None,
    };

    try_enqueue_tts_message(&app, &state.config, &state.tts, msg);
    Ok(())
}

/// 現在の TTS 設定で棒読みちゃんへ疎通テストの読み上げを送る。
#[tauri::command]
async fn test_tts(state: State<'_, AppState>) -> Result<String, String> {
    let tts = {
        let cfg = state.config.lock().unwrap();
        cfg.tts.clone()
    };

    let backend_kind = tts.backend;
    let opt = tts.options;
    match backend_kind {
        TtsBackendKind::Bouyomi => {
            let host = opt.bouyomi_host.clone();
            let port = opt.bouyomi_port;
            let path = opt.bouyomi_path.clone();
            let elevated = opt.bouyomi_launch_elevated;
            let launch_host = host.clone();
            let launch_outcome = tauri::async_runtime::spawn_blocking(move || {
                bouyomi::ensure_launched(path, launch_host, port, elevated)
            })
            .await
            .map_err(|e| format!("棒読みちゃんの自動起動処理に失敗しました: {e}"))?;

            if matches!(launch_outcome, bouyomi::LaunchOutcome::NeedsElevation) {
                return Err(
                    "棒読みちゃんの自動起動には管理者権限が必要です。棒読みちゃんを手動で管理者として起動するか、設定で「管理者として起動する」を有効化してください（有効化時はUAC確認が出ます）".to_string()
                );
            }

            if let bouyomi::LaunchOutcome::Failed(e) = &launch_outcome {
                return Err(format!(
                    "棒読みちゃんの自動起動に失敗しました: {e}。BouyomiChan のパス/権限を確認してください"
                ));
            }

            if !wait_for_bouyomi_socket(&host, port).await {
                return Err(format!(
                    "棒読みちゃんに接続できません: {host}:{port}。BouyomiChan のパス、起動状態、または『プログラム連携 → ソケット通信を許可』とポート設定を確認してください"
                ));
            }

            let backend = bouyomi::BouyomiBackend::new(
                host.clone(),
                port,
                opt.bouyomi_speed,
                opt.bouyomi_tone,
                opt.bouyomi_volume,
                opt.bouyomi_voice,
            );

            match backend.speak("読み上げテストです".to_string()).await {
                Ok(()) => {
                    let launch_note = if matches!(launch_outcome, bouyomi::LaunchOutcome::Launched)
                    {
                        " (自動起動しました)"
                    } else {
                        ""
                    };
                    Ok(format!(
                        "棒読みちゃんへ送出しました ({host}:{port}){launch_note}"
                    ))
                }
                Err(e) => {
                    if let Some(detail) = bouyomi::connect_error_detail(&e) {
                        Err(format!(
                            "棒読みちゃんに接続できません: {detail}。BouyomiChan を起動し『プログラム連携 → ソケット通信を許可』を ON に、ポートが {port} か確認してください"
                        ))
                    } else {
                        Err(format!("棒読みちゃんへの送出に失敗しました: {e}"))
                    }
                }
            }
        }
        TtsBackendKind::Voicevox => Ok(
            "現在のバックエンドは VOICEVOX です。棒読みちゃんを選択するとテストできます"
                .to_string(),
        ),
        TtsBackendKind::WebSpeech => Ok(
            "現在のバックエンドは Web Speech です。棒読みちゃんを選択するとテストできます"
                .to_string(),
        ),
        TtsBackendKind::None => {
            Ok("現在、読み上げはOFFです。棒読みちゃんを選択するとテストできます".to_string())
        }
    }
}

/// VOICEVOX 互換エンジンの `/version` と `/speakers` を取得する(設定フォームの未保存値で接続テスト)。
///
/// `url` が空ならエンジン種別の既定 URL を使う。
#[tauri::command]
async fn probe_tts_engine(
    engine: VoicevoxEngineKind,
    url: String,
) -> Result<voicevox::TtsEngineProbe, String> {
    let url = match url.trim() {
        "" => match engine {
            VoicevoxEngineKind::Voicevox => "http://127.0.0.1:50021",
            VoicevoxEngineKind::AivisSpeech => "http://127.0.0.1:10101",
        }
        .to_string(),
        u => u.to_string(),
    };
    voicevox::VoicevoxClient::new(&url)
        .probe()
        .await
        .map_err(|e| format!("エンジンに接続できません ({url}): {e}"))
}

/// 現在のコメントログ CSV を config dir 配下 `exports/` に書き出す。
#[tauri::command]
fn export_comments_csv(app: AppHandle, csv: String) -> Result<String, String> {
    let config_dir = app
        .path()
        .app_config_dir()
        .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let exports_dir = config_dir.join("exports");
    fs::create_dir_all(&exports_dir)
        .map_err(|e| format!("CSV出力ディレクトリ作成失敗 {}: {e}", exports_dir.display()))?;

    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("現在時刻取得失敗: {e}"))?
        .as_millis();
    let path = exports_dir.join(format!("comments_{epoch_millis}.csv"));
    let mut body = String::with_capacity(csv.len() + 3);
    body.push('\u{feff}');
    body.push_str(&csv);
    fs::write(&path, body.as_bytes())
        .map_err(|e| format!("CSVファイル書込失敗 {}: {e}", path.display()))?;

    Ok(path.to_string_lossy().into_owned())
}

/// チャットへコメントを投稿する。
#[tauri::command]
async fn send_chat_message(
    state: State<'_, AppState>,
    platform: String,
    channel: String,
    text: String,
) -> Result<(), String> {
    match platform.trim().to_lowercase().as_str() {
        "twitch" => {
            let (oauth, username, target_channel) = {
                let cfg = state.config.lock().unwrap();
                let target_channel = if channel.trim().is_empty() {
                    cfg.channels
                        .iter()
                        .find(|ch| ch.platform == ChannelPlatform::Twitch && ch.enabled)
                        .or_else(|| {
                            cfg.channels
                                .iter()
                                .find(|ch| ch.platform == ChannelPlatform::Twitch)
                        })
                        .map(|ch| ch.identifier.clone())
                        .unwrap_or_default()
                } else {
                    channel.trim().to_string()
                };
                (
                    cfg.credentials.twitch_oauth.clone(),
                    cfg.credentials.twitch_username.clone(),
                    target_channel,
                )
            };

            crate::sources::twitch_send::send_twitch_message(
                &target_channel,
                &text,
                &oauth,
                &username,
            )
            .await
            .map_err(|e| e.to_string())
        }
        "youtube" => {
            let (client_id, target_channel) = {
                let cfg = state.config.lock().unwrap();
                let target_channel = if channel.trim().is_empty() {
                    cfg.channels
                        .iter()
                        .find(|ch| ch.platform == ChannelPlatform::Youtube && ch.enabled)
                        .or_else(|| {
                            cfg.channels
                                .iter()
                                .find(|ch| ch.platform == ChannelPlatform::Youtube)
                        })
                        .map(|ch| ch.identifier.clone())
                        .unwrap_or_default()
                } else {
                    channel.trim().to_string()
                };
                (
                    cfg.credentials.youtube_oauth_client_id.clone(),
                    target_channel,
                )
            };
            state
                .youtube_auth
                .send_message(&client_id, &target_channel, &text)
                .await
        }
        other => Err(format!("不明なplatformです: {other}")),
    }
}

/// YouTube投稿用Google OAuthの接続状態を取得する。
#[tauri::command]
async fn get_youtube_oauth_status(
    state: State<'_, AppState>,
) -> Result<YoutubeOauthStatus, String> {
    let client_id = state
        .config
        .lock()
        .unwrap()
        .credentials
        .youtube_oauth_client_id
        .clone();
    state.youtube_auth.status(&client_id).await
}

/// システムブラウザを開き、YouTube投稿権限をGoogleアカウントから取得する。
#[tauri::command]
async fn connect_youtube_oauth(
    state: State<'_, AppState>,
    client_id: String,
    on_config_saved: tauri::ipc::Channel<ConfigCommit<serde_json::Value>>,
) -> Result<YoutubeOauthStatus, String> {
    let client_id = client_id.trim().to_string();
    if client_id.is_empty() {
        return Err("YouTube OAuth クライアントIDを設定してください".to_string());
    }
    // OAuthボタンはこの項目だけを保存する。他の未保存設定を巻き込まない。
    let changed = {
        let mut cfg = state.config.lock().unwrap();
        let changed = cfg.credentials.youtube_oauth_client_id != client_id;
        if changed {
            let mut next = cfg.clone();
            next.credentials.youtube_oauth_client_id = client_id.clone();
            let revision = state.commit_config(&mut cfg, next).map_err(|e| e.to_string())?;
            let saved = serde_json::json!({ "youtubeOauthClientId": cfg.credentials.youtube_oauth_client_id });
            let _ = on_config_saved.send(ConfigCommit::new(Some(revision), saved));
        }
        changed
    };
    if changed {
        state.youtube_auth.clear_caches().await;
    }
    state.youtube_auth.connect(&client_id).await
}

/// このPCに保存したYouTube投稿用認証を削除する。
#[tauri::command]
async fn disconnect_youtube_oauth(
    state: State<'_, AppState>,
) -> Result<YoutubeOauthStatus, String> {
    let client_id = state
        .config
        .lock()
        .unwrap()
        .credentials
        .youtube_oauth_client_id
        .clone();
    state.youtube_auth.disconnect(&client_id).await
}

/// Twitch モデレーションの接続状態を取得する。
#[tauri::command]
async fn get_twitch_oauth_status(
    state: State<'_, AppState>,
) -> Result<TwitchOauthStatus, String> {
    let client_id = state
        .config
        .lock()
        .unwrap()
        .credentials
        .twitch_client_id
        .clone();
    state.twitch_auth.status(&client_id).await
}

/// Twitch の Device Code Flow を開始し、システムブラウザで認証ページを開く。
#[tauri::command]
async fn begin_twitch_login(state: State<'_, AppState>) -> Result<TwitchDeviceLogin, String> {
    let client_id = state
        .config
        .lock()
        .unwrap()
        .credentials
        .twitch_client_id
        .trim()
        .to_string();
    let login = state.twitch_auth.begin_login(&client_id).await?;
    // ブラウザを開けなくても、UI に出したコードと URL で手動認証できるので続行する。
    if crate::update::open_url(login.verification_uri.clone()).is_err() {
        tracing::warn!("Twitchの認証ページを開けませんでした");
    }
    Ok(login)
}

/// 開始済みの Twitch ログインを完了・期限切れ・取消まで待つ。
#[tauri::command]
async fn await_twitch_login(state: State<'_, AppState>) -> Result<TwitchOauthStatus, String> {
    state.twitch_auth.await_login().await
}

/// 進行中の Twitch ログインを取り消す。
#[tauri::command]
async fn cancel_twitch_login(state: State<'_, AppState>) -> Result<(), String> {
    state.twitch_auth.cancel_login().await;
    Ok(())
}

/// このPCに保存した Twitch モデレーション用認証を削除する。
#[tauri::command]
async fn disconnect_twitch_oauth(
    state: State<'_, AppState>,
) -> Result<TwitchOauthStatus, String> {
    let client_id = state
        .config
        .lock()
        .unwrap()
        .credentials
        .twitch_client_id
        .clone();
    state.twitch_auth.disconnect(&client_id).await
}

/// 配信側のコメントを削除する(YouTube は `LCC.` 形式の受信分のみ)。
#[tauri::command]
async fn moderate_delete_message(
    state: State<'_, AppState>,
    platform: String,
    channel: String,
    message_id: String,
    author_id: String,
) -> Result<(), ModerationError> {
    let _ = author_id;
    match platform.trim().to_lowercase().as_str() {
        "youtube" => {
            let client_id = state
                .config
                .lock()
                .unwrap()
                .credentials
                .youtube_oauth_client_id
                .trim()
                .to_string();
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でYouTube OAuthクライアントIDを入力し、Googleに接続してください",
                ));
            }
            let video_id = crate::sources::youtube::extract_video_id(&channel);
            state
                .youtube_auth
                .delete_message(&client_id, &video_id, &message_id)
                .await?;
            state.moderator.lock().unwrap().hide_id(message_id.as_str());
            send_app_moderation(
                &state,
                ModerationEvent::MessageDeleted {
                    platform: Platform::Youtube,
                    channel,
                    message_id,
                },
            );
            Ok(())
        }
        "twitch" => {
            let (client_id, client_secret) = {
                let cfg = state.config.lock().unwrap();
                (
                    cfg.credentials.twitch_client_id.trim().to_string(),
                    cfg.credentials.twitch_client_secret.trim().to_string(),
                )
            };
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でTwitchのクライアントIDを入力し、Twitchにログインしてください",
                ));
            }
            state
                .twitch_auth
                .delete_message(&client_id, &client_secret, &channel, &message_id)
                .await?;
            state.moderator.lock().unwrap().hide_id(message_id.as_str());
            send_app_moderation(
                &state,
                ModerationEvent::MessageDeleted {
                    platform: Platform::Twitch,
                    channel,
                    message_id,
                },
            );
            Ok(())
        }
        _ => Err(ModerationError::new(
            ModerationErrorCode::Unavailable,
            "このプラットフォームのモデレーションには対応していません",
        )),
    }
}

/// ユーザーを BAN する。`duration_secs: None` は永久 BAN。
#[tauri::command]
async fn moderate_ban_user(
    state: State<'_, AppState>,
    platform: String,
    channel: String,
    author_id: String,
    duration_secs: Option<u32>,
    reason: Option<String>,
) -> Result<(), ModerationError> {
    // YouTube の liveChatBans には reason 項目が無いため、この値は使わない。
    let (target_platform, result) = match platform.trim().to_lowercase().as_str() {
        "youtube" => {
            let client_id = state
                .config
                .lock()
                .unwrap()
                .credentials
                .youtube_oauth_client_id
                .trim()
                .to_string();
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でYouTube OAuthクライアントIDを入力し、Googleに接続してください",
                ));
            }
            let video_id = crate::sources::youtube::extract_video_id(&channel);
            let result = state
                .youtube_auth
                .ban_user(&client_id, &video_id, author_id.trim(), duration_secs)
                .await;
            (Platform::Youtube, result)
        }
        "twitch" => {
            let (client_id, client_secret) = {
                let cfg = state.config.lock().unwrap();
                (
                    cfg.credentials.twitch_client_id.trim().to_string(),
                    cfg.credentials.twitch_client_secret.trim().to_string(),
                )
            };
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でTwitchのクライアントIDを入力し、Twitchにログインしてください",
                ));
            }
            let result = state
                .twitch_auth
                .ban_user(
                    &client_id,
                    &client_secret,
                    &channel,
                    author_id.trim(),
                    duration_secs,
                    reason.as_deref(),
                )
                .await;
            (Platform::Twitch, result)
        }
        _ => {
            return Err(ModerationError::new(
                ModerationErrorCode::Unavailable,
                "このプラットフォームのモデレーションには対応していません",
            ))
        }
    };
    if result.is_ok() {
        // 配信側エコーを待たず OBS・TTS へ反映する(エコーと二重でも冪等)。
        // アプリの時計とコメントのサーバー時刻は別物なので境界は付けない。
        send_app_moderation(
            &state,
            ModerationEvent::UserCleared {
                platform: target_platform,
                channel,
                author_id: author_id.trim().to_string(),
                duration_secs: duration_secs.map(u64::from),
                certain: true,
                before_ms: None,
            },
        );
    }
    result
}

/// アプリ発の削除・BAN をプラットフォーム発と同じ経路(moderation forwarder)へ流す。
/// UI・TTS・OBS へ 1 フレーム単位で届く。満杯でも操作自体は成功扱い(try_send)。
fn send_app_moderation(state: &AppState, event: ModerationEvent) {
    if let Err(e) = state.mod_tx.try_send(event) {
        tracing::debug!("アプリ発モデレーションイベントをdrop: {e}");
    }
}

/// UI から来たプラットフォーム文字列を `Platform` へ。未知なら None。
fn parse_platform(value: &str) -> Option<Platform> {
    serde_json::from_value(serde_json::Value::String(value.trim().to_lowercase())).ok()
}

/// ローカル非表示を OBS opt-in クライアントへ伝える `MessageDeleted`。
/// UI には送らない(UI 側の非表示は独自管理で、「削除済み」ラベルを付けないため)。
fn local_hide_event(
    id: &str,
    platform: Option<&str>,
    channel: Option<&str>,
) -> Option<ModerationEvent> {
    let id = id.trim();
    let channel = channel?.trim();
    if id.is_empty() || channel.is_empty() {
        return None;
    }
    Some(ModerationEvent::MessageDeleted {
        platform: parse_platform(platform?)?,
        channel: channel.to_string(),
        message_id: id.to_string(),
    })
}

/// このセッションで BAN したユーザーの BAN を解除する。
#[tauri::command]
async fn moderate_unban_user(
    state: State<'_, AppState>,
    platform: String,
    channel: String,
    author_id: String,
) -> Result<(), ModerationError> {
    match platform.trim().to_lowercase().as_str() {
        "youtube" => {
            let client_id = state
                .config
                .lock()
                .unwrap()
                .credentials
                .youtube_oauth_client_id
                .trim()
                .to_string();
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でYouTube OAuthクライアントIDを入力し、Googleに接続してください",
                ));
            }
            let video_id = crate::sources::youtube::extract_video_id(&channel);
            state
                .youtube_auth
                .unban_user(&client_id, &video_id, author_id.trim())
                .await
        }
        "twitch" => {
            let (client_id, client_secret) = {
                let cfg = state.config.lock().unwrap();
                (
                    cfg.credentials.twitch_client_id.trim().to_string(),
                    cfg.credentials.twitch_client_secret.trim().to_string(),
                )
            };
            if client_id.is_empty() {
                return Err(ModerationError::new(
                    ModerationErrorCode::NotLoggedIn,
                    "設定でTwitchのクライアントIDを入力し、Twitchにログインしてください",
                ));
            }
            state
                .twitch_auth
                .unban_user(&client_id, &client_secret, &channel, author_id.trim())
                .await
        }
        _ => Err(ModerationError::new(
            ModerationErrorCode::Unavailable,
            "このプラットフォームのモデレーションには対応していません",
        )),
    }
}

/// コメントログ設定を反映する。失敗(ログ用ディレクトリを開けない等)は警告のみで、
/// 設定保存・通知・チャンネル差分適用は止めない。ChatLog 側は設定を確定したまま
/// writer を未オープンで残すため、次回の configure で open を再試行する。
/// 失敗時は UI 向けの警告メッセージを返す。
async fn configure_chatlog_nonfatal(
    chatlog: &chatlog::ChatLog,
    config: config::LogConfig,
) -> Option<String> {
    match chatlog.configure(config).await {
        Ok(()) => None,
        Err(err) => {
            tracing::warn!("chatlog configure failed: {err}");
            Some(format!("コメントログを開けませんでした: {err}"))
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
enum OAuthService {
    XPost,
    Twitcasting,
}

fn parse_oauth_attempt(attempt: &str) -> Result<Uuid, &'static str> {
    Uuid::parse_str(attempt).map_err(|_| "ログインの試行 ID が不正です")
}

const MAX_OAUTH_RETIRED_ATTEMPTS: usize = 32;

#[derive(Default)]
struct OAuthAttempts {
    running: HashMap<Uuid, CancellationToken>,
    /// 開始前に取り消した試行と終了した試行を、確定順に保持する。
    retired: VecDeque<Uuid>,
}

impl OAuthAttempts {
    fn retire(&mut self, attempt: Uuid) {
        if self.retired.contains(&attempt) {
            return;
        }
        if self.retired.len() == MAX_OAUTH_RETIRED_ATTEMPTS {
            self.retired.pop_front();
        }
        self.retired.push_back(attempt);
    }
}

/// 1 サービスの OAuth ログインの開始順と、試行 ID ごとの取消を管理する。
#[derive(Default)]
struct OAuthLoginGate {
    attempts: Mutex<OAuthAttempts>,
    /// 設定の保存から認証側のセッション登録(ブラウザを開く直前)までを 1 件ずつ通す。
    order: tokio::sync::Mutex<()>,
}

impl OAuthLoginGate {
    fn is_active(&self) -> bool {
        !self.attempts.lock().unwrap().running.is_empty()
    }

    fn cancel(&self, attempt: Uuid) {
        let mut attempts = self.attempts.lock().unwrap();
        if let Some(cancel) = attempts.running.get(&attempt) {
            cancel.cancel();
        } else {
            attempts.retire(attempt);
        }
    }

    async fn begin(&self, attempt: Uuid) -> Option<OAuthLogin<'_>> {
        let cancel = CancellationToken::new();
        {
            let mut attempts = self.attempts.lock().unwrap();
            if attempts.running.contains_key(&attempt) || attempts.retired.contains(&attempt) {
                return None;
            }
            attempts.running.insert(attempt, cancel.clone());
        }
        let mut login = OAuthLogin {
            gate: self,
            attempt,
            cancel,
            order: None,
        };
        let order = tokio::select! {
            biased;
            _ = login.cancel.cancelled() => return None,
            order = self.order.lock() => order,
        };
        login.order = Some(order);
        Some(login)
    }
}

/// 同じサービスで次のログインが前のログインを取り消す場合も、最後の終了まで保護する。
struct OAuthLogin<'a> {
    gate: &'a OAuthLoginGate,
    attempt: Uuid,
    cancel: CancellationToken,
    order: Option<tokio::sync::MutexGuard<'a, ()>>,
}

impl OAuthLogin<'_> {
    fn cancelled(&self) -> bool {
        self.cancel.is_cancelled()
    }

    /// 認証側のセッション登録後、次の試行に開始順のロックを渡す。
    fn open_browser(
        &mut self,
        url: String,
        open: impl FnOnce(String) -> Result<(), String>,
    ) -> Result<(), String> {
        self.order.take();
        if self.cancelled() {
            return Err("cancelled".into());
        }
        open(url)
    }
}

impl Drop for OAuthLogin<'_> {
    fn drop(&mut self) {
        let mut attempts = self.gate.attempts.lock().unwrap();
        attempts.running.remove(&self.attempt);
        attempts.retire(self.attempt);
    }
}

fn protect_oauth_config(
    current: &AppConfig,
    incoming: &mut AppConfig,
    active: impl Fn(OAuthService) -> bool,
) -> Vec<OAuthService> {
    let mut protected = Vec::new();
    if active(OAuthService::XPost)
        && (current.credentials.x_post_client_id != incoming.credentials.x_post_client_id
            || current.x_post.callback_port != incoming.x_post.callback_port)
    {
        incoming.credentials.x_post_client_id = current.credentials.x_post_client_id.clone();
        incoming.x_post.callback_port = current.x_post.callback_port;
        protected.push(OAuthService::XPost);
    }
    if active(OAuthService::Twitcasting)
        && (current.credentials.twitcasting_client_id != incoming.credentials.twitcasting_client_id
            || current.twitcasting.callback_port != incoming.twitcasting.callback_port)
    {
        incoming.credentials.twitcasting_client_id =
            current.credentials.twitcasting_client_id.clone();
        incoming.twitcasting.callback_port = current.twitcasting.callback_port;
        protected.push(OAuthService::Twitcasting);
    }
    protected
}

fn parse_oauth_callback_port(value: serde_json::Value) -> Result<u16, &'static str> {
    value
        .as_u64()
        .and_then(|port| u16::try_from(port).ok())
        .filter(|&port| port >= config::MIN_OAUTH_CALLBACK_PORT)
        .ok_or("待受ポートは 1024〜65535 の整数にしてください")
}

/// `update_config` で保存した時の警告と、既存値を維持した OAuth の項目。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct UpdateConfigOutcome {
    warnings: Vec<String>,
    /// UI は `protectedOAuth` で読む(camelCase 変換では `protectedOauth` になる)。
    #[serde(rename = "protectedOAuth")]
    protected_oauth: Vec<OAuthService>,
    /// 保存後の設定 revision。
    revision: u64,
    /// 保存した正規化済みの設定全体。
    config: AppConfig,
}

/// 状態を計算した時点での設定上の Client ID。公開クライアントの ID を UI の照合に使う。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OAuthStatus<T> {
    client_id: String,
    #[serde(flatten)]
    status: T,
}

/// 古いフォームは型変換を含む他の検証より先に競合として拒否し、それから設定へ変換する。
fn parse_config_update(
    revision: u64,
    base_revision: Option<u64>,
    new_config: serde_json::Value,
) -> Result<AppConfig, String> {
    if base_revision.is_some_and(|base| base != revision) {
        return Err(CONFIG_CHANGED_ERROR.to_string());
    }
    AppConfig::from_config_update(new_config)
}

/// 設定全体を更新して保存する。moderation/tts/obs などの実行時状態も反映する。
///
/// チャンネル一覧の差分適用(追加/削除)も行う。
#[tauri::command]
async fn update_config(
    app: AppHandle,
    state: State<'_, AppState>,
    new_config: serde_json::Value,
    base_revision: Option<u64>,
) -> Result<UpdateConfigOutcome, String> {
    let write = state.config_write.lock().await;
    let mut new_config = parse_config_update(
        state.config_revision.load(Ordering::SeqCst),
        base_revision,
        new_config,
    )?;
    validate_x_post_template(&new_config.x_post.template)?;
    let protected_oauth = protect_oauth_config(
        &state.config.lock().unwrap(),
        &mut new_config,
        |service| match service {
            OAuthService::XPost => state.x_post_login.is_active(),
            OAuthService::Twitcasting => state.twitcasting_login.is_active(),
        },
    );
    new_config.normalize_all();
    // 削除済みのカスタム効果音を指す規則は既定の内蔵音にして保存する(保存値は画面へ返る)。
    new_config.sound_effects.resolve_missing(|id| state.sounds.contains(id));
    validate_x_post_mode(&state, &new_config).await?;
    let (
        revision,
        youtube_source_config_changed,
        youtube_oauth_client_changed,
        x_source_config_changed,
        twitch_viewer_auth_changed,
        tts_purged,
    ) = {
        let mut current = state.config.lock().unwrap();
        let changes = (
            current.youtube_overrides != new_config.youtube_overrides
                || current.credentials.youtube_api_key.trim()
                    != new_config.credentials.youtube_api_key.trim(),
            current.credentials.youtube_oauth_client_id.trim()
                != new_config.credentials.youtube_oauth_client_id.trim(),
            // overrides に加え、名前解決 cookie の変更でも張り直す
            // (新しい XAuth は再スポーン時に読み直される)。
            current.x_overrides != new_config.x_overrides
                || current.credentials.x_auth_token.trim()
                    != new_config.credentials.x_auth_token.trim()
                || current.credentials.x_csrf_token.trim()
                    != new_config.credentials.x_csrf_token.trim(),
            current.credentials.twitch_oauth.trim() != new_config.credentials.twitch_oauth.trim()
                || current.credentials.twitch_client_id.trim()
                    != new_config.credentials.twitch_client_id.trim()
                || current.credentials.twitch_client_secret.trim()
                    != new_config.credentials.twitch_client_secret.trim(),
        );

        // ディスクとメモリの確定を揃え、その間に別の設定保存を挟まない。
        // 画面の全体保存は読込時の revision を送り、別経路の書込後なら拒否する。
        let revision = commit_config(
            &state.config_dir,
            &state.config_revision,
            &state.config_tx,
            &mut current,
            new_config.clone(),
            base_revision,
        )
        .map_err(|e| e.to_string())?;
        let purged = purge_disabled_channel_tts(&current, &state.tts);
        (revision, changes.0, changes.1, changes.2, changes.3, purged)
    };
    if tts_purged {
        state.tts.emit_queue_state(&app);
    }

    // moderation 反映。
    state
        .moderator
        .lock()
        .unwrap()
        .update_config(&new_config.moderation);

    // OBS ポート反映。ポートが変わった場合は旧サーバを停止し、新ポートで再 bind する。
    restart_obs_server_if_needed(&app, &state, new_config.obs.port)?;
    drop(write);

    let mut warnings = Vec::new();
    if let Some(w) = configure_chatlog_nonfatal(&state.chatlog, new_config.log.clone()).await {
        warnings.push(w);
    }

    if youtube_source_config_changed {
        stop_active_youtube_channels(&state);
    }
    if x_source_config_changed {
        stop_active_x_channels(&state);
    }
    if twitch_viewer_auth_changed {
        stop_active_twitch_channels(&state);
    }
    if youtube_oauth_client_changed {
        state.youtube_auth.clear_caches().await;
    }

    // チャンネル差分適用(コミット済み設定を参照する)。
    apply_channel_diff(&state.config, &state.channels, |cfg, ch| {
        spawn_one_channel(&state, cfg, ch)
    });

    // 棒読みちゃん自動起動: 設定保存時にも(起動時だけでなく)未起動なら立ち上げる。
    // パスを設定した直後に効くようにする。ensure_launched 側で TCP 接続を確認し、
    // 既起動なら何もしないため、保存のたびに呼んでも無害。
    if new_config.tts.backend == config::TtsBackendKind::Bouyomi {
        let path = new_config.tts.options.bouyomi_path.trim().to_string();
        let host = new_config.tts.options.bouyomi_host.clone();
        let port = new_config.tts.options.bouyomi_port;
        let elevated = new_config.tts.options.bouyomi_launch_elevated;
        ensure_bouyomi_launched_and_emit_notice(app.clone(), path, host, port, elevated).await;
    }

    Ok(UpdateConfigOutcome {
        warnings,
        protected_oauth,
        revision,
        config: new_config,
    })
}

/// YouTubeの受信方式に関わる設定変更時だけ既存タスクを止め、差分適用で再起動させる。
fn stop_active_youtube_channels(state: &AppState) {
    let mut channels = state.channels.lock().unwrap();
    let keys: Vec<String> = channels
        .keys()
        .filter(|key| key.starts_with("youtube:"))
        .cloned()
        .collect();
    for key in keys {
        if let Some(token) = channels.remove(&key) {
            token.cancel();
        }
    }
}

/// X の overrides 変更時だけ既存タスクを止め、差分適用で再起動させる。
fn stop_active_x_channels(state: &AppState) {
    let mut channels = state.channels.lock().unwrap();
    let keys: Vec<String> = channels
        .keys()
        .filter(|key| key.starts_with("x:"))
        .cloned()
        .collect();
    for key in keys {
        if let Some(token) = channels.remove(&key) {
            token.cancel();
        }
    }
}

/// Twitch の viewer_count 認証情報変更時だけ既存タスクを止め、差分適用で再起動させる。
fn stop_active_twitch_channels(state: &AppState) {
    let mut channels = state.channels.lock().unwrap();
    let keys: Vec<String> = channels
        .keys()
        .filter(|key| key.starts_with("twitch:"))
        .cloned()
        .collect();
    for key in keys {
        if let Some(token) = channels.remove(&key) {
            token.cancel();
        }
    }
}

/// メイン画面の目標表示だけを最新の設定に反映する。
#[tauri::command]
async fn set_goals_show_in_app(
    state: State<'_, AppState>,
    show_in_app: bool,
) -> Result<ConfigCommit<serde_json::Value>, String> {
    let _write = state.config_write.lock().await;
    let mut current = state.config.lock().unwrap();
    let mut next = current.clone();
    next.goals.show_in_app = show_in_app;
    let revision = state
        .commit_config(&mut current, next)
        .map_err(|_| "目標表示の設定を保存できませんでした".to_string())?;
    Ok(ConfigCommit::new(
        Some(revision),
        serde_json::json!({ "showInApp": current.goals.show_in_app }),
    ))
}

/// 呼び出し側が config_write を保持した状態で、最新の moderation だけを保存・反映する。
/// 変更が無ければ保存せず `revision: None` を返す。
fn update_moderation(
    state: &AppState,
    edit: impl FnOnce(&mut config::ModerationConfig),
) -> Result<ConfigCommit<ModerationSaved>, String> {
    let mut current = state.config.lock().unwrap();
    let mut next = current.clone();
    edit(&mut next.moderation);
    if next.moderation == current.moderation {
        return Ok(ConfigCommit::new(None, ModerationSaved { moderation: next.moderation }));
    }
    let moderation = next.moderation.clone();
    let revision = state
        .commit_config(&mut current, next)
        .map_err(|_| "モデレーション規則を保存できませんでした".to_string())?;
    state.moderator.lock().unwrap().update_config(&moderation);
    Ok(ConfigCommit::new(Some(revision), ModerationSaved { moderation }))
}

#[derive(Clone, Serialize)]
struct ModerationSaved {
    moderation: config::ModerationConfig,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct ModerationAppended {
    /// 追加した正規化済みの値。空や重複で追加しなかった時は None。
    value: Option<String>,
    #[serde(flatten)]
    rules: ModerationSaved,
}

fn append_ng_value(entries: &mut Vec<String>, value: &str) -> Option<String> {
    // ECMAScript WhiteSpace + LineTerminator (U+0085 は除き、U+FEFF を含む)。
    let value = value.trim_matches(|c| {
        matches!(
            c,
            '\u{0009}'..='\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2000}'..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
        )
    });
    if value.is_empty() {
        return None;
    }
    let value = value.to_owned();
    if !entries.contains(&value) {
        entries.push(value.clone());
    }
    Some(value)
}

#[tauri::command]
async fn append_moderation_value(
    state: State<'_, AppState>,
    kind: serde_json::Value,
    value: serde_json::Value,
) -> Result<ConfigCommit<ModerationAppended>, String> {
    let select: fn(&mut config::ModerationConfig) -> &mut Vec<String> = match kind.as_str() {
        Some("ngWords") => |rules: &mut config::ModerationConfig| &mut rules.ng_words,
        Some("ngUsers") => |rules: &mut config::ModerationConfig| &mut rules.ng_users,
        _ => return Err("NG リストの種類が不正です".into()),
    };
    let value = value.as_str().ok_or("NG 規則は文字列で指定してください")?;
    let _write = state.config_write.lock().await;
    let mut normalized = None;
    let commit = update_moderation(&state, |rules| {
        normalized = append_ng_value(select(rules), value);
    })?;
    Ok(ConfigCommit::new(
        commit.revision,
        ModerationAppended { value: normalized, rules: commit.saved },
    ))
}

#[tauri::command]
async fn set_moderation(
    state: State<'_, AppState>,
    moderation: serde_json::Value,
) -> Result<ConfigCommit<ModerationSaved>, String> {
    let moderation = serde_json::from_value::<config::ModerationConfig>(moderation)
        .map_err(|_| "モデレーション規則の形式が不正です".to_string())?;
    let _write = state.config_write.lock().await;
    update_moderation(&state, |rules| *rules = moderation)
}

/// チャンネルを1件追加または置き換え、有効なら起動する。
#[tauri::command]
fn add_channel(
    state: State<'_, AppState>,
    channel: serde_json::Value,
    replaces: Option<serde_json::Value>,
) -> Result<ConfigCommit<ChannelsSaved>, String> {
    let channel: ChannelConfig = from_ipc_value(channel, "チャンネル")?;
    let replaces = parse_replaces(replaces)?;
    let mut cfg = state.config.lock().unwrap();
    add_channel_and_commit(
        &mut cfg,
        &state.channels,
        channel,
        &replaces,
        |current, next| state.commit_config(current, next),
        |cfg, ch| spawn_one_channel(&state, cfg, ch),
    )
}

fn add_channel_and_commit(
    cfg: &mut AppConfig,
    channels: &Mutex<HashMap<String, CancellationToken>>,
    mut channel: ChannelConfig,
    replaces: &[String],
    commit: impl FnOnce(&mut AppConfig, AppConfig) -> Result<u64, ConfigCommitError>,
    spawn: impl FnOnce(&AppConfig, &ChannelConfig) -> CancellationToken,
) -> Result<ConfigCommit<ChannelsSaved>, String> {
    channel.overrides.normalize();
    let key = AppState::channel_key(&channel);
    let mut next = cfg.clone();
    if !replaces.is_empty() {
        replace_channels_in(&mut next, replaces, channel.clone()).map_err(|e| e.to_string())?;
        ensure_new_channel(&next, &channel, Some(&key))?;
    } else {
        ensure_new_channel(cfg, &channel, None)?;
        next.channels.push(channel.clone());
    }
    let revision = commit(cfg, next).map_err(|e| e.to_string())?;
    let mut active = channels.lock().unwrap();
    for token in replaces.iter().filter_map(|old_key| active.remove(old_key)) {
        token.cancel();
    }
    if channel.enabled {
        let saved_channel = cfg
            .channels
            .iter()
            .find(|ch| ch.platform == channel.platform && ch.identifier == channel.identifier)
            .expect("コミットした行が見つかる");
        let token = spawn(cfg, saved_channel);
        if let Some(old) = active.insert(key, token) {
            old.cancel();
        }
    }
    Ok(channels_commit(cfg, revision))
}

/// IPC 入力を型に変換する。serde の文面は入力値を含みうるので、固定文と項目名だけ返す。
fn from_ipc_value<T: serde::de::DeserializeOwned>(value: serde_json::Value, what: &str) -> Result<T, String> {
    serde_path_to_error::deserialize(value).map_err(|e| {
        format!("{what}の形式が不正です (項目 {})", ipc_error_path(e.path()))
    })
}

/// 構造体のフィールド名だけを辿る。配列要素や英数字以外のキーには進まない。
fn ipc_error_path(path: &serde_path_to_error::Path) -> String {
    let mut names = Vec::new();
    for segment in path {
        match segment {
            serde_path_to_error::Segment::Map { key }
                if key.len() <= 32 && !key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') =>
            {
                names.push(key.as_str())
            }
            _ => break,
        }
    }
    if names.is_empty() {
        ".".to_owned()
    } else {
        names.join(".")
    }
}

/// `replaces` は 1 件の key(従来形)か key の配列。どちらも 1 回の保存で置き換える。
fn parse_replaces(value: Option<serde_json::Value>) -> Result<Vec<String>, String> {
    match value {
        None | Some(serde_json::Value::Null) => Ok(Vec::new()),
        Some(serde_json::Value::String(key)) => Ok(vec![key]),
        Some(value) => {
            let keys: Vec<String> = from_ipc_value(value, "置き換え対象")?;
            let mut unique = Vec::with_capacity(keys.len());
            for key in keys {
                if !unique.contains(&key) {
                    unique.push(key);
                }
            }
            Ok(unique)
        }
    }
}

/// 既存の同一キーがあれば重複追加しない。キーは URL やトークンでありうるのでエラー文に入れない。
fn ensure_new_channel(
    cfg: &AppConfig,
    channel: &ChannelConfig,
    exclude: Option<&str>,
) -> Result<(), String> {
    let key = duplicate_key(channel);
    if cfg.channels.iter().any(|c| {
        duplicate_key(c) == key
            && !exclude.is_some_and(|excluded| AppState::channel_key(c) == excluded)
    }) {
        return Err(ChannelEditError::Duplicate.to_string());
    }
    Ok(())
}

/// 重複判定のキー。ツイキャスは URL・大小文字違いでも同じ配信者なので screen id に寄せる
/// (別行にすると共有のレート枠を二重に使う)。抽出できなければ生の識別子のまま。
fn duplicate_key(ch: &ChannelConfig) -> String {
    if ch.platform == config::ChannelPlatform::Twitcasting {
        if let Some(id) = sources::twitcasting::extract_screen_id(&ch.identifier) {
            return format!("twitcasting:{}", id.to_lowercase());
        }
    }
    AppState::channel_key(ch)
}

/// チャンネルを1件削除して停止する。`key` は `twitch:name` / `youtube:videoId`。
#[tauri::command]
fn remove_channel(state: State<'_, AppState>, key: String) -> Result<ConfigCommit<ChannelsSaved>, String> {
    let mut cfg = state.config.lock().unwrap();
    let mut next = cfg.clone();
    next.channels.retain(|c| AppState::channel_key(c) != key);
    let revision = state.commit_config(&mut cfg, next).map_err(|e| e.to_string())?;
    if let Some(token) = state.channels.lock().unwrap().remove(&key) {
        token.cancel();
    }
    Ok(channels_commit(&cfg, revision))
}

/// 行の「自分の配信」印だけを保存する。Source は再起動しない(印は自動告知の判定にだけ使う)。
#[tauri::command]
fn set_channel_own_stream(
    state: State<'_, AppState>,
    key: String,
    own: bool,
) -> Result<ConfigCommit<ChannelsSaved>, String> {
    let mut cfg = state.config.lock().unwrap();
    let mut next = cfg.clone();
    set_own_stream_in(&mut next, &key, own)?;
    let revision = state.commit_config(&mut cfg, next).map_err(|e| e.to_string())?;
    Ok(channels_commit(&cfg, revision))
}

/// 行の上書きだけを保存する。実行中の Source はそのまま使う。
#[tauri::command]
async fn set_channel_overrides(
    app: AppHandle,
    state: State<'_, AppState>,
    key: serde_json::Value,
    overrides: serde_json::Value,
    base_revision: serde_json::Value,
) -> Result<ConfigCommit<ChannelsSaved>, String> {
    let key: String = from_ipc_value(key, "枠のキー")?;
    let overrides: ChannelOverrides = from_ipc_value(overrides, "上書き設定")?;
    let base_revision: u64 = from_ipc_value(base_revision, "基準 revision")?;
    let _write = state.config_write.lock().await;
    let (commit, purged) = {
        let mut cfg = state.config.lock().unwrap();
        let revision = commit_channel_overrides(
            &state.config_dir,
            &state.config_revision,
            &state.config_tx,
            &mut cfg,
            &key,
            overrides,
            base_revision,
        )
        .map_err(|e| e.to_string())?;
        (channels_commit(&cfg, revision), purge_disabled_channel_tts(&cfg, &state.tts))
    };
    if purged {
        state.tts.emit_queue_state(&app);
    }
    Ok(commit)
}

#[derive(Clone, Serialize)]
struct ChannelsSaved {
    channels: Vec<ChannelConfig>,
}

fn channels_commit(cfg: &AppConfig, revision: u64) -> ConfigCommit<ChannelsSaved> {
    ConfigCommit::new(Some(revision), ChannelsSaved { channels: cfg.channels.clone() })
}

fn set_own_stream_in(cfg: &mut AppConfig, key: &str, own: bool) -> Result<(), String> {
    let row = cfg
        .channels
        .iter_mut()
        .find(|c| AppState::channel_key(c) == key)
        .ok_or_else(|| ChannelEditError::NotFound.to_string())?;
    row.own_stream = own;
    Ok(())
}

fn idle_autopost_status() -> AutopostStatus {
    AutopostStatus::new("idle")
}

/// 起動時の X 自動告知の状態。以後はイベント `x-autopost-status`。
#[tauri::command]
fn get_x_autopost_status(state: State<'_, AppState>) -> AutopostStatus {
    state.autopost_status.borrow().clone()
}

fn send_autopost_command(state: &AppState, command: AutopostCommand) -> Result<(), String> {
    state
        .autopost_user_tx
        .try_send(command)
        .map_err(|e| format!("自動告知へ操作を送れませんでした: {e}"))
}

/// 猶予中・承認待ちの告知を取り消す。`id` はバナーに出ていた告知(`AutopostStatus::id`)。
#[tauri::command]
fn cancel_x_autopost(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    send_autopost_command(&state, AutopostCommand::Cancel(id))
}

/// 承認待ちの告知を今すぐ開く(`intentOpen = "button"` のバナー操作)。`id` は同上。
#[tauri::command]
fn approve_x_autopost(state: State<'_, AppState>, id: u64) -> Result<(), String> {
    send_autopost_command(&state, AutopostCommand::Approve(id))
}

/// 失敗した告知を再試行する。
#[tauri::command]
fn retry_x_autopost(state: State<'_, AppState>) -> Result<(), String> {
    send_autopost_command(&state, AutopostCommand::Retry)
}

/// API モードは X に接続済みの時だけ選べる。すでに API モードの設定を保存し直すのは
/// (認証が切れていても)止めない。告知は `notConnected` で止まり、他の設定は保存できる。
async fn validate_x_post_mode(state: &AppState, new_config: &AppConfig) -> Result<(), String> {
    if new_config.x_post.mode != "api" {
        return Ok(());
    }
    let current_mode = state.config.lock().unwrap().x_post.mode.clone();
    if current_mode == "api" {
        return Ok(());
    }
    let client_id = autopost::post_client_id(new_config);
    let connected = !client_id.is_empty()
        && state
            .x_post_auth
            .status(&client_id, new_config.x_post.callback_port)
            .await
            .connected;
    if connected {
        Ok(())
    } else {
        Err("API モードは X に接続してから選んでください".into())
    }
}

fn x_post_target(state: &AppState) -> (String, u16) {
    let cfg = state.config.lock().unwrap();
    (autopost::post_client_id(&cfg), cfg.x_post.callback_port)
}

/// 駆動タスクに「接続状態が変わった」ことを伝える(満杯なら次の設定変更で追いつく)。
fn notify_x_auth_changed(state: &AppState) {
    let _ = state.autopost_user_tx.try_send(AutopostCommand::AuthChanged);
}

fn twitcasting_client_id(state: &AppState) -> Option<String> {
    let cfg = state.config.lock().unwrap();
    effective_client_id(cfg.credentials.twitcasting_client_id.as_deref())
}

/// tauri.conf.json の main ウィンドウ設定(label 省略時は "main")。
fn main_window_config(
    windows: &[tauri::utils::config::WindowConfig],
) -> Option<&tauri::utils::config::WindowConfig> {
    windows.iter().find(|w| w.label == "main")
}

/// 2 つ目の起動から呼ぶ。main が閉じられていれば設定から作り直し、表示・前面化する。
#[cfg(desktop)]
fn show_main_window(app: &AppHandle) {
    let window = match app.get_webview_window("main") {
        Some(window) => window,
        None => {
            let Some(config) = main_window_config(&app.config().app.windows).cloned() else {
                tracing::warn!("main ウィンドウの設定がありません");
                return;
            };
            match tauri::WebviewWindowBuilder::from_config(app, &config).and_then(|b| b.build()) {
                Ok(window) => window,
                Err(e) => {
                    tracing::warn!("main ウィンドウを作り直せません: {e}");
                    return;
                }
            }
        }
    };
    let _ = window.unminimize();
    let _ = window.show();
    let _ = window.set_focus();
}

fn twitcasting_logout_marker(app_config_dir: Option<&std::path::Path>) -> Option<PathBuf> {
    app_config_dir
        .filter(|dir| dir.is_absolute())
        .map(|dir| dir.join(LOGOUT_MARKER_FILE))
}

fn setup_twitcasting<S: integrations::SecretStore, R, Register: FnOnce() -> R>(
    auth: &Arc<TwitcastingAuth<S>>,
    client_id: Option<&str>,
    prepare: impl FnOnce() -> Register,
) -> (R, tauri::async_runtime::JoinHandle<()>) {
    let register = prepare();
    // Publish the Loading state before IPC can see it. Poll the future after Integrations::open
    // and state registration, so its credential reads cannot block setup through the keyring lock.
    let initialization = auth.initialize(client_id, timer_now_ms());
    let managed = register();
    (managed, tauri::async_runtime::spawn(initialization))
}

#[tauri::command]
fn get_twitcasting_status(state: State<'_, AppState>) -> OAuthStatus<TwitcastingStatus> {
    let (client_id, port) = {
        let cfg = state.config.lock().unwrap();
        (
            cfg.credentials
                .twitcasting_client_id
                .as_deref()
                .unwrap_or("")
                .trim()
                .to_owned(),
            cfg.twitcasting.callback_port,
        )
    };
    let effective = effective_client_id(Some(&client_id));
    let status = state.twitcasting_auth.status_on(port, effective.as_deref(), timer_now_ms());
    OAuthStatus { client_id, status }
}

#[tauri::command]
async fn reload_twitcasting_status(
    state: State<'_, AppState>,
) -> Result<OAuthStatus<TwitcastingStatus>, ()> {
    let client_id = twitcasting_client_id(&state);
    state
        .twitcasting_auth
        .reload(client_id.as_deref(), timer_now_ms())
        .await;
    Ok(get_twitcasting_status(state))
}

#[tauri::command]
async fn connect_twitcasting(
    state: State<'_, AppState>,
    client_id: Option<String>,
    callback_port: serde_json::Value,
    attempt: String,
    on_config_saved: tauri::ipc::Channel<ConfigCommit<OAuthClientSaved>>,
) -> Result<OAuthStatus<TwitcastingStatus>, TwitcastingAuthError> {
    let attempt = parse_oauth_attempt(&attempt).map_err(|message| TwitcastingAuthError {
        code: TwitcastingAuthErrorCode::Other,
        message: message.into(),
    })?;
    let mut login = state
        .twitcasting_login
        .begin(attempt)
        .await
        .ok_or_else(|| TwitcastingAuthError::new(TwitcastingAuthErrorCode::Cancelled))?;
    let callback_port = parse_oauth_callback_port(callback_port).map_err(|message| {
        TwitcastingAuthError {
            code: TwitcastingAuthErrorCode::Other,
            message: message.into(),
        }
    })?;
    let client_id = client_id.unwrap_or_default().trim().to_owned();
    let effective = effective_client_id(Some(&client_id))
        .ok_or_else(|| TwitcastingAuthError::new(TwitcastingAuthErrorCode::NotConfigured))?;
    {
        let _write = state.config_write.lock().await;
        if login.cancelled() {
            return Err(TwitcastingAuthError::new(TwitcastingAuthErrorCode::Cancelled));
        }
        let mut cfg = state.config.lock().unwrap();
        let mut next = cfg.clone();
        next.credentials.twitcasting_client_id = (!client_id.is_empty()).then(|| client_id.clone());
        next.twitcasting.callback_port = callback_port;
        let revision = state
            .commit_config(&mut cfg, next)
            .map_err(|_| TwitcastingAuthError::new(TwitcastingAuthErrorCode::Other))?;
        let _ = on_config_saved.send(ConfigCommit::new(
            Some(revision),
            OAuthClientSaved {
                client_id: cfg.credentials.twitcasting_client_id.clone(),
                callback_port: cfg.twitcasting.callback_port,
            },
        ));
    }
    let status = state
        .twitcasting_auth
        .connect_on_with_cancel(
            callback_port,
            &effective,
            timer_now_ms(),
            login.cancel.clone(),
            |url| login.open_browser(url, open_url),
        )
        .await
        .map_err(|e| {
            if login.cancelled() {
                TwitcastingAuthError::new(TwitcastingAuthErrorCode::Cancelled)
            } else {
                e
            }
        })?;
    Ok(OAuthStatus { client_id, status })
}

#[tauri::command]
fn cancel_twitcasting_login(state: State<'_, AppState>, attempt: String) -> Result<(), &'static str> {
    state.twitcasting_login.cancel(parse_oauth_attempt(&attempt)?);
    Ok(())
}

#[tauri::command]
async fn disconnect_twitcasting(
    state: State<'_, AppState>,
) -> Result<OAuthStatus<TwitcastingStatus>, TwitcastingAuthError> {
    state.twitcasting_auth.disconnect().await?;
    Ok(get_twitcasting_status(state))
}

/// X 自動告知(API モード)の接続状態。
#[tauri::command]
async fn get_x_post_status(
    state: State<'_, AppState>,
) -> Result<OAuthStatus<XPostStatus>, XPostError> {
    let (client_id, port) = x_post_target(&state);
    let status = state.x_post_auth.status(&client_id, port).await;
    Ok(OAuthStatus { client_id, status })
}

/// Client ID とコールバックポートだけを保存し、ブラウザで X にログインする
/// (`connect_youtube_oauth` と同じく他の未保存設定を巻き込まない)。
#[tauri::command]
async fn connect_x_post(
    state: State<'_, AppState>,
    client_id: String,
    callback_port: serde_json::Value,
    attempt: String,
    on_config_saved: tauri::ipc::Channel<ConfigCommit<OAuthClientSaved>>,
) -> Result<OAuthStatus<XPostStatus>, XPostError> {
    let attempt = parse_oauth_attempt(&attempt).map_err(|message| XPostError {
        code: XPostErrorCode::Other,
        message: message.into(),
    })?;
    let mut login = state
        .x_post_login
        .begin(attempt)
        .await
        .ok_or_else(|| XPostError::new(XPostErrorCode::Cancelled))?;
    let callback_port = parse_oauth_callback_port(callback_port).map_err(|message| XPostError {
        code: XPostErrorCode::Other,
        message: message.into(),
    })?;
    let client_id = client_id.trim().to_owned();
    if client_id.is_empty() {
        return Err(XPostError::new(XPostErrorCode::NotLoggedIn));
    }
    {
        let _write = state.config_write.lock().await;
        if login.cancelled() {
            return Err(XPostError::new(XPostErrorCode::Cancelled));
        }
        let mut cfg = state.config.lock().unwrap();
        let mut next = cfg.clone();
        next.credentials.x_post_client_id = Some(client_id.clone());
        next.x_post.callback_port = callback_port;
        let revision = state
            .commit_config(&mut cfg, next)
            .map_err(|_| XPostError::new(XPostErrorCode::Other))?;
        let _ = on_config_saved.send(ConfigCommit::new(
            Some(revision),
            OAuthClientSaved {
                client_id: cfg.credentials.x_post_client_id.clone(),
                callback_port: cfg.x_post.callback_port,
            },
        ));
    }
    let result = state
        .x_post_auth
        .connect_with_cancel(&client_id, callback_port, login.cancel.clone(), |url| {
            login.open_browser(url, open_url)
        })
        .await
        .map_err(|e| {
            if login.cancelled() {
                XPostError::new(XPostErrorCode::Cancelled)
            } else {
                e
            }
        });
    notify_x_auth_changed(&state);
    Ok(OAuthStatus { client_id, status: result? })
}

/// 進行中の X ログインを取り消す(待受ポートを即座に解放する)。
#[tauri::command]
fn cancel_x_post_login(state: State<'_, AppState>, attempt: String) -> Result<(), &'static str> {
    state.x_post_login.cancel(parse_oauth_attempt(&attempt)?);
    Ok(())
}

/// この PC の X 認証を消す(X 側の許可は取り消さない)。API モードなら Intent モードへ戻す。
#[tauri::command]
async fn disconnect_x_post(
    state: State<'_, AppState>,
    on_config_saved: tauri::ipc::Channel<ConfigCommit<serde_json::Value>>,
) -> Result<OAuthStatus<XPostStatus>, XPostError> {
    let result = state.x_post_auth.disconnect().await;
    {
        let mut cfg = state.config.lock().unwrap();
        if cfg.x_post.mode == "api" {
            let mut next = cfg.clone();
            next.x_post.mode = "intent".into();
            match state.commit_config(&mut cfg, next) {
                Ok(revision) => {
                    let saved = serde_json::json!({ "xPost": { "mode": cfg.x_post.mode } });
                    let _ = on_config_saved.send(ConfigCommit::new(Some(revision), saved));
                }
                Err(e) => tracing::warn!("X 切断後の設定保存に失敗: {e}"),
            }
        }
    }
    notify_x_auth_changed(&state);
    result?;
    get_x_post_status(state).await
}

/// テスト投稿の結果。`tweetId` は X が ID を返さなかった時だけ空。
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct XTestPostResult {
    text: String,
    tweet_id: Option<String>,
}

fn x_test_post_text(now: chrono::DateTime<chrono::Local>) -> String {
    format!("fast-comment 連携テスト {}", now.format("%Y/%m/%d %H:%M"))
}

/// テスト投稿(§8.2)。UI が確認ダイアログを出してから呼ぶ。台帳・クールダウン・1 日上限には数えない。
#[tauri::command]
async fn send_x_test_post(state: State<'_, AppState>) -> Result<XTestPostResult, XPostError> {
    let (client_id, _) = x_post_target(&state);
    if client_id.is_empty() {
        return Err(XPostError::new(XPostErrorCode::NotLoggedIn));
    }
    let text = x_test_post_text(chrono::Local::now());
    let result = autopost::post_text(&state.x_post_auth, &client_id, &text).await;
    if let Err(e) = &result {
        if matches!(
            e.code,
            XPostErrorCode::ReloginRequired | XPostErrorCode::NotLoggedIn
        ) {
            notify_x_auth_changed(&state);
        }
    }
    let posted = result?;
    Ok(XTestPostResult {
        text,
        tweet_id: Some(posted.id).filter(|id| !id.is_empty()),
    })
}

/// 保存前のテンプレート検査(§7.1: `{url}` の無いテンプレートは保存時に拒否)。
fn validate_x_post_template(template: &str) -> Result<(), String> {
    match preview_x_post(template.to_string()) {
        Err(e) if e.code == autopost::XPostErrorCode::MissingUrl => Err(
            "X 告知のテンプレートに {url} がありません。{url} を入れてから保存してください".into(),
        ),
        _ => Ok(()),
    }
}

/// 見本データでテンプレートを描画する。文字数は Rust 側で数える(UI は数えない)。
#[tauri::command]
fn preview_x_post(template: String) -> Result<autopost::Rendered, autopost::XPostError> {
    let now = chrono::Local::now();
    let sample = LiveStream {
        platform: Platform::Youtube,
        channel_identifier: "@example".into(),
        stream_id: "preview".into(),
        title: Some("配信タイトルの見本".into()),
        url: "https://www.youtube.com/watch?v=xxxxxxxxxxx".into(),
        started_at_ms: Some(now.timestamp_millis().max(0) as u64),
    };
    autopost::render(&template, &[sample], now)
}

/// 自動告知の状態変化を UI へ 1 回ずつ emit する(watch は変化時だけ更新される)。
fn spawn_autopost_status_emitter(
    mut status_rx: watch::Receiver<AutopostStatus>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                changed = status_rx.changed() => {
                    if changed.is_err() {
                        break;
                    }
                    let status = status_rx.borrow_and_update().clone();
                    if let Err(e) = app.emit("x-autopost-status", &status) {
                        tracing::warn!("x-autopost-status emit 失敗: {e}");
                    }
                }
            }
        }
    });
}

/// 個別コメントをローカル非表示にする(以後の同一 ID 送出を抑止)。
#[tauri::command]
///
/// `platform` / `channel` があれば OBS(`/ws?modEvents=1`)の表示からも除く。
/// 解除(unhide)は OBS へは戻さない(除去済みの DOM は復元しない)。
fn hide_message(
    state: State<'_, AppState>,
    id: String,
    platform: Option<String>,
    channel: Option<String>,
) -> Result<(), String> {
    if let Some(event) = local_hide_event(&id, platform.as_deref(), channel.as_deref()) {
        state.bus.publish_moderation(event);
    }
    state.moderator.lock().unwrap().hide_id(id);
    Ok(())
}

/// 個別コメントの非表示を解除する。
#[tauri::command]
fn unhide_message(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.moderator.lock().unwrap().unhide_id(&id);
    Ok(())
}

/// OBS ブラウザソース用 URL を返す(テンプレ名/チャンネル指定可)。
#[tauri::command]
fn get_obs_url(
    state: State<'_, AppState>,
    template: Option<String>,
    channel: Option<String>,
) -> Result<String, String> {
    let port = state.obs_server.lock().unwrap().port;
    obs_url(port, template.as_deref(), channel.as_deref())
}

/// OBS ブラウザソース用 URL を組み立てる。`template` は ID(`default` / `custom:foo`)。
fn obs_url(port: u16, template: Option<&str>, channel: Option<&str>) -> Result<String, String> {
    let template = TemplateRef::parse(template.unwrap_or("default"))
        .map_err(|_| "テンプレート名は半角英数字、'-'、'_' のみ使用できます".to_string())?
        .id();
    let mut url =
        format!("http://127.0.0.1:{port}/?template={template}&ws=ws://127.0.0.1:{port}/ws");
    if let Some(ch) = channel {
        url.push_str(&format!("&channel={ch}"));
    }
    Ok(url)
}

/// OBS Goals overlay 用 URL を返す。
#[tauri::command]
fn get_obs_goals_url(state: State<'_, AppState>) -> Result<String, String> {
    let port = state.obs_server.lock().unwrap().port;
    Ok(format!(
        "http://127.0.0.1:{port}/?template=goals&ws=ws://127.0.0.1:{port}/stats"
    ))
}

/// Timer overlay の基準スナップショットを更新する。
#[tauri::command]
fn control_timer(
    state: State<'_, AppState>,
    action: String,
    duration_sec: Option<u32>,
) -> Result<(), String> {
    let action = match action.as_str() {
        "start" => api::TimerAction::Start,
        "pause" => api::TimerAction::Pause,
        "resume" => api::TimerAction::Resume,
        "reset" => api::TimerAction::Reset,
        other => {
            return Err(format!(
                "不正な timer action です: {other} (start/pause/resume/reset)"
            ))
        }
    };
    control_timer_impl(&state, action, duration_sec);
    Ok(())
}

fn control_timer_impl(
    state: &AppState,
    action: api::TimerAction,
    duration_sec: Option<u32>,
) -> TimerSnapshot {
    let cfg = state.config.lock().unwrap().timer.clone();
    let mut next = TimerSnapshot::default();
    state.timer_tx.send_modify(|current| {
        next = stats::next_timer_snapshot(
            current,
            action,
            duration_sec,
            cfg.default_duration_sec,
            &cfg.mode,
            timer_now_ms(),
        );
        *current = next.clone();
    });
    next
}

/// OBS Timer overlay 用 URL を返す。
#[tauri::command]
fn get_obs_timer_url(state: State<'_, AppState>) -> Result<String, String> {
    let port = state.obs_server.lock().unwrap().port;
    Ok(format!(
        "http://127.0.0.1:{port}/?template=timer&ws=ws://127.0.0.1:{port}/timer"
    ))
}

#[tauri::command]
fn get_obs_poll_url(state: State<'_, AppState>) -> String {
    let port = state.obs_server.lock().unwrap().port;
    format!("http://127.0.0.1:{port}/?template=poll&ws=ws://127.0.0.1:{port}/poll")
}

#[tauri::command]
fn poll_state(state: State<'_, AppState>) -> PollSnapshot {
    state.poll.snapshot()
}

#[tauri::command]
async fn poll_start(
    state: State<'_, AppState>,
    source: Option<PollSource>,
    question: String,
    options: Vec<String>,
    duration_secs: Option<u32>,
    display: PollDisplay,
    show_percent: bool,
    show_counts: bool,
) -> Result<PollSnapshot, String> {
    let request = PollStart {
        source: source.unwrap_or_default(),
        question,
        options,
        duration_secs,
        display,
        show_percent,
        show_counts,
    };
    let start = request.validate().map_err(|error| error.to_string())?;
    state
        .poll_control
        .start(start)
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn poll_stop(state: State<'_, AppState>) -> Result<PollSnapshot, String> {
    state
        .poll_control
        .stop()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn poll_reset(state: State<'_, AppState>) -> Result<PollSnapshot, String> {
    state
        .poll_control
        .reset()
        .await
        .map_err(|error| error.to_string())
}

#[tauri::command]
async fn poll_set_display(
    state: State<'_, AppState>,
    display: PollDisplay,
    show_percent: bool,
    show_counts: bool,
) -> Result<PollSnapshot, String> {
    let settings = PollDisplaySettings {
        display,
        show_percent,
        show_counts,
    };
    state
        .poll_control
        .set_display(settings)
        .await
        .map_err(|error| error.to_string())
}

fn timer_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// OBS テンプレート一覧(内蔵 → カスタムの順)を返す。
#[tauri::command]
fn list_templates(state: State<'_, AppState>) -> Result<Vec<TemplateInfo>, String> {
    state.templates.list()
}

/// OBS テンプレート内の編集可能ファイルを読み込む。`name` はテンプレート ID。
#[tauri::command]
fn read_template_file(
    state: State<'_, AppState>,
    name: String,
    file: String,
) -> Result<String, String> {
    state.templates.read(&TemplateRef::parse(&name)?, &file)
}

/// OBS テンプレート内の編集可能ファイルを書き込む。内蔵は読み取り専用で Err。
#[tauri::command]
fn write_template_file(
    state: State<'_, AppState>,
    name: String,
    file: String,
    contents: String,
) -> Result<(), String> {
    state.ensure_restore_writable("templates")?;
    state
        .templates
        .write(&TemplateRef::parse(&name)?, &file, &contents)
}

/// テンプレート `source`(ID)をカスタム `name` として複製する。
#[tauri::command]
fn duplicate_template(
    state: State<'_, AppState>,
    source: String,
    name: String,
) -> Result<TemplateInfo, String> {
    state.ensure_restore_writable("templates")?;
    state
        .templates
        .duplicate(&TemplateRef::parse(&source)?, &name)
}

/// カスタムテンプレート(ID)を削除する。内蔵は Err。
#[tauri::command]
fn delete_template(state: State<'_, AppState>, id: String) -> Result<(), String> {
    state.ensure_restore_writable("templates")?;
    state.templates.delete(&TemplateRef::parse(&id)?)
}

/// zip(raw body)をカスタムテンプレートとして取り込む。名前はヘッダ `x-template-name`。
/// ファイルパスは受け取らない。展開はブロッキングスレッドで行う。
#[tauri::command]
async fn import_template_zip(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<template_import::ImportResult, String> {
    state.ensure_restore_writable("templates")?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("zip のバイト列(raw body)を渡してください".to_string());
    };
    let name = request
        .headers()
        .get("x-template-name")
        .ok_or_else(|| "ヘッダ x-template-name がありません".to_string())?
        .to_str()
        .map_err(|_| "ヘッダ x-template-name が UTF-8 ではありません".to_string())?
        .to_string();
    let bytes = bytes.clone();
    let store = state.templates.clone();
    tauri::async_runtime::spawn_blocking(move || template_import::import_zip(&store, &name, &bytes))
        .await
        .map_err(|e| format!("取り込み処理が異常終了しました: {e}"))?
}

/// カスタム効果音の一覧。
#[tauri::command]
fn list_custom_sounds(state: State<'_, AppState>) -> Result<Vec<sounds::SoundEntry>, String> {
    state.sounds.list()
}

/// 音声ファイルのバイト列(raw body)を取り込む。表示名はヘッダ `x-sound-name`(URI エンコード)。
/// ファイルパスは受け取らない。保存名は Rust が UUID で付ける。
#[tauri::command]
async fn add_custom_sound(
    state: State<'_, AppState>,
    request: tauri::ipc::Request<'_>,
) -> Result<sounds::SoundEntry, String> {
    state.ensure_restore_writable("sounds")?;
    state.ensure_restore_writable("sounds.json")?;
    let tauri::ipc::InvokeBody::Raw(bytes) = request.body() else {
        return Err("音声のバイト列(raw body)を渡してください".to_string());
    };
    let name = request
        .headers()
        .get("x-sound-name")
        .and_then(|v| v.to_str().ok())
        .map(decode_sound_name)
        .unwrap_or_default();
    state.sounds.add(&name, bytes)
}

fn decode_sound_name(encoded: &str) -> String {
    url::form_urlencoded::parse(format!("n={encoded}").as_bytes())
        .next()
        .map(|(_, v)| v.into_owned())
        .unwrap_or_default()
}

/// 再生用に中身を返す。ID を検証した物だけが読める。
#[tauri::command]
fn read_custom_sound(state: State<'_, AppState>, id: String) -> Result<tauri::ipc::Response, String> {
    state.sounds.read(&id).map(|data| tauri::ipc::Response::new(data.bytes))
}

/// カスタム効果音を削除する。設定は書き換えない(使っていた規則は保存時と再生時に既定音へ寄る)。
/// 設定の保存と順序を揃えるため、保存と同じ書込ロックを取る。
#[tauri::command]
async fn remove_custom_sound(state: State<'_, AppState>, id: String) -> Result<bool, String> {
    state.ensure_restore_writable("sounds")?;
    state.ensure_restore_writable("sounds.json")?;
    let _write = state.config_write.lock().await;
    state.sounds.remove(&id)
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct GoalSkinInfo {
    directory: String,
    files: Vec<String>,
}

/// ユーザー作成Goals PNGスキンの配置先とファイル一覧を返す。
#[tauri::command]
fn get_goal_skin_info(state: State<'_, AppState>) -> Result<GoalSkinInfo, String> {
    if state.ensure_restore_writable("skin").is_ok() {
        fs::create_dir_all(&state.goals_skin_dir).map_err(|e| {
            format!(
                "skinフォルダの作成に失敗 {}: {e}",
                state.goals_skin_dir.display()
            )
        })?;
    }

    let mut files = Vec::new();
    let entries = fs::read_dir(&state.goals_skin_dir).map_err(|e| {
        format!(
            "skinフォルダの読み込みに失敗 {}: {e}",
            state.goals_skin_dir.display()
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("skinファイル一覧の取得に失敗: {e}"))?;
        if !entry
            .file_type()
            .map_err(|e| format!("skinファイル種別の取得に失敗: {e}"))?
            .is_file()
        {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_png_skin_file_name(&name) {
            files.push(name);
        }
    }
    files.sort_by_key(|name| name.to_lowercase());
    Ok(GoalSkinInfo {
        directory: state.goals_skin_dir.to_string_lossy().into_owned(),
        files,
    })
}

fn is_png_skin_file_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    let path = std::path::Path::new(name);
    path.parent()
        .is_none_or(|parent| parent.as_os_str().is_empty())
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("png"))
}

#[cfg(test)]
mod obs_url_tests {
    use super::obs_url;

    #[test]
    fn obs_url_accepts_custom_id() {
        let url = obs_url(11180, Some("custom:foo"), None).unwrap();
        assert!(url.contains("?template=custom:foo"), "{url}");
        assert!(obs_url(11180, Some("custom:../x"), None).is_err());
        assert!(obs_url(11180, None, None).unwrap().contains("?template=default&"));
    }
}

#[cfg(test)]
mod goal_skin_tests {
    use super::is_png_skin_file_name;

    #[test]
    fn accepts_png_files_and_rejects_paths_or_other_extensions() {
        assert!(is_png_skin_file_name("my-goals.png"));
        assert!(is_png_skin_file_name("配信用スキン.PNG"));
        assert!(!is_png_skin_file_name("../skin.png"));
        assert!(!is_png_skin_file_name("nested/skin.png"));
        assert!(!is_png_skin_file_name("skin.jpg"));
        assert!(!is_png_skin_file_name(""));
    }
}

/// 参加者一覧を取得する。
#[tauri::command]
fn get_participants(state: State<'_, AppState>) -> Vec<Participant> {
    state.participants.lock().unwrap().clone()
}

/// 未選出の先頭参加者を選出する。
#[tauri::command]
fn pick_next_participant(app: AppHandle, state: State<'_, AppState>) -> Option<Participant> {
    let (picked, snapshot) = {
        let mut participants = state.participants.lock().unwrap();
        let Some(index) = participants.iter().position(|p| !p.picked) else {
            return None;
        };
        participants[index].picked = true;
        let picked = participants[index].clone();
        (picked, participants.clone())
    };
    emit_participants(&app, &snapshot);
    Some(picked)
}

/// 未選出の参加者から疑似ランダムに1人を選出する。
#[tauri::command]
fn pick_random_participant(app: AppHandle, state: State<'_, AppState>) -> Option<Participant> {
    let (picked, snapshot) = {
        let mut participants = state.participants.lock().unwrap();
        let unpicked: Vec<usize> = participants
            .iter()
            .enumerate()
            .filter_map(|(index, p)| (!p.picked).then_some(index))
            .collect();
        if unpicked.is_empty() {
            return None;
        }
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let index = unpicked[(nanos % (unpicked.len() as u128)) as usize];
        participants[index].picked = true;
        let picked = participants[index].clone();
        (picked, participants.clone())
    };
    emit_participants(&app, &snapshot);
    Some(picked)
}

/// 指定した参加者を削除する。
#[tauri::command]
fn remove_participant(
    app: AppHandle,
    state: State<'_, AppState>,
    platform: String,
    user_id: String,
) {
    let snapshot = {
        let mut participants = state.participants.lock().unwrap();
        participants.retain(|p| !(p.platform == platform && p.user_id == user_id));
        participants.clone()
    };
    emit_participants(&app, &snapshot);
}

/// 参加者一覧を全消去する。
#[tauri::command]
fn clear_participants(app: AppHandle, state: State<'_, AppState>) {
    let snapshot = {
        let mut participants = state.participants.lock().unwrap();
        participants.clear();
        participants.clone()
    };
    emit_participants(&app, &snapshot);
}

/// 配信前確認用の合成コメントを通常の Source → pipeline 経路へ注入する。
#[tauri::command]
fn inject_test_comment(
    state: State<'_, AppState>,
    platform: String,
    name: String,
    text: String,
    kind: Option<String>,
    amount: Option<f64>,
    count: Option<u32>,
) -> Result<(), String> {
    inject_test_comments_impl(
        &state,
        &api::TestCommentRequest {
            platform,
            name,
            text,
            kind: kind.unwrap_or_else(|| "normal".into()),
            amount,
            count: count.unwrap_or(1).clamp(1, 20),
        },
    )
    .map(|_| ())
}

/// inject_test_comment が流すメッセージを組み立てる(テスト可能な純関数部分)。
#[cfg(test)]
fn build_injected_test_messages(
    platform: &str,
    name: &str,
    text: &str,
    kind: Option<&str>,
    amount: Option<f64>,
    count: Option<u32>,
) -> Result<Vec<ChatMessage>, String> {
    inject::build_test_messages(&api::TestCommentRequest {
        platform: platform.into(),
        name: name.into(),
        text: text.into(),
        kind: kind.unwrap_or("normal").into(),
        amount,
        count: count.unwrap_or(1).clamp(1, 20),
    })
}

fn inject_test_comments_impl(
    state: &AppState,
    req: &api::TestCommentRequest,
) -> Result<Vec<String>, String> {
    let messages = inject::build_test_messages(req)?;
    let mut ids = Vec::with_capacity(messages.len());
    for msg in messages {
        ids.push(msg.id.clone());
        let _ = state.source_tx.send(msg);
    }
    Ok(ids)
}

// ============ 内部ヘルパ ============

fn participant_platform(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::X => "x",
        Platform::Niconico => "niconico",
        Platform::Twitcasting => "twitcasting",
    }
}

fn emit_participants(app: &AppHandle, participants: &[Participant]) {
    if let Err(e) = app.emit("participants-updated", participants) {
        tracing::warn!("参加者一覧の emit に失敗: {e}");
    }
}

fn register_participant_if_needed(app: &AppHandle, state: &AppState, msg: &ChatMessage) {
    let participation = {
        let cfg = state.config.lock().unwrap();
        cfg.participation.clone()
    };
    if !participation.enabled {
        return;
    }
    if !msg.plain_text().contains(&participation.keyword) {
        return;
    }

    let platform = participant_platform(msg.platform).to_string();
    let user_id = msg.author.id.clone();
    let name = msg.author.name.clone();

    let snapshot = {
        let mut participants = state.participants.lock().unwrap();
        if participants
            .iter()
            .any(|p| p.platform == platform && p.user_id == user_id)
        {
            return;
        }
        if participation.max != 0 && participants.len() >= participation.max as usize {
            return;
        }
        participants.push(Participant {
            platform,
            user_id,
            name,
            picked: false,
        });
        participants.clone()
    };
    emit_participants(app, &snapshot);
}

fn local_api_status_json(state: &AppState, status: integrations::LocalApiStatus) -> serde_json::Value {
    let mut value = serde_json::to_value(status).expect("local api status serialization");
    value["baseUrl"] = format!(
        "http://127.0.0.1:{}/api/v1",
        state.obs_server.lock().unwrap().port
    ).into();
    value
}
// Tauri app commands are unrestricted by the plugin capabilities alone.
// Check the injected caller identity before accessing settings or credentials.
fn require_main_integrations_window(label: &str) -> Result<(), integrations::IntegrationError> {
    if label == "main" {
        Ok(())
    } else {
        Err(integrations::IntegrationError::ForbiddenWindow)
    }
}
#[tauri::command]
fn get_local_api_status(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    Ok(local_api_status_json(
        &state,
        state.integrations.local_api_status(),
    ))
}
#[tauri::command]
async fn set_local_api_enabled(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    enabled: bool,
) -> Result<serde_json::Value, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    let status = state.integrations.set_local_api_enabled(enabled).await?;
    Ok(local_api_status_json(&state, status))
}
#[tauri::command]
async fn get_local_api_token(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<String, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    state.integrations.local_api_token().await
}
#[tauri::command]
async fn regenerate_local_api_token(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<serde_json::Value, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    let status = state.integrations.regenerate_local_api_token().await?;
    Ok(local_api_status_json(&state, status))
}

#[tauri::command]
fn list_webhooks(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
) -> Result<Vec<serde_json::Value>, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    let statuses = state.webhook.status();
    Ok(state
        .integrations
        .list_webhooks()
        .into_iter()
        .map(|v| {
            let mut json = serde_json::to_value(&v).expect("Webhook view");
            json["status"] =
                serde_json::to_value(statuses.iter().find(|s| s.id == v.definition.id))
                    .expect("Webhook status");
            json
        })
        .collect())
}
#[tauri::command]
async fn save_webhook(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    input: integrations::WebhookInput,
) -> Result<integrations::WebhookView, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    let view = state.integrations.save_webhook(input).await?;
    state.webhook.reload().await;
    Ok(view)
}
#[tauri::command]
async fn delete_webhook(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    state.integrations.delete_webhook(&id).await?;
    state.webhook.reload().await;
    Ok(())
}
#[tauri::command]
async fn resume_webhook(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<(), integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    state.integrations.resume_webhook(&id).await?;
    state.webhook.reload().await;
    Ok(())
}
#[tauri::command]
async fn test_webhook(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<webhook::TestSendResult, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    Ok(state.webhook.test_send(&id).await)
}
#[tauri::command]
async fn reveal_webhook_signing_secret(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    state.integrations.reveal_signing_secret(&id).await
}
#[tauri::command]
async fn regenerate_webhook_signing_secret(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    id: String,
) -> Result<String, integrations::IntegrationError> {
    require_main_integrations_window(window.label())?;
    let secret = state.integrations.regenerate_signing_secret(&id).await?;
    state.webhook.reload().await;
    Ok(secret)
}

/// Bus 購読は有効な送り先が存在する間だけ。状態 emit は差分がある時に最大毎秒 1 回。
/// `announce.tts` の実績解除だけを TTS キューへ入れる。視聴者の `tts_muted` は listeners で引く。
/// 取りこぼし(Lagged)は件数だけ warn に出して続ける。
fn spawn_achievement_tts_forwarder(
    mut rx: broadcast::Receiver<bus::SequencedAchievement>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                incoming = rx.recv() => match incoming {
                    Ok(unlock) => {
                        let state = app.state::<AppState>();
                        let defs = state.achievement_defs.current();
                        let message = announce_message(&unlock.unlock, &defs, |platform, author_id| {
                            state
                                .listeners
                                .get(participant_platform(platform), author_id)
                                .is_some_and(|record| record.tts_muted)
                        });
                        if let Some(message) = message {
                            try_enqueue_achievement_tts(&app, &state.config, &state.tts, message);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "実績の読み上げ通知を取りこぼしました");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

/// Webhook forwarder の 1 ラウンドで chat・実績それぞれに処理する上限。
const WEBHOOK_RELAY_ROUND_LIMIT: usize = 256;

/// 終了時は送らない(直後の shutdown で待ち行列が破棄される)。保留中だった解除の件数だけログに残す(数値のみ)。
fn discard_webhook_relay(
    runtime: &webhook::WebhookRuntime<integrations::KeyringSecretStore, webhook::ReqwestSender>,
    relay: &mut bus::relay::AchievementRelay,
    achievement_rx: Option<&mut tokio::sync::broadcast::Receiver<bus::SequencedAchievement>>,
) {
    let dropped = relay.discard(achievement_rx);
    if dropped > 0 && runtime.has_achievement_destinations() {
        tracing::warn!(dropped, "終了時に保留中の実績通知を送らず破棄しました(件数は概数)");
    }
}

fn spawn_webhook_forwarder(app: AppHandle, cancel: CancellationToken) {
    use crate::bus::relay::AchievementRelay;
    use crate::webhook::runtime::{drain_relay_round, sync_subscription};
    tauri::async_runtime::spawn(async move {
        let (runtime, bus) = {
            let state = app.state::<AppState>();
            (state.webhook.clone(), state.bus.clone())
        };
        runtime.reload().await;
        let mut rx = None;
        let mut achievement_rx = None;
        let mut achievement_closed = false;
        let mut relay = AchievementRelay::default();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut last = runtime.status();
        let mut chat_lagged = 0u64;
        let mut achievement_lagged = 0u64;
        loop {
            // 実績も chat と同じく有効な送り先があれば購読し、送り先ごとに配信を判定する。
            let active = runtime.has_active_webhooks();
            sync_subscription(&mut rx, active, false, || {
                let (chat, baseline) = bus.subscribe_chat_with_baseline();
                relay = AchievementRelay::new(baseline);
                chat
            });
            sync_subscription(&mut achievement_rx, active, achievement_closed, || {
                bus.subscribe_achievements()
            });
            if !active {
                relay = AchievementRelay::default();
            }
            let first = tokio::select! {
                biased;
                _ = cancel.cancelled() => {
                    discard_webhook_relay(&runtime, &mut relay, achievement_rx.as_mut());
                    runtime.shutdown();
                    break;
                },
                _ = runtime.changed() => continue,
                _ = tick.tick() => {
                    let status = runtime.status();
                    if status != last { let _ = app.emit("webhook-status", &status); last = status; }
                    continue;
                },
                incoming = async { rx.as_mut().unwrap().recv().await }, if rx.is_some() => (Some(incoming.into()), None),
                unlock = async { achievement_rx.as_mut().unwrap().recv().await }, if achievement_rx.is_some() => (None, Some(unlock.into())),
                _ = std::future::ready(()), if relay.has_ready() => (None, None),
            };
            let round = drain_relay_round(
                first,
                &mut relay,
                rx.as_mut(),
                achievement_rx.as_mut(),
                WEBHOOK_RELAY_ROUND_LIMIT,
                |msg| runtime.handle_message(&msg),
                |unlock| runtime.handle_achievement(&unlock),
            );
            if round.chat_lagged > 0 {
                chat_lagged = chat_lagged.saturating_add(round.chat_lagged);
                tracing::warn!(lagged = chat_lagged, "Webhook Bus の遅延分を破棄しました");
            }
            if round.achievement_lagged > 0 && runtime.has_achievement_destinations() {
                achievement_lagged = achievement_lagged.saturating_add(round.achievement_lagged);
                tracing::warn!(lagged = achievement_lagged, "Webhook の実績通知を取りこぼしました");
            }
            if round.achievement_closed {
                achievement_closed = true;
                achievement_rx = None;
            }
            if round.chat_closed {
                discard_webhook_relay(&runtime, &mut relay, achievement_rx.as_mut());
                runtime.shutdown();
                break;
            }
        }
    });
}

/// OBS サーバの待受ポートが変わったら、旧サーバを止めて新ポートで起動する。
fn restart_obs_server_if_needed(
    app: &AppHandle,
    state: &AppState,
    new_port: u16,
) -> Result<(), String> {
    let mut server = state.obs_server.lock().unwrap();
    if server.port == new_port {
        return Ok(());
    }

    tracing::info!("OBS サーバを再起動: {} -> {}", server.port, new_port);

    let cancel = state.app_cancel.child_token();
    let _obs_task = state.bus.spawn_obs_server_on_port(
        state.templates.builtin_root().to_path_buf(),
        state.templates.custom_root().to_path_buf(),
        state.goals_skin_dir.clone(),
        new_port,
        cancel.clone(),
        Some(api::router(
            api::ApiDeps::new(state.integrations.gate(), cancel.clone())
                .with_feed(state.api_feed.clone())
                .with_ops(Arc::new(TauriAppOps(app.clone())))
                .with_poll(state.poll_control.clone())
                .with_rate_limit(state.api_rate_limit.clone()),
            new_port,
        )),
    )?;

    server.cancel.cancel();
    *server = ObsServerControl {
        port: new_port,
        cancel,
    };
    Ok(())
}

/// 最新の設定と起動状態を突き合わせる。追加・削除と同じ順でロックする。
fn apply_channel_diff(
    config: &Mutex<AppConfig>,
    channels: &Mutex<HashMap<String, CancellationToken>>,
    mut spawn: impl FnMut(&AppConfig, &ChannelConfig) -> CancellationToken,
) {
    let config = config.lock().unwrap();
    let desired_keys: std::collections::HashSet<String> = config
        .channels
        .iter()
        .filter(|c| c.enabled)
        .map(AppState::channel_key)
        .collect();

    let mut active = channels.lock().unwrap();
    active.retain(|key, token| {
        let keep = desired_keys.contains(key);
        if !keep {
            token.cancel();
        }
        keep
    });
    for ch in config.channels.iter().filter(|c| c.enabled) {
        active
            .entry(AppState::channel_key(ch))
            .or_insert_with(|| spawn(&config, ch));
    }
}

/// 有効なチャンネルの Source を起動し、停止トークンを返す。
fn spawn_one_channel(state: &AppState, config: &AppConfig, ch: &ChannelConfig) -> CancellationToken {
    let (
        overrides,
        youtube_api_key,
        x_overrides,
        x_auth,
        niconico_overrides,
        twitcasting_overrides,
        twitcasting_mode,
        twitch_viewer_auth,
    ) = {
        // X の名前解決 cookie は auth_token / ct0 の両方が揃って初めて有効。
        let x_auth = {
            let token = config.credentials.x_auth_token.trim();
            let csrf = config.credentials.x_csrf_token.trim();
            (!token.is_empty() && !csrf.is_empty()).then(|| sources::x::XAuth {
                auth_token: token.to_string(),
                csrf_token: csrf.to_string(),
            })
        };
        let twitch_viewer_auth = if ch.platform == ChannelPlatform::Twitch {
            sources::twitch_helix::viewer_auth(
                &config.credentials.twitch_oauth,
                &config.credentials.twitch_client_id,
                &config.credentials.twitch_client_secret,
            )
        } else {
            None
        };
        (
            config.youtube_overrides.clone(),
            config.credentials.youtube_api_key.clone(),
            config.x_overrides.clone(),
            x_auth,
            config.niconico_overrides.clone(),
            config.twitcasting_overrides.clone(),
            config.twitcasting.mode,
            twitch_viewer_auth,
        )
    };
    let token = CancellationToken::new();
    let source_tx = channel_source_sender(ch, &state.source_tx, &token);
    if ch.platform == ChannelPlatform::Youtube
        && sources::youtube::is_channel_identifier(&ch.identifier)
    {
        sources::youtube::live_resolve::spawn_live_resolve_poller(
            ch.identifier.clone(),
            overrides.clone(),
            youtube_api_key.clone(),
            source_tx,
            state.metadata_tx.clone(),
            state.reaction_tx.clone(),
            state.mod_tx.clone(),
            Some(state.live_tx.clone()),
            token.clone(),
        );
    } else {
        let manager = SourceManager::new(
            source_tx,
            overrides.clone(),
            youtube_api_key,
            x_overrides,
            x_auth,
            niconico_overrides,
            twitcasting_overrides,
            Some(state.metadata_tx.clone()),
            Some(state.reaction_tx.clone()),
            Some(state.mod_tx.clone()),
        )
        .with_twitcasting(state.twitcasting_api.clone(), state.twitcasting_auth.subscribe())
        .with_twitcasting_mode(twitcasting_mode)
        .with_twitcasting_started_movies(state.twitcasting_started_movies.clone())
        .with_live_tx(Some(state.live_tx.clone()));
        manager.spawn_channel_with_cancel(ch, token.clone());
    }
    let app_cancel = state.app_cancel.clone();
    let token_for_app_cancel = token.clone();
    tauri::async_runtime::spawn(relay_channel_cancel(app_cancel, token_for_app_cancel));
    if ch.platform == ChannelPlatform::Youtube
        && !sources::youtube::is_channel_identifier(&ch.identifier)
    {
        sources::youtube::metadata::spawn_metadata_poller(
            ch.identifier.clone(),
            sources::youtube::extract_video_id(&ch.identifier),
            overrides,
            state.metadata_tx.clone(),
            token.clone(),
        );
    }
    if let Some(auth) = twitch_viewer_auth {
        sources::twitch_helix::spawn_twitch_viewer_poller(
            ch.identifier.clone(),
            auth,
            state.twitch_auth.clone(),
            state.metadata_tx.clone(),
            Some(state.live_tx.clone()),
            token.clone(),
        );
    }
    token
}

fn channel_source_sender(
    ch: &ChannelConfig,
    upstream: &broadcast::Sender<ChatMessage>,
    cancel: &CancellationToken,
) -> broadcast::Sender<ChatMessage> {
    sources::origin_relay::spawn_origin_relay(
        config::channel_key(ch),
        upstream.clone(),
        cancel.child_token(),
    )
}

async fn relay_channel_cancel(app: CancellationToken, channel: CancellationToken) {
    tokio::select! {
        _ = app.cancelled() => channel.cancel(),
        _ = channel.cancelled() => {}
    }
}

/// プラットフォーム側の削除・BAN を Rust 側の状態へ反映する(1 フレーム分まとめて)。
///
/// - MessageDeleted: hidden_ids に入れ、後着(イベントが chat より先に届いた)分を抑止する。
/// - 全種: 未読み上げの該当コメントを TTS キューから除く(`TtsQueueItem.id` = `ChatMessage.id`)。
fn apply_platform_moderation(app: &AppHandle, events: &[ModerationEvent]) {
    let state = app.state::<AppState>();
    {
        let mut moderator = state.moderator.lock().unwrap();
        for event in events {
            if let ModerationEvent::MessageDeleted { message_id, .. } = event {
                moderator.hide_id(message_id.as_str());
            }
        }
    }
    state.tts.remove_moderated(app, events);
    // OBS opt-in クライアントへ(配信側・アプリ発の削除・BAN を同じ経路で)。
    for event in events {
        state.bus.publish_moderation(event.clone());
    }
}

/// moderation + TTS パイプラインを起動する。
///
/// source_tx を購読し、各メッセージを判定。
/// - Hide   : 破棄
/// - Highlight: ハイライトバッジを付けて bus へ流す + TTS
/// - Show   : そのまま bus へ流す + TTS
fn spawn_pipeline(app: AppHandle, cancel: CancellationToken) {
    let mut rx = app.state::<AppState>().source_tx.subscribe();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = rx.recv() => {
                    let mut msg = match recv {
                        Ok(m) => m,
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            tracing::debug!("pipeline が {n} 件 lag");
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    };

                    // 判定はロックを短く保持して取り出す(MutexGuard を await 跨ぎで持たない)。
                    state.listeners.observe(&mut msg);
                    let verdict = state.moderator.lock().unwrap().judge(&msg);
                    stamp_message_sound(&state, &mut msg, &verdict);
                    let log_config = state.config.lock().unwrap().log.clone();
                    if log_config.enabled && (!matches!(verdict, Verdict::Hide) || log_config.include_hidden) {
                        state.chatlog.try_send(msg.clone(), matches!(verdict, Verdict::Hide));
                    }
                    match verdict {
                        Verdict::Hide => continue,
                        Verdict::Highlight => {
                            // モデルを変えずにハイライトを伝えるため、バッジを1枚足す。
                            msg.author.badges.push(Badge {
                                kind: "highlight".to_string(),
                                label: "highlight".to_string(),
                                image_url: None,
                            });
                        }
                        Verdict::Show => {}
                    }

                    register_participant_if_needed(&app, &state, &msg);
                    state.poll.record(&msg, &verdict);

                    // 同期 I/O やネットワーク待ちをせず、有界キューに投入する。
                    try_enqueue_tts_message(&app, &state.config, &state.tts, msg.clone());

                    let effects = keyword_effects_for(&state, &msg);
                    state.publish_achievement_event(AchievementEvent::Chat(msg));
                    // 演出は原因のコメントの後に流す。
                    for effect in effects {
                        state.bus.publish_effect(effect);
                    }
                }
            }
        }
        tracing::info!("moderation/TTS パイプライン終了");
    });
}

/// キーワードエフェクトを判定する(非表示判定の後に呼ぶ)。
fn keyword_effects_for(state: &AppState, msg: &ChatMessage) -> Vec<keyword_effects::EffectTriggered> {
    let cfg = state.config.lock().unwrap();
    if !cfg.effects.enabled {
        return Vec::new();
    }
    let mut effects = state
        .keyword_effects
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    effects.evaluate(&cfg.effects, msg, std::time::Instant::now())
}

/// 設定画面の「テスト」ボタン: 1 規則をクールダウン無視で 1 回、表示先へ流す。
#[tauri::command]
fn test_keyword_effect(
    state: State<'_, AppState>,
    index: u32,
    rule: config::EffectRule,
) -> Result<(), String> {
    let event = keyword_effects::test_trigger(index, &rule)
        .ok_or_else(|| "表示物が空です".to_string())?;
    state.bus.publish_effect(event);
    Ok(())
}

/// 効果音を判定して `msg.sound` に入れる。設定と実績のレート表は必要な時だけ読む。
fn stamp_message_sound(state: &AppState, msg: &mut ChatMessage, verdict: &Verdict) {
    let cfg = state.config.lock().unwrap();
    if !cfg.sound_effects.enabled {
        return;
    }
    let (rates, symbols) = if msg.amount.is_some() {
        let defs = state.achievement_defs.current();
        (defs.rates, defs.symbol_overrides)
    } else {
        Default::default()
    };
    let mut gate = state.sound_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    sound_effects::stamp_sound(msg, verdict, &cfg, (&rates, &symbols), |id| state.sounds.contains(id), &mut gate, std::time::Instant::now());
}

fn spawn_achievement_events(
    app: AppHandle,
    mut live_rx: mpsc::Receiver<LiveEvent>,
    mut stats_rx: watch::Receiver<StatsSnapshot>,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                Some(event) = live_rx.recv() => {
                    state.publish_achievement_event(AchievementEvent::Live(&event));
                }
                Ok(()) = stats_rx.changed() => {
                    let snapshot = stats_rx.borrow_and_update().clone();
                    state.publish_achievement_event(AchievementEvent::Stats(&snapshot));
                }
            }
        }
    });
}

/// 単一の長命 TTS ワーカーを起動する。
fn spawn_tts_worker(app: AppHandle, cancel: CancellationToken) {
    let shared = app.state::<AppState>().tts.clone();
    let config_app = app.clone();
    let config_fn = move || {
        let state = config_app.state::<AppState>();
        let cfg = state.config.lock().unwrap().tts.clone();
        cfg
    };
    let dispatcher = TtsDispatcher::new(config_fn());
    tauri::async_runtime::spawn(run_tts_worker(
        shared, Arc::new(app), dispatcher, config_fn, cancel,
    ));
}

/// OBS テンプレートの配信元ディレクトリを解決する。
///
/// リソース同梱の `templates/` を優先し、無ければ実行ファイル隣 / CWD を試す。
fn resolve_templates_dir(app: &AppHandle) -> PathBuf {
    // Tauri リソースに同梱した templates を探す。
    if let Ok(dir) = app.path().resource_dir() {
        let candidate = dir.join("templates");
        if candidate.exists() {
            return candidate;
        }
    }
    // 開発時フォールバック候補(cwd は dev 時 src-tauri/ になりがち)。
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    for candidate in [
        cwd.join("templates"),            // cwd=リポジトリルートの場合
        cwd.join("..").join("templates"), // cwd=src-tauri/ の場合(dev)
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    // 最終手段(存在しなくても ServeDir は空配信になるだけで panic しない)。
    cwd.join("templates")
}

fn log_subscriber<W>(env: Option<&str>, writer: W) -> impl tracing::Subscriber + Send + Sync
where
    W: for<'writer> tracing_subscriber::fmt::MakeWriter<'writer> + Send + Sync + 'static,
{
    use tracing_subscriber::{filter::filter_fn, prelude::*, EnvFilter};

    let env_filter = env
        .and_then(|value| EnvFilter::try_new(value).ok())
        .unwrap_or_else(|| EnvFilter::new("info"));
    // WS のハンドシェイクや HTTP のトレースには認証情報やクエリが含まれ得る。
    let transport_ceiling = filter_fn(|meta| {
        let transport = [
            "tungstenite",
            "tokio_tungstenite",
            "reqwest",
            "hyper",
            "hyper_util",
            "h2",
        ]
            .into_iter()
        .any(|target| {
            meta.target() == target
                || meta
                    .target()
                    .strip_prefix(target)
                    .is_some_and(|suffix| suffix.starts_with("::"))
        });
        !transport || *meta.level() <= tracing::Level::INFO
    });
    tracing_subscriber::registry()
        .with(env_filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_filter(transport_ceiling),
        )
}

/// Tauri アプリを構築して実行する(desktop only)。
pub fn run() {
    use tracing_subscriber::util::SubscriberInitExt;
    log_subscriber(std::env::var("RUST_LOG").ok().as_deref(), std::io::stdout).init();

    let builder = tauri::Builder::default();
    // 単一インスタンス(SPEC「単一インスタンス」)。最初に登録する。2 つ目の起動は
    // 既存のメインウィンドウを前面に出して終了する(資格情報・マーカーの競合を防ぐ)。
    // 弾幕ウィンドウだけが残っていれば main を作り直す。ウィンドウ作成はイベントループの
    // コールバック内で同期的に行わない(Windows で止まるため)。
    #[cfg(desktop)]
    let builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { show_main_window(&app) });
    }));
    builder
        .plugin(tauri_plugin_shell::init())
        // 自動更新後の relaunch() 用。
        .plugin(tauri_plugin_process::init())
        .setup(|app| {
            // 自動更新(SPEC「自動更新」)。確認・DL・インストールは UI から
            // plugin-updater の JS API で呼ぶ。登録失敗は致命にしない。
            #[cfg(desktop)]
            if let Err(e) = app
                .handle()
                .plugin(tauri_plugin_updater::Builder::new().build())
            {
                tracing::warn!("updater プラグイン登録失敗・自動更新なしで続行: {e}");
            }

            let handle = app.handle().clone();

            // 設定ディレクトリ。
            let app_config_dir = handle.path().app_config_dir().ok();
            let config_dir = app_config_dir
                .clone()
                .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            // 稼働中のストアがディスクを読む前に、再起動前に準備した復元を適用する。
            let (restore_result, restore_blocked) =
                backup::apply_pending_restore_at_startup(&config_dir, timer_now_ms());
            let startup_restore_failed = restore_result
                .inspect_err(|error| tracing::warn!("復元に失敗しました: {error}"))
                .is_err();
            let restore_blocks = |root: &str| restore_blocked.iter().any(|blocked| blocked == root);

            let (config, load_outcome) = if restore_blocks("config.json") {
                // 復元の通知を使う。設定破損時の退避処理は起動しない。
                (AppConfig::load_read_only(&config_dir), ConfigLoadOutcome::Loaded)
            } else {
                AppConfig::load_or_preserve(&config_dir)
            };

            let obs_port = config.obs.port;

            // 内部 source broadcast(Source → パイプライン)。
            let (source_tx, _source_rx) = broadcast::channel::<ChatMessage>(sources::SOURCE_CAPACITY);

            let tts = Arc::new(TtsShared::default());

            // Goals stats 用 channel。
            let (stats_tx, stats_rx) = watch::channel::<StatsSnapshot>(StatsSnapshot::default());
            let (timer_tx, _timer_rx) = watch::channel::<TimerSnapshot>(TimerSnapshot::default());
            let (config_tx, config_rx) = watch::channel::<AppConfig>(config.clone());
            let (metadata_tx, metadata_rx) = mpsc::channel::<YoutubeMetadataUpdate>(64);
            let (reaction_tx, reaction_rx) = mpsc::channel::<Vec<YoutubeReaction>>(64);
            let (mod_tx, mod_rx) = mpsc::channel::<ModerationEvent>(1024);
            let (live_tx, live_rx) = mpsc::channel::<LiveEvent>(64);
            let (autopost_user_tx, autopost_user_rx) = mpsc::channel::<AutopostCommand>(16);
            let (autopost_status_tx, autopost_status) =
                watch::channel::<AutopostStatus>(idle_autopost_status());
            let autopost_config_rx = config_tx.subscribe();
            let ledger_path = config_dir.join(autopost::LEDGER_FILE);
            let x_post_auth = Arc::new(
                XPostAuth::new(
                    KeyringTokenStore,
                    XEndpoints::with_overrides(&config.x_overrides.endpoints),
                )
                .expect("X投稿用HTTPクライアントの作成に失敗しました"),
            );

            let twitcasting_auth = TwitcastingAuth::with_logout_files(
                integrations::KeyringSecretStore,
                TwitcastingEndpoints::default(),
                AuthOptions::default(),
                twitcasting_logout_marker(app_config_dir.as_deref()),
                handle
                    .path()
                    .app_local_data_dir()
                    .ok()
                    .filter(|dir| dir.is_absolute())
                    .map(|dir| dir.join(CREDENTIAL_LOCK_FILE)),
            );
            let twitcasting_client_id =
                effective_client_id(config.credentials.twitcasting_client_id.as_deref());
            let twitcasting_config_rx = config_tx.subscribe();
            let twitcasting_api = sources::twitcasting_api::TwitcastingApi::new(
                twitcasting_auth.clone(),
                sources::twitcasting_api::official_endpoints(&config),
                Arc::new(move || {
                    let config = twitcasting_config_rx.borrow();
                    effective_client_id(config.credentials.twitcasting_client_id.as_deref())
                }),
            );

            // 下流 Bus(パイプライン → UI/OBS)。
            let poll_config_app = handle.clone();
            let poll_event_app = handle.clone();
            let poll = Arc::new(Poll::new(
                move |change| poll_config_app.state::<AppState>().save_poll_preferences(change),
                move |snapshot| {
                    let _ = poll_event_app.emit("poll-updated", snapshot);
                },
            ));
            let bus = Bus::new(obs_port, stats_tx.clone(), timer_tx.clone()).with_poll(poll.sender());

            // モデレータ。
            let moderator = Moderator::new(&config.moderation);

            // アプリ全体の停止トークン(将来 on_window_event 等から cancel 可能)。
            let app_cancel = CancellationToken::new();
            let twitch_auth = Arc::new(
                TwitchAuth::new().expect("Twitch認証用HTTPクライアントの作成に失敗しました"),
            );
            let poll_notice_app = handle.clone();
            let poll_control = Arc::new(poll_control::PollControl::with_youtube_notice(
                poll.clone(),
                twitch_auth.clone(),
                config_tx.subscribe(),
                app_cancel.clone(),
                move |message| {
                    let _ = poll_notice_app.emit("poll-error", message);
                },
            ));
            let youtube_auth = poll_control.youtube_auth();
            let obs_cancel = app_cancel.child_token();
            let templates =
                TemplateStore::new(resolve_templates_dir(&handle), config_dir.join("templates"));
            // カスタムの保存先。失敗しても内蔵の配信は続ける。
            if !restore_blocks("templates") {
                if let Err(e) = fs::create_dir_all(templates.custom_root()) {
                    tracing::warn!(
                        "カスタムテンプレートフォルダの作成に失敗 {}: {e}",
                        templates.custom_root().display()
                    );
                }
                if let Err(e) = templates.cleanup_stale_staging() {
                    tracing::warn!("テンプレート staging の掃除に失敗: {e}");
                }
            }
            let goals_skin_dir = config_dir.join("skin");
            if !restore_blocks("skin") {
                if let Err(e) = fs::create_dir_all(&goals_skin_dir) {
                    tracing::warn!(
                        "Goals skinフォルダの作成に失敗 {}: {e}",
                        goals_skin_dir.display()
                    );
                }
            }

            let ((api_router, listener_store), _) = setup_twitcasting(
                &twitcasting_auth,
                twitcasting_client_id.as_deref(),
                || {
                    let integrations = tauri::async_runtime::block_on(integrations::Integrations::open(
                        config_dir.clone(),
                        integrations::KeyringSecretStore,
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default().as_millis() as u64,
                    ));
                    let api_feed = api::ApiFeed::new(1000);
                    api_feed.bind(&bus, &integrations.gate(), app_cancel.clone());
                    let api_rate_limit = Arc::new(api::RateLimit::new());
                    let api_router = api::router(
                        api::ApiDeps::new(integrations.gate(), obs_cancel.clone())
                            .with_feed(api_feed.clone())
                            .with_ops(Arc::new(TauriAppOps(handle.clone())))
                            .with_poll(poll_control.clone())
                            .with_rate_limit(api_rate_limit.clone()),
                        obs_port,
                    );
                    let webhook = webhook::WebhookRuntime::new(integrations.clone(), webhook::ReqwestSender::new());
                    let now_ms = timer_now_ms();
                    let achievement_defs = Arc::new(DefsStore::load_for_startup(
                        &config_dir, now_ms,
                        restore_blocks("achievements.json"),
                        restore_blocks("achievement-progress.json"),
                    ));
                    let listener_store = if restore_blocks("listeners.json") {
                        listeners::ListenerStore::load_read_only(&config_dir)
                    } else {
                        listeners::ListenerStore::load(&config_dir)
                    }.with_achievement_defs(achievement_defs.clone());
                    let defs = achievement_defs.current();
                    let achievement_engine = defs.enabled.then(|| {
                        Arc::new(AchievementEngine::open_with(
                            &config_dir, defs, now_ms, achievement_defs.progress_io(),
                        ))
                    });
                    let state = AppState {
                        webhook,
                        integrations,
                        api_feed,
                        api_rate_limit,
                        listeners: Arc::new(listener_store),
                        achievement_engine: Mutex::new(achievement_engine),
                        achievement_defs,
                        chatlog: chatlog::ChatLog::start(&config_dir, config.log.clone()),
                        sounds: sounds::SoundStore::new(&config_dir),
                        sound_gate: Mutex::new(sound_effects::SoundGate::default()),
                        keyword_effects: Mutex::new(keyword_effects::KeywordEffects::default()),
                        config: Mutex::new(config.clone()),
                        startup_config_outcome: Mutex::new(Some(load_outcome)),
                        startup_restore_failed: startup_restore_failed.into(),
                        restore_failed_at_startup: startup_restore_failed,
                        restore_blocked: restore_blocked.clone(),
                        backup_lock: Mutex::new(()),
                        last_auto_backup_failure: Mutex::new(None),
                        config_revision: AtomicU64::new(0),
                        config_tx,
                        timer_tx,
                        poll: poll.clone(),
                        poll_control,
                        config_dir,
                        source_tx: source_tx.clone(),
                        bus: bus.clone(),
                        templates: templates.clone(),
                        goals_skin_dir: goals_skin_dir.clone(),
                        app_cancel: app_cancel.clone(),
                        obs_server: Mutex::new(ObsServerControl {
                            port: obs_port,
                            cancel: obs_cancel.clone(),
                        }),
                        channels: Mutex::new(HashMap::new()),
                        moderator: Mutex::new(moderator),
                        tts,
                        metadata_tx,
                        reaction_tx,
                        mod_tx,
                        live_tx,
                        twitcasting_started_movies: Default::default(),
                        autopost_user_tx,
                        autopost_status: autopost_status.clone(),
                        config_write: tokio::sync::Mutex::new(()),
                        x_post_login: OAuthLoginGate::default(),
                        twitcasting_login: OAuthLoginGate::default(),
                        x_post_auth: x_post_auth.clone(),
                        twitcasting_auth: twitcasting_auth.clone(),
                        twitcasting_api,
                        participants: Mutex::new(Vec::new()),
                        youtube_auth,
                        twitch_auth,
                    };
                    let listener_store = state.listeners.clone();
                    move || {
                        app.manage(state);
                        (api_router, listener_store)
                    }
                },
            );
            let (autopost_live_tx, autopost_live_rx) = mpsc::channel(64);
            let (achievement_live_tx, achievement_live_rx) = mpsc::channel(64);
            let fanout_cancel = app_cancel.clone();
            tauri::async_runtime::spawn(async move {
                let mut fanout = live_fanout::spawn_live_fanout(
                    live_rx, vec![autopost_live_tx, achievement_live_tx],
                );
                tokio::select! {
                    _ = fanout_cancel.cancelled() => fanout.abort(),
                    _ = &mut fanout => {}
                }
            });
            spawn_achievement_events(handle.clone(), achievement_live_rx, stats_rx, app_cancel.clone());
            // 配信開始/終了イベント → X 自動告知。状態は変化時だけ UI へ emit する。
            autopost::spawn_autopost(
                autopost_live_rx,
                autopost_config_rx,
                autopost_user_rx,
                x_post_auth,
                ledger_path,
                autopost_status_tx,
                |url: &str| open_url(url.to_string()),
                app_cancel.clone(),
            );
            spawn_autopost_status_emitter(autopost_status, handle.clone(), app_cancel.clone());
            spawn_auto_backup(handle.clone(), app_cancel.clone());
            let listener_cancel = app_cancel.clone();
            let flush_app = handle.clone();
            tauri::async_runtime::spawn(async move {
                let mut interval = tokio::time::interval(Duration::from_secs(5));
                interval.tick().await;
                loop {
                    tokio::select! {
                        _ = listener_cancel.cancelled() => break,
                        _ = interval.tick() => {
                            let store = listener_store.clone();
                            let flush_app = flush_app.clone();
                            match tauri::async_runtime::spawn_blocking(move || {
                                let result = store.flush();
                                flush_app.state::<AppState>().flush_achievements();
                                result
                            }).await {
                                Ok(Ok(())) => {},
                                result => tracing::error!("リスナー記録の保存失敗: {result:?}"),
                            }
                        }
                    }
                }
            });

            if config.tts.backend == config::TtsBackendKind::Bouyomi {
                let path = config.tts.options.bouyomi_path.trim().to_string();
                let host = config.tts.options.bouyomi_host.clone();
                let port = config.tts.options.bouyomi_port;
                let elevated = config.tts.options.bouyomi_launch_elevated;
                tauri::async_runtime::spawn(ensure_bouyomi_launched_and_emit_notice(
                    handle.clone(),
                    path,
                    host,
                    port,
                    elevated,
                ));
            }

            // Bus の UI forwarder と OBS サーバを起動。
            bus.spawn_ui_forwarder(handle.clone(), app_cancel.clone());
            spawn_reaction_ui_forwarder(reaction_rx, handle.clone(), app_cancel.clone());
            spawn_achievement_ui_forwarder(
                bus.subscribe_achievements(),
                handle.clone(),
                app_cancel.clone(),
            );
            spawn_effect_ui_forwarder(bus.subscribe_effects(), handle.clone(), app_cancel.clone());
            spawn_achievement_tts_forwarder(
                bus.subscribe_achievements(),
                handle.clone(),
                app_cancel.clone(),
            );
            {
                let app_for_mod = handle.clone();
                spawn_moderation_ui_forwarder(
                    mod_rx,
                    handle.clone(),
                    app_cancel.clone(),
                    move |events| apply_platform_moderation(&app_for_mod, events),
                );
            }
            if let Err(e) = bus.spawn_obs_server(
                templates.builtin_root().to_path_buf(),
                templates.custom_root().to_path_buf(),
                goals_skin_dir,
                obs_cancel,
                Some(api_router),
            ) {
                tracing::error!("{e}");
            }

            // Goals stats 集約を起動。
            spawn_stats_aggregator(
                bus.subscribe(),
                stats_tx,
                config_rx,
                metadata_rx,
                handle.clone(),
                app_cancel.clone(),
            );

            // moderation + TTS パイプライン起動。
            tauri::async_runtime::spawn(poll.run(app_cancel.clone()));
            spawn_pipeline(handle.clone(), app_cancel.clone());
            spawn_webhook_forwarder(handle.clone(), app_cancel.clone());

            // 単一の長命 TTS ワーカー起動。
            spawn_tts_worker(handle.clone(), app_cancel.clone());

            // 設定済みチャンネルを起動。
            {
                let st = handle.state::<AppState>();
                apply_channel_diff(&st.config, &st.channels, |cfg, ch| {
                    spawn_one_channel(&st, cfg, ch)
                });
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_backups,
            get_last_auto_backup_failure,
            create_backup_now,
            restore_backup,
            open_backup_dir,
            take_startup_restore_failure,
            list_achievements,
            achievement_schema,
            commit_achievements,
            new_achievement,
            duplicate_achievement,
            reset_achievement_to_preset,
            listener_achievements,
            list_webhooks,
            save_webhook,
            delete_webhook,
            resume_webhook,
            test_webhook,
            reveal_webhook_signing_secret,
            regenerate_webhook_signing_secret,
            get_local_api_status,
            set_local_api_enabled,
            get_local_api_token,
            regenerate_local_api_token,
            get_listener,
            listener_donation,
            reset_listener_donation,
            reset_all_listener_donations,
            update_listener,
            set_listener_voice,
            list_listeners,
            delete_listener,
            get_config,
            take_startup_config_notice,
            get_comment_log_dir,
            open_comment_log_dir,
            get_tts_queue_state,
            set_tts_paused,
            clear_tts_queue,
            skip_current_tts,
            tts_speak_text,
            copy_text_to_clipboard,
            test_tts,
            probe_tts_engine,
            export_comments_csv,
            send_chat_message,
            get_youtube_oauth_status,
            connect_youtube_oauth,
            disconnect_youtube_oauth,
            get_twitch_oauth_status,
            begin_twitch_login,
            await_twitch_login,
            cancel_twitch_login,
            disconnect_twitch_oauth,
            moderate_delete_message,
            moderate_ban_user,
            moderate_unban_user,
            update_config,
            set_goals_show_in_app,
            append_moderation_value,
            set_moderation,
            add_channel,
            set_channel_overrides,
            remove_channel,
            hide_message,
            unhide_message,
            get_obs_url,
            get_obs_goals_url,
            get_obs_timer_url,
            get_obs_poll_url,
            poll_state,
            poll_start,
            poll_stop,
            poll_reset,
            poll_set_display,
            control_timer,
            list_templates,
            read_template_file,
            write_template_file,
            duplicate_template,
            delete_template,
            import_template_zip,
            list_custom_sounds,
            add_custom_sound,
            read_custom_sound,
            remove_custom_sound,
            get_goal_skin_info,
            get_participants,
            pick_next_participant,
            pick_random_participant,
            remove_participant,
            clear_participants,
            inject_test_comment,
            test_keyword_effect,
            check_for_update,
            open_url,
            get_x_autopost_status,
            cancel_x_autopost,
            approve_x_autopost,
            retry_x_autopost,
            preview_x_post,
            set_channel_own_stream,
            get_twitcasting_status,
            reload_twitcasting_status,
            connect_twitcasting,
            cancel_twitcasting_login,
            disconnect_twitcasting,
            get_x_post_status,
            connect_x_post,
            cancel_x_post_login,
            disconnect_x_post,
            send_x_test_post,
        ])
        .build(tauri::generate_context!())
        .expect("Tauri アプリの起動に失敗しました")
        .run(|app, event| {
            if matches!(event, tauri::RunEvent::Exit) {
                let state = app.state::<AppState>();
                state.app_cancel.cancel();
                if let Err(e) = tauri::async_runtime::block_on(state.chatlog.shutdown()) {
                    tracing::error!("終了時のコメントログ保存失敗: {e}");
                }
                if let Err(e) = state.listeners.shutdown() {
                    tracing::error!("終了時のリスナー記録の保存失敗: {e}");
                }
                state.flush_achievements();
            }
        });
}

#[cfg(test)]
mod sound_command_tests {
    use super::decode_sound_name;

    #[test]
    fn sound_name_header_is_percent_decoded_literally() {
        assert_eq!(decode_sound_name("%E3%83%94%E3%83%B3.wav"), "ピン.wav");
        assert_eq!(decode_sound_name("a%2Bb%20c.mp3"), "a+b c.mp3");
        assert_eq!(decode_sound_name(""), "");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use achievements::store::AchievementDefs;

    #[test]
    fn deleting_listener_from_disabled_engine_preserves_future_unlocks() {
        use achievements::model::{
            Achievement, Announce, Condition, CurrencyMode, Metric, SubjectKind, Tier, Window,
        };

        let dir = tempfile::tempdir().unwrap();
        let defs = AchievementDefs {
            enabled: true,
            achievements: vec![Achievement {
                id: "two-comments".into(),
                name: "Two comments".into(),
                description: String::new(),
                icon: "star".into(),
                tier: Tier::Bronze,
                subject: SubjectKind::Viewer,
                enabled: true,
                builtin_id: None,
                edited: false,
                condition: Some(Condition {
                    window: Window::Lifetime,
                    metric: Metric::Comments,
                    threshold: 2.0,
                    currency: CurrencyMode::None,
                    platforms: Vec::new(),
                }),
                announce: Announce { tts: false },
            }],
            ..AchievementDefs::default()
        };
        let evaluator = AchievementEngine::open(dir.path(), defs.clone(), 1);
        let comment = |id: &str| -> ChatMessage {
            serde_json::from_value(serde_json::json!({
                "id": id,
                "platform": "twitch",
                "channel": "fixture",
                "author": { "id": "u1", "name": "viewer" },
                "fragments": [],
                "kind": "normal",
                "timestampMs": 1
            }))
            .unwrap()
        };
        assert!(evaluator.observe(&comment("old-1"), 2).is_empty());
        assert_eq!(evaluator.observe(&comment("old-2"), 3).len(), 1);

        let mut disabled = defs.clone();
        disabled.enabled = false;
        assert!(evaluator.set_definitions(disabled, 4).is_empty());
        delete_listener_achievement_progress(Some(&evaluator), dir.path(), "twitch", "u1").unwrap();
        assert!(evaluator.observe(&comment("disabled-1"), 5).is_empty());
        assert!(evaluator.observe(&comment("disabled-2"), 6).is_empty());

        assert!(evaluator.set_definitions(defs, 7).is_empty());
        assert!(evaluator.listener_unlocks("twitch", "u1").is_empty());
        assert!(evaluator.observe(&comment("new-1"), 8).is_empty());
        assert_eq!(evaluator.observe(&comment("new-2"), 9).len(), 1);
    }

    #[test]
    fn startup_config_notice_can_be_taken_only_once() {
        for outcome in [
            ConfigLoadOutcome::BackedUp,
            ConfigLoadOutcome::SavesBlocked,
            ConfigLoadOutcome::Loaded,
        ] {
            let stored = Mutex::new(Some(outcome));
            assert_eq!(
                take_config_load_notice(&stored),
                outcome.notice().map(|message| TtsNoticePayload {
                    level: "warn",
                    message: message.to_owned(),
                })
            );
            assert_eq!(*stored.lock().unwrap(), None);
            assert_eq!(take_config_load_notice(&stored), None);
        }
    }

    #[test]
    fn tts_disabled_channel_message_is_dropped_before_enqueue() {
        struct Sink;
        impl tts::TtsSink for Sink {
            fn audio(&self, _: String) {}
            fn web_speech(&self, _: tts::WebSpeechPayload) {}
            fn cancel(&self) {}
            fn notice(&self, _: &'static str, _: String) {}
            fn queue_state(&self, _: TtsQueueStatePayload) {}
        }

        let channel = ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "fixture".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides {
                tts_enabled: Some(false),
                ..Default::default()
            },
        };
        let key = config::channel_key(&channel);
        let mut config = AppConfig::default();
        config.channels.push(channel);
        config.tts.options.queue.max_pending = 1;
        let config = Mutex::new(config);
        let queue = TtsShared::default();
        let allowed: ChatMessage = serde_json::from_value(serde_json::json!({
            "id": "allowed",
            "platform": "twitch",
            "channel": "fixture",
            "author": { "id": "u1", "name": "viewer" },
            "fragments": [],
            "kind": "normal",
            "timestampMs": 1
        }))
        .unwrap();
        let mut disabled = allowed.clone();
        disabled.id = "disabled".into();
        disabled.route.origin_key = Some(key);

        try_enqueue_tts_message(&Sink, &config, &queue, allowed);
        try_enqueue_tts_message(&Sink, &config, &queue, disabled);

        let snapshot = queue.snapshot();
        assert_eq!(snapshot.depth, 1);
        assert_eq!(snapshot.items[0].id, "allowed");
    }

    #[tokio::test]
    async fn spawned_channel_messages_reach_pipeline_with_origin_key() {
        let ch = ChannelConfig {
            platform: ChannelPlatform::Youtube,
            identifier: "@fixture".into(),
            enabled: true,
            own_stream: false,
            overrides: Default::default(),
        };
        let (upstream, mut pipeline_rx) = broadcast::channel(sources::SOURCE_CAPACITY);
        let cancel = CancellationToken::new();
        let source_tx = channel_source_sender(&ch, &upstream, &cancel);
        let message: ChatMessage = serde_json::from_value(serde_json::json!({
            "id": "first",
            "platform": "youtube",
            "channel": "ABCDEFGHIJK",
            "author": { "id": "u1", "name": "viewer" },
            "fragments": [],
            "kind": "normal",
            "timestampMs": 1
        }))
        .unwrap();
        source_tx.send(message.clone()).unwrap();
        let received = tokio::time::timeout(Duration::from_secs(2), pipeline_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let mut expected = message;
        expected.route.origin_key = Some(config::channel_key(&ch));
        assert_eq!(received, expected);

        cancel.cancel();
        drop(upstream);
        let closed = tokio::time::timeout(Duration::from_secs(2), pipeline_rx.recv())
            .await
            .unwrap();
        assert!(matches!(closed, Err(broadcast::error::RecvError::Closed)));
    }
}

#[cfg(test)]
mod followup_tests {
    use super::*;

    #[test]
    fn ng_append_matches_javascript_trim_and_deduplicates_bom() {
        let mut rules = config::ModerationConfig {
            ng_users: vec!["bot".into()],
            ..config::ModerationConfig::default()
        };
        assert_eq!(
            append_ng_value(&mut rules.ng_users, "\u{FEFF}bot").as_deref(),
            Some("bot")
        );
        assert_eq!(rules.ng_users, ["bot"]);

        let js_whitespace = "\t\n\u{000B}\u{000C}\r \u{00A0}\u{1680}\u{2000}\u{2001}\u{2002}\u{2003}\u{2004}\u{2005}\u{2006}\u{2007}\u{2008}\u{2009}\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}";
        assert_eq!(append_ng_value(&mut rules.ng_users, js_whitespace), None);
        let preserved = "\u{0085}\u{180E}bot\u{200B}";
        assert_eq!(
            append_ng_value(&mut rules.ng_users, preserved).as_deref(),
            Some(preserved)
        );
        assert_eq!(rules.ng_users, ["bot", preserved]);
    }

    // 指摘1: UI は `protectedOAuth` を読む。camelCase 変換の `protectedOauth` では届かない。
    #[test]
    fn oauth_review_update_config_outcome_uses_the_ui_key() {
        let outcome = UpdateConfigOutcome {
            warnings: vec!["w".into()],
            protected_oauth: vec![OAuthService::XPost, OAuthService::Twitcasting],
            revision: 7,
            config: AppConfig::default(),
        };
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(value["warnings"], serde_json::json!(["w"]));
        assert_eq!(value["protectedOAuth"], serde_json::json!(["xPost", "twitcasting"]));
        assert_eq!(value["revision"], 7);
        assert!(value["config"].is_object());
    }

    // ログイン待ちの間だけ、update_config の保護判定が立つ。
    #[tokio::test]
    async fn oauth_review_pending_login_protects_the_config_until_it_ends() {
        let gate = OAuthLoginGate::default();
        let current = AppConfig::default();
        let protect = |gate: &OAuthLoginGate| {
            let mut next = AppConfig::default();
            next.credentials.twitcasting_client_id = Some("other".into());
            next.twitcasting.callback_port = current.twitcasting.callback_port + 1;
            protect_oauth_config(&current, &mut next, |service| {
                service == OAuthService::Twitcasting && gate.is_active()
            })
        };

        assert!(protect(&gate).is_empty());
        let mut login = gate.begin(Uuid::new_v4()).await.unwrap();
        let _ = login.open_browser("url".into(), |_| Ok(()));
        // ブラウザを開いた後も、ログインが終わるまで保護する。
        assert_eq!(protect(&gate), [OAuthService::Twitcasting]);
        drop(login);
        assert!(protect(&gate).is_empty());
    }

    fn twitcasting_auth_for_gate() -> Arc<TwitcastingAuth<integrations::MemorySecretStore>> {
        TwitcastingAuth::new(
            integrations::MemorySecretStore::new(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        )
    }

    #[tokio::test]
    async fn oauth_review_cancel_before_begin_prevents_login() {
        let gate = OAuthLoginGate::default();
        let attempt = Uuid::new_v4();
        let begin = gate.begin(attempt);
        gate.cancel(attempt);
        // 合計 32 件までは、最初に受け付けた先行取消も残る。
        for _ in 0..31 {
            gate.cancel(Uuid::new_v4());
        }
        assert!(begin.await.is_none());
        assert!(gate.begin(attempt).await.is_none());
        assert!(!gate.is_active());
    }

    #[tokio::test]
    async fn oauth_review_finished_attempt_cannot_begin_again() {
        let gate = OAuthLoginGate::default();
        let attempt = Uuid::new_v4();
        drop(gate.begin(attempt).await.unwrap());
        assert!(gate.begin(attempt).await.is_none());
        gate.cancel(attempt);
        assert!(gate.begin(attempt).await.is_none());
        assert!(!gate.is_active());
    }

    #[tokio::test]
    async fn oauth_review_bounds_early_and_late_cancels_without_evicting_running() {
        let gate = OAuthLoginGate::default();
        let finished = Uuid::new_v4();
        drop(gate.begin(finished).await.unwrap());
        let running = Uuid::new_v4();
        let mut login = gate.begin(running).await.unwrap();
        login.open_browser(String::new(), |_| Ok(())).unwrap();

        gate.cancel(finished);
        let pending: Vec<_> = (0..32).map(|_| Uuid::new_v4()).collect();
        for &attempt in &pending {
            gate.cancel(attempt);
            gate.cancel(attempt);
        }

        // 終了済み試行は最古なので、新しい取消 32 件を残して捨てる。
        for attempt in pending {
            assert!(gate.begin(attempt).await.is_none());
        }
        drop(gate.begin(finished).await.unwrap());
        assert!(gate.is_active());
        assert!(!login.cancelled());
        gate.cancel(running);
        assert!(login.cancelled());
        drop(login);
        assert!(gate.begin(running).await.is_none());
        assert!(!gate.is_active());
    }

    #[tokio::test]
    async fn oauth_review_duplicate_begin_keeps_the_first_login() {
        let gate = OAuthLoginGate::default();
        let attempt = Uuid::new_v4();
        let mut first = gate.begin(attempt).await.unwrap();
        let duplicate = gate.begin(attempt);
        tokio::pin!(duplicate);
        assert!(matches!(futures_util::poll!(duplicate), std::task::Poll::Ready(None)));
        assert!(gate.is_active());
        assert!(!first.cancelled());
        first.open_browser(String::new(), |_| Ok(())).unwrap();
        gate.cancel(attempt);
        assert!(first.cancelled());
        drop(first);
        assert!(!gate.is_active());
    }

    #[tokio::test]
    async fn oauth_review_cancel_while_waiting_for_the_previous_login_applies() {
        let gate = OAuthLoginGate::default();
        let first = gate.begin(Uuid::new_v4()).await.unwrap();
        let attempt = Uuid::new_v4();
        let second = gate.begin(attempt);
        tokio::pin!(second);
        assert!(futures_util::poll!(second.as_mut()).is_pending());
        gate.cancel(attempt);
        assert!(matches!(futures_util::poll!(second.as_mut()), std::task::Poll::Ready(None)));
        assert!(!first.cancelled());
        drop(first);
        assert!(!gate.is_active());
    }

    #[test]
    fn oauth_review_rejects_invalid_attempt_ids_without_echoing_input() {
        assert_eq!(parse_oauth_attempt("client-id"), Err("ログインの試行 ID が不正です"));
    }

    // 古い試行の取消は、ツイキャスの後続ログインを止めない。
    #[tokio::test]
    async fn oauth_review_stale_cancel_does_not_stop_a_newer_twitcasting_login() {
        let gate = Arc::new(OAuthLoginGate::default());
        let auth = twitcasting_auth_for_gate();
        auth.load(Some("client"), 0).await;
        let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let mut task = {
            let gate = gate.clone();
            tokio::spawn(async move {
                let mut login = gate.begin(b).await.unwrap();
                auth.connect_on_with_cancel(0, "client", 0, login.cancel.clone(), |url| {
                    login.open_browser(url, |_| {
                        let _ = opened_tx.send(());
                        Ok(())
                    })
                })
                .await
                .map(|_| ())
                .map_err(|e| e.code)
            })
        };
        opened_rx.await.unwrap();
        gate.cancel(a);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut task).await.is_err());
        gate.cancel(b);
        assert!(matches!(
            task.await.unwrap(),
            Err(TwitcastingAuthErrorCode::Cancelled)
        ));
    }

    // X では後続ログインが前の試行を置き換え、前の試行を取り消しても後続は待受を続ける。
    #[tokio::test]
    async fn oauth_review_x_post_logins_replace_in_order_and_cancel_by_attempt() {
        use crate::autopost::oauth::MemoryTokenStore;

        let gate = Arc::new(OAuthLoginGate::default());
        let auth = Arc::new(XPostAuth::new(MemoryTokenStore::new(), XEndpoints::default()).unwrap());
        let start = |attempt: Uuid| {
            let (gate, auth) = (gate.clone(), auth.clone());
            let (opened_tx, opened_rx) = tokio::sync::oneshot::channel();
            let task = tokio::spawn(async move {
                let mut login = gate.begin(attempt).await.unwrap();
                auth.connect_with_cancel("client", 0, login.cancel.clone(), |url| {
                    login.open_browser(url, |_| {
                        let _ = opened_tx.send(());
                        Ok(())
                    })
                })
                .await
                .map(|_| ())
                .map_err(|e| e.code)
            });
            (task, opened_rx)
        };
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let (first, first_opened) = start(a);
        first_opened.await.unwrap();
        let (mut second, second_opened) = start(b);
        second_opened.await.unwrap();
        assert_eq!(first.await.unwrap(), Err(XPostErrorCode::Cancelled));
        gate.cancel(a);
        assert!(tokio::time::timeout(Duration::from_millis(50), &mut second).await.is_err());
        gate.cancel(b);
        assert_eq!(second.await.unwrap(), Err(XPostErrorCode::Cancelled));
    }

    // 指摘5: 保存の順と認証セッションの登録順が逆転しない(後に保存した方が最後に登録される)。
    #[tokio::test]
    async fn oauth_review_concurrent_logins_register_in_the_order_they_saved() {
        let gate = OAuthLoginGate::default();
        let saved = Mutex::new(Vec::new());
        let registered = Mutex::new(Vec::new());
        let login = |name: &'static str, delay: usize| {
            let (gate, saved, registered) = (&gate, &saved, &registered);
            async move {
                let mut login = gate.begin(Uuid::new_v4()).await.unwrap();
                saved.lock().unwrap().push(name);
                // 先に保存した方ほど、認証側の登録まで時間がかかる。
                for _ in 0..delay {
                    tokio::task::yield_now().await;
                }
                registered.lock().unwrap().push(name);
                let _ = login.open_browser(name.into(), |_| Ok(()));
            }
        };
        tokio::join!(login("A", 10), login("B", 0));
        let saved = saved.into_inner().unwrap();
        assert_eq!(registered.into_inner().unwrap(), saved);
    }

    #[test]
    fn oauth_review_config_update_preserves_only_active_services() {
        let mut current = AppConfig::default();
        current.credentials.x_post_client_id = Some("current-x".into());
        current.credentials.twitcasting_client_id = Some("current-tc".into());
        current.x_post.callback_port = 41321;
        current.twitcasting.callback_port = 41322;
        for active in [
            &[][..],
            &[OAuthService::XPost],
            &[OAuthService::Twitcasting],
            &[OAuthService::XPost, OAuthService::Twitcasting],
        ] {
            let mut incoming = AppConfig::default();
            incoming.credentials.x_post_client_id = Some("backup-x".into());
            incoming.credentials.twitcasting_client_id = Some("backup-tc".into());
            incoming.x_post.callback_port = 42321;
            incoming.twitcasting.callback_port = 42322;
            incoming.x_post.enabled = !current.x_post.enabled;
            incoming.goals.show_in_app = !current.goals.show_in_app;

            let protected = protect_oauth_config(&current, &mut incoming, |service| {
                active.contains(&service)
            });
            assert_eq!(protected, active);
            let expected_x = if active.contains(&OAuthService::XPost) {
                ("current-x", 41321)
            } else {
                ("backup-x", 42321)
            };
            let expected_tc = if active.contains(&OAuthService::Twitcasting) {
                ("current-tc", 41322)
            } else {
                ("backup-tc", 42322)
            };
            assert_eq!(
                incoming.credentials.x_post_client_id.as_deref(),
                Some(expected_x.0)
            );
            assert_eq!(incoming.x_post.callback_port, expected_x.1);
            assert_eq!(
                incoming.credentials.twitcasting_client_id.as_deref(),
                Some(expected_tc.0)
            );
            assert_eq!(incoming.twitcasting.callback_port, expected_tc.1);
            assert_ne!(incoming.x_post.enabled, current.x_post.enabled);
            assert_ne!(incoming.goals.show_in_app, current.goals.show_in_app);
        }
    }

    #[test]
    fn twitcasting_duplicates_are_detected_by_screen_id() {
        let tc = |id: &str| -> ChannelConfig {
            serde_json::from_value(serde_json::json!({"platform": "twitcasting", "identifier": id}))
                .unwrap()
        };
        let mut cfg = AppConfig::default();
        cfg.channels.push(tc("alice"));
        for dup in ["https://twitcasting.tv/alice", "ALICE", "https://twitcasting.tv/Alice/movie/1"] {
            assert_eq!(
                ensure_new_channel(&cfg, &tc(dup), None).unwrap_err(),
                "チャンネルは既に存在します",
                "{dup}"
            );
        }
        assert_eq!(ensure_new_channel(&cfg, &tc("bob"), None), Ok(()));
        // 他のプラットフォームは従来どおり生の識別子で比べる。
        let mut twitch = tc("alice");
        twitch.platform = config::ChannelPlatform::Twitch;
        assert_eq!(ensure_new_channel(&cfg, &twitch, None), Ok(()));
    }

    #[test]
    fn channel_errors_do_not_include_the_identifier() {
        let mut cfg = AppConfig::default();
        let channel: ChannelConfig = serde_json::from_value(serde_json::json!({
            "platform": "twitcasting",
            "identifier": "https://invalid.example/?token=MARKER",
        }))
        .unwrap();
        assert_eq!(ensure_new_channel(&cfg, &channel, None), Ok(()));
        cfg.channels.push(channel.clone());
        let duplicate = ensure_new_channel(&cfg, &channel, None).unwrap_err();
        assert_eq!(duplicate, "チャンネルは既に存在します");
        let missing =
            set_own_stream_in(&mut cfg, "twitcasting:https://other.example/?token=MARKER", true)
                .unwrap_err();
        assert_eq!(missing, "チャンネルが見つかりません");
        for text in [duplicate, missing] {
            assert!(!text.contains("MARKER") && !text.contains("example"), "{text}");
        }
    }

    #[test]
    fn twitcasting_login_protection_does_not_revert_mode() {
        let mut current = AppConfig::default();
        current.credentials.twitcasting_client_id = Some("current-tc".into());
        current.twitcasting.callback_port = 41322;
        let mut incoming = current.clone();
        incoming.credentials.twitcasting_client_id = Some("backup-tc".into());
        incoming.twitcasting.callback_port = 42322;
        incoming.twitcasting.mode = config::TwitcastingMode::Official;

        let protected = protect_oauth_config(&current, &mut incoming, |service| {
            service == OAuthService::Twitcasting
        });

        assert_eq!(protected, [OAuthService::Twitcasting]);
        assert_eq!(
            incoming.credentials.twitcasting_client_id.as_deref(),
            Some("current-tc")
        );
        assert_eq!(incoming.twitcasting.callback_port, 41322);
        assert_eq!(incoming.twitcasting.mode, config::TwitcastingMode::Official);

        // mode だけの変更は保護の対象にならず、そのまま通る。
        let mut incoming = current.clone();
        incoming.twitcasting.mode = config::TwitcastingMode::Unofficial;
        let protected = protect_oauth_config(&current, &mut incoming, |_| true);
        assert!(protected.is_empty());
        assert_eq!(incoming.twitcasting.mode, config::TwitcastingMode::Unofficial);
    }

    #[test]
    fn oauth_review_connect_callback_port_errors_omit_the_json_value() {
        const PUBLIC_ERROR: &str = "待受ポートは 1024〜65535 の整数にしてください";
        for value in [
            serde_json::json!("https://example.test/?token=TEST"),
            serde_json::json!({"sensitive": "TEST"}),
            serde_json::json!(null),
            serde_json::json!(-1),
            serde_json::json!(65536),
            serde_json::json!(1024.5),
        ] {
            assert_eq!(parse_oauth_callback_port(value).unwrap_err(), PUBLIC_ERROR);
        }
        for value in [1024, 65535] {
            assert_eq!(parse_oauth_callback_port(serde_json::json!(value)), Ok(value));
        }
    }

    #[test]
    fn oauth_review_twitcasting_connect_rejects_callback_ports_below_1024() {
        for value in [0, 1023] {
            assert!(parse_oauth_callback_port(serde_json::json!(value)).is_err());
        }
    }

    // 修正10-2: 2 つ目の起動で main が無い時に作り直す設定を tauri.conf.json から取れる。
    #[test]
    fn revision10_main_window_config_comes_from_tauri_conf() {
        let conf: serde_json::Value =
            serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        let windows: Vec<tauri::utils::config::WindowConfig> =
            serde_json::from_value(conf["app"]["windows"].clone()).unwrap();
        let main = main_window_config(&windows).expect("main の設定が無い");
        assert_eq!(main.label, "main");
        assert_eq!(main.title, "fast-comment");
        assert!(main_window_config(&[]).is_none());
    }

    fn observe_twitcasting_setup() -> (Vec<&'static str>, TwitcastingStatus) {
        use crate::integrations::SecretStore;

        #[derive(Clone)]
        struct ObservedStore(Arc<Mutex<Vec<&'static str>>>);

        impl SecretStore for ObservedStore {
            async fn get(&self, key: &str) -> Result<Option<String>, String> {
                self.0.lock().unwrap().push(match key {
                    "local-api-token" => "integration read",
                    "twitcasting-oauth-token" => "auth read",
                    _ => panic!("unexpected credential read"),
                });
                Ok(None)
            }

            async fn put(&self, _: &str, _: &str) -> Result<(), String> {
                panic!("unexpected credential write");
            }

            async fn delete(&self, _: &str) -> Result<(), String> {
                panic!("unexpected credential deletion");
            }
        }

        let events = Arc::new(Mutex::new(Vec::new()));
        let store = ObservedStore(events.clone());
        let dir = tempfile::tempdir().unwrap();
        let auth = TwitcastingAuth::new(
            store.clone(),
            TwitcastingEndpoints::default(),
            AuthOptions::default(),
        );
        let auth_at_manage = auth.clone();
        let events_at_manage = events.clone();
        let (at_manage, initialization) = setup_twitcasting(&auth, Some("client"), || {
            tauri::async_runtime::block_on(integrations::Integrations::open(
                dir.path().to_path_buf(),
                store,
                0,
            ));
            events.lock().unwrap().push("integrations opened");
            move || {
                let status = auth_at_manage.status(Some("client"), 0);
                events_at_manage.lock().unwrap().push("managed");
                status
            }
        });
        tauri::async_runtime::block_on(initialization).unwrap();
        let events = events.lock().unwrap().clone();
        (events, at_manage)
    }

    #[test]
    fn revision11_twitcasting_is_loading_when_managed() {
        let (_, at_manage) = observe_twitcasting_setup();
        assert_eq!(at_manage.state, "loading");
    }

    #[test]
    fn revision11_twitcasting_reads_after_integrations_open_and_state_registration() {
        let (events, _) = observe_twitcasting_setup();
        assert_eq!(
            events,
            [
                "integration read",
                "integrations opened",
                "managed",
                "auth read"
            ]
        );
    }

    #[test]
    fn revision8_marker_requires_an_absolute_app_config_dir() {
        assert_eq!(twitcasting_logout_marker(None), None);
        assert_eq!(
            twitcasting_logout_marker(Some(std::path::Path::new("."))),
            None
        );
        let dir = std::env::temp_dir();
        assert_eq!(
            twitcasting_logout_marker(Some(&dir)),
            Some(dir.join(LOGOUT_MARKER_FILE))
        );
    }

    #[test]
    fn integrations_commands_allow_only_main_window() {
        assert!(require_main_integrations_window("main").is_ok());
        for label in ["danmaku", "settings", "main-2", "Main", ""] {
            assert!(matches!(
                require_main_integrations_window(label),
                Err(integrations::IntegrationError::ForbiddenWindow)
            ));
        }
    }

    #[test]
    fn set_own_stream_changes_only_the_flag_of_the_matching_row() {
        let mut cfg = AppConfig::default();
        cfg.channels = vec![
            ChannelConfig {
                platform: ChannelPlatform::Twitch,
                identifier: "mine".into(),
                enabled: true,
                own_stream: false,
                overrides: ChannelOverrides::default(),
            },
            ChannelConfig {
                platform: ChannelPlatform::Youtube,
                identifier: "@other".into(),
                enabled: true,
                own_stream: false,
                overrides: ChannelOverrides::default(),
            },
        ];
        let before = cfg.channels.clone();
        set_own_stream_in(&mut cfg, "twitch:mine", true).unwrap();
        assert!(cfg.channels[0].own_stream);
        assert_eq!(cfg.channels[1], before[1]);
        // 再起動判定に使う識別キーと有効フラグは変わらない(Source を張り直さない)。
        assert_eq!(AppState::channel_key(&cfg.channels[0]), "twitch:mine");
        assert_eq!(cfg.channels[0].enabled, before[0].enabled);
        assert!(set_own_stream_in(&mut cfg, "twitch:missing", true).is_err());
    }

    #[test]
    fn template_without_url_is_rejected_before_save() {
        assert!(validate_x_post_template("配信開始").is_err());
        assert!(validate_x_post_template("{title}\n{url}").is_ok());
        assert!(validate_x_post_template(&AppConfig::default().x_post.template).is_ok());
    }

    #[test]
    fn x_test_post_text_has_no_url_and_includes_minute() {
        use chrono::TimeZone;
        let now = chrono::Local.with_ymd_and_hms(2026, 11, 14, 9, 5, 0).unwrap();
        let text = x_test_post_text(now);
        assert_eq!(text, "fast-comment 連携テスト 2026/11/14 09:05");
        assert!(!text.contains("http"));
    }

    #[test]
    fn preview_x_post_renders_sample_with_rust_side_length() {
        let rendered = preview_x_post("{title}\n{url}".into()).unwrap();
        assert!(rendered.text.contains("配信タイトルの見本"));
        assert!(rendered.text.contains("https://www.youtube.com/watch?v="));
        assert!(rendered.weighted_len > 0);
    }

    #[test]
    fn local_hide_event_requires_known_platform_channel_and_id() {
        assert_eq!(
            local_hide_event("m1", Some(" Twitch "), Some("ch")),
            Some(ModerationEvent::MessageDeleted {
                platform: Platform::Twitch,
                channel: "ch".into(),
                message_id: "m1".into(),
            })
        );
        assert!(local_hide_event("m1", Some("youtube"), Some("vid")).is_some());
        // 旧 UI(platform/channel なし)・空 ID・未知プラットフォームは OBS へ送らない。
        assert_eq!(local_hide_event("m1", None, None), None);
        assert_eq!(local_hide_event("", Some("twitch"), Some("ch")), None);
        assert_eq!(local_hide_event("m1", Some("twitch"), Some(" ")), None);
        assert_eq!(local_hide_event("m1", Some("mystery"), Some("ch")), None);
    }

    struct TestDir(PathBuf);
    impl TestDir {
        fn new(tag: &str) -> Self {
            let path = std::env::temp_dir()
                .join(format!("fast-comment-{tag}-{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn enabled_log() -> config::LogConfig {
        config::LogConfig {
            enabled: true,
            ..config::LogConfig::default()
        }
    }

    #[tokio::test]
    async fn chatlog_configure_failure_is_nonfatal_and_retried() {
        let dir = TestDir::new("chatlog-nonfatal");
        // logs をファイルにしてログ用ディレクトリ作成を失敗させる。
        std::fs::write(dir.0.join("logs"), b"not a dir").unwrap();
        let log = chatlog::ChatLog::start(
            &dir.0,
            config::LogConfig {
                enabled: false,
                ..config::LogConfig::default()
            },
        );
        // 失敗しても警告メッセージを返すだけで、呼び出し側(update_config)は続行する。
        let warning = configure_chatlog_nonfatal(&log, enabled_log()).await;
        assert!(warning.unwrap().contains("コメントログを開けませんでした"));
        // 実際に失敗経路を通っていること、かつワーカーは生存し再試行することを確認。
        assert!(log.configure(enabled_log()).await.is_err());
        std::fs::remove_file(dir.0.join("logs")).unwrap();
        assert!(configure_chatlog_nonfatal(&log, enabled_log()).await.is_none());
        assert!(chatlog::log_dir(&dir.0).is_dir());
        log.shutdown().await.unwrap();
    }

    fn commit_parts() -> (tempfile::TempDir, AtomicU64, watch::Sender<AppConfig>, AppConfig) {
        let dir = tempfile::tempdir().unwrap();
        let (tx, _rx) = watch::channel(AppConfig::default());
        (dir, AtomicU64::new(0), tx, AppConfig::default())
    }

    #[test]
    fn set_channel_overrides_does_not_restart_source() {
        let (dir, revision, tx, mut current) = commit_parts();
        let channel = ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "active".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides::default(),
        };
        let key = AppState::channel_key(&channel);
        current.channels.push(channel);
        let config = Mutex::new(current);
        let token = CancellationToken::new();
        let active = Mutex::new(HashMap::from([(key.clone(), token.clone())]));
        let overrides = ChannelOverrides {
            tts_enabled: Some(false),
            ..Default::default()
        };
        {
            let mut current = config.lock().unwrap();
            assert_eq!(
                commit_channel_overrides(dir.path(), &revision, &tx, &mut current, &key, overrides.clone(), 0)
                    .unwrap(),
                1
            );
            assert_eq!(current.channel_overrides(&key), Some(&overrides));
        }
        let mut starts = 0;
        apply_channel_diff(&config, &active, |_, _| {
            starts += 1;
            CancellationToken::new()
        });
        assert_eq!(starts, 0);
        assert!(!token.is_cancelled());
        assert!(active.lock().unwrap().contains_key(&key));
    }

    #[test]
    fn add_channel_with_replaces_commits_once_and_inherits_overrides() {
        for enabled in [true, false] {
            let (dir, revision, tx, mut current) = commit_parts();
            let overrides = ChannelOverrides {
                tts_enabled: Some(false),
                ..Default::default()
            };
            let old = ChannelConfig {
                platform: ChannelPlatform::Niconico,
                identifier: "lv1".into(),
                enabled: true,
                own_stream: true,
                overrides: overrides.clone(),
            };
            let new = ChannelConfig {
                identifier: "lv2".into(),
                enabled,
                own_stream: false,
                overrides: ChannelOverrides::default(),
                ..old.clone()
            };
            let old_key = AppState::channel_key(&old);
            let new_key = AppState::channel_key(&new);
            current.channels.push(old);
            let old_token = CancellationToken::new();
            let active = Mutex::new(HashMap::from([(old_key.clone(), old_token.clone())]));
            let mut starts = 0;

            let saved = add_channel_and_commit(
                &mut current,
                &active,
                new,
                std::slice::from_ref(&old_key),
                |cfg, next| commit_config(dir.path(), &revision, &tx, cfg, next, None),
                |cfg, ch| {
                    assert!(old_token.is_cancelled());
                    assert_eq!(&cfg.channels[0], ch);
                    assert_eq!(ch.overrides, overrides);
                    starts += 1;
                    CancellationToken::new()
                },
            )
            .unwrap();
            assert_eq!(saved.revision, Some(1));
            assert_eq!(revision.load(Ordering::SeqCst), 1);
            assert_eq!(saved.saved.channels, current.channels);
            assert_eq!(current.channels.len(), 1);
            assert_eq!(current.channel_overrides(&new_key), Some(&overrides));
            assert!(!current.channels[0].own_stream);
            assert_eq!(tx.borrow().channels, current.channels);
            assert_eq!(AppConfig::load(dir.path()).unwrap().channels, current.channels);
            assert!(old_token.is_cancelled());
            assert_eq!(starts, usize::from(enabled));
            let active = active.lock().unwrap();
            assert!(!active.contains_key(&old_key));
            assert_eq!(active.contains_key(&new_key), enabled);
        }
    }

    #[tokio::test]
    async fn channel_deleted_after_config_save_is_not_restarted_by_diff() {
        let (dir, revision, tx, initial) = commit_parts();
        let config = Mutex::new(initial);
        let active = Mutex::new(HashMap::new());
        let (saved, wait_for_save) = tokio::sync::oneshot::channel();
        let (deleted, wait_for_delete) = tokio::sync::oneshot::channel();
        let save_and_reconcile = async {
            {
                let mut current = config.lock().unwrap();
                let mut next = current.clone();
                next.channels = ["alice", "bob"]
                    .into_iter()
                    .map(|identifier| ChannelConfig {
                        platform: ChannelPlatform::Twitcasting,
                        identifier: identifier.into(),
                        enabled: true,
                        own_stream: false,
                        overrides: ChannelOverrides::default(),
                    })
                    .collect();
                commit_config(dir.path(), &revision, &tx, &mut current, next, Some(0)).unwrap();
            }
            saved.send(()).unwrap();
            wait_for_delete.await.unwrap();
            apply_channel_diff(&config, &active, |_, ch| {
                assert_eq!(ch.identifier, "bob");
                CancellationToken::new()
            });
        };
        let delete = async {
            wait_for_save.await.unwrap();
            let mut current = config.lock().unwrap();
            let mut next = current.clone();
            next.channels.retain(|ch| ch.identifier != "alice");
            commit_config(dir.path(), &revision, &tx, &mut current, next, None).unwrap();
            deleted.send(()).unwrap();
        };
        tokio::join!(save_and_reconcile, delete);

        let active = active.lock().unwrap();
        assert!(!active.contains_key("twitcasting:alice"));
        assert!(active.contains_key("twitcasting:bob"));
    }

    #[tokio::test]
    async fn channel_cancel_relay_exits_on_channel_stop_and_propagates_app_stop() {
        let app = CancellationToken::new();
        let channel = CancellationToken::new();
        let relay = tokio::spawn(relay_channel_cancel(app.clone(), channel.clone()));
        channel.cancel();
        tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .unwrap()
            .unwrap();
        assert!(!app.is_cancelled());

        let channel = CancellationToken::new();
        let relay = tokio::spawn(relay_channel_cancel(app.clone(), channel.clone()));
        app.cancel();
        tokio::time::timeout(Duration::from_secs(1), relay)
            .await
            .unwrap()
            .unwrap();
        assert!(channel.is_cancelled());
    }

    #[test]
    fn commit_config_advances_revision_and_persists() {
        let (dir, revision, tx, mut current) = commit_parts();
        let mut next = current.clone();
        next.goals.show_in_app = true;
        assert_eq!(commit_config(dir.path(), &revision, &tx, &mut current, next.clone(), None).unwrap(), 1);
        next.moderation.ng_words = vec!["a".into()];
        assert_eq!(commit_config(dir.path(), &revision, &tx, &mut current, next, Some(1)).unwrap(), 2);
        assert_eq!(revision.load(Ordering::SeqCst), 2);
        assert_eq!(current.moderation.ng_words, vec!["a".to_string()]);
        assert!(tx.borrow().goals.show_in_app);
        let loaded = AppConfig::load(dir.path()).unwrap();
        assert!(loaded.goals.show_in_app);
        assert_eq!(loaded.moderation.ng_words, vec!["a".to_string()]);
    }

    #[test]
    fn stale_base_revision_update_is_rejected_without_changes() {
        let (dir, revision, tx, mut current) = commit_parts();
        // 画面が revision 0 で読んだ後、別経路(NG 追加など)が保存した。
        let mut patched = current.clone();
        patched.moderation.ng_words = vec!["spam".into()];
        commit_config(dir.path(), &revision, &tx, &mut current, patched, None).unwrap();

        let mut stale_form = AppConfig::default();
        stale_form.ui.notify_sound = !stale_form.ui.notify_sound;
        let err = commit_config(dir.path(), &revision, &tx, &mut current, stale_form, Some(0)).unwrap_err();
        assert!(matches!(err, ConfigCommitError::Stale));
        assert_eq!(err.to_string(), CONFIG_CHANGED_ERROR);
        assert_eq!(revision.load(Ordering::SeqCst), 1);
        assert_eq!(current.moderation.ng_words, vec!["spam".to_string()]);
        assert_eq!(tx.borrow().moderation.ng_words, vec!["spam".to_string()]);
        assert_eq!(AppConfig::load(dir.path()).unwrap().moderation.ng_words, vec!["spam".to_string()]);
    }

    #[test]
    fn stale_base_revision_is_reported_before_type_errors() {
        let mut value = serde_json::to_value(AppConfig::default()).unwrap();
        value["log"]["retentionDays"] = serde_json::json!("bad");
        assert_eq!(parse_config_update(1, Some(0), value.clone()).unwrap_err(), CONFIG_CHANGED_ERROR);
        let type_error = parse_config_update(1, Some(1), value.clone()).unwrap_err();
        assert_ne!(type_error, CONFIG_CHANGED_ERROR);
        assert!(!type_error.contains("bad"));
        assert!(parse_config_update(1, None, value).is_err());
    }

    #[test]
    fn commit_config_save_failure_keeps_revision_and_memory() {
        let (dir, revision, tx, mut current) = commit_parts();
        let file = dir.path().join("not-a-dir");
        std::fs::write(&file, b"x").unwrap();
        let mut next = current.clone();
        next.goals.show_in_app = true;
        let err = commit_config(&file, &revision, &tx, &mut current, next, None).unwrap_err();
        assert!(matches!(err, ConfigCommitError::Save(_)));
        assert_eq!(revision.load(Ordering::SeqCst), 0);
        assert!(!current.goals.show_in_app);
        assert!(!tx.borrow().goals.show_in_app);
    }

    #[test]
    fn every_config_write_goes_through_commit_config() {
        // revision を進めずに設定を保存・差し替える経路が無いことを、クレートの全ソースで見る。
        // AppConfig の保存先は設定ディレクトリなので、save に設定ディレクトリ(config_dir)を渡す呼び出しを禁じる。
        fn visit(dir: &std::path::Path, out: &mut Vec<(PathBuf, String)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    visit(&path, out);
                } else if path.extension().is_some_and(|e| e == "rs") {
                    out.push((path.clone(), std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        let mut files = Vec::new();
        visit(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src"), &mut files);
        assert!(files.len() > 10);
        let banned = [
            ["config_tx", ".send("].concat(),
            ["config_tx", ".send_replace("].concat(),
            ["config_tx", ".send_modify("].concat(),
            ["AppConfig::", "save("].concat(),
        ];
        let save_call = ["." , "save("].concat();
        for (path, source) in &files {
            let compact: String = source.chars().filter(|c| !c.is_whitespace()).collect();
            for needle in &banned {
                assert!(!compact.contains(needle.as_str()), "{}: {needle} は commit_config を通すこと", path.display());
            }
            for (at, _) in compact.match_indices(save_call.as_str()) {
                let args: String = compact[at + save_call.len()..].chars().take_while(|&c| c != ')').collect();
                let in_commit = path.ends_with("config.rs") && args == "dir";
                assert!(in_commit || !args.contains("config_dir"), "{}: 設定の保存は commit_config を通すこと", path.display());
            }
        }
        let config = files.iter().find(|(p, _)| p.ends_with("config.rs")).unwrap();
        assert_eq!(config.1.matches("next.save(dir)").count(), 1);
    }

    #[test]
    fn ipc_refuses_restore_blocked_stores_before_changing_them() {
        // 保存禁止のストアを変える IPC は、何かを変える前に拒否する(片方だけ確定させない)。
        // 設定は commit_config が保存の成功後にだけメモリを差し替えるので対象外。
        let source: String = include_str!("lib.rs").chars().filter(|c| !c.is_whitespace()).collect();
        let listeners = &["listeners.json"][..];
        let listener_and_progress = &["listeners.json", "achievement-progress.json"][..];
        let templates = &["templates"][..];
        let sounds = &["sounds", "sounds.json"][..];
        for (command, roots, change) in [
            ("fnreset_listener_donation(", listeners, "state.listeners.reset_donation("),
            ("fnreset_all_listener_donations(", listeners, "state.listeners.reset_all_donations("),
            ("fnupdate_listener(", listeners, "state.listeners.update("),
            ("fnset_listener_voice(", listeners, "state.listeners.set_voice("),
            ("fndelete_listener(", listener_and_progress, "state.listeners.delete("),
            (
                "fncommit_achievements(",
                &["achievements.json", "achievement-progress.json"][..],
                "achievements::ipc::commit_draft(",
            ),
            ("fnwrite_template_file(", templates, ".templates.write("),
            ("fnduplicate_template(", templates, ".templates.duplicate("),
            ("fndelete_template(", templates, ".templates.delete("),
            ("fnimport_template_zip(", templates, "template_import::import_zip("),
            ("fnadd_custom_sound(", sounds, "state.sounds.add("),
            ("fnremove_custom_sound(", sounds, "state.sounds.remove("),
        ] {
            let start = source.find(command).unwrap_or_else(|| panic!("{command}"));
            let body = &source[start..];
            let body = &body[..body.find("#[tauri::command]").unwrap_or(body.len())];
            let change_at = body.find(change).unwrap_or_else(|| panic!("{command}: {change}"));
            for root in roots {
                let guard = format!("ensure_restore_writable(\"{root}\")");
                let guard_at = body.find(&guard).unwrap_or_else(|| panic!("{command}: {guard}"));
                assert!(guard_at < change_at, "{command}: {root} の確認が変更より後");
            }
        }
    }



    #[tokio::test]
    async fn injected_test_flag_survives_broadcast_and_clone_but_not_serde() {
        let msgs = build_injected_test_messages("twitch", "", "", None, None, Some(2)).unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs.iter().all(|m| m.injected_test && m.is_injected_test_comment()));

        let (tx, mut rx) = broadcast::channel::<ChatMessage>(4);
        tx.send(msgs[0].clone()).unwrap();
        let received = rx.recv().await.unwrap();
        assert!(received.injected_test);
        assert!(received.clone().injected_test);

        let json = serde_json::to_value(&received).unwrap();
        assert!(json.get("injectedTest").is_none());
        assert!(json.get("injected_test").is_none());
        let back: ChatMessage = serde_json::from_value(json).unwrap();
        assert!(!back.injected_test);
    }

    #[tokio::test]
    async fn injected_test_message_is_not_recorded_as_listener() {
        let dir = TestDir::new("injected-listener");
        let store = listeners::ListenerStore::load(&dir.0);
        let msgs = build_injected_test_messages("youtube", "a", "b", None, None, None).unwrap();
        let (tx, mut rx) = broadcast::channel::<ChatMessage>(4);
        tx.send(msgs[0].clone()).unwrap();
        let mut msg = rx.recv().await.unwrap();
        store.observe(&mut msg);
        assert!(msg.listener.is_none());
        assert!(store.list(None, 100).is_empty());
    }

    #[test]
    fn build_injected_test_messages_rejects_bad_input() {
        assert!(build_injected_test_messages("bogus", "", "", None, None, None).is_err());
        assert!(build_injected_test_messages("x", "", "", Some("bogus"), None, None).is_err());
    }

    struct NullSink;
    impl tts::TtsSink for NullSink {
        fn audio(&self, _: String) {}
        fn web_speech(&self, _: tts::WebSpeechPayload) {}
        fn cancel(&self) {}
        fn notice(&self, _: &'static str, _: String) {}
        fn queue_state(&self, _: TtsQueueStatePayload) {}
    }

    fn tts_message(id: &str, origin_key: Option<&str>) -> ChatMessage {
        let mut msg: ChatMessage = serde_json::from_value(serde_json::json!({
            "id": id,
            "platform": "twitch",
            "channel": "fixture",
            "author": { "id": "u1", "name": "viewer" },
            "fragments": [],
            "kind": "normal",
            "timestampMs": 1
        }))
        .unwrap();
        msg.route.origin_key = origin_key.map(str::to_owned);
        msg
    }

    #[test]
    fn tts_announce_skips_when_tts_disabled() {
        let mut cfg = AppConfig::default();
        cfg.tts.options.queue.max_pending = 100;
        let config = Mutex::new(cfg);
        let queue = TtsShared::default();
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("waiting", None));
        config.lock().unwrap().tts.backend = TtsBackendKind::None;
        try_enqueue_achievement_tts(&NullSink, &config, &queue, tts_message("unlock", None));
        let snapshot = queue.snapshot();
        assert_eq!(snapshot.depth, 1);
        assert_eq!(snapshot.items[0].id, "waiting");
        config.lock().unwrap().tts.backend = TtsBackendKind::WebSpeech;
        try_enqueue_achievement_tts(&NullSink, &config, &queue, tts_message("unlock", None));
        assert_eq!(queue.snapshot().depth, 2);
    }

    #[test]
    fn disabling_channel_tts_drops_its_queued_comments() {
        let (dir, revision, tx, mut current) = commit_parts();
        for identifier in ["off", "on"] {
            current.channels.push(ChannelConfig {
                platform: ChannelPlatform::Twitch,
                identifier: identifier.into(),
                enabled: true,
                own_stream: false,
                overrides: ChannelOverrides::default(),
            });
        }
        current.tts.options.queue.max_pending = 100;
        let config = Mutex::new(current);
        let queue = TtsShared::default();
        for i in 0..20 {
            try_enqueue_tts_message(&NullSink, &config, &queue, tts_message(&format!("off{i}"), Some("twitch:off")));
        }
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("on", Some("twitch:on")));
        assert_eq!(queue.snapshot().depth, 21);

        {
            let mut cfg = config.lock().unwrap();
            let off = ChannelOverrides {
                tts_enabled: Some(false),
                ..Default::default()
            };
            commit_channel_overrides(dir.path(), &revision, &tx, &mut cfg, "twitch:off", off, 0).unwrap();
            assert!(purge_disabled_channel_tts(&cfg, &queue));
        }
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("late", Some("twitch:off")));

        let mut spoken = Vec::new();
        let mut core = queue.core.lock().unwrap();
        while let (Some(popped), _) = core.pop_fresh(std::time::Instant::now(), None) {
            spoken.push(popped.job.msg.id.clone());
        }
        assert_eq!(spoken, ["on"]);
    }

    #[test]
    fn full_config_save_turning_channel_off_purges_its_queue() {
        let (dir, revision, tx, mut current) = commit_parts();
        current.channels.push(ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "full".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides::default(),
        });
        let config = Mutex::new(current);
        let queue = TtsShared::default();
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("q", Some("twitch:full")));
        assert_eq!(queue.snapshot().depth, 1);

        // update_config と同じく、画面の全体設定 JSON を解析して保存し、同じロック内で捨てる。
        let mut json = serde_json::to_value(&*config.lock().unwrap()).unwrap();
        json["channels"][0]["overrides"] = serde_json::json!({ "ttsEnabled": false });
        let mut next = AppConfig::from_config_update(json).unwrap();
        next.normalize_all();
        let mut cfg = config.lock().unwrap();
        commit_config(dir.path(), &revision, &tx, &mut cfg, next, Some(0)).unwrap();
        assert!(purge_disabled_channel_tts(&cfg, &queue));
        drop(cfg);
        assert_eq!(queue.snapshot().depth, 0);
    }

    #[test]
    fn full_config_save_keeps_queued_comment_when_first_duplicate_allows_tts() {
        let (dir, revision, tx, mut current) = commit_parts();
        for tts_enabled in [true, false] {
            current.channels.push(ChannelConfig {
                platform: ChannelPlatform::Twitch,
                identifier: "a".into(),
                enabled: true,
                own_stream: false,
                overrides: ChannelOverrides {
                    tts_enabled: Some(tts_enabled),
                    ..Default::default()
                },
            });
        }
        let config = Mutex::new(current);
        let queue = TtsShared::default();
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("q", Some("twitch:a")));
        assert_eq!(queue.snapshot().depth, 1);

        let mut cfg = config.lock().unwrap();
        let mut json = serde_json::to_value(&*cfg).unwrap();
        json["goals"]["showInApp"] = serde_json::json!(true);
        let mut next = AppConfig::from_config_update(json).unwrap();
        next.normalize_all();
        commit_config(dir.path(), &revision, &tx, &mut cfg, next, Some(0)).unwrap();
        assert!(cfg.goals.show_in_app);
        assert!(!purge_disabled_channel_tts(&cfg, &queue));
        assert_eq!(queue.snapshot().depth, 1);
    }

    #[test]
    fn tts_queue_state_is_emitted_after_config_lock_is_released() {
        struct LockProbe<'a> {
            config: &'a Mutex<AppConfig>,
            locked: Mutex<Vec<bool>>,
        }
        impl tts::TtsSink for LockProbe<'_> {
            fn audio(&self, _: String) {}
            fn web_speech(&self, _: tts::WebSpeechPayload) {}
            fn cancel(&self) {}
            fn notice(&self, _: &'static str, _: String) {}
            fn queue_state(&self, _: TtsQueueStatePayload) {
                self.locked.lock().unwrap().push(self.config.try_lock().is_ok());
            }
        }
        let mut cfg = AppConfig::default();
        cfg.channels.push(ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "probe".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides::default(),
        });
        let config = Mutex::new(cfg);
        let sink = LockProbe { config: &config, locked: Mutex::new(Vec::new()) };
        let queue = TtsShared::default();
        try_enqueue_tts_message(&sink, &config, &queue, tts_message("a", Some("twitch:probe")));
        let purged = {
            let mut cfg = config.lock().unwrap();
            cfg.channels[0].overrides.tts_enabled = Some(false);
            purge_disabled_channel_tts(&cfg, &queue)
        };
        assert!(purged);
        queue.emit_queue_state(&sink);
        assert_eq!(*sink.locked.lock().unwrap(), [true, true]);
    }

    #[test]
    fn replaces_dedupes_keys_and_replaces_every_row_sharing_a_key() {
        let (dir, revision, tx, mut current) = commit_parts();
        let row = |identifier: &str| ChannelConfig {
            platform: ChannelPlatform::X,
            identifier: identifier.into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides::default(),
        };
        current.channels = vec![row("a"), row("a")];
        let keep = CancellationToken::new();
        let old = CancellationToken::new();
        let active = Mutex::new(HashMap::from([
            ("x:a".to_string(), old.clone()),
            ("x:other".to_string(), keep.clone()),
        ]));
        let replaces = parse_replaces(Some(serde_json::json!(["x:a", "x:a"]))).unwrap();
        assert_eq!(replaces, ["x:a"]);
        add_channel_and_commit(
            &mut current,
            &active,
            row("b"),
            &replaces,
            |cfg, next| commit_config(dir.path(), &revision, &tx, cfg, next, None),
            |_, _| CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(current.channels, vec![row("b")]);
        assert!(old.is_cancelled());
        assert!(!keep.is_cancelled());
        let mut keys: Vec<_> = active.lock().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["x:b", "x:other"]);
    }

    #[test]
    fn set_channel_overrides_scalar_args_hide_values_in_errors() {
        for value in [serde_json::json!("SECRET_REV_MARK"), serde_json::json!(-1), serde_json::json!(1.5)] {
            let err = from_ipc_value::<u64>(value, "基準 revision").unwrap_err();
            assert_eq!(err, "基準 revisionの形式が不正です (項目 .)");
        }
        let err = from_ipc_value::<String>(serde_json::json!(123456789), "枠のキー").unwrap_err();
        assert!(!err.contains("123456789"), "{err}");
    }

    #[test]
    fn enqueued_message_carries_channel_voice() {
        let voice = config::ChannelTtsVoice {
            bouyomi_voice: Some(3),
            ..Default::default()
        };
        let mut cfg = AppConfig::default();
        cfg.channels.push(ChannelConfig {
            platform: ChannelPlatform::Twitch,
            identifier: "voiced".into(),
            enabled: true,
            own_stream: false,
            overrides: ChannelOverrides {
                tts_voice: Some(voice.clone()),
                ..Default::default()
            },
        });
        let config = Mutex::new(cfg);
        let queue = TtsShared::default();
        try_enqueue_tts_message(&NullSink, &config, &queue, tts_message("m", Some("twitch:voiced")));
        let mut core = queue.core.lock().unwrap();
        let (popped, _) = core.pop_fresh(std::time::Instant::now(), None);
        assert_eq!(popped.unwrap().job.msg.route.channel_tts_voice, Some(voice));
    }

    #[test]
    fn ipc_inputs_accept_single_or_many_replaces_and_hide_values_in_errors() {
        assert_eq!(parse_replaces(None).unwrap(), Vec::<String>::new());
        assert_eq!(parse_replaces(Some(serde_json::Value::Null)).unwrap(), Vec::<String>::new());
        assert_eq!(parse_replaces(Some(serde_json::json!("x:a"))).unwrap(), ["x:a"]);
        assert_eq!(parse_replaces(Some(serde_json::json!(["x:a", "x:b"]))).unwrap(), ["x:a", "x:b"]);
        let err = parse_replaces(Some(serde_json::json!([1, "SECRET_KEY_MARK"]))).unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");

        let err = from_ipc_value::<ChannelConfig>(
            serde_json::json!({ "platform": "SECRET_PLATFORM_MARK", "identifier": "SECRET_ID_MARK" }),
            "チャンネル",
        )
        .unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.contains("platform"), "{err}");
        let err = from_ipc_value::<ChannelOverrides>(
            serde_json::json!({ "ttsVoice": { "webSpeechVoice": ["SECRET_VOICE_MARK"] } }),
            "上書き設定",
        )
        .unwrap_err();
        assert!(!err.contains("SECRET"), "{err}");
        assert!(err.starts_with("上書き設定の形式が不正です"), "{err}");
    }

    #[test]
    fn add_channel_replaces_every_old_row_in_one_commit() {
        let (dir, revision, tx, mut current) = commit_parts();
        let overrides = ChannelOverrides {
            tts_enabled: Some(false),
            ..Default::default()
        };
        let row = |identifier: &str, overrides: ChannelOverrides| ChannelConfig {
            platform: ChannelPlatform::Niconico,
            identifier: identifier.into(),
            enabled: true,
            own_stream: false,
            overrides,
        };
        current.channels = vec![row("lv1", overrides.clone()), row("lv2", ChannelOverrides::default())];
        let keys: Vec<String> = current.channels.iter().map(AppState::channel_key).collect();
        let tokens: Vec<_> = keys.iter().map(|_| CancellationToken::new()).collect();
        let active = Mutex::new(keys.iter().cloned().zip(tokens.iter().cloned()).collect::<HashMap<_, _>>());
        let mut commits = 0;

        let saved = add_channel_and_commit(
            &mut current,
            &active,
            row("lv3", ChannelOverrides::default()),
            &keys,
            |cfg, next| {
                commits += 1;
                commit_config(dir.path(), &revision, &tx, cfg, next, None)
            },
            |_, _| CancellationToken::new(),
        )
        .unwrap();
        assert_eq!(commits, 1);
        assert_eq!(saved.revision, Some(1));
        assert_eq!(current.channels, vec![row("lv3", overrides)]);
        assert_eq!(AppConfig::load(dir.path()).unwrap().channels, current.channels);
        assert!(tokens.iter().all(CancellationToken::is_cancelled));
        let active = active.lock().unwrap();
        assert_eq!(active.keys().collect::<Vec<_>>(), ["niconico:lv3"]);
    }
}

#[cfg(test)]
mod log_filter_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    pub(crate) struct Sink(Arc<Mutex<Vec<u8>>>);

    impl Sink {
        pub(crate) fn output(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
        type Writer = Sink;
        fn make_writer(&'a self) -> Sink {
            self.clone()
        }
    }

    fn logged(env: Option<&str>) -> String {
        let sink = Sink::default();
        let subscriber = log_subscriber(env, sink.clone());
        tracing::subscriber::with_default(subscriber, || {
            tracing::trace!(target: "tungstenite::handshake::client", "GET /events?token=SECRET");
            tracing::trace!(target: "tokio_tungstenite", "GET /events?token=SECRET");
            tracing::debug!(target: "tungstenite::handshake::client", "GET /events?token=SECRET");
            tracing::debug!(target: "tokio_tungstenite::handshake", "GET /events?token=SECRET");
            tracing::info!(target: "tungstenite::handshake::client", "ws-client-info");
            tracing::info!(target: "tokio_tungstenite", "ws-info");
            tracing::info!(target: "fast_comment_lib", "kept");
            tracing::debug!(target: "fast_comment_lib", "debug-line");
        });
        sink.output()
    }

    #[test]
    fn ws_handshake_url_never_reaches_the_log_even_with_trace_env() {
        for env in [
            "tungstenite=trace",
            "tungstenite::handshake::client=trace",
            "tokio_tungstenite::handshake=debug",
            "trace",
            "tokio_tungstenite=trace,tungstenite=trace",
            "debug",
        ] {
            assert!(!logged(Some(env)).contains("SECRET"), "RUST_LOG={env}");
        }
        let specific = logged(Some("tungstenite::handshake::client=trace"));
        assert!(specific.contains("ws-client-info"));
        assert!(!specific.contains("ws-info"));
    }

    #[test]
    fn ws_filter_does_not_raise_rust_log_levels() {
        assert!(logged(Some("off")).is_empty());
        assert!(logged(Some("warn")).is_empty());
        let info = logged(Some("info"));
        assert!(info.contains("ws-client-info"));
        assert!(info.contains("ws-info"));
    }

    #[tokio::test]
    async fn tungstenite_client_handshake_keeps_token_out_of_bridged_logs() {
        use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
        use tracing_subscriber::util::SubscriberInitExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = async {
            let (socket, _) = listener.accept().await.unwrap();
            tokio_tungstenite::accept_hdr_async(socket, |request: &Request, response: Response| {
                assert_eq!(request.method(), "GET");
                assert_eq!(request.uri(), "/events?token=SECRET");
                Ok(response)
            })
            .await
            .unwrap();
        };
        let sink = Sink::default();
        let _guard = log_subscriber(Some("tungstenite::handshake::client=trace"), sink.clone())
            .set_default();
        let url = format!("ws://127.0.0.1:{port}/events?token=SECRET");
        let client = async { tokio_tungstenite::connect_async(&url).await.unwrap() };
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(client, server);
        })
        .await
        .unwrap();
        assert!(!sink.output().contains("SECRET"));
    }

    #[test]
    fn default_and_other_targets_keep_their_levels() {
        let default = logged(None);
        assert!(default.contains("kept"));
        assert!(!default.contains("debug-line"));
        assert!(logged(Some("debug")).contains("debug-line"));
        // 不正な値は既定 info に戻る。
        assert!(logged(Some("=[")).contains("kept"));
    }
}
