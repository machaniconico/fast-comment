//! Niconico Live NDGR Source.
//!
//! 2024-08 以降のニコ生コメントは、視聴ページの embedded-data から
//! watch WebSocket URL を取得し、watch WS で座席を維持しながら
//! NDGR HTTP streaming API の Length-Delimited Protobuf を読む。

use std::collections::HashSet;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context as _};
use futures_util::{SinkExt, StreamExt};
use prost::Message as _;
use serde_json::{json, Value};
use tokio::sync::{broadcast, mpsc};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::model::{Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};

const WATCH_BASE_URL: &str = "https://live.nicovideo.jp/watch/";
const NICONICO_ORIGIN: &str = "https://live.nicovideo.jp";
const USER_AGENT: &str = "fast-comment/0.1 niconico-source";
const DEFAULT_KEEP_SEAT_INTERVAL_SECS: u64 = 30;
const MAX_PROTOBUF_FRAME_LEN: usize = 8 * 1024 * 1024;
/// View/Segment ストリームのチャンク間(無通信)タイムアウト。
/// reqwest 0.12 は既定タイムアウト無し。half-open ストール(TCPは生きているが
/// サーバが送信を止める)を検知して Err にし、run() の Backoff 再接続へ落とす。
/// ストリームが健全に流れている間はチャンク毎に reset するので誤発火しない。
const STREAM_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// ニコ生番組1件を購読する Source。
pub struct NiconicoSource {
    /// `lv` から始まるニコ生番組 ID。
    live_id: String,
}

impl NiconicoSource {
    pub fn new(identifier: String) -> Self {
        let live_id =
            normalize_live_id(&identifier).unwrap_or_else(|| identifier.trim().to_string());
        NiconicoSource { live_id }
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
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>,
    > {
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
                        if stable {
                            backoff.reset();
                        }
                    }
                    Err(e) => {
                        tracing::warn!("niconico:{} 接続エラー: {e:#}", self.live_id);
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
    async fn connect_and_listen(
        &self,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> anyhow::Result<bool> {
        // connect_timeout で TCP/TLS ハンドシェイクを縛る(全リクエスト共通)。
        // blanket な .timeout() は付けない — NDGR の長期ストリーミング GET を
        // 健全でも打ち切ってしまうため。ストリームの停滞は STREAM_IDLE_TIMEOUT で見る。
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(15))
            .build()
            .context("reqwest client 初期化失敗")?;

        let Some(ws_url) = fetch_watch_websocket_url(&client, &self.live_id).await? else {
            tracing::info!(
                "niconico:{} embedded-data に webSocketUrl がありません",
                self.live_id
            );
            return Ok(false);
        };

        let mut request = ws_url
            .as_str()
            .into_client_request()
            .context("watch WebSocket request 作成失敗")?;
        request
            .headers_mut()
            .insert("Origin", HeaderValue::from_static(NICONICO_ORIGIN));

        let (ws_stream, _resp) = tokio_tungstenite::connect_async(request)
            .await
            .context("watch WebSocket 接続失敗")?;
        let (mut write, mut read) = ws_stream.split();

        write.send(ws_json(start_watching_payload())).await?;
        tracing::info!("niconico:{} startWatching 送信完了", self.live_id);

        let session_cancel = cancel.child_token();
        let (view_done_tx, mut view_done_rx) = mpsc::channel::<(u64, anyhow::Result<bool>)>(4);
        let mut current_view_uri: Option<String> = None;
        let mut current_view_generation = 0u64;
        let mut current_view_cancel: Option<CancellationToken> = None;
        let mut stable = false;

        let mut keep_delay = Duration::from_secs(DEFAULT_KEEP_SEAT_INTERVAL_SECS);
        let mut keep_sleep = Box::pin(tokio::time::sleep(keep_delay));

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    session_cancel.cancel();
                    if let Some(view_cancel) = current_view_cancel.take() {
                        view_cancel.cancel();
                    }
                    let _ = write.send(WsMessage::Close(None)).await;
                    return Ok(true);
                }
                _ = &mut keep_sleep => {
                    write.send(ws_json(json!({"type": "keepSeat"}))).await?;
                    keep_sleep.as_mut().reset(Instant::now() + keep_delay);
                }
                done = view_done_rx.recv() => {
                    let Some((generation, result)) = done else {
                        continue;
                    };
                    if generation != current_view_generation {
                        continue;
                    }
                    match result {
                        Ok(seen_message) => {
                            stable |= seen_message;
                            if !cancel.is_cancelled() && !session_cancel.is_cancelled() {
                                return Ok(stable);
                            }
                        }
                        Err(e) => {
                            session_cancel.cancel();
                            return Err(e).context("NDGR view stream 終了");
                        }
                    }
                }
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(m)) => m,
                        Some(Err(e)) => {
                            session_cancel.cancel();
                            return Err(e.into());
                        }
                        None => {
                            session_cancel.cancel();
                            return Ok(stable);
                        }
                    };

