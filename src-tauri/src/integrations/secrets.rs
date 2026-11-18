use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
pub trait SecretStore: Clone + Send + Sync + 'static {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<String>, String>> + Send;
    fn put(&self, key: &str, value: &str) -> impl Future<Output = Result<(), String>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), String>> + Send;
    /// Reports whether a previously observed entry was removed when the backend can distinguish it.
    fn delete_if_present(&self, key: &str) -> impl Future<Output = Result<bool, String>> + Send {
        async move { self.delete(key).await.map(|()| true) }
    }
}
#[derive(Clone)]
pub struct KeyringSecretStore;
// The trait exchanges secret strings; only the OS adapter wraps the on-disk record.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokenRecord {
    token: String,
    created_at_ms: u64,
}

pub(crate) fn token_fits_windows_credential(value: &str) -> bool {
    // Include the outer JSON escaping and reserve room for the timestamp at write time.
    let record = TokenRecord {
        token: value.into(),
        created_at_ms: u64::MAX,
    };
    serde_json::to_string(&record)
        .is_ok_and(|password| crate::secret_store::fits_windows_credential(&password))
}

impl SecretStore for KeyringSecretStore {
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        if key.starts_with("webhook:") {
            // 壊れた JSON もそのまま取得し、上書き・削除の失敗時に復元できるようにする。
            // 内容の検証は load_webhook_secrets で行う。
            let key = key.to_owned();
            let _guard = crate::secret_store::keyring_lock().lock().await;
            return tokio::task::spawn_blocking(move || {
                let entry = keyring::Entry::new("fast-comment", &key)
                    .map_err(|_| "資格情報を読めません".to_string())?;
                match entry.get_password() {
                    Ok(raw) => Ok(Some(raw)),
                    Err(keyring::Error::NoEntry) => Ok(None),
                    Err(_) => Err("資格情報を読めません".into()),
                }
            })
            .await
            .map_err(|_| "資格情報を読めません".to_string())?;
        }
        crate::secret_store::load::<TokenRecord>(key)
            .await
            .map(|v| v.map(|r| r.token))
            .map_err(|_| "資格情報を読めません".into())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), String> {
        if key.starts_with("webhook:") {
            let (key, value) = (key.to_owned(), value.to_owned());
            let _guard = crate::secret_store::keyring_lock().lock().await;
            return tokio::task::spawn_blocking(move || {
                keyring::Entry::new("fast-comment", &key)
                    .and_then(|entry| entry.set_password(&value))
                    .map_err(|_| "資格情報を保存できません".to_string())
            })
            .await
            .map_err(|_| "資格情報を保存できません".to_string())?;
        }
        let record = TokenRecord {
            token: value.into(),
            created_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64,
        };
        crate::secret_store::save(key, &record)
            .await
            .map_err(|_| "資格情報を保存できません".into())
    }
    async fn delete(&self, key: &str) -> Result<(), String> {
        crate::secret_store::delete(key)
            .await
            .map_err(|_| "資格情報を削除できません".into())
    }
    async fn delete_if_present(&self, key: &str) -> Result<bool, String> {
        crate::secret_store::delete_if_present(key)
            .await
            .map_err(|_| "資格情報を削除できません".to_string())
    }
}
#[derive(Default)]
struct Memory {
    values: HashMap<String, String>,
    fail: [bool; 3],
}
#[derive(Clone, Default)]
pub struct MemorySecretStore(Arc<Mutex<Memory>>);
impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn fail_next_get(&self) {
        self.0.lock().unwrap().fail[0] = true;
    }
    pub fn fail_next_put(&self) {
        self.0.lock().unwrap().fail[1] = true;
    }
    pub fn fail_next_delete(&self) {
        self.0.lock().unwrap().fail[2] = true;
    }
    pub fn raw(&self, key: &str) -> Option<String> {
        self.0.lock().unwrap().values.get(key).cloned()
    }
    pub fn keys(&self) -> Vec<String> {
        self.0.lock().unwrap().values.keys().cloned().collect()
    }
}
impl SecretStore for MemorySecretStore {
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        let mut m = self.0.lock().unwrap();
        if std::mem::take(&mut m.fail[0]) {
            return Err("資格情報を読めません".into());
        }
        Ok(m.values.get(key).cloned())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), String> {
        let mut m = self.0.lock().unwrap();
        if std::mem::take(&mut m.fail[1]) {
            return Err("資格情報を保存できません".into());
        }
        m.values.insert(key.into(), value.into());
        Ok(())
    }
    async fn delete(&self, key: &str) -> Result<(), String> {
        self.delete_if_present(key).await.map(|_| ())
    }
    async fn delete_if_present(&self, key: &str) -> Result<bool, String> {
        let mut m = self.0.lock().unwrap();
        if std::mem::take(&mut m.fail[2]) {
            return Err("資格情報を削除できません".into());
        }
        Ok(m.values.remove(key).is_some())
    }
}
