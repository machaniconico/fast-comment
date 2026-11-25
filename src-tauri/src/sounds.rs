//! カスタム効果音の保管庫。
//!
//! 保存先は `<app_config_dir>/sounds/<uuid>.<ext>`。ファイル名は常にこちらで付け、
//! ユーザーが入れた名前は表示名として `sounds.json` に持つだけでパスには使わない。

use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::sound_effects::is_valid_sound_id;

pub const MAX_SOUND_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_SOUNDS: usize = 32;
const MAX_NAME_CHARS: usize = 60;
const INDEX_FILE: &str = "sounds.json";
const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SoundEntry {
    pub id: String,
    /// 表示名(ユーザーが入れた名前を整形した物)。
    pub name: String,
    pub ext: String,
    pub size: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sounds: Vec<SoundEntry>,
}

pub(crate) fn validate_backup_index(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Index>(bytes).is_ok()
}

/// 再生用に UI へ返す中身。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoundData {
    pub mime: &'static str,
    pub bytes: Vec<u8>,
}

pub struct SoundStore {
    root: PathBuf,
    dir: PathBuf,
    lock: Mutex<()>,
    /// 登録済み ID のキャッシュ(索引を書くたびに捨てる)。
    ids: Mutex<Option<std::sync::Arc<std::collections::HashSet<String>>>>,
}

/// マジックバイトから種別を判定する。`wav` / `mp3` / `ogg`。
pub fn detect_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WAVE" {
        Some("wav")
    } else if bytes.starts_with(b"OggS") {
        Some("ogg")
    } else if bytes.starts_with(b"ID3") || (bytes.len() >= 2 && bytes[0] == 0xFF && bytes[1] & 0xE0 == 0xE0) {
        Some("mp3")
    } else {
        None
    }
}

fn mime_of(ext: &str) -> Option<&'static str> {
    match ext {
        "wav" => Some("audio/wav"),
        "mp3" => Some("audio/mpeg"),
        "ogg" => Some("audio/ogg"),
        _ => None,
    }
}

/// パス区切り以前を捨てた名前。
fn base_name(raw: &str) -> &str {
    raw.rsplit(['/', '\\']).next().unwrap_or_default()
}

/// 表示名の整形。パス区切り以前を捨て、制御文字を除き、長さを切る。
pub fn display_name(raw: &str) -> String {
    let cleaned: String = base_name(raw).chars().filter(|c| !c.is_control()).collect();
    let name: String = cleaned.trim().chars().take(MAX_NAME_CHARS).collect();
    if name.is_empty() { "sound".to_owned() } else { name }
}

fn is_usable(entry: &SoundEntry) -> bool {
    is_valid_sound_id(&entry.id) && mime_of(&entry.ext).is_some()
}

