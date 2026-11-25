//! Automatic backups of the on-disk user data and startup-only restore.

use crate::config::{AppConfig, CredentialsConfig};
use crate::listeners::ListenerRecord;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use uuid::Uuid;
use zip::{write::SimpleFileOptions, CompressionMethod, ZipArchive, ZipWriter};

const FILES: &[&str] = &[
    "config.json",
    "listeners.json",
    "achievements.json",
    "achievement-progress.json",
    "sounds.json",
];
const DIRECTORIES: &[&str] = &["sounds", "templates", "skin"];
const CREDENTIAL_SECRETS: &[&str] = &[
    "twitchOauth",
    "twitchClientSecret",
    "youtubeApiKey",
    "xAuthToken",
    "xCsrfToken",
];
const OVERRIDE_SECRETS: &[&[&str]] = &[
    &["xOverrides", "bearerToken"],
    &["youtubeOverrides", "apiKey"],
];
/// 資格情報の欄。zip では空にし、設定の書き出し(SPEC §8.1)では欄ごと消す。
const CREDENTIALS: &[&str] = &["credentials"];
/// 外部プログラムの起動の欄。ファイルを読んだだけで任意の exe を(昇格して)起動させないよう、
/// 復元でも設定の読み込みでも今の値を引き継ぐ。
const LOCAL_CONFIG_FIELDS: &[&[&str]] = &[
    BOUYOMI_PATH,
    &["tts", "options", "bouyomiLaunchElevated"],
];
/// 棒読みちゃんの起動パス。Windows のユーザー名を含みうるので、設定の書き出しには入れない。
const BOUYOMI_PATH: &[&str] = &["tts", "options", "bouyomiPath"];
/// OAuth の redirect URI のポート。今の Client ID に登録した値と揃えるため、
/// 復元でも設定の読み込みでも今の値を引き継ぐ。
const CALLBACK_PORTS: &[&[&str]] = &[
    &["xPost", "callbackPort"],
    &["twitcasting", "callbackPort"],
];
/// X の接続先。ログイン cookie の送り先なので、設定の読み込みでも今の値を引き継ぐ。
const X_ENDPOINTS: &[&str] = &["xOverrides", "endpoints"];
/// URL の欄。zip ではユーザー情報(user:pass@)を消し、復元時は今の値を引き継ぐ。
const URL_FIELDS: &[&[&str]] = &[
    &["tts", "options", "voicevoxUrl"],
    X_ENDPOINTS,
    &["niconicoOverrides", "endpoints"],
    &["twitcastingOverrides", "endpoints"],
];
const EXPORT_SECRET_ERROR: &str = "資格情報と同じ値が他の欄にあるため、設定を書き出せません";
const EXPORT_CHECK_ERROR: &str = "資格情報の検査に失敗したため、設定を書き出せません";
const IMPORT_ERROR: &str = "設定を取り込めません";
const X_POST_DISABLED_BY_IMPORT: &str = "X の自動告知は取り込み後に無効にしました。設定画面で確認してから有効にしてください。有効にする前に、自分の配信(ownStream)の設定も確認してください。";
const BOUYOMI_NOT_LAUNCHED_BY_IMPORT: &str = "棒読みちゃんは自動では起動しません。";
const TTS_DESTINATION_KEPT: &str =
    "読み上げの接続先は、この PC 以外を指していたため今の設定のままにしました。";
pub(crate) const X_POST_MODE_KEPT: &str = "X の投稿方式は今の設定のままにしました。";

/// 復元でも設定の読み込みでも、ファイルの値を使わず今の値を引き継ぐ欄。
fn kept_fields() -> impl Iterator<Item = &'static [&'static str]> {
    std::iter::once(CREDENTIALS)
        .chain(OVERRIDE_SECRETS.iter().copied())
        .chain(LOCAL_CONFIG_FIELDS.iter().copied())
        .chain(CALLBACK_PORTS.iter().copied())
        .chain(std::iter::once(X_ENDPOINTS))
}
const SCHEMA: &str = "fast-comment-backup";
const MANIFEST: &str = "manifest.json";
const READY: &str = "READY";
const APPLYING: &str = "APPLYING";
/// 復元元の zip の名前。復元に失敗した世代では、次に復元が成功するまで prune から外す。
const SOURCE: &str = "SOURCE";
const PREPARED_CONFIG: &str = "config.restore.tmp";
const MAX_ENTRIES: usize = 2000;
const MAX_FILE_BYTES: u64 = 50 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 200 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackupSettings {
    pub enabled: bool,
    pub keep: u32,
}

impl Default for BackupSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            keep: 10,
        }
    }
}

impl BackupSettings {
    pub fn normalized(self) -> Self {
        Self {
            keep: self.keep.clamp(1, 50),
            ..self
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupEntry {
    pub file_name: String,
    pub created_at_ms: u64,
    pub size: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
    #[error("バックアップのファイル操作に失敗しました（{}）", backup_io_reason(.0))]
    Io(String),
    #[error("バックアップが不正です（{0}）")]
    Invalid(&'static str),
    #[error("バックアップが見つかりません")]
    NotFound,
}

pub(crate) const RESTORE_SAVE_BLOCKED: &str =
    "復元の取り消しが完了していないため、保存を停止しています";

#[derive(Debug, thiserror::Error)]
#[error("{RESTORE_SAVE_BLOCKED}")]
pub(crate) struct RestoreSaveBlocked;

fn backup_io_reason(kind: &str) -> &'static str {
    match kind {
        "PermissionDenied" => "アクセス権限がありません",
        "StorageFull" => "空き容量がありません",
        "zip" => "zip の書き込み",
        "listeners" => "リスナー記録の保存",
        "achievements" => "実績の進捗の保存",
        _ => "入出力エラー",
    }
}

impl From<io::Error> for BackupError {
    fn from(error: io::Error) -> Self {
        // OS のエラー種別だけを残す。パスや入力データは含めない。
        Self::Io(format!("{:?}", error.kind()))
    }
}

type Result<T> = std::result::Result<T, BackupError>;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    schema: String,
    version: u32,
    created_at_ms: u64,
    app_version: String,
    entries: Vec<String>,
}

#[derive(Default)]
struct Contents {
    files: BTreeMap<String, Vec<u8>>,
    directories: BTreeSet<String>,
    total_bytes: u64,
}

impl Contents {
    fn check_limit(&self, size: u64) -> Result<()> {
        if self.files.len() >= MAX_ENTRIES
            || size > MAX_FILE_BYTES
            || size > MAX_TOTAL_BYTES - self.total_bytes
        {
            return Err(BackupError::Invalid("容量またはエントリ数の上限超過"));
        }
        Ok(())
    }

    fn read_file(&mut self, name: String, size: u64, reader: impl Read) -> Result<()> {
        self.check_limit(size)?;
        let limit = MAX_FILE_BYTES.min(MAX_TOTAL_BYTES - self.total_bytes);
        let mut bytes = Vec::new();
        reader.take(limit + 1).read_to_end(&mut bytes)?;
        let actual = bytes.len() as u64;
        self.check_limit(actual)?;
        self.total_bytes += actual;
        self.files.insert(name, bytes);
        Ok(())
    }

    fn read_directory(&mut self, path: &Path, prefix: &str, include_dirs: bool) -> Result<()> {
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let part = entry.file_name();
            let part = part
                .to_str()
                .ok_or(BackupError::Invalid("パスの文字コード"))?;
            if prefix.split('/').next() == Some("sounds") && excluded(part) {
                if include_dirs {
                    return Err(BackupError::Invalid("復元対象外のパス"));
                }
                continue;
            }
            let name = format!("{prefix}/{part}");
            let meta = fs::symlink_metadata(entry.path())?;
            let directory = meta.is_dir();
            validate_path(&name, directory)?;
            check_kind(&meta, directory)?;
            if directory {
                if include_dirs {
                    self.directories.insert(name.clone());
                }
                self.read_directory(&entry.path(), &name, include_dirs)?;
            } else {
                self.read_file(name, meta.len(), File::open(entry.path())?)?;
            }
        }
        Ok(())
    }

    fn digest(&self) -> BTreeMap<String, [u8; 32]> {
        self.files
            .iter()
            .filter(|(name, _)| name.as_str() != MANIFEST)
            .map(|(name, bytes)| (name.clone(), Sha256::digest(bytes).into()))
            .collect()
    }

    fn validate(&self) -> Result<()> {
        let manifest: Manifest = self
            .files
            .get(MANIFEST)
            .and_then(|bytes| serde_json::from_slice(bytes).ok())
            .ok_or(BackupError::Invalid("マニフェスト"))?;
        let listed: BTreeSet<_> = manifest.entries.iter().map(String::as_str).collect();
        let actual: BTreeSet<_> = self
            .files
            .keys()
            .map(String::as_str)
            .filter(|name| *name != MANIFEST)
            .collect();
        if manifest.schema != SCHEMA
            || manifest.version != 1
            || listed.len() != manifest.entries.len()
            || listed != actual
        {
            return Err(BackupError::Invalid("マニフェスト"));
        }
        self.validate_paths()?;
        Ok(())
    }

    /// 実際のストアの読み込みと同じ型で検証する。作成・stage・apply で共通。
    fn validate_stores(&self) -> Result<()> {
        use crate::achievements::store::{validate_backup_defs, validate_backup_progress};

        for (name, bytes) in &self.files {
            // Windows では大小違いの名前も同じストアを指すので、大小を無視して照合する。
            let Some(store) = typed_store(name) else {
                continue;
            };
            let (valid, reason) = match store {
                "config.json" => (config_object(bytes).is_ok(), "設定の形式"),
                "listeners.json" => (
                    serde_json::from_slice::<HashMap<String, ListenerRecord>>(bytes).is_ok(),
                    "リスナー記録の形式",
                ),
                "achievements.json" => (validate_backup_defs(bytes), "実績の定義の形式"),
                "achievement-progress.json" => {
                    (validate_backup_progress(bytes), "実績の進捗の形式")
                }
                "sounds.json" | "sounds/sounds.json" => (
                    crate::sounds::validate_backup_index(bytes),
                    "効果音の一覧の形式",
                ),
                _ => unreachable!("backup store"),
            };
            let object = bytes.iter().find(|byte| !byte.is_ascii_whitespace()) == Some(&b'{');
            if !object || !valid {
                return Err(BackupError::Invalid(reason));
            }
        }
        Ok(())
    }

    fn validate_paths(&self) -> Result<()> {
        // Windows 上で同じ場所を指す名前と、ファイル/親ディレクトリの衝突を拒否する。
        let mut paths = BTreeMap::new();
        for (name, directory) in self
            .files
            .keys()
            .map(|p| (p, false))
            .chain(self.directories.iter().map(|p| (p, true)))
        {
            for (prefix, is_dir) in name
                .match_indices('/')
                .map(|(end, _)| (&name[..end], true))
                .chain(std::iter::once((name.as_str(), directory)))
            {
                if let Some(previous) = paths.insert(prefix.to_uppercase(), (prefix, is_dir)) {
                    if previous != (prefix, is_dir) {
                        return Err(BackupError::Invalid("パスの衝突"));
                    }
                }
            }
        }
        Ok(())
    }
}

/// 型を検証するストアの正規名。
fn typed_store(name: &str) -> Option<&'static str> {
    let name = name.to_uppercase();
    FILES
        .iter()
        .copied()
        .chain(["sounds/sounds.json"])
        .find(|store| store.to_uppercase() == name)
}

/// 許可された名前と大小だけが異なる(Windows では同じ場所を指す)。
fn case_variant(part: &str, names: &[&str]) -> bool {
    let upper = part.to_uppercase();
    names
        .iter()
        .any(|name| part != *name && name.to_uppercase() == upper)
}

fn excluded(part: &str) -> bool {
    let part = part.to_ascii_lowercase();
    part.ends_with(".tmp")
        || [".broken-", ".corrupt-", ".bak."]
            .iter()
            .any(|fragment| part.contains(fragment))
        || matches!(
            part.as_str(),
            "integrations.json" | "x-autopost-ledger.json" | "launch.json" | "launch.lock" | "logs" | "exports" | "backups"
        )
}

