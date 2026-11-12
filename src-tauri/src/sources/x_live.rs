//! X (Twitter) の `@handle` から現在のライブ配信 ID を検知する。
//!
//! 設計は `.omc/plans/x-live-detect-design-2026-11-12.md`、実測は
//! `.omc/research/x-live-detect-2026-11-12.md`。cookie 認証が有る場合のみ
//! 自動検知する:
//!
//! 1. GraphQL `UserByScreenName` で handle → user id(rest_id)。寿命中キャッシュ。
//! 2. `fleets/v1/avatar_content?user_ids={uid}&only_spaces=false` で
//!    `users[uid].spaces.live_content.livevideo` を探し、RUNNING のみ ID を返す。
//! 3. 候補 ID は既存 `broadcasts/show.json` で state と所有者を確認してから接続する
//!    (HTTP は呼ばず、確認用の純関数 [`is_running_owned`] だけを提供する)。
//!
//! パースは固い struct deserialize をせず `serde_json::Value` のパス探索で行い、
//! 欠落は NotLive / Unknown へ劣化させる。cookie の実値はログ・エラー・Debug に
//! 出さない(`LiveDetector` の Debug は URL と認証種別のみ)。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::header::{HeaderMap, HeaderValue};
use serde_json::{json, Value};

use super::x::{self, XAuth};
use crate::config::XOverrides;

/// X Web クライアントの公開 Bearer(シークレットではない)。x.rs と同じ値。
const X_WEB_BEARER: &str = "AAAAAAAAAAAAAAAAAAAAANRILgAAAAAAnNwIzUejRCOuH5E6I8xnZz4puTs=1Zv7ttfk8LF81IUq16cHjhLTvJu4FA33AGWWjCpTnA";
const USER_BY_SCREEN_NAME_URL: &str =
    "https://x.com/i/api/graphql/eqshD_Hdu6hEX1GUSXOKeA/UserByScreenName";
const AVATAR_CONTENT_URL: &str = "https://x.com/i/api/fleets/v1/avatar_content";
const USER_AGENT: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// ポーリング間隔の上限(秒)。`livePollSecs` の巨大値で実質停止しないため。
pub const MAX_POLL_SECS: u64 = 3_600;

/// reset ヘッダがある時の待機の上限(時計ずれ対策。到達したら再度ヘッダを読み直す)。
pub const MAX_RESET_WAIT: Duration = Duration::from_secs(3_600);

/// アカウント不在(NotFound)時の再解決間隔。凍結解除・改名への追従用。
pub const NOT_FOUND_RETRY: Duration = Duration::from_secs(600);

/// この回数 Unknown が続いたら待機を指数的に延ばす(queryId 失効の 404 等)。
pub const UNKNOWN_ESCALATE_AFTER: u32 = 5;

/// 既定のポーリング間隔(秒)。
pub const DEFAULT_POLL_SECS: u64 = 30;
/// `livePollSecs` を設定した場合の下限(秒)。
pub const MIN_POLL_SECS: u64 = 15;
/// ポーリング待機に足すジッターの最大値(ミリ秒)。
pub const MAX_JITTER_MS: u64 = 3_000;
/// 401/403(cookie 失効)後の再試行待機。
pub const AUTH_RETRY: Duration = Duration::from_secs(300);
/// ヘッダ無し 429 の指数バックオフ上限。
pub const BACKOFF_CAP: Duration = Duration::from_secs(900);

/// チャンネル識別子の解釈結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum XTarget {
    /// 従来の配信 ID モード(配信 URL / 生 ID)。
    Broadcast(String),
    /// アカウントモード。handle は入力の表記を保つ(小文字化しない)。
    Account(String),
}

/// `avatar_content` 1ユーザー分の判定結果。欠落は NotLive、型不一致は Unknown。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveProbe {
    Live(String),
    NotLive,
    /// 応答は取れたが解釈できない(「配信なし」とは区別する)。
    Unknown(String),
}

/// `x-rate-limit-*` ヘッダの値。両方が数値のときだけ Some。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RateLimit {
    /// `x-rate-limit-remaining`。
    pub remaining: u64,
    /// `x-rate-limit-reset`(UTC Unix 秒)。
    pub reset_epoch_s: u64,
}

/// 待機理由。`next_wait` の純粋な入力。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitCondition {
    /// 通常のポーリング。
    Normal,
    /// レート制限。`until` が None なら reset ヘッダ無しの 429(指数バックオフ)。
    RateLimited { until: Option<u64> },
    /// cookie 失効(401/403)。
    AuthFailed,
    /// アカウント不在。ゆっくり再解決する。
    NotFound,
}

/// 検知ループの1回分の結果。
#[derive(Debug)]
pub enum DetectOutcome {
    Live(String),
    NotLive,
    /// 通信/パース失敗。「配信なし」とは区別して状態を維持する。
    Unknown(anyhow::Error),
    /// レート制限。`until` は UTC Unix 秒。None は reset 不明。
    RateLimited { until: Option<u64> },
    /// cookie 失効。検知を止めて長く待つ。
    AuthFailed,
    /// handle のアカウントが存在しない/凍結等。再試行しても直らない。
    NotFound,
}

