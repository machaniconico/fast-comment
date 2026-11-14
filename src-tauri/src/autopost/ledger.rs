//! Durable, bounded posting history. Callers must save `posting` before performing
//! an external action, and must stop automatic posting after `LoadOutcome::Corrupt`.
//! No clock or application handle is used here: recovery timestamps are injected.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::Platform;

pub const MAX_ENTRIES: usize = 200;
pub const RECENT_TEXT_LIMIT: usize = 20;
const VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LedgerState {
    Posting,
    Posted,
    Opened,
    Failed,
    Uncertain,
    Cancelled,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LedgerEntry {
    pub key: String,
    pub platform: Platform,
    pub stream_id: String,
    pub channel: String,
    pub state: LedgerState,
    pub at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tweet_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl LedgerEntry {
    fn validate(&self) -> io::Result<()> {
        if self.state == LedgerState::Skipped
            && self.reason.as_deref().map_or(true, |s| s.trim().is_empty())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "skipped ledger entry requires a reason",
            ));
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Ledger {
    version: u32,
    entries: Vec<LedgerEntry>,
}

impl Default for Ledger {
    fn default() -> Self {
        Self {
            version: VERSION,
            entries: Vec::new(),
        }
    }
}

/// Corruption is deliberately distinct from an empty ledger. The caller must
/// disable automatic posting for this run; the next launch can start empty.
#[derive(Debug)]
pub enum LoadOutcome {
    Loaded(Ledger),
    Corrupt { backup_path: PathBuf },
}

impl Ledger {
    /// Missing files are empty. Other I/O errors (including backup failures)
    /// propagate, so they cannot accidentally enable posting with empty history.
    pub fn load(path: &Path, now_ms: u64) -> io::Result<LoadOutcome> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Ok(LoadOutcome::Loaded(Self::default()));
            }
            Err(e) => return Err(e),
        };
        let parsed = serde_json::from_slice::<Self>(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
            .and_then(|ledger| {
                ledger.validate()?;
                Ok(ledger)
            });
        match parsed {
            Ok(mut ledger) => {
                ledger.normalize();
                ledger.recover_posting();
                Ok(LoadOutcome::Loaded(ledger))
            }
            Err(_) => {
                // Preserve every corrupt file, even for repeated recovery with
                // the same injected timestamp. Never overwrite an older backup.
                let backup_path =
                    suffixed_path(path, &format!(".corrupt-{now_ms}-{}", uuid::Uuid::new_v4()));
                fs::rename(path, &backup_path)?;
                Ok(LoadOutcome::Corrupt { backup_path })
            }
        }
    }

    pub fn entries(&self) -> &[LedgerEntry] {
        &self.entries
    }

    pub fn get(&self, key: &str) -> Option<&LedgerEntry> {
        self.entries.iter().find(|entry| entry.key == key)
    }

    /// Insert or replace a stream's record, retaining the newest 200 records.
    /// Equal timestamps retain insertion order with the newest insertion first.
    pub fn upsert(&mut self, entry: LedgerEntry) -> io::Result<()> {
        entry.validate()?;
        self.entries.retain(|old| old.key != entry.key);
        self.entries.insert(0, entry);
        self.normalize();
        Ok(())
    }

    /// Only inspect the newest 20 entries, even when some have no text.
    pub fn recent_texts(&self) -> impl Iterator<Item = &str> {
        self.entries
            .iter()
            .take(RECENT_TEXT_LIMIT)
            .filter_map(|entry| entry.text.as_deref())
    }

    /// A previous process may have posted before crashing. Preserve its key,
    /// timestamp and text, and never turn an interrupted attempt into a retry.
    /// Returns how many entries changed; repeated calls are harmless.
    pub fn recover_posting(&mut self) -> usize {
        let mut recovered = 0;
        for entry in &mut self.entries {
            if entry.state == LedgerState::Posting {
                entry.state = LedgerState::Uncertain;
                entry.reason = Some("interrupted".into());
                recovered += 1;
            }
        }
        recovered
    }

    /// Serialize first, then write and sync a uniquely named sibling temporary
    /// file before replacing the destination. Never delete the old file first.
    /// A failed write, sync, or rename leaves the old destination untouched.
    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.save_with(path, |file, bytes| {
            file.write_all(bytes)?;
            file.sync_all()
        })
    }

    fn save_with(
        &self,
        path: &Path,
        write_and_sync: impl FnOnce(&mut fs::File, &[u8]) -> io::Result<()>,
    ) -> io::Result<()> {
        self.validate()?;
        // Also enforce the bound when constructed through Deserialize rather
        // than load/upsert; the on-disk format always stays bounded and sorted.
        let mut snapshot = self.clone();
        snapshot.normalize();
        let bytes = serde_json::to_vec_pretty(&snapshot)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let temp = suffixed_path(path, &format!(".tmp-{}", uuid::Uuid::new_v4()));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        let result = write_and_sync(&mut file, &bytes);
        // Windows requires closing the temporary handle before replacement.
        drop(file);
        let result = result.and_then(|()| fs::rename(&temp, path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result
    }

    fn validate(&self) -> io::Result<()> {
        if self.version != VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "unsupported ledger version",
            ));
        }
        for entry in &self.entries {
            entry.validate()?;
        }
        Ok(())
    }

    fn normalize(&mut self) {
        self.entries.sort_by(|a, b| b.at_ms.cmp(&a.at_ms));
        self.entries.truncate(MAX_ENTRIES);
    }
}

