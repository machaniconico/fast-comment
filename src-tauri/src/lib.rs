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

pub mod bus;
pub mod config;
pub mod model;
pub mod moderation;
pub mod sources;
pub mod stats;
pub mod tts;
mod update;

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::net::TcpStream;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tokio_util::sync::CancellationToken;

use crate::bus::{is_valid_template_name, Bus};
use crate::config::{AppConfig, ChannelConfig, ChannelPlatform, TtsBackendKind};
use crate::model::{
    Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Participant, Platform, Roles,
};
use crate::moderation::{Moderator, Verdict};
use crate::sources::SourceManager;
use crate::stats::{spawn_stats_aggregator, StatsSnapshot, TimerSnapshot, YoutubeMetadataUpdate};
use crate::tts::{
    bouyomi, emit_tts_queue_state, tts_queue_state, TtsBackend, TtsDispatcher, TtsNoticePayload,
    TtsQueueItem, TtsQueueStatePayload,
};
use crate::update::{check_for_update, open_url};

const EDITABLE_TEMPLATE_FILES: &[&str] = &["style.css", "index.html", "app.js"];
const MAX_TEMPLATE_FILE_BYTES: usize = 1024 * 1024;

/// アプリ全体で共有する実行時状態。
pub struct AppState {
    /// 現在の設定。
    config: Mutex<AppConfig>,
    /// stats 集約などへ設定変更を伝える watch 送信端。
    config_tx: watch::Sender<AppConfig>,
    /// Timer overlay へ基準スナップショットを配る watch 送信端。
    timer_tx: watch::Sender<TimerSnapshot>,
    /// 設定の保存先ディレクトリ(app config dir)。
    config_dir: PathBuf,
    /// Source → パイプラインへ流す内部 broadcast 送信端。
    source_tx: broadcast::Sender<ChatMessage>,
    /// UI/OBS へ流す下流 Bus。
    bus: Bus,
    /// OBS テンプレートの配信元ディレクトリ。
    templates_dir: PathBuf,
    /// アプリ全体の停止トークン。
    app_cancel: CancellationToken,
    /// OBS サーバの現在の待受状態。
    obs_server: Mutex<ObsServerControl>,
    /// チャンネル識別キー → そのチャンネルの停止トークン。
    channels: Mutex<HashMap<String, CancellationToken>>,
    /// モデレータ(手動非表示状態も保持)。
    moderator: Mutex<Moderator>,
    /// 長命 TTS ワーカーへの bounded 送信端(溢れたら drop)。
    tts_tx: mpsc::Sender<ChatMessage>,
    /// UI 表示用の TTS 待ちキュースナップショット。
    tts_queue: Mutex<VecDeque<TtsQueueItem>>,
    /// TTS 一時停止中はワーカーが受信したメッセージを読み上げずに破棄する。
    tts_paused: Arc<AtomicBool>,
    /// TTS キュー全消し要求。rx はワーカー所有のためワーカー内で drain する。
    tts_clear: Arc<AtomicBool>,
    /// 空キュー時でも TTS 全消し要求をワーカーへ通知して clear flag を reset する。
    tts_clear_notify: Arc<tokio::sync::Notify>,
    /// YouTube メタデータ poller → stats 集約への bounded 送信端。
    metadata_tx: mpsc::Sender<YoutubeMetadataUpdate>,
    /// 参加型配信の参加者一覧。
    participants: Mutex<Vec<Participant>>,
}

/// OBS サーバの再起動に必要な実行時ハンドル。
struct ObsServerControl {
    port: u16,
    cancel: CancellationToken,
}

impl AppState {
    /// チャンネルの一意キー(プラットフォーム + 識別子)。
    fn channel_key(ch: &ChannelConfig) -> String {
        let p = match ch.platform {
            config::ChannelPlatform::Twitch => "twitch",
            config::ChannelPlatform::Youtube => "youtube",
            config::ChannelPlatform::Niconico => "niconico",
        };
        format!("{p}:{}", ch.identifier)
    }
}

