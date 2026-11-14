//! YouTube チャンネル指定(@handle / channelId)から現在ライブ中の videoId を解決する。
//!
//! watch ページ抽出と同じく、固い deserialize は避け `serde_json::Value` と
//! 文字列マーカー/パス探索で欠落に強くする。

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::config::{ChannelConfig, ChannelPlatform, YoutubeOverrides};
use crate::model::{
    ChatMessage, LiveEvent, LiveStream, ModerationEvent, Platform, YoutubeReaction,
};
use crate::sources::SourceManager;
use crate::stats::{ViewerCountKind, YoutubeMetadataUpdate};

use super::{extract_video_id, is_video_id};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const POLL_INTERVAL: Duration = Duration::from_secs(20);
const LIVE_RESOLVE_MISS_LIMIT: u8 = 3;

const KEY_PLAYER_RESPONSE_MARKERS: &str = "liveResolvePlayerResponseMarkers";
const KEY_INITIAL_DATA_MARKERS: &str = "liveResolveInitialDataMarkers";
const KEY_VIDEO_ID_PATHS: &str = "liveResolveVideoIdPaths";
const KEY_VIDEO_ID_MARKER: &str = "liveResolveVideoIdMarker";
const KEY_START_TIME_PATHS: &str = "liveResolveStartTimestampPaths";

const DEFAULT_PLAYER_RESPONSE_MARKERS: &[&str] = &[
    "var ytInitialPlayerResponse =",
    "ytInitialPlayerResponse =",
    "\"ytInitialPlayerResponse\":",
];
const DEFAULT_INITIAL_DATA_MARKERS: &[&str] = &[
    "var ytInitialData =",
    "ytInitialData =",
    "\"ytInitialData\":",
];
const DEFAULT_VIDEO_ID_PATHS: &[&str] = &["videoDetails>videoId"];
const DEFAULT_VIDEO_ID_MARKER: &str = "\"videoId\":\"";
/// 配信開始時刻(ISO 8601)のパス。実ページ未確認の推測。
/// `youtubeOverrides.paths` の `liveResolveStartTimestampPaths`(改行区切り)で上書きできる。
const DEFAULT_START_TIME_PATHS: &[&str] =
    &["microformat>playerMicroformatRenderer>liveBroadcastDetails>startTimestamp"];

/// 現在ライブ中の動画 1 件分の情報(X 自動告知用)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveVideoInfo {
    pub video_id: String,
    pub title: Option<String>,
    /// 配信開始時刻(epoch ms)。取れなければ None。
    pub started_at_ms: Option<u64>,
    /// 対象 videoId とライブ確定シグナルが同じ動画オブジェクトに属すると確認できた時だけ true。
    /// 告知(Started)はこれが true の時だけ送る。チャット接続の判定には使わない。
    pub confirmed_live: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelIdentifier {
    Handle(String),
    ChannelId(String),
}

impl ChannelIdentifier {
    fn live_url(&self) -> String {
        match self {
            ChannelIdentifier::Handle(handle) => {
                format!("https://www.youtube.com/@{}/live", trim_handle_at(handle))
            }
            ChannelIdentifier::ChannelId(channel_id) => {
                format!("https://www.youtube.com/channel/{channel_id}/live")
            }
        }
    }
}

/// `ChannelConfig.identifier` をチャンネル指定として解釈できる場合だけ返す。
pub fn parse_channel_identifier(input: &str) -> Option<ChannelIdentifier> {
    let s = input.trim();
    if s.is_empty() || is_video_id(s) {
        return None;
    }

    if let Some(handle) = s.strip_prefix('@').and_then(normalize_handle) {
        return Some(ChannelIdentifier::Handle(handle));
    }

    if is_channel_id(s) {
        return Some(ChannelIdentifier::ChannelId(s.to_string()));
    }

    let path = youtube_url_path(s)?;
    let mut segments = path
        .split('/')
        .map(str::trim)
        .filter(|segment| !segment.is_empty());
    let first = segments.next()?;

    if let Some(handle) = first.strip_prefix('@').and_then(normalize_handle) {
        return Some(ChannelIdentifier::Handle(handle));
    }
    if first.eq_ignore_ascii_case("channel") {
        let channel_id = segments.next()?;
        if is_channel_id(channel_id) {
            return Some(ChannelIdentifier::ChannelId(channel_id.to_string()));
        }
    }

    None
}

