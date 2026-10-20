//! niconico 生放送コメント Source。
//!
//! ログイン不要の視聴ページ経由で NDGR メッセージサーバーへ接続する:
//! 1. GET https://live.nicovideo.jp/watch/{lvId} → HTML 内 embedded-data(data-props JSON)
//! 2. site.relive.webSocketUrl へ watch WebSocket 接続し startWatching を送信
//!    (seat/keepSeat で座席維持、アプリレベル ping には pong を返す)
//! 3. messageServer で受け取った NDGR view URI を `?at=now` → ReadyForNext.at で
//!    追いかけ、MessageSegment.uri の ChunkedMessage ストリームからコメントを読む
//!
//! NDGR は length-delimited Protobuf over HTTP chunked。定義は
//! n-air-app/nicolive-comment-protobuf から必要フィールドのみを prost derive で
//! 手書きし、未知フィールドは prost が自動スキップする(SPEC の寛容パース方針)。
//! watch ページ URL は `NiconicoOverrides.endpoints` の watchPageBaseUrl で
//! 再ビルド無しに上書きできる。

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::{Buf, BytesMut};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::config::NiconicoOverrides;
use crate::model::{Amount, Author, ChatMessage, Fragment, MessageKind, Platform, Roles};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

const WATCH_PAGE_BASE_URL: &str = "https://live.nicovideo.jp/watch/";
/// ブラウザ相当の UA。視聴ページはブラウザ以外の UA を弾くことがある。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
/// 単発 HTTP リクエスト(視聴ページ取得)の全体タイムアウト。
/// NDGR の view/segment は長時間開きっぱなしのストリームなので、これは付けない
/// (Client 側に付けると、その時間ちょうどで必ずストリームが切れる)。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
/// HTTP 接続確立のタイムアウト。ストリームを殺さないので Client 側に付けてよい。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// WebSocket ハンドシェイクのタイムアウト。
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// watch WS はサーバーから約30秒毎に ping が来る。この時間何も来なければ
/// half-open とみなし再接続する。
const WATCH_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
/// NDGR view ストリームの無通信タイムアウト。view は次窓を指す `next` を必ず
/// 流してくるので、これだけ無音なら half-open とみなしてセッションを張り直す。
/// 全体タイムアウトの代わりなので、正常な窓の長さより十分長く取る。
const NDGR_IDLE_TIMEOUT: Duration = Duration::from_secs(180);
/// seat メッセージが keepIntervalSec を運んでこなかった場合の既定値。
const DEFAULT_KEEP_SEAT_SEC: u64 = 30;

/// NDGR の Protobuf 定義(必要フィールドのみの手書き最小サブセット)。
///
/// 完全な定義は n-air-app/nicolive-comment-protobuf を参照。ここに無い
/// フィールド/oneof variant は prost が unknown として読み飛ばすため、
/// 追加された仕様に対しても None 劣化で動き続ける。
pub mod ndgr {
    /// google.protobuf.Timestamp 互換(依存追加を避けるため手書き)。
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Timestamp {
        #[prost(int64, tag = "1")]
        pub seconds: i64,
        #[prost(int32, tag = "2")]
        pub nanos: i32,
    }

    /// view API が返すエントリ。segment(現行) と next(継続ポーリング) のみ拾う。
    /// backward(2)/previous(3) は過去コメント用のため定義せず読み飛ばす。
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ChunkedEntry {
        #[prost(oneof = "chunked_entry::Entry", tags = "1, 4")]
        pub entry: Option<chunked_entry::Entry>,
    }

