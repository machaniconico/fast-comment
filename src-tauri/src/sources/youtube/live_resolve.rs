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
use crate::model::{ChatMessage, Platform};
use crate::sources::SourceManager;
use crate::stats::YoutubeMetadataUpdate;

use super::{extract_video_id, is_video_id};

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const POLL_INTERVAL: Duration = Duration::from_secs(20);

const KEY_PLAYER_RESPONSE_MARKERS: &str = "liveResolvePlayerResponseMarkers";
const KEY_INITIAL_DATA_MARKERS: &str = "liveResolveInitialDataMarkers";
const KEY_VIDEO_ID_PATHS: &str = "liveResolveVideoIdPaths";
const KEY_VIDEO_ID_MARKER: &str = "liveResolveVideoIdMarker";

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

pub fn spawn_live_resolve_poller(
    identifier: String,
    overrides: YoutubeOverrides,
    source_tx: broadcast::Sender<ChatMessage>,
    metadata_tx: mpsc::Sender<YoutubeMetadataUpdate>,
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

        let manager = SourceManager::new(source_tx, overrides.clone());
        let mut active_video_id: Option<String> = None;
        let mut active_cancel: Option<CancellationToken> = None;
        let mut live_state: Option<bool> = None;

        loop {
            if cancel.is_cancelled() {
                break;
            }

            // resolve の HTTP await も親 cancel と race させ、停止要求が来たら
            // 進行中のフェッチを待たず即座にループを抜ける(子接続の停止遅延を防ぐ)。
            let resolved = tokio::select! {
                _ = cancel.cancelled() => break,
                res = resolve_live_video_id_with_client(&http, &identifier, &overrides) => res,
            };
            match resolved {
                Ok(Some(video_id)) if is_video_id(&video_id) => {
                    if active_video_id.as_deref() != Some(video_id.as_str()) {
                        if let Some(child) = active_cancel.take() {
                            child.cancel();
                        }

                        let child_config = ChannelConfig {
                            platform: ChannelPlatform::Youtube,
                            identifier: video_id.clone(),
                            enabled: true,
                        };
                        let child = manager.spawn_channel(&child_config);
                        super::metadata::spawn_metadata_poller(
                            video_id.clone(),
                            identifier.clone(),
                            overrides.clone(),
                            metadata_tx.clone(),
                            child.clone(),
                        );
                        tracing::info!(
                            "youtube:{identifier} live videoId 解決: {video_id} を接続"
                        );
                        active_video_id = Some(video_id);
                        active_cancel = Some(child);
                    }
                    if live_state != Some(true) {
                        if !send_live_status(&metadata_tx, &identifier, true, &cancel).await {
                            break;
                        }
                        live_state = Some(true);
                    }
                }
                Ok(Some(video_id)) => {
                    tracing::debug!(
                        "youtube:{identifier} live resolve が不正な videoId を返した: {video_id}"
                    );
                }
                Ok(None) => {
                    if let Some(child) = active_cancel.take() {
                        tracing::info!(
                            "youtube:{identifier} live videoId 未解決につき子接続を停止"
                        );
                        child.cancel();
                    }
                    active_video_id = None;
                    if live_state != Some(false) {
                        if !send_live_status(&metadata_tx, &identifier, false, &cancel).await {
                            break;
                        }
                        live_state = Some(false);
                    }
                }
                Err(e) => {
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
        likes: None,
        title: None,
        live: Some(live),
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

    Ok(extract_live_video_id_from_html(&html, &overrides.paths))
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
    for marker in split_lines(paths, KEY_INITIAL_DATA_MARKERS, DEFAULT_INITIAL_DATA_MARKERS) {
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
        let key_lower = key.to_lowercase();
        matches!(key_lower.as_str(), "islive" | "islivenow") && value.as_bool().unwrap_or(false)
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
    let lower = html
        .to_lowercase()
        .replace("\\\"", "\"")
        .replace("&quot;", "\"");
    let compact: String = lower.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    // 「現在配信中」を表すフラグのみ。`isLiveContent`(配信タイプ=終了後も true) や
    // `liveBroadcastDetails`(終了配信にも存在) は live-now の証拠にならないので採用しない。
    compact.contains("\"islive\":true")
        || compact.contains("\"islivenow\":true")
        || compact.contains("\"isnowlive\":true")
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
    let lower = tag.to_lowercase();
    let attr_pos = lower.find(attr)?;
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
    let lower = input.to_lowercase();
    let marker = "youtube.com/";
    let idx = lower.find(marker)?;
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
}
