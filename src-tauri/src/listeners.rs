//! Persistent listener records. Message processing only changes memory; flush snapshots on a worker.
use crate::model::{ChatMessage, ListenerInfo, MessageKind, Platform};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Per-listener overrides. Web Speech rate is 0.5..=2.0, pitch is 0.0..=2.0.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct TtsVoiceOverride {
    pub voicevox_speaker: Option<u32>,
    pub bouyomi_voice: Option<i32>,
    pub web_speech_voice: Option<String>,
    #[serde(deserialize_with = "deserialize_rate")]
    pub rate: Option<f32>,
    #[serde(deserialize_with = "deserialize_pitch")]
    pub pitch: Option<f32>,
}

fn clamp_value(value: Option<f32>, min: f32, max: f32) -> Option<f32> {
    value.filter(|v| !v.is_nan()).map(|v| v.clamp(min, max))
}
fn deserialize_rate<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    Ok(clamp_value(Option::<f32>::deserialize(d)?, 0.5, 2.0))
}
fn deserialize_pitch<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<f32>, D::Error> {
    Ok(clamp_value(Option::<f32>::deserialize(d)?, 0.0, 2.0))
}
impl TtsVoiceOverride {
    pub fn normalize(&mut self) {
        self.rate = clamp_value(self.rate, 0.5, 2.0);
        self.pitch = clamp_value(self.pitch, 0.0, 2.0);
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NameHistory {
    pub name: String,
    pub changed_at_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ListenerRecord {
    pub platform: String,
    pub author_id: String,
    pub name: String,
    pub nickname: Option<String>,
    pub label: Option<String>,
    pub memo: Option<String>,
    pub comment_count: u64,
    pub gift_count: u64,
    pub first_seen_ms: u64,
    pub last_seen_ms: u64,
    pub name_history: Vec<NameHistory>,
    pub tts_muted: bool,
    #[serde(default)]
    pub tts_voice: Option<TtsVoiceOverride>,
}

impl ListenerRecord {
    fn new(platform: &str, author_id: &str) -> Self {
        Self {
            platform: platform.into(),
            author_id: author_id.into(),
            name: author_id.into(),
            nickname: None,
            label: None,
            memo: None,
            comment_count: 0,
            gift_count: 0,
            first_seen_ms: 0,
            last_seen_ms: 0,
            name_history: Vec::new(),
            tts_muted: false,
            tts_voice: None,
        }
    }
}

#[derive(Default)]
struct Records {
    entries: HashMap<String, ListenerRecord>,
    dirty: bool,
    revision: u64,
    closed: bool,
}
impl Records {
    fn changed(&mut self) {
        self.dirty = true;
        self.revision = self.revision.wrapping_add(1);
    }
}

pub struct ListenerStore {
    path: PathBuf,
    records: Mutex<Records>,
    // Serializes periodic and exit saves, without blocking observe on disk I/O.
    saving: Mutex<()>,
    flush_blocked: bool,
}

fn key(platform: &str, author_id: &str) -> String {
    format!("{platform}:{author_id}")
}
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}
fn normalized(value: Option<String>, max: usize) -> Option<String> {
    value.and_then(|s| {
        let s: String = s.trim().chars().take(max).collect();
        (!s.is_empty()).then_some(s)
    })
}

// create_new prevents overwriting a backup, even when two loads share an epoch ms.
fn backup_corrupt(path: &Path, backup: &Path) -> std::io::Result<()> {
    let mut source = fs::File::open(path)?;
    let mut target = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(backup)?;
    std::io::copy(&mut source, &mut target)?;
    target.sync_all()?;
    drop(target);
    drop(source);
    fs::remove_file(path)
}

impl ListenerStore {
    pub fn load(dir: &Path) -> Self {
        Self::load_with_backup(dir, backup_corrupt)
    }

    fn load_with_backup(
        dir: &Path,
        backup: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    ) -> Self {
        let mut flush_blocked = false;
        let path = dir.join("listeners.json");
        let entries = match fs::read(&path) {
            Ok(bytes) => match serde_json::from_slice::<HashMap<String, ListenerRecord>>(&bytes) {
                Ok(entries) => entries,
                Err(e) => {
                    tracing::error!("リスナー記録のJSONが壊れています・空で開始: {e}");
                    if let Err(e) =
                        backup(&path, &dir.join(format!("listeners.json.bak.{}", now_ms())))
                    {
                        flush_blocked = true;
                        tracing::error!("リスナー記録の退避に失敗・保存を禁止: {e}");
                    }
                    HashMap::new()
                }
            },
            Err(e) => {
                if e.kind() != std::io::ErrorKind::NotFound {
                    flush_blocked = true;
                    tracing::error!("リスナー記録の読込失敗・保存を禁止: {e}");
                }
                HashMap::new()
            }
        };
        Self {
            path,
            flush_blocked,
            records: Mutex::new(Records {
                entries,
                ..Records::default()
            }),
            saving: Mutex::new(()),
        }
    }

    pub fn observe(&self, msg: &mut ChatMessage) {
        self.observe_at(msg, now_ms());
    }

    fn observe_at(&self, msg: &mut ChatMessage, now: u64) {
        if msg.kind == MessageKind::System || msg.is_injected_test_comment() {
            return;
        }
        let platform = match msg.platform {
            Platform::Twitch => "twitch",
            Platform::Youtube => "youtube",
            Platform::X => "x",
            Platform::Niconico => "niconico",
        };
        let author_id = if msg.author.id.is_empty() {
            &msg.author.name
        } else {
            &msg.author.id
        };
        let mut records = self.records.lock().unwrap();
        if records.closed {
            return;
        }
        let record = records
            .entries
            .entry(key(platform, author_id))
            .or_insert_with(|| ListenerRecord::new(platform, author_id));
        let is_first_ever = record.comment_count == 0;
        let prev_seen_ms = (!is_first_ever).then_some(record.last_seen_ms);
        if is_first_ever {
            record.first_seen_ms = now;
        } else if record.name != msg.author.name {
            record.name_history.insert(
                0,
                NameHistory {
                    name: record.name.clone(),
                    changed_at_ms: now,
                },
            );
            record.name_history.truncate(5);
        }
        record.name = msg.author.name.clone();
        record.last_seen_ms = now;
        record.comment_count = record.comment_count.saturating_add(1);
        if msg.amount.is_some()
            || matches!(
                msg.kind,
                MessageKind::SuperChat | MessageKind::Bits | MessageKind::Gift
            )
        {
            record.gift_count = record.gift_count.saturating_add(1);
        }
        msg.author.nickname = record.nickname.clone();
        msg.listener = Some(ListenerInfo {
            comment_count: record.comment_count,
            gift_count: record.gift_count,
            first_seen_ms: record.first_seen_ms,
            prev_seen_ms,
            is_first_ever,
            label: record.label.clone(),
            tts_muted: record.tts_muted,
            tts_voice: record.tts_voice.clone(),
        });
        records.changed();
    }

    pub fn get(&self, platform: &str, author_id: &str) -> Option<ListenerRecord> {
        self.records
            .lock()
            .unwrap()
            .entries
            .get(&key(platform, author_id))
            .cloned()
    }

    pub fn update(
        &self,
        platform: &str,
        author_id: &str,
        nickname: Option<String>,
        label: Option<String>,
        memo: Option<String>,
        tts_muted: bool,
    ) -> ListenerRecord {
        let mut records = self.records.lock().unwrap();
        let record = records
            .entries
            .entry(key(platform, author_id))
            .or_insert_with(|| ListenerRecord::new(platform, author_id));
        record.nickname = normalized(nickname, 200);
        record.label = normalized(label, 200);
        record.memo = normalized(memo, 2000);
        record.tts_muted = tts_muted;
        let result = record.clone();
        records.changed();
        result
    }

    pub fn set_voice(
        &self,
        platform: &str,
        author_id: &str,
        mut voice: Option<TtsVoiceOverride>,
    ) -> ListenerRecord {
        if let Some(voice) = &mut voice {
            voice.normalize();
        }
        let mut records = self.records.lock().unwrap();
        let record = records
            .entries
            .entry(key(platform, author_id))
            .or_insert_with(|| ListenerRecord::new(platform, author_id));
        record.tts_voice = voice;
        let result = record.clone();
        records.changed();
        result
    }

    pub fn list(&self, query: Option<&str>, limit: u32) -> Vec<ListenerRecord> {
        let query = query.unwrap_or_default().to_lowercase();
        let records = self.records.lock().unwrap();
        let mut found: Vec<_> = records
            .entries
            .values()
            .filter(|r| {
                [
                    &r.name,
                    r.nickname.as_deref().unwrap_or(""),
                    r.label.as_deref().unwrap_or(""),
                    &r.author_id,
                ]
                .iter()
                .any(|s| s.to_lowercase().contains(&query))
            })
            .cloned()
            .collect();
        drop(records);
        found.sort_by(|a, b| {
            b.last_seen_ms
                .cmp(&a.last_seen_ms)
                .then_with(|| a.platform.cmp(&b.platform))
                .then_with(|| a.author_id.cmp(&b.author_id))
        });
        found.truncate(limit.clamp(1, 1000) as usize);
        found
    }

    pub fn delete(&self, platform: &str, author_id: &str) -> bool {
        let mut records = self.records.lock().unwrap();
        let deleted = records.entries.remove(&key(platform, author_id)).is_some();
        if deleted {
            records.changed();
        }
        deleted
    }

    pub fn flush(&self) -> anyhow::Result<()> {
        if self.flush_blocked {
            anyhow::bail!("リスナー記録の読込/退避失敗のため保存は禁止されています");
        }
        let _saving = self.saving.lock().unwrap();
        let (snapshot, revision) = {
            let records = self.records.lock().unwrap();
            if !records.dirty {
                return Ok(());
            }
            (records.entries.clone(), records.revision)
        };
        fs::create_dir_all(self.path.parent().expect("config directory"))?;
        let tmp = self.path.with_extension("json.tmp");
        let bytes = serde_json::to_vec(&snapshot)?;
        {
            let mut file = fs::File::create(&tmp)?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        fs::rename(tmp, &self.path)?;
        let mut records = self.records.lock().unwrap();
        if records.revision == revision {
            records.dirty = false;
        }
        Ok(())
    }

    pub fn shutdown(&self) -> anyhow::Result<()> {
        self.records.lock().unwrap().closed = true;
        self.flush()
    }
}

/// Shared by the pipeline and enqueue boundary, including explicit speech commands.
pub fn allows_tts(msg: &ChatMessage) -> bool {
    !msg.skip_tts && !msg.listener.as_ref().is_some_and(|info| info.tts_muted)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Amount, Author, Roles};

    struct TestDir(PathBuf);
    impl TestDir {
        fn new() -> Self {
            let path = std::env::temp_dir()
                .join(format!("fast-comment-listeners-{}", uuid::Uuid::new_v4()));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn store(&self) -> ListenerStore {
            ListenerStore::load(&self.0)
        }
    }
    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn message() -> ChatMessage {
        ChatMessage {
            listener: None,
            id: "message".into(),
            platform: Platform::Twitch,
            channel: "channel".into(),
            author: Author {
                nickname: None,
                avatar_url: None,
                id: "user".into(),
                name: "Alice".into(),
                display_color: None,
                badges: vec![],
                roles: Roles::default(),
            },
            fragments: vec![],
            kind: MessageKind::Normal,
            amount: None,
            timestamp_ms: 999,
            raw: None,
            skip_tts: false,
            injected_test: false,
        }
    }

    #[test]
    fn listeners_first_and_repeat_observations_and_name_history() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        store.observe_at(&mut msg, 10);
        let first = msg.listener.as_ref().unwrap();
        assert!(first.is_first_ever);
        assert_eq!(first.prev_seen_ms, None);
        assert_eq!(first.comment_count, 1);
        store.observe_at(&mut msg, 20);
        let next = msg.listener.as_ref().unwrap();
        assert!(!next.is_first_ever);
        assert_eq!(next.prev_seen_ms, Some(10));
        assert_eq!(next.comment_count, 2);
        assert_eq!(next.first_seen_ms, 10);
        for i in 0..7 {
            msg.author.name = format!("Name{i}");
            store.observe_at(&mut msg, 30 + i);
        }
        let record = store.get("twitch", "user").unwrap();
        assert_eq!(record.name, "Name6");
        assert_eq!(record.name_history.len(), 5);
        assert_eq!(
            record.name_history[0],
            NameHistory {
                name: "Name5".into(),
                changed_at_ms: 36
            }
        );
        assert_eq!(record.name_history[4].name, "Name1");
        assert!(!dir.0.join("listeners.json").exists());
    }

    #[test]
    fn listeners_system_gifts_and_fallback_identity() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        msg.kind = MessageKind::System;
        store.observe_at(&mut msg, 1);
        assert!(store.list(None, 10).is_empty());
        assert!(msg.listener.is_none());
        for kind in [
            MessageKind::Normal,
            MessageKind::Membership,
            MessageKind::SuperChat,
            MessageKind::Bits,
            MessageKind::Gift,
        ] {
            msg.kind = kind;
            store.observe_at(&mut msg, 2);
        }
        assert_eq!(store.get("twitch", "user").unwrap().gift_count, 3);
        msg.kind = MessageKind::Normal;
        msg.amount = Some(Amount {
            value: 1.,
            currency: "pt".into(),
            raw_text: "1".into(),
        });
        store.observe_at(&mut msg, 3);
        assert_eq!(store.get("twitch", "user").unwrap().gift_count, 4);
        msg.kind = MessageKind::Gift;
        store.observe_at(&mut msg, 4);
        assert_eq!(store.get("twitch", "user").unwrap().gift_count, 5);
        msg.author.id.clear();
        store.observe_at(&mut msg, 5);
        assert!(store.get("twitch", "Alice").is_some());
        msg.platform = Platform::Youtube;
        store.observe_at(&mut msg, 6);
        assert_eq!(store.get("youtube", "Alice").unwrap().comment_count, 1);
    }

    #[test]
    fn listeners_edit_normalization_nickname_and_tts_gate() {
        let dir = TestDir::new();
        let store = dir.store();
        let edited = store.update(
            "twitch",
            "user",
            Some(" Nick ".into()),
            Some(" label ".into()),
            Some("   ".into()),
            true,
        );
        assert_eq!(edited.nickname.as_deref(), Some("Nick"));
        assert_eq!(edited.memo, None);
        let mut msg = message();
        store.observe_at(&mut msg, 100);
        assert_eq!(msg.author.nickname.as_deref(), Some("Nick"));
        assert_eq!(
            msg.listener.as_ref().unwrap().label.as_deref(),
            Some("label")
        );
        assert!(msg.listener.as_ref().unwrap().is_first_ever);
        assert!(!allows_tts(&msg));
        store.update("twitch", "user", None, None, None, false);
        store.observe_at(&mut msg, 101);
        assert!(allows_tts(&msg));
        msg.skip_tts = true;
        assert!(!allows_tts(&msg));
        let edited = store.update(
            "twitch",
            "user",
            Some("あ".repeat(201)),
            Some("い".repeat(201)),
            Some("う".repeat(2001)),
            false,
        );
        assert_eq!(edited.nickname.unwrap().chars().count(), 200);
        assert_eq!(edited.label.unwrap().chars().count(), 200);
        assert_eq!(edited.memo.unwrap().chars().count(), 2000);
        assert!(store.delete("twitch", "user"));
        assert!(!store.delete("twitch", "user"));
    }

    #[test]
    fn listeners_search_order_and_limit_clamp() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        for i in 0..1005 {
            msg.author.id = format!("id{i:04}");
            store.observe_at(&mut msg, i);
        }
        store.update(
            "twitch",
            "id0001",
            Some("Nickname".into()),
            Some("Label".into()),
            Some("MemoOnly".into()),
            false,
        );
        for query in ["aLiCe", "nICKname", "lABEL", "ID0001"] {
            assert!(!store.list(Some(query), 10).is_empty());
        }
        assert!(store.list(Some("MemoOnly"), 10).is_empty());
        assert_eq!(store.list(None, 0).len(), 1);
        let found = store.list(None, u32::MAX);
        assert_eq!(found.len(), 1000);
        assert_eq!(found[0].author_id, "id1004");
        assert!(found
            .windows(2)
            .all(|w| w[0].last_seen_ms >= w[1].last_seen_ms));
    }

