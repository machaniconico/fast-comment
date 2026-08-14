//! X (Twitter) ライブ配信 (broadcasts) チャット Source。
//!
//! ログイン不要のゲストトークンフローで Periscope 由来 chatapi の WebSocket に接続する:
//! 1. POST guest/activate.json (公開 Bearer) → guest_token
//! 2. GET broadcasts/show.json?ids={id} → media_key
//! 3. GET live_video_stream/status/{media_key} → chatToken
//! 4. POST accessChatPublic (chat_token) → endpoint + access_token
//! 5. {endpoint}/chatapi/v1/chatnow (wss) へ接続し kind:3(認証) → kind:2(参加) を送信
//! 6. {endpoint}/chatapi/v1/history を cursor 保持で定期ポーリングしチャット本文を取得
//!
//! 現行の chatman ancillary クラスタは chatnow WS には presence(視聴者数、
//! 外側 kind:2 / 内側 kind:4)しか流さず、チャット本文は WS では届かない(実測:
//! ゲストでもログイン済みトークンでも occupancy のみ)。本文は history エンドポイントの
//! POST ポーリングで得る。history の各メッセージは外側 kind:2 のエンベロープで、
//! 内側 payload が WS チャット(kind:1)相当のため `{kind:1, payload}` に組み替えて
//! WS と共通の正規化経路へ流す。
//!
//! チャットフレームの payload は二重 JSON ネスト(payload 文字列 → その中の body
//! 文字列を再パース)。仕様変更に強いよう固い struct デシリアライズはせず
//! `serde_json::Value` のパス探索で欠落を None に劣化させる(SPEC の YouTube
//! パースと同方針)。URL/Bearer は `XOverrides` で再ビルド無しに上書きできる。

use std::time::{Duration, Instant};

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::config::XOverrides;
use crate::model::{Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use crate::stats::YoutubeMetadataUpdate;

/// X Web クライアントに埋め込まれている公開 Bearer(シークレットではない)。
const X_WEB_BEARER: &str = "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs=1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const GUEST_ACTIVATE_URL: &str = "https://api.x.com/1.1/guest/activate.json";
const BROADCAST_SHOW_URL: &str = "https://x.com/i/api/1.1/broadcasts/show.json";
const LIVE_STATUS_URL: &str = "https://x.com/i/api/1.1/live_video_stream/status";
const ACCESS_CHAT_URL: &str = "https://proxsee-cf.pscp.tv/api/v2/accessChatPublic";
/// ブラウザ相当の UA。X の Web API はブラウザ以外の UA を弾くことがある。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
/// HTTP bootstrap 各リクエストの全体タイムアウト。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// WebSocket ハンドシェイクのタイムアウト。
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// この時間フレームを受信しなければ Ping で生存確認する。
/// chatapi はサーバ側 heartbeat が保証されず、TCP half-open だと
/// `read.next()` が永久に返らないため、クライアント側で監視する。
const IDLE_PING_AFTER: Duration = Duration::from_secs(45);
/// Ping 送信後この時間フレームが無ければ half-open とみなし再接続する。
const IDLE_PONG_WAIT: Duration = Duration::from_secs(15);
/// この間隔で受信状況を info ログへ出す。Ping/Pong だけで生存し続ける
/// 「チャット0件セッション」は切断されず SessionStats の Drop サマリも
/// 出ないため、生きている間の証拠はこのハートビートだけになる。
/// 短時間のテスト実行でも1回は出るよう 30 秒にしている。
const HEARTBEAT_LOG_EVERY: Duration = Duration::from_secs(30);
/// チャット本文取得のための history ポーリング間隔。低遅延重視だが、
/// サーバ負荷と cursor 差分の粒度を考え 1.5 秒。初回 tick は即時に発火する。
const HISTORY_POLL_INTERVAL: Duration = Duration::from_millis(1500);
/// dedup で保持する直近チャット ID の上限(メモリ上限)。超過分は古い順に捨てる。
const DEDUP_CAPACITY: usize = 4096;

/// URL または生 ID から broadcast ID を取り出す。
///
/// `https://x.com/i/broadcasts/{id}` / `https://twitter.com/i/broadcasts/{id}`
/// 形式のほか、英数字のみの入力はそのまま ID として扱う。
pub fn extract_broadcast_id(input: &str) -> String {
    let s = input.trim();
    if let Some(pos) = s.find("/i/broadcasts/") {
        let rest = &s[pos + "/i/broadcasts/".len()..];
        let id: String = rest
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .collect();
        if !id.is_empty() {
            return id;
        }
    }
    // URL でなければクエリ/フラグメント/末尾スラッシュだけ落として ID とみなす。
    let core = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    core.rsplit('/').next().unwrap_or(core).to_string()
}

/// チャット接続に必要な bootstrap 結果。
struct ChatBootstrap {
    endpoint: String,
    access_token: String,
    room_id: String,
    /// 配信者の Twitter user_id(broadcaster ロール判定用)。
    broadcaster_id: Option<String>,
    /// 配信タイトル(チップ表示用)。
    title: Option<String>,
}

#[derive(Debug)]
struct XSessionError {
    error: anyhow::Error,
    stable: bool,
}

impl XSessionError {
    fn new<E>(error: E, stable: bool) -> Self
    where
        E: Into<anyhow::Error>,
    {
        XSessionError {
            error: error.into(),
            stable,
        }
    }
}

/// X ライブ配信1件のチャットを購読する Source。
pub struct XSource {
    /// 正規化済み broadcast ID。
    broadcast_id: String,
    overrides: XOverrides,
    /// チップの接続状態表示用。live フラグとタイトルのみ送る。
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl XSource {
    pub fn new(
        identifier: String,
        overrides: XOverrides,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        XSource {
            broadcast_id: extract_broadcast_id(&identifier),
            overrides,
            metadata_tx,
        }
    }

    fn bearer(&self) -> String {
        self.overrides
            .bearer_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| X_WEB_BEARER.to_string())
    }

    /// overrides の endpoints にキーがあればそれを、無ければ既定 URL を使う。
    fn endpoint_url(&self, key: &str, default: &str) -> String {
        self.overrides
            .endpoints
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| default.to_string())
    }
}

