//! PR4 受け入れテスト: Webhook の保存・削除・再開・署名シークレット(設計書 §0.6・§3.6・§5.6)。
//! 資格情報ストアは `MemorySecretStore`、保存先は一時ディレクトリ。
//!
//! 実装者が満たす契約(付録 D に加えて、このファイルが前提にする物):
//! - 資格情報のキーは `webhook:<id>`。値は JSON 文字列 `{"url": "...", "signingSecret": "..."}`。
//!   Discord 形式では `signingSecret` を持たない(欠落または `null`)。汎用形式では作成時に自動生成する
//!   64 桁の小文字 16 進(32 バイト)。編集では変えない。再生成だけが変える。
//! - Discord 形式の `reveal_signing_secret` / `regenerate_signing_secret` は `Err`。
//! - `WebhookView` の JSON: `id`、`name`、`format`、`enabled`、`triggers`、`keywords`、`hasUrl`、`urlPreview`
//!   (= `webhook::target::redact(url)`、URL が無ければ `null`)、`paused`(`null` か `{ reason, atMs }`)。
//!   `url` と `signingSecret` のキーは持たない。
//! - `save_webhook`: `id: None` は新規(uuid v4 を採番)、`Some` は編集。編集で `url: None` なら保存済みの URL を保つ。
//!   `name` は前後の空白を除いた後の 1〜64 文字、`keywords` は各語の前後の空白を除き、空になった語を捨てた後に
//!   20 語以内・各 1〜64 文字。文字数は `chars()`。Webhook は 10 件まで。
//!   形式を変える編集では保存済みの URL を新しい形式の規則で検証し直し、違えば `Err` で何も変えない。
//! - 失敗した `save_webhook` / `delete_webhook` は、一覧・ファイル・資格情報を呼ぶ前の状態に保つ。
//!   新規でファイル保存に失敗した時は、作った資格情報を消す。
//! - `resume_webhook` は `paused` を消して保存する。
//! - 資格情報が読めない(エントリが無い)Webhook は `hasUrl: false`、状態は `noUrl`。
//! - `Integrations::open` は `integrations.json` の Webhook をそのまま `list_webhooks` へ出す(`paused` を含む)。

use crate::webhook_common::*;
use fast_comment_lib::integrations::{
    self, Integrations, IntegrationsFile, LoadOutcome, MemorySecretStore, SecretStore, WebhookFormat, FILE_NAME,
};
use fast_comment_lib::webhook::target::redact;
use fast_comment_lib::webhook::WebhookRuntime;
use std::path::Path;

const DISCORD: &str = DISCORD_URL;
const GENERIC: &str = "https://hooks.example.test/in/PRIVATEPATH-1234";

async fn open(dir: &Path, secrets: &MemorySecretStore) -> Integrations<MemorySecretStore> {
    Integrations::open(dir.to_path_buf(), secrets.clone(), NOW).await
}

fn loaded(dir: &Path) -> IntegrationsFile {
    match integrations::load(dir, NOW).unwrap() {
        LoadOutcome::Loaded(f) => f,
        LoadOutcome::Missing => IntegrationsFile::default(),
        LoadOutcome::Corrupt { .. } => panic!("壊れていない"),
    }
}

fn list_json(it: &Integrations<MemorySecretStore>) -> serde_json::Value {
    serde_json::to_value(it.list_webhooks()).unwrap()
}

fn webhook_keys(secrets: &MemorySecretStore) -> Vec<String> {
    secrets.keys().into_iter().filter(|k| k.starts_with("webhook:")).collect()
}

struct Store {
    dir: tempfile::TempDir,
    secrets: MemorySecretStore,
    it: Integrations<MemorySecretStore>,
}

async fn store() -> Store {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(dir.path(), &secrets).await;
    Store { dir, secrets, it }
}

impl Store {
    async fn add(&self, name: &str, format: WebhookFormat, url: &str) -> String {
        view_id(&self.it.save_webhook(input(name, format, Some(url))).await.expect("保存できる"))
    }
}

#[tokio::test]
async fn save_new_webhook_stores_url_in_secrets_not_in_file() {
    let s = store().await;
    let id = s.add("通知", WebhookFormat::Discord, DISCORD).await;

    assert_eq!(secret_json(&s.secrets, &id)["url"], DISCORD);
    let file = file_json(s.dir.path());
    assert!(file.contains(&id), "定義はファイルにある");
    assert!(!file.contains("SECRETTOKEN") && !file.contains("discord.com"), "URL はファイルに無い: {file}");
    assert_eq!(loaded(s.dir.path()).webhooks.len(), 1);
}