impl DetectOutcome {
    pub fn wait_condition(&self) -> WaitCondition {
        match self {
            DetectOutcome::RateLimited { until } => WaitCondition::RateLimited { until: *until },
            DetectOutcome::AuthFailed => WaitCondition::AuthFailed,
            DetectOutcome::NotFound => WaitCondition::NotFound,
            _ => WaitCondition::Normal,
        }
    }
}

/// `resolve_user_id` の失敗種別。cookie 値は含めない。
#[derive(Debug, thiserror::Error)]
pub enum DetectError {
    #[error("X API rate limited (until: {until:?})")]
    RateLimited { until: Option<u64> },
    #[error("X 認証失敗(cookie 失効の可能性)")]
    AuthFailed,
    #[error("X アカウントが見つからない")]
    NotFound,
    #[error("{0}")]
    Other(#[from] anyhow::Error),
}

impl DetectError {
    pub fn wait_condition(&self) -> WaitCondition {
        match self {
            DetectError::RateLimited { until } => WaitCondition::RateLimited { until: *until },
            DetectError::AuthFailed => WaitCondition::AuthFailed,
            DetectError::NotFound => WaitCondition::NotFound,
            DetectError::Other(_) => WaitCondition::Normal,
        }
    }
}

impl From<DetectError> for DetectOutcome {
    fn from(e: DetectError) -> Self {
        match e {
            DetectError::RateLimited { until } => DetectOutcome::RateLimited { until },
            DetectError::AuthFailed => DetectOutcome::AuthFailed,
            DetectError::NotFound => DetectOutcome::NotFound,
            DetectError::Other(e) => DetectOutcome::Unknown(e),
        }
    }
}

/// cookie 認証付きの X ライブ検知クライアント。
///
/// `XAuth` を保持するが Debug 実装では値を出さない。
pub struct LiveDetector {
    client: reqwest::Client,
    bearer: String,
    auth: XAuth,
    user_by_screen_name_url: String,
    avatar_content_url: String,
}

impl std::fmt::Debug for LiveDetector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveDetector")
            .field("user_by_screen_name_url", &self.user_by_screen_name_url)
            .field("avatar_content_url", &self.avatar_content_url)
            .field("auth", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl LiveDetector {
    /// 呼び出し側が用意した `reqwest::Client` を使う。
    pub fn new(client: reqwest::Client, overrides: XOverrides, auth: XAuth) -> Self {
        let bearer = overrides
            .bearer_token
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| X_WEB_BEARER.to_string());
        LiveDetector {
            client,
            bearer,
            auth,
            user_by_screen_name_url: override_url(
                &overrides,
                "userByScreenNameUrl",
                USER_BY_SCREEN_NAME_URL,
            ),
            avatar_content_url: override_url(&overrides, "avatarContentUrl", AVATAR_CONTENT_URL),
        }
    }

    /// ブラウザ相当の UA と接続タイムアウトを持つ既定クライアントで作る。
    pub fn with_default_client(
        overrides: XOverrides,
        auth: XAuth,
    ) -> anyhow::Result<LiveDetector> {
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()?;
        Ok(LiveDetector::new(client, overrides, auth))
    }

    /// handle → user id(rest_id)。新形 `user_result_by_screen_name` と
    /// 旧形 `user.result` の両方を見る。呼び出し側でキャッシュする想定。
    pub async fn resolve_user_id(&self, handle: &str) -> Result<String, DetectError> {
        let handle = handle.trim().trim_start_matches('@');
        if !is_valid_handle(handle) {
            return Err(DetectError::Other(anyhow::anyhow!("不正な X handle")));
        }
        let variables = json!({
            "screenName": handle,
            "withIdentity": true,
            "__relay_internal__pv__xclientsfeaturesappviewerisloggedinrelayprovider": true,
        })
        .to_string();
        let req = self
            .request(&self.user_by_screen_name_url)?
            .query(&[("variables", variables.as_str())]);
        let (v, _) = self.send_json(req).await?;
        if let Some(id) = parse_user_id(&v) {
            return Ok(id);
        }
        if user_not_found(&v) {
            return Err(DetectError::NotFound);
        }
        Err(DetectError::Other(anyhow::anyhow!("UserByScreenName に rest_id が無い")))
    }

    /// `avatar_content` を1回引き、RUNNING のライブ動画 ID を返す。
    ///
    /// 戻り値の第2要素は「Live だがレート残量 0」のときの reset(UTC Unix 秒)。
    /// Live はそのまま返すので、呼び出し側は先に接続し、次の probe 前にだけ待つ。
    /// NotLive/Unknown で残量 0 の場合は `RateLimited` にして待たせる。
    pub async fn probe(&self, uid: &str) -> (DetectOutcome, Option<u64>) {
        let uid = uid.trim();
        if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
            return (DetectOutcome::Unknown(anyhow::anyhow!("不正な user id")), None);
        }
        let req = match self.request(&self.avatar_content_url) {
            Ok(r) => r.query(&[("user_ids", uid), ("only_spaces", "false")]),
            Err(e) => return (e.into(), None),
        };
        let (v, rate) = match self.send_json(req).await {
            Ok(v) => v,
            Err(e) => return (e.into(), None),
        };
        let exhausted = rate.filter(|r| r.remaining == 0).map(|r| r.reset_epoch_s);
        let outcome = match parse_avatar_livevideo(&v, uid) {
            LiveProbe::Live(id) => return (DetectOutcome::Live(id), exhausted),
            LiveProbe::NotLive => DetectOutcome::NotLive,
            LiveProbe::Unknown(reason) => DetectOutcome::Unknown(anyhow::anyhow!(reason)),
        };
        // remaining==0 はこの後すぐ枯渇する。reset まで待ってから次を撃つ。
        if exhausted.is_some() {
            return (DetectOutcome::RateLimited { until: exhausted }, None);
        }
        (outcome, None)
    }

