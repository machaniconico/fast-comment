//! X (Twitter) ライブ配信 (broadcasts) チャット Source。
//!
//! ログイン不要のゲストトークンフローで Periscope 由来 chatapi の WebSocket に接続する:
//! 1. POST guest/activate.json (公開 Bearer) → guest_token
//! 2. GET broadcasts/show.json?ids={id} → media_key
//! 3. GET live_video_stream/status/{media_key} → chatToken
//! 4. POST accessChatPublic (chat_token) → endpoint + access_token
//! 5. {endpoint}/chatapi/v1/chatnow (wss) へ接続し kind:3(認証) → kind:2(参加) を送信
//!
//! 受信フレームは kind==1 のみがチャットで、payload は二重 JSON ネスト
//! (payload 文字列 → その中の body 文字列を再パース)。仕様変更に強いよう
//! 固い struct デシリアライズはせず `serde_json::Value` のパス探索で欠落を
//! None に劣化させる(SPEC の YouTube パースと同方針)。URL/Bearer は
//! `XOverrides` で再ビルド無しに上書きできる。

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
        let (ws_stream, _resp) = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = tokio::time::timeout(
                WS_CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async(req),
            ) => r
                .map_err(|e| XSessionError::new(e, false))?
                .map_err(|e| XSessionError::new(e, false))?,
        };
        let (mut write, mut read) = ws_stream.split();

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

        tracing::info!("x:{} chatnow 接続・参加完了", self.broadcast_id);
        // チップに「接続中」を出す。X には視聴者数の取得手段が無いため live のみ。
        self.send_live_state(true, boot.title.clone()).await;

        let connected_at = Instant::now();
        // 「安定」判定はチャットの実受信のみで立てる。認証エラー通知や Ping 等の
        // 雑フレームでも立ててしまうと、接続→即切断の反復で毎回バックオフが
        // 1秒へ戻り、実質リトライ間隔が伸びなくなる。
        let mut received_chat = false;
        let mut last_activity = tokio::time::Instant::now();
        let mut ping_sent_at: Option<tokio::time::Instant> = None;

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
                _ = tokio::time::sleep_until(idle_deadline) => {
                    if ping_sent_at.is_some() {
                        // Ping 後も無応答: half-open とみなして再接続へ。
                        return Err(XSessionError::new(
                            anyhow::anyhow!("アイドルタイムアウト(Ping 応答なし)"),
                            session_is_stable(received_chat, connected_at),
                        ));
                    }
                    write.send(WsMessage::Ping(Vec::new().into())).await.map_err(|e| {
                        XSessionError::new(e, session_is_stable(received_chat, connected_at))
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
                            return Err(XSessionError::new(
                                e,
                                session_is_stable(received_chat, connected_at),
                            ));
                        }
                        None => {
                            return Ok(session_is_stable(received_chat, connected_at));
                        }
                    };

                    match msg {
                        WsMessage::Text(text) => {
                            if let Some(chat) = self.frame_to_chat(&text, boot.broadcaster_id.as_deref()) {
                                received_chat = true;
                                let _ = tx.send(chat);
                            }
                        }
                        WsMessage::Ping(payload) => {
                            write.send(WsMessage::Pong(payload)).await.map_err(|e| {
                                XSessionError::new(
                                    e,
                                    session_is_stable(received_chat, connected_at),
                                )
                            })?;
                        }
                        WsMessage::Close(_) => {
                            return Ok(session_is_stable(received_chat, connected_at));
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

        // body.type: 1 がチャット本文。それ以外(ハート等)は捨てる。
        if body.get("type").and_then(Value::as_i64) != Some(1) {
            return None;
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

fn session_is_stable(received_data: bool, connected_at: Instant) -> bool {
    received_data || connected_at.elapsed().as_secs() >= 30
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
}
