//! PR1 受け入れテスト: `integrations.json` の読み書きと、API の有効化・トークン管理(設計書 §5.3)。
//! 資格情報ストアは `MemorySecretStore`、保存先は一時ディレクトリ。

use fast_comment_lib::config::AppConfig;
use fast_comment_lib::integrations::{
    self, Integrations, IntegrationsFile, LoadOutcome, LocalApiSettings, MemorySecretStore, SecretStore,
    WebhookDef, WebhookFormat, WebhookTriggers, FILE_NAME,
};
use std::path::Path;
use tempfile::TempDir;

const TOKEN_KEY: &str = "local-api-token";
const NOW: u64 = 1_731_600_000_000;

fn file_path(dir: &Path) -> std::path::PathBuf {
    dir.join(FILE_NAME)
}

fn write_raw(dir: &Path, body: &str) {
    std::fs::write(file_path(dir), body).unwrap();
}

async fn open(dir: &TempDir, secrets: &MemorySecretStore) -> Integrations<MemorySecretStore> {
    Integrations::open(dir.path().to_path_buf(), secrets.clone(), NOW).await
}

fn status_json<T: serde::Serialize>(status: &T) -> serde_json::Value {
    serde_json::to_value(status).unwrap()
}

fn token_error_is_set(status: &serde_json::Value) -> bool {
    status.get("tokenError").is_some_and(|v| !v.is_null())
}

fn loaded(dir: &Path) -> IntegrationsFile {
    match integrations::load(dir, NOW).unwrap() {
        LoadOutcome::Loaded(f) => f,
        _ => panic!("Loaded を期待"),
    }
}

fn sample_file() -> IntegrationsFile {
    IntegrationsFile {
        version: 1,
        local_api: LocalApiSettings { enabled: true },
        webhooks: vec![WebhookDef {
            id: "wh-1".into(),
            name: "配信通知".into(),
            format: WebhookFormat::Discord,
            enabled: true,
            triggers: WebhookTriggers { donation: true, membership: false, keyword: true },
            keywords: vec!["初見".into(), "Hello".into()],
            paused: None,
        }],
    }
}

// ---- ファイルの読み書き ----

#[test]
fn missing_file_loads_defaults() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(integrations::load(dir.path(), NOW).unwrap(), LoadOutcome::Missing));
    let d = IntegrationsFile::default();
    assert!(!d.local_api.enabled);
    assert!(d.webhooks.is_empty());
}

#[tokio::test]
async fn missing_file_opens_with_api_disabled() {
    let dir = tempfile::tempdir().unwrap();
    let it = open(&dir, &MemorySecretStore::new()).await;
    assert_eq!(status_json(&it.local_api_status())["enabled"], false);
    assert!(!it.gate().is_open());
}

#[test]
fn save_then_load_roundtrips_all_fields() {
    let dir = tempfile::tempdir().unwrap();
    let mut file = sample_file();
    file.webhooks.push(WebhookDef {
        id: "wh-2".into(),
        name: "汎用".into(),
        format: WebhookFormat::Generic,
        enabled: false,
        triggers: WebhookTriggers { donation: false, membership: true, keyword: false },
        keywords: vec![],
        paused: Some(integrations::Paused { reason: "HTTP 404".into(), at_ms: 1234 }),
    });
    integrations::save(dir.path(), &file).unwrap();
    assert_eq!(loaded(dir.path()), file);
}

#[test]
fn corrupt_json_is_moved_aside_and_defaults_are_used() {
    let dir = tempfile::tempdir().unwrap();
    write_raw(dir.path(), "{ not json");
    match integrations::load(dir.path(), 4242).unwrap() {
        LoadOutcome::Corrupt { backup } => {
            assert_eq!(backup, dir.path().join(format!("{FILE_NAME}.corrupt-4242")));
            assert!(backup.exists(), "退避ファイルが出来る");
            assert_eq!(std::fs::read_to_string(&backup).unwrap(), "{ not json");
        }
        _ => panic!("Corrupt を期待"),
    }
    assert!(!file_path(dir.path()).exists(), "元のパスは無くなる");
}

#[tokio::test]
async fn corrupt_json_opens_with_api_disabled() {
    let dir = tempfile::tempdir().unwrap();
    write_raw(dir.path(), "{ not json");
    let it = open(&dir, &MemorySecretStore::new()).await;
    assert_eq!(status_json(&it.local_api_status())["enabled"], false);
    assert!(!it.gate().is_open());
}