impl Source for XSource {
    fn name(&self) -> String {
        format!("x:{}", self.broadcast_id)
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let mut backoff = Backoff::new();
            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                match self.connect_and_listen(&tx, &cancel).await {
                    Ok(stable) => {
                        if cancel.is_cancelled() {
                            return Ok(());
                        }
                        self.send_live_state(false, None).await;
                        if stable {
                            backoff.reset();
                        }
                    }
                    Err(e) => {
                        self.send_live_state(false, None).await;
                        if e.stable {
                            backoff.reset();
                        }
                        tracing::warn!("x:{} 接続エラー: {:#}", self.broadcast_id, e.error);
                    }
                }

                let delay = backoff.next_delay();
                tracing::info!(
                    "x:{} {}ms 後に再接続",
                    self.broadcast_id,
                    delay.as_millis()
                );
                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }
}

impl XSource {
    /// 1接続分のセッション。HTTP bootstrap → WS 接続 → 認証/参加 → 受信ループ。
    ///
    /// 戻り値は Twitch と同じ「安定セッション」判定(30秒以上 or データ受信あり)。
    async fn connect_and_listen(
        &self,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> Result<bool, XSessionError> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .timeout(HTTP_TIMEOUT)
            .build()
            .map_err(|e| XSessionError::new(e, false))?;

        // HTTP フローは配信未開始/終了でも失敗するため、失敗は不安定扱いで
        // バックオフを伸ばし続ける(=配信開始待ちのポーリングを兼ねる)。
        // チャンネル削除/設定変更の cancel は bootstrap 中も効かせる。
        let boot = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = self.bootstrap(&client) => r.map_err(|e| XSessionError::new(e, false))?,
        };

