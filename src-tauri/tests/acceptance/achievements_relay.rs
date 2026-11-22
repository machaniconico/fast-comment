//! 実績 PR4 の受け入れテスト: 解除イベントの Webhook / SSE への配信(設計書 §2.5・決定 K12)。
//!
//! 実装者が満たす契約(このファイルが前提にする物。実装を見ずに書いた):
//! - `integrations::WebhookTriggers` に `pub achievement: bool` を足す(`#[serde(default)]`、既定は false。
//!   既存の保存ファイルはそのまま読める)。既存の受け入れテスト 2 ファイルの構造体リテラルには
//!   `achievement: false` を足してある。
//! - `webhook::Trigger::Achievement`(JSON では `"achievement"`)。
//! - `WebhookRuntime::handle_achievement(&self, unlock: &AchievementUnlocked)`(同期。`handle_message` と同じく
//!   キューへ積むだけ)。`triggers.achievement` が true の送り先だけに積む。キーワードや他のきっかけは見ない。
//! - 汎用 JSON は既存の v1 封筒(`schema`・`eventId`・`trigger:"achievement"`・`sentAtMs`)のまま、
//!   `comment` を持たず、代わりに K12 の形のオブジェクトを `achievement` キーに入れる。
//!   K12 のキーは `type`・`achievementId`・`name`・`tier`・`subject`・`value`・`threshold`・`currency`・`unit`・
//!   `windowKey`・`unlockedAtMs` のちょうど 11 個(`description`・`icon`・内部 `id` は送らない)。
//!   `currency` が Some なら金額系(`value`・`threshold` は `*_milli` をそのまま、`unit:"milli"`)、
//!   None なら回数系(`*_milli / 1000` の整数、`unit:"count"`、`currency:null`)。
//! - Discord 形式は `embeds[0].description`(または `content`)に視聴者名(視聴者の時)と実績名を入れる。
//! - `ApiFeed::push_achievement(&self, unlock: &AchievementUnlocked)`。次の tick で SSE イベント
//!   `achievement` を 1 解除につき 1 件送る。`data` は K12 のオブジェクトそのもの(`id:` 行は付けない)。
//!   フィードが無効なら何もしない。
#![allow(dead_code)]

use crate::common::*;
use crate::webhook_common::*;
use axum::Router;
use fast_comment_lib::achievements::engine::{AchievementUnlocked, UnlockSubject};
use fast_comment_lib::achievements::model::{SubjectKind, Tier};
use fast_comment_lib::api::{self, ApiDeps, ApiFeed};
use fast_comment_lib::integrations::{WebhookFormat, WebhookTriggers};
use fast_comment_lib::model::Platform;
use fast_comment_lib::webhook::Trigger;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const AT: u64 = 1_763_700_000_000;

// ---- 解除の fixture ----

fn viewer_subject() -> UnlockSubject {
    UnlockSubject {
        kind: SubjectKind::Viewer,
        platform: Some(Platform::Youtube),
        author_id: Some("UCxxx".into()),
        name: Some("Alice".into()),
    }
}

fn streamer_subject() -> UnlockSubject {
    UnlockSubject { kind: SubjectKind::Streamer, platform: None, author_id: None, name: None }
}

#[allow(clippy::too_many_arguments)]
fn unlock(
    achievement_id: &str,
    name: &str,
    tier: Tier,
    subject: UnlockSubject,
    value_milli: i64,
    threshold_milli: i64,
    currency: Option<&str>,
) -> AchievementUnlocked {
    AchievementUnlocked {
        id: format!("uuid-{achievement_id}"),
        achievement_id: achievement_id.into(),
        name: name.into(),
        description: "DESCRIPTION-NOT-SENT".into(),
        tier,
        icon: "ICON-NOT-SENT".into(),
        subject,
        value_milli,
        threshold_milli,
        currency: currency.map(|c| c.into()),
        window_key: "*".into(),
        unlocked_at_ms: AT,
    }
}

/// 金額系・視聴者(JpyConverted 相当)。K12 の例そのもの。
fn viewer_amount_jpy() -> AchievementUnlocked {
    unlock("viewer-donation-10k", "累計 1 万円", Tier::Gold, viewer_subject(), 10_500_000, 10_000_000, Some("JPY"))
}