#[tokio::test]
async fn save_new_webhook_without_url_is_err() {
    let s = store().await;
    assert!(s.it.save_webhook(input("通知", WebhookFormat::Discord, None)).await.is_err());
    assert!(s.it.list_webhooks().is_empty());
    assert!(webhook_keys(&s.secrets).is_empty());
    assert!(loaded(s.dir.path()).webhooks.is_empty());
}

#[tokio::test]
async fn save_existing_without_url_keeps_url() {
    let s = store().await;
    let id = s.add("通知", WebhookFormat::Discord, DISCORD).await;

    let mut edit = input("改名後", WebhookFormat::Discord, None);
    edit.id = Some(id.clone());
    let view = s.it.save_webhook(edit).await.expect("URL なしの編集は通る");

    assert_eq!(serde_json::to_value(&view).unwrap()["name"], "改名後");
    assert_eq!(secret_json(&s.secrets, &id)["url"], DISCORD, "URL は変わらない");
    assert_eq!(list_json(&s.it)[0]["hasUrl"], true);
}

#[tokio::test]
async fn save_generic_generates_signing_secret_once_and_keeps_it_on_edit() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, GENERIC).await;

    let first = secret_json(&s.secrets, &id)["signingSecret"].as_str().expect("署名シークレット").to_string();
    assert_eq!(first.len(), 64);
    assert!(first.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{first}");

    let mut edit = input("汎用 改", WebhookFormat::Generic, Some("https://hooks.example.test/in/new-path"));
    edit.id = Some(id.clone());
    s.it.save_webhook(edit).await.unwrap();

    let after = secret_json(&s.secrets, &id);
    assert_eq!(after["signingSecret"], first.as_str(), "編集では変わらない");
    assert_eq!(after["url"], "https://hooks.example.test/in/new-path");
}

#[tokio::test]
async fn save_discord_has_no_signing_secret() {
    let s = store().await;
    let id = s.add("通知", WebhookFormat::Discord, DISCORD).await;
    let v = secret_json(&s.secrets, &id);
    assert!(v.get("signingSecret").map_or(true, |x| x.is_null()), "{v}");
    assert!(s.it.reveal_signing_secret(&id).await.is_err());
}

#[tokio::test]
async fn save_rejects_invalid_url_for_format_and_changes_nothing() {
    let s = store().await;
    // 新規
    assert!(s.it.save_webhook(input("通知", WebhookFormat::Discord, Some("https://example.com/hook"))).await.is_err());
    assert!(s.it.list_webhooks().is_empty());
    assert!(webhook_keys(&s.secrets).is_empty());
    assert!(loaded(s.dir.path()).webhooks.is_empty());

    // 編集
    let id = s.add("通知", WebhookFormat::Discord, DISCORD).await;
    let before_file = file_json(s.dir.path());
    let mut edit = input("通知 改", WebhookFormat::Discord, Some("http://discord.com/api/webhooks/1/x"));
    edit.id = Some(id.clone());
    assert!(s.it.save_webhook(edit).await.is_err());
    assert_eq!(secret_json(&s.secrets, &id)["url"], DISCORD);
    assert_eq!(file_json(s.dir.path()), before_file);
    assert_eq!(list_json(&s.it)[0]["name"], "通知");
}

#[tokio::test]
async fn save_rejects_11th_webhook() {
    let s = store().await;
    for i in 0..10 {
        s.add(&format!("w{i}"), WebhookFormat::Generic, &format!("https://hooks.example.test/{i}")).await;
    }
    assert!(s.it.save_webhook(input("11 件目", WebhookFormat::Generic, Some("https://hooks.example.test/x"))).await.is_err());
    assert_eq!(s.it.list_webhooks().len(), 10);
    assert_eq!(webhook_keys(&s.secrets).len(), 10);
    assert_eq!(loaded(s.dir.path()).webhooks.len(), 10);

    // 既存の編集は 10 件でも通る
    let existing = view_id(&s.it.list_webhooks()[0]);
    let mut edit = input("編集", WebhookFormat::Generic, None);
    edit.id = Some(existing);
    assert!(s.it.save_webhook(edit).await.is_ok());
}

#[tokio::test]
async fn save_rejects_name_over_64_chars_and_blank_name() {
    let s = store().await;
    assert!(s.it.save_webhook(input(&"あ".repeat(65), WebhookFormat::Discord, Some(DISCORD))).await.is_err());
    assert!(s.it.save_webhook(input("", WebhookFormat::Discord, Some(DISCORD))).await.is_err());
    assert!(s.it.save_webhook(input("  \u{3000} ", WebhookFormat::Discord, Some(DISCORD))).await.is_err());
    assert!(s.it.list_webhooks().is_empty());
    assert!(webhook_keys(&s.secrets).is_empty());

    assert!(
        s.it.save_webhook(input(&"あ".repeat(64), WebhookFormat::Discord, Some(DISCORD))).await.is_ok(),
        "64 文字ちょうどは通る(バイト数ではなく chars())"
    );
}

