use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::currency::RateTable;
use super::engine::Progress;
use super::model::{
    presets, reset_to_preset, validate_all, validate_for_save, Achievement, ValidationError,
};

pub const DEFS_FILE: &str = "achievements.json";
pub const PROGRESS_FILE: &str = "achievement-progress.json";
pub const STORE_VERSION: u32 = 1;

pub trait StoreIo: Send + Sync {
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()>;
    fn create_backup(&self, src: &Path, dst: &Path) -> io::Result<()>;
}

pub struct FsIo;

impl StoreIo for FsIo {
    fn write_atomic(&self, path: &Path, bytes: &[u8]) -> io::Result<()> {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        let tmp = path.with_file_name(format!("{name}.{}.tmp", Uuid::new_v4()));
        write_new(&tmp, bytes)?;
        let result = fs::rename(&tmp, path);
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result
    }

    fn create_backup(&self, src: &Path, dst: &Path) -> io::Result<()> {
        write_new(dst, &fs::read(src)?)
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = file.write_all(bytes).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AchievementDefs {
    pub version: u32,
    pub revision: u64,
    pub enabled: bool,
    pub rates: RateTable,
    pub rates_updated_at_ms: Option<u64>,
    pub symbol_overrides: BTreeMap<String, String>,
    pub achievements: Vec<Achievement>,
}

impl Default for AchievementDefs {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            revision: 0,
            enabled: false,
            rates: BTreeMap::from([("JPY".to_owned(), 1.0)]),
            rates_updated_at_ms: None,
            symbol_overrides: BTreeMap::new(),
            achievements: presets(),
        }
    }
}

#[derive(PartialEq, Eq)]
pub enum CommitError {
    Stale,
    Invalid(String, ValidationError),
    Io,
    SaveRefused,
}

impl fmt::Debug for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => f.write_str("Stale"),
            Self::Invalid(_, reason) => f.debug_tuple("Invalid").field(reason).finish(),
            Self::Io => f.write_str("Io"),
            Self::SaveRefused => f.write_str("SaveRefused"),
        }
    }
}

impl fmt::Display for CommitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale => f.write_str("achievement definitions have changed"),
            Self::Invalid(_, reason) => reason.fmt(f),
            Self::Io => f.write_str("could not save achievement data"),
            Self::SaveRefused => f.write_str("achievement data could not be preserved"),
        }
    }
}

impl std::error::Error for CommitError {}

// Serialize definition transactions across handles in this process.
static DEFS_TRANSACTION: Mutex<()> = Mutex::new(());

struct DefsState {
    current: AchievementDefs,
    saved_bytes: Option<Vec<u8>>,
    /// 再読込で退避済みの壊れた内容。取り込み(flush)に失敗しても同じ内容は二度退避しない。
    backed_up: Option<Vec<u8>>,
}

/// 読み込んだファイル内容の識別子。revision を据え置いた外部編集も見分ける。
fn content_token(bytes: &Option<Vec<u8>>) -> String {
    bytes
        .as_ref()
        .map(|b| hex::encode(Sha256::digest(b)))
        .unwrap_or_default()
}

pub struct DefsStore {
    path: PathBuf,
    io: Arc<dyn StoreIo>,
    save_refused: bool,
    state: Mutex<DefsState>,
}

impl DefsStore {
    pub fn load(dir: &Path, now_ms: u64) -> Self {
        Self::load_with(dir, now_ms, Arc::new(FsIo))
    }

    pub fn load_with(dir: &Path, now_ms: u64, io: Arc<dyn StoreIo>) -> Self {
        let path = dir.join(DEFS_FILE);
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let (loaded, saved_bytes, save_refused) =
            match load_versioned(&path, now_ms, io.as_ref(), |defs: &AchievementDefs| {
                validate_all(&defs.achievements).is_ok()
            }) {
                Ok((loaded, bytes)) => (loaded, bytes, false),
                Err(_) => (None, None, true),
            };
        Self {
            path,
            io,
            save_refused,
            state: Mutex::new(DefsState {
                current: loaded.unwrap_or_default(),
                saved_bytes,
                backed_up: None,
            }),
        }
    }

    pub fn current(&self) -> AchievementDefs {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .current
            .clone()
    }

    /// 定義と、その元になったファイル内容のトークン。画面は保存時にトークンを返す。
    pub fn snapshot(&self) -> (AchievementDefs, String) {
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        (state.current.clone(), content_token(&state.saved_bytes))
    }