fn emit_tts_notice(app: &AppHandle, level: &'static str, message: String) {
    let payload = TtsNoticePayload { level, message };
    if let Err(e) = app.emit("tts-notice", payload) {
        tracing::warn!("tts-notice emit 失敗: {e}");
    }
}

fn current_tts_queue_state(state: &AppState) -> TtsQueueStatePayload {
    let paused = state.tts_paused.load(Ordering::Relaxed);
    let queue = state.tts_queue.lock().unwrap();
    tts_queue_state(paused, &queue)
}

fn emit_current_tts_queue_state(app: &AppHandle, state: &AppState) {
    emit_tts_queue_state(app, current_tts_queue_state(state));
}

fn remove_tts_queue_item(state: &AppState, id: &str) -> bool {
    let mut queue = state.tts_queue.lock().unwrap();
    if let Some(index) = queue.iter().position(|item| item.id == id) {
        queue.remove(index);
        true
    } else {
        false
    }
}

fn try_enqueue_tts_message(
    app: &AppHandle,
    state: &AppState,
    msg: ChatMessage,
) -> Result<(), mpsc::error::TrySendError<ChatMessage>> {
    let item = TtsQueueItem::from_message(&msg);
    {
        let mut queue = state.tts_queue.lock().unwrap();
        queue.push_back(item.clone());
    }

    match state.tts_tx.try_send(msg) {
        Ok(()) => {
            emit_current_tts_queue_state(app, state);
            Ok(())
        }
        Err(e) => {
            remove_tts_queue_item(state, &item.id);
            Err(e)
        }
    }
}

fn clear_tts_queue_snapshot(app: &AppHandle, state: &AppState) {
    state.tts_queue.lock().unwrap().clear();
    emit_current_tts_queue_state(app, state);
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

// ============ Tauri コマンド ============

/// 現在の設定を取得する。
#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().unwrap().clone())
}

/// TTS 読み上げを一時停止/再開する。paused 中に届いたメッセージは保持せず破棄する。
#[tauri::command]
fn set_tts_paused(app: AppHandle, state: State<'_, AppState>, paused: bool) {
    state.tts_paused.store(paused, Ordering::Relaxed);
    emit_current_tts_queue_state(&app, &state);
}

/// 現在の TTS 待ちキュー状態を取得する。
#[tauri::command]
fn get_tts_queue_state(state: State<'_, AppState>) -> TtsQueueStatePayload {
    current_tts_queue_state(&state)
}

/// 未送出の TTS キューを全消しし、WebSpeech の現在発話も即時停止させる。
#[tauri::command]
fn clear_tts_queue(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.tts_clear.store(true, Ordering::Relaxed);
    state.tts_clear_notify.notify_one();
    clear_tts_queue_snapshot(&app, &state);
    app.emit("tts-cancel", ())
        .map_err(|e| format!("TTSキャンセルイベント送信失敗: {e}"))
}

