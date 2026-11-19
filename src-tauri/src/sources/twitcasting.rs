//! ツイキャス(非公式 WebSocket)コメント Source。
//!
//! ログイン不要の視聴ページからたどる:
//! 1. GET `https://twitcasting.tv/<screen_id>` の HTML から
//!    `data-movie-id` / `data-is-onlive` を読む。
//! 2. 配信中なら GET `userajax.php?c=listall…` で直近コメントを取得する
//!    (読み上げ対象外の既読として流し、WS 分とは id で重複排除する)。
//! 3. POST `eventpubsuburl.php` でトークン入り WS URL を得て、`&gift=1` を
//!    付けて接続する。ping は送らない(サーバー側 ping には pong を返す)。
//!
//! 応答は `serde_json::Value` と正規表現で寛容に読み、欠落は None に劣化させる。
//! エンドポイントと正規表現は `TwitcastingOverrides` で差し替えられる。
//! eventpubsuburl が返すトークン入り URL は `SecretWsUrl` で包み、
//! ログ・エラー文・Debug/Display にはホスト名までしか出さない。

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::model::{Author, Badge, ChatMessage, Fragment, MessageKind, Platform, Roles};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

const USER_PAGE_BASE_URL: &str = "https://twitcasting.tv/";
const EVENT_PUBSUB_URL: &str = "https://twitcasting.tv/eventpubsuburl.php";
const COMMENT_HISTORY_URL: &str = "https://twitcasting.tv/userajax.php";
/// 既存 Source と同じ一般ブラウザ文字列。cookie・ログイン情報は使わない。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// 未配信・取得不能時の HTML 再取得間隔(±20% の揺らぎを付ける)。
const OFFLINE_INTERVAL: Duration = Duration::from_secs(30);
/// 存在しないユーザー(302)の再試行間隔。
const NOT_FOUND_RETRY: Duration = Duration::from_secs(300);
/// 90 秒フレームが来なければ配信状態を HTML で確認し直す。
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HISTORY_LIMIT: usize = 20;
const SEEN_LIMIT: usize = 4096;
/// 匿名コメントは複数人が同じ `c:tw*` を共有するため、固定 ID に寄せる。
/// ユーザー単位 NG・色分けに使わせない意図を anonymous バッジで示す。
const ANONYMOUS_AUTHOR_ID: &str = "tc:anonymous";

pub const DEFAULT_MOVIE_ID_PATTERN: &str = r#"data-movie-id="(\d+)""#;
pub const DEFAULT_MOVIE_ID_FALLBACK_PATTERN: &str = r#"var\s+movieid\s*=\s*"(\d+)""#;
pub const DEFAULT_IS_ONLIVE_PATTERN: &str = r#"data-is-onlive="(true|false)""#;

const NOT_FOUND_NOTICE: &str = "ツイキャスのユーザーが見つかりません";
const UNAVAILABLE_NOTICE: &str = "この配信は取得できません";
const INVALID_ID_NOTICE: &str = "ツイキャスの配信者 ID を解釈できません";

/// ツイキャスの非公式経路で使うエンドポイント・正規表現の上書き。
///
/// `config.json` の `twitcastingOverrides` から読む想定(配線は US-002)。
/// 空文字・未設定のキーは既定値を使う。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default, rename_all = "camelCase")]
pub struct TwitcastingOverrides {
    /// キー: `userPageBaseUrl` / `eventPubsubUrl` / `commentHistoryUrl`
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub endpoints: HashMap<String, String>,
    /// キー: `movieId` / `movieIdFallback` / `isOnlive`
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub patterns: HashMap<String, String>,
}

impl TwitcastingOverrides {
    fn endpoint(&self, key: &str, default: &str) -> String {
        self.endpoints
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    }

    fn pattern(&self, key: &str, default: &str) -> String {
        self.patterns
            .get(key)
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(default)
            .to_string()
    }
}

/// URL・生 ID・接頭辞付き ID から screen_id を取り出す。
///
/// `https://twitcasting.tv/<id>`、`…/<id>/movie/123`、`twitcasting.tv/<id>`、
/// `c:abc` / `g:123`、裸の ID を受ける。他ホスト・空・不正文字は None。
/// 返り値は大小文字を保つ(API 呼び出し用)。
pub fn extract_screen_id(input: &str) -> Option<String> {
    let raw = input.trim();
    if raw.is_empty() {
        return None;
    }

    let lower = raw.to_ascii_lowercase();
    let path = if lower.starts_with("http://") || lower.starts_with("https://") {
        let url = url::Url::parse(raw).ok()?;
        if !is_allowed_host(url.host_str()?) {
            return None;
        }
        url.path().to_string()
    } else if raw.starts_with("//") {
        let url = url::Url::parse(&format!("https:{raw}")).ok()?;
        if !is_allowed_host(url.host_str()?) {
            return None;
        }
        url.path().to_string()
    } else if raw.contains("://") {
        return None;
    } else if let Some(rest) = strip_allowed_host_prefix(raw) {
        rest.to_string()
    } else if raw.contains('/') {
        // スキーム無しの他ホスト・パスは受け付けない。
        return None;
    } else {
        format!("/{raw}")
    };

    let path = path.split(['?', '#']).next().unwrap_or("");
    let segment = path.split('/').find(|s| !s.is_empty())?;
    let segment = decode_percent_colon(segment);
    validate_screen_id(&segment)
}

const ALLOWED_HOSTS: [&str; 5] = [
    "twitcasting.tv",
    "www.twitcasting.tv",
    "ssl.twitcasting.tv",
    "en.twitcasting.tv",
    "ja.twitcasting.tv",
];

