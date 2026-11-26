//! Bus 層 — Source 群から流れてくる正規化済み `ChatMessage` を
//! UI(Tauri IPC) と OBS(axum WebSocket) の双方へ配る中継ハブ。
//!
//! 設計上の要点:
//! - 内部は `tokio::sync::broadcast`(容量上限あり)。lag は drop 容認=最新優先。
//! - UI 向けは個別 emit せず、~16ms(1フレーム)単位で配列にまとめて `emit("chat", batch)`。
//! - OBS 向けは axum の `/ws` で push、`/` は `?template=<name>` を
//!   `templates/<name>/index.html` に解決し、静的アセットは `/<name>/...` で配信。
//! - クライアント(WS)ごとに bounded queue を持ち、溢れたら古いものから捨てる。

use std::net::{SocketAddr, TcpListener as StdTcpListener};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::StatusCode,
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::achievements::engine::AchievementUnlocked;
use crate::keyword_effects::EffectTriggered;
use crate::model::{ChatMessage, ModerationEvent, YoutubeReaction};
use crate::poll::PollSnapshot;
use crate::stats::{StatsSnapshot, TimerSnapshot};
use crate::templates::{TemplateRef, TemplateStore};

pub(crate) mod relay;

/// broadcast チャネルの容量。UI/OBS の購読者が遅れても最新が優先される。
const BROADCAST_CAPACITY: usize = 4096;
/// UI へのバッチ送出間隔(約 60fps = 16ms)。
const UI_BATCH_INTERVAL_MS: u64 = 16;
/// 1回のリアクションIPCに載せる種類数上限。表示用だけをdropし、統計には影響させない。
const MAX_REACTION_ITEMS_PER_UI_BATCH: usize = 256;
/// 1回のモデレーションIPCに載せるイベント数上限(異常流量時の保険)。
const MAX_MODERATION_EVENTS_PER_UI_BATCH: usize = 1024;
/// WS クライアントごとの送信キュー上限(溢れたら古いものから drop)。
const WS_CLIENT_QUEUE: usize = 256;
/// OBS 向けモデレーションイベントの broadcast 容量。
const MODERATION_BROADCAST_CAPACITY: usize = 1024;
/// opt-in WS クライアントごとのモデレーションイベント待ち上限(溢れたら古いものから drop)。
const WS_CLIENT_MODERATION_QUEUE: usize = 1024;
/// 実績解除の broadcast 容量。解除記録は保存済みなので、落ちるのは通知だけ。
const ACHIEVEMENT_BROADCAST_CAPACITY: usize = 1024;
/// 1回の実績 IPC に載せる解除数上限(異常流量時の保険)。
const MAX_ACHIEVEMENTS_PER_UI_BATCH: usize = 1024;
/// opt-in WS クライアントごとの実績待ち上限(溢れたら古いものから drop)。
const WS_CLIENT_ACHIEVEMENT_QUEUE: usize = 1024;
/// キーワードエフェクトの broadcast 容量。落ちるのは演出だけ。
const EFFECT_BROADCAST_CAPACITY: usize = 1024;
/// 1回のエフェクト IPC に載せる件数上限(異常流量時の保険)。
const MAX_EFFECTS_PER_UI_BATCH: usize = 256;
/// opt-in WS クライアントごとのエフェクト待ち上限(溢れたら古いものから drop)。
const WS_CLIENT_EFFECT_QUEUE: usize = 256;

/// YouTube リアクションをUIへ1フレーム単位でまとめて送る。
///
/// Sourceごとの応答バッチをさらに16ms窓で平坦化し、複数配信から同時に届いても
/// 1リアクション1 IPC にならないようにする。OBSコメントWebSocketには混ぜない。
pub fn spawn_reaction_ui_forwarder(
    mut rx: mpsc::Receiver<Vec<YoutubeReaction>>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
        let mut batch = Vec::<YoutubeReaction>::with_capacity(32);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if !batch.is_empty() {
                        if let Err(e) = app.emit("youtube-reactions", &batch) {
                            tracing::warn!("YouTubeリアクションのUI emitに失敗: {e}");
                        }
                        batch.clear();
                    }
                }
                incoming = rx.recv() => {
                    match incoming {
                        Some(reactions) => {
                            let available = MAX_REACTION_ITEMS_PER_UI_BATCH.saturating_sub(batch.len());
                            batch.extend(reactions.into_iter().take(available));
                        }
                        None => break,
                    }
                }
            }
        }

        tracing::info!("YouTube reaction forwarder 終了");
    });
}

/// プラットフォーム側の削除・BAN 通知を UI へ1フレーム単位でまとめて送る。
///
/// `on_batch` は emit の直前に1フレーム分のイベントで1回呼ばれる(Moderator の
/// hidden_ids 反映・TTS キュー除外など)。chat とは別イベント `chat-moderation` で送り、
/// UI 側の rAF flush で chat バッチの後に適用する。
pub fn spawn_moderation_ui_forwarder<F>(
    mut rx: mpsc::Receiver<ModerationEvent>,
    app: AppHandle,
    cancel: CancellationToken,
    on_batch: F,
) where
    F: Fn(&[ModerationEvent]) + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
        let mut batch = Vec::<ModerationEvent>::with_capacity(16);

        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = ticker.tick() => {
                    if !batch.is_empty() {
                        on_batch(&batch);
                        if let Err(e) = app.emit("chat-moderation", &batch) {
                            tracing::warn!("モデレーションイベントのUI emitに失敗: {e}");
                        }
                        batch.clear();
                    }
                }
                incoming = rx.recv() => {
                    match incoming {
                        Some(event) => {
                            if batch.len() < MAX_MODERATION_EVENTS_PER_UI_BATCH {
                                batch.push(event);
                            }
                        }
                        None => break,
                    }
                }
            }
        }

        tracing::info!("moderation forwarder 終了");
    });
}