/// 金額系・視聴者(Only("USD") 相当)。
fn viewer_amount_usd() -> AchievementUnlocked {
    unlock("viewer-donation-usd", "累計 100 ドル", Tier::Silver, viewer_subject(), 120_500, 100_000, Some("USD"))
}

/// 回数系・視聴者。
fn viewer_count() -> AchievementUnlocked {
    unlock("viewer-comments-100", "100 コメント", Tier::Bronze, viewer_subject(), 100_000, 100_000, None)
}

/// 回数系・配信者。
fn streamer_count() -> AchievementUnlocked {
    unlock("streamer-streams-10", "10 回配信", Tier::Platinum, streamer_subject(), 10_000, 10_000, None)
}

/// 金額系・配信者。
fn streamer_amount() -> AchievementUnlocked {
    unlock("streamer-donation", "配信者の累計", Tier::Gold, streamer_subject(), 3_000_000, 1_000_000, Some("JPY"))
}

const K12_KEYS: [&str; 11] = [
    "type", "achievementId", "name", "tier", "subject", "value", "threshold", "currency", "unit", "windowKey",
    "unlockedAtMs",
];

fn keys_of(v: &Value) -> Vec<String> {
    let mut k: Vec<String> = v.as_object().expect("オブジェクト").keys().cloned().collect();
    k.sort();
    k
}

fn k12_key_list() -> Vec<String> {
    let mut k: Vec<String> = K12_KEYS.iter().map(|s| s.to_string()).collect();
    k.sort();
    k
}

// ---- Webhook の環境 ----

fn ach_triggers() -> WebhookTriggers {
    WebhookTriggers { donation: false, membership: false, keyword: false, achievement: true }
}

fn other_triggers() -> WebhookTriggers {
    WebhookTriggers { donation: true, membership: true, keyword: true, achievement: false }
}

async fn generic_env() -> WhEnv {
    let env = wh_env().await;
    env.add("汎用", WebhookFormat::Generic, GENERIC_URL_A, ach_triggers()).await;
    env
}

/// 汎用 JSON の送り先に 1 件流し、届いた 1 通の本文(JSON)を返す。
async fn send_generic(u: &AchievementUnlocked) -> Value {
    let env = generic_env().await;
    env.rt.handle_achievement(u);
    settle().await;
    let calls = env.sender.calls_for("hook-a");
    assert_eq!(calls.len(), 1, "1 解除につき 1 通");
    calls[0].json()
}

fn k12_of(body: &Value) -> Value {
    body["achievement"].clone()
}

// ---- 1. 汎用 JSON の形(v1 封筒 + K12) ----

#[test]
fn trigger_achievement_serializes_as_lowercase_name() {
    assert_eq!(serde_json::to_value(Trigger::Achievement).unwrap(), json!("achievement"));
}

#[tokio::test(start_paused = true)]
async fn webhook_achievement_trigger_payload_v1_shape() {
    let body = send_generic(&viewer_amount_jpy()).await;
    assert_eq!(body["schema"], "fast-comment.webhook.v1", "スキーマは v1 のまま");
    assert_eq!(body["trigger"], "achievement");
    assert!(body["eventId"].as_str().is_some_and(|s| !s.is_empty()), "eventId がある");
    assert!(body["sentAtMs"].is_u64(), "sentAtMs は整数");
    assert!(body.get("comment").is_none(), "実績にコメントは無い");
    assert_eq!(
        k12_of(&body),
        json!({
            "type": "achievement",
            "achievementId": "viewer-donation-10k",
            "name": "累計 1 万円",
            "tier": "gold",
            "subject": { "kind": "viewer", "platform": "youtube", "authorId": "UCxxx", "name": "Alice" },
            "value": 10_500_000,
            "threshold": 10_000_000,
            "currency": "JPY",
            "unit": "milli",
            "windowKey": "*",
            "unlockedAtMs": AT
        })
    );
}