        let ws_url = format!(
            "{}/chatapi/v1/chatnow",
            boot.endpoint
                .replace("https://", "wss://")
                .replace("http://", "ws://")
        );
        let mut req = ws_url
            .clone()
            .into_client_request()
            .map_err(|e| XSessionError::new(e, false))?;
        req.headers_mut().insert(
            "Origin",
            "https://x.com".parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    XSessionError::new(e, false)
                },
            )?,
        );
        // reqwest 側の UA は WS ハンドシェイクには乗らない。ブラウザ実接続との
        // 差異を減らすため明示する。
        req.headers_mut().insert(
            "User-Agent",
            USER_AGENT.parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    XSessionError::new(e, false)
                },
            )?,
        );
        let (ws_stream, resp) = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = tokio::time::timeout(
                WS_CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async(req),
            ) => r
                .map_err(|e| XSessionError::new(e, false))?
                .map_err(|e| XSessionError::new(e, false))?,
        };
        // サーバーが受理したサブプロトコル/拡張は 101 応答にしか現れない。
        // auth/join 黙殺の切り分け材料としてヘッダを残す(トークン類は含まれない)。
        tracing::info!(
            "x:{} WS handshake status={} protocol={:?} extensions={:?} server={:?}",
            self.broadcast_id,
            resp.status(),
            resp.headers().get("sec-websocket-protocol"),
            resp.headers().get("sec-websocket-extensions"),
            resp.headers().get("server"),
        );
        let (mut write, mut read) = ws_stream.split();

        // 統計はハンドシェイク直後から取り、auth/join 送信区間も含める。
        // Drop で必ずサマリを info ログに残す。
        let mut stats = SessionStats::new(&self.broadcast_id);

        // kind:3 = 認証、kind:2 = ルーム参加。payload は文字列化 JSON の入れ子。
        let auth = json!({
            "payload": json!({ "access_token": boot.access_token }).to_string(),
            "kind": 3,
        });
        write
            .send(ws_text(&auth.to_string()))
            .await
            .map_err(|e| XSessionError::new(e, false))?;
        let join = json!({
            "payload": json!({
                "body": json!({ "room": boot.room_id }).to_string(),
                "kind": 1,
            })
            .to_string(),
            "kind": 2,
        });
        write
            .send(ws_text(&join.to_string()))
            .await
            .map_err(|e| XSessionError::new(e, false))?;

        // send 成功はローカル write buffer に載っただけで、サーバーが auth/join を
        // 受理した証拠にはならない(受理 ACK の仕様は未確認)。
        tracing::info!(
            "x:{} chatnow 接続、auth/join 送信完了(受理は未確認)",
            self.broadcast_id
        );
        // チップに「接続中」を出す。X には視聴者数の取得手段が無いため live のみ。
        self.send_live_state(true, boot.title.clone()).await;

        // 「安定」判定はチャット実受信 or 30秒以上継続(SessionStats::is_stable)。
        // Ping/認証エラー通知等の雑フレーム「だけ」で即安定扱いにすると、
        // 接続→即切断の反復で毎回バックオフが1秒へ戻り、実質リトライ間隔が
        // 伸びなくなるため、フレーム受信そのものは安定条件にしない。
        let mut last_activity = tokio::time::Instant::now();
        let mut ping_sent_at: Option<tokio::time::Instant> = None;
        let mut heartbeat = tokio::time::interval_at(
            tokio::time::Instant::now() + HEARTBEAT_LOG_EVERY,
            HEARTBEAT_LOG_EVERY,
        );
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // チャット本文は chatnow WS ではなく history ポーリングで取得する
        // (現行クラスタは WS に presence しか流さない)。history URL は
        // accessChatPublic の endpoint 由来。配信終了で replay クラスタへ
        // 切り替わる個体にも endpoint がそのまま追随する。overrides で差し替え可(テスト用)。
        let history_url = self
            .overrides
            .endpoints
            .get("historyUrl")
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("{}/chatapi/v1/history", boot.endpoint));
        let mut history_cursor = String::new();
        let mut seen = ChatDedup::new(DEDUP_CAPACITY);
        let mut history_poll = tokio::time::interval(HISTORY_POLL_INTERVAL);
        history_poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            let idle_deadline = match ping_sent_at {
                Some(at) => at + IDLE_PONG_WAIT,
                None => last_activity + IDLE_PING_AFTER,
            };
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(WsMessage::Close(None)).await;
                    return Ok(true);
                }
                _ = heartbeat.tick() => {
                    stats.log_progress();
                }
                _ = history_poll.tick() => {
                    self.poll_history(
                        &client,
                        &history_url,
                        &boot.access_token,
                        &mut history_cursor,
                        &mut seen,
                        boot.broadcaster_id.as_deref(),
                        tx,
                        &mut stats,
                    )
                    .await;
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    if ping_sent_at.is_some() {
                        // Ping 後も無応答: half-open とみなして再接続へ。
                        return Err(XSessionError::new(
                            anyhow::anyhow!("アイドルタイムアウト(Ping 応答なし)"),
                            stats.is_stable(),
                        ));
                    }
                    write.send(WsMessage::Ping(Vec::new().into())).await.map_err(|e| {
                        XSessionError::new(e, stats.is_stable())
                    })?;
                    ping_sent_at = Some(tokio::time::Instant::now());
                }
                msg = read.next() => {
                    // Pong 含む何らかのフレームが来れば接続は生きている。
                    last_activity = tokio::time::Instant::now();
                    ping_sent_at = None;
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            return Err(XSessionError::new(e, stats.is_stable()));
                        }
                        None => {
                            // Close フレーム無しの TCP 切断。Close 経由の終了と
                            // 区別できないと接続断の分類ができない。
                            tracing::info!(
                                "x:{} read ストリーム終了(Close フレームなし)",
                                self.broadcast_id
                            );
                            return Ok(stats.is_stable());
                        }
                    };

                    match msg {
                        WsMessage::Text(text) => {
                            stats.rx_text += 1;
                            self.handle_frame(
                                &text,
                                boot.broadcaster_id.as_deref(),
                                tx,
                                &mut stats,
                                &mut seen,
                            );
                        }
                        // chatman が opcode を変えても取りこぼさないよう Binary も同経路。
                        WsMessage::Binary(data) => {
                            stats.rx_binary += 1;
                            match std::str::from_utf8(&data) {
                                Ok(text) => {
                                    self.handle_frame(
                                        text,
                                        boot.broadcaster_id.as_deref(),
                                        tx,
                                        &mut stats,
                                        &mut seen,
                                    );
                                }
                                Err(_) => {
                                    stats.rx_binary_invalid_utf8 += 1;
                                    tracing::debug!(
                                        "x:{} UTF-8 でない binary フレーム {}B",
                                        self.broadcast_id,
                                        data.len()
                                    );
                                }
                            }
                        }
                        WsMessage::Ping(payload) => {
                            // tungstenite にも自動 Pong 応答があり二重になりうるが、
                            // 自動側は flush タイミングが read 駆動に依存するため、
                            // 「必ず返す」保証としてここでも明示送信する(重複 Pong は
                            // RFC 6455 上無害)。
                            stats.rx_ping += 1;
                            write.send(WsMessage::Pong(payload)).await.map_err(|e| {
                                XSessionError::new(e, stats.is_stable())
                            })?;
                        }
                        WsMessage::Pong(_) => {
                            stats.rx_pong += 1;
                        }
                        WsMessage::Close(frame) => {
                            // 認証/参加の拒否はエラーフレームではなく Close で現れる
                            // ことが多い。code/reason を必ず残す。
                            tracing::info!(
                                "x:{} サーバー Close: {:?}",
                                self.broadcast_id,
                                frame
                            );
                            return Ok(stats.is_stable());
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// HTTP 4段フローでチャット接続情報を得る。
    async fn bootstrap(&self, client: &reqwest::Client) -> anyhow::Result<ChatBootstrap> {
        let bearer = self.bearer();

        // 1. ゲストトークン。
        let v: Value = client
            .post(self.endpoint_url("guestActivateUrl", GUEST_ACTIVATE_URL))
            .header("authorization", format!("Bearer {bearer}"))
            .header("origin", "https://x.com")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let guest_token = v
            .get("guest_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("guest_token が取得できない"))?
            .to_string();

        // 2. broadcast メタデータ → media_key。
        // ID は外部入力なので手組み連結せず .query() でエンコードさせる。
        let v: Value = client
            .get(self.endpoint_url("broadcastShowUrl", BROADCAST_SHOW_URL))
            .query(&[("ids", self.broadcast_id.as_str())])
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-guest-token", &guest_token)
            .header("origin", "https://x.com")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let broadcast = v
            .pointer(&format!("/broadcasts/{}", self.broadcast_id))
            .cloned()
            .unwrap_or(Value::Null);
        let media_key = broadcast
            .get("media_key")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                anyhow::anyhow!("media_key が取得できない(配信が未開始/終了済みの可能性)")
            })?
            .to_string();
        // ENDED/TIMED_OUT でも media_key/chatToken は取得できてしまい、chatnow の
        // WS ハンドシェイクで初めて失敗する(実測: 非101で拒否)。ここで止めて
        // 原因をログに明示する。ループ側の再接続は継続するので、同じ枠で
        // 再配信が始まれば自動で拾える。
        let state = broadcast.get("state").and_then(Value::as_str).unwrap_or("");
        if matches!(state, "ENDED" | "TIMED_OUT") {
            anyhow::bail!("配信が終了済み (state={state})");
        }
        if !state.is_empty() && state != "RUNNING" {
            tracing::warn!("x:{} 想定外の配信状態 state={}", self.broadcast_id, state);
        }
        let broadcaster_id = id_string(broadcast.get("user_id"));
        // Periscope 由来の "status" が配信タイトル。将来の変化に備え "title" も見る。
        let title = broadcast
            .get("status")
            .or_else(|| broadcast.get("title"))
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        // 3. ライブストリーム状態 → chatToken。media_key は API 応答由来。
        let url = format!(
            "{}/{}",
            self.endpoint_url("liveStatusUrl", LIVE_STATUS_URL),
            media_key
        );
        let v: Value = client
            .get(&url)
            .query(&[
                ("client", "web"),
                ("use_syndication_guest_id", "false"),
                ("cookie_set_host", "x.com"),
            ])
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-guest-token", &guest_token)
            .header("origin", "https://x.com")
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let chat_token = v
            .get("chatToken")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("chatToken が取得できない"))?
            .to_string();

        // 4. チャットアクセストークン交換。
        let v: Value = client
            .post(self.endpoint_url("accessChatUrl", ACCESS_CHAT_URL))
            .header("content-type", "application/json")
            .header("x-periscope-user-agent", "Twitter/m5")
            .header("x-attempt", "1")
            .header("origin", "https://x.com")
            .json(&json!({ "chat_token": chat_token }))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let endpoint = v
            .get("endpoint")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("chat endpoint が取得できない"))?
            .to_string();
        let access_token = v
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("chat access_token が取得できない"))?
            .to_string();
        // room_id が無い応答もあるため broadcast ID にフォールバックする。
        // 数値で返る個体にも耐える(id_string)。
        let room_id =
            id_string(v.get("room_id")).unwrap_or_else(|| self.broadcast_id.clone());

        Ok(ChatBootstrap {
            endpoint,
            access_token,
            room_id,
            broadcaster_id,
            title,
        })
    }

    /// チップの接続状態表示用に live 状態を stats へ送る。
    /// `full_snapshot: false` の部分更新なので、切断時も直前のタイトルは
    /// stats 側の live=false 処理に従って整理される。
    async fn send_live_state(&self, live: bool, title: Option<String>) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::X,
            channel: self.broadcast_id.clone(),
            concurrent_viewers: None,
            likes: None,
            title,
            live: Some(live),
            reactions_delta: None,
            full_snapshot: false,
        };
        let _ = tx.send(update).await;
    }

    /// 受信フレーム1件を処理する。チャットなら emit、そうでなければ診断用に
    /// ダンプする(接続直後の数フレームは info、以降は debug)。
    fn handle_frame(
        &self,
        text: &str,
        broadcaster_id: Option<&str>,
        tx: &broadcast::Sender<ChatMessage>,
        stats: &mut SessionStats,
        seen: &mut ChatDedup,
    ) {
        stats.frames_data += 1;
        if let Some(chat) = self.frame_to_chat(text, broadcaster_id) {
            // WS 経由と history 経由、および history の cursor 境界で同じメッセージが
            // 二重に来る。確定した ID で dedup し、UI への重複配送を防ぐ。
            if !seen.insert(&chat.id) {
                stats.frames_dup += 1;
                return;
            }
            stats.frames_chat += 1;
            if stats.frames_chat == 1 {
                tracing::info!("x:{} 初チャット受信", self.broadcast_id);
            }
            // 購読者ゼロだと send は Err になる。無言のままだと
            // 「初チャット受信は出るのに UI に届かない」を切り分けられない。
            if tx.send(chat).is_err() {
                stats.tx_send_fail += 1;
                if stats.tx_send_fail == 1 {
                    tracing::warn!(
                        "x:{} tx.send 失敗(購読者なし) — パイプライン未接続の疑い",
                        self.broadcast_id
                    );
                }
            }
            return;
        }
        let reason = classify_non_chat(text);
        *stats.non_chat.entry(reason).or_insert(0) += 1;
        if stats.frames_data <= 5 {
            // RUST_LOG 未指定(既定 info)でも最初の数フレームは中身を確認できる
            // ようにする。チャットが来ない時の一次証拠。
            tracing::info!(
                "x:{} 非チャットフレーム({}): {}",
                self.broadcast_id,
                reason,
                truncate_chars(text, 300)
            );
        } else {
            tracing::debug!(
                "x:{} 非チャットフレーム({}): {}",
                self.broadcast_id,
                reason,
                truncate_chars(text, 300)
            );
        }
    }

    /// 受信フレーム1件を `ChatMessage` へ正規化する。チャット以外(kind!=1、
    /// ハート/参加通知等)は None。
    fn frame_to_chat(&self, text: &str, broadcaster_id: Option<&str>) -> Option<ChatMessage> {
        let outer: Value = serde_json::from_str(text).ok()?;
        if outer.get("kind").and_then(Value::as_i64) != Some(1) {
            return None;
        }
        // payload → body の二重ネスト(いずれも文字列化 JSON)。
        let payload: Value =
            serde_json::from_str(outer.get("payload").and_then(Value::as_str)?).ok()?;
        let body: Value =
            serde_json::from_str(payload.get("body").and_then(Value::as_str)?).ok()?;

        // body.type: 1 がチャット本文。ハート(2)/参加(3)等は捨てる。
        // type 欠落は本文の有無の判定に倒す(仕様変化への寛容)。
        match body.get("type").and_then(Value::as_i64) {
            Some(1) | None => {}
            Some(_) => return None,
        }
        let text_body = body.get("body").and_then(Value::as_str).unwrap_or("");
        if text_body.trim().is_empty() {
            return None;
        }

        let sender = payload.get("sender").cloned().unwrap_or(Value::Null);
        let username = body
            .get("username")
            .and_then(Value::as_str)
            .or_else(|| sender.get("username").and_then(Value::as_str))
            .unwrap_or("")
            .to_string();
        let display_name = body
            .get("displayName")
            .and_then(Value::as_str)
            .or_else(|| sender.get("display_name").and_then(Value::as_str))
            .map(str::to_string)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| username.clone());
        // remoteID が Twitter user_id。欠落フレームでは sender 側の
        // twitter_id / id / user_id(Periscope) を順に拾う。数値で来る個体も
        // あるため文字列/数値の両方を受ける。
        let user_id = id_string(body.get("remoteID"))
            .or_else(|| id_string(sender.get("twitter_id")))
            .or_else(|| id_string(sender.get("id")))
            .or_else(|| id_string(sender.get("user_id")))
            .unwrap_or_else(|| username.clone());
        if user_id.is_empty() && display_name.is_empty() {
            return None;
        }

        let roles = Roles {
            broadcaster: broadcaster_id.is_some_and(|b| b == user_id),
            ..Roles::default()
        };

        let timestamp_ms = body
            .get("timestamp")
            .and_then(Value::as_i64)
            .or_else(|| payload.get("timestamp").and_then(Value::as_i64))
            .map(normalize_epoch_ms)
            .unwrap_or_else(now_ms);

        let id = body
            .get("uuid")
            .and_then(Value::as_str)
            .or_else(|| payload.get("uuid").and_then(Value::as_str))
            .map(str::to_string)
            .unwrap_or_else(ChatMessage::new_id);

        Some(ChatMessage {
            id,
            platform: Platform::X,
            channel: self.broadcast_id.clone(),
            author: Author {
                id: user_id,
                name: display_name,
                display_color: None,
                badges: Vec::new(),
                roles,
            },
            fragments: vec![Fragment::text(text_body.to_string())],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms,
            raw: None,
            skip_tts: false,
        })
    }

    /// history エンドポイントを1回ポーリングし、得たチャット本文を共通経路へ流す。
    /// 失敗は握りつぶして次回に委ねる(WS 側が生存監視・再接続を担うため、history の
    /// 一時失敗ではセッションを落とさない)。cursor は呼び出し側が保持する。
    #[allow(clippy::too_many_arguments)]
    async fn poll_history(
        &self,
        client: &reqwest::Client,
        url: &str,
        access_token: &str,
        cursor: &mut String,
        seen: &mut ChatDedup,
        broadcaster_id: Option<&str>,
        tx: &broadcast::Sender<ChatMessage>,
        stats: &mut SessionStats,
    ) {
        let req_body = json!({
            "access_token": access_token,
            "cursor": cursor.as_str(),
            "limit": 100,
        });
        let resp = match client
            .post(url)
            .header("content-type", "application/json")
            .json(&req_body)
            .send()
            .await
            .and_then(reqwest::Response::error_for_status)
        {
            Ok(r) => r,
            Err(e) => {
                stats.history_errors += 1;
                // 初回だけ warn、以降は debug(復配信待ちで恒常的に失敗する枠もある)。
                if stats.history_errors == 1 {
                    tracing::warn!("x:{} history ポーリング失敗: {:#}", self.broadcast_id, e);
                } else {
                    tracing::debug!("x:{} history ポーリング失敗: {:#}", self.broadcast_id, e);
                }
                return;
            }
        };
        let v: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                stats.history_errors += 1;
                tracing::debug!("x:{} history JSON パース失敗: {:#}", self.broadcast_id, e);
                return;
            }
        };
        stats.history_polls += 1;

        if let Some(msgs) = v.get("messages").and_then(Value::as_array) {
            stats.history_msgs += msgs.len() as u64;
            for m in msgs {
                // history メッセージは外側 kind:2 のエンベロープで、内側 payload が
                // WS チャット(kind:1)相当。{kind:1, payload} に組み替えて handle_frame の
                // 共通正規化・dedup 経路へ流す。payload が文字列でない個体は frame_to_chat
                // 側で None に劣化する(寛容パース)。
                let Some(payload) = m.get("payload") else {
                    continue;
                };
                let framed = json!({ "kind": 1, "payload": payload }).to_string();
                self.handle_frame(&framed, broadcaster_id, tx, stats, seen);
            }
        }
        // cursor は前方カーソル(次回はこれ以降の新着のみを返す)。空文字は据え置く。
        if let Some(c) = v.get("cursor").and_then(Value::as_str) {
            if !c.is_empty() {
                *cursor = c.to_string();
            }
        }
    }
}

