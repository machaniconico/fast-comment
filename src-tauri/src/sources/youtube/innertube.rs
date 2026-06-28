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
use std::time::Duration;

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
    "\"invalidationContinuationData\":{\"continuation\":\"",
    "\"timedContinuationData\":{\"continuation\":\"",
    "\"reloadContinuationData\":{\"continuation\":\"",
    "\"liveChatReplayContinuationData\":{\"continuation\":\"",
    "\"continuation\":\"",
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
            .timeout(Duration::from_secs(15))
            .connect_timeout(Duration::from_secs(10))
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

/// `<marker>` 直後の JSON 文字列値を、終端 `"` まで取り出す。
fn extract_json_string_field(html: &str, marker: &str) -> Option<String> {
    let start = html.find(marker)? + marker.len();
    let tail = &html[start..];
    let mut escaped = false;
    for (idx, ch) in tail.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        match ch {
            '\\' => escaped = true,
            '"' => return Some(tail[..idx].to_string()),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_paths(entries: &[(&str, &str)]) -> std::collections::HashMap<String, String> {
        entries
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string()))
            .collect()
    }

    #[test]
    fn extracts_json_string_field_after_marker() {
        // 指定マーカー直後の文字列値だけを取り出す。
        let html = r#"prefix {"target":"expected-value","other":"ignored"} suffix"#;

        assert_eq!(
            extract_json_string_field(html, r#""target":""#).as_deref(),
            Some("expected-value")
        );
    }

    #[test]
    fn json_string_field_returns_none_when_marker_is_absent() {
        // マーカーが見つからない場合は抽出しない。
        let html = r#"{"other":"value"}"#;

        assert_eq!(extract_json_string_field(html, r#""target":""#), None);
    }

    #[test]
    fn extracts_api_key_and_client_version_with_default_markers() {
        // 既定マーカーで API_KEY と clientVersion を抽出する。
        let html = r#"
            <script>
            ytcfg.set({
                "INNERTUBE_API_KEY":"default-api-key",
                "INNERTUBE_CONTEXT_CLIENT_VERSION":"2.20240529.01.00"
            });
            </script>
        "#;
        let paths = make_paths(&[]);

        assert_eq!(extract_api_key(html, &paths).as_deref(), Some("default-api-key"));
        assert_eq!(
            extract_client_version(html, &paths).as_deref(),
            Some("2.20240529.01.00")
        );
    }

    #[test]
    fn override_markers_take_precedence_for_api_key_and_client_version() {
        // overrides.paths の代替マーカーを既定マーカーより優先する。
        let html = r#"
            {
                "INNERTUBE_API_KEY":"default-api-key",
                "customApiKey":"override-api-key",
                "INNERTUBE_CONTEXT_CLIENT_VERSION":"default-client-version",
                "customClientVersion":"override-client-version"
            }
        "#;
        let paths = make_paths(&[
            (KEY_API_KEY_MARKER, r#""customApiKey":""#),
            (KEY_CLIENT_VERSION_MARKER, r#""customClientVersion":""#),
        ]);

        assert_eq!(extract_api_key(html, &paths).as_deref(), Some("override-api-key"));
        assert_eq!(
            extract_client_version(html, &paths).as_deref(),
            Some("override-client-version")
        );
    }

    #[test]
    fn initial_continuation_prefers_specific_markers_before_generic() {
        // 汎用 continuation が先に現れても、限定マーカーを優先する。
        let paths = make_paths(&[]);
        let html = r#"
            {
                "continuation":"generic-token",
                "invalidationContinuationData":{"continuation":"invalidation-token"},
                "timedContinuationData":{"continuation":"timed-token"},
                "reloadContinuationData":{"continuation":"reload-token"},
                "liveChatReplayContinuationData":{"continuation":"replay-token"}
            }
        "#;

        assert_eq!(
            extract_initial_continuation(html, &paths).as_deref(),
            Some("invalidation-token")
        );
    }

    #[test]
    fn initial_continuation_uses_generic_marker_as_last_fallback() {
        // 限定マーカーが無い場合だけ汎用 continuation にフォールバックする。
        let paths = make_paths(&[]);
        let html = r#"
            {
                "metadata":"no specific continuation container",
                "continuation":"generic-token"
            }
        "#;

        assert_eq!(
            extract_initial_continuation(html, &paths).as_deref(),
            Some("generic-token")
        );
    }

    #[test]
    fn initial_continuation_specific_marker_order_is_stable() {
        // invalidation が無い場合も timed -> reload -> replay -> 汎用の順序を守る。
        let paths = make_paths(&[]);
        let html = r#"
            {
                "continuation":"generic-token",
                "reloadContinuationData":{"continuation":"reload-token"},
                "liveChatReplayContinuationData":{"continuation":"replay-token"},
                "timedContinuationData":{"continuation":"timed-token"}
            }
        "#;

        assert_eq!(
            extract_initial_continuation(html, &paths).as_deref(),
            Some("timed-token")
        );
    }

    #[test]
    fn empty_or_missing_override_paths_match_default_behavior() {
        // 空/未指定の paths は現行の既定マーカー挙動と完全一致する。
        let html = r#"
            {
                "INNERTUBE_API_KEY":"default-api-key",
                "innertubeApiKey":"alt-api-key",
                "INNERTUBE_CONTEXT_CLIENT_VERSION":"2.20240529.01.00",
                "clientVersion":"alt-client-version",
                "invalidationContinuationData":{"continuation":"invalidation-token"}
            }
        "#;
        let missing_paths = make_paths(&[]);
        let empty_paths = make_paths(&[
            (KEY_API_KEY_MARKER, ""),
            (KEY_API_KEY_MARKER_ALT, ""),
            (KEY_CLIENT_VERSION_MARKER, ""),
            (KEY_CLIENT_VERSION_MARKER_ALT, ""),
            (KEY_INITIAL_CONTINUATION_MARKERS, ""),
        ]);

        assert_eq!(
            extract_api_key(html, &empty_paths),
            extract_api_key(html, &missing_paths)
        );
        assert_eq!(
            extract_client_version(html, &empty_paths),
            extract_client_version(html, &missing_paths)
        );
        assert_eq!(
            extract_initial_continuation(html, &empty_paths),
            extract_initial_continuation(html, &missing_paths)
        );
    }
}