/// 実績の解除を購読して 1 フレーム単位でまとめ、`emit` へ渡す。空のフレームでは呼ばない。
///
/// 購読側が遅れて `Lagged(n)` を受けても件数だけ warn に出して続ける(名前・ID は出さない)。
async fn run_achievement_forwarder<F>(
    mut rx: broadcast::Receiver<SequencedAchievement>,
    cancel: CancellationToken,
    mut emit: F,
) where
    F: FnMut(&[AchievementUnlocked]),
{
    let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
    let mut batch = Vec::<AchievementUnlocked>::with_capacity(8);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    emit(&batch);
                    batch.clear();
                }
            }
            incoming = rx.recv() => {
                match incoming {
                    Ok(unlock) => {
                        if batch.len() < MAX_ACHIEVEMENTS_PER_UI_BATCH {
                            batch.push(unlock.unlock);
                        }
                        // 既に届いている分も同じフレームに入れる。tick と受信が同時に準備できた時に、
                        // 同時に送られた解除が 2 フレームに割れないようにする。
                        loop {
                            match rx.try_recv() {
                                Ok(unlock) => {
                                    if batch.len() < MAX_ACHIEVEMENTS_PER_UI_BATCH {
                                        batch.push(unlock.unlock);
                                    }
                                }
                                Err(broadcast::error::TryRecvError::Lagged(n)) => {
                                    tracing::warn!(lagged = n, "実績解除の通知を取りこぼしました");
                                }
                                Err(_) => break,
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "実績解除の通知を取りこぼしました");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// 実績の解除を UI へ 1 フレーム単位でまとめて `emit("achievements", batch)` する。
pub(crate) fn spawn_achievement_ui_forwarder(
    rx: broadcast::Receiver<SequencedAchievement>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        run_achievement_forwarder(rx, cancel, |batch| {
            if let Err(e) = app.emit("achievements", batch) {
                tracing::warn!("実績解除のUI emitに失敗: {e}");
            }
        })
        .await;
        tracing::info!("achievement forwarder 終了");
    });
}

/// アプリ内表示向けのキーワードエフェクトを購読して 1 フレーム単位でまとめ、`emit` へ渡す。
/// 表示先に app を含まない物は捨てる。空のフレームでは呼ばない。
async fn run_effect_forwarder<F>(
    mut rx: broadcast::Receiver<EffectTriggered>,
    cancel: CancellationToken,
    mut emit: F,
) where
    F: FnMut(&[EffectTriggered]),
{
    let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
    let mut batch = Vec::<EffectTriggered>::with_capacity(8);

    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = ticker.tick() => {
                if !batch.is_empty() {
                    emit(&batch);
                    batch.clear();
                }
            }
            incoming = rx.recv() => {
                match incoming {
                    Ok(event) => {
                        if event.target.includes_app() && batch.len() < MAX_EFFECTS_PER_UI_BATCH {
                            batch.push(event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "キーワードエフェクトの通知を取りこぼしました");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
}

/// キーワードエフェクトを UI へ 1 フレーム単位でまとめて `emit("keyword-effects", batch)` する。
pub(crate) fn spawn_effect_ui_forwarder(
    rx: broadcast::Receiver<EffectTriggered>,
    app: AppHandle,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        run_effect_forwarder(rx, cancel, |batch| {
            if let Err(e) = app.emit("keyword-effects", batch) {
                tracing::warn!("キーワードエフェクトのUI emitに失敗: {e}");
            }
        })
        .await;
        tracing::info!("effect forwarder 終了");
    });
}

#[derive(Clone, Debug)]
pub struct SequencedChat {
    pub seq: u64,
    pub message: ChatMessage,
}

#[derive(Clone, Debug)]
pub(crate) struct SequencedAchievement {
    pub(crate) after_seq: u64,
    pub(crate) unlock: AchievementUnlocked,
}

/// Bus のハンドル。正規化済みメッセージの投入と、UI/OBS への配信起動を担う。
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<SequencedChat>,
    chat_seq: Arc<Mutex<u64>>,
    /// OBS `/ws?modEvents=1` へ流す削除・BAN・ローカル非表示(配信側・アプリ発の両方)。
    mod_tx: broadcast::Sender<ModerationEvent>,
    /// 実績の解除。UI・OBS(`/ws?achievementEvents=1`)・TTS が購読する。
    achievement_tx: broadcast::Sender<SequencedAchievement>,
    /// キーワードエフェクト。UI(target に app)と OBS(`/ws?effectEvents=1`、target に obs)が購読する。
    effect_tx: broadcast::Sender<EffectTriggered>,
    stats_tx: watch::Sender<StatsSnapshot>,
    timer_tx: watch::Sender<TimerSnapshot>,
    poll_tx: watch::Sender<PollSnapshot>,
    obs_port: u16,
}

impl Bus {
    /// 新しい Bus を生成する。`obs_port` は OBS overlay サーバの待受ポート。
    pub fn new(
        obs_port: u16,
        stats_tx: watch::Sender<StatsSnapshot>,
        timer_tx: watch::Sender<TimerSnapshot>,
    ) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        let (mod_tx, _mod_rx) = broadcast::channel(MODERATION_BROADCAST_CAPACITY);
        let (achievement_tx, _achievement_rx) = broadcast::channel(ACHIEVEMENT_BROADCAST_CAPACITY);
        let (effect_tx, _effect_rx) = broadcast::channel(EFFECT_BROADCAST_CAPACITY);
        Bus {
            tx,
            chat_seq: Arc::new(Mutex::new(0)),
            mod_tx,
            achievement_tx,
            effect_tx,
            stats_tx,
            timer_tx,
            poll_tx: watch::channel(PollSnapshot::default()).0,
            obs_port,
        }
    }

    pub(crate) fn with_poll(mut self, poll_tx: watch::Sender<PollSnapshot>) -> Self {
        self.poll_tx = poll_tx;
        self
    }

    /// 削除・BAN イベントの購読者を得る。
    pub fn subscribe_moderation(&self) -> broadcast::Receiver<ModerationEvent> {
        self.mod_tx.subscribe()
    }

    pub(crate) fn subscribe_achievements(&self) -> broadcast::Receiver<SequencedAchievement> {
        self.achievement_tx.subscribe()
    }

    pub(crate) fn publish_achievement(&self, unlock: AchievementUnlocked, after_seq: u64) {
        let _ = self
            .achievement_tx
            .send(SequencedAchievement { after_seq, unlock });
    }

    pub(crate) fn subscribe_effects(&self) -> broadcast::Receiver<EffectTriggered> {
        self.effect_tx.subscribe()
    }

    /// キーワードエフェクトを流す。呼び出し側は原因の chat を先に発行しておく。
    pub(crate) fn publish_effect(&self, event: EffectTriggered) {
        let _ = self.effect_tx.send(event);
    }

    /// 新規コメント購読者を得る。
    pub fn subscribe(&self) -> broadcast::Receiver<SequencedChat> {
        self.tx.subscribe()
    }

    pub(crate) fn subscribe_chat_with_baseline(&self) -> (broadcast::Receiver<SequencedChat>, u64) {
        let seq = self
            .chat_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (self.tx.subscribe(), *seq)
    }

    pub(crate) fn latest_chat_seq(&self) -> u64 {
        *self
            .chat_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// 1メッセージを Bus に流す(Tauri コマンド経由のシステムメッセージ等にも使う)。
    /// 購読者が居なくてもエラーにしない。
    pub fn publish(&self, msg: ChatMessage) {
        self.publish_sequenced(msg);
    }

    pub(crate) fn publish_sequenced(&self, message: ChatMessage) -> u64 {
        // 番号と broadcast 上の順序を一致させ、購読境界も同じロックで確定する。
        let mut seq = self
            .chat_seq
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *seq += 1;
        let _ = self.tx.send(SequencedChat { seq: *seq, message });
        *seq
    }

    /// OBS の opt-in クライアントへモデレーションイベントを流す。購読者が居なくてもエラーにしない。
    pub fn publish_moderation(&self, event: ModerationEvent) {
        let _ = self.mod_tx.send(event);
    }

    /// UI 向けバッチ送出ループを起動する。
    ///
    /// broadcast を購読し、~16ms ごとに溜まったメッセージを配列で `emit("chat", batch)`。
    /// `cancel` 発火で終了。
    pub fn spawn_ui_forwarder(&self, app: AppHandle, cancel: CancellationToken) {
        let mut rx = self.tx.subscribe();
        tauri::async_runtime::spawn(async move {
            let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
            let mut batch: Vec<ChatMessage> = Vec::with_capacity(64);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = ticker.tick() => {
                        if !batch.is_empty() {
                            // 1フレーム分をまとめて UI へ。IPC 往復を削減する。
                            if let Err(e) = app.emit("chat", &batch) {
                                tracing::warn!("UI への emit に失敗: {e}");
                            }
                            batch.clear();
                        }
                    }
                    recv = rx.recv() => {
                        match recv {
                            Ok(chat) => batch.push(chat.message),
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                // 最新優先: 取りこぼしはログのみ。
                                tracing::debug!("UI forwarder が {n} 件 lag");
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
            tracing::info!("UI forwarder 終了");
        });
    }

    /// OBS overlay サーバ(axum)を起動する。
    ///
    /// - `GET /ws` : WebSocket。UI と同じ ~16ms バッチ(JSON 配列)で push。
    ///   クエリ `?channel=...` 指定時はそのチャンネルのみに絞り込む。
    /// - `GET /?template=<id>`: 内蔵 `<name>` は `templates_dir/<name>/index.html`、
    ///   カスタム `custom:<name>` は `custom_templates_dir/<name>/index.html` を返す。
    ///   `template` 未指定時は `default`。`../` 等のトラバーサルは拒否する。
    /// - `GET /<name>/...`: 内蔵テンプレの静的アセットを配信(ServeDir)。
    /// - `GET /custom/<name>/...`: カスタムテンプレの静的アセットを配信(ServeDir)。
    /// - `GET /skin/<file>.png`: ユーザー作成のGoals背景スキンを配信。
    ///
    /// `cancel` 発火でサーバを graceful shutdown する。
    pub fn spawn_obs_server(
        &self,
        templates_dir: PathBuf,
        custom_templates_dir: PathBuf,
        goals_skin_dir: PathBuf,
        cancel: CancellationToken,
        extra: Option<Router>,
    ) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
        self.spawn_obs_server_on_port(
            templates_dir,
            custom_templates_dir,
            goals_skin_dir,
            self.obs_port,
            cancel,
            extra,
        )
    }

    /// 指定ポートで OBS overlay サーバ(axum)を起動する。
    ///
    /// `update_config` によるポート変更時の再 bind で使う。
    pub fn spawn_obs_server_on_port(
        &self,
        templates_dir: PathBuf,
        custom_templates_dir: PathBuf,
        goals_skin_dir: PathBuf,
        port: u16,
        cancel: CancellationToken,
        extra: Option<Router>,
    ) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
        let addr = SocketAddr::from(([127, 0, 0, 1], port));
        let std_listener = StdTcpListener::bind(addr).map_err(|e| {
            let msg = format!("OBS サーバの bind に失敗 {addr}: {e}");
            tracing::error!("{msg}");
            msg
        })?;
        std_listener.set_nonblocking(true).map_err(|e| {
            let msg = format!("OBS サーバ listener の nonblocking 設定に失敗 {addr}: {e}");
            tracing::error!("{msg}");
            msg
        })?;

        let app = self.obs_app_until(
            templates_dir.clone(),
            custom_templates_dir.clone(),
            goals_skin_dir.clone(),
            extra,
            cancel.clone(),
        );

        let handle = tauri::async_runtime::spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("OBS サーバ listener 初期化に失敗 {addr}: {e}");
                    return;
                }
            };

            tracing::info!(
                "OBS overlay サーバ起動: http://{addr}/  (templates: {}, custom: {}, skins: {})",
                templates_dir.display(),
                custom_templates_dir.display(),
                goals_skin_dir.display()
            );

            let shutdown = async move {
                cancel.cancelled().await;
                tracing::info!("OBS サーバ shutdown 開始");
            };

            if let Err(e) = axum::serve(listener, app)
                .with_graceful_shutdown(shutdown)
                .await
            {
                tracing::error!("OBS サーバ異常終了: {e}");
            }
        });

        Ok(handle)
    }

    /// Compose additional routes outside the legacy OBS CORS layer.
    pub fn obs_app(
        &self,
        templates_dir: PathBuf,
        custom_templates_dir: PathBuf,
        goals_skin_dir: PathBuf,
        extra: Option<Router>,
    ) -> Router {
        self.obs_app_until(
            templates_dir,
            custom_templates_dir,
            goals_skin_dir,
            extra,
            CancellationToken::new(),
        )
    }

    /// `obs_app` と同じ。`cancel` 発火で確立済みの WebSocket も閉じる
    /// (graceful shutdown はアップグレード後の接続を待たず、閉じもしない)。
    fn obs_app_until(
        &self,
        templates_dir: PathBuf,
        custom_templates_dir: PathBuf,
        goals_skin_dir: PathBuf,
        extra: Option<Router>,
        cancel: CancellationToken,
    ) -> Router {
        let app = obs_router(
            self.obs_state(
                TemplateStore::new(templates_dir, custom_templates_dir),
                cancel,
            ),
            &goals_skin_dir,
        );
        match extra {
            Some(api) => app.layer(axum::middleware::from_fn(
                // `/api` 配下はルータの照合(末尾スラッシュ等)に任せず、CORS レイヤより外側で
                // 丸ごと API ルータへ渡す。旧ルートの fallback や CORS に落とさないため。
                move |req: axum::extract::Request, next: axum::middleware::Next| {
                    let api = api.clone();
                    async move {
                        let path = req.uri().path();
                        if path == "/api" || path.starts_with("/api/") {
                            use tower::ServiceExt;
                            match api.oneshot(req).await {
                                Ok(response) => response,
                                Err(never) => match never {},
                            }
                        } else {
                            next.run(req).await
                        }
                    }
                },
            )),
            None => app,
        }
    }

    fn obs_state(&self, templates: TemplateStore, cancel: CancellationToken) -> ObsState {
        ObsState {
            cancel,
            tx: Arc::new(self.tx.clone()),
            mod_tx: Arc::new(self.mod_tx.clone()),
            achievement_tx: Arc::new(self.achievement_tx.clone()),
            effect_tx: Arc::new(self.effect_tx.clone()),
            stats_tx: Arc::new(self.stats_tx.clone()),
            timer_tx: Arc::new(self.timer_tx.clone()),
            poll_tx: Arc::new(self.poll_tx.clone()),
            templates: Arc::new(templates),
        }
    }
}