fn validate_path(name: &str, directory: bool) -> Result<()> {
    let mut parts = name.split('/');
    let root = parts.next().unwrap_or_default();
    for (index, part) in std::iter::once(root).chain(parts).enumerate() {
        let stem = part
            .split('.')
            .next()
            .unwrap_or_default()
            .trim_end()
            .to_ascii_uppercase();
        let device = matches!(
            stem.as_str(),
            "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
        ) || ["COM", "LPT"].iter().any(|prefix| {
            stem.strip_prefix(prefix).is_some_and(|suffix| {
                matches!(
                    suffix,
                    "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            })
        });
        let variant = match index {
            0 => [FILES, DIRECTORIES, &[MANIFEST]]
                .iter()
                .any(|names| case_variant(part, names)),
            1 => root == "sounds" && case_variant(part, &["sounds.json"]),
            _ => false,
        };
        if variant {
            return Err(BackupError::Invalid("大文字小文字だけが異なる名前"));
        }
        if part.is_empty()
            || part.ends_with(['.', ' '])
            || part.contains(|c: char| c.is_control() || "\\:\"<>|?*".contains(c))
            || device
            || ((index == 0 || root == "sounds") && excluded(part))
        {
            return Err(BackupError::Invalid("パス"));
        }
    }
    let nested = name.contains('/');
    if (DIRECTORIES.contains(&root) && (nested || directory))
        || (!nested && !directory && (FILES.contains(&root) || root == MANIFEST))
    {
        return Ok(());
    }
    Err(BackupError::Invalid("復元対象外のパス"))
}

fn check_kind(meta: &Metadata, directory: bool) -> Result<()> {
    #[cfg(windows)]
    let reparse_point = {
        use std::os::windows::fs::MetadataExt;
        meta.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse_point = false;
    if reparse_point
        || !(if directory {
            meta.is_dir()
        } else {
            meta.is_file()
        })
    {
        return Err(BackupError::Invalid(
            "通常ファイルまたはディレクトリではありません",
        ));
    }
    Ok(())
}

fn metadata(path: &Path) -> Result<Option<Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn directory_exists(path: &Path) -> Result<bool> {
    let Some(meta) = metadata(path)? else {
        return Ok(false);
    };
    check_kind(&meta, true)?;
    Ok(true)
}

fn backups_dir(config_dir: &Path) -> Result<PathBuf> {
    let path = config_dir.join("backups");
    directory_exists(&path)?;
    Ok(path)
}

pub(crate) fn auto_dir(config_dir: &Path) -> Result<PathBuf> {
    let path = backups_dir(config_dir)?.join("auto");
    directory_exists(&path)?;
    Ok(path)
}

fn timestamp(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("fc-backup-")?.strip_suffix(".zip")?;
    numeric_timestamp(digits)
}

fn numeric_timestamp(digits: &str) -> Option<u64> {
    if !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return digits.parse().ok();
    }
    None
}

pub(crate) fn read_backups(config_dir: &Path) -> Result<Vec<BackupEntry>> {
    let path = auto_dir(config_dir)?;
    if !directory_exists(&path)? {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let Ok(file_name) = entry.file_name().into_string() else {
            continue;
        };
        let Some(created_at_ms) = timestamp(&file_name) else {
            continue;
        };
        let meta = fs::symlink_metadata(entry.path())?;
        if check_kind(&meta, false).is_ok() {
            entries.push(BackupEntry {
                file_name,
                created_at_ms,
                size: meta.len(),
            });
        }
    }
    entries.sort_unstable_by(|a, b| {
        (b.created_at_ms, &b.file_name).cmp(&(a.created_at_ms, &a.file_name))
    });
    Ok(entries)
}

pub fn list_backups(config_dir: &Path) -> Vec<BackupEntry> {
    read_backups(config_dir).unwrap_or_default()
}

pub fn due_at_startup(config_dir: &Path, now_ms: u64) -> bool {
    list_backups(config_dir)
        .first()
        .is_none_or(|entry| now_ms.saturating_sub(entry.created_at_ms) >= 20 * 60 * 60 * 1000)
}

/// 復元の準備から適用までの間は何も消さない。復元に失敗した世代の復元元も消さない。
pub fn prune(config_dir: &Path, keep: u32) -> Result<usize> {
    let path = auto_dir(config_dir)?;
    let dir = backups_dir(config_dir)?;
    if metadata(&dir.join("restore-pending"))?.is_some() {
        return Ok(0);
    }
    let protected = protected_sources(&dir)?;
    let mut removed = 0;
    let entries = read_backups(config_dir)?;
    for entry in entries.iter().skip(keep.clamp(1, 50) as usize).rev() {
        if protected.contains(&entry.file_name) {
            continue;
        }
        fs::remove_file(path.join(&entry.file_name))?;
        removed += 1;
    }
    Ok(removed)
}

#[derive(Clone, Copy)]
pub(crate) enum BackupTrigger {
    Startup,
    Interval,
    Manual,
}

/// 自動・手動の作成と世代の整理。呼び出し側が直列化する。
/// `flush` が返す値(ストアのロックなど)は、`sounds/` などを読み取る間だけ持つ。圧縮と書き込みの前に放す。
pub(crate) fn run_backup<G>(
    config_dir: &Path,
    settings: BackupSettings,
    trigger: BackupTrigger,
    restore_failed: bool,
    now_ms: u64,
    flush: impl FnOnce() -> Result<G>,
) -> Result<bool> {
    let manual = matches!(trigger, BackupTrigger::Manual);
    // 復元に失敗した後の起動では、再試行できるよう復元元を残す(自動作成も整理もしない)。
    if (!settings.enabled || restore_failed) && !manual {
        return Ok(false);
    }
    let created = if matches!(trigger, BackupTrigger::Startup) && !due_at_startup(config_dir, now_ms)
    {
        false
    } else {
        create_backup_with(config_dir, now_ms, flush)?.is_some()
    };
    if !restore_failed {
        prune(config_dir, settings.keep)?;
    }
    Ok(created)
}

fn json_object(bytes: &[u8]) -> Result<serde_json::Map<String, Value>> {
    serde_json::from_slice(bytes).map_err(|_| BackupError::Invalid("JSON の形式"))
}

fn config_object(bytes: &[u8]) -> Result<serde_json::Map<String, Value>> {
    let config = json_object(bytes)?;
    serde_json::from_slice::<AppConfig>(bytes).map_err(|_| BackupError::Invalid("設定の形式"))?;
    // Struct deserializers can also read positional arrays. Sensitive fields must
    // be addressable by name when they are cleared or replaced.
    for path in [
        &["credentials"][..],
        &["xOverrides"],
        &["youtubeOverrides"],
        &["tts"],
        &["tts", "options"],
        &["xPost"],
        &["twitcasting"],
        &["niconicoOverrides"],
        &["twitcastingOverrides"],
    ] {
        if config_field(&config, path).is_some_and(|value| !value.is_object()) {
            return Err(BackupError::Invalid("設定の形式"));
        }
    }
    Ok(config)
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|_| BackupError::Invalid("JSON の保存形式"))
}

fn empty_credentials() -> Value {
    serde_json::to_value(CredentialsConfig::default()).expect("default credentials serialize")
}

fn collect_secret(value: Option<&Value>, secrets: &mut BTreeSet<String>) {
    // 設定は前後の空白を除いて使う。写しが空白なしで残っていても見つける。
    let Some(value) = value.and_then(Value::as_str).map(str::trim) else {
        return;
    };
    // Twitch accepts the OAuth token with or without the "oauth:" prefix.
    for secret in [value, value.strip_prefix("oauth:").unwrap_or(value)] {
        if secret.chars().take(8).count() < 8 {
            continue;
        }
        secrets.insert(regex::escape(secret));
        if let Ok(encoded) = serde_json::to_string(secret) {
            secrets.insert(regex::escape(&encoded[1..encoded.len() - 1]));
        }
    }
}

/// reqwest と同じ `url::Url` の解釈でユーザー情報を消した URL。無ければ None。
/// 解析できない値(ホストを持たない値を含む)で `@` を含むものは、安全側に倒して空にする。
fn strip_user_info(value: &str) -> Option<String> {
    match url::Url::parse(value) {
        Ok(mut url) if !url.cannot_be_a_base() => {
            if url.username().is_empty() && url.password().is_none() {
                return None;
            }
            url.set_username("").ok()?;
            url.set_password(None).ok()?;
            Some(url.into())
        }
        _ if value.contains('@') => Some(String::new()),
        _ => None,
    }
}

fn strip_url_user_info(value: &mut Value) {
    match value {
        Value::String(url) => {
            if let Some(stripped) = strip_user_info(url) {
                *url = stripped;
            }
        }
        Value::Object(map) => map.values_mut().for_each(strip_url_user_info),
        _ => {}
    }
}

fn config_field<'a>(
    config: &'a serde_json::Map<String, Value>,
    path: &[&str],
) -> Option<&'a Value> {
    let (field, parents) = path.split_last()?;
    let mut object = config;
    for parent in parents {
        object = object.get(*parent)?.as_object()?;
    }
    object.get(*field)
}

fn set_config_field(
    config: &mut serde_json::Map<String, Value>,
    path: &[&str],
    value: Option<Value>,
) {
    let (field, parents) = path.split_last().expect("config field");
    let mut object = config;
    for parent in parents {
        let child = if value.is_some() {
            object
                .entry(*parent)
                .or_insert_with(|| Value::Object(Default::default()))
        } else if let Some(child) = object.get_mut(*parent) {
            child
        } else {
            return;
        };
        let Some(child) = child.as_object_mut() else {
            return;
        };
        object = child;
    }
    if let Some(value) = value {
        object.insert((*field).into(), value);
    } else {
        object.remove(*field);
    }
}

/// 設定にある鍵・トークン・Cookie を、残存検査用の正規表現として集める。
fn collect_secrets(config: &serde_json::Map<String, Value>) -> BTreeSet<String> {
    let mut secrets = BTreeSet::new();
    for field in CREDENTIAL_SECRETS {
        collect_secret(config_field(config, &["credentials", field]), &mut secrets);
    }
    for path in OVERRIDE_SECRETS {
        collect_secret(config_field(config, path), &mut secrets);
    }
    secrets
}

/// zip と設定の書き出しで共通の伏せ方。`credentials` の扱いだけ呼び出し側で分かれる。
fn redact_overrides(config: &mut serde_json::Map<String, Value>) {
    for path in OVERRIDE_SECRETS {
        set_config_field(config, path, None);
    }
    for path in URL_FIELDS {
        if let Some(mut value) = config_field(config, path).cloned() {
            strip_url_user_info(&mut value);
            set_config_field(config, path, Some(value));
        }
    }
}

fn contains_secret<'a>(
    secrets: BTreeSet<String>,
    mut haystacks: impl Iterator<Item = &'a [u8]>,
) -> Result<bool> {
    if secrets.is_empty() {
        return Ok(false);
    }
    let matches =
        regex::bytes::RegexSet::new(secrets).map_err(|_| BackupError::Invalid("資格情報の検査"))?;
    Ok(haystacks.any(|bytes| matches.is_match(bytes)))
}

/// 設定の書き出し(SPEC §8.1)。zip と同じ欄を伏せ、`credentials` と棒読みちゃんの起動パスは欄ごと消す。
/// 伏せた後も 8 文字以上の鍵・トークン・Cookie が残る設定は書き出さない。
pub(crate) fn export_config(config: &AppConfig) -> std::result::Result<Value, &'static str> {
    let Ok(Value::Object(mut exported)) = serde_json::to_value(config) else {
        return Err("設定を書き出せません");
    };
    let secrets = collect_secrets(&exported);
    redact_overrides(&mut exported);
    set_config_field(&mut exported, CREDENTIALS, None);
    set_config_field(&mut exported, BOUYOMI_PATH, None);
    let exported = Value::Object(exported);
    let text = exported.to_string();
    match contains_secret(secrets, std::iter::once(text.as_bytes())) {
        Ok(false) => Ok(exported),
        Ok(true) => Err(EXPORT_SECRET_ERROR),
        Err(_) => Err(EXPORT_CHECK_ERROR),
    }
}