/// 現在の WebSpeech 発話を停止する。外部バックエンドの送信済み発話は停止できない。
#[tauri::command]
fn skip_current_tts(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    emit_current_tts_queue_state(&app, &state);
    app.emit("tts-cancel", ())
        .map_err(|e| format!("TTSキャンセルイベント送信失敗: {e}"))
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
        id: format!("tts-text-{epoch_millis}"),
        platform: Platform::Youtube,
        channel: "system".to_string(),
        author: Author {
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
    };

    match try_enqueue_tts_message(&app, &state, msg) {
        Ok(()) => Ok(()),
        Err(mpsc::error::TrySendError::Full(_)) => {
            tracing::debug!("TTS キュー満杯につき任意テキスト読み上げを drop");
            Ok(())
        }
        Err(mpsc::error::TrySendError::Closed(_)) => Err("TTS キューが閉じています".to_string()),
    }
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
                    let launch_note =
                        if matches!(launch_outcome, bouyomi::LaunchOutcome::Launched) {
                            " (自動起動しました)"
                        } else {
                            ""
                        };
                    Ok(format!("棒読みちゃんへ送出しました ({host}:{port}){launch_note}"))
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
        TtsBackendKind::Voicevox => {
            Ok("現在のバックエンドは VOICEVOX です。棒読みちゃんを選択するとテストできます".to_string())
        }
        TtsBackendKind::WebSpeech => {
            Ok("現在のバックエンドは Web Speech です。棒読みちゃんを選択するとテストできます".to_string())
        }
        TtsBackendKind::None => {
            Ok("現在、読み上げはOFFです。棒読みちゃんを選択するとテストできます".to_string())
        }
    }
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
// TODO(YouTube投稿): liveChatId 解決 → liveChatMessages.insert → quota 管理と
// Google OAuth 設定を次フェーズで実装する。
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
        "youtube" => Err(
            "YouTube投稿は未対応です(YouTube Data API v3 + Google OAuth の設定が必要・次フェーズ)"
                .into(),
        ),
        other => Err(format!("不明なplatformです: {other}")),
    }
}

/// 設定全体を更新して保存する。moderation/tts/obs などの実行時状態も反映する。
///
/// チャンネル一覧の差分適用(追加/削除)も行う。
#[tauri::command]
async fn update_config(
    app: AppHandle,
    state: State<'_, AppState>,
    new_config: AppConfig,
) -> Result<(), String> {
    // 保存。
    new_config
        .save(&state.config_dir)
        .map_err(|e| e.to_string())?;

    // moderation 反映。
    state
        .moderator
        .lock()
        .unwrap()
        .update_config(&new_config.moderation);

    // OBS ポート反映。ポートが変わった場合は旧サーバを停止し、新ポートで再 bind する。
    restart_obs_server_if_needed(&state, new_config.obs.port)?;

    // 差分計算用にチャンネル一覧を退避しておく。
    let desired_channels = new_config.channels.clone();

    // 設定を先に確定する。これにより、続く差分適用で起動される新規 Source が
    // 最新の youtube_overrides を読む(#8 / SPEC §4.2「再ビルド無しで overrides 反映」)。
    let committed_config = new_config.clone();
    *state.config.lock().unwrap() = new_config;
    let _ = state.config_tx.send(committed_config.clone());

    // チャンネル差分適用(コミット済み設定を参照する)。
    apply_channel_diff(&app, &state, &desired_channels);

    // 棒読みちゃん自動起動: 設定保存時にも(起動時だけでなく)未起動なら立ち上げる。
    // パスを設定した直後に効くようにする。ensure_launched 側で TCP 接続を確認し、
    // 既起動なら何もしないため、保存のたびに呼んでも無害。
    if committed_config.tts.backend == config::TtsBackendKind::Bouyomi {
        let path = committed_config.tts.options.bouyomi_path.trim().to_string();
        let host = committed_config.tts.options.bouyomi_host.clone();
        let port = committed_config.tts.options.bouyomi_port;
        let elevated = committed_config.tts.options.bouyomi_launch_elevated;
        ensure_bouyomi_launched_and_emit_notice(app.clone(), path, host, port, elevated).await;
    }

    Ok(())
}

/// チャンネルを1件追加して起動する。
#[tauri::command]
fn add_channel(
    app: AppHandle,
    state: State<'_, AppState>,
    channel: ChannelConfig,
) -> Result<(), String> {
    let next_config = {
        let mut cfg = state.config.lock().unwrap();
        // 既存の同一キーがあれば重複追加しない。
        let key = AppState::channel_key(&channel);
        if cfg
            .channels
            .iter()
            .any(|c| AppState::channel_key(c) == key)
        {
            return Err(format!("チャンネルは既に存在します: {key}"));
        }
        cfg.channels.push(channel.clone());
        cfg.save(&state.config_dir).map_err(|e| e.to_string())?;
        cfg.clone()
    };
    let _ = state.config_tx.send(next_config);

    spawn_one_channel(&app, &state, &channel);
    Ok(())
}