    /// ディスクの内容を読み直す(外部で更新された時、Stale のあとの再読込用)。変わったら true。
    pub fn reload(&self, now_ms: u64) -> bool {
        self.reload_with(now_ms, |_| Ok(())).unwrap_or(false)
    }

    /// `reload` と同じだが、取り込む前に `before_adopt` を呼ぶ。Err なら取り込まずに返す
    /// (次の再読込でやり直す)。内容が前回と同じなら復号も退避もしない。
    pub fn reload_with(
        &self,
        now_ms: u64,
        before_adopt: impl FnOnce(&AchievementDefs) -> Result<(), CommitError>,
    ) -> Result<bool, CommitError> {
        if self.save_refused {
            return Ok(false);
        }
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let unchanged = |bytes: &Option<Vec<u8>>| {
            *bytes == self.state.lock().unwrap_or_else(|e| e.into_inner()).saved_bytes
        };
        // 読めない(他プロセスが排他で開いている等)時は「変化なし」にせずエラーにする。
        // Ok(false) にすると古い revision の一覧のまま保存が Stale を繰り返す。
        let bytes = match read_optional(&self.path) {
            Ok(bytes) if !unchanged(&bytes) => bytes,
            Ok(_) => return Ok(false),
            Err(_) => return Err(CommitError::Io),
        };
        let loaded = bytes.as_deref().and_then(|b| {
            decode_versioned(b, |defs: &AchievementDefs| validate_all(&defs.achievements).is_ok())
        });
        if loaded.is_none() && bytes.is_some() {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            if state.backed_up != bytes {
                // 退避できなければ取り込まずにエラーにする(画面に保存先の確認を促す)。
                preserve_broken(&self.path, now_ms, self.io.as_ref())?;
                state.backed_up.clone_from(&bytes);
            }
        }
        let loaded = loaded.unwrap_or_default();
        before_adopt(&loaded)?;
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.current = loaded;
        state.saved_bytes = bytes;
        Ok(true)
    }

    pub fn commit(&self, base_revision: u64, next: AchievementDefs) -> Result<u64, CommitError> {
        self.commit_at(base_revision, None, next)
    }

    /// 書かずに `commit_at` と同じ判定(Stale・検証)だけ行う。無効化の flush より前に呼ぶ。
    pub fn check(
        &self,
        base_revision: u64,
        base_token: Option<&str>,
        next: &AchievementDefs,
    ) -> Result<(), CommitError> {
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.check_locked(&state, base_revision, base_token, next)
    }

    /// 競合(Stale)だけを判定する。入力の正規化・検証より前に呼ぶ。
    pub fn check_base(&self, base_revision: u64, base_token: Option<&str>) -> Result<(), CommitError> {
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.check_base_locked(&state, base_revision, base_token)
    }

    fn check_base_locked(
        &self,
        state: &DefsState,
        base_revision: u64,
        base_token: Option<&str>,
    ) -> Result<(), CommitError> {
        if self.save_refused {
            return Err(CommitError::SaveRefused);
        }
        if base_revision != state.current.revision
            || base_token.is_some_and(|token| token != content_token(&state.saved_bytes))
            || read_optional(&self.path).map_err(|_| CommitError::Io)? != state.saved_bytes
        {
            return Err(CommitError::Stale);
        }
        Ok(())
    }

    fn check_locked(
        &self,
        state: &DefsState,
        base_revision: u64,
        base_token: Option<&str>,
        next: &AchievementDefs,
    ) -> Result<(), CommitError> {
        self.check_base_locked(state, base_revision, base_token)?;
        validate_for_save(&next.achievements)
            .map_err(|(id, reason)| CommitError::Invalid(id, reason))?;
        if next.rates.values().any(|rate| !rate.is_finite()) {
            return Err(CommitError::Io);
        }
        Ok(())
    }

    /// `base_token` があれば、画面が読んだ内容とディスクの現内容が同じ時だけ保存する。
    pub fn commit_at(
        &self,
        base_revision: u64,
        base_token: Option<&str>,
        mut next: AchievementDefs,
    ) -> Result<u64, CommitError> {
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        self.check_locked(&state, base_revision, base_token, &next)?;

        for achievement in &mut next.achievements {
            if let Some(mut preset) = reset_to_preset(achievement) {
                preset.edited = achievement.edited;
                achievement.edited = preset != *achievement;
            }
        }
        next.version = STORE_VERSION;
        next.revision = base_revision.checked_add(1).ok_or(CommitError::Io)?;
        let bytes = write_json(self.io.as_ref(), &self.path, &next)?;
        let revision = next.revision;
        state.current = next;
        state.saved_bytes = Some(bytes);
        Ok(revision)
    }
}