#[tokio::test(start_paused = true)]
async fn webhook_amount_unlock_keeps_milli_values_and_currency_code() {
    let k = k12_of(&send_generic(&viewer_amount_usd()).await);
    assert_eq!(k["value"], json!(120_500));
    assert_eq!(k["threshold"], json!(100_000));
    assert_eq!(k["currency"], "USD");
    assert_eq!(k["unit"], "milli");
}

#[tokio::test(start_paused = true)]
async fn webhook_count_unlock_sends_plain_counts_with_null_currency() {
    let k = k12_of(&send_generic(&viewer_count()).await);
    assert_eq!(k["value"], json!(100), "回数そのもの(ミリ単位ではない)");
    assert_eq!(k["threshold"], json!(100));
    assert!(k["currency"].is_null(), "currency は null: {k}");
    assert_eq!(k["unit"], "count");
}

#[tokio::test(start_paused = true)]
async fn webhook_payload_has_exactly_the_k12_keys() {
    for u in [viewer_amount_jpy(), viewer_count(), streamer_count()] {
        let k = k12_of(&send_generic(&u).await);
        assert_eq!(keys_of(&k), k12_key_list(), "{k}");
    }
}

#[tokio::test(start_paused = true)]
async fn webhook_payload_does_not_leak_description_icon_or_internal_id() {
    let raw = send_generic(&viewer_amount_jpy()).await.to_string();
    for s in ["DESCRIPTION-NOT-SENT", "ICON-NOT-SENT", "uuid-viewer-donation-10k"] {
        assert!(!raw.contains(s), "{s} は外に出さない");
    }
}

// ---- 2・3. 主体の形 ----

#[tokio::test(start_paused = true)]
async fn viewer_unlock_sent_with_name_and_author_id() {
    let k = k12_of(&send_generic(&viewer_count()).await);
    assert_eq!(k["subject"], json!({ "kind": "viewer", "platform": "youtube", "authorId": "UCxxx", "name": "Alice" }));
}

#[tokio::test(start_paused = true)]
async fn streamer_unlock_sent_without_subject_identity() {
    for u in [streamer_count(), streamer_amount()] {
        let k = k12_of(&send_generic(&u).await);
        assert_eq!(k["subject"], json!({ "kind": "streamer" }), "authorId・name・platform のキーを持たない: {k}");
    }
}

// ---- 4. SSE ----

