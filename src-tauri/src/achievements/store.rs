use std::collections::BTreeMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{de::DeserializeOwned, Deserialize, Serialize};
use uuid::Uuid;

use super::currency::RateTable;
use super::model::{presets, reset_to_preset, validate_all, Achievement, ValidationError};

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

    pub fn commit(
        &self,
        base_revision: u64,
        mut next: AchievementDefs,
    ) -> Result<u64, CommitError> {
        if self.save_refused {
            return Err(CommitError::SaveRefused);
        }
        let _transaction = DEFS_TRANSACTION.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if base_revision != state.current.revision
            || read_optional(&self.path).map_err(|_| CommitError::Io)? != state.saved_bytes
        {
            return Err(CommitError::Stale);
        }
        validate_all(&next.achievements)
            .map_err(|(id, reason)| CommitError::Invalid(id, reason))?;
        if next.rates.values().any(|rate| !rate.is_finite()) {
            return Err(CommitError::Io);
        }

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
    if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
        if value.get("version").and_then(serde_json::Value::as_u64)
            == Some(u64::from(STORE_VERSION))
        {
            if let Ok(data) = serde_json::from_value(value) {
                if validate(&data) {
                    return Ok((Some(data), Some(bytes)));
                }
            }
        }
    }
    preserve_broken(path, now_ms, io)?;
    Ok((None, Some(bytes)))
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
}