fn is_allowed_host(host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    ALLOWED_HOSTS.contains(&host.as_str())
}

fn strip_allowed_host_prefix(input: &str) -> Option<&str> {
    let lower = input.to_ascii_lowercase();
    for host in ALLOWED_HOSTS {
        let prefix = format!("{host}/");
        if lower.starts_with(&prefix) {
            return Some(&input[prefix.len()..]);
        }
    }
    None
}

fn decode_percent_colon(s: &str) -> String {
    s.replace("%3A", ":").replace("%3a", ":")
}

fn validate_screen_id(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let (prefix, body) = match raw.split_once(':') {
        Some((prefix, body)) => (Some(prefix), body),
        None => (None, raw),
    };
    if let Some(prefix) = prefix {
        let known = ["c", "g", "f", "ig"]
            .iter()
            .any(|p| prefix.eq_ignore_ascii_case(p));
        if !known {
            return None;
        }
    }
    if body.is_empty() || body.len() > 32 {
        return None;
    }
    if !body.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_') {
        return None;
    }
    Some(raw.to_string())
}

/// 視聴ページ HTML から読み取った配信状態。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserPage {
    pub movie_id: Option<String>,
    pub is_onlive: Option<bool>,
}

#[derive(Debug)]
struct UserPagePatterns {
    movie_id: Regex,
    movie_id_fallback: Regex,
    is_onlive: Regex,
}

impl UserPagePatterns {
    fn from_overrides(overrides: &TwitcastingOverrides) -> Self {
        fn build(overrides: &TwitcastingOverrides, key: &str, default: &str) -> Regex {
            let pattern = overrides.pattern(key, default);
            Regex::new(&pattern)
                .or_else(|_| Regex::new(default))
                .unwrap_or_else(|_| Regex::new(r#"(?!)"#).expect("empty regex"))
        }
        UserPagePatterns {
            movie_id: build(overrides, "movieId", DEFAULT_MOVIE_ID_PATTERN),
            movie_id_fallback: build(
                overrides,
                "movieIdFallback",
                DEFAULT_MOVIE_ID_FALLBACK_PATTERN,
            ),
            is_onlive: build(overrides, "isOnlive", DEFAULT_IS_ONLIVE_PATTERN),
        }
    }
}

impl UserPagePatterns {
    fn parse(&self, html: &str) -> Option<UserPage> {
        let movie_id = self
            .movie_id
            .captures(html)
            .or_else(|| self.movie_id_fallback.captures(html))
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().to_string())?;
        let is_onlive = self
            .is_onlive
            .captures(html)
            .and_then(|c| c.get(1))
            .map(|m| m.as_str().eq_ignore_ascii_case("true"));
        Some(UserPage {
            movie_id: Some(movie_id),
            is_onlive,
        })
    }
}

/// 既定パターンで HTML を解析する。
pub fn parse_user_page(html: &str) -> Option<UserPage> {
    UserPagePatterns::from_overrides(&TwitcastingOverrides::default()).parse(html)
}

/// eventpubsuburl.php の応答から WS URL を取り出す。欠落・空・非 JSON は None。
pub fn parse_eventpubsub_response(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    value
        .get("url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// トークン入り WS URL に `&gift=1` を付ける(実 URL は必ず `?` を持つ)。
pub fn ws_connect_url(url: &str) -> String {
    if url.contains('?') {
        format!("{url}&gift=1")
    } else {
        format!("{url}?gift=1")
    }
}

/// トークン入り WS URL。Debug/Display はホスト名だけを出し、token を漏らさない。
#[derive(Clone, PartialEq, Eq)]
pub struct SecretWsUrl(String);

impl SecretWsUrl {
    pub fn new(url: impl Into<String>) -> Self {
        SecretWsUrl(url.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn host(&self) -> Option<String> {
        url::Url::parse(&self.0)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    }
}

impl fmt::Display for SecretWsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.host() {
            Some(host) => write!(f, "wss://{host}/…(token omitted)"),
            None => f.write_str("wss://…(token omitted)"),
        }
    }
}

impl fmt::Debug for SecretWsUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SecretWsUrl({self})")
    }
}

/// WS フレームの要素を `ChatMessage` へ正規化する時の文脈。
pub struct MapContext<'a> {
    pub channel: &'a str,
    pub broadcaster_id: Option<&'a str>,
    pub now_ms: i64,
}

/// WS テキストフレーム(JSON 配列)の要素を取り出す。壊れていれば空。
pub fn parse_ws_entries(text: &str) -> Vec<Value> {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Array(items)) => items,
        Ok(_) => {
            tracing::debug!("twitcasting: WS フレームが配列ではないため無視");
            Vec::new()
        }
        Err(_) => {
            tracing::debug!("twitcasting: WS フレームが JSON ではないため無視");
            Vec::new()
        }
    }
}

/// WS フレームを `ChatMessage` の配列にする。未知 type・壊れた要素は捨てる。
pub fn parse_ws_frame(text: &str, ctx: &MapContext<'_>) -> Vec<ChatMessage> {
    parse_ws_entries(text)
        .iter()
        .filter_map(|v| event_to_chat(v, ctx))
        .collect()
}

/// type ごとに正規化する。未知 type は debug を出して捨てる。
pub fn event_to_chat(v: &Value, ctx: &MapContext<'_>) -> Option<ChatMessage> {
    let Some(kind) = v.get("type").and_then(Value::as_str) else {
        tracing::debug!("twitcasting: type の無い要素を無視");
        return None;
    };
    match kind {
        "comment" => comment_to_chat(v, ctx),
        "gift" => gift_to_chat(v, ctx),
        other => {
            tracing::debug!(event_type = other, "twitcasting: 未知のイベント種別を無視");
            None
        }
    }
}

