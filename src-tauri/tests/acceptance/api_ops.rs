//! PR3 受け入れテスト: アプリ内操作(テスト注入・TTS・タイマー・流量制限)(設計書 §1.1 / §1.2 / §1.5 / §5.5)。
//! 実装を見ずに、設計書 §5.5 と付録 D の公開面だけから書く。実装者は編集しない。
//!
//! # 実装者が満たす契約(付録 D に無い所はこのテストが決めた最小の名前)
//!
//! 付録 D どおり(名前と形):
//! - `api::ApiDeps::with_ops(self, Arc<dyn AppOps>) -> Self`
//! - `api::AppOps`(`Send + Sync + 'static`): `inject_test_comments(&TestCommentRequest) -> Result<Vec<String>, OpsError>` /
//!   `tts_state() -> TtsQueueStatePayload` / `tts_set_paused(bool)` / `tts_skip()` / `tts_clear()` /
//!   `timer_state() -> TimerSnapshot` / `timer_control(TimerAction, Option<u32>) -> Result<TimerSnapshot, OpsError>`
//! - `api::TestCommentRequest { pub platform, name, text, kind: String, amount: Option<f64>, count: u32 }`
//! - `api::TimerAction { Start, Pause, Resume, Reset }`、`api::OpsError(pub String)`
//! - `inject::build_test_messages(&api::TestCommentRequest) -> Result<Vec<ChatMessage>, String>`
//! - `stats::next_timer_snapshot(&TimerSnapshot, TimerAction, Option<u32>, default_duration_sec: u32, mode: &str, now_ms: u64) -> TimerSnapshot`
//!
//! このテストが追加で置く前提(テスト側が決めた最小の取り決め):
//! 1. `fast_comment_lib::inject` は `pub mod`、`TestCommentRequest` / `TimerAction` / `OpsError` / `AppOps` は `api` から再公開される。
//! 2. `TestCommentRequest` / `TimerAction` / `OpsError` に特別な derive は要求しない(テストの偽物は欄を自前の型へ写して記録する)。
//!    `TimerSnapshot` は `PartialEq` を持たないので、テストは欄ごとに比べる。
//! 3. ハンドラは検証済みの要求だけを `AppOps` へ渡す。既定値(`kind` = "normal"、`count` = 1)は API 層で埋めて渡す。
//!    `name` / `text` の省略は空文字のまま渡す(既定文言は `inject::build_test_messages` が付ける)。
//! 4. 変更系のレート制限は `tokio::time::Instant`(= `start_paused` で進められる時計)で測る。
//!    バケット容量 20・毎秒 10 補充・全変更系 POST で共有。429 は `error.code = "rate_limited"` と整数秒の `Retry-After`(1 以上)。
//!    認証・検査に通った要求だけが 1 トークンを使う前提で書く(401 や 400 がトークンを消費するかはテストしない)。
//! 5. `POST /tts/*`・`POST /timer/*` の成功応答は、操作後の `AppOps::tts_state()` / `timer_state()` ではなく
//!    それぞれ `tts_state()` の形・`timer_control` が返したスナップショットの形をそのまま JSON にしたもの。
//! 6. `POST /test-comments` の応答は 202 `{"injected": <ids の件数>, "ids": <AppOps が返した ID そのまま>}`。
//! 7. `AppOps` が `Err` を返した時は 500 `error.code = "internal"`。`message` に `OpsError` の中身を出さない。
//! 8. 注入の検証は API 層(`AppOps` を呼ぶ前)で行い、範囲外は丸めず 400 `invalid_request`。
//!    `platform` / `kind` は大文字小文字を区別する(設計書 §1.5 の綴りのみ)。
//! 9. `ApiDeps::with_ops` を呼ばないルータでの操作系の応答は、このファイルではテストしない(PR1/PR2 のテストは `with_ops` 無しで通る)。

use crate::common::*;
use axum::http::{Method, StatusCode};
use fast_comment_lib::api::{self, ApiDeps, AppOps, OpsError, TestCommentRequest, TimerAction};
use fast_comment_lib::inject;
use fast_comment_lib::model::{Fragment, MessageKind, Platform};
use fast_comment_lib::stats::{self, TimerSnapshot};
use fast_comment_lib::tts::{TtsQueueItem, TtsQueueStatePayload};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

// ---- 偽の AppOps ----

