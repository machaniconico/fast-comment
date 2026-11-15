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
    #[serde(default)]
    pub local_api: LocalApiSettings,
    #[serde(default)]
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
#[serde(default, rename_all = "camelCase")]
pub struct LocalApiSettings {
    pub enabled: bool,
}
#[derive(Serialize, Deserialize, Default, Clone, Debug, PartialEq)]
#[serde(default, rename_all = "camelCase")]
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
#[serde(default, rename_all = "camelCase")]
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
    let shaped = serde_json::from_slice::<serde_json::Value>(&bytes)
        .ok()
        .filter(has_expected_shape);
    if let Some(file) = shaped.and_then(|v| serde_json::from_value::<IntegrationsFile>(v).ok()) {
        if file.version == 1 {
            return Ok(LoadOutcome::Loaded(file));
        }
    }
    let backup = dir.join(format!("{FILE_NAME}.corrupt-{now_ms}"));
    std::fs::rename(path, &backup)?;
    Ok(LoadOutcome::Corrupt { backup })
}
/// 派生 `Deserialize` は構造体を配列(位置引数)からも受け取るので、
/// 構造体の位置にオブジェクト以外が来たファイルは破損として扱う。
fn has_expected_shape(root: &serde_json::Value) -> bool {
    fn object_or_absent(v: Option<&serde_json::Value>) -> bool {
        v.map_or(true, serde_json::Value::is_object)
    }
    let Some(root) = root.as_object() else {
        return false;
    };
    if !object_or_absent(root.get("localApi")) {
        return false;
    }
    match root.get("webhooks") {
        None => true,
        Some(serde_json::Value::Array(items)) => items.iter().all(|item| {
            item.as_object().is_some_and(|w| {
                object_or_absent(w.get("triggers"))
                    && w.get("paused")
                        .map_or(true, |p| p.is_null() || p.is_object())
            })
        }),
        Some(_) => false,
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn struct_given_as_array_is_treated_as_corrupt() {
        for input in [
            r#"[1,{"enabled":true},[]]"#,
            r#"{"version":1,"localApi":[true]}"#,
            r#"{"version":1,"webhooks":[["a","b"]]}"#,
            r#"{"version":1,"webhooks":[{"triggers":[true,true,true]}]}"#,
            r#"{"version":1,"webhooks":[{"paused":["x",1]}]}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(FILE_NAME), input).unwrap();
            assert!(
                matches!(load(dir.path(), 7).unwrap(), LoadOutcome::Corrupt { .. }),
                "受理された: {input}"
            );
        }
    }

    #[test]
    fn object_shaped_file_with_null_paused_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"version":1,"localApi":{"enabled":true},"webhooks":[{"paused":null,"triggers":{}}]}"#,
        )
        .unwrap();
        match load(dir.path(), 7).unwrap() {
            LoadOutcome::Loaded(file) => assert!(file.local_api.enabled),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_webhooks_loads_with_defaults_without_backup() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            r#"{"version":1,"localApi":{"enabled":true}}"#,
        )
        .unwrap();
        let LoadOutcome::Loaded(file) = load(dir.path(), 1).unwrap() else {
            panic!("optional fields must not make the file corrupt");
        };
        assert!(file.local_api.enabled);
        assert!(file.webhooks.is_empty());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn missing_version_is_backed_up_as_corrupt() {
        for input in [
            r#"{"localApi":{}}"#,
            "{}",
            r#"{"foo":1}"#,
            r#"{"webhooks":[]}"#,
        ] {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join(FILE_NAME);
            std::fs::write(&path, input).unwrap();
            let LoadOutcome::Corrupt { backup } = load(dir.path(), 42).unwrap() else {
                panic!("missing version must be corrupt: {input}");
            };
            assert!(!path.exists());
            assert_eq!(backup, dir.path().join("integrations.json.corrupt-42"));
            assert_eq!(std::fs::read_to_string(backup).unwrap(), input);
        }
    }

    #[test]
    fn nested_settings_default_but_paused_requires_reason_and_time() {
        let file: IntegrationsFile =
            serde_json::from_str(r#"{"version":1,"localApi":{},"webhooks":[{"triggers":{}}]}"#)
                .unwrap();
        assert_eq!(
            serde_json::from_str::<IntegrationsFile>(r#"{"version":1}"#).unwrap(),
            IntegrationsFile::default()
        );
        assert!(!file.local_api.enabled);
        assert_eq!(file.webhooks[0], WebhookDef::default());
        for paused in [r#"{}"#, r#"{"reason":"unauthorized"}"#, r#"{"atMs":1}"#] {
            assert!(serde_json::from_str::<Paused>(paused).is_err());
        }
    }
}