    pub mod chunked_entry {
        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Entry {
            #[prost(message, tag = "1")]
            Segment(super::MessageSegment),
            #[prost(message, tag = "4")]
            Next(super::ReadyForNext),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct MessageSegment {
        #[prost(string, tag = "3")]
        pub uri: String,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ReadyForNext {
        #[prost(int64, tag = "1")]
        pub at: i64,
    }

    /// segment ストリームの1メッセージ。payload は message(2) のみ拾い、
    /// state(4)/signal(5) は読み飛ばす。
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct ChunkedMessage {
        #[prost(message, optional, tag = "1")]
        pub meta: Option<Meta>,
        #[prost(oneof = "chunked_message::Payload", tags = "2")]
        pub payload: Option<chunked_message::Payload>,
    }

    pub mod chunked_message {
        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Payload {
            #[prost(message, tag = "2")]
            Message(super::NicoliveMessage),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Meta {
        #[prost(string, tag = "1")]
        pub id: String,
        #[prost(message, optional, tag = "2")]
        pub at: Option<Timestamp>,
    }

    /// chat(1) / gift(8) / overflowed_chat(20 = 流量超過時の間引きコメント) を拾う。
    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct NicoliveMessage {
        #[prost(oneof = "nicolive_message::Data", tags = "1, 8, 20")]
        pub data: Option<nicolive_message::Data>,
    }

    pub mod nicolive_message {
        #[derive(Clone, PartialEq, ::prost::Oneof)]
        pub enum Data {
            #[prost(message, tag = "1")]
            Chat(super::Chat),
            #[prost(message, tag = "8")]
            Gift(super::Gift),
            #[prost(message, tag = "20")]
            OverflowedChat(super::Chat),
        }
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Chat {
        #[prost(string, tag = "1")]
        pub content: String,
        /// コテハン等。無名コメントでは欠落する。
        #[prost(string, optional, tag = "2")]
        pub name: Option<String>,
        /// 生ID(ログインユーザー)。匿名(184)では欠落する。
        #[prost(int64, optional, tag = "5")]
        pub raw_user_id: Option<i64>,
        /// 匿名コメントのハッシュ ID。
        #[prost(string, optional, tag = "6")]
        pub hashed_user_id: Option<String>,
    }

    #[derive(Clone, PartialEq, ::prost::Message)]
    pub struct Gift {
        #[prost(int64, optional, tag = "2")]
        pub advertiser_user_id: Option<i64>,
        #[prost(string, tag = "3")]
        pub advertiser_name: String,
        #[prost(int64, tag = "4")]
        pub point: i64,
        #[prost(string, tag = "5")]
        pub message: String,
        #[prost(string, tag = "6")]
        pub item_name: String,
    }
}

/// URL または生 ID から lv 番組 ID を取り出す。
///
/// `https://live.nicovideo.jp/watch/lv123456` 形式のほか、`lv123456` 単体、
/// 短縮 URL(`nico.ms/lv123456`)にも耐える。lv が見つからない入力は
/// クエリ/フラグメント/末尾スラッシュだけ落として素通しする
/// (co チャンネル URL 等は視聴ページ側のリダイレクトに任せる)。
pub fn extract_live_id(input: &str) -> String {
    let s = input.trim();
    let bytes = s.as_bytes();
    let mut i = 0;
    while let Some(pos) = s[i..].find("lv") {
        let start = i + pos;
        let prev_is_alnum = start > 0 && bytes[start - 1].is_ascii_alphanumeric();
        let digits: String = s[start + 2..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if !prev_is_alnum && !digits.is_empty() {
            return format!("lv{digits}");
        }
        i = start + 2;
    }
    let core = s.split(['?', '#']).next().unwrap_or(s).trim_end_matches('/');
    core.rsplit('/').next().unwrap_or(core).to_string()
}

/// 視聴ページ HTML から embedded-data(data-props 属性の JSON)を取り出す。
fn extract_embedded_data(html: &str) -> Option<Value> {
    let tail = match html.find("id=\"embedded-data\"") {
        Some(a) => &html[a..],
        None => html,
    };
    let dp = tail.find("data-props=\"")?;
    let start = dp + "data-props=\"".len();
    let end = start + tail[start..].find('"')?;
    serde_json::from_str(&html_unescape(&tail[start..end])).ok()
}

/// HTML 属性値の最小限のエンティティ復号。`&amp;` は最後に戻す。
fn html_unescape(s: &str) -> String {
    s.replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

/// watch WS の受信メッセージを分類したアクション。
#[derive(Debug, PartialEq)]
enum WatchAction {
    Ignore,
    /// アプリレベル ping への応答要求。
    Pong,
    Seat {
        keep_interval_sec: u64,
    },
    MessageServer {
        view_uri: String,
    },
    Statistics {
        viewers: Option<u32>,
    },
    /// サーバー都合の切断・エラー通知。理由文字列付き。
    Disconnect {
        reason: String,
    },
}

/// watch WS の1テキストフレームを分類する。未知 type は Ignore。
fn parse_watch_message(text: &str) -> WatchAction {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return WatchAction::Ignore;
    };
    match v.get("type").and_then(Value::as_str) {
        Some("ping") => WatchAction::Pong,
        Some("seat") => WatchAction::Seat {
            keep_interval_sec: v
                .pointer("/data/keepIntervalSec")
                .and_then(Value::as_u64)
                .filter(|s| *s > 0)
                .unwrap_or(DEFAULT_KEEP_SEAT_SEC),
        },
        Some("messageServer") => match v
            .pointer("/data/viewUri")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        {
            Some(uri) => WatchAction::MessageServer {
                view_uri: uri.to_string(),
            },
            None => WatchAction::Ignore,
        },
        Some("statistics") => WatchAction::Statistics {
            viewers: v
                .pointer("/data/viewers")
                .and_then(Value::as_i64)
                .filter(|n| *n >= 0)
                .map(|n| n.min(u32::MAX as i64) as u32),
        },
        Some("disconnect") => WatchAction::Disconnect {
            reason: v
                .pointer("/data/reason")
                .and_then(Value::as_str)
                .unwrap_or("不明")
                .to_string(),
        },
        Some("error") => WatchAction::Disconnect {
            reason: v
                .pointer("/data/code")
                .and_then(Value::as_str)
                .unwrap_or("エラー")
                .to_string(),
        },
        _ => WatchAction::Ignore,
    }
}

/// length-delimited Protobuf ストリームのバッファから1メッセージ取り出す。
///
/// データ不足なら Ok(None)(呼び出し側が続きの chunk を待つ)。長さプレフィクスや
/// 本体が壊れている場合は Err(ストリーム全体を張り直す)。
fn try_take_message<M: prost::Message + Default>(
    buf: &mut BytesMut,
) -> anyhow::Result<Option<M>> {
    if buf.is_empty() {
        return Ok(None);
    }
    let mut peek: &[u8] = &buf[..];
    let len = match prost::decode_length_delimiter(&mut peek) {
        Ok(l) => l,
        Err(e) => {
            // varint は最大10バイト。それだけ揃っていて読めないなら破損。
            if buf.len() >= 10 {
                return Err(e.into());
            }
            return Ok(None);
        }
    };
    let header = buf.len() - peek.len();
    if peek.len() < len {
        return Ok(None);
    }
    let msg = M::decode(&peek[..len])?;
    buf.advance(header + len);
    Ok(Some(msg))
}

/// 視聴ページから得た接続情報。
struct WatchBootstrap {
    web_socket_url: String,
    title: Option<String>,
    /// 放送者のユーザー ID(broadcaster ロール判定用)。
    broadcaster_id: Option<String>,
}

#[derive(Debug)]
struct NiconicoSessionError {
    error: anyhow::Error,
    stable: bool,
}

impl NiconicoSessionError {
    fn new<E>(error: E, stable: bool) -> Self
    where
        E: Into<anyhow::Error>,
    {
        NiconicoSessionError {
            error: error.into(),
            stable,
        }
    }
}

/// ニコニコ生放送1番組のコメントを購読する Source。
pub struct NiconicoSource {
    /// 正規化済み lv 番組 ID。
    live_id: String,
    overrides: NiconicoOverrides,
    /// チップの接続状態表示用。live/タイトル/視聴者数を送る。
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl NiconicoSource {
    pub fn new(
        identifier: String,
        overrides: NiconicoOverrides,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        NiconicoSource {
            live_id: extract_live_id(&identifier),
            overrides,
            metadata_tx,
        }
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

impl Source for NiconicoSource {
    fn name(&self) -> String {
        format!("niconico:{}", self.live_id)
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
                        self.send_live_state(false, None, None).await;
                        if stable {
                            backoff.reset();
                        }
                    }
                    Err(e) => {
                        self.send_live_state(false, None, None).await;
                        if e.stable {
                            backoff.reset();
                        }
                        tracing::warn!("niconico:{} 接続エラー: {:#}", self.live_id, e.error);
                    }
                }

                let delay = backoff.next_delay();
                tracing::info!(
                    "niconico:{} {}ms 後に再接続",
                    self.live_id,
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

impl NiconicoSource {
    /// 1接続分のセッション。視聴ページ → watch WS → NDGR 購読 → 受信ループ。
    ///
    /// 戻り値は他 Source と同じ「安定セッション」判定(30秒以上 or コメント受信あり)。
    async fn connect_and_listen(
        &self,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> Result<bool, NiconicoSessionError> {
        // NDGR ストリームを全体タイムアウトで殺さないよう Client には connect_timeout
        // のみ設定し、視聴ページ取得側に個別タイムアウトを付ける(x.rs と同方針)。
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| NiconicoSessionError::new(e, false))?;

        // 視聴ページ取得は配信未開始/終了でも失敗するため、失敗は不安定扱いで
        // バックオフを伸ばし続ける(=配信開始待ちのポーリングを兼ねる)。
        let boot = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = self.bootstrap(&client) => r.map_err(|e| NiconicoSessionError::new(e, false))?,
        };

        let mut req = boot
            .web_socket_url
            .clone()
            .into_client_request()
            .map_err(|e| NiconicoSessionError::new(e, false))?;
        req.headers_mut().insert(
            "Origin",
            "https://live.nicovideo.jp".parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    NiconicoSessionError::new(e, false)
                },
            )?,
        );
        req.headers_mut().insert(
            "User-Agent",
            USER_AGENT.parse().map_err(
                |e: tokio_tungstenite::tungstenite::http::header::InvalidHeaderValue| {
                    NiconicoSessionError::new(e, false)
                },
            )?,
        );
        let (ws_stream, _resp) = tokio::select! {
            _ = cancel.cancelled() => return Ok(true),
            r = tokio::time::timeout(
                WS_CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async(req),
            ) => r
                .map_err(|e| NiconicoSessionError::new(e, false))?
                .map_err(|e| NiconicoSessionError::new(e, false))?,
        };
        let (mut write, mut read) = ws_stream.split();

        // コメント購読のみが目的なので stream(映像座席)は要求しない。
        let start = json!({ "type": "startWatching", "data": { "reconnect": false } });
        write
            .send(ws_text(&start.to_string()))
            .await
            .map_err(|e| NiconicoSessionError::new(e, false))?;

        tracing::info!("niconico:{} watch セッション開始", self.live_id);
        self.send_live_state(true, boot.title.clone(), None).await;

        let connected_at = Instant::now();
        // 「安定」判定は NDGR からのコメント実受信のみで立てる(x.rs と同方針)。
        let received_chat = Arc::new(AtomicBool::new(false));
        let mut last_activity = tokio::time::Instant::now();
        let mut keep_seat: Option<tokio::time::Interval> = None;
        // NDGR 購読タスク。watch WS より先に死んだらセッション全体を張り直す。
        let mut ndgr_task: Option<tokio::task::JoinHandle<anyhow::Result<()>>> = None;
        // セッション終了時に NDGR 購読も確実に止める(全 return 経路で発火)。
        let ndgr_cancel = cancel.child_token();
        let _ndgr_guard = ndgr_cancel.clone().drop_guard();

        let stable = |received: &AtomicBool| {
            received.load(Ordering::Relaxed) || connected_at.elapsed().as_secs() >= 30
        };

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(WsMessage::Close(None)).await;
                    return Ok(true);
                }
                _ = async { keep_seat.as_mut().expect("guarded").tick().await }, if keep_seat.is_some() => {
                    write
                        .send(ws_text(r#"{"type":"keepSeat"}"#))
                        .await
                        .map_err(|e| NiconicoSessionError::new(e, stable(&received_chat)))?;
                }
                r = async { ndgr_task.as_mut().expect("guarded").await }, if ndgr_task.is_some() => {
                    // NDGR 側の終了はエラー起点(view ストリーム破損等)でも正常 EOF でも
                    // コメントが取れなくなるので、セッションを張り直す。
                    let detail = match r {
                        Ok(Ok(())) => "正常終了".to_string(),
                        Ok(Err(e)) => format!("{e:#}"),
                        Err(e) => format!("パニック/中断: {e}"),
                    };
                    return Err(NiconicoSessionError::new(
                        anyhow::anyhow!("NDGR 購読が終了: {detail}"),
                        stable(&received_chat),
                    ));
                }
                _ = tokio::time::sleep_until(last_activity + WATCH_IDLE_TIMEOUT) => {
                    return Err(NiconicoSessionError::new(
                        anyhow::anyhow!("watch WS アイドルタイムアウト"),
                        stable(&received_chat),
                    ));
                }
                msg = read.next() => {
                    last_activity = tokio::time::Instant::now();
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            return Err(NiconicoSessionError::new(e, stable(&received_chat)));
                        }
                        None => return Ok(stable(&received_chat)),
                    };

                    match msg {
                        WsMessage::Text(text) => match parse_watch_message(&text) {
                            WatchAction::Pong => {
                                // 公式 Web クライアントに合わせ pong + keepSeat を返す。
                                write
                                    .send(ws_text(r#"{"type":"pong"}"#))
                                    .await
                                    .map_err(|e| NiconicoSessionError::new(e, stable(&received_chat)))?;
                                write
                                    .send(ws_text(r#"{"type":"keepSeat"}"#))
                                    .await
                                    .map_err(|e| NiconicoSessionError::new(e, stable(&received_chat)))?;
                            }
                            WatchAction::Seat { keep_interval_sec } => {
                                let mut iv = tokio::time::interval(Duration::from_secs(keep_interval_sec));
                                iv.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                                iv.reset(); // interval は初回即発火のため1周期後から。
                                keep_seat = Some(iv);
                            }
                            WatchAction::MessageServer { view_uri } => {
                                if let Some(prev) = ndgr_task.take() {
                                    prev.abort();
                                }
                                ndgr_task = Some(tokio::spawn(ndgr_view_loop(
                                    client.clone(),
                                    view_uri,
                                    tx.clone(),
                                    ndgr_cancel.clone(),
                                    self.live_id.clone(),
                                    boot.broadcaster_id.clone(),
                                    received_chat.clone(),
                                )));
                            }
                            WatchAction::Statistics { viewers } => {
                                if viewers.is_some() {
                                    self.send_live_state(true, None, viewers).await;
                                }
                            }
                            WatchAction::Disconnect { reason } => {
                                return Err(NiconicoSessionError::new(
                                    anyhow::anyhow!("サーバーから切断: {reason}"),
                                    stable(&received_chat),
                                ));
                            }
                            WatchAction::Ignore => {}
                        },
                        WsMessage::Ping(payload) => {
                            write.send(WsMessage::Pong(payload)).await.map_err(|e| {
                                NiconicoSessionError::new(e, stable(&received_chat))
                            })?;
                        }
                        WsMessage::Close(_) => {
                            return Ok(stable(&received_chat));
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    /// 視聴ページから WebSocket URL・タイトル・放送者 ID を得る。
    async fn bootstrap(&self, client: &reqwest::Client) -> anyhow::Result<WatchBootstrap> {
        let url = format!(
            "{}{}",
            self.endpoint_url("watchPageBaseUrl", WATCH_PAGE_BASE_URL),
            self.live_id
        );
        let html = client
            .get(&url)
            .timeout(HTTP_TIMEOUT)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let props = extract_embedded_data(&html)
            .ok_or_else(|| anyhow::anyhow!("embedded-data が見つからない(ページ構造変更?)"))?;

        let status = props
            .pointer("/program/status")
            .and_then(Value::as_str)
            .unwrap_or("");
        if !status.is_empty() && status != "ON_AIR" {
            anyhow::bail!("配信中ではない(status={status})");
        }

        let web_socket_url = props
            .pointer("/site/relive/webSocketUrl")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow::anyhow!("webSocketUrl が取得できない(会員限定/タイムシフト等の可能性)")
            })?
            .to_string();

        let title = props
            .pointer("/program/title")
            .and_then(Value::as_str)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let broadcaster_id = id_string(props.pointer("/program/supplier/programProviderId"));

        Ok(WatchBootstrap {
            web_socket_url,
            title,
            broadcaster_id,
        })
    }

    /// チップの接続状態表示用に live 状態と視聴者数を stats へ送る。
    async fn send_live_state(&self, live: bool, title: Option<String>, viewers: Option<u32>) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::Niconico,
            channel: self.live_id.clone(),
            concurrent_viewers: viewers,
            // `statistics.viewers` は同接ではなく来場者数（累計）。
            viewers_kind: ViewerCountKind::Cumulative,
            likes: None,
            title,
            live: Some(live),
            reactions_delta: None,
            full_snapshot: false,
        };
        let _ = tx.send(update).await;
    }
}

/// NDGR view API を追いかけ、MessageSegment ごとに購読タスクを起動する。
async fn ndgr_view_loop(
    client: reqwest::Client,
    view_uri: String,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
    live_id: String,
    broadcaster_id: Option<String>,
    received_chat: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut at = "now".to_string();
    let mut seen_segments: HashSet<String> = HashSet::new();
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        let resp = client
            .get(&view_uri)
            .query(&[("at", at.as_str())])
            .send()
            .await?
            .error_for_status()?;
        let mut stream = resp.bytes_stream();
        let mut buf = BytesMut::new();
        let mut next_at: Option<i64> = None;
        let mut last_chunk = tokio::time::Instant::now();

        loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Ok(()),
                _ = tokio::time::sleep_until(last_chunk + NDGR_IDLE_TIMEOUT) => {
                    anyhow::bail!(
                        "view ストリームが{}秒無通信",
                        NDGR_IDLE_TIMEOUT.as_secs()
                    );
                }
                c = stream.next() => c,
            };
            let Some(chunk) = chunk else { break };
            last_chunk = tokio::time::Instant::now();
            buf.extend_from_slice(&chunk?);
            while let Some(entry) = try_take_message::<ndgr::ChunkedEntry>(&mut buf)? {
                match entry.entry {
                    Some(ndgr::chunked_entry::Entry::Segment(seg)) => {
                        let uri = seg.uri;
                        if !uri.is_empty() && seen_segments.insert(uri.clone()) {
                            tokio::spawn(read_segment(
                                client.clone(),
                                uri,
                                tx.clone(),
                                cancel.child_token(),
                                live_id.clone(),
                                broadcaster_id.clone(),
                                received_chat.clone(),
                            ));
                        }
                    }
                    Some(ndgr::chunked_entry::Entry::Next(n)) => next_at = Some(n.at),
                    None => {}
                }
            }
        }

        match next_at {
            Some(n) => at = n.to_string(),
            None => anyhow::bail!("view ストリームが next 無しで終了"),
        }
    }
}

/// MessageSegment の ChunkedMessage ストリームを読み、コメントを流す。
///
/// セグメント単位の失敗はセッション全体を巻き込まず warn ログに留める
/// (次のセグメントは view loop 側で引き続き購読される)。
async fn read_segment(
    client: reqwest::Client,
    uri: String,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
    live_id: String,
    broadcaster_id: Option<String>,
    received_chat: Arc<AtomicBool>,
) {
    if let Err(e) = read_segment_inner(
        client,
        &uri,
        tx,
        cancel,
        &live_id,
        broadcaster_id.as_deref(),
        received_chat,
    )
    .await
    {
        tracing::warn!("niconico:{live_id} セグメント読みエラー: {e:#}");
    }
}

async fn read_segment_inner(
    client: reqwest::Client,
    uri: &str,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
    live_id: &str,
    broadcaster_id: Option<&str>,
    received_chat: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let resp = client.get(uri).send().await?.error_for_status()?;
    let mut stream = resp.bytes_stream();
    let mut buf = BytesMut::new();
    loop {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            c = stream.next() => c,
        };
        // セグメント期間が終わるとサーバー側が EOF する = 正常終了。
        let Some(chunk) = chunk else { return Ok(()) };
        buf.extend_from_slice(&chunk?);
        while let Some(m) = try_take_message::<ndgr::ChunkedMessage>(&mut buf)? {
            if let Some(msg) = chunked_to_chat(&m, live_id, broadcaster_id) {
                received_chat.store(true, Ordering::Relaxed);
                let _ = tx.send(msg);
            }
        }
    }
}

/// ChunkedMessage 1件を `ChatMessage` へ正規化する。コメント/ギフト以外は None。
fn chunked_to_chat(
    m: &ndgr::ChunkedMessage,
    live_id: &str,
    broadcaster_id: Option<&str>,
) -> Option<ChatMessage> {
    let ndgr::chunked_message::Payload::Message(nm) = m.payload.as_ref()?;

    let timestamp_ms = m
        .meta
        .as_ref()
        .and_then(|meta| meta.at.as_ref())
        .map(|t| t.seconds.saturating_mul(1000) + i64::from(t.nanos) / 1_000_000)
        .filter(|v| *v > 0)
        .unwrap_or_else(now_ms);
    let id = m
        .meta
        .as_ref()
        .map(|meta| meta.id.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(ChatMessage::new_id);

    match nm.data.as_ref()? {
        ndgr::nicolive_message::Data::Chat(c)
        | ndgr::nicolive_message::Data::OverflowedChat(c) => {
            let content = c.content.trim();
            if content.is_empty() {
                return None;
            }

            // 生ID(ログイン)を優先し、匿名は hashed ID で同一人物を追跡する。
            let (author_id, anonymous) = match (c.raw_user_id, c.hashed_user_id.as_deref()) {
                (Some(raw), _) => (raw.to_string(), false),
                (None, Some(h)) if !h.trim().is_empty() => (h.trim().to_string(), true),
                _ => (String::new(), true),
            };
            // 名前: コテハン > 匿名ハッシュ短縮 > 生ID。ニコ生は無名が多数派。
            let name = c
                .name
                .clone()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    if author_id.is_empty() {
                        "名無し".to_string()
                    } else if anonymous {
                        author_id.chars().take(7).collect()
                    } else {
                        author_id.clone()
                    }
                });

            let roles = Roles {
                broadcaster: !anonymous
                    && broadcaster_id.is_some_and(|b| b == author_id),
                ..Roles::default()
            };

            Some(ChatMessage {
                id,
                platform: Platform::Niconico,
                channel: live_id.to_string(),
                author: Author {
                    id: author_id,
                    name,
                    display_color: None,
                    badges: Vec::new(),
                    roles,
                },
                fragments: vec![Fragment::text(content.to_string())],
                kind: MessageKind::Normal,
                amount: None,
                timestamp_ms,
                raw: None,
                skip_tts: false,
            })
        }
        ndgr::nicolive_message::Data::Gift(g) => {
            let item = g.item_name.trim();
            let gift_msg = g.message.trim();
            let text = match (item.is_empty(), gift_msg.is_empty()) {
                (false, false) => format!("{item} {gift_msg}"),
                (false, true) => format!("{item} を贈りました"),
                (true, false) => gift_msg.to_string(),
                (true, true) => "ギフト".to_string(),
            };
            let author_name = {
                let n = g.advertiser_name.trim();
                if n.is_empty() {
                    "名無し".to_string()
                } else {
                    n.to_string()
                }
            };
            let author_id = g
                .advertiser_user_id
                .map(|v| v.to_string())
                .unwrap_or_else(|| author_name.clone());

            Some(ChatMessage {
                id,
                platform: Platform::Niconico,
                channel: live_id.to_string(),
                author: Author {
                    id: author_id,
                    name: author_name,
                    display_color: None,
                    badges: Vec::new(),
                    roles: Roles::default(),
                },
                fragments: vec![Fragment::text(text)],
                kind: MessageKind::Gift,
                amount: (g.point > 0).then(|| Amount {
                    value: g.point as f64,
                    currency: "pt".to_string(),
                    raw_text: format!("{}pt", g.point),
                }),
                timestamp_ms,
                raw: None,
                skip_tts: false,
            })
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
    use prost::Message;

    #[test]
    fn extracts_live_id_from_urls_and_raw_ids() {
        assert_eq!(
            extract_live_id("https://live.nicovideo.jp/watch/lv123456"),
            "lv123456"
        );
        assert_eq!(
            extract_live_id("https://live.nicovideo.jp/watch/lv123456?ref=top"),
            "lv123456"
        );
        assert_eq!(extract_live_id("https://nico.ms/lv123456"), "lv123456");
        assert_eq!(extract_live_id("  lv123456  "), "lv123456");
        // lv を含まない入力は素通し(co チャンネル等はリダイレクトに任せる)。
        assert_eq!(
            extract_live_id("https://live.nicovideo.jp/watch/co1234"),
            "co1234"
        );
        // 単語中の "lv"(直前が英数字)は誤検出しない。
        assert_eq!(extract_live_id("solve42"), "solve42");
    }

    #[test]
    fn extracts_embedded_data_from_watch_html() {
        let html = concat!(
            "<html><body>",
            r#"<script id="embedded-data" data-props="{&quot;site&quot;:{&quot;relive&quot;:{&quot;webSocketUrl&quot;:&quot;wss://example/ws?t=a&amp;b=c&quot;}},&quot;program&quot;:{&quot;title&quot;:&quot;テスト配信&quot;,&quot;status&quot;:&quot;ON_AIR&quot;}}"></script>"#,
            "</body></html>"
        );
        let props = extract_embedded_data(html).expect("embedded data");
        assert_eq!(
            props.pointer("/site/relive/webSocketUrl").and_then(Value::as_str),
            Some("wss://example/ws?t=a&b=c")
        );
        assert_eq!(
            props.pointer("/program/title").and_then(Value::as_str),
            Some("テスト配信")
        );
        assert!(extract_embedded_data("<html>no data</html>").is_none());
    }

    #[test]
    fn parses_watch_messages() {
        assert_eq!(parse_watch_message(r#"{"type":"ping"}"#), WatchAction::Pong);
        assert_eq!(
            parse_watch_message(r#"{"type":"seat","data":{"keepIntervalSec":25}}"#),
            WatchAction::Seat { keep_interval_sec: 25 }
        );
        assert_eq!(
            parse_watch_message(
                r#"{"type":"messageServer","data":{"viewUri":"https://mpn.example/v4/x","vposBaseTime":"2026-01-01T00:00:00+09:00"}}"#
            ),
            WatchAction::MessageServer {
                view_uri: "https://mpn.example/v4/x".to_string()
            }
        );
        assert_eq!(
            parse_watch_message(r#"{"type":"statistics","data":{"viewers":1234,"comments":56}}"#),
            WatchAction::Statistics { viewers: Some(1234) }
        );
        assert_eq!(
            parse_watch_message(r#"{"type":"disconnect","data":{"reason":"END_PROGRAM"}}"#),
            WatchAction::Disconnect { reason: "END_PROGRAM".to_string() }
        );
        assert_eq!(
            parse_watch_message(r#"{"type":"schedule","data":{}}"#),
            WatchAction::Ignore
        );
        assert_eq!(parse_watch_message("not json"), WatchAction::Ignore);
    }

    #[test]
    fn takes_length_delimited_messages_incrementally() {
        let entry = ndgr::ChunkedEntry {
            entry: Some(ndgr::chunked_entry::Entry::Next(ndgr::ReadyForNext {
                at: 1_700_000_000,
            })),
        };
        let mut wire = Vec::new();
        entry.encode_length_delimited(&mut wire).unwrap();

        // 1バイトずつ与えても、揃うまで None のまま壊れない。
        let mut buf = BytesMut::new();
        let mut decoded = None;
        for b in &wire {
            buf.extend_from_slice(&[*b]);
            if let Some(m) = try_take_message::<ndgr::ChunkedEntry>(&mut buf).unwrap() {
                decoded = Some(m);
            }
        }
        assert_eq!(decoded, Some(entry));
        assert!(buf.is_empty());
    }

    /// prost の oneof 部分定義: 未定義タグ(state=4 等)は None に劣化する。
    #[test]
    fn unknown_payload_degrades_to_none() {
        // ChunkedMessage { meta.id="x", 未知の field 4 (state 相当のダミー) }
        let mut wire = Vec::new();
        // meta (tag1, len-delimited): Meta { id: "x" }
        wire.extend_from_slice(&[0x0a, 0x03, 0x0a, 0x01, b'x']);
        // 未知 field 4 (len-delimited): 空メッセージ
        wire.extend_from_slice(&[0x22, 0x00]);
        let m = ndgr::ChunkedMessage::decode(&wire[..]).expect("decode");
        assert_eq!(m.meta.as_ref().map(|x| x.id.as_str()), Some("x"));
        assert!(m.payload.is_none());
        assert!(chunked_to_chat(&m, "lv1", None).is_none());
    }

    fn chat_message(chat: ndgr::Chat, meta_id: &str, at_sec: i64) -> ndgr::ChunkedMessage {
        ndgr::ChunkedMessage {
            meta: Some(ndgr::Meta {
                id: meta_id.to_string(),
                at: Some(ndgr::Timestamp {
                    seconds: at_sec,
                    nanos: 500_000_000,
                }),
            }),
            payload: Some(ndgr::chunked_message::Payload::Message(
                ndgr::NicoliveMessage {
                    data: Some(ndgr::nicolive_message::Data::Chat(chat)),
                },
            )),
        }
    }

    #[test]
    fn chat_normalizes_to_message() {
        let m = chat_message(
            ndgr::Chat {
                content: "こんにちは".to_string(),
                name: None,
                raw_user_id: Some(4242),
                hashed_user_id: None,
            },
            "msg-1",
            1_700_000_000,
        );
        let msg = chunked_to_chat(&m, "lv123", Some("4242")).expect("chat");
        assert_eq!(msg.platform, Platform::Niconico);
        assert_eq!(msg.channel, "lv123");
        assert_eq!(msg.id, "msg-1");
        assert_eq!(msg.author.id, "4242");
        assert_eq!(msg.author.name, "4242");
        assert!(msg.author.roles.broadcaster);
        assert_eq!(msg.plain_text(), "こんにちは");
        assert_eq!(msg.timestamp_ms, 1_700_000_000_500);

        // 匿名(184)コメント: hashed ID の短縮が名前になり、broadcaster は立たない。
        let m = chat_message(
            ndgr::Chat {
                content: "わこつ".to_string(),
                name: None,
                raw_user_id: None,
                hashed_user_id: Some("aBcDeFgHiJk".to_string()),
            },
            "",
            0,
        );
        let msg = chunked_to_chat(&m, "lv123", Some("aBcDeFgHiJk")).expect("chat");
        assert_eq!(msg.author.id, "aBcDeFgHiJk");
        assert_eq!(msg.author.name, "aBcDeFg");
        assert!(!msg.author.roles.broadcaster);
        assert!(!msg.id.is_empty());
        assert!(msg.timestamp_ms > 0);

        // 空コメントは捨てる。
        let m = chat_message(
            ndgr::Chat {
                content: "   ".to_string(),
                name: None,
                raw_user_id: None,
                hashed_user_id: None,
            },
            "m",
            1,
        );
        assert!(chunked_to_chat(&m, "lv123", None).is_none());
    }

    #[test]
    fn gift_normalizes_to_message() {
        let m = ndgr::ChunkedMessage {
            meta: Some(ndgr::Meta {
                id: "gift-1".to_string(),
                at: Some(ndgr::Timestamp {
                    seconds: 1_700_000_000,
                    nanos: 0,
                }),
            }),
            payload: Some(ndgr::chunked_message::Payload::Message(
                ndgr::NicoliveMessage {
                    data: Some(ndgr::nicolive_message::Data::Gift(ndgr::Gift {
                        advertiser_user_id: Some(777),
                        advertiser_name: "太郎".to_string(),
                        point: 500,
                        message: "応援してます".to_string(),
                        item_name: "花束".to_string(),
                    })),
                },
            )),
        };
        let msg = chunked_to_chat(&m, "lv123", None).expect("gift");
        assert_eq!(msg.kind, MessageKind::Gift);
        assert_eq!(msg.author.id, "777");
        assert_eq!(msg.author.name, "太郎");
        assert_eq!(msg.plain_text(), "花束 応援してます");
        let amount = msg.amount.expect("amount");
        assert_eq!(amount.value, 500.0);
        assert_eq!(amount.currency, "pt");
        assert_eq!(amount.raw_text, "500pt");
    }
}