/// OBS overlay サーバのルータを組み立てる。
fn obs_router(state: ObsState, goals_skin_dir: &Path) -> Router {
    // テンプレ静的配信。ディレクトリ直アクセス時は index.html を返す。
    let serve_dir =
        ServeDir::new(state.templates.builtin_root()).append_index_html_on_directories(true);
    let skin_dir = ServeDir::new(goals_skin_dir);
    // 2 つのルートが重なる時はカスタムを配信しない(TemplateStore と同じ判定)。
    let custom_dir = state.templates.custom_enabled().then(|| {
        ServeDir::new(state.templates.custom_root()).append_index_html_on_directories(true)
    });

    let app = match custom_dir {
        Some(custom_dir) => Router::new().nest_service("/custom", custom_dir),
        None => Router::new(),
    };
    app
        .route("/ws", get(ws_handler))
        .route("/stats", get(stats_ws_handler))
        .route("/timer", get(timer_ws_handler))
        .route("/poll", get(poll_ws_handler))
        .route("/", get(template_index_handler))
        .nest_service("/skin", skin_dir)
        .fallback_service(serve_dir)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// axum ハンドラ間で共有する状態。
#[derive(Clone)]
struct ObsState {
    /// サーバの停止トークン。発火で各 WebSocket 接続を閉じる。
    cancel: CancellationToken,
    tx: Arc<broadcast::Sender<SequencedChat>>,
    mod_tx: Arc<broadcast::Sender<ModerationEvent>>,
    achievement_tx: Arc<broadcast::Sender<SequencedAchievement>>,
    effect_tx: Arc<broadcast::Sender<EffectTriggered>>,
    stats_tx: Arc<watch::Sender<StatsSnapshot>>,
    timer_tx: Arc<watch::Sender<TimerSnapshot>>,
    poll_tx: Arc<watch::Sender<PollSnapshot>>,
    templates: Arc<TemplateStore>,
}

/// `/ws` のクエリ。`channel` 指定時は当該チャンネルのみに絞り込む(任意)。
#[derive(Debug, Deserialize)]
struct WsQuery {
    channel: Option<String>,
    /// `modEvents=1`(または `true`)のクライアントにだけ `{"moderation":[…]}` フレームを送る。
    /// 未指定・`0`・値なしは opt-out(改造テンプレに未知フレームを送らない)。
    #[serde(rename = "modEvents")]
    mod_events: Option<String>,
    /// `achievementEvents=1`(または `true`)のクライアントにだけ `{"achievements":[…]}` を送る。
    /// 解除には channel が無いので `channel` フィルタは掛けない。
    #[serde(rename = "achievementEvents")]
    achievement_events: Option<String>,
    /// `effectEvents=1`(または `true`)のクライアントにだけ `{"effects":[…]}` を送る。
    /// エフェクトには channel が無いので `channel` フィルタは掛けない。
    #[serde(rename = "effectEvents")]
    effect_events: Option<String>,
}

/// `modEvents` クエリ値が opt-in か。
fn mod_events_opted_in(value: Option<&str>) -> bool {
    matches!(value.map(str::trim), Some("1") | Some("true"))
}

/// opt-in のときだけモデレーションイベントを購読する(opt-out は購読自体しない)。
fn moderation_receiver(
    mod_tx: &broadcast::Sender<ModerationEvent>,
    query: &WsQuery,
) -> Option<broadcast::Receiver<ModerationEvent>> {
    mod_events_opted_in(query.mod_events.as_deref()).then(|| mod_tx.subscribe())
}

/// opt-in のときだけ実績解除を購読する(opt-out は購読自体しない)。
fn achievement_receiver(
    achievement_tx: &broadcast::Sender<SequencedAchievement>,
    query: &WsQuery,
) -> Option<broadcast::Receiver<SequencedAchievement>> {
    mod_events_opted_in(query.achievement_events.as_deref()).then(|| achievement_tx.subscribe())
}

/// opt-in のときだけキーワードエフェクトを購読する(opt-out は購読自体しない)。
fn effect_receiver(
    effect_tx: &broadcast::Sender<EffectTriggered>,
    query: &WsQuery,
) -> Option<broadcast::Receiver<EffectTriggered>> {
    mod_events_opted_in(query.effect_events.as_deref()).then(|| effect_tx.subscribe())
}

/// 1 tick 分のエフェクトを `{"effects":[…]}` へ。空なら送らない。
fn effect_frame(events: &[EffectTriggered]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    match serde_json::to_string(&serde_json::json!({ "effects": events })) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("WS 向けエフェクト JSON 化失敗: {e}");
            None
        }
    }
}