impl SoundStore {
    pub fn new(config_dir: &Path) -> Self {
        Self {
            root: config_dir.to_path_buf(),
            dir: config_dir.join("sounds"),
            lock: Mutex::new(()),
            ids: Mutex::new(None),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn file_path(&self, entry: &SoundEntry) -> PathBuf {
        // id と ext は索引を読む時に検証済みの物だけが来る。
        self.dir.join(format!("{}.{}", entry.id, entry.ext))
    }

    /// `sounds` ディレクトリがリンク(symlink・ジャンクション)で保管庫の外を指していないか確かめる。
    /// 無ければ問題なし。リンクは `symlink_metadata` で、ジャンクションは canonicalize の比較で弾く。
    fn check_dir(&self) -> Result<(), String> {
        const REFUSED: &str = "効果音の保存先が不正です";
        let meta = match fs::symlink_metadata(&self.dir) {
            Ok(meta) => meta,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(()),
            Err(_) => return Err(REFUSED.to_owned()),
        };
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(REFUSED.to_owned());
        }
        let root = fs::canonicalize(&self.root).map_err(|_| REFUSED.to_owned())?;
        let dir = fs::canonicalize(&self.dir).map_err(|_| REFUSED.to_owned())?;
        if dir.parent() != Some(root.as_path()) {
            return Err(REFUSED.to_owned());
        }
        Ok(())
    }

    /// 索引を読む。無い時だけ空。読めない・壊れている時はエラー(上書きして一覧を失わないため)。
    fn load(&self) -> Result<Vec<SoundEntry>, String> {
        const BROKEN: &str = "効果音の一覧を読み込めません";
        self.check_dir()?;
        let text = match fs::read_to_string(self.index_path()) {
            Ok(text) => text,
            Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(BROKEN.to_owned()),
        };
        let index = serde_json::from_str::<Index>(&text).map_err(|_| BROKEN.to_owned())?;
        Ok(index.sounds.into_iter().filter(is_usable).collect())
    }

    fn save(&self, sounds: Vec<SoundEntry>) -> Result<(), String> {
        const FAILED: &str = "効果音の一覧を保存できませんでした";
        let text = serde_json::to_vec_pretty(&Index { version: INDEX_VERSION, sounds }).map_err(|_| FAILED)?;
        let tmp = self.dir.join(format!("{INDEX_FILE}.{}.tmp", uuid::Uuid::new_v4()));
        let result = write_new(&tmp, &text).and_then(|()| fs::rename(&tmp, self.index_path()));
        *self.ids.lock().unwrap_or_else(|e| e.into_inner()) = None;
        if result.is_err() {
            let _ = fs::remove_file(&tmp);
        }
        result.map_err(|_| FAILED.to_owned())
    }

    /// 索引と本体を変える操作を止める。バックアップが `sounds/` を読む間に持つ。
    pub fn lock_for_backup(&self) -> std::sync::MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn list(&self) -> Result<Vec<SoundEntry>, String> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        self.load()
    }

    /// 登録済みの ID の集合(キャッシュ)。索引が読めない時は None(判断できない)。
    /// 呼び出し側は、他のロックを持たずに取り、その後で照合する。
    pub fn known_ids(&self) -> Option<std::sync::Arc<std::collections::HashSet<String>>> {
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut cache = self.ids.lock().unwrap_or_else(|e| e.into_inner());
        if cache.is_none() {
            let sounds = self.load().ok()?;
            *cache = Some(std::sync::Arc::new(sounds.into_iter().map(|s| s.id).collect()));
        }
        cache.clone()
    }

    /// 登録済みの ID か。索引が読めない時は判断できないので true(規則は触らない)。
    pub fn contains(&self, id: &str) -> bool {
        self.known_ids().is_none_or(|ids| ids.contains(id))
    }

    /// `file_name` は拡張子の検査と表示名にだけ使う。
    pub fn add(&self, file_name: &str, bytes: &[u8]) -> Result<SoundEntry, String> {
        if bytes.len() > MAX_SOUND_BYTES {
            return Err(format!("効果音のファイルが大きすぎます(上限 {} MiB)", MAX_SOUND_BYTES / 1024 / 1024));
        }
        // 拡張子は切り詰める前の名前で調べる。
        let claimed = base_name(file_name).rsplit_once('.').map(|(_, ext)| ext.trim().to_ascii_lowercase());
        let detected = detect_ext(bytes);
        match (claimed.as_deref(), detected) {
            (Some(c), Some(d)) if c == d => {}
            (Some("wav" | "mp3" | "ogg"), _) | (_, None) => {
                return Err("wav / mp3 / ogg の音声ファイルを選んでください(拡張子と中身が一致しません)".to_owned());
            }
            _ => return Err("wav / mp3 / ogg の音声ファイルを選んでください".to_owned()),
        }
        let ext = detected.unwrap_or_default();
        let name = display_name(file_name);

        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut sounds = self.load()?;
        if sounds.len() >= MAX_SOUNDS {
            return Err(format!("効果音は {MAX_SOUNDS} 件までです。不要な物を削除してください"));
        }
        fs::create_dir_all(&self.dir).map_err(|_| "効果音の保存先を作れませんでした".to_owned())?;
        self.check_dir()?;
        let entry = SoundEntry {
            id: uuid::Uuid::new_v4().to_string(),
            name,
            ext: ext.to_owned(),
            size: bytes.len() as u64,
        };
        let path = self.file_path(&entry);
        write_new(&path, bytes).map_err(|_| "効果音を保存できませんでした".to_owned())?;
        sounds.push(entry.clone());
        if let Err(e) = self.save(sounds) {
            let _ = fs::remove_file(&path);
            return Err(e);
        }
        Ok(entry)
    }