fn read_optional(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e),
    }
}

/// Returns the decoded contents and the original bytes used to detect changes at commit.
pub(crate) fn load_versioned<T: DeserializeOwned>(
    path: &Path,
    now_ms: u64,
    io: &dyn StoreIo,
    validate: impl FnOnce(&T) -> bool,
) -> Result<(Option<T>, Option<Vec<u8>>), CommitError> {
    let Some(bytes) = read_optional(path).map_err(|_| CommitError::SaveRefused)? else {
        return Ok((None, None));
    };
    if let Some(data) = decode_versioned(&bytes, validate) {
        return Ok((Some(data), Some(bytes)));
    }
    preserve_broken(path, now_ms, io)?;
    Ok((None, Some(bytes)))
}

fn decode_versioned<T: DeserializeOwned>(bytes: &[u8], validate: impl FnOnce(&T) -> bool) -> Option<T> {
    let value = serde_json::from_slice::<serde_json::Value>(bytes).ok()?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(STORE_VERSION)) {
        return None;
    }
    serde_json::from_value(value).ok().filter(|data| validate(data))
}

fn preserve_broken(path: &Path, now_ms: u64, io: &dyn StoreIo) -> Result<(), CommitError> {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    for n in 0u64..=u64::MAX {
        let suffix = if n == 0 {
            String::new()
        } else {
            format!("-{n}")
        };
        let backup = path.with_file_name(format!("{name}.broken-{now_ms}{suffix}"));
        match io.create_backup(path, &backup) {
            Ok(()) => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(_) => return Err(CommitError::SaveRefused),
        }
    }
    Err(CommitError::SaveRefused)
}

pub(crate) fn write_json<T: Serialize>(
    io: &dyn StoreIo,
    path: &Path,
    data: &T,
) -> Result<Vec<u8>, CommitError> {
    let bytes = serde_json::to_vec_pretty(data).map_err(|_| CommitError::Io)?;
    io.write_atomic(path, &bytes).map_err(|_| CommitError::Io)?;
    Ok(bytes)
}

// Each open progress snapshot can save until another handle commits to the same file.
// The generation check runs alongside the atomic write without rereading the file.
static PROGRESS_GENERATIONS: Mutex<BTreeMap<PathBuf, Weak<Mutex<u64>>>> =
    Mutex::new(BTreeMap::new());

pub(super) struct ProgressFile {
    path: PathBuf,
    io: Arc<dyn StoreIo>,
    generation: Arc<Mutex<u64>>,
    loaded_generation: AtomicU64,
    save_refused: bool,
}

impl ProgressFile {
    pub fn open(dir: &Path, now_ms: u64, io: Arc<dyn StoreIo>) -> (Self, Option<Progress>) {
        let canonical_dir = dir.canonicalize().unwrap_or_else(|_| dir.to_owned());
        let path = canonical_dir.join(PROGRESS_FILE);
        let generation = {
            let mut generations = PROGRESS_GENERATIONS.lock().unwrap_or_else(|e| e.into_inner());
            generations.retain(|_, entry| entry.strong_count() != 0);
            let entry = generations.entry(path.clone()).or_default();
            if let Some(generation) = entry.upgrade() {
                generation
            } else {
                let generation = Arc::new(Mutex::new(0));
                *entry = Arc::downgrade(&generation);
                generation
            }
        };
        let current = generation.lock().unwrap_or_else(|e| e.into_inner());
        let loaded_generation = AtomicU64::new(*current);
        let (progress, save_refused) =
            match load_versioned(&path, now_ms, io.as_ref(), |_: &Progress| true) {
                Ok((progress, _)) => (progress, false),
                Err(_) => (None, true),
            };
        drop(current);
        (
            Self { path, io, generation, loaded_generation, save_refused },
            progress,
        )
    }

    pub fn save(&self, progress: &Progress) -> Result<(), CommitError> {
        self.commit(Some(progress))
    }

    fn commit(&self, progress: Option<&Progress>) -> Result<(), CommitError> {
        if self.save_refused {
            return Err(CommitError::SaveRefused);
        }
        let mut current = self.generation.lock().unwrap_or_else(|e| e.into_inner());
        if *current != self.loaded_generation.load(Ordering::Relaxed) {
            return Err(CommitError::Io);
        }
        let next = current.checked_add(1).ok_or(CommitError::Io)?;
        if let Some(progress) = progress {
            write_json(self.io.as_ref(), &self.path, progress)?;
        }
        *current = next;
        self.loaded_generation.store(next, Ordering::Relaxed);
        Ok(())
    }
}