/// チャンネルを1件削除して停止する。`key` は `twitch:name` / `youtube:videoId` / `niconico:lvId`。
#[tauri::command]
fn remove_channel(state: State<'_, AppState>, key: String) -> Result<(), String> {
    // 起動中タスクを停止。
    if let Some(token) = state.channels.lock().unwrap().remove(&key) {
        token.cancel();
    }
    // 設定から除去して保存。
    let next_config = {
        let mut cfg = state.config.lock().unwrap();
        cfg.channels
            .retain(|c| AppState::channel_key(c) != key);
        cfg.save(&state.config_dir).map_err(|e| e.to_string())?;
        cfg.clone()
    };
    let _ = state.config_tx.send(next_config);
    Ok(())
}

/// 個別コメントをローカル非表示にする(以後の同一 ID 送出を抑止)。
#[tauri::command]
fn hide_message(state: State<'_, AppState>, id: String) -> Result<(), String> {
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
    let template = template.unwrap_or_else(|| "default".to_string());
    if !is_valid_template_name(&template) {
        return Err("テンプレート名は半角英数字、'-'、'_' のみ使用できます".to_string());
    }
    let port = state.obs_server.lock().unwrap().port;
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
    let timer_config = {
        let cfg = state.config.lock().unwrap();
        cfg.timer.clone()
    };
    let now_ms = timer_now_ms();
    let timer_rx = state.timer_tx.subscribe();
    let mut next = timer_rx.borrow().clone();

    match action.as_str() {
        "start" => {
            let dur = duration_sec.unwrap_or(timer_config.default_duration_sec);
            next = TimerSnapshot {
                state: "running".to_string(),
                mode: normalize_timer_mode(&timer_config.mode),
                duration_sec: dur,
                base_elapsed_sec: 0,
                running_since_ms: now_ms,
                updated_at: now_ms,
            };
        }
        "pause" => {
            if next.state == "running" {
                let elapsed_sec = now_ms.saturating_sub(next.running_since_ms) / 1000;
                let elapsed_sec = u32::try_from(elapsed_sec).unwrap_or(u32::MAX);
                next.base_elapsed_sec = next.base_elapsed_sec.saturating_add(elapsed_sec);
            }
            next.state = "paused".to_string();
            next.running_since_ms = 0;
            next.updated_at = now_ms;
        }
        "resume" => {
            next.state = "running".to_string();
            next.running_since_ms = now_ms;
            next.updated_at = now_ms;
        }
        "reset" => {
            next.state = "idle".to_string();
            next.base_elapsed_sec = 0;
            next.running_since_ms = 0;
            next.updated_at = now_ms;
        }
        other => {
            return Err(format!(
                "不正な timer action です: {other} (start/pause/resume/reset)"
            ));
        }
    }

    let _ = state.timer_tx.send_replace(next);
    Ok(())
}

/// OBS Timer overlay 用 URL を返す。
#[tauri::command]
fn get_obs_timer_url(state: State<'_, AppState>) -> Result<String, String> {
    let port = state.obs_server.lock().unwrap().port;
    Ok(format!(
        "http://127.0.0.1:{port}/?template=timer&ws=ws://127.0.0.1:{port}/timer"
    ))
}

fn normalize_timer_mode(mode: &str) -> String {
    if mode == "elapsed" {
        "elapsed".to_string()
    } else {
        "countdown".to_string()
    }
}

fn timer_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64
}

/// OBS テンプレートディレクトリ一覧を返す。
#[tauri::command]
fn list_templates(state: State<'_, AppState>) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    let entries = fs::read_dir(&state.templates_dir).map_err(|e| {
        format!(
            "テンプレートディレクトリ読み込み失敗 {}: {e}",
            state.templates_dir.display()
        )
    })?;

    for entry in entries {
        let entry = entry.map_err(|e| format!("テンプレート一覧取得失敗: {e}"))?;
        let file_type = entry
            .file_type()
            .map_err(|e| format!("テンプレート種別取得失敗: {e}"))?;
        if !file_type.is_dir() {
            continue;
        }
        let Some(name) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if is_valid_template_name(&name) {
            names.push(name);
        }
    }
    names.sort();
    Ok(names)
}

