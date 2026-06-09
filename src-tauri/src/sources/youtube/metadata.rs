//! YouTube watch ページから Goals 用メタデータを取得する poller。
//!
//! InnerTube のチャット取得と同じく、固い struct deserialize は避け、
//! `serde_json::Value` と文字列マーカーで寛容に抽出する。

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::YoutubeOverrides;
use crate::model::Platform;
use crate::stats::YoutubeMetadataUpdate;

use super::extract_video_id;

const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const POLL_INTERVAL: Duration = Duration::from_secs(20);

const KEY_PLAYER_RESPONSE_MARKERS: &str = "metadataPlayerResponseMarkers";
const KEY_INITIAL_DATA_MARKERS: &str = "metadataInitialDataMarkers";
const KEY_CONCURRENT_PATHS: &str = "metadataConcurrentViewersPaths";
const KEY_LIKES_PATHS: &str = "metadataLikesPaths";
const KEY_TITLE_PATHS: &str = "metadataTitlePaths";
const KEY_CONCURRENT_MARKER: &str = "metadataConcurrentViewersMarker";
const KEY_LIKES_MARKER: &str = "metadataLikesMarker";

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
const DEFAULT_CONCURRENT_PATHS: &[&str] = &[
    "microformat>playerMicroformatRenderer>liveBroadcastDetails>concurrentViewers",
    "videoDetails>isLiveContent>concurrentViewers",
];
const DEFAULT_LIKES_PATHS: &[&str] = &[
    "contents>twoColumnWatchNextResults>results>results>contents>0>videoPrimaryInfoRenderer>videoActions>menuRenderer>topLevelButtons>0>segmentedLikeDislikeButtonRenderer>likeButton>toggleButtonRenderer>defaultText",
    "contents>twoColumnWatchNextResults>results>results>contents>0>videoPrimaryInfoRenderer>videoActions>menuRenderer>topLevelButtons>0>toggleButtonRenderer>defaultText",
];
const DEFAULT_TITLE_PATHS: &[&str] = &["videoDetails>title"];
const DEFAULT_CONCURRENT_MARKER: &str = "\"concurrentViewers\":\"";
const DEFAULT_LIKES_MARKER: &str = "\"likeCount\":\"";

#[derive(Debug, Clone, Default)]
struct MetadataValues {
    concurrent_viewers: Option<u32>,
    likes: Option<u32>,
    title: Option<String>,
}

