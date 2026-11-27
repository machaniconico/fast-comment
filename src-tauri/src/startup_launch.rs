//! 起動時に開く項目の検証・承認・実行(SPEC §20)。

use std::{collections::HashSet, fs, io::Write, path::Path, sync::LazyLock, time::Duration};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

const FILE_NAME: &str = "launch.json";
const VERSION: u32 = 1;
const APPROVAL_USER: &str = "launch-approval";
const KEY_LEN: usize = 32;
type HmacSha256 = Hmac<Sha256>;
static UNSAFE_TEXT: LazyLock<regex::Regex> = LazyLock::new(|| {
    regex::Regex::new(r"[\p{Cc}\p{Cf}]").expect("control and format character regex")
});

#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LaunchKind {
    Exe,
    Url,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchItem {
    pub id: String,
    pub kind: LaunchKind,
    pub target: String,
    pub args: Vec<String>,
    pub enabled: bool,
    pub label: Option<String>,
    /// 起動したアプリのウィンドウを出さない(Windows の CREATE_NO_WINDOW)。アプリ項目だけ。
    #[serde(default)]
    pub hide_window: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error("保存するには内容の確認と承認が必要です")]
    NotConfirmed,
    #[error("{0}")]
    Invalid(&'static str),
    #[error("{number}件目の項目：{reason}")]
    InvalidItem { number: usize, reason: &'static str },
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
    let mut ids = HashSet::with_capacity(items.len());
    for (index, item) in items.iter().enumerate() {
        let invalid = |reason| LaunchError::InvalidItem {
            number: index + 1,
            reason,
        };
        if !(1..=64).contains(&item.id.len())
            || !item
                .id
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, b'-' | b'_'))
        {
            return Err(invalid(
                "IDは1〜64文字の半角英数字、ハイフン、アンダースコアで指定してください",
            ));
        }
        if !ids.insert(item.id.as_str()) {
            return Err(invalid("IDが重複しています"));
        }
        if item.target.chars().count() > 2048 {
            return Err(invalid("パスとURLは2048文字までです"));
        }
        if item.args.len() > 32 || item.args.iter().any(|arg| arg.chars().count() > 1024) {
            return Err(invalid("引数は32個まで、1個あたり1024文字までです"));
        }
        if item
            .label
            .as_ref()
            .is_some_and(|label| label.chars().count() > 100)
        {
            return Err(invalid("表示名は100文字までです"));
        }
        if std::iter::once(item.target.as_str())
            .chain(item.args.iter().map(String::as_str))
            .chain(item.label.as_deref())
            .any(|text| UNSAFE_TEXT.is_match(text))
        {
            return Err(invalid(
                "パス・URL・引数・表示名に制御文字や書式文字は使えません",
            ));
        }
        match item.kind {
            LaunchKind::Exe if !is_absolute_app_path(&item.target) => {
                return Err(invalid(
                    "アプリには拡張子が .exe / .bat / .cmd の Windows 絶対パスを指定してください",
                ));
            }
            LaunchKind::Exe if is_batch(&item.target) => {
                if item.target.contains('%') {
                    return Err(invalid(
                        ".bat / .cmd のパスに % は使えません(cmd.exe が環境変数として展開します)",
                    ));
                }
                if is_unc(&item.target) {
                    return Err(invalid(
                        ".bat / .cmd はネットワークパス(UNC)では使えません。ドライブ文字付きのパスを指定してください",
                    ));
                }
                if batch_command_line_estimate(item) > BATCH_COMMAND_LINE_LIMIT {
                    return Err(invalid(
                        ".bat / .cmd のパスと引数が長すぎます(cmd.exe のコマンドライン上限を超えます)",
                    ));
                }
            }
            LaunchKind::Url if item.hide_window => {
                return Err(invalid("URLにはウィンドウを隠す設定を使えません"));
            }
            LaunchKind::Url if !is_web_url(&item.target) => {
                return Err(invalid(
                    "ユーザー情報・空白・二重引用符・山括弧を含まない http:// または https:// のURLを指定してください",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// cmd.exe のコマンドライン上限 8191 文字に対する、保守的な見積もりの上限。
const BATCH_COMMAND_LINE_LIMIT: usize = 8000;

fn is_batch(target: &str) -> bool {
    Path::new(target)
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("bat") || ext.eq_ignore_ascii_case("cmd"))
}

/// `\\server\share` と `\\?\UNC\...`。`\\?\C:\...` はドライブ付きなので含めない。
fn is_unc(target: &str) -> bool {
    let normalized = target.replace('/', "\\");
    match normalized.strip_prefix(r"\\?\") {
        Some(rest) => rest
            .get(..4)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("UNC\\")),
        None => normalized.starts_with(r"\\"),
    }
}

/// cmd.exe /e:ON /v:OFF /d /c 経由で起動した時のコマンドライン長の見積もり(UTF-16 単位)。
/// `%` は 8、`"` は 2 に数える。
fn batch_command_line_estimate(item: &LaunchItem) -> usize {
    let utf16 = |text: &str| text.encode_utf16().count();
    let args: usize = item
        .args
        .iter()
        .map(|arg| {
            3 + arg
                .chars()
                .map(|ch| match ch {
                    '%' => 8,
                    '"' => 2,
                    _ => ch.len_utf16(),
                })
                .sum::<usize>()
        })
        .sum();
    32 + utf16(&item.target) + 2 + args
}

/// `run.cmd` と `run.cmd.exe` が並ぶと、Rust 標準ライブラリは後者を先に選ぶ。
/// 承認していない exe を起動しないため、.bat / .cmd の起動前に確かめる。
fn has_ambiguous_exe_sibling(target: &Path) -> bool {
    let is_batch_target = target.to_str().is_some_and(is_batch);
    let mut sibling = target.as_os_str().to_owned();
    sibling.push(".exe");
    is_batch_target && Path::new(&sibling).exists()
}

fn is_absolute_app_path(target: &str) -> bool {
    if target
        .chars()
        .any(|ch| matches!(ch, '"' | '<' | '>' | '|' | '*'))
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
                Path::new(file).extension().is_some_and(|ext| {
                    ["exe", "bat", "cmd"]
                        .iter()
                        .any(|allowed| ext.eq_ignore_ascii_case(allowed))
                })
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
        && !target
            .chars()
            .any(|ch| ch.is_whitespace() || matches!(ch, '\\' | '"' | '<' | '>'))
        && url::Url::parse(target).is_ok_and(|url| {
            url.host().is_some() && url.username().is_empty() && url.password().is_none()
        })
}

pub(crate) fn items_from_ipc(
    items: serde_json::Value,
    confirmed: serde_json::Value,
) -> Result<Vec<LaunchItem>, String> {
    if !crate::from_ipc_value::<bool>(confirmed, "保存の承認")? {
        return Err(LaunchError::NotConfirmed.to_string());
    }
    let items: Vec<serde_json::Value> = crate::from_ipc_value(items, "起動時に開く項目の一覧")?;
    items
        .into_iter()
        .enumerate()
        .map(|(index, item)| crate::from_ipc_value(item, &format!("{}件目の項目", index + 1)))
        .collect()
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
        // false の時はキーを出さず、この項目追加前に承認済みの一覧の MAC を変えない。
        #[serde(rename = "hideWindow", skip_serializing_if = "std::ops::Not::not")]
        hide_window: bool,
    }

    let approved: Vec<_> = items
        .iter()
        .map(|item| ApprovedItem {
            id: &item.id,
            kind: item.kind,
            target: &item.target,
            args: &item.args,
            enabled: item.enabled,
            hide_window: item.hide_window,
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
            if has_ambiguous_exe_sibling(target) {
                return Err("ambiguousPath");
            }
            let mut command = std::process::Command::new(target);
            command.args(&item.args).current_dir(parent);
            #[cfg(windows)]
            if item.hide_window {
                use std::os::windows::process::CommandExt;
                command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
            }
            command.spawn().map(|_| ()).map_err(|_| "appOpenFailed")
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
            hide_window: false,
        }]
    }

    fn app(target: &str, hide_window: bool) -> Vec<LaunchItem> {
        let mut launch = items("https://example.com/");
        launch[0].kind = LaunchKind::Exe;
        launch[0].target = target.into();
        launch[0].hide_window = hide_window;
        launch
    }

    #[test]
    fn app_paths_accept_exe_bat_and_cmd_only() {
        for path in [
            r"C:\Tools\a.exe",
            r"C:\Tools\a.bat",
            r"C:\Tools\a.CMD",
            r"C:\Tools\a.Bat",
            r"\\server\share\a.exe",
        ] {
            assert!(validate_items(&app(path, false)).is_ok(), "{path}");
        }
        for path in [
            r"C:\Tools\a.bat.txt",
            r"C:\Tools\a.ps1",
            r"C:\Tools\a.bat:stream",
            "x.bat",
            r"C:\Tools\a",
        ] {
            assert!(validate_items(&app(path, false)).is_err(), "{path}");
        }
    }

    fn batch_app(args: Vec<String>) -> Vec<LaunchItem> {
        let mut launch = app(r"C:\a.bat", false);
        launch[0].args = args;
        launch
    }

    #[test]
    fn batch_rejects_percent_and_unc_but_exe_unc_is_allowed() {
        for path in [r"C:\100%\a.bat", r"C:\a%PATH%.CMD"] {
            let error = validate_items(&app(path, false)).unwrap_err().to_string();
            assert!(
                error.contains("1件目") && !error.contains("PATH"),
                "{error}"
            );
        }
        assert!(validate_items(&app(r"C:\100%\a.exe", false)).is_ok());
        for path in [
            r"\\server\share\a.bat",
            r"\\server\share\a.cmd",
            r"\\?\UNC\server\share\a.bat",
            "//server/share/a.cmd",
        ] {
            assert!(validate_items(&app(path, false)).is_err(), "{path}");
        }
        for path in [
            r"\\server\share\a.exe",
            r"\\?\UNC\server\share\a.exe",
            r"\\?\C:\Tools\a.bat",
        ] {
            assert!(validate_items(&app(path, false)).is_ok(), "{path}");
        }
    }

    #[test]
    fn batch_command_line_estimate_boundary_is_8000() {
        // 見積もり = 32 + 8(target) + 2 + Σ(3 + 引数長) = 42 + 引数ごとの 3 + 文字数
        let args_of = |total: usize| {
            let mut args = vec!["a".repeat(1024); 7];
            args.push("a".repeat(total - 7 * 1027 - 3));
            args
        };
        let exact = batch_app(args_of(8000 - 42));
        assert_eq!(batch_command_line_estimate(&exact[0]), 8000);
        assert!(validate_items(&exact).is_ok());
        let over = batch_app(args_of(8000 - 42 + 1));
        assert_eq!(batch_command_line_estimate(&over[0]), 8001);
        assert!(validate_items(&over).is_err());
        // % は 8、" は 2、BMP 外は 2 に数える。
        let weighted = batch_app(vec!["%\"\u{1F600}".into()]);
        assert_eq!(
            batch_command_line_estimate(&weighted[0]),
            42 + 3 + 8 + 2 + 2
        );
        // .exe は対象外。
        let mut exe = batch_app(args_of(8000 - 42 + 1));
        exe[0].target = r"C:\a.exe".into();
        assert!(validate_items(&exe).is_ok());
    }

    #[test]
    fn ambiguous_exe_sibling_is_detected_only_for_batch_targets() {
        let dir = tempfile::tempdir().unwrap();
        let cmd = dir.path().join("run.cmd");
        fs::write(&cmd, "").unwrap();
        assert!(!has_ambiguous_exe_sibling(&cmd));
        fs::write(dir.path().join("run.cmd.exe"), "").unwrap();
        assert!(has_ambiguous_exe_sibling(&cmd));
        let exe = dir.path().join("run.cmd.exe");
        assert!(!has_ambiguous_exe_sibling(&exe));
    }

    #[test]
    fn mac_for_hide_window_true_uses_camel_case_key() {
        let key = [7; KEY_LEN];
        let launch = app(r"C:\a.exe", true);
        let fixed = r#"[{"id":"test","kind":"exe","target":"C:\\a.exe","args":[],"enabled":true,"hideWindow":true}]"#;
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(fixed.as_bytes());
        assert_eq!(
            approval_mac(&key, &launch),
            mac.finalize().into_bytes().to_vec()
        );
    }

    #[test]
    fn legacy_json_without_hide_window_reads_as_false() {
        let item: LaunchItem = serde_json::from_str(
            r#"{"id":"a","kind":"exe","target":"C:\\a.exe","args":[],"enabled":true,"label":null}"#,
        )
        .unwrap();
        assert!(!item.hide_window);
        let json = serde_json::to_value(app(r"C:\a.bat", true).remove(0)).unwrap();
        assert_eq!(json["hideWindow"], true);
    }

    #[test]
    fn mac_omits_hide_window_when_false_for_compatibility() {
        let key = [7; KEY_LEN];
        let launch = app(r"C:\a.exe", false);
        // hideWindow 導入前の形式(id,kind,target,args,enabled の順)を直接組み立てる。
        let legacy =
            r#"[{"id":"test","kind":"exe","target":"C:\\a.exe","args":[],"enabled":true}]"#;
        let mut mac = HmacSha256::new_from_slice(&key).unwrap();
        mac.update(legacy.as_bytes());
        assert_eq!(
            approval_mac(&key, &launch),
            mac.finalize().into_bytes().to_vec()
        );
    }

    #[test]
    fn changing_hide_window_changes_mac_and_revokes_approval() {
        let store = MemoryApprovalStore::default();
        let key = [7; KEY_LEN];
        let mut launch = app(r"C:\a.bat", false);
        store.save(&key, &approval_mac(&key, &launch)).unwrap();
        assert!(is_approved(&store, &launch));
        let before = approval_mac(&key, &launch);
        launch[0].hide_window = true;
        assert_ne!(before, approval_mac(&key, &launch));
        assert!(!is_approved(&store, &launch));
    }

    #[test]
    fn url_with_hide_window_is_rejected_without_input_values() {
        let mut launch = items("https://example.com/PRIVATE");
        launch[0].hide_window = true;
        let error = validate_items(&launch).unwrap_err().to_string();
        assert!(error.contains("1件目"), "{error}");
        assert!(!error.contains("PRIVATE"), "{error}");
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

    #[test]
    fn target_args_and_label_reject_control_and_format_characters() {
        let mut launch = items("C:\\Tools\\\u{202e}ppA\u{202c}.exe");
        launch[0].kind = LaunchKind::Exe;
        assert!(validate_items(&launch).is_err());

        for ch in ['\u{200b}', '\n', '\u{85}', '\u{feff}', '\u{e0001}'] {
            let mut launch = items(&format!("https://example.com/{ch}"));
            assert!(validate_items(&launch).is_err());
            launch[0].target = "https://example.com/".into();
            launch[0].args = vec![format!("--flag={ch}")];
            assert!(validate_items(&launch).is_err());
            launch[0].args.clear();
            launch[0].label = Some(format!("表示名{ch}"));
            assert!(validate_items(&launch).is_err());
        }
    }

    #[test]
    fn previously_approved_file_with_format_characters_needs_review() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryApprovalStore::default();
        let mut launch = items("https://example.com/");
        launch[0].args.push("--flag=\u{200b}".into());
        let key = [42; KEY_LEN];
        store.save(&key, &approval_mac(&key, &launch)).unwrap();
        let bytes = serde_json::to_vec(&LaunchFile {
            version: VERSION,
            items: launch,
        })
        .unwrap();
        fs::write(dir.path().join(FILE_NAME), bytes).unwrap();

        assert!(matches!(
            load_for_startup(dir.path(), &store),
            StartupDecision::NeedsApproval
        ));
    }

    #[test]
    fn raw_url_whitespace_quotes_and_angle_brackets_are_rejected() {
        for ch in [' ', '\u{3000}', '"', '<', '>'] {
            assert!(validate_items(&items(&format!("https://example.com/a{ch}b"))).is_err());
        }
        assert!(validate_items(&items("https://example.com/a%20%22%3C%3E")).is_ok());
    }

    #[test]
    fn ids_have_ascii_limits_and_must_be_unique() {
        let mut launch = items("https://example.com/");
        for id in ["", &"a".repeat(65), "é", "a.b", "a b"] {
            launch[0].id = id.into();
            assert!(validate_items(&launch).is_err());
        }
        launch[0].id = format!("A-0_{}", "z".repeat(60));
        assert!(validate_items(&launch).is_ok());

        launch.extend(items("https://example.com/"));
        assert!(validate_items(&launch).is_ok());
        launch[1].id = launch[0].id.clone();
        assert!(matches!(
            validate_items(&launch),
            Err(LaunchError::InvalidItem { number: 2, .. })
        ));
    }

    #[test]
    fn save_validation_errors_report_position_without_input_values() {
        let dir = tempfile::tempdir().unwrap();
        let store = MemoryApprovalStore::default();
        let mut launch = items("https://example.com/");
        let mut second = items("https://example.com/PRIVATE_TARGET<");
        second[0].id = "PRIVATE_ID".into();
        second[0].args.push("PRIVATE_ARG".into());
        launch.extend(second);

        let error = save_approved(dir.path(), &store, launch, true)
            .unwrap_err()
            .to_string();
        assert!(error.contains("2件目"), "{error}");
        assert!(!error.contains("PRIVATE"), "{error}");
        assert!(!dir.path().join(FILE_NAME).exists());
    }

    #[test]
    fn ipc_type_errors_do_not_echo_inputs() {
        use serde_json::json;

        for (items, confirmed, expected) in [
            (json!([]), json!("PRIVATE_CONFIRMATION"), "保存の承認"),
            (json!("PRIVATE_LIST"), json!(true), "起動時に開く項目の一覧"),
            (
                json!([items("https://example.com/").remove(0), {
                    "id": "second", "kind": "PRIVATE_KIND", "target": "PRIVATE_TARGET",
                    "args": [], "enabled": true
                }]),
                json!(true),
                "2件目",
            ),
        ] {
            let error = items_from_ipc(items, confirmed)
                .err()
                .expect("invalid IPC input");
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("PRIVATE"), "{error}");
        }
        assert_eq!(
            items_from_ipc(json!([]), json!(false)).err().as_deref(),
            Some("保存するには内容の確認と承認が必要です")
        );
        assert_eq!(
            items_from_ipc(json!(items("https://example.com/")), json!(true))
                .unwrap()
                .len(),
            1
        );
    }
}