/// 偽が受けた変更系の呼び出し(読み取りは記録しない)。
#[derive(Debug, Clone, PartialEq)]
enum Call {
    Inject { platform: String, name: String, text: String, kind: String, amount: Option<f64>, count: u32 },
    TtsSetPaused(bool),
    TtsSkip,
    TtsClear,
    Timer { action: &'static str, duration_sec: Option<u32> },
}

struct FakeOps {
    calls: Mutex<Vec<Call>>,
    paused: Mutex<bool>,
    queue: Mutex<Vec<(String, String)>>,
    timer: Mutex<TimerSnapshot>,
    fail_with: Mutex<Option<String>>,
}

impl FakeOps {
    fn new() -> Arc<Self> {
        Arc::new(FakeOps {
            calls: Mutex::new(vec![]),
            paused: Mutex::new(false),
            queue: Mutex::new(vec![]),
            timer: Mutex::new(TimerSnapshot::default()),
            fail_with: Mutex::new(None),
        })
    }
    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }
    fn clear_calls(&self) {
        self.calls.lock().unwrap().clear();
    }
    fn set_queue(&self, items: &[(&str, &str)]) {
        *self.queue.lock().unwrap() =
            items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
    }
    fn set_timer(&self, t: TimerSnapshot) {
        *self.timer.lock().unwrap() = t;
    }
    fn fail_next_calls_with(&self, detail: &str) {
        *self.fail_with.lock().unwrap() = Some(detail.to_string());
    }
    fn record(&self, c: Call) {
        self.calls.lock().unwrap().push(c);
    }
}

fn action_name(a: &TimerAction) -> &'static str {
    match a {
        TimerAction::Start => "start",
        TimerAction::Pause => "pause",
        TimerAction::Resume => "resume",
        TimerAction::Reset => "reset",
    }
}

impl AppOps for FakeOps {
    fn inject_test_comments(&self, req: &TestCommentRequest) -> Result<Vec<String>, OpsError> {
        self.record(Call::Inject {
            platform: req.platform.clone(),
            name: req.name.clone(),
            text: req.text.clone(),
            kind: req.kind.clone(),
            amount: req.amount,
            count: req.count,
        });
        if let Some(d) = self.fail_with.lock().unwrap().clone() {
            return Err(OpsError(d));
        }
        Ok((0..req.count).map(|i| format!("injected-test:fake-{i}")).collect())
    }
    fn tts_state(&self) -> TtsQueueStatePayload {
        let items: Vec<TtsQueueItem> = self
            .queue
            .lock()
            .unwrap()
            .iter()
            .map(|(id, preview)| TtsQueueItem { id: id.clone(), preview: preview.clone() })
            .collect();
        TtsQueueStatePayload {
            depth: items.len() as u32,
            paused: *self.paused.lock().unwrap(),
            items,
        }
    }
    fn tts_set_paused(&self, paused: bool) {
        self.record(Call::TtsSetPaused(paused));
        *self.paused.lock().unwrap() = paused;
    }
    fn tts_skip(&self) {
        self.record(Call::TtsSkip);
    }
    fn tts_clear(&self) {
        self.record(Call::TtsClear);
        self.queue.lock().unwrap().clear();
    }
    fn timer_state(&self) -> TimerSnapshot {
        self.timer.lock().unwrap().clone()
    }
    fn timer_control(
        &self,
        action: TimerAction,
        duration_sec: Option<u32>,
    ) -> Result<TimerSnapshot, OpsError> {
        self.record(Call::Timer { action: action_name(&action), duration_sec });
        if let Some(d) = self.fail_with.lock().unwrap().clone() {
            return Err(OpsError(d));
        }
        let mut t = self.timer.lock().unwrap().clone();
        t.state = match action {
            TimerAction::Start | TimerAction::Resume => "running",
            TimerAction::Pause => "paused",
            TimerAction::Reset => "idle",
        }
        .to_string();
        if let Some(d) = duration_sec {
            t.duration_sec = d;
        }
        *self.timer.lock().unwrap() = t.clone();
        Ok(t)
    }
}

// ---- 環境 ----

struct OpsEnv {
    app: axum::Router,
    ops: Arc<FakeOps>,
    token: String,
}

fn ops_env() -> OpsEnv {
    let (gate, token) = open_gate();
    let ops = FakeOps::new();
    let deps = ApiDeps::new(gate, CancellationToken::new()).with_ops(ops.clone());
    OpsEnv { app: api::router(deps, PORT), ops, token }
}