/// 1 tick 分の実績解除を `{"achievements":[…]}` へ。空なら送らない。
fn achievement_frame(unlocks: &[AchievementUnlocked]) -> Option<String> {
    if unlocks.is_empty() {
        return None;
    }
    match serde_json::to_string(&serde_json::json!({ "achievements": unlocks })) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("WS 向け実績 JSON 化失敗: {e}");
            None
        }
    }
}

/// 1 tick 分のモデレーションイベントを `{"moderation":[…]}` へ。空なら送らない。
fn moderation_frame(events: &[ModerationEvent]) -> Option<String> {
    if events.is_empty() {
        return None;
    }
    match serde_json::to_string(&serde_json::json!({ "moderation": events })) {
        Ok(json) => Some(json),
        Err(e) => {
            tracing::warn!("WS 向けモデレーション JSON 化失敗: {e}");
            None
        }
    }
}

/// `channel` フィルタ付きクライアントには、そのチャンネルのイベントだけを渡す。
fn moderation_event_channel(event: &ModerationEvent) -> &str {
    match event {
        ModerationEvent::MessageDeleted { channel, .. }
        | ModerationEvent::UserCleared { channel, .. }
        | ModerationEvent::ChatCleared { channel, .. } => channel,
    }
}

/// `/` のクエリ。`template` 未指定時は default を使う。
#[derive(Debug, Deserialize)]
struct TemplateQuery {
    template: Option<String>,
}

/// OBS テンプレート名として許可する文字種。
///
/// 1階層のディレクトリ名だけを許し、`../` や `%2e%2e` のようなパストラバーサルを拒否する。
pub fn is_valid_template_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// `GET /?template=<id>` を内蔵 `<name>` / カスタム `custom:<name>` の index.html に解決する。
async fn template_index_handler(
    State(state): State<ObsState>,
    Query(query): Query<TemplateQuery>,
) -> impl IntoResponse {
    let template = query.template.as_deref().unwrap_or("default");
    let Ok(template_ref) = TemplateRef::parse(template) else {
        return (
            StatusCode::BAD_REQUEST,
            "invalid template name: use only ASCII letters, digits, '-' or '_'",
        )
            .into_response();
    };

    let dir = match &template_ref {
        // 内蔵は従来どおり単純連結(名前は parse で検証済み)。
        TemplateRef::Builtin(name) => state.templates.builtin_root().join(name),
        TemplateRef::Custom(_) => match state.templates.resolve(&template_ref) {
            Ok(dir) => dir,
            Err(_) => {
                return (
                    StatusCode::NOT_FOUND,
                    format!("template not found: {template}"),
                )
                    .into_response()
            }
        },
    };
    let index_path = dir.join("index.html");
    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => Html(inject_base_href(html, &template_ref.url_base())).into_response(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::NOT_FOUND,
            format!("template not found: {template}"),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!(
                "OBS テンプレ index.html 読み込み失敗 {}: {e}",
                index_path.display()
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "failed to read template index.html",
            )
                .into_response()
        }
    }
}

/// ルート URL から返した index.html 内の相対 CSS/JS を `/<template>/...` へ向ける。
#[cfg(test)]
fn inject_template_base(html: String, template: &str) -> String {
    inject_base_href(html, &format!("/{template}/"))
}

/// index.html の `<head>` 直後に `<base href="{href}">` を入れる(既にあれば何もしない)。
fn inject_base_href(mut html: String, href: &str) -> String {
    if html.contains("<base ") || html.contains("<base>") {
        return html;
    }

    let base = format!(r#"<base href="{href}">"#);
    if let Some(pos) = html.find("<head>") {
        html.insert_str(pos + "<head>".len(), &base);
    }
    html
}

/// `/ws` のアップグレードハンドラ。
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let rx = state.tx.subscribe();
    let mod_rx = moderation_receiver(&state.mod_tx, &query);
    let achievement_rx = achievement_receiver(&state.achievement_tx, &query);
    let effect_rx = effect_receiver(&state.effect_tx, &query);
    ws.on_upgrade(move |socket| {
        until_stopped(
            state.cancel,
            handle_ws_client(socket, rx, mod_rx, achievement_rx, effect_rx, query.channel),
        )
    })
}

/// `/stats` のアップグレードハンドラ。
async fn stats_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.stats_tx.subscribe();
    ws.on_upgrade(move |socket| until_stopped(state.cancel, handle_stats_ws_client(socket, rx)))
}

/// `/timer` のアップグレードハンドラ。
async fn timer_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.timer_tx.subscribe();
    ws.on_upgrade(move |socket| {
        until_stopped(state.cancel, handle_snapshot_ws_client(socket, rx))
    })
}

async fn poll_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.poll_tx.subscribe();
    ws.on_upgrade(move |socket| {
        until_stopped(state.cancel, handle_snapshot_ws_client(socket, rx))
    })
}

/// サーバの停止で接続ごとのタスクを打ち切る。ソケットは future と一緒に破棄されて閉じる。
async fn until_stopped(cancel: CancellationToken, client: impl std::future::Future<Output = ()>) {
    cancel.run_until_cancelled(client).await;
}