#[tokio::test]
async fn save_rejects_more_than_20_keywords_or_keyword_over_64_chars() {
    let s = store().await;
    let mut many = input("多い", WebhookFormat::Discord, Some(DISCORD));
    many.keywords = (0..21).map(|i| format!("k{i}")).collect();
    assert!(s.it.save_webhook(many).await.is_err());

    let mut long = input("長い", WebhookFormat::Discord, Some(DISCORD));
    long.keywords = vec!["あ".repeat(65)];
    assert!(s.it.save_webhook(long).await.is_err());
    assert!(s.it.list_webhooks().is_empty());

    let mut edge = input("境界", WebhookFormat::Discord, Some(DISCORD));
    edge.keywords = (0..19).map(|i| format!("k{i}")).chain(["あ".repeat(64)]).collect();
    assert_eq!(edge.keywords.len(), 20);
    assert!(s.it.save_webhook(edge).await.is_ok(), "20 語・64 文字ちょうどは通る");
}

#[tokio::test]
async fn save_trims_and_drops_blank_keywords() {
    let s = store().await;
    let mut i = input("語", WebhookFormat::Discord, Some(DISCORD));
    i.keywords = vec!["  質問 ".into(), "".into(), "   ".into(), "\u{3000}".into(), "hello".into()];
    s.it.save_webhook(i).await.unwrap();

    assert_eq!(list_json(&s.it)[0]["keywords"], serde_json::json!(["質問", "hello"]));
    assert_eq!(loaded(s.dir.path()).webhooks[0].keywords, vec!["質問".to_string(), "hello".to_string()]);
}

#[tokio::test]
async fn save_fails_cleanly_when_secret_put_fails() {
    let s = store().await;
    // 新規
    s.secrets.fail_next_put();
    assert!(s.it.save_webhook(input("通知", WebhookFormat::Discord, Some(DISCORD))).await.is_err());
    assert!(s.it.list_webhooks().is_empty());
    assert!(webhook_keys(&s.secrets).is_empty());
    assert!(loaded(s.dir.path()).webhooks.is_empty());

    // 編集(URL 変更)
    let id = s.add("通知", WebhookFormat::Discord, DISCORD).await;
    let before_file = file_json(s.dir.path());
    s.secrets.fail_next_put();
    let mut edit = input("通知 改", WebhookFormat::Discord, Some("https://discord.com/api/webhooks/2/other"));
    edit.id = Some(id.clone());
    assert!(s.it.save_webhook(edit).await.is_err());
    assert_eq!(secret_json(&s.secrets, &id)["url"], DISCORD);
    assert_eq!(file_json(s.dir.path()), before_file);
    assert_eq!(list_json(&s.it)[0]["name"], "通知");
}

#[tokio::test]
async fn save_new_rolls_back_secret_when_file_save_fails() {
    let s = store().await;
    // `integrations.json` の場所をディレクトリにして、rename を失敗させる。
    std::fs::create_dir(s.dir.path().join(FILE_NAME)).unwrap();

    let result = s.it.save_webhook(input("通知", WebhookFormat::Discord, Some(DISCORD))).await;

    assert!(result.is_err());
    assert!(webhook_keys(&s.secrets).is_empty(), "作った資格情報は消す: {:?}", s.secrets.keys());
    assert!(s.it.list_webhooks().is_empty());
}

#[tokio::test]
async fn changing_format_revalidates_stored_url() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, "https://example.com/hook").await;

    let mut edit = input("汎用", WebhookFormat::Discord, None);
    edit.id = Some(id.clone());
    assert!(s.it.save_webhook(edit).await.is_err(), "Discord の規則に合わない URL");

    assert_eq!(list_json(&s.it)[0]["format"], "generic");
    assert_eq!(loaded(s.dir.path()).webhooks[0].format, WebhookFormat::Generic);
    assert_eq!(secret_json(&s.secrets, &id)["url"], "https://example.com/hook");
}

#[tokio::test]
async fn delete_removes_secret_then_definition() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, GENERIC).await;
    let other = s.add("別", WebhookFormat::Discord, DISCORD).await;

    s.it.delete_webhook(&id).await.expect("削除できる");

    assert!(s.secrets.raw(&secret_key(&id)).is_none());
    assert!(s.secrets.raw(&secret_key(&other)).is_some(), "他の Webhook は残る");
    let ids: Vec<String> = loaded(s.dir.path()).webhooks.into_iter().map(|w| w.id).collect();
    assert_eq!(ids, [other.clone()]);
    assert_eq!(s.it.list_webhooks().len(), 1);
}