#[allow(clippy::too_many_arguments)]
pub fn spawn_live_resolve_poller(
    identifier: String,
    overrides: YoutubeOverrides,
    official_api_key: String,
    source_tx: broadcast::Sender<ChatMessage>,
    metadata_tx: mpsc::Sender<YoutubeMetadataUpdate>,
    reaction_tx: mpsc::Sender<Vec<YoutubeReaction>>,
    mod_tx: mpsc::Sender<ModerationEvent>,
    live_tx: Option<mpsc::Sender<LiveEvent>>,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        let http = match Client::builder()
            .user_agent(USER_AGENT)
            .gzip(true)
            .timeout(Duration::from_secs(15))
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("youtube:{identifier} live resolve HTTP client 初期化失敗: {e}");
                return;
            }
        };

        let manager = SourceManager::new(
            source_tx,
            overrides.clone(),
            official_api_key,
            // ここは YouTube 専用の spawn 経路なので X/niconico overrides は使われない。
            crate::config::XOverrides::default(),
            None,
            crate::config::NiconicoOverrides::default(),
            Some(metadata_tx.clone()),
            Some(reaction_tx),
            Some(mod_tx),
        );
        let mut active_video_id: Option<String> = None;
        let mut active_cancel: Option<CancellationToken> = None;
        let mut live_state: Option<bool> = None;
        let mut started_video_id: Option<String> = None;
        let mut consecutive_unresolved: u8 = 0;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            // resolve の HTTP await も親 cancel と race させ、停止要求が来たら
            // 進行中のフェッチを待たず即座にループを抜ける(子接続の停止遅延を防ぐ)。
            let resolved = tokio::select! {
                _ = cancel.cancelled() => break,
                res = resolve_live_info_with_client(&http, &identifier, &overrides) => res,
            };
            match resolved {
                Ok(Some(info)) if is_video_id(&info.video_id) => {
                    let video_id = info.video_id.clone();
                    consecutive_unresolved = 0;
                    if active_video_id.as_deref() != Some(video_id.as_str()) {
                        if let Some(child) = active_cancel.take() {
                            child.cancel();
                        }

                        let child_config = ChannelConfig {
                            platform: ChannelPlatform::Youtube,
                            identifier: video_id.clone(),
                            enabled: true,
                            own_stream: false,
                        };
                        let child = manager.spawn_channel(&child_config);
                        super::metadata::spawn_metadata_poller(
                            video_id.clone(),
                            identifier.clone(),
                            overrides.clone(),
                            metadata_tx.clone(),
                            child.clone(),
                        );
                        tracing::info!("youtube:{identifier} live videoId 解決: {video_id} を接続");
                        active_video_id = Some(video_id.clone());
                        active_cancel = Some(child);
                    }
                    if info.confirmed_live
                        && started_video_id.as_deref() != Some(video_id.as_str())
                    {
                        if let Some(live_tx) = &live_tx {
                            send_live_started(live_tx, &identifier, &info);
                        }
                        started_video_id = Some(video_id.clone());
                    }
                    if live_state != Some(true) {
                        if !send_live_status(&metadata_tx, &identifier, true, &cancel).await {
                            break;
                        }
                        live_state = Some(true);
                    }
                }
                Ok(Some(info)) => {
                    consecutive_unresolved = 0;
                    tracing::debug!(
                        "youtube:{identifier} live resolve が不正な videoId を返した: {}",
                        info.video_id
                    );
                }
                Ok(None) => {
                    consecutive_unresolved = consecutive_unresolved.saturating_add(1);
                    let has_active_child = active_cancel.is_some();
                    if live_resolve_none_confirms_offline(has_active_child, consecutive_unresolved)
                    {
                        if let Some(child) = active_cancel.take() {
                            tracing::info!(
                                "youtube:{identifier} live videoId 未解決が続いたため子接続を停止 ({consecutive_unresolved}回)"
                            );
                            child.cancel();
                        }
                        active_video_id = None;
                        started_video_id = None;
                        if live_state != Some(false) {
                            if !send_live_status(&metadata_tx, &identifier, false, &cancel).await {
                                break;
                            }
                            live_state = Some(false);
                            if let Some(live_tx) = &live_tx {
                                crate::sources::send_live_event(
                                    live_tx,
                                    LiveEvent::Ended {
                                        platform: Platform::Youtube,
                                        channel_identifier: identifier.clone(),
                                    },
                                );
                            }
                        }
                    } else {
                        tracing::debug!(
                            "youtube:{identifier} live videoId 未解決 ({consecutive_unresolved}/{LIVE_RESOLVE_MISS_LIMIT})。active={:?} は継続",
                            active_video_id
                        );
                    }
                }
                Err(e) => {
                    consecutive_unresolved = 0;
                    tracing::debug!("youtube:{identifier} live resolve 取得失敗: {e:#}");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }

        if let Some(child) = active_cancel {
            child.cancel();
        }
        tracing::info!("youtube:{identifier} live resolve poller 終了");
    });
}

