//! PR4 受け入れテスト共通の小道具(設計書 §5.6・付録 D)。
//!
//! 実装者が満たす契約のうち、複数ファイルが共有する物はここで使う形で固定される。
//! 各ファイル冒頭の「実装者が満たす契約」と合わせて読むこと。
#![allow(dead_code)]

use crate::common::msg;
use fast_comment_lib::integrations::{
    Integrations, IntegrationsFile, MemorySecretStore, Paused, SecretStore, WebhookDef, WebhookFormat,
    WebhookInput, WebhookTriggers, FILE_NAME,
};
use fast_comment_lib::model::{Amount, ChatMessage, Fragment, MessageKind, Platform};
use fast_comment_lib::webhook::delivery::Outcome;
use fast_comment_lib::webhook::{OutboundRequest, Sender, WebhookRuntime};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::time::Instant;

pub const NOW: u64 = 1_731_600_000_000;
pub const DISCORD_URL: &str = "https://discord.com/api/webhooks/123456789/SECRETTOKEN-abcdef";
pub const GENERIC_URL_A: &str = "https://hook-a.example.test/in/SECRETPATH-aaaa";
pub const GENERIC_URL_B: &str = "https://hook-b.example.test/in/SECRETPATH-bbbb";

// ---- 時間 ----

/// 偽の時計を少し進め、ワーカーに仕事をさせる(`start_paused = true` のテストで使う)。
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

pub async fn advance(secs: u64) {
    tokio::time::sleep(Duration::from_secs(secs)).await;
}

// ---- メッセージ ----

pub fn normal(id: &str, text: &str) -> ChatMessage {
    let mut m = msg(id);
    m.fragments = vec![Fragment::text(text)];
    m
}

pub fn with_kind(id: &str, kind: MessageKind, text: &str) -> ChatMessage {
    let mut m = normal(id, text);
    m.kind = kind;
    m
}

pub fn superchat(id: &str, raw_text: &str) -> ChatMessage {
    let mut m = with_kind(id, MessageKind::SuperChat, "ありがとう");
    m.platform = Platform::Youtube;
    m.amount = Some(Amount { value: 500.0, currency: "JPY".into(), raw_text: raw_text.into() });
    m
}

// ---- 定義 ----

pub fn triggers(donation: bool, membership: bool, keyword: bool) -> WebhookTriggers {
    WebhookTriggers { donation, membership, keyword, achievement: false }
}

pub fn def(format: WebhookFormat, t: WebhookTriggers, keywords: &[&str]) -> WebhookDef {
    WebhookDef {
        id: "wh-test".into(),
        name: "テスト".into(),
        format,
        enabled: true,
        triggers: t,
        keywords: keywords.iter().map(|s| s.to_string()).collect(),
        paused: None,
    }
}

pub fn input(name: &str, format: WebhookFormat, url: Option<&str>) -> WebhookInput {
    WebhookInput {
        id: None,
        name: name.into(),
        format,
        enabled: true,
        triggers: triggers(true, true, false),
        keywords: vec![],
        url: url.map(|s| s.into()),
    }
}

pub fn view_id<T: serde::Serialize>(view: &T) -> String {
    serde_json::to_value(view).unwrap()["id"].as_str().expect("view.id は文字列").to_string()
}

pub fn secret_key(id: &str) -> String {
    format!("webhook:{id}")
}

pub fn secret_json(secrets: &MemorySecretStore, id: &str) -> serde_json::Value {
    let raw = secrets.raw(&secret_key(id)).unwrap_or_else(|| panic!("資格情報 webhook:{id} が無い"));
    serde_json::from_str(&raw).expect("webhook:<id> の値は JSON")
}