impl OpsEnv {
    async fn get(&self, path: &str) -> Resp {
        send(&self.app, get(path).bearer(&self.token)).await
    }
    async fn post(&self, path: &str, body: Option<&str>) -> Resp {
        let mut r = req(Method::POST, path).bearer(&self.token);
        if let Some(b) = body {
            r = r.json_body(b);
        }
        send(&self.app, r).await
    }
    async fn inject(&self, body: &str) -> Resp {
        self.post(TEST_COMMENTS, Some(body)).await
    }
}

const TEST_COMMENTS: &str = "/api/v1/test-comments";
const TTS: &str = "/api/v1/tts";
const TIMER: &str = "/api/v1/timer";

fn keys(v: &Value) -> BTreeSet<String> {
    v.as_object().expect("オブジェクト").keys().cloned().collect()
}

fn set(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|s| s.to_string()).collect()
}

fn injected(platform: &str) -> Call {
    Call::Inject {
        platform: platform.to_string(),
        name: String::new(),
        text: String::new(),
        kind: "normal".to_string(),
        amount: None,
        count: 1,
    }
}

fn snapshot(state: &str, mode: &str, duration: u32, base: u32, since: u64, updated: u64) -> TimerSnapshot {
    TimerSnapshot {
        state: state.to_string(),
        mode: mode.to_string(),
        duration_sec: duration,
        base_elapsed_sec: base,
        running_since_ms: since,
        updated_at: updated,
    }
}

fn request(platform: &str, name: &str, text: &str, kind: &str, amount: Option<f64>, count: u32) -> TestCommentRequest {
    TestCommentRequest {
        platform: platform.to_string(),
        name: name.to_string(),
        text: text.to_string(),
        kind: kind.to_string(),
        amount,
        count,
    }
}

// ================= 注入(POST /test-comments) =================