fn comment_to_chat(v: &Value, ctx: &MapContext<'_>) -> Option<ChatMessage> {
    let message = v.get("message").and_then(Value::as_str)?;
    if message.trim().is_empty() {
        return None;
    }
    let author = v.get("author")?;
    let real_author_id = id_string(author.get("id"))?;

    let anonymous = is_anonymous(v, author);
    let name = display_name(author).unwrap_or_else(|| real_author_id.clone());
    let (author_id, badges) = if anonymous {
        (ANONYMOUS_AUTHOR_ID.to_string(), anonymous_badges())
    } else {
        (real_author_id.clone(), Vec::new())
    };
    let roles = Roles {
        broadcaster: !anonymous
            && ctx
                .broadcaster_id
                .is_some_and(|broadcaster| broadcaster == real_author_id),
        ..Roles::default()
    };
    let id = match id_string(v.get("id")) {
        Some(id) => format!("tc-{id}"),
        None => ChatMessage::new_id(),
    };

    Some(ChatMessage {
        listener: None,
        id,
        native_id: false,
        platform: Platform::Twitcasting,
        channel: ctx.channel.to_string(),
        author: Author {
            nickname: None,
            avatar_url: avatar_url(author),
            id: author_id,
            name,
            display_color: None,
            badges,
            roles,
        },
        fragments: vec![Fragment::text(message.to_string())],
        kind: MessageKind::Normal,
        amount: None,
        timestamp_ms: event_timestamp_ms(v, ctx.now_ms),
        raw: None,
        skip_tts: false,
        injected_test: false,
    })
}

fn gift_to_chat(v: &Value, ctx: &MapContext<'_>) -> Option<ChatMessage> {
    let sender = v.get("sender").or_else(|| v.get("author"));
    let raw_author_id = sender
        .and_then(|s| id_string(s.get("id")))
        .or_else(|| sender.and_then(display_name))
        .unwrap_or_else(|| ANONYMOUS_AUTHOR_ID.to_string());
    let anonymous = sender.is_none()
        || sender.is_some_and(|s| is_anonymous(v, s))
        || raw_author_id.to_ascii_lowercase().starts_with("c:tw");
    let name = sender
        .and_then(display_name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| "匿名".to_string());
    let (author_id, badges) = if anonymous {
        (ANONYMOUS_AUTHOR_ID.to_string(), anonymous_badges())
    } else {
        (raw_author_id.clone(), Vec::new())
    };
    let roles = Roles {
        broadcaster: !anonymous
            && ctx
                .broadcaster_id
                .is_some_and(|broadcaster| broadcaster == raw_author_id),
        ..Roles::default()
    };
    let id = match id_string(v.get("id")) {
        Some(id) => format!("tc-gift-{id}"),
        None => ChatMessage::new_id(),
    };

    Some(ChatMessage {
        listener: None,
        id,
        native_id: false,
        platform: Platform::Twitcasting,
        channel: ctx.channel.to_string(),
        author: Author {
            nickname: None,
            avatar_url: sender.and_then(avatar_url),
            id: author_id,
            name,
            display_color: None,
            badges,
            roles,
        },
        fragments: vec![Fragment::text(gift_text(v))],
        kind: MessageKind::Gift,
        // 金額換算はしない。アイテム名と数だけを本文に出す。
        amount: None,
        timestamp_ms: event_timestamp_ms(v, ctx.now_ms),
        raw: None,
        skip_tts: false,
        injected_test: false,
    })
}

fn anonymous_badges() -> Vec<Badge> {
    vec![Badge {
        kind: "anonymous".to_string(),
        label: "匿名".to_string(),
        image_url: None,
    }]
}

fn is_anonymous(event: &Value, author: &Value) -> bool {
    let flagged = event
        .get("isAnonymous")
        .and_then(Value::as_bool)
        .or_else(|| author.get("isAnonymous").and_then(Value::as_bool))
        .unwrap_or(false);
    flagged
        || author
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.trim().to_ascii_lowercase().starts_with("c:tw"))
}

fn display_name(author: &Value) -> Option<String> {
    ["name", "screenName"]
        .iter()
        .filter_map(|key| author.get(*key).and_then(Value::as_str))
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

fn avatar_url(author: &Value) -> Option<String> {
    let url = author.get("profileImage").and_then(Value::as_str)?.trim();
    if url.is_empty() {
        None
    } else if let Some(rest) = url.strip_prefix("//") {
        Some(format!("https://{rest}"))
    } else if url.starts_with("https://") {
        Some(url.to_string())
    } else {
        None
    }
}

fn event_timestamp_ms(v: &Value, now_ms: i64) -> i64 {
    match raw_timestamp_ms(v) {
        Some(n) if n > 0 => normalize_epoch_ms(n) as i64,
        _ => now_ms,
    }
}

fn raw_timestamp_ms(v: &Value) -> Option<u64> {
    match v.get("createdAt")? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    }
}

/// 秒とミリ秒の両方を受ける。10 桁以下は秒とみなす。
fn normalize_epoch_ms(n: u64) -> u64 {
    if n < 100_000_000_000 {
        n.saturating_mul(1000)
    } else {
        n
    }
}