#[test]
fn unsupported_version_is_treated_as_corrupt() {
    let dir = tempfile::tempdir().unwrap();
    write_raw(dir.path(), r#"{"version":2,"localApi":{"enabled":true},"webhooks":[]}"#);
    assert!(matches!(
        integrations::load(dir.path(), 77).unwrap(),
        LoadOutcome::Corrupt { .. }
    ));
    assert!(!file_path(dir.path()).exists());
}

#[test]
fn unknown_keys_are_ignored_on_load() {
    let dir = tempfile::tempdir().unwrap();
    write_raw(
        dir.path(),
        r#"{"version":1,"futureTop":{"a":1},"localApi":{"enabled":true,"futureKey":"x"},"webhooks":[]}"#,
    );
    let f = loaded(dir.path());
    assert!(f.local_api.enabled);
    assert!(f.webhooks.is_empty());
}

#[test]
fn save_leaves_no_temp_file_behind() {
    let dir = tempfile::tempdir().unwrap();
    integrations::save(dir.path(), &sample_file()).unwrap();
    integrations::save(dir.path(), &IntegrationsFile { version: 1, ..Default::default() }).unwrap();
    let names: Vec<String> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, vec![FILE_NAME.to_string()]);
}

#[test]
fn save_into_missing_directory_returns_err_without_creating_files() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("no-such-dir");
    assert!(integrations::save(&missing, &sample_file()).is_err());
    assert!(!missing.exists(), "ディレクトリを勝手に作らない");
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

// ---- 有効化・トークン ----

#[tokio::test]
async fn enable_generates_token_once_and_opens_gate() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.set_local_api_enabled(true).await.unwrap();
    assert!(it.gate().is_open());
    let first = secrets.raw(TOKEN_KEY).expect("資格情報に local-api-token");
    assert!(it.gate().verify(&first));
    it.set_local_api_enabled(true).await.unwrap();
    assert_eq!(secrets.raw(TOKEN_KEY).as_deref(), Some(first.as_str()), "トークンは作り直さない");
    assert_eq!(it.local_api_token().await.unwrap(), first);
}

#[tokio::test]
async fn enable_fails_closed_when_secret_store_put_fails() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    secrets.fail_next_put();
    assert!(it.set_local_api_enabled(true).await.is_err());
    assert!(!it.gate().is_open());
    assert_eq!(status_json(&it.local_api_status())["enabled"], false);
    match integrations::load(dir.path(), NOW).unwrap() {
        LoadOutcome::Missing => {}
        LoadOutcome::Loaded(f) => assert!(!f.local_api.enabled, "ファイルは enabled:false のまま"),
        _ => panic!("想定外の LoadOutcome"),
    }
}

#[tokio::test]
async fn disable_closes_gate_and_keeps_token() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.set_local_api_enabled(true).await.unwrap();
    let token = secrets.raw(TOKEN_KEY).unwrap();
    it.set_local_api_enabled(false).await.unwrap();
    assert!(!it.gate().is_open());
    assert_eq!(secrets.raw(TOKEN_KEY).as_deref(), Some(token.as_str()), "トークンは残す");
    it.set_local_api_enabled(true).await.unwrap();
    assert!(it.gate().verify(&token), "再有効化で同じトークンが通る");
}

#[tokio::test]
async fn open_with_enabled_true_but_no_token_keeps_gate_closed() {
    let dir = tempfile::tempdir().unwrap();
    integrations::save(dir.path(), &sample_file()).unwrap();
    let it = open(&dir, &MemorySecretStore::new()).await;
    assert!(!it.gate().is_open());
    assert!(token_error_is_set(&status_json(&it.local_api_status())));
}

#[tokio::test]
async fn open_with_enabled_true_and_secret_store_error_keeps_gate_closed() {
    let dir = tempfile::tempdir().unwrap();
    integrations::save(dir.path(), &sample_file()).unwrap();
    let secrets = MemorySecretStore::new();
    let token = fast_comment_lib::api::generate_token();
    secrets.put(TOKEN_KEY, &token).await.unwrap();
    secrets.fail_next_get();
    let it = open(&dir, &secrets).await;
    assert!(!it.gate().is_open());
    assert!(!it.gate().verify(&token));
    assert!(token_error_is_set(&status_json(&it.local_api_status())));
}

