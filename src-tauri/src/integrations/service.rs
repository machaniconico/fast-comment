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
    pub writes_blocked: bool,
    pub has_token: bool,
    pub token_error: Option<String>,
    pub notice: Option<String>,
}
#[derive(Debug, Serialize, thiserror::Error)]
pub enum IntegrationError {
    #[error("メインウィンドウからのみ操作できます")]
    ForbiddenWindow,
    #[error("Webhook の設定が正しくありません")]
    InvalidWebhook,
    #[error("Webhook の資格情報を読めません。URL を設定し直してください")]
    WebhookSecretRead,
    #[error("資格情報を保存できませんでした")]
    SecretWrite,
    #[error("トークンを読めません。再生成してください")]
    SecretRead,
    #[error("連携設定を保存できませんでした")]
    FileWrite,
}
pub(super) struct Inner<S> {
    pub(super) dir: PathBuf,
    // A failed load may conceal user data. Only a successful reopen can permit mutations.
    writes_blocked: bool,
    pub(super) secrets: S,
    pub(super) file: Mutex<IntegrationsFile>,
    pub(super) webhooks: RwLock<super::WebhookSnapshot>,
    status: RwLock<LocalApiStatus>,
    gate: ApiGate,
}
#[derive(Clone)]
pub struct Integrations<S: SecretStore>(pub(super) Arc<Inner<S>>);
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
        let (file, notice, writes_blocked) = match file::load(&dir, now_ms) {
            Ok(LoadOutcome::Loaded(file)) => (file, None, false),
            Ok(LoadOutcome::Missing) => (IntegrationsFile::default(), None, false),
            Ok(LoadOutcome::Corrupt { .. }) => (
                IntegrationsFile::default(),
                Some("設定ファイルが壊れていたため初期化しました".into()),
                false,
            ),
            Err(_) => (
                IntegrationsFile::default(),
                Some(
                    "連携設定を読めませんでした。設定ファイルを確認してアプリを再起動してください"
                        .into(),
                ),
                true,
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
        let webhooks = super::load_webhook_secrets(&file, &secrets).await;
        let status = LocalApiStatus {
            enabled: file.local_api.enabled,
            has_token,
            writes_blocked,
            token_error,
            notice,
        };
        Self(Arc::new(Inner {
            dir,
            writes_blocked,
            secrets,
            file: Mutex::new(file),
            webhooks: RwLock::new(webhooks),
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
    pub(super) fn ensure_writable(&self) -> Result<(), IntegrationError> {
        if self.0.writes_blocked {
            Err(IntegrationError::FileWrite)
        } else {
            Ok(())
        }
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
        self.ensure_writable()?;
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
        self.ensure_writable()?;
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
    async fn load_error_blocks_mutations_and_preserves_replacement_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file::FILE_NAME);
        std::fs::create_dir(&path).unwrap();
        let secrets = MemorySecretStore::new();
        secrets.fail_next_get();
        let service = Integrations::open(dir.path().to_path_buf(), secrets.clone(), 1).await;
        let status = service.local_api_status();
        assert!(status.writes_blocked);
        assert!(status.token_error.is_some());
        assert!(status.notice.as_deref().unwrap().contains("再起動"));
        assert_eq!(
            serde_json::to_value(&status).unwrap()["writesBlocked"],
            true
        );
        std::fs::remove_dir(&path).unwrap();
        let saved = IntegrationsFile {
            webhooks: vec![crate::integrations::WebhookDef {
                id: "existing".into(),
                name: "Existing webhook".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        file::save(dir.path(), &saved).unwrap();
        let before = std::fs::read(&path).unwrap();
        for enabled in [true, false] {
            assert!(matches!(
                service.set_local_api_enabled(enabled).await,
                Err(IntegrationError::FileWrite)
            ));
        }
        assert!(matches!(
            service.regenerate_local_api_token().await,
            Err(IntegrationError::FileWrite)
        ));
        let LoadOutcome::Loaded(after) = file::load(dir.path(), 2).unwrap() else {
            panic!("file must remain valid")
        };
        assert_eq!(after.webhooks.len(), 1);
        assert_eq!(after, saved);
        assert_eq!(std::fs::read(&path).unwrap(), before);
        assert!(secrets.keys().is_empty());
        assert!(!service.gate().is_open());
    }

    #[tokio::test]
    async fn failed_corrupt_backup_blocks_mutations() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(file::FILE_NAME);
        std::fs::write(&path, b"broken json").unwrap();
        std::fs::create_dir(dir.path().join("integrations.json.corrupt-1")).unwrap();
        let secrets = MemorySecretStore::new();
        let service = Integrations::open(dir.path().to_path_buf(), secrets.clone(), 1).await;
        assert!(service.local_api_status().writes_blocked);
        assert!(matches!(
            service.set_local_api_enabled(true).await,
            Err(IntegrationError::FileWrite)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), b"broken json");
        assert!(secrets.keys().is_empty());
    }

    #[tokio::test]
    async fn failed_first_enable_removes_new_secret_and_keeps_gate_closed() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecretStore::new();
        let service = Integrations::open(dir.path().join("missing"), secrets.clone(), 1).await;
        assert!(!service.local_api_status().writes_blocked);
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