                    match msg {
                        WsMessage::Text(text) => {
                            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                                continue;
                            };

                            if let Some(interval) = keep_interval_secs(&value) {
                                keep_delay = Duration::from_secs(interval);
                                keep_sleep.as_mut().reset(Instant::now() + keep_delay);
                            }

                            match value.get("type").and_then(Value::as_str).unwrap_or("") {
                                "ping" => {
                                    write.send(ws_json(json!({"type": "pong"}))).await?;
                                }
                                "messageServer" => {
                                    let Some(view_uri) = value
                                        .pointer("/data/viewUri")
                                        .and_then(Value::as_str)
                                        .filter(|s| !s.is_empty())
                                        .map(ToOwned::to_owned)
                                    else {
                                        continue;
                                    };

                                    if current_view_uri.as_deref() == Some(view_uri.as_str()) {
                                        continue;
                                    }

                                    stable = true;
                                    if let Some(view_cancel) = current_view_cancel.take() {
                                        view_cancel.cancel();
                                    }
                                    current_view_generation =
                                        current_view_generation.wrapping_add(1);
                                    current_view_uri = Some(view_uri.clone());

                                    let view_cancel = session_cancel.child_token();
                                    current_view_cancel = Some(view_cancel.clone());
                                    let task_client = client.clone();
                                    let task_tx = tx.clone();
                                    let task_live_id = self.live_id.clone();
                                    let done_tx = view_done_tx.clone();
                                    let generation = current_view_generation;
                                    tauri::async_runtime::spawn(async move {
                                        let result = stream_view_loop(
                                            task_client,
                                            view_uri,
                                            task_live_id,
                                            task_tx,
                                            view_cancel,
                                        )
                                        .await;
                                        let _ = done_tx.send((generation, result)).await;
                                    });
                                }
                                "disconnect" => {
                                    session_cancel.cancel();
                                    return Ok(stable);
                                }
                                "error" => {
                                    tracing::warn!(
                                        "niconico:{} watch WS error: {value}",
                                        self.live_id
                                    );
                                }
                                _ => {}
                            }
                        }
                        WsMessage::Ping(payload) => {
                            write.send(WsMessage::Pong(payload)).await?;
                        }
                        WsMessage::Close(_) => {
                            session_cancel.cancel();
                            return Ok(stable);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

async fn fetch_watch_websocket_url(
    client: &reqwest::Client,
    live_id: &str,
) -> anyhow::Result<Option<String>> {
    let watch_url = format!("{WATCH_BASE_URL}{live_id}");
    // 視聴ページ取得は非ストリーミングなので全体タイムアウトで縛ってよい
    // (本文 text() まで含めて 15s)。停滞したHTML取得で関数がハングしない。
    let response = client
        .get(&watch_url)
        .timeout(Duration::from_secs(15))
        .send()
        .await?;
    if !response.status().is_success() {
        tracing::warn!("niconico:{live_id} 視聴ページ取得失敗: {}", response.status());
        return Ok(None);
    }

    let html = response.text().await?;
    let Some(props) = extract_embedded_data_props(&html) else {
        return Ok(None);
    };
    let Ok(value) = serde_json::from_str::<Value>(&props) else {
        tracing::warn!("niconico:{live_id} embedded-data JSON parse 失敗");
        return Ok(None);
    };

    Ok(value
        .pointer("/site/relive/webSocketUrl")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned))
}

async fn stream_view_loop(
    client: reqwest::Client,
    view_uri: String,
    live_id: String,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
) -> anyhow::Result<bool> {
    let (segment_done_tx, mut segment_done_rx) =
        mpsc::channel::<(String, anyhow::Result<bool>)>(32);
    let mut active_segments = HashSet::new();
    let mut at = "now".to_string();
    let mut seen_message = false;

    loop {
        if cancel.is_cancelled() {
            return Ok(seen_message);
        }

        let previous_at = at.clone();
        let url = view_url_with_at(&view_uri, &at)?;
        read_length_delimited_stream(&client, &url, &cancel, |frame| {
            // View フレームも1件 decode 失敗で View ストリーム全体を巻き込まず log-and-skip。
            let entry = match ChunkedEntry::decode(frame.as_slice()) {
                Ok(entry) => entry,
                Err(e) => {
                    tracing::warn!("niconico:{live_id} ChunkedEntry decode skip: {e}");
                    return Ok(());
                }
            };
            match entry.entry {
                Some(chunked_entry::Entry::Segment(segment)) => {
                    if segment.uri.is_empty() || active_segments.contains(&segment.uri) {
                        return Ok(());
                    }

                    let segment_uri = resolve_uri(&view_uri, &segment.uri)?;
                    active_segments.insert(segment.uri.clone());

                    let segment_client = client.clone();
                    let segment_tx = tx.clone();
                    let segment_live_id = live_id.clone();
                    let segment_cancel = cancel.child_token();
                    let done_tx = segment_done_tx.clone();
                    let segment_key = segment.uri;
                    tauri::async_runtime::spawn(async move {
                        let result = stream_segment(
                            segment_client,
                            segment_uri,
                            segment_live_id,
                            segment_tx,
                            segment_cancel,
                        )
                        .await;
                        let _ = done_tx.send((segment_key, result)).await;
                    });
                }
                Some(chunked_entry::Entry::Next(next)) => {
                    if next.at > 0 {
                        at = next.at.to_string();
                    }
                }
                None => {}
            }
            Ok(())
        })
        .await?;

        while let Ok((uri, result)) = segment_done_rx.try_recv() {
            active_segments.remove(&uri);
            match result {
                Ok(segment_seen) => seen_message |= segment_seen,
                Err(e) => tracing::warn!("niconico:{live_id} segment stream error: {e:#}"),
            }
        }

        if at == previous_at {
            tokio::select! {
                _ = cancel.cancelled() => return Ok(seen_message),
                _ = tokio::time::sleep(Duration::from_millis(500)) => {}
            }
        }
    }
}

async fn stream_segment(
    client: reqwest::Client,
    segment_uri: String,
    live_id: String,
    tx: broadcast::Sender<ChatMessage>,
    cancel: CancellationToken,
) -> anyhow::Result<bool> {
    let mut seen_message = false;
    read_length_delimited_stream(&client, &segment_uri, &cancel, |frame| {
        // 1フレームが decode 不能でも、その1件だけ捨てて後続フレームを読み続ける。
        // (寛容パース原則: 欠落/想定外は巻き込まず None 劣化。直下の to_chat_message
        //  も同様に skip しており、ここで ? で巻き添えにするのは内部不整合だった。)
        let chunked = match ChunkedMessage::decode(frame.as_slice()) {
            Ok(chunked) => chunked,
            Err(e) => {
                tracing::warn!("niconico:{live_id} ChunkedMessage decode skip: {e}");
                return Ok(());
            }
        };
        let Some(msg) = chunked.to_chat_message(&live_id) else {
            return Ok(());
        };
        seen_message |= tx.send(msg).is_ok();
        Ok(())
    })
    .await?;
    Ok(seen_message)
}

async fn read_length_delimited_stream<F>(
    client: &reqwest::Client,
    url: &str,
    cancel: &CancellationToken,
    mut on_frame: F,
) -> anyhow::Result<()>
where
    F: FnMut(Vec<u8>) -> anyhow::Result<()>,
{
    // send().await(接続+ヘッダ待ち)と error_for_status も cancel に乗せ、
    // 停止要求(アプリ終了/チャンネル削除)へ pre-stream 段階でも即応する。
    let response = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        r = client
            .get(url)
            .header("Origin", NICONICO_ORIGIN)
            .header("Referer", format!("{NICONICO_ORIGIN}/"))
            .send() => r?.error_for_status()?,
    };
    let mut stream = response.bytes_stream();
    let mut buffer = Vec::with_capacity(8192);
    // チャンク間の無通信が STREAM_IDLE_TIMEOUT を超えたらストール扱いで Err。
    // 健全に流れている間はチャンク受信毎に reset するので誤発火しない。
    let mut idle = Box::pin(tokio::time::sleep(STREAM_IDLE_TIMEOUT));