fn pump_once() -> impl std::future::Future<Output = ()> {
    async {
        for _ in 0..40 {
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

struct Srv {
    port: u16,
    token: String,
    feed: ApiFeed,
    shutdown: CancellationToken,
}

impl Drop for Srv {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn start() -> Srv {
    let (gate, token) = open_gate();
    let feed = ApiFeed::new(1000);
    feed.set_enabled(true);
    let shutdown = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().unwrap().port();
    let deps = ApiDeps::new(gate.clone(), shutdown.clone()).with_feed(feed.clone());
    let app: Router = api::router(deps, port);
    let stop = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(listener, app).with_graceful_shutdown(stop.cancelled_owned()).await.expect("serve");
    });
    Srv { port, token, feed, shutdown }
}

struct Sse {
    buf: Arc<Mutex<Vec<u8>>>,
    status: Arc<Mutex<Option<u16>>>,
    task: JoinHandle<()>,
}

impl Drop for Sse {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[derive(Debug)]
struct Ev {
    event: Option<String>,
    data: Option<String>,
    id: Option<String>,
}

impl Sse {
    async fn open(srv: &Srv) -> Sse {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let status = Arc::new(Mutex::new(None));
        let (b2, s2) = (buf.clone(), status.clone());
        let url = format!("http://127.0.0.1:{}/api/v1/events", srv.port);
        let token = srv.token.clone();
        let task = tokio::spawn(async move {
            let client = reqwest::Client::builder().no_proxy().build().unwrap();
            let mut resp = client
                .get(&url)
                .header("authorization", format!("Bearer {token}"))
                .header("accept", "text/event-stream")
                .send()
                .await
                .expect("接続できる");
            *s2.lock().unwrap() = Some(resp.status().as_u16());
            while let Ok(Some(c)) = resp.chunk().await {
                b2.lock().unwrap().extend_from_slice(&c);
            }
        });
        let sse = Sse { buf, status, task };
        for _ in 0..200 {
            if sse.status.lock().unwrap().is_some() {
                break;
            }
            pump_once().await;
        }
        assert_eq!(*sse.status.lock().unwrap(), Some(200), "SSE に接続できる");
        pump_once().await;
        sse
    }

    fn raw(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }

    /// 完結したイベントだけを取り出す(コメント行は捨てる)。
    fn drain(&self) -> Vec<Ev> {
        let mut b = self.buf.lock().unwrap();
        let text = String::from_utf8_lossy(&b).replace("\r\n", "\n");
        let Some(end) = text.rfind("\n\n") else { return vec![] };
        let (done, rest) = text.split_at(end + 2);
        let rest = rest.as_bytes().to_vec();
        let mut out = vec![];
        for block in done.split("\n\n").filter(|s| !s.trim().is_empty()) {
            let mut ev = Ev { event: None, data: None, id: None };
            for line in block.lines() {
                if let Some(v) = line.strip_prefix("event:") {
                    ev.event = Some(v.trim_start().to_string());
                } else if let Some(v) = line.strip_prefix("data:") {
                    let v = v.strip_prefix(' ').unwrap_or(v);
                    ev.data = Some(match ev.data.take() {
                        Some(p) => format!("{p}\n{v}"),
                        None => v.to_string(),
                    });
                } else if let Some(v) = line.strip_prefix("id:") {
                    ev.id = Some(v.trim_start().to_string());
                }
            }
            out.push(ev);
        }
        *b = rest;
        out
    }

    fn achievements(&self) -> Vec<Ev> {
        self.drain().into_iter().filter(|e| e.event.as_deref() == Some("achievement")).collect()
    }
}

async fn tick() {
    tokio::time::advance(Duration::from_millis(17)).await;
    pump_once().await;
}

#[tokio::test(start_paused = true)]
async fn sse_emits_achievement_event() {
    let srv = start().await;
    let sse = Sse::open(&srv).await;
    sse.drain();

    srv.feed.push_achievement(&viewer_amount_jpy());
    srv.feed.push_achievement(&streamer_count());
    for _ in 0..3 {
        tick().await;
    }
    let evs = sse.achievements();
    assert_eq!(evs.len(), 2, "配信者分・視聴者分が 1 解除 1 イベントで流れる: {evs:?}");
    let first: Value = serde_json::from_str(evs[0].data.as_deref().unwrap()).expect("data は JSON");
    let second: Value = serde_json::from_str(evs[1].data.as_deref().unwrap()).expect("data は JSON");

    // 汎用 Webhook の `achievement` と同じ形(同じ fixture から作った K12)。
    let via_webhook = k12_of(&send_generic(&viewer_amount_jpy()).await);
    assert_eq!(first, via_webhook, "SSE の data は Webhook の K12 と同じ");
    assert_eq!(keys_of(&first), k12_key_list());
    assert_eq!(first["subject"]["kind"], "viewer");
    assert_eq!(second["subject"], json!({ "kind": "streamer" }));
    assert_eq!(second["unit"], "count");
    assert!(second["currency"].is_null());
    assert_eq!(second["value"], json!(10));
}

#[tokio::test(start_paused = true)]
async fn sse_achievement_event_has_no_resume_cursor_id() {
    let srv = start().await;
    let sse = Sse::open(&srv).await;
    sse.drain();
    srv.feed.push_achievement(&viewer_count());
    tick().await;
    tick().await;
    let evs = sse.achievements();
    assert_eq!(evs.len(), 1, "{evs:?}");
    assert!(evs[0].id.is_none(), "チャットの再開カーソルを壊さないよう id は付けない");
}

// ---- 5. 秘密が本文に出ない ----

const SIGNING_SECRET: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn assert_no_secrets(body: &str, label: &str) {
    for s in [
        "SECRETPATH-aaaa",
        "SECRETTOKEN-abcdef",
        "hook-a.example.test",
        "discord.com/api/webhooks",
        "123456789/SECRET",
        SIGNING_SECRET,
    ] {
        assert!(!body.contains(s), "{label} の本文に {s} が含まれる");
    }
}

#[tokio::test(start_paused = true)]
async fn payload_contains_no_tokens() {
    let env = wh_env_with(vec![
        (def(WebhookFormat::Generic, ach_triggers(), &[]), Some(GENERIC_URL_A)),
        (
            {
                let mut d = def(WebhookFormat::Discord, ach_triggers(), &[]);
                d.id = "wh-discord".into();
                d
            },
            Some(DISCORD_URL),
        ),
    ])
    .await;
    for u in [viewer_amount_jpy(), streamer_count()] {
        env.rt.handle_achievement(&u);
    }
    settle().await;
    let generic = env.sender.calls_for("hook-a");
    let discord = env.sender.calls_for("discord.com");
    assert_eq!(generic.len(), 2);
    assert_eq!(discord.len(), 2);
    for c in generic.iter().chain(discord.iter()) {
        assert_no_secrets(&String::from_utf8_lossy(&c.body), "Webhook");
    }

    let srv = start().await;
    let sse = Sse::open(&srv).await;
    srv.feed.push_achievement(&viewer_amount_jpy());
    srv.feed.push_achievement(&streamer_count());
    tick().await;
    tick().await;
    let raw = sse.raw();
    assert!(raw.contains("achievement"), "SSE に解除が流れている: {raw}");
    assert!(!raw.contains(&srv.token), "SSE の本文に API トークンが含まれる");
    assert_no_secrets(&raw, "SSE");
}

// ---- 6. Discord 形式 ----

fn discord_text(body: &Value) -> String {
    let d = body["embeds"][0]["description"].as_str().unwrap_or_default();
    let c = body["content"].as_str().unwrap_or_default();
    format!("{d}{c}")
}

async fn send_discord(u: &AchievementUnlocked) -> Value {
    let env = wh_env().await;
    env.add("Discord", WebhookFormat::Discord, DISCORD_URL, ach_triggers()).await;
    env.rt.handle_achievement(u);
    settle().await;
    let calls = env.sender.calls_for("discord.com");
    assert_eq!(calls.len(), 1);
    calls[0].json()
}

#[tokio::test(start_paused = true)]
async fn discord_achievement_embed_has_nonempty_description() {
    let body = send_discord(&viewer_amount_jpy()).await;
    let text = discord_text(&body);
    assert!(!text.trim().is_empty(), "description も content も空: {body}");
    assert!(text.contains("Alice"), "視聴者名が入る: {text}");
    assert!(text.contains("累計 1 万円"), "実績名が入る: {text}");
}

#[tokio::test(start_paused = true)]
async fn discord_streamer_achievement_description_has_achievement_name() {
    let body = send_discord(&streamer_count()).await;
    let text = discord_text(&body);
    assert!(text.contains("10 回配信"), "実績名が入る: {text}");
}

#[tokio::test(start_paused = true)]
async fn discord_achievement_payload_keeps_mention_suppression() {
    let body = send_discord(&viewer_count()).await;
    assert_eq!(body["allowed_mentions"], json!({ "parse": [] }));
}

// ---- 7. きっかけの選択 ----

#[tokio::test(start_paused = true)]
async fn destination_without_achievement_trigger_receives_nothing() {
    let env = wh_env().await;
    env.add("実績なし", WebhookFormat::Generic, GENERIC_URL_A, other_triggers()).await;
    env.add("実績あり", WebhookFormat::Generic, GENERIC_URL_B, ach_triggers()).await;
    env.rt.handle_achievement(&viewer_amount_jpy());
    env.rt.handle_achievement(&streamer_count());
    settle().await;
    assert!(env.sender.calls_for("hook-a").is_empty(), "実績を有効にしていない送り先には何も送らない");
    assert_eq!(env.sender.calls_for("hook-b").len(), 2, "有効にした送り先には 2 件(対照)");
}

#[tokio::test(start_paused = true)]
async fn achievement_only_destination_ignores_chat_donations() {
    let env = generic_env().await;
    env.rt.handle_message(&superchat("sc1", "¥500"));
    settle().await;
    assert!(env.sender.calls_for("hook-a").is_empty(), "実績だけを選んだ送り先に投げ銭は届かない");
}