fn send_live_started(tx: &mpsc::Sender<LiveEvent>, identifier: &str, info: &LiveVideoInfo) {
    crate::sources::send_live_event(
        tx,
        LiveEvent::Started(LiveStream {
            platform: Platform::Youtube,
            channel_identifier: identifier.to_string(),
            stream_id: info.video_id.clone(),
            title: info.title.clone(),
            url: format!("https://www.youtube.com/watch?v={}", info.video_id),
            started_at_ms: info.started_at_ms,
        }),
    );
}

fn live_resolve_none_confirms_offline(has_active_child: bool, consecutive_misses: u8) -> bool {
    !has_active_child || consecutive_misses >= LIVE_RESOLVE_MISS_LIMIT
}

async fn send_live_status(
    tx: &mpsc::Sender<YoutubeMetadataUpdate>,
    identifier: &str,
    live: bool,
    cancel: &CancellationToken,
) -> bool {
    let update = YoutubeMetadataUpdate {
        platform: Platform::Youtube,
        channel: identifier.to_string(),
        concurrent_viewers: None,
        viewers_kind: ViewerCountKind::Concurrent,
        likes: None,
        title: None,
        live: Some(live),
        notice: None,
        reactions_delta: None,
        full_snapshot: false,
    };
    tokio::select! {
        _ = cancel.cancelled() => false,
        sent = tx.send(update) => sent.is_ok(),
    }
}

pub async fn resolve_live_video_id(
    identifier: &str,
    overrides: &YoutubeOverrides,
) -> anyhow::Result<Option<String>> {
    let http = Client::builder()
        .user_agent(USER_AGENT)
        .gzip(true)
        .build()?;
    resolve_live_video_id_with_client(&http, identifier, overrides).await
}

pub async fn resolve_live_video_id_with_client(
    http: &Client,
    identifier: &str,
    overrides: &YoutubeOverrides,
) -> anyhow::Result<Option<String>> {
    Ok(resolve_live_info_with_client(http, identifier, overrides)
        .await?
        .map(|info| info.video_id))
}

pub async fn resolve_live_info_with_client(
    http: &Client,
    identifier: &str,
    overrides: &YoutubeOverrides,
) -> anyhow::Result<Option<LiveVideoInfo>> {
    let target = match parse_channel_identifier(identifier) {
        Some(target) => target,
        None => return Ok(None),
    };
    let html = http
        .get(target.live_url())
        .header("Accept-Language", "ja,en;q=0.8")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    Ok(extract_live_info_from_html(&html, &overrides.paths))
}

/// ライブ中の videoId に加え、同じ HTML からタイトルと開始時刻を取る。
/// `isLive` 等の確定シグナルが無ければ(待機枠を含め)None。
pub fn extract_live_info_from_html(
    html: &str,
    paths: &HashMap<String, String>,
) -> Option<LiveVideoInfo> {
    let video_id = extract_live_video_id_from_html(html, paths)?;
    let title = super::metadata::extract_title_from_html(html, paths);
    let started_at_ms = extract_start_time_ms(html, paths);
    let confirmed_live = confirm_live_same_video(html, paths, &video_id);
    Some(LiveVideoInfo {
        video_id,
        title,
        started_at_ms,
        confirmed_live,
    })
}

