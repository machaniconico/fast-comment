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
const LOCAL_CONFIG_FIELDS: &[&[&str]] = &[
    &["credentials"],
    &["xOverrides", "endpoints"],
    &["tts", "options", "bouyomiPath"],
    &["tts", "options", "bouyomiLaunchElevated"],
];
const SCHEMA: &str = "fast-comment-backup";
const MANIFEST: &str = "manifest.json";
const READY: &str = "READY";
const APPLYING: &str = "APPLYING";
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

    fn validate_stores(&self) -> Result<()> {
        use crate::achievements::store::{validate_backup_defs, validate_backup_progress};

        for name in FILES.iter().copied().chain(["sounds/sounds.json"]) {
            let Some(bytes) = self.files.get(name) else {
                continue;
            };
            if bytes.iter().find(|byte| !byte.is_ascii_whitespace()) != Some(&b'{') {
                return Err(BackupError::Invalid("JSON の形式"));
            }
            let valid = match name {
                "config.json" => config_object(bytes).is_ok(),
                "listeners.json" => {
                    serde_json::from_slice::<HashMap<String, ListenerRecord>>(bytes).is_ok()
                }
                "achievements.json" => validate_backup_defs(bytes),
                "achievement-progress.json" => validate_backup_progress(bytes),
                "sounds.json" | "sounds/sounds.json" => crate::sounds::validate_backup_index(bytes),
                _ => unreachable!("backup store"),
            };
            if !valid {
                return Err(BackupError::Invalid("ストアの JSON 形式"));
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

fn excluded(part: &str) -> bool {
    let part = part.to_ascii_lowercase();
    part.ends_with(".tmp")
        || [".broken-", ".corrupt-", ".bak."]
            .iter()
            .any(|fragment| part.contains(fragment))
        || matches!(
            part.as_str(),
            "integrations.json" | "x-autopost-ledger.json" | "logs" | "exports" | "backups"
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

pub fn prune(config_dir: &Path, keep: u32) -> Result<usize> {
    prune_except(config_dir, keep, None)
}

fn prune_except(config_dir: &Path, keep: u32, source: Option<&str>) -> Result<usize> {
    let path = auto_dir(config_dir)?;
    let mut removed = 0;
    let entries = read_backups(config_dir)?;
    for entry in entries.iter().skip(keep.clamp(1, 50) as usize).rev() {
        if Some(entry.file_name.as_str()) == source {
            continue;
        }
        fs::remove_file(path.join(&entry.file_name))?;
        removed += 1;
    }
    Ok(removed)
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
    let Some(value) = value.and_then(Value::as_str) else {
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
        for field in CREDENTIAL_SECRETS {
            collect_secret(config_field(&config, &["credentials", field]), &mut secrets);
        }
        for path in OVERRIDE_SECRETS {
            collect_secret(config_field(&config, path), &mut secrets);
            set_config_field(&mut config, path, None);
        }
        config.insert("credentials".into(), empty_credentials());
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
    for name in FILES.iter().copied().chain(["sounds/sounds.json"]) {
        if let Some(bytes) = contents.files.get(name) {
            json_object(bytes)?;
        }
    }
    if !secrets.is_empty() {
        let matches = regex::bytes::RegexSet::new(secrets)
            .map_err(|_| BackupError::Invalid("資格情報の検査"))?;
        if contents
            .files
            .iter()
            .any(|(name, bytes)| matches.is_match(name.as_bytes()) || matches.is_match(bytes))
        {
            return Err(BackupError::Invalid(
                "バックアップ対象に資格情報が含まれています",
            ));
        }
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
    let contents = snapshot(config_dir, stamp)?;
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

    let settings = match read_config(config_dir)? {
        Some(config) => config
            .get("backup")
            .cloned()
            .and_then(|value| serde_json::from_value::<BackupSettings>(value).ok())
            .unwrap_or_default(),
        None => BackupSettings::default(),
    };
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
        create_backup(config_dir, now_ms)?;
        prune_except(config_dir, settings.keep, Some(backup_file_name))?;
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
        if name == READY {
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
        for path in LOCAL_CONFIG_FIELDS.iter().chain(OVERRIDE_SECRETS) {
            if config_field(&current, path) == config_field(defaults, path) {
                set_config_field(&mut current, path, None);
            }
        }
    }
    for path in LOCAL_CONFIG_FIELDS.iter().chain(OVERRIDE_SECRETS) {
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
    for (path, directory) in generations
        .into_iter()
        .rev()
        .skip(3)
        .flat_map(|(_, files)| files)
    {
        if directory {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
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

fn cleanup_backup_work_files(config_dir: &Path, dir: &Path) -> Result<()> {
    let auto = auto_dir(config_dir)?;
    if directory_exists(&auto)? {
        for entry in fs::read_dir(auto)? {
            let entry = entry?;
            if entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "tmp")
                && check_kind(&fs::symlink_metadata(entry.path())?, false).is_ok()
            {
                fs::remove_file(entry.path())?;
            }
        }
    }
    if !directory_exists(dir)? {
        return Ok(());
    }
    let mut failed = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let path = entry.path();
        if check_kind(&fs::symlink_metadata(&path)?, true).is_err() {
            continue;
        }
        if name.starts_with("restore-applied-") && name.ends_with(".tmp") {
            fs::remove_dir_all(path)?;
        } else if let Some(stamp) = failed_timestamp(name) {
            failed.push((stamp, path));
        }
    }
    failed.sort_unstable();
    for (_, path) in failed.into_iter().rev().skip(3) {
        fs::remove_dir_all(path)?;
    }
    Ok(())
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
        if let Err(error) = cleanup_backup_work_files(config_dir, &dir) {
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
}