#[tokio::test]
async fn delete_keeps_definition_when_secret_delete_fails() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, GENERIC).await;

    s.secrets.fail_next_delete();
    assert!(s.it.delete_webhook(&id).await.is_err());

    assert_eq!(s.it.list_webhooks().len(), 1, "秘密が残ったまま定義だけ消さない");
    assert_eq!(loaded(s.dir.path()).webhooks.len(), 1);
    assert!(s.secrets.raw(&secret_key(&id)).is_some());
}

#[tokio::test(start_paused = true)]
async fn webhook_with_missing_secret_is_listed_as_no_url_and_never_sends() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, GENERIC).await;
    s.secrets.delete(&secret_key(&id)).await.unwrap();

    let it = open(s.dir.path(), &s.secrets).await;
    let list = serde_json::to_value(it.list_webhooks()).unwrap();
    assert_eq!(list[0]["hasUrl"], false);
    assert!(list[0]["urlPreview"].is_null());

    let sender = FakeSender::new();
    let rt = WebhookRuntime::new(it, sender.clone());
    rt.reload().await;
    rt.handle_message(&superchat("n1", "¥500"));
    advance(60).await;
    assert!(sender.calls().is_empty());
    let status = serde_json::to_value(rt.status()).unwrap();
    assert_eq!(status[0]["state"], "noUrl");
}

#[tokio::test]
async fn list_view_serialization_exposes_only_url_preview() {
    let s = store().await;
    let gid = s.add("汎用", WebhookFormat::Generic, GENERIC).await;
    s.add("通知", WebhookFormat::Discord, DISCORD).await;
    let signing = secret_json(&s.secrets, &gid)["signingSecret"].as_str().unwrap().to_string();

    let json = serde_json::to_string(&s.it.list_webhooks()).unwrap();
    for leak in ["PRIVATEPATH", "SECRETTOKEN", "123456789", signing.as_str()] {
        assert!(!json.contains(leak), "{json} に {leak} が出ている");
    }
    let list: serde_json::Value = serde_json::from_str(&json).unwrap();
    for v in list.as_array().unwrap() {
        assert!(v.get("url").is_none() && v.get("signingSecret").is_none());
        assert_eq!(v["hasUrl"], true);
    }
    assert_eq!(list[0]["urlPreview"], redact(GENERIC));
    assert_eq!(list[1]["urlPreview"], redact(DISCORD));
    assert!(list[0]["urlPreview"].as_str().unwrap().starts_with("https://hooks.example.test/"));
}

#[tokio::test]
async fn reveal_signing_secret_returns_stored_value_and_regenerate_changes_it() {
    let s = store().await;
    let id = s.add("汎用", WebhookFormat::Generic, GENERIC).await;
    let stored = secret_json(&s.secrets, &id)["signingSecret"].as_str().unwrap().to_string();

    assert_eq!(s.it.reveal_signing_secret(&id).await.unwrap(), stored);

    let new = s.it.regenerate_signing_secret(&id).await.unwrap();
    assert_ne!(new, stored);
    assert_eq!(new.len(), 64);
    assert_eq!(s.it.reveal_signing_secret(&id).await.unwrap(), new);
    let after = secret_json(&s.secrets, &id);
    assert_eq!(after["signingSecret"], new.as_str());
    assert_eq!(after["url"], GENERIC, "URL は変わらない");
}

#[tokio::test]
async fn resume_clears_paused_and_persists() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let mut d = def(WebhookFormat::Generic, triggers(true, false, false), &[]);
    d.id = "wh-a".into();
    d.paused = paused("unauthorized");
    seed(dir.path(), &secrets, vec![(d, Some(GENERIC))]).await;
    let it = open(dir.path(), &secrets).await;
    assert!(!list_json(&it)[0]["paused"].is_null());

    it.resume_webhook("wh-a").await.expect("再開できる");

    assert!(list_json(&it)[0]["paused"].is_null());
    assert!(loaded(dir.path()).webhooks[0].paused.is_none(), "ファイルにも反映");
    assert!(secrets.raw(&secret_key("wh-a")).is_some(), "資格情報は触らない");
}

#[tokio::test]
async fn paused_state_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let mut d = def(WebhookFormat::Discord, triggers(true, false, false), &[]);
    d.id = "wh-a".into();
    d.paused = paused("notFound");
    seed(dir.path(), &secrets, vec![(d, Some(DISCORD))]).await;

    let it = open(dir.path(), &secrets).await;
    let again = open(dir.path(), &secrets).await;

    for it in [&it, &again] {
        let p = &list_json(it)[0]["paused"];
        assert_eq!(p["reason"], "notFound");
        assert_eq!(p["atMs"], 1234);
    }
    assert_eq!(loaded(dir.path()).webhooks[0].paused.as_ref().unwrap().reason, "notFound");
}