    #[test]
    fn listeners_atomic_save_load_delete_and_shutdown() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        store.observe_at(&mut msg, 12);
        store.update(
            "twitch",
            "user",
            Some("Nick".into()),
            Some("Label".into()),
            Some("Memo".into()),
            true,
        );
        let expected = store.get("twitch", "user");
        store.flush().unwrap();
        assert!(!store.records.lock().unwrap().dirty);
        assert_eq!(dir.store().get("twitch", "user"), expected);
        assert!(!dir.0.join("listeners.json.tmp").exists());
        // Saving again replaces the existing file on Windows too.
        store.delete("twitch", "user");
        store.flush().unwrap();
        assert!(dir.store().list(None, 10).is_empty());
        store.observe_at(&mut msg, 13);
        store.shutdown().unwrap();
        store.observe_at(&mut msg, 14);
        assert_eq!(dir.store().get("twitch", "user").unwrap().comment_count, 1);
    }

    #[test]
    fn listeners_corrupt_json_is_backed_up() {
        let dir = TestDir::new();
        fs::write(dir.0.join("listeners.json"), b"{broken").unwrap();
        let store = dir.store();
        assert!(store.list(None, 10).is_empty());
        assert_eq!(
            fs::read(
                fs::read_dir(&dir.0)
                    .unwrap()
                    .map(|e| e.unwrap().path())
                    .find(|p| p
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .starts_with("listeners.json.bak."))
                    .unwrap()
            )
            .unwrap(),
            b"{broken"
        );
        let mut msg = message();
        store.observe_at(&mut msg, 1);
        store.flush().unwrap();
        assert!(dir.store().get("twitch", "user").is_some());
    }
    #[test]
    fn listeners_legacy_missing_and_unknown_fields_load_without_backup() {
        let dir = TestDir::new();
        let legacy = serde_json::json!({
            "twitch:user": {"platform": "twitch", "authorId": "user", "name": "Alice",
                "nickname": null, "label": null, "memo": null, "commentCount": 5,
                "giftCount": 1, "firstSeenMs": 10, "lastSeenMs": 20,
                "nameHistory": [], "ttsMuted": false},
            "youtube:old": {"platform": "youtube", "authorId": "old", "futureField": true}
        });
        fs::write(
            dir.0.join("listeners.json"),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let store = dir.store();
        let record = store.get("twitch", "user").unwrap();
        assert_eq!(record.comment_count, 5);
        assert!(record.tts_voice.is_none());
        assert_eq!(store.get("youtube", "old").unwrap().name_history, vec![]);
        assert_eq!(fs::read_dir(&dir.0).unwrap().count(), 1);
        assert_eq!(
            serde_json::from_str::<ListenerRecord>("{}").unwrap(),
            ListenerRecord::default()
        );
    }

    #[test]
    fn listener_voice_normalizes_persists_propagates_and_clears_independently() {
        let dir = TestDir::new();
        let store = dir.store();
        store.update(
            "twitch",
            "user",
            Some("Nick".into()),
            Some("VIP".into()),
            None,
            true,
        );
        let record = store.set_voice(
            "twitch",
            "user",
            Some(TtsVoiceOverride {
                voicevox_speaker: Some(9),
                bouyomi_voice: Some(2),
                web_speech_voice: Some("voice".into()),
                rate: Some(9.0),
                pitch: Some(-2.0),
            }),
        );
        let voice = record.tts_voice.as_ref().unwrap();
        assert_eq!((voice.rate, voice.pitch), (Some(2.0), Some(0.0)));
        let mut msg = message();
        store.observe_at(&mut msg, 1);
        assert_eq!(msg.listener.as_ref().unwrap().tts_voice, record.tts_voice);
        assert!(msg.listener.as_ref().unwrap().tts_muted);
        store.flush().unwrap();
        let reloaded = dir.store();
        assert_eq!(
            reloaded.get("twitch", "user").unwrap().tts_voice,
            record.tts_voice
        );
        let record = reloaded.set_voice("twitch", "user", None);
        assert!(record.tts_voice.is_none());
        assert_eq!(record.nickname.as_deref(), Some("Nick"));
        assert_eq!(record.label.as_deref(), Some("VIP"));
        assert_eq!(record.comment_count, 1);
        assert!(record.tts_muted);
        reloaded.flush().unwrap();
        assert!(dir
            .store()
            .get("twitch", "user")
            .unwrap()
            .tts_voice
            .is_none());
    }

    #[test]
    fn listener_voice_clamps_json_and_nonfinite_values() {
        for (input, rate, pitch) in [
            (r#"{"rate":-9,"pitch":9}"#, 0.5, 2.0),
            (r#"{"rate":9,"pitch":-9}"#, 2.0, 0.0),
            (r#"{"rate":1.25,"pitch":1.5}"#, 1.25, 1.5),
        ] {
            let voice: TtsVoiceOverride = serde_json::from_str(input).unwrap();
            assert_eq!((voice.rate, voice.pitch), (Some(rate), Some(pitch)));
        }
        let mut voice = TtsVoiceOverride {
            rate: Some(f32::NAN),
            pitch: Some(f32::INFINITY),
            ..TtsVoiceOverride::default()
        };
        voice.normalize();
        assert_eq!((voice.rate, voice.pitch), (None, Some(2.0)));
    }

    #[test]
    fn listeners_read_error_blocks_flush_even_after_file_recovers() {
        let dir = TestDir::new();
        let path = dir.0.join("listeners.json");
        fs::create_dir(&path).unwrap(); // Deterministic non-NotFound error on Windows and Unix.
        let store = dir.store();
        assert!(store.flush_blocked);
        fs::remove_dir(&path).unwrap();
        let original = br#"{"twitch:original":{"name":"Keep"}}"#;
        fs::write(&path, original).unwrap();
        store.observe_at(&mut message(), 1);
        assert!(store.flush().is_err());
        assert!(store.shutdown().is_err());
        assert_eq!(fs::read(path).unwrap(), original);
    }

    #[test]
    fn listeners_failed_backup_blocks_flush_and_keeps_original() {
        let dir = TestDir::new();
        let path = dir.0.join("listeners.json");
        fs::write(&path, b"{broken").unwrap();
        let store = ListenerStore::load_with_backup(&dir.0, |_, backup| {
            // Simulate a same-millisecond collision using the real create_new path.
            fs::write(backup, b"older backup")?;
            backup_corrupt(&path, backup)
        });
        assert!(store.flush_blocked);
        store.observe_at(&mut message(), 1);
        assert!(store.flush().is_err());
        assert!(store.shutdown().is_err());
        assert_eq!(fs::read(path).unwrap(), b"{broken");
        let backup = fs::read_dir(&dir.0)
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| {
                p.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with("listeners.json.bak.")
            })
            .unwrap();
        assert_eq!(fs::read(backup).unwrap(), b"older backup");
    }

    #[test]
    fn listeners_injected_test_comments_are_never_observed() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        msg.injected_test = true;
        store.observe_at(&mut msg, 1);
        assert!(msg.listener.is_none());
        assert!(store.list(None, 100).is_empty());
        assert!(!store.records.lock().unwrap().dirty);
    }

    #[test]
    fn listeners_injected_test_prefix_without_flag_is_observed() {
        let dir = TestDir::new();
        let store = dir.store();
        let mut msg = message();
        msg.id = "injected-test:123-0".into();
        store.observe_at(&mut msg, 1);
        assert!(msg.listener.is_some());
        assert_eq!(store.list(None, 100).len(), 1);
    }
}