/// OBS テンプレート内の編集可能ファイルを読み込む。
#[tauri::command]
fn read_template_file(
    state: State<'_, AppState>,
    name: String,
    file: String,
) -> Result<String, String> {
    let path = template_file_path(&state, &name, &file)?;
    fs::read_to_string(&path)
        .map_err(|e| format!("テンプレートファイル読み込み失敗 {}: {e}", path.display()))
}

/// OBS テンプレート内の編集可能ファイルを書き込む。
#[tauri::command]
fn write_template_file(
    state: State<'_, AppState>,
    name: String,
    file: String,
    contents: String,
) -> Result<(), String> {
    if contents.len() > MAX_TEMPLATE_FILE_BYTES {
        return Err("テンプレートファイルは1MiB以下にしてください".to_string());
    }

    let path = template_file_path(&state, &name, &file)?;
    fs::write(&path, contents.as_bytes())
        .map_err(|e| format!("テンプレートファイル書き込み失敗 {}: {e}", path.display()))
}

fn validate_template_file_name(file: &str) -> Result<&str, String> {
    EDITABLE_TEMPLATE_FILES
        .iter()
        .copied()
        .find(|allowed| *allowed == file)
        .ok_or_else(|| "編集できるファイルは style.css / index.html / app.js のみです".to_string())
}

fn template_file_path(state: &AppState, name: &str, file: &str) -> Result<PathBuf, String> {
    if !is_valid_template_name(name) {
        return Err("テンプレート名は半角英数字、'-'、'_' のみ使用できます".to_string());
    }
    let file = validate_template_file_name(file)?;
    let dir = state.templates_dir.join(name);
    if !dir.is_dir() {
        return Err(format!("テンプレートディレクトリが見つかりません: {name}"));
    }
    Ok(dir.join(file))
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
    let platform = match platform.as_str() {
        "twitch" => Platform::Twitch,
        "youtube" => Platform::Youtube,
        "niconico" => Platform::Niconico,
        other => return Err(format!("不正な platform です: {other}")),
    };
    let kind = match kind.as_deref().unwrap_or("normal") {
        "normal" => MessageKind::Normal,
        "superChat" => MessageKind::SuperChat,
        "membership" => MessageKind::Membership,
        "bits" => MessageKind::Bits,
        other => return Err(format!("不正な kind です: {other}")),
    };
    let count = count.unwrap_or(1).clamp(1, 20);
    let epoch_millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| format!("現在時刻取得失敗: {e}"))?
        .as_millis();
    let timestamp_ms = i64::try_from(epoch_millis).unwrap_or(i64::MAX);
    let author_name = name.trim();
    let author_name = if author_name.is_empty() {
        "テストユーザー"
    } else {
        author_name
    };
    let body = text.trim();
    let body = if body.is_empty() {
        "テストコメントです"
    } else {
        body
    };

    for i in 0..count {
        let value = amount.filter(|_| matches!(kind, MessageKind::SuperChat | MessageKind::Bits));
        let msg = ChatMessage {
            id: format!("test-{epoch_millis}-{i}"),
            platform,
            channel: "test".to_string(),
            author: Author {
                id: format!("test{i}"),
                name: author_name.to_string(),
                display_color: None,
                badges: Vec::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::text(body.to_string())],
            kind,
            amount: value.map(|value| Amount {
                value,
                currency: "JPY".to_string(),
                raw_text: format!("¥{value}"),
            }),
            timestamp_ms: timestamp_ms.saturating_add(i64::from(i)),
            raw: None,
            skip_tts: false,
        };
        let _ = state.source_tx.send(msg);
    }

    Ok(())
}

// ============ 内部ヘルパ ============

