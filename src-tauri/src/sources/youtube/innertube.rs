//! YouTube InnerTube(非公式 API)のリクエスト組み立て。
//!
//! 手順:
//!  1. ライブ配信ページの HTML を取得し、`INNERTUBE_API_KEY` / clientVersion /
//!     初期 continuation を抽出する。
//!  2. `POST /youtubei/v1/live_chat/get_live_chat?key=<API_KEY>` に
//!     context+continuation を送り、繰り返しポーリングする。
//!
//! API_KEY・clientVersion・抽出パターンは `config.youtube_overrides` で
//! 再ビルド無しに差し替えできる(SPEC §4.2)。

use std::collections::HashMap;

use reqwest::Client;
use serde_json::{json, Value};

use crate::config::YoutubeOverrides;

/// HTML 取得時に名乗る UA(通常ブラウザ相当)。
const USER_AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0 Safari/537.36";

/// clientVersion が抽出できなかった場合の保険値(古くなる可能性あり=overrides 推奨)。
const FALLBACK_CLIENT_VERSION: &str = "2.20240101.00.00";

// ─────────────────────────────────────────────────────────────────────────
// youtube_overrides.paths のキー名(SPEC §4.2「抽出パスを再ビルド無しで差し替え」)。
// いずれも欠落/空のときは下記ハードコード既定にフォールバックし、現行挙動を維持する。
// ─────────────────────────────────────────────────────────────────────────

/// API_KEY 抽出マーカー(1次)。既定 `"INNERTUBE_API_KEY":"`。
const KEY_API_KEY_MARKER: &str = "apiKeyMarker";
/// API_KEY 抽出マーカー(2次フォールバック)。既定 `"innertubeApiKey":"`。
const KEY_API_KEY_MARKER_ALT: &str = "apiKeyMarkerAlt";
/// clientVersion 抽出マーカー(1次)。既定 `"INNERTUBE_CONTEXT_CLIENT_VERSION":"`。
const KEY_CLIENT_VERSION_MARKER: &str = "clientVersionMarker";
/// clientVersion 抽出マーカー(2次フォールバック)。既定 `"clientVersion":"`。
const KEY_CLIENT_VERSION_MARKER_ALT: &str = "clientVersionMarkerAlt";
/// 初期 continuation 抽出マーカー群(改行区切りで複数指定可)。
/// 既定は下記 `DEFAULT_INITIAL_CONTINUATION_MARKERS`。
const KEY_INITIAL_CONTINUATION_MARKERS: &str = "initialContinuationMarkers";

const DEFAULT_API_KEY_MARKER: &str = "\"INNERTUBE_API_KEY\":\"";
const DEFAULT_API_KEY_MARKER_ALT: &str = "\"innertubeApiKey\":\"";
const DEFAULT_CLIENT_VERSION_MARKER: &str = "\"INNERTUBE_CONTEXT_CLIENT_VERSION\":\"";
const DEFAULT_CLIENT_VERSION_MARKER_ALT: &str = "\"clientVersion\":\"";
const DEFAULT_INITIAL_CONTINUATION_MARKERS: &[&str] = &[
    "\"continuation\":\"",
    "\"invalidationContinuationData\":{\"continuation\":\"",
    "\"timedContinuationData\":{\"continuation\":\"",
    "\"reloadContinuationData\":{\"continuation\":\"",
];

/// `paths` から指定キーの非空値を取り出す。欠落/空文字なら `default` を返す。
/// キーが無い/空のときは厳密に既定挙動を維持する。
fn path_or<'a>(paths: &'a HashMap<String, String>, key: &str, default: &'a str) -> &'a str {
    paths
        .get(key)
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}

/// 1ポーリングセッションで使い回す状態。
#[derive(Debug, Clone)]
pub struct Session {
    /// INNERTUBE_API_KEY。
    pub api_key: String,
    /// WEB クライアントのバージョン。
    pub client_version: String,
    /// 次に送る continuation トークン。
    pub continuation: String,
}

/// InnerTube への HTTP クライアント。
pub struct InnerTubeClient {
    http: Client,
    overrides: YoutubeOverrides,
}

impl InnerTubeClient {
    pub fn new(overrides: YoutubeOverrides) -> anyhow::Result<Self> {
        let http = Client::builder()
            .user_agent(USER_AGENT)
            .gzip(true)
            .build()?;
        Ok(InnerTubeClient { http, overrides })
    }

