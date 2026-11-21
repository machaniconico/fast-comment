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
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use futures_util::{SinkExt, StreamExt};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use super::{Backoff, Source};
use crate::model::{
    Author, Badge, ChatMessage, Fragment, LiveEvent, LiveStream, MessageKind, Platform, Roles,
};
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

const USER_PAGE_BASE_URL: &str = "https://twitcasting.tv/";
const EVENT_PUBSUB_URL: &str = "https://twitcasting.tv/eventpubsuburl.php";
/// `<screen_id>/userajax.php` を組み立てる基準 URL。
const COMMENT_HISTORY_URL: &str = "https://twitcasting.tv/";
/// 既存 Source と同じ一般ブラウザ文字列。cookie・ログイン情報は使わない。
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// キャンセル時に Close を送るが、相手が詰まっても終了を待たない。
const WS_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);
/// 未配信・取得不能時の HTML 再取得間隔(±20% の揺らぎを付ける)。
const OFFLINE_INTERVAL: Duration = Duration::from_secs(30);
/// 存在しないユーザー(302)の再試行間隔。
const NOT_FOUND_RETRY: Duration = Duration::from_secs(300);
/// 90 秒フレームが来なければ配信状態を HTML で確認し直す。
const WS_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const HISTORY_LIMIT: usize = 20;
const SEEN_LIMIT: usize = 4096;
const STARTED_MOVIES_LIMIT: usize = 256;
/// 匿名コメントは複数人が同じ `c:tw*` を共有するため、固定 ID に寄せる。
/// ユーザー単位 NG・色分けに使わせない意図を anonymous バッジで示す。
const ANONYMOUS_AUTHOR_ID: &str = "tc:anonymous";

pub const DEFAULT_MOVIE_ID_PATTERN: &str = r#"data-movie-id="(\d+)""#;
pub const DEFAULT_MOVIE_ID_FALLBACK_PATTERN: &str = r#"var\s+movieid\s*=\s*"(\d+)""#;
pub const DEFAULT_IS_ONLIVE_PATTERN: &str = r#"data-is-onlive="([^"]*)""#;

static HTML_TAGS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?s)<!--.*?(?:-->|$)|<[a-zA-Z](?:[^<>"']|"[^"]*"|'[^']*')*>"#)
        .expect("static tag pattern")
});
static HTML_ATTRIBUTES: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"\s+([^\s=/<>"']+)(?:\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s<>"']+)))?"#)
        .expect("static attribute pattern")
});

pub const NOT_FOUND_NOTICE: &str = "ツイキャスのユーザーが見つかりません";
pub const UNAVAILABLE_NOTICE: &str = "この配信は非公開のため取得できません";
const UNREADABLE_LOG: &str =
    "視聴ページから配信状態を読み取れません(仕様変更の可能性)。30 秒後に再確認します";
pub const INVALID_ID_NOTICE: &str = "ツイキャスの配信者 ID を解釈できません";
/// 解釈できない識別子の channel 名。入力(URL やトークンでありうる)を生で出さない。
pub const INVALID_CHANNEL_LABEL: &str = "twitcasting-invalid";

/// 非公式 Source の待ち時間。既定は従来の定数と同じ。テストで短くする。
#[derive(Clone)]
pub struct UnofficialTuning {
    /// 未配信・限定配信の HTML 再取得間隔(`jitter` 時は ±20%)。
    pub offline: Duration,
    /// 読めない HTML・eventpubsuburl の 3xx/4xx の再確認間隔。
    pub unreadable_recheck: Duration,
    /// 存在しないユーザーの再試行間隔。
    pub not_found_retry: Duration,
    pub jitter: bool,
}

impl Default for UnofficialTuning {
    fn default() -> Self {
        UnofficialTuning {
            offline: OFFLINE_INTERVAL,
            unreadable_recheck: OFFLINE_INTERVAL,
            not_found_retry: NOT_FOUND_RETRY,
            jitter: true,
        }
    }
}

/// 非公式経路の構造が読めたか(公式への切替カウンタ用)。結末・notice とは別に出す。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnofficialSignal {
    /// HTML・eventpubsuburl の 2xx 本文・WS の最初のフレームの構造が読めない。
    Unreadable,
    /// 明示的な未配信ページ、または接続ごとの最初の WS 配列フレーム。
    Healthy,
    /// 到達性の問題や限定配信など。数えず、リセットもしない。
    Neutral,
}

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
    let raw = input.trim_matches(is_js_trim_char);
    if raw.is_empty() {
        return None;
    }

    let lower = raw.to_ascii_lowercase();
    // 裸の ID の検査は ?/# より前だけに行う(UI と同じ順。`abc?next=/movie` は abc)。
    let head = raw.split(['?', '#']).next().unwrap_or("");
    let path = if lower.starts_with("http://") || lower.starts_with("https://") {
        let after_scheme = &raw[raw.find("://")? + 3..];
        let authority_end = after_scheme
            .find(['/', '?', '#'])
            .unwrap_or(after_scheme.len());
        let authority = &after_scheme[..authority_end];
        // UI はホスト部分をそのまま照合するので、認証情報とポートも URL として認めない。
        if authority.contains('@') || authority.contains(':') {
            return None;
        }
        if !is_allowed_host(authority) {
            return None;
        }
        &after_scheme[authority_end..]
    } else if head.starts_with("//") {
        // プロトコル相対 URL は UI と同じく受け付けない。
        return None;
    } else if let Some(rest) = strip_allowed_host_prefix(raw) {
        // 許可ホストを先に判定する。後続パスの `://`(`…/movie/https://x`)で全体を拒否しない(UI と同じ)。
        rest
    } else if head.contains("://") {
        return None;
    } else if head.contains('/') {
        // スキーム無しの他ホスト・パスは受け付けない。
        return None;
    } else {
        raw
    };

    let path = path.split(['?', '#']).next().unwrap_or("");
    // 先頭の / は 1 つだけ剥がす。//abc や /./abc を URL 正規化で通さない。
    let path = path.strip_prefix('/').unwrap_or(path);
    let segment = path.split('/').next().unwrap_or("");
    let segment = decode_percent_colon(segment);
    validate_screen_id(&segment)
}

/// JS の `String.prototype.trim` と同じ集合(Unicode White_Space から U+0085 を除き U+FEFF を足す)。
/// UI(twitcastingId.ts)と trim 対象を揃えるため明示する。
fn is_js_trim_char(c: char) -> bool {
    (c.is_whitespace() && c != '\u{85}') || c == '\u{FEFF}'
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
        if lower.starts_with(host) {
            let rest = &input[host.len()..];
            if rest.is_empty() || rest.starts_with(['/', '?', '#']) {
                return Some(rest);
            }
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
    movie_id: Option<Regex>,
    movie_id_fallback: Option<Regex>,
    is_onlive: Option<Regex>,
}

impl UserPagePatterns {
    fn from_overrides(overrides: &TwitcastingOverrides) -> Self {
        fn build(overrides: &TwitcastingOverrides, key: &str, default: &str) -> Option<Regex> {
            let pattern = overrides.pattern(key, default);
            match Regex::new(&pattern) {
                Ok(regex) => Some(regex),
                Err(error) => {
                    // パターン自体は秘密ではないので、原因究明のため内容も出す。
                    tracing::warn!(
                        pattern = %pattern,
                        %error,
                        "twitcasting: overrides の正規表現が不正なため既定に戻します"
                    );
                    Regex::new(default).ok()
                }
            }
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
    fn capture<'a>(
        regex: Option<&Regex>,
        text: &'a str,
        ignored: &[std::ops::Range<usize>],
    ) -> Option<&'a str> {
        regex?
            .captures_iter(text)
            .filter_map(|captures| captures.get(1))
            .find(|value| {
                let index = ignored.partition_point(|range| range.end <= value.start());
                !ignored
                    .get(index)
                    .is_some_and(|range| range.contains(&value.start()))
            })
            .map(|value| value.as_str())
    }

    fn first_movie_id(&self, html: &str, ignored: &[std::ops::Range<usize>]) -> Option<String> {
        Self::capture(self.movie_id.as_ref(), html, ignored)
            .or_else(|| Self::capture(self.movie_id_fallback.as_ref(), html, ignored))
            .map(str::to_owned)
    }

    fn parse(&self, html: &str) -> Option<UserPage> {
        self.parse_for(html, None)
    }

    fn parse_for(&self, html: &str, screen_id: Option<&str>) -> Option<UserPage> {
        let mut owner_movie_id = None;
        let mut has_broadcaster = false;
        let mut has_owner = false;
        let mut ignored_tags = Vec::new();
        for tag in HTML_TAGS.find_iter(html) {
            if tag.as_str().starts_with("<!--") {
                ignored_tags.push(tag.range());
                continue;
            }
            if let Some(owner_matches) =
                screen_id.and_then(|screen_id| tag_broadcaster_matches(tag.as_str(), screen_id))
            {
                has_broadcaster = true;
                if owner_matches {
                    has_owner = true;
                    if owner_movie_id.is_none() {
                        owner_movie_id = self.first_movie_id(tag.as_str(), &[]);
                    }
                } else {
                    ignored_tags.push(tag.range());
                }
            }
        }
        // body の状態はページ全体の値なので、別の配信者しかいない場合は使わない。
        if has_broadcaster && !has_owner {
            return None;
        }
        let movie_id = if has_broadcaster {
            owner_movie_id
        } else {
            self.first_movie_id(html, &ignored_tags)
        };
        let is_onlive = match Self::capture(self.is_onlive.as_ref(), html, &ignored_tags) {
            Some(value) if value.eq_ignore_ascii_case("true") => Some(true),
            Some(value) if value.eq_ignore_ascii_case("false") => Some(false),
            Some(_) => return None,
            None => None,
        };
        // movie_id が無くても is-onlive=false が明示されていれば未配信(配信歴ゼロ)。
        // それ以外で何も読めない HTML は None(呼び出し側で Unreadable)。
        if movie_id.is_none() && is_onlive != Some(false) {
            return None;
        }
        Some(UserPage {
            movie_id,
            is_onlive,
        })
    }
}

fn tag_broadcaster_matches(tag: &str, screen_id: &str) -> Option<bool> {
    let mut has_broadcaster = false;
    for attr in HTML_ATTRIBUTES.captures_iter(tag) {
        let name = &attr[1];
        if name.eq_ignore_ascii_case("data-broadcaster-id")
            || name.eq_ignore_ascii_case("data-broadcaster-screen-name")
        {
            has_broadcaster = true;
            let value = attr.get(2).or_else(|| attr.get(3)).or_else(|| attr.get(4));
            if value.is_some_and(|value| value.as_str().trim().eq_ignore_ascii_case(screen_id)) {
                return Some(true);
            }
        }
    }
    has_broadcaster.then_some(false)
}

/// 既定パターンで HTML を解析する。
pub fn parse_user_page(html: &str) -> Option<UserPage> {
    UserPagePatterns::from_overrides(&TwitcastingOverrides::default()).parse(html)
}

/// 視聴ページ HTML から、指定した配信者本人の movie_id を優先して読む。
pub fn parse_user_page_for(html: &str, screen_id: &str) -> Option<UserPage> {
    UserPagePatterns::from_overrides(&TwitcastingOverrides::default())
        .parse_for(html, Some(screen_id))
}

/// eventpubsuburl.php の応答の解釈。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventPubsub {
    Url(String),
    /// url キーが無い・null・空文字(限定配信など、公開状態として読める形)。
    Private,
    /// 非 JSON・オブジェクトでない・url が文字列でない(解析不能。非公開とは区別する)。
    Malformed,
}