/// 1 WS クライアントの送信ループ。
///
/// broadcast を購読し、UI forwarder と同形の ~16ms バッチ(JSON 配列)で push する。
/// 受信(rx.recv)と送信(tick での flush)を分離し、遅いクライアントの socket.send が
/// 受信をブロックしないようにする。受信は per-client の bounded queue を埋め続け、
/// 満杯なら古いものから drop(最新優先)。`channel` 指定時は一致しないメッセージを捨てる。
///
/// `mod_rx` は `?modEvents=1` の opt-in クライアントだけ Some。同じ tick で chat 配列の
/// **後に** `{"moderation":[…]}` を送り、chat との順序を同じソケット上で保つ。
/// `achievement_rx` は `?achievementEvents=1` の opt-in だけ Some。その後に `{"achievements":[…]}`
/// を送る(`channel` フィルタは掛けない)。
/// `effect_rx` は `?effectEvents=1` の opt-in だけ Some。最後に `{"effects":[…]}` を送る
/// (`channel` フィルタは掛けず、表示先に obs を含む物だけ)。
async fn handle_ws_client(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<SequencedChat>,
    mut mod_rx: Option<broadcast::Receiver<ModerationEvent>>,
    mut achievement_rx: Option<broadcast::Receiver<SequencedAchievement>>,
    mut effect_rx: Option<broadcast::Receiver<EffectTriggered>>,
    channel: Option<String>,
) {
    let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
    // クライアント個別の bounded queue。溢れたら古いものから drop(最新優先)。
    let mut queue: std::collections::VecDeque<ChatMessage> = std::collections::VecDeque::new();
    let mut mod_queue: std::collections::VecDeque<ModerationEvent> =
        std::collections::VecDeque::new();
    let mut achievement_queue: std::collections::VecDeque<AchievementUnlocked> =
        std::collections::VecDeque::new();
    // 受信した解除は 1 tick 保留する。原因のコメントは解除より後に publish されうるので、
    // 次の tick でチャットを送った後に出す。
    let mut achievement_held: std::collections::VecDeque<AchievementUnlocked> =
        std::collections::VecDeque::new();

    let mut effect_queue: std::collections::VecDeque<EffectTriggered> =
        std::collections::VecDeque::new();
    // 実績と同じく 1 tick 保留し、原因のコメントを先に送る。
    let mut effect_held: std::collections::VecDeque<EffectTriggered> =
        std::collections::VecDeque::new();

    loop {
        tokio::select! {
            // broadcast からの受信 → channel フィルタ後キューへ。受信は止めない。
            recv = rx.recv() => {
                match recv {
                    Ok(chat) => {
                        let msg = chat.message;
                        if let Some(ref ch) = channel {
                            if &msg.channel != ch {
                                continue;
                            }
                        }
                        if queue.len() >= WS_CLIENT_QUEUE {
                            queue.pop_front(); // 古いものを捨てる(drop-oldest)
                        }
                        queue.push_back(msg);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        // 最新優先: 取りこぼしはログのみ。
                        tracing::debug!("WS forwarder が {n} 件 lag");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // opt-in クライアントだけ: モデレーションイベントをキューへ。
            recv = async { mod_rx.as_mut().unwrap().recv().await }, if mod_rx.is_some() => {
                match recv {
                    Ok(event) => {
                        if let Some(ref ch) = channel {
                            if moderation_event_channel(&event) != ch {
                                continue;
                            }
                        }
                        if mod_queue.len() >= WS_CLIENT_MODERATION_QUEUE {
                            mod_queue.pop_front();
                        }
                        mod_queue.push_back(event);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!("WS moderation forwarder が {n} 件 lag");
                    }
                    // 送信端が消えても chat 配信は続ける。
                    Err(broadcast::error::RecvError::Closed) => mod_rx = None,
                }
            }
            // opt-in クライアントだけ: 実績解除をキューへ(channel フィルタなし)。
            recv = async { achievement_rx.as_mut().unwrap().recv().await }, if achievement_rx.is_some() => {
                match recv {
                    Ok(unlock) => {
                        if achievement_queue.len() >= WS_CLIENT_ACHIEVEMENT_QUEUE {
                            achievement_queue.pop_front();
                        }
                        achievement_queue.push_back(unlock.unlock);
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "WS 実績 forwarder が取りこぼしました");
                    }
                    Err(broadcast::error::RecvError::Closed) => achievement_rx = None,
                }
            }
            // opt-in クライアントだけ: キーワードエフェクトをキューへ(channel フィルタなし)。
            recv = async { effect_rx.as_mut().unwrap().recv().await }, if effect_rx.is_some() => {
                match recv {
                    Ok(event) => {
                        if event.target.includes_obs() {
                            if effect_queue.len() >= WS_CLIENT_EFFECT_QUEUE {
                                effect_queue.pop_front();
                            }
                            effect_queue.push_back(event);
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(lagged = n, "WS エフェクト forwarder が取りこぼしました");
                    }
                    Err(broadcast::error::RecvError::Closed) => effect_rx = None,
                }
            }
            // ~16ms ごとに溜まった分を1つの JSON 配列としてまとめて送る。
            _ = ticker.tick() => {
                if !queue.is_empty() {
                    let batch: Vec<ChatMessage> = queue.drain(..).collect();
                    match serde_json::to_string(&batch) {
                        Ok(json) => {
                            if socket.send(WsMessage::Text(json.into())).await.is_err() {
                                return; // 切断
                            }
                        }
                        Err(e) => tracing::warn!("WS 向け JSON 化失敗: {e}"),
                    }
                }
                // chat の後にモデレーション(1 tick 1 フレーム)。
                let events: Vec<ModerationEvent> = mod_queue.drain(..).collect();
                if let Some(json) = moderation_frame(&events) {
                    if socket.send(WsMessage::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                // モデレーションの後に実績(1 tick 1 フレーム)。
                let unlocks: Vec<AchievementUnlocked> = achievement_held.drain(..).collect();
                achievement_held.append(&mut achievement_queue);
                if let Some(json) = achievement_frame(&unlocks) {
                    if socket.send(WsMessage::Text(json.into())).await.is_err() {
                        return;
                    }
                }
                // 最後にエフェクト(1 tick 1 フレーム)。
                let effects: Vec<EffectTriggered> = effect_held.drain(..).collect();
                effect_held.append(&mut effect_queue);
                if let Some(json) = effect_frame(&effects) {
                    if socket.send(WsMessage::Text(json.into())).await.is_err() {
                        return;
                    }
                }
            }
            // クライアントからのメッセージ(主に close / ping)を待つ。
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => { /* ping/pong/text は無視 */ }
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

/// `/stats` WS クライアントの送信ループ。
///
/// 接続直後に現在値を1回送り、以後 watch の変更ごとに JSON オブジェクトを push する。
async fn handle_stats_ws_client(
    mut socket: WebSocket,
    mut rx: watch::Receiver<StatsSnapshot>,
) {
    let initial = rx.borrow().clone();
    if send_stats_snapshot(&mut socket, &initial).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = rx.borrow_and_update().clone();
                if send_stats_snapshot(&mut socket, &snapshot).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn send_stats_snapshot(
    socket: &mut WebSocket,
    snapshot: &StatsSnapshot,
) -> Result<(), axum::Error> {
    match serde_json::to_string(snapshot) {
        Ok(json) => socket.send(WsMessage::Text(json.into())).await,
        Err(e) => {
            tracing::warn!("stats WS 向け JSON 化失敗: {e}");
            Ok(())
        }
    }
}

/// 接続直後と watch 更新時にスナップショットを送る(`/timer` / `/poll`)。
async fn handle_snapshot_ws_client<T: Clone + Serialize + Send + Sync + 'static>(
    mut socket: WebSocket,
    mut rx: watch::Receiver<T>,
) {
    let initial = rx.borrow_and_update().clone();
    if send_snapshot(&mut socket, &initial).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = rx.borrow_and_update().clone();
                if send_snapshot(&mut socket, &snapshot).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }
}

async fn send_snapshot(
    socket: &mut WebSocket,
    snapshot: &impl Serialize,
) -> Result<(), axum::Error> {
    match serde_json::to_string(snapshot) {
        Ok(json) => socket.send(WsMessage::Text(json.into())).await,
        Err(e) => {
            tracing::warn!("スナップショットの JSON 化失敗: {e}");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn composed_api_responses_have_no_cors_headers() {
        use axum::{
            body::Body,
            http::{header, Method, Request, StatusCode},
        };
        use http_body_util::BodyExt;
        use tower::ServiceExt;

        let root = tempfile::tempdir().unwrap();
        let (stats_tx, _) = watch::channel(StatsSnapshot::default());
        let (timer_tx, _) = watch::channel(TimerSnapshot::default());
        let bus = Bus::new(0, stats_tx, timer_tx);
        let gate = crate::api::ApiGate::closed();
        let token = crate::api::generate_token();
        let extra = crate::api::router(
            crate::api::ApiDeps::new(gate.clone(), Default::default()),
            1234,
        );
        let router = bus.obs_app(
            root.path().join("builtin"),
            root.path().join("custom"),
            root.path().join("skin"),
            Some(extra),
        );
        for enabled in [false, true] {
            if enabled {
                gate.open(&token);
            }
            for path in [
                "/api",
                "/api/",
                "/api/v1",
                "/api/v1/",
                "/api/v1/status",
                "/api/v1/status/",
                "/api/v1/status/x",
                "/api/nope-xyz",
            ] {
                for (method, origin, authorized) in [
                    (Method::GET, false, false),
                    (Method::GET, false, true),
                    (Method::GET, true, true),
                    (Method::POST, false, true),
                    (Method::OPTIONS, true, false),
                ] {
                    let mut request = Request::builder()
                        .method(method.clone())
                        .uri(path)
                        .header(header::HOST, "127.0.0.1:1234");
                    if origin {
                        request = request.header(header::ORIGIN, "https://example.com");
                    }
                    if method == Method::OPTIONS {
                        request = request
                            .header(header::ACCESS_CONTROL_REQUEST_METHOD, "GET")
                            .header(header::ACCESS_CONTROL_REQUEST_HEADERS, "authorization");
                    }
                    if authorized {
                        request = request.header(header::AUTHORIZATION, format!("Bearer {token}"));
                    }
                    let response = router
                        .clone()
                        .oneshot(request.body(Body::empty()).unwrap())
                        .await
                        .unwrap();
                    let expected = if !enabled {
                        StatusCode::NOT_FOUND
                    } else if origin {
                        StatusCode::FORBIDDEN
                    } else if !authorized {
                        StatusCode::UNAUTHORIZED
                    } else if path != "/api/v1/status" {
                        StatusCode::NOT_FOUND
                    } else if method != Method::GET {
                        StatusCode::METHOD_NOT_ALLOWED
                    } else {
                        StatusCode::OK
                    };
                    assert_eq!(
                        response.status(), expected,
                        "enabled={enabled}, {method} {path}"
                    );
                    for name in response.headers().keys() {
                        assert!(
                            !name.as_str().starts_with("access-control-") && name != header::VARY,
                            "unexpected {name}: enabled={enabled}, {method} {path}"
                        );
                    }
                    if !enabled {
                        assert!(!response.headers().contains_key(header::CACHE_CONTROL));
                        assert!(!response.headers().contains_key(header::X_CONTENT_TYPE_OPTIONS));
                        assert!(response
                            .into_body()
                            .collect()
                            .await
                            .unwrap()
                            .to_bytes()
                            .is_empty());
                    }
                }
            }
        }
    }

    // --- is_valid_template_name ---

    #[test]
    fn valid_template_name_default() {
        // 最も基本的な正常系: "default" は常に許可される
        assert!(is_valid_template_name("default"));
    }

    #[test]
    fn valid_template_name_with_hyphen_underscore_and_digits() {
        // ハイフン・アンダースコア・数字を含む名前は許可される
        assert!(is_valid_template_name("my-template_1"));
    }

    #[test]
    fn valid_template_name_uppercase() {
        // 大文字 ASCII も許可される
        assert!(is_valid_template_name("ABC123"));
    }

    #[test]
    fn valid_template_name_exactly_64_chars() {
        // 境界値: ちょうど 64 文字は許可される
        let name = "a".repeat(64);
        assert!(is_valid_template_name(&name));
    }

    #[test]
    fn invalid_template_name_65_chars() {
        // 境界値: 65 文字は拒否される
        let name = "a".repeat(65);
        assert!(!is_valid_template_name(&name));
    }

    #[test]
    fn invalid_template_name_empty() {
        // 空文字列は拒否される
        assert!(!is_valid_template_name(""));
    }

    #[test]
    fn invalid_template_name_dotdot() {
        // パストラバーサル ".." は拒否される
        assert!(!is_valid_template_name(".."));
    }

    #[test]
    fn invalid_template_name_dotdot_slash() {
        // パストラバーサル "../foo" は拒否される
        assert!(!is_valid_template_name("../foo"));
    }

    #[test]
    fn invalid_template_name_slash() {
        // スラッシュ区切り "a/b" は拒否される(1階層のみ許可)
        assert!(!is_valid_template_name("a/b"));
    }

    #[test]
    fn invalid_template_name_dot() {
        // ドットを含む "a.b" は拒否される
        assert!(!is_valid_template_name("a.b"));
    }

    #[test]
    fn invalid_template_name_percent_encoded_dotdot() {
        // URL エンコードされたトラバーサル "%2e%2e" は拒否される
        assert!(!is_valid_template_name("%2e%2e"));
    }

    #[test]
    fn invalid_template_name_backslash() {
        // バックスラッシュ "a\\b" は拒否される(Windows パス区切り対策)
        assert!(!is_valid_template_name("a\\b"));
    }

    #[test]
    fn invalid_template_name_space() {
        // 空白を含む名前は拒否される
        assert!(!is_valid_template_name("a b"));
    }

    // --- OBS moderation opt-in ---

    fn query(mod_events: Option<&str>) -> WsQuery {
        WsQuery {
            achievement_events: None,
            effect_events: None,
            channel: None,
            mod_events: mod_events.map(str::to_string),
        }
    }

    fn deleted(id: &str) -> ModerationEvent {
        ModerationEvent::MessageDeleted {
            platform: crate::model::Platform::Twitch,
            channel: "ch".into(),
            message_id: id.into(),
        }
    }

    #[test]
    fn mod_events_opt_in_parsing() {
        assert!(mod_events_opted_in(Some("1")));
        assert!(mod_events_opted_in(Some("true")));
        assert!(mod_events_opted_in(Some(" 1 ")));
        let rejected = [None, Some(""), Some("0"), Some("false"), Some("yes")];
        for v in rejected.into_iter().chain([Some("11")]) {
            assert!(!mod_events_opted_in(v), "{v:?}");
        }
    }

    #[test]
    fn ws_query_deserializes_mod_events_from_query_string() {
        let parse = |qs: &str| {
            let uri: axum::http::Uri = format!("/ws?{qs}").parse().unwrap();
            Query::<WsQuery>::try_from_uri(&uri).unwrap().0
        };
        let opted = |qs: &str| mod_events_opted_in(parse(qs).mod_events.as_deref());
        assert!(opted("modEvents=1&channel=a"));
        assert!(!opted("modEvents=0"));
        assert!(!opted("modEvents"));
        assert!(!opted("channel=a"));
    }

    #[test]
    fn only_opted_in_clients_receive_moderation_events() {
        let (mod_tx, _keep) = broadcast::channel(8);
        let mut opted_in =
            moderation_receiver(&mod_tx, &query(Some("1"))).expect("opt-in subscribes");
        assert!(moderation_receiver(&mod_tx, &query(None)).is_none());
        assert!(moderation_receiver(&mod_tx, &query(Some("0"))).is_none());
        // opt-out は購読していないので受信者数は opt-in の 1 + _keep の 1 だけ。
        assert_eq!(mod_tx.receiver_count(), 2);
        mod_tx.send(deleted("m1")).unwrap();
        assert_eq!(opted_in.try_recv().unwrap(), deleted("m1"));
    }

    #[test]
    fn moderation_frame_wraps_batch_and_skips_empty() {
        assert_eq!(moderation_frame(&[]), None);
        let json = moderation_frame(&[deleted("a"), deleted("b")]).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["moderation"].as_array().unwrap().len(), 2);
        assert_eq!(v["moderation"][0]["type"], "messageDeleted");
        assert_eq!(v["moderation"][1]["messageId"], "b");
        assert_eq!(moderation_event_channel(&deleted("a")), "ch");
    }

    // --- inject_template_base ---

    #[test]
    fn inject_base_inserts_after_head_tag() {
        // <head> がある html には <base href="/obs-skin/"> が <head> 直後に挿入される
        let html = "<html><head><title>Test</title></head><body></body></html>".to_string();
        let result = inject_template_base(html, "obs-skin");
        assert!(result.contains(r#"<head><base href="/obs-skin/">"#));
    }

    #[test]
    fn inject_base_reflects_template_name_in_href() {
        // template 名がそのまま base href のパスに反映される
        let html = "<html><head></head><body></body></html>".to_string();
        let result = inject_template_base(html, "my-overlay");
        assert!(result.contains(r#"<base href="/my-overlay/">"#));
    }

    #[test]
    fn inject_base_idempotent_when_base_tag_already_present() {
        // 既に <base ...> が存在する場合は変更されずに返る(冪等)
        let html = r#"<html><head><base href="/other/"><title>X</title></head></html>"#.to_string();
        let expected = html.clone();
        let result = inject_template_base(html, "default");
        assert_eq!(result, expected);
    }

    #[test]
    fn inject_base_idempotent_when_bare_base_tag_present() {
        // <base> (属性なし) が存在する場合も変更されずに返る
        let html = "<html><head><base><title>X</title></head></html>".to_string();
        let expected = html.clone();
        let result = inject_template_base(html, "default");
        assert_eq!(result, expected);
    }

    #[test]
    fn inject_base_no_change_when_no_head_tag() {
        // <head> タグが無い html は変更されずに返る
        let html = "<html><body>no head</body></html>".to_string();
        let expected = html.clone();
        let result = inject_template_base(html, "default");
        assert_eq!(result, expected);
    }

    // --- OBS HTTP(内蔵とカスタムの 2 ルート配信) ---

    fn temp_root() -> PathBuf {
        let root = std::env::temp_dir().join(format!("fc-obs-http-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    fn put_template(root: &Path, name: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("index.html"),
            format!("<html><head><title>{name}</title></head></html>"),
        )
        .unwrap();
        std::fs::write(dir.join("style.css"), "body{}").unwrap();
    }

    /// 内蔵 default を置いた builtin と、未作成の custom を持つ一時ルート。
    fn obs_roots() -> (PathBuf, PathBuf, PathBuf) {
        let root = temp_root();
        let builtin = root.join("builtin");
        put_template(&builtin, "default");
        (root.clone(), builtin, root.join("config").join("templates"))
    }

    async fn serve(builtin: &Path, custom: &Path) -> String {
        let (stats_tx, _) = watch::channel(StatsSnapshot::default());
        let (timer_tx, _) = watch::channel(TimerSnapshot::default());
        serve_bus(Bus::new(0, stats_tx, timer_tx), builtin, custom).await
    }

    async fn serve_bus(bus: Bus, builtin: &Path, custom: &Path) -> String {
        let state = bus.obs_state(
            TemplateStore::new(builtin.into(), custom.into()),
            CancellationToken::new(),
        );
        let app = obs_router(state, &builtin.join("no-skin"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn poll_ws_sends_snapshot_and_updates() {
        use futures_util::StreamExt;
        let (root, builtin, custom) = obs_roots();
        std::fs::create_dir_all(builtin.join("poll")).unwrap();
        std::fs::write(builtin.join("poll/app.js"), "/* test asset */").unwrap();
        let (stats_tx, _) = watch::channel(StatsSnapshot::default());
        let (timer_tx, _) = watch::channel(TimerSnapshot::default());
        let poll = crate::poll::Poll::default();
        let bus = Bus::new(0, stats_tx, timer_tx).with_poll(poll.sender());
        let base = serve_bus(bus, &builtin, &custom).await;
        let url = base.replacen("http:", "ws:", 1) + "/poll";
        let (mut socket, _) = tokio_tungstenite::connect_async(url).await.unwrap();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await.unwrap().unwrap().unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
        assert_eq!(snapshot["status"], "idle");
        let start: crate::poll::PollStart = serde_json::from_value(serde_json::json!({
            "question": "質問", "options": ["A", "B"],
            "display": "pie", "showPercent": true, "showCounts": false
        })).unwrap();
        let opened = poll.start(start.validate().unwrap()).unwrap();
        let update = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await.unwrap().unwrap().unwrap();
        let snapshot: serde_json::Value = serde_json::from_str(update.to_text().unwrap()).unwrap();
        assert_eq!(snapshot, serde_json::to_value(opened).unwrap());
        let asset = reqwest::get(format!("{base}/poll/app.js")).await.unwrap();
        assert_eq!(asset.status(), reqwest::StatusCode::OK);
        std::fs::remove_dir_all(root).unwrap();
    }

    async fn get_text(url: String) -> (u16, String) {
        let response = reqwest::get(url).await.unwrap();
        let status = response.status().as_u16();
        (status, response.text().await.unwrap())
    }

    #[tokio::test]
    async fn obs_http_builtin_unchanged() {
        let (root, builtin, custom) = obs_roots();
        let base = serve(&builtin, &custom).await;

        let (status, body) = get_text(format!("{base}/?template=default")).await;
        assert_eq!(status, 200);
        assert!(body.contains(r#"<base href="/default/">"#), "{body}");
        assert_eq!(get_text(format!("{base}/default/style.css")).await.0, 200);
        let (status, body) = get_text(format!("{base}/")).await;
        assert_eq!(status, 200);
        assert!(body.contains(r#"<base href="/default/">"#), "{body}");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn obs_http_serves_custom_namespace() {
        let (root, builtin, custom) = obs_roots();
        let base = serve(&builtin, &custom).await;
        // サーバ起動後に置いたファイルも配信される。
        put_template(&custom, "foo");

        for query in ["custom:foo", "custom%3Afoo"] {
            let (status, body) = get_text(format!("{base}/?template={query}")).await;
            assert_eq!(status, 200, "{query}");
            assert!(body.contains(r#"<base href="/custom/foo/">"#), "{body}");
        }
        assert_eq!(get_text(format!("{base}/custom/foo/style.css")).await.0, 200);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn obs_http_custom_does_not_leak_into_builtin_paths() {
        let (root, builtin, custom) = obs_roots();
        put_template(&custom, "foo");
        let base = serve(&builtin, &custom).await;

        assert_eq!(get_text(format!("{base}/?template=foo")).await.0, 404);
        assert_eq!(get_text(format!("{base}/foo/style.css")).await.0, 404);
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn obs_http_rejects_bad_ids() {
        let (root, builtin, custom) = obs_roots();
        put_template(&custom, "foo");
        let base = serve(&builtin, &custom).await;

        for query in ["custom:../x", "custom:", "other:foo"] {
            let (status, _) = get_text(format!("{base}/?template={query}")).await;
            assert_eq!(status, 400, "{query}");
        }
        let (status, body) = get_text(format!("{base}/?template=custom:nope")).await;
        assert_eq!(status, 404);
        assert_eq!(body, "template not found: custom:nope");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn obs_http_custom_root_missing_is_ok() {
        let (root, builtin, custom) = obs_roots();
        assert!(!custom.exists());
        let base = serve(&builtin, &custom).await;

        assert_eq!(get_text(format!("{base}/?template=default")).await.0, 200);
        assert_eq!(get_text(format!("{base}/default/style.css")).await.0, 200);
        assert_eq!(get_text(format!("{base}/?template=custom:foo")).await.0, 404);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn builtin_names_do_not_collide_with_routes() {
        let templates = Path::new(env!("CARGO_MANIFEST_DIR")).join("../templates");
        assert!(templates.join("default").is_dir(), "{}", templates.display());
        // timer は既存の内蔵テンプレ。`/timer` は完全一致ルートなので、`/timer/...` のアセットは
        // fallback の ServeDir に届く(既存の OBS URL を壊さないため名前は変えない)。
        for reserved in ["custom", "skin", "ws", "stats", "api"] {
            assert!(!templates.join(reserved).exists(), "templates/{reserved} が配信ルートと衝突する");
        }
    }

    fn sample_unlock(id: &str) -> AchievementUnlocked {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "achievementId": "viewer-platinum",
            "name": "テスト",
            "description": "",
            "tier": "platinum",
            "icon": "T",
            "subject": { "kind": "viewer", "platform": "twitch", "authorId": "a1", "name": "x" },
            "valueMilli": 0,
            "thresholdMilli": 0,
            "currency": null,
            "windowKey": "",
            "unlockedAtMs": 1
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn unlocks_batched_into_one_emit_per_frame() {
        let (tx, rx) = broadcast::channel(ACHIEVEMENT_BROADCAST_CAPACITY);
        let cancel = CancellationToken::new();
        let emits = Arc::new(std::sync::Mutex::new(Vec::<usize>::new()));
        let sink = emits.clone();
        let task = tokio::spawn(run_achievement_forwarder(rx, cancel.clone(), move |batch| {
            sink.lock().unwrap().push(batch.len());
        }));
        // 空のフレームでは emit しない。
        tokio::time::sleep(Duration::from_millis(80)).await;
        assert!(emits.lock().unwrap().is_empty());
        // 同一フレームの 3 件は 1 回の emit にまとまる(送信は同期的に完了する)。
        for i in 0..3 {
            tx.send(SequencedAchievement {
                after_seq: 0,
                unlock: sample_unlock(&format!("u{i}")),
            })
            .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(*emits.lock().unwrap(), vec![3]);
        cancel.cancel();
        task.await.unwrap();
    }

    #[tokio::test]
    async fn achievement_forwarder_survives_lagged() {
        let (tx, rx) = broadcast::channel(2);
        for i in 0..5 {
            tx.send(SequencedAchievement {
                after_seq: 0,
                unlock: sample_unlock(&format!("old{i}")),
            })
            .unwrap();
        }
        let cancel = CancellationToken::new();
        let emits = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = emits.clone();
        let task = tokio::spawn(run_achievement_forwarder(rx, cancel.clone(), move |batch| {
            sink.lock().unwrap().extend(batch.iter().map(|u| u.id.clone()));
        }));
        tokio::time::sleep(Duration::from_millis(80)).await;
        // Lagged の後も購読が続き、新しい解除が届く。
        tx.send(SequencedAchievement {
            after_seq: 0,
            unlock: sample_unlock("fresh"),
        })
        .unwrap();
        tokio::time::sleep(Duration::from_millis(80)).await;
        let seen = emits.lock().unwrap().clone();
        assert!(seen.contains(&"fresh".to_string()), "{seen:?}");
        assert!(!seen.contains(&"old0".to_string()), "{seen:?}");
        cancel.cancel();
        task.await.unwrap();
    }

    async fn ws_connect(
        bus: &Bus,
        query: &str,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
        let root = temp_root();
        let state = bus.obs_state(
            TemplateStore::new(root.join("builtin"), root.join("custom")),
            CancellationToken::new(),
        );
        let app = obs_router(state, &root.join("no-skin"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let (ws, _) = tokio_tungstenite::connect_async(format!("ws://{addr}/ws{query}"))
            .await
            .unwrap();
        ws
    }

    #[tokio::test]
    async fn stopping_obs_server_closes_established_websockets() {
        use futures_util::StreamExt;
        let bus = test_bus();
        let root = temp_root();
        let port = StdTcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let cancel = CancellationToken::new();
        let _server = bus
            .spawn_obs_server_on_port(
                root.join("builtin"),
                root.join("custom"),
                root.join("no-skin"),
                port,
                cancel.clone(),
                None,
            )
            .unwrap();
        let mut clients = Vec::new();
        for path in ["ws", "stats", "timer", "poll"] {
            let (ws, _) = tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{port}/{path}"))
                .await
                .unwrap();
            clients.push((path, ws));
        }
        // 接続は確立したまま(初回スナップショットを読んでも閉じない)。
        for (path, ws) in &mut clients {
            if *path != "ws" {
                assert!(ws.next().await.unwrap().is_ok(), "{path}");
            }
        }

        cancel.cancel();
        for (path, ws) in &mut clients {
            let closed = tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    match ws.next().await {
                        None | Some(Err(_)) => break,
                        Some(Ok(frame)) if frame.is_close() => break,
                        Some(Ok(_)) => {}
                    }
                }
            })
            .await;
            assert!(closed.is_ok(), "停止後も {path} が開いたまま");
        }
    }

    fn test_bus() -> Bus {
        let (stats_tx, _) = watch::channel(StatsSnapshot::default());
        let (timer_tx, _) = watch::channel(TimerSnapshot::default());
        Bus::new(0, stats_tx, timer_tx)
    }

    fn sample_chat(channel: &str) -> ChatMessage {
        serde_json::from_value(serde_json::json!({
            "id": "c1",
            "platform": "twitch",
            "channel": channel,
            "author": { "id": "u1", "name": "viewer" },
            "fragments": [],
            "kind": "normal",
            "timestampMs": 1
        }))
        .unwrap()
    }

    /// 受信したテキストフレームを `wait` の間だけ集める。
    async fn collect_frames(
        ws: &mut tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        wait: Duration,
    ) -> Vec<String> {
        use futures_util::StreamExt;
        let mut frames = Vec::new();
        let deadline = tokio::time::Instant::now() + wait;
        while let Ok(Some(Ok(msg))) = tokio::time::timeout_at(deadline, ws.next()).await {
            if let tokio_tungstenite::tungstenite::Message::Text(text) = msg {
                frames.push(text.to_string());
            }
        }
        frames
    }

    #[tokio::test]
    async fn ws_without_param_receives_no_achievement_frames() {
        let bus = test_bus();
        for query in ["", "?achievementEvents=0", "?achievementEvents=yes"] {
            let mut ws = ws_connect(&bus, query).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            bus.publish_achievement(sample_unlock("u1"), bus.latest_chat_seq());
            bus.publish(sample_chat("c"));
            let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
            assert_eq!(frames.len(), 1, "{query}: {frames:?}");
            assert!(frames[0].starts_with('['), "{query}: {frames:?}");
        }
    }

    #[tokio::test]
    async fn ws_with_param_receives_achievements_after_chat() {
        let bus = test_bus();
        let mut ws = ws_connect(&bus, "?achievementEvents=1").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(sample_chat("c"));
        bus.publish_achievement(sample_unlock("u1"), bus.latest_chat_seq());
        let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
        assert_eq!(frames.len(), 2, "{frames:?}");
        assert!(frames[0].starts_with('['), "{frames:?}");
        let value: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
        assert_eq!(value["achievements"][0]["id"], "u1");
        assert_eq!(value["achievements"][0]["achievementId"], "viewer-platinum");
    }

    #[tokio::test]
    async fn ws_achievements_follow_chat_published_after_them() {
        let bus = test_bus();
        let mut ws = ws_connect(&bus, "?achievementEvents=1").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        for i in 0..5 {
            bus.publish_achievement(sample_unlock("u1"), bus.latest_chat_seq());
            bus.publish(sample_chat(&format!("c{i}")));
            tokio::time::sleep(Duration::from_millis(7)).await;
        }
        let frames = collect_frames(&mut ws, Duration::from_millis(300)).await;
        let first_unlock = frames.iter().position(|f| f.starts_with("{\"achievements\"")).unwrap();
        let first_chat = frames.iter().position(|f| f.starts_with('[')).unwrap();
        assert!(first_chat < first_unlock, "{frames:?}");
    }

    #[tokio::test]
    async fn ws_achievement_frames_ignore_channel_filter() {
        let bus = test_bus();
        let mut ws = ws_connect(&bus, "?achievementEvents=1&channel=only-this").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(sample_chat("other"));
        bus.publish_achievement(sample_unlock("u1"), bus.latest_chat_seq());
        let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
        // chat は channel で絞られて届かず、実績だけが届く。
        assert_eq!(frames.len(), 1, "{frames:?}");
        let value: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(value["achievements"][0]["id"], "u1");
    }

    fn sample_effect(rule_id: u32, target: crate::config::EffectTarget) -> EffectTriggered {
        EffectTriggered {
            rule_id,
            emoji: "🎉".into(),
            count: 5,
            animation: "burst".into(),
            duration_ms: 2500,
            target,
        }
    }

    #[tokio::test]
    async fn ws_without_param_receives_no_effect_frames() {
        let bus = test_bus();
        for query in ["", "?effectEvents=0", "?effectEvents=yes", "?achievementEvents=1"] {
            let mut ws = ws_connect(&bus, query).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            bus.publish_effect(sample_effect(0, crate::config::EffectTarget::Both));
            bus.publish(sample_chat("c"));
            let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
            assert_eq!(frames.len(), 1, "{query}: {frames:?}");
            assert!(frames[0].starts_with('['), "{query}: {frames:?}");
        }
    }

    #[tokio::test]
    async fn ws_with_param_receives_effects_after_unsplit_chat() {
        let bus = test_bus();
        for query in ["?effectEvents=1", "?effectEvents=true"] {
            let mut ws = ws_connect(&bus, query).await;
            tokio::time::sleep(Duration::from_millis(50)).await;
            bus.publish(sample_chat("c"));
            bus.publish(sample_chat("c"));
            bus.publish_effect(sample_effect(3, crate::config::EffectTarget::Obs));
            let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
            assert_eq!(frames.len(), 2, "{query}: {frames:?}");
            let chats: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
            assert_eq!(chats.as_array().unwrap().len(), 2, "chat batch must not be split");
            let value: serde_json::Value = serde_json::from_str(&frames[1]).unwrap();
            assert_eq!(
                value,
                serde_json::json!({"effects":[{
                    "ruleId":3,"emoji":"🎉","count":5,"animation":"burst","durationMs":2500
                }]})
            );
        }
    }

    #[tokio::test]
    async fn ws_effects_follow_chat_published_after_them() {
        let bus = test_bus();
        let mut ws = ws_connect(&bus, "?effectEvents=1").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        for i in 0..5 {
            bus.publish_effect(sample_effect(0, crate::config::EffectTarget::Both));
            bus.publish(sample_chat(&format!("c{i}")));
            tokio::time::sleep(Duration::from_millis(7)).await;
        }
        let frames = collect_frames(&mut ws, Duration::from_millis(300)).await;
        let first_effect = frames.iter().position(|f| f.starts_with("{\"effects\"")).unwrap();
        let first_chat = frames.iter().position(|f| f.starts_with('[')).unwrap();
        assert!(first_chat < first_effect, "{frames:?}");
    }

    #[tokio::test]
    async fn ws_effects_skip_app_only_rules_and_ignore_channel_filter() {
        let bus = test_bus();
        let mut ws = ws_connect(&bus, "?effectEvents=1&channel=only-this").await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        bus.publish(sample_chat("other"));
        bus.publish_effect(sample_effect(1, crate::config::EffectTarget::App));
        bus.publish_effect(sample_effect(2, crate::config::EffectTarget::Both));
        let frames = collect_frames(&mut ws, Duration::from_millis(200)).await;
        assert_eq!(frames.len(), 1, "{frames:?}");
        let value: serde_json::Value = serde_json::from_str(&frames[0]).unwrap();
        assert_eq!(value["effects"].as_array().unwrap().len(), 1);
        assert_eq!(value["effects"][0]["ruleId"], 2);
    }

    #[tokio::test]
    async fn ui_forwarder_batches_one_emit_per_frame_and_skips_obs_only() {
        let (tx, rx) = broadcast::channel(EFFECT_BROADCAST_CAPACITY);
        let cancel = CancellationToken::new();
        let emits = Arc::new(std::sync::Mutex::new(Vec::<Vec<u32>>::new()));
        let seen = emits.clone();
        let task = tokio::spawn(run_effect_forwarder(rx, cancel.clone(), move |batch| {
            seen.lock().unwrap().push(batch.iter().map(|e| e.rule_id).collect());
        }));
        tx.send(sample_effect(1, crate::config::EffectTarget::App)).unwrap();
        tx.send(sample_effect(2, crate::config::EffectTarget::Obs)).unwrap();
        tx.send(sample_effect(3, crate::config::EffectTarget::Both)).unwrap();
        tokio::time::sleep(Duration::from_millis(120)).await;
        cancel.cancel();
        task.await.unwrap();
        let emits = emits.lock().unwrap();
        assert_eq!(*emits, vec![vec![1, 3]]);
    }
}