    /// 初期HTMLから API_KEY / clientVersion / 初期 continuation を抽出する。
    ///
    /// overrides に api_key / client_version があればそれを優先し、抽出をスキップ可能。
    pub async fn bootstrap(&self, video_id: &str) -> anyhow::Result<Session> {
        // ライブチャット専用ページを使うと continuation が確実に取りやすい。
        let url = format!(
            "https://www.youtube.com/live_chat?is_popout=1&v={video_id}"
        );
        let html = self
            .http
            .get(&url)
            .header("Accept-Language", "ja,en;q=0.8")
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        // 各値: overrides 優先 → HTML 抽出 → フォールバック。
        // 抽出マーカーも overrides.paths で差し替え可能(既定にフォールバック)。
        let paths = &self.overrides.paths;
        let api_key = self
            .overrides
            .api_key
            .clone()
            .or_else(|| extract_api_key(&html, paths))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "INNERTUBE_API_KEY を抽出できませんでした(youtubeOverrides.apiKey で指定可)"
                )
            })?;

        let client_version = self
            .overrides
            .client_version
            .clone()
            .or_else(|| extract_client_version(&html, paths))
            .unwrap_or_else(|| FALLBACK_CLIENT_VERSION.to_string());

        let continuation = extract_initial_continuation(&html, paths).unwrap_or_default();

        Ok(Session {
            api_key,
            client_version,
            continuation,
        })
    }

    /// `get_live_chat` を1回叩き、生の JSON(Value)を返す。
    pub async fn get_live_chat(&self, session: &Session) -> anyhow::Result<Value> {
        let url = format!(
            "https://www.youtube.com/youtubei/v1/live_chat/get_live_chat?key={}&prettyPrint=false",
            session.api_key
        );

        let body = json!({
            "context": {
                "client": {
                    "clientName": "WEB",
                    "clientVersion": session.client_version,
                    "hl": "ja",
                    "gl": "JP",
                }
            },
            "continuation": session.continuation,
        });

        let resp = self
            .http
            .post(&url)
            .header("Content-Type", "application/json")
            .header("X-YouTube-Client-Name", "1")
            .header("X-YouTube-Client-Version", &session.client_version)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;

        Ok(resp)
    }
}

/// HTML から `"INNERTUBE_API_KEY":"..."` を抽出。
///
/// マーカー文字列は `paths` の `apiKeyMarker`/`apiKeyMarkerAlt` で差し替え可能。
/// 欠落/空のときは既定マーカーで現行どおり 2 通りの綴りを試す。
fn extract_api_key(html: &str, paths: &HashMap<String, String>) -> Option<String> {
    let marker = path_or(paths, KEY_API_KEY_MARKER, DEFAULT_API_KEY_MARKER);
    let marker_alt = path_or(paths, KEY_API_KEY_MARKER_ALT, DEFAULT_API_KEY_MARKER_ALT);
    extract_json_string_field(html, marker)
        .or_else(|| extract_json_string_field(html, marker_alt))
}

/// HTML から clientVersion を抽出。
///
/// マーカー文字列は `paths` の `clientVersionMarker`/`clientVersionMarkerAlt` で差し替え可能。
fn extract_client_version(html: &str, paths: &HashMap<String, String>) -> Option<String> {
    let marker = path_or(paths, KEY_CLIENT_VERSION_MARKER, DEFAULT_CLIENT_VERSION_MARKER);
    let marker_alt = path_or(
        paths,
        KEY_CLIENT_VERSION_MARKER_ALT,
        DEFAULT_CLIENT_VERSION_MARKER_ALT,
    );
    extract_json_string_field(html, marker)
        .or_else(|| extract_json_string_field(html, marker_alt))
}

/// HTML(live_chat ページ)から初期 continuation を抽出。
///
/// `liveChatRenderer` 配下の `continuations[].invalidationContinuationData` 等に入る。
/// 寛容に複数マーカーを試す。マーカー群は `paths` の `initialContinuationMarkers`
/// (改行区切りで複数指定可)で差し替え可能。欠落/空のときは既定マーカー群を使う。
fn extract_initial_continuation(html: &str, paths: &HashMap<String, String>) -> Option<String> {
    // override が指定されていれば改行区切りで分割、無ければ既定群。
    let overridden: Vec<&str> = paths
        .get(KEY_INITIAL_CONTINUATION_MARKERS)
        .map(|s| {
            s.split('\n')
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let markers: &[&str] = if overridden.is_empty() {
        DEFAULT_INITIAL_CONTINUATION_MARKERS
    } else {
        &overridden
    };

    for marker in markers {
        if let Some(v) = extract_json_string_field(html, marker) {
            if !v.is_empty() {
                return Some(v);
            }
        }
    }
    None
}

/// `<marker>` 直後の JSON 文字列値を、終端 `"` まで(エスケープ考慮なしの単純抽出)で取り出す。
///
/// continuation/api_key にはエスケープが入りにくいため単純抽出で十分。
fn extract_json_string_field(html: &str, marker: &str) -> Option<String> {
    let start = html.find(marker)? + marker.len();
    let tail = &html[start..];
    // 次の `"` までを値とする(値内に `\"` を含まない前提)。
    let end = tail.find('"')?;
    Some(tail[..end].to_string())
}