fn gift_text(v: &Value) -> String {
    let item = v.get("item");
    let score_title = v
        .get("score_items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("title"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let item_name = item
        .and_then(|item| item.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .or(score_title)
        .unwrap_or("ギフト");
    let count = gift_count(v, item).unwrap_or_else(|| "1".to_string());
    format!("{item_name} ×{count}")
}

fn gift_count(v: &Value, item: Option<&Value>) -> Option<String> {
    v.get("score_items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .and_then(|item| item.get("text"))
        .and_then(|value| id_string(Some(value)))
        .or_else(|| {
            item.and_then(|item| item.get("num"))
                .and_then(|value| id_string(Some(value)))
        })
        .or_else(|| {
            let message = v.get("message").and_then(Value::as_str)?;
            let digits: String = message
                .chars()
                .rev()
                .take_while(|c| c.is_ascii_digit())
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            (!digits.is_empty()).then_some(digits)
        })
}

/// 文字列/数値の ID を寛容に文字列化する。
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

/// listall 応答を古い順・id 重複なしで返す。
pub fn parse_listall(body: &str) -> Vec<Value> {
    let Ok(value) = serde_json::from_str::<Value>(body.trim()) else {
        return Vec::new();
    };
    let Some(comments) = value.get("comments").and_then(Value::as_array) else {
        return Vec::new();
    };
    let mut items: Vec<Value> = comments
        .iter()
        .filter(|item| {
            matches!(
                item.get("type").and_then(Value::as_str),
                Some("comment") | Some("gift")
            )
        })
        .cloned()
        .collect();
    items.sort_by_key(list_sort_key);

    let mut seen = HashSet::new();
    items.retain(|item| match event_key(item) {
        Some(key) => seen.insert(key),
        None => true,
    });
    items
}

fn list_sort_key(v: &Value) -> (u64, u64) {
    let created = raw_timestamp_ms(v).map(normalize_epoch_ms).unwrap_or(0);
    let id = id_string(v.get("id"))
        .and_then(|id| id.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    (created, id)
}

/// 重複排除キー。コメントとギフトで id 空間が違うので type を前置する。
pub fn event_key(v: &Value) -> Option<String> {
    let kind = v.get("type").and_then(Value::as_str)?;
    let id = id_string(v.get("id"))?;
    Some(format!("{kind}:{id}"))
}

/// 直近 id を覚えて listall と WS の重複を落とす純粋な構造体。
pub struct CommentDedup {
    seen: HashSet<String>,
    order: VecDeque<String>,
    limit: usize,
}

impl CommentDedup {
    pub fn new(limit: usize) -> Self {
        CommentDedup {
            seen: HashSet::new(),
            order: VecDeque::new(),
            limit: limit.max(1),
        }
    }

    pub fn clear(&mut self) {
        self.seen.clear();
        self.order.clear();
    }

    /// 未見の要素だけを返し、見たキーを記録する。
    pub fn admit(&mut self, entries: Vec<Value>) -> Vec<Value> {
        let mut fresh = Vec::new();
        for entry in entries {
            match event_key(&entry) {
                Some(key) => {
                    if self.seen.contains(&key) {
                        continue;
                    }
                    self.seen.insert(key.clone());
                    self.order.push_back(key);
                    while self.order.len() > self.limit {
                        if let Some(old) = self.order.pop_front() {
                            self.seen.remove(&old);
                        }
                    }
                    fresh.push(entry);
                }
                None => fresh.push(entry),
            }
        }
        fresh
    }
}

impl Default for CommentDedup {
    fn default() -> Self {
        Self::new(SEEN_LIMIT)
    }
}

/// ツイキャス1チャンネルを購読する Source。
pub struct TwitcastingSource {
    /// 設定の identifier(LiveStream.channel_identifier の照合キー)。
    identifier: String,
    screen_id: Option<String>,
    overrides: TwitcastingOverrides,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
}

impl TwitcastingSource {
    pub fn new(
        identifier: String,
        overrides: TwitcastingOverrides,
        metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    ) -> Self {
        let screen_id = extract_screen_id(&identifier);
        TwitcastingSource {
            identifier,
            screen_id,
            overrides,
            metadata_tx,
        }
    }

    fn channel_name(&self) -> String {
        self.screen_id
            .as_deref()
            .unwrap_or(self.identifier.trim())
            .to_ascii_lowercase()
    }

    async fn send_live_state(&self, live: bool) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::Twitcasting,
            channel: self.channel_name(),
            concurrent_viewers: None,
            viewers_kind: ViewerCountKind::Concurrent,
            likes: None,
            title: None,
            live: Some(live),
            notice: None,
            reactions_delta: None,
            full_snapshot: false,
        };
        let _ = tx.send(update).await;
    }

    /// 待機中(live=false)のままチップへ状態メッセージを出す。
    /// 次の send_live_state で消える。
    async fn send_notice(&self, message: &str) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::Twitcasting,
            channel: self.channel_name(),
            concurrent_viewers: None,
            viewers_kind: ViewerCountKind::Concurrent,
            likes: None,
            title: None,
            live: Some(false),
            notice: Some(message.to_string()),
            reactions_delta: None,
            full_snapshot: false,
        };
        let _ = tx.send(update).await;
    }
}

enum SessionOutcome {
    Cancelled,
    /// is-onlive=false。30 秒 ±20% で待機する。
    Offline,
    /// 302/404。行にエラーを出して 5 分後に再試行する。
    NotFound,
    /// movie_id が読めない・eventpubsuburl が url を返さない(限定配信など)。
    Unavailable,
    /// WS 切断・通信失敗。Backoff で張り直す。
    Disconnected {
        stable: bool,
    },
}

impl Source for TwitcastingSource {
    fn name(&self) -> String {
        format!("twitcasting:{}", self.channel_name())
    }