    fn request(&self, url: &str) -> Result<reqwest::RequestBuilder, DetectError> {
        // 秘密値はログ/Debug に出ないよう sensitive 指定のヘッダ値にする。
        let sensitive = |s: &str| -> Result<HeaderValue, DetectError> {
            let mut v = HeaderValue::from_str(s)
                .map_err(|_| DetectError::Other(anyhow::anyhow!("認証情報に使えない文字がある")))?;
            v.set_sensitive(true);
            Ok(v)
        };
        Ok(self
            .client
            .get(url)
            .timeout(HTTP_TIMEOUT)
            .header("authorization", format!("Bearer {}", self.bearer))
            .header("x-csrf-token", sensitive(&self.auth.csrf_token)?)
            .header("cookie", sensitive(&self.cookie_header())?)
            .header("origin", "https://x.com")
            .header("referer", "https://x.com/")
            .header("x-twitter-active-user", "yes")
            .header("x-twitter-client-language", "en"))
    }

    fn cookie_header(&self) -> String {
        format!(
            "auth_token={}; ct0={}",
            self.auth.auth_token, self.auth.csrf_token
        )
    }

    /// 共通の HTTP 応答分類。401/403 → AuthFailed、429 → RateLimited。
    /// cookie 値をエラー文言へ含めない。
    async fn send_json(
        &self,
        req: reqwest::RequestBuilder,
    ) -> Result<(Value, Option<RateLimit>), DetectError> {
        let resp = req.send().await.map_err(|e| DetectError::Other(e.into()))?;
        let status = resp.status();
        let rate = parse_rate_limit(resp.headers());
        if status == reqwest::StatusCode::UNAUTHORIZED
            || status == reqwest::StatusCode::FORBIDDEN
        {
            return Err(DetectError::AuthFailed);
        }
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let until = rate
                .map(|r| r.reset_epoch_s)
                .or_else(|| reset_epoch(resp.headers()))
                .or_else(|| retry_after_epoch(resp.headers()));
            return Err(DetectError::RateLimited { until });
        }
        if !status.is_success() {
            return Err(DetectError::Other(anyhow::anyhow!("X API HTTP {status}")));
        }
        let v = resp
            .json::<Value>()
            .await
            .map_err(|e| DetectError::Other(e.into()))?;
        // 200 でも errors に認証系コードが入る場合がある。
        if has_auth_error(&v) {
            return Err(DetectError::AuthFailed);
        }
        Ok((v, rate))
    }
}

/// チャンネル識別子を配信 ID モード / アカウントモードへ振り分ける。
///
/// 後方互換のため、`/i/broadcasts/` を含む URL・`@handle`・
/// `x.com/{handle}` / `twitter.com/{handle}` 以外の裸の文字列は従来どおり
/// 配信 ID として扱う(設計 §1 の「bare handle も Account」より互換性を優先)。
pub fn parse_x_target(input: &str) -> XTarget {
    let s = input.trim();
    if s.contains("/i/broadcasts/") {
        let id = x::extract_broadcast_id(s);
        if !id.is_empty() {
            return XTarget::Broadcast(id);
        }
    }
    if let Some(handle) = s.strip_prefix('@') {
        if is_valid_handle(handle) {
            return XTarget::Account(handle.to_string());
        }
    }
    if let Some(handle) = account_from_url(s) {
        return XTarget::Account(handle);
    }
    XTarget::Broadcast(x::extract_broadcast_id(s))
}

/// UserByScreenName 応答から rest_id を取り出す(新形→旧形)。
pub fn parse_user_id(v: &Value) -> Option<String> {
    v.pointer("/data/user_result_by_screen_name/result/rest_id")
        .and_then(|v| id_string(Some(v)))
        .or_else(|| v.pointer("/data/user/result/rest_id").and_then(|v| id_string(Some(v))))
}