    loop {
        tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            _ = &mut idle => {
                return Err(anyhow!("NDGR stream idle timeout ({STREAM_IDLE_TIMEOUT:?})"));
            }
            chunk = stream.next() => {
                let Some(chunk) = chunk else {
                    return Ok(());
                };
                idle.as_mut().reset(Instant::now() + STREAM_IDLE_TIMEOUT);
                buffer.extend_from_slice(&chunk?);
                while let Some(frame) = pop_length_delimited_frame(&mut buffer)? {
                    on_frame(frame)?;
                }
            }
        }
    }
}

fn pop_length_delimited_frame(buffer: &mut Vec<u8>) -> anyhow::Result<Option<Vec<u8>>> {
    let Some((len, prefix_len)) = try_read_varint(buffer)? else {
        return Ok(None);
    };
    if len > MAX_PROTOBUF_FRAME_LEN {
        return Err(anyhow!("protobuf frame too large: {len} bytes"));
    }
    let total_len = prefix_len
        .checked_add(len)
        .ok_or_else(|| anyhow!("protobuf frame length overflow"))?;
    if buffer.len() < total_len {
        return Ok(None);
    }
    let frame = buffer[prefix_len..total_len].to_vec();
    buffer.drain(..total_len);
    Ok(Some(frame))
}