#[tokio::test]
async fn open_with_enabled_true_and_token_present_opens_gate() {
    let dir = tempfile::tempdir().unwrap();
    integrations::save(dir.path(), &sample_file()).unwrap();
    let secrets = MemorySecretStore::new();
    let token = fast_comment_lib::api::generate_token();
    secrets.put(TOKEN_KEY, &token).await.unwrap();
    let it = open(&dir, &secrets).await;
    assert!(it.gate().is_open());
    assert!(it.gate().verify(&token));
    assert!(!token_error_is_set(&status_json(&it.local_api_status())));
}

#[tokio::test]
async fn regenerate_replaces_token_and_old_one_stops_verifying() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.set_local_api_enabled(true).await.unwrap();
    let old = secrets.raw(TOKEN_KEY).unwrap();
    it.regenerate_local_api_token().await.unwrap();
    let new = secrets.raw(TOKEN_KEY).unwrap();
    assert_ne!(old, new);
    assert!(it.gate().verify(&new));
    assert!(!it.gate().verify(&old));
    assert_eq!(it.local_api_token().await.unwrap(), new);
}

#[tokio::test]
async fn regenerate_keeps_old_token_when_put_fails() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.set_local_api_enabled(true).await.unwrap();
    let old = secrets.raw(TOKEN_KEY).unwrap();
    secrets.fail_next_put();
    assert!(it.regenerate_local_api_token().await.is_err());
    assert!(it.gate().verify(&old), "旧トークンのまま通る");
    assert_eq!(secrets.raw(TOKEN_KEY).as_deref(), Some(old.as_str()));
}

#[tokio::test]
async fn regenerate_while_disabled_stores_token_and_stays_closed() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.regenerate_local_api_token().await.unwrap();
    assert!(secrets.raw(TOKEN_KEY).is_some());
    assert!(!it.gate().is_open());
    assert_eq!(status_json(&it.local_api_status())["enabled"], false);
}

// ---- 秘密がどこにも出ない ----

#[tokio::test]
async fn integrations_file_never_contains_token() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    it.set_local_api_enabled(true).await.unwrap();
    it.regenerate_local_api_token().await.unwrap();
    let token = secrets.raw(TOKEN_KEY).unwrap();
    let body = std::fs::read_to_string(file_path(dir.path())).expect("有効化後にファイルがある");
    assert!(!body.contains(&token));
    assert!(!body.contains(&token[3..]));
}

#[tokio::test]
async fn status_serialization_and_debug_do_not_contain_token() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    let status = it.set_local_api_enabled(true).await.unwrap();
    let token = secrets.raw(TOKEN_KEY).unwrap();
    let json = serde_json::to_string(&status).unwrap();
    let dbg = format!("{status:?} {status:#?}");
    for text in [json, dbg] {
        assert!(!text.contains(&token));
        assert!(!text.contains(&token[3..]));
    }
}

// ---- 並行 ----

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_enable_and_regenerate_leave_file_and_gate_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let secrets = MemorySecretStore::new();
    let it = open(&dir, &secrets).await;
    let mut tasks = Vec::new();
    for i in 0..20 {
        let it = it.clone();
        tasks.push(tokio::spawn(async move {
            if i % 2 == 0 {
                let _ = it.set_local_api_enabled(true).await;
            } else {
                let _ = it.regenerate_local_api_token().await;
            }
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    let stored = secrets.raw(TOKEN_KEY).expect("トークンが保存されている");
    assert!(it.gate().is_open(), "有効化を 10 回呼んだので開いている");
    assert!(it.gate().verify(&stored), "ゲートが通すトークン = 資格情報の中身");
    assert!(loaded(dir.path()).local_api.enabled, "ファイルも enabled");
    assert_eq!(it.local_api_token().await.unwrap(), stored);
}

// ---- AppConfig に載せない ----

#[test]
fn app_config_default_json_has_no_local_api_or_webhooks_keys() {
    let json = serde_json::to_string(&AppConfig::default()).unwrap();
    for key in ["localApi", "local_api", "webhooks", "integrations"] {
        assert!(!json.contains(&format!("\"{key}\"")), "AppConfig に {key} がある");
    }
}