/// `avatar_content` の `users[uid].spaces.live_content.livevideo` を寛容に読む。
///
/// - users 無し/null・トップレベル errors → Unknown
/// - users 内の uid / livevideo の欠落 → NotLive(未検出)
/// - `audiospace` だけがある場合 → NotLive
/// - state が RUNNING(大小無視)かつ id がある → Live
/// - 型不一致・RUNNING なのに id が無い → Unknown
pub fn parse_avatar_livevideo(v: &Value, uid: &str) -> LiveProbe {
    if v.get("errors").is_some() {
        return LiveProbe::Unknown("avatar_content 応答に errors が含まれる".to_string());
    }
    // 配信なしのアカウントでも users 自体は返る(uid キーだけ無い)。users が無い/null は異常。
    let Some(users) = v.get("users").filter(|u| !u.is_null()) else {
        return LiveProbe::Unknown("avatar_content 応答に users が無い".to_string());
    };
    let Some(users) = users.as_object() else {
        return LiveProbe::Unknown("users がオブジェクトでない".to_string());
    };
    let Some(user) = users.get(uid) else {
        return LiveProbe::NotLive;
    };
    let Some(spaces) = user.get("spaces") else {
        return LiveProbe::NotLive;
    };
    let Some(spaces) = spaces.as_object() else {
        return LiveProbe::Unknown("spaces がオブジェクトでない".to_string());
    };
    let Some(content) = spaces.get("live_content") else {
        return LiveProbe::NotLive;
    };
    let Some(content) = content.as_object() else {
        return LiveProbe::Unknown("live_content がオブジェクトでない".to_string());
    };
    let Some(livevideo) = content.get("livevideo") else {
        return LiveProbe::NotLive;
    };
    if livevideo.is_null() {
        return LiveProbe::NotLive;
    }
    let Some(livevideo) = livevideo.as_object() else {
        return LiveProbe::Unknown("livevideo がオブジェクトでない".to_string());
    };
    let Some(state) = livevideo.get("state").and_then(Value::as_str) else {
        return LiveProbe::Unknown("livevideo.state が無い".to_string());
    };
    if !state.eq_ignore_ascii_case("RUNNING") {
        return LiveProbe::NotLive;
    }
    match id_string(livevideo.get("id")) {
        Some(id) => LiveProbe::Live(id),
        None => LiveProbe::Unknown("RUNNING だが livevideo.id が無い".to_string()),
    }
}

/// show.json の state と所有者(`twitter_user_id`)が、RUNNING かつ uid の配信かを判定する。
pub fn is_running_owned(state: &str, broadcaster_id: Option<&str>, uid: &str) -> bool {
    state.eq_ignore_ascii_case("RUNNING") && broadcaster_id == Some(uid)
}

/// UserByScreenName 応答が「アカウント不在/凍結/利用不可」を示すか。
/// result が空オブジェクト・null、または `__typename: UserUnavailable`。
/// errors だけで data が無い等の曖昧な応答は false(Unknown 扱いで再試行)。
pub fn user_not_found(v: &Value) -> bool {
    ["/data/user_result_by_screen_name", "/data/user"]
        .iter()
        .filter_map(|p| v.pointer(p))
        .filter(|u| u.is_object())
        .any(|u| match u.get("result") {
            None | Some(Value::Null) => true,
            Some(r) => {
                r.get("__typename").and_then(Value::as_str) == Some("UserUnavailable")
                    || r.as_object().is_some_and(|o| o.is_empty())
            }
        })
}

/// 200 応答内の `errors[].code` が認証系(32 / 89 / 239)か。
pub fn has_auth_error(v: &Value) -> bool {
    v.get("errors")
        .and_then(Value::as_array)
        .is_some_and(|errs| {
            errs.iter().any(|e| {
                matches!(e.get("code").and_then(Value::as_i64), Some(32 | 89 | 239))
            })
        })
}

/// Unknown が連続した回数から待機条件を選ぶ。閾値未満は通常、以降は指数バックオフ。
/// 戻り値の u32 は `next_wait` の attempt(閾値超過ぶん)。
pub fn escalate_unknown(streak: u32) -> (WaitCondition, u32) {
    if streak < UNKNOWN_ESCALATE_AFTER {
        (WaitCondition::Normal, 0)
    } else {
        (
            WaitCondition::RateLimited { until: None },
            streak - UNKNOWN_ESCALATE_AFTER + 1,
        )
    }
}

/// `x-rate-limit-*` を読む。どちらかが欠落/非数値なら None。
pub fn parse_rate_limit(headers: &HeaderMap) -> Option<RateLimit> {
    Some(RateLimit {
        remaining: header_u64(headers, "x-rate-limit-remaining")?,
        reset_epoch_s: header_u64(headers, "x-rate-limit-reset")?,
    })
}

/// `xOverrides.livePollSecs` を加味したポーリング間隔。既定 30 秒、下限 15 秒、上限 1 時間。
pub fn poll_interval(override_secs: Option<u64>) -> Duration {
    Duration::from_secs(
        override_secs
            .unwrap_or(DEFAULT_POLL_SECS)
            .clamp(MIN_POLL_SECS, MAX_POLL_SECS),
    )
}

/// 次回ポーリングまでの待機時間(純関数)。
///
/// - RateLimited + until あり → reset まで(+ジッター)
/// - RateLimited + until なし(ヘッダ無し 429)→ 指数バックオフ(上限 15 分)
/// - AuthFailed → 5 分
/// - それ以外 → 基本間隔(+ジッター)
pub fn next_wait(
    condition: WaitCondition,
    base: Duration,
    now_epoch_s: u64,
    attempt: u32,
    jitter: Duration,
) -> Duration {
    let wait = match condition {
        WaitCondition::Normal => base,
        WaitCondition::AuthFailed => AUTH_RETRY,
        WaitCondition::NotFound => NOT_FOUND_RETRY,
        WaitCondition::RateLimited { until: Some(until) } => {
            let secs = until.saturating_sub(now_epoch_s);
            if secs == 0 {
                base
            } else {
                // reset ヘッダは尊重する。時計ずれ対策の上限(1h)だけ設ける。
                Duration::from_secs(secs).min(MAX_RESET_WAIT)
            }
        }
        WaitCondition::RateLimited { until: None } => {
            let factor = 1u64 << attempt.min(10);
            base.saturating_mul(factor as u32).min(BACKOFF_CAP)
        }
    };
    wait.saturating_add(jitter)
}