/// 進捗ファイルを読み取り専用で読む(退避も書き込みもしない)。無い・壊れている・版違いは None。
pub(super) fn read_progress(dir: &Path) -> Option<Progress> {
    let bytes = read_optional(&dir.join(PROGRESS_FILE)).ok()??;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(u64::from(STORE_VERSION)) {
        return None;
    }
    serde_json::from_value(value).ok()
}

/// Remove a viewer's achievements when there is no active engine.
pub fn delete_listener_progress(
    dir: &Path,
    platform: &str,
    author_id: &str,
) -> anyhow::Result<bool> {
    use std::time::{SystemTime, UNIX_EPOCH};

    static DELETE_TRANSACTION: Mutex<()> = Mutex::new(());
    let _transaction = DELETE_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    let (file, mut progress) = ProgressFile::open(dir, now_ms, Arc::new(FsIo));
    let subject = format!("{platform}:{author_id}");
    let removed = progress.as_mut().is_some_and(|progress| progress.remove_listener(&subject));
    // Even when the file has no matching row, an older engine may have pending counters.
    file.commit(progress.as_ref().filter(|_| removed))?;
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_preserves_structurally_valid_but_invalid_definitions() {
        let dir = tempfile::tempdir().unwrap();
        let mut invalid = AchievementDefs::default();
        invalid.enabled = true;
        invalid.achievements[0].name.clear();
        let bytes = serde_json::to_vec(&invalid).unwrap();
        fs::write(dir.path().join(DEFS_FILE), &bytes).unwrap();

        let store = DefsStore::load(dir.path(), 42);
        assert!(!store.current().enabled);
        assert_eq!(
            fs::read(dir.path().join(format!("{DEFS_FILE}.broken-42"))).unwrap(),
            bytes
        );
    }

    #[test]
    fn commit_derives_edited_flag_for_builtin_definition() {
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut next = store.current();
        let original = next.achievements[0].clone();
        next.achievements[0].condition.as_mut().unwrap().threshold += 1.0;
        assert_eq!(store.commit(0, next), Ok(1));
        let mut next = store.current();
        assert!(next.achievements[0].edited);

        next.achievements[0] = original;
        next.achievements[0].enabled = false;
        next.achievements[0].edited = true;
        assert_eq!(store.commit(1, next), Ok(2));
        assert!(!store.current().achievements[0].edited);
    }

    #[test]
    fn defs_commit_rejects_a_stale_separately_loaded_store() {
        let dir = tempfile::tempdir().unwrap();
        let winner = DefsStore::load(dir.path(), 0);
        let loser = DefsStore::load(dir.path(), 0);
        let mut first = winner.current();
        first.enabled = true;
        assert_eq!(winner.commit(0, first), Ok(1));

        let path = dir.path().join(DEFS_FILE);
        let saved = fs::read(&path).unwrap();
        assert_eq!(loser.commit(0, loser.current()), Err(CommitError::Stale));
        assert_eq!(loser.current().revision, 0);
        assert_eq!(fs::read(path).unwrap(), saved);
    }

    #[test]
    fn reload_does_not_back_up_the_same_broken_file_again() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(DEFS_FILE), "{broken").unwrap();
        let store = DefsStore::load(dir.path(), 1);
        for now in 2..5 {
            assert!(!store.reload(now));
        }
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2, "defs と退避 1 個だけ");
    }

    #[test]
    fn reload_reports_unreadable_defs_instead_of_unchanged() {
        // 読めない定義(ここではディレクトリに差し替え。実機では他プロセスの排他オープン)を
        // 「変化なし」にすると、古い revision の一覧から保存して Stale を繰り返す。
        let dir = tempfile::tempdir().unwrap();
        let store = DefsStore::load(dir.path(), 0);
        let mut next = store.current();
        next.enabled = true;
        assert_eq!(store.commit(0, next), Ok(1));
        let path = dir.path().join(DEFS_FILE);
        fs::remove_file(&path).unwrap();
        fs::create_dir(&path).unwrap();
        assert_eq!(store.reload_with(1, |_| Ok(())), Err(CommitError::Io));
        assert_eq!(store.current().revision, 1, "読めない間は取り込まない");
    }
}
