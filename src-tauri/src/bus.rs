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
use std::sync::Arc;
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
use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::model::{ChatMessage, ModerationEvent, YoutubeReaction};
use crate::stats::{StatsSnapshot, TimerSnapshot};
use crate::templates::{TemplateRef, TemplateStore};

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

/// Bus のハンドル。正規化済みメッセージの投入と、UI/OBS への配信起動を担う。
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<ChatMessage>,
    /// OBS `/ws?modEvents=1` へ流す削除・BAN・ローカル非表示(配信側・アプリ発の両方)。
    mod_tx: broadcast::Sender<ModerationEvent>,
    stats_tx: watch::Sender<StatsSnapshot>,
    timer_tx: watch::Sender<TimerSnapshot>,
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
        Bus {
            tx,
            mod_tx,
            stats_tx,
            timer_tx,
            obs_port,
        }
    }

    /// 新規購読者を得る。
    pub fn subscribe(&self) -> broadcast::Receiver<ChatMessage> {
        self.tx.subscribe()
    }

    /// 1メッセージを Bus に流す(Tauri コマンド経由のシステムメッセージ等にも使う)。
    /// 購読者が居なくてもエラーにしない。
    pub fn publish(&self, msg: ChatMessage) {
        let _ = self.tx.send(msg);
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
                            Ok(msg) => batch.push(msg),
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

        let app = self.obs_app(
            templates_dir.clone(),
            custom_templates_dir.clone(),
            goals_skin_dir.clone(),
            extra,
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
        let app = obs_router(
            self.obs_state(TemplateStore::new(templates_dir, custom_templates_dir)),
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

    fn obs_state(&self, templates: TemplateStore) -> ObsState {
        ObsState {
            tx: Arc::new(self.tx.clone()),
            mod_tx: Arc::new(self.mod_tx.clone()),
            stats_tx: Arc::new(self.stats_tx.clone()),
            timer_tx: Arc::new(self.timer_tx.clone()),
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
        .route("/", get(template_index_handler))
        .nest_service("/skin", skin_dir)
        .fallback_service(serve_dir)
        .layer(CorsLayer::permissive())
        .with_state(state)
}

/// axum ハンドラ間で共有する状態。
#[derive(Clone)]
struct ObsState {
    tx: Arc<broadcast::Sender<ChatMessage>>,
    mod_tx: Arc<broadcast::Sender<ModerationEvent>>,
    stats_tx: Arc<watch::Sender<StatsSnapshot>>,
    timer_tx: Arc<watch::Sender<TimerSnapshot>>,
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
    ws.on_upgrade(move |socket| handle_ws_client(socket, rx, mod_rx, query.channel))
}

/// `/stats` のアップグレードハンドラ。
async fn stats_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.stats_tx.subscribe();
    ws.on_upgrade(move |socket| handle_stats_ws_client(socket, rx))
}

/// `/timer` のアップグレードハンドラ。
async fn timer_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.timer_tx.subscribe();
    ws.on_upgrade(move |socket| handle_timer_ws_client(socket, rx))
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
async fn handle_ws_client(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ChatMessage>,
    mut mod_rx: Option<broadcast::Receiver<ModerationEvent>>,
    channel: Option<String>,
) {
    let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
    // クライアント個別の bounded queue。溢れたら古いものから drop(最新優先)。
    let mut queue: std::collections::VecDeque<ChatMessage> = std::collections::VecDeque::new();
    let mut mod_queue: std::collections::VecDeque<ModerationEvent> =
        std::collections::VecDeque::new();

    loop {
        tokio::select! {
            // broadcast からの受信 → channel フィルタ後キューへ。受信は止めない。
            recv = rx.recv() => {
                match recv {
                    Ok(msg) => {
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

/// `/timer` WS クライアントの送信ループ。
///
/// 接続直後に現在値を1回送り、以後 watch の変更ごとに JSON オブジェクトを push する。
async fn handle_timer_ws_client(
    mut socket: WebSocket,
    mut rx: watch::Receiver<TimerSnapshot>,
) {
    let initial = rx.borrow().clone();
    if send_timer_snapshot(&mut socket, &initial).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            changed = rx.changed() => {
                if changed.is_err() {
                    break;
                }
                let snapshot = rx.borrow_and_update().clone();
                if send_timer_snapshot(&mut socket, &snapshot).await.is_err() {
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

async fn send_timer_snapshot(
    socket: &mut WebSocket,
    snapshot: &TimerSnapshot,
) -> Result<(), axum::Error> {
    match serde_json::to_string(snapshot) {
        Ok(json) => socket.send(WsMessage::Text(json.into())).await,
        Err(e) => {
            tracing::warn!("timer WS 向け JSON 化失敗: {e}");
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
        let bus = Bus::new(0, stats_tx, timer_tx);
        let state = bus.obs_state(TemplateStore::new(builtin.into(), custom.into()));
        let app = obs_router(state, &builtin.join("no-skin"));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        format!("http://{addr}")
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
}