fn override_url(overrides: &XOverrides, key: &str, default: &str) -> String {
    overrides
        .endpoints
        .get(key)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| default.to_string())
}

fn account_from_url(input: &str) -> Option<String> {
    // スキームは大小無視(`HTTPS://x.com/..`)。ASCII なのでバイト長は保たれる。
    let lower = input.to_ascii_lowercase();
    let rest = if lower.starts_with("https://") {
        &input[8..]
    } else if lower.starts_with("http://") {
        &input[7..]
    } else {
        input
    };
    let (host, path) = rest.split_once('/')?;
    let host = host.split(':').next().unwrap_or("").to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    if !matches!(
        host.as_str(),
        "x.com" | "twitter.com" | "mobile.x.com" | "mobile.twitter.com"
    ) {
        return None;
    }
    let path = path.split(['?', '#']).next().unwrap_or(path);
    let first = path.split('/').find(|segment| !segment.is_empty())?;
    let lower = first.to_ascii_lowercase();
    if is_reserved_path(&lower) || !is_valid_handle(first) {
        return None;
    }
    Some(first.to_string())
}

/// URL の第1パスとして扱わないセグメント。`live` / `help` などの実在 handle も
/// URL 形(`x.com/live`)では弾くが、`@live` の直接入力は通る。
/// UI 側 `ChannelAdd.svelte` の `X_RESERVED_PATHS` と同期すること。
fn is_reserved_path(segment: &str) -> bool {
    matches!(
        segment,
        "i"
            | "home"
            | "search"
            | "explore"
            | "settings"
            | "messages"
            | "notifications"
            | "compose"
            | "login"
            | "logout"
            | "signup"
            | "tos"
            | "privacy"
            | "intent"
            | "share"
            | "hashtag"
            | "account"
            | "jobs"
            | "about"
            | "download"
            | "help"
            | "communities"
            | "premium"
            | "verified"
            | "bookmark"
            | "followers"
            | "following"
            | "lists"
            | "moments"
            | "topics"
            | "live"
            | "broadcasts"
    )
}