fn participant_platform(platform: Platform) -> &'static str {
    match platform {
        Platform::Twitch => "twitch",
        Platform::Youtube => "youtube",
        Platform::Niconico => "niconico",
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

/// OBS サーバの待受ポートが変わったら、旧サーバを止めて新ポートで起動する。
fn restart_obs_server_if_needed(state: &AppState, new_port: u16) -> Result<(), String> {
    let mut server = state.obs_server.lock().unwrap();
    if server.port == new_port {
        return Ok(());
    }

    tracing::info!("OBS サーバを再起動: {} -> {}", server.port, new_port);

    let cancel = state.app_cancel.child_token();
    let _obs_task = state.bus.spawn_obs_server_on_port(
        state.templates_dir.clone(),
        new_port,
        cancel.clone(),
    )?;

    server.cancel.cancel();
    *server = ObsServerControl {
        port: new_port,
        cancel,
    };
    Ok(())
}

/// 新しい設定のチャンネル一覧と現在の起動状態を突き合わせ、追加/削除を適用する。
fn apply_channel_diff(app: &AppHandle, state: &AppState, desired: &[ChannelConfig]) {
    let desired_keys: std::collections::HashSet<String> = desired
        .iter()
        .filter(|c| c.enabled)
        .map(AppState::channel_key)
        .collect();

    // 不要になったチャンネルを停止。
    {
        let mut chans = state.channels.lock().unwrap();
        let to_remove: Vec<String> = chans
            .keys()
            .filter(|k| !desired_keys.contains(*k))
            .cloned()
            .collect();
        for k in to_remove {
            if let Some(token) = chans.remove(&k) {
                token.cancel();
            }
        }
    }

    // 新規チャンネルを起動。
    for ch in desired.iter().filter(|c| c.enabled) {
        let key = AppState::channel_key(ch);
        let already = state.channels.lock().unwrap().contains_key(&key);
        if !already {
            spawn_one_channel(app, state, ch);
        }
    }
}

/// 1チャンネル分の Source を起動し、停止トークンを state に登録する。
fn spawn_one_channel(_app: &AppHandle, state: &AppState, ch: &ChannelConfig) {
    if !ch.enabled {
        return;
    }
    let key = AppState::channel_key(ch);
    let overrides = state.config.lock().unwrap().youtube_overrides.clone();
    let token = if ch.platform == ChannelPlatform::Youtube
        && sources::youtube::is_channel_identifier(&ch.identifier)
    {
        let token = CancellationToken::new();
        sources::youtube::live_resolve::spawn_live_resolve_poller(
            ch.identifier.clone(),
            overrides.clone(),
            state.source_tx.clone(),
            state.metadata_tx.clone(),
            token.clone(),
        );
        token
    } else {
        let manager = SourceManager::new(state.source_tx.clone(), overrides.clone());
        manager.spawn_channel(ch)
    };
    let app_cancel = state.app_cancel.clone();
    let token_for_app_cancel = token.clone();
    tauri::async_runtime::spawn(async move {
        app_cancel.cancelled().await;
        token_for_app_cancel.cancel();
    });
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
    if ch.platform == ChannelPlatform::Twitch {
        let oauth = state.config.lock().unwrap().credentials.twitch_oauth.clone();
        if !oauth.trim().is_empty() {
            sources::twitch_helix::spawn_twitch_viewer_poller(
                ch.identifier.clone(),
                oauth,
                state.metadata_tx.clone(),
                token.clone(),
            );
        }
    }
    let mut map = state.channels.lock().unwrap();
    // 同一キーの旧タスクが残っていれば確実にキャンセルしてからリーク無く差し替える(#18)。
    if let Some(old) = map.insert(key, token) {
        old.cancel();
    }
}

/// moderation + TTS パイプラインを起動する。
///
/// source_tx を購読し、各メッセージを判定。
/// - Hide   : 破棄
/// - Highlight: ハイライトバッジを付けて bus へ流す + TTS
/// - Show   : そのまま bus へ流す + TTS
fn spawn_pipeline(app: AppHandle, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        let mut rx = state.source_tx.subscribe();
        let bus = state.bus.clone();

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
                    let verdict = state.moderator.lock().unwrap().judge(&msg);
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

                    // 読み上げは単一の長命ワーカーへ bounded channel で渡す。
                    // バースト時は try_send が Full を返すので drop し、UI/OBS 配信は止めない
                    // (背圧/有界・SPEC 設計原則2)。
                    if !msg.skip_tts {
                        if let Err(mpsc::error::TrySendError::Full(_)) =
                            try_enqueue_tts_message(&app, &state, msg.clone())
                        {
                            tracing::debug!("TTS キュー満杯につき読み上げを drop");
                        }
                    }

                    // 下流(UI/OBS)へ。
                    bus.publish(msg);
                }
            }
        }
        tracing::info!("moderation/TTS パイプライン終了");
    });
}