/// 告知用の厳密判定。同じ player response 内で、対象 videoId を持つ動画オブジェクトが
/// 自身で配信中(isLive / isLiveNow)を示し、かつ待機枠(isUpcoming)でない時だけ true。
/// microformat の liveBroadcastDetails.isLiveNow も、同じ player response が対象 videoId の
/// ものである場合に限り採用する。関連付けが不明(別動画の isLive・HTML 近傍文字列等)は false。
fn confirm_live_same_video(
    html: &str,
    paths: &HashMap<String, String>,
    video_id: &str,
) -> bool {
    let id_paths = split_lines(paths, KEY_VIDEO_ID_PATHS, DEFAULT_VIDEO_ID_PATHS);
    for marker in split_lines(
        paths,
        KEY_PLAYER_RESPONSE_MARKERS,
        DEFAULT_PLAYER_RESPONSE_MARKERS,
    ) {
        let Some(root) = extract_json_value_after_marker(html, marker) else {
            continue;
        };
        for path in &id_paths {
            if dig_path(&root, path).and_then(Value::as_str) != Some(video_id) {
                continue;
            }
            let parent_path = path.rsplit_once('>').map_or("", |(parent, _)| parent);
            let Some(parent) = dig_path(&root, parent_path).and_then(Value::as_object) else {
                continue;
            };
            let upcoming = parent
                .get("isUpcoming")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if upcoming {
                continue;
            }
            let broadcast_live = root
                .get("microformat")
                .and_then(|v| v.get("playerMicroformatRenderer"))
                .and_then(|v| v.get("liveBroadcastDetails"))
                .and_then(Value::as_object)
                .is_some_and(object_says_live);
            if object_says_live(parent) || broadcast_live {
                return true;
            }
        }
    }
    false
}

fn extract_start_time_ms(html: &str, paths: &HashMap<String, String>) -> Option<u64> {
    let start_paths = split_lines(paths, KEY_START_TIME_PATHS, DEFAULT_START_TIME_PATHS);
    for marker in split_lines(
        paths,
        KEY_PLAYER_RESPONSE_MARKERS,
        DEFAULT_PLAYER_RESPONSE_MARKERS,
    ) {
        let Some(root) = extract_json_value_after_marker(html, marker) else {
            continue;
        };
        for path in &start_paths {
            let ms = dig_path(&root, path)
                .and_then(Value::as_str)
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(s.trim()).ok())
                .and_then(|t| u64::try_from(t.timestamp_millis()).ok());
            if ms.is_some() {
                return ms;
            }
        }
    }
    None
}

pub fn extract_live_video_id_from_html(
    html: &str,
    paths: &HashMap<String, String>,
) -> Option<String> {
    extract_from_json_roots(html, paths)
        .or_else(|| extract_marker_video_id(html, paths))
        .or_else(|| extract_canonical_video_id(html))
        .or_else(|| extract_og_url_video_id(html))
}

fn extract_from_json_roots(html: &str, paths: &HashMap<String, String>) -> Option<String> {
    let mut roots = Vec::new();
    for marker in split_lines(
        paths,
        KEY_PLAYER_RESPONSE_MARKERS,
        DEFAULT_PLAYER_RESPONSE_MARKERS,
    ) {
        if let Some(value) = extract_json_value_after_marker(html, marker) {
            roots.push(value);
        }
    }
    for marker in split_lines(
        paths,
        KEY_INITIAL_DATA_MARKERS,
        DEFAULT_INITIAL_DATA_MARKERS,
    ) {
        if let Some(value) = extract_json_value_after_marker(html, marker) {
            roots.push(value);
        }
    }

    extract_string_by_paths(
        &roots,
        &split_lines(paths, KEY_VIDEO_ID_PATHS, DEFAULT_VIDEO_ID_PATHS),
    )
    .or_else(|| roots.iter().find_map(find_live_video_id))
}

fn extract_canonical_video_id(html: &str) -> Option<String> {
    if !html_has_live_hint(html) {
        return None;
    }
    extract_link_href(html, "rel=\"canonical\"")
        .or_else(|| extract_link_href(html, "rel='canonical'"))
        .and_then(|url| valid_video_id_from_url(&url))
}

