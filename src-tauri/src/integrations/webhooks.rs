//! 定義と資格情報を一つの変更ロックで更新する。
use super::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookInput {
    pub id: Option<String>,
    pub name: String,
    pub format: WebhookFormat,
    pub enabled: bool,
    pub triggers: WebhookTriggers,
    pub keywords: Vec<String>,
    pub url: Option<String>,
}
impl std::fmt::Debug for WebhookInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookInput { [redacted] }")
    }
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WebhookView {
    #[serde(flatten)]
    pub definition: WebhookDef,
    pub has_url: bool,
    pub url_preview: Option<String>,
}
#[derive(Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct WebhookSecret {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signing_secret: Option<String>,
}
impl std::fmt::Debug for WebhookSecret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WebhookSecret { [redacted] }")
    }
}
pub(crate) type WebhookSnapshot = Vec<(WebhookDef, Option<WebhookSecret>)>;
fn view(d: &WebhookDef, s: Option<&WebhookSecret>) -> WebhookView {
    WebhookView {
        definition: d.clone(),
        has_url: s.is_some(),
        url_preview: s.map(|s| crate::webhook::target::redact(&s.url)),
    }
}
fn key(id: &str) -> String {
    format!("webhook:{id}")
}
fn generate_secret() -> Result<String, IntegrationError> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|_| IntegrationError::SecretWrite)?;
    Ok(hex::encode(bytes))
}
pub(crate) async fn load_webhook_secrets<S: SecretStore>(
    file: &IntegrationsFile,
    store: &S,
) -> WebhookSnapshot {
    let mut result = Vec::new();
    for d in &file.webhooks {
        let secret = store
            .get(&key(&d.id))
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<WebhookSecret>(&s).ok())
            .filter(|s| {
                crate::webhook::target::validate(d.format.clone(), &s.url).is_ok()
                    && (d.format != WebhookFormat::Generic
                        || s.signing_secret.as_ref().is_some_and(|k| !k.is_empty()))
            });
        result.push((d.clone(), secret));
    }
    result
}
impl<S: SecretStore> Integrations<S> {
    pub fn list_webhooks(&self) -> Vec<WebhookView> {
        self.0
            .webhooks
            .read()
            .unwrap()
            .iter()
            .map(|(d, s)| view(d, s.as_ref()))
            .collect()
    }
    pub(crate) fn webhook_snapshot(&self) -> WebhookSnapshot {
        self.0.webhooks.read().unwrap().clone()
    }
    async fn refresh_webhooks(&self, file: &IntegrationsFile) {
        let mut snapshot = load_webhook_secrets(file, &self.0.secrets).await;
        // mark_auto_paused と同じ順(webhooks → auto_paused)で取り、記録を取りこぼさない。
        let mut current = self.0.webhooks.write().unwrap();
        let auto = self.0.auto_paused.lock().unwrap();
        for (d, s) in &mut snapshot {
            if let Some((p, secret)) = auto.get(&d.id) {
                if d.paused.is_none() && s.as_ref() == Some(secret) {
                    d.paused = Some(p.clone());
                }
            }
        }
        *current = snapshot;
    }
    /// 自動停止を await せずにメモリへ反映する。永続化は後から pause_webhook で行う。
    /// 名前などの変更は問わず、URL/署名鍵が送った時と違えば反映せず false を返す。
    pub(crate) fn mark_auto_paused(
        &self,
        def: &WebhookDef,
        secret: &WebhookSecret,
        paused: Paused,
    ) -> bool {
        let mut snapshot = self.0.webhooks.write().unwrap();
        let Some((d, _)) = snapshot
            .iter_mut()
            .find(|(d, s)| d.id == def.id && s.as_ref() == Some(secret))
        else {
            return false;
        };
        d.paused = Some(paused.clone());
        self.0
            .auto_paused
            .lock()
            .unwrap()
            .insert(def.id.clone(), (paused, secret.clone()));
        true
    }
    pub async fn save_webhook(&self, input: WebhookInput) -> Result<WebhookView, IntegrationError> {
        let mut file = self.0.file.lock().await;
        self.ensure_writable()?;
        let existing = match &input.id {
            Some(id) => Some(
                file.webhooks
                    .iter()
                    .position(|d| &d.id == id)
                    .ok_or(IntegrationError::InvalidWebhook)?,
            ),
            None => None,
        };
        if existing.is_none() && file.webhooks.len() >= 10 {
            return Err(IntegrationError::InvalidWebhook);
        }
        let name = input.name.trim().to_string();
        let keywords: Vec<String> = input
            .keywords
            .iter()
            .map(|k| k.trim())
            .filter(|k| !k.is_empty())
            .map(str::to_owned)
            .collect();
        if name.is_empty()
            || name.chars().count() > 64
            || keywords.len() > 20
            || keywords.iter().any(|k| k.chars().count() > 64)
        {
            return Err(IntegrationError::InvalidWebhook);
        }
        let id = input.id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
        let old_raw = self
            .0
            .secrets
            .get(&key(&id))
            .await
            .map_err(|_| IntegrationError::WebhookSecretRead)?;
        let old: Option<WebhookSecret> = old_raw
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok());
        let url = input
            .url
            .or_else(|| old.as_ref().map(|s| s.url.clone()))
            .ok_or(IntegrationError::InvalidWebhook)?;
        crate::webhook::target::validate(input.format.clone(), &url)
            .map_err(|_| IntegrationError::InvalidWebhook)?;
        let signing_secret = if input.format == WebhookFormat::Generic {
            Some(match old.as_ref().and_then(|s| s.signing_secret.clone()) {
                Some(s) => s,
                None => generate_secret()?,
            })
        } else {
            None
        };
        let secret = WebhookSecret {
            url,
            signing_secret,
        };
        let d = WebhookDef {
            id: id.clone(),
            name,
            format: input.format,
            enabled: input.enabled,
            triggers: input.triggers,
            keywords,
            paused: existing.and_then(|i| file.webhooks[i].paused.clone()),
        };
        let raw = serde_json::to_string(&secret).map_err(|_| IntegrationError::SecretWrite)?;
        self.0
            .secrets
            .put(&key(&id), &raw)
            .await
            .map_err(|_| IntegrationError::SecretWrite)?;
        let mut next = file.clone();
        if let Some(i) = existing {
            next.webhooks[i] = d.clone();
        } else {
            next.webhooks.push(d.clone());
        }
        if file::save(&self.0.dir, &next).is_err() {
            self.restore_secret(&id, old_raw.as_deref()).await;
            return Err(IntegrationError::FileWrite);
        }
        // 送り先(URL・署名鍵・形式)が変われば、旧送り先の自動停止の記録は使わない。
        // 戻した時にメモリだけの停止が蘇らないよう消す。
        if old.as_ref() != Some(&secret)
            || existing.is_some_and(|i| file.webhooks[i].format != d.format)
        {
            self.0.auto_paused.lock().unwrap().remove(&id);
        }
        *file = next;
        self.refresh_webhooks(&file).await;
        Ok(view(&d, Some(&secret)))
    }
    async fn restore_secret(&self, id: &str, raw: Option<&str>) {
        let result = if let Some(raw) = raw {
            self.0.secrets.put(&key(id), raw).await
        } else {
            self.0.secrets.delete(&key(id)).await
        };
        if result.is_err() {
            tracing::warn!("Webhook 資格情報の復元に失敗しました");
        }
    }
    pub async fn delete_webhook(&self, id: &str) -> Result<(), IntegrationError> {
        let mut file = self.0.file.lock().await;
        self.ensure_writable()?;
        let i = file
            .webhooks
            .iter()
            .position(|d| d.id == id)
            .ok_or(IntegrationError::InvalidWebhook)?;
        let old = self
            .0
            .secrets
            .get(&key(id))
            .await
            .map_err(|_| IntegrationError::WebhookSecretRead)?;
        self.0
            .secrets
            .delete(&key(id))
            .await
            .map_err(|_| IntegrationError::SecretWrite)?;
        let mut next = file.clone();
        next.webhooks.remove(i);
        if file::save(&self.0.dir, &next).is_err() {
            self.restore_secret(id, old.as_deref()).await;
            return Err(IntegrationError::FileWrite);
        }
        *file = next;
        self.0.auto_paused.lock().unwrap().remove(id);
        self.refresh_webhooks(&file).await;
        Ok(())
    }
    pub async fn resume_webhook(&self, id: &str) -> Result<(), IntegrationError> {
        self.set_paused(id, None, None).await
    }
    /// mark_auto_paused で反映した自動停止をディスクへ保存する。
    pub(crate) async fn pause_webhook(
        &self,
        def: &WebhookDef,
        secret: &WebhookSecret,
        paused: Paused,
    ) -> Result<(), IntegrationError> {
        self.set_paused(&def.id, Some(paused), Some((def, secret)))
            .await
    }
    async fn set_paused(
        &self,
        id: &str,
        paused: Option<Paused>,
        expected: Option<(&WebhookDef, &WebhookSecret)>,
    ) -> Result<(), IntegrationError> {
        let mut file = self.0.file.lock().await;
        self.ensure_writable()?;
        let i = file
            .webhooks
            .iter()
            .position(|d| d.id == id)
            .ok_or(IntegrationError::InvalidWebhook)?;
        // URL/署名鍵を変更した直後に、旧ワーカーの失敗で新しい送り先を停止しない。
        // 照合は mark_auto_paused と同じく id と資格情報だけ。名前などの変更と競合しても
        // 停止は保存する。反映後に再開・削除・送り先変更があれば記録が消えており、保存しない。
        if expected.is_some_and(|(d, secret)| {
            let marked =
                matches!(self.0.auto_paused.lock().unwrap().get(id), Some((_, s)) if s == secret);
            !marked
                || !self
                    .0
                    .webhooks
                    .read()
                    .unwrap()
                    .iter()
                    .any(|(current, s)| current.id == d.id && s.as_ref() == Some(secret))
        }) {
            return Err(IntegrationError::InvalidWebhook);
        }
        let mut next = file.clone();
        let auto_pause = paused.is_some();
        next.webhooks[i].paused = paused;
        let saved = file::save(&self.0.dir, &next).map_err(|_| IntegrationError::FileWrite);
        // 自動停止は保存失敗時もメモリに残す。無関係な保存・reload で解除しない。
        // 明示的な再開は保存成功後にだけ反映する。
        if saved.is_ok() || auto_pause {
            if !auto_pause {
                self.0.auto_paused.lock().unwrap().remove(id);
            }
            *file = next;
            self.refresh_webhooks(&file).await;
        }
        saved
    }
    pub async fn reveal_signing_secret(&self, id: &str) -> Result<String, IntegrationError> {
        let file = self.0.file.lock().await;
        self.read_signing_secret(&file, id)
            .await
            .map(|s| s.signing_secret.unwrap())
    }
    async fn read_signing_secret(
        &self,
        file: &IntegrationsFile,
        id: &str,
    ) -> Result<WebhookSecret, IntegrationError> {
        if !file
            .webhooks
            .iter()
            .any(|d| d.id == id && d.format == WebhookFormat::Generic)
        {
            return Err(IntegrationError::InvalidWebhook);
        }
        let raw = self
            .0
            .secrets
            .get(&key(id))
            .await
            .map_err(|_| IntegrationError::WebhookSecretRead)?
            .ok_or(IntegrationError::WebhookSecretRead)?;
        serde_json::from_str::<WebhookSecret>(&raw)
            .ok()
            .filter(|s| s.signing_secret.is_some())
            .ok_or(IntegrationError::WebhookSecretRead)
    }
    pub async fn regenerate_signing_secret(&self, id: &str) -> Result<String, IntegrationError> {
        let file = self.0.file.lock().await;
        self.ensure_writable()?;
        let mut secret = self.read_signing_secret(&file, id).await?;
        let new = generate_secret()?;
        secret.signing_secret = Some(new.clone());
        self.0
            .secrets
            .put(
                &key(id),
                &serde_json::to_string(&secret).map_err(|_| IntegrationError::SecretWrite)?,
            )
            .await
            .map_err(|_| IntegrationError::SecretWrite)?;
        self.0.auto_paused.lock().unwrap().remove(id);
        self.refresh_webhooks(&file).await;
        Ok(new)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn input(id: Option<String>, url: Option<&str>) -> WebhookInput {
        WebhookInput {
            id,
            name: "通知".into(),
            format: WebhookFormat::Generic,
            enabled: true,
            triggers: WebhookTriggers {
                donation: true,
                ..Default::default()
            },
            keywords: vec![],
            url: url.map(str::to_owned),
        }
    }
    #[tokio::test]
    async fn failed_edit_and_delete_restore_secret_and_cached_view() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecretStore::new();
        let it = Integrations::open(dir.path().into(), secrets.clone(), 1).await;
        let saved = it
            .save_webhook(input(None, Some("https://example.test/original")))
            .await
            .unwrap();
        let id = saved.definition.id;
        let before = secrets.raw(&key(&id));
        let before_view = serde_json::to_value(it.list_webhooks()).unwrap();
        let original_file = std::fs::read(dir.path().join(FILE_NAME)).unwrap();
        std::fs::remove_file(dir.path().join(FILE_NAME)).unwrap();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        assert!(matches!(
            it.save_webhook(input(
                Some(id.clone()),
                Some("https://example.test/changed")
            ))
            .await,
            Err(IntegrationError::FileWrite)
        ));
        assert_eq!(secrets.raw(&key(&id)), before);
        assert_eq!(
            serde_json::to_value(it.list_webhooks()).unwrap(),
            before_view
        );
        assert!(matches!(
            it.delete_webhook(&id).await,
            Err(IntegrationError::FileWrite)
        ));
        assert_eq!(secrets.raw(&key(&id)), before);
        assert_eq!(
            serde_json::to_value(it.list_webhooks()).unwrap(),
            before_view
        );
        // 復元された定義と秘密は、再度保存可能になった後にも一致する。
        std::fs::remove_dir(dir.path().join(FILE_NAME)).unwrap();
        std::fs::write(dir.path().join(FILE_NAME), original_file).unwrap();
        let reopened = Integrations::open(dir.path().into(), secrets, 2).await;
        assert_eq!(
            serde_json::to_value(reopened.list_webhooks()).unwrap(),
            before_view
        );
    }
    #[tokio::test]
    async fn api_and_webhook_mutations_share_one_transaction_lock() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let (a, b, c) = tokio::join!(
            it.set_local_api_enabled(true),
            it.save_webhook(input(None, Some("https://example.test/a"))),
            it.save_webhook(input(None, Some("https://example.test/b")))
        );
        a.unwrap();
        b.unwrap();
        c.unwrap();
        let LoadOutcome::Loaded(file) = load(dir.path(), 2).unwrap() else {
            panic!("saved file");
        };
        assert!(file.local_api.enabled);
        assert_eq!(file.webhooks.len(), 2);
        assert_eq!(it.list_webhooks().len(), 2);
        assert!(it.gate().is_open());
    }
    #[tokio::test]
    async fn old_delivery_failure_cannot_pause_a_replaced_url() {
        let dir = tempfile::tempdir().unwrap();
        let it = Integrations::open(dir.path().into(), MemorySecretStore::new(), 1).await;
        let saved = it
            .save_webhook(input(None, Some("https://example.test/old")))
            .await
            .unwrap();
        let (old_def, old_secret) = it.webhook_snapshot().remove(0);
        let id = saved.definition.id;
        it.save_webhook(input(Some(id.clone()), Some("https://example.test/new")))
            .await
            .unwrap();
        assert!(it
            .pause_webhook(
                &old_def,
                old_secret.as_ref().unwrap(),
                Paused {
                    reason: "unauthorized".into(),
                    at_ms: 2
                }
            )
            .await
            .is_err());
        assert!(it.list_webhooks()[0].definition.paused.is_none());
        let LoadOutcome::Loaded(file) = load(dir.path(), 3).unwrap() else {
            panic!("saved file");
        };
        assert!(file.webhooks[0].paused.is_none());
    }
    #[tokio::test]
    async fn corrupt_load_protection_applies_to_webhook_mutations() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join(FILE_NAME)).unwrap();
        let secrets = MemorySecretStore::new();
        let it = Integrations::open(dir.path().into(), secrets.clone(), 1).await;
        assert!(matches!(
            it.save_webhook(input(None, Some("https://example.test/a")))
                .await,
            Err(IntegrationError::FileWrite)
        ));
        assert!(matches!(
            it.delete_webhook("id").await,
            Err(IntegrationError::FileWrite)
        ));
        assert!(matches!(
            it.resume_webhook("id").await,
            Err(IntegrationError::FileWrite)
        ));
        assert!(matches!(
            it.regenerate_signing_secret("id").await,
            Err(IntegrationError::FileWrite)
        ));
        assert!(secrets.keys().is_empty());
    }
    #[tokio::test]
    async fn regression_corrupt_credentials_can_be_replaced_deleted_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let secrets = MemorySecretStore::new();
        let it = Integrations::open(dir.path().into(), secrets.clone(), 1).await;
        let id = it
            .save_webhook(input(None, Some("https://example.test/old")))
            .await
            .unwrap()
            .definition
            .id;
        for delete in [false, true] {
            secrets.put(&key(&id), "not json").await.unwrap();
            let path = dir.path().join(FILE_NAME);
            let original = std::fs::read(&path).unwrap();
            std::fs::remove_file(&path).unwrap();
            std::fs::create_dir(&path).unwrap();
            let failed = if delete {
                it.delete_webhook(&id).await.map(|_| ())
            } else {
                it.save_webhook(input(Some(id.clone()), Some("https://example.test/new")))
                    .await
                    .map(|_| ())
            };
            assert!(matches!(failed, Err(IntegrationError::FileWrite)));
            assert_eq!(secrets.get(&key(&id)).await.unwrap().unwrap(), "not json");
            std::fs::remove_dir(&path).unwrap();
            std::fs::write(&path, original).unwrap();
            if delete {
                it.delete_webhook(&id).await.unwrap();
            } else {
                assert!(
                    it.save_webhook(input(Some(id.clone()), Some("https://example.test/new")))
                        .await
                        .unwrap()
                        .has_url
                );
            }
        }
        assert_eq!(secrets.get(&key(&id)).await.unwrap(), None);
        assert!(it.list_webhooks().is_empty());
    }
}