    fn run(
        &self,
        tx: broadcast::Sender<ChatMessage>,
        cancel: CancellationToken,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<()>> + Send + '_>> {
        Box::pin(async move {
            let Some(screen_id) = self.screen_id.clone() else {
                tracing::warn!("twitcasting:{} 配信者 ID を解釈できません", self.identifier);
                self.send_notice(INVALID_ID_NOTICE).await;
                cancel.cancelled().await;
                return Ok(());
            };

            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                // 存在しないユーザーは 302。reqwest の既定追従を切って検知する。
                .redirect(reqwest::redirect::Policy::none())
                .build()?;
            let patterns = UserPagePatterns::from_overrides(&self.overrides);
            let channel = self.channel_name();
            let mut backoff = Backoff::new();
            let mut seen = CommentDedup::new(SEEN_LIMIT);
            let mut current_movie: Option<String> = None;
            let mut jitter = jitter_seed();

            self.send_live_state(false).await;

            loop {
                if cancel.is_cancelled() {
                    return Ok(());
                }

                let outcome = self
                    .session(
                        &client,
                        &patterns,
                        &screen_id,
                        &channel,
                        &mut seen,
                        &mut current_movie,
                        &tx,
                        &cancel,
                    )
                    .await;

                let delay = match outcome {
                    SessionOutcome::Cancelled => return Ok(()),
                    SessionOutcome::Offline => {
                        backoff.reset();
                        self.send_live_state(false).await;
                        jittered(OFFLINE_INTERVAL, 20, &mut jitter)
                    }
                    SessionOutcome::NotFound => {
                        backoff.reset();
                        tracing::warn!("twitcasting:{channel} {NOT_FOUND_NOTICE}");
                        self.send_notice(NOT_FOUND_NOTICE).await;
                        NOT_FOUND_RETRY
                    }
                    SessionOutcome::Unavailable => {
                        backoff.reset();
                        tracing::warn!("twitcasting:{channel} {UNAVAILABLE_NOTICE}");
                        self.send_notice(UNAVAILABLE_NOTICE).await;
                        jittered(OFFLINE_INTERVAL, 20, &mut jitter)
                    }
                    SessionOutcome::Disconnected { stable } => {
                        self.send_live_state(false).await;
                        if stable {
                            backoff.reset();
                        }
                        backoff.next_delay()
                    }
                };

                tokio::select! {
                    _ = cancel.cancelled() => return Ok(()),
                    _ = tokio::time::sleep(delay) => {}
                }
            }
        })
    }
}