fn extract_og_url_video_id(html: &str) -> Option<String> {
    if !html_has_live_hint(html) {
        return None;
    }
    extract_meta_content(html, "property=\"og:url\"")
        .or_else(|| extract_meta_content(html, "property='og:url'"))
        .or_else(|| extract_meta_content(html, "name=\"og:url\""))
        .or_else(|| extract_meta_content(html, "name='og:url'"))
        .and_then(|url| valid_video_id_from_url(&url))
}

fn valid_video_id_from_url(url: &str) -> Option<String> {
    let video_id = extract_video_id(url);
    if is_video_id(&video_id) {
        Some(video_id)
    } else {
        None
    }
}

fn extract_string_by_paths(roots: &[Value], paths: &[&str]) -> Option<String> {
    for root in roots {
        if !value_has_live_hint(root) {
            continue;
        }
        for path in paths {
            if let Some(video_id) = dig_path(root, path)
                .and_then(|value| value.as_str())
                .filter(|s| is_video_id(s))
            {
                return Some(video_id.to_string());
            }
        }
    }
    None
}

fn dig_path<'a>(value: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = value;
    for raw in path.split('>').map(str::trim).filter(|s| !s.is_empty()) {
        if let Ok(index) = raw.parse::<usize>() {
            cur = cur.get(index)?;
        } else {
            cur = cur.get(raw)?;
        }
    }
    Some(cur)
}

fn find_live_video_id(value: &Value) -> Option<String> {
    match value {
        Value::Object(map) => {
            if object_says_live(map) {
                if let Some(video_id) = map.get("videoId").and_then(|v| v.as_str()) {
                    if is_video_id(video_id) {
                        return Some(video_id.to_string());
                    }
                }
                if let Some(video_id) = map
                    .get("watchEndpoint")
                    .and_then(|v| v.get("videoId"))
                    .and_then(|v| v.as_str())
                {
                    if is_video_id(video_id) {
                        return Some(video_id.to_string());
                    }
                }
            }
            map.values().find_map(find_live_video_id)
        }
        Value::Array(values) => values.iter().find_map(find_live_video_id),
        _ => None,
    }
}

fn object_says_live(map: &serde_json::Map<String, Value>) -> bool {
    map.iter().any(|(key, value)| {
        // 「現在配信中」の確定シグナルのみを採用する。
        // `isLiveContent` は“ライブ配信タイプの動画”を表し配信終了後も true のため除外
        // (終了済み/アーカイブへの誤接続を防ぐ)。live-now を表す `isLive` / `isLiveNow` のみ。
        (key.eq_ignore_ascii_case("isLive") || key.eq_ignore_ascii_case("isLiveNow"))
            && value.as_bool().unwrap_or(false)
    })
}

fn value_has_live_hint(value: &Value) -> bool {
    match value {
        Value::Object(map) => object_says_live(map) || map.values().any(value_has_live_hint),
        Value::Array(values) => values.iter().any(value_has_live_hint),
        _ => false,
    }
}

fn extract_marker_video_id(html: &str, paths: &HashMap<String, String>) -> Option<String> {
    let marker = path_or(paths, KEY_VIDEO_ID_MARKER, DEFAULT_VIDEO_ID_MARKER);
    let mut offset = 0usize;
    while let Some(relative) = html[offset..].find(marker) {
        let start = offset + relative + marker.len();
        let tail = &html[start..];
        let end = tail.find('"')?;
        let video_id = &tail[..end];
        if is_video_id(video_id) && nearby_live_hint(html, start) {
            return Some(video_id.to_string());
        }
        offset = start + end;
    }
    None
}

fn nearby_live_hint(html: &str, center: usize) -> bool {
    let start = previous_char_boundary(html, center.saturating_sub(512));
    let end = next_char_boundary(html, (center + 512).min(html.len()));
    html_has_live_hint(&html[start..end])
}