fn ws_text(s: &str) -> WsMessage {
    WsMessage::Text(s.to_string().into())
}

/// ID フィールドを寛容に文字列化する(文字列/数値の両方を受ける)。
fn id_string(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::String(s) => {
            let t = s.trim();
            (!t.is_empty()).then(|| t.to_string())
        }
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// 直近チャット ID の FIFO 集合。history ポーリングは cursor 境界で同じ
/// メッセージを再度返すことがあり、また WS 経由と history 経由が同一メッセージを
/// 出すこともあるため、既出 ID を弾いて二重表示を防ぐ。容量超過分は古い順に捨てる。
struct ChatDedup {
    set: std::collections::HashSet<String>,
    order: std::collections::VecDeque<String>,
    capacity: usize,
}

impl ChatDedup {
    fn new(capacity: usize) -> Self {
        ChatDedup {
            set: std::collections::HashSet::new(),
            order: std::collections::VecDeque::new(),
            capacity,
        }
    }

    /// 新規 ID なら登録して true、既出なら false を返す。
    fn insert(&mut self, id: &str) -> bool {
        if self.set.contains(id) {
            return false;
        }
        self.set.insert(id.to_string());
        self.order.push_back(id.to_string());
        if self.order.len() > self.capacity {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        true
    }
}

/// 1 WS セッションの受信統計。Drop で必ずサマリを info ログへ残し、
/// 「接続はできるがチャットが来ない」を現地で診断可能にする。
/// opcode 別のカウンタは容疑の切り分け(Binary 配送 / Pong だけの生存 /
/// 非チャット判定の内訳)に使う。
struct SessionStats {
    broadcast_id: String,
    started: Instant,
    /// チャット判定まで進んだデータフレーム数(Text + UTF-8 な Binary)。
    frames_data: u64,
    frames_chat: u64,
    rx_text: u64,
    rx_binary: u64,
    rx_binary_invalid_utf8: u64,
    rx_ping: u64,
    rx_pong: u64,
    /// broadcast channel への送信失敗数(購読者ゼロ)。
    tx_send_fail: u64,
    /// dedup で既出のため捨てたチャット数(history の cursor 境界重複など)。
    frames_dup: u64,
    /// history ポーリングの成功回数。
    history_polls: u64,
    /// history が返した生メッセージ総数(dedup 前)。
    history_msgs: u64,
    /// history ポーリングの失敗回数(通信/HTTP/JSON エラー)。
    history_errors: u64,
    /// 非チャット判定の理由別件数。
    non_chat: std::collections::HashMap<&'static str, u64>,
}

impl SessionStats {
    fn new(broadcast_id: &str) -> Self {
        SessionStats {
            broadcast_id: broadcast_id.to_string(),
            started: Instant::now(),
            frames_data: 0,
            frames_chat: 0,
            rx_text: 0,
            rx_binary: 0,
            rx_binary_invalid_utf8: 0,
            rx_ping: 0,
            rx_pong: 0,
            tx_send_fail: 0,
            frames_dup: 0,
            history_polls: 0,
            history_msgs: 0,
            history_errors: 0,
            non_chat: std::collections::HashMap::new(),
        }
    }

    /// Twitch と同じ「安定セッション」判定(チャット実受信 or 30秒以上継続)。
    fn is_stable(&self) -> bool {
        self.frames_chat > 0 || self.started.elapsed().as_secs() >= 30
    }

    fn summary(&self) -> String {
        format!(
            "{}秒 chat:{} 重複:{} history(poll:{} msg:{} err:{}) text:{} binary:{}(非UTF8 {}) ping:{} pong:{} 送信失敗:{} 非チャット:{:?}",
            self.started.elapsed().as_secs(),
            self.frames_chat,
            self.frames_dup,
            self.history_polls,
            self.history_msgs,
            self.history_errors,
            self.rx_text,
            self.rx_binary,
            self.rx_binary_invalid_utf8,
            self.rx_ping,
            self.rx_pong,
            self.tx_send_fail,
            self.non_chat,
        )
    }

    /// 生存中の定期サマリ。「接続は生きているがチャットが来ない」の可視化。
    fn log_progress(&self) {
        tracing::info!("x:{} 接続中 {}", self.broadcast_id, self.summary());
    }
}

impl Drop for SessionStats {
    fn drop(&mut self) {
        tracing::info!("x:{} セッション終了: {}", self.broadcast_id, self.summary());
        // 30秒以上生きたのにチャット0件は、auth/join が黙殺されたまま
        // transport だけ生存していた疑いが強い(視聴者ゼロの配信でも起きるため
        // 断定はしない)。is_stable() はこのケースでも true になり backoff が
        // リセットされ続ける点に注意。
        if self.frames_chat == 0 && self.started.elapsed().as_secs() >= 30 {
            tracing::warn!(
                "x:{} チャット0件のまま{}秒生存したセッションが終了(auth/join 黙殺の疑い)",
                self.broadcast_id,
                self.started.elapsed().as_secs(),
            );
        }
    }
}

/// `frame_to_chat` が None を返した理由の分類(診断カウンタ用)。
/// 非チャットフレームは低頻度の前提で、二度パースのコストは許容する。
fn classify_non_chat(text: &str) -> &'static str {
    let Ok(outer) = serde_json::from_str::<Value>(text) else {
        return "outer_json_invalid";
    };
    if outer.get("kind").and_then(Value::as_i64) != Some(1) {
        return "kind_not_1";
    }
    let Some(payload_str) = outer.get("payload").and_then(Value::as_str) else {
        return "payload_not_string";
    };
    let Ok(payload) = serde_json::from_str::<Value>(payload_str) else {
        return "payload_json_invalid";
    };
    let Some(body_str) = payload.get("body").and_then(Value::as_str) else {
        return "body_not_string";
    };
    let Ok(body) = serde_json::from_str::<Value>(body_str) else {
        return "body_json_invalid";
    };
    match body.get("type").and_then(Value::as_i64) {
        Some(1) | None => {}
        Some(_) => return "type_not_chat",
    }
    if body
        .get("body")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .is_empty()
    {
        return "body_text_empty";
    }
    "identity_missing"
}

/// UTF-8 文字境界を守って先頭 max 文字へ切り詰める(ログ用)。
fn truncate_chars(s: &str, max: usize) -> String {
    let mut t: String = s.chars().take(max).collect();
    if t.len() < s.len() {
        t.push('…');
    }
    t
}

/// 秒/ミリ秒/ナノ秒が混在しうる epoch 値をミリ秒へ寄せる。
fn normalize_epoch_ms(v: i64) -> i64 {
    if v <= 0 {
        return now_ms();
    }
    if v >= 1_000_000_000_000_000 {
        // ナノ秒(Periscope の payload.timestamp)。
        v / 1_000_000
    } else if v < 100_000_000_000 {
        // 秒。
        v.saturating_mul(1000)
    } else {
        v
    }
}

fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_broadcast_id_from_urls_and_raw_ids() {
        assert_eq!(
            extract_broadcast_id("https://x.com/i/broadcasts/1yoJMWvbtbtxQ"),
            "1yoJMWvbtbtxQ"
        );
        assert_eq!(
            extract_broadcast_id("https://twitter.com/i/broadcasts/1yoJMWvbtbtxQ?s=20"),
            "1yoJMWvbtbtxQ"
        );
        assert_eq!(extract_broadcast_id("  1yoJMWvbtbtxQ  "), "1yoJMWvbtbtxQ");
    }

    #[test]
    fn normalizes_epoch_units() {
        assert_eq!(normalize_epoch_ms(1_700_000_000), 1_700_000_000_000); // 秒
        assert_eq!(normalize_epoch_ms(1_700_000_000_000), 1_700_000_000_000); // ミリ秒
        assert_eq!(
            normalize_epoch_ms(1_700_000_000_000_000_000),
            1_700_000_000_000
        ); // ナノ秒
    }

    #[test]
    fn chat_frame_normalizes_to_message() {
        let src = XSource::new("1yoJMWvbtbtxQ".to_string(), XOverrides::default(), None);
        let body = serde_json::json!({
            "type": 1,
            "body": "こんにちは",
            "displayName": "Alice",
            "username": "alice",
            "remoteID": "12345",
            "timestamp": 1_700_000_000_123i64,
            "uuid": "abc-123",
        })
        .to_string();
        let payload = serde_json::json!({
            "body": body,
            "sender": { "user_id": "ps-1", "username": "alice", "display_name": "Alice" },
        })
        .to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();

        let msg = src.frame_to_chat(&frame, Some("999")).expect("chat message");
        assert_eq!(msg.platform, Platform::X);
        assert_eq!(msg.channel, "1yoJMWvbtbtxQ");
        assert_eq!(msg.author.id, "12345");
        assert_eq!(msg.author.name, "Alice");
        assert_eq!(msg.plain_text(), "こんにちは");
        assert_eq!(msg.timestamp_ms, 1_700_000_000_123);
        assert_eq!(msg.id, "abc-123");
        assert!(!msg.author.roles.broadcaster);

        // broadcaster_id が一致すれば broadcaster ロールが立つ。
        let msg = src.frame_to_chat(&frame, Some("12345")).expect("chat message");
        assert!(msg.author.roles.broadcaster);
    }

    #[test]
    fn user_id_falls_back_to_numeric_sender_twitter_id() {
        // remoteID 欠落フレーム: sender.twitter_id(数値)を拾い、ハンドル変更でも
        // 同一人物を追跡できる ID を採用する。
        let src = XSource::new("b1".to_string(), XOverrides::default(), None);
        let body = serde_json::json!({
            "type": 1,
            "body": "hi",
            "username": "bob",
        })
        .to_string();
        let payload = serde_json::json!({
            "body": body,
            "sender": { "twitter_id": 424242u64, "username": "bob" },
        })
        .to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();

        let msg = src.frame_to_chat(&frame, Some("424242")).expect("chat message");
        assert_eq!(msg.author.id, "424242");
        assert!(msg.author.roles.broadcaster);
    }

    /// fake bootstrap HTTP + fake chatnow WS に対する結合テスト。
    /// bootstrap 4段 → WS 接続 → auth/join の送信内容 → Text/Binary 両 opcode の
    /// チャット受信 → Ping への Pong 応答、をライブ配信なしで検証する。
    #[tokio::test]
    async fn end_to_end_receives_text_and_binary_chat_via_fake_servers() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // --- fake chatnow WS ---
        let ws_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let ws_port = ws_listener.local_addr().unwrap().port();
        let (pong_tx, mut pong_rx) = tokio::sync::mpsc::unbounded_channel::<u32>();
        tokio::spawn(async move {
            let (stream, _) = ws_listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(stream).await.unwrap();

            // auth(kind:3) → join(kind:2) の順で Text として届くこと。
            let auth: Value = match ws.next().await.unwrap().unwrap() {
                WsMessage::Text(t) => serde_json::from_str(&t).unwrap(),
                other => panic!("auth frame が Text でない: {other:?}"),
            };
            assert_eq!(auth.get("kind").and_then(Value::as_i64), Some(3));
            let auth_payload: Value =
                serde_json::from_str(auth.get("payload").and_then(Value::as_str).unwrap())
                    .unwrap();
            assert_eq!(
                auth_payload.get("access_token").and_then(Value::as_str),
                Some("at1")
            );

            let join: Value = match ws.next().await.unwrap().unwrap() {
                WsMessage::Text(t) => serde_json::from_str(&t).unwrap(),
                other => panic!("join frame が Text でない: {other:?}"),
            };
            assert_eq!(join.get("kind").and_then(Value::as_i64), Some(2));
            let join_payload: Value =
                serde_json::from_str(join.get("payload").and_then(Value::as_str).unwrap())
                    .unwrap();
            let join_body: Value = serde_json::from_str(
                join_payload.get("body").and_then(Value::as_str).unwrap(),
            )
            .unwrap();
            assert_eq!(join_body.get("room").and_then(Value::as_str), Some("b1"));

            // Text と Binary の両 opcode でチャットを配信。
            let mk_frame = |text: &str, uid: &str| {
                let body = serde_json::json!({
                    "type": 1, "body": text, "username": "u", "remoteID": uid,
                })
                .to_string();
                let payload = serde_json::json!({ "body": body, "sender": {} }).to_string();
                serde_json::json!({ "kind": 1, "payload": payload }).to_string()
            };
            ws.send(ws_text(&mk_frame("text経由", "1"))).await.unwrap();
            ws.send(WsMessage::Binary(
                mk_frame("binary経由", "2").into_bytes().into(),
            ))
            .await
            .unwrap();
            // Ping も送り、クライアントの Pong 応答数を観察する
            // (tungstenite の自動 Pong と手動 Pong の重複検出)。
            ws.send(WsMessage::Ping(b"hb".to_vec().into())).await.unwrap();

            let mut pongs = 0u32;
            loop {
                match ws.next().await {
                    Some(Ok(WsMessage::Pong(_))) => {
                        pongs += 1;
                        let _ = pong_tx.send(pongs);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(_)) | None => break,
                }
            }
        });

        // --- fake bootstrap HTTP (1リクエスト1コネクション) ---
        let http_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let http_port = http_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = http_listener.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 8192];
                    let n = stream.read(&mut buf).await.unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]);
                    let path = req.split_whitespace().nth(1).unwrap_or("");
                    let body = if path.starts_with("/activate") {
                        r#"{"guest_token":"g1"}"#.to_string()
                    } else if path.starts_with("/show") {
                        r#"{"broadcasts":{"b1":{"media_key":"mk1","state":"RUNNING","user_id":"99","status":"t"}}}"#
                            .to_string()
                    } else if path.starts_with("/status/") {
                        r#"{"chatToken":"ct1"}"#.to_string()
                    } else if path.starts_with("/access") {
                        format!(
                            r#"{{"endpoint":"http://127.0.0.1:{ws_port}","access_token":"at1","room_id":"b1"}}"#
                        )
                    } else if path.starts_with("/history") {
                        // history 経路の検証用。外側 kind:2 エンベロープ + 内側 kind:1。
                        // uuid 固定なので、繰り返しポーリングされても dedup により
                        // 1件だけ emit されることも兼ねて確認できる。
                        let inner_body = serde_json::json!({
                            "type": 1, "body": "history経由", "username": "h",
                            "remoteID": "3", "uuid": "hist-1"
                        })
                        .to_string();
                        let payload = serde_json::json!({
                            "kind": 1, "sender": {}, "body": inner_body
                        })
                        .to_string();
                        let msg = serde_json::json!({ "kind": 2, "payload": payload });
                        serde_json::json!({ "messages": [msg], "cursor": "c1" }).to_string()
                    } else {
                        "{}".to_string()
                    };
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });

        // --- XSource を fake へ向けて起動 ---
        let mut overrides = XOverrides::default();
        let base = format!("http://127.0.0.1:{http_port}");
        overrides
            .endpoints
            .insert("guestActivateUrl".to_string(), format!("{base}/activate"));
        overrides
            .endpoints
            .insert("broadcastShowUrl".to_string(), format!("{base}/show"));
        overrides
            .endpoints
            .insert("liveStatusUrl".to_string(), format!("{base}/status"));
        overrides
            .endpoints
            .insert("accessChatUrl".to_string(), format!("{base}/access"));
        overrides
            .endpoints
            .insert("historyUrl".to_string(), format!("{base}/history"));

        let (tx, mut rx) = broadcast::channel(16);
        let cancel = CancellationToken::new();
        let src = XSource::new("b1".to_string(), overrides, None);
        let cancel_run = cancel.clone();
        let run = tokio::spawn(async move { src.run(tx, cancel_run).await });

        // WS Text / WS Binary / history の3経路それぞれ1件、計3件が届く。
        // 到着順は非決定的なので集めてから内容を検証する。
        let mut texts = Vec::new();
        let mut first_platform = None;
        for i in 0..3 {
            let m = tokio::time::timeout(Duration::from_secs(10), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("{}件目タイムアウト", i + 1))
                .expect("channel closed");
            if first_platform.is_none() {
                first_platform = Some(m.platform);
                assert_eq!(m.channel, "b1");
            }
            texts.push(m.plain_text());
        }
        assert!(texts.contains(&"text経由".to_string()), "texts={texts:?}");
        assert!(texts.contains(&"binary経由".to_string()), "texts={texts:?}");
        assert!(texts.contains(&"history経由".to_string()), "texts={texts:?}");
        assert_eq!(first_platform, Some(Platform::X));

        // Ping への Pong 応答(自動+手動の重複があり得るため 1 以上)。
        let pongs = tokio::time::timeout(Duration::from_secs(10), pong_rx.recv())
            .await
            .expect("Pong タイムアウト")
            .expect("pong channel closed");
        assert!(pongs >= 1, "Pong が返らない");

        cancel.cancel();
        let _ = run.await;
    }

    #[test]
    fn classify_non_chat_reports_reason() {
        assert_eq!(classify_non_chat("not json"), "outer_json_invalid");
        assert_eq!(
            classify_non_chat(r#"{"kind":2,"payload":"{}"}"#),
            "kind_not_1"
        );
        let body = serde_json::json!({ "type": 2, "body": "" }).to_string();
        let payload = serde_json::json!({ "body": body }).to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();
        assert_eq!(classify_non_chat(&frame), "type_not_chat");
        let body = serde_json::json!({ "type": 1, "body": "  " }).to_string();
        let payload = serde_json::json!({ "body": body }).to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();
        assert_eq!(classify_non_chat(&frame), "body_text_empty");
    }

    #[test]
    fn chat_frame_without_type_still_parses() {
        // type 欠落フレーム: 本文があればチャットとして通す(寛容パース)。
        let src = XSource::new("b1".to_string(), XOverrides::default(), None);
        let body = serde_json::json!({ "body": "hello", "username": "carol" }).to_string();
        let payload =
            serde_json::json!({ "body": body, "sender": { "username": "carol" } }).to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();
        let msg = src.frame_to_chat(&frame, None).expect("chat message");
        assert_eq!(msg.plain_text(), "hello");
    }

    #[test]
    fn truncate_chars_respects_utf8_boundaries() {
        assert_eq!(truncate_chars("あいうえお", 3), "あいう…");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn non_chat_frames_are_ignored() {
        let src = XSource::new("b1".to_string(), XOverrides::default(), None);
        // kind != 1。
        assert!(src.frame_to_chat(r#"{"kind":2,"payload":"{}"}"#, None).is_none());
        // ハート(type=2)。
        let body = serde_json::json!({ "type": 2, "body": "" }).to_string();
        let payload = serde_json::json!({ "body": body }).to_string();
        let frame = serde_json::json!({ "kind": 1, "payload": payload }).to_string();
        assert!(src.frame_to_chat(&frame, None).is_none());
        // 壊れた JSON。
        assert!(src.frame_to_chat("not json", None).is_none());
    }

    #[test]
    fn history_envelope_reframes_to_chat() {
        // history メッセージ(外側 kind:2)の内側 payload を {kind:1, payload} に
        // 組み替えると frame_to_chat が本文を取り出せること(poll_history の組み替え相当)。
        let src = XSource::new("b1".to_string(), XOverrides::default(), None);
        let inner_body = serde_json::json!({
            "type": 1, "body": "やあ", "username": "dave", "remoteID": "7", "uuid": "h7"
        })
        .to_string();
        let inner_payload = serde_json::json!({
            "kind": 1, "sender": { "username": "dave" }, "body": inner_body
        })
        .to_string();
        let history_msg = serde_json::json!({ "kind": 2, "payload": inner_payload });
        let framed =
            serde_json::json!({ "kind": 1, "payload": history_msg.get("payload").unwrap() })
                .to_string();
        let chat = src.frame_to_chat(&framed, None).expect("chat message");
        assert_eq!(chat.plain_text(), "やあ");
        assert_eq!(chat.author.id, "7");
        assert_eq!(chat.id, "h7");
    }

    #[test]
    fn dedup_rejects_repeats_and_evicts_oldest() {
        let mut d = ChatDedup::new(2);
        assert!(d.insert("a"));
        assert!(d.insert("b"));
        assert!(!d.insert("a")); // 既出は false。
        assert!(d.insert("c")); // 容量2超過で最古の "a" が捨てられる。
        assert!(d.insert("a")); // "a" は捨てられたので再び新規扱い。
    }
}
