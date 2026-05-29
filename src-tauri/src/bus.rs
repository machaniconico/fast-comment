//! Bus 層 — Source 群から流れてくる正規化済み `ChatMessage` を
//! UI(Tauri IPC) と OBS(axum WebSocket) の双方へ配る中継ハブ。
//!
//! 設計上の要点:
//! - 内部は `tokio::sync::broadcast`(容量上限あり)。lag は drop 容認=最新優先。
//! - UI 向けは個別 emit せず、~16ms(1フレーム)単位で配列にまとめて `emit("chat", batch)`。
//! - OBS 向けは axum の `/ws` で push、`/` 配下で `templates/<name>/` を静的配信。
//! - クライアント(WS)ごとに bounded queue を持ち、溢れたら古いものから捨てる。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::{
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    response::IntoResponse,
    routing::get,
    Router,
};
use serde::Deserialize;
use tauri::{AppHandle, Emitter};
use tokio::sync::broadcast;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::model::ChatMessage;

/// broadcast チャネルの容量。UI/OBS の購読者が遅れても最新が優先される。
const BROADCAST_CAPACITY: usize = 4096;
/// UI へのバッチ送出間隔(約 60fps = 16ms)。
const UI_BATCH_INTERVAL_MS: u64 = 16;
/// WS クライアントごとの送信キュー上限(溢れたら古いものから drop)。
const WS_CLIENT_QUEUE: usize = 256;

/// Bus のハンドル。Source からの送信と、UI/OBS への配信起動を担う。
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<ChatMessage>,
    obs_port: u16,
}

impl Bus {
    /// 新しい Bus を生成する。`obs_port` は OBS overlay サーバの待受ポート。
    pub fn new(obs_port: u16) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Bus { tx, obs_port }
    }

    /// Source 層が正規化済みメッセージを投入するための送信端。
    pub fn sender(&self) -> broadcast::Sender<ChatMessage> {
        self.tx.clone()
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

    /// UI 向けバッチ送出ループを起動する。
    ///
    /// broadcast を購読し、~16ms ごとに溜まったメッセージを配列で `emit("chat", batch)`。
    /// `cancel` 発火で終了。
    pub fn spawn_ui_forwarder(&self, app: AppHandle, cancel: CancellationToken) {
        let mut rx = self.tx.subscribe();
        tokio::spawn(async move {
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
    /// - `GET /` 以下: `templates_dir/<...>` を静的配信(ServeDir)。
    ///
    /// `cancel` 発火でサーバを graceful shutdown する。
    pub fn spawn_obs_server(
        &self,
        templates_dir: PathBuf,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        let tx = self.tx.clone();
        let port = self.obs_port;

        let state = ObsState { tx: Arc::new(tx) };

        tokio::spawn(async move {
            // テンプレ静的配信。ディレクトリ直アクセス時は index.html を返す。
            let serve_dir =
                ServeDir::new(&templates_dir).append_index_html_on_directories(true);

            let app = Router::new()
                .route("/ws", get(ws_handler))
                .fallback_service(serve_dir)
                .layer(CorsLayer::permissive())
                .with_state(state);

            let addr = SocketAddr::from(([127, 0, 0, 1], port));
            let listener = match tokio::net::TcpListener::bind(addr).await {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("OBS サーバの bind に失敗 {addr}: {e}");
                    return;
                }
            };
            tracing::info!("OBS overlay サーバ起動: http://{addr}/  (templates: {})", templates_dir.display());

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
        })
    }
}

/// axum ハンドラ間で共有する状態。
#[derive(Clone)]
struct ObsState {
    tx: Arc<broadcast::Sender<ChatMessage>>,
}

/// `/ws` のクエリ。`channel` 指定時は当該チャンネルのみに絞り込む(任意)。
#[derive(Debug, Deserialize)]
struct WsQuery {
    channel: Option<String>,
}

/// `/ws` のアップグレードハンドラ。
async fn ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
    Query(query): Query<WsQuery>,
) -> impl IntoResponse {
    let rx = state.tx.subscribe();
    ws.on_upgrade(move |socket| handle_ws_client(socket, rx, query.channel))
}

/// 1 WS クライアントの送信ループ。
///
/// broadcast を購読し、UI forwarder と同形の ~16ms バッチ(JSON 配列)で push する。
/// 受信(rx.recv)と送信(tick での flush)を分離し、遅いクライアントの socket.send が
/// 受信をブロックしないようにする。受信は per-client の bounded queue を埋め続け、
/// 満杯なら古いものから drop(最新優先)。`channel` 指定時は一致しないメッセージを捨てる。
async fn handle_ws_client(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<ChatMessage>,
    channel: Option<String>,
) {
    let mut ticker = interval(Duration::from_millis(UI_BATCH_INTERVAL_MS));
    // クライアント個別の bounded queue。溢れたら古いものから drop(最新優先)。
    let mut queue: std::collections::VecDeque<ChatMessage> = std::collections::VecDeque::new();

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
            // ~16ms ごとに溜まった分を1つの JSON 配列としてまとめて送る。
            _ = ticker.tick() => {
                if queue.is_empty() {
                    continue; // 空 tick では送らない
                }
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