fn try_read_varint(buffer: &[u8]) -> anyhow::Result<Option<(usize, usize)>> {
    let mut value = 0usize;
    for (i, byte) in buffer.iter().copied().enumerate().take(10) {
        let shift = i * 7;
        if shift >= usize::BITS as usize {
            return Err(anyhow!("protobuf frame length varint overflow"));
        }
        value |= usize::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(Some((value, i + 1)));
        }
    }
    if buffer.len() >= 10 {
        Err(anyhow!("protobuf frame length varint too long"))
    } else {
        Ok(None)
    }
}

fn extract_embedded_data_props(html: &str) -> Option<String> {
    let id_pos = html
        .find("id=\"embedded-data\"")
        .or_else(|| html.find("id='embedded-data'"))?;
    let script_start = html[..id_pos].rfind("<script")?;
    let tag_end = id_pos + html[id_pos..].find('>')?;
    let tag = &html[script_start..tag_end];
    let raw = extract_attr(tag, "data-props")?;
    Some(decode_html_entities(&raw))
}

fn extract_attr(tag: &str, name: &str) -> Option<String> {
    let attr_pos = tag.find(name)?;
    let after_name = &tag[attr_pos + name.len()..];
    let eq_pos = after_name.find('=')?;
    let after_eq = after_name[eq_pos + 1..].trim_start();
    let mut chars = after_eq.chars();
    let quote = chars.next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    let value = chars.as_str();
    let end = value.find(quote)?;
    Some(value[..end].to_string())
}

fn decode_html_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#34;", "\"")
        .replace("&#x22;", "\"")
        .replace("&#X22;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn normalize_live_id(input: &str) -> Option<String> {
    let trimmed = input.trim();
    let candidate = if let Some(pos) = trimmed.find("/watch/") {
        &trimmed[pos + "/watch/".len()..]
    } else {
        trimmed
    };
    let live_id = candidate
        .split(['?', '#', '/'])
        .next()
        .unwrap_or("")
        .trim()
        .to_lowercase();
    if live_id.starts_with("lv") && live_id[2..].chars().all(|c| c.is_ascii_digit()) {
        Some(live_id)
    } else {
        None
    }
}

fn start_watching_payload() -> Value {
    json!({
        "type": "startWatching",
        "data": {
            "stream": {
                "quality": "abr",
                "protocol": "hls",
                "latency": "low",
                "chasePlay": false
            },
            "room": {
                "protocol": "webSocket",
                "commentable": true
            },
            "reconnect": false
        }
    })
}

fn keep_interval_secs(value: &Value) -> Option<u64> {
    [
        "/data/keepIntervalSec",
        "/data/seat/keepIntervalSec",
        "/seat/keepIntervalSec",
    ]
    .iter()
    .find_map(|path| value.pointer(path).and_then(Value::as_u64))
    .filter(|sec| *sec > 0)
}

fn ws_json(value: Value) -> WsMessage {
    WsMessage::Text(value.to_string().into())
}