    /// 無い ID は `Ok(false)`。形式が不正な ID は触らずエラー。本体 → 索引の順に消す。
    pub fn remove(&self, id: &str) -> Result<bool, String> {
        if !is_valid_sound_id(id) {
            return Err("効果音の ID が不正です".to_owned());
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut sounds = self.load()?;
        let Some(pos) = sounds.iter().position(|s| s.id == id) else { return Ok(false) };
        let entry = sounds.remove(pos);
        match fs::remove_file(self.file_path(&entry)) {
            Ok(()) => {}
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(_) => return Err("効果音のファイルを削除できませんでした".to_owned()),
        }
        self.save(sounds)?;
        Ok(true)
    }

    pub fn read(&self, id: &str) -> Result<SoundData, String> {
        if !is_valid_sound_id(id) {
            return Err("効果音の ID が不正です".to_owned());
        }
        let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let entry = self
            .load()?
            .into_iter()
            .find(|s| s.id == id)
            .ok_or_else(|| "効果音が見つかりません".to_owned())?;
        let path = self.file_path(&entry);
        let meta = fs::symlink_metadata(&path).map_err(|_| "効果音が見つかりません".to_owned())?;
        if !meta.is_file() || meta.len() > MAX_SOUND_BYTES as u64 {
            return Err("効果音が読み込めません".to_owned());
        }
        let bytes = fs::read(&path).map_err(|_| "効果音が読み込めません".to_owned())?;
        let mime = mime_of(&entry.ext).unwrap_or("application/octet-stream");
        Ok(SoundData { mime, bytes })
    }
}

fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav() -> Vec<u8> {
        let mut v = b"RIFF".to_vec();
        v.extend_from_slice(&[36, 0, 0, 0]);
        v.extend_from_slice(b"WAVEfmt ");
        v.extend_from_slice(&[0; 16]);
        v
    }
    fn mp3_id3() -> Vec<u8> {
        let mut v = b"ID3".to_vec();
        v.extend_from_slice(&[4, 0, 0, 0, 0, 0, 0, 0xFF, 0xFB, 0x90, 0]);
        v
    }
    fn mp3_frame() -> Vec<u8> {
        vec![0xFF, 0xFB, 0x90, 0x00, 0, 0, 0, 0]
    }
    fn ogg() -> Vec<u8> {
        let mut v = b"OggS".to_vec();
        v.extend_from_slice(&[0; 24]);
        v
    }

    fn store() -> (tempfile::TempDir, SoundStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = SoundStore::new(dir.path());
        (dir, s)
    }

    #[test]
    fn detects_by_magic_bytes() {
        assert_eq!(detect_ext(&wav()), Some("wav"));
        assert_eq!(detect_ext(&mp3_id3()), Some("mp3"));
        assert_eq!(detect_ext(&mp3_frame()), Some("mp3"));
        assert_eq!(detect_ext(&ogg()), Some("ogg"));
        assert_eq!(detect_ext(b"RIFF\0\0\0\0AVI LIST"), None);
        assert_eq!(detect_ext(b"MZ\x90\0"), None);
        assert_eq!(detect_ext(b""), None);
        assert_eq!(detect_ext(&[0xFF]), None);
    }

