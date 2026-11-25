//! 起動時に開く項目の検証・承認・実行(SPEC §20)。

use std::{fs, io::Write, path::Path, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const FILE_NAME: &str = "launch.json";
const VERSION: u32 = 1;
const APPROVAL_USER: &str = "launch-approval";
const KEY_LEN: usize = 32;
type HmacSha256 = Hmac<Sha256>;

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind {
    Exe,
    Url,
}

#[derive(Serialize, Deserialize)]
pub struct LaunchItem {
    pub id: String,
    pub kind: LaunchKind,
    pub target: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub label: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("保存するには内容の確認と承認が必要です")]
    NotConfirmed,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("起動時に開く設定のファイル操作に失敗しました")]
    Io(String),
    #[error("起動時に開く設定の承認情報を保存または取得できませんでした")]
    Store(String),
}

fn io_error(error: std::io::Error) -> LaunchError {
    LaunchError::Io(format!("{:?}", error.kind()))
}

pub fn validate_items(items: &[LaunchItem]) -> Result<(), LaunchError> {
    if items.len() > 20 {
        return Err(LaunchError::Invalid("登録できる項目は20件までです"));
    }
    for item in items {
        if item.target.chars().count() > 2048 {
            return Err(LaunchError::Invalid("パスとURLは2048文字までです"));
        }
        if item.args.len() > 32 || item.args.iter().any(|arg| arg.chars().count() > 1024) {
            return Err(LaunchError::Invalid(
                "引数は32個まで、1個あたり1024文字までです",
            ));
        }
        if item.args.iter().any(|arg| arg.contains('\0')) {
            return Err(LaunchError::Invalid("引数にNUL文字は使えません"));
        }
        if item
            .label
            .as_ref()
            .is_some_and(|label| label.chars().count() > 100)
        {
            return Err(LaunchError::Invalid("表示名は100文字までです"));
        }
        match item.kind {
            LaunchKind::Exe if !is_absolute_exe(&item.target) => {
                return Err(LaunchError::Invalid(
                    "アプリには拡張子が .exe の Windows 絶対パスを指定してください",
                ));
            }
            LaunchKind::Url if !is_web_url(&item.target) => {
                return Err(LaunchError::Invalid(
                    "ユーザー情報を含まない http:// または https:// のURLを指定してください",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn is_absolute_exe(target: &str) -> bool {
    if target
        .chars()
        .any(|ch| ch.is_control() || matches!(ch, '"' | '<' | '>' | '|' | '*'))
    {
        return false;
    }

    // Path::is_absolute はホスト OS の規則で判定するため、Windows の
    // ドライブ付きパスと UNC をここで判定する。デバイスパスと代替ストリームは除く。
    let normalized = target.replace('/', "\\");
    let drive_tail = |path: &str| {
        matches!(path.as_bytes(), [drive, b':', b'\\', ..] if drive.is_ascii_alphabetic())
            .then_some(3)
    };
    let unc_tail = |path: &str| {
        let mut parts = path.splitn(3, '\\');
        let server = parts.next()?;
        let share = parts.next()?;
        if server.is_empty()
            || share.is_empty()
            || matches!(server, "." | "..")
            || matches!(share, "." | "..")
        {
            return None;
        }
        parts.next().map(|_| server.len() + share.len() + 2)
    };
    let path = if let Some(rest) = target.strip_prefix(r"\\?\") {
        // Verbatim パスではスラッシュの読み替えは行われない。
        if rest.contains('/') {
            return false;
        }
        if let Some(unc) = rest.get(..4).filter(|p| p.eq_ignore_ascii_case("UNC\\")) {
            unc_tail(&rest[unc.len()..]).map(|start| &rest[unc.len() + start..])
        } else {
            drive_tail(rest).map(|start| &rest[start..])
        }
    } else if let Some(rest) = normalized.strip_prefix(r"\\") {
        if rest.contains('?') || rest.contains(':') {
            return false;
        }
        unc_tail(rest).map(|start| &rest[start..])
    } else {
        drive_tail(&normalized).map(|start| &normalized[start..])
    };
    path.is_some_and(|path| {
        !path.contains([':', '?'])
            && path.rsplit('\\').next().is_some_and(|file| {
                Path::new(file)
                    .extension()
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
            })
    })
}

fn is_web_url(target: &str) -> bool {
    let Some(rest) = target
        .strip_prefix("https://")
        .or_else(|| target.strip_prefix("http://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    !authority.is_empty()
        && !authority.contains('@')
        && !target.contains('\\')
        && target.trim() == target
        && !target.chars().any(char::is_control)
        && url::Url::parse(target).is_ok_and(|url| {
            url.host().is_some() && url.username().is_empty() && url.password().is_none()
        })
}

fn approval_hmac(key: &[u8], items: &[LaunchItem]) -> HmacSha256 {
    // Serialize のフィールド宣言順を正規化 JSON のキー順として固定する。
    #[derive(Serialize)]
    struct ApprovedItem<'a> {
        id: &'a str,
        kind: LaunchKind,
        target: &'a str,
        args: &'a [String],
        enabled: bool,
    }

    let approved: Vec<_> = items
        .iter()
        .map(|item| ApprovedItem {
            id: &item.id,
            kind: item.kind,
            target: &item.target,
            args: &item.args,
            enabled: item.enabled,
        })
        .collect();
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&serde_json::to_vec(&approved).expect("launch items are JSON serializable"));
    mac
}

pub fn approval_mac(key: &[u8], items: &[LaunchItem]) -> Vec<u8> {
    approval_hmac(key, items).finalize().into_bytes().to_vec()
}

pub trait ApprovalStore {
    fn load(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>, LaunchError>;
    fn save(&self, key: &[u8], mac: &[u8]) -> Result<(), LaunchError>;
}

#[derive(Deserialize, Serialize)]
struct ApprovalRecord {
    key: String,
    mac: String,
}

// この同期アダプターは Tauri の spawn_blocking から呼ぶ。
pub(crate) struct KeyringApprovalStore;

impl ApprovalStore for KeyringApprovalStore {
    fn load(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>, LaunchError> {
        let record = tauri::async_runtime::block_on(crate::secret_store::load::<ApprovalRecord>(
            APPROVAL_USER,
        ))
        .map_err(|_| LaunchError::Store("read".into()))?;
        let decode = |value: String| {
            STANDARD
                .decode(value)
                .map_err(|_| LaunchError::Store("base64".into()))
        };
        record
            .map(|record| Ok((decode(record.key)?, decode(record.mac)?)))
            .transpose()
    }

    fn save(&self, key: &[u8], mac: &[u8]) -> Result<(), LaunchError> {
        let record = ApprovalRecord {
            key: STANDARD.encode(key),
            mac: STANDARD.encode(mac),
        };
        tauri::async_runtime::block_on(crate::secret_store::save(APPROVAL_USER, &record))
            .map_err(|_| LaunchError::Store("write".into()))
    }
}

#[derive(Deserialize, Serialize)]
struct LaunchFile<T> {
    version: u32,
    items: T,
}

pub fn save_approved(
    config_dir: &Path,
    store: &dyn ApprovalStore,
    items: Vec<LaunchItem>,
    confirmed: bool,
) -> Result<(), LaunchError> {
    if !confirmed {
        return Err(LaunchError::NotConfirmed);
    }
    validate_items(&items)?;
    let mut key = [0; KEY_LEN];
    getrandom::getrandom(&mut key).map_err(|_| LaunchError::Store("random".into()))?;
    let mac = approval_mac(&key, &items);
    let bytes = serde_json::to_vec_pretty(&LaunchFile {
        version: VERSION,
        items,
    })
    .expect("launch file is JSON serializable");

    fs::create_dir_all(config_dir).map_err(io_error)?;
    // 同じユーザーの別ログオンセッションによるファイルと承認の書き込みも直列化する。
    let lock = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config_dir.join("launch.lock"))
        .map_err(io_error)?;
    lock.try_lock().map_err(|_| {
        LaunchError::Invalid("起動時に開く設定を保存できません。少し待ってもう一度お試しください")
    })?;

    let temporary = config_dir.join(format!(".launch-{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(io_error)?;
    let written = file.write_all(&bytes).and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = written.and_then(|()| fs::rename(&temporary, config_dir.join(FILE_NAME))) {
        let _ = fs::remove_file(&temporary);
        return Err(io_error(error));
    }
    // ここで承認の保存が失敗しても、新しいファイルは照合に通らない。
    store.save(&key, &mac)
}

fn read_items(config_dir: &Path) -> Result<Option<Vec<LaunchItem>>, LaunchError> {
    let bytes = match fs::read(config_dir.join(FILE_NAME)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let file: LaunchFile<Vec<LaunchItem>> = serde_json::from_slice(&bytes)
        .map_err(|_| LaunchError::Invalid("起動時に開く設定のファイルが壊れています"))?;
    if file.version != VERSION {
        return Err(LaunchError::Invalid(
            "起動時に開く設定のファイル形式に対応していません",
        ));
    }
    Ok(Some(file.items))
}

pub fn load_items(config_dir: &Path) -> Result<Vec<LaunchItem>, LaunchError> {
    Ok(read_items(config_dir)?.unwrap_or_default())
}

fn is_approved(store: &dyn ApprovalStore, items: &[LaunchItem]) -> bool {
    if validate_items(items).is_err() {
        return false;
    }
    let Ok(Some((key, mac))) = store.load() else {
        return false;
    };
    key.len() == KEY_LEN && approval_hmac(&key, items).verify_slice(&mac).is_ok()
}

pub enum StartupDecision {
    Nothing,
    Run(Vec<LaunchItem>),
    NeedsApproval,
}

pub fn load_for_startup(config_dir: &Path, store: &dyn ApprovalStore) -> StartupDecision {
    match read_items(config_dir) {
        Ok(None) => StartupDecision::Nothing,
        Ok(Some(items)) if is_approved(store, &items) => {
            StartupDecision::Run(items.into_iter().filter(|item| item.enabled).collect())
        }
        _ => StartupDecision::NeedsApproval,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LaunchSettings {
    items: Vec<LaunchItem>,
    needs_approval: bool,
    read_error: Option<String>,
    pub(crate) failures: u64,
}

pub(crate) fn settings(config_dir: &Path) -> LaunchSettings {
    match read_items(config_dir) {
        Ok(None) => LaunchSettings {
            items: vec![],
            needs_approval: false,
            read_error: None,
            failures: 0,
        },
        Ok(Some(items)) => {
            let needs_approval = !is_approved(&KeyringApprovalStore, &items);
            LaunchSettings {
                items,
                needs_approval,
                read_error: None,
                failures: 0,
            }
        }
        Err(error) => LaunchSettings {
            items: vec![],
            needs_approval: true,
            read_error: Some(error.to_string()),
            failures: 0,
        },
    }
}

fn launch(item: &LaunchItem) -> Result<(), &'static str> {
    match item.kind {
        LaunchKind::Exe => {
            let target = Path::new(&item.target);
            let parent = target.parent().ok_or("invalidPath")?;
            if !target.is_absolute()
                || !fs::metadata(target).is_ok_and(|metadata| metadata.is_file())
            {
                return Err("notAFile");
            }
            std::process::Command::new(target)
                .args(&item.args)
                .current_dir(parent)
                .spawn()
                .map(|_| ())
                .map_err(|_| "appOpenFailed")
        }
        LaunchKind::Url => {
            crate::update::open_url(item.target.clone()).map_err(|_| "urlOpenFailed")
        }
    }
}

pub(crate) async fn run(items: Vec<LaunchItem>, mut notify_failure: impl FnMut()) {
    for (index, item) in items.into_iter().enumerate() {
        if index > 0 {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        let result = tauri::async_runtime::spawn_blocking(move || {
            let result = launch(&item);
            if let Err(kind) = result {
                tracing::warn!(label = ?item.label, kind, "起動時に開く項目の実行に失敗しました");
            }
            result
        })
        .await;
        if !matches!(result, Ok(Ok(()))) {
            notify_failure();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    #[derive(Default)]
    struct MemoryApprovalStore {
        approval: RefCell<Option<(Vec<u8>, Vec<u8>)>>,
        fail_save: Cell<bool>,
    }

    impl ApprovalStore for MemoryApprovalStore {
        fn load(&self) -> Result<Option<(Vec<u8>, Vec<u8>)>, LaunchError> {
            Ok(self.approval.borrow().clone())
        }

        fn save(&self, key: &[u8], mac: &[u8]) -> Result<(), LaunchError> {
            if self.fail_save.get() {
                return Err(LaunchError::Store("write".into()));
            }
            self.approval.replace(Some((key.to_vec(), mac.to_vec())));
            Ok(())
        }
    }

    fn items(target: &str) -> Vec<LaunchItem> {
        vec![LaunchItem {
            id: "test".into(),
            kind: LaunchKind::Url,
            target: target.into(),
            args: vec![],
            enabled: true,
            label: None,
        }]
    }

    #[test]
    fn replacing_settings_rotates_key_and_approves_new_list() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryApprovalStore::default();
        save_approved(dir.path(), &store, items("https://example.com/old"), true).unwrap();
        let (old_key, _) = store.load().unwrap().unwrap();
        save_approved(dir.path(), &store, items("https://example.com/new"), true).unwrap();
        let (new_key, _) = store.load().unwrap().unwrap();
        assert_eq!(new_key.len(), KEY_LEN);
        assert_ne!(old_key, new_key);
        let StartupDecision::Run(approved) = load_for_startup(dir.path(), &store) else {
            panic!("expected new list to be approved");
        };
        assert_eq!(approved[0].target, "https://example.com/new");
    }

    #[test]
    fn failed_store_write_leaves_list_available_for_review_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryApprovalStore::default();
        save_approved(dir.path(), &store, items("https://example.com/old"), true).unwrap();
        store.fail_save.set(true);
        assert!(matches!(
            save_approved(
                dir.path(),
                &store,
                items("https://example.com/review"),
                true
            ),
            Err(LaunchError::Store(_))
        ));
        assert!(matches!(
            load_for_startup(dir.path(), &store),
            StartupDecision::NeedsApproval
        ));
        assert_eq!(
            load_items(dir.path()).unwrap()[0].target,
            "https://example.com/review"
        );
    }

    #[test]
    fn malformed_url_authorities_are_rejected() {
        for url in [
            "https://@example.com",
            "https:///example.com",
            "https://example.com\0",
        ] {
            assert!(validate_items(&items(url)).is_err());
        }
        assert!(validate_items(&items("https://example.com/path@name")).is_ok());
    }
}