impl TwitcastingSource {
    #[allow(clippy::too_many_arguments)]
    async fn session(
        &self,
        client: &reqwest::Client,
        patterns: &UserPagePatterns,
        screen_id: &str,
        channel: &str,
        seen: &mut CommentDedup,
        current_movie: &mut Option<String>,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> SessionOutcome {
        let page = match self
            .fetch_user_page(client, patterns, screen_id, cancel)
            .await
        {
            Ok(page) => page,
            Err(outcome) => return outcome,
        };

        if page.is_onlive == Some(false) {
            return SessionOutcome::Offline;
        }
        let Some(movie_id) = page.movie_id else {
            return SessionOutcome::Unavailable;
        };

        // 枠が変わった時だけ listall に入り直す。同じ枠の WS 再接続では
        // 既読 id を保ったまま新着だけを流す。
        if current_movie.as_deref() != Some(movie_id.as_str()) {
            seen.clear();
            *current_movie = Some(movie_id.clone());
            self.load_history(client, &movie_id, channel, seen, tx, cancel)
                .await;
        }

        let url = match self
            .fetch_eventpubsub_url(client, &movie_id, screen_id, cancel)
            .await
        {
            Ok(url) => url,
            Err(outcome) => return outcome,
        };
        let ws_url = SecretWsUrl::new(ws_connect_url(&url));
        self.listen_ws(&ws_url, channel, seen, tx, cancel).await
    }

    async fn fetch_user_page(
        &self,
        client: &reqwest::Client,
        patterns: &UserPagePatterns,
        screen_id: &str,
        cancel: &CancellationToken,
    ) -> Result<UserPage, SessionOutcome> {
        let base = self
            .overrides
            .endpoint("userPageBaseUrl", USER_PAGE_BASE_URL);
        let url = join_base(&base, screen_id);
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(SessionOutcome::Cancelled),
            r = client.get(&url).timeout(HTTP_TIMEOUT).send() => r,
        };
        let response = match response {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!("twitcasting:{screen_id} 視聴ページ取得失敗: {e}");
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        let status = response.status();
        if status.is_redirection() || status == reqwest::StatusCode::NOT_FOUND {
            return Err(SessionOutcome::NotFound);
        }
        if !status.is_success() {
            tracing::warn!(
                "twitcasting:{screen_id} 視聴ページが HTTP {} を返しました",
                status.as_u16()
            );
            return Err(SessionOutcome::Disconnected { stable: false });
        }
        let html = match response.text().await {
            Ok(html) => html,
            Err(e) => {
                tracing::warn!("twitcasting:{screen_id} 視聴ページ本文の読み取り失敗: {e}");
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        match patterns.parse(&html) {
            Some(page) => Ok(page),
            None => {
                tracing::warn!(
                    "twitcasting:{screen_id} HTML から movie_id を取得できません(限定配信・仕様変更の可能性)"
                );
                Err(SessionOutcome::Unavailable)
            }
        }
    }

    async fn load_history(
        &self,
        client: &reqwest::Client,
        movie_id: &str,
        channel: &str,
        seen: &mut CommentDedup,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) {
        let url = self
            .overrides
            .endpoint("commentHistoryUrl", COMMENT_HISTORY_URL);
        let limit = HISTORY_LIMIT.to_string();
        let response = tokio::select! {
            _ = cancel.cancelled() => return,
            r = client
                .get(&url)
                .query(&[
                    ("c", "listall"),
                    ("m", movie_id),
                    ("k", "0"),
                    ("f", "0"),
                    ("n", limit.as_str()),
                ])
                .timeout(HTTP_TIMEOUT)
                .send() => r,
        };
        let body = match response {
            Ok(response) if response.status().is_success() => match response.text().await {
                Ok(body) => body,
                Err(e) => {
                    tracing::warn!("twitcasting:{channel} listall 本文の読み取り失敗: {e}");
                    return;
                }
            },
            Ok(response) => {
                tracing::warn!(
                    "twitcasting:{channel} listall が HTTP {} を返しました",
                    response.status().as_u16()
                );
                return;
            }
            Err(e) => {
                tracing::warn!("twitcasting:{channel} listall 取得失敗: {e}");
                return;
            }
        };

        let ctx = MapContext {
            channel,
            broadcaster_id: None,
            now_ms: now_ms(),
        };
        for value in seen.admit(parse_listall(&body)) {
            if let Some(mut message) = event_to_chat(&value, &ctx) {
                // 接続前の直近コメントは既読扱い(読み上げない)。
                message.skip_tts = true;
                let _ = tx.send(message);
            }
        }
    }

    async fn fetch_eventpubsub_url(
        &self,
        client: &reqwest::Client,
        movie_id: &str,
        screen_id: &str,
        cancel: &CancellationToken,
    ) -> Result<String, SessionOutcome> {
        let url = self.overrides.endpoint("eventPubsubUrl", EVENT_PUBSUB_URL);
        let nonce = now_ms().to_string();
        let response = tokio::select! {
            _ = cancel.cancelled() => return Err(SessionOutcome::Cancelled),
            r = client
                .post(&url)
                .form(&[("movie_id", movie_id), ("__n", nonce.as_str())])
                .timeout(HTTP_TIMEOUT)
                .send() => r,
        };
        let response = match response {
            Ok(response) => response,
            Err(e) => {
                tracing::warn!("twitcasting:{screen_id} eventpubsuburl 取得失敗: {e}");
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        if !response.status().is_success() {
            tracing::warn!(
                "twitcasting:{screen_id} eventpubsuburl が HTTP {} を返しました",
                response.status().as_u16()
            );
            return Err(SessionOutcome::Disconnected { stable: false });
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!("twitcasting:{screen_id} eventpubsuburl 本文の読み取り失敗: {e}");
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        match parse_eventpubsub_response(&body) {
            Some(url) => Ok(url),
            None => {
                tracing::warn!(
                    "twitcasting:{screen_id} eventpubsuburl が url を返しません(限定配信など)"
                );
                Err(SessionOutcome::Unavailable)
            }
        }
    }

    async fn listen_ws(
        &self,
        ws_url: &SecretWsUrl,
        channel: &str,
        seen: &mut CommentDedup,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) -> SessionOutcome {
        let mut request = match ws_url.as_str().into_client_request() {
            Ok(request) => request,
            Err(_) => {
                // トークン入り URL をエラー文に出さない。
                tracing::warn!("twitcasting:{channel} WS URL を解釈できません ({ws_url})");
                return SessionOutcome::Disconnected { stable: false };
            }
        };
        if let Ok(agent) = USER_AGENT.parse() {
            request.headers_mut().insert("User-Agent", agent);
        }

        let connected = tokio::select! {
            _ = cancel.cancelled() => return SessionOutcome::Cancelled,
            r = tokio::time::timeout(
                WS_CONNECT_TIMEOUT,
                tokio_tungstenite::connect_async(request),
            ) => r,
        };
        let (ws_stream, _response) = match connected {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) | Err(_) => {
                // tungstenite のエラーは URL を含みうるので中身を出さない。
                tracing::warn!("twitcasting:{channel} WS 接続失敗 ({ws_url})");
                return SessionOutcome::Disconnected { stable: false };
            }
        };
        tracing::info!("twitcasting:{channel} WS 接続開始 ({ws_url})");

        let (mut write, mut read) = ws_stream.split();
        let connected_at = Instant::now();
        let mut received = false;
        let mut last_frame = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    let _ = write.send(WsMessage::Close(None)).await;
                    return SessionOutcome::Cancelled;
                }
                _ = tokio::time::sleep_until(last_frame + WS_IDLE_TIMEOUT) => {
                    tracing::warn!(
                        "twitcasting:{channel} WS が{}秒無フレーム",
                        WS_IDLE_TIMEOUT.as_secs()
                    );
                    return SessionOutcome::Disconnected {
                        stable: session_is_stable(received, connected_at),
                    };
                }
                msg = read.next() => {
                    let msg = match msg {
                        Some(Ok(msg)) => msg,
                        Some(Err(_)) => {
                            tracing::warn!("twitcasting:{channel} WS 受信エラー");
                            return SessionOutcome::Disconnected {
                                stable: session_is_stable(received, connected_at),
                            };
                        }
                        None => {
                            return SessionOutcome::Disconnected {
                                stable: session_is_stable(received, connected_at),
                            };
                        }
                    };
                    last_frame = tokio::time::Instant::now();

                    match msg {
                        WsMessage::Text(text) => {
                            received = true;
                            let entries = seen.admit(parse_ws_entries(&text));
                            let ctx = MapContext {
                                channel,
                                broadcaster_id: None,
                                now_ms: now_ms(),
                            };
                            for message in entries
                                .iter()
                                .filter_map(|v| event_to_chat(v, &ctx))
                            {
                                let _ = tx.send(message);
                            }
                        }
                        WsMessage::Ping(payload) => {
                            if write.send(WsMessage::Pong(payload)).await.is_err() {
                                return SessionOutcome::Disconnected {
                                    stable: session_is_stable(received, connected_at),
                                };
                            }
                        }
                        WsMessage::Close(_) => {
                            return SessionOutcome::Disconnected {
                                stable: session_is_stable(received, connected_at),
                            };
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

fn join_base(base: &str, id: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{id}")
    } else {
        format!("{base}/{id}")
    }
}

fn session_is_stable(received: bool, connected_at: Instant) -> bool {
    received || connected_at.elapsed() >= Duration::from_secs(30)
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn jitter_seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// ±percent% の揺らぎを付けた待機時間。テスト可能な純粋関数。
fn jittered(base: Duration, percent: u64, seed: &mut u64) -> Duration {
    *seed = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    let delta = base.as_millis() * percent as u128 / 100;
    let span = delta * 2 + 1;
    let offset = (*seed as u128) % span;
    let millis = (base.as_millis() + offset).saturating_sub(delta);
    Duration::from_millis(millis.max(1) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ctx() -> MapContext<'static> {
        MapContext {
            channel: "alice",
            broadcaster_id: None,
            now_ms: 1_800_000_000_000,
        }
    }

    #[test]
    fn extracts_screen_id_from_urls_and_raw_ids() {
        assert_eq!(
            extract_screen_id("twitcasting.tv/alice").as_deref(),
            Some("alice")
        );
        assert_eq!(
            extract_screen_id("https://twitcasting.tv/alice/movie/123").as_deref(),
            Some("alice")
        );
        assert_eq!(
            extract_screen_id("http://www.twitcasting.tv/alice/").as_deref(),
            Some("alice")
        );
        assert_eq!(
            extract_screen_id("https://en.twitcasting.tv/Alice?x=1#frag").as_deref(),
            Some("Alice")
        );
        assert_eq!(
            extract_screen_id("https://twitcasting.tv/c%3Aalice/movie/1").as_deref(),
            Some("c:alice")
        );
        assert_eq!(extract_screen_id("c:abc").as_deref(), Some("c:abc"));
        assert_eq!(extract_screen_id("  g:123  ").as_deref(), Some("g:123"));
        assert_eq!(extract_screen_id("alice_01").as_deref(), Some("alice_01"));
    }

    #[test]
    fn rejects_other_hosts_empty_and_bad_charset() {
        for input in [
            "",
            "   ",
            "https://apiv2.twitcasting.tv/alice",
            "https://eviltwitcasting.tv/alice",
            "https://twitcasting.tv.evil.example/alice",
            "twitcasting.tv.evil.example/alice",
            "https://twitcasting.tv/",
            "https://twitcasting.tv/indexapi.php",
            "x:alice",
            "abcdefghijklmnopqrstuvwxyz0123456",
            "ａｌｉｃｅ",
        ] {
            assert_eq!(extract_screen_id(input), None, "input={input:?}");
        }
    }

    #[test]
    fn parses_user_page_with_data_attribute_and_fallback() {
        let html = concat!(
            "<html><body>",
            r#"<div id="movie" data-movie-id="843147578" data-is-onlive="true" data-audience-id=""></div>"#,
            "</body></html>"
        );
        let page = parse_user_page(html).expect("page");
        assert_eq!(page.movie_id.as_deref(), Some("843147578"));
        assert_eq!(page.is_onlive, Some(true));

        let fallback =
            r#"<script>var movieid = "12345";</script><div data-is-onlive="false"></div>"#;
        let page = parse_user_page(fallback).expect("fallback page");
        assert_eq!(page.movie_id.as_deref(), Some("12345"));
        assert_eq!(page.is_onlive, Some(false));

        assert!(parse_user_page("<html><body>no movie</body></html>").is_none());
    }

    #[test]
    fn user_page_patterns_can_be_overridden() {
        let overrides = TwitcastingOverrides {
            patterns: HashMap::from([
                ("movieId".to_string(), r#"data-custom="(\d+)""#.to_string()),
                (
                    "isOnlive".to_string(),
                    r#"data-custom-live="(true|false)""#.to_string(),
                ),
            ]),
            ..TwitcastingOverrides::default()
        };
        let patterns = UserPagePatterns::from_overrides(&overrides);
        let page = patterns
            .parse(r#"<div data-custom="777" data-custom-live="false"></div>"#)
            .expect("override page");
        assert_eq!(page.movie_id.as_deref(), Some("777"));
        assert_eq!(page.is_onlive, Some(false));
    }

    #[test]
    fn parses_eventpubsub_response_and_appends_gift() {
        assert_eq!(
            parse_eventpubsub_response(r#"{"url":"wss://example/events?token=t"}"#).as_deref(),
            Some("wss://example/events?token=t")
        );
        assert_eq!(parse_eventpubsub_response(r#"{"url":"  "}"#), None);
        assert_eq!(parse_eventpubsub_response(r#"{"url":null}"#), None);
        assert_eq!(parse_eventpubsub_response(""), None);
        assert_eq!(parse_eventpubsub_response("not json"), None);
        assert_eq!(
            ws_connect_url("wss://example/events?token=t"),
            "wss://example/events?token=t&gift=1"
        );
    }

    #[test]
    fn normalizes_comment_fields() {
        let frame = r#"[
            {"type":"comment","id":34006235690,"message":"こんにちは","createdAt":1795121838000,
             "author":{"id":"tonton_boo2","name":"表示名","screenName":"tonton_boo2",
                       "profileImage":"//image.example/a.jpg","grade":3},"numComments":1}
        ]"#;
        let messages = parse_ws_frame(frame, &ctx());
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.platform, Platform::Twitcasting);
        assert_eq!(message.channel, "alice");
        assert_eq!(message.kind, MessageKind::Normal);
        assert!(!message.native_id);
        assert_eq!(message.id, "tc-34006235690");
        assert_eq!(message.author.id, "tonton_boo2");
        assert_eq!(message.author.name, "表示名");
        assert_eq!(
            message.author.avatar_url.as_deref(),
            Some("https://image.example/a.jpg")
        );
        assert_eq!(message.timestamp_ms, 1795121838000);
        assert_eq!(message.plain_text(), "こんにちは");
        assert!(message.amount.is_none());
    }

    #[test]
    fn anonymous_comments_share_fixed_author_id_and_badge() {
        let frame = r#"[
            {"type":"comment","id":1,"message":"a","createdAt":1000,"isAnonymous":true,
             "author":{"id":"c:tw1","name":"匿名コメント","screenName":"c:tw1"}},
            {"type":"comment","id":2,"message":"b","createdAt":2000,
             "author":{"id":"c:tw2","name":"匿名コメント","screenName":"c:tw2"}}
        ]"#;
        let messages = parse_ws_frame(frame, &ctx());
        assert_eq!(messages.len(), 2);
        for message in &messages {
            assert_eq!(message.author.id, ANONYMOUS_AUTHOR_ID);
            assert_eq!(message.author.badges[0].kind, "anonymous");
            assert!(!message.author.roles.broadcaster);
        }
        assert_ne!(messages[0].id, messages[1].id);
    }

    #[test]
    fn skips_unknown_types_and_broken_elements_but_keeps_the_rest() {
        let frame = r#"[
            {"type":"comment","id":1,"message":"a","createdAt":1000,
             "author":{"id":"u1","name":"A"}},
            {"type":"unknown","id":2,"message":"x"},
            42,
            {"type":"comment","id":3,"message":"c","createdAt":3000,
             "author":{"id":"u3","name":"C"}}
        ]"#;
        let messages = parse_ws_frame(frame, &ctx());
        let ids: Vec<_> = messages.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["tc-1", "tc-3"]);
    }

    #[test]
    fn gift_keeps_item_name_and_count_without_amount() {
        let frame = r#"[
            {"id":"gift-1","type":"gift","message":"(+🍡10)","plainMessage":"",
             "item":{"name":"お団子","image":"https://example/item.png"},
             "sender":{"id":"sender1","name":"贈り主","screenName":"sender1",
                       "profileImage":"https://image.example/s.jpg"},
             "createdAt":1795121873000,"score_items":[{"title":"🍡","text":"10"}]}
        ]"#;
        let messages = parse_ws_frame(frame, &ctx());
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.kind, MessageKind::Gift);
        assert_eq!(message.id, "tc-gift-gift-1");
        assert_eq!(message.author.id, "sender1");
        assert_eq!(message.author.name, "贈り主");
        assert!(message.amount.is_none());
        let text = message.plain_text();
        assert!(text.contains("お団子"), "text={text}");
        assert!(text.contains("10"), "text={text}");
    }

    #[test]
    fn listall_is_oldest_first_and_deduped_with_ws() {
        let body = r#"{"comments":[
            {"type":"comment","id":3,"message":"c","createdAt":3000,"author":{"id":"u","name":"N"}},
            {"type":"comment","id":1,"message":"a","createdAt":1000,"author":{"id":"u","name":"N"}},
            {"type":"comment","id":2,"message":"b","createdAt":2000,"author":{"id":"u","name":"N"}},
            {"type":"filtered_comment","id":9,"message":"x","createdAt":2500,"author":{"id":"u","name":"N"}},
            {"type":"comment","id":2,"message":"b2","createdAt":2000,"author":{"id":"u","name":"N"}}
        ]}"#;
        let items = parse_listall(body);
        let ids: Vec<_> = items
            .iter()
            .filter_map(|v| id_string(v.get("id")))
            .collect();
        assert_eq!(ids, ["1", "2", "3"]);

        let mut seen = CommentDedup::new(10);
        let first = seen.admit(items);
        assert_eq!(first.len(), 3);

        let ws = parse_ws_entries(
            r#"[
                {"type":"comment","id":3,"message":"dup","createdAt":3000,"author":{"id":"u","name":"N"}},
                {"type":"comment","id":4,"message":"new","createdAt":4000,"author":{"id":"u","name":"N"}}
            ]"#,
        );
        let fresh = seen.admit(ws);
        assert_eq!(fresh.len(), 1);
        assert_eq!(id_string(fresh[0].get("id")).as_deref(), Some("4"));
    }

    #[test]
    fn secret_ws_url_never_prints_token() {
        let url = SecretWsUrl::new(
            "wss://203-1-2-3.twitcasting.tv/event.pubsub/v1/streams/1/events?token=SUPERSECRET&n=abc",
        );
        for text in [url.to_string(), format!("{url:?}")] {
            assert!(!text.contains("SUPERSECRET"), "text={text}");
            assert!(text.contains("twitcasting.tv"), "text={text}");
        }
    }

    #[test]
    fn offline_jitter_stays_within_20_percent() {
        let mut seed = 12345;
        for _ in 0..64 {
            let delay = jittered(OFFLINE_INTERVAL, 20, &mut seed);
            assert!(delay >= Duration::from_secs(24), "delay={delay:?}");
            assert!(delay <= Duration::from_secs(36), "delay={delay:?}");
        }
    }

    #[test]
    fn gift_text_includes_item_and_count_for_numeric_counts() {
        let value = json!({
            "type": "gift",
            "message": "(+10)",
            "item": {"name": "ハート", "num": 7},
        });
        assert!(gift_text(&value).contains("ハート"));
        assert!(gift_text(&value).contains('7'));
    }
}
