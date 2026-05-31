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
use std::path::PathBuf;
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
use tokio::sync::{broadcast, watch};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tower_http::cors::CorsLayer;
use tower_http::services::ServeDir;

use crate::model::ChatMessage;
use crate::stats::StatsSnapshot;

/// broadcast チャネルの容量。UI/OBS の購読者が遅れても最新が優先される。
const BROADCAST_CAPACITY: usize = 4096;
/// UI へのバッチ送出間隔(約 60fps = 16ms)。
const UI_BATCH_INTERVAL_MS: u64 = 16;
/// WS クライアントごとの送信キュー上限(溢れたら古いものから drop)。
const WS_CLIENT_QUEUE: usize = 256;

/// Bus のハンドル。正規化済みメッセージの投入と、UI/OBS への配信起動を担う。
#[derive(Clone)]
pub struct Bus {
    tx: broadcast::Sender<ChatMessage>,
    stats_tx: watch::Sender<StatsSnapshot>,
    obs_port: u16,
}

impl Bus {
    /// 新しい Bus を生成する。`obs_port` は OBS overlay サーバの待受ポート。
    pub fn new(obs_port: u16, stats_tx: watch::Sender<StatsSnapshot>) -> Self {
        let (tx, _rx) = broadcast::channel(BROADCAST_CAPACITY);
        Bus {
            tx,
            stats_tx,
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
    /// - `GET /?template=<name>`: `templates_dir/<name>/index.html` を返す。
    ///   `template` 未指定時は `default`。`../` 等のトラバーサルは拒否する。
    /// - `GET /<name>/...`: テンプレの静的アセットを配信(ServeDir)。
    ///
    /// `cancel` 発火でサーバを graceful shutdown する。
    pub fn spawn_obs_server(
        &self,
        templates_dir: PathBuf,
        cancel: CancellationToken,
    ) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
        self.spawn_obs_server_on_port(templates_dir, self.obs_port, cancel)
    }

    /// 指定ポートで OBS overlay サーバ(axum)を起動する。
    ///
    /// `update_config` によるポート変更時の再 bind で使う。
    pub fn spawn_obs_server_on_port(
        &self,
        templates_dir: PathBuf,
        port: u16,
        cancel: CancellationToken,
    ) -> Result<tauri::async_runtime::JoinHandle<()>, String> {
        let tx = self.tx.clone();
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

        let state = ObsState {
            tx: Arc::new(tx),
            stats_tx: Arc::new(self.stats_tx.clone()),
            templates_dir: Arc::new(templates_dir.clone()),
        };

        let handle = tauri::async_runtime::spawn(async move {
            let listener = match tokio::net::TcpListener::from_std(std_listener) {
                Ok(l) => l,
                Err(e) => {
                    tracing::error!("OBS サーバ listener 初期化に失敗 {addr}: {e}");
                    return;
                }
            };

            // テンプレ静的配信。ディレクトリ直アクセス時は index.html を返す。
            let serve_dir =
                ServeDir::new(&templates_dir).append_index_html_on_directories(true);

            let app = Router::new()
                .route("/ws", get(ws_handler))
                .route("/stats", get(stats_ws_handler))
                .route("/", get(template_index_handler))
                .fallback_service(serve_dir)
                .layer(CorsLayer::permissive())
                .with_state(state);

            tracing::info!(
                "OBS overlay サーバ起動: http://{addr}/  (templates: {})",
                templates_dir.display()
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
}

/// axum ハンドラ間で共有する状態。
#[derive(Clone)]
struct ObsState {
    tx: Arc<broadcast::Sender<ChatMessage>>,
    stats_tx: Arc<watch::Sender<StatsSnapshot>>,
    templates_dir: Arc<PathBuf>,
}

/// `/ws` のクエリ。`channel` 指定時は当該チャンネルのみに絞り込む(任意)。
#[derive(Debug, Deserialize)]
struct WsQuery {
    channel: Option<String>,
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

/// `GET /?template=<name>` を `templates/<name>/index.html` に解決する。
async fn template_index_handler(
    State(state): State<ObsState>,
    Query(query): Query<TemplateQuery>,
) -> impl IntoResponse {
    let template = query.template.as_deref().unwrap_or("default");
    if !is_valid_template_name(template) {
        return (
            StatusCode::BAD_REQUEST,
            "invalid template name: use only ASCII letters, digits, '-' or '_'",
        )
            .into_response();
    }

    let index_path = state.templates_dir.join(template).join("index.html");
    match tokio::fs::read_to_string(&index_path).await {
        Ok(html) => Html(inject_template_base(html, template)).into_response(),
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
fn inject_template_base(mut html: String, template: &str) -> String {
    if html.contains("<base ") || html.contains("<base>") {
        return html;
    }

    let base = format!(r#"<base href="/{template}/">"#);
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
    ws.on_upgrade(move |socket| handle_ws_client(socket, rx, query.channel))
}

/// `/stats` のアップグレードハンドラ。
async fn stats_ws_handler(
    ws: WebSocketUpgrade,
    State(state): State<ObsState>,
) -> impl IntoResponse {
    let rx = state.stats_tx.subscribe();
    ws.on_upgrade(move |socket| handle_stats_ws_client(socket, rx))
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
