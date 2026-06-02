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

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::bus::{is_valid_template_name, Bus};
use crate::config::{AppConfig, ChannelConfig, ChannelPlatform};
use crate::model::{
    Amount, Author, Badge, ChatMessage, Fragment, MessageKind, Participant, Platform, Roles,
};
use crate::moderation::{Moderator, Verdict};
use crate::sources::SourceManager;
use crate::stats::{spawn_stats_aggregator, StatsSnapshot, YoutubeMetadataUpdate};
use crate::tts::{bouyomi, TtsDispatcher};
use crate::update::{check_for_update, open_url};

/// アプリ全体で共有する実行時状態。
pub struct AppState {
    /// 現在の設定。
    config: Mutex<AppConfig>,
    /// stats 集約などへ設定変更を伝える watch 送信端。
    config_tx: watch::Sender<AppConfig>,
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
    /// TTS 一時停止中はワーカーが受信したメッセージを読み上げずに破棄する。
    tts_paused: Arc<AtomicBool>,
    /// TTS キュー全消し要求。rx はワーカー所有のためワーカー内で drain する。
    tts_clear: Arc<AtomicBool>,
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
        };
        format!("{p}:{}", ch.identifier)
    }
}

// ============ Tauri コマンド ============

/// 現在の設定を取得する。
#[tauri::command]
fn get_config(state: State<'_, AppState>) -> Result<AppConfig, String> {
    Ok(state.config.lock().unwrap().clone())
}

/// TTS 読み上げを一時停止/再開する。paused 中に届いたメッセージは保持せず破棄する。
#[tauri::command]
fn set_tts_paused(state: State<'_, AppState>, paused: bool) {
    state.tts_paused.store(paused, Ordering::Relaxed);
}

/// 未送出の TTS キューを全消しし、WebSpeech の現在発話も即時停止させる。
#[tauri::command]
fn clear_tts_queue(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    state.tts_clear.store(true, Ordering::Relaxed);
    app.emit("tts-cancel", ())
        .map_err(|e| format!("TTSキャンセルイベント送信失敗: {e}"))
}

/// 現在の WebSpeech 発話を停止する。外部バックエンドの送信済み発話は停止できない。
#[tauri::command]
fn skip_current_tts(app: AppHandle) -> Result<(), String> {
    app.emit("tts-cancel", ())
        .map_err(|e| format!("TTSキャンセルイベント送信失敗: {e}"))
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

/// 設定全体を更新して保存する。moderation/tts/obs などの実行時状態も反映する。
///
/// チャンネル一覧の差分適用(追加/削除)も行う。
#[tauri::command]
fn update_config(
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
        if !path.is_empty() {
            let host = committed_config.tts.options.bouyomi_host.clone();
            let port = committed_config.tts.options.bouyomi_port;
            tauri::async_runtime::spawn_blocking(move || {
                bouyomi::ensure_launched(path, host, port);
            });
        }
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

/// チャンネルを1件削除して停止する。`key` は `twitch:name` / `youtube:videoId`。
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
    let manager = SourceManager::new(state.source_tx.clone(), overrides.clone());
    let token = manager.spawn_channel(ch);
    let app_cancel = state.app_cancel.clone();
    let token_for_app_cancel = token.clone();
    tauri::async_runtime::spawn(async move {
        app_cancel.cancelled().await;
        token_for_app_cancel.cancel();
    });
    if ch.platform == ChannelPlatform::Youtube {
        sources::youtube::metadata::spawn_metadata_poller(
            ch.identifier.clone(),
            overrides,
            state.metadata_tx.clone(),
            token.clone(),
        );
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
                            state.tts_tx.try_send(msg.clone())
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
            tokio::select! {
                _ = cancel.cancelled() => break,
                recv = rx.recv() => {
                    let msg = match recv {
                        Some(m) => m,
                        None => break,
                    };
                    {
                        let state = app.state::<AppState>();
                        if state.tts_clear.swap(false, Ordering::Relaxed) {
                            while rx.try_recv().is_ok() {}
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
            let (config_tx, config_rx) = watch::channel::<AppConfig>(config.clone());
            let (metadata_tx, metadata_rx) = mpsc::channel::<YoutubeMetadataUpdate>(64);

            // 下流 Bus(パイプライン → UI/OBS)。
            let bus = Bus::new(obs_port, stats_tx.clone());

            // モデレータ。
            let moderator = Moderator::new(&config.moderation);

            // アプリ全体の停止トークン(将来 on_window_event 等から cancel 可能)。
            let app_cancel = CancellationToken::new();
            let obs_cancel = app_cancel.child_token();
            let templates_dir = resolve_templates_dir(&handle);

            let state = AppState {
                config: Mutex::new(config.clone()),
                config_tx,
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
                tts_paused: Arc::new(AtomicBool::new(false)),
                tts_clear: Arc::new(AtomicBool::new(false)),
                metadata_tx,
                participants: Mutex::new(Vec::new()),
            };
            app.manage(state);

            if config.tts.backend == config::TtsBackendKind::Bouyomi {
                let path = config.tts.options.bouyomi_path.trim().to_string();
                if !path.is_empty() {
                    let host = config.tts.options.bouyomi_host.clone();
                    let port = config.tts.options.bouyomi_port;
                    tauri::async_runtime::spawn_blocking(move || {
                        bouyomi::ensure_launched(path, host, port);
                    });
                }
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
            set_tts_paused,
            clear_tts_queue,
            skip_current_tts,
            export_comments_csv,
            update_config,
            add_channel,
            remove_channel,
            hide_message,
            unhide_message,
            get_obs_url,
            get_obs_goals_url,
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