pub fn parse_eventpubsub_response(body: &str) -> EventPubsub {
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(body.trim()) else {
        return EventPubsub::Malformed;
    };
    match map.get("url") {
        None | Some(Value::Null) => EventPubsub::Private,
        Some(Value::String(s)) => {
            let s = s.trim();
            if s.is_empty() {
                EventPubsub::Private
            } else {
                EventPubsub::Url(s.to_string())
            }
        }
        Some(_) => EventPubsub::Malformed,
    }
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
    parse_ws_array(text).unwrap_or_default()
}

/// WS フレームを 1 回だけ解析する。配列なら要素、それ以外は None(信号と処理で同じ結果を使う)。
fn parse_ws_array(text: &str) -> Option<Vec<Value>> {
    match serde_json::from_str::<Value>(text.trim()) {
        Ok(Value::Array(items)) => Some(items),
        Ok(_) => {
            tracing::debug!("twitcasting: WS フレームが配列ではないため無視");
            None
        }
        Err(_) => {
            tracing::debug!("twitcasting: WS フレームが JSON ではないため無視");
            None
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
                .is_some_and(|broadcaster| broadcaster.eq_ignore_ascii_case(&real_author_id)),
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
        route: Default::default(),
    })
}

fn gift_to_chat(v: &Value, ctx: &MapContext<'_>) -> Option<ChatMessage> {
    let sender = v.get("sender").or_else(|| v.get("author"));
    let raw_author_id = sender
        .and_then(|s| id_string(s.get("id")))
        .or_else(|| sender.and_then(display_name))
        .unwrap_or_else(|| ANONYMOUS_AUTHOR_ID.to_string());
    let anonymous = sender.is_none() || sender.is_some_and(|s| is_anonymous(v, s));
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
                .is_some_and(|broadcaster| broadcaster.eq_ignore_ascii_case(&raw_author_id)),
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
        // 金額換算はしない。アイテム名と score の表記だけを本文に出す。
        amount: None,
        timestamp_ms: event_timestamp_ms(v, ctx.now_ms),
        raw: None,
        skip_tts: false,
        injected_test: false,
        route: Default::default(),
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
    if flagged {
        return true;
    }
    author
        .get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_ascii_lowercase)
        .is_some_and(|id| is_anonymous_id(&id) || id == "twitcastinguest")
}