fn html_has_live_hint(html: &str) -> bool {
    // 「現在配信中」を表すフラグのみ。`isLiveContent`(配信タイプ=終了後も true) や
    // `liveBroadcastDetails`(終了配信にも存在) は live-now の証拠にならないので採用しない。
    [
        "\"isLive\":true",
        "\"isLiveNow\":true",
        "\"isNowLive\":true",
        "\\\"isLive\\\":true",
        "\\\"isLiveNow\\\":true",
        "\\\"isNowLive\\\":true",
        "&quot;isLive&quot;:true",
        "&quot;isLiveNow&quot;:true",
        "&quot;isNowLive&quot;:true",
    ]
    .iter()
    .any(|needle| contains_ascii_case_insensitive_ignoring_ws(html, needle))
}

fn previous_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx > 0 && !text.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn next_char_boundary(text: &str, mut idx: usize) -> usize {
    while idx < text.len() && !text.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

fn extract_json_value_after_marker(html: &str, marker: &str) -> Option<Value> {
    let marker_pos = html.find(marker)? + marker.len();
    let tail = &html[marker_pos..];
    let brace_pos = tail.find('{')?;
    let json_start = marker_pos + brace_pos;
    let json = balanced_json_object(&html[json_start..])?;
    serde_json::from_str(json).ok()
}

fn balanced_json_object(text: &str) -> Option<&str> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;

    for (idx, ch) in text.char_indices() {
        if in_string {
            if escaped {
                escaped = false;
                continue;
            }
            match ch {
                '\\' => escaped = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }

        match ch {
            '"' => in_string = true,
            '{' => depth = depth.saturating_add(1),
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(&text[..idx + ch.len_utf8()]);
                }
            }
            _ => {}
        }
    }

    None
}

fn extract_link_href(html: &str, needle: &str) -> Option<String> {
    let tag_start = html.find(needle)?;
    let tag_prefix = html[..tag_start].rfind('<')?;
    let tag_tail = &html[tag_prefix..];
    let tag_end = tag_tail.find('>')?;
    let tag = &tag_tail[..tag_end];
    extract_attr_value(tag, "href")
}

fn extract_meta_content(html: &str, needle: &str) -> Option<String> {
    let tag_start = html.find(needle)?;
    let tag_prefix = html[..tag_start].rfind('<')?;
    let tag_tail = &html[tag_prefix..];
    let tag_end = tag_tail.find('>')?;
    let tag = &tag_tail[..tag_end];
    extract_attr_value(tag, "content")
}

fn extract_attr_value(tag: &str, attr: &str) -> Option<String> {
    let attr_pos = find_ascii_case_insensitive(tag, attr)?;
    let after_attr = &tag[attr_pos + attr.len()..];
    let equals_pos = after_attr.find('=')?;
    let mut value = after_attr[equals_pos + 1..].trim_start();
    let quote = value.chars().next()?;
    if quote != '"' && quote != '\'' {
        return None;
    }
    value = &value[quote.len_utf8()..];
    let end = value.find(quote)?;
    Some(decode_html_entities(&value[..end]))
}

fn decode_html_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn contains_ascii_case_insensitive_ignoring_ws(haystack: &str, needle: &str) -> bool {
    let needle = needle.as_bytes();
    if needle.is_empty() {
        return true;
    }
    let mut matched = 0usize;
    for &byte in haystack.as_bytes() {
        if byte.is_ascii_whitespace() {
            continue;
        }
        if byte.eq_ignore_ascii_case(&needle[matched]) {
            matched += 1;
            if matched == needle.len() {
                return true;
            }
        } else {
            matched = usize::from(byte.eq_ignore_ascii_case(&needle[0]));
        }
    }
    false
}

fn path_or<'a>(paths: &'a HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
    paths
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}

fn split_lines<'a>(
    paths: &'a HashMap<String, String>,
    key: &str,
    default: &'static [&'static str],
) -> Vec<&'a str> {
    match paths.get(key).map(String::as_str).filter(|s| !s.is_empty()) {
        Some(s) => s
            .split('\n')
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .collect(),
        None => default.to_vec(),
    }
}

fn youtube_url_path(input: &str) -> Option<&str> {
    let marker = "youtube.com/";
    let idx = find_ascii_case_insensitive(input, marker)?;
    let tail = &input[idx + marker.len()..];
    let path = tail
        .split(['?', '#'])
        .next()
        .unwrap_or("")
        .trim_matches('/');
    if path.is_empty() {
        None
    } else {
        Some(path)
    }
}

