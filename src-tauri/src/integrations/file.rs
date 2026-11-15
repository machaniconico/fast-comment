use serde::{Deserialize, Serialize};
use std::{
    io::{self, Write},
    path::{Path, PathBuf},
};
pub const FILE_NAME: &str = "integrations.json";
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct IntegrationsFile {
    pub version: u32,
    pub local_api: LocalApiSettings,
    pub webhooks: Vec<WebhookDef>,
}
impl Default for IntegrationsFile {
    fn default() -> Self {
        Self {
            version: 1,
            local_api: LocalApiSettings::default(),
            webhooks: Vec::new(),
        }
    }
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct LocalApiSettings {
    pub enabled: bool,
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookDef {
    pub id: String,
    pub name: String,
    pub format: WebhookFormat,
    pub enabled: bool,
    pub triggers: WebhookTriggers,
    pub keywords: Vec<String>,
    pub paused: Option<Paused>,
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum WebhookFormat {
    #[default]
    Discord,
    Generic,
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WebhookTriggers {
    pub donation: bool,
    pub membership: bool,
    pub keyword: bool,
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Paused {
    pub reason: String,
    pub at_ms: u64,
}
#[derive(Debug)]
pub enum LoadOutcome {
    Loaded(IntegrationsFile),
    Missing,
    Corrupt { backup: PathBuf },
}
pub fn load(dir: &Path, now_ms: u64) -> io::Result<LoadOutcome> {
    let path = dir.join(FILE_NAME);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(LoadOutcome::Missing),
        Err(e) => return Err(e),
    };
    if let Ok(file) = serde_json::from_slice::<IntegrationsFile>(&bytes) {
        if file.version == 1 {
            return Ok(LoadOutcome::Loaded(file));
        }
    }
    let backup = dir.join(format!("{FILE_NAME}.corrupt-{now_ms}"));
    std::fs::rename(path, &backup)?;
    Ok(LoadOutcome::Corrupt { backup })
}
pub fn save(dir: &Path, file: &IntegrationsFile) -> io::Result<()> {
    let temp = dir.join(format!(".{FILE_NAME}.{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut output = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)?;
        output.write_all(&serde_json::to_vec_pretty(file)?)?;
        output.sync_all()?;
        drop(output);
        std::fs::rename(&temp, dir.join(FILE_NAME))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temp);
    }
    result
}