fn view_url_with_at(view_uri: &str, at: &str) -> anyhow::Result<String> {
    let mut url = reqwest::Url::parse(view_uri)?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .filter(|(key, _)| key != "at")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    {
        let mut query = url.query_pairs_mut();
        for (key, value) in pairs {
            query.append_pair(&key, &value);
        }
        query.append_pair("at", at);
    }
    Ok(url.to_string())
}

fn resolve_uri(base: &str, uri: &str) -> anyhow::Result<String> {
    if uri.starts_with("http://") || uri.starts_with("https://") {
        return Ok(uri.to_string());
    }
    Ok(reqwest::Url::parse(base)?.join(uri)?.to_string())
}

fn now_ms() -> i64 {
    let epoch_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    i64::try_from(epoch_ms).unwrap_or(i64::MAX)
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ChunkedEntry {
    #[prost(oneof = "chunked_entry::Entry", tags = "1, 4")]
    entry: Option<chunked_entry::Entry>,
}

mod chunked_entry {
    #[derive(Clone, PartialEq, ::prost::Oneof)]
    pub enum Entry {
        #[prost(message, tag = "1")]
        Segment(super::MessageSegment),
        #[prost(message, tag = "4")]
        Next(super::ReadyForNext),
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ReadyForNext {
    #[prost(int64, tag = "1")]
    at: i64,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct MessageSegment {
    #[prost(string, tag = "3")]
    uri: String,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ChunkedMessage {
    #[prost(message, optional, tag = "1")]
    meta: Option<ChunkedMessageMeta>,
    #[prost(message, optional, tag = "2")]
    message: Option<NicoliveMessage>,
}

impl ChunkedMessage {
    fn to_chat_message(&self, live_id: &str) -> Option<ChatMessage> {
        let chat = self.message.as_ref()?.chat()?;
        let content = chat.content.clone();
        let meta = self.meta.as_ref();
        let id = meta
            .and_then(|m| (!m.id.is_empty()).then(|| m.id.clone()))
            .unwrap_or_else(ChatMessage::new_id);
        let timestamp_ms = meta
            .and_then(|m| m.at.as_ref())
            .map(proto_timestamp_ms)
            .unwrap_or_else(now_ms);

        Some(ChatMessage {
            id,
            platform: Platform::Niconico,
            channel: live_id.to_string(),
            author: Author {
                id: chat.user_id(),
                name: chat.name.clone().unwrap_or_default(),
                display_color: None,
                badges: Vec::<Badge>::new(),
                roles: Roles::default(),
            },
            fragments: vec![Fragment::text(content)],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms,
            raw: Some(json!({
                "vpos": chat.vpos,
                "no": chat.no,
                "date": timestamp_ms,
            })),
            skip_tts: false,
        })
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ChunkedMessageMeta {
    #[prost(string, tag = "1")]
    id: String,
    #[prost(message, optional, tag = "2")]
    at: Option<ProtoTimestamp>,
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct ProtoTimestamp {
    #[prost(int64, tag = "1")]
    seconds: i64,
    #[prost(int32, tag = "2")]
    nanos: i32,
}

fn proto_timestamp_ms(ts: &ProtoTimestamp) -> i64 {
    ts.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(ts.nanos / 1_000_000))
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct NicoliveMessage {
    #[prost(message, optional, tag = "1")]
    chat: Option<NicoliveChat>,
    #[prost(message, optional, tag = "20")]
    overflowed_chat: Option<NicoliveChat>,
}

impl NicoliveMessage {
    fn chat(&self) -> Option<&NicoliveChat> {
        self.chat.as_ref().or(self.overflowed_chat.as_ref())
    }
}

#[derive(Clone, PartialEq, ::prost::Message)]
struct NicoliveChat {
    #[prost(string, tag = "1")]
    content: String,
    #[prost(string, optional, tag = "2")]
    name: Option<String>,
    #[prost(int32, tag = "3")]
    vpos: i32,
    #[prost(int64, optional, tag = "5")]
    raw_user_id: Option<i64>,
    #[prost(string, optional, tag = "6")]
    hashed_user_id: Option<String>,
    #[prost(int32, tag = "8")]
    no: i32,
}

impl NicoliveChat {
    fn user_id(&self) -> String {
        if let Some(raw_user_id) = self.raw_user_id {
            return raw_user_id.to_string();
        }
        self.hashed_user_id.clone().unwrap_or_default()
    }
}