pub fn spawn_metadata_poller(
    video_input: String,
    status_channel: String,
    overrides: YoutubeOverrides,
    tx: mpsc::Sender<YoutubeMetadataUpdate>,
    cancel: CancellationToken,
) {
    tauri::async_runtime::spawn(async move {
        let video_id = extract_video_id(&video_input);
        let http = match Client::builder()
            .user_agent(USER_AGENT)
            .gzip(true)
            .build()
        {
            Ok(client) => client,
            Err(e) => {
                tracing::warn!("youtube:{video_id} metadata HTTP client 初期化失敗: {e}");
                return;
            }
        };

        let mut last = MetadataValues::default();
        loop {
            if cancel.is_cancelled() {
                break;
            }

            match fetch_metadata(&http, &video_id, &overrides.paths).await {
                Ok(values) => {
                    if values.concurrent_viewers.is_some() {
                        last.concurrent_viewers = values.concurrent_viewers;
                    }
                    if values.likes.is_some() {
                        last.likes = values.likes;
                    }
                    if values.title.is_some() {
                        last.title = values.title;
                    }

                    let update = YoutubeMetadataUpdate {
                        platform: Platform::Youtube,
                        channel: status_channel.clone(),
                        concurrent_viewers: last.concurrent_viewers,
                        likes: last.likes,
                        title: last.title.clone(),
                        live: None,
                    };
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        sent = tx.send(update) => {
                            if sent.is_err() {
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("youtube:{video_id} metadata 取得失敗: {e:#}");
                }
            }

            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tokio::time::sleep(POLL_INTERVAL) => {}
            }
        }
        tracing::info!("youtube:{video_id} metadata poller 終了");
    });
}

async fn fetch_metadata(
    http: &Client,
    video_id: &str,
    paths: &HashMap<String, String>,
) -> anyhow::Result<MetadataValues> {
    let url = format!("https://www.youtube.com/watch?v={video_id}");
    let html = http
        .get(url)
        .header("Accept-Language", "ja,en;q=0.8")
        .send()
        .await?
        .error_for_status()?
        .text()
        .await?;

    Ok(extract_metadata_from_html(&html, paths))
}

fn extract_metadata_from_html(html: &str, paths: &HashMap<String, String>) -> MetadataValues {
    let mut roots = Vec::new();
    for marker in split_lines(paths, KEY_PLAYER_RESPONSE_MARKERS, DEFAULT_PLAYER_RESPONSE_MARKERS) {
        if let Some(value) = extract_json_value_after_marker(html, marker) {
            roots.push(value);
        }
    }
    for marker in split_lines(paths, KEY_INITIAL_DATA_MARKERS, DEFAULT_INITIAL_DATA_MARKERS) {
        if let Some(value) = extract_json_value_after_marker(html, marker) {
            roots.push(value);
        }
    }

    let concurrent = extract_by_paths(
        &roots,
        &split_lines(paths, KEY_CONCURRENT_PATHS, DEFAULT_CONCURRENT_PATHS),
    )
    .or_else(|| {
        extract_json_string_field(
            html,
            path_or(paths, KEY_CONCURRENT_MARKER, DEFAULT_CONCURRENT_MARKER),
        )
        .and_then(|s| parse_count_text(&s))
    })
    .or_else(|| roots.iter().find_map(find_concurrent_count));

    let likes = extract_by_paths(&roots, &split_lines(paths, KEY_LIKES_PATHS, DEFAULT_LIKES_PATHS))
        .or_else(|| {
            extract_json_string_field(html, path_or(paths, KEY_LIKES_MARKER, DEFAULT_LIKES_MARKER))
                .and_then(|s| parse_count_text(&s))
        })
        .or_else(|| roots.iter().find_map(|v| find_like_count(v, false)));
    let title = extract_string_by_paths(
        &roots,
        &split_lines(paths, KEY_TITLE_PATHS, DEFAULT_TITLE_PATHS),
    )
    .or_else(|| extract_og_title(html))
    .or_else(|| extract_html_title(html));

    MetadataValues {
        concurrent_viewers: concurrent,
        likes,
        title,
    }
}

fn extract_by_paths(roots: &[Value], paths: &[&str]) -> Option<u32> {
    for root in roots {
        for path in paths {
            if let Some(value) = dig_path(root, path).and_then(parse_count_value) {
                return Some(value);
            }
        }
    }
    None
}

fn extract_string_by_paths(roots: &[Value], paths: &[&str]) -> Option<String> {
    for root in roots {
        for path in paths {
            if let Some(value) = dig_path(root, path)
                .and_then(|value| value.as_str())
                .and_then(normalize_title)
            {
                return Some(value);
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

fn find_concurrent_count(value: &Value) -> Option<u32> {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                let key_lower = key.to_lowercase();
                if (key_lower.contains("concurrent")
                    || key_lower == "concurrentviewers"
                    || key_lower == "viewercount")
                    && parse_count_value(child).is_some()
                {
                    return parse_count_value(child);
                }
            }
            map.values().find_map(find_concurrent_count)
        }
        Value::Array(values) => values.iter().find_map(find_concurrent_count),
        _ => None,
    }
}

fn find_like_count(value: &Value, like_context: bool) -> Option<u32> {
    match value {
        Value::Object(map) => {
            let local_like_context = like_context
                || map.keys().any(|key| key.to_lowercase().contains("like"))
                || map.values().any(|v| {
                    v.as_str()
                        .map(|s| s.eq_ignore_ascii_case("LIKE"))
                        .unwrap_or(false)
                });

            for (key, child) in map {
                let key_lower = key.to_lowercase();
                if (key_lower.contains("likecount")
                    || (key_lower.contains("like") && key_lower.contains("count")))
                    && parse_count_value(child).is_some()
                {
                    return parse_count_value(child);
                }
                if local_like_context
                    && matches!(
                        key_lower.as_str(),
                        "title" | "accessibilitytext" | "label" | "simpletext" | "text"
                    )
                    && parse_count_value(child).is_some()
                {
                    return parse_count_value(child);
                }
            }

            map.values()
                .find_map(|child| find_like_count(child, local_like_context))
        }
        Value::Array(values) => values
            .iter()
            .find_map(|child| find_like_count(child, like_context)),
        _ => None,
    }
}

fn parse_count_value(value: &Value) -> Option<u32> {
    match value {
        Value::Number(n) => n.as_u64().and_then(to_u32),
        Value::String(s) => parse_count_text(s),
        Value::Object(map) => {
            if let Some(s) = map.get("simpleText").and_then(|v| v.as_str()) {
                return parse_count_text(s);
            }
            if let Some(values) = map.get("runs").and_then(|v| v.as_array()) {
                let joined = values
                    .iter()
                    .filter_map(|run| run.get("text").and_then(|v| v.as_str()))
                    .collect::<String>();
                return parse_count_text(&joined);
            }
            None
        }
        _ => None,
    }
}

fn parse_count_text(text: &str) -> Option<u32> {
    let compact = text.trim().replace(',', "");
    if compact.is_empty() {
        return None;
    }

    let mut start = None;
    let mut end = 0usize;
    for (idx, ch) in compact.char_indices() {
        let is_part = ch.is_ascii_digit() || ch == '.';
        if is_part && start.is_none() {
            start = Some(idx);
        }
        if start.is_some() {
            if is_part {
                end = idx + ch.len_utf8();
            } else {
                break;
            }
        }
    }

    let start = start?;
    let number = compact[start..end].parse::<f64>().ok()?;
    let suffix = compact[end..]
        .chars()
        .find(|ch| !ch.is_whitespace())
        .unwrap_or('\0');
    let lower = compact.to_lowercase();
    let multiplier = match suffix {
        '億' => 100_000_000.0,
        '万' => 10_000.0,
        'k' | 'K' => 1_000.0,
        'm' | 'M' => 1_000_000.0,
        'b' | 'B' => 1_000_000_000.0,
        _ if lower.contains("万") => 10_000.0,
        _ if lower.contains("億") => 100_000_000.0,
        _ => 1.0,
    };
    to_u32((number * multiplier).round() as u64)
}

fn to_u32(value: u64) -> Option<u32> {
    u32::try_from(value).ok()
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

fn extract_json_string_field(html: &str, marker: &str) -> Option<String> {
    let start = html.find(marker)? + marker.len();
    let tail = &html[start..];
    let end = tail.find('"')?;
    Some(tail[..end].to_string())
}

fn extract_og_title(html: &str) -> Option<String> {
    extract_meta_content(html, "property=\"og:title\"")
        .or_else(|| extract_meta_content(html, "property='og:title'"))
        .or_else(|| extract_meta_content(html, "name=\"og:title\""))
        .or_else(|| extract_meta_content(html, "name='og:title'"))
        .and_then(|s| normalize_title(&s))
}

fn extract_meta_content(html: &str, needle: &str) -> Option<String> {
    let tag_start = html.find(needle)?;
    let tag_prefix = html[..tag_start].rfind('<')?;
    let tag_tail = &html[tag_prefix..];
    let tag_end = tag_tail.find('>')?;
    let tag = &tag_tail[..tag_end];
    extract_attr_value(tag, "content")
}

fn extract_html_title(html: &str) -> Option<String> {
    let start = find_ascii_case_insensitive(html, "<title")?;
    let open_end = html[start..].find('>')? + start + 1;
    let close = find_ascii_case_insensitive(&html[open_end..], "</title>")? + open_end;
    let title = decode_html_entities(&html[open_end..close]);
    normalize_title(strip_youtube_suffix(&title))
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

fn strip_youtube_suffix(title: &str) -> &str {
    title
        .trim()
        .strip_suffix("- YouTube")
        .map(str::trim_end)
        .unwrap_or_else(|| title.trim())
}

fn normalize_title(title: &str) -> Option<String> {
    let title = title.trim();
    if title.is_empty() || title.eq_ignore_ascii_case("YouTube") {
        return None;
    }
    Some(title.to_string())
}

fn decode_html_entities(text: &str) -> String {
    text.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn empty_paths() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn parses_plain_and_compact_counts() {
        assert_eq!(parse_count_text("1,234"), Some(1234));
        assert_eq!(parse_count_text("1.2K likes"), Some(1200));
        assert_eq!(parse_count_text("2.5万"), Some(25_000));
    }

    #[test]
    fn extracts_balanced_initial_json() {
        let html = r#"
            <script>
            var ytInitialPlayerResponse = {"microformat":{"playerMicroformatRenderer":{"liveBroadcastDetails":{"concurrentViewers":"321"}}},"nested":{"text":"brace } in string"}};
            </script>
        "#;
        let values = extract_metadata_from_html(html, &empty_paths());
        assert_eq!(values.concurrent_viewers, Some(321));
    }

    #[test]
    fn override_paths_take_precedence() {
        let html = r#"
            var ytInitialPlayerResponse = {"custom":{"now":"44","likes":{"simpleText":"88"}}};
        "#;
        let mut paths = HashMap::new();
        paths.insert(KEY_CONCURRENT_PATHS.to_string(), "custom>now".to_string());
        paths.insert(KEY_LIKES_PATHS.to_string(), "custom>likes".to_string());

        let values = extract_metadata_from_html(html, &paths);
        assert_eq!(values.concurrent_viewers, Some(44));
        assert_eq!(values.likes, Some(88));
    }

    #[test]
    fn extracts_title_from_video_details_path() {
        let roots = vec![json!({
            "videoDetails": {
                "title": "US-1301 live stream"
            }
        })];

        assert_eq!(
            extract_string_by_paths(&roots, DEFAULT_TITLE_PATHS),
            Some("US-1301 live stream".to_string())
        );
    }

    #[test]
    fn missing_title_path_returns_none() {
        let roots = vec![json!({
            "videoDetails": {
                "videoId": "abc123"
            }
        })];

        assert_eq!(extract_string_by_paths(&roots, DEFAULT_TITLE_PATHS), None);
    }
}