fn normalize_handle(value: &str) -> Option<String> {
    let handle = value
        .split(['/', '?', '#'])
        .next()
        .unwrap_or("")
        .trim()
        .trim_start_matches('@');
    if handle.is_empty() {
        None
    } else {
        Some(handle.to_string())
    }
}

fn trim_handle_at(value: &str) -> &str {
    value.trim().trim_start_matches('@')
}

fn is_channel_id(value: &str) -> bool {
    let s = value.trim();
    s.len() == 24
        && s.starts_with("UC")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_paths() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn parses_channel_identifiers() {
        assert_eq!(
            parse_channel_identifier("@example"),
            Some(ChannelIdentifier::Handle("example".to_string()))
        );
        assert_eq!(
            parse_channel_identifier("youtube.com/@example/live"),
            Some(ChannelIdentifier::Handle("example".to_string()))
        );
        assert_eq!(
            parse_channel_identifier(
                "https://www.youtube.com/channel/UC1234567890123456789012/live"
            ),
            Some(ChannelIdentifier::ChannelId(
                "UC1234567890123456789012".to_string()
            ))
        );
        assert_eq!(parse_channel_identifier("dQw4w9WgXcQ"), None);
        assert_eq!(
            parse_channel_identifier("https://www.youtube.com/watch?v=dQw4w9WgXcQ"),
            None
        );
    }

    const LIVE_PAGE: &str = r#"<html><head><title>テスト配信 - YouTube</title></head><body><script>
        var ytInitialPlayerResponse = {
            "videoDetails": {"videoId": "dQw4w9WgXcQ", "title": "夜のゲーム配信", "isLive": true},
            "microformat": {"playerMicroformatRenderer": {"liveBroadcastDetails": {
                "isLiveNow": true, "startTimestamp": "2026-11-14T10:00:00+09:00"}}}
        };
    </script></body></html>"#;

    /// AC-3
    #[test]
    fn live_info_has_video_id_title_and_start_time() {
        let info = extract_live_info_from_html(LIVE_PAGE, &empty_paths()).expect("live");
        assert_eq!(info.video_id, "dQw4w9WgXcQ");
        assert_eq!(info.title.as_deref(), Some("夜のゲーム配信"));
        // 2026-11-14T01:00:00Z
        assert_eq!(info.started_at_ms, Some(1_794_618_000_000));
    }

    #[test]
    fn live_info_start_time_is_none_when_path_missing_and_overridable() {
        let html = r#"<script>var ytInitialPlayerResponse = {
            "videoDetails": {"videoId": "dQw4w9WgXcQ", "title": "t", "isLive": true},
            "custom": {"begin": "2026-11-14T00:00:00Z"}};</script>"#;
        let info = extract_live_info_from_html(html, &empty_paths()).expect("live");
        assert_eq!(info.started_at_ms, None);
        let mut paths = HashMap::new();
        paths.insert(KEY_START_TIME_PATHS.to_string(), "custom>begin".to_string());
        let info = extract_live_info_from_html(html, &paths).expect("live");
        assert_eq!(info.started_at_ms, Some(1_794_614_400_000));
    }

    #[test]
    fn live_info_is_none_without_is_live_even_with_start_time() {
        // 待機枠(isUpcoming)・終了済み(isLiveContent のみ)は配信中ではない。
        let html = r#"<script>var ytInitialPlayerResponse = {
            "videoDetails": {"videoId": "dQw4w9WgXcQ", "title": "t", "isUpcoming": true, "isLiveContent": true},
            "microformat": {"playerMicroformatRenderer": {"liveBroadcastDetails": {
                "startTimestamp": "2026-11-14T10:00:00+09:00"}}}};</script>"#;
        assert_eq!(extract_live_info_from_html(html, &empty_paths()), None);
        assert_eq!(extract_live_info_from_html("<html></html>", &empty_paths()), None);
    }

    #[test]
    fn live_info_is_not_confirmed_for_upcoming_target_with_other_video_live() {
        // 待機枠(対象)+ 同じ JSON 内の別動画に isLive:true。広い判定では videoId が取れるが、
        // 告知用の confirmed_live は false(Started を送らない)。
        let html = r#"<script>var ytInitialPlayerResponse = {
            "videoDetails": {"videoId": "dQw4w9WgXcQ", "title": "t", "isUpcoming": true},
            "related": [{"videoId": "aaaaaaaaaaa", "isLive": true}]};</script>"#;
        if let Some(info) = extract_live_info_from_html(html, &empty_paths()) {
            assert!(!info.confirmed_live);
        }
        // 近傍文字列フォールバックも未確定。
        let html = r#"{"isLive":true,"videoId":"dQw4w9WgXcQ"}"#;
        let info = extract_live_info_from_html(html, &empty_paths()).expect("wide live");
        assert!(!info.confirmed_live);
        // 対象動画自身の isLive は確定。
        let info = extract_live_info_from_html(LIVE_PAGE, &empty_paths()).expect("live");
        assert!(info.confirmed_live);
    }

    #[test]
    fn extracts_live_video_id_from_canonical_and_og_url() {
        let html = r#"
            <link rel="canonical" href="https://www.youtube.com/watch?v=dQw4w9WgXcQ">
            <meta property="og:url" content="https://www.youtube.com/watch?v=aaaaaaaaaaa">
            <script>{\"isLive\":true}</script>
        "#;
        assert_eq!(
            extract_live_video_id_from_html(html, &empty_paths()),
            Some("dQw4w9WgXcQ".to_string())
        );

        let html = r#"<meta property="og:url" content="https://youtu.be=dQw4w9WgXcQ">"#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);
    }

    #[test]
    fn ignores_canonical_and_og_url_without_live_hint() {
        let html = r#"
            <link rel="canonical" href="https://www.youtube.com/watch?v=dQw4w9WgXcQ">
            <meta property="og:url" content="https://www.youtube.com/watch?v=aaaaaaaaaaa">
        "#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);

        let html = r#"
            <link rel="canonical" href="https://www.youtube.com/watch?v=dQw4w9WgXcQ">
            <script>{"isLiveContent":false}</script>
        "#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);
    }

    #[test]
    fn extracts_live_video_id_from_json_paths() {
        let html = r#"
            var ytInitialPlayerResponse = {
                "videoDetails": {
                    "videoId": "dQw4w9WgXcQ",
                    "isLive": true
                }
            };
        "#;
        assert_eq!(
            extract_live_video_id_from_html(html, &empty_paths()),
            Some("dQw4w9WgXcQ".to_string())
        );
    }

    #[test]
    fn missing_live_video_id_returns_none() {
        let html = r#"
            var ytInitialData = {
                "contents": { "videoRenderer": { "videoId": "dQw4w9WgXcQ" } }
            };
        "#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);

        let html = r#"
            var ytInitialPlayerResponse = {
                "videoDetails": {
                    "videoId": "dQw4w9WgXcQ",
                    "isLiveContent": false
                }
            };
        "#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);
    }

    #[test]
    fn extracts_marker_video_id_only_with_live_hint() {
        let html = r#"{"isLive":true,"videoId":"dQw4w9WgXcQ"}"#;
        assert_eq!(
            extract_live_video_id_from_html(html, &empty_paths()),
            Some("dQw4w9WgXcQ".to_string())
        );

        let html = r#"{"videoId":"dQw4w9WgXcQ"}"#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);

        // isLiveContent は配信タイプ(終了後も true)であり live-now ではないので接続しない。
        let html = r#"{"isLiveContent":true,"videoId":"dQw4w9WgXcQ"}"#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);

        let html = r#"{"isLiveContent":false,"videoId":"dQw4w9WgXcQ"}"#;
        assert_eq!(extract_live_video_id_from_html(html, &empty_paths()), None);
    }

    #[test]
    fn unresolved_live_resolve_is_offline_when_no_active_child() {
        assert!(live_resolve_none_confirms_offline(false, 1));
    }

    #[test]
    fn unresolved_live_resolve_needs_consecutive_misses_with_active_child() {
        assert!(!live_resolve_none_confirms_offline(true, 1));
        assert!(!live_resolve_none_confirms_offline(true, 2));
        assert!(live_resolve_none_confirms_offline(true, 3));
    }
}