    #[test]
    fn add_list_read_remove_roundtrip_with_uuid_file_name() {
        let (dir, s) = store();
        let e = s.add("ピンポン.WAV", &wav()).unwrap();
        assert_eq!(e.ext, "wav");
        assert_eq!(e.name, "ピンポン.WAV");
        assert!(is_valid_sound_id(&e.id));
        assert_eq!(s.list().unwrap(), vec![e.clone()]);
        let on_disk = dir.path().join("sounds").join(format!("{}.wav", e.id));
        assert!(on_disk.is_file());
        // ユーザー名は保存先のどこにも出ない。
        let names: Vec<_> = fs::read_dir(dir.path().join("sounds")).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(names.iter().all(|n| !n.contains("ピンポン")), "{names:?}");
        let data = s.read(&e.id).unwrap();
        assert_eq!(data.mime, "audio/wav");
        assert_eq!(data.bytes, wav());
        assert!(s.remove(&e.id).unwrap());
        assert!(!on_disk.exists());
        assert!(s.list().unwrap().is_empty());
        assert!(!s.remove(&e.id).unwrap());
        assert!(s.read(&e.id).is_err());
    }

    #[test]
    fn backup_lock_holds_off_removal_until_released() {
        let (_d, s) = store();
        let e = s.add("a.wav", &wav()).unwrap();
        let s = std::sync::Arc::new(s);
        let guard = s.lock_for_backup();
        let (tx, rx) = std::sync::mpsc::channel();
        let worker = {
            let s = s.clone();
            let id = e.id.clone();
            std::thread::spawn(move || tx.send(s.remove(&id)).unwrap())
        };
        assert!(rx.recv_timeout(std::time::Duration::from_millis(200)).is_err());
        assert!(s.file_path(&e).is_file(), "ロック中は本体を消さない");
        drop(guard);
        assert_eq!(rx.recv().unwrap(), Ok(true));
        worker.join().unwrap();
    }

    #[test]
    fn rejects_oversize_but_accepts_exactly_the_limit() {
        let (_d, s) = store();
        let mut ok = wav();
        ok.resize(MAX_SOUND_BYTES, 0);
        assert!(s.add("a.wav", &ok).is_ok());
        let mut big = wav();
        big.resize(MAX_SOUND_BYTES + 1, 0);
        assert!(s.add("b.wav", &big).is_err());
        assert_eq!(s.list().unwrap().len(), 1);
    }

