use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex},
};
pub trait SecretStore: Clone + Send + Sync + 'static {
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<String>, String>> + Send;
    fn put(&self, key: &str, value: &str) -> impl Future<Output = Result<(), String>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = Result<(), String>> + Send;
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
impl SecretStore for KeyringSecretStore {
    async fn get(&self, key: &str) -> Result<Option<String>, String> {
        crate::secret_store::load::<TokenRecord>(key)
            .await
            .map(|v| v.map(|r| r.token))
            .map_err(|_| "資格情報を読めません".into())
    }
    async fn put(&self, key: &str, value: &str) -> Result<(), String> {
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
        let mut m = self.0.lock().unwrap();
        if std::mem::take(&mut m.fail[2]) {
            return Err("資格情報を削除できません".into());
        }
        m.values.remove(key);
        Ok(())
    }
}
