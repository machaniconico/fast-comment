use super::{file, IntegrationsFile, LoadOutcome, SecretStore};
use crate::api::{generate_token, ApiGate};
use serde::Serialize;
use std::{
    path::PathBuf,
    sync::{Arc, RwLock},
};
use tokio::sync::Mutex;
const TOKEN_KEY: &str = "local-api-token";
const TOKEN_ERROR: &str = "トークンを読めません。再生成してください";
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalApiStatus {
    pub enabled: bool,
    pub has_token: bool,
    pub token_error: Option<String>,
    pub notice: Option<String>,
}
#[derive(Debug, thiserror::Error)]
pub enum IntegrationError {
    #[error("資格情報を保存できませんでした")]
    SecretWrite,
    #[error("トークンを読めません。再生成してください")]
    SecretRead,
    #[error("連携設定を保存できませんでした")]
    FileWrite,
}
struct Inner<S> {
    dir: PathBuf,
    secrets: S,
    file: Mutex<IntegrationsFile>,
    status: RwLock<LocalApiStatus>,
    gate: ApiGate,
}
#[derive(Clone)]
pub struct Integrations<S: SecretStore>(Arc<Inner<S>>);
fn valid_token(token: &str) -> bool {
    token.len() == 46
        && token.starts_with("fc_")
        && token[3..]
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
async fn read_token<S: SecretStore>(secrets: &S) -> Result<Option<String>, IntegrationError> {
    let raw = secrets
        .get(TOKEN_KEY)
        .await
        .map_err(|_| IntegrationError::SecretRead)?;
    raw.map(|value| {
        if valid_token(&value) {
            Ok(value)
        } else {
            Err(IntegrationError::SecretRead)
        }
    })
    .transpose()
}
impl<S: SecretStore> Integrations<S> {
    pub async fn open(dir: PathBuf, secrets: S, now_ms: u64) -> Self {
        let (file, notice) = match file::load(&dir, now_ms) {
            Ok(LoadOutcome::Loaded(file)) => (file, None),
            Ok(LoadOutcome::Missing) => (IntegrationsFile::default(), None),
            Ok(LoadOutcome::Corrupt { .. }) => (
                IntegrationsFile::default(),
                Some("設定ファイルが壊れていたため初期化しました".into()),
            ),
            Err(_) => (
                IntegrationsFile::default(),
                Some("連携設定を読めませんでした".into()),
            ),
        };
        let gate = ApiGate::closed();
        let token = read_token(&secrets).await;
        let has_token = matches!(&token, Ok(Some(_)));
        let token_error = if token.is_err() || (file.local_api.enabled && !has_token) {
            Some(TOKEN_ERROR.into())
        } else {
            None
        };
        if file.local_api.enabled {
            if let Ok(Some(token)) = token {
                gate.open(&token);
            }
        }
        let status = LocalApiStatus {
            enabled: file.local_api.enabled,
            has_token,
            token_error,
            notice,
        };
        Self(Arc::new(Inner {
            dir,
            secrets,
            file: Mutex::new(file),
            status: RwLock::new(status),
            gate,
        }))
    }
    pub fn gate(&self) -> ApiGate {
        self.0.gate.clone()
    }
    pub fn local_api_status(&self) -> LocalApiStatus {
        self.0.status.read().unwrap().clone()
    }
    async fn write_token(&self, token: &str) -> Result<(), IntegrationError> {
        self.0
            .secrets
            .put(TOKEN_KEY, token)
            .await
            .map_err(|_| IntegrationError::SecretWrite)
    }
    pub async fn set_local_api_enabled(
        &self,
        enabled: bool,
    ) -> Result<LocalApiStatus, IntegrationError> {
        let mut file = self.0.file.lock().await;
        let mut generated = false;
        let token = if enabled {
            match read_token(&self.0.secrets).await {
                Ok(Some(token)) => Some(token),
                Ok(None) => {
                    let token = generate_token();
                    self.write_token(&token).await?;
                    generated = true;
                    Some(token)
                }
                Err(e) => {
                    self.0.gate.close();
                    self.0.status.write().unwrap().token_error = Some(TOKEN_ERROR.into());
                    return Err(e);
                }
            }
        } else {
            None
        };
        let mut next = file.clone();
        next.local_api.enabled = enabled;
        if file::save(&self.0.dir, &next).is_err() {
            if generated && self.0.secrets.delete(TOKEN_KEY).await.is_err() {
                tracing::warn!("ローカル API 資格情報のロールバックに失敗しました");
            }
            return Err(IntegrationError::FileWrite);
        }
        *file = next;
        if let Some(token) = token {
            self.0.gate.open(&token);
        } else {
            self.0.gate.close();
        }
        let mut status = self.0.status.write().unwrap();
        status.enabled = enabled;
        if enabled {
            status.has_token = true;
            status.token_error = None;
        }
        Ok(status.clone())
    }
    pub async fn local_api_token(&self) -> Result<String, IntegrationError> {
        let _lock = self.0.file.lock().await;
        read_token(&self.0.secrets)
            .await?
            .ok_or(IntegrationError::SecretRead)
    }
    pub async fn regenerate_local_api_token(&self) -> Result<LocalApiStatus, IntegrationError> {
        let file = self.0.file.lock().await;
        let token = generate_token();
        self.write_token(&token).await?;
        if file.local_api.enabled {
            self.0.gate.open(&token);
        }
        let mut status = self.0.status.write().unwrap();
        status.has_token = true;
        status.token_error = None;
        Ok(status.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::MemorySecretStore;

    #[tokio::test]
    async fn failed_first_enable_removes_new_secret_and_keeps_gate_closed() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecretStore::new();
        let service = Integrations::open(dir.path().join("missing"), secrets.clone(), 1).await;
        assert!(service.set_local_api_enabled(true).await.is_err());
        assert!(secrets.keys().is_empty());
        assert!(!service.gate().is_open());
        assert!(!service.local_api_status().enabled);
    }

    #[tokio::test]
    async fn failed_file_save_preserves_existing_secret_and_gate() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecretStore::new();
        let service = Integrations::open(dir.path().to_path_buf(), secrets.clone(), 1).await;
        service.set_local_api_enabled(true).await.unwrap();
        let token = service.local_api_token().await.unwrap();
        std::fs::remove_file(dir.path().join(file::FILE_NAME)).unwrap();
        std::fs::create_dir(dir.path().join(file::FILE_NAME)).unwrap();
        assert!(service.set_local_api_enabled(false).await.is_err());
        assert!(service.gate().verify(&token));
        assert_eq!(service.local_api_token().await.unwrap(), token);
        assert!(service.local_api_status().enabled);
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