/// 単一の長命 TTS ワーカーを起動する。
///
/// bounded mpsc を逐次に消費し、1つの `TtsDispatcher` を再利用する。
/// 設定は都度スナップショットを取り `update_config` で反映する(バックエンド再構築は
/// 各 speak 内で行うが、毎メッセージのタスク spawn / 再プローブは行わない)。
/// `speak_message` は内部で speak→Err 時に Web Speech フォールバックする(#15 と整合)。
fn spawn_tts_worker(app: AppHandle, mut rx: mpsc::Receiver<ChatMessage>, cancel: CancellationToken) {
    tauri::async_runtime::spawn(async move {
        let initial_cfg = {
            let state = app.state::<AppState>();
            // MutexGuard 一時値をブロック末尾まで生かさず束縛して即 drop させる
            // (末尾式のままだと guard が state より後に落ちて E0597 になる)。
            let cfg = state.config.lock().unwrap().tts.clone();
            cfg
        };
        let mut dispatcher = TtsDispatcher::new(initial_cfg, app.clone());

        loop {
            let notify = {
                let state = app.state::<AppState>();
                state.tts_clear_notify.clone()
            };
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = notify.notified() => {
                    while rx.try_recv().is_ok() {}
                    let state = app.state::<AppState>();
                    state.tts_clear.store(false, Ordering::Relaxed);
                    clear_tts_queue_snapshot(&app, &state);
                    continue;
                }
                recv = rx.recv() => {
                    let msg = match recv {
                        Some(m) => m,
                        None => break,
                    };
                    {
                        let state = app.state::<AppState>();
                        if remove_tts_queue_item(&state, &msg.id) {
                            emit_current_tts_queue_state(&app, &state);
                        }
                    }
                    {
                        let state = app.state::<AppState>();
                        if state.tts_clear.swap(false, Ordering::Relaxed) {
                            while rx.try_recv().is_ok() {}
                            clear_tts_queue_snapshot(&app, &state);
                            tracing::debug!("TTS キュー全消し要求につき読み上げを drop");
                            continue;
                        }
                        if state.tts_paused.load(Ordering::Relaxed) {
                            tracing::debug!("TTS 一時停止中につき読み上げを drop");
                            continue;
                        }
                    }
                    // 最新設定を反映してから読み上げる。
                    let cfg = {
                        let state = app.state::<AppState>();
                        // 同上: MutexGuard 一時値を束縛して即 drop(E0597 回避)。
                        let snapshot = state.config.lock().unwrap().tts.clone();
                        snapshot
                    };
                    dispatcher.update_config(cfg);
                    dispatcher.speak_message(&msg).await;
                }
            }
        }
        tracing::info!("TTS ワーカー終了");
    });
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
        cwd.join("templates"),         // cwd=リポジトリルートの場合
        cwd.join("..").join("templates"), // cwd=src-tauri/ の場合(dev)
    ] {
        if candidate.exists() {
            return candidate;
        }
    }
    // 最終手段(存在しなくても ServeDir は空配信になるだけで panic しない)。
    cwd.join("templates")
}