/// `c:tw` の後ろが数字 1 個以上だけ(`c:tw1`)を匿名とする。`c:twinkle` は実ユーザー。
fn is_anonymous_id(id: &str) -> bool {
    id.strip_prefix("c:tw")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
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
    let score = v
        .get("score_items")
        .and_then(Value::as_array)
        .and_then(|items| items.first())
        .map(|item| {
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or("");
            let text = item
                .get("text")
                .and_then(|value| id_string(Some(value)))
                .unwrap_or_default();
            format!("{title}{text}")
        })
        .map(|score| score.trim().to_string())
        .filter(|score| !score.is_empty());
    let item_name = v
        .get("item")
        .and_then(|item| item.get("name"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());

    match (item_name, score) {
        // score_items[].text は点数で個数ではないので「×N」にしない。
        (Some(name), Some(score)) => format!("{name} {score}"),
        (Some(name), None) => name.to_string(),
        (None, Some(score)) => score,
        (None, None) => "ギフト".to_string(),
    }
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

/// 設定の identifier から ChatMessage.channel / 集計キーに使う正規化名を作る。
/// 解釈できない識別子は生で出さず `INVALID_CHANNEL_LABEL` にする。
pub fn channel_name(identifier: &str) -> String {
    extract_screen_id(identifier)
        .map(|id| id.to_ascii_lowercase())
        .unwrap_or_else(|| INVALID_CHANNEL_LABEL.to_string())
}

/// アプリ内の全 Source で共有し、送信に成功した movie_id を古い順に保持する。
pub(crate) type StartedMovies = Arc<Mutex<VecDeque<String>>>;

/// ツイキャス1チャンネルを購読する Source。
pub struct TwitcastingSource {
    identifier: String,
    screen_id: Option<String>,
    overrides: TwitcastingOverrides,
    metadata_tx: Option<mpsc::Sender<YoutubeMetadataUpdate>>,
    /// 配信開始/終了イベント(X 自動告知用)。
    live_tx: Option<mpsc::Sender<LiveEvent>>,
    started_movies: StartedMovies,
    live_movie: Mutex<Option<String>>,
    tuning: UnofficialTuning,
    signal_tx: Option<mpsc::UnboundedSender<UnofficialSignal>>,
}

/// identifier は URL やトークンでありうるので、正規化した channel 名だけを出す。
impl fmt::Debug for TwitcastingSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TwitcastingSource")
            .field("channel", &self.channel_name())
            .finish_non_exhaustive()
    }
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
            live_tx: None,
            started_movies: Default::default(),
            live_movie: Mutex::new(None),
            tuning: UnofficialTuning::default(),
            signal_tx: None,
        }
    }

    pub fn with_tuning(mut self, tuning: UnofficialTuning) -> Self {
        self.tuning = tuning;
        self
    }

    /// 構造が読めたかの信号を受け取る。信号は 1 セッションに数件なので無制限で落とさず、
    /// 送信で待たないので Source の待ち時間は変えない。
    pub(crate) fn with_signal_tx(
        mut self,
        signal_tx: mpsc::UnboundedSender<UnofficialSignal>,
    ) -> Self {
        self.signal_tx = Some(signal_tx);
        self
    }

    fn send_signal(&self, signal: UnofficialSignal) {
        if let Some(tx) = &self.signal_tx {
            let _ = tx.send(signal);
        }
    }

    pub fn with_live_tx(mut self, live_tx: Option<mpsc::Sender<LiveEvent>>) -> Self {
        self.live_tx = live_tx;
        self
    }

    pub(crate) fn with_started_movies(mut self, started_movies: StartedMovies) -> Self {
        self.started_movies = started_movies;
        self
    }

    pub(crate) fn take_live_movie(&self) -> Option<String> {
        self.live_movie
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
    }

    fn send_live_started(&self, screen_id: &str, movie_id: &str) {
        *self.live_movie.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(movie_id.to_string());
        let Some(tx) = &self.live_tx else { return };
        // 複数の Source が同じ配信を検知しても、照合から記録までを直列化する。
        let mut started = self
            .started_movies
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if started.iter().any(|id| id == movie_id) {
            return;
        }
        if super::send_live_event(
            tx,
            LiveEvent::Started(LiveStream {
                platform: Platform::Twitcasting,
                channel_identifier: self.channel_name(),
                stream_id: movie_id.to_string(),
                title: None,
                url: format!("{USER_PAGE_BASE_URL}{screen_id}/movie/{movie_id}"),
                started_at_ms: None,
            }),
        ) {
            if started.len() == STARTED_MOVIES_LIMIT {
                started.pop_front();
            }
            started.push_back(movie_id.to_string());
        }
    }

    fn send_live_ended(&self) {
        let _ = self.take_live_movie();
        let Some(tx) = &self.live_tx else { return };
        super::send_live_event(
            tx,
            LiveEvent::Ended {
                platform: Platform::Twitcasting,
                channel_identifier: self.channel_name(),
            },
        );
    }

    fn channel_name(&self) -> String {
        channel_name(&self.identifier)
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

    /// WS 接続成功。配信中として通知し、待機中の notice を消す。
    async fn send_connected(&self) {
        let Some(tx) = &self.metadata_tx else { return };
        let update = YoutubeMetadataUpdate {
            platform: Platform::Twitcasting,
            channel: self.channel_name(),
            concurrent_viewers: None,
            viewers_kind: ViewerCountKind::Concurrent,
            likes: None,
            title: None,
            live: Some(true),
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionOutcome {
    Cancelled,
    /// is-onlive=false・配信歴ゼロ。30 秒 ±20% で待機する。
    Offline,
    /// 不存在ユーザー(302)・404。行にエラーを出して 5 分後に再試行する。
    NotFound,
    /// eventpubsuburl が url を返さない(限定配信など)。
    Unavailable,
    /// eventpubsuburl が 302/4xx。HTML から配信状態を確認し直す。
    Recheck,
    /// HTTP 200 だが HTML から配信状態を読み取れない(仕様変更・一時的な崩れ)。
    /// 配信終了とは限らないので Ended を送らず、live 状態も変えずに 30 秒後に再確認する。
    Unreadable,
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
                tracing::warn!("twitcasting: {INVALID_ID_NOTICE}");
                self.send_notice(INVALID_ID_NOTICE).await;
                cancel.cancelled().await;
                return Ok(());
            };

            let client = reqwest::Client::builder()
                .user_agent(USER_AGENT)
                .connect_timeout(CONNECT_TIMEOUT)
                // 存在しないユーザーは 302。reqwest の既定追従を切って検知する。
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .map_err(reqwest::Error::without_url)?;
            let patterns = UserPagePatterns::from_overrides(&self.overrides);
            let channel = self.channel_name();
            let mut backoff = Backoff::new();
            let mut seen = CommentDedup::new(SEEN_LIMIT);
            let mut current_movie: Option<String> = None;
            let mut jitter = jitter_seed();
            let mut last_quiet: Option<SessionOutcome> = None;

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

                if let Some(signal) = outcome_signal(outcome) {
                    self.send_signal(signal);
                }
                let repeated = is_repeated_quiet(&mut last_quiet, outcome);
                match outcome {
                    SessionOutcome::Cancelled => return Ok(()),
                    SessionOutcome::Offline => {
                        self.send_live_state(false).await;
                        self.send_live_ended();
                    }
                    SessionOutcome::NotFound => {
                        tracing::warn!("twitcasting:{channel} {NOT_FOUND_NOTICE}");
                        self.send_notice(NOT_FOUND_NOTICE).await;
                    }
                    SessionOutcome::Unavailable => {
                        if repeated {
                            tracing::debug!("twitcasting:{channel} {UNAVAILABLE_NOTICE}");
                        } else {
                            tracing::warn!("twitcasting:{channel} {UNAVAILABLE_NOTICE}");
                        }
                        self.send_notice(UNAVAILABLE_NOTICE).await;
                    }
                    SessionOutcome::Recheck => {
                        // 次のループで HTML から配信状態を確認し直す。
                        if repeated {
                            tracing::debug!("twitcasting:{channel} eventpubsuburl の応答から再確認");
                        } else {
                            tracing::warn!("twitcasting:{channel} eventpubsuburl の応答から再確認");
                        }
                    }
                    SessionOutcome::Unreadable => {
                        if repeated {
                            tracing::debug!("twitcasting:{channel} {UNREADABLE_LOG}");
                        } else {
                            tracing::warn!("twitcasting:{channel} {UNREADABLE_LOG}");
                        }
                    }
                    SessionOutcome::Disconnected { .. } => {
                        self.send_live_state(false).await;
                    }
                }
                let Some(delay) = outcome_delay_for(outcome, &self.tuning, &mut backoff, &mut jitter)
                else {
                    return Ok(());
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

        let Some(movie_id) = page.movie_id else {
            return SessionOutcome::Unreadable;
        };

        // 枠が変わった時だけ listall に入り直す。同じ枠の WS 再接続では
        // 既読 id を保ったまま新着だけを流す。
        if begin_movie(&movie_id, current_movie, seen) {
            self.load_history(client, &movie_id, screen_id, channel, seen, tx, cancel)
                .await;
        }

        let url = match self
            .fetch_eventpubsub_url(client, &movie_id, screen_id, cancel)
            .await
        {
            Ok(url) => url,
            Err(outcome) => return outcome,
        };
        self.send_live_started(screen_id, &movie_id);
        let ws_url = SecretWsUrl::new(ws_connect_url(&url));
        self.listen_ws(&ws_url, screen_id, channel, seen, tx, cancel)
            .await
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
                tracing::warn!(
                    "twitcasting:{screen_id} 視聴ページ取得失敗: {}",
                    e.without_url()
                );
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        let status = response.status();
        if let Some(outcome) = classify_user_page_status(status) {
            if outcome == SessionOutcome::NotFound {
                return Err(outcome);
            }
            tracing::warn!(
                "twitcasting:{screen_id} 視聴ページが HTTP {} を返しました",
                status.as_u16()
            );
            return Err(outcome);
        }
        let html = match response.text().await {
            Ok(html) => html,
            Err(e) => {
                tracing::warn!(
                    "twitcasting:{screen_id} 視聴ページ本文の読み取り失敗: {}",
                    e.without_url()
                );
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        classify_fetched_page(patterns.parse_for(&html, Some(screen_id)))
    }

    #[allow(clippy::too_many_arguments)]
    async fn load_history(
        &self,
        client: &reqwest::Client,
        movie_id: &str,
        screen_id: &str,
        channel: &str,
        seen: &mut CommentDedup,
        tx: &broadcast::Sender<ChatMessage>,
        cancel: &CancellationToken,
    ) {
        let base = self
            .overrides
            .endpoint("commentHistoryUrl", COMMENT_HISTORY_URL);
        let url = comment_history_url(&base, screen_id);
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
                    tracing::warn!(
                        "twitcasting:{channel} listall 本文の読み取り失敗: {}",
                        e.without_url()
                    );
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
                tracing::warn!(
                    "twitcasting:{channel} listall 取得失敗: {}",
                    e.without_url()
                );
                return;
            }
        };

        let ctx = MapContext {
            channel,
            broadcaster_id: Some(screen_id),
            now_ms: now_ms(),
        };
        for value in seen.admit(parse_listall(&body)) {
            if let Some(mut message) = event_to_chat(&value, &ctx) {
                // 接続前の直近コメントは既読扱い(読み上げない)。
                message.skip_tts = true;
                message.route.uncounted = true;
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
                tracing::warn!(
                    "twitcasting:{screen_id} eventpubsuburl 取得失敗: {}",
                    e.without_url()
                );
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        let status = response.status();
        if let Some(outcome) = classify_eventpubsub_status(status) {
            // Recheck は 30 秒ごとに繰り返しうるので run 側が初回だけ warn にする。
            // Backoff 経路(5xx)は run 側で出さないのでここで warn する。
            if is_quiet_outcome(outcome) {
                tracing::debug!(
                    "twitcasting:{screen_id} eventpubsuburl が HTTP {} を返しました",
                    status.as_u16()
                );
            } else {
                tracing::warn!(
                    "twitcasting:{screen_id} eventpubsuburl が HTTP {} を返しました",
                    status.as_u16()
                );
            }
            return Err(outcome);
        }
        let body = match response.text().await {
            Ok(body) => body,
            Err(e) => {
                tracing::warn!(
                    "twitcasting:{screen_id} eventpubsuburl 本文の読み取り失敗: {}",
                    e.without_url()
                );
                return Err(SessionOutcome::Disconnected { stable: false });
            }
        };
        match parse_eventpubsub_response(&body) {
            EventPubsub::Url(url) => Ok(url),
            EventPubsub::Private => {
                tracing::debug!(
                    "twitcasting:{screen_id} eventpubsuburl が url を返しません(限定配信など)"
                );
                Err(SessionOutcome::Unavailable)
            }
            EventPubsub::Malformed => {
                tracing::warn!("twitcasting:{screen_id} eventpubsuburl の応答を解析できません");
                // 2xx で本文の構造が読めない(H6)。結末は従来どおり Disconnected。
                self.send_signal(UnofficialSignal::Unreadable);
                Err(SessionOutcome::Disconnected { stable: false })
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn listen_ws(
        &self,
        ws_url: &SecretWsUrl,
        screen_id: &str,
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
        self.send_connected().await;

        let (mut write, mut read) = ws_stream.split();
        let connected_at = tokio::time::Instant::now();
        let mut received = false;
        // 接続ごとに、最初の配列フレームまで構造を確かめる(H11/H12 は各 1 回)。
        let mut array_seen = false;
        let mut unreadable_sent = false;
        let mut last_frame = tokio::time::Instant::now();

        loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    // 相手が詰まってもキャンセルを待たせない。
                    let _ = tokio::time::timeout(
                        WS_CLOSE_TIMEOUT,
                        write.send(WsMessage::Close(None)),
                    )
                    .await;
                    return SessionOutcome::Cancelled;
                }
                _ = wait_for_idle(last_frame) => {
                    tracing::warn!(
                        "twitcasting:{channel} WS が{}秒無フレーム",
                        WS_IDLE_TIMEOUT.as_secs()
                    );
                    return idle_outcome(received, connected_at);
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
                            let parsed = parse_ws_array(&text);
                            if !array_seen {
                                if parsed.is_some() {
                                    array_seen = true;
                                    self.send_signal(UnofficialSignal::Healthy);
                                } else if !unreadable_sent {
                                    unreadable_sent = true;
                                    self.send_signal(UnofficialSignal::Unreadable);
                                }
                            }
                            let entries = seen.admit(parsed.unwrap_or_default());
                            let ctx = MapContext {
                                channel,
                                broadcaster_id: Some(screen_id),
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

/// Unavailable / Recheck が同じ状態のまま続いているか。続く 2 回目以降は debug に落とす。
fn is_quiet_outcome(outcome: SessionOutcome) -> bool {
    matches!(
        outcome,
        SessionOutcome::Unavailable | SessionOutcome::Recheck | SessionOutcome::Unreadable
    )
}

fn is_repeated_quiet(last: &mut Option<SessionOutcome>, outcome: SessionOutcome) -> bool {
    let quiet = is_quiet_outcome(outcome);
    let repeated = quiet && *last == Some(outcome);
    *last = quiet.then_some(outcome);
    repeated
}

/// 視聴ページの HTTP 状態から次の遷移を決める。302/404 だけを行エラーにする。
fn classify_user_page_status(status: reqwest::StatusCode) -> Option<SessionOutcome> {
    if status.is_redirection() || status == reqwest::StatusCode::NOT_FOUND {
        Some(SessionOutcome::NotFound)
    } else if !status.is_success() {
        Some(SessionOutcome::Disconnected { stable: false })
    } else {
        None
    }
}

/// 未配信(Offline)は HTML が明示した時だけ(is-onlive=false・配信歴ゼロ)。
/// 何も読めない HTML は Unreadable にして、配信終了として扱わない。
fn classify_fetched_page(page: Option<UserPage>) -> Result<UserPage, SessionOutcome> {
    match page {
        Some(page) if page.movie_id.is_some() && page.is_onlive != Some(false) => Ok(page),
        Some(_) => Err(SessionOutcome::Offline),
        None => Err(SessionOutcome::Unreadable),
    }
}

/// eventpubsuburl の 302/4xx は、次のループで HTML から再確認させる。
fn classify_eventpubsub_status(status: reqwest::StatusCode) -> Option<SessionOutcome> {
    if status.is_success() {
        None
    } else if status.is_redirection() || status.is_client_error() {
        Some(SessionOutcome::Recheck)
    } else {
        Some(SessionOutcome::Disconnected { stable: false })
    }
}

/// セッションの結末から切替カウンタ用の信号を導く(§2 の表)。H6・H11・H12 は発生箇所で出す。
fn outcome_signal(outcome: SessionOutcome) -> Option<UnofficialSignal> {
    match outcome {
        SessionOutcome::Cancelled => None,
        SessionOutcome::Offline => Some(UnofficialSignal::Healthy),
        SessionOutcome::Unreadable => Some(UnofficialSignal::Unreadable),
        SessionOutcome::NotFound
        | SessionOutcome::Unavailable
        | SessionOutcome::Recheck
        | SessionOutcome::Disconnected { .. } => Some(UnofficialSignal::Neutral),
    }
}

/// 既定の待ち時間での各 outcome の待機時間。
#[cfg(test)]
fn outcome_delay(
    outcome: SessionOutcome,
    backoff: &mut Backoff,
    jitter_seed: &mut u64,
) -> Option<Duration> {
    outcome_delay_for(outcome, &UnofficialTuning::default(), backoff, jitter_seed)
}

/// 各 outcome の待機時間。ディレイの計算を純粋にして状態遷移をテストできる。
fn outcome_delay_for(
    outcome: SessionOutcome,
    tuning: &UnofficialTuning,
    backoff: &mut Backoff,
    jitter_seed: &mut u64,
) -> Option<Duration> {
    match outcome {
        SessionOutcome::Cancelled => None,
        SessionOutcome::Offline | SessionOutcome::Unavailable => {
            backoff.reset();
            if tuning.jitter {
                Some(jittered(tuning.offline, 20, jitter_seed))
            } else {
                Some(tuning.offline)
            }
        }
        SessionOutcome::Recheck | SessionOutcome::Unreadable => {
            // 連続失敗でも 30 秒を下回らない。
            backoff.reset();
            Some(tuning.unreadable_recheck)
        }
        SessionOutcome::NotFound => {
            backoff.reset();
            Some(tuning.not_found_retry)
        }
        SessionOutcome::Disconnected { stable } => {
            if stable {
                backoff.reset();
            }
            Some(backoff.next_delay())
        }
    }
}

/// 無フレーム監視の待機。テストで tokio の pause/advance を使えるよう切り出す。
async fn wait_for_idle(last_frame: tokio::time::Instant) {
    tokio::time::sleep_until(last_frame + WS_IDLE_TIMEOUT).await;
}

fn idle_outcome(received: bool, connected_at: tokio::time::Instant) -> SessionOutcome {
    SessionOutcome::Disconnected {
        stable: session_is_stable(received, connected_at),
    }
}

/// 枠(movie_id)が変わった時だけ listall から入り直す。
fn begin_movie(movie_id: &str, current: &mut Option<String>, seen: &mut CommentDedup) -> bool {
    if current.as_deref() == Some(movie_id) {
        return false;
    }
    seen.clear();
    *current = Some(movie_id.to_string());
    true
}

/// MCV と同じく `<screen_id>/userajax.php` を基準に listall を呼ぶ。
fn comment_history_url(base: &str, screen_id: &str) -> String {
    let user_base = join_base(base, screen_id);
    if user_base.ends_with('/') {
        format!("{user_base}userajax.php")
    } else {
        format!("{user_base}/userajax.php")
    }
}

fn join_base(base: &str, id: &str) -> String {
    if base.ends_with('/') {
        format!("{base}{id}")
    } else {
        format!("{base}/{id}")
    }
}

fn session_is_stable(received: bool, connected_at: tokio::time::Instant) -> bool {
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

    /// UI(src/lib/twitcastingId.test.ts)と共有する識別子の入力表。
    const ID_CASES: &str = include_str!("../../../src/lib/twitcastingId.cases.json");
    const TWITCASTING_JP_PAGE: &str = r#"<body data-is-mobile="false" data-mode="" data-is-onlive="false" data-view-mode="default">
...
<div id="comment-list-app" data-base-url="https://twitcasting.tv/" data-broadcaster-id="twitcasting_jp" data-broadcaster-name="ツイキャス公式" data-broadcaster-screen-name="twitcasting_jp" data-movie-id="842918966" data-audience-id="" data-csrf-token="REDACTED" data-account-type="not_logged_in"></div>"#;

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
    fn extract_screen_id_matches_ui_shared_cases() {
        let cases: Vec<(String, Option<String>)> =
            serde_json::from_str(ID_CASES).expect("twitcastingId.cases.json");
        for (input, expected) in cases {
            assert_eq!(
                extract_screen_id(&input),
                expected,
                "input={input:?} (UI と同じ結果になっていない)"
            );
        }
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
    fn invalid_override_patterns_fall_back_to_defaults_without_panicking() {
        let overrides = TwitcastingOverrides {
            patterns: HashMap::from([
                ("movieId".to_string(), "(?=lookaround)".to_string()),
                ("isOnlive".to_string(), "(".to_string()),
            ]),
            ..TwitcastingOverrides::default()
        };
        let patterns = UserPagePatterns::from_overrides(&overrides);
        let page = patterns
            .parse(r#"<div data-movie-id="777" data-is-onlive="true"></div>"#)
            .expect("既定パターンへ戻る");
        assert_eq!(page.movie_id.as_deref(), Some("777"));
        assert_eq!(page.is_onlive, Some(true));
    }

    #[test]
    fn parses_eventpubsub_response_and_appends_gift() {
        assert_eq!(
            parse_eventpubsub_response(r#"{"url":"wss://example/events?token=t"}"#),
            EventPubsub::Url("wss://example/events?token=t".to_string())
        );
        assert_eq!(parse_eventpubsub_response(r#"{"url":"  "}"#), EventPubsub::Private);
        assert_eq!(parse_eventpubsub_response(r#"{"url":null}"#), EventPubsub::Private);
        assert_eq!(parse_eventpubsub_response("{}"), EventPubsub::Private);
        for malformed in ["", "not json", r#"{"url":42}"#, r#"{"url":["x"]}"#, "[]", "null"] {
            assert_eq!(
                parse_eventpubsub_response(malformed),
                EventPubsub::Malformed,
                "body={malformed:?}"
            );
        }
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
    fn anonymous_id_requires_digits_after_c_tw() {
        for id in ["c:tw1", "c:tw23", "C:TW7"] {
            assert!(is_anonymous_id(&id.to_ascii_lowercase()), "{id}");
        }
        for id in ["c:twinkle", "c:tw", "c:tw1a", "c:twitter", "c:alice"] {
            assert!(!is_anonymous_id(id), "{id}");
        }
        let event = json!({"type":"comment","id":1,"message":"a","createdAt":1000,
            "author":{"id":"c:twinkle","name":"Twinkle","screenName":"c:twinkle"}});
        assert!(!is_anonymous(&event, &event["author"]));
        let event = json!({"type":"comment","id":1,"message":"a","createdAt":1000,
            "author":{"id":"C:TW23","name":"x","screenName":"c:tw23"}});
        assert!(is_anonymous(&event, &event["author"]));
    }

    #[test]
    fn repeated_quiet_outcomes_log_only_first_at_warn() {
        let mut last = None;
        assert!(!is_repeated_quiet(&mut last, SessionOutcome::Unavailable));
        assert!(is_repeated_quiet(&mut last, SessionOutcome::Unavailable));
        assert!(!is_repeated_quiet(&mut last, SessionOutcome::Recheck));
        assert!(is_repeated_quiet(&mut last, SessionOutcome::Recheck));
        assert!(!is_repeated_quiet(&mut last, SessionOutcome::Offline));
        assert!(!is_repeated_quiet(&mut last, SessionOutcome::Recheck));
    }

    #[test]
    fn only_recheck_style_statuses_are_quiet_candidates() {
        let quiet = |s| is_quiet_outcome(classify_eventpubsub_status(s).unwrap());
        assert!(quiet(reqwest::StatusCode::NOT_FOUND));
        assert!(!quiet(reqwest::StatusCode::SERVICE_UNAVAILABLE));
        assert!(!quiet(reqwest::StatusCode::INTERNAL_SERVER_ERROR));
    }

    #[test]
    fn extract_screen_id_trims_like_js_and_checks_slash_after_query() {
        assert_eq!(extract_screen_id("abc?next=/movie").as_deref(), Some("abc"));
        assert_eq!(extract_screen_id("abc#a/b").as_deref(), Some("abc"));
        assert_eq!(extract_screen_id("\u{85}abc?x"), None);
        assert_eq!(extract_screen_id("\u{feff}abc").as_deref(), Some("abc"));
    }

    /// 全リクエストへ同じ本文を返す最小の HTTP サーバー。リクエスト数を数える。
    async fn serve_html(body: &'static str) -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(response.as_bytes()).await;
                let _ = sock.shutdown().await;
            }
        });
        (port, hits)
    }

    fn source_for_page(port: u16) -> (TwitcastingSource, mpsc::Receiver<LiveEvent>) {
        let overrides = TwitcastingOverrides {
            endpoints: HashMap::from([(
                "userPageBaseUrl".to_string(),
                format!("http://127.0.0.1:{port}/"),
            )]),
            ..TwitcastingOverrides::default()
        };
        let (live_tx, live_rx) = mpsc::channel(8);
        let source = TwitcastingSource::new("alice".to_string(), overrides, None)
            .with_live_tx(Some(live_tx));
        (source, live_rx)
    }

    #[tokio::test]
    async fn unreadable_html_never_sends_ended() {
        let (port, hits) = serve_html("<html></html>").await;
        let (source, mut live_rx) = source_for_page(port);
        let (tx, _rx) = broadcast::channel(8);
        let cancel = CancellationToken::new();
        let runner = {
            let cancel = cancel.clone();
            tokio::spawn(async move { source.run(tx, cancel).await })
        };
        while hits.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancel.cancel();
        let _ = runner.await;
        assert!(live_rx.try_recv().is_err(), "読めない HTML で Ended を送らない");
    }

    #[tokio::test]
    async fn explicit_offline_html_sends_ended() {
        let (port, _hits) = serve_html(r#"<body data-is-onlive="false"></body>"#).await;
        let (source, mut live_rx) = source_for_page(port);
        let (tx, _rx) = broadcast::channel(8);
        let cancel = CancellationToken::new();
        let runner = {
            let cancel = cancel.clone();
            tokio::spawn(async move { source.run(tx, cancel).await })
        };
        let event = tokio::time::timeout(Duration::from_secs(5), live_rx.recv())
            .await
            .expect("Ended が届く")
            .expect("channel open");
        cancel.cancel();
        let _ = runner.await;
        assert!(matches!(event, LiveEvent::Ended { .. }));
    }

    #[test]
    fn foreign_broadcaster_status_does_not_end_alice() {
        let bob = r#"<div data-broadcaster-id="bob" data-movie-id="7" data-is-onlive="false"></div>"#;
        assert_eq!(
            classify_fetched_page(parse_user_page_for(bob, "alice")),
            Err(SessionOutcome::Unreadable)
        );

        let alice =
            r#"<div data-broadcaster-id="Alice" data-movie-id="42" data-is-onlive="TrUe"></div>"#;
        let page = classify_fetched_page(parse_user_page_for(&format!("{bob}{alice}"), "alice"))
            .expect("Alice is live");
        assert_eq!(page.movie_id.as_deref(), Some("42"));
        assert_eq!(page.is_onlive, Some(true));
        assert_eq!(
            classify_fetched_page(parse_user_page_for(
                r#"<body data-is-onlive="FaLsE">"#,
                "alice"
            )),
            Err(SessionOutcome::Offline)
        );
    }

    #[test]
    fn either_broadcaster_attribute_can_match() {
        for html in [
            r#"<div data-broadcaster-id="182224938" data-broadcaster-screen-name="alice" data-movie-id="42" data-is-onlive="true">"#,
            r#"<div data-broadcaster-id="182224938" data-broadcaster-screen-name=" ALIce " data-movie-id="42" data-is-onlive="true">"#,
            r#"<div data-broadcaster-id=" ALIce " data-broadcaster-screen-name="other" data-movie-id="42" data-is-onlive="true">"#,
        ] {
            assert_eq!(
                classify_fetched_page(parse_user_page_for(html, "alice")),
                Ok(UserPage {
                    movie_id: Some("42".into()),
                    is_onlive: Some(true),
                })
            );
        }
    }

    #[test]
    fn official_page_reads_body_status_for_its_broadcaster() {
        let page = parse_user_page_for(TWITCASTING_JP_PAGE, "twitcasting_jp");
        assert_eq!(
            page.as_ref().and_then(|page| page.movie_id.as_deref()),
            Some("842918966")
        );
        assert_eq!(classify_fetched_page(page), Err(SessionOutcome::Offline));

        let live =
            TWITCASTING_JP_PAGE.replace(r#"data-is-onlive="false""#, r#"data-is-onlive="true""#);
        assert_eq!(
            classify_fetched_page(parse_user_page_for(&live, "twitcasting_jp")),
            Ok(UserPage {
                movie_id: Some("842918966".into()),
                is_onlive: Some(true),
            })
        );
    }

    #[test]
    fn official_page_does_not_end_another_broadcasters_stream() {
        assert_eq!(
            classify_fetched_page(parse_user_page_for(TWITCASTING_JP_PAGE, "someone_else")),
            Err(SessionOutcome::Unreadable)
        );
    }

    #[test]
    fn user_page_ignores_commented_movie_ids_and_status() {
        let comment = r#"<!-- <div data-broadcaster-id="alice" data-movie-id="7" data-is-onlive="false"></div><script>var movieid = "8";</script> -->"#;
        assert_eq!(
            classify_fetched_page(parse_user_page_for(comment, "alice")),
            Err(SessionOutcome::Unreadable)
        );
        for movie in [
            r#"<div data-movie-id="42"></div>"#,
            r#"<script>var movieid = "42";</script>"#,
        ] {
            let html = format!(r#"{comment}<body data-is-onlive="true">{movie}"#);
            assert_eq!(
                classify_fetched_page(parse_user_page_for(&html, "alice")),
                Ok(UserPage {
                    movie_id: Some("42".into()),
                    is_onlive: Some(true),
                })
            );
        }
        assert_eq!(
            classify_fetched_page(parse_user_page_for(
                r#"<!-- <script>var movieid = "8";</script> --><body data-is-onlive="true">"#,
                "alice",
            )),
            Err(SessionOutcome::Unreadable)
        );
    }

    #[test]
    fn unknown_onlive_override_is_unreadable() {
        let overrides = TwitcastingOverrides {
            patterns: HashMap::from([(
                "isOnlive".to_string(),
                r#"(?i)data-is-onlive="(\w+)""#.to_string(),
            )]),
            ..TwitcastingOverrides::default()
        };
        let patterns = UserPagePatterns::from_overrides(&overrides);
        for body in [
            r#"<body data-is-onlive="不明">"#,
            r#"<body data-is-onlive="不明" data-movie-id="42">"#,
        ] {
            assert_eq!(
                classify_fetched_page(patterns.parse_for(body, Some("alice"))),
                Err(SessionOutcome::Unreadable)
            );
        }
    }

    #[test]
    fn unreadable_is_a_quiet_recheck_of_at_least_30s() {
        let mut last = None;
        assert!(!is_repeated_quiet(&mut last, SessionOutcome::Unreadable));
        assert!(is_repeated_quiet(&mut last, SessionOutcome::Unreadable));
        let mut backoff = Backoff::new();
        let mut seed = 1;
        let delay = outcome_delay(SessionOutcome::Unreadable, &mut backoff, &mut seed).unwrap();
        assert!(delay >= OFFLINE_INTERVAL);
    }

    #[tokio::test]
    async fn started_stays_deduplicated_after_ended_and_source_replacement() {
        let (port, _) = serve_html(r#"<body data-is-onlive="false"></body>"#).await;
        let (source, mut live_rx) = source_for_page(port);
        source.send_live_started("alice", "42");
        source.send_live_started("alice", "42");
        let Ok(LiveEvent::Started(first)) = live_rx.try_recv() else {
            panic!("missing Started(42)");
        };
        assert_eq!(first.stream_id, "42");
        assert_eq!(
            live_rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );

        let (tx, _rx) = broadcast::channel(8);
        let cancel = CancellationToken::new();
        {
            let runner = source.run(tx, cancel.clone());
            tokio::pin!(runner);
            tokio::select! {
                _ = &mut runner => panic!("source stopped before Ended"),
                event = tokio::time::timeout(Duration::from_secs(5), live_rx.recv()) => {
                    assert!(matches!(event.unwrap(), Some(LiveEvent::Ended { .. })));
                }
            }
            cancel.cancel();
            runner.await.unwrap();
        }
        source.send_live_started("alice", "42");
        assert_eq!(
            live_rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );

        // 設定更新で Source を作り直した場合も、同じアプリ内では記録を共有する。
        let replacement = TwitcastingSource::new("alice".into(), Default::default(), None)
            .with_started_movies(source.started_movies.clone())
            .with_live_tx(source.live_tx.clone());
        replacement.send_live_started("alice", "42");
        replacement.send_live_started("alice", "43");
        replacement.send_live_started("alice", "42");
        assert!(matches!(
            live_rx.try_recv(),
            Ok(LiveEvent::Started(stream)) if stream.stream_id == "43"
        ));
        assert_eq!(
            live_rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );
    }

    #[test]
    fn live_events_use_normalized_screen_id_without_url_fragment() {
        let (live_tx, mut live_rx) = mpsc::channel(2);
        let source = TwitcastingSource::new(
            "https://twitcasting.tv/Alice#access_token=MARKER".into(),
            Default::default(),
            None,
        )
        .with_live_tx(Some(live_tx));
        source.send_live_started(source.screen_id.as_deref().unwrap(), "42");
        source.send_live_ended();
        let started = live_rx.try_recv().unwrap();
        let ended = live_rx.try_recv().unwrap();
        assert!(matches!(
            &started,
            LiveEvent::Started(stream) if stream.channel_identifier == "alice"
        ));
        assert!(matches!(
            &ended,
            LiveEvent::Ended { channel_identifier, .. } if channel_identifier == "alice"
        ));
        assert!(!format!("{started:?} {ended:?}").contains("MARKER"));
    }

    #[test]
    fn started_evicts_oldest_successful_movie_at_capacity() {
        let (live_tx, mut live_rx) = mpsc::channel(1);
        let source = TwitcastingSource::new("alice".into(), Default::default(), None)
            .with_live_tx(Some(live_tx));
        for id in 1..=256 {
            let id = id.to_string();
            source.send_live_started("alice", &id);
            assert!(matches!(
                live_rx.try_recv(),
                Ok(LiveEvent::Started(stream)) if stream.stream_id == id
            ));
        }

        source.send_live_started("alice", "1");
        assert_eq!(
            live_rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );
        source.send_live_started("alice", "257");
        source.send_live_started("alice", "258");
        assert!(matches!(
            live_rx.try_recv(),
            Ok(LiveEvent::Started(stream)) if stream.stream_id == "257"
        ));
        for id in ["2", "256", "257"] {
            source.send_live_started("alice", id);
            assert_eq!(
                live_rx.try_recv().unwrap_err(),
                mpsc::error::TryRecvError::Empty
            );
        }
        source.send_live_started("alice", "1");
        assert!(matches!(
            live_rx.try_recv(),
            Ok(LiveEvent::Started(stream)) if stream.stream_id == "1"
        ));
    }

    #[test]
    fn started_retries_movie_43_after_a_full_notification_queue() {
        let (live_tx, mut live_rx) = mpsc::channel(1);
        let source = TwitcastingSource::new("alice".into(), Default::default(), None)
            .with_live_tx(Some(live_tx));
        source.send_live_ended();
        source.send_live_started("alice", "43");
        assert!(matches!(live_rx.try_recv(), Ok(LiveEvent::Ended { .. })));
        source.send_live_started("alice", "43");
        assert!(matches!(
            live_rx.try_recv(),
            Ok(LiveEvent::Started(stream)) if stream.stream_id == "43"
        ));
        source.send_live_started("alice", "43");
        assert_eq!(
            live_rx.try_recv().unwrap_err(),
            mpsc::error::TryRecvError::Empty
        );
    }

    #[tokio::test]
    async fn eventpubsub_connection_error_omits_override_url_and_query() {
        use tracing::instrument::WithSubscriber;

        let url = "http://127.0.0.1:1/?token=SECRET";
        let overrides = TwitcastingOverrides {
            endpoints: HashMap::from([("eventPubsubUrl".into(), url.into())]),
            ..Default::default()
        };
        let source = TwitcastingSource::new("alice".into(), overrides, None);
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let sink = crate::log_filter_tests::Sink::default();
        let outcome = source
            .fetch_eventpubsub_url(&client, "42", "alice", &CancellationToken::new())
            .with_subscriber(crate::log_subscriber(Some("warn"), sink.clone()))
            .await;
        assert_eq!(outcome, Err(SessionOutcome::Disconnected { stable: false }));
        let output = sink.output();
        assert!(output.contains("eventpubsuburl 取得失敗"));
        assert!(!output.contains("127.0.0.1:1"));
        assert!(!output.contains("SECRET"));
    }

    #[test]
    fn unavailable_notice_matches_design() {
        assert_eq!(UNAVAILABLE_NOTICE, "この配信は非公開のため取得できません");
    }

    #[tokio::test]
    async fn ws_connect_sends_live_true_without_notice_then_disconnect_is_not_live() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(sock).await.unwrap();
            let _ = ws.close(None).await;
        });
        let (mtx, mut mrx) = mpsc::channel(8);
        let source = TwitcastingSource::new(
            "alice".to_string(),
            TwitcastingOverrides::default(),
            Some(mtx),
        );
        let (tx, _rx) = broadcast::channel(8);
        let mut seen = CommentDedup::new(10);
        let cancel = CancellationToken::new();
        let url = SecretWsUrl::new(format!("ws://127.0.0.1:{port}/"));
        let outcome = source
            .listen_ws(&url, "alice", "alice", &mut seen, &tx, &cancel)
            .await;
        let update = mrx.try_recv().expect("接続成功で通知が届く");
        assert_eq!(update.live, Some(true));
        assert_eq!(update.notice, None);
        // 切断は Disconnected で返り、run ループが live=false を送る経路に入る。
        assert!(matches!(outcome, SessionOutcome::Disconnected { .. }));
        source.send_live_state(false).await;
        assert_eq!(mrx.try_recv().expect("切断後").live, Some(false));
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
    fn broadcaster_role_matches_screen_id_case_insensitively() {
        let ctx = MapContext {
            channel: "alice",
            broadcaster_id: Some("Alice"),
            now_ms: 1_800_000_000_000,
        };
        let frame = r#"[
            {"type":"comment","id":1,"message":"a","createdAt":1000,
             "author":{"id":"alice","name":"配信者"}},
            {"type":"comment","id":2,"message":"b","createdAt":2000,
             "author":{"id":"viewer","name":"視聴者"}}
        ]"#;
        let messages = parse_ws_frame(frame, &ctx);
        assert!(messages[0].author.roles.broadcaster);
        assert!(!messages[1].author.roles.broadcaster);

        let gift = r#"[
            {"id":"g1","type":"gift","message":"(+🍡10)","createdAt":3000,
             "item":{"name":"お団子"},"sender":{"id":"ALICE","name":"配信者"},
             "score_items":[{"title":"🍡","text":"10"}]}
        ]"#;
        let messages = parse_ws_frame(gift, &ctx);
        assert!(messages[0].author.roles.broadcaster);
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
    fn gift_keeps_item_name_and_score_notation_without_amount() {
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
        assert_eq!(text, "お団子 🍡10");
        assert!(!text.contains('×'), "text={text}");
    }

    #[test]
    fn anonymous_gift_sender_twitcastinguest_is_anonymized() {
        // .omc/research/twitcasting-unofficial-ws-2026-11-20.md 61〜68 行の実測形。
        let frame = r#"[
            {"id":"a085f979.6aff62d1793795.43223706","type":"gift","message":"(+🍡10)",
             "plainMessage":"","isPaidGift":false,
             "item":{"name":"お団子","image":"https://s01.twitcasting.tv/img/item_frame.png",
                     "detailImage":"","effectCommand":"frameitem(\"https://twitcasting.tv/img/anim/a.json\")",
                     "showsSenderInfo":true},
             "sender":{"id":"twitcastinguest","name":"匿名さん","screenName":"TwitCastinGuest",
                       "profileImage":"https://imagegw02.twitcasting.tv/image3s/guest.jpg","grade":0},
             "createdAt":1795121873000,"score_items":[{"title":"🍡","text":"10"}]}
        ]"#;
        let raw: Value = serde_json::from_str(frame).expect("frame json");
        assert_eq!(raw[0]["sender"]["id"].as_str(), Some("twitcastinguest"));
        assert!(is_anonymous(&raw[0], &raw[0]["sender"]));
        let upper = json!({"sender":{"id":"TWITCASTINGUEST","name":"匿名さん"}});
        assert!(is_anonymous(&upper, &upper["sender"]), "大小無視");
        let messages = parse_ws_frame(frame, &ctx());
        assert_eq!(messages.len(), 1);
        let message = &messages[0];
        assert_eq!(message.author.id, ANONYMOUS_AUTHOR_ID);
        assert_eq!(message.author.name, "匿名さん");
        assert_eq!(message.author.badges[0].kind, "anonymous");
        assert!(!message.author.roles.broadcaster);
        assert_eq!(message.plain_text(), "お団子 🍡10");
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

    #[tokio::test]
    async fn listall_history_is_uncounted() {
        let (port, _) = serve_html(r#"{"comments":[
            {"type":"comment","id":1,"message":"history","author":{"id":"u","name":"N"}}
        ]}"#).await;
        let overrides = TwitcastingOverrides {
            endpoints: HashMap::from([(
                "commentHistoryUrl".to_owned(),
                format!("http://127.0.0.1:{port}/"),
            )]),
            ..TwitcastingOverrides::default()
        };
        let source = TwitcastingSource::new("alice".to_owned(), overrides, None);
        let (tx, mut rx) = broadcast::channel(4);
        source
            .load_history(
                &reqwest::Client::new(),
                "42",
                "alice",
                "alice",
                &mut CommentDedup::new(10),
                &tx,
                &CancellationToken::new(),
            )
            .await;
        let message = rx.try_recv().unwrap();
        assert_eq!(message.plain_text(), "history");
        assert!(message.route.uncounted);
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
    fn gift_text_uses_score_notation_instead_of_count() {
        let value = json!({
            "type": "gift",
            "message": "(+🍡10)",
            "item": {"name": "お団子"},
            "score_items": [{"title": "🍡", "text": "10"}],
        });
        assert_eq!(gift_text(&value), "お団子 🍡10");
        let no_score = json!({"type": "gift", "item": {"name": "ハート"}});
        assert_eq!(gift_text(&no_score), "ハート");
    }

    #[test]
    fn user_page_prefers_the_broadcasters_own_movie_id() {
        let html = concat!(
            r#"<div class="card" data-broadcaster-screen-name="other" data-movie-id="111"></div>"#,
            r#"<div id="comment-list-app" data-broadcaster-screen-name="Alice" data-movie-id="222"></div>"#,
            r#"<body data-is-onlive="true"></body>"#
        );
        let page = parse_user_page_for(html, "Alice").expect("own page");
        assert_eq!(page.movie_id.as_deref(), Some("222"));
        let page = parse_user_page_for(html, "alice").expect("case-insensitive own page");
        assert_eq!(page.movie_id.as_deref(), Some("222"));
        assert!(parse_user_page_for(html, "bob").is_none());

        let legacy = r#"<div data-movie-id="333" data-is-onlive="true"></div>"#;
        assert_eq!(
            parse_user_page_for(legacy, "Alice")
                .expect("legacy page")
                .movie_id
                .as_deref(),
            Some("333")
        );
    }

    #[test]
    fn missing_movie_id_is_waiting_and_only_302_is_an_error() {
        assert_eq!(classify_fetched_page(None), Err(SessionOutcome::Unreadable));
        let zero_history = UserPage {
            movie_id: None,
            is_onlive: Some(false),
        };
        assert_eq!(
            classify_fetched_page(Some(zero_history)),
            Err(SessionOutcome::Offline)
        );
        let live = UserPage {
            movie_id: Some("1".to_string()),
            is_onlive: Some(true),
        };
        assert_eq!(classify_fetched_page(Some(live.clone())), Ok(live));

        assert_eq!(
            classify_user_page_status(reqwest::StatusCode::FOUND),
            Some(SessionOutcome::NotFound)
        );
        assert_eq!(
            classify_user_page_status(reqwest::StatusCode::NOT_FOUND),
            Some(SessionOutcome::NotFound)
        );
        assert_eq!(
            classify_user_page_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            Some(SessionOutcome::Disconnected { stable: false })
        );
        assert_eq!(classify_user_page_status(reqwest::StatusCode::OK), None);
    }

    #[test]
    fn eventpubsub_redirect_and_client_errors_recheck_html_after_30s() {
        for status in [
            reqwest::StatusCode::FOUND,
            reqwest::StatusCode::BAD_REQUEST,
            reqwest::StatusCode::NOT_FOUND,
        ] {
            assert_eq!(
                classify_eventpubsub_status(status),
                Some(SessionOutcome::Recheck)
            );
        }
        assert_eq!(
            classify_eventpubsub_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            Some(SessionOutcome::Disconnected { stable: false })
        );
        assert_eq!(classify_eventpubsub_status(reqwest::StatusCode::OK), None);

        let mut backoff = Backoff::new();
        let mut seed = 1;
        for _ in 0..5 {
            let delay = outcome_delay(SessionOutcome::Recheck, &mut backoff, &mut seed)
                .expect("recheck delay");
            assert!(delay >= OFFLINE_INTERVAL, "delay={delay:?}");
        }
    }

    #[test]
    fn not_found_retries_after_five_minutes() {
        let mut backoff = Backoff::new();
        let mut seed = 1;
        assert_eq!(NOT_FOUND_RETRY, Duration::from_secs(300));
        assert_eq!(
            outcome_delay(SessionOutcome::NotFound, &mut backoff, &mut seed),
            Some(NOT_FOUND_RETRY)
        );
    }

    #[test]
    fn backoff_delays_grow_then_cap() {
        let mut backoff = Backoff::new();
        let mut seed = 1;
        let d1 = outcome_delay(
            SessionOutcome::Disconnected { stable: false },
            &mut backoff,
            &mut seed,
        )
        .expect("first");
        let d2 = outcome_delay(
            SessionOutcome::Disconnected { stable: false },
            &mut backoff,
            &mut seed,
        )
        .expect("second");
        let d3 = outcome_delay(
            SessionOutcome::Disconnected { stable: false },
            &mut backoff,
            &mut seed,
        )
        .expect("third");
        assert!(d2 > d1, "d1={d1:?} d2={d2:?}");
        assert!(d3 > d2, "d2={d2:?} d3={d3:?}");
        for _ in 0..12 {
            let delay = outcome_delay(
                SessionOutcome::Disconnected { stable: false },
                &mut backoff,
                &mut seed,
            )
            .expect("capped");
            assert!(delay <= Duration::from_secs(36), "delay={delay:?}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn ws_idle_timeout_is_90s_then_html_is_rechecked() {
        assert_eq!(WS_IDLE_TIMEOUT, Duration::from_secs(90));
        let start = tokio::time::Instant::now();
        let waiter = wait_for_idle(start);
        tokio::pin!(waiter);
        let mut finished = false;
        tokio::select! {
            _ = &mut waiter => finished = true,
            _ = tokio::task::yield_now() => {}
        }
        assert!(!finished, "90 秒経過前に idle になった");
        tokio::time::advance(WS_IDLE_TIMEOUT).await;
        waiter.await;
        assert!(matches!(
            idle_outcome(false, start),
            SessionOutcome::Disconnected { .. }
        ));
    }

    #[test]
    fn movie_change_restarts_from_listall_and_keeps_seen_within_same_movie() {
        let mut seen = CommentDedup::new(10);
        let mut current = None;
        assert!(begin_movie("100", &mut current, &mut seen));

        let first = parse_ws_entries(
            r#"[{"type":"comment","id":1,"message":"a","createdAt":1000,"author":{"id":"u","name":"N"}}]"#,
        );
        assert_eq!(seen.admit(first).len(), 1);

        assert!(!begin_movie("100", &mut current, &mut seen));
        let same = parse_ws_entries(
            r#"[{"type":"comment","id":1,"message":"a","createdAt":1000,"author":{"id":"u","name":"N"}}]"#,
        );
        assert_eq!(seen.admit(same).len(), 0);

        assert!(begin_movie("200", &mut current, &mut seen));
        let again = parse_ws_entries(
            r#"[{"type":"comment","id":1,"message":"a","createdAt":1000,"author":{"id":"u","name":"N"}}]"#,
        );
        assert_eq!(seen.admit(again).len(), 1);
    }

    #[test]
    fn comment_history_url_uses_screen_id_and_parses_actual_shape() {
        assert_eq!(
            comment_history_url("https://twitcasting.tv/", "Alice"),
            "https://twitcasting.tv/Alice/userajax.php"
        );
        assert_eq!(
            comment_history_url("https://example.test/api", "a"),
            "https://example.test/api/a/userajax.php"
        );

        // 2026-11-20 の実測応答と同じ形(comments[].author の 5 キー)。
        let body = r#"{"comments":[{"type":"comment","id":34006263354,"message":"m",
            "createdAt":1795123404000,
            "author":{"id":"u","name":"N","screenName":"u",
                      "profileImage":"https://image.example/a.jpg","grade":0}}]}"#;
        let items = parse_listall(body);
        assert_eq!(items.len(), 1);
        assert_eq!(
            id_string(items[0].get("id")).as_deref(),
            Some("34006263354")
        );
    }

    // ---- PR3' B3: 既存の結末から導く UnofficialSignal(§2 の表) ----

    const LIVE_HTML: &str =
        r#"<html><body data-is-onlive="true"><div data-movie-id="42"></div></body></html>"#;

    /// 経路で応答を分ける最小の HTTP サーバー。POST は eventpubsuburl、userajax は listall、
    /// それ以外は視聴ページ。リクエストは本文まで読んでから返す。
    async fn serve_routes(html: (u16, &'static str), pubsub: (u16, String)) -> u16 {
        serve_pages(&[], html, pubsub).await
    }

    /// 視聴ページの n 回目に `first[n]` を返し、尽きたら `html` を返す。
    async fn serve_pages(
        first: &'static [(u16, &'static str)],
        html: (u16, &'static str),
        pubsub: (u16, String),
    ) -> u16 {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let pubsub = std::sync::Arc::new(pubsub);
        let page_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { return };
                let pubsub = pubsub.clone();
                let page_hits = page_hits.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut chunk = [0u8; 4096];
                    let head_end = loop {
                        let Ok(n) = sock.read(&mut chunk).await else { return };
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break i + 4;
                        }
                    };
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length:"))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    while buf.len() < head_end + length {
                        let Ok(n) = sock.read(&mut chunk).await else { return };
                        if n == 0 {
                            break;
                        }
                        buf.extend_from_slice(&chunk[..n]);
                    }
                    let (status, body): (u16, &str) = if head.starts_with("post") {
                        (pubsub.0, pubsub.1.as_str())
                    } else if head.contains("userajax") {
                        (200, "[]")
                    } else {
                        let n = page_hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        first.get(n).copied().unwrap_or(html)
                    };
                    let response = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = sock.write_all(response.as_bytes()).await;
                    let _ = sock.shutdown().await;
                });
            }
        });
        port
    }

    /// 接続ごとに frames を Text で送って閉じる WS サーバー。
    async fn serve_ws(frames: &'static [&'static str]) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { return };
                tokio::spawn(async move {
                    let Ok(mut ws) = tokio_tungstenite::accept_async(sock).await else { return };
                    for frame in frames {
                        if ws.send(WsMessage::Text(frame.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    let _ = ws.close(None).await;
                });
            }
        });
        port
    }

    fn short_tuning() -> UnofficialTuning {
        UnofficialTuning {
            offline: Duration::from_millis(20),
            unreadable_recheck: Duration::from_millis(20),
            not_found_retry: Duration::from_millis(20),
            jitter: false,
        }
    }

    fn signal_source(
        base: &str,
    ) -> (
        TwitcastingSource,
        mpsc::UnboundedReceiver<UnofficialSignal>,
        mpsc::Receiver<YoutubeMetadataUpdate>,
    ) {
        let overrides = TwitcastingOverrides {
            endpoints: HashMap::from([
                ("userPageBaseUrl".to_string(), format!("{base}/")),
                ("eventPubsubUrl".to_string(), format!("{base}/eventpubsub")),
                ("commentHistoryUrl".to_string(), format!("{base}/")),
            ]),
            ..TwitcastingOverrides::default()
        };
        let (signal_tx, signal_rx) = mpsc::unbounded_channel();
        let (metadata_tx, metadata_rx) = mpsc::channel(256);
        let source = TwitcastingSource::new("alice".to_string(), overrides, Some(metadata_tx))
            .with_tuning(short_tuning())
            .with_signal_tx(signal_tx);
        (source, signal_rx, metadata_rx)
    }

    /// Source を走らせ、`done` が真になるまで信号を集めて止める。notice も返す。
    async fn collect_signals(
        base: String,
        done: impl Fn(&[UnofficialSignal]) -> bool,
    ) -> (Vec<UnofficialSignal>, Vec<String>) {
        let (source, mut signal_rx, mut metadata_rx) = signal_source(&base);
        let (tx, _rx) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let runner = {
            let cancel = cancel.clone();
            tokio::spawn(async move { source.run(tx, cancel).await })
        };
        let mut signals = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(10), async {
            while !done(&signals) {
                match signal_rx.recv().await {
                    Some(signal) => signals.push(signal),
                    None => return,
                }
            }
        })
        .await;
        cancel.cancel();
        let _ = runner.await;
        let mut notices = Vec::new();
        while let Ok(update) = metadata_rx.try_recv() {
            notices.extend(update.notice);
        }
        (signals, notices)
    }

    async fn signals_for_page(html: &'static str, n: usize) -> (Vec<UnofficialSignal>, Vec<String>) {
        let port = serve_routes((200, html), (200, "{}".into())).await;
        collect_signals(format!("http://127.0.0.1:{port}"), |s| s.len() >= n).await
    }

    fn until_neutral(signals: &[UnofficialSignal]) -> bool {
        signals.last() == Some(&UnofficialSignal::Neutral)
    }

    #[tokio::test]
    async fn signal_h1_no_data_page_is_unreadable() {
        let (signals, notices) =
            signals_for_page("<html><body><p>maintenance</p></body></html>", 3).await;
        assert_eq!(signals, vec![UnofficialSignal::Unreadable; 3]);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[tokio::test]
    async fn signal_h2_live_without_movie_id_is_unreadable() {
        let (signals, notices) =
            signals_for_page(r#"<html><body data-is-onlive="true"></body></html>"#, 3).await;
        assert_eq!(signals, vec![UnofficialSignal::Unreadable; 3]);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[tokio::test]
    async fn signal_h3_garbage_onlive_is_unreadable() {
        let (signals, notices) = signals_for_page(
            r#"<html><body data-is-onlive="maybe"><div data-movie-id="42"></div></body></html>"#,
            3,
        )
        .await;
        assert_eq!(signals, vec![UnofficialSignal::Unreadable; 3]);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[tokio::test]
    async fn signal_h4_explicit_offline_is_healthy() {
        let (signals, notices) =
            signals_for_page(r#"<html><body data-is-onlive="false"></body></html>"#, 2).await;
        assert_eq!(signals, vec![UnofficialSignal::Healthy; 2]);
        assert!(notices.is_empty(), "{notices:?}");
    }

    #[tokio::test]
    async fn signal_h5_eventpubsub_private_is_neutral() {
        for body in [r#"{}"#, r#"{"url":null}"#, r#"{"url":""}"#] {
            let port = serve_routes((200, LIVE_HTML), (200, body.into())).await;
            let (signals, notices) =
                collect_signals(format!("http://127.0.0.1:{port}"), |s| s.len() >= 2).await;
            assert_eq!(signals, vec![UnofficialSignal::Neutral; 2], "{body}");
            assert!(notices.iter().all(|n| n == UNAVAILABLE_NOTICE), "{body}: {notices:?}");
            assert!(!notices.is_empty(), "{body}");
        }
    }

    #[tokio::test]
    async fn signal_h6_eventpubsub_malformed_2xx_is_unreadable() {
        for body in ["not json", "[1]", r#"{"url":1}"#] {
            let port = serve_routes((200, LIVE_HTML), (200, body.into())).await;
            let (signals, notices) =
                collect_signals(format!("http://127.0.0.1:{port}"), until_neutral).await;
            // 結末は従来どおり Disconnected(Neutral)。その前に Unreadable が 1 回出る。
            assert_eq!(
                signals,
                vec![UnofficialSignal::Unreadable, UnofficialSignal::Neutral],
                "{body}"
            );
            assert!(notices.is_empty(), "{body}: {notices:?}");
        }
    }

    #[tokio::test]
    async fn signal_h7_h8_eventpubsub_status_is_neutral() {
        // 3xx/4xx は 20ms の Recheck で繰り返すので数回見る。5xx は Backoff なので最初の 1 回。
        for (status, n) in [(302, 3), (404, 3), (500, 1), (503, 1)] {
            let port = serve_routes((200, LIVE_HTML), (status, "{}".into())).await;
            let (signals, notices) =
                collect_signals(format!("http://127.0.0.1:{port}"), |s| s.len() >= n).await;
            assert_eq!(signals, vec![UnofficialSignal::Neutral; n], "HTTP {status}");
            assert!(notices.is_empty(), "HTTP {status}: {notices:?}");
        }
    }

    #[tokio::test]
    async fn signal_h9_h10_network_and_not_found_are_neutral() {
        let port = serve_routes((500, "oops"), (200, "{}".into())).await;
        let (signals, notices) =
            collect_signals(format!("http://127.0.0.1:{port}"), |s| !s.is_empty()).await;
        assert_eq!(signals, vec![UnofficialSignal::Neutral], "HTML 5xx");
        assert!(notices.is_empty(), "{notices:?}");

        let (signals, notices) =
            collect_signals("http://127.0.0.1:1".into(), |s| !s.is_empty()).await;
        assert_eq!(signals, vec![UnofficialSignal::Neutral], "接続拒否");
        assert!(notices.is_empty(), "{notices:?}");

        let port = serve_routes((404, "no"), (200, "{}".into())).await;
        let (signals, notices) =
            collect_signals(format!("http://127.0.0.1:{port}"), |s| s.len() >= 2).await;
        assert_eq!(signals, vec![UnofficialSignal::Neutral; 2], "404");
        assert!(notices.iter().all(|n| n == NOT_FOUND_NOTICE), "{notices:?}");
        assert!(!notices.is_empty());
    }

    async fn ws_signals(frames: &'static [&'static str]) -> Vec<UnofficialSignal> {
        let ws_port = serve_ws(frames).await;
        let pubsub = format!(r#"{{"url":"ws://127.0.0.1:{ws_port}/"}}"#);
        let port = serve_routes((200, LIVE_HTML), (200, pubsub)).await;
        collect_signals(format!("http://127.0.0.1:{port}"), until_neutral).await.0
    }

    #[tokio::test]
    async fn signal_h11_first_array_frame_is_healthy_once() {
        let signals = ws_signals(&["[]", r#"[{"type":"unknown"}]"#, "[]"]).await;
        // 切断は Neutral。配列フレームが何枚でも Healthy は接続ごとに 1 回。
        assert_eq!(signals, vec![UnofficialSignal::Healthy, UnofficialSignal::Neutral]);
    }

    #[tokio::test]
    async fn signal_h12_non_array_frame_before_first_array_is_unreadable_once() {
        let signals = ws_signals(&[r#"{"x":1}"#, "ping", "[]"]).await;
        assert_eq!(
            signals,
            vec![
                UnofficialSignal::Unreadable,
                UnofficialSignal::Healthy,
                UnofficialSignal::Neutral
            ]
        );
    }

    #[tokio::test]
    async fn signal_h12b_non_array_frame_after_array_is_neutral() {
        let signals = ws_signals(&["[]", r#"{"x":1}"#, "ping"]).await;
        assert_eq!(signals, vec![UnofficialSignal::Healthy, UnofficialSignal::Neutral]);
    }

    #[tokio::test]
    async fn ws_signal_and_entries_come_from_one_parse() {
        // 前後の空白(NBSP)は既存の処理が trim して配列として読む。信号も同じ結果で Healthy。
        let signals = ws_signals(&["\u{00A0}[]\u{00A0}"]).await;
        assert_eq!(signals, vec![UnofficialSignal::Healthy, UnofficialSignal::Neutral]);
        assert_eq!(parse_ws_array("\u{00A0}[1]\u{00A0}"), Some(vec![json!(1)]));
        assert_eq!(parse_ws_entries("\u{00A0}[1]\u{00A0}"), vec![json!(1)]);
    }

    #[tokio::test]
    async fn signals_are_not_lost_while_the_receiver_is_not_read() {
        // H12 → H11 の接続の後、HTML が読めなくなる(H1×2)。受け手が読まずに溜めても
        // 間の Healthy が落ちない(切替カウンタが実際のリセットを見落とさない)。
        let ws_port = serve_ws(&[r#"{"x":1}"#, "[]"]).await;
        let pubsub = format!(r#"{{"url":"ws://127.0.0.1:{ws_port}/"}}"#);
        let port = serve_pages(
            &[(200, LIVE_HTML)],
            (200, "<html><body><p>maintenance</p></body></html>"),
            (200, pubsub),
        )
        .await;
        let (source, mut signal_rx, _metadata_rx) = signal_source(&format!("http://127.0.0.1:{port}"));
        let (tx, _rx) = broadcast::channel(64);
        let cancel = CancellationToken::new();
        let runner = {
            let cancel = cancel.clone();
            tokio::spawn(async move { source.run(tx, cancel).await })
        };
        // 受け手は読まない。Backoff 1 秒の後、20ms ごとに Unreadable が溜まる。
        let mut signals = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        while signals.iter().filter(|s| **s == UnofficialSignal::Unreadable).count() < 3
            && tokio::time::Instant::now() < deadline
        {
            tokio::time::sleep(Duration::from_millis(300)).await;
            while let Ok(signal) = signal_rx.try_recv() {
                signals.push(signal);
            }
        }
        cancel.cancel();
        let _ = runner.await;
        assert_eq!(
            signals[..5],
            [
                UnofficialSignal::Unreadable,
                UnofficialSignal::Healthy,
                UnofficialSignal::Neutral,
                UnofficialSignal::Unreadable,
                UnofficialSignal::Unreadable,
            ],
            "{signals:?}"
        );
    }

    #[tokio::test]
    async fn signal_channel_keeps_more_than_64_unread_signals_in_order() {
        // 受け手が読まないまま 64 件を超えて溜めても、落とさず順序も保つ(有界 64 では通らない)。
        let (signal_tx, mut signal_rx) = mpsc::unbounded_channel();
        let source = TwitcastingSource::new("alice".to_string(), TwitcastingOverrides::default(), None)
            .with_signal_tx(signal_tx);
        let mut sent = Vec::new();
        for i in 0..100 {
            let signal = match i % 3 {
                0 => UnofficialSignal::Unreadable,
                1 => UnofficialSignal::Neutral,
                _ => UnofficialSignal::Unreadable,
            };
            source.send_signal(signal);
            sent.push(signal);
        }
        source.send_signal(UnofficialSignal::Healthy);
        sent.push(UnofficialSignal::Healthy);
        let mut received = Vec::new();
        while let Ok(signal) = signal_rx.try_recv() {
            received.push(signal);
        }
        assert_eq!(received.len(), 101);
        assert_eq!(received.last(), Some(&UnofficialSignal::Healthy));
        assert_eq!(received, sent);
    }

    #[test]
    fn default_unofficial_tuning_matches_previous_constants() {
        let tuning = UnofficialTuning::default();
        assert_eq!(tuning.offline, OFFLINE_INTERVAL);
        assert_eq!(tuning.offline, Duration::from_secs(30));
        assert_eq!(tuning.unreadable_recheck, Duration::from_secs(30));
        assert_eq!(tuning.not_found_retry, NOT_FOUND_RETRY);
        assert_eq!(tuning.not_found_retry, Duration::from_secs(300));
        assert!(tuning.jitter);
        // 既定の待ち時間で with_tuning 無しと同じ遅延になる。
        let mut backoff = Backoff::new();
        let mut seed = 7;
        assert_eq!(
            outcome_delay_for(SessionOutcome::Unreadable, &tuning, &mut backoff, &mut seed),
            Some(Duration::from_secs(30))
        );
        assert_eq!(
            outcome_delay_for(SessionOutcome::NotFound, &tuning, &mut backoff, &mut seed),
            Some(Duration::from_secs(300))
        );
        let (mut a, mut b) = (11, 11);
        assert_eq!(
            outcome_delay_for(SessionOutcome::Offline, &tuning, &mut backoff, &mut a),
            outcome_delay(SessionOutcome::Offline, &mut Backoff::new(), &mut b)
        );
    }

    // ---- PR3' B2: 解釈できない識別子を生で出さない ----

    #[test]
    fn invalid_identifier_channel_name_is_fixed_label() {
        assert_eq!(channel_name("https://invalid.example/?token=MARKER"), INVALID_CHANNEL_LABEL);
        assert_eq!(channel_name("x:bad"), INVALID_CHANNEL_LABEL);
        assert_eq!(channel_name("https://twitcasting.tv/Alice"), "alice");
    }

    #[tokio::test]
    async fn invalid_identifier_warn_does_not_include_identifier() {
        use tracing::instrument::WithSubscriber;

        let (metadata_tx, mut metadata_rx) = mpsc::channel(8);
        let source = TwitcastingSource::new(
            "https://invalid.example/?token=MARKER".to_string(),
            TwitcastingOverrides::default(),
            Some(metadata_tx),
        );
        let debug = format!("{source:?}");
        let name = source.name();
        let sink = crate::log_filter_tests::Sink::default();
        let (tx, _rx) = broadcast::channel(8);
        let cancel = CancellationToken::new();
        let runner = {
            let cancel = cancel.clone();
            let subscriber = crate::log_subscriber(Some("trace"), sink.clone());
            tokio::spawn(async move { source.run(tx, cancel).with_subscriber(subscriber).await })
        };
        let update = tokio::time::timeout(Duration::from_secs(5), metadata_rx.recv())
            .await
            .expect("notice が届く")
            .expect("channel open");
        cancel.cancel();
        let _ = runner.await;

        assert_eq!(update.notice.as_deref(), Some(INVALID_ID_NOTICE));
        assert_eq!(update.channel, INVALID_CHANNEL_LABEL);
        let output = sink.output();
        assert!(output.contains(INVALID_ID_NOTICE), "{output}");
        for text in [output, format!("{update:?}"), debug, name] {
            assert!(!text.contains("MARKER"), "{text}");
            assert!(!text.contains("invalid.example"), "{text}");
        }
    }
}