/// 設定の読み込み(SPEC §8.1)の入力。`config` がオブジェクトでないファイルは、型変換の前に拒否する
/// (serde は配列からも struct を読める)。エラー文は固定。
pub(crate) fn parse_import(value: Value) -> std::result::Result<AppConfig, String> {
    if !value.is_object() {
        return Err("設定の形式が不正です (項目 .)".into());
    }
    AppConfig::from_config_update(value)
}

fn loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

/// reqwest と同じ `url::Url` の解釈で、接続先がこの PC(localhost、127.0.0.0/8、::1)か。
fn loopback_url(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    match url.host() {
        Some(url::Host::Domain(domain)) => loopback_host(domain),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    }
}

/// 設定の読み込み(SPEC §8.1)。`incoming` はファイルから型変換した設定、戻り値は保存する設定。
/// - 資格情報、上書き用の鍵、外部プログラムの起動、OAuth の待受ポート、X の接続先は今の値にする
///   (旧版の書き出しに残る資格情報も適用しない)
/// - 読み上げの本文の送り先(VOICEVOX の URL、棒読みちゃんのホスト)は、ファイルの値がこの PC を
///   指す時だけ使う。ポートはファイルの値を使う
/// - それ以外の URL の欄はファイルの値を使い、ユーザー情報を消す
/// - X の自動告知は無効にする(今の資格情報と他人のファイルの組み合わせで投稿させない)
pub(crate) fn merge_import(
    incoming: &AppConfig,
    current: &AppConfig,
) -> std::result::Result<AppConfig, &'static str> {
    // 型変換した後の値は必ず名前で辿れる(配列で書かれたファイルでも差し替えを外さない)。
    let (Ok(Value::Object(mut merged)), Ok(Value::Object(kept))) =
        (serde_json::to_value(incoming), serde_json::to_value(current))
    else {
        return Err(IMPORT_ERROR);
    };
    redact_overrides(&mut merged);
    for path in kept_fields() {
        set_config_field(&mut merged, path, config_field(&kept, path).cloned());
    }
    let mut merged: AppConfig =
        serde_json::from_value(Value::Object(merged)).map_err(|_| IMPORT_ERROR)?;
    let (options, current_options) = (&mut merged.tts.options, &current.tts.options);
    // 接続先が今と同じなら、今の値(ユーザー情報を含む)をそのまま残す。保存直前にもう一度
    // マージするので、1 回目で残した今の値が 2 回目で伏せられないようにするためでもある。
    let without_user_info = |value: &str| match url::Url::parse(value) {
        Ok(mut url) if !url.cannot_be_a_base() => {
            let _ = url.set_username("");
            let _ = url.set_password(None);
            String::from(url)
        }
        _ => value.to_owned(),
    };
    if !loopback_url(&options.voicevox_url)
        || without_user_info(&options.voicevox_url)
            == without_user_info(&current_options.voicevox_url)
    {
        options.voicevox_url = current_options.voicevox_url.clone();
    }
    if !loopback_host(options.bouyomi_host.trim()) {
        options.bouyomi_host = current_options.bouyomi_host.clone();
    }
    merged.x_post.enabled = false;
    Ok(merged)
}

/// 読み込みがファイルの値を使わなかったことを知らせる固定の文。`merged` は `merge_import` の結果。
pub(crate) fn import_notices(
    file: &AppConfig,
    current: &AppConfig,
    merged: &AppConfig,
) -> Vec<&'static str> {
    use crate::config::TtsBackendKind::Bouyomi;
    let mut notices = Vec::new();
    if file.x_post.enabled || current.x_post.enabled {
        notices.push(X_POST_DISABLED_BY_IMPORT);
    }
    // 取り込みでは、保存後の自動起動をしない(ファイルの接続先・ポートに合わせて起動させない)。
    if merged.tts.backend == Bouyomi && current.tts.backend != Bouyomi {
        notices.push(BOUYOMI_NOT_LAUNCHED_BY_IMPORT);
    }
    let (file, current) = (&file.tts.options, &current.tts.options);
    if (!loopback_url(&file.voicevox_url) && file.voicevox_url != current.voicevox_url)
        || (!loopback_host(file.bouyomi_host.trim()) && file.bouyomi_host != current.bouyomi_host)
    {
        notices.push(TTS_DESTINATION_KEPT);
    }
    notices
}

fn snapshot(config_dir: &Path, now_ms: u64) -> Result<Contents> {
    let mut contents = Contents::default();
    for &name in FILES {
        let path = config_dir.join(name);
        if let Some(meta) = metadata(&path)? {
            check_kind(&meta, false)?;
            contents.read_file(name.into(), meta.len(), File::open(path)?)?;
        }
    }
    for &name in DIRECTORIES {
        let path = config_dir.join(name);
        if directory_exists(&path)? {
            contents.read_directory(&path, name, false)?;
        }
    }
    let mut secrets = BTreeSet::new();
    if let Some(bytes) = contents.files.get_mut("config.json") {
        let mut config = config_object(bytes)?;
        secrets = collect_secrets(&config);
        redact_overrides(&mut config);
        set_config_field(&mut config, CREDENTIALS, Some(empty_credentials()));
        contents.total_bytes -= bytes.len() as u64;
        *bytes = encode(&config)?;
        if bytes.len() as u64 > MAX_FILE_BYTES
            || bytes.len() as u64 > MAX_TOTAL_BYTES - contents.total_bytes
        {
            return Err(BackupError::Invalid("容量の上限超過"));
        }
        contents.total_bytes += bytes.len() as u64;
    }
    let manifest = encode(&Manifest {
        schema: SCHEMA.into(),
        version: 1,
        created_at_ms: now_ms,
        app_version: env!("CARGO_PKG_VERSION").into(),
        entries: contents.files.keys().cloned().collect(),
    })?;
    contents.read_file(MANIFEST.into(), manifest.len() as u64, manifest.as_slice())?;
    contents.validate()?;
    // 復元できない写しで正常な世代を押し出さない。
    contents.validate_stores()?;
    let files = contents
        .files
        .iter()
        .flat_map(|(name, bytes)| [name.as_bytes(), bytes.as_slice()]);
    if contains_secret(secrets, files)? {
        return Err(BackupError::Invalid(
            "バックアップ対象に資格情報が含まれています",
        ));
    }
    Ok(contents)
}

fn read_archive(path: &Path) -> Result<Contents> {
    let invalid_zip = |_| BackupError::Invalid("zip の形式");
    let mut archive = ZipArchive::new(File::open(path)?).map_err(invalid_zip)?;
    if archive.has_duplicate_names() || archive.len() > MAX_ENTRIES {
        return Err(BackupError::Invalid("zip のエントリ数または重複"));
    }
    let mut contents = Contents::default();
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(invalid_zip)?;
        let raw = std::str::from_utf8(entry.name_raw())
            .map_err(|_| BackupError::Invalid("パスの文字コード"))?;
        let directory = raw.ends_with('/');
        let name = raw.strip_suffix('/').unwrap_or(raw).to_owned();
        let kind = entry.unix_mode().unwrap_or_default() & 0o170000;
        if entry.encrypted()
            || entry.is_symlink()
            || (kind != 0 && kind != if directory { 0o040000 } else { 0o100000 })
        {
            return Err(BackupError::Invalid("zip のエントリ種別"));
        }
        validate_path(&name, directory)?;
        let size = entry.size();
        drop(entry);
        contents.check_limit(size)?;
        let entry = archive.by_index(index).map_err(invalid_zip)?;
        if directory {
            if size != 0 || entry.bytes().next().transpose()?.is_some() {
                return Err(BackupError::Invalid("zip のディレクトリエントリ"));
            }
            contents.directories.insert(name);
        } else {
            let total_before = contents.total_bytes;
            contents.read_file(name, size, entry)?;
            if contents.total_bytes - total_before != size {
                return Err(BackupError::Invalid("zip の展開サイズ"));
            }
        }
    }
    contents.validate()?;
    Ok(contents)
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result.map_err(BackupError::from)
}

pub fn create_backup(config_dir: &Path, now_ms: u64) -> Result<Option<PathBuf>> {
    create_backup_with(config_dir, now_ms, || Ok(()))
}