/// Tauri アプリを構築して実行する(desktop only)。
pub fn run() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .setup(|app| {
            let handle = app.handle().clone();

            // 設定ディレクトリ。
            let config_dir = handle
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));

            // 設定ロード(失敗時は既定値で続行)。
            let config = AppConfig::load(&config_dir).unwrap_or_else(|e| {
                tracing::error!("設定ロード失敗・既定値で続行: {e}");
                AppConfig::default()
            });

            let obs_port = config.obs.port;

            // 内部 source broadcast(Source → パイプライン)。
            let (source_tx, _source_rx) = broadcast::channel::<ChatMessage>(4096);

            // 長命 TTS ワーカー用 bounded channel(溢れたら drop)。
            let (tts_tx, tts_rx) = mpsc::channel::<ChatMessage>(64);

            // Goals stats 用 channel。
            let (stats_tx, _stats_rx) = watch::channel::<StatsSnapshot>(StatsSnapshot::default());
            let (timer_tx, _timer_rx) = watch::channel::<TimerSnapshot>(TimerSnapshot::default());
            let (config_tx, config_rx) = watch::channel::<AppConfig>(config.clone());
            let (metadata_tx, metadata_rx) = mpsc::channel::<YoutubeMetadataUpdate>(64);

            // 下流 Bus(パイプライン → UI/OBS)。
            let bus = Bus::new(obs_port, stats_tx.clone(), timer_tx.clone());

            // モデレータ。
            let moderator = Moderator::new(&config.moderation);

            // アプリ全体の停止トークン(将来 on_window_event 等から cancel 可能)。
            let app_cancel = CancellationToken::new();
            let obs_cancel = app_cancel.child_token();
            let templates_dir = resolve_templates_dir(&handle);

            let state = AppState {
                config: Mutex::new(config.clone()),
                config_tx,
                timer_tx,
                config_dir,
                source_tx: source_tx.clone(),
                bus: bus.clone(),
                templates_dir: templates_dir.clone(),
                app_cancel: app_cancel.clone(),
                obs_server: Mutex::new(ObsServerControl {
                    port: obs_port,
                    cancel: obs_cancel.clone(),
                }),
                channels: Mutex::new(HashMap::new()),
                moderator: Mutex::new(moderator),
                tts_tx,
                tts_queue: Mutex::new(VecDeque::new()),
                tts_paused: Arc::new(AtomicBool::new(false)),
                tts_clear: Arc::new(AtomicBool::new(false)),
                tts_clear_notify: Arc::new(Notify::new()),
                metadata_tx,
                participants: Mutex::new(Vec::new()),
            };
            app.manage(state);

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
            if let Err(e) = bus.spawn_obs_server(templates_dir, obs_cancel) {
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
            spawn_pipeline(handle.clone(), app_cancel.clone());

            // 単一の長命 TTS ワーカー起動。
            spawn_tts_worker(handle.clone(), tts_rx, app_cancel.clone());

            // 設定済みチャンネルを起動。
            {
                let st = handle.state::<AppState>();
                for ch in config.channels.iter().filter(|c| c.enabled) {
                    spawn_one_channel(&handle, &st, ch);
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_config,
            get_tts_queue_state,
            set_tts_paused,
            clear_tts_queue,
            skip_current_tts,
            tts_speak_text,
            test_tts,
            export_comments_csv,
            send_chat_message,
            update_config,
            add_channel,
            remove_channel,
            hide_message,
            unhide_message,
            get_obs_url,
            get_obs_goals_url,
            get_obs_timer_url,
            control_timer,
            list_templates,
            read_template_file,
            write_template_file,
            get_participants,
            pick_next_participant,
            pick_random_participant,
            remove_participant,
            clear_participants,
            inject_test_comment,
            check_for_update,
            open_url,
        ])
        .run(tauri::generate_context!())
        .expect("Tauri アプリの起動に失敗しました");
}