fn is_valid_handle(handle: &str) -> bool {
    !handle.is_empty()
        && handle.len() <= 15
        && handle
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

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

fn header_u64(headers: &HeaderMap, name: &str) -> Option<u64> {
    headers
        .get(name)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
}

fn reset_epoch(headers: &HeaderMap) -> Option<u64> {
    header_u64(headers, "x-rate-limit-reset")
}

fn retry_after_epoch(headers: &HeaderMap) -> Option<u64> {
    header_u64(headers, "retry-after").map(|secs| now_epoch_s().saturating_add(secs))
}

fn now_epoch_s() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rate_limit(headers: &[(&str, &str)]) -> Option<RateLimit> {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            map.insert(
                reqwest::header::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                reqwest::header::HeaderValue::from_str(v).unwrap(),
            );
        }
        parse_rate_limit(&map)
    }

    #[test]
    fn parse_target_distinguishes_broadcast_and_account() {
        // 配信 URL / 生 ID は従来どおり Broadcast。
        assert_eq!(
            parse_x_target("https://x.com/i/broadcasts/1yoJMWvbtbtxQ"),
            XTarget::Broadcast("1yoJMWvbtbtxQ".to_string())
        );
        assert_eq!(
            parse_x_target("https://twitter.com/i/broadcasts/1yoJMWvbtbtxQ?s=20"),
            XTarget::Broadcast("1yoJMWvbtbtxQ".to_string())
        );
        assert_eq!(
            parse_x_target("  1yoJMWvbtbtxQ  "),
            XTarget::Broadcast("1yoJMWvbtbtxQ".to_string())
        );
        // 裸の文字列は後方互換のため配信 ID 扱い(設計 §1 からの意図的な逸脱)。
        assert_eq!(
            parse_x_target("ndtv"),
            XTarget::Broadcast("ndtv".to_string())
        );

        // @handle / URL 形は Account。表記は保つ。
        assert_eq!(
            parse_x_target("@Ndtv"),
            XTarget::Account("Ndtv".to_string())
        );
        assert_eq!(
            parse_x_target("https://x.com/ndtv"),
            XTarget::Account("ndtv".to_string())
        );
        assert_eq!(
            parse_x_target("https://twitter.com/ndtv/"),
            XTarget::Account("ndtv".to_string())
        );
        assert_eq!(
            parse_x_target("https://x.com/ndtv/live?ref=abc"),
            XTarget::Account("ndtv".to_string())
        );
        assert_eq!(
            parse_x_target("https://www.x.com/some_one"),
            XTarget::Account("some_one".to_string())
        );
        assert_eq!(
            parse_x_target("x.com/ndtv"),
            XTarget::Account("ndtv".to_string())
        );

        // 予約パスは Account と見なさない。
        for url in [
            "https://x.com/home",
            "https://x.com/search?q=x",
            "https://x.com/explore",
            "https://x.com/settings",
            "https://x.com/messages",
            "https://x.com/notifications",
            "https://x.com/i/flow/login",
        ] {
            assert!(
                matches!(parse_x_target(url), XTarget::Broadcast(_)),
                "{url} が Account 扱いになった"
            );
        }

        // 不正 handle(16 文字・記号)も Account にしない。
        assert!(matches!(
            parse_x_target("https://x.com/sixteen_chars_xx"),
            XTarget::Broadcast(_)
        ));
        assert!(matches!(
            parse_x_target("https://x.com/nd-tv"),
            XTarget::Broadcast(_)
        ));
        assert!(matches!(
            parse_x_target("@bad handle"),
            XTarget::Broadcast(_)
        ));
    }

    #[test]
    fn parse_user_id_accepts_new_and_old_shapes() {
        // 実測の新形(fixture の ID は架空値)。
        let v: Value = serde_json::from_str(
            r#"{"data":{"user_result_by_screen_name":{"result":{"rest_id":"1234567890","core":{"screen_name":"example"}}}}}"#,
        )
        .unwrap();
        assert_eq!(parse_user_id(&v).as_deref(), Some("1234567890"));

        // 旧形。
        let v: Value =
            serde_json::from_str(r#"{"data":{"user":{"result":{"rest_id":"42"}}}}"#).unwrap();
        assert_eq!(parse_user_id(&v).as_deref(), Some("42"));

        // 数値 rest_id も受ける。
        let v: Value =
            serde_json::from_str(r#"{"data":{"user":{"result":{"rest_id":42}}}}"#).unwrap();
        assert_eq!(parse_user_id(&v).as_deref(), Some("42"));

        // 欠落・型違い・空文字は None。
        for raw in [
            r#"{}"#,
            r#"{"data":{"user_result_by_screen_name":{"result":{}}}}"#,
            r#"{"data":{"user":{"result":{"rest_id":null}}}}"#,
            r#"{"data":{"user":{"result":{"rest_id":""}}}}"#,
            r#"{"data":{"user":{"result":{"rest_id":{}}}}}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(parse_user_id(&v).is_none(), "raw={raw}");
        }
    }

    #[test]
    fn parse_avatar_reads_running_livevideo_only() {
        // 実測形: users[uid].spaces.live_content.livevideo(架空 ID)。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"id":"2abcDEFghiJKL","state":"RUNNING","twitter_id":"111"}}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_avatar_livevideo(&v, "111"),
            LiveProbe::Live("2abcDEFghiJKL".to_string())
        );

        // state は大小無視。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"id":"2abc","state":"running"}}}}}}"#,
        )
        .unwrap();
        assert_eq!(
            parse_avatar_livevideo(&v, "111"),
            LiveProbe::Live("2abc".to_string())
        );

        // ENDED は配信なし。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"id":"2abc","state":"ENDED"}}}}}}"#,
        )
        .unwrap();
        assert_eq!(parse_avatar_livevideo(&v, "111"), LiveProbe::NotLive);

        // users に uid のキーが無い(実測: sen) → 未検出。
        let v: Value = serde_json::from_str(r#"{"users":{"222":{}}}"#).unwrap();
        assert_eq!(parse_avatar_livevideo(&v, "111"), LiveProbe::NotLive);

        // users 自体が無い/null、errors 付き 200 は「配信なし」にしない。
        for raw in [r#"{}"#, r#"{"users":null}"#, r#"{"users":{},"errors":[{"code":1}]}"#] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(
                matches!(parse_avatar_livevideo(&v, "111"), LiveProbe::Unknown(_)),
                "raw={raw}"
            );
        }

        // livevideo が無く audiospace だけ → 配信なし。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"audiospace":{"id":"space1","state":"RUNNING"}}}}}}"#,
        )
        .unwrap();
        assert_eq!(parse_avatar_livevideo(&v, "111"), LiveProbe::NotLive);

        // livevideo が null → 配信なし。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":null}}}}}"#,
        )
        .unwrap();
        assert_eq!(parse_avatar_livevideo(&v, "111"), LiveProbe::NotLive);

        // 型違いは Unknown(配信なしと区別する)。
        let v: Value = serde_json::from_str(
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":"RUNNING"}}}}}"#,
        )
        .unwrap();
        assert!(matches!(
            parse_avatar_livevideo(&v, "111"),
            LiveProbe::Unknown(_)
        ));

        let v: Value = serde_json::from_str(r#"{"users":[]}"#).unwrap();
        assert!(matches!(
            parse_avatar_livevideo(&v, "111"),
            LiveProbe::Unknown(_)
        ));

        // RUNNING なのに id が無い/空 → Unknown。
        for raw in [
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"state":"RUNNING"}}}}}}"#,
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"id":"","state":"RUNNING"}}}}}}"#,
            r#"{"users":{"111":{"spaces":{"live_content":{"livevideo":{"id":"x"}}}}}}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(
                matches!(parse_avatar_livevideo(&v, "111"), LiveProbe::Unknown(_)),
                "raw={raw}"
            );
        }
    }

    #[test]
    fn running_owned_requires_running_and_owner_match() {
        assert!(is_running_owned("RUNNING", Some("111"), "111"));
        // state は大小無視。
        assert!(is_running_owned("running", Some("111"), "111"));
        // 所有者不一致(実測: 引用/リポストの別人枠)。
        assert!(!is_running_owned("RUNNING", Some("999"), "111"));
        // ENDED は接続しない。
        assert!(!is_running_owned("ENDED", Some("111"), "111"));
        // 所有者欠落。
        assert!(!is_running_owned("RUNNING", None, "111"));
    }

    #[test]
    fn user_not_found_and_auth_errors_are_classified() {
        for raw in [
            r#"{"data":{"user_result_by_screen_name":{"result":{}}}}"#,
            r#"{"data":{"user_result_by_screen_name":{}}}"#,
            r#"{"data":{"user_result_by_screen_name":{"result":{"__typename":"UserUnavailable","reason":"Suspended"}}}}"#,
            r#"{"data":{"user":{"result":null}}}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(parse_user_id(&v).is_none() && user_not_found(&v), "raw={raw}");
        }
        // 存在するアカウント / 曖昧な応答は不在扱いにしない。
        for raw in [
            r#"{"data":{"user_result_by_screen_name":{"result":{"rest_id":"1"}}}}"#,
            r#"{"errors":[{"code":0}]}"#,
            r#"{}"#,
            r#"{"data":{}}"#,
        ] {
            let v: Value = serde_json::from_str(raw).unwrap();
            assert!(!user_not_found(&v), "raw={raw}");
        }
        for code in [32, 89, 239] {
            let v: Value = serde_json::from_str(&format!(r#"{{"errors":[{{"code":{code}}}]}}"#)).unwrap();
            assert!(has_auth_error(&v), "code={code}");
        }
        let v: Value = serde_json::from_str(r#"{"errors":[{"code":88}]}"#).unwrap();
        assert!(!has_auth_error(&v));
        assert!(!has_auth_error(&serde_json::json!({})));
    }

    #[test]
    fn unknown_streak_escalates_after_threshold() {
        assert_eq!(escalate_unknown(1), (WaitCondition::Normal, 0));
        assert_eq!(escalate_unknown(UNKNOWN_ESCALATE_AFTER - 1), (WaitCondition::Normal, 0));
        assert_eq!(
            escalate_unknown(UNKNOWN_ESCALATE_AFTER),
            (WaitCondition::RateLimited { until: None }, 1)
        );
        let (cond, attempt) = escalate_unknown(100);
        let base = Duration::from_secs(30);
        assert_eq!(next_wait(cond, base, 0, attempt, Duration::ZERO), BACKOFF_CAP);
    }

    #[test]
    fn waits_are_bounded_and_do_not_overflow() {
        assert_eq!(poll_interval(Some(u64::MAX)), Duration::from_secs(MAX_POLL_SECS));
        let base = poll_interval(Some(u64::MAX));
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: Some(u64::MAX) }, base, 0, 0, Duration::ZERO),
            MAX_RESET_WAIT
        );
        // reset が 30 分先ならそのまま待つ(BACKOFF_CAP で切らない)。
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: Some(1_800) }, base, 0, 0, Duration::ZERO),
            Duration::from_secs(1_800)
        );
        // reset が過去なら基本間隔。
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: Some(50) }, base, 100, 0, Duration::ZERO),
            base
        );
        assert_eq!(
            next_wait(WaitCondition::NotFound, base, 0, 0, Duration::ZERO),
            NOT_FOUND_RETRY
        );
        assert_eq!(
            next_wait(WaitCondition::Normal, Duration::MAX, 0, 0, Duration::from_secs(3)),
            Duration::MAX
        );
    }

    #[test]
    fn parse_avatar_reads_trimmed_real_shape() {
        // 実測形を縮約(ID は架空値。image_url / JWT 系フィールドは除去済み)。
        let v: Value = serde_json::from_str(
            r#"{"users":{
                "1001":{"spaces":{"live_content":{"livevideo":{"id":"2AaAaAaAaAaAaA","is_locked":false,"n_total_watching":703,"start":"2026-01-01T00:00:00.000000000Z","state":"RUNNING","status":"SAMPLE LIVE","twitter_id":"1001"}},"participants":[1001]}},
                "1002":{"spaces":{"live_content":{"livevideo":{"id":"2BbBbBbBbBbBbB","is_locked":false,"language":"en_US","n_total_watching":54,"start":"2026-01-01T00:00:00.000000000Z","state":"RUNNING","status":"OTHER","twitter_id":"1002"}},"participants":[1002]}}
            },"refresh_delay_secs":30}"#,
        )
        .unwrap();
        assert_eq!(parse_avatar_livevideo(&v, "1001"), LiveProbe::Live("2AaAaAaAaAaAaA".to_string()));
        assert_eq!(parse_avatar_livevideo(&v, "1002"), LiveProbe::Live("2BbBbBbBbBbBbB".to_string()));
        assert_eq!(parse_avatar_livevideo(&v, "1003"), LiveProbe::NotLive);
    }

    #[test]
    fn parse_target_url_edge_cases() {
        let acct = |s: &str| XTarget::Account(s.to_string());
        // 先頭パスだけ見る: 投稿 URL でもアカウントとして扱う(仕様)。
        assert_eq!(parse_x_target("https://x.com/ndtv/status/123"), acct("ndtv"));
        assert_eq!(parse_x_target("https://mobile.x.com/ndtv"), acct("ndtv"));
        assert_eq!(parse_x_target("https://twitter.com/x/live"), acct("x"));
        assert_eq!(parse_x_target("HTTPS://x.com/ndtv"), acct("ndtv"));
        assert_eq!(parse_x_target("Http://WWW.X.com/ndtv"), acct("ndtv"));
        // 16 文字の handle は不正 → アカウントにならない。
        assert!(matches!(parse_x_target("@abcdefghijklmnop"), XTarget::Broadcast(_)));
        assert!(matches!(parse_x_target("https://x.com/abcdefghijklmnop"), XTarget::Broadcast(_)));
        for path in ["compose", "intent", "hashtag", "share", "home", "search"] {
            assert!(
                matches!(parse_x_target(&format!("https://x.com/{path}/x")), XTarget::Broadcast(_)),
                "path={path}"
            );
        }
        // `@live` の直接入力は予約パスの影響を受けない。
        assert_eq!(parse_x_target("@live"), acct("live"));
    }

    #[test]
    fn parse_rate_limit_requires_both_numeric_headers() {
        assert_eq!(
            rate_limit(&[
                ("x-rate-limit-remaining", "149"),
                ("x-rate-limit-reset", "1794490411"),
            ]),
            Some(RateLimit {
                remaining: 149,
                reset_epoch_s: 1794490411,
            })
        );
        // 大文字小文字は HeaderMap が吸収する。
        assert_eq!(
            rate_limit(&[
                ("X-Rate-Limit-Remaining", "0"),
                ("X-Rate-Limit-Reset", "1794490411"),
            ])
            .map(|r| r.remaining),
            Some(0)
        );
        // 欠落。
        assert_eq!(rate_limit(&[]), None);
        assert_eq!(rate_limit(&[("x-rate-limit-remaining", "1")]), None);
        assert_eq!(rate_limit(&[("x-rate-limit-reset", "2")]), None);
        // 非数値。
        assert_eq!(
            rate_limit(&[
                ("x-rate-limit-remaining", "many"),
                ("x-rate-limit-reset", "1794490411"),
            ]),
            None
        );
        assert_eq!(
            rate_limit(&[
                ("x-rate-limit-remaining", "1"),
                ("x-rate-limit-reset", "soon"),
            ]),
            None
        );
    }

    #[test]
    fn poll_interval_defaults_and_clamps() {
        assert_eq!(poll_interval(None), Duration::from_secs(30));
        assert_eq!(poll_interval(Some(60)), Duration::from_secs(60));
        assert_eq!(poll_interval(Some(15)), Duration::from_secs(15));
        assert_eq!(poll_interval(Some(10)), Duration::from_secs(15));
        assert_eq!(poll_interval(Some(0)), Duration::from_secs(15));
    }

    #[test]
    fn next_wait_handles_rate_limit_auth_and_backoff() {
        let base = Duration::from_secs(30);
        let zero = Duration::ZERO;

        assert_eq!(
            next_wait(WaitCondition::Normal, base, 100, 0, zero),
            base
        );
        assert_eq!(
            next_wait(
                WaitCondition::Normal,
                base,
                100,
                0,
                Duration::from_millis(500)
            ),
            Duration::from_millis(30_500)
        );
        // reset まで待つ。
        assert_eq!(
            next_wait(
                WaitCondition::RateLimited {
                    until: Some(1_000)
                },
                base,
                900,
                0,
                zero
            ),
            Duration::from_secs(100)
        );
        // reset が過去なら基本間隔に戻す(ビジーループ防止)。
        assert_eq!(
            next_wait(
                WaitCondition::RateLimited {
                    until: Some(800)
                },
                base,
                900,
                0,
                zero
            ),
            base
        );
        // ヘッダ無し 429 は指数バックオフ、上限 15 分。
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: None }, base, 0, 0, zero),
            base
        );
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: None }, base, 0, 2, zero),
            Duration::from_secs(120)
        );
        assert_eq!(
            next_wait(WaitCondition::RateLimited { until: None }, base, 0, 10, zero),
            BACKOFF_CAP
        );
        // cookie 失効は 5 分。
        assert_eq!(
            next_wait(WaitCondition::AuthFailed, base, 0, 0, zero),
            AUTH_RETRY
        );
        // DetectOutcome / DetectError からの変換。
        assert_eq!(
            DetectOutcome::RateLimited { until: Some(5) }.wait_condition(),
            WaitCondition::RateLimited { until: Some(5) }
        );
        assert_eq!(
            DetectOutcome::NotLive.wait_condition(),
            WaitCondition::Normal
        );
        assert_eq!(
            DetectError::AuthFailed.wait_condition(),
            WaitCondition::AuthFailed
        );
    }

    #[test]
    fn detector_debug_redacts_cookie() {
        let auth = XAuth {
            auth_token: "secret-auth-token".to_string(),
            csrf_token: "secret-csrf".to_string(),
        };
        let d = LiveDetector::new(reqwest::Client::new(), XOverrides::default(), auth);
        let text = format!("{d:?}");
        assert!(!text.contains("secret-auth-token"));
        assert!(!text.contains("secret-csrf"));
        assert!(text.contains("<redacted>"));
    }
}