fn suffixed_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let path =
                std::env::temp_dir().join(format!("autopost-ledger-{}", uuid::Uuid::new_v4()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }

        fn path(&self) -> PathBuf {
            self.0.join("ledger.json")
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn entry(index: u64, state: LedgerState) -> LedgerEntry {
        LedgerEntry {
            key: format!("youtube:{index}"),
            platform: Platform::Youtube,
            stream_id: index.to_string(),
            channel: "my-channel".into(),
            state,
            at_ms: index,
            tweet_id: None,
            text: Some(format!("announcement {index}")),
            reason: (state == LedgerState::Skipped).then(|| "stale".into()),
        }
    }

    fn loaded(path: &Path) -> Ledger {
        match Ledger::load(path, 1234).unwrap() {
            LoadOutcome::Loaded(ledger) => ledger,
            LoadOutcome::Corrupt { .. } => panic!("unexpected corruption"),
        }
    }

    #[test]
    fn missing_file_is_empty_without_creating_a_file() {
        let dir = TestDir::new();
        assert!(loaded(&dir.path()).entries().is_empty());
        assert!(!dir.path().exists());
    }

    #[test]
    fn states_and_optional_fields_round_trip_and_replace_existing_file() {
        let dir = TestDir::new();
        Ledger::default().save(&dir.path()).unwrap();
        let mut ledger = Ledger::default();
        for (index, state) in [
            LedgerState::Posted,
            LedgerState::Opened,
            LedgerState::Failed,
            LedgerState::Uncertain,
            LedgerState::Cancelled,
            LedgerState::Skipped,
        ]
        .into_iter()
        .enumerate()
        {
            ledger.upsert(entry(index as u64, state)).unwrap();
        }
        ledger.entries[0].tweet_id = Some("tweet-123".into());
        ledger.entries[1].text = None;
        ledger.save(&dir.path()).unwrap();
        assert_eq!(loaded(&dir.path()), ledger);
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path()).unwrap()).unwrap();
        assert_eq!(json["version"], 1);
        assert_eq!(json["entries"][0]["streamId"], "5");
        assert_eq!(json["entries"][0]["state"], "skipped");
        assert_eq!(json["entries"][0]["reason"], "stale");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn oldest_entry_is_evicted_on_201st_insert_and_on_load() {
        let dir = TestDir::new();
        let mut ledger = Ledger::default();
        for index in 0..201 {
            ledger.upsert(entry(index, LedgerState::Posted)).unwrap();
        }
        assert_eq!(ledger.entries().len(), MAX_ENTRIES);
        assert_eq!(ledger.entries()[0].at_ms, 200);
        assert_eq!(ledger.entries()[199].at_ms, 1);
        assert!(ledger.get("youtube:0").is_none());
        ledger.save(&dir.path()).unwrap();
        assert_eq!(loaded(&dir.path()), ledger);
        ledger.entries.push(entry(0, LedgerState::Posted));
        ledger.entries.reverse();
        fs::write(dir.path(), serde_json::to_vec(&ledger).unwrap()).unwrap();
        let restored = loaded(&dir.path());
        assert_eq!(restored.entries().len(), MAX_ENTRIES);
        assert_eq!(restored.entries()[0].at_ms, 200);
        assert!(restored.get("youtube:0").is_none());
    }

    #[test]
    fn upsert_replaces_a_key_and_recent_texts_use_only_20_newest_entries() {
        let mut ledger = Ledger::default();
        for index in (0..25).rev() {
            ledger.upsert(entry(index, LedgerState::Posted)).unwrap();
        }
        let mut replacement = entry(24, LedgerState::Opened);
        replacement.text = None;
        ledger.upsert(replacement).unwrap();
        assert_eq!(ledger.entries().len(), 25);
        assert_eq!(ledger.get("youtube:24").unwrap().state, LedgerState::Opened);
        let texts: Vec<_> = ledger.recent_texts().collect();
        assert_eq!(texts.len(), 19);
        assert_eq!(texts[0], "announcement 23");
        assert_eq!(texts[18], "announcement 5");
    }

    #[test]
    fn corrupt_json_and_invalid_schema_are_backed_up_without_losing_bytes() {
        let dir = TestDir::new();
        let mut backups = Vec::new();
        for bytes in [
            b"{broken".as_slice(),
            b"{}",
            b"{\"version\":2,\"entries\":[]}",
        ] {
            fs::write(dir.path(), bytes).unwrap();
            match Ledger::load(&dir.path(), 5678).unwrap() {
                LoadOutcome::Corrupt { backup_path } => {
                    assert!(backup_path
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("ledger.json.corrupt-5678"));
                    assert_eq!(fs::read(&backup_path).unwrap(), bytes);
                    assert!(!dir.path().exists());
                    backups.push(backup_path);
                }
                _ => panic!("corrupt ledger must be reported"),
            }
            assert!(loaded(&dir.path()).entries().is_empty());
        }
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 3);
        assert!(backups.iter().all(|path| path.exists()));
    }

    #[test]
    fn posting_is_recovered_as_uncertain_and_never_retried() {
        let dir = TestDir::new();
        let mut ledger = Ledger::default();
        ledger.upsert(entry(1, LedgerState::Posting)).unwrap();
        ledger.upsert(entry(2, LedgerState::Posted)).unwrap();
        ledger.save(&dir.path()).unwrap();
        let mut recovered_in_memory = ledger.clone();
        assert_eq!(recovered_in_memory.recover_posting(), 1);
        let mut restored = loaded(&dir.path());
        assert_eq!(restored, recovered_in_memory);
        let recovered = restored.get("youtube:1").unwrap();
        assert_eq!(recovered.state, LedgerState::Uncertain);
        assert_eq!(recovered.reason.as_deref(), Some("interrupted"));
        assert_eq!(recovered.at_ms, 1);
        assert_eq!(recovered.text.as_deref(), Some("announcement 1"));
        assert_eq!(
            restored.get("youtube:2").unwrap().state,
            LedgerState::Posted
        );
        assert_eq!(restored.recover_posting(), 0);
        restored.save(&dir.path()).unwrap();
        assert_eq!(loaded(&dir.path()), restored);
    }

    #[test]
    fn partial_write_or_sync_failure_preserves_original_and_removes_temp() {
        let dir = TestDir::new();
        let mut ledger = Ledger::default();
        ledger.save(&dir.path()).unwrap();
        let original = fs::read(dir.path()).unwrap();
        ledger.upsert(entry(1, LedgerState::Posting)).unwrap();
        for partial in [true, false] {
            let result = ledger.save_with(&dir.path(), |file, bytes| {
                file.write_all(if partial { &bytes[..5] } else { bytes })?;
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected write/sync failure",
                ))
            });
            assert!(result.is_err());
            assert_eq!(fs::read(dir.path()).unwrap(), original);
            assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        }
    }

    #[test]
    fn rename_failure_cleans_temp_and_io_errors_are_not_empty_ledgers() {
        let dir = TestDir::new();
        fs::create_dir(dir.path()).unwrap();
        fs::write(dir.path().join("keep"), b"keep").unwrap();
        assert!(Ledger::default().save(&dir.path()).is_err());
        assert_eq!(fs::read(dir.path().join("keep")).unwrap(), b"keep");
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        assert!(Ledger::load(&dir.path(), 0).is_err());
    }

    #[test]
    fn skipped_requires_a_nonempty_reason() {
        let mut ledger = Ledger::default();
        let mut skipped = entry(1, LedgerState::Skipped);
        skipped.reason = Some("  ".into());
        assert!(ledger.upsert(skipped).is_err());
        assert!(ledger.entries().is_empty());
    }

    #[test]
    fn save_bounds_deserialized_history_and_orders_equal_timestamps_stably() {
        let dir = TestDir::new();
        let mut ledger = Ledger::default();
        for index in 0..201 {
            ledger.entries.push(entry(index, LedgerState::Posted));
        }
        ledger.save(&dir.path()).unwrap();
        let json: serde_json::Value =
            serde_json::from_slice(&fs::read(dir.path()).unwrap()).unwrap();
        assert_eq!(json["entries"].as_array().unwrap().len(), MAX_ENTRIES);
        assert_eq!(json["entries"][0]["atMs"], 200);

        let mut ledger = Ledger::default();
        let first = entry(1, LedgerState::Opened);
        let mut second = entry(2, LedgerState::Opened);
        second.at_ms = first.at_ms;
        ledger.upsert(first.clone()).unwrap();
        ledger.upsert(second.clone()).unwrap();
        assert_eq!(ledger.entries(), &[second, first]);
        ledger.save(&dir.path()).unwrap();
        assert_eq!(loaded(&dir.path()), ledger);
    }
}