    #[test]
    fn rejects_fake_extension_and_magic_mismatch() {
        let (_d, s) = store();
        assert!(s.add("a.exe", &wav()).is_err());
        assert!(s.add("a", &wav()).is_err());
        assert!(s.add("a.wav", b"MZ\x90\0\x03\0\0\0").is_err());
        assert!(s.add("a.mp3", &wav()).is_err());
        assert!(s.add("a.wav.exe", &wav()).is_err());
        assert!(s.add("a.ogg", &mp3_frame()).is_err());
        assert!(s.add("a.wav", b"").is_err());
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn enforces_max_count() {
        let (_d, s) = store();
        for i in 0..MAX_SOUNDS {
            s.add(&format!("{i}.ogg"), &ogg()).unwrap();
        }
        assert!(s.add("x.ogg", &ogg()).is_err());
        let first = s.list().unwrap()[0].id.clone();
        s.remove(&first).unwrap();
        assert!(s.add("x.ogg", &ogg()).is_ok());
    }

    #[test]
    fn invalid_ids_never_touch_the_filesystem() {
        let (dir, s) = store();
        let outside = dir.path().join("secret.wav");
        fs::write(&outside, wav()).unwrap();
        for bad in ["", "..", "../secret", "../secret.wav", "a/b", "A0B1C2D3-0000-4000-8000-000000000000", "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d/../x", "0b1c2d3e4f504a6b8c7d9e0f1a2b3c4d"] {
            assert!(s.read(bad).is_err(), "{bad}");
            assert!(s.remove(bad).is_err(), "{bad}");
        }
        assert!(outside.exists());
    }

    #[test]
    fn unknown_valid_id_is_not_found_and_errors_do_not_echo_names() {
        let (_d, s) = store();
        assert!(s.read("0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d").is_err());
        let err = s.add("秘密の名前.exe", &wav()).unwrap_err();
        assert!(!err.contains("秘密"), "{err}");
        let dir_str = _d.path().to_string_lossy().into_owned();
        assert!(!err.contains(&dir_str));
    }

    #[test]
    fn display_name_is_sanitized() {
        assert_eq!(display_name("C:\\Users\\x\\a.wav"), "a.wav");
        assert_eq!(display_name("../../etc/passwd.mp3"), "passwd.mp3");
        assert_eq!(display_name("  a\u{0}b\n.wav "), "ab.wav");
        assert_eq!(display_name(""), "sound");
        assert_eq!(display_name("/"), "sound");
        assert_eq!(display_name(&"あ".repeat(100)).chars().count(), 60);
    }

    #[test]
    fn missing_index_is_empty_but_unreadable_or_broken_index_is_an_error_and_never_overwritten() {
        let (dir, s) = store();
        assert!(s.list().unwrap().is_empty());
        let e = s.add("a.wav", &wav()).unwrap();
        let index = dir.path().join("sounds").join(INDEX_FILE);

        // 壊れた索引: 一覧も追加も削除もエラーで、中身は触らない。
        fs::write(&index, "{not json").unwrap();
        assert!(s.list().is_err());
        assert!(s.add("b.wav", &wav()).is_err());
        assert!(s.remove(&e.id).is_err());
        assert_eq!(fs::read_to_string(&index).unwrap(), "{not json");

        // 読めない索引(ディレクトリ)も同じ。
        fs::remove_file(&index).unwrap();
        fs::create_dir(&index).unwrap();
        assert!(s.list().is_err());
        assert!(s.add("b.wav", &wav()).is_err());
        assert!(index.is_dir());
    }

    #[test]
    fn remove_deletes_the_file_before_the_index_and_keeps_the_index_when_it_fails() {
        let (dir, s) = store();
        let e = s.add("a.wav", &wav()).unwrap();
        let file = dir.path().join("sounds").join(format!("{}.wav", e.id));
        // 本体が消せない(ディレクトリに差し替え)なら索引は変えずにエラー。
        fs::remove_file(&file).unwrap();
        fs::create_dir(&file).unwrap();
        assert!(s.remove(&e.id).is_err());
        assert_eq!(s.list().unwrap(), vec![e.clone()]);
        // 本体が既に無ければ成功として索引だけ更新する。
        fs::remove_dir(&file).unwrap();
        assert!(s.remove(&e.id).unwrap());
        assert!(s.list().unwrap().is_empty());
    }

    #[test]
    fn long_file_name_is_judged_before_it_is_truncated() {
        let (_d, s) = store();
        let e = s.add(&format!("{}.wav", "a".repeat(57)), &wav()).unwrap();
        assert_eq!(e.ext, "wav");
        let e = s.add(&format!("{}.mp3", "あ".repeat(200)), &mp3_frame()).unwrap();
        assert_eq!(e.name.chars().count(), 60);
        assert!(s.add(&format!("{}.exe", "a".repeat(200)), &wav()).is_err());
    }

    #[test]
    fn contains_tracks_adds_and_removes() {
        let (_d, s) = store();
        let missing = "0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d";
        assert!(!s.contains(missing));
        let e = s.add("a.wav", &wav()).unwrap();
        assert!(s.contains(&e.id));
        s.remove(&e.id).unwrap();
        assert!(!s.contains(&e.id));
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_sounds_directory_is_refused_for_every_operation() {
        let (dir, s) = store();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("sounds")).unwrap();
        assert!(s.add("a.wav", &wav()).is_err());
        assert!(s.list().is_err());
        assert!(s.read("0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d").is_err());
        assert!(s.remove("0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d").is_err());
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn index_entries_with_bad_ids_or_exts_are_ignored() {
        let (dir, s) = store();
        fs::create_dir_all(dir.path().join("sounds")).unwrap();
        let idx = r#"{"version":1,"sounds":[{"id":"../x","name":"n","ext":"wav","size":1},{"id":"0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d","name":"n","ext":"exe","size":1}]}"#;
        fs::write(dir.path().join("sounds").join(INDEX_FILE), idx).unwrap();
        assert!(s.list().unwrap().is_empty());
        assert!(s.read("0b1c2d3e-4f50-4a6b-8c7d-9e0f1a2b3c4d").is_err());
    }
}