#[tokio::test]
async fn test_comments_valid_request_is_202_with_ids_and_calls_ops_once() {
    let env = ops_env();
    let r = env
        .inject(r#"{"platform":"youtube","name":"Alice","text":"こんにちは","kind":"superChat","amount":500,"count":3}"#)
        .await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", String::from_utf8_lossy(&r.body));
    let j = r.json();
    assert_eq!(j["injected"], json!(3));
    assert_eq!(
        j["ids"],
        json!(["injected-test:fake-0", "injected-test:fake-1", "injected-test:fake-2"])
    );
    assert_eq!(
        env.ops.calls(),
        vec![Call::Inject {
            platform: "youtube".to_string(),
            name: "Alice".to_string(),
            text: "こんにちは".to_string(),
            kind: "superChat".to_string(),
            amount: Some(500.0),
            count: 3,
        }]
    );
}

#[tokio::test]
async fn test_comments_defaults_are_kind_normal_and_count_1() {
    let env = ops_env();
    let r = env.inject(r#"{"platform":"twitch"}"#).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json()["injected"], json!(1));
    assert_eq!(env.ops.calls(), vec![injected("twitch")]);
}

#[tokio::test]
async fn test_comments_missing_or_unknown_platform_is_400() {
    let env = ops_env();
    for body in [r#"{}"#, r#"{"name":"a"}"#, r#"{"platform":"bogus"}"#, r#"{"platform":"Twitch"}"#, r#"{"platform":""}"#] {
        let r = env.inject(body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {body}");
        assert_eq!(r.error_code(), "invalid_request", "body: {body}");
    }
    for p in ["twitch", "youtube", "x", "niconico"] {
        let r = env.inject(&format!(r#"{{"platform":"{p}"}}"#)).await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "platform: {p}");
    }
}

#[tokio::test]
async fn test_comments_unknown_kind_is_400() {
    let env = ops_env();
    for kind in ["bogus", "SuperChat", "superchat", ""] {
        let r = env.inject(&format!(r#"{{"platform":"twitch","kind":"{kind}"}}"#)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "kind: {kind:?}");
        assert_eq!(r.error_code(), "invalid_request");
    }
    for kind in ["normal", "superChat", "membership", "bits", "gift"] {
        let r = env.inject(&format!(r#"{{"platform":"twitch","kind":"{kind}"}}"#)).await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "kind: {kind}");
    }
}

#[tokio::test]
async fn test_comments_count_0_and_21_are_400_and_20_is_ok() {
    let env = ops_env();
    for count in ["0", "21", "-1"] {
        let r = env.inject(&format!(r#"{{"platform":"twitch","count":{count}}}"#)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "count: {count}");
        assert_eq!(r.error_code(), "invalid_request");
    }
    let r = env.inject(r#"{"platform":"twitch","count":20}"#).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    assert_eq!(r.json()["injected"], json!(20));
}

#[tokio::test]
async fn test_comments_name_limit_counts_chars_not_bytes() {
    let env = ops_env();
    let ok = "あ".repeat(64);
    let r = env.inject(&json!({"platform":"twitch","name":ok}).to_string()).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "日本語 64 文字は通る");
    let too_long = "あ".repeat(65);
    let r = env.inject(&json!({"platform":"twitch","name":too_long}).to_string()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_code(), "invalid_request");
    assert!(
        r.json()["error"]["message"].as_str().unwrap_or_default().contains("name"),
        "message に欄の名前: {:?}",
        r.json()
    );
}

#[tokio::test]
async fn test_comments_text_501_chars_is_400() {
    let env = ops_env();
    let ok = "字".repeat(500);
    let r = env.inject(&json!({"platform":"twitch","text":ok}).to_string()).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "500 文字は通る");
    let too_long = "字".repeat(501);
    let r = env.inject(&json!({"platform":"twitch","text":too_long}).to_string()).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_code(), "invalid_request");
    assert!(
        r.json()["error"]["message"].as_str().unwrap_or_default().contains("text"),
        "message に欄の名前: {:?}",
        r.json()
    );
}

#[tokio::test]
async fn test_comments_amount_negative_or_over_1e9_is_400() {
    let env = ops_env();
    for amount in ["-1", "-0.01", "1000000001", "1e10"] {
        let r = env
            .inject(&format!(r#"{{"platform":"youtube","kind":"superChat","amount":{amount}}}"#))
            .await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "amount: {amount}");
        assert_eq!(r.error_code(), "invalid_request");
    }
    for amount in ["0", "1000000000"] {
        let r = env
            .inject(&format!(r#"{{"platform":"youtube","kind":"superChat","amount":{amount}}}"#))
            .await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "境界は通る: {amount}");
    }
}

#[tokio::test]
async fn test_comments_amount_is_ignored_for_normal_kind() {
    // API は通す。実際のコメントでは金額が付かない(既存どおり)。
    let env = ops_env();
    let r = env.inject(r#"{"platform":"twitch","kind":"normal","amount":500}"#).await;
    assert_eq!(r.status, StatusCode::ACCEPTED);
    let msgs = inject::build_test_messages(&request("twitch", "", "", "normal", Some(500.0), 1)).unwrap();
    assert!(msgs.iter().all(|m| m.amount.is_none()), "normal の金額は無視");
    let msgs = inject::build_test_messages(&request("twitch", "", "", "membership", Some(500.0), 1)).unwrap();
    assert!(msgs.iter().all(|m| m.amount.is_none()), "membership の金額も無視");
}

#[tokio::test]
async fn test_comments_unknown_json_key_is_400() {
    let env = ops_env();
    let r = env.inject(r#"{"platform":"twitch","bogus":1}"#).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_code(), "invalid_request");
}

#[tokio::test]
async fn test_comments_non_json_body_is_400_and_text_plain_is_415() {
    let env = ops_env();
    let r = env.inject("this is not json").await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    assert_eq!(r.error_code(), "invalid_request");
    let r = send(
        &env.app,
        req(Method::POST, TEST_COMMENTS)
            .bearer(&env.token)
            .json_body(r#"{"platform":"twitch"}"#)
            .header("content-type", "text/plain"),
    )
    .await;
    assert_eq!(r.status, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    assert_eq!(r.error_code(), "unsupported_media_type");
}

#[tokio::test]
async fn test_comments_body_over_16kib_is_413() {
    let env = ops_env();
    let pad = "a".repeat(16 * 1024 + 1);
    let body = format!(r#"{{"platform":"twitch","text":"{pad}"}}"#);
    assert!(body.len() > 16 * 1024);
    let r = env.inject(&body).await;
    assert_eq!(r.status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(r.error_code(), "payload_too_large");
}

#[tokio::test]
async fn invalid_request_never_calls_ops() {
    let env = ops_env();
    for body in [
        r#"{}"#,
        r#"{"platform":"bogus"}"#,
        r#"{"platform":"twitch","kind":"bogus"}"#,
        r#"{"platform":"twitch","count":0}"#,
        r#"{"platform":"twitch","count":21}"#,
        r#"{"platform":"youtube","kind":"superChat","amount":-1}"#,
        r#"{"platform":"twitch","bogus":true}"#,
        "not json",
    ] {
        let r = env.inject(body).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {body}");
    }
    for body in [r#"{"durationSec":0}"#, r#"{"durationSec":360000}"#, r#"{"bogus":1}"#, "not json"] {
        let r = env.post(&format!("{TIMER}/start"), Some(body)).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "body: {body}");
    }
    assert_eq!(env.ops.calls(), vec![], "不正な要求では AppOps を呼ばない");
}

// ================= inject::build_test_messages =================

#[test]
fn injected_messages_have_unique_ids_across_rapid_calls() {
    let mut seen: HashSet<String> = HashSet::new();
    for _ in 0..100 {
        let msgs = inject::build_test_messages(&request("twitch", "a", "b", "normal", None, 20)).unwrap();
        assert_eq!(msgs.len(), 20);
        for m in msgs {
            assert!(m.id.starts_with("injected-test:"), "id: {}", m.id);
            assert!(seen.insert(m.id.clone()), "ID が重複: {}", m.id);
        }
    }
    assert_eq!(seen.len(), 2000);
}

#[test]
fn injected_messages_carry_injected_test_flag_and_requested_fields() {
    let msgs =
        inject::build_test_messages(&request("youtube", "Bob", "hi there", "superChat", Some(500.0), 3)).unwrap();
    assert_eq!(msgs.len(), 3);
    for m in &msgs {
        assert!(m.injected_test, "injected_test フラグ");
        assert!(matches!(m.platform, Platform::Youtube));
        assert!(matches!(m.kind, MessageKind::SuperChat));
        assert_eq!(m.author.name, "Bob");
        let texts: Vec<&str> = m.text_fragments().collect();
        assert_eq!(texts, vec!["hi there"]);
        assert_eq!(m.amount.as_ref().map(|a| a.value), Some(500.0));
    }
}

#[test]
fn injected_empty_name_and_text_fall_back_to_existing_defaults() {
    for (name, text) in [("", ""), ("   ", "  ")] {
        let msgs = inject::build_test_messages(&request("twitch", name, text, "normal", None, 1)).unwrap();
        assert_eq!(msgs[0].author.name, "テストユーザー", "name: {name:?}");
        let texts: Vec<&str> = msgs[0].text_fragments().collect();
        assert_eq!(texts, vec!["テストコメントです"], "text: {text:?}");
        assert!(msgs[0].fragments.iter().all(|f| matches!(f, Fragment::Text { .. })));
    }
}

// ================= TTS =================

#[tokio::test]
async fn tts_get_returns_paused_depth_items() {
    let env = ops_env();
    env.ops.set_queue(&[("c1", "最初のコメント"), ("c2", "二つ目")]);
    let r = env.get(TTS).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(keys(&j), set(&["paused", "depth", "items"]));
    assert_eq!(
        j,
        json!({"paused": false, "depth": 2, "items": [
            {"id": "c1", "preview": "最初のコメント"},
            {"id": "c2", "preview": "二つ目"},
        ]})
    );
}

#[tokio::test]
async fn tts_pause_sets_paused_true_and_returns_state() {
    let env = ops_env();
    let r = env.post(&format!("{TTS}/pause"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json(), json!({"paused": true, "depth": 0, "items": []}));
    assert_eq!(env.ops.calls(), vec![Call::TtsSetPaused(true)]);
}

#[tokio::test]
async fn tts_resume_sets_paused_false_and_returns_state() {
    let env = ops_env();
    env.post(&format!("{TTS}/pause"), None).await;
    env.ops.clear_calls();
    let r = env.post(&format!("{TTS}/resume"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["paused"], json!(false));
    assert_eq!(env.ops.calls(), vec![Call::TtsSetPaused(false)]);
}

#[tokio::test]
async fn tts_pause_twice_is_idempotent_200() {
    let env = ops_env();
    let a = env.post(&format!("{TTS}/pause"), None).await;
    let b = env.post(&format!("{TTS}/pause"), None).await;
    assert_eq!(a.status, StatusCode::OK);
    assert_eq!(b.status, StatusCode::OK);
    assert_eq!(a.json(), b.json());
    assert_eq!(b.json()["paused"], json!(true));
}

#[tokio::test]
async fn tts_skip_calls_ops_once() {
    let env = ops_env();
    let r = env.post(&format!("{TTS}/skip"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(keys(&r.json()), set(&["paused", "depth", "items"]));
    assert_eq!(env.ops.calls(), vec![Call::TtsSkip]);
}

#[tokio::test]
async fn tts_clear_calls_ops_once() {
    let env = ops_env();
    env.ops.set_queue(&[("c1", "a")]);
    let r = env.post(&format!("{TTS}/clear"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["depth"], json!(0));
    assert_eq!(env.ops.calls(), vec![Call::TtsClear]);
}

#[tokio::test]
async fn tts_pause_with_get_is_405() {
    let env = ops_env();
    for p in ["pause", "resume", "skip", "clear"] {
        let r = env.get(&format!("{TTS}/{p}")).await;
        assert_eq!(r.status, StatusCode::METHOD_NOT_ALLOWED, "GET /tts/{p}");
        assert_eq!(r.error_code(), "method_not_allowed");
    }
    assert_eq!(env.ops.calls(), vec![]);
}

// ================= タイマー =================

#[tokio::test]
async fn timer_get_returns_snapshot_with_camel_case_keys() {
    let env = ops_env();
    env.ops.set_timer(snapshot("paused", "elapsed", 300, 42, 0, 1_731_600_000_123));
    let r = env.get(TIMER).await;
    assert_eq!(r.status, StatusCode::OK);
    let j = r.json();
    assert_eq!(
        keys(&j),
        set(&["state", "mode", "durationSec", "baseElapsedSec", "runningSinceMs", "updatedAt"])
    );
    assert_eq!(
        j,
        json!({"state":"paused","mode":"elapsed","durationSec":300,"baseElapsedSec":42,
               "runningSinceMs":0,"updatedAt":1_731_600_000_123u64})
    );
}

#[tokio::test]
async fn timer_start_without_body_passes_no_duration() {
    let env = ops_env();
    let r = env.post(&format!("{TIMER}/start"), None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["state"], json!("running"));
    assert_eq!(env.ops.calls(), vec![Call::Timer { action: "start", duration_sec: None }]);
}

#[tokio::test]
async fn timer_start_with_duration_passes_it_through() {
    let env = ops_env();
    let r = env.post(&format!("{TIMER}/start"), Some(r#"{"durationSec":300}"#)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json()["durationSec"], json!(300));
    assert_eq!(env.ops.calls(), vec![Call::Timer { action: "start", duration_sec: Some(300) }]);
    for d in ["1", "359999"] {
        let r = env.post(&format!("{TIMER}/start"), Some(&format!(r#"{{"durationSec":{d}}}"#))).await;
        assert_eq!(r.status, StatusCode::OK, "境界は通る: {d}");
    }
}

#[tokio::test]
async fn timer_start_duration_0_and_360000_are_400() {
    let env = ops_env();
    for d in ["0", "360000", "-1", "1.5", r#""300""#] {
        let r = env.post(&format!("{TIMER}/start"), Some(&format!(r#"{{"durationSec":{d}}}"#))).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "durationSec: {d}");
        assert_eq!(r.error_code(), "invalid_request");
    }
}

#[tokio::test]
async fn timer_pause_resume_reset_call_ops_with_matching_action() {
    let env = ops_env();
    for action in ["pause", "resume", "reset"] {
        let r = env.post(&format!("{TIMER}/{action}"), None).await;
        assert_eq!(r.status, StatusCode::OK, "POST /timer/{action}");
    }
    assert_eq!(
        env.ops.calls(),
        vec![
            Call::Timer { action: "pause", duration_sec: None },
            Call::Timer { action: "resume", duration_sec: None },
            Call::Timer { action: "reset", duration_sec: None },
        ]
    );
}

// ---- stats::next_timer_snapshot(純関数) ----

#[test]
fn next_timer_snapshot_start_sets_running_and_zero_elapsed() {
    let cur = snapshot("paused", "countdown", 60, 77, 0, 1);
    let next = stats::next_timer_snapshot(&cur, TimerAction::Start, Some(120), 300, "countdown", 5_000);
    assert_eq!(next.state, "running");
    assert_eq!(next.mode, "countdown");
    assert_eq!(next.duration_sec, 120);
    assert_eq!(next.base_elapsed_sec, 0);
    assert_eq!(next.running_since_ms, 5_000);
    assert_eq!(next.updated_at, 5_000);
}

#[test]
fn next_timer_snapshot_start_uses_default_duration_when_none() {
    let cur = TimerSnapshot::default();
    let next = stats::next_timer_snapshot(&cur, TimerAction::Start, None, 300, "countdown", 5_000);
    assert_eq!(next.duration_sec, 300);
    assert_eq!(next.state, "running");
}

#[test]
fn next_timer_snapshot_pause_accumulates_whole_elapsed_seconds() {
    let cur = snapshot("running", "countdown", 300, 10, 1_000_000, 1_000_000);
    let next = stats::next_timer_snapshot(&cur, TimerAction::Pause, None, 300, "countdown", 1_002_999);
    assert_eq!(next.state, "paused");
    assert_eq!(next.base_elapsed_sec, 12, "10 + 2.999 秒 → 端数は切り捨て");
    assert_eq!(next.running_since_ms, 0);
    assert_eq!(next.updated_at, 1_002_999);
    assert_eq!(next.duration_sec, 300);
}

#[test]
fn next_timer_snapshot_pause_when_not_running_keeps_elapsed() {
    for state in ["paused", "idle"] {
        let cur = snapshot(state, "countdown", 300, 5, 0, 1);
        let next = stats::next_timer_snapshot(&cur, TimerAction::Pause, None, 300, "countdown", 9_999_999);
        assert_eq!(next.state, "paused", "from {state}");
        assert_eq!(next.base_elapsed_sec, 5, "from {state}");
        assert_eq!(next.running_since_ms, 0);
    }
}

#[test]
fn next_timer_snapshot_resume_sets_running_since_now() {
    let cur = snapshot("paused", "countdown", 300, 12, 0, 1);
    let next = stats::next_timer_snapshot(&cur, TimerAction::Resume, None, 300, "countdown", 7_000);
    assert_eq!(next.state, "running");
    assert_eq!(next.running_since_ms, 7_000);
    assert_eq!(next.updated_at, 7_000);
    assert_eq!(next.base_elapsed_sec, 12);
}

#[test]
fn next_timer_snapshot_reset_returns_idle_with_zero_elapsed() {
    let cur = snapshot("running", "countdown", 300, 40, 1_000, 1_000);
    let next = stats::next_timer_snapshot(&cur, TimerAction::Reset, None, 300, "countdown", 8_000);
    assert_eq!(next.state, "idle");
    assert_eq!(next.base_elapsed_sec, 0);
    assert_eq!(next.running_since_ms, 0);
    assert_eq!(next.updated_at, 8_000);
    assert_eq!(next.duration_sec, 300, "目標秒数は保つ(既存の control_timer と同じ)");
}

#[test]
fn next_timer_snapshot_unknown_mode_falls_back_to_countdown() {
    let cur = TimerSnapshot::default();
    let next = stats::next_timer_snapshot(&cur, TimerAction::Start, None, 300, "bogus", 1_000);
    assert_eq!(next.mode, "countdown");
    let next = stats::next_timer_snapshot(&cur, TimerAction::Start, None, 300, "elapsed", 1_000);
    assert_eq!(next.mode, "elapsed", "elapsed はそのまま");
}

// ================= 流量と範囲 =================

/// 変更系を 20 回で使い切る(経路を混ぜて、バケットが全体で 1 つであることも確かめる)。
async fn drain_bucket(env: &OpsEnv) {
    for i in 0..20 {
        let r = match i % 3 {
            0 => env.post(&format!("{TTS}/pause"), None).await,
            1 => env.post(&format!("{TIMER}/reset"), None).await,
            _ => env.inject(r#"{"platform":"twitch"}"#).await,
        };
        assert!(
            r.status.is_success(),
            "バースト内 {i} 回目は通る: {} {:?}",
            r.status,
            String::from_utf8_lossy(&r.body)
        );
    }
}

#[tokio::test(start_paused = true)]
async fn mutating_requests_beyond_burst_20_get_429_with_retry_after() {
    let env = ops_env();
    drain_bucket(&env).await;
    let r = env.post(&format!("{TTS}/pause"), None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(r.error_code(), "rate_limited");
    let retry: u64 = r
        .header("retry-after")
        .expect("Retry-After が付く")
        .parse()
        .expect("Retry-After は整数秒");
    assert!(retry >= 1, "Retry-After: {retry}");
}

#[tokio::test(start_paused = true)]
async fn rate_limited_request_does_not_call_ops() {
    let env = ops_env();
    drain_bucket(&env).await;
    env.ops.clear_calls();
    for (path, body) in [
        (format!("{TTS}/skip"), None),
        (format!("{TTS}/clear"), None),
        (format!("{TIMER}/start"), Some(r#"{"durationSec":60}"#)),
        (TEST_COMMENTS.to_string(), Some(r#"{"platform":"twitch"}"#)),
    ] {
        let r = env.post(&path, body).await;
        assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "{path}");
    }
    assert_eq!(env.ops.calls(), vec![], "429 の要求は実行しない");
}

#[tokio::test(start_paused = true)]
async fn rate_limit_refills_10_per_second() {
    let env = ops_env();
    drain_bucket(&env).await;
    assert_eq!(env.post(&format!("{TTS}/pause"), None).await.status, StatusCode::TOO_MANY_REQUESTS);
    tokio::time::advance(Duration::from_secs(1)).await;
    for i in 0..10 {
        let r = env.post(&format!("{TTS}/pause"), None).await;
        assert_eq!(r.status, StatusCode::OK, "1 秒後の {i} 回目は通る");
    }
    let r = env.post(&format!("{TTS}/pause"), None).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS, "補充は 1 秒に 10 個まで");
}

#[tokio::test(start_paused = true)]
async fn get_requests_are_not_rate_limited() {
    let env = ops_env();
    drain_bucket(&env).await;
    for i in 0..60 {
        for path in ["/api/v1/status", TTS, TIMER] {
            let r = env.get(path).await;
            assert_eq!(r.status, StatusCode::OK, "GET {path} の {i} 回目");
        }
    }
}

#[tokio::test]
async fn mutating_routes_without_token_are_401_and_do_not_call_ops() {
    let env = ops_env();
    for (path, body) in [
        (TEST_COMMENTS.to_string(), Some(r#"{"platform":"twitch"}"#)),
        (format!("{TTS}/pause"), None),
        (format!("{TTS}/resume"), None),
        (format!("{TTS}/skip"), None),
        (format!("{TTS}/clear"), None),
        (format!("{TIMER}/start"), None),
        (format!("{TIMER}/pause"), None),
        (format!("{TIMER}/resume"), None),
        (format!("{TIMER}/reset"), None),
    ] {
        let mut r = req(Method::POST, &path);
        if let Some(b) = body {
            r = r.json_body(b);
        }
        let resp = send(&env.app, r).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "POST {path}");
        assert_eq!(resp.error_code(), "unauthorized");
        assert!(resp.header("www-authenticate").unwrap_or_default().contains("Bearer"));
        // 誤ったトークンも同じ。
        let mut r = req(Method::POST, &path).bearer("wrong-token");
        if let Some(b) = body {
            r = r.json_body(b);
        }
        assert_eq!(send(&env.app, r).await.status, StatusCode::UNAUTHORIZED, "wrong token: {path}");
    }
    for path in [TTS, TIMER] {
        let resp = send(&env.app, get(path)).await;
        assert_eq!(resp.status, StatusCode::UNAUTHORIZED, "GET {path}");
    }
    assert_eq!(env.ops.calls(), vec![], "認証に通らない要求では AppOps を呼ばない");
}

#[tokio::test]
async fn ops_error_maps_to_500_internal_without_detail() {
    let env = ops_env();
    env.ops.fail_next_calls_with("SECRET-DETAIL-xyz /home/user/path");
    let r = env.inject(r#"{"platform":"twitch"}"#).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r.error_code(), "internal");
    let r2 = env.post(&format!("{TIMER}/start"), None).await;
    assert_eq!(r2.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(r2.error_code(), "internal");
    for resp in [&r, &r2] {
        let text = String::from_utf8_lossy(&resp.body).into_owned();
        assert!(!text.contains("SECRET-DETAIL"), "内部の詳細を返さない: {text}");
        assert!(!text.contains("/home/user"), "内部の詳細を返さない: {text}");
    }
}

#[tokio::test]
async fn excluded_routes_do_not_exist() {
    let env = ops_env();
    for path in [
        "/api/v1/config",
        "/api/v1/webhooks",
        "/api/v1/token",
        "/api/v1/chat/send",
        "/api/v1/moderation/ban",
        "/api/v1/tts/speak",
    ] {
        let g = env.get(path).await;
        assert_eq!(g.status, StatusCode::NOT_FOUND, "GET {path}");
        assert_eq!(g.error_code(), "not_found", "GET {path}");
        let p = env.post(path, Some("{}")).await;
        assert_eq!(p.status, StatusCode::NOT_FOUND, "POST {path}");
        assert_eq!(p.error_code(), "not_found", "POST {path}");
    }
    assert_eq!(env.ops.calls(), vec![]);
}