/// `flush` はストアを書き出し、読み取りの間だけ持つ値(ロック)を返す。写しを取り終えたら放す。
fn create_backup_with<G>(
    config_dir: &Path,
    now_ms: u64,
    flush: impl FnOnce() -> Result<G>,
) -> Result<Option<PathBuf>> {
    let dir = auto_dir(config_dir)?;
    let backups = read_backups(config_dir)?;
    // 連続作成や時計が戻った場合にも、新しいファイルを最新世代にする。
    let mut stamp = now_ms;
    if let Some(latest) = backups.first() {
        stamp = stamp.max(latest.created_at_ms.saturating_add(1));
    }
    let previous_digest = backups
        .first()
        .and_then(|previous| read_archive(&dir.join(&previous.file_name)).ok())
        .map(|previous| previous.digest());
    let held = flush()?;
    let contents = snapshot(config_dir, stamp);
    drop(held);
    let contents = contents?;
    if previous_digest.is_some_and(|previous| previous == contents.digest()) {
        return Ok(None);
    }
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("fc-backup-{stamp}.zip"));
    if metadata(&path)?.is_some() {
        return Err(BackupError::Invalid("バックアップ名の衝突"));
    }
    let tmp = dir.join(format!("{}.tmp", Uuid::new_v4()));
    let file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
    let result = (|| {
        let zip_error = |_| BackupError::Io("zip".into());
        let mut zip = ZipWriter::new(file);
        let options = SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        for (name, bytes) in contents.files {
            zip.start_file(name, options).map_err(zip_error)?;
            zip.write_all(&bytes)?;
        }
        zip.finish().map_err(zip_error)?.sync_all()?;
        fs::rename(&tmp, &path)?;
        Ok(Some(path))
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn stage_restore(config_dir: &Path, backup_file_name: &str, now_ms: u64) -> Result<()> {
    stage_restore_with(config_dir, backup_file_name, now_ms, || Ok(()))
}

/// `flush` は、現在の状態の安全バックアップを取る直前に呼ぶ(`create_backup_with` と同じ)。
pub(crate) fn stage_restore_with<G>(
    config_dir: &Path,
    backup_file_name: &str,
    now_ms: u64,
    flush: impl FnOnce() -> Result<G>,
) -> Result<()> {
    if timestamp(backup_file_name).is_none() {
        return Err(BackupError::Invalid("バックアップ名"));
    }
    let path = auto_dir(config_dir)?.join(backup_file_name);
    let meta = metadata(&path)?.ok_or(BackupError::NotFound)?;
    check_kind(&meta, false)?;
    let contents = read_archive(&path)?;
    contents.validate_stores()?;
    let pending = backups_dir(config_dir)?.join("restore-pending");
    if metadata(&pending)?.is_some() {
        return Err(BackupError::Invalid("準備済みの復元があります"));
    }

    fs::create_dir(&pending)?;
    let result = (|| {
        for name in contents.directories {
            fs::create_dir_all(pending.join(name))?;
        }
        for (name, bytes) in contents.files {
            let path = pending.join(name);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            write_new(&path, &bytes)?;
        }
        write_new(&pending.join(SOURCE), backup_file_name.as_bytes())?;
        // 適用するまで prune しない(失敗した復元の復元元を、別の世代の準備で押し出さない)。
        create_backup_with(config_dir, now_ms, flush)?;
        let ready_tmp = pending.join("READY.tmp");
        write_new(&ready_tmp, b"")?;
        fs::rename(ready_tmp, pending.join(READY))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}

fn read_config(config_dir: &Path) -> Result<Option<serde_json::Map<String, Value>>> {
    let path = config_dir.join("config.json");
    let Some(meta) = metadata(&path)? else {
        return Ok(None);
    };
    check_kind(&meta, false)?;
    let mut contents = Contents::default();
    contents.read_file("config.json".into(), meta.len(), File::open(path)?)?;
    config_object(&contents.files["config.json"]).map(Some)
}

fn read_pending(pending: &Path) -> Result<Contents> {
    let mut contents = Contents::default();
    for entry in fs::read_dir(pending)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| BackupError::Invalid("パスの文字コード"))?;
        let meta = fs::symlink_metadata(entry.path())?;
        let directory = meta.is_dir();
        check_kind(&meta, directory)?;
        if name == READY || name == SOURCE {
            check_kind(&meta, false)?;
            continue;
        }
        // APPLYING 公開前に中断した場合の一時ファイル。
        if [PREPARED_CONFIG, "APPLYING.tmp"].contains(&name.as_str()) {
            check_kind(&meta, false)?;
            fs::remove_file(entry.path())?;
            continue;
        }
        validate_path(&name, directory)?;
        if directory {
            contents.directories.insert(name.clone());
            contents.read_directory(&entry.path(), &name, true)?;
        } else {
            contents.read_file(name, meta.len(), File::open(entry.path())?)?;
        }
    }
    // 展開時に補われる親ディレクトリは zip のエントリ数に含まれない。
    let parents: BTreeSet<_> = contents
        .files
        .keys()
        .chain(&contents.directories)
        .filter_map(|name| name.rsplit_once('/').map(|(parent, _)| parent))
        .collect();
    let empty_dirs = contents
        .directories
        .iter()
        .filter(|name| !parents.contains(name.as_str()))
        .count();
    if contents.files.len() + empty_dirs > MAX_ENTRIES {
        return Err(BackupError::Invalid("エントリ数の上限超過"));
    }
    contents.validate()?;
    contents.validate_stores()?;
    Ok(contents)
}

#[derive(Serialize, Deserialize)]
struct RestoreJournal {
    stamp: u64,
    roots: Vec<String>,
    #[serde(default)]
    absent: BTreeSet<String>,
}

fn saved_path(config_dir: &Path, root: &str, stamp: u64) -> PathBuf {
    config_dir.join(format!("{root}.pre-restore-{stamp}"))
}

fn incoming_path(pending: &Path, root: &str) -> PathBuf {
    pending.join(if root == "config.json" {
        PREPARED_CONFIG
    } else {
        root
    })
}

fn rollback_root(
    config_dir: &Path,
    pending: &Path,
    journal: &RestoreJournal,
    root: &str,
) -> Result<()> {
    let target = config_dir.join(root);
    let saved = saved_path(config_dir, root, journal.stamp);
    let incoming = incoming_path(pending, root);
    if !journal.absent.contains(root)
        && metadata(&incoming)?.is_none()
        && metadata(&target)?.is_some()
    {
        fs::rename(&target, &incoming)?;
    }
    if metadata(&saved)?.is_some() {
        fs::rename(&saved, &target)?;
    }
    Ok(())
}

fn rollback(
    config_dir: &Path,
    pending: &Path,
    journal: &RestoreJournal,
    blocked: &mut Vec<String>,
) -> Result<()> {
    for root in journal.roots.iter().rev() {
        if retry_rollback(|| rollback_root(config_dir, pending, journal, root)).is_err() {
            blocked.push(root.clone());
        }
    }
    if blocked.is_empty() {
        Ok(())
    } else {
        Err(BackupError::Invalid("復元の取り消しが完了していません"))
    }
}

fn retry_rollback<T>(mut operation: impl FnMut() -> Result<T>) -> Result<T> {
    for _ in 0..2 {
        if let Ok(value) = operation() {
            return Ok(value);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    operation()
}

fn prepare_config(config_dir: &Path, snapshot: Option<&[u8]>) -> Result<Option<Vec<u8>>> {
    let mut current = read_config(config_dir)?.unwrap_or_default();
    let mut restored = snapshot.map(json_object).transpose()?.unwrap_or_default();
    if snapshot.is_none() {
        // An absent file carries forward only effective local values.
        let typed = serde_json::from_value::<AppConfig>(Value::Object(current))
            .map_err(|_| BackupError::Invalid("設定の形式"))?;
        current = serde_json::to_value(typed)
            .expect("config serializes")
            .as_object()
            .expect("config object")
            .clone();
        let defaults =
            serde_json::to_value(AppConfig::default()).expect("default config serializes");
        let defaults = defaults.as_object().expect("config object");
        for path in URL_FIELDS.iter().copied().chain(kept_fields()) {
            if config_field(&current, path) == config_field(defaults, path) {
                set_config_field(&mut current, path, None);
            }
        }
    }
    for path in URL_FIELDS.iter().copied().chain(kept_fields()) {
        set_config_field(&mut restored, path, config_field(&current, path).cloned());
    }
    if snapshot.is_none() && restored.is_empty() {
        return Ok(None);
    }
    let bytes = encode(&restored)?;
    serde_json::from_slice::<AppConfig>(&bytes)
        .map_err(|_| BackupError::Invalid("復元後の設定の形式"))?;
    Ok(Some(bytes))
}

fn install_pending(config_dir: &Path, pending: &Path, now_ms: u64) -> Result<()> {
    let contents = read_pending(pending)?;
    let config = prepare_config(
        config_dir,
        contents.files.get("config.json").map(Vec::as_slice),
    )?;
    let mut journal = RestoreJournal {
        stamp: now_ms,
        roots: Vec::new(),
        absent: BTreeSet::new(),
    };
    for &root in FILES.iter().chain(DIRECTORIES) {
        let incoming = if root == "config.json" {
            config.is_some()
        } else {
            metadata(&pending.join(root))?.is_some()
        };
        let current = metadata(&config_dir.join(root))?;
        if !incoming && current.is_none() {
            continue;
        }
        if metadata(&saved_path(config_dir, root, now_ms))?.is_some() {
            return Err(BackupError::Invalid("復元の退避名が重複しています"));
        }
        if let Some(meta) = current {
            check_kind(&meta, DIRECTORIES.contains(&root))?;
        }
        if !incoming {
            journal.absent.insert(root.into());
        }
        journal.roots.push(root.into());
    }
    drop(contents);
    if let Some(config) = config {
        write_new(&pending.join(PREPARED_CONFIG), &config)?;
    }
    // 起動が中断されても、次回はこの記録を使って置き換えの途中から戻せる。
    let temp_journal = pending.join("APPLYING.tmp");
    write_new(&temp_journal, &encode(&journal)?)?;
    fs::rename(temp_journal, pending.join(APPLYING))?;

    for root in &journal.roots {
        let target = config_dir.join(root);
        if metadata(&target)?.is_some() {
            fs::rename(&target, saved_path(config_dir, root, now_ms))?;
        }
        if !journal.absent.contains(root) {
            fs::rename(incoming_path(pending, root), target)?;
        }
    }
    let applied = pending.with_file_name(format!("restore-applied-{}.tmp", Uuid::new_v4()));
    fs::rename(pending, &applied)?;
    Ok(())
}

fn read_journal(pending: &Path) -> Result<Option<RestoreJournal>> {
    let path = pending.join(APPLYING);
    let Some(meta) = metadata(&path)? else {
        return Ok(None);
    };
    check_kind(&meta, false)?;
    let mut data = Vec::new();
    File::open(path)?.take(4096).read_to_end(&mut data)?;
    let journal: RestoreJournal =
        serde_json::from_slice(&data).map_err(|_| BackupError::Invalid("復元の作業記録"))?;
    let roots: BTreeSet<_> = journal.roots.iter().collect();
    if roots.len() != journal.roots.len()
        || journal.absent.iter().any(|root| !roots.contains(root))
        || roots
            .iter()
            .any(|root| !FILES.contains(&root.as_str()) && !DIRECTORIES.contains(&root.as_str()))
    {
        return Err(BackupError::Invalid("復元の作業記録"));
    }
    Ok(Some(journal))
}

fn recover_pending(config_dir: &Path, pending: &Path, blocked: &mut Vec<String>) -> Result<()> {
    let journal = retry_rollback(|| read_journal(pending)).inspect_err(|_| block_all(blocked))?;
    if let Some(journal) = journal {
        rollback(config_dir, pending, &journal, blocked)?;
    }
    Ok(())
}

fn prune_pre_restore(config_dir: &Path) -> Result<()> {
    if !directory_exists(config_dir)? {
        return Ok(());
    }
    let mut generations = BTreeMap::<u64, Vec<(PathBuf, bool)>>::new();
    for entry in fs::read_dir(config_dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some((root, digits)) = name
            .to_str()
            .and_then(|name| name.split_once(".pre-restore-"))
        else {
            continue;
        };
        let Some(stamp) = numeric_timestamp(digits) else {
            continue;
        };
        let directory = DIRECTORIES.contains(&root);
        if (directory || FILES.contains(&root))
            && check_kind(&fs::symlink_metadata(entry.path())?, directory).is_ok()
        {
            generations
                .entry(stamp)
                .or_default()
                .push((entry.path(), directory));
        }
    }
    let mut first_error = None;
    for (path, directory) in generations
        .into_iter()
        .rev()
        .skip(3)
        .flat_map(|(_, files)| files)
    {
        let result = if directory {
            fs::remove_dir_all(path)
        } else {
            fs::remove_file(path)
        };
        keep_first_error(&mut first_error, result.map_err(Into::into));
    }
    first_error.map_or(Ok(()), Err)
}

fn failed_timestamp(name: &str) -> Option<u64> {
    let suffix = name.strip_prefix("restore-failed-")?;
    if let Some((stamp, id)) = suffix.split_once('-') {
        Uuid::parse_str(id).ok()?;
        numeric_timestamp(stamp)
    } else {
        numeric_timestamp(suffix)
    }
}

/// 復元に失敗した世代(`restore-failed-*`)が記録している復元元の名前。
fn failed_source(failed: &Path) -> Result<Option<String>> {
    let path = failed.join(SOURCE);
    let Some(meta) = metadata(&path)? else {
        return Ok(None);
    };
    check_kind(&meta, false)?;
    let mut data = Vec::new();
    File::open(path)?.take(256).read_to_end(&mut data)?;
    Ok(String::from_utf8(data)
        .ok()
        .filter(|name| timestamp(name).is_some()))
}

fn protected_sources(dir: &Path) -> Result<BTreeSet<String>> {
    let mut sources = BTreeSet::new();
    if !directory_exists(dir)? {
        return Ok(sources);
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let is_failed = entry.file_name().to_str().and_then(failed_timestamp).is_some();
        if is_failed && check_kind(&fs::symlink_metadata(entry.path())?, true).is_ok() {
            sources.extend(failed_source(&entry.path())?);
        }
    }
    Ok(sources)
}

fn keep_first_error(first: &mut Option<BackupError>, result: Result<()>) {
    if let Err(error) = result {
        first.get_or_insert(error);
    }
}

fn remove_auto_tmp(entry: io::Result<fs::DirEntry>) -> Result<()> {
    let entry = entry?;
    if entry
        .path()
        .extension()
        .is_some_and(|extension| extension == "tmp")
        && check_kind(&fs::symlink_metadata(entry.path())?, false).is_ok()
    {
        fs::remove_file(entry.path())?;
    }
    Ok(())
}

/// restore-applied-* を消し、restore-failed-* の世代を集める。復元元を記録した世代は集めない
/// (次に復元が成功した起動で記録を消し、通常の扱いに戻す)。
fn sweep_restore_dir(
    entry: io::Result<fs::DirEntry>,
    restored: bool,
    failed: &mut Vec<(u64, PathBuf)>,
) -> Result<()> {
    let entry = entry?;
    let name = entry.file_name();
    let Some(name) = name.to_str() else {
        return Ok(());
    };
    let path = entry.path();
    if check_kind(&fs::symlink_metadata(&path)?, true).is_err() {
        return Ok(());
    }
    if name.starts_with("restore-applied-") && name.ends_with(".tmp") {
        fs::remove_dir_all(path)?;
    } else if let Some(stamp) = failed_timestamp(name) {
        if restored {
            match fs::remove_file(path.join(SOURCE)) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
                _ => {}
            }
        }
        if failed_source(&path)?.is_none() {
            failed.push((stamp, path));
        }
    }
    Ok(())
}

/// 起動時の掃除。1 件の失敗で残りを打ち切らず、最初の失敗を返す。
fn cleanup_backup_work_files(config_dir: &Path, dir: &Path, restored: bool) -> Result<()> {
    let mut first = None;
    let auto = auto_dir(config_dir)?;
    if directory_exists(&auto)? {
        for entry in fs::read_dir(auto)? {
            keep_first_error(&mut first, remove_auto_tmp(entry));
        }
    }
    if directory_exists(dir)? {
        let mut failed = Vec::new();
        for entry in fs::read_dir(dir)? {
            keep_first_error(&mut first, sweep_restore_dir(entry, restored, &mut failed));
        }
        failed.sort_unstable();
        for (_, path) in failed.into_iter().rev().skip(3) {
            keep_first_error(&mut first, fs::remove_dir_all(path).map_err(Into::into));
        }
    }
    first.map_or(Ok(()), Err)
}

pub fn apply_pending_restore(config_dir: &Path, now_ms: u64) -> Result<bool> {
    apply_pending_restore_at_startup(config_dir, now_ms).0
}

pub(crate) fn apply_pending_restore_at_startup(
    config_dir: &Path,
    now_ms: u64,
) -> (Result<bool>, Vec<String>) {
    let mut blocked = Vec::new();
    let result = (|| {
        let dir = backups_dir(config_dir).inspect_err(|error| {
            if matches!(error, BackupError::Io(_)) {
                block_all(&mut blocked);
            }
        })?;
        let result = apply_restore(config_dir, &dir, now_ms, &mut blocked);
        let restored = matches!(result, Ok(true));
        if let Err(error) = cleanup_backup_work_files(config_dir, &dir, restored) {
            tracing::warn!("バックアップの作業ファイルを整理できませんでした: {error}");
        }
        if blocked.is_empty() {
            if let Err(error) = prune_pre_restore(config_dir) {
                tracing::warn!("復元の退避ファイルを整理できませんでした: {error}");
            }
        }
        result
    })();
    (result, blocked)
}

fn block_all(blocked: &mut Vec<String>) {
    // If the journal is unreadable, any store may still need recovery.
    blocked.extend(FILES.iter().chain(DIRECTORIES).map(|root| (*root).into()));
}

fn apply_restore(
    config_dir: &Path,
    dir: &Path,
    now_ms: u64,
    blocked: &mut Vec<String>,
) -> Result<bool> {
    let pending = dir.join("restore-pending");
    if !directory_exists(&pending).inspect_err(|_| block_all(blocked))? {
        return Ok(false);
    }
    let applying = metadata(&pending.join(APPLYING))
        .inspect_err(|_| block_all(blocked))?
        .is_some();
    let error = if applying {
        recover_pending(config_dir, &pending, blocked)?;
        BackupError::Invalid("前回の復元が中断されました")
    } else {
        let Some(ready) = metadata(&pending.join(READY))? else {
            fs::remove_dir_all(pending)?;
            return Ok(false);
        };
        match check_kind(&ready, false).and_then(|()| install_pending(config_dir, &pending, now_ms))
        {
            Ok(()) => return Ok(true),
            Err(error) => {
                recover_pending(config_dir, &pending, blocked)?;
                error
            }
        }
    };
    let mut failed = dir.join(format!("restore-failed-{now_ms}"));
    if metadata(&failed)?.is_some() {
        failed = dir.join(format!("restore-failed-{now_ms}-{}", Uuid::new_v4()));
    }
    fs::rename(&pending, &failed)?;
    match fs::remove_file(failed.join(PREPARED_CONFIG)) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => tracing::warn!("復元用の一時設定を削除できませんでした"),
    }
    Err(error)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, name: &str, body: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    #[test]
    fn public_identifiers_and_short_secrets_allow_backup() {
        for (username, token) in [("my_twitch_channel", ""), ("", "short77")] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(
                root,
                "config.json",
                &serde_json::json!({
                    "channels": [{"platform": "twitch", "identifier": "my_twitch_channel"}],
                    "credentials": {"twitchUsername": username, "xCsrfToken": token},
                })
                .to_string(),
            );
            write(root, "skin/short77.css", "short77");
            assert!(create_backup(root, 1).unwrap().is_some());
            stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        }
    }

    #[test]
    fn stage_keeps_restore_source_alongside_safety_backup() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "config.json", r#"{"backup":{"keep":1}}"#);
        let source = create_backup(root, 1).unwrap().unwrap();
        write(root, "listeners.json", "{}");
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(source.is_file());
        assert!(root.join("backups/auto/fc-backup-2.zip").is_file());
        assert!(root.join("backups/restore-pending/READY").is_file());
    }

    #[test]
    fn restore_removes_roots_missing_from_snapshot() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        create_backup(root, 1).unwrap();
        write(
            root,
            "config.json",
            r#"{"credentials":{},"backup":{"keep":8}}"#,
        );
        write(root, "listeners.json", "{}");
        write(root, "templates/custom/file.html", "current");
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(apply_pending_restore(root, 3).unwrap());
        assert!(!root.join("config.json").exists());
        assert!(!root.join("listeners.json").exists());
        assert!(!root.join("templates").exists());
        assert!(root.join("listeners.json.pre-restore-3").is_file());
        assert!(root
            .join("templates.pre-restore-3/custom/file.html")
            .is_file());
    }

    #[test]
    fn stage_and_apply_reject_invalid_store_types() {
        for (name, body) in [
            ("listeners.json", r#"{"viewer":1}"#),
            ("achievements.json", r#"{"version":1,"achievements":false}"#),
            (
                "achievement-progress.json",
                r#"{"version":1,"unlocks":false}"#,
            ),
            ("sounds.json", r#"{"sounds":false}"#),
            ("sounds/sounds.json", r#"{"sounds":false}"#),
            ("config.json", r#"{"channels":false}"#),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let manifest = encode(&Manifest {
                schema: SCHEMA.into(),
                version: 1,
                created_at_ms: 1,
                app_version: String::new(),
                entries: vec![name.into()],
            })
            .unwrap();
            let archive = root.join("backups/auto/fc-backup-1.zip");
            fs::create_dir_all(archive.parent().unwrap()).unwrap();
            let mut zip = ZipWriter::new(File::create(archive).unwrap());
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
            zip.start_file(MANIFEST, SimpleFileOptions::default())
                .unwrap();
            zip.write_all(&manifest).unwrap();
            zip.finish().unwrap();

            assert!(
                matches!(
                    stage_restore(root, "fc-backup-1.zip", 2),
                    Err(BackupError::Invalid(_))
                ),
                "{name}"
            );
            let pending = root.join("backups/restore-pending");
            assert!(!pending.exists(), "{name}");
            assert_eq!(list_backups(root).len(), 1, "{name}");
            write(&pending, name, body);
            write(&pending, MANIFEST, std::str::from_utf8(&manifest).unwrap());
            write(&pending, READY, "");
            assert!(
                matches!(apply_pending_restore(root, 3), Err(BackupError::Invalid(_))),
                "{name}"
            );
            assert!(!root.join(name).exists(), "{name}");
        }
    }

    #[test]
    fn backup_empties_override_secrets() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "config.json",
            r#"{
            "xOverrides":{"bearerToken":"private-bearer-value"},
            "youtubeOverrides":{"apiKey":"private-youtube-key"}
        }"#,
        );
        let path = create_backup(root, 1).unwrap().unwrap();
        let archive = read_archive(&path).unwrap();
        let config: AppConfig = serde_json::from_slice(&archive.files["config.json"]).unwrap();
        assert!(config.x_overrides.bearer_token.is_none());
        assert!(config.youtube_overrides.api_key.is_none());
    }

    #[test]
    fn restore_keeps_current_endpoints_and_program_settings() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "config.json",
            r#"{
            "xOverrides":{"bearerToken":"old-bearer","endpoints":{"userQueryUrl":"https://old.invalid"},"livePollSecs":60},
            "youtubeOverrides":{"apiKey":"old-youtube"},
            "tts":{"options":{"bouyomiPath":"old.exe","bouyomiLaunchElevated":true}}
        }"#,
        );
        create_backup(root, 1).unwrap();
        write(
            root,
            "config.json",
            r#"{
            "xOverrides":{"bearerToken":"current-bearer","endpoints":{"avatarContentUrl":"https://current.invalid"}},
            "youtubeOverrides":{"apiKey":"current-youtube"},
            "tts":{"options":{"bouyomiPath":"current.exe","bouyomiLaunchElevated":false}}
        }"#,
        );
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(apply_pending_restore(root, 3).unwrap());
        let config = AppConfig::load(root).unwrap();
        assert_eq!(
            config.x_overrides.endpoints,
            [("avatarContentUrl".into(), "https://current.invalid".into())].into()
        );
        assert_eq!(config.tts.options.bouyomi_path, "current.exe");
        assert!(!config.tts.options.bouyomi_launch_elevated);
        assert_eq!(
            config.x_overrides.bearer_token.as_deref(),
            Some("current-bearer")
        );
        assert_eq!(
            config.youtube_overrides.api_key.as_deref(),
            Some("current-youtube")
        );
        assert_eq!(config.x_overrides.live_poll_secs, Some(60));
    }

    #[test]
    fn startup_cleans_work_files_and_keeps_three_generations() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for stamp in [1, 2, 9, 10] {
            write(root, &format!("backups/restore-failed-{stamp}/READY"), "");
            write(root, &format!("listeners.json.pre-restore-{stamp}"), "{}");
        }
        write(root, "backups/restore-applied-id.tmp/READY", "");
        write(root, "backups/auto/abandoned.tmp", "");
        write(root, "backups/auto/notes.txt", "keep");
        assert!(!apply_pending_restore(root, 11).unwrap());
        assert!(!root.join("backups/restore-failed-1").exists());
        assert!(!root.join("listeners.json.pre-restore-1").exists());
        assert!(!root.join("backups/restore-applied-id.tmp").exists());
        assert!(!root.join("backups/auto/abandoned.tmp").exists());
        assert!(root.join("backups/auto/notes.txt").exists());
        for stamp in [2, 9, 10] {
            assert!(root
                .join(format!("backups/restore-failed-{stamp}"))
                .is_dir());
            assert!(root
                .join(format!("listeners.json.pre-restore-{stamp}"))
                .is_file());
        }
    }

    #[test]
    fn templates_and_skin_allow_names_excluded_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "templates/logs/index.html", "template");
        write(root, "skin/a.bak.png", "image");
        write(root, "config.json.tmp", "excluded");
        let path = create_backup(root, 1).unwrap().unwrap();
        let archive = read_archive(&path).unwrap();
        assert!(archive.files.contains_key("templates/logs/index.html"));
        assert!(archive.files.contains_key("skin/a.bak.png"));
        assert!(!archive.files.contains_key("config.json.tmp"));
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(apply_pending_restore(root, 3).unwrap());
    }

    #[test]
    fn recovery_attempts_other_roots_when_one_cannot_roll_back() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pending = root.join("backups/restore-pending");
        let journal = r#"{"stamp":10,"roots":["config.json","listeners.json"]}"#;
        write(&pending, APPLYING, journal);
        write(&pending, READY, "");
        write(&pending, "listeners.json", "{}");
        fs::create_dir(root.join("listeners.json")).unwrap();
        write(root, "listeners.json/conflict", "blocks rename");
        write(root, "listeners.json.pre-restore-10", "{}");
        write(&pending, PREPARED_CONFIG, "{}");
        write(root, "config.json.pre-restore-10", "{}");

        let (result, blocked) = apply_pending_restore_at_startup(root, 11);
        assert!(result.is_err());
        assert_eq!(blocked, ["listeners.json"]);
        assert!(pending.join(APPLYING).is_file());
        assert!(root.join("config.json").is_file());
        fs::remove_dir_all(root.join("listeners.json")).unwrap();
        assert!(apply_pending_restore(root, 12).is_err());
        assert!(root.join("listeners.json").is_file());
        assert!(!pending.exists());
    }

    #[test]
    fn rollback_retries_at_most_three_times() {
        for fail_count in [0, 2, 3, 8] {
            let mut attempts = 0;
            let success = retry_rollback(|| {
                attempts += 1;
                if attempts <= fail_count {
                    Err(io::Error::from(io::ErrorKind::PermissionDenied).into())
                } else {
                    Ok(())
                }
            });
            assert_eq!(attempts, (fail_count + 1).min(3));
            assert_eq!(success.is_ok(), fail_count < 3);
        }
    }

    #[cfg(unix)]
    #[test]
    fn listing_permission_failure_is_reported() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let auto = dir.path().join("backups/auto");
        fs::create_dir_all(&auto).unwrap();
        fs::set_permissions(&auto, fs::Permissions::from_mode(0o000)).unwrap();
        let accessible = fs::read_dir(&auto).is_ok();
        let listing = read_backups(dir.path());
        fs::set_permissions(&auto, fs::Permissions::from_mode(0o700)).unwrap();

        // A privileged runner can list a directory with mode 000.
        if !accessible {
            let error = listing.unwrap_err();
            assert!(matches!(error, BackupError::Io(ref kind) if kind == "PermissionDenied"));
            assert!(error.to_string().contains("アクセス権限"));
        }
    }

    #[test]
    fn capacity_failure_has_a_reason_for_the_backup_panel() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let path = root.join("skin/oversized.png");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        File::create(path)
            .unwrap()
            .set_len(MAX_FILE_BYTES + 1)
            .unwrap();
        let error = create_backup(root, 1).unwrap_err();
        assert!(error.to_string().contains("上限超過"));
        assert!(list_backups(root).is_empty());
    }

    #[test]
    fn read_only_stores_preserve_files_during_recovery() {
        use crate::achievements::store::{DefsStore, PROGRESS_FILE};
        use crate::listeners::ListenerStore;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for name in ["config.json", "listeners.json"] {
            write(root, name, "unreadable");
        }
        let config = AppConfig::load_read_only(root);
        assert!(config.save(root).is_err());
        assert!(ListenerStore::load_read_only(root).flush().is_err());
        assert_eq!(
            fs::read_to_string(root.join("config.json")).unwrap(),
            "unreadable"
        );
        assert_eq!(
            fs::read_to_string(root.join("listeners.json")).unwrap(),
            "unreadable"
        );

        let defs = DefsStore::load_for_startup(root, 1, true, true);
        let data = defs.current();
        assert!(defs.commit(data.revision, data).is_err());
        assert!(defs
            .progress_io()
            .write_atomic(&root.join(PROGRESS_FILE), b"{}")
            .is_err());
        assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    }

    #[test]
    fn startup_rolls_back_an_interrupted_restore() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let pending = root.join("backups/restore-pending");
        fs::create_dir_all(&pending).unwrap();
        fs::write(pending.join(READY), "").unwrap();
        let journal = RestoreJournal {
            stamp: 10,
            absent: BTreeSet::new(),
            roots: vec![
                "config.json".into(),
                "listeners.json".into(),
                "sounds".into(),
            ],
        };
        fs::write(pending.join(APPLYING), encode(&journal).unwrap()).unwrap();
        // 設定は置き換え済み。リスナーは退避だけ済み。効果音にはまだ手を付けていない。
        fs::write(root.join("config.json"), "restored").unwrap();
        fs::write(saved_path(root, "config.json", 10), "original config").unwrap();
        fs::write(saved_path(root, "listeners.json", 10), "original listeners").unwrap();
        fs::write(pending.join("listeners.json"), "pending").unwrap();
        fs::create_dir(root.join("sounds")).unwrap();
        fs::write(root.join("sounds/live.wav"), "original sound").unwrap();
        fs::create_dir(pending.join("sounds")).unwrap();

        assert!(apply_pending_restore(root, 20).is_err());
        assert_eq!(
            fs::read_to_string(root.join("config.json")).unwrap(),
            "original config"
        );
        assert_eq!(
            fs::read_to_string(root.join("listeners.json")).unwrap(),
            "original listeners"
        );
        assert_eq!(
            fs::read_to_string(root.join("sounds/live.wav")).unwrap(),
            "original sound"
        );
        assert!(root.join("backups/restore-failed-20/APPLYING").is_file());
        assert!(!root
            .join("backups/restore-failed-20")
            .join(PREPARED_CONFIG)
            .exists());
        assert!(!apply_pending_restore(root, 21).unwrap());
    }

    #[test]
    fn snapshot_never_writes_a_copy_of_credentials_from_another_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let config = br#"{"credentials":{"xAuthToken":"a-private-test-value"}}"#;
        fs::write(root.join("config.json"), config).unwrap();
        fs::create_dir(root.join("skin")).unwrap();
        fs::write(root.join("skin/custom.css"), "a-private-test-value").unwrap();

        assert!(create_backup(root, 1).is_err());
        assert!(list_backups(root).is_empty());
        assert_eq!(fs::read(root.join("config.json")).unwrap(), config);
    }

    fn write_zip(root: &Path, stamp: u64, entries: &[(&str, &str)]) {
        let manifest = encode(&Manifest {
            schema: SCHEMA.into(),
            version: 1,
            created_at_ms: stamp,
            app_version: String::new(),
            entries: entries.iter().map(|(name, _)| (*name).into()).collect(),
        })
        .unwrap();
        let archive = root.join(format!("backups/auto/fc-backup-{stamp}.zip"));
        fs::create_dir_all(archive.parent().unwrap()).unwrap();
        let mut zip = ZipWriter::new(File::create(archive).unwrap());
        for (name, body) in entries
            .iter()
            .copied()
            .chain([(MANIFEST, std::str::from_utf8(&manifest).unwrap())])
        {
            zip.start_file(name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
    }

    fn backup_names(root: &Path) -> Vec<String> {
        list_backups(root)
            .into_iter()
            .map(|entry| entry.file_name)
            .collect()
    }

    const KEEP_ONE: BackupSettings = BackupSettings {
        enabled: true,
        keep: 1,
    };

    /// keep=1 で復元元を選び、適用に失敗させる(ロールバックは成功する)。
    fn fail_restore_with_keep_one(root: &Path) -> PathBuf {
        write(root, "config.json", r#"{"backup":{"keep":1}}"#);
        write(root, "listeners.json", "{}");
        let source = create_backup(root, 1).unwrap().unwrap();
        write(root, "listeners.json", "{ }");
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        write(
            &root.join("backups/restore-pending"),
            "listeners.json",
            r#"{"viewer":1}"#,
        );
        assert!(apply_pending_restore(root, 3).is_err());
        assert!(source.is_file());
        source
    }

    #[test]
    fn startup_after_failed_restore_neither_creates_nor_prunes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = fail_restore_with_keep_one(root);
        let before = backup_names(root);
        let later = 3 + 21 * 60 * 60 * 1000;
        for trigger in [BackupTrigger::Startup, BackupTrigger::Interval] {
            write(root, "skin/new.css", "changed");
            let flushed = std::cell::Cell::new(false);
            let created = run_backup(root, KEEP_ONE, trigger, true, later, || {
                flushed.set(true);
                Ok(())
            });
            assert!(!created.unwrap());
            assert!(!flushed.get());
            assert!(source.is_file());
            assert_eq!(backup_names(root), before);
        }
    }

    #[test]
    fn manual_backup_after_failed_restore_does_not_prune() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = fail_restore_with_keep_one(root);
        write(root, "skin/new.css", "changed");
        assert!(run_backup(root, KEEP_ONE, BackupTrigger::Manual, true, 10, || Ok(())).unwrap());
        assert!(source.is_file());
        assert_eq!(backup_names(root).len(), 3);
        // 失敗の無い起動でも、次に復元が成功するまでは復元元を残す。
        write(root, "skin/new.css", "changed again");
        assert!(run_backup(root, KEEP_ONE, BackupTrigger::Manual, false, 11, || Ok(())).unwrap());
        assert!(source.is_file());
        assert_eq!(backup_names(root), ["fc-backup-11.zip", "fc-backup-1.zip"]);
    }

    #[test]
    fn restoring_another_generation_after_a_failure_keeps_the_first_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let source = fail_restore_with_keep_one(root);
        assert_eq!(backup_names(root), ["fc-backup-2.zip", "fc-backup-1.zip"]);
        // 復元の準備から適用までは、手動・自動の作成でも prune しない。
        stage_restore(root, "fc-backup-2.zip", 4).unwrap();
        assert!(source.is_file());
        write(root, "skin/new.css", "changed");
        assert!(run_backup(root, KEEP_ONE, BackupTrigger::Manual, false, 5, || Ok(())).unwrap());
        assert_eq!(prune(root, 1).unwrap(), 0);
        assert!(source.is_file());
        assert_eq!(backup_names(root).len(), 3);

        // 復元が成功した起動で、失敗した世代の記録は通常の扱いに戻る。
        assert!(apply_pending_restore(root, 6).unwrap());
        let failed = root.join("backups/restore-failed-3");
        assert!(failed.is_dir());
        assert!(!failed.join(SOURCE).exists());
        assert_eq!(prune(root, 1).unwrap(), 2);
        assert!(!source.exists());
    }

    #[test]
    fn failed_generations_with_a_source_are_not_swept() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for stamp in 1..=5 {
            write(root, &format!("backups/restore-failed-{stamp}/READY"), "");
        }
        write(root, "backups/restore-failed-1/SOURCE", "fc-backup-1.zip");
        assert!(!apply_pending_restore(root, 10).unwrap());
        assert!(root.join("backups/restore-failed-1/SOURCE").is_file());
        assert!(!root.join("backups/restore-failed-2").exists());
        for stamp in 3..=5 {
            assert!(root.join(format!("backups/restore-failed-{stamp}")).is_dir());
        }
    }

    #[test]
    fn case_variants_of_store_names_are_invalid() {
        for name in [
            "sounds/SOUNDS.JSON",
            "sounds/Sounds.json",
            "sounds/sounds.JSON",
            "sounds/\u{17f}ounds.json",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            let body = r#"{"sounds":false}"#;
            write_zip(root, 1, &[(name, body)]);
            assert!(
                matches!(
                    stage_restore(root, "fc-backup-1.zip", 2),
                    Err(BackupError::Invalid(_))
                ),
                "{name}"
            );
            let pending = root.join("backups/restore-pending");
            assert!(!pending.exists(), "{name}");
            let manifest = encode(&Manifest {
                schema: SCHEMA.into(),
                version: 1,
                created_at_ms: 1,
                app_version: String::new(),
                entries: vec![name.into()],
            })
            .unwrap();
            write(&pending, name, body);
            write(&pending, MANIFEST, std::str::from_utf8(&manifest).unwrap());
            write(&pending, READY, "");
            assert!(
                matches!(apply_pending_restore(root, 3), Err(BackupError::Invalid(_))),
                "{name}"
            );
            assert!(!root.join("sounds").exists(), "{name}");
        }
    }

    #[test]
    fn create_rejects_unreadable_stores_without_pruning() {
        for (name, body, reason) in [
            ("sounds/sounds.json", r#"{"sounds":false}"#, "効果音"),
            ("sounds.json", r#"{"sounds":false}"#, "効果音"),
            ("listeners.json", r#"{"viewer":1}"#, "リスナー"),
            ("achievements.json", r#"{"version":1,"achievements":false}"#, "実績"),
            (
                "achievement-progress.json",
                r#"{"version":1,"unlocks":false}"#,
                "実績",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write(root, "config.json", r#"{"backup":{"keep":1}}"#);
            create_backup(root, 1).unwrap().unwrap();
            write(root, name, body);
            for trigger in [BackupTrigger::Manual, BackupTrigger::Interval] {
                let error = run_backup(root, KEEP_ONE, trigger, false, 2, || Ok(())).unwrap_err();
                assert!(matches!(error, BackupError::Invalid(_)), "{name}");
                assert!(error.to_string().contains(reason), "{name}: {error}");
                assert_eq!(backup_names(root), ["fc-backup-1.zip"], "{name}");
            }
        }
    }

    #[test]
    fn restore_keeps_current_oauth_callback_ports() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "config.json",
            r#"{"xPost":{"callbackPort":41002,"dailyLimit":9},"twitcasting":{"callbackPort":41003}}"#,
        );
        create_backup(root, 1).unwrap();
        write(
            root,
            "config.json",
            r#"{"xPost":{"callbackPort":41001},"twitcasting":{"callbackPort":41004}}"#,
        );
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(apply_pending_restore(root, 3).unwrap());
        let config = AppConfig::load(root).unwrap();
        assert_eq!(config.x_post.callback_port, 41001);
        assert_eq!(config.twitcasting.callback_port, 41004);
        assert_eq!(config.x_post.daily_limit, 9);
    }

    #[test]
    fn backup_strips_url_user_info_and_restore_keeps_current_urls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            root,
            "config.json",
            r#"{
            "tts":{"options":{"voicevoxUrl":"https://reader:example-pass@tts.example.invalid/v"}},
            "xOverrides":{"endpoints":{"liveChatUrl":"https://u:example-pass@x.example.invalid/c"}},
            "niconicoOverrides":{"endpoints":{"watchPageBaseUrl":"http://u@nico.example.invalid/w/"}},
            "twitcastingOverrides":{"endpoints":{"userPageBaseUrl":"https://u:p%40ss@tc.example.invalid/?a=b@c"}}
        }"#,
        );
        let path = create_backup(root, 1).unwrap().unwrap();
        let text = String::from_utf8(read_archive(&path).unwrap().files["config.json"].clone()).unwrap();
        for leaked in ["example-pass", "reader", "u@", "p%40ss"] {
            assert!(!text.contains(leaked), "{leaked}: {text}");
        }
        for kept in [
            "https://tts.example.invalid/v",
            "https://x.example.invalid/c",
            "http://nico.example.invalid/w/",
            "https://tc.example.invalid/?a=b@c",
        ] {
            assert!(text.contains(kept), "{kept}: {text}");
        }
        for (value, expected) in [
            // 空の authority の後ろもユーザー情報として読まれる(reqwest と同じ解釈)。
            ("https:///u:p@h", Some("https://h/")),
            // 解析できない値は、`@` があれば値ごと空にする。
            ("https://u:pa/ss@h", Some("")),
            ("user:pass@host:50021", Some("")),
            ("http://127.0.0.1:50021", None),
        ] {
            assert_eq!(strip_user_info(value).as_deref(), expected, "{value}");
        }

        write(
            root,
            "config.json",
            r#"{
            "tts":{"options":{"voicevoxUrl":"http://127.0.0.1:50099"}},
            "niconicoOverrides":{"endpoints":{"watchPageBaseUrl":"https://current.invalid/w/"}}
        }"#,
        );
        stage_restore(root, "fc-backup-1.zip", 2).unwrap();
        assert!(apply_pending_restore(root, 3).unwrap());
        let config = AppConfig::load(root).unwrap();
        assert_eq!(config.tts.options.voicevox_url, "http://127.0.0.1:50099");
        assert!(config.x_overrides.endpoints.is_empty());
        assert_eq!(
            config.niconico_overrides.endpoints,
            [("watchPageBaseUrl".into(), "https://current.invalid/w/".into())].into()
        );
        assert!(config.twitcasting_overrides.endpoints.is_empty());
    }

    /// 合成値だけで作った設定。伏せる欄の値はすべて `syn-<tag>-` を含む。
    fn synthetic_config(tag: &str) -> AppConfig {
        let secret = |name: &str| format!("syn-{tag}-{name}");
        let url = |host: &str| format!("https://syn-{tag}-user:syn-{tag}-pass@{host}/path");
        let mut config = AppConfig {
            // 全フィールドを列挙する(欄を足した時にコンパイルエラーで気付く)。
            credentials: CredentialsConfig {
                twitch_oauth: format!("oauth:{}", secret("twitch-oauth")),
                twitch_username: secret("twitch-username"),
                twitch_client_id: secret("twitch-client-id"),
                twitch_client_secret: secret("twitch-client-secret"),
                youtube_api_key: secret("youtube-api-key"),
                youtube_oauth_client_id: secret("youtube-oauth-client-id"),
                x_auth_token: secret("x-auth-token"),
                x_csrf_token: secret("x-csrf-token"),
                x_post_client_id: Some(secret("x-post-client-id")),
                twitcasting_client_id: Some(secret("twitcasting-client-id")),
            },
            ..AppConfig::default()
        };
        config.x_overrides.bearer_token = Some(secret("bearer-token"));
        config.youtube_overrides.api_key = Some(secret("innertube-key"));
        // 読み上げの送り先はこの PC(取り込みでファイルの値を使う条件)。
        config.tts.options.voicevox_url =
            format!("http://syn-{tag}-user:syn-{tag}-pass@127.0.0.1:50021/{tag}");
        config.tts.options.bouyomi_path = format!("C:\\Users\\syn-{tag}-user\\BouyomiChan.exe");
        config
            .x_overrides
            .endpoints
            .insert("liveChatUrl".into(), url("x.example.invalid"));
        config
            .niconico_overrides
            .endpoints
            .insert("watchPageBaseUrl".into(), url("nico.example.invalid"));
        config
            .twitcasting_overrides
            .endpoints
            .insert("eventPubSubUrl".into(), url("tc.example.invalid"));
        config
    }

    /// `file` を読み込んだ時に期待する設定(SPEC §8.1 の規則を型の上で書いたもの)。
    fn expected_import(mut file: AppConfig, current: &AppConfig) -> AppConfig {
        file.credentials = current.credentials.clone();
        file.x_overrides.bearer_token = current.x_overrides.bearer_token.clone();
        file.youtube_overrides.api_key = current.youtube_overrides.api_key.clone();
        file.x_overrides.endpoints = current.x_overrides.endpoints.clone();
        file.x_post.callback_port = current.x_post.callback_port;
        file.twitcasting.callback_port = current.twitcasting.callback_port;
        file.tts.options.bouyomi_path = current.tts.options.bouyomi_path.clone();
        file.tts.options.bouyomi_launch_elevated = current.tts.options.bouyomi_launch_elevated;
        file.x_post.enabled = false;
        if !loopback_host(file.tts.options.bouyomi_host.trim()) {
            file.tts.options.bouyomi_host = current.tts.options.bouyomi_host.clone();
        }
        if !loopback_url(&file.tts.options.voicevox_url) {
            file.tts.options.voicevox_url = current.tts.options.voicevox_url.clone();
            return file_urls_stripped(file, false);
        }
        file_urls_stripped(file, true)
    }

    fn file_urls_stripped(mut file: AppConfig, voicevox: bool) -> AppConfig {
        let voicevox = voicevox.then_some(&mut file.tts.options.voicevox_url);
        let urls = voicevox
            .into_iter()
            .chain(file.niconico_overrides.endpoints.values_mut())
            .chain(file.twitcasting_overrides.endpoints.values_mut());
        for url in urls {
            if let Some(stripped) = strip_user_info(url) {
                *url = stripped;
            }
        }
        file
    }

    #[test]
    fn settings_export_contains_no_secret_value() {
        let mut config = synthetic_config("exp");
        config.obs.port = 11190;
        let exported = export_config(&config).unwrap();
        let text = exported.to_string();
        assert!(!text.contains("syn-exp-"), "{text}");
        assert!(exported.get("credentials").is_none());
        assert!(exported["xOverrides"].get("bearerToken").is_none());
        assert!(exported["youtubeOverrides"].get("apiKey").is_none());
        // URL はユーザー情報だけを消し、接続先は残す。
        for (path, url) in [
            (&["tts", "options", "voicevoxUrl"][..], "http://127.0.0.1:50021/exp"),
            (&["xOverrides", "endpoints", "liveChatUrl"], "https://x.example.invalid/path"),
            (&["niconicoOverrides", "endpoints", "watchPageBaseUrl"], "https://nico.example.invalid/path"),
            (&["twitcastingOverrides", "endpoints", "eventPubSubUrl"], "https://tc.example.invalid/path"),
        ] {
            let object = exported.as_object().unwrap();
            assert_eq!(config_field(object, path), Some(&Value::from(url)), "{path:?}");
        }
        // 資格情報ではない設定はそのまま書き出す。
        assert_eq!(exported["obs"]["port"], 11190);
        // 起動パスは Windows のユーザー名を含みうるので書き出さない。取り込みは今の値を残す。
        assert!(exported["tts"]["options"].get("bouyomiPath").is_none());
        assert_eq!(exported["tts"]["options"]["bouyomiHost"], "127.0.0.1");

        // 既定の設定には伏せる値が無く、欄だけが消える。
        let exported = export_config(&AppConfig::default()).unwrap();
        assert!(exported.get("credentials").is_none());

        // 資格情報と同じ値が他の欄にあれば、書き出さない。
        config.tts.options.name_format = config.credentials.x_auth_token.clone();
        assert_eq!(export_config(&config), Err(EXPORT_SECRET_ERROR));
        // 資格情報の前後に空白があっても、空白なしの写しを見つける。
        config.credentials.x_auth_token = format!(" {}\n", config.tts.options.name_format);
        assert_eq!(export_config(&config), Err(EXPORT_SECRET_ERROR));
    }

    #[test]
    fn settings_import_keeps_current_secrets_and_x_endpoints() {
        let current = synthetic_config("cur");
        let mut other = synthetic_config("new");
        other.obs.port = 11190;
        other.x_post.callback_port = 47999;
        other.twitcasting.callback_port = 39999;
        let expected = expected_import(other.clone(), &current);
        for file in [
            // 今の版の書き出し(資格情報の欄が無い)。
            export_config(&other).unwrap(),
            // 旧版の書き出し(資格情報、鍵、URL のユーザー情報を含む)。
            serde_json::to_value(&other).unwrap(),
        ] {
            let merged =
                merge_import(&AppConfig::from_config_update(file).unwrap(), &current).unwrap();
            assert_eq!(merged, expected);
            // 資格情報、鍵、待受ポート、X の接続先は今の値のまま。
            assert_eq!(merged.credentials, current.credentials);
            assert_eq!(merged.x_overrides, current.x_overrides);
            assert_eq!(merged.youtube_overrides.api_key, current.youtube_overrides.api_key);
            assert_eq!(merged.x_post.callback_port, current.x_post.callback_port);
            assert_eq!(merged.twitcasting.callback_port, current.twitcasting.callback_port);
            // それ以外の URL はファイルの接続先を使い、ユーザー情報は持ち込まない。
            assert_eq!(merged.tts.options.voicevox_url, "http://127.0.0.1:50021/new");
            assert_eq!(
                merged.niconico_overrides.endpoints["watchPageBaseUrl"],
                "https://nico.example.invalid/path"
            );
            assert_eq!(
                merged.twitcasting_overrides.endpoints["eventPubSubUrl"],
                "https://tc.example.invalid/path"
            );
            assert_eq!(merged.obs.port, 11190);
            let text = serde_json::to_string(&merged).unwrap();
            assert!(!text.contains("syn-new-"), "{text}");
            // 保存の直前にもう一度通しても変わらない。
            assert_eq!(merge_import(&merged, &current).unwrap(), merged);
        }

        // 欄が無い・空のファイルでも、今の資格情報を空で上書きしない。
        for file in [
            serde_json::json!({}),
            serde_json::json!({
                "credentials": {"xAuthToken": "", "xCsrfToken": ""},
                "xOverrides": {},
            }),
            // 配列で書かれた欄も、型変換の後で今の値に差し替わる。
            serde_json::json!({
                "credentials": ["oauth:syn-new-positional-oauth"],
                "xOverrides": ["syn-new-positional-bearer", {"liveChatUrl": "https://new.example.invalid/c"}],
            }),
        ] {
            let merged =
                merge_import(&AppConfig::from_config_update(file).unwrap(), &current).unwrap();
            assert_eq!(merged, expected_import(AppConfig::default(), &current));
            assert_eq!(merged.credentials, current.credentials);
            assert_eq!(merged.x_overrides, current.x_overrides);
        }
    }

    #[test]
    fn settings_import_drops_file_secrets_when_current_has_none() {
        let current = AppConfig::default();
        let file = synthetic_config("new");
        let merged = merge_import(&file, &current).unwrap();
        // 今の設定に無い鍵と X の接続先は、ファイルにあっても取り込まない。
        assert_eq!(merged.credentials, CredentialsConfig::default());
        assert_eq!(merged.x_overrides.bearer_token, None);
        assert!(merged.x_overrides.endpoints.is_empty());
        assert_eq!(merged.youtube_overrides.api_key, None);
        // X 以外の接続先はファイルの値(ユーザー情報なし)。
        assert_eq!(merged.tts.options.voicevox_url, "http://127.0.0.1:50021/new");
        assert_eq!(merged.niconico_overrides.endpoints.len(), 1);
        assert_eq!(merged.twitcasting_overrides.endpoints.len(), 1);
        assert!(!serde_json::to_string(&merged).unwrap().contains("syn-new-"));
        assert_eq!(merged, expected_import(file, &current));
    }

    #[test]
    fn settings_import_never_enables_x_autopost() {
        // 接続済みの環境(今の資格情報)に、API モードの自動告知を有効にした他の配信者のファイル。
        let mut current = synthetic_config("cur");
        let file = serde_json::json!({
            "channels": [{"platform": "twitch", "identifier": "other_streamer", "ownStream": true}],
            "xPost": {"enabled": true, "mode": "api", "intentOpen": "auto", "callbackPort": 47999},
        });
        let file = AppConfig::from_config_update(file).unwrap();
        assert!(file.x_post.enabled && file.channels[0].own_stream);
        for current_enabled in [false, true] {
            current.x_post.enabled = current_enabled;
            let merged = merge_import(&file, &current).unwrap();
            assert!(!merged.x_post.enabled);
            let notices = import_notices(&file, &current, &merged);
            assert_eq!(notices, [X_POST_DISABLED_BY_IMPORT]);
            assert!(notices[0].contains("ownStream"));
            // 無効にするのは告知だけ。他はファイルの値を読む。
            assert_eq!(merged.x_post.mode, "api");
            assert!(merged.channels[0].own_stream);
            assert_eq!(merged.credentials, current.credentials);
            assert_eq!(merged.x_post.callback_port, current.x_post.callback_port);
        }
    }

    #[test]
    fn settings_import_takes_tts_destination_only_from_this_pc() {
        let mut current = AppConfig::default();
        current.tts.options.voicevox_url = "http://192.0.2.5:50021".into();
        current.tts.options.bouyomi_host = "192.0.2.6".into();
        let import = |voicevox: &str, bouyomi: &str| {
            let file = AppConfig::from_config_update(serde_json::json!({
                "tts": {"options": {
                    "voicevoxUrl": voicevox,
                    "bouyomiHost": bouyomi,
                    "bouyomiPort": 50002,
                }},
            }))
            .unwrap();
            let merged = merge_import(&file, &current).unwrap();
            // ポートはファイルの値を使う。
            assert_eq!(merged.tts.options.bouyomi_port, 50002);
            assert_eq!(merge_import(&merged, &current).unwrap(), merged);
            let notices = import_notices(&file, &current, &merged);
            (merged.tts.options, notices)
        };
        // この PC を指す値はファイルから取り込む(AivisSpeech の 10101 など)。
        for (voicevox, bouyomi) in [
            ("http://127.0.0.1:10101", "127.0.0.1"),
            ("http://localhost:10101", "localhost"),
            ("http://LOCALHOST:10101/", "LocalHost"),
            ("http://127.8.9.10:50021/", "127.8.9.10"),
            ("http://[::1]:50021/", "::1"),
        ] {
            let (options, notices) = import(voicevox, bouyomi);
            assert_eq!(options.voicevox_url, voicevox);
            assert_eq!(options.bouyomi_host, bouyomi);
            assert!(notices.is_empty(), "{voicevox} {bouyomi}");
        }
        // この PC 以外を指す値・解析できない値は取り込まず、今の値を残して知らせる。
        for (voicevox, bouyomi) in [
            ("http://198.51.100.7:50021", "198.51.100.7"),
            ("https://tts.example.invalid/", "tts.example.invalid"),
            ("http://127.0.0.1@tts.example.invalid/", "127.0.0.1.example.invalid"),
            ("http://localhost.example.invalid:50021/", "localhost.example.invalid"),
            ("127.0.0.1:50021", "http://127.0.0.1"),
            ("", ""),
        ] {
            for (voicevox, bouyomi) in [(voicevox, "127.0.0.1"), ("http://127.0.0.1:50021", bouyomi)] {
                let (options, notices) = import(voicevox, bouyomi);
                let kept = (
                    options.voicevox_url == current.tts.options.voicevox_url,
                    options.bouyomi_host == current.tts.options.bouyomi_host,
                );
                assert_eq!(kept, (bouyomi == "127.0.0.1", voicevox == "http://127.0.0.1:50021"));
                assert_eq!(notices, [TTS_DESTINATION_KEPT], "{voicevox} {bouyomi}");
            }
        }
        // 今の値と同じなら、残したことを知らせない(同じ PC での書き出しと読み込み)。
        let (options, notices) = import("http://192.0.2.5:50021", "192.0.2.6");
        assert_eq!(options.voicevox_url, "http://192.0.2.5:50021");
        assert_eq!(options.bouyomi_host, "192.0.2.6");
        assert!(notices.is_empty());
    }

    #[test]
    fn settings_import_rejects_a_config_that_is_not_an_object() {
        for value in [
            serde_json::json!([]),
            serde_json::json!([{"enabled": false}, {"enabled": true}]),
            serde_json::json!("syn-new-root"),
            serde_json::json!(null),
        ] {
            assert_eq!(
                parse_import(value),
                Err("設定の形式が不正です (項目 .)".to_string())
            );
        }
        assert_eq!(parse_import(serde_json::json!({})), Ok(AppConfig::default()));
    }

    #[test]
    fn settings_import_keeps_current_voicevox_user_info_through_both_merges() {
        let mut current = AppConfig::default();
        current.tts.options.voicevox_url = "http://syn-user:syn-pass@127.0.0.1:50021".into();
        // ファイルの接続先が外部(今の値を残す)、または今と同じ接続先(ユーザー情報なし)。
        for file_url in ["http://192.0.2.10:50021", "http://127.0.0.1:50021"] {
            let file = AppConfig::from_config_update(serde_json::json!({
                "tts": {"options": {"voicevoxUrl": file_url}},
            }))
            .unwrap();
            // 保存処理と同じく 2 回マージする(検証の前と、保存直前のロックの中)。
            let first = merge_import(&file, &current).unwrap();
            let second = merge_import(&first, &current).unwrap();
            assert_eq!(second.tts.options.voicevox_url, current.tts.options.voicevox_url, "{file_url}");
        }
        // 別のループバックのポートならファイルの値を使う(AivisSpeech を移すため)。
        let file = AppConfig::from_config_update(serde_json::json!({
            "tts": {"options": {"voicevoxUrl": "http://127.0.0.1:10101"}},
        }))
        .unwrap();
        let first = merge_import(&file, &current).unwrap();
        let second = merge_import(&first, &current).unwrap();
        assert_eq!(second.tts.options.voicevox_url, "http://127.0.0.1:10101");
    }

    #[test]
    fn settings_import_keeps_current_bouyomi_launch() {
        // 棒読みちゃんを使う設定に、別の exe を昇格して起動させるファイル。
        let file = serde_json::json!({
            "tts": {"backend": "bouyomi", "options": {
                "bouyomiPath": "C:\\syn-new\\other.exe",
                "bouyomiLaunchElevated": true,
                "bouyomiPort": 50002,
            }},
        });
        let file = AppConfig::from_config_update(file).unwrap();
        assert!(file.tts.options.bouyomi_launch_elevated);
        assert_eq!(file.tts.backend, crate::config::TtsBackendKind::Bouyomi);
        let mut current = AppConfig::default();
        for (path, elevated) in [("", false), ("C:\\syn-cur\\BouyomiChan.exe", false)] {
            current.tts.options.bouyomi_path = path.into();
            current.tts.options.bouyomi_launch_elevated = elevated;
            let merged = merge_import(&file, &current).unwrap();
            assert_eq!(merged.tts.options.bouyomi_path, path);
            assert_eq!(merged.tts.options.bouyomi_launch_elevated, elevated);
            // 取り込みでは起動しないので、棒読みちゃんに切り替わった時はそれを知らせる。
            assert_eq!(
                import_notices(&file, &current, &merged),
                [BOUYOMI_NOT_LAUNCHED_BY_IMPORT]
            );
            let mut already = current.clone();
            already.tts.backend = crate::config::TtsBackendKind::Bouyomi;
            assert!(import_notices(&file, &already, &merged).is_empty());
            // 起動に関わらない読み上げ設定はファイルの値を読む。
            assert_eq!(merged.tts.backend, crate::config::TtsBackendKind::Bouyomi);
            assert_eq!(merged.tts.options.bouyomi_port, 50002);
            assert_eq!(merged, expected_import(file.clone(), &current));
        }
    }

    #[test]
    fn settings_import_reads_an_older_export() {
        let current = synthetic_config("cur");
        // 旧版の形: 新しい節(backup・soundEffects・twitcasting 等)が無く、資格情報が入っている。
        let file = serde_json::json!({
            "channels": [{"platform": "twitch", "identifier": "old_channel"}],
            "obs": {"port": 11190},
            "tts": {"options": {
                "bouyomiPort": 50002,
                "voicevoxUrl": "http://syn-old-user:syn-old-pass@127.0.0.1:10101",
            }},
            "xOverrides": {
                "bearerToken": "syn-old-bearer-token",
                "livePollSecs": 45,
                "endpoints": {"liveChatUrl": "https://syn-old-user:syn-old-pass@x.example.invalid/c"},
            },
            "youtubeOverrides": {"apiKey": "syn-old-innertube-key", "clientVersion": "2.1"},
            "credentials": {
                "twitchOauth": "oauth:syn-old-twitch-oauth",
                "twitchUsername": "syn-old-twitch-username",
                "youtubeApiKey": "syn-old-youtube-api-key",
                "xAuthToken": "syn-old-x-auth-token",
                "xCsrfToken": "syn-old-x-csrf-token",
            },
        });
        let file = AppConfig::from_config_update(file).unwrap();
        let merged = merge_import(&file, &current).unwrap();
        assert!(!serde_json::to_string(&merged).unwrap().contains("syn-old-"));
        // 資格情報ではない設定はファイルの値になる。
        assert_eq!(merged.channels.len(), 1);
        assert_eq!(merged.channels[0].identifier, "old_channel");
        assert_eq!(merged.obs.port, 11190);
        assert_eq!(merged.tts.options.bouyomi_port, 50002);
        assert_eq!(merged.tts.options.voicevox_url, "http://127.0.0.1:10101/");
        assert_eq!(merged.x_overrides.live_poll_secs, Some(45));
        assert_eq!(merged.youtube_overrides.client_version.as_deref(), Some("2.1"));
        assert_eq!(merged, expected_import(file, &current));
    }

    #[cfg(unix)]
    #[test]
    fn startup_cleanup_continues_after_a_failed_removal() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "backups/auto/locked.tmp", "");
        write(root, "backups/restore-applied-id.tmp/READY", "");
        for stamp in 1..=4 {
            write(root, &format!("backups/restore-failed-{stamp}/READY"), "");
        }
        let auto = root.join("backups/auto");
        fs::set_permissions(&auto, fs::Permissions::from_mode(0o555)).unwrap();
        let result = apply_pending_restore(root, 10);
        let locked = auto.join("locked.tmp").exists();
        fs::set_permissions(&auto, fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!result.unwrap());
        // A privileged runner can remove the file anyway.
        if locked {
            assert!(!root.join("backups/restore-applied-id.tmp").exists());
            assert!(!root.join("backups/restore-failed-1").exists());
            assert!(root.join("backups/restore-failed-2").is_dir());
        }
    }
}