/// ファイルと資格情報を直接作る(`Integrations::open` が読む状態を用意する)。
pub async fn seed(dir: &Path, secrets: &MemorySecretStore, defs: Vec<(WebhookDef, Option<&str>)>) {
    let mut file = IntegrationsFile { version: 1, ..Default::default() };
    for (d, url) in defs {
        if let Some(url) = url {
            let signing = if d.format == WebhookFormat::Generic {
                serde_json::json!({ "url": url, "signingSecret": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef" })
            } else {
                serde_json::json!({ "url": url })
            };
            secrets.put(&secret_key(&d.id), &signing.to_string()).await.unwrap();
        }
        file.webhooks.push(d);
    }
    fast_comment_lib::integrations::save(dir, &file).unwrap();
}

pub fn paused(reason: &str) -> Option<Paused> {
    Some(Paused { reason: reason.into(), at_ms: 1234 })
}

pub fn file_json(dir: &Path) -> String {
    std::fs::read_to_string(dir.join(FILE_NAME)).unwrap_or_default()
}

// ---- 偽の送信器 ----

pub fn status(code: u16) -> Outcome {
    Outcome::Status {
        code,
        header_retry_after: None,
        body_retry_after: None,
        remaining_zero_reset_after: None,
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub at: Instant,
}

impl Call {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    pub fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).expect("本文は JSON")
    }
    /// 汎用 JSON の `comment.id`。
    pub fn comment_id(&self) -> String {
        self.json()["comment"]["id"].as_str().expect("comment.id").to_string()
    }
}

#[derive(Default)]
struct FakeState {
    calls: Vec<Call>,
    scripts: HashMap<String, VecDeque<Outcome>>,
    delays: HashMap<String, Duration>,
    delays_once: HashMap<String, VecDeque<Duration>>,
    in_flight: usize,
    max_in_flight: usize,
}

/// 送信の抽象(`Sender`)の偽物。ネットワークには出ない。
/// 台本が尽きたら 204 を返す。`key` は URL の部分文字列で照合する。
#[derive(Clone, Default)]
pub struct FakeSender(Arc<Mutex<FakeState>>);

impl FakeSender {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn script(&self, key: &str, outcomes: Vec<Outcome>) {
        self.0.lock().unwrap().scripts.entry(key.into()).or_default().extend(outcomes);
    }
    pub fn delay(&self, key: &str, d: Duration) {
        self.0.lock().unwrap().delays.insert(key.into(), d);
    }
    /// 次の 1 回だけ応答を遅らせる(積むと呼ばれた順に消費する)。
    pub fn delay_next(&self, key: &str, d: Duration) {
        self.0.lock().unwrap().delays_once.entry(key.into()).or_default().push_back(d);
    }
    pub fn calls(&self) -> Vec<Call> {
        self.0.lock().unwrap().calls.clone()
    }
    pub fn calls_for(&self, key: &str) -> Vec<Call> {
        self.calls().into_iter().filter(|c| c.url.contains(key)).collect()
    }
    pub fn max_in_flight(&self) -> usize {
        self.0.lock().unwrap().max_in_flight
    }
}

impl Sender for FakeSender {
    async fn send(&self, request: OutboundRequest) -> Outcome {
        let url = request.url_for_test().to_string();
        let (delay, outcome) = {
            let mut s = self.0.lock().unwrap();
            s.calls.push(Call {
                url: url.clone(),
                headers: request.headers.clone(),
                body: request.body.clone(),
                at: Instant::now(),
            });
            s.in_flight += 1;
            s.max_in_flight = s.max_in_flight.max(s.in_flight);
            let once_key = s.delays_once.keys().find(|k| url.contains(k.as_str())).cloned();
            let once = once_key.and_then(|k| s.delays_once.get_mut(&k).and_then(|q| q.pop_front()));
            let sticky = s.delays.iter().find(|(k, _)| url.contains(k.as_str())).map(|(_, d)| *d);
            let delay = once.or(sticky);
            let key = s.scripts.keys().find(|k| url.contains(k.as_str())).cloned();
            let outcome = key.and_then(|k| s.scripts.get_mut(&k).and_then(|q| q.pop_front()));
            (delay, outcome.unwrap_or_else(|| status(204)))
        };
        if let Some(d) = delay {
            tokio::time::sleep(d).await;
        }
        self.0.lock().unwrap().in_flight -= 1;
        outcome
    }
}

// ---- 環境 ----

pub struct WhEnv {
    pub dir: TempDir,
    pub secrets: MemorySecretStore,
    pub it: Integrations<MemorySecretStore>,
    pub sender: FakeSender,
    pub rt: WebhookRuntime<MemorySecretStore, FakeSender>,
}

/// 空の連携設定 + 偽の送信器 + ランタイム。tokio の実行時の中で呼ぶ。
pub async fn wh_env() -> WhEnv {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let sender = FakeSender::new();
    let it = Integrations::open(dir.path().to_path_buf(), secrets.clone(), NOW).await;
    let rt = WebhookRuntime::new(it.clone(), sender.clone());
    WhEnv { dir, secrets, it, sender, rt }
}

/// 先にファイルと資格情報を用意してから開く(停止中・URL なし等の状態を作る)。
pub async fn wh_env_with(defs: Vec<(WebhookDef, Option<&str>)>) -> WhEnv {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    seed(dir.path(), &secrets, defs).await;
    let sender = FakeSender::new();
    let it = Integrations::open(dir.path().to_path_buf(), secrets.clone(), NOW).await;
    let rt = WebhookRuntime::new(it.clone(), sender.clone());
    rt.reload().await;
    WhEnv { dir, secrets, it, sender, rt }
}

impl WhEnv {
    /// Webhook を保存してランタイムへ反映し、id を返す。
    pub async fn add(&self, name: &str, format: WebhookFormat, url: &str, t: WebhookTriggers) -> String {
        let mut i = input(name, format, Some(url));
        i.triggers = t;
        let view = self.it.save_webhook(i).await.expect("保存できる");
        self.rt.reload().await;
        view_id(&view)
    }
    /// 投げ銭だけに反応する汎用 JSON の送り先。
    pub async fn add_generic(&self, url: &str) -> String {
        self.add("汎用", WebhookFormat::Generic, url, triggers(true, false, false)).await
    }
    pub async fn add_discord(&self) -> String {
        self.add("Discord", WebhookFormat::Discord, DISCORD_URL, triggers(true, false, false)).await
    }
    pub fn status_of(&self, id: &str) -> serde_json::Value {
        let all = serde_json::to_value(self.rt.status()).unwrap();
        all.as_array()
            .expect("status は配列")
            .iter()
            .find(|s| s["id"] == id)
            .unwrap_or_else(|| panic!("status に {id} が無い"))
            .clone()
    }
}

// ---- 署名の独立計算(実装を使わない) ----

pub fn hmac_sha256_hex(key: &[u8], data: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut k = if key.len() > BLOCK { Sha256::digest(key).to_vec() } else { key.to_vec() };
    k.resize(BLOCK, 0);
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = Sha256::new();
    inner.update(&ipad);
    inner.update(data);
    let mut outer = Sha256::new();
    outer.update(&opad);
    outer.update(inner.finalize());
    outer.finalize().iter().map(|b| format!("{b:02x}")).collect()
}
